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
}

struct DownloadContext<'a> {
    app: &'a AppHandle,
    state: &'a AppState,
    session: &'a DownloadSession,
    package_directory: &'a Path,
    package_id: &'a str,
    resource_count: usize,
}

pub fn enqueue(app: &AppHandle, manifest_url: String) -> AppResult<bool> {
    let state = app.state::<AppState>();
    if !state.reserve_download(&manifest_url)? {
        return Ok(false);
    }
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
        let result = run(&app, &manifest_url).await;
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

async fn run(app: &AppHandle, manifest_url: &str) -> AppResult<String> {
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
    let saved_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| AppError::Internal(error.to_string()))?
        .as_secs()
        .to_string();

    state.library().begin_package(
        &node_origin,
        &manifest.module.id,
        &manifest.module.title,
        &manifest.module.root_url,
        &manifest.package_id,
        &manifest.package_title,
        &manifest.root_url,
        &saved_at,
    )?;

    let package_directory =
        package_directory(&session.data_dir, &node_origin, &manifest.package_id);
    tokio::fs::create_dir_all(&package_directory)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    crate::atomic_file::write_async(package_directory.join("manifest.json"), manifest_bytes)
        .await?;

    let result =
        download_resources(app, &session, &node_origin, &manifest, &package_directory).await;
    match result {
        Ok(total_size) => {
            state.library().finish_package(
                &node_origin,
                &manifest.package_id,
                "ready",
                total_size,
            )?;
            Ok(manifest.package_id)
        }
        Err(error) => {
            let _ = state
                .library()
                .finish_package(&node_origin, &manifest.package_id, "failed", 0);
            let _ = app.emit_to("main", "library-updated", ());
            Err(error)
        }
    }
}

async fn download_resources(
    app: &AppHandle,
    session: &DownloadSession,
    node_origin: &str,
    manifest: &PackageManifest,
    package_directory: &Path,
) -> AppResult<u64> {
    let state = app.state::<AppState>();
    let mut total_size = 0_u64;
    let resource_count = manifest.resources.len();
    let context = DownloadContext {
        app,
        state: &state,
        session,
        package_directory,
        package_id: &manifest.package_id,
        resource_count,
    };
    for (index, resource) in manifest.resources.iter().enumerate() {
        emit(
            app,
            DownloadEvent {
                package_id: Some(manifest.package_id.clone()),
                status: "downloading",
                progress: index as f64 / resource_count.max(1) as f64,
                message: format!("Downloading {}", resource.url),
            },
        );
        let downloaded = download_resource(&context, resource, index).await?;
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
        state.library().record_resource(
            node_origin,
            &manifest.package_id,
            &resource.url,
            &resource.resource_type,
            local_path,
            downloaded.mime_type.as_deref(),
            downloaded.byte_size,
            &downloaded.sha256,
        )?;
    }
    Ok(total_size)
}

async fn download_resource(
    context: &DownloadContext<'_>,
    resource: &ResourceManifest,
    resource_index: usize,
) -> AppResult<DownloadedResource> {
    let app = context.app;
    let state = context.state;
    let session = context.session;
    let package_directory = context.package_directory;
    let package_id = context.package_id;
    let resource_count = context.resource_count;
    if !state.session_is_current(session.generation) {
        return Err(AppError::SessionMissing);
    }
    let response = state
        .node_client()
        .get_authenticated(&session.node_url, &resource.url, &session.credential)
        .await?;
    if !response.status().is_success() {
        return Err(AppError::Download(format!(
            "{} returned HTTP {}",
            resource.url,
            response.status()
        )));
    }
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
    let temporary_path =
        package_directory.join(format!("resource-{}.part", digest_hex(&resource.url)));
    let mut file = tokio::fs::File::create(&temporary_path)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
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
            .map_err(|error| AppError::Storage(error.to_string()))?;
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
        .map_err(|error| AppError::Storage(error.to_string()))?;
    file.sync_all()
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
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
    let object_directory = session.data_dir.join("objects").join(&sha256[..2]);
    tokio::fs::create_dir_all(&object_directory)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let local_path = object_directory.join(&sha256);
    if local_path.exists() {
        tokio::fs::remove_file(&temporary_path)
            .await
            .map_err(|error| AppError::Storage(error.to_string()))?;
    } else {
        tokio::fs::rename(&temporary_path, &local_path)
            .await
            .map_err(|error| AppError::Storage(error.to_string()))?;
    }
    Ok(DownloadedResource {
        local_path,
        mime_type,
        byte_size,
        sha256,
    })
}

fn validate_manifest(manifest: &PackageManifest) -> AppResult<()> {
    if manifest.schema_version != 1 {
        return Err(AppError::InvalidPackage(
            "unsupported schema_version".into(),
        ));
    }
    validate_id(&manifest.package_id, "package_id")?;
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
    use super::{PackageManifest, validate_manifest};

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
}
