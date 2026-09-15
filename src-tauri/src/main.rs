// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `hardwave-workspace.exe --free-up-space` runs the same Free Up Space the tray menu runs,
    // without a window. It is how a large workspace can be freed over a remote shell, and how
    // support can reclaim space on a machine whose owner cannot reach the tray.
    let flag = |name: &str| std::env::args().skip(1).any(|a| a == name);
    if flag("--free-up-space") {
        hardwave_workspace_lib::free_up_space_cli();
        return;
    }
    // Repairs a machine whose sync root registration has gone missing: without it Windows does
    // not know the folder is a cloud folder, every placeholder in it is inert, and nothing can
    // connect to it. Normally the app registers at start up, but it only does that once its sync
    // loop runs, which needs a signed-in window.
    if flag("--register-sync-root") {
        hardwave_workspace_lib::register_sync_root_cli();
        return;
    }
    hardwave_workspace_lib::run()
}
