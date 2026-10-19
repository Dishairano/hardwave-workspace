//! Windows uninstall support.
//!
//! - `register_uninstaller`: Writes the Add/Remove Programs entry under
//!   HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\HardwaveWorkspace
//!   with a DisplayName, Publisher, UninstallString, and icon.
//!
//! - `start_uninstall`: Uninstall mode with the window. Runs the steps the
//!   user confirmed on the first screen (`removal::plan`) and reports each one
//!   to the window as `uninstall://progress`.
//!
//! - `run_quiet`: `--uninstall --quiet`. Removes the app (never anything in
//!   the Workspace folder) without a window, for scripts.
//!
//! The steps read the install directory out of the registry, wait for the
//! app's uploads and delete the downloaded copies when asked, close a running
//! Workspace app, delete its files and shortcuts, and clear the registry entries.

use crate::removal::{self, Failure, Step, Stopped};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use tauri::{AppHandle, Emitter, Manager};
use winreg::enums::*;
use winreg::RegKey;

const UNINSTALL_KEY_PATH: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\HardwaveWorkspace";
const INSTALL_KEY_PATH: &str = r"Software\Hardwave Studios\Hardwave Workspace";
const UNINSTALLER_EXE_NAME: &str = "Hardwave Workspace Uninstaller.exe";

/// How long to wait between two counts of the files still uploading.
const UPLOAD_POLL: std::time::Duration = std::time::Duration::from_secs(3);
/// Counts in a row without one more file uploaded before the uninstall stops
/// and says how many are left: 30 counts, about a minute and a half.
const UPLOAD_PATIENCE: u32 = 30;

/// Writes the uninstall registry entry so Workspace appears in Add/Remove Programs.
/// Also copies the running installer into the install directory as
/// `Hardwave Workspace Uninstaller.exe` so the UninstallString is stable.
pub fn register_uninstaller(install_dir: &Path, app_exe: &Path) -> Result<(), String> {
    // 1. Copy self as the uninstaller binary
    let current_exe =
        std::env::current_exe().map_err(|e| format!("Cannot find installer exe: {e}"))?;
    let uninstaller_path = install_dir.join(UNINSTALLER_EXE_NAME);
    if current_exe != uninstaller_path {
        std::fs::copy(&current_exe, &uninstaller_path)
            .map_err(|e| format!("Cannot copy uninstaller: {e}"))?;
    }

    // 2. Write the uninstall entry
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (uninstall_key, _) = hkcu
        .create_subkey(UNINSTALL_KEY_PATH)
        .map_err(|e| format!("Cannot create uninstall key: {e}"))?;

    let install_dir_str = install_dir.to_string_lossy().to_string();
    let app_exe_str = app_exe.to_string_lossy().to_string();
    let uninstall_cmd = format!("\"{}\" --uninstall", uninstaller_path.display());
    let quiet_uninstall_cmd = format!("{uninstall_cmd} --quiet");

    set_str(&uninstall_key, "DisplayName", "Hardwave Workspace")?;
    set_str(&uninstall_key, "Publisher", "Hardwave Studios")?;
    set_str(
        &uninstall_key,
        "DisplayIcon",
        &format!("\"{}\"", app_exe_str),
    )?;
    set_str(&uninstall_key, "InstallLocation", &install_dir_str)?;
    set_str(&uninstall_key, "UninstallString", &uninstall_cmd)?;
    set_str(&uninstall_key, "QuietUninstallString", &quiet_uninstall_cmd)?;
    set_str(
        &uninstall_key,
        "URLInfoAbout",
        "https://hardwavestudios.com",
    )?;
    set_str(
        &uninstall_key,
        "HelpLink",
        "https://hardwavestudios.com/support",
    )?;
    set_dword(&uninstall_key, "NoModify", 1)?;
    set_dword(&uninstall_key, "NoRepair", 1)?;

    // Estimated size (in KB) — rough, based on the install dir.
    let size_kb = folder_size_kb(install_dir).unwrap_or(200_000);
    set_dword(&uninstall_key, "EstimatedSize", size_kb as u32)?;

    // Also write a key Hardwave can query to know where Workspace is installed.
    let (install_key, _) = hkcu
        .create_subkey(INSTALL_KEY_PATH)
        .map_err(|e| format!("Cannot create install key: {e}"))?;
    set_str(&install_key, "InstallLocation", &install_dir_str)?;
    set_str(&install_key, "Executable", &app_exe_str)?;

    Ok(())
}

