use std::time::Duration;

mod atomic_file;
mod config;
mod credentials;
mod downloads;
mod error;
mod library;
#[cfg(target_os = "android")]
mod mobile_node;
mod node;
#[cfg(not(mobile))]
mod node_view;
mod nsp;
mod offline_gateway;
#[cfg(not(mobile))]
mod offline_view;
mod state;
#[cfg(not(mobile))]
mod window_chrome;

use state::{AppState, BootstrapState, ConnectRequest, ConnectionState};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::error::AppResult;

#[tauri::command]
async fn bootstrap(app: AppHandle, state: State<'_, AppState>) -> AppResult<BootstrapState> {
    let result = state.bootstrap().await?;
    #[cfg(target_os = "android")]
    let result = {
        let mut result = result;
        if result.vault_exists
            && let Some(password) = mobile_node::load_vault_password(&app)?
        {
            result.connection = state.unlock(password).await?;
        }
        result
    };
    #[cfg(not(target_os = "android"))]
    let _ = app;
    Ok(result)
}

#[tauri::command]
async fn diagnose_node(
    state: State<'_, AppState>,
    node_url: String,
    allow_insecure_http: bool,
) -> AppResult<Vec<String>> {
    state.diagnose_node(node_url, allow_insecure_http).await
}

#[tauri::command]
fn download_package(app: AppHandle, manifest_url: String) -> AppResult<bool> {
    downloads::enqueue(&app, manifest_url)
}

#[tauri::command]
async fn connect_node(
    app: AppHandle,
    state: State<'_, AppState>,
    request: ConnectRequest,
) -> AppResult<ConnectionState> {
    #[cfg(target_os = "android")]
    let (request, device_password) = {
        let mut request = request;
        let password = if request.remember_without_password {
            let password = mobile_node::new_vault_password()?;
            request.vault_password = password.to_string();
            Some(password)
        } else {
            None
        };
        (request, password)
    };
    let _ = app.emit_to("main", "connection-progress", "Проверяем доступ к узлу…");
    let progress_app = app.clone();
    let connection = tokio::time::timeout(
        Duration::from_secs(20),
        state.connect(request, move || {
            let _ =
                progress_app.emit_to("main", "connection-progress", "Создаём защищённый vault…");
        }),
    )
    .await
    .map_err(|_| {
        crate::error::AppError::NodeUnavailable("превышено время ожидания авторизации".into())
    })??;
    #[cfg(target_os = "android")]
    if let Some(password) = device_password {
        if let Err(error) = mobile_node::store_vault_password(&app, password.as_str()) {
            let _ = state.disconnect();
            return Err(error);
        }
    } else {
        mobile_node::clear_vault_password(&app);
    }
    #[cfg(not(mobile))]
    node_view::open(&app, &state)?;
    #[cfg(target_os = "android")]
    mobile_node::open(&app, &state)?;
    #[cfg(all(mobile, not(target_os = "android")))]
    let _ = app;
    Ok(connection)
}

#[tauri::command]
fn save_mobile_session(
    state: State<'_, AppState>,
    request: ConnectRequest,
    access_token: String,
    expires_in: u64,
) -> AppResult<ConnectionState> {
    state.save_mobile_session(request, access_token, expires_in)
}

#[tauri::command]
async fn unlock_vault(
    app: AppHandle,
    state: State<'_, AppState>,
    password: String,
) -> AppResult<ConnectionState> {
    let connection = state.unlock(password).await?;
    #[cfg(not(mobile))]
    if connection.is_connected() {
        node_view::open(&app, &state)?;
    }
    #[cfg(target_os = "android")]
    if connection.is_connected() {
        mobile_node::open(&app, &state)?;
    }
    #[cfg(all(mobile, not(target_os = "android")))]
    let _ = app;
    Ok(connection)
}

#[tauri::command]
fn open_node(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    #[cfg(not(mobile))]
    {
        node_view::open(&app, &state)
    }
    #[cfg(target_os = "android")]
    {
        mobile_node::open(&app, &state)
    }
    #[cfg(all(mobile, not(target_os = "android")))]
    {
        let _ = (app, state);
        Err(crate::error::AppError::Internal(
            "Live node browsing is not available on this platform yet.".into(),
        ))
    }
}

#[tauri::command]
fn disconnect_node(app: AppHandle, state: State<'_, AppState>) -> AppResult<ConnectionState> {
    let result = state.disconnect();
    #[cfg(target_os = "android")]
    mobile_node::clear_vault_password(&app);
    #[cfg(not(mobile))]
    {
        for label in ["node", "offline"] {
            if let Some(webview) = app.get_webview(label) {
                let _ = webview.clear_all_browsing_data();
                let _ = webview.close();
            }
        }
        window_chrome::show_home(&app);
    }
    #[cfg(mobile)]
    let _ = app;
    result
}

