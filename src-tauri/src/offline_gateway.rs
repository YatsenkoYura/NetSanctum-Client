use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, Response, StatusCode, header};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::RwLock;
use tokio_util::io::ReaderStream;
use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};
use crate::library::{Library, StoredResource};
use crate::nsp::{NspEntry, NspIndex};
use crate::sealed::{self, PackageKey};

pub const SESSION_COOKIE: &str = "netsanctum_offline";
const MAX_MERGED_JSON_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone)]
struct SealedSessionKey {
    package_id: String,
    key: PackageKey,
}

#[derive(Clone)]
struct OfflineSession {
    token: String,
    node_origin: String,
    package_ids: Vec<String>,
    /// Packages that open only sealed. Stable across idle locks: after the
    /// keys are dropped a sealed URL is a 403, never a ciphertext fallback.
    sealed_package_ids: Vec<String>,
    sealed_keys: Vec<SealedSessionKey>,
    /// Sliding idle deadline for the sealed keys: every sealed serve pushes
    /// it forward, silence past it drops the keys. `None` means no sealed
    /// keys are held (or they were locked already).
    sealed_expires_at: Option<std::time::Instant>,
}

/// Fifteen minutes without a sealed request, and the DEKs forget themselves.
const SEALED_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

struct Runtime {
    data_dir: PathBuf,
    library: Arc<Library>,
    session: RwLock<Option<OfflineSession>>,
    nsp_cache: RwLock<HashMap<String, Arc<NspIndex>>>,
}

pub struct OfflineAccess {
    pub origin: String,
    pub token: String,
}

pub struct OfflineGateway {
    origin: String,
    runtime: Arc<Runtime>,
}

impl OfflineGateway {
    pub async fn start(library: Arc<Library>, data_dir: &Path) -> AppResult<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| AppError::Internal(error.to_string()))?;
        let address = listener
            .local_addr()
            .map_err(|error| AppError::Internal(error.to_string()))?;
        let runtime = Arc::new(Runtime {
            data_dir: data_dir.to_owned(),
            library,
            session: RwLock::new(None),
            nsp_cache: RwLock::new(HashMap::new()),
        });
        let router = Router::new().fallback(handle).with_state(runtime.clone());
        tauri::async_runtime::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                eprintln!("[offline-gateway] server stopped: {error}");
            }
        });
        Ok(Self {
            origin: format!("http://127.0.0.1:{}", address.port()),
            runtime,
        })
    }

    pub async fn activate(
        &self,
        node_origin: String,
        package_id: String,
    ) -> AppResult<OfflineAccess> {
        let token = random_token()?;
        *self.runtime.session.write().await = Some(OfflineSession {
            token: token.clone(),
            node_origin,
            package_ids: vec![package_id],
            sealed_package_ids: Vec::new(),
            sealed_keys: Vec::new(),
            sealed_expires_at: None,
        });
        Ok(OfflineAccess {
            origin: self.origin.clone(),
            token,
        })
    }

    /// Activate one sealed package: unwrap its DEK now, hold it in RAM for
    /// this session only, and zeroize the passphrase on return. Every sealed
    /// byte served afterwards is decrypted per request under a per-resource
    /// subkey; nothing decrypted is ever written back to disk.
    pub async fn activate_sealed(
        &self,
        node_origin: String,
        package_id: String,
        passphrase: String,
    ) -> AppResult<OfflineAccess> {
        let passphrase = Zeroizing::new(passphrase);
        let key = sealed_key_for(
            &self.runtime.data_dir,
            &node_origin,
            &package_id,
            passphrase.as_str(),
        )
        .await?;
        let token = random_token()?;
        *self.runtime.session.write().await = Some(OfflineSession {
            token: token.clone(),
            node_origin,
            package_ids: vec![package_id.clone()],
            sealed_package_ids: vec![package_id.clone()],
            sealed_keys: vec![SealedSessionKey { package_id, key }],
            sealed_expires_at: Some(std::time::Instant::now() + SEALED_IDLE_TIMEOUT),
        });
        Ok(OfflineAccess {
            origin: self.origin.clone(),
            token,
        })
    }

    pub async fn activate_module(
        &self,
        node_origin: String,
        package_ids: Vec<String>,
    ) -> AppResult<OfflineAccess> {
        if package_ids.is_empty() {
            return Err(AppError::InvalidPackage("module has no packages".into()));
        }
        let token = random_token()?;
        *self.runtime.session.write().await = Some(OfflineSession {
            token: token.clone(),
            node_origin,
            package_ids,
            sealed_package_ids: Vec::new(),
            sealed_keys: Vec::new(),
            sealed_expires_at: None,
        });
        Ok(OfflineAccess {
            origin: self.origin.clone(),
            token,
        })
    }

    /// Activate a module whose packages may be sealed. `phrases` carries one
    /// passphrase per sealed package id; plain packages need none. A sealed
    /// package without its phrase is refused here — the gateway never serves
    /// a sealed byte without the session holding its unwrapped DEK.
    pub async fn activate_module_sealed(
        &self,
        node_origin: String,
        package_ids: Vec<String>,
        phrases: &[(String, String)],
    ) -> AppResult<OfflineAccess> {
        if package_ids.is_empty() {
            return Err(AppError::InvalidPackage("module has no packages".into()));
        }
        let mut sealed_keys = Vec::new();
        for package_id in &package_ids {
            let manifest = crate::downloads::read_package_manifest(
                &self.runtime.data_dir,
                &node_origin,
                package_id,
            )
            .await?;
            if manifest.get("sealing").is_none() {
                continue;
            }
            let phrase = phrases
                .iter()
                .find(|(id, _)| id == package_id)
                .map(|(_, phrase)| phrase.as_str())
                .ok_or_else(|| {
                    AppError::InvalidPackage(format!(
                        "sealed package {package_id} needs its passphrase"
                    ))
                })?;
            let key =
                sealed_key_for(&self.runtime.data_dir, &node_origin, package_id, phrase).await?;
            sealed_keys.push(SealedSessionKey {
                package_id: package_id.clone(),
                key,
            });
        }
        let token = random_token()?;
        let sealed_package_ids = sealed_keys
            .iter()
            .map(|sealed| sealed.package_id.clone())
            .collect();
        let sealed_expires_at =
            (!sealed_keys.is_empty()).then(|| std::time::Instant::now() + SEALED_IDLE_TIMEOUT);
        *self.runtime.session.write().await = Some(OfflineSession {
            token: token.clone(),
            node_origin,
            package_ids,
            sealed_package_ids,
            sealed_keys,
            sealed_expires_at,
        });
        Ok(OfflineAccess {
            origin: self.origin.clone(),
            token,
        })
    }

    /// Drop the session's sealed keys now: the packages stay listed, but
    /// every sealed URL answers 403 until the passphrase unwraps them again.
    /// Plain packages are unaffected.
    pub async fn lock_sealed(&self) {
        let mut session = self.runtime.session.write().await;
        if let Some(active) = session.as_mut() {
            active.sealed_keys.clear();
            active.sealed_expires_at = None;
        }
    }
}

