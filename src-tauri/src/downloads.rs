use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::AsyncWriteExt;

use crate::error::{AppError, AppResult};
use crate::library::{Library, PackageReplacement, ResourceRecord};
use crate::state::{AppState, DownloadSession};

const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RESOURCES: usize = 20_000;
const MAX_RESOURCE_BYTES: u64 = 128 * 1024 * 1024 * 1024;
const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct PackageManifest {
    schema_version: u8,
    package_id: String,
    package_title: String,
    module: ModuleManifest,
    root_url: String,
    resources: Vec<ResourceManifest>,
}

#[derive(Debug, Deserialize)]
struct ModuleManifest {
    id: String,
    title: String,
    root_url: String,
}

#[derive(Debug, Deserialize)]
struct ResourceManifest {
    url: String,
    #[serde(rename = "type")]
    resource_type: String,
    size: Option<u64>,
    sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct DownloadEvent {
    package_id: Option<String>,
    status: &'static str,
    progress: f64,
    message: String,
}

struct DownloadedResource {
    local_path: PathBuf,
    mime_type: Option<String>,
    byte_size: u64,
    sha256: String,
    created: bool,
}

struct DownloadContext<'a> {
    app: &'a AppHandle,
    state: &'a AppState,
    session: &'a DownloadSession,
    download_directory: &'a Path,
    node_origin: &'a str,
    package_id: &'a str,
    operation_sequence: u64,
    resource_count: usize,
}

pub fn enqueue(app: &AppHandle, manifest_url: String) -> AppResult<bool> {
    let state = app.state::<AppState>();
    let Some(operation_sequence) = state.reserve_download(&manifest_url)? else {
        return Ok(false);
    };
    emit(
        app,
        DownloadEvent {
            package_id: None,
            status: "downloading",
            progress: 0.0,
            message: "Preparing package download...".into(),
        },
    );
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = run(&app, &manifest_url, operation_sequence).await;
        app.state::<AppState>().release_download(&manifest_url);
        match result {
            Ok(package_id) => {
                emit(
                    &app,
                    DownloadEvent {
                        package_id: Some(package_id),
                        status: "ready",
                        progress: 1.0,
                        message: "Package saved on this device.".into(),
                    },
                );
                let _ = app.emit_to("main", "library-updated", ());
            }
            Err(AppError::OperationSuperseded) => {}
            Err(error) => emit(
                &app,
                DownloadEvent {
                    package_id: None,
                    status: "failed",
                    progress: 0.0,
                    message: error.to_string(),
                },
            ),
        }
    });
    Ok(true)
}