/// The uninstall window's state (managed only in uninstall mode on Windows).
#[derive(Default)]
pub struct UninstallState {
    /// Read from the registry once, so "Try again" after a run that stopped
    /// half-way still knows the folder.
    install_dir: OnceLock<Option<PathBuf>>,
    /// Set by a finished uninstall; removed once this process has exited.
    cleanup_dir: Mutex<Option<PathBuf>>,
}

/// What the window hears before each step, while a long step runs, and when
/// all are done.
#[derive(Clone, Serialize)]
struct Progress<'a> {
    steps: &'a [Step],
    done: usize,
    /// What the running step is doing ("12 files still uploading"), if anything.
    detail: Option<String>,
}

/// The Workspace folder, for the option on the first screen. Reads only;
/// nothing is removed before "Remove".
#[tauri::command]
pub fn uninstall_overview() -> String {
    workspace_folder()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Runs the uninstall the user confirmed with "Remove". Emits
/// `uninstall://progress` before each step, while a long step runs, and when
/// all are done.
#[tauri::command]
pub async fn start_uninstall(app: AppHandle, delete_local_copies: bool) -> Result<(), Failure> {
    tauri::async_runtime::spawn_blocking(move || run_with_window(&app, delete_local_copies))
        .await
        .unwrap_or_else(|e| {
            Err(Failure {
                step: None,
                reason: format!("The uninstaller stopped unexpectedly: {e}"),
                still_uploading: None,
            })
        })
}

fn run_with_window(app: &AppHandle, delete_local_copies: bool) -> Result<(), Failure> {
    let state = app.state::<UninstallState>();
    let install_dir = state
        .install_dir
        .get_or_init(install_dir_on_record)
        .clone()
        .ok_or_else(no_record)?;
    let steps = removal::plan(delete_local_copies);
    let done_now = std::cell::Cell::new(0);
    let emit = |done: usize, detail: Option<String>| {
        let _ = app.emit(
            "uninstall://progress",
            Progress {
                steps: &steps,
                done,
                detail,
            },
        );
    };
    removal::run_plan(
        &steps,
        |step| {
            run_step(step, &install_dir, &|detail: String| {
                emit(done_now.get(), Some(detail))
            })
        },
        |done| {
            done_now.set(done);
            emit(done, None);
        },
    )?;
    // The folder still holds this running exe, so it goes once the window
    // has closed and this process has exited.
    if install_dir.is_dir() {
        *state
            .cleanup_dir
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(install_dir);
    }
    Ok(())
}

/// Called when the window app exits: starts the removal of the install folder
/// a finished uninstall left behind (it held this running exe).
pub fn cleanup_after_exit(app: &AppHandle) {
    let Some(state) = app.try_state::<UninstallState>() else {
        return;
    };
    let dir = state
        .cleanup_dir
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(dir) = dir {
        schedule_delayed_cleanup(&dir);
    }
}

/// `--uninstall --quiet`: removes the app, never anything in the Workspace
/// folder, without a window. Returns the process exit code.
pub fn run_quiet() -> i32 {
    let result = install_dir_on_record()
        .ok_or_else(no_record)
        .and_then(|dir| {
            removal::run_plan(
                &removal::plan(false),
                |step| run_step(step, &dir, &|_: String| {}),
                |_| {},
            )?;
            Ok(dir)
        });
    match result {
        Ok(dir) => {
            // The folder may still hold this running exe, so it is removed by
            // a detached cleanup once this process has exited.
            if dir.is_dir() {
                schedule_delayed_cleanup(&dir);
            }
            0
        }
        Err(failure) => {
            eprintln!("Uninstall failed: {}", failure.reason);
            1
        }
    }
}

/// Without a recorded install folder nothing is removed, not even the shortcuts.
fn no_record() -> Failure {
    Failure {
        step: None,
        reason: "Windows has no record of where Hardwave Workspace is installed for this user, \
                 so there is nothing to remove."
            .to_string(),
        still_uploading: None,
    }
}

/// The install folder Windows has on record for this user, if any.
fn install_dir_on_record() -> Option<PathBuf> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    read_string(&hkcu, UNINSTALL_KEY_PATH, "InstallLocation")
        .or_else(|| read_string(&hkcu, INSTALL_KEY_PATH, "InstallLocation"))
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

/// The Workspace folder, where the app keeps it (`sync::sync_root` in the app).
fn workspace_folder() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Hardwave"))
}