/// Unwrap one stored package's DEK. The manifest on disk holds the `sealing`
/// descriptor (ciphertext + public costs); the passphrase arrives only as a
/// `&str` and never outlives this call.
async fn sealed_key_for(
    data_dir: &Path,
    node_origin: &str,
    package_id: &str,
    passphrase: &str,
) -> AppResult<PackageKey> {
    let manifest =
        crate::downloads::read_package_manifest(data_dir, node_origin, package_id).await?;
    let stored_id = manifest
        .get("package_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if stored_id != package_id {
        return Err(AppError::InvalidPackage(
            "stored manifest does not match the package".into(),
        ));
    }
    let descriptor = manifest
        .get("sealing")
        .ok_or_else(|| AppError::InvalidPackage("package is not sealed".into()))?;
    let sealing = sealed::parse_sealing(descriptor)?;
    sealed::unwrap_package_dek(&sealing, passphrase, package_id)
}

async fn handle(State(runtime): State<Arc<Runtime>>, request: Request<Body>) -> Response<Body> {
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return plain(
            StatusCode::METHOD_NOT_ALLOWED,
            "Offline packages are read-only",
        );
    }
    let session = match authorized_session(&runtime, request.headers()).await {
        Some(session) => session,
        None => return plain(StatusCode::FORBIDDEN, "Invalid offline session"),
    };
    let session = refresh_sealed_session(&runtime, session).await;
    let resource_url = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(request.uri().path());
    let range = request
        .headers()
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());

    match resolve_resource(&runtime, &session, resource_url).await {
        Ok(Some(segment)) => {
            if matches!(segment, Segment::Sealed { .. }) {
                return match serve_sealed(segment, range, method == Method::HEAD).await {
                    Ok(response) => response,
                    Err(AppError::SealedUnlock) => {
                        plain(StatusCode::FORBIDDEN, "Cannot open this sealed resource")
                    }
                    Err(AppError::RangeNotSatisfiable) => plain(
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        "Range is not satisfiable",
                    ),
                    Err(error) => plain(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
                };
            }
            match serve_segment(segment, range, method == Method::HEAD).await {
                Ok(response) => response,
                Err(AppError::RangeNotSatisfiable) => plain(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "Range is not satisfiable",
                ),
                Err(error) => plain(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
            }
        }
        Ok(None) => plain(
            StatusCode::NOT_FOUND,
            "Resource is not part of this offline package",
        ),
        Err(AppError::SealedUnlock) => {
            plain(StatusCode::FORBIDDEN, "Cannot open this sealed resource")
        }
        Err(error) => plain(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

/// Enforce the sealed idle lock: silence past the deadline drops the keys,
/// and sealed URLs then refuse — they never fall back to serving ciphertext.
/// The deadline itself slides only on sealed serves (see `resolve_resource`),
/// so browsing plain packages does not keep sealed keys alive.
async fn refresh_sealed_session(runtime: &Runtime, session: OfflineSession) -> OfflineSession {
    if session.sealed_keys.is_empty() {
        return session;
    }
    let expired = session
        .sealed_expires_at
        .is_none_or(|deadline| std::time::Instant::now() >= deadline);
    if !expired {
        return session;
    }
    let mut stored = runtime.session.write().await;
    if let Some(active) = stored
        .as_mut()
        .filter(|active| active.token == session.token)
    {
        active.sealed_keys.clear();
        active.sealed_expires_at = None;
        active.clone()
    } else {
        session
    }
}

async fn authorized_session(
    runtime: &Runtime,
    headers: &axum::http::HeaderMap,
) -> Option<OfflineSession> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    let token = cookie_header.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == SESSION_COOKIE).then_some(value)
    })?;
    let session = runtime.session.read().await;
    session
        .as_ref()
        .filter(|session| session.token == token)
        .cloned()
}

enum Segment {
    File(StoredResource),
    Nsp(NspEntry),
    Memory(Vec<u8>, String),
    /// Ciphertext on disk plus the session's unwrapped package key. The `url`
    /// is the exact request string — never the `nsp.rs` normalized form: the
    /// resource subkey and every AAD bind the exact manifest spelling.
    Sealed {
        local_path: String,
        package_id: String,
        url: String,
        key: PackageKey,
        mime: String,
    },
}

