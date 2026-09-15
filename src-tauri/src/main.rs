// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `hardwave-workspace.exe --free-up-space` runs the same Free Up Space the tray menu runs,
    // without a window. It is how a large workspace can be freed over a remote shell, and how
    // support can reclaim space on a machine whose owner cannot reach the tray.
    if std::env::args().skip(1).any(|a| a == "--free-up-space") {
        hardwave_workspace_lib::free_up_space_cli();
        return;
    }
    hardwave_workspace_lib::run()
}