async fn run(app: &AppHandle, manifest_url: &str, operation_sequence: u64) -> AppResult<String> {
    validate_relative_url(manifest_url)?;
    let state = app.state::<AppState>();
    let session = state.download_session()?;
    let response = state
        .node_client()
        .get_authenticated(&session.node_url, manifest_url, &session.credential)
        .await?;
    if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
        return Err(AppError::SessionMissing);
    }
    if !response.status().is_success() {
        return Err(AppError::Download(format!(
            "manifest returned HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES)
    {
        return Err(AppError::InvalidPackage("manifest is too large".into()));
    }
    let manifest_bytes = read_limited(response, MAX_MANIFEST_BYTES).await?;
    let manifest: PackageManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| AppError::InvalidPackage(error.to_string()))?;
    validate_manifest(&manifest)?;

    let node_origin = session.node_url.as_str().trim_end_matches('/').to_owned();
    if !state.register_package_refresh(&node_origin, &manifest.package_id, operation_sequence)? {
        return Err(AppError::OperationSuperseded);
    }
    let saved_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| AppError::Internal(error.to_string()))?
        .as_secs()
        .to_string();

    let download_directory = session
        .data_dir
        .join("downloads")
        .join(digest_hex(&node_origin))
        .join(digest_hex(&manifest.package_id))
        .join(digest_hex(manifest_url));
    tokio::fs::create_dir_all(&download_directory)
        .await
        .map_err(|error| storage_error("create download directory", &download_directory, error))?;
    let staged_manifest = download_directory.join("manifest.json");
    crate::atomic_file::write_async(staged_manifest, manifest_bytes.clone()).await?;
    // Keep reuse checks valid until the new references are published and old objects are collected.
    let _object_store = state.lock_object_store().await;
    if let Err(error) = ensure_current_operation(
        &state,
        &node_origin,
        &manifest.package_id,
        operation_sequence,
    ) {
        let _ = tokio::fs::remove_dir_all(&download_directory).await;
        return Err(error);
    }
    let mut created_objects = HashSet::new();
    let result = download_resources(
        app,
        &session,
        &node_origin,
        &manifest,
        &download_directory,
        operation_sequence,
        &mut created_objects,
    )
    .await;
    let prepared = match result {
        Ok(prepared) => prepared,
        Err(error) => {
            cleanup_objects(state.library(), &session.data_dir, &created_objects).await;
            let _ = tokio::fs::remove_dir_all(&download_directory).await;
            return Err(error);
        }
    };
    if !state.session_is_current(session.generation) {
        cleanup_objects(state.library(), &session.data_dir, &created_objects).await;
        let _ = tokio::fs::remove_dir_all(&download_directory).await;
        return Err(AppError::SessionMissing);
    }
    if let Err(error) = ensure_current_operation(
        &state,
        &node_origin,
        &manifest.package_id,
        operation_sequence,
    ) {
        cleanup_objects(state.library(), &session.data_dir, &created_objects).await;
        let _ = tokio::fs::remove_dir_all(&download_directory).await;
        return Err(error);
    }

    let package_directory =
        package_directory(&session.data_dir, &node_origin, &manifest.package_id);
    if let Err(error) = tokio::fs::create_dir_all(&package_directory).await {
        cleanup_objects(state.library(), &session.data_dir, &created_objects).await;
        let _ = tokio::fs::remove_dir_all(&download_directory).await;
        return Err(storage_error(
            "create package directory",
            &package_directory,
            error,
        ));
    }
    let manifest_path = package_directory.join("manifest.json");
    let previous_manifest = match tokio::fs::read(&manifest_path).await {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            cleanup_objects(state.library(), &session.data_dir, &created_objects).await;
            let _ = tokio::fs::remove_dir_all(&download_directory).await;
            return Err(storage_error(
                "read existing manifest",
                &manifest_path,
                error,
            ));
        }
    };
    if let Err(error) = crate::atomic_file::write_async(manifest_path.clone(), manifest_bytes).await
    {
        cleanup_objects(state.library(), &session.data_dir, &created_objects).await;
        let _ = tokio::fs::remove_dir_all(&download_directory).await;
        return Err(error);
    }

    let replacement = PackageReplacement {
        node_origin: &node_origin,
        module_id: &manifest.module.id,
        module_title: &manifest.module.title,
        module_root_url: &manifest.module.root_url,
        package_id: &manifest.package_id,
        package_title: &manifest.package_title,
        root_url: &manifest.root_url,
        saved_at: &saved_at,
        byte_size: prepared.total_size,
        resources: &prepared.resources,
    };
    let publish_result = state.publish_package_if_current(
        &node_origin,
        &manifest.package_id,
        operation_sequence,
        || state.library().replace_package(&replacement),
    );
    let _ = tokio::fs::remove_dir_all(&download_directory).await;
    match publish_result {
        Ok(Some(orphaned_paths)) => {
            cleanup_path_strings(state.library(), &session.data_dir, &orphaned_paths).await;
            Ok(manifest.package_id)
        }
        Ok(None) => {
            restore_manifest(manifest_path, previous_manifest).await;
            cleanup_objects(state.library(), &session.data_dir, &created_objects).await;
            Err(AppError::OperationSuperseded)
        }
        Err(error) => {
            restore_manifest(manifest_path, previous_manifest).await;
            cleanup_objects(state.library(), &session.data_dir, &created_objects).await;
            Err(error)
        }
    }
}