async fn resolve_resource(
    runtime: &Runtime,
    session: &OfflineSession,
    resource_url: &str,
) -> AppResult<Option<Segment>> {
    for package_id in &session.package_ids {
        if let Some(resource) =
            runtime
                .library
                .resource(&session.node_origin, package_id, resource_url)?
        {
            if tokio::fs::metadata(&resource.local_path)
                .await
                .is_ok_and(|metadata| metadata.is_file())
            {
                // A package unlocked sealed in this session decrypts on every
                // serve. The lookup above is an exact string match, so the URL
                // doubling as key info is byte-identical to the manifest one.
                if session.sealed_package_ids.iter().any(|id| id == package_id) {
                    let Some(sealed) = session
                        .sealed_keys
                        .iter()
                        .find(|sealed| &sealed.package_id == package_id)
                    else {
                        // Locked or expired: refuse. Serving the stored bytes
                        // as a plain file here would hand out ciphertext, and
                        // failing open is exactly how sealed systems die.
                        return Err(AppError::SealedUnlock);
                    };
                    // Sealed activity slides the idle deadline.
                    if let Some(active) = runtime
                        .session
                        .write()
                        .await
                        .as_mut()
                        .filter(|active| active.token == session.token)
                    {
                        active.sealed_expires_at =
                            Some(std::time::Instant::now() + SEALED_IDLE_TIMEOUT);
                    }
                    return Ok(Some(Segment::Sealed {
                        local_path: resource.local_path,
                        package_id: package_id.clone(),
                        url: resource_url.to_owned(),
                        key: sealed.key.clone(),
                        mime: resource
                            .mime_type
                            .unwrap_or_else(|| "application/octet-stream".into()),
                    }));
                }
                return Ok(Some(Segment::File(resource)));
            }
        }
    }
    let mut entries = Vec::new();
    for package_id in &session.package_ids {
        let Some(container) = runtime
            .library
            .container_resource(&session.node_origin, package_id)?
        else {
            continue;
        };
        if !tokio::fs::metadata(&container.local_path)
            .await
            .is_ok_and(|metadata| metadata.is_file())
        {
            continue;
        }
        let index = nsp_index(runtime, &container.local_path).await?;
        if let Some(entry) = index
            .get(resource_url)
            .or_else(|| index.find_equivalent(resource_url))
        {
            entries.push(entry);
        }
    }
    if entries.len() > 1
        && entries
            .iter()
            .all(|entry| entry.mime.starts_with("application/json"))
        && let Some(bytes) = merge_json_arrays(&entries).await?
    {
        return Ok(Some(Segment::Memory(
            bytes,
            "application/json; charset=utf-8".into(),
        )));
    }
    Ok(entries.into_iter().next().map(Segment::Nsp))
}

async fn merge_json_arrays(entries: &[NspEntry]) -> AppResult<Option<Vec<u8>>> {
    let mut merged = Vec::<serde_json::Value>::new();
    let mut seen = std::collections::HashSet::new();
    let mut input_size = 0_u64;
    for entry in entries {
        input_size = input_size
            .checked_add(entry.length)
            .ok_or_else(|| AppError::InvalidPackage("offline JSON size overflow".into()))?;
        if input_size > MAX_MERGED_JSON_BYTES {
            return Err(AppError::InvalidPackage(
                "offline JSON aggregate exceeds the memory safety limit".into(),
            ));
        }
        let bytes = read_nsp_entry(entry).await?;
        let Ok(values) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) else {
            return Ok(None);
        };
        for value in values {
            let identity = value
                .get("id")
                .map(ToString::to_string)
                .unwrap_or_else(|| value.to_string());
            if seen.insert(identity) {
                merged.push(value);
            }
        }
    }
    serde_json::to_vec(&merged)
        .map(Some)
        .map_err(|error| AppError::Storage(error.to_string()))
}

async fn read_nsp_entry(entry: &NspEntry) -> AppResult<Vec<u8>> {
    let mut file = tokio::fs::File::open(&entry.path)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    file.seek(std::io::SeekFrom::Start(entry.offset))
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let length = usize::try_from(entry.length)
        .map_err(|_| AppError::Storage("NSP entry is too large".into()))?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    Ok(bytes)
}

async fn nsp_index(runtime: &Runtime, path: &str) -> AppResult<Arc<NspIndex>> {
    if let Some(index) = runtime.nsp_cache.read().await.get(path).cloned() {
        return Ok(index);
    }
    let index = Arc::new(NspIndex::open(Path::new(path)).await?);
    runtime
        .nsp_cache
        .write()
        .await
        .insert(path.to_owned(), index.clone());
    Ok(index)
}

async fn serve_segment(
    segment: Segment,
    range: Option<&str>,
    head: bool,
) -> AppResult<Response<Body>> {
    let (path, base_offset, full_length, mime) = match segment {
        Segment::File(resource) => (
            resource.local_path,
            0,
            resource.byte_size,
            resource
                .mime_type
                .unwrap_or_else(|| "application/octet-stream".into()),
        ),
        Segment::Nsp(entry) => (
            entry.path.to_string_lossy().into_owned(),
            entry.offset,
            entry.length,
            entry.mime,
        ),
        Segment::Memory(bytes, mime) => {
            let length = bytes.len();
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_LENGTH, length)
                .header(header::CACHE_CONTROL, "no-store")
                .header("X-Content-Type-Options", "nosniff")
                .header("Content-Security-Policy", offline_csp())
                .body(if head {
                    Body::empty()
                } else {
                    Body::from(bytes)
                })
                .map_err(|error| AppError::Internal(error.to_string()));
        }
        // Sealed segments never reach the plain file path: `handle` routes
        // them to `serve_sealed` first, so raw ciphertext can never be served.
        Segment::Sealed { .. } => {
            return Err(AppError::Internal(
                "sealed segment reached plain serving".into(),
            ));
        }
    };
    if full_length == 0 {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CONTENT_LENGTH, 0)
            .header("Content-Security-Policy", offline_csp())
            .header("X-Content-Type-Options", "nosniff")
            .body(Body::empty())
            .map_err(|error| AppError::Internal(error.to_string()));
    }
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let (start, end, partial) = parse_range(range, full_length)?;
    let length = end - start + 1;
    file.seek(std::io::SeekFrom::Start(base_offset + start))
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let mut builder = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, length)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", offline_csp());
    if partial {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{full_length}"),
        );
    }
    let body = if head {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file.take(length)))
    };
    builder
        .body(body)
        .map_err(|error| AppError::Internal(error.to_string()))
}

