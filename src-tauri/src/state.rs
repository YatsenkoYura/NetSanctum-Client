use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::config::{ConfigStore, StoredConfig, normalize_node_url};
use crate::credentials::CredentialStore;
use crate::error::{AppError, AppResult};
use crate::library::{Library, SavedModule};
use crate::node::{DesktopSession, NodeClient, SessionCredential};

#[derive(Debug, Deserialize)]
pub struct ConnectRequest {
    pub(crate) node_url: String,
    pub(crate) master_token: String,
    pub(crate) vault_password: String,
    pub(crate) allow_insecure_http: bool,
    #[serde(default)]
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) remember_without_password: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Connected,
    Disconnected,
    VaultLocked,
    Unreachable,
    CredentialsInvalid,
}

#[derive(Clone, Copy)]
pub enum ActiveView {
    Desktop,
    Node,
    Offline,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConnectionState {
    status: ConnectionStatus,
    node_url: Option<String>,
    node_name: Option<String>,
    message: Option<String>,
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self.status, ConnectionStatus::Connected)
    }
}

#[derive(Debug, Serialize)]
pub struct BootstrapState {
    pub(crate) connection: ConnectionState,
    pub(crate) modules: Vec<SavedModule>,
    pub(crate) vault_exists: bool,
}

struct ActiveSession {
    credential: SessionCredential,
    web_cookie: Zeroizing<String>,
    _expires_at: Instant,
    _node_name: String,
}

pub struct AppState {
    config_store: ConfigStore,
    credentials: CredentialStore,
    node_client: NodeClient,
    library: Arc<Library>,
    config: Mutex<Option<StoredConfig>>,
    session: Mutex<Option<ActiveSession>>,
    master_token: Mutex<Option<Zeroizing<String>>>,
    data_dir: PathBuf,
    active_downloads: Mutex<HashSet<String>>,
    offline_gateway: tokio::sync::OnceCell<crate::offline_gateway::OfflineGateway>,
    active_view: Mutex<ActiveView>,
    session_generation: AtomicU64,
    tray_available: AtomicBool,
}

pub struct NodeViewSession {
    pub node_url: url::Url,
    pub cookie_value: Zeroizing<String>,
}

pub struct DownloadSession {
    pub node_url: url::Url,
    pub credential: SessionCredential,
    pub data_dir: PathBuf,
    pub generation: u64,
}

impl AppState {
    pub fn new(data_dir: &Path) -> AppResult<Self> {
        // Downloads are staged separately from completed objects and are never valid after restart.
        let _ = std::fs::remove_dir_all(data_dir.join("downloads"));
        let config_store = ConfigStore::new(data_dir);
        let config = config_store.load()?;
        let credentials = CredentialStore::new(data_dir);
        Ok(Self {
            config_store,
            credentials,
            node_client: NodeClient::new()?,
            library: Arc::new(Library::open(data_dir)?),
            config: Mutex::new(config),
            session: Mutex::new(None),
            master_token: Mutex::new(None),
            data_dir: data_dir.to_owned(),
            active_downloads: Mutex::new(HashSet::new()),
            offline_gateway: tokio::sync::OnceCell::new(),
            active_view: Mutex::new(ActiveView::Desktop),
            session_generation: AtomicU64::new(1),
            tray_available: AtomicBool::new(false),
        })
    }

    pub async fn bootstrap(&self) -> AppResult<BootstrapState> {
        let config = self
            .config
            .lock()
            .map_err(|_| AppError::Internal("блокировка конфигурации повреждена".into()))?
            .clone();
        let mut vault_exists = self.credentials.exists();
        let connection = if config.is_some() && vault_exists {
            ConnectionState {
                status: ConnectionStatus::VaultLocked,
                node_url: config.map(|value| value.node_url.to_string()),
                node_name: None,
                message: Some("Введите пароль локального хранилища для продолжения.".into()),
            }
        } else {
            if config.is_some() != vault_exists {
                self.clear_invalid_connection()?;
                vault_exists = false;
            }
            disconnected_state(None, None)
        };
        Ok(BootstrapState {
            connection,
            modules: self.library.modules()?,
            vault_exists,
        })
    }

