mod api;
mod models;
mod sync;

use models::SyncStatus;
use std::sync::Arc;
use tauri::{
    Emitter, Manager, State,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tokio::sync::Mutex as TokioMutex;

pub struct AppState {
    pub api_token: std::sync::Mutex<Option<String>>,
    pub sync_engine: TokioMutex<Option<Arc<sync::SyncEngine>>>,
}

/// Shared auth token path — same as VSTs and Suite.
fn shared_token_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("hardwave").join("auth_token"))
}

fn sync_vst_token(token: Option<&str>) {
    if let Some(path) = shared_token_path() {
        match token {
            Some(t) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&path, t);
            }
            None => {
                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

fn load_saved_token() -> Option<String> {
    shared_token_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ─── Tauri Commands ────────────────────────────────────────────────────────

#[tauri::command]
async fn login(
    email: String,
    password: String,
    state: State<'_, AppState>,
) -> Result<models::AuthResponse, String> {
    let res = api::login(&email, &password).await?;
    if res.success {
        if let Some(ref token) = res.token {
            *state.api_token.lock().unwrap() = Some(token.clone());
            sync_vst_token(Some(token));
            // Update sync engine token
            if let Some(engine) = state.sync_engine.lock().await.as_ref() {
                engine.set_token(Some(token.clone())).await;
            }
        }
    }
    Ok(res)
}

#[tauri::command]
async fn logout(state: State<'_, AppState>) -> Result<(), String> {
    let token = state.api_token.lock().unwrap().clone();
    if let Some(t) = token {
        let _ = api::logout(&t).await;
    }
    *state.api_token.lock().unwrap() = None;
    sync_vst_token(None);
    if let Some(engine) = state.sync_engine.lock().await.as_ref() {
        engine.set_token(None).await;
    }
    Ok(())
}

#[tauri::command]
async fn get_auth_status(state: State<'_, AppState>) -> Result<bool, String> {
    let token = state.api_token.lock().unwrap().clone();
    match token {
        Some(t) => api::get_auth_status(&t).await,
        None => Ok(false),
    }
}

#[tauri::command]
async fn set_token(token: String, state: State<'_, AppState>) -> Result<(), String> {
    *state.api_token.lock().unwrap() = Some(token.clone());
    sync_vst_token(Some(&token));
    if let Some(engine) = state.sync_engine.lock().await.as_ref() {
        engine.set_token(Some(token)).await;
    }
    Ok(())
}

#[tauri::command]
async fn get_sync_status(state: State<'_, AppState>) -> Result<SyncStatus, String> {
    if let Some(engine) = state.sync_engine.lock().await.as_ref() {
        Ok(engine.get_status().await)
    } else {
        Ok(SyncStatus {
            state: "idle".into(),
            files_pending: 0,
            files_synced: 0,
            last_sync: None,
            error: None,
        })
    }
}

#[tauri::command]
async fn pause_sync(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(engine) = state.sync_engine.lock().await.as_ref() {
        engine.pause().await;
    }
    Ok(())
}

#[tauri::command]
async fn resume_sync(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(engine) = state.sync_engine.lock().await.as_ref() {
        engine.resume().await;
    }
    Ok(())
}

#[tauri::command]
fn get_sync_folder() -> String {
    sync::sync_root().to_string_lossy().to_string()
}

#[tauri::command]
fn open_sync_folder() -> Result<(), String> {
    let root = sync::sync_root();
    let _ = std::fs::create_dir_all(&root);

    #[cfg(target_os = "windows")]
    std::process::Command::new("explorer")
        .arg(&root)
        .spawn()
        .map_err(|e| e.to_string())?;

    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .arg(&root)
        .spawn()
        .map_err(|e| e.to_string())?;

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    std::process::Command::new("xdg-open")
        .arg(&root)
        .spawn()
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
async fn download_file(
    workspace_id: String,
    file_id: String,
    filename: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let token = state.api_token.lock().unwrap().clone()
        .ok_or("Not logged in")?;

    // Get presigned download URL from API
    let url = api::get_download_url(&token, &workspace_id, &file_id).await?;

    // Pick save location — use the system Downloads folder
    let downloads = dirs::download_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("Downloads"));
    let _ = std::fs::create_dir_all(&downloads);
    let save_path = downloads.join(&filename);

    // Download the file bytes
    let client = reqwest::Client::new();
    let res = client.get(&url).send().await.map_err(|e| format!("Download failed: {}", e))?;
    if !res.status().is_success() {
        return Err(format!("Download error: {}", res.status()));
    }
    let bytes = res.bytes().await.map_err(|e| format!("Read error: {}", e))?;
    std::fs::write(&save_path, &bytes).map_err(|e| format!("Save error: {}", e))?;

    Ok(save_path.to_string_lossy().to_string())
}

// ─── Update Check ─────────────────────────────────────────────────────────

#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn check_for_updates(handle: tauri::AppHandle) {
    use tauri_plugin_updater::UpdaterExt;

    let updater = match handle.updater() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[Workspace] Failed to get updater: {}", e);
            return;
        }
    };

    let update = match updater.check().await {
        Ok(Some(update)) => update,
        Ok(None) => return,
        Err(e) => {
            eprintln!("[Workspace] Update check failed: {}", e);
            return;
        }
    };

    // Set window variable so the frontend modal picks it up
    let version = update.version.clone();
    let body = update.body.clone().unwrap_or_default().replace('\\', "\\\\").replace('\'', "\\'").replace('\n', "\\n");
    let current = env!("CARGO_PKG_VERSION");
    if let Some(win) = handle.get_webview_window("main") {
        let js = format!(
            "window.__HW_UPDATE__ = {{ version: '{}', body: '{}', currentVersion: '{}' }};",
            version, body, current
        );
        let _ = win.eval(&js);
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
async fn install_update(handle: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    let updater = handle.updater().map_err(|e| e.to_string())?;
    let update = updater.check().await.map_err(|e| e.to_string())?
        .ok_or("No update available")?;

    let h = handle.clone();
    update.download_and_install(
        move |downloaded, total| {
            let pct = total.map(|t| (downloaded as f64 / t as f64 * 100.0) as u32).unwrap_or(0);
            if let Some(win) = h.get_webview_window("main") {
                let _ = win.eval(&format!("window.__HW_UPDATE_PROGRESS__ = {};", pct));
            }
        },
        || {},
    ).await.map_err(|e| format!("Install failed: {}", e))?;

    handle.restart();
}

// ─── App Entry ─────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                app.handle().plugin(tauri_plugin_updater::Builder::new().build())?;
                app.handle().plugin(tauri_plugin_process::init())?;
                app.handle().plugin(tauri_plugin_dialog::init())?;
            }

            // Build tray icon
            let open_i = MenuItem::with_id(app, "open", "Open Workspace", true, None::<&str>)?;
            let sync_i = MenuItem::with_id(app, "sync_folder", "Open Sync Folder", true, None::<&str>)?;
            let pause_i = MenuItem::with_id(app, "pause", "Pause Sync", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &sync_i, &pause_i, &quit_i])?;

            let _tray = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("Hardwave Workspace")
                .on_menu_event(|app, event| {
                    match event.id.as_ref() {
                        "open" => {
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                        "sync_folder" => {
                            let root = sync::sync_root();
                            let _ = std::fs::create_dir_all(&root);
                            #[cfg(target_os = "windows")]
                            { let _ = std::process::Command::new("explorer").arg(&root).spawn(); }
                            #[cfg(target_os = "macos")]
                            { let _ = std::process::Command::new("open").arg(&root).spawn(); }
                            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                            { let _ = std::process::Command::new("xdg-open").arg(&root).spawn(); }
                        }
                        "pause" => {
                            let handle = app.clone();
                            tauri::async_runtime::spawn(async move {
                                let app_state = handle.state::<AppState>();
                                let guard = app_state.sync_engine.lock().await;
                                if let Some(engine) = guard.as_ref() {
                                    engine.pause().await;
                                }
                            });
                        }
                        "quit" => {
                            std::process::exit(0);
                        }
                        _ => {}
                    }
                })
                .build(app)?;

            // Check for updates
            let update_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Wait a few seconds for the app to settle
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                check_for_updates(update_handle).await;
            });

            // Initialize sync engine
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let engine = Arc::new(sync::SyncEngine::new(handle.clone()));

                // Load existing auth token
                if let Some(token) = load_saved_token() {
                    engine.set_token(Some(token)).await;
                }

                // Store engine in state
                let app_state = handle.state::<AppState>();
                *app_state.sync_engine.lock().await = Some(Arc::clone(&engine));

                // Start the sync loop
                engine.start().await;
            });

            Ok(())
        })
        .manage(AppState {
            api_token: std::sync::Mutex::new(load_saved_token()),
            sync_engine: TokioMutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            login,
            logout,
            get_auth_status,
            set_token,
            get_sync_status,
            pause_sync,
            resume_sync,
            get_sync_folder,
            open_sync_folder,
            download_file,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Hardwave Workspace");
}