/// Serve one sealed resource: decrypt-on-read, in RAM, never via temp files.
/// A chunked envelope streams tag-verified chunks straight into the response
/// body, so a Range into a gigabyte video costs one or two chunks; a small
/// envelope (the items snapshot) opens whole under its ~32 MiB ceiling.
async fn serve_sealed(
    segment: Segment,
    range: Option<&str>,
    head: bool,
) -> AppResult<Response<Body>> {
    let Segment::Sealed {
        local_path,
        package_id,
        url,
        key,
        mime,
    } = segment
    else {
        return Err(AppError::Internal(
            "plain segment reached sealed serving".into(),
        ));
    };
    let path = Path::new(&local_path);
    let mut probe = tokio::fs::File::open(path)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let mut magic = [0_u8; sealed::SEALED_MAGIC.len()];
    probe
        .read_exact(&mut magic)
        .await
        .map_err(|_| AppError::SealedUnlock)?;
    let resource_key = sealed::resource_key(&key, &package_id, &url)?;
    if magic == *sealed::SEALED_MAGIC {
        serve_sealed_chunked(path, &url, &resource_key, mime, range, head).await
    } else {
        serve_sealed_small(path, &url, &resource_key, mime, range, head).await
    }
}

async fn serve_sealed_small(
    path: &Path,
    url: &str,
    resource_key: &[u8; sealed::KEY_BYTES],
    mime: String,
    range: Option<&str>,
    head: bool,
) -> AppResult<Response<Body>> {
    let on_disk = tokio::fs::metadata(path)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?
        .len();
    if on_disk > sealed::MAX_SMALL_BLOB_BYTES {
        return Err(AppError::InvalidPackage(
            "sealed snapshot exceeds the offline ceiling".into(),
        ));
    }
    let blob = tokio::fs::read(path)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let plaintext = sealed::open_small(resource_key, url, &blob)?;
    let plain_len = plaintext.len() as u64;
    let (start, end, partial) = parse_range(range, plain_len)?;
    let body = plaintext[start as usize..=end as usize].to_vec();
    sealed_response(
        &mime,
        plain_len,
        start,
        end,
        partial,
        head,
        Body::from(body),
    )
}

async fn serve_sealed_chunked(
    path: &Path,
    url: &str,
    resource_key: &[u8; sealed::KEY_BYTES],
    mime: String,
    range: Option<&str>,
    head: bool,
) -> AppResult<Response<Body>> {
    let header_bytes = tokio::fs::read(path)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let header = sealed::parse_chunked_header(&header_bytes)?;
    let on_disk = header_bytes.len() as u64;
    if on_disk != sealed::promised_ciphertext_len(&header)? {
        return Err(AppError::SealedUnlock);
    }
    let (start, end, partial) = parse_range(range, header.plain_len)?;
    if head {
        return sealed_response(
            &mime,
            header.plain_len,
            start,
            end,
            partial,
            true,
            Body::empty(),
        );
    }
    if header.plain_len == 0 {
        return sealed_response(&mime, 0, 0, 0, false, false, Body::empty());
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let stream = sealed_chunk_stream(file, url.to_owned(), *resource_key, header, start, end);
    sealed_response(
        &mime,
        header.plain_len,
        start,
        end,
        partial,
        false,
        Body::from_stream(stream),
    )
}

fn sealed_response(
    mime: &str,
    plain_len: u64,
    start: u64,
    end: u64,
    partial: bool,
    head: bool,
    body: Body,
) -> AppResult<Response<Body>> {
    let length = if plain_len == 0 { 0 } else { end - start + 1 };
    let mut builder = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime.to_owned())
        .header(header::CONTENT_LENGTH, length)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", offline_csp());
    if partial {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{plain_len}"),
        );
    }
    builder
        .body(if head { Body::empty() } else { body })
        .map_err(|error| AppError::Internal(error.to_string()))
}

struct SealedStreamState {
    file: tokio::fs::File,
    header: sealed::ChunkHeader,
    key: [u8; sealed::KEY_BYTES],
    url: String,
    index: u32,
    position: u64,
    start: u64,
    wanted_end: u64,
}

/// Stream tag-verified plaintext chunks of `[start, end]` from a sealed file.
/// One chunk in flight at a time; a tag failure aborts the body mid-stream,
/// and the short Content-Length tells the client the bytes are incomplete.
fn sealed_chunk_stream(
    file: tokio::fs::File,
    url: String,
    key: [u8; sealed::KEY_BYTES],
    header: sealed::ChunkHeader,
    start: u64,
    end: u64,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    use futures_util::stream::unfold;
    let chunk_size = u64::from(header.chunk_size);
    unfold(
        Some(SealedStreamState {
            file,
            header,
            key,
            url,
            index: u32::try_from(start / chunk_size).unwrap_or(u32::MAX),
            position: start / chunk_size * chunk_size,
            start,
            wanted_end: end + 1,
        }),
        |state| async move {
            let mut current = state?;
            if current.index == u32::MAX {
                return Some((
                    Err(std::io::Error::other("sealed range starts past the end")),
                    None,
                ));
            }
            match next_sealed_piece(&mut current).await {
                Ok(None) => None,
                Ok(Some(piece)) => Some((Ok(piece), Some(current))),
                Err(error) => Some((Err(std::io::Error::other(error.to_string())), None)),
            }
        },
    )
}