    pub async fn connect<F>(
        &self,
        mut request: ConnectRequest,
        vault_stage: F,
    ) -> AppResult<ConnectionState>
    where
        F: FnOnce(),
    {
        let node_url = normalize_node_url(&request.node_url, request.allow_insecure_http)?;
        let master_token = Zeroizing::new(std::mem::take(&mut request.master_token));
        let vault_password = Zeroizing::new(std::mem::take(&mut request.vault_password));
        if master_token.trim().is_empty() {
            return Err(AppError::EmptyMasterToken);
        }
        self.credentials
            .validate_new_password(vault_password.as_str())?;

        let session = self
            .node_client
            .create_session(&node_url, master_token.trim())
            .await?;
        vault_stage();
        self.persist_connection(
            node_url,
            request.allow_insecure_http,
            master_token,
            vault_password,
            session,
        )
    }

    pub fn save_mobile_session(
        &self,
        mut request: ConnectRequest,
        access_token: String,
        expires_in: u64,
    ) -> AppResult<ConnectionState> {
        let node_url = normalize_node_url(&request.node_url, request.allow_insecure_http)?;
        let master_token = Zeroizing::new(std::mem::take(&mut request.master_token));
        let vault_password = Zeroizing::new(std::mem::take(&mut request.vault_password));
        if master_token.trim().is_empty()
            || access_token.is_empty()
            || expires_in == 0
            || expires_in > 86_400
        {
            return Err(AppError::InvalidNodeResponse(
                "узел вернул некорректную временную сессию".into(),
            ));
        }
        self.credentials
            .validate_new_password(vault_password.as_str())?;
        let node_name = node_url
            .host_str()
            .ok_or_else(|| AppError::InvalidNodeUrl("адрес не содержит host".into()))?
            .to_owned();
        self.persist_connection(
            node_url,
            request.allow_insecure_http,
            master_token,
            vault_password,
            DesktopSession {
                credential: SessionCredential::Bearer(Zeroizing::new(access_token)),
                web_cookie: Zeroizing::new(String::new()),
                expires_in,
                node_name,
            },
        )
    }

    fn persist_connection(
        &self,
        node_url: url::Url,
        allow_insecure_http: bool,
        master_token: Zeroizing<String>,
        vault_password: Zeroizing<String>,
        session: DesktopSession,
    ) -> AppResult<ConnectionState> {
        self.credentials
            .save(master_token.trim(), vault_password.as_str())?;
        let config = StoredConfig {
            node_url: node_url.clone(),
            allow_insecure_http,
        };
        if let Err(error) = self.config_store.save(&config) {
            let _ = self.credentials.clear();
            return Err(error);
        }
        *self
            .master_token
            .lock()
            .map_err(|_| AppError::Internal("блокировка master token повреждена".into()))? =
            Some(master_token);
        *self
            .config
            .lock()
            .map_err(|_| AppError::Internal("блокировка конфигурации повреждена".into()))? =
            Some(config);
        self.install_session(node_url.as_str(), session)
    }

    pub async fn unlock(&self, password: String) -> AppResult<ConnectionState> {
        let password = Zeroizing::new(password);
        let master_token = self.credentials.read(password.as_str())?;
        let connection = self.restore_connection(master_token.as_str()).await;
        if matches!(
            connection.status,
            ConnectionStatus::Connected | ConnectionStatus::Unreachable
        ) {
            *self
                .master_token
                .lock()
                .map_err(|_| AppError::Internal("блокировка master token повреждена".into()))? =
                Some(master_token);
        }
        Ok(connection)
    }

    pub fn disconnect(&self) -> AppResult<ConnectionState> {
        let credentials_result = self.credentials.clear();
        let config_result = self.config_store.clear();
        *self
            .config
            .lock()
            .map_err(|_| AppError::Internal("блокировка конфигурации повреждена".into()))? = None;
        *self
            .session
            .lock()
            .map_err(|_| AppError::Internal("блокировка сессии повреждена".into()))? = None;
        *self
            .master_token
            .lock()
            .map_err(|_| AppError::Internal("блокировка master token повреждена".into()))? = None;
        self.session_generation.fetch_add(1, Ordering::SeqCst);
        credentials_result?;
        config_result?;
        Ok(disconnected_state(None, None))
    }

