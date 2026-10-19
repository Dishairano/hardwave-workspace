//! What an uninstall removes and in which order, kept apart from the Windows
//! code in `uninstall.rs` that carries it out, so the decisions are tested on
//! every platform.
//!
//! The option on the first screen deletes the downloaded copies in the
//! Workspace folder. A copy is deleted only when the app's sync index records
//! it as uploaded and the file on this PC has exactly the uploaded bytes (the
//! same size and the same SHA-256), so the server holds what is deleted.
//! Nothing online is touched: the uninstaller never talks to the server.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// The app's own list of what it never uploads, so the uninstaller waits for
// exactly the files the app uploads and never for one it leaves alone.
#[path = "../../../src-tauri/src/rules.rs"]
mod rules;

/// One step of an uninstall, in the order the steps run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Wait until the app has uploaded every file in the Workspace folder
    /// (only when the option is ticked).
    WaitForUploads,
    /// Close Hardwave Workspace if it runs from the install folder.
    CloseApp,
    /// Delete the downloaded copies in the Workspace folder (only when the
    /// option is ticked).
    DeleteLocalCopies,
    /// Remove the install folder (what is left goes once this process exits).
    RemoveInstallFolder,
    /// Remove the desktop and Start menu shortcuts.
    RemoveShortcuts,
    /// Remove the Add/Remove Programs entry and the Workspace registry key.
    RemoveRegistryKeys,
}

/// Why an uninstall stopped: the step that could not finish (none when the
/// uninstaller itself stopped), the reason in words a producer can act on, and
/// how many files are not uploaded yet when that is what stopped it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Failure {
    pub step: Option<Step>,
    pub reason: String,
    pub still_uploading: Option<usize>,
}

/// Why one step could not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopped {
    pub reason: String,
    pub still_uploading: Option<usize>,
}

impl From<String> for Stopped {
    fn from(reason: String) -> Self {
        Self {
            reason,
            still_uploading: None,
        }
    }
}

impl From<&str> for Stopped {
    fn from(reason: &str) -> Self {
        reason.to_string().into()
    }
}

/// The steps of one uninstall. With the option ticked, the wait for uploads
/// comes before the app is closed (the app is what uploads), and the local
/// copies go once the app is closed (it would bring them back) but before the
/// app is removed, so "Try again" after a stop starts from a whole app.
pub fn plan(delete_local_copies: bool) -> Vec<Step> {
    if delete_local_copies {
        vec![
            Step::WaitForUploads,
            Step::CloseApp,
            Step::DeleteLocalCopies,
            Step::RemoveInstallFolder,
            Step::RemoveShortcuts,
            Step::RemoveRegistryKeys,
        ]
    } else {
        vec![
            Step::CloseApp,
            Step::RemoveInstallFolder,
            Step::RemoveShortcuts,
            Step::RemoveRegistryKeys,
        ]
    }
}

/// Runs `steps` in order with `run`, telling `on_progress` how many steps are
/// done before each step and once more when all are. Stops at the first step
/// that fails: the steps after it do not run.
pub fn run_plan(
    steps: &[Step],
    mut run: impl FnMut(Step) -> Result<(), Stopped>,
    mut on_progress: impl FnMut(usize),
) -> Result<(), Failure> {
    for (done, &step) in steps.iter().enumerate() {
        on_progress(done);
        run(step).map_err(|stopped| Failure {
            step: Some(step),
            reason: stopped.reason,
            still_uploading: stopped.still_uploading,
        })?;
    }
    on_progress(steps.len());
    Ok(())
}

/// Waits while the app uploads. `still_uploading` counts the files not
/// uploaded yet; `pause` waits between two counts. Done once the count is
/// zero. Stops with the count when the app is not running (nothing would
/// upload them) or when the count has not gone down for `patience` counts in
/// a row.
pub fn wait_for_uploads(
    mut still_uploading: impl FnMut() -> usize,
    mut app_running: impl FnMut() -> bool,
    mut pause: impl FnMut(),
    mut on_count: impl FnMut(usize),
    patience: u32,
) -> Result<(), usize> {
    let mut fewest = usize::MAX;
    let mut unchanged = 0u32;
    loop {
        let count = still_uploading();
        if count == 0 {
            return Ok(());
        }
        on_count(count);
        if count < fewest {
            fewest = count;
            unchanged = 0;
        } else {
            unchanged = unchanged.saturating_add(1);
        }
        if unchanged >= patience || !app_running() {
            return Err(count);
        }
        pause();
    }
}