async fn next_sealed_piece(state: &mut SealedStreamState) -> AppResult<Option<Vec<u8>>> {
    if state.position >= state.wanted_end {
        return Ok(None);
    }
    let chunk_size = u64::from(state.header.chunk_size);
    let is_last = (u64::from(state.index) + 1) * chunk_size >= state.header.plain_len;
    let chunk_plain = if is_last {
        state.header.plain_len - u64::from(state.index) * chunk_size
    } else {
        chunk_size
    };
    state
        .file
        .seek(std::io::SeekFrom::Start(
            sealed::CHUNKED_HEADER_SIZE as u64
                + u64::from(state.index) * (chunk_size + sealed::TAG_BYTES as u64),
        ))
        .await
        .map_err(|_| AppError::SealedUnlock)?;
    let mut stored = vec![0_u8; (chunk_plain + sealed::TAG_BYTES as u64) as usize];
    state
        .file
        .read_exact(&mut stored)
        .await
        .map_err(|_| AppError::SealedUnlock)?;
    let plaintext =
        sealed::open_stored_chunk(&state.key, &state.url, &state.header, state.index, &stored)?;
    let trim = state.start.saturating_sub(state.position) as usize;
    let keep = (state.wanted_end - state.position.max(state.start)) as usize;
    let piece = plaintext[trim..trim + keep.min(plaintext.len() - trim)].to_vec();
    state.position += chunk_plain;
    state.index = state.index.checked_add(1).ok_or(AppError::SealedUnlock)?;
    Ok(Some(piece))
}

fn parse_range(range: Option<&str>, length: u64) -> AppResult<(u64, u64, bool)> {
    if length == 0 {
        return Ok((0, 0, false));
    }
    let Some(range) = range else {
        return Ok((0, length - 1, false));
    };
    let value = range
        .strip_prefix("bytes=")
        .ok_or(AppError::RangeNotSatisfiable)?;
    if value.contains(',') {
        return Err(AppError::RangeNotSatisfiable);
    }
    let (start, end) = value.split_once('-').ok_or(AppError::RangeNotSatisfiable)?;
    let (start, end) = if start.is_empty() {
        let suffix = end
            .parse::<u64>()
            .map_err(|_| AppError::RangeNotSatisfiable)?;
        let suffix = suffix.min(length);
        (length - suffix, length - 1)
    } else {
        let start = start
            .parse::<u64>()
            .map_err(|_| AppError::RangeNotSatisfiable)?;
        let end = if end.is_empty() {
            length - 1
        } else {
            end.parse::<u64>()
                .map_err(|_| AppError::RangeNotSatisfiable)?
                .min(length - 1)
        };
        (start, end)
    };
    if start >= length || start > end {
        return Err(AppError::RangeNotSatisfiable);
    }
    Ok((start, end, true))
}