    pub fn node_view_session(&self) -> AppResult<NodeViewSession> {
        let node_url = self
            .config
            .lock()
            .map_err(|_| AppError::Internal("блокировка конфигурации повреждена".into()))?
            .as_ref()
            .map(|config| config.node_url.clone())
            .ok_or(AppError::SessionMissing)?;
        let session = self
            .session
            .lock()
            .map_err(|_| AppError::Internal("блокировка сессии повреждена".into()))?;
        let cookie_value = session
            .as_ref()
            .map(|active| active.web_cookie.as_str())
            .ok_or(AppError::SessionMissing)?;
        Ok(NodeViewSession {
            node_url,
            cookie_value: Zeroizing::new(cookie_value.to_owned()),
        })
    }

    pub fn lock_vault(&self) -> ConnectionState {
        if let Ok(mut session) = self.session.lock() {
            *session = None;
        }
        if let Ok(mut token) = self.master_token.lock() {
            *token = None;
        }
        self.session_generation.fetch_add(1, Ordering::SeqCst);
        let node_url = self
            .config
            .lock()
            .ok()
            .and_then(|config| config.as_ref().map(|value| value.node_url.to_string()));
        ConnectionState {
            status: ConnectionStatus::VaultLocked,
            node_url,
            node_name: None,
            message: Some("Сессия узла истекла. Разблокируйте локальное хранилище снова.".into()),
        }
    }

    pub fn download_session(&self) -> AppResult<DownloadSession> {
        let node_url = self
            .config
            .lock()
            .map_err(|_| AppError::Internal("блокировка конфигурации повреждена".into()))?
            .as_ref()
            .map(|config| config.node_url.clone())
            .ok_or(AppError::SessionMissing)?;
        let credential = self
            .session
            .lock()
            .map_err(|_| AppError::Internal("блокировка сессии повреждена".into()))?
            .as_ref()
            .map(|session| session.credential.clone())
            .ok_or(AppError::SessionMissing)?;
        Ok(DownloadSession {
            node_url,
            credential,
            data_dir: self.data_dir.clone(),
            generation: self.session_generation.load(Ordering::SeqCst),
        })
    }

    pub fn session_is_current(&self, generation: u64) -> bool {
        self.session_generation.load(Ordering::SeqCst) == generation
    }

    pub fn set_tray_available(&self, available: bool) {
        self.tray_available.store(available, Ordering::SeqCst);
    }

    pub fn tray_available(&self) -> bool {
        self.tray_available.load(Ordering::SeqCst)
    }

    pub fn reserve_download(&self, manifest_url: &str) -> AppResult<bool> {
        Ok(self
            .active_downloads
            .lock()
            .map_err(|_| AppError::Internal("блокировка очереди загрузок повреждена".into()))?
            .insert(manifest_url.to_owned()))
    }

    pub fn release_download(&self, manifest_url: &str) {
        if let Ok(mut downloads) = self.active_downloads.lock() {
            downloads.remove(manifest_url);
        }
    }

    pub fn library(&self) -> &Library {
        &self.library
    }

    pub fn node_client(&self) -> &NodeClient {
        &self.node_client
    }

    pub async fn diagnose_node(
        &self,
        raw_url: String,
        allow_insecure_http: bool,
    ) -> AppResult<Vec<String>> {
        let node_url = normalize_node_url(&raw_url, allow_insecure_http)?;
        self.node_client.diagnose(&node_url).await
    }

    pub async fn shortcut_online_available(&self, node_origin: &str) -> AppResult<bool> {
        let session = match self.node_view_session() {
            Ok(session) => session,
            Err(AppError::SessionMissing) => return Ok(false),
            Err(error) => return Err(error),
        };
        if session.node_url.as_str().trim_end_matches('/') != node_origin.trim_end_matches('/') {
            return Ok(false);
        }
        let credential = SessionCredential::Cookie(session.cookie_value);
        self.node_client
            .check_authenticated(&session.node_url, &credential)
            .await
    }

    pub fn saved_modules(&self) -> AppResult<Vec<SavedModule>> {
        self.library.modules()
    }

