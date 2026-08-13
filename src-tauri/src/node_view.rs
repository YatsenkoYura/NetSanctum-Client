use cookie::SameSite;
use tauri::{AppHandle, Emitter, Manager, WebviewBuilder, WebviewUrl, webview::Cookie};

use crate::error::{AppError, AppResult};
use crate::state::{AppState, NodeViewSession};

const NODE_WEBVIEW_LABEL: &str = "node";
const HOME_NAVIGATION_HOST: &str = "desktop.invalid";

pub fn open(app: &AppHandle, state: &AppState) -> AppResult<()> {
    let NodeViewSession {
        node_url,
        cookie_value,
    } = state.node_view_session()?;
    let main_window = app
        .get_window("main")
        .ok_or_else(|| AppError::Internal("главное окно не найдено".into()))?;
    if let Some(existing) = app.get_webview(NODE_WEBVIEW_LABEL) {
        existing
            .close()
            .map_err(|error| AppError::Internal(error.to_string()))?;
    }

    let allowed_origin = node_url.origin().ascii_serialization();
    let script_origin = serde_json::to_string(&allowed_origin)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let initialization_script = format!(
        r#"
        (() => {{
          if (location.origin !== {script_origin}) return;
          Object.defineProperty(window, "__NETSANCTUM_DESKTOP__", {{
            value: Object.freeze({{
              version: 1,
              requestDownload(manifestUrl) {{
                const manifest = new URL(manifestUrl, location.href);
                if (manifest.origin !== location.origin) throw new Error("Manifest must use the node origin");
                const bridge = new URL("https://desktop.invalid/download-package");
                bridge.searchParams.set("manifest", manifest.pathname + manifest.search);
                location.href = bridge.href;
              }}
            }}),
            configurable: false,
            writable: false
          }});
          addEventListener("DOMContentLoaded", () => {{
            document.documentElement.classList.add("has-outpost-bridge");
            window.sendToOutpost = (manifestUrl) => {{
              window.__NETSANCTUM_DESKTOP__.requestDownload(manifestUrl);
              alert("Package sync requested.");
            }};
            const serverNav = document.querySelector("body > nav");
            {{
              const selects = serverNav ? [...serverNav.querySelectorAll("select")] : [];
              const moduleSelect = selects.find((select) =>
                [...select.options].some((option) => option.value.startsWith("/") && !option.value.startsWith("/set-language"))
              );
              const languageSelect = selects.find((select) =>
                [...select.options].some((option) => option.value.startsWith("/set-language"))
              );
              if (serverNav) serverNav.style.display = "none";
              const style = document.createElement("style");
              style.textContent = `
                #netsanctum-desktop-chrome {{ height:40px;display:flex;align-items:stretch;position:sticky;top:0;z-index:2147483647;border-bottom:1px solid #1f3431;background:#050807;color:#74827f;font:700 9px/1 ui-monospace,monospace;letter-spacing:.09em;user-select:none }}
                #netsanctum-desktop-chrome button {{ min-width:40px;padding:0 11px;border:0;border-right:1px solid #162825;background:transparent;color:#778582;font:inherit;text-transform:uppercase;white-space:nowrap;cursor:pointer;box-shadow:none }}
                #netsanctum-desktop-chrome button:hover,#netsanctum-desktop-chrome button.active {{ color:#2dd4bf;background:#0d1c19 }}
                #netsanctum-desktop-chrome .nsd-brand {{ display:flex;align-items:center;padding:0 14px;border-right:1px solid #162825;color:#2dd4bf;letter-spacing:.14em }}
                #netsanctum-desktop-chrome .nsd-modules {{ display:flex;min-width:0;overflow-x:auto;scrollbar-width:none }}
                #netsanctum-desktop-chrome .nsd-drag {{ flex:1;min-width:24px }}
                #netsanctum-desktop-chrome .nsd-controls {{ display:flex;margin-left:auto }}
                #netsanctum-desktop-chrome .nsd-controls button {{ border-right:0;border-left:1px solid #162825 }}
                #netsanctum-desktop-chrome .nsd-close:hover {{ color:#fb7185;background:#211011 }}
              `;
              document.head.appendChild(style);
              const chrome = document.createElement("div");
              chrome.id = "netsanctum-desktop-chrome";
              const action = (name) => {{
                location.href = `https://desktop.invalid/chrome-action?action=${{encodeURIComponent(name)}}`;
              }};
              const addButton = (parent, text, title, callback, className = "") => {{
                const button = document.createElement("button");
                button.type = "button";
                button.textContent = text;
                button.title = title;
                button.className = className;
                button.addEventListener("click", callback);
                parent.appendChild(button);
                return button;
              }};
              addButton(chrome, "←", "Назад", () => action("back"));
              addButton(chrome, "NS", "Desktop Home", () => action("home"));
              const brand = document.createElement("div");
              brand.className = "nsd-brand";
              brand.textContent = "NETSANCTUM";
              chrome.appendChild(brand);
              const modules = document.createElement("div");
              modules.className = "nsd-modules";
              if (moduleSelect) {{
                [...moduleSelect.options]
                  .filter((option) => option.value.startsWith("/") && !option.value.startsWith("/set-language"))
                  .forEach((option) => {{
                    const button = addButton(modules, option.textContent.trim(), option.textContent.trim(), () => location.href = option.value);
                    if (location.pathname.startsWith(option.value)) button.classList.add("active");
                  }});
              }}
              chrome.appendChild(modules);
              const drag = document.createElement("div");
              drag.className = "nsd-drag";
              drag.addEventListener("mousedown", (event) => {{
                if (event.button === 0) action("drag");
              }});
              drag.addEventListener("dblclick", () => action("maximize"));
              chrome.appendChild(drag);
              const controls = document.createElement("div");
              controls.className = "nsd-controls";
              if (languageSelect) {{
                const language = languageSelect.selectedOptions[0]?.textContent.trim().toUpperCase() || "EN";
                addButton(controls, language, "Сменить язык", () => location.href = `/set-language?lang=${{language === "RU" ? "en" : "ru"}}`);
              }}
              addButton(controls, "−", "Свернуть", () => action("minimize"));
              addButton(controls, "□", "Развернуть", () => action("maximize"));
              addButton(controls, "×", "Скрыть в tray", () => action("hide"), "nsd-close");
              chrome.appendChild(controls);
              document.body.prepend(chrome);
            }}
          }}, {{ once: true }});
        }})();
        "#
    );

    let navigation_app = app.clone();
    let navigation_origin = allowed_origin.clone();
    let builder = WebviewBuilder::new(
        NODE_WEBVIEW_LABEL,
        WebviewUrl::External("about:blank".parse().expect("valid static URL")),
    )
    .initialization_script(initialization_script)
    .on_navigation(move |url| {
        if url.host_str() == Some(HOME_NAVIGATION_HOST) {
            if url.path() == "/chrome-action" {
                if let Some(action) = url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "action").then(|| value.into_owned()))
                {
                    crate::window_chrome::handle_chrome_action(&navigation_app, &action);
                }
                return false;
            }
            if url.path() == "/download-package" {
                let manifest_url = url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "manifest").then(|| value.into_owned()));
                if let Some(manifest_url) = manifest_url {
                    match crate::downloads::enqueue(&navigation_app, manifest_url) {
                        Ok(true) => {}
                        Ok(false) => eprintln!("[downloads] package request is already active"),
                        Err(error) => eprintln!("[downloads] could not queue package: {error}"),
                    }
                }
                return false;
            }
            crate::window_chrome::show_home(&navigation_app);
            return false;
        }
        let is_node_origin = url.origin().ascii_serialization() == navigation_origin;
        if is_node_origin && url.path().ends_with("/auth/login-page") {
            expire_session(&navigation_app);
            return false;
        }
        url.as_str() == "about:blank" || is_node_origin
    });

    let node_webview = main_window
        .add_child(
            builder,
            tauri::LogicalPosition::new(0, 0),
            tauri::LogicalSize::new(1, 1),
        )
        .map_err(|error| AppError::Internal(error.to_string()))?;
    let host = node_url
        .host_str()
        .ok_or_else(|| AppError::InvalidNodeUrl("адрес не содержит host".into()))?;
    let cookie = Cookie::build(("access_token", cookie_value.as_str()))
        .domain(host)
        .path("/")
        .http_only(true)
        .secure(node_url.scheme() == "https")
        .same_site(SameSite::Lax)
        .build();
    node_webview
        .set_cookie(cookie)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    node_webview
        .navigate(node_url)
        .map_err(|error| AppError::Internal(error.to_string()))?;
    crate::window_chrome::show_content(app, crate::state::ActiveView::Node)?;
    Ok(())
}

fn expire_session(app: &AppHandle) {
    let connection = app.state::<AppState>().lock_vault();
    crate::window_chrome::show_home(app);
    let _ = app.emit_to("main", "node-session-expired", connection);
}