fn random_token() -> AppResult<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| AppError::Internal(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn plain(status: StatusCode, message: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("Content-Security-Policy", offline_csp())
        .body(Body::from(message.to_owned()))
        .expect("static response is valid")
}

fn offline_csp() -> &'static str {
    "default-src 'self' data: blob:; connect-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; font-src 'self' data:; object-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'self'"
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use tokio::sync::RwLock;

    use super::{
        OfflineSession, Runtime, SealedSessionKey, Segment, parse_range, resolve_resource,
        serve_sealed,
    };
    use crate::library::{Library, PackageReplacement, ResourceRecord};

    #[test]
    fn parses_media_ranges() {
        assert_eq!((0, 99, false), parse_range(None, 100).unwrap());
        assert_eq!(
            (10, 19, true),
            parse_range(Some("bytes=10-19"), 100).unwrap()
        );
        assert_eq!((90, 99, true), parse_range(Some("bytes=-10"), 100).unwrap());
        assert_eq!((50, 99, true), parse_range(Some("bytes=50-"), 100).unwrap());
    }

    #[test]
    fn skips_missing_resource_and_uses_next_package() {
        let directory = tempfile::tempdir().unwrap();
        let library = Arc::new(Library::open(directory.path()).unwrap());
        let missing = directory.path().join("missing.bin");
        let available = directory.path().join("available.bin");
        std::fs::write(&available, b"media").unwrap();
        for (package_id, path) in [("missing", &missing), ("available", &available)] {
            let resources = [ResourceRecord {
                resource_url: "/media/file".into(),
                resource_type: "file".into(),
                local_path: path.to_str().unwrap().into(),
                mime_type: Some("audio/mpeg".into()),
                byte_size: 5,
                sha256: package_id.into(),
            }];
            library
                .replace_package(&PackageReplacement {
                    node_origin: "https://node.example",
                    module_id: "media",
                    module_title: "Media",
                    module_root_url: "/media",
                    package_id,
                    package_title: package_id,
                    root_url: "/media",
                    saved_at: "1",
                    byte_size: 5,
                    resources: &resources,
                })
                .unwrap();
        }
        let runtime = Runtime {
            data_dir: directory.path().to_owned(),
            library,
            session: RwLock::new(None),
            nsp_cache: RwLock::new(HashMap::new()),
        };
        let session = OfflineSession {
            token: "token".into(),
            node_origin: "https://node.example".into(),
            package_ids: vec!["missing".into(), "available".into()],
            sealed_package_ids: Vec::new(),
            sealed_keys: Vec::new(),
            sealed_expires_at: None,
        };

        let segment =
            tauri::async_runtime::block_on(resolve_resource(&runtime, &session, "/media/file"))
                .unwrap()
                .unwrap();
        match segment {
            Segment::File(resource) => assert_eq!(resource.local_path, available.to_str().unwrap()),
            _ => panic!("expected direct file resource"),
        }
    }

    fn decode_hex(even_hex: &str) -> Vec<u8> {
        (0..even_hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&even_hex[index..index + 2], 16).unwrap())
            .collect()
    }

    /// End to end on node-generated vectors: ciphertext files on disk resolve
    /// through the library, decrypt per request under the session key, and
    /// serve byte-exact plaintext with correct 200/206 semantics.
    #[tokio::test]
    async fn serves_sealed_snapshot_and_chunked_ranges() {
        use sha2::{Digest, Sha256};

        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("sealed/vectors.json")).unwrap();
        let package_id = vectors.get("package_id").unwrap().as_str().unwrap();
        let wrap = vectors.get("wrap").unwrap();
        let sealing = crate::sealed::parse_sealing(wrap.get("fragment").unwrap()).unwrap();
        let key = crate::sealed::unwrap_package_dek(
            &sealing,
            wrap.get("passphrase").unwrap().as_str().unwrap(),
            package_id,
        )
        .unwrap();

        let directory = tempfile::tempdir().unwrap();
        let small = vectors.get("small").unwrap();
        let chunk = vectors.get("chunk").unwrap();
        let small_url = small.get("url").unwrap().as_str().unwrap();
        let chunk_url = chunk.get("url").unwrap().as_str().unwrap();
        let small_blob = decode_hex(small.get("blob_hex").unwrap().as_str().unwrap());
        let chunk_blob = decode_hex(chunk.get("blob_hex").unwrap().as_str().unwrap());
        let small_path = directory.path().join("items.sealed");
        let chunk_path = directory.path().join("media.sealed");
        std::fs::write(&small_path, &small_blob).unwrap();
        std::fs::write(&chunk_path, &chunk_blob).unwrap();
        let record = |url: &str, path: &std::path::Path, bytes: &[u8], mime: &str| ResourceRecord {
            resource_url: url.into(),
            resource_type: "sealed".into(),
            local_path: path.to_str().unwrap().into(),
            mime_type: Some(mime.into()),
            byte_size: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        };
        let library = Arc::new(Library::open(directory.path()).unwrap());
        library
            .replace_package(&PackageReplacement {
                node_origin: "https://node.example",
                module_id: "vault",
                module_title: "Vault",
                module_root_url: small_url,
                package_id,
                package_title: "Vault (sealed)",
                root_url: small_url,
                saved_at: "1",
                byte_size: (small_blob.len() + chunk_blob.len()) as u64,
                resources: &[
                    record(small_url, &small_path, &small_blob, "application/json"),
                    record(chunk_url, &chunk_path, &chunk_blob, "video/mp4"),
                ],
            })
            .unwrap();

        let runtime = Runtime {
            data_dir: directory.path().to_owned(),
            library,
            session: RwLock::new(None),
            nsp_cache: RwLock::new(HashMap::new()),
        };
        let session = OfflineSession {
            token: "token".into(),
            node_origin: "https://node.example".into(),
            package_ids: vec![package_id.into()],
            sealed_package_ids: vec![package_id.into()],
            sealed_keys: vec![SealedSessionKey {
                package_id: package_id.into(),
                key,
            }],
            sealed_expires_at: None,
        };

        // The snapshot opens whole and serves as JSON with a 200.
        let segment = resolve_resource(&runtime, &session, small_url)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(segment, Segment::Sealed { .. }));
        let response = serve_sealed(segment, None, false).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            body.to_vec(),
            decode_hex(small.get("plaintext_hex").unwrap().as_str().unwrap())
        );

        // A media range serves 206 with an exact Content-Range and byte-exact
        // plaintext, decrypted from the covered chunks only.
        let segment = resolve_resource(&runtime, &session, chunk_url)
            .await
            .unwrap()
            .unwrap();
        let response = serve_sealed(segment, Some("bytes=4000-6999"), false)
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::PARTIAL_CONTENT);
        let plain_len = chunk.get("plain_len").unwrap().as_u64().unwrap();
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_RANGE)
                .unwrap(),
            &format!("bytes 4000-6999/{plain_len}")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            body.to_vec(),
            decode_hex(chunk.get("range_hex").unwrap().as_str().unwrap())
        );

        // A bit flip on disk aborts the streamed body mid-flight: the serve
        // call itself is lazy, so the tag failure surfaces as a body error
        // and the client sees a short Content-Length, never short bytes.
        let mut tampered = chunk_blob.clone();
        tampered[100] ^= 1;
        std::fs::write(&chunk_path, &tampered).unwrap();
        let segment = resolve_resource(&runtime, &session, chunk_url)
            .await
            .unwrap()
            .unwrap();
        let response = serve_sealed(segment, None, false).await.unwrap();
        assert!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .is_err()
        );

        // The small envelope opens eagerly, so its tamper fails at serve.
        let mut tampered_small = small_blob.clone();
        tampered_small[20] ^= 1;
        std::fs::write(&small_path, &tampered_small).unwrap();
        let segment = resolve_resource(&runtime, &session, small_url)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            serve_sealed(segment, None, false).await,
            Err(crate::error::AppError::SealedUnlock)
        ));
    }

    /// The full HTTP path: a stored manifest plus ciphertext objects activate
    /// with the passphrase and answer gateway requests — 200 JSON for the
    /// snapshot, 206 bytes for a media range, 403 for a wrong phrase.
    #[tokio::test]
    async fn sealed_package_serves_over_http() {
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::{Method, Request};

        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("sealed/vectors.json")).unwrap();
        let package_id = vectors.get("package_id").unwrap().as_str().unwrap();
        let wrap = vectors.get("wrap").unwrap();
        let passphrase = wrap.get("passphrase").unwrap().as_str().unwrap();
        let small = vectors.get("small").unwrap();
        let chunk = vectors.get("chunk").unwrap();
        let small_url = small.get("url").unwrap().as_str().unwrap();
        let chunk_url = chunk.get("url").unwrap().as_str().unwrap();

        let directory = tempfile::tempdir().unwrap();
        let node_origin = "https://node.example";
        // Objects exactly as the downloader stores them: content-addressed
        // ciphertext plus the stored manifest with its `sealing` descriptor.
        let store = |url: &str, blob: &[u8], mime: &str| {
            use sha2::{Digest, Sha256};
            let digest = format!("{:x}", Sha256::digest(blob));
            let path = directory
                .path()
                .join("objects")
                .join(&digest[..2])
                .join(&digest);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, blob).unwrap();
            ResourceRecord {
                resource_url: url.into(),
                resource_type: "sealed".into(),
                local_path: path.to_str().unwrap().into(),
                mime_type: Some(mime.into()),
                byte_size: blob.len() as u64,
                sha256: digest,
            }
        };
        let small_blob = decode_hex(small.get("blob_hex").unwrap().as_str().unwrap());
        let chunk_blob = decode_hex(chunk.get("blob_hex").unwrap().as_str().unwrap());
        let library = Arc::new(Library::open(directory.path()).unwrap());
        library
            .replace_package(&PackageReplacement {
                node_origin,
                module_id: "vault",
                module_title: "Vault",
                module_root_url: small_url,
                package_id,
                package_title: "Vault (sealed)",
                root_url: small_url,
                saved_at: "1",
                byte_size: (small_blob.len() + chunk_blob.len()) as u64,
                resources: &[
                    store(small_url, &small_blob, "application/json"),
                    store(chunk_url, &chunk_blob, "video/mp4"),
                ],
            })
            .unwrap();
        let manifest_dir =
            crate::downloads::package_directory(directory.path(), node_origin, package_id);
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "package_id": package_id,
                "package_title": "Vault (sealed)",
                "module": {"id": "vault", "title": "Vault", "root_url": small_url},
                "root_url": small_url,
                "resources": [
                    {"url": small_url, "type": "sealed", "mime": "application/json"},
                    {"url": chunk_url, "type": "sealed", "mime": "video/mp4"},
                ],
                "sealing": wrap.get("fragment").unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();

        let gateway = super::OfflineGateway::start(library, directory.path())
            .await
            .unwrap();
        // A wrong phrase never activates: one indistinguishable error.
        assert!(matches!(
            gateway
                .activate_sealed(node_origin.into(), package_id.into(), "wrong phrase".into(),)
                .await,
            Err(crate::error::AppError::SealedUnlock)
        ));
        let access = gateway
            .activate_sealed(node_origin.into(), package_id.into(), passphrase.to_owned())
            .await
            .unwrap();
        let get = |url: &str, range: Option<&str>| {
            let mut builder = Request::builder()
                .method(Method::GET)
                .uri(format!("{}{}", access.origin, url))
                .header(
                    axum::http::header::COOKIE,
                    format!("{}={}", super::SESSION_COOKIE, access.token),
                );
            if let Some(range) = range {
                builder = builder.header(axum::http::header::RANGE, range);
            }
            builder.body(Body::empty()).unwrap()
        };
        let state = gateway.runtime.clone();

        let response = super::handle(State(state.clone()), get(small_url, None)).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            body.to_vec(),
            decode_hex(small.get("plaintext_hex").unwrap().as_str().unwrap())
        );

        let response =
            super::handle(State(state.clone()), get(chunk_url, Some("bytes=0-99"))).await;
        assert_eq!(response.status(), axum::http::StatusCode::PARTIAL_CONTENT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let sealing = crate::sealed::parse_sealing(wrap.get("fragment").unwrap()).unwrap();
        let dek = crate::sealed::unwrap_package_dek(&sealing, passphrase, package_id).unwrap();
        let resource_key = crate::sealed::resource_key(&dek, package_id, chunk_url).unwrap();
        let expected =
            crate::sealed::open_chunked_range(&resource_key, chunk_url, &chunk_blob, 0, 100)
                .unwrap();
        assert_eq!(body.to_vec(), expected);
    }

    /// Idle expiry and the manual lock both refuse sealed URLs with 403 —
    /// and, critically, never fall back to serving the stored ciphertext.
    #[tokio::test]
    async fn sealed_idle_lock_and_manual_lock_refuse() {
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::{Method, Request};

        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("sealed/vectors.json")).unwrap();
        let package_id = fixture.get("package_id").unwrap().as_str().unwrap();
        let wrap = fixture.get("wrap").unwrap();
        let passphrase = wrap.get("passphrase").unwrap().as_str().unwrap();
        let small = fixture.get("small").unwrap();
        let small_url = small.get("url").unwrap().as_str().unwrap();
        let small_blob = decode_hex(small.get("blob_hex").unwrap().as_str().unwrap());

        let directory = tempfile::tempdir().unwrap();
        let node_origin = "https://node.example";
        let object = directory.path().join("items.sealed");
        std::fs::write(&object, &small_blob).unwrap();
        let library = Arc::new(Library::open(directory.path()).unwrap());
        library
            .replace_package(&PackageReplacement {
                node_origin,
                module_id: "vault",
                module_title: "Vault",
                module_root_url: small_url,
                package_id,
                package_title: "Vault (sealed)",
                root_url: small_url,
                saved_at: "1",
                byte_size: small_blob.len() as u64,
                resources: &[ResourceRecord {
                    resource_url: small_url.into(),
                    resource_type: "sealed".into(),
                    local_path: object.to_str().unwrap().into(),
                    mime_type: Some("application/json".into()),
                    byte_size: small_blob.len() as u64,
                    sha256: "aa".repeat(32),
                }],
            })
            .unwrap();
        let manifest_dir =
            crate::downloads::package_directory(directory.path(), node_origin, package_id);
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "package_id": package_id,
                "package_title": "Vault (sealed)",
                "module": {"id": "vault", "title": "Vault", "root_url": small_url},
                "root_url": small_url,
                "resources": [
                    {"url": small_url, "type": "sealed", "mime": "application/json"},
                ],
                "sealing": wrap.get("fragment").unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();

        let gateway = super::OfflineGateway::start(library, directory.path())
            .await
            .unwrap();
        let access = gateway
            .activate_sealed(node_origin.into(), package_id.into(), passphrase.to_owned())
            .await
            .unwrap();
        let state = gateway.runtime.clone();
        let get = || {
            Request::builder()
                .method(Method::GET)
                .uri(format!("{}{}", access.origin, small_url))
                .header(
                    axum::http::header::COOKIE,
                    format!("{}={}", super::SESSION_COOKIE, access.token),
                )
                .body(Body::empty())
                .unwrap()
        };
        let response = super::handle(State(state.clone()), get()).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // Fifteen silent minutes pass: the next request finds no keys.
        gateway
            .runtime
            .session
            .write()
            .await
            .as_mut()
            .unwrap()
            .sealed_expires_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        let response = super::handle(State(state.clone()), get()).await;
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);

        // Re-unlock, then lock by hand: same refusal, same 403, no ciphertext.
        let access = gateway
            .activate_sealed(node_origin.into(), package_id.into(), passphrase.to_owned())
            .await
            .unwrap();
        let state = gateway.runtime.clone();
        gateway.lock_sealed().await;
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("{}{}", access.origin, small_url))
            .header(
                axum::http::header::COOKIE,
                format!("{}={}", super::SESSION_COOKIE, access.token),
            )
            .body(Body::empty())
            .unwrap();
        let response = super::handle(State(state.clone()), request).await;
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    }

    /// The disk canary from the sealed spec §7: every sealed byte on this box
    /// carries a known marker in its plaintext. After storing, activating and
    /// serving the package — snapshot, full media, ranged media — no file
    /// under the data dir (objects, manifests, sqlite, WAL included) may
    /// contain the marker. Ciphertext rests here; plaintext never does.
    #[tokio::test]
    async fn sealed_plaintext_never_touches_disk() {
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::{Method, Request};

        fn hex_to_bytes(even_hex: &str) -> Vec<u8> {
            (0..even_hex.len())
                .step_by(2)
                .map(|index| u8::from_str_radix(&even_hex[index..index + 2], 16).unwrap())
                .collect()
        }

        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("sealed/canary.json")).unwrap();
        let package_id = fixture.get("package_id").unwrap().as_str().unwrap();
        let passphrase = fixture.get("passphrase").unwrap().as_str().unwrap();
        let canary = fixture.get("canary").unwrap().as_str().unwrap().as_bytes();
        let small = fixture.get("small").unwrap();
        let chunk = fixture.get("chunk").unwrap();

        let directory = tempfile::tempdir().unwrap();
        let node_origin = "https://node.example";
        let mut records = Vec::new();
        for (entry, mime) in [(small, "application/json"), (chunk, "video/mp4")] {
            use sha2::{Digest, Sha256};
            let blob = hex_to_bytes(entry.get("blob_hex").unwrap().as_str().unwrap());
            let digest = format!("{:x}", Sha256::digest(&blob));
            let path = directory
                .path()
                .join("objects")
                .join(&digest[..2])
                .join(&digest);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &blob).unwrap();
            records.push(ResourceRecord {
                resource_url: entry.get("url").unwrap().as_str().unwrap().into(),
                resource_type: "sealed".into(),
                local_path: path.to_str().unwrap().into(),
                mime_type: Some(mime.into()),
                byte_size: blob.len() as u64,
                sha256: digest,
            });
        }
        let library = Arc::new(Library::open(directory.path()).unwrap());
        let small_url = small.get("url").unwrap().as_str().unwrap();
        library
            .replace_package(&PackageReplacement {
                node_origin,
                module_id: "vault",
                module_title: "Vault",
                module_root_url: small_url,
                package_id,
                package_title: "Vault (sealed)",
                root_url: small_url,
                saved_at: "1",
                byte_size: records.iter().map(|record| record.byte_size).sum(),
                resources: &records,
            })
            .unwrap();
        let manifest_dir =
            crate::downloads::package_directory(directory.path(), node_origin, package_id);
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "package_id": package_id,
                "package_title": "Vault (sealed)",
                "module": {"id": "vault", "title": "Vault", "root_url": small_url},
                "root_url": small_url,
                "resources": records.iter().map(|record| serde_json::json!({
                    "url": record.resource_url,
                    "type": "sealed",
                    "mime": record.mime_type,
                })).collect::<Vec<_>>(),
                "sealing": fixture.get("fragment").unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();

        let gateway = super::OfflineGateway::start(library, directory.path())
            .await
            .unwrap();
        let access = gateway
            .activate_sealed(node_origin.into(), package_id.into(), passphrase.to_owned())
            .await
            .unwrap();
        let state = gateway.runtime.clone();
        for entry in [small, chunk] {
            let url = entry.get("url").unwrap().as_str().unwrap();
            let request = Request::builder()
                .method(Method::GET)
                .uri(format!("{}{}", access.origin, url))
                .header(
                    axum::http::header::COOKIE,
                    format!("{}={}", super::SESSION_COOKIE, access.token),
                )
                .body(Body::empty())
                .unwrap();
            let response = super::handle(State(state.clone()), request).await;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            // Drain the whole body: the full plaintext crosses RAM here.
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(body.windows(canary.len()).any(|window| window == canary));
        }
        drop(gateway);
        drop(state);

        let mut stack = vec![directory.path().to_owned()];
        let mut files = 0;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    files += 1;
                    let bytes = std::fs::read(&path).unwrap();
                    assert!(
                        !bytes.windows(canary.len()).any(|window| window == canary),
                        "plaintext marker leaked into {}",
                        path.display()
                    );
                }
            }
        }
        assert!(files > 0, "canary walk inspected no files");
    }
}