    pub async fn delete_package(
        &self,
        node_origin: String,
        package_id: String,
    ) -> AppResult<Vec<SavedModule>> {
        if package_id.is_empty()
            || matches!(package_id.as_str(), "." | "..")
            || package_id.len() > 160
            || !package_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(AppError::InvalidPackage("invalid package_id".into()));
        }
        let orphaned_files = self.library.delete_package(&node_origin, &package_id)?;
        let objects_dir = self.data_dir.join("objects");
        for path in orphaned_files {
            let path = PathBuf::from(path);
            if path.starts_with(&objects_dir) {
                let _ = tokio::fs::remove_file(path).await;
            }
        }
        let package_dir =
            crate::downloads::package_directory(&self.data_dir, &node_origin, &package_id);
        let _ = tokio::fs::remove_dir_all(package_dir).await;
        self.library.modules()
    }

    pub async fn offline_gateway(&self) -> AppResult<&crate::offline_gateway::OfflineGateway> {
        self.offline_gateway
            .get_or_try_init(|| crate::offline_gateway::OfflineGateway::start(self.library.clone()))
            .await
    }

    pub fn set_active_view(&self, view: ActiveView) {
        if let Ok(mut active) = self.active_view.lock() {
            *active = view;
        }
    }

    pub fn active_view(&self) -> ActiveView {
        self.active_view
            .lock()
            .map(|active| *active)
            .unwrap_or(ActiveView::Desktop)
    }

    async fn restore_connection(&self, master_token: &str) -> ConnectionState {
        let config = match self.config.lock() {
            Ok(config) => config.clone(),
            Err(_) => {
                return disconnected_state(None, Some("Не удалось прочитать конфигурацию.".into()));
            }
        };
        let Some(config) = config else {
            return disconnected_state(None, None);
        };
        let url = config.node_url.to_string();

        match self
            .node_client
            .create_session(&config.node_url, master_token.trim())
            .await
        {
            Ok(session) => self.install_session(&url, session).unwrap_or_else(|error| {
                disconnected_state(Some(url.clone()), Some(error.to_string()))
            }),
            Err(AppError::InvalidCredentials) => {
                let _ = self.clear_invalid_connection();
                ConnectionState {
                    status: ConnectionStatus::CredentialsInvalid,
                    node_url: None,
                    node_name: None,
                    message: Some(
                        "Мастер-токен больше не действителен. Данные подключения удалены.".into(),
                    ),
                }
            }
            Err(error) => ConnectionState {
                status: ConnectionStatus::Unreachable,
                node_url: Some(url),
                node_name: None,
                message: Some(error.to_string()),
            },
        }
    }

    fn install_session(
        &self,
        node_url: &str,
        session: DesktopSession,
    ) -> AppResult<ConnectionState> {
        if session.credential.is_empty() {
            return Err(AppError::InvalidNodeResponse(
                "узел вернул пустую временную сессию".into(),
            ));
        }
        let node_name = session.node_name.clone();
        *self
            .session
            .lock()
            .map_err(|_| AppError::Internal("блокировка сессии повреждена".into()))? =
            Some(ActiveSession {
                credential: session.credential,
                web_cookie: session.web_cookie,
                _expires_at: Instant::now() + Duration::from_secs(session.expires_in),
                _node_name: node_name.clone(),
            });
        Ok(ConnectionState {
            status: ConnectionStatus::Connected,
            node_url: Some(node_url.into()),
            node_name: Some(node_name.clone()),
            message: Some(format!(
                "Безопасная временная сессия с «{node_name}» создана."
            )),
        })
    }

    fn clear_invalid_connection(&self) -> AppResult<()> {
        self.credentials.clear()?;
        self.config_store.clear()?;
        *self
            .config
            .lock()
            .map_err(|_| AppError::Internal("блокировка конфигурации повреждена".into()))? = None;
        *self
            .session
            .lock()
            .map_err(|_| AppError::Internal("блокировка сессии повреждена".into()))? = None;
        *self
            .master_token
            .lock()
            .map_err(|_| AppError::Internal("блокировка master token повреждена".into()))? = None;
        self.session_generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn disconnected_state(node_url: Option<String>, message: Option<String>) -> ConnectionState {
    ConnectionState {
        status: ConnectionStatus::Disconnected,
        node_url,
        node_name: None,
        message,
    }
}
