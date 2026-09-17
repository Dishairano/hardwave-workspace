mod cloudfiles;
mod hydration;
mod api;
mod models;
mod sync;

use models::SyncStatus;
use std::sync::Arc;
use tauri::{
    Manager, State, WebviewWindow,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tokio::sync::Mutex as TokioMutex;

pub struct AppState {
    pub api_token: TokioMutex<Option<String>>,
    pub sync_engine: TokioMutex<Option<Arc<sync::SyncEngine>>>,
    pub auto_sync: TokioMutex<bool>,
}

/// Safely call a frontend function with serializable data.
/// Embeds JSON directly as a JS expression (valid JSON is valid JS).
pub fn safe_eval<T: serde::Serialize>(win: &WebviewWindow, fn_name: &str, data: &T) {
    let Ok(json) = serde_json::to_string(data) else { return };
    let fn_safe: String = fn_name.chars().filter(|c| *c != '\'' && *c != '\\').collect();
    let js = format!(
        "try{{window['{}'] && window['{}']({})}}catch(e){{}}",
        fn_safe, fn_safe, json
    );
    let _ = win.eval(&js);
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

/// Free Up Space from the command line, without starting the interface.
///
/// The tray action hands `free_up_space` the running engine so it can ask the server what it
/// holds. Here there is no engine, so it works from the sync index on disk, which is exactly
/// what the tray action falls back to when the server is unreachable. Everything the index
/// lists as uploaded becomes a placeholder; anything still uploading is left alone.
///
/// The result goes to stdout AND to `%TEMP%\\hardwave-freeup.txt`, because the release build is
/// a windows subsystem binary: started from a shell it has no console to print to.
pub fn free_up_space_cli() {
    #[cfg(target_os = "windows")]
    unsafe {
        // Borrow the calling shell's console so the output is visible when run by hand.
        windows::Win32::System::Console::AttachConsole(
            windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
        )
        .ok();
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            report_free_up(&format!("Could not start: {e}"));
            std::process::exit(1);
        }
    };

    // Windows only lets the process that is CONNECTED to the sync root turn files into
    // placeholders. Without this, every conversion comes back 0x8007017C ("the cloud operation
    // is invalid") and nothing is freed, which is exactly what the first version of this command
    // did on the founder's machine: 95,615 files attempted, 0 freed.
    //
    // Only one process can hold that connection, so the app must be closed while this runs.
    // Hydration requests that arrive meanwhile are refused: nothing should be opening these
    // files with the app shut, and refusing is safer than serving bytes with no token.
    #[cfg(target_os = "windows")]
    let _connection = {
        let root = sync::sync_root();
        let fetcher: hydration::Fetcher = std::sync::Arc::new(|_identity, _offset, _length| {
            Box::pin(async { Err("Hardwave Workspace is not running".to_string()) })
        });
        match runtime.block_on(async { hydration::connect(&root, fetcher) }) {
            Ok(c) => c,
            Err(e) => {
                report_free_up(&format!(
                    "Could not take over Files On-Demand ({e}). Close Hardwave Workspace \
                     (right click the tray icon, Quit) and run this again."
                ));
                std::process::exit(1);
            }
        }
    };

    // No policy switching here: re-registering the root with CF_POPULATION_POLICY_FULL is itself
    // refused with 0x8007017C, so that idea is dead. Run with the root exactly as the app leaves it.
    let message = match runtime.block_on(sync::free_up_space(None)) {
        Ok((0, _)) => "Nothing to free up. Either every synced file is already a placeholder, or \
                       the rest are not on the server yet."
            .to_string(),
        Ok((files, bytes)) => format!(
            "Freed {:.2} GB across {} files. They stay in Explorer and download again when opened.",
            bytes as f64 / 1_073_741_824.0,
            files
        ),
        Err(e) => {
            report_free_up(&format!("Could not free up space: {e}"));
            std::process::exit(1);
        }
    };
    report_free_up(&message);
}

/// Register the Hardwave folder as a cloud sync root again, and say what happened.
pub fn register_sync_root_cli() {
    #[cfg(target_os = "windows")]
    unsafe {
        windows::Win32::System::Console::AttachConsole(
            windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
        )
        .ok();
    }
    let root = sync::sync_root();
    match cloudfiles::register(&root, "Hardwave Workspace") {
        Ok(()) => report_free_up(&format!("Registered {} as a cloud folder again.", root.display())),
        Err(e) => {
            report_free_up(&format!("Could not register {}: {e}", root.display()));
            std::process::exit(1);
        }
    }
}

fn report_free_up(message: &str) {
    println!("{message}");
    let path = std::env::temp_dir().join("hardwave-freeup.txt");
    let _ = std::fs::write(&path, format!("{message}\n"));
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
            *state.api_token.lock().await = Some(token.clone());
            sync_vst_token(Some(token));
            if let Some(engine) = state.sync_engine.lock().await.as_ref() {
                engine.set_token(Some(token.clone())).await;
            }
        }
    }
    Ok(res)
}