struct PreparedResources {
    resources: Vec<ResourceRecord>,
    total_size: u64,
}

struct CachedObject {
    local_path: PathBuf,
    byte_size: u64,
    sha256: String,
}

async fn download_resources(
    app: &AppHandle,
    session: &DownloadSession,
    node_origin: &str,
    manifest: &PackageManifest,
    download_directory: &Path,
    operation_sequence: u64,
    created_objects: &mut HashSet<PathBuf>,
) -> AppResult<PreparedResources> {
    let state = app.state::<AppState>();
    let mut total_size = 0_u64;
    let mut resources = Vec::with_capacity(manifest.resources.len());
    let resource_count = manifest.resources.len();
    let context = DownloadContext {
        app,
        state: &state,
        session,
        download_directory,
        node_origin,
        package_id: &manifest.package_id,
        operation_sequence,
        resource_count,
    };
    for (index, resource) in manifest.resources.iter().enumerate() {
        if !state.session_is_current(session.generation) {
            return Err(AppError::SessionMissing);
        }
        ensure_current_operation(
            &state,
            node_origin,
            &manifest.package_id,
            operation_sequence,
        )?;
        let downloaded = if let Some(cached) = reusable_object(&session.data_dir, resource) {
            emit(
                app,
                DownloadEvent {
                    package_id: Some(manifest.package_id.clone()),
                    status: "downloading",
                    progress: (index + 1) as f64 / resource_count.max(1) as f64,
                    message: format!("Reusing cached {}", resource.url),
                },
            );
            DownloadedResource {
                mime_type: state.library().mime_type_for_resource(
                    node_origin,
                    &manifest.package_id,
                    &resource.url,
                    &resource.resource_type,
                    &cached.sha256,
                    cached.byte_size,
                )?,
                local_path: cached.local_path,
                byte_size: cached.byte_size,
                sha256: cached.sha256,
                created: false,
            }
        } else {
            emit(
                app,
                DownloadEvent {
                    package_id: Some(manifest.package_id.clone()),
                    status: "downloading",
                    progress: index as f64 / resource_count.max(1) as f64,
                    message: format!("Downloading {}", resource.url),
                },
            );
            download_resource(&context, resource, index).await?
        };
        if downloaded.created {
            created_objects.insert(downloaded.local_path.clone());
        }
        total_size = total_size
            .checked_add(downloaded.byte_size)
            .ok_or_else(|| AppError::Storage("package size overflow".into()))?;
        if total_size > MAX_PACKAGE_BYTES {
            return Err(AppError::Download(
                "package exceeds the 512 GiB safety limit".into(),
            ));
        }
        let local_path = downloaded
            .local_path
            .to_str()
            .ok_or_else(|| AppError::Storage("resource path is not UTF-8".into()))?;
        resources.push(ResourceRecord {
            resource_url: resource.url.clone(),
            resource_type: resource.resource_type.clone(),
            local_path: local_path.to_owned(),
            mime_type: downloaded.mime_type,
            byte_size: downloaded.byte_size,
            sha256: downloaded.sha256,
        });
    }
    Ok(PreparedResources {
        resources,
        total_size,
    })
}