/// The Workspace folder and the app's record of what it has uploaded.
fn workspace_paths() -> Result<(PathBuf, PathBuf), String> {
    let root =
        workspace_folder().ok_or("Cannot find your Workspace folder, so no file was deleted.")?;
    let index = dirs::data_dir()
        .map(|d| d.join("hardwave").join("workspace-sync-index.json"))
        .ok_or("Cannot find the Workspace record of uploaded files, so no file was deleted.")?;
    Ok((root, index))
}

/// Carries out one step. Every step can run again after a failed run.
/// `detail` tells the window what a long step is doing.
fn run_step(step: Step, install_dir: &Path, detail: &dyn Fn(String)) -> Result<(), Stopped> {
    match step {
        Step::WaitForUploads => wait_for_uploads(install_dir, detail),
        Step::CloseApp if install_dir.is_dir() => {
            close_processes_in(install_dir).map_err(Stopped::from)
        }
        Step::DeleteLocalCopies => delete_local_copies(detail),
        // Everything except the running uninstaller goes now, so a locked
        // file is reported instead of silently left behind.
        Step::RemoveInstallFolder if install_dir.is_dir() => {
            check_safe_to_delete(install_dir)?;
            let self_exe = std::env::current_exe().ok();
            remove_folder_contents(install_dir, self_exe.as_deref()).map_err(Stopped::from)
        }
        // The folder is gone already: nothing to close or remove in it.
        Step::CloseApp | Step::RemoveInstallFolder => Ok(()),
        Step::RemoveShortcuts => {
            crate::shortcut::remove_shortcuts();
            Ok(())
        }
        Step::RemoveRegistryKeys => {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            delete_key(&hkcu, UNINSTALL_KEY_PATH)?;
            delete_key(&hkcu, INSTALL_KEY_PATH).map_err(Stopped::from)
        }
    }
}

/// Waits until the running app has uploaded every file in the Workspace
/// folder. Stops with the count when the app is not running, or when no more
/// file has finished uploading for a while.
fn wait_for_uploads(install_dir: &Path, detail: &dyn Fn(String)) -> Result<(), Stopped> {
    let (root, index_path) = workspace_paths()?;
    removal::wait_for_uploads(
        || {
            // The app may be writing its index at this moment; a half-written
            // one counts every file as not uploaded, and the next count is right.
            let index = removal::read_index(&index_path).unwrap_or_default();
            removal::not_uploaded(&removal::local_files(&root), &index)
        },
        || install_dir.is_dir() && app_running_in(install_dir),
        || std::thread::sleep(UPLOAD_POLL),
        |n| detail(format!("{} still uploading", files(n))),
        UPLOAD_PATIENCE,
    )
    .map_err(|n| Stopped {
        reason: format!(
            "Hardwave Workspace has not finished uploading {} from {}, so nothing was removed.\n\n\
             Open Hardwave Workspace and leave it open until they are uploaded, then try again. \
             To keep the files on this PC instead, close this window and uninstall again with \
             the option off.",
            files(n),
            root.display()
        ),
        still_uploading: Some(n),
    })
}

