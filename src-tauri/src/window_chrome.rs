use tauri::{
    App, AppHandle, Manager, PhysicalPosition, PhysicalSize, Rect,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

use crate::error::{AppError, AppResult};
use crate::state::{ActiveView, AppState};

pub fn full_bounds(size: PhysicalSize<u32>) -> Rect {
    Rect {
        position: PhysicalPosition::new(0, 0).into(),
        size: size.into(),
    }
}

pub fn resize_content_webviews(app: &AppHandle, size: PhysicalSize<u32>) {
    let label = match app.state::<AppState>().active_view() {
        ActiveView::Desktop => return,
        ActiveView::Node => "node",
        ActiveView::Offline => "offline",
    };
    if let Some(webview) = app.get_webview(label) {
        let _ = webview.set_bounds(full_bounds(size));
    }
}

pub fn show_content(app: &AppHandle, view: ActiveView) -> AppResult<()> {
    let window = app
        .get_window("main")
        .ok_or_else(|| AppError::Internal("главное окно не найдено".into()))?;
    let size = window
        .inner_size()
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let active_label = match view {
        ActiveView::Node => "node",
        ActiveView::Offline => "offline",
        ActiveView::Desktop => return Err(AppError::Internal("invalid content view".into())),
    };
    for label in ["node", "offline"] {
        if let Some(webview) = app.get_webview(label) {
            if label == active_label {
                webview
                    .set_bounds(full_bounds(size))
                    .map_err(|error| AppError::Internal(error.to_string()))?;
                webview
                    .show()
                    .map_err(|error| AppError::Internal(error.to_string()))?;
                webview
                    .set_focus()
                    .map_err(|error| AppError::Internal(error.to_string()))?;
            } else {
                let _ = webview.hide();
            }
        }
    }
    if let Some(main) = app.get_webview("main") {
        let _ = main.hide();
    }
    app.state::<AppState>().set_active_view(view);
    Ok(())
}

pub fn show_home(app: &AppHandle) {
    for label in ["node", "offline"] {
        if let Some(webview) = app.get_webview(label) {
            let _ = webview.hide();
        }
    }
    if let Some(main) = app.get_webview("main") {
        if let Some(window) = app.get_window("main")
            && let Ok(size) = window.inner_size()
        {
            let _ = main.set_bounds(full_bounds(size));
        }
        let _ = main.show();
        let _ = main.set_focus();
    }
    app.state::<AppState>().set_active_view(ActiveView::Desktop);
}

pub fn navigate_back(app: &AppHandle) {
    let label = match app.state::<AppState>().active_view() {
        ActiveView::Node => "node",
        ActiveView::Offline => "offline",
        ActiveView::Desktop => return,
    };
    if let Some(webview) = app.get_webview(label) {
        let _ = webview.eval("history.back()");
    }
}

pub fn handle_chrome_action(app: &AppHandle, action: &str) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    match action {
        "home" => show_home(app),
        "back" => navigate_back(app),
        "minimize" => {
            let _ = window.minimize();
        }
        "maximize" => {
            if window.is_maximized().unwrap_or(false) {
                let _ = window.unmaximize();
            } else {
                let _ = window.maximize();
            }
        }
        "hide" => {
            hide_or_close(app);
        }
        "drag" => {
            let _ = window.start_dragging();
        }
        _ => {}
    }
}

pub fn hide_or_close(app: &AppHandle) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    if app.state::<AppState>().tray_available() {
        let _ = window.hide();
    } else {
        let _ = window.close();
    }
}

pub fn install_tray(app: &App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "tray-show", "Открыть", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray-quit", "Выйти", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let icon = app
        .default_window_icon()
        .expect("default window icon is configured")
        .clone();
    TrayIconBuilder::new()
        .icon(icon)
        .tooltip("Netsanctum Client")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-show" => restore_window(app),
            "tray-quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                restore_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn restore_window(app: &AppHandle) {
    if let Some(window) = app.get_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        match app.state::<AppState>().active_view() {
            ActiveView::Desktop => show_home(app),
            ActiveView::Node => {
                let _ = show_content(app, ActiveView::Node);
            }
            ActiveView::Offline => {
                let _ = show_content(app, ActiveView::Offline);
            }
        }
        let _ = window.set_focus();
    }
}