#[tauri::command]
async fn logout(state: State<'_, AppState>) -> Result<(), String> {
    let token = state.api_token.lock().await.clone();
    if let Some(t) = token {
        let _ = api::logout(&t).await;
    }
    *state.api_token.lock().await = None;
    sync_vst_token(None);
    if let Some(engine) = state.sync_engine.lock().await.as_ref() {
        engine.set_token(None).await;
    }
    Ok(())
}

#[tauri::command]
async fn get_auth_status(state: State<'_, AppState>) -> Result<bool, String> {
    let token = state.api_token.lock().await.clone();
    match token {
        Some(t) => api::get_auth_status(&t).await,
        None => Ok(false),
    }
}

#[tauri::command]
async fn set_token(token: String, state: State<'_, AppState>) -> Result<(), String> {
    *state.api_token.lock().await = Some(token.clone());
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
            files_total: 0,
            bytes_synced: 0,
            bytes_total: 0,
            current_file: None,
            current_percent: 0,
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
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let token = state.api_token.lock().await.clone()
        .ok_or("Not logged in")?;

    let url = api::get_download_url(&token, &workspace_id, &file_id).await?;

    let downloads = dirs::download_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("Downloads"));
    let _ = std::fs::create_dir_all(&downloads);
    let save_path = downloads.join(&filename);

    let res = api::http_client().get(&url).send().await.map_err(|e| format!("Download failed: {}", e))?;
    if !res.status().is_success() {
        return Err(format!("Download error: {}", res.status()));
    }
    let total = res.content_length().unwrap_or(0);
    let mut stream = res.bytes_stream();
    let mut file = tokio::fs::File::create(&save_path).await.map_err(|e| format!("Create file error: {}", e))?;
    let mut downloaded: u64 = 0;
    let mut last_pct: u64 = 0;

    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Download stream error: {}", e))?;
        file.write_all(&chunk).await.map_err(|e| format!("Write error: {}", e))?;
        downloaded += chunk.len() as u64;
        let pct = (downloaded * 100).checked_div(total).unwrap_or(0);
        if pct != last_pct {
            last_pct = pct;
            if let Some(win) = app.get_webview_window("main") {
                safe_eval(&win, "__HW_DOWNLOAD_PROGRESS__", &serde_json::json!({
                    "pct": pct,
                    "downloaded": downloaded,
                    "total": total,
                    "filename": filename,
                }));
            }
        }
    }
    file.flush().await.map_err(|e| format!("Flush error: {}", e))?;

    Ok(save_path.to_string_lossy().to_string())
}

#[tauri::command]
async fn toggle_auto_sync(enabled: bool, state: State<'_, AppState>) -> Result<(), String> {
    *state.auto_sync.lock().await = enabled;
    if let Some(engine) = state.sync_engine.lock().await.as_ref() {
        if enabled {
            engine.resume().await;
        } else {
            engine.pause().await;
        }
    }
    if let Some(dir) = dirs::data_dir() {
        let pref_path = dir.join("hardwave").join("auto_sync");
        if let Some(parent) = pref_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&pref_path, if enabled { "1" } else { "0" });
    }
    Ok(())
}

/// Open a native folder picker and return the chosen path.
/// Separate from starting the move so the UI can show what was picked and let
/// the user choose a destination before anything is uploaded or deleted.
#[tauri::command]
async fn archive_pick_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    // A std::sync::mpsc recv() here blocks the async executor thread that also
    // has to pump the dialog, so the picker never appeared at all. A oneshot we
    // await yields instead of blocking.
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |p| { let _ = tx.send(p); });
    let picked = rx.await.map_err(|e| e.to_string())?;
    Ok(picked.and_then(|p| p.into_path().ok()).map(|p| p.to_string_lossy().to_string()))
}

