use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Manager, Runtime,
    plugin::{Builder, PluginHandle, TauriPlugin},
};

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

struct MobileNodePlugin<R: Runtime>(PluginHandle<R>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenNodePayload<'a> {
    node_url: &'a str,
    node_origin: &'a str,
    module_id: &'a str,
    module_title: &'a str,
    module_path: &'a str,
    cookie_name: &'a str,
    cookie_value: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadNotificationPayload<'a> {
    status: &'a str,
    progress: f64,
    message: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateShortcutPayload<'a> {
    node_origin: &'a str,
    module_id: &'a str,
    module_title: &'a str,
    module_path: &'a str,
    name: &'a str,
    icon_text: &'a str,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutTarget {
    pub node_origin: String,
    pub module_id: String,
    pub module_title: String,
    pub module_path: String,
}

#[derive(Serialize)]
struct VaultKeyPayload<'a> {
    value: &'a str,
}

#[derive(Deserialize)]
struct VaultKeyResponse {
    value: String,
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("mobile-node")
        .setup(|app, api| {
            let handle = api.register_android_plugin("dev.netsanctum.desktop", "NodeViewPlugin")?;
            app.manage(MobileNodePlugin(handle));
            Ok(())
        })
        .build()
}

pub fn open<R: Runtime>(app: &AppHandle<R>, state: &AppState) -> AppResult<()> {
    open_path(app, state, None)
}

pub fn open_path<R: Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    module_path: Option<&str>,
) -> AppResult<()> {
    let session = state.node_view_session()?;
    if session.cookie_value.is_empty() {
        return Err(AppError::SessionMissing);
    }
    let target = match module_path {
        Some(path) => {
            if !path.starts_with('/') || path.starts_with("//") || path.contains('#') {
                return Err(AppError::InvalidPackage("invalid module path".into()));
            }
            let target = session
                .node_url
                .join(path)
                .map_err(|error| AppError::InvalidNodeUrl(error.to_string()))?;
            if target.origin() != session.node_url.origin() {
                return Err(AppError::InvalidNodeUrl(
                    "module path leaves the node origin".into(),
                ));
            }
            target.to_string()
        }
        None => session.node_url.to_string(),
    };
    app.state::<MobileNodePlugin<R>>()
        .0
        .run_mobile_plugin::<()>(
            "open",
            OpenNodePayload {
                node_url: &target,
                node_origin: session.node_url.as_str(),
                module_id: "",
                module_title: "",
                module_path: module_path.unwrap_or(""),
                cookie_name: "access_token",
                cookie_value: session.cookie_value.as_str(),
            },
        )
        .map_err(|error| AppError::Internal(error.to_string()))
}

pub fn open_offline<R: Runtime>(
    app: &AppHandle<R>,
    url: &str,
    node_origin: &str,
    module_id: &str,
    module_title: &str,
    module_path: &str,
    cookie_value: &str,
) -> AppResult<()> {
    app.state::<MobileNodePlugin<R>>()
        .0
        .run_mobile_plugin::<()>(
            "open",
            OpenNodePayload {
                node_url: url,
                node_origin,
                module_id,
                module_title,
                module_path,
                cookie_name: crate::offline_gateway::SESSION_COOKIE,
                cookie_value,
            },
        )
        .map_err(|error| AppError::Internal(error.to_string()))
}

pub fn notify_download<R: Runtime>(app: &AppHandle<R>, status: &str, progress: f64, message: &str) {
    let Some(plugin) = app.try_state::<MobileNodePlugin<R>>() else {
        return;
    };
    let _ = plugin.0.run_mobile_plugin::<()>(
        "notifyDownload",
        DownloadNotificationPayload {
            status,
            progress,
            message,
        },
    );
}

pub fn create_shortcut<R: Runtime>(
    app: &AppHandle<R>,
    node_origin: &str,
    module_id: &str,
    module_title: &str,
    module_path: &str,
    name: &str,
    icon_text: &str,
) -> AppResult<()> {
    app.state::<MobileNodePlugin<R>>()
        .0
        .run_mobile_plugin::<()>(
            "createShortcut",
            CreateShortcutPayload {
                node_origin,
                module_id,
                module_title,
                module_path,
                name,
                icon_text,
            },
        )
        .map_err(|error| AppError::Internal(error.to_string()))
}

pub fn take_shortcut<R: Runtime>(app: &AppHandle<R>) -> AppResult<Option<ShortcutTarget>> {
    app.state::<MobileNodePlugin<R>>()
        .0
        .run_mobile_plugin("takeShortcut", ())
        .map_err(|error| AppError::Internal(error.to_string()))
}

pub fn new_vault_password() -> AppResult<zeroize::Zeroizing<String>> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| AppError::Internal(error.to_string()))?;
    Ok(zeroize::Zeroizing::new(
        bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
    ))
}

pub fn store_vault_password<R: Runtime>(app: &AppHandle<R>, password: &str) -> AppResult<()> {
    app.state::<MobileNodePlugin<R>>()
        .0
        .run_mobile_plugin::<()>("storeVaultKey", VaultKeyPayload { value: password })
        .map_err(|error| AppError::Internal(error.to_string()))
}

pub fn load_vault_password<R: Runtime>(app: &AppHandle<R>) -> AppResult<Option<String>> {
    app.state::<MobileNodePlugin<R>>()
        .0
        .run_mobile_plugin::<Option<VaultKeyResponse>>("loadVaultKey", ())
        .map(|value| value.map(|value| value.value))
        .map_err(|error| AppError::Internal(error.to_string()))
}

pub fn clear_vault_password<R: Runtime>(app: &AppHandle<R>) {
    if let Some(plugin) = app.try_state::<MobileNodePlugin<R>>() {
        let _ = plugin.0.run_mobile_plugin::<()>("clearVaultKey", ());
    }
}
