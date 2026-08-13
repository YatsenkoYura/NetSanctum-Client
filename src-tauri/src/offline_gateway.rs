use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, Response, StatusCode, header};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::RwLock;
use tokio_util::io::ReaderStream;

use crate::error::{AppError, AppResult};
use crate::library::{Library, StoredResource};
use crate::nsp::{NspEntry, NspIndex};

pub const SESSION_COOKIE: &str = "netsanctum_offline";
const MAX_MERGED_JSON_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone)]
struct OfflineSession {
    token: String,
    node_origin: String,
    package_ids: Vec<String>,
}

struct Runtime {
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
    pub async fn start(library: Arc<Library>) -> AppResult<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| AppError::Internal(error.to_string()))?;
        let address = listener
            .local_addr()
            .map_err(|error| AppError::Internal(error.to_string()))?;
        let runtime = Arc::new(Runtime {
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
        });
        Ok(OfflineAccess {
            origin: self.origin.clone(),
            token,
        })
    }
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
        Ok(Some(segment)) => match serve_segment(segment, range, method == Method::HEAD).await {
            Ok(response) => response,
            Err(AppError::RangeNotSatisfiable) => plain(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "Range is not satisfiable",
            ),
            Err(error) => plain(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        },
        Ok(None) => plain(
            StatusCode::NOT_FOUND,
            "Resource is not part of this offline package",
        ),
        Err(error) => plain(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
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
            return Ok(Some(Segment::File(resource)));
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
    use super::parse_range;

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
}