// ─── The Workspace folder ────────────────────────────────────────────────────

/// A file in the Workspace folder with its bytes on this PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    /// The path from the Workspace folder with `/`, as the sync index keys it
    /// ("My Workspace/Kicks/kick.wav").
    pub rel_path: String,
    pub path: PathBuf,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// Every file the app would upload from the Workspace folder (`root`): inside
/// a workspace's folder, not hidden, not a temporary or system file (the app's
/// own rules), and with its bytes on this PC. Online-only placeholders are not
/// listed, and links and junctions are never followed.
pub fn local_files(root: &Path) -> Vec<LocalFile> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    // A file directly in the Workspace folder belongs to no workspace: the app
    // never uploads it, so it is neither waited for nor deleted.
    for entry in entries.flatten() {
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && !is_hidden(&entry) {
            walk(root, &entry.path(), &mut out);
        }
    }
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<LocalFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if is_hidden(&entry) {
            continue;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_path = rel.to_string_lossy().replace('\\', "/");
        if rules::is_excluded(&rel_path) {
            continue;
        }
        // A link or junction is not a dir or a file here, so it is skipped.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let bytes_on_this_pc = entry.metadata().is_ok_and(|m| !is_online_only(&m));
        if !bytes_on_this_pc {
            continue;
        }
        if file_type.is_dir() {
            walk(root, &path, out);
        } else if file_type.is_file() {
            // Size and time from the file itself, as the app reads them.
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                out.push(LocalFile {
                    rel_path,
                    size: meta.len(),
                    mtime: mtime_secs(&meta),
                    path,
                });
            }
        }
    }
}

fn is_hidden(entry: &std::fs::DirEntry) -> bool {
    entry.file_name().to_string_lossy().starts_with('.')
}

/// A placeholder whose bytes are online only. Reading one would download it.
#[cfg(windows)]
fn is_online_only(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    meta.file_attributes() & (FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

#[cfg(not(windows))]
fn is_online_only(_meta: &std::fs::Metadata) -> bool {
    false
}

/// The modified time in unix seconds, as the app records it.
fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

// ─── The app's sync index ────────────────────────────────────────────────────

/// The sync index is the app's own file, but a damaged or foreign file there
/// must not exhaust memory; the biggest real ones are tens of megabytes.
const MAX_INDEX_BYTES: u64 = 1 << 30;

/// What the app's sync index (`workspace-sync-index.json`) says about one file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct IndexEntry {
    sha256: String,
    size: u64,
    remote_id: Option<String>,
    workspace_id: Option<String>,
    #[serde(default)]
    mtime: Option<i64>,
}

impl IndexEntry {
    /// The upload finished and its checksum can be compared.
    fn is_uploaded(&self) -> bool {
        let present = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
        present(&self.remote_id)
            && present(&self.workspace_id)
            && self.sha256.len() == 64
            && self.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    }
}

/// The files the app has uploaded, by their path from the Workspace folder.
#[derive(Debug, Default)]
pub struct SyncIndex {
    entries: HashMap<String, IndexEntry>,
}

impl SyncIndex {
    /// Reads the index text. Anything that is not a well-formed entry counts
    /// as not uploaded, so a damaged index can only keep files, never delete one.
    pub fn parse(text: &str) -> Self {
        let raw: HashMap<String, serde_json::Value> =
            serde_json::from_str(text).unwrap_or_default();
        let entries = raw
            .into_iter()
            .filter_map(|(key, value)| {
                let entry: IndexEntry = serde_json::from_value(value).ok()?;
                entry.is_uploaded().then_some((key, entry))
            })
            .collect();
        Self { entries }
    }

    fn uploaded(&self, rel_path: &str) -> Option<&IndexEntry> {
        self.entries.get(rel_path)
    }
}

/// Reads the sync index at `path`; an index that does not exist is empty.
pub fn read_index(path: &Path) -> Result<SyncIndex, String> {
    Ok(read_index_text(path)?
        .map(|text| SyncIndex::parse(&text))
        .unwrap_or_default())
}

fn read_index_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
        Ok(m) if m.len() > MAX_INDEX_BYTES => {
            return Err(format!(
                "{} is too big to be the Workspace record of uploaded files.",
                path.display()
            ))
        }
        Ok(_) => {}
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))
}

