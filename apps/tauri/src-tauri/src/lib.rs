//! OpenMirasim Tauri v2 Desktop Thin Shell (Phase 3).
//!
//! Provides system tray residency, native OS notifications, OS Keychain token storage,
//! and multi-server profile switching around the shared `apps/web` workbench.

pub mod profiles;

use profiles::{
    ServerProfile, delete_token_from_keychain, load_profiles, load_token_from_keychain,
    remove_profile, save_token_in_keychain, upsert_profile,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;

const TRAY_ID: &str = "openmirasim-tray";
const MAX_NOTIFICATION_TITLE_CHARS: usize = 120;
const MAX_NOTIFICATION_BODY_CHARS: usize = 512;
const NOTIFY_WINDOW: Duration = Duration::from_secs(10);
const MAX_NOTIFICATIONS_PER_WINDOW: usize = 10;

static NOTIFY_TIMESTAMPS: Mutex<VecDeque<Instant>> = Mutex::new(VecDeque::new());

fn profiles_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("failed to resolve app config dir: {e}"))?;
    Ok(dir.join("profiles.json"))
}

fn sanitize_notification_text(raw: &str, max_chars: usize) -> String {
    raw.chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string()
}

fn allow_notification_now() -> bool {
    let now = Instant::now();
    let mut queue = NOTIFY_TIMESTAMPS.lock().unwrap_or_else(|e| e.into_inner());
    while let Some(&front) = queue.front() {
        if now.duration_since(front) >= NOTIFY_WINDOW {
            queue.pop_front();
        } else {
            break;
        }
    }
    if queue.len() >= MAX_NOTIFICATIONS_PER_WINDOW {
        return false;
    }
    queue.push_back(now);
    true
}

#[tauri::command]
async fn list_server_profiles(app: AppHandle) -> Result<Vec<ServerProfile>, String> {
    let path = profiles_path(&app)?;
    tauri::async_runtime::spawn_blocking(move || load_profiles(&path))
        .await
        .map_err(|e| format!("load profiles task failed: {e}"))
}

#[tauri::command]
async fn save_server_profile(
    app: AppHandle,
    profile: ServerProfile,
) -> Result<Vec<ServerProfile>, String> {
    let path = profiles_path(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let (list, evicted) = upsert_profile(&path, profile)?;
        for old_id in evicted {
            let _ = delete_token_from_keychain(&old_id);
        }
        Ok::<_, String>(list)
    })
    .await
    .map_err(|e| format!("save profile task failed: {e}"))?
}

#[tauri::command]
async fn delete_server_profile(
    app: AppHandle,
    profile_id: String,
) -> Result<Vec<ServerProfile>, String> {
    let path = profiles_path(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        delete_token_from_keychain(&profile_id)?;
        remove_profile(&path, &profile_id)
    })
    .await
    .map_err(|e| format!("delete profile task failed: {e}"))?
}

#[tauri::command]
async fn save_profile_token(profile_id: String, token: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || save_token_in_keychain(&profile_id, &token))
        .await
        .map_err(|e| format!("save token task failed: {e}"))?
}

#[tauri::command]
async fn load_profile_token(profile_id: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || load_token_from_keychain(&profile_id))
        .await
        .map_err(|e| format!("load token task failed: {e}"))?
}

#[tauri::command]
async fn delete_profile_token(profile_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || delete_token_from_keychain(&profile_id))
        .await
        .map_err(|e| format!("delete token task failed: {e}"))?
}

#[tauri::command]
fn send_native_notification(app: AppHandle, title: String, body: String) -> Result<(), String> {
    if !allow_notification_now() {
        return Ok(());
    }
    let clean_title = sanitize_notification_text(&title, MAX_NOTIFICATION_TITLE_CHARS);
    let clean_body = sanitize_notification_text(&body, MAX_NOTIFICATION_BODY_CHARS);
    if clean_title.is_empty() && clean_body.is_empty() {
        return Ok(());
    }
    app.notification()
        .builder()
        .title(&clean_title)
        .body(&clean_body)
        .show()
        .map_err(|e| format!("failed to send native notification: {e}"))
}

#[tauri::command]
fn update_tray_badge(app: AppHandle, pending_approvals: u32) -> Result<(), String> {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let tooltip = if pending_approvals > 0 {
            format!("OpenMirasim Workbench — {pending_approvals} 项待审批")
        } else {
            "OpenMirasim Workbench".to_string()
        };
        let _ = tray.set_tooltip(Some(&tooltip));
    }
    Ok(())
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let show_i = MenuItem::with_id(app, "show", "打开工作台", true, None::<&str>)?;
            let hide_i = MenuItem::with_id(app, "hide", "隐藏到系统托盘", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "退出 OpenMirasim", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_i, &hide_i, &quit_i])?;

            let mut tray = TrayIconBuilder::with_id(TRAY_ID)
                .tooltip("OpenMirasim Workbench")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "hide" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.hide();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                });

            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            let _ = tray.build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Keep app resident in system tray on close request so approval notifications keep arriving.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_server_profiles,
            save_server_profile,
            delete_server_profile,
            save_profile_token,
            load_profile_token,
            delete_profile_token,
            send_native_notification,
            update_tray_badge,
        ])
        .run(tauri::generate_context!())
        .expect("error while running OpenMirasim desktop shell");
}
