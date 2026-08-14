mod atomic_file;
mod config;
mod credentials;
mod downloads;
mod error;
mod library;
mod node;
mod node_view;
mod nsp;
mod offline_gateway;
mod offline_view;
mod state;
mod window_chrome;

use state::{AppState, BootstrapState, ConnectRequest, ConnectionState};
use tauri::{AppHandle, Manager, State};

use crate::error::AppResult;

#[tauri::command]
async fn bootstrap(state: State<'_, AppState>) -> AppResult<BootstrapState> {
    state.bootstrap().await
}

#[tauri::command]
async fn connect_node(
    app: AppHandle,
    state: State<'_, AppState>,
    request: ConnectRequest,
) -> AppResult<ConnectionState> {
    let connection = state.connect(request).await?;
    node_view::open(&app, &state)?;
    Ok(connection)
}

#[tauri::command]
async fn unlock_vault(
    app: AppHandle,
    state: State<'_, AppState>,
    password: String,
) -> AppResult<ConnectionState> {
    let connection = state.unlock(password).await?;
    if connection.is_connected() {
        node_view::open(&app, &state)?;
    }
    Ok(connection)
}

#[tauri::command]
fn open_node(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    node_view::open(&app, &state)
}

#[tauri::command]
fn disconnect_node(app: AppHandle, state: State<'_, AppState>) -> AppResult<ConnectionState> {
    let result = state.disconnect();
    for label in ["node", "offline"] {
        if let Some(webview) = app.get_webview(label) {
            let _ = webview.clear_all_browsing_data();
            let _ = webview.close();
        }
    }
    window_chrome::show_home(&app);
    result
}

#[tauri::command]
fn list_library(state: State<'_, AppState>) -> AppResult<Vec<crate::library::SavedModule>> {
    state.saved_modules()
}

#[tauri::command]
async fn open_offline(
    app: AppHandle,
    state: State<'_, AppState>,
    node_origin: String,
    package_id: String,
) -> AppResult<()> {
    offline_view::open(&app, &state, node_origin, package_id).await
}

#[tauri::command]
async fn open_offline_module(
    app: AppHandle,
    state: State<'_, AppState>,
    node_origin: String,
    module_id: String,
) -> AppResult<()> {
    offline_view::open_module(&app, &state, node_origin, module_id).await
}

#[tauri::command]
fn show_home(app: AppHandle) {
    window_chrome::show_home(&app);
}

#[tauri::command]
fn navigate_back(app: AppHandle) {
    window_chrome::navigate_back(&app);
}

#[tauri::command]
fn hide_to_tray(app: AppHandle) {
    window_chrome::hide_or_close(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let state = AppState::new(&data_dir).map_err(|error| error.to_string())?;
            app.manage(state);
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
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            connect_node,
            unlock_vault,
            open_node,
            disconnect_node,
            list_library,
            open_offline,
            open_offline_module,
            show_home,
            navigate_back,
            hide_to_tray
        ])
        .run(tauri::generate_context!())
        .expect("NetSanctum Desktop runtime failed");
}