/// Workspaces the signed-in user can move files into.
#[tauri::command]
async fn archive_workspaces(state: State<'_, AppState>) -> Result<Vec<api::Workspace>, String> {
    let token = state.api_token.lock().await.clone()
        .ok_or_else(|| "Not signed in".to_string())?;
    api::list_workspaces(&token).await
}

/// Upload a folder then delete the local copies. Progress arrives as
/// `archive-progress` events; the report comes back when it finishes.
#[tauri::command]
async fn archive_start(
    state: State<'_, AppState>,
    path: String,
    workspace_id: Option<String>,
    dest_folder: Option<String>,
) -> Result<sync::ArchiveReport, String> {
    let engine = state.sync_engine.lock().await.clone()
        .ok_or_else(|| "Sign in first".to_string())?;
    sync::archive_folder(engine, std::path::PathBuf::from(path), workspace_id, dest_folder).await
}

#[tauri::command]
async fn get_auto_sync(state: State<'_, AppState>) -> Result<bool, String> {
    // Tauri requires async commands with borrowed inputs to return a Result;
    // the frontend still receives the plain value on success.
    Ok(*state.auto_sync.lock().await)
}

fn load_auto_sync_pref() -> bool {
    dirs::data_dir()
        .map(|d| d.join("hardwave").join("auto_sync"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim() != "0")
        .unwrap_or(true) // default: enabled
}

// ─── Token Bridge ─────────────────────────────────────────────────────────
// Periodically read hw_session cookie from webview and pass to sync engine.

async fn start_token_bridge(handle: tauri::AppHandle) {
    // Five seconds until the session is known, then every five minutes. The
    // token is a JWT that outlives any five-second window, and the old fixed
    // 5 s loop meant one idle app asked the server for it 17,000 times a day.
    // A logged-out app goes back to the short interval, because that is when
    // an answer actually changes something.
    use std::time::Duration;
    const FAST: Duration = Duration::from_secs(5);
    const SLOW: Duration = Duration::from_secs(300);
    loop {
        let have_token = {
            let state = handle.state::<AppState>();
            let token = state.api_token.lock().await;
            token.is_some()
        };
        tokio::time::sleep(if have_token { SLOW } else { FAST }).await;
        if let Some(win) = handle.get_webview_window("main") {
            // The hw_session cookie is httpOnly so JS can't read document.cookie.
            // Instead, fetch the /api/auth/sync-token endpoint which returns the JWT.
            // The browser sends the httpOnly cookie automatically with fetch().
            let js = r#"
                (async function() {
                    try {
                        var res = await fetch('/api/auth/sync-token', { credentials: 'include' });
                        if (!res.ok) return;
                        var data = await res.json();
                        if (data.token && window.__TAURI_INTERNALS__) {
                            var current = window.__HW_SYNCED_TOKEN__;
                            if (current !== data.token) {
                                window.__HW_SYNCED_TOKEN__ = data.token;
                                window.__TAURI_INTERNALS__.invoke('set_token', { token: data.token });
                            }
                        }
                    } catch(e) {}
                })();
            "#;
            let _ = win.eval(js);
        }
    }
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

    if let Some(win) = handle.get_webview_window("main") {
        safe_eval(&win, "__HW_UPDATE__", &serde_json::json!({
            "version": update.version,
            "body": update.body.unwrap_or_default(),
            "currentVersion": env!("CARGO_PKG_VERSION"),
        }));
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
                let _ = win.eval(format!("window.__HW_UPDATE_PROGRESS__ = {};", pct));
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
        // Must be registered first. A second launch focuses the running window
        // instead of starting another copy — two instances meant two tray icons
        // and two sync engines fighting over the same folder.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            use tauri::Manager;
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
        // Info and above, and none of reqwest's per-connection chatter. The log
        // file was 38 KB of "starting new connection" covering four minutes,
        // which is worse than useless: the sync engine's own failures were the
        // one thing it did not record.
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .level_for("reqwest", log::LevelFilter::Warn)
                .level_for("hyper", log::LevelFilter::Warn)
                .level_for("hyper_util", log::LevelFilter::Warn)
                .level_for("rustls", log::LevelFilter::Warn)
                .build(),
        )
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
            let free_i = MenuItem::with_id(app, "free_space", "Free Up Space", true, None::<&str>)?;
            let move_i = MenuItem::with_id(app, "move_folder", "Move Folder to Cloud…", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &sync_i, &free_i, &move_i, &pause_i, &quit_i])?;

            let _tray = TrayIconBuilder::new()
                // Without an icon the tray shows a nameless blank entry that is
                // almost impossible to find among the others.
                .icon(app.default_window_icon().cloned().ok_or("no window icon")?)
                .menu(&menu)
                .tooltip("Hardwave Workspace")
                // Left-click should open the app, the way every other tray app
                // behaves; the menu stays on right-click.
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    match event.id.as_ref() {
                        "open" => {
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                        "move_folder" => {
                            // The destination matters, so this opens the app
                            // rather than guessing a workspace from the tray.
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = win.show();
                                let _ = win.set_focus();
                                let _ = win.eval("window.__HW_OPEN_ARCHIVE__ && window.__HW_OPEN_ARCHIVE__()");
                            }
                        }
                        "free_space" => {
                            // Dehydrating thousands of files is far too slow for
                            // the menu callback thread.
                            let app = app.clone();
                            std::thread::spawn(move || {
                                // The engine carries the signed-in token, which
                                // is what lets this ask the server which files
                                // it already holds instead of trusting an index
                                // that may be far behind.
                                let engine = tauri::async_runtime::block_on(async {
                                    app.state::<AppState>().sync_engine.lock().await.clone()
                                });
                                let msg = match tauri::async_runtime::block_on(sync::free_up_space(engine)) {
                                    Ok((0, _)) => "Nothing to free up. Either every synced file is already a placeholder, or the rest are not on the server yet.".to_string(),
                                    Ok((n, b)) => format!(
                                        "Freed {:.2} GB across {} files. They stay in Explorer and download again when opened.",
                                        b as f64 / 1_073_741_824.0, n),
                                    Err(e) => format!("Could not free up space: {e}"),
                                };
                                eprintln!("[FreeSpace] {msg}");

                                // Report through a native notification. The
                                // webview toast below cannot be the only channel:
                                // window.__HW_TOAST__ is not defined anywhere in
                                // the deployed UI, and the `&&` guard turns that
                                // into silence rather than an error. This is a
                                // tray action, so the window is usually closed
                                // too, and get_webview_window returns None. The
                                // result is that freeing space appeared to do
                                // nothing at all.
                                use tauri_plugin_notification::NotificationExt;
                                if let Err(e) = app.notification()
                                    .builder()
                                    .title("Hardwave Workspace")
                                    .body(&msg)
                                    .show()
                                {
                                    eprintln!("[FreeSpace] notification failed: {e}");
                                }

                                if let Some(win) = app.get_webview_window("main") {
                                    let safe: String =
                                        msg.chars().filter(|c| *c != '\\' && *c != '\'').collect();
                                    let _ = win.eval(format!("window.__HW_TOAST__ && window.__HW_TOAST__('{}')", safe));
                                }
                            });
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
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .build(app)?;

            // Check for updates
            let update_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                check_for_updates(update_handle).await;
            });

            // Start token bridge — reads hw_session cookie from webview
            let bridge_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Wait for webview to load
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                start_token_bridge(bridge_handle).await;
            });

            // Initialize sync engine
            let handle = app.handle().clone();
            let auto_sync_enabled = load_auto_sync_pref();
            tauri::async_runtime::spawn(async move {
                let engine = Arc::new(sync::SyncEngine::new(handle.clone()));

                // Store engine in state FIRST so set_token from bridge finds it
                let app_state = handle.state::<AppState>();
                *app_state.sync_engine.lock().await = Some(Arc::clone(&engine));

                // Load existing auth token (also done via set_token, which calls engine.set_token)
                if let Some(token) = load_saved_token() {
                    api::http_client(); // init shared client early
                    engine.set_token(Some(token)).await;
                }

                if !auto_sync_enabled {
                    engine.pause().await;
                }

                engine.start().await;
            });

            Ok(())
        })
        .manage(AppState {
            api_token: TokioMutex::new(load_saved_token()),
            sync_engine: TokioMutex::new(None),
            auto_sync: TokioMutex::new(load_auto_sync_pref()),
        })
        // Closing the window must not quit the app. The process IS the cloud
        // provider: quitting disconnects it, so sync stops and — worse — every
        // placeholder in Explorer becomes unopenable, because nothing is left
        // to answer the hydration callback. Hide to the tray instead; Quit in
        // the tray menu is the deliberate way out.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
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
            toggle_auto_sync,
            get_auto_sync,
            archive_pick_folder,
            archive_workspaces,
            archive_start,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Hardwave Workspace");
}