fn reusable_object(data_dir: &Path, resource: &ResourceManifest) -> Option<CachedObject> {
    let expected_size = resource.size?;
    let sha256 = resource.sha256.as_deref()?.to_ascii_lowercase();
    let local_path = object_path(data_dir, &sha256)?;
    let parent = local_path.parent()?;
    if !is_regular_directory(&data_dir.join("objects")) || !is_regular_directory(parent) {
        return None;
    }
    let metadata = std::fs::symlink_metadata(&local_path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.len() > MAX_RESOURCE_BYTES
        || expected_size != metadata.len()
    {
        return None;
    }
    Some(CachedObject {
        local_path,
        byte_size: metadata.len(),
        sha256,
    })
}

fn is_regular_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn object_path(data_dir: &Path, sha256: &str) -> Option<PathBuf> {
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let sha256 = sha256.to_ascii_lowercase();
    Some(data_dir.join("objects").join(&sha256[..2]).join(sha256))
}

async fn download_resource(
    context: &DownloadContext<'_>,
    resource: &ResourceManifest,
    resource_index: usize,
) -> AppResult<DownloadedResource> {
    let app = context.app;
    let state = context.state;
    let session = context.session;
    let download_directory = context.download_directory;
    let package_id = context.package_id;
    let node_origin = context.node_origin;
    let operation_sequence = context.operation_sequence;
    let resource_count = context.resource_count;
    if !state.session_is_current(session.generation) {
        return Err(AppError::SessionMissing);
    }
    let response = state
        .node_client()
        .get_authenticated(&session.node_url, &resource.url, &session.credential)
        .await?;
    validate_resource_status(&resource.url, response.status())?;
    if resource.size.is_some_and(|size| size > MAX_RESOURCE_BYTES)
        || response
            .content_length()
            .is_some_and(|size| size > MAX_RESOURCE_BYTES)
    {
        return Err(AppError::Download(format!(
            "{} exceeds the 128 GiB resource limit",
            resource.url
        )));
    }
    if let (Some(expected), Some(actual)) = (resource.size, response.content_length())
        && expected != actual
    {
        return Err(AppError::Download(format!(
            "size mismatch for {}",
            resource.url
        )));
    }
    let expected_size = resource.size.or(response.content_length());
    let mime_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    tokio::fs::create_dir_all(download_directory)
        .await
        .map_err(|error| storage_error("create download directory", download_directory, error))?;
    let temporary_path =
        download_directory.join(format!("resource-{}.part", digest_hex(&resource.url)));
    let mut file = tokio::fs::File::create(&temporary_path)
        .await
        .map_err(|error| storage_error("create temporary resource", &temporary_path, error))?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut byte_size = 0_u64;
    let mut last_percent = (resource_index * 100 / resource_count.max(1)) as u32;
    while let Some(chunk) = stream.next().await {
        if !state.session_is_current(session.generation) {
            drop(file);
            let _ = tokio::fs::remove_file(&temporary_path).await;
            return Err(AppError::SessionMissing);
        }
        let chunk = chunk.map_err(|error| AppError::Download(error.to_string()))?;
        byte_size = byte_size
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| AppError::Storage("resource size overflow".into()))?;
        if byte_size > MAX_RESOURCE_BYTES {
            drop(file);
            let _ = tokio::fs::remove_file(&temporary_path).await;
            return Err(AppError::Download(format!(
                "{} exceeds the 128 GiB resource limit",
                resource.url
            )));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| storage_error("write temporary resource", &temporary_path, error))?;
        if let Some(expected_size) = expected_size.filter(|size| *size > 0) {
            let resource_progress = (byte_size as f64 / expected_size as f64).min(1.0);
            let progress =
                (resource_index as f64 + resource_progress) / resource_count.max(1) as f64;
            let percent = (progress * 100.0) as u32;
            if percent > last_percent {
                last_percent = percent;
                emit(
                    app,
                    DownloadEvent {
                        package_id: Some(package_id.to_owned()),
                        status: "downloading",
                        progress,
                        message: format!("Downloading {}", resource.url),
                    },
                );
            }
        }
    }
    file.flush()
        .await
        .map_err(|error| storage_error("flush temporary resource", &temporary_path, error))?;
    file.sync_all()
        .await
        .map_err(|error| storage_error("sync temporary resource", &temporary_path, error))?;
    drop(file);

    if resource.size.is_some_and(|expected| expected != byte_size) {
        let _ = tokio::fs::remove_file(&temporary_path).await;
        return Err(AppError::Download(format!(
            "size mismatch for {}",
            resource.url
        )));
    }
    let sha256 = format!("{:x}", hasher.finalize());
    if resource
        .sha256
        .as_ref()
        .is_some_and(|expected| !expected.eq_ignore_ascii_case(&sha256))
    {
        let _ = tokio::fs::remove_file(&temporary_path).await;
        return Err(AppError::Download(format!(
            "SHA-256 mismatch for {}",
            resource.url
        )));
    }
    if let Err(error) = ensure_current_operation(state, node_origin, package_id, operation_sequence)
    {
        let _ = tokio::fs::remove_file(&temporary_path).await;
        return Err(error);
    }
    let local_path = object_path(&session.data_dir, &sha256)
        .ok_or_else(|| AppError::Storage("calculated invalid object hash".into()))?;
    let object_directory = local_path
        .parent()
        .ok_or_else(|| AppError::Storage("object path has no parent".into()))?;
    ensure_object_directory(&session.data_dir, object_directory).await?;
    let existing = match tokio::fs::symlink_metadata(&local_path).await {
        Ok(metadata) if metadata.file_type().is_file() && metadata.len() == byte_size => true,
        Ok(_) => {
            let _ = tokio::fs::remove_file(&temporary_path).await;
            return Err(AppError::Storage(format!(
                "invalid existing object at {}",
                local_path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary_path).await;
            return Err(storage_error("inspect object path", &local_path, error));
        }
    };
    if existing {
        tokio::fs::remove_file(&temporary_path)
            .await
            .map_err(|error| storage_error("remove duplicate resource", &temporary_path, error))?;
    } else {
        tokio::fs::rename(&temporary_path, &local_path)
            .await
            .map_err(|error| storage_error("store downloaded resource", &local_path, error))?;
        crate::atomic_file::sync_directory_async(object_directory.to_path_buf()).await?;
    }
    Ok(DownloadedResource {
        local_path,
        mime_type,
        byte_size,
        sha256,
        created: !existing,
    })
}

fn validate_resource_status(resource_url: &str, status: StatusCode) -> AppResult<()> {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(AppError::SessionMissing);
    }
    if !status.is_success() {
        return Err(AppError::Download(format!(
            "{resource_url} returned HTTP {status}"
        )));
    }
    Ok(())
}

fn ensure_current_operation(
    state: &AppState,
    node_origin: &str,
    package_id: &str,
    operation_sequence: u64,
) -> AppResult<()> {
    if state.package_operation_is_current(node_origin, package_id, operation_sequence)? {
        Ok(())
    } else {
        Err(AppError::OperationSuperseded)
    }
}

async fn restore_manifest(path: PathBuf, previous_manifest: Option<Vec<u8>>) {
    match previous_manifest {
        Some(bytes) => {
            let _ = crate::atomic_file::write_async(path, bytes).await;
        }
        None => {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

async fn ensure_object_directory(data_dir: &Path, object_directory: &Path) -> AppResult<()> {
    let objects_directory = data_dir.join("objects");
    for directory in [&objects_directory, object_directory] {
        match tokio::fs::symlink_metadata(directory).await {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(AppError::Storage(format!(
                    "object directory is not a regular directory: {}",
                    directory.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = tokio::fs::create_dir(directory).await {
                    if error.kind() != std::io::ErrorKind::AlreadyExists
                        || !tokio::fs::symlink_metadata(directory)
                            .await
                            .is_ok_and(|metadata| metadata.file_type().is_dir())
                    {
                        return Err(storage_error("create object directory", directory, error));
                    }
                }
            }
            Err(error) => return Err(storage_error("inspect object directory", directory, error)),
        }
    }
    Ok(())
}

async fn cleanup_objects(library: &Library, data_dir: &Path, paths: &HashSet<PathBuf>) {
    let paths = paths
        .iter()
        .filter_map(|path| path.to_str().map(str::to_owned))
        .collect::<Vec<_>>();
    cleanup_path_strings(library, data_dir, &paths).await;
}

pub(crate) async fn cleanup_path_strings(library: &Library, data_dir: &Path, paths: &[String]) {
    let candidates = paths
        .iter()
        .filter(|path| is_object_path(data_dir, Path::new(path)))
        .cloned()
        .collect::<Vec<_>>();
    let Ok(unreferenced) = library.unreferenced_paths(&candidates) else {
        return;
    };
    for path in unreferenced {
        let path = PathBuf::from(path);
        if tokio::fs::remove_file(&path).await.is_ok()
            && let Some(parent) = path.parent()
        {
            let _ = tokio::fs::remove_dir(parent).await;
        }
    }
}

pub(crate) fn gc_unreferenced_objects(data_dir: &Path, library: &Library) {
    let objects_directory = data_dir.join("objects");
    if !is_regular_directory(&objects_directory) {
        return;
    }
    let Ok(prefixes) = std::fs::read_dir(&objects_directory) else {
        return;
    };
    let mut candidates = Vec::new();
    for prefix in prefixes.flatten() {
        if !prefix.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(prefix.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|file_type| file_type.is_file())
                && is_object_path(data_dir, &path)
                && let Some(path) = path.to_str()
            {
                candidates.push(path.to_owned());
            }
        }
    }
    let Ok(unreferenced) = library.unreferenced_paths(&candidates) else {
        return;
    };
    for path in unreferenced {
        let path = PathBuf::from(path);
        if std::fs::remove_file(&path).is_ok()
            && let Some(parent) = path.parent()
        {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

fn is_object_path(data_dir: &Path, path: &Path) -> bool {
    let Some(sha256) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    object_path(data_dir, sha256).is_some_and(|expected| expected == path)
}

fn storage_error(operation: &str, path: &Path, error: std::io::Error) -> AppError {
    AppError::Storage(format!("{operation} at {}: {error}", path.display()))
}

fn validate_manifest(manifest: &PackageManifest) -> AppResult<()> {
    if manifest.schema_version != 1 {
        return Err(AppError::InvalidPackage(
            "unsupported schema_version".into(),
        ));
    }
    validate_package_id(&manifest.package_id)?;
    validate_id(&manifest.module.id, "module.id")?;
    if manifest.package_title.trim().is_empty() || manifest.package_title.len() > 512 {
        return Err(AppError::InvalidPackage("invalid package_title".into()));
    }
    if manifest.module.title.trim().is_empty() || manifest.module.title.len() > 256 {
        return Err(AppError::InvalidPackage("invalid module.title".into()));
    }
    validate_relative_url(&manifest.root_url)?;
    validate_relative_url(&manifest.module.root_url)?;
    if manifest.resources.is_empty() || manifest.resources.len() > MAX_RESOURCES {
        return Err(AppError::InvalidPackage("invalid resource count".into()));
    }
    let allowed_types = [
        "binary",
        "container",
        "css",
        "html",
        "image",
        "js",
        "json",
        "text",
    ];
    let mut urls = HashSet::new();
    for resource in &manifest.resources {
        validate_relative_url(&resource.url)?;
        if !allowed_types.contains(&resource.resource_type.as_str()) {
            return Err(AppError::InvalidPackage(format!(
                "unsupported resource type: {}",
                resource.resource_type
            )));
        }
        if !urls.insert(&resource.url) {
            return Err(AppError::InvalidPackage(format!(
                "duplicate resource URL: {}",
                resource.url
            )));
        }
        if let Some(hash) = &resource.sha256
            && (hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(AppError::InvalidPackage("invalid resource SHA-256".into()));
        }
        if resource.size.is_some_and(|size| size > MAX_RESOURCE_BYTES) {
            return Err(AppError::InvalidPackage(
                "resource exceeds the 128 GiB safety limit".into(),
            ));
        }
    }
    Ok(())
}

fn validate_id(value: &str, field: &str) -> AppResult<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(AppError::InvalidPackage(format!("invalid {field}")));
    }
    Ok(())
}

pub(crate) fn validate_package_id(value: &str) -> AppResult<()> {
    let mut bytes = value.bytes();
    if value.len() > 128
        || !bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(AppError::InvalidPackage("invalid package_id".into()));
    }
    Ok(())
}

fn validate_relative_url(value: &str) -> AppResult<()> {
    if !value.starts_with('/')
        || value.starts_with("//")
        || value.contains('#')
        || value.len() > 4096
    {
        return Err(AppError::InvalidPackage("invalid relative URL".into()));
    }
    Ok(())
}

pub(crate) fn package_directory(data_dir: &Path, node_origin: &str, package_id: &str) -> PathBuf {
    data_dir
        .join("packages")
        .join(digest_hex(node_origin))
        .join(digest_hex(package_id))
}

async fn read_limited(response: reqwest::Response, limit: u64) -> AppResult<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AppError::Download(error.to_string()))?;
        let next_length = (bytes.len() as u64)
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| AppError::InvalidPackage("response size overflow".into()))?;
        if next_length > limit {
            return Err(AppError::InvalidPackage("manifest is too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn digest_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn emit(app: &AppHandle, event: DownloadEvent) {
    #[cfg(target_os = "android")]
    crate::mobile_node::notify_download(app, event.status, event.progress, &event.message);
    let _ = app.emit_to("main", "download-status", event);
}

#[cfg(test)]
mod tests {
    use super::{
        PackageManifest, ResourceManifest, gc_unreferenced_objects, is_object_path, object_path,
        reusable_object, validate_manifest, validate_package_id, validate_resource_status,
    };
    use crate::error::AppError;
    use crate::library::{Library, PackageReplacement, ResourceRecord};

    fn resource(sha256: Option<&str>, size: Option<u64>) -> ResourceManifest {
        ResourceManifest {
            url: "/resource".into(),
            resource_type: "binary".into(),
            size,
            sha256: sha256.map(str::to_owned),
        }
    }

    #[test]
    fn accepts_versioned_same_origin_manifest() {
        let manifest: PackageManifest = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "package_id": "video_123",
            "package_title": "Video: Example",
            "module": {"id": "video_archiver", "title": "Video Archive", "root_url": "/video-archiver/dashboard"},
            "root_url": "/video-archiver/dashboard?package_id=video_123",
            "resources": [{"url": "/api/packages/video_123/nsp", "type": "container"}]
        }))
        .unwrap();
        validate_manifest(&manifest).unwrap();
    }

    #[test]
    fn rejects_cross_origin_resource() {
        let manifest: PackageManifest = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "package_id": "video_123",
            "package_title": "Video: Example",
            "module": {"id": "video_archiver", "title": "Video Archive", "root_url": "/video-archiver/dashboard"},
            "root_url": "/video-archiver/dashboard",
            "resources": [{"url": "https://attacker.invalid/payload", "type": "binary"}]
        }))
        .unwrap();
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn rejects_path_traversal_package_id() {
        let manifest: PackageManifest = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "package_id": "..",
            "package_title": "Invalid",
            "module": {"id": "video_archiver", "title": "Video Archive", "root_url": "/video-archiver/dashboard"},
            "root_url": "/video-archiver/dashboard",
            "resources": [{"url": "/api/packages/video_123/nsp", "type": "container"}]
        }))
        .unwrap();
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn enforces_standard_package_id_pattern() {
        assert!(validate_package_id("a").is_ok());
        assert!(validate_package_id("A0_-z").is_ok());
        assert!(validate_package_id(&format!("a{}", "b".repeat(127))).is_ok());

        for invalid in ["", "_package", "-package", "package.id"] {
            assert!(validate_package_id(invalid).is_err(), "accepted {invalid}");
        }
        assert!(validate_package_id(&format!("a{}", "b".repeat(128))).is_err());
    }

    #[test]
    fn maps_resource_auth_failures_to_missing_session() {
        assert!(matches!(
            validate_resource_status("/resource", reqwest::StatusCode::UNAUTHORIZED),
            Err(AppError::SessionMissing)
        ));
        assert!(matches!(
            validate_resource_status("/resource", reqwest::StatusCode::FORBIDDEN),
            Err(AppError::SessionMissing)
        ));
        assert!(matches!(
            validate_resource_status("/resource", reqwest::StatusCode::NOT_FOUND),
            Err(AppError::Download(_))
        ));
    }

    #[test]
    fn builds_only_valid_content_addressed_paths() {
        let directory = tempfile::tempdir().unwrap();
        let uppercase = "AB".repeat(32);
        let expected_hash = "ab".repeat(32);
        let path = object_path(directory.path(), &uppercase).unwrap();

        assert_eq!(
            path,
            directory
                .path()
                .join("objects")
                .join("ab")
                .join(&expected_hash)
        );
        assert!(is_object_path(directory.path(), &path));
        assert!(object_path(directory.path(), "../object").is_none());
        assert!(object_path(directory.path(), &"g".repeat(64)).is_none());
        assert!(!is_object_path(
            directory.path(),
            &directory.path().join("objects/ff/not-a-hash")
        ));
    }

    #[test]
    fn reuses_only_regular_objects_with_matching_size() {
        let directory = tempfile::tempdir().unwrap();
        let hash = "ab".repeat(32);
        let path = object_path(directory.path(), &hash).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"data").unwrap();

        let cached = reusable_object(directory.path(), &resource(Some(&hash), Some(4))).unwrap();
        assert_eq!(cached.local_path, path);
        assert_eq!(cached.byte_size, 4);
        assert!(reusable_object(directory.path(), &resource(Some(&hash), Some(3))).is_none());
        assert!(reusable_object(directory.path(), &resource(None, Some(4))).is_none());
    }

    #[test]
    fn hash_without_size_does_not_reuse_object() {
        let directory = tempfile::tempdir().unwrap();
        let hash = "ab".repeat(32);
        let path = object_path(directory.path(), &hash).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"data").unwrap();

        assert!(reusable_object(directory.path(), &resource(Some(&hash), None)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_reuse_symlinked_object() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let hash = "cd".repeat(32);
        let path = object_path(directory.path(), &hash).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"data").unwrap();
        symlink(target, &path).unwrap();

        assert!(reusable_object(directory.path(), &resource(Some(&hash), Some(4))).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_reuse_objects_through_symlinked_store() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let hash = "ef".repeat(32);
        let external_path = external.path().join("ef").join(&hash);
        std::fs::create_dir_all(external_path.parent().unwrap()).unwrap();
        std::fs::write(external_path, b"data").unwrap();
        symlink(external.path(), directory.path().join("objects")).unwrap();

        assert!(reusable_object(directory.path(), &resource(Some(&hash), Some(4))).is_none());
    }

    #[test]
    fn startup_gc_removes_only_unreferenced_objects() {
        let directory = tempfile::tempdir().unwrap();
        let library = Library::open(directory.path()).unwrap();
        let referenced_hash = "12".repeat(32);
        let orphan_hash = "34".repeat(32);
        let referenced_path = object_path(directory.path(), &referenced_hash).unwrap();
        let orphan_path = object_path(directory.path(), &orphan_hash).unwrap();
        std::fs::create_dir_all(referenced_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(orphan_path.parent().unwrap()).unwrap();
        std::fs::write(&referenced_path, b"kept").unwrap();
        std::fs::write(&orphan_path, b"removed").unwrap();
        let resources = [ResourceRecord {
            resource_url: "/kept".into(),
            resource_type: "binary".into(),
            local_path: referenced_path.to_str().unwrap().into(),
            mime_type: None,
            byte_size: 4,
            sha256: referenced_hash,
        }];
        library
            .replace_package(&PackageReplacement {
                node_origin: "https://node.example",
                module_id: "video",
                module_title: "Video",
                module_root_url: "/video",
                package_id: "video_1",
                package_title: "Video",
                root_url: "/video",
                saved_at: "1",
                byte_size: 4,
                resources: &resources,
            })
            .unwrap();

        gc_unreferenced_objects(directory.path(), &library);

        assert!(referenced_path.is_file());
        assert!(!orphan_path.exists());
    }
}
