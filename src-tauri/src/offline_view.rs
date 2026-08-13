use cookie::SameSite;
use tauri::{AppHandle, Manager, WebviewBuilder, WebviewUrl, webview::Cookie};

use crate::error::{AppError, AppResult};
use crate::offline_gateway::{OfflineAccess, SESSION_COOKIE};
use crate::state::AppState;

const OFFLINE_WEBVIEW_LABEL: &str = "offline";
const HOME_NAVIGATION_HOST: &str = "desktop.invalid";

pub async fn open(
    app: &AppHandle,
    state: &AppState,
    node_origin: String,
    package_id: String,
) -> AppResult<()> {
    let root_url = state
        .library()
        .package_root_url(&node_origin, &package_id)?;
    if !root_url.starts_with('/') || root_url.starts_with("//") || root_url.contains('#') {
        return Err(AppError::InvalidPackage("invalid offline root URL".into()));
    }
    let access = state
        .offline_gateway()
        .await?
        .activate(node_origin, package_id)
        .await?;
    open_access(app, root_url, access).await
}

pub async fn open_module(
    app: &AppHandle,
    state: &AppState,
    node_origin: String,
    module_id: String,
) -> AppResult<()> {
    let (root_url, package_ids) = state
        .library()
        .module_offline_info(&node_origin, &module_id)?;
    let access = state
        .offline_gateway()
        .await?
        .activate_module(node_origin, package_ids)
        .await?;
    open_access(app, root_url, access).await
}

async fn open_access(app: &AppHandle, root_url: String, access: OfflineAccess) -> AppResult<()> {
    if !root_url.starts_with('/') || root_url.starts_with("//") || root_url.contains('#') {
        return Err(AppError::InvalidPackage("invalid offline root URL".into()));
    }
    let target = format!("{}{}", access.origin, root_url)
        .parse()
        .map_err(|error: url::ParseError| AppError::Internal(error.to_string()))?;
    let allowed_origin = access.origin.clone();
    let main_window = app
        .get_window("main")
        .ok_or_else(|| AppError::Internal("главное окно не найдено".into()))?;
    if let Some(existing) = app.get_webview(OFFLINE_WEBVIEW_LABEL) {
        existing
            .close()
            .map_err(|error| AppError::Internal(error.to_string()))?;
    }
    let navigation_app = app.clone();
    let initialization_script = r#"
      (() => {
        addEventListener("DOMContentLoaded", () => {
          const serverNav = document.querySelector("body > nav");
          if (serverNav) serverNav.style.display = "none";
          const style = document.createElement("style");
          style.textContent = `
            #netsanctum-desktop-chrome { height:40px;display:flex;align-items:stretch;position:sticky;top:0;z-index:2147483647;border-bottom:1px solid #1f3431;background:#050807;color:#74827f;font:700 9px/1 ui-monospace,monospace;letter-spacing:.09em;user-select:none }
            #netsanctum-desktop-chrome button { min-width:40px;padding:0 11px;border:0;border-right:1px solid #162825;background:transparent;color:#778582;font:inherit;text-transform:uppercase;cursor:pointer;box-shadow:none }
            #netsanctum-desktop-chrome button:hover { color:#2dd4bf;background:#0d1c19 }
            #netsanctum-desktop-chrome .nsd-label { display:flex;align-items:center;padding:0 14px;border-right:1px solid #162825;color:#2dd4bf;letter-spacing:.14em }
            #netsanctum-desktop-chrome .nsd-drag { flex:1 }
            #netsanctum-desktop-chrome .nsd-controls { display:flex }
            #netsanctum-desktop-chrome .nsd-controls button { border-right:0;border-left:1px solid #162825 }
            #netsanctum-desktop-chrome .nsd-close:hover { color:#fb7185;background:#211011 }
          `;
          document.head.appendChild(style);
          const chrome = document.createElement("div");
          chrome.id = "netsanctum-desktop-chrome";
          const action = (name) => location.href = `https://desktop.invalid/chrome-action?action=${encodeURIComponent(name)}`;
          const button = (text, title, callback, className = "") => {
            const element = document.createElement("button");
            element.type = "button";
            element.textContent = text;
            element.title = title;
            element.className = className;
            element.addEventListener("click", callback);
            return element;
          };
          chrome.append(button("←", "Назад", () => action("back")));
          chrome.append(button("NS", "Desktop Home", () => action("home")));
          const label = document.createElement("div");
          label.className = "nsd-label";
          label.textContent = "OFFLINE PACKAGE";
          chrome.append(label);
          const drag = document.createElement("div");
          drag.className = "nsd-drag";
          drag.addEventListener("mousedown", (event) => { if (event.button === 0) action("drag"); });
          drag.addEventListener("dblclick", () => action("maximize"));
          chrome.append(drag);
          const controls = document.createElement("div");
          controls.className = "nsd-controls";
          controls.append(button("−", "Свернуть", () => action("minimize")));
          controls.append(button("□", "Развернуть", () => action("maximize")));
          controls.append(button("×", "Скрыть в tray", () => action("hide"), "nsd-close"));
          chrome.append(controls);
          document.body.prepend(chrome);
        }, { once: true });
      })();
    "#;
    let builder = WebviewBuilder::new(
        OFFLINE_WEBVIEW_LABEL,
        WebviewUrl::External("about:blank".parse().expect("valid static URL")),
    )
    .incognito(true)
    .initialization_script(initialization_script)
    .on_navigation(move |url| {
        if url.host_str() == Some(HOME_NAVIGATION_HOST) {
            if url.path() == "/chrome-action"
                && let Some(action) = url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "action").then(|| value.into_owned()))
            {
                crate::window_chrome::handle_chrome_action(&navigation_app, &action);
            } else {
                crate::window_chrome::show_home(&navigation_app);
            }
            return false;
        }
        url.as_str() == "about:blank" || url.origin().ascii_serialization() == allowed_origin
    });
    let offline_webview = main_window
        .add_child(
            builder,
            tauri::LogicalPosition::new(0, 0),
            tauri::LogicalSize::new(1, 1),
        )
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let cookie = Cookie::build((SESSION_COOKIE, access.token))
        .domain("127.0.0.1")
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .build();
    offline_webview
        .set_cookie(cookie)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    offline_webview
        .navigate(target)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    if let Some(node) = app.get_webview("node") {
        let _ = node.hide();
    }
    crate::window_chrome::show_content(app, crate::state::ActiveView::Offline)?;
    Ok(())
}