/// Deletes the downloaded copies in the Workspace folder: only files whose
/// bytes are exactly what the app uploaded. Anything else stays and stops the
/// uninstall with how many files that is.
fn delete_local_copies(detail: &dyn Fn(String)) -> Result<(), Stopped> {
    let (root, index_path) = workspace_paths()?;
    let index = removal::read_index(&index_path)?;
    let files_here = removal::local_files(&root);
    let total = files_here.len();
    let outcome = removal::delete_uploaded_copies(&files_here, &index, |n| {
        if n % 25 == 0 || n == total {
            detail(format!("Checked {n} of {}", files(total)));
        }
    });
    // Also after a stop: the copies deleted so far must come back with a
    // later install.
    removal::forget_missing(&index_path, &root)?;

    if !outcome.not_uploaded.is_empty() {
        let n = outcome.not_uploaded.len();
        return Err(Stopped {
            reason: format!(
                "Hardwave Workspace has not uploaded {} in {} (or they changed since the \
                 upload), so they were kept: {}. The downloaded copies of the other {} were \
                 deleted.\n\n\
                 Open Hardwave Workspace, let it upload them, then try again.",
                files(n),
                root.display(),
                some_names(&outcome.not_uploaded),
                files(outcome.deleted.len())
            ),
            still_uploading: Some(n),
        });
    }
    if !outcome.in_use.is_empty() {
        return Err(format!(
            "Some downloaded copies are open in another program and were not deleted: {}.\n\n\
             Close your DAW or the program using them, then try again.",
            some_names(&outcome.in_use)
        )
        .into());
    }
    Ok(())
}

/// "1 file" or "12 files".
fn files(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

/// Up to five of `names`, and how many more there are.
fn some_names(names: &[String]) -> String {
    let shown = names.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    match names.len().saturating_sub(5) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

/// The script's exit code when a process from the folder is still running.
const STILL_RUNNING: i32 = 3;

/// Finds the processes whose executable lives in `dir` (the Workspace app),
/// except this uninstaller. With `close`, asks their windows to close first,
/// then ends whatever is still running after a few seconds. Returns whether
/// one is running at the end; `None` when PowerShell could not run.
fn processes_in(dir: &Path, close: bool) -> Option<bool> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    // Paths go in through environment variables, so no quoting of the path
    // into the script is needed.
    const SCRIPT: &str = r#"
$dir = $env:HW_UNINSTALL_DIR.TrimEnd('\') + '\'
$self = [int]$env:HW_UNINSTALL_PID
function Find-Workspace {
    @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
        $_.Id -ne $self -and $_.Path -and
        $_.Path.StartsWith($dir, [System.StringComparison]::OrdinalIgnoreCase)
    })
}
if ($env:HW_UNINSTALL_CLOSE -eq '1') {
    $procs = @(Find-Workspace)
    foreach ($p in $procs) { try { [void]$p.CloseMainWindow() } catch {} }
    foreach ($p in $procs) {
        try {
            if (-not $p.WaitForExit(3000)) { $p.Kill(); [void]$p.WaitForExit(5000) }
        } catch {}
    }
}
if (@(Find-Workspace).Count -gt 0) { exit 3 }
exit 0
"#;

    let status = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .env("HW_UNINSTALL_DIR", dir)
        .env("HW_UNINSTALL_PID", std::process::id().to_string())
        .env("HW_UNINSTALL_CLOSE", if close { "1" } else { "0" })
        .creation_flags(CREATE_NO_WINDOW)
        .status();

    match status {
        Ok(s) if s.code() == Some(STILL_RUNNING) => Some(true),
        Ok(s) if s.success() => Some(false),
        _ => None,
    }
}

/// Whether the Workspace app runs from `dir`. When PowerShell cannot tell,
/// it may be, so the wait for its uploads goes on.
fn app_running_in(dir: &Path) -> bool {
    processes_in(dir, false).unwrap_or(true)
}

/// Closes the Workspace app if it runs from `dir`. Fails when it is still
/// running after that.
fn close_processes_in(dir: &Path) -> Result<(), String> {
    match processes_in(dir, true) {
        Some(true) => Err(
            "Hardwave Workspace is still running and could not be closed. Quit it from its \
             icon in the system tray, then try again."
                .to_string(),
        ),
        // If PowerShell cannot run, an app that is still running shows up in
        // the next step as files in use, with a message saying so.
        _ => Ok(()),
    }
}