#[tauri::command]
fn reset_connection(app: AppHandle, state: State<'_, AppState>) -> AppResult<ConnectionState> {
    let result = state.disconnect();
    #[cfg(target_os = "android")]
    mobile_node::clear_vault_password(&app);
    #[cfg(not(target_os = "android"))]
    let _ = app;
    result
}

#[tauri::command]
fn list_library(state: State<'_, AppState>) -> AppResult<Vec<crate::library::SavedModule>> {
    state.saved_modules()
}

#[tauri::command]
async fn delete_package(
    state: State<'_, AppState>,
    node_origin: String,
    package_id: String,
) -> AppResult<Vec<crate::library::SavedModule>> {
    state.delete_package(node_origin, package_id).await
}

#[tauri::command]
async fn open_offline(
    app: AppHandle,
    state: State<'_, AppState>,
    node_origin: String,
    package_id: String,
) -> AppResult<()> {
    #[cfg(not(mobile))]
    {
        return offline_view::open(&app, &state, node_origin, package_id).await;
    }
    #[cfg(mobile)]
    {
        let _ = (app, state, node_origin, package_id);
        Err(crate::error::AppError::Internal(
            "Offline package browsing is not available in the Android preview yet.".into(),
        ))
    }
}

#[tauri::command]
async fn open_offline_module(
    app: AppHandle,
    state: State<'_, AppState>,
    node_origin: String,
    module_id: String,
) -> AppResult<()> {
    #[cfg(not(mobile))]
    {
        return offline_view::open_module(&app, &state, node_origin, module_id).await;
    }
    #[cfg(mobile)]
    {
        let (root_url, package_ids) = state
            .library()
            .module_offline_info(&node_origin, &module_id)?;
        let access = state
            .offline_gateway()
            .await?
            .activate_module(node_origin, package_ids)
            .await?;
        let target = format!("{}{}", access.origin, root_url);
        mobile_node::open_offline(&app, &target, &access.token)
    }
}

#[tauri::command]
fn is_mobile() -> bool {
    cfg!(mobile)
}

#[tauri::command]
fn create_module_shortcut(
    app: AppHandle,
    node_origin: String,
    module_id: String,
    name: String,
) -> AppResult<()> {
    #[cfg(target_os = "android")]
    {
        return mobile_node::create_shortcut(&app, &node_origin, &module_id, &name);
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, node_origin, module_id, name);
        Err(crate::error::AppError::Internal(
            "Module shortcuts are only available on Android.".into(),
        ))
    }
}

#[tauri::command]
#[cfg(target_os = "android")]
fn take_mobile_shortcut(app: AppHandle) -> AppResult<Option<mobile_node::ShortcutTarget>> {
    mobile_node::take_shortcut(&app)
}

#[tauri::command]
#[cfg(not(target_os = "android"))]
fn take_mobile_shortcut(_app: AppHandle) -> AppResult<Option<serde_json::Value>> {
    Ok(None)
}

#[tauri::command]
fn show_home(app: AppHandle) {
    #[cfg(not(mobile))]
    window_chrome::show_home(&app);
    #[cfg(mobile)]
    let _ = app;
}

#[tauri::command]
fn navigate_back(app: AppHandle) {
    #[cfg(not(mobile))]
    window_chrome::navigate_back(&app);
    #[cfg(mobile)]
    let _ = app;
}

#[tauri::command]
fn hide_to_tray(app: AppHandle) {
    #[cfg(not(mobile))]
    window_chrome::hide_or_close(&app);
    #[cfg(mobile)]
    let _ = app;
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(target_os = "android")]
    let builder = builder.plugin(mobile_node::init());
    builder
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let state = AppState::new(&data_dir).map_err(|error| error.to_string())?;
            app.manage(state);
            #[cfg(not(mobile))]
            {
                let tray_available = window_chrome::install_tray(app).is_ok();
                app.state::<AppState>().set_tray_available(tray_available);
                let app_handle = app.handle().clone();
                let window = app
                    .get_window("main")
                    .ok_or_else(|| "main window is missing".to_owned())?;
                window.on_window_event(move |event| match event {
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        if app_handle.state::<AppState>().tray_available() {
                            api.prevent_close();
                            if let Some(window) = app_handle.get_window("main") {
                                let _ = window.hide();
                            }
                        }
                    }
                    tauri::WindowEvent::Resized(size) => {
                        window_chrome::resize_content_webviews(&app_handle, *size);
                    }
                    tauri::WindowEvent::ScaleFactorChanged { new_inner_size, .. } => {
                        window_chrome::resize_content_webviews(&app_handle, *new_inner_size);
                    }
                    _ => {}
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            diagnose_node,
            download_package,
            connect_node,
            save_mobile_session,
            unlock_vault,
            open_node,
            disconnect_node,
            reset_connection,
            list_library,
            delete_package,
            open_offline,
            open_offline_module,
            is_mobile,
            create_module_shortcut,
            take_mobile_shortcut,
            show_home,
            navigate_back,
            hide_to_tray
        ])
        .run(tauri::generate_context!())
        .expect("NetSanctum Desktop runtime failed");
}