/// How many of `files` the app has not uploaded yet, decided as the app
/// decides what to upload, without reading the files: not in the index as
/// uploaded, or another size, or another modified time than at the upload.
pub fn not_uploaded(files: &[LocalFile], index: &SyncIndex) -> usize {
    files
        .iter()
        .filter(|file| {
            let unchanged = index.uploaded(&file.rel_path).is_some_and(|e| {
                e.size == file.size
                    && match (e.mtime, file.mtime) {
                        // An entry from before the app recorded times: the
                        // app takes the same size as unchanged.
                        (None, _) => true,
                        (Some(at_upload), Some(now)) => at_upload == now,
                        (Some(_), None) => false,
                    }
            });
            !unchanged
        })
        .count()
}

/// What deleting the downloaded copies did, by path from the Workspace folder.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    pub deleted: Vec<String>,
    /// Kept: not uploaded, or changed since the upload.
    pub not_uploaded: Vec<String>,
    /// Uploaded, but could not be read or deleted (open in a DAW, say).
    pub in_use: Vec<String>,
}

/// Deletes each of `files` whose bytes are exactly what was uploaded: in
/// `index` as uploaded, the same size, and the same SHA-256. Every other file
/// stays. Each candidate is read in full to hash it; `on_file` hears how many
/// files have been looked at so far.
pub fn delete_uploaded_copies(
    files: &[LocalFile],
    index: &SyncIndex,
    mut on_file: impl FnMut(usize),
) -> Deleted {
    let mut outcome = Deleted::default();
    for (i, file) in files.iter().enumerate() {
        let entry = index
            .uploaded(&file.rel_path)
            .filter(|e| e.size == file.size);
        match entry.map(|e| (e, sha256_hex(&file.path))) {
            None => outcome.not_uploaded.push(file.rel_path.clone()),
            Some((_, Err(_))) => outcome.in_use.push(file.rel_path.clone()),
            Some((e, Ok(sha))) if !sha.eq_ignore_ascii_case(&e.sha256) => {
                outcome.not_uploaded.push(file.rel_path.clone())
            }
            Some((_, Ok(_))) => match std::fs::remove_file(&file.path) {
                Ok(()) => outcome.deleted.push(file.rel_path.clone()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    outcome.deleted.push(file.rel_path.clone())
                }
                Err(_) => outcome.in_use.push(file.rel_path.clone()),
            },
        }
        on_file(i + 1);
    }
    outcome
}