/// Refuses folders that can never be a Workspace install on their own (a
/// drive root or a standard user folder), so a bad registry value cannot make
/// the uninstaller wipe them.
fn check_safe_to_delete(dir: &Path) -> Result<(), String> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target = canon(dir);
    let is_root = target.parent().is_none() || dir.parent().is_none();
    let protected = [
        dirs::home_dir(),
        workspace_folder(),
        dirs::desktop_dir(),
        dirs::document_dir(),
        dirs::download_dir(),
        dirs::audio_dir(),
        dirs::data_dir(),
        dirs::data_local_dir(),
        dirs::data_local_dir().map(|p| p.join("Programs")),
        dirs::data_local_dir().map(|p| p.join("Programs").join("Hardwave")),
    ];
    let is_protected = protected.iter().flatten().any(|p| canon(p) == target);
    if is_root || is_protected {
        return Err(format!(
            "The install folder on record is {}, which is not a Hardwave Workspace folder, \
             so it was left alone. Delete the Hardwave Workspace files from it by hand.",
            dir.display()
        ));
    }
    Ok(())
}

/// Removes everything inside `dir` except `keep` (the running uninstaller).
fn remove_folder_contents(dir: &Path, keep: Option<&Path>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("Cannot open the install folder {}: {e}", dir.display()))?;
    let keep = keep.and_then(|k| std::fs::canonicalize(k).ok());

    let mut failed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if keep.is_some() && std::fs::canonicalize(&path).ok() == keep {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let removed = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if removed.is_err() {
            failed.push(entry.file_name().to_string_lossy().to_string());
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Some files in {} are in use and could not be deleted: {}.\n\n\
             Quit Hardwave Workspace (also from its icon in the system tray), then try again.",
            dir.display(),
            some_names(&failed)
        ))
    }
}

fn read_string(hkcu: &RegKey, path: &str, name: &str) -> Option<String> {
    hkcu.open_subkey(path)
        .ok()
        .and_then(|k| k.get_value::<String, _>(name).ok())
}

fn delete_key(hkcu: &RegKey, path: &str) -> Result<(), String> {
    match hkcu.delete_subkey_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Cannot remove the registry entry {path}: {e}")),
    }
}

/// Schedules the install folder for deletion after the current process exits.
/// Waits about three seconds with `ping`, then runs `rmdir /s /q "<dir>"`, in a detached cmd.
/// Not `timeout`: without a console it exits at once ("Input redirection is not supported"),
/// so the rmdir ran while this uninstaller was still running and the folder stayed.
/// The command line goes to cmd.exe verbatim (raw_arg): args() escapes the quotes around the
/// path as \", which cmd.exe does not read, so rmdir got an invalid path ("The filename,
/// directory name, or volume label syntax is incorrect") and the folder stayed as well.
fn schedule_delayed_cleanup(dir: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const DETACHED_PROCESS: u32 = 0x00000008;

    let dir_str = dir.to_string_lossy().to_string();
    let cmd = format!("ping -n 4 127.0.0.1 > nul & rmdir /s /q \"{}\"", dir_str);
    let _ = std::process::Command::new("cmd.exe")
        .raw_arg(format!("/C {cmd}"))
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn();
}

// ─── helpers ─────────────────────────────────────────────────────────────────

fn set_str(key: &RegKey, name: &str, value: &str) -> Result<(), String> {
    key.set_value(name, &value.to_string())
        .map_err(|e| format!("Cannot set {name}: {e}"))
}

fn set_dword(key: &RegKey, name: &str, value: u32) -> Result<(), String> {
    key.set_value(name, &value)
        .map_err(|e| format!("Cannot set {name}: {e}"))
}

fn folder_size_kb(path: &Path) -> Option<u64> {
    let mut total: u64 = 0;
    walk(path, &mut total).ok()?;
    Some(total / 1024)
}

fn walk(path: &Path, total: &mut u64) -> std::io::Result<()> {
    if path.is_file() {
        *total += path.metadata()?.len();
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            walk(&p, total)?;
        } else {
            *total += entry.metadata()?.len();
        }
    }
    Ok(())
}