fn sha256_hex(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Drops from the sync index at `index_path` every entry whose file is gone
/// from the Workspace folder (`root`), as the app does itself when it sees a
/// file go. An entry that stayed would tell a later install that the file was
/// already on this PC, and the file would never come back from Workspace.
/// Every other entry, and every field the uninstaller does not know, is kept.
/// Returns how many entries were dropped.
pub fn forget_missing(index_path: &Path, root: &Path) -> Result<usize, String> {
    let Some(text) = read_index_text(index_path)? else {
        return Ok(0);
    };
    // Not readable as an index: there is nothing of ours to correct in it.
    let Ok(mut map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
    else {
        return Ok(0);
    };
    let before = map.len();
    map.retain(|key, _| {
        !is_safe_rel_path(key)
            || !matches!(
                std::fs::symlink_metadata(root.join(key)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound
            )
    });
    let dropped = before - map.len();
    if dropped == 0 {
        return Ok(0);
    }

    let json = serde_json::to_string_pretty(&map).map_err(|e| e.to_string())?;
    // Written next to the index and moved over it, so the index is never half written.
    let tmp = index_path.with_extension("json.uninstall-tmp");
    std::fs::write(&tmp, json)
        .and_then(|()| std::fs::rename(&tmp, index_path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("Cannot update {}: {e}", index_path.display())
        })?;
    Ok(dropped)
}

/// `/`-separated parts, none empty, `.`, `..`, or with a separator, a drive
/// colon or a control character: joined to the Workspace folder it stays in it.
/// The same test the app applies to every path it writes.
fn is_safe_rel_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains('\\')
                && !part.contains(':')
                && !part.chars().any(char::is_control)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty folder under the system temp folder for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hw-ws-removal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// The index entry the app writes after uploading `path`.
    fn uploaded_entry(rel: &str, path: &Path) -> serde_json::Value {
        let meta = std::fs::metadata(path).unwrap();
        serde_json::json!({
            "rel_path": rel,
            "sha256": sha256_hex(path).unwrap(),
            "modified": "2026-10-01T10:00:00Z",
            "size": meta.len(),
            "remote_id": "f-1",
            "workspace_id": "w-1",
            "mtime": mtime_secs(&meta),
            "verified": true,
        })
    }

    fn rel_paths(files: &[LocalFile]) -> Vec<String> {
        let mut v: Vec<String> = files.iter().map(|f| f.rel_path.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn option_off_removes_the_app_only() {
        assert_eq!(
            plan(false),
            vec![
                Step::CloseApp,
                Step::RemoveInstallFolder,
                Step::RemoveShortcuts,
                Step::RemoveRegistryKeys,
            ]
        );
    }

    #[test]
    fn option_on_waits_for_uploads_then_deletes_the_local_copies_after_closing_the_app() {
        assert_eq!(
            plan(true),
            vec![
                Step::WaitForUploads,
                Step::CloseApp,
                Step::DeleteLocalCopies,
                Step::RemoveInstallFolder,
                Step::RemoveShortcuts,
                Step::RemoveRegistryKeys,
            ]
        );
    }

    #[test]
    fn steps_have_stable_names_for_the_window() {
        let names: Vec<String> = plan(true)
            .iter()
            .map(|s| {
                serde_json::to_value(s)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            [
                "wait_for_uploads",
                "close_app",
                "delete_local_copies",
                "remove_install_folder",
                "remove_shortcuts",
                "remove_registry_keys",
            ]
        );
    }

    #[test]
    fn a_failed_step_stops_the_steps_after_it() {
        let mut ran = Vec::new();
        let mut progress = Vec::new();
        let result = run_plan(
            &plan(true),
            |step| {
                ran.push(step);
                Err(Stopped {
                    reason: "3 files are still uploading".to_string(),
                    still_uploading: Some(3),
                })
            },
            |done| progress.push(done),
        );
        assert_eq!(
            result,
            Err(Failure {
                step: Some(Step::WaitForUploads),
                reason: "3 files are still uploading".to_string(),
                still_uploading: Some(3),
            })
        );
        assert_eq!(ran, vec![Step::WaitForUploads]);
        assert_eq!(progress, vec![0]);
    }

    #[test]
    fn a_full_run_reports_every_step_done() {
        let mut progress = Vec::new();
        let steps = plan(false);
        assert_eq!(
            run_plan(&steps, |_| Ok(()), |done| progress.push(done)),
            Ok(())
        );
        assert_eq!(progress, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn nothing_to_upload_means_no_wait() {
        let mut pauses = 0;
        let result = wait_for_uploads(|| 0, || false, || pauses += 1, |_| {}, 3);
        assert_eq!(result, Ok(()));
        assert_eq!(pauses, 0);
    }

    #[test]
    fn waits_while_the_app_uploads() {
        let mut counts = vec![5, 5, 3, 1, 0].into_iter();
        let mut heard = Vec::new();
        let result = wait_for_uploads(
            || counts.next().unwrap(),
            || true,
            || {},
            |n| heard.push(n),
            3,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(heard, vec![5, 5, 3, 1]);
    }

    #[test]
    fn stops_at_once_when_the_app_is_not_running() {
        let mut pauses = 0;
        let result = wait_for_uploads(|| 4, || false, || pauses += 1, |_| {}, 30);
        assert_eq!(result, Err(4));
        assert_eq!(pauses, 0);
    }

    #[test]
    fn stops_when_the_uploads_stop_moving() {
        let mut pauses = 0;
        let result = wait_for_uploads(|| 7, || true, || pauses += 1, |_| {}, 2);
        assert_eq!(result, Err(7));
        assert_eq!(pauses, 2);
    }

    #[test]
    fn only_files_the_app_uploads_are_listed() {
        let root = scratch("listed");
        write(&root, "My Workspace/Kicks/kick.wav", b"kick");
        write(&root, "My Workspace/Song.flp", b"song");
        write(&root, "Label/Masters/master.wav", b"master");
        // Never uploaded by the app.
        write(&root, "loose.txt", b"in no workspace");
        write(&root, ".hidden ws/a.wav", b"a");
        write(&root, "My Workspace/.cache/b.wav", b"b");
        write(&root, "My Workspace/site/node_modules/x.js", b"x");
        write(&root, "My Workspace/Bounces/.hwtmp-mix.wav", b"partial");
        write(&root, "My Workspace/Bounces/mix.wav.tmp", b"partial");
        write(&root, "My Workspace/Thumbs.db", b"thumbs");

        assert_eq!(
            rel_paths(&local_files(&root)),
            vec![
                "Label/Masters/master.wav",
                "My Workspace/Kicks/kick.wav",
                "My Workspace/Song.flp",
            ]
        );
        assert!(local_files(&root.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed() {
        let root = scratch("links");
        let outside = scratch("links-outside");
        write(&outside, "Samples/snare.wav", b"snare");
        write(&root, "My Workspace/kick.wav", b"kick");
        std::os::unix::fs::symlink(outside.join("Samples"), root.join("My Workspace/Samples"))
            .unwrap();
        std::os::unix::fs::symlink(
            outside.join("Samples/snare.wav"),
            root.join("My Workspace/snare.wav"),
        )
        .unwrap();

        assert_eq!(
            rel_paths(&local_files(&root)),
            vec!["My Workspace/kick.wav"]
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn files_count_as_uploading_until_the_index_has_them_unchanged() {
        let root = scratch("uploading");
        let done = write(&root, "WS/done.wav", b"done");
        let old = write(&root, "WS/old-entry.wav", b"old");
        let resized = write(&root, "WS/resized.wav", b"resized");
        let touched = write(&root, "WS/touched.wav", b"touched");
        let half = write(&root, "WS/half.wav", b"half");
        write(&root, "WS/new.wav", b"new");

        let mut old_entry = uploaded_entry("WS/old-entry.wav", &old);
        old_entry["mtime"] = serde_json::Value::Null;
        let mut resized_entry = uploaded_entry("WS/resized.wav", &resized);
        resized_entry["size"] = serde_json::json!(1);
        let mut touched_entry = uploaded_entry("WS/touched.wav", &touched);
        touched_entry["mtime"] = serde_json::json!(1);
        let mut half_entry = uploaded_entry("WS/half.wav", &half);
        half_entry["remote_id"] = serde_json::Value::Null;
        let index = serde_json::json!({
            "WS/done.wav": uploaded_entry("WS/done.wav", &done),
            "WS/old-entry.wav": old_entry,
            "WS/resized.wav": resized_entry,
            "WS/touched.wav": touched_entry,
            "WS/half.wav": half_entry,
        });
        let index = SyncIndex::parse(&index.to_string());

        // resized, touched, half and new.
        assert_eq!(not_uploaded(&local_files(&root), &index), 4);
        assert_eq!(not_uploaded(&local_files(&root), &SyncIndex::default()), 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_copies_with_exactly_the_uploaded_bytes_are_deleted() {
        let root = scratch("delete");
        let kick = write(&root, "WS/Kicks/kick.wav", b"kick");
        let song = write(&root, "WS/Song.flp", b"song v1");
        let index = serde_json::json!({
            "WS/Kicks/kick.wav": uploaded_entry("WS/Kicks/kick.wav", &kick),
            "WS/Song.flp": uploaded_entry("WS/Song.flp", &song),
        });
        // Saved again since the upload, at the same length.
        std::fs::write(&song, b"song v2").unwrap();
        write(&root, "WS/new.wav", b"not uploaded");
        write(&root, "loose.txt", b"in no workspace");

        let files = local_files(&root);
        let mut looked_at = 0;
        let outcome = delete_uploaded_copies(&files, &SyncIndex::parse(&index.to_string()), |n| {
            looked_at = n
        });

        assert_eq!(outcome.deleted, vec!["WS/Kicks/kick.wav"]);
        let mut kept = outcome.not_uploaded.clone();
        kept.sort();
        assert_eq!(kept, vec!["WS/Song.flp", "WS/new.wav"]);
        assert!(outcome.in_use.is_empty());
        assert_eq!(looked_at, 3);

        assert!(!kick.exists());
        assert_eq!(std::fs::read(&song).unwrap(), b"song v2");
        assert!(root.join("WS/new.wav").exists());
        assert!(root.join("loose.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_index_deletes_nothing() {
        let root = scratch("damaged");
        let kick = write(&root, "WS/kick.wav", b"kick");
        let files = local_files(&root);

        for text in ["", "not json", r#"["WS/kick.wav"]"#] {
            let outcome = delete_uploaded_copies(&files, &SyncIndex::parse(text), |_| {});
            assert!(outcome.deleted.is_empty());
        }
        // A checksum that is not one, or no workspace: not counted as uploaded.
        let mut entry = uploaded_entry("WS/kick.wav", &kick);
        entry["sha256"] = serde_json::json!("ab");
        let mut no_ws = uploaded_entry("WS/kick.wav", &kick);
        no_ws["workspace_id"] = serde_json::json!("");
        for entry in [entry, no_ws] {
            let index = serde_json::json!({ "WS/kick.wav": entry }).to_string();
            let outcome = delete_uploaded_copies(&files, &SyncIndex::parse(&index), |_| {});
            assert!(outcome.deleted.is_empty());
        }
        assert!(kick.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn one_broken_entry_does_not_hide_the_others() {
        let root = scratch("broken-entry");
        let kick = write(&root, "WS/kick.wav", b"kick");
        let index = serde_json::json!({
            "WS/broken.wav": { "size": "big" },
            "WS/kick.wav": uploaded_entry("WS/kick.wav", &kick),
        });
        let index = SyncIndex::parse(&index.to_string());
        assert_eq!(not_uploaded(&local_files(&root), &index), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_index_is_empty_and_a_huge_one_is_refused() {
        let root = scratch("index-file");
        let index = read_index(&root.join("workspace-sync-index.json")).unwrap();
        assert!(index.entries.is_empty());

        let huge = root.join("huge.json");
        let f = std::fs::File::create(&huge).unwrap();
        f.set_len(MAX_INDEX_BYTES + 1).unwrap();
        assert!(read_index(&huge).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deleted_copies_are_dropped_from_the_index_and_the_rest_is_kept() {
        let root = scratch("forget");
        let data = scratch("forget-data");
        let kick = write(&root, "WS/kick.wav", b"kick");
        let song = write(&root, "WS/Song.flp", b"song");
        let index_path = data.join("workspace-sync-index.json");
        let index = serde_json::json!({
            "WS/kick.wav": uploaded_entry("WS/kick.wav", &kick),
            "WS/Song.flp": uploaded_entry("WS/Song.flp", &song),
            "../outside.wav": { "size": 1 },
        });
        std::fs::write(&index_path, index.to_string()).unwrap();
        std::fs::remove_file(&kick).unwrap();

        assert_eq!(forget_missing(&index_path, &root), Ok(1));

        let left: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(&index_path).unwrap()).unwrap();
        let mut keys: Vec<&String> = left.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["../outside.wav", "WS/Song.flp"]);
        // Fields the uninstaller does not read are written back as they were.
        assert_eq!(left["WS/Song.flp"]["verified"], serde_json::json!(true));
        assert_eq!(left["WS/Song.flp"]["modified"], "2026-10-01T10:00:00Z");

        // Nothing more to drop: the file is left as it is.
        assert_eq!(forget_missing(&index_path, &root), Ok(0));
        assert_eq!(forget_missing(&data.join("missing.json"), &root), Ok(0));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn index_paths_that_could_leave_the_workspace_folder_are_not_safe() {
        assert!(is_safe_rel_path("WS/Kicks/kick.wav"));
        for path in [
            "",
            "../x.wav",
            "WS/../../x.wav",
            "WS//x.wav",
            "/WS/x.wav",
            "C:/x.wav",
            "WS\\..\\x.wav",
            "WS/./x.wav",
            "WS/x\n.wav",
        ] {
            assert!(!is_safe_rel_path(path), "{path:?}");
        }
    }
}
