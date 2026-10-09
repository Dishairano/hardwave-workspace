//! Bidirectional file sync engine.
//!
//! Watches `~/Hardwave/` for local changes and periodically polls the
//! Workspace API for remote changes. Uses SHA-256 hashes to detect diffs.

use crate::api;
use futures_util::stream::StreamExt as _;
use crate::conflicts::{self, Conflict, Indexed, Local, RemoteAction};
use crate::pins;
use crate::models::{SyncEntry, SyncStatus};
use notify::{RecursiveMode, Watcher, Event, EventKind};
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{mpsc, Mutex, RwLock};
use tauri::{Emitter, Manager};

/// How often to poll the remote for changes (seconds).
const POLL_INTERVAL_SECS: u64 = 30;

/// Sync root directory.
pub fn sync_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Hardwave")
}

/// Path to the local sync index (tracks what we've already synced).
fn index_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hardwave")
        .join("workspace-sync-index.json")
}

/// Read the sync index from disk.
pub fn read_index() -> HashMap<String, SyncEntry> {
    let path = index_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write the sync index to disk.
fn write_index(index: &HashMap<String, SyncEntry>) {
    let path = index_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(index) {
        let _ = std::fs::write(&path, json);
    }
}

/// Compute SHA-256 of a file using buffered reads (avoids loading entire file into RAM).
fn hash_file(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Open error: {}", e))?;
    let mut reader = std::io::BufReader::with_capacity(65536, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("Read error: {}", e))?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Validate that a path component is safe (no path traversal). A ':' would
/// read as a drive ("C:") or an alternate data stream on Windows.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
        && !name.chars().any(char::is_control)
}

/// Validate that a relative path is safe: `/`-separated components, each one
/// safe, so joining it to the sync root can never leave the sync root. Every
/// remote file's path goes through this before anything is written; a
/// workspace editor chooses those names (launch audit 2026-10-08).
pub(crate) fn is_safe_rel_path(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(is_safe_component)
}

/// Scan the sync root and return all files with their hashes.
/// Local file mtime as unix seconds, if the platform reports it.
fn file_mtime(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Whether a local file can be matched to a server file without reading it.
///
/// Hashing every unindexed local file before matching it meant reading up to
/// ~200 GB, one file at a time, before a big workspace finished its first pass,
/// and it hydrated any cloud-only placeholder it touched. This decides from
/// metadata alone, and only in the safe direction: exact same size (the caller
/// has already matched the path), a server checksum to record, and a local
/// modified time no later than when the server row was created. A file changed
/// after that, such as a stem re-bounced to the same length, has a newer mtime
/// and falls through to the upload scan, which hashes it.
///
/// The entry this produces is marked unverified; Free Up Space hashes the file
/// before discarding its bytes.
pub(crate) fn trust_without_hash(
    local_size: u64,
    local_mtime: Option<i64>,
    remote_size: u64,
    remote_sha: Option<&str>,
    remote_created_at: Option<&str>,
) -> bool {
    if local_size != remote_size || remote_sha.is_none_or(str::is_empty) {
        return false;
    }
    match (local_mtime, remote_created_at.and_then(parse_server_time)) {
        (Some(modified), Some(created)) => modified <= created,
        _ => false,
    }
}

/// Server timestamps arrive as ISO 8601 in UTC. Anything else is treated as
/// unknown, which makes `trust_without_hash` refuse rather than guess.
fn parse_server_time(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp())
}

#[cfg(test)]
mod trust_without_hash_tests {
    use super::*;

    const CREATED: &str = "2026-09-01T10:00:00.000Z";

    fn created() -> i64 {
        parse_server_time(CREATED).unwrap()
    }

    #[test]
    fn unchanged_since_upload_is_trusted() {
        assert!(trust_without_hash(10, Some(created() - 60), 10, Some("ab"), Some(CREATED)));
    }

    #[test]
    fn edited_after_upload_is_not_trusted_even_at_the_same_size() {
        assert!(!trust_without_hash(10, Some(created() + 60), 10, Some("ab"), Some(CREATED)));
    }

    #[test]
    fn different_size_is_not_trusted() {
        assert!(!trust_without_hash(11, Some(created() - 60), 10, Some("ab"), Some(CREATED)));
    }

    #[test]
    fn no_server_checksum_is_not_trusted() {
        assert!(!trust_without_hash(10, Some(created() - 60), 10, None, Some(CREATED)));
        assert!(!trust_without_hash(10, Some(created() - 60), 10, Some(""), Some(CREATED)));
    }

    #[test]
    fn unknown_times_are_not_trusted() {
        assert!(!trust_without_hash(10, None, 10, Some("ab"), Some(CREATED)));
        assert!(!trust_without_hash(10, Some(0), 10, Some("ab"), Some("2026-09-01 10:00:00")));
        assert!(!trust_without_hash(10, Some(0), 10, Some("ab"), None));
    }
}

/// What the upload scan should do with one local file — decided from size,
/// mtime and whether it is already uploaded, no hashing, so the scan never
/// blocks the sync loop on I/O.
#[derive(Debug, PartialEq, Eq)]
enum UploadPlan {
    /// Untracked, size changed, or indexed but never uploaded: upload it.
    /// Hashing is deferred to the upload worker, so a first sync of 100k files
    /// starts uploading immediately instead of SHA-256'ing every file up front.
    UploadNew,
    /// Already uploaded, same size, but never tagged with an mtime (an entry
    /// from before mtime tracking): trust it as unchanged and just record the
    /// mtime. No hashing — which also means dehydrated placeholders are NOT
    /// hydrated just to be checked. Tradeoff: a same-size edit made before this
    /// version shipped would not be re-detected on this one pass; any later
    /// edit changes the mtime and is caught, and the server still holds the
    /// prior copy. Worth it to not re-hash/hydrate ~70 GB on every upgrade.
    TrustBackfill,
    /// Same size, had an mtime, but it changed (or is now unreadable) — a
    /// possible genuine edit; hash to decide.
    VerifyByHash,
    /// Same size and mtime as recorded: unchanged, skip without hashing.
    Skip,
}

fn plan_upload(
    indexed_size: Option<u64>,
    indexed_mtime: Option<i64>,
    already_uploaded: bool,
    size: u64,
    mtime: Option<i64>,
) -> UploadPlan {
    match indexed_size {
        None => UploadPlan::UploadNew,
        Some(s) if s != size => UploadPlan::UploadNew,
        // Indexed but the previous upload never completed (no remote id): send it.
        Some(_) if !already_uploaded => UploadPlan::UploadNew,
        Some(_) => match (indexed_mtime, mtime) {
            (Some(a), Some(b)) if a == b => UploadPlan::Skip,
            // No stored mtime = a pre-mtime entry; it is already uploaded, so
            // trust it and backfill rather than re-hash/hydrate.
            (None, _) => UploadPlan::TrustBackfill,
            // Had an mtime and it no longer matches: verify by hash.
            (Some(_), _) => UploadPlan::VerifyByHash,
        },
    }
}

#[cfg(test)]
mod upload_plan_tests {
    use super::*;
    #[test]
    fn untracked_uploads() {
        assert_eq!(plan_upload(None, None, false, 10, Some(5)), UploadPlan::UploadNew);
    }
    #[test]
    fn size_change_uploads() {
        assert_eq!(plan_upload(Some(9), Some(5), true, 10, Some(5)), UploadPlan::UploadNew);
    }
    #[test]
    fn indexed_but_not_uploaded_uploads() {
        assert_eq!(plan_upload(Some(10), None, false, 10, Some(5)), UploadPlan::UploadNew);
    }
    #[test]
    fn same_size_and_mtime_skips() {
        assert_eq!(plan_upload(Some(10), Some(5), true, 10, Some(5)), UploadPlan::Skip);
    }
    #[test]
    fn uploaded_no_stored_mtime_trusts() {
        assert_eq!(plan_upload(Some(10), None, true, 10, Some(5)), UploadPlan::TrustBackfill);
    }
    #[test]
    fn same_size_diff_mtime_verifies() {
        assert_eq!(plan_upload(Some(10), Some(5), true, 10, Some(6)), UploadPlan::VerifyByHash);
    }
    #[test]
    fn same_size_unknown_current_mtime_verifies() {
        assert_eq!(plan_upload(Some(10), Some(5), true, 10, None), UploadPlan::VerifyByHash);
    }
}

fn scan_local(root: &Path) -> Vec<(String, PathBuf, u64)> {
    let mut files = Vec::new();
    if !root.exists() {
        return files;
    }
    scan_dir_recursive(root, root, &mut files);
    files
}

fn scan_dir_recursive(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf, u64)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        // A dehydrated placeholder has no local content, and hashing it would
        // force Windows to download the whole file — exactly what On-Demand is
        // meant to avoid. Leave those to the remote index.
        if crate::cloudfiles::is_placeholder(&path) {
            continue;
        }

        // Skip hidden files and .hardwave-sync metadata
        if path.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }
        let rel_str = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        // node_modules, temporary files and the like (rules.rs).
        if crate::rules::is_excluded(&rel_str) {
            continue;
        }
        if path.is_dir() {
            scan_dir_recursive(root, &path, out);
        } else if let Ok(meta) = path.metadata() {
            out.push((rel_str, path.clone(), meta.len()));
        }
    }
}

pub struct SyncEngine {
    token: Arc<RwLock<Option<String>>>,
    status: Arc<RwLock<SyncStatus>>,
    index: Arc<Mutex<HashMap<String, SyncEntry>>>,
    app: tauri::AppHandle,
    paused: Arc<RwLock<bool>>,
    sync_active: AtomicBool,
    /// Abort handles for per-workspace SSE listener tasks.
    sse_handles: Mutex<Vec<tokio::task::AbortHandle>>,
    /// Notified by SSE listeners to trigger an immediate full sync.
    sse_trigger: Arc<tokio::sync::Notify>,
    /// Cached (computed_at, files_on_disk, bytes_on_disk) so get_status can
    /// report the honest total without re-walking 120k files on every call.
    disk_cache: RwLock<Option<(std::time::Instant, u32, u64)>>,
    /// How many uploads to run at once right now (see UPLOAD_PARALLEL).
    upload_parallel: AtomicUsize,
    /// Files changed here and in Workspace before they synced (conflicts.rs),
    /// waiting for the producer. Kept on disk so a restart does not forget them.
    conflicts: Arc<Mutex<HashMap<String, Conflict>>>,
    /// Files the engine itself just wrote, with their hash, so the watcher does
    /// not upload a download straight back.
    self_writes: Arc<Mutex<HashMap<String, String>>>,
    /// What stays on this PC, per folder (pins.rs).
    modes: Arc<Mutex<HashMap<String, pins::Mode>>>,
    /// Cached folder overview for the This PC page (walking 100k files takes seconds).
    overview_cache: Mutex<Option<(std::time::Instant, FolderOverview)>>,
    /// The last things that moved, for the tray panel.
    recent: Mutex<std::collections::VecDeque<Activity>>,
    /// Folder names of the signed-in account's workspaces, from the last full
    /// pass. The sync folder can also hold another account's workspaces (two
    /// accounts on one PC); totals and the folder list leave those out.
    /// None until the first pass has listed the workspaces.
    own_workspaces: RwLock<Option<Vec<String>>>,
}

/// One line in the tray panel: "↑ Face Of Fire.flp, 4 min ago".
#[derive(Clone, Debug, serde::Serialize)]
pub struct Activity {
    /// "up", "down" or "conflict".
    pub kind: &'static str,
    pub rel_path: String,
    pub at: String,
}

impl SyncEngine {
    pub fn new(app: tauri::AppHandle) -> Self {
        let index = read_index();
        Self {
            token: Arc::new(RwLock::new(None)),
            status: Arc::new(RwLock::new(SyncStatus {
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
            })),
            index: Arc::new(Mutex::new(index)),
            app,
            paused: Arc::new(RwLock::new(false)),
            sync_active: AtomicBool::new(false),
            upload_parallel: AtomicUsize::new(UPLOAD_PARALLEL),
            sse_handles: Mutex::new(Vec::new()),
            sse_trigger: Arc::new(tokio::sync::Notify::new()),
            disk_cache: RwLock::new(None),
            conflicts: Arc::new(Mutex::new(conflicts::load())),
            self_writes: Arc::new(Mutex::new(HashMap::new())),
            modes: Arc::new(Mutex::new(pins::load())),
            overview_cache: Mutex::new(None),
            own_workspaces: RwLock::new(None),
            recent: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    async fn note(&self, kind: &'static str, rel_path: &str) {
        let mut r = self.recent.lock().await;
        r.push_front(Activity { kind, rel_path: rel_path.to_string(), at: chrono::Utc::now().to_rfc3339() });
        r.truncate(20);
    }

    /// Newest first.
    pub async fn recent_activity(&self) -> Vec<Activity> {
        self.recent.lock().await.iter().cloned().collect()
    }

    pub async fn set_token(&self, token: Option<String>) {
        // Abort any existing SSE connections (token changed or logged out)
        {
            let mut handles = self.sse_handles.lock().await;
            for h in handles.drain(..) { h.abort(); }
        }

        let is_new = token.is_some();
        *self.token.write().await = token.clone();

        if is_new {
            if let Some(t) = token {
                let root = sync_root();
                let _ = std::fs::create_dir_all(&root);
                if let Ok(workspaces) = api::list_workspaces(&t).await {
                    for ws in &workspaces {
                        if !is_safe_component(&ws.name) {
                            log::info!("[Sync] Skipping workspace with unsafe name: {}", ws.name);
                            continue;
                        }
                        let ws_dir = root.join(&ws.name);
                        let _ = std::fs::create_dir_all(&ws_dir);
                        log::info!("[Sync] Created workspace folder: {}", ws_dir.display());
                        if let Ok(folders) = api::list_folders(&t, &ws.id).await {
                            for f in &folders {
                                let folder_path = f.path.as_deref()
                                    .unwrap_or(&f.name)
                                    .trim_matches('/');
                                if !folder_path.is_empty() && is_safe_rel_path(folder_path) {
                                    let dir = ws_dir.join(folder_path);
                                    let _ = std::fs::create_dir_all(&dir);
                                    log::info!("[Sync] Created subfolder: {}", dir.display());
                                }
                            }
                        }

                        // Start real-time SSE listener for this workspace
                        let handle = self.start_sse_listener(ws.id.clone(), t.clone());
                        self.sse_handles.lock().await.push(handle);
                    }
                }
            }
        }
    }

    pub async fn pause(&self) {
        *self.paused.write().await = true;
        self.update_status("paused", None).await;
    }

    pub async fn resume(&self) {
        *self.paused.write().await = false;
        self.update_status("idle", None).await;
    }

    pub async fn get_status(&self) -> SyncStatus {
        // Start from the stored state (state/last_sync/error) then overlay the
        // honest counts so the UI shows uploaded-of-actual, never a fake 100%.
        let mut s = self.status.read().await.clone();
        let (fs, bs) = self.synced_totals().await;
        let (ft, bt) = self.disk_totals().await;
        s.files_synced = fs;
        s.bytes_synced = bs;
        // Never let the total read below what is already synced (a stale cache
        // mid-upload could otherwise show synced > total).
        s.files_total = ft.max(fs);
        s.bytes_total = bt.max(bs);
        s.files_pending = s.files_total.saturating_sub(fs);
        s
    }

    /// What has actually been uploaded: index entries carrying a remote id.
    async fn synced_totals(&self) -> (u32, u64) {
        let idx = self.index.lock().await;
        let mut n = 0u32;
        let mut b = 0u64;
        for e in idx.values() {
            if e.remote_id.is_some() {
                n += 1;
                b += e.size;
            }
        }
        (n, b)
    }

    /// Files + bytes actually on disk under the sync root. Stat only, no
    /// hashing, so it is cheap; still cached for 30s to keep get_status snappy.
    async fn disk_totals(&self) -> (u32, u64) {
        const TTL: std::time::Duration = std::time::Duration::from_secs(30);
        if let Some((t, f, b)) = *self.disk_cache.read().await {
            if t.elapsed() < TTL {
                return (f, b);
            }
        }
        let own = self.own_workspaces.read().await.clone();
        let (f, b) = tokio::task::spawn_blocking(move || Self::scan_disk_totals(own.as_deref()))
            .await
            .unwrap_or((0, 0));
        *self.disk_cache.write().await = Some((std::time::Instant::now(), f, b));
        (f, b)
    }

    fn scan_disk_totals(own: Option<&[String]>) -> (u32, u64) {
        let root = sync_root();
        let files = match own {
            Some(names) => names.iter().flat_map(|n| scan_local(&root.join(n))).collect(),
            None => scan_local(&root),
        };
        let bytes: u64 = files.iter().map(|(_, _, s)| *s).sum();
        (files.len() as u32, bytes)
    }

    async fn update_status(&self, state: &str, error: Option<String>) {
        let mut status = self.status.write().await;
        status.state = state.to_string();
        status.error = error;
        let _ = self.app.emit("sync:status", status.clone());
    }

    /// Push per-file sync progress to the webview via safe eval.
    fn emit_file_progress(&self, rel_path: &str, direction: &str, percent: u32) {
        if let Some(win) = self.app.get_webview_window("main") {
            crate::safe_eval(&win, "__HW_SYNC_FILE__", &serde_json::json!({
                "rel_path": rel_path,
                "direction": direction,
                "percent": percent,
            }));
        }
    }

    /// Record a conflict once, tell the window, and keep it on disk.
    async fn add_conflict(&self, c: Conflict) {
        let list = {
            let mut all = self.conflicts.lock().await;
            if all.contains_key(&c.rel_path) {
                return;
            }
            log::info!(
                "[Sync] Conflict: '{}' changed here ({} bytes) and in Workspace ({} bytes)",
                c.rel_path, c.local_size, c.remote_size
            );
            all.insert(c.rel_path.clone(), c.clone());
            conflicts::save(&all);
            all.values().cloned().collect::<Vec<_>>()
        };
        self.note("conflict", &c.rel_path).await;
        let _ = self.app.emit("sync:conflicts", list);
    }

    /// Conflicts waiting for the producer, newest first.
    pub async fn list_conflicts(&self) -> Vec<Conflict> {
        let mut list: Vec<Conflict> = self.conflicts.lock().await.values().cloned().collect();
        list.sort_by(|a, b| b.detected_at.cmp(&a.detected_at));
        list
    }

    /// Settle one conflict the way the producer chose. Nothing is thrown away:
    /// Workspace keeps every earlier version, and a copy of this PC's file
    /// that is replaced goes to the Recycle Bin.
    pub async fn resolve_conflict(&self, rel_path: &str, choice: conflicts::Choice) -> Result<String, String> {
        let token = self.token.read().await.clone().ok_or("Sign in first.")?;
        let c = self
            .conflicts
            .lock()
            .await
            .get(rel_path)
            .cloned()
            .ok_or("This one is already settled.")?;
        let path = sync_root().join(rel_path);

        let message = match choice {
            conflicts::Choice::Local => {
                self.sync_local_file(rel_path, true).await?;
                "This PC's version is now the newest in Workspace. The other one stays in its versions.".to_string()
            }
            conflicts::Choice::Remote => {
                if path.exists() {
                    trash::delete(&path)
                        .map_err(|e| format!("This PC's copy could not go to the Recycle Bin, so nothing changed: {e}"))?;
                }
                let entry = self.download_replace(&token, &c.workspace_id, &c.remote_id, rel_path).await?;
                self.index_put(entry).await;
                "Workspace's version is on this PC. This PC's copy is in the Recycle Bin.".to_string()
            }
            conflicts::Choice::Both => {
                let dir = path.parent().ok_or("The file has no folder.")?.to_path_buf();
                let name = path.file_name().and_then(|n| n.to_str()).ok_or("The file name cannot be read.")?;
                let kept = conflicts::kept_name(name, &crate::device::device_name(), |n| dir.join(n).exists());
                // An earlier try may have renamed it already and then failed to
                // download; then only the download is left to do.
                if path.exists() {
                    std::fs::rename(&path, dir.join(&kept))
                        .map_err(|e| format!("This PC's copy could not be renamed (is it open?): {e}"))?;
                }
                let entry = self.download_replace(&token, &c.workspace_id, &c.remote_id, rel_path).await?;
                self.index_put(entry).await;
                // The renamed copy is a new file; the next pass uploads it.
                format!("Both kept. This PC's copy is now \"{kept}\".")
            }
        };

        let list = {
            let mut all = self.conflicts.lock().await;
            all.remove(rel_path);
            conflicts::save(&all);
            all.values().cloned().collect::<Vec<_>>()
        };
        let _ = self.app.emit("sync:conflicts", list);
        self.sse_trigger.notify_one();
        Ok(message)
    }

    /// The folders of the This PC page: each workspace and its top-level
    /// folders, with how much is on this PC, how much is online only, and the
    /// mode that governs it. Cached for a minute.
    pub async fn folder_overview(&self, fresh: bool) -> FolderOverview {
        if !fresh {
            if let Some((t, o)) = &*self.overview_cache.lock().await {
                if t.elapsed() < std::time::Duration::from_secs(60) {
                    return o.clone();
                }
            }
        }
        let modes = self.modes.lock().await.clone();
        let own = self.own_workspaces.read().await.clone();
        let overview = tokio::task::spawn_blocking(move || build_overview(&sync_root(), &modes, own.as_deref()))
            .await
            .unwrap_or_default();
        *self.overview_cache.lock().await = Some((std::time::Instant::now(), overview.clone()));
        overview
    }

    /// Apply the producer's choice for a folder. The pin state is set at once;
    /// bringing bytes in ("always") or freeing them ("online only") runs in the
    /// background and can take a while on a big folder.
    pub async fn set_folder_mode(self: &Arc<Self>, folder: &str, mode: pins::Mode) -> Result<String, String> {
        if !is_safe_rel_path(folder) {
            return Err("That folder name cannot be used.".into());
        }
        let path = sync_root().join(folder);
        if !path.is_dir() {
            return Err("That folder is not in the sync folder.".into());
        }
        {
            let mut m = self.modes.lock().await;
            pins::set(&mut m, folder, mode);
            pins::save(&m);
        }
        *self.overview_cache.lock().await = None;
        if crate::cloudfiles::is_supported() {
            if let Err(e) = crate::cloudfiles::set_pin(&path, mode.pin()) {
                log::info!("[Pins] {folder}: {e}");
            }
        }
        let name = folder.rsplit('/').next().unwrap_or(folder).to_string();
        match mode {
            pins::Mode::Always => {
                let engine = Arc::clone(self);
                tokio::spawn(async move {
                    let (n, failed) = tokio::task::spawn_blocking(move || hydrate_tree(&path)).await.unwrap_or((0, 0));
                    log::warn!("[Pins] brought {n} files onto this PC, {failed} failed");
                    *engine.overview_cache.lock().await = None;
                });
                Ok(format!("{name} stays on this PC. Files that were online only are coming down now."))
            }
            pins::Mode::Online => {
                let engine = Arc::clone(self);
                let scope = folder.to_string();
                tokio::spawn(async move {
                    match free_up_space(Some(Arc::clone(&engine)), Some(&scope)).await {
                        Ok((n, b)) => log::info!("[Pins] freed {n} files, {b} bytes in {scope}"),
                        Err(e) => log::warn!("[Pins] freeing {scope}: {e}"),
                    }
                    *engine.overview_cache.lock().await = None;
                });
                Ok(format!("{name} is online only. Files that are backed up are being freed now; anything not yet uploaded stays."))
            }
            pins::Mode::WhenOpened => Ok(format!("{name}: files come down when you open them and stay until you free up space.")),
        }
    }

    /// Where a Workspace file is on this PC: its place in the sync folder (a
    /// placeholder is fine, Windows fetches the bytes when FL Studio reads it),
    /// or a copy downloaded for dragging earlier.
    pub async fn local_path_for(&self, ws_id: &str, file_id: &str, name: &str) -> Option<PathBuf> {
        let root = sync_root();
        let rel = {
            let idx = self.index.lock().await;
            idx.values()
                .find(|e| e.remote_id.as_deref() == Some(file_id) && e.workspace_id.as_deref() == Some(ws_id))
                .map(|e| e.rel_path.clone())
        };
        if let Some(rel) = rel {
            let p = root.join(rel);
            if p.exists() {
                return Some(p);
            }
        }
        let cached = drag_cache_path(file_id, name);
        cached.is_file().then_some(cached)
    }

    /// Download a file that is not in the sync folder, for dragging.
    pub async fn cache_for_drag(&self, ws_id: &str, file_id: &str, name: &str) -> Result<PathBuf, String> {
        let token = self.token.read().await.clone().ok_or("Sign in first.")?;
        let dest = drag_cache_path(file_id, name);
        let dir = dest.parent().ok_or("no folder")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!("{}download", crate::rules::TEMP_PREFIX));
        stream_to_file(&token, ws_id, file_id, &tmp).await?;
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        Ok(dest)
    }

    async fn index_put(&self, entry: SyncEntry) {
        let mut idx = self.index.lock().await;
        idx.insert(entry.rel_path.clone(), entry);
        write_index(&idx);
    }

    /// Put Workspace's version of a file in its place on this PC. Streamed to a
    /// temporary file beside it and moved over the old one at the end, so an
    /// interrupted download never leaves half a project where the whole one
    /// was, and a 2 GB recording never sits in memory (the old download read
    /// the whole body into memory and wrote straight over the file).
    async fn download_replace(&self, token: &str, ws_id: &str, file_id: &str, rel_path: &str) -> Result<SyncEntry, String> {
        if !is_safe_rel_path(rel_path) {
            return Err(format!("unsafe path {rel_path}"));
        }
        let dest = sync_root().join(rel_path);
        // What is at the path now. If it changes while the download runs (FL
        // Studio saved it), the download is thrown away instead of written
        // over that save; the next pass sees both sides changed.
        let stamp = |p: &Path| p.metadata().ok().map(|m| (m.len(), file_mtime(p)));
        let before = stamp(&dest);
        let dir = dest.parent().ok_or("no folder")?.to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let name = dest.file_name().and_then(|n| n.to_str()).ok_or("bad file name")?;
        let tmp = dir.join(format!("{}{}", crate::rules::TEMP_PREFIX, name));

        self.emit_file_progress(rel_path, "download", 0);
        let mut last_err = String::new();
        for attempt in 0..3u32 {
            match stream_to_file(token, ws_id, file_id, &tmp).await {
                Ok((sha, size)) => {
                    if stamp(&dest) != before {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(format!("{rel_path} changed on this PC during the download; left as it is"));
                    }
                    self.self_writes.lock().await.insert(rel_path.to_string(), sha.clone());
                    if let Err(e) = std::fs::rename(&tmp, &dest) {
                        let _ = std::fs::remove_file(&tmp);
                        self.self_writes.lock().await.remove(rel_path);
                        // Most often FL Studio holding the file open. Next pass.
                        return Err(format!("replace {}: {e}", dest.display()));
                    }
                    self.emit_file_progress(rel_path, "download", 100);
                    self.note("down", rel_path).await;
                    return Ok(SyncEntry {
                        rel_path: rel_path.to_string(),
                        sha256: sha,
                        modified: chrono::Utc::now().to_rfc3339(),
                        size,
                        remote_id: Some(file_id.to_string()),
                        workspace_id: Some(ws_id.to_string()),
                        mtime: file_mtime(&dest),
                        verified: true,
                    });
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    last_err = e;
                    tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
                }
            }
        }
        Err(last_err)
    }

    /// Start the sync loop. Call this once on app startup.
    pub async fn start(self: Arc<Self>) {
        let root = sync_root();
        let _ = std::fs::create_dir_all(&root);

        // Files On-Demand. Claim the sync root and start answering hydration
        // requests, so placeholders can be created below and opened later.
        // Every failure here is non-fatal: without it the engine simply
        // downloads files in full, which is what it did before.
        if crate::cloudfiles::is_supported() {
            match crate::cloudfiles::register(&root, "Hardwave Workspace") {
                Ok(()) => {
                    let token = Arc::clone(&self.token);
                    let stale_trigger = Arc::clone(&self.sse_trigger);
                    let fetcher: crate::hydration::Fetcher = std::sync::Arc::new(move |identity: String, offset: u64, length: u64, file_size: u64| {
                        let token = Arc::clone(&token);
                        let stale_trigger = Arc::clone(&stale_trigger);
                        Box::pin(async move {
                            // identity is "workspaceId/fileId", set when the
                            // placeholder was created.
                            let (ws_id, file_id) = identity
                                .split_once('/')
                                .ok_or_else(|| format!("bad file identity: {identity}"))?;
                            let tok = token.read().await.clone()
                                .ok_or_else(|| "not signed in".to_string())?;
                            let url = api::get_download_url(&tok, ws_id, file_id).await?;

                            // Ask object storage for just the slice Windows
                            // wants, so opening one sample does not pull a
                            // whole pack. Not every backend honours Range, so
                            // fall back to a whole-object GET rather than
                            // failing the hydration.
                            let end = offset + length.saturating_sub(1);
                            let ranged = api::http_client()
                                .get(&url)
                                .header("Range", format!("bytes={}-{}", offset, end))
                                .send().await
                                .map_err(|e| format!("hydrate request failed: {e}"))?;

                            // The placeholder describes one version of the file;
                            // Workspace serves the newest. When the sizes differ
                            // the file changed on another PC and this placeholder
                            // is stale: serving the new bytes cut to the old size
                            // gave a corrupt file (Windows test, 2026-10-09).
                            // Refuse, so the opening program gets an error, and
                            // run a sync pass, which refreshes the placeholder.
                            let stale = |total: u64| -> Result<(), String> {
                                if file_size > 0 && total != file_size {
                                    stale_trigger.notify_one();
                                    return Err(format!(
                                        "{identity} changed in Workspace ({total} bytes now, {file_size} here); the placeholder is refreshed on the next pass"
                                    ));
                                }
                                Ok(())
                            };
                            let status = ranged.status();
                            if status.as_u16() == 206 {
                                // "bytes 0-1023/52998": the number after / is the whole object.
                                let total = ranged
                                    .headers()
                                    .get(reqwest::header::CONTENT_RANGE)
                                    .and_then(|v| v.to_str().ok())
                                    .and_then(|v| v.rsplit('/').next())
                                    .and_then(|v| v.trim().parse::<u64>().ok());
                                if let Some(total) = total {
                                    stale(total)?;
                                }
                                let b = ranged.bytes().await
                                    .map_err(|e| format!("hydrate body failed: {e}"))?;
                                return Ok(b.to_vec());
                            }
                            if !status.is_success() {
                                return Err(format!("hydrate HTTP {status}"));
                            }
                            // 200 means the whole object came back; take the
                            // window Windows actually asked for.
                            let all = ranged.bytes().await
                                .map_err(|e| format!("hydrate body failed: {e}"))?;
                            stale(all.len() as u64)?;
                            let start = (offset as usize).min(all.len());
                            let stop = (start + length as usize).min(all.len());
                            Ok(all[start..stop].to_vec())
                        })
                    });

                    match crate::hydration::connect(&root, fetcher) {
                        Ok(conn) => {
                            log::info!("[Sync] Files On-Demand active at {}", root.display());
                            // The OS connection lives until CfDisconnectSyncRoot
                            // is called, which we never do while running. Park the
                            // handle rather than dropping it so a future clean
                            // shutdown has something to close.
                            static HYDRATION: std::sync::OnceLock<crate::hydration::Connection> =
                                std::sync::OnceLock::new();
                            let _ = HYDRATION.set(conn);
                        }
                        Err(e) => log::warn!("[Sync] hydration connect failed: {e} — falling back to full downloads"),
                    }
                }
                Err(e) => log::warn!("[Sync] sync-root registration failed: {e} — falling back to full downloads"),
            }
        }

        // Start file watcher for instant local change detection
        let _engine = Arc::clone(&self);
        let (fs_tx, mut fs_rx) = mpsc::channel::<String>(256);

        let watch_root = root.clone();
        std::thread::spawn(move || {
            let tx = fs_tx;
            let mut watcher = match notify::recommended_watcher(move |res: Result<Event, _>| {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                            for path in &event.paths {
                                if let Ok(rel) = path.strip_prefix(&watch_root) {
                                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                                    let _ = tx.blocking_send(rel_str);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    log::warn!("[Sync] Failed to create file watcher: {}", e);
                    return;
                }
            };
            if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
                log::warn!("[Sync] Failed to start watching '{}': {}", root.display(), e);
                return;
            }
            // Keep the watcher alive
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        });

        // Handle file system events
        let engine_fs = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(rel_path) = fs_rx.recv().await {
                if crate::rules::is_excluded(&rel_path) {
                    continue;
                }
                if *engine_fs.paused.read().await {
                    continue;
                }
                // Debounce: wait a bit for writes to finish
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                let _ = engine_fs.sync_local_file(&rel_path, false).await;
            }
        });

        // Periodic full sync loop — also wakes immediately on SSE event
        let engine_poll = Arc::clone(&self);
        let sse_trigger = Arc::clone(&self.sse_trigger);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(POLL_INTERVAL_SECS)) => {}
                    _ = sse_trigger.notified() => {
                        log::info!("[Sync] SSE trigger: running immediate sync");
                    }
                }
                if *engine_poll.paused.read().await {
                    continue;
                }
                let _ = engine_poll.full_sync().await;
            }
        });
    }

    /// Sync a single local file change to remote.
    /// Upload one changed local file. `force` is the producer's "keep this
    /// PC's" from a conflict; without it a file Workspace moved on from since
    /// the last sync becomes a conflict instead of being uploaded over.
    async fn sync_local_file(&self, rel_path: &str, force: bool) -> Result<(), String> {
        let token = self.token.read().await.clone();
        let token = match token {
            Some(t) => t,
            None => return Ok(()),
        };

        let root = sync_root();
        let full_path = root.join(rel_path);

        if !full_path.exists() {
            let mut index = self.index.lock().await;
            index.remove(rel_path);
            write_index(&index);
            return Ok(());
        }

        if full_path.is_dir() || crate::rules::is_excluded(rel_path) {
            return Ok(());
        }
        if !force && self.conflicts.lock().await.contains_key(rel_path) {
            return Ok(());
        }

        let meta = std::fs::metadata(&full_path).map_err(|e| e.to_string())?;
        let sha = hash_file(&full_path)?;

        // A download the engine just wrote: not a local change.
        {
            let mut writes = self.self_writes.lock().await;
            if writes.get(rel_path) == Some(&sha) {
                writes.remove(rel_path);
                return Ok(());
            }
        }

        // Check hash against index (brief lock)
        let known = {
            let index = self.index.lock().await;
            match index.get(rel_path) {
                Some(entry) if entry.sha256 == sha => return Ok(()),
                Some(entry) => Some((entry.sha256.clone(), entry.remote_id.clone(), entry.workspace_id.clone())),
                None => None,
            }
        };

        // Another PC may have saved a newer version that this one has not
        // pulled yet. Uploading now would bury it under this one. The check
        // fails closed: when it cannot be made, nothing is uploaded and the
        // next pass tries again (launch audit 2026-10-08).
        if !force {
            // A file this PC never synced may already exist in Workspace under
            // the same path, as an old copy on a second PC does. Only the full
            // pass, which holds the server's list, may decide that one.
            if known.is_none() {
                self.sse_trigger.notify_one();
                return Ok(());
            }
            if let Some((entry_sha, Some(remote_id), Some(ws_id))) = &known {
                let state = api::file_state(&token, ws_id, remote_id).await?;
                if let Some(remote) = state {
                    let remote_sha = remote.sha256.clone();
                    if remote_sha.as_deref() != Some(sha.as_str())
                        && !conflicts::may_upload(Some(entry_sha), Some(remote_sha.as_deref()))
                    {
                        self.add_conflict(Conflict {
                            rel_path: rel_path.to_string(),
                            workspace_id: ws_id.clone(),
                            remote_id: remote_id.clone(),
                            local_size: meta.len(),
                            local_mtime: file_mtime(&full_path),
                            remote_size: remote.size,
                            remote_sha: remote_sha.unwrap_or_default(),
                            remote_updated_at: remote.updated_at.clone(),
                            detected_at: chrono::Utc::now().to_rfc3339(),
                        }).await;
                        return Ok(());
                    }
                }
            }
        }

        let parts: Vec<&str> = rel_path.splitn(2, '/').collect();
        if parts.len() < 2 {
            return Ok(());
        }
        let workspace_name = parts[0];
        let file_path_in_ws = parts[1];

        if !is_safe_component(workspace_name) {
            return Err(format!("Unsafe workspace name: {}", workspace_name));
        }

        let workspaces = api::list_workspaces(&token).await?;
        let ws = workspaces.iter().find(|w| w.name == workspace_name);
        let ws_id = match ws {
            Some(w) => w.id.clone(),
            None => return Ok(()),
        };

        let filename = full_path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let folder = Path::new(file_path_in_ws).parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty());

        self.update_status("syncing", None).await;
        self.emit_file_progress(rel_path, "upload", 0);

        // Upload with retry (no index lock held)
        let file_size = meta.len();
        let upload = api::with_retry({
            let token = token.clone();
            let ws_id = ws_id.clone();
            let filename = filename.to_string();
            let folder = folder.map(|s| s.to_string());
            let sha = sha.clone();
            let full_path = full_path.clone();
            move || {
                let token = token.clone();
                let ws_id = ws_id.clone();
                let filename = filename.clone();
                let folder = folder.clone();
                let sha = sha.clone();
                let full_path = full_path.clone();
                Box::pin(async move {
                    let file_id = api::upload_file(&token, &ws_id, &filename, file_size, folder.as_deref(), &sha, &full_path).await?;
                    Ok::<_, String>((file_id, ws_id))
                })
            }
        }, 2).await?;

        self.emit_file_progress(rel_path, "upload", 100);
        self.note("up", rel_path).await;

        // Update index (brief lock)
        {
            let mut index = self.index.lock().await;
            index.insert(rel_path.to_string(), SyncEntry {
                rel_path: rel_path.to_string(),
                sha256: sha,
                modified: chrono::Utc::now().to_rfc3339(),
                size: meta.len(),
                remote_id: Some(upload.0),
                workspace_id: Some(upload.1),
                mtime: file_mtime(&full_path),
                verified: true,
            });
            write_index(&index);
        }

        self.update_status("idle", None).await;
        Ok(())
    }

    /// Full bidirectional sync — compare local index with remote state.
    async fn full_sync(&self) -> Result<(), String> {
        // Prevent overlapping sync cycles
        if self.sync_active.swap(true, Ordering::SeqCst) {
            log::info!("[Sync] Full sync already in progress, skipping");
            return Ok(());
        }
        let _guard = SyncGuard(&self.sync_active);

        let token = self.token.read().await.clone();
        let token = match token {
            Some(t) => t,
            None => return Ok(()),
        };

        self.update_status("syncing", None).await;

        let result = self.do_full_sync(&token).await;

        let mut status = self.status.write().await;
        if result.is_ok() {
            status.state = "idle".into();
            status.last_sync = Some(chrono::Utc::now().to_rfc3339());
            status.error = None;
        } else {
            status.state = "error".into();
            status.error = result.as_ref().err().cloned();
        }
        let _ = self.app.emit("sync:status", status.clone());

        result
    }

    /// Inner sync logic (status already set to "syncing").
    async fn do_full_sync(&self, token: &str) -> Result<(), String> {
        let root = sync_root();
        let workspaces = api::list_workspaces(token).await?;
        log::info!("[Sync] Full sync: {} workspace(s)", workspaces.len());
        let own: Vec<String> = workspaces.iter().filter(|w| is_safe_component(&w.name)).map(|w| w.name.clone()).collect();
        let changed = self.own_workspaces.read().await.as_ref() != Some(&own);
        if changed {
            *self.own_workspaces.write().await = Some(own);
            *self.disk_cache.write().await = None;
            *self.overview_cache.lock().await = None;
        }

        for ws in &workspaces {
            if !is_safe_component(&ws.name) {
                log::info!("[Sync] Skipping workspace with unsafe name: {}", ws.name);
                continue;
            }
            let ws_dir = root.join(&ws.name);
            let _ = std::fs::create_dir_all(&ws_dir);

            if let Ok(folders) = api::list_folders(token, &ws.id).await {
                for f in &folders {
                    let folder_path = f.path.as_deref()
                        .unwrap_or(&f.name)
                        .trim_matches('/');
                    if !folder_path.is_empty() && is_safe_rel_path(folder_path) {
                        let _ = std::fs::create_dir_all(ws_dir.join(folder_path));
                    }
                }
            }

            let remote_files = match api::list_files(token, &ws.id).await {
                Ok(f) => {
                    log::info!("[Sync] Workspace '{}': {} remote file(s)", ws.name, f.len());
                    f
                }
                Err(e) => {
                    log::warn!("[Sync] Failed to list files for '{}': {}", ws.name, e);
                    continue;
                }
            };

            // ── Download phase ─────────────────────────────────────
            // Determine what needs downloading (index lock held briefly)
            // Each candidate carries what the index remembers about it (sha,
            // size, mtime), so the decision below needs no lock. The checksum
            // of every remote file by path is kept for the upload scan.
            type Remembered = Option<(String, u64, Option<i64>)>;
            let mut remote_sha_by_rel: HashMap<String, Option<String>> = HashMap::new();
            let rel_of = |rf: &api::WorkspaceFile| {
                let folder = rf.folder_path.as_deref().unwrap_or("/").trim_matches('/');
                if folder.is_empty() {
                    format!("{}/{}", ws.name, rf.name)
                } else {
                    format!("{}/{}/{}", ws.name, folder, rf.name)
                }
            };
            // Several server rows can land on one path here: folders whose
            // stored path is wrong (635 in production on 2026-10-08) and names
            // that differ only in case, which Windows treats as one file.
            // Deciding each row separately replaced the file with the other row
            // on alternate passes. The newest row wins; the others are left
            // alone. A path that would leave the sync folder is never used.
            let mut newest_by_path: HashMap<String, usize> = HashMap::new();
            let (mut unsafe_paths, mut collapsed) = (0usize, 0usize);
            for (i, rf) in remote_files.iter().enumerate() {
                let rel = rel_of(rf);
                if !is_safe_rel_path(&rel) {
                    unsafe_paths += 1;
                    continue;
                }
                let key = rel.to_lowercase();
                match newest_by_path.get(&key) {
                    Some(&j) if remote_files[j].updated_at >= rf.updated_at => collapsed += 1,
                    Some(_) => {
                        collapsed += 1;
                        newest_by_path.insert(key, i);
                    }
                    None => {
                        newest_by_path.insert(key, i);
                    }
                }
            }
            if unsafe_paths + collapsed > 0 {
                log::info!("[Sync] '{}': {} unsafe path(s) skipped, {} duplicate path(s) folded into the newest", ws.name, unsafe_paths, collapsed);
            }
            let mut chosen: Vec<usize> = newest_by_path.into_values().collect();
            chosen.sort_unstable();
            let to_download: Vec<(String, api::WorkspaceFile, Remembered)> = {
                let idx = self.index.lock().await;
                chosen.iter().map(|&i| &remote_files[i]).filter_map(|rf| {
                    let rel_path = rel_of(rf);
                    remote_sha_by_rel.insert(rel_path.clone(), rf.sha256.clone().filter(|h| !h.is_empty()));
                    let entry = idx.get(&rel_path);
                    let should = match entry {
                        Some(e) => rf.sha256.as_deref().is_some_and(|h| !h.is_empty() && h != e.sha256),
                        None => true,
                    };
                    should.then(|| (rel_path, rf.clone(), entry.map(|e| (e.sha256.clone(), e.size, e.mtime))))
                }).collect()
            };

            // Process downloads without holding the index lock.
            //
            // Flushed in batches, NOT only at the end: reconciling a big
            // workspace hashes every local file that is not yet indexed, and
            // writing the results once at the end meant an interrupted pass
            // (app closed, sleep, network) threw all of it away and started
            // from nothing next time. On a 136k-file workspace that pass never
            // reached the end, so the index stayed near-empty for months and
            // Free Up Space had almost nothing it was allowed to touch.
            const INDEX_FLUSH_EVERY: usize = 200;
            let mut downloaded: Vec<SyncEntry> = Vec::new();
            let mut pending_flush: Vec<SyncEntry> = Vec::new();
            for (rel_path, rf, remembered) in &to_download {
                if pending_flush.len() >= INDEX_FLUSH_EVERY {
                    let mut idx = self.index.lock().await;
                    for e in pending_flush.drain(..) {
                        idx.insert(e.rel_path.clone(), e);
                    }
                    write_index(&idx);
                }
                let local_path = root.join(rel_path);

                // Waiting on the producer's choice: leave both sides alone.
                if self.conflicts.lock().await.contains_key(rel_path.as_str()) {
                    continue;
                }

                // What is on this PC, the last synced version and Workspace's
                // decide together (conflicts.rs). Matching by size and time
                // comes first and hashing only when that cannot tell: hashing
                // was the hour-long first pass, and it hydrated any cloud-only
                // placeholder it read.
                let local = if !local_path.exists() {
                    Local::Missing
                } else if crate::cloudfiles::is_placeholder(&local_path) {
                    Local::Placeholder
                } else {
                    Local::File {
                        size: local_path.metadata().map(|m| m.len()).unwrap_or(0),
                        mtime: file_mtime(&local_path),
                    }
                };
                let indexed = remembered.as_ref().map(|(sha, size, mtime)| Indexed { sha, size: *size, mtime: *mtime });
                let trusted = match local {
                    Local::File { size, mtime } => trust_without_hash(
                        size,
                        mtime,
                        rf.size,
                        rf.sha256.as_deref(),
                        rf.created_at.as_deref(),
                    ),
                    _ => false,
                };
                let mut action = conflicts::plan_remote(indexed, local, rf.sha256.as_deref(), rf.size, trusted);
                let mut hashed = false;
                if action == RemoteAction::NeedsHash {
                    action = match hash_file(&local_path) {
                        Ok(h) => {
                            hashed = true;
                            conflicts::after_hash(indexed, &h, rf.sha256.as_deref())
                        }
                        // Unreadable, open in FL Studio say: try again next pass.
                        Err(_) => RemoteAction::Skip,
                    };
                }

                match action {
                    RemoteAction::Skip | RemoteAction::NeedsHash => continue,
                    RemoteAction::Index => {
                        let Local::File { size, mtime } = local else { continue };
                        let entry = SyncEntry {
                            rel_path: rel_path.clone(),
                            sha256: rf.sha256.clone().unwrap_or_default(),
                            modified: chrono::Utc::now().to_rfc3339(),
                            size,
                            remote_id: Some(rf.id.clone()),
                            workspace_id: Some(ws.id.clone()),
                            mtime,
                            // Matched from metadata alone: Free Up Space hashes it first.
                            verified: hashed,
                        };
                        pending_flush.push(entry.clone());
                        downloaded.push(entry);
                        continue;
                    }
                    RemoteAction::Conflict => {
                        let Local::File { size, mtime } = local else { continue };
                        self.add_conflict(Conflict {
                            rel_path: rel_path.clone(),
                            workspace_id: ws.id.clone(),
                            remote_id: rf.id.clone(),
                            local_size: size,
                            local_mtime: mtime,
                            remote_size: rf.size,
                            remote_sha: rf.sha256.clone().unwrap_or_default(),
                            remote_updated_at: rf.updated_at.clone(),
                            detected_at: chrono::Utc::now().to_rfc3339(),
                        }).await;
                        continue;
                    }
                    RemoteAction::RefreshPlaceholder => {
                        let identity = format!("{}/{}", ws.id, rf.id);
                        match crate::cloudfiles::refresh_placeholder(&root, rel_path, rf.size, &identity) {
                            Ok(()) => {
                                let entry = SyncEntry {
                                    rel_path: rel_path.clone(),
                                    sha256: rf.sha256.clone().unwrap_or_default(),
                                    modified: chrono::Utc::now().to_rfc3339(),
                                    size: rf.size,
                                    remote_id: Some(rf.id.clone()),
                                    workspace_id: Some(ws.id.clone()),
                                    mtime: file_mtime(&local_path),
                                    verified: true,
                                };
                                pending_flush.push(entry.clone());
                                downloaded.push(entry);
                                self.emit_file_progress(rel_path, "placeholder", 100);
                            }
                            Err(e) => log::warn!("[Sync] Placeholder refresh failed for '{}': {}", rel_path, e),
                        }
                        continue;
                    }
                    RemoteAction::Fetch => {
                        // Untouched here and newer in Workspace: replace it whole,
                        // so a file that lived on this PC stays on this PC.
                        if let Local::File { .. } = local {
                            match self.download_replace(token, &ws.id, &rf.id, rel_path).await {
                                Ok(entry) => {
                                    pending_flush.push(entry.clone());
                                    downloaded.push(entry);
                                }
                                Err(e) => log::warn!("[Sync] Update failed for '{}': {}", rel_path, e),
                            }
                            continue;
                        }
                        // Not here yet: a placeholder or a download, below.
                    }
                }

                if let Some(parent) = local_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }

                // Files On-Demand: create a placeholder rather than pulling the
                // bytes down. It shows in Explorer at full size for ~1 KB of
                // disk, and Windows calls our hydration handler if anything
                // actually opens it. Falls through to a real download when the
                // platform does not support it (non-Windows, or pre-1709).
                // A folder kept "always on this PC" gets the bytes, not a placeholder.
                let always = pins::mode_for(rel_path, &*self.modes.lock().await) == pins::Mode::Always;
                if crate::cloudfiles::is_supported() && !always {
                    let dir = rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                    let ph = crate::cloudfiles::RemoteFile {
                        rel_path: rel_path.clone(),
                        size: rf.size,
                        identity: format!("{}/{}", ws.id, rf.id),
                    };
                    match crate::cloudfiles::create_placeholders(&root, dir, &[ph]) {
                        Ok(n) if n > 0 => {
                            log::info!("[Sync] Placeholder: {}", rel_path);
                            let entry = SyncEntry {
                                rel_path: rel_path.clone(),
                                // The remote hash is authoritative until the
                                // file is hydrated and edited locally.
                                sha256: rf.sha256.clone().unwrap_or_default(),
                                modified: chrono::Utc::now().to_rfc3339(),
                                size: rf.size,
                                remote_id: Some(rf.id.clone()),
                                workspace_id: Some(ws.id.clone()),
                                // Record the placeholder's mtime so the upload
                                // scan skips it by size+mtime and never hashes
                                // (which would hydrate) a cloud-only file.
                                mtime: file_mtime(&local_path),
                                verified: true,
                            };
                            pending_flush.push(entry.clone());
                            downloaded.push(entry);
                            self.emit_file_progress(rel_path, "placeholder", 100);
                            continue;
                        }
                        Ok(_) => log::info!("[Sync] Placeholder skipped for '{}'", rel_path),
                        Err(e) => log::warn!("[Sync] Placeholder failed for '{}': {} — downloading instead", rel_path, e),
                    }
                }

                log::info!("[Sync] Downloading: {}", rel_path);
                match self.download_replace(token, &ws.id, &rf.id, rel_path).await {
                    Ok(entry) => downloaded.push(entry),
                    Err(e) => log::warn!("[Sync] Download failed for '{}': {}", rel_path, e),
                }
            }

            // Whatever the batch flush above has not written yet.
            if !downloaded.is_empty() {
                let mut idx = self.index.lock().await;
                for e in &downloaded {
                    idx.insert(e.rel_path.clone(), e.clone());
                }
                write_index(&idx);
            }

            // ── Upload phase ────────────────────────────────────────
            let local_files = scan_local(&ws_dir);

            // Snapshot index metadata once, then release the lock. The scan
            // below does file I/O (stat, and sometimes a hash) and must NOT
            // hold the mutex while it runs — holding it here while SHA-256'ing
            // every file is what wedged the whole engine on a big first sync.
            // Tuple: (size, sha256, mtime, already_uploaded).
            let idx_snapshot: HashMap<String, (u64, String, Option<i64>, bool)> = {
                let idx = self.index.lock().await;
                idx.iter()
                    .map(|(k, e)| (k.clone(), (e.size, e.sha256.clone(), e.mtime, e.remote_id.is_some())))
                    .collect()
            };

            // Decide what to upload WITHOUT hashing new files. `sha` is None
            // when hashing is deferred to the upload worker (untracked or
            // size-changed files), so a first sync of 100k files starts
            // uploading at once instead of hashing 100 GB up front. It is Some
            // only when we already had to hash to confirm a real edit.
            let mut to_upload: Vec<(String, PathBuf, u64, Option<String>)> = Vec::new();
            let mut backfill_mtime: Vec<(String, i64)> = Vec::new();
            let waiting: std::collections::HashSet<String> = self.conflicts.lock().await.keys().cloned().collect();
            for (local_rel, local_path, size) in &local_files {
                let full_rel = format!("{}/{}", ws.name, local_rel);
                let indexed = idx_snapshot.get(&full_rel);
                // A conflict waits for the producer; and a file Workspace moved
                // on from since the last sync is the download side's call.
                if waiting.contains(&full_rel)
                    || !conflicts::may_upload(
                        indexed.map(|e| e.1.as_str()),
                        remote_sha_by_rel.get(&full_rel).map(|o| o.as_deref()),
                    )
                {
                    continue;
                }
                let mtime = file_mtime(local_path);
                let uploaded = indexed.map(|e| e.3).unwrap_or(false);
                match plan_upload(indexed.map(|e| e.0), indexed.and_then(|e| e.2), uploaded, *size, mtime) {
                    UploadPlan::Skip => {}
                    // Already uploaded, just never tagged: record its mtime, no
                    // hash, no re-upload (and no placeholder hydration).
                    UploadPlan::TrustBackfill => {
                        if let Some(m) = mtime {
                            backfill_mtime.push((full_rel, m));
                        }
                    }
                    UploadPlan::UploadNew => {
                        to_upload.push((full_rel, local_path.clone(), *size, None));
                    }
                    UploadPlan::VerifyByHash => match hash_file(local_path) {
                        // Unchanged after all: record the mtime so the next pass
                        // skips it for free instead of hashing it again. This is
                        // what stops the engine re-hashing everything every cycle.
                        Ok(h) if indexed.map(|e| e.1.as_str()) == Some(h.as_str()) => {
                            if let Some(m) = mtime {
                                backfill_mtime.push((full_rel, m));
                            }
                        }
                        Ok(h) => {
                            log::info!("[Sync] Modified, re-uploading: {}", full_rel);
                            to_upload.push((full_rel, local_path.clone(), *size, Some(h)));
                        }
                        // Unreadable (locked by the DAW, say). Leave for next pass.
                        Err(_) => {}
                    },
                }
            }

            // Backfill mtimes for files confirmed unchanged (brief lock).
            if !backfill_mtime.is_empty() {
                let mut idx = self.index.lock().await;
                for (rel, m) in &backfill_mtime {
                    if let Some(e) = idx.get_mut(rel) {
                        e.mtime = Some(*m);
                    }
                }
                write_index(&idx);
            }

            // Upload in parallel batches. 16-wide to push through a large
            // backlog of many small files, where per-file API round-trips (not
            // bandwidth) are the limit; the server upload rate limit was raised
            // to match. The index lock is only taken for the brief per-batch
            // flush below.
            let mut uploaded_count = 0usize;
            // A continuous queue, not batches. Batching waited for the slowest file in each group
            // of sixteen, so one 4 GB recording left fifteen workers idle; on a line with 5.9
            // Mbit/s spare that showed up as a fifth of the bandwidth being used.
            let uploads = futures_util::stream::iter(to_upload.into_iter().map(
                |(full_rel, local_path, size, sha_opt)| {
                    let token = token.to_string();
                    let ws_id = ws.id.clone();
                    let ws_name = ws.name.clone();

                    async move {
                        let rel_in_ws = full_rel.strip_prefix(&format!("{}/", ws_name))
                            .unwrap_or(&full_rel);
                        let filename = local_path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let folder = Path::new(rel_in_ws).parent()
                            .and_then(|p| p.to_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string());

                        // Record the file's mtime, and hash it now if hashing was
                        // deferred — once, off the async worker, before any retry.
                        let entry_mtime = file_mtime(&local_path);
                        let sha = match sha_opt {
                            Some(s) => s,
                            None => {
                                let p = local_path.clone();
                                match tokio::task::spawn_blocking(move || hash_file(&p)).await {
                                    Ok(Ok(h)) => h,
                                    Ok(Err(e)) => return Err(format!("hash failed: {}", e)),
                                    Err(e) => return Err(format!("hash task panicked: {}", e)),
                                }
                            }
                        };

                        let upload = api::with_retry({
                            let token = token.clone();
                            let ws_id = ws_id.clone();
                            let filename = filename.clone();
                            let folder = folder.clone();
                            let sha = sha.clone();
                            let local_path = local_path.clone();
                            move || {
                                let token = token.clone();
                                let ws_id = ws_id.clone();
                                let filename = filename.clone();
                                let folder = folder.clone();
                                let sha = sha.clone();
                                let local_path = local_path.clone();
                                let full_rel = full_rel.clone();
                                Box::pin(async move {
                                    let file_id = api::upload_file(&token, &ws_id, &filename, size, folder.as_deref(), &sha, &local_path).await?;
                                    Ok::<_, String>((full_rel.clone(), sha.clone(), size, file_id, ws_id.clone()))
                                })
                            }
                        }, 2).await?;
                        let (fr, sh, sz, fid, wid) = upload;
                        Ok::<_, String>((fr, sh, sz, fid, wid, entry_mtime))
                    }
                },
            ))
            .buffer_unordered(self.upload_parallel.load(Ordering::Relaxed).max(UPLOAD_PARALLEL_MIN));

            futures_util::pin_mut!(uploads);
            // Outcomes of this pass, so the width can follow the line (see below).
            let mut pass_ok = 0usize;
            let mut pass_failed = 0usize;
            // Record finished files in small groups: the index is written to disk each time, and
            // writing it after every single file on a 120k-file workspace costs more than it saves.
            let mut pending_entries: Vec<SyncEntry> = Vec::new();
            while let Some(result) = uploads.next().await {
                match result {
                    Ok((full_rel, sha, size, file_id, ws_id, entry_mtime)) => {
                        pass_ok += 1;
                        self.note("up", &full_rel).await;
                        pending_entries.push(SyncEntry {
                            rel_path: full_rel,
                            sha256: sha,
                            modified: chrono::Utc::now().to_rfc3339(),
                            size,
                            remote_id: Some(file_id),
                            workspace_id: Some(ws_id),
                            mtime: entry_mtime,
                            verified: true,
                        });
                    }
                    Err(e) => {
                        pass_failed += 1;
                        log::warn!("[Sync] Upload failed: {}", e);
                    }
                }

                if pending_entries.len() >= UPLOAD_FLUSH_EVERY {
                    uploaded_count += pending_entries.len();
                    {
                        let mut idx = self.index.lock().await;
                        for e in &pending_entries {
                            idx.insert(e.rel_path.clone(), e.clone());
                        }
                        write_index(&idx);
                    }
                    pending_entries.clear();
                    let _ = self.app.emit("sync:status", self.get_status().await);
                }
            }
            if !pending_entries.is_empty() {
                uploaded_count += pending_entries.len();
                {
                    let mut idx = self.index.lock().await;
                    for e in &pending_entries {
                        idx.insert(e.rel_path.clone(), e.clone());
                    }
                    write_index(&idx);
                }
                let _ = self.app.emit("sync:status", self.get_status().await);
            }
            if uploaded_count > 0 {
                log::info!("[Sync] Uploaded {} file(s) in '{}'", uploaded_count, ws.name);
            }

            // Follow the line. Most uploads failing means the path to storage is
            // refusing work, and sixteen hung transfers at once only make that
            // worse, so halve the queue; a clean pass widens it again. Both ends
            // are clamped, and a pass that tried nothing changes nothing.
            let tried = pass_ok + pass_failed;
            if tried > 0 {
                let width = self.upload_parallel.load(Ordering::Relaxed).max(UPLOAD_PARALLEL_MIN);
                let next = if pass_failed * 2 > tried {
                    (width / 2).max(UPLOAD_PARALLEL_MIN)
                } else if pass_failed * 10 <= tried {
                    (width + 2).min(UPLOAD_PARALLEL)
                } else {
                    width
                };
                if next != width {
                    log::info!(
                        "[Sync] {}/{} uploads failed in '{}': {} at a time -> {}",
                        pass_failed, tried, ws.name, width, next
                    );
                    self.upload_parallel.store(next, Ordering::Relaxed);
                }
            }
        }

        Ok(())
    }

    /// Spawn an SSE listener task for one workspace. Returns an abort handle.
    fn start_sse_listener(&self, workspace_id: String, token: String) -> tokio::task::AbortHandle {
        let paused_arc = Arc::clone(&self.paused);
        let app = self.app.clone();
        let sse_trigger = Arc::clone(&self.sse_trigger);
        let sync_flag = std::sync::Arc::new(AtomicBool::new(false));

        let handle = tokio::spawn(async move {
            // SSE client needs a no-timeout HTTP client (separate from the shared one)
            let sse_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(0)) // no timeout for SSE
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());

            // The token goes in a header, not the query string. reqwest prints the whole URL in
            // its error messages, so a token in the query ends up in the log file in plain text
            // on every failed connect, which is where the founder's admin token was found on
            // 2026-09-16. The route accepts either; the query form exists for browser EventSource,
            // which cannot set headers. This is not a browser.
            let url = format!("{}/workspaces/{}/events", api::ws_base(), workspace_id);

            let mut backoff_ms = 2_000u64;
            loop {
                log::info!("[SSE] Connecting to workspace {}", workspace_id);
                match sse_client.get(&url).bearer_auth(&token).send().await {
                    Err(e) => {
                        log::warn!("[SSE] Connect error: {}", redact_token(&e.to_string()));
                    }
                    Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                        || resp.status() == reqwest::StatusCode::FORBIDDEN => {
                        log::warn!("[SSE] Auth error {}: stopping SSE for workspace {}", resp.status(), workspace_id);
                        return; // 401/403: the token is gone, retrying will not help
                    }
                    Ok(resp) if !resp.status().is_success() => {
                        // Maintenance (503) or a server error: keep trying with backoff.
                        // This used to stop the stream for good, so live updates only
                        // came back after the app was restarted.
                        log::info!("[SSE] Server answered {} for workspace {}; retrying", resp.status(), workspace_id);
                    }
                    Ok(resp) => {
                        backoff_ms = 2_000; // reset on successful connect
                        let mut stream = resp.bytes_stream();
                        let mut buf = String::new();
                        use futures_util::StreamExt;

                        while let Some(Ok(chunk)) = stream.next().await {
                            buf.push_str(&String::from_utf8_lossy(&chunk));
                            while let Some(pos) = buf.find("\n\n") {
                                let msg = buf[..pos].to_string();
                                buf = buf[pos + 2..].to_string();
                                for line in msg.lines() {
                                    if let Some(data) = line.strip_prefix("data: ") {
                                        if let Ok(ev) = serde_json::from_str::<serde_json::Value>(data) {
                                            let kind = ev["type"].as_str().unwrap_or("");
                                            if kind == "ping" { continue; }
                                            log::info!("[SSE] Event: {}", kind);
                                            // Trigger immediate sync if not already running
                                            if !*paused_arc.read().await
                                                && !sync_flag.swap(true, Ordering::SeqCst)
                                            {
                                                // Notify the webview
                                                if let Some(win) = app.get_webview_window("main") {
                                                    crate::safe_eval(&win, "__HW_SSE_EVENT__", &ev);
                                                }
                                                // Small delay to batch rapid successive events
                                                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                                                // Wake the poll loop for an immediate sync
                                                sse_trigger.notify_one();
                                                sync_flag.store(false, Ordering::SeqCst);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        log::info!("[SSE] Stream ended for workspace {}", workspace_id);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(60_000);
            }
        });
        handle.abort_handle()
    }
}

/// RAII guard to reset sync_active on scope exit.
struct SyncGuard<'a>(&'a AtomicBool);
impl Drop for SyncGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A file Free Up Space may convert to a placeholder, once proven.
struct FreeCandidate {
    rel_path: String,
    path: PathBuf,
    identity: String,
    size: u64,
    expected_sha: String,
    /// Hash before discarding. False only for an entry that is verified AND
    /// unchanged since (same size and mtime as recorded).
    #[allow(dead_code)] // every candidate is hashed now; the flag stays for the callers that set it.
    needs_hash: bool,
    /// Index entry to record once freed, for a server match the index did not
    /// have yet. None for files already in the index.
    new_entry: Option<SyncEntry>,
}

/// Match local files the index does not know against what the server holds.
///
/// Metadata only, no hashing, so this takes seconds rather than hours:
/// - a file that passes `trust_without_hash` becomes an unverified index entry;
/// - a file with the same size and a server checksum that does not pass (it is
///   newer, or times are unknown) becomes a hash candidate. It is NOT indexed
///   yet, because an index entry would make the upload scan skip a real edit.
///
/// Nothing is discarded here. `free_up_space` hashes every unverified or
/// changed file before converting it.
async fn reconcile_from_server(
    engine: &Arc<SyncEngine>,
    root: &Path,
    index: &HashMap<String, SyncEntry>,
) -> Result<(Vec<SyncEntry>, Vec<FreeCandidate>), String> {
    let token = engine.token.read().await.clone().ok_or_else(|| "not signed in".to_string())?;
    let workspaces = crate::api::list_workspaces(&token).await?;
    let mut trusted = Vec::new();
    let mut to_hash = Vec::new();

    for ws in workspaces {
        let files = match crate::api::list_files(&token, &ws.id).await {
            Ok(f) => f,
            Err(e) => {
                log::info!("[FreeSpace] list '{}': {e}", ws.name);
                continue;
            }
        };

        for rf in files {
            let folder = rf.folder_path.as_deref().unwrap_or("/").trim_matches('/');
            let rel_path = if folder.is_empty() {
                format!("{}/{}", ws.name, rf.name)
            } else {
                format!("{}/{}/{}", ws.name, folder, rf.name)
            };

            // Already known and usable.
            if index.get(&rel_path).is_some_and(|e| e.remote_id.is_some() && e.workspace_id.is_some()) {
                continue;
            }

            let path = root.join(&rel_path);
            if !path.is_file() || crate::cloudfiles::is_placeholder(&path) {
                continue;
            }
            let size = match path.metadata() {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            let Some(remote_sha) = rf.sha256.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            if size != rf.size {
                continue;
            }
            let mtime = file_mtime(&path);
            let entry = SyncEntry {
                rel_path: rel_path.clone(),
                sha256: remote_sha.to_string(),
                modified: rf.updated_at.clone().unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
                size,
                remote_id: Some(rf.id.clone()),
                workspace_id: Some(ws.id.clone()),
                mtime,
                verified: false,
            };

            if trust_without_hash(size, mtime, rf.size, Some(remote_sha), rf.created_at.as_deref()) {
                trusted.push(entry);
            } else {
                to_hash.push(FreeCandidate {
                    rel_path,
                    path,
                    identity: format!("{}/{}", ws.id, rf.id),
                    size,
                    expected_sha: remote_sha.to_string(),
                    needs_hash: true,
                    // Recorded only after the hash proves it, and then as verified.
                    new_entry: Some(SyncEntry { verified: true, ..entry }),
                });
            }
        }
    }
    Ok((trusted, to_hash))
}

/// How many files Free Up Space hashes and converts at once.
/// Files uploading at the same time. Sixteen keeps a queue of small files moving without the
/// per-file API round trips becoming the limit, and with the queue continuous a few large files
/// no longer block the rest.
/// Strip any `token=...` out of text on its way to a log.
///
/// Logs get pasted into bug reports and support mail. A login token in one is a working key to
/// somebody's account for as long as it lives.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct FolderRow {
    /// Relative to the sync root, `/` separators.
    pub folder: String,
    pub name: String,
    /// 0 for a workspace, 1 for a folder in it.
    pub depth: u8,
    pub files: u64,
    pub bytes_here: u64,
    pub bytes_online: u64,
    pub mode: Option<pins::Mode>,
    /// The mode that applies, set here or on the workspace above.
    pub effective: Option<pins::Mode>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct FolderOverview {
    pub rows: Vec<FolderRow>,
    pub bytes_here: u64,
    pub bytes_online: u64,
}

/// Walk the sync root once: a placeholder counts as online only (its size is
/// what Explorer shows), every other file as here.
fn build_overview(root: &Path, modes: &HashMap<String, pins::Mode>, own: Option<&[String]>) -> FolderOverview {
    fn skip(name: &str) -> bool {
        name.starts_with('.') || crate::rules::is_excluded(name)
    }
    fn count(p: &Path, files: &mut u64, here: &mut u64, online: &mut u64) {
        if let Ok(m) = p.metadata() {
            *files += 1;
            if crate::cloudfiles::is_placeholder(p) {
                *online += m.len();
            } else {
                *here += m.len();
            }
        }
    }
    fn walk(dir: &Path, files: &mut u64, here: &mut u64, online: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if skip(&e.file_name().to_string_lossy()) {
                continue;
            }
            if p.is_dir() {
                walk(&p, files, here, online);
            } else {
                count(&p, files, here, online);
            }
        }
    }
    let row = |folder: String, name: String, depth: u8| FolderRow {
        mode: modes.get(&folder).copied(),
        effective: Some(pins::mode_for(&folder, modes)),
        folder,
        name,
        depth,
        files: 0,
        bytes_here: 0,
        bytes_online: 0,
    };
    let mut out = FolderOverview::default();
    let Ok(workspaces) = std::fs::read_dir(root) else { return out };
    let mut ws_dirs: Vec<PathBuf> = workspaces.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    ws_dirs.sort();
    for ws in ws_dirs {
        let ws_name = ws.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if ws_name.starts_with('.') || own.is_some_and(|o| !o.contains(&ws_name)) {
            continue;
        }
        // Each file is read once: a workspace's totals are its loose files plus
        // its folders' totals. Walking the workspace and then each folder again
        // doubled the work, and a sync folder can hold hundreds of GB.
        let mut ws_row = row(ws_name.clone(), ws_name.clone(), 0);
        let mut subs: Vec<PathBuf> = Vec::new();
        for e in std::fs::read_dir(&ws).map(|r| r.flatten().collect::<Vec<_>>()).unwrap_or_default() {
            let p = e.path();
            if skip(&e.file_name().to_string_lossy()) {
                continue;
            }
            if p.is_dir() {
                subs.push(p);
            } else {
                count(&p, &mut ws_row.files, &mut ws_row.bytes_here, &mut ws_row.bytes_online);
            }
        }
        subs.sort();
        let mut sub_rows = Vec::with_capacity(subs.len());
        for sub in subs {
            let name = sub.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let mut r = row(format!("{ws_name}/{name}"), name, 1);
            walk(&sub, &mut r.files, &mut r.bytes_here, &mut r.bytes_online);
            ws_row.files += r.files;
            ws_row.bytes_here += r.bytes_here;
            ws_row.bytes_online += r.bytes_online;
            sub_rows.push(r);
        }
        out.bytes_here += ws_row.bytes_here;
        out.bytes_online += ws_row.bytes_online;
        out.rows.push(ws_row);
        out.rows.extend(sub_rows);
    }
    out
}

/// Bring every cloud-only file under `dir` onto this PC. Returns (done, failed).
fn hydrate_tree(dir: &Path) -> (u32, u32) {
    let (mut done, mut failed) = (0u32, 0u32);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if crate::cloudfiles::is_placeholder(&p) {
                match crate::cloudfiles::hydrate(&p) {
                    Ok(()) => done += 1,
                    Err(err) => {
                        failed += 1;
                        log::warn!("[Pins] {}: {err}", p.display());
                    }
                }
            }
        }
    }
    (done, failed)
}

/// Copies of files dragged out of the window that were not in the sync folder.
fn drag_cache_path(file_id: &str, name: &str) -> PathBuf {
    let safe_id: String = file_id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let safe_name: String = name.chars().map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c }).collect();
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hardwave")
        .join("drag")
        .join(safe_id)
        .join(if safe_name.trim().is_empty() { "sound".to_string() } else { safe_name })
}

/// Stream one file from Workspace to `path`, hashing on the way.
async fn stream_to_file(token: &str, ws_id: &str, file_id: &str, path: &Path) -> Result<(String, u64), String> {
    use tokio::io::AsyncWriteExt;
    let url = api::get_download_url(token, ws_id, file_id).await?;
    let res = api::http_client().get(&url).send().await.map_err(|e| format!("Download failed: {}", e))?;
    if !res.status().is_success() {
        return Err(format!("Download failed: {}", res.status()));
    }
    let mut file = tokio::fs::File::create(path).await.map_err(|e| format!("Write failed: {}", e))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut body = res.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("Read body failed: {}", e))?;
        hasher.update(&chunk);
        size += chunk.len() as u64;
        file.write_all(&chunk).await.map_err(|e| format!("Write failed: {}", e))?;
    }
    file.flush().await.map_err(|e| format!("Write failed: {}", e))?;
    file.sync_all().await.map_err(|e| format!("Write failed: {}", e))?;
    Ok((hex::encode(hasher.finalize()), size))
}

pub fn redact_token(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("token=") {
        out.push_str(&rest[..at + "token=".len()]);
        out.push_str("REDACTED");
        let after = &rest[at + "token=".len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'))
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Files uploading at the same time when the line is behaving. A pass that
/// fails most of its uploads halves this, down to one, and a clean pass grows
/// it back.
///
/// Sixteen was too many. A home line carries one upload rate no matter how it
/// is divided, and on 2026-09-17 sixteen shares of 2.7 MB/s meant no single
/// 60 MB stem could finish before its own timeout: 240 files, six hours, zero
/// uploaded. Eight files of two parts each is sixteen connections at most,
/// which is enough to keep the line full without starving any one transfer.
const UPLOAD_PARALLEL: usize = 8;
const UPLOAD_PARALLEL_MIN: usize = 1;
/// How many finished files to collect before writing the index to disk.
const UPLOAD_FLUSH_EVERY: usize = 16;

const FREE_SPACE_PARALLEL: usize = 4;

/// Convert every already-uploaded local file into a dehydrated placeholder.
///
/// The OneDrive "Free up space" behaviour. Files stay visible and openable;
/// only their bytes go, and opening one pulls it back.
///
/// Never discards unproven bytes: a file is converted only when the server
/// holds it AND its contents are proven identical, either by a hash taken now
/// or by a verified index entry whose size and mtime have not changed since.
/// An unsynced or modified file keeps its bytes, because converting it would
/// destroy the only current copy.
///
/// Hashing happens here, per file, right before freeing it, several files at a
/// time, instead of up front in the sync pass.
pub async fn free_up_space(engine: Option<Arc<SyncEngine>>, scope: Option<&str>) -> Result<(u32, u64), String> {
    use futures_util::stream::{self, StreamExt};

    if !crate::cloudfiles::is_supported() {
        return Err("Files On-Demand is not available on this system".into());
    }
    let root = sync_root();

    // Only the given folder when there is one, and never a folder the producer
    // keeps "always on this PC".
    let modes = match &engine {
        Some(e) => e.modes.lock().await.clone(),
        None => pins::load(),
    };
    let in_scope = |rel: &str| -> bool {
        let inside = match scope {
            Some(f) => rel.len() > f.len() && rel.starts_with(f) && rel.as_bytes()[f.len()] == b'/',
            None => true,
        };
        inside && pins::mode_for(rel, &modes) != pins::Mode::Always
    };

    // Work from the running engine's index when there is one. Reading and
    // writing the file on disk behind its back let the engine's next flush
    // overwrite everything recorded here.
    let mut index = match &engine {
        Some(e) => e.index.lock().await.clone(),
        None => read_index(),
    };

    let mut updates: Vec<SyncEntry> = Vec::new();
    let mut candidates: Vec<FreeCandidate> = Vec::new();

    // The index is a cache of what this machine has reconciled, not the truth
    // about the server. On a large workspace it can be far behind (one machine
    // had 1,459 entries against 136,313 server files), and every file missing
    // from it used to be skipped. Ask the server first.
    if let Some(engine) = &engine {
        match reconcile_from_server(engine, &root, &index).await {
            Ok((trusted, to_hash)) => {
                log::info!(
                    "[FreeSpace] matched {} files by metadata, {} more need a hash",
                    trusted.len(),
                    to_hash.len()
                );
                for e in trusted {
                    index.insert(e.rel_path.clone(), e.clone());
                    updates.push(e);
                }
                candidates.extend(to_hash);
            }
            // Only ever adds candidates; the index alone is still usable.
            Err(e) => log::warn!("[FreeSpace] could not reach the server ({e}); using the local index only"),
        }
    }

    candidates.retain(|c| in_scope(&c.rel_path));
    let mut skipped = 0u32;
    for (rel_path, entry) in index.iter() {
        if !in_scope(rel_path) {
            continue;
        }
        let path = root.join(rel_path);
        // Already dehydrated: nothing to reclaim.
        if !path.is_file() || crate::cloudfiles::is_placeholder(&path) {
            continue;
        }
        // Without both halves of the identity we cannot ask the server for the
        // bytes again, so discarding them locally would lose the file.
        let (Some(ws_id), Some(file_id)) = (&entry.workspace_id, &entry.remote_id) else {
            skipped += 1;
            continue;
        };
        let size = match path.metadata() {
            Ok(m) => m.len(),
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if size != entry.size {
            skipped += 1;
            continue;
        }
        let unchanged = matches!((entry.mtime, file_mtime(&path)), (Some(a), Some(b)) if a == b);
        candidates.push(FreeCandidate {
            rel_path: rel_path.clone(),
            path,
            identity: format!("{ws_id}/{file_id}"),
            size,
            expected_sha: entry.sha256.clone(),
            needs_hash: !entry.verified || !unchanged,
            new_entry: None,
        });
    }

    // Hash (when needed) and convert, a few files at a time, off the async
    // threads. A hash that does not match leaves the file fully on disk.
    let results: Vec<(FreeCandidate, Result<bool, String>)> = stream::iter(candidates)
        .map(|c| {
            let root = root.clone();
            async move {
            let path = c.path.clone();
            let expected = c.expected_sha.clone();
            let identity = c.identity.clone();
            let rel_path = c.rel_path.clone();
            let size = c.size;
            let outcome = tokio::task::spawn_blocking(move || -> Result<bool, String> {
                // Always hash, never trust the index alone. Freeing an ordinary file deletes the
                // only local copy before the placeholder takes its place, so the server copy has
                // to be proven identical first, not merely recorded as uploaded.
                match hash_file(&path) {
                    Ok(h) if h == expected => {}
                    _ => return Ok(false),
                }
                crate::cloudfiles::replace_with_placeholder(&root, &rel_path, size, &identity)
                    .map(|()| true)
            })
            .await
            .unwrap_or_else(|e| Err(format!("free-space task failed: {e}")));
            (c, outcome)
            }
        })
        .buffer_unordered(FREE_SPACE_PARALLEL)
        .collect()
        .await;

    let mut freed_files = 0u32;
    let mut freed_bytes = 0u64;
    for (c, outcome) in results {
        match outcome {
            Ok(true) => {
                freed_files += 1;
                freed_bytes += c.size;
                // A freed file is proven: record it, or mark its entry verified.
                let proven = match c.new_entry {
                    Some(e) => Some(e),
                    None => index.get(&c.rel_path).cloned().map(|mut e| {
                        e.verified = true;
                        e
                    }),
                };
                updates.extend(proven);
            }
            Ok(false) => skipped += 1,
            Err(e) => {
                log::info!("[FreeSpace] {}: {e}", c.rel_path);
                skipped += 1;
            }
        }
    }

    if !updates.is_empty() {
        match &engine {
            Some(e) => {
                let mut live = e.index.lock().await;
                for u in updates {
                    live.insert(u.rel_path.clone(), u);
                }
                write_index(&live);
            }
            None => {
                for u in updates {
                    index.insert(u.rel_path.clone(), u);
                }
                write_index(&index);
            }
        }
    }

    log::info!("[FreeSpace] dehydrated {freed_files} files, {freed_bytes} bytes, skipped {skipped}");
    Ok((freed_files, freed_bytes))
}

/// Result of moving a local folder into the cloud.
#[derive(Clone, serde::Serialize)]
pub struct ArchiveReport {
    pub moved: u32,
    pub bytes_freed: u64,
    pub skipped: u32,
    pub workspace: String,
    pub errors: Vec<String>,
    /// Whether the source folder itself is gone, not just its contents.
    pub folder_removed: bool,
    pub seconds: u64,
}

/// Upload every file under `src` to a workspace and delete the local copy,
/// freeing the space it occupied.
///
/// Deleting someone's only copy is the worst thing this app could do, so a file
/// is removed only after `register_upload` returns Ok — and the server calls
/// `objectExists()` before it will mark a file ready, so that is a real
/// guarantee the bytes are in object storage, not just that we sent them.
///
/// Files are deleted one at a time rather than in a batch at the end: someone
/// doing this has run out of disk, and space needs to come back as it goes.
pub async fn archive_folder(
    engine: Arc<SyncEngine>,
    src: PathBuf,
    workspace_id: Option<String>,
    dest_folder: Option<String>,
) -> Result<ArchiveReport, String> {
    let token = engine
        .token
        .read()
        .await
        .clone()
        .ok_or_else(|| "Not signed in".to_string())?;

    // Moving the sync folder into itself would upload placeholders and delete
    // the originals.
    let root = sync_root();
    if src == root || src.starts_with(&root) {
        return Err("That folder is already synced. Use Free Up Space instead.".into());
    }
    // A home directory or a drive root is never a deliberate choice here.
    if src.parent().is_none() || dirs::home_dir().map(|h| h == src).unwrap_or(false) {
        return Err("Refusing to move an entire drive or home folder.".into());
    }

    let mut workspaces = api::list_workspaces(&token).await?;
    if workspaces.is_empty() {
        return Err("No workspace to move files into".into());
    }
    let ws = match &workspace_id {
        Some(id) => workspaces
            .iter()
            .find(|w| &w.id == id)
            .cloned()
            .ok_or_else(|| format!("Workspace {id} not found"))?,
        None => workspaces.remove(0),
    };

    // Where the files land inside that workspace. Defaults to a folder named
    // after the one being moved.
    let base = match dest_folder.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => d.trim_matches('/').to_string(),
        None => src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Archived".into()),
    };

    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(&src, &mut files);

    let total = files.len();
    let mut report = ArchiveReport {
        moved: 0,
        bytes_freed: 0,
        skipped: 0,
        workspace: ws.name.clone(),
        errors: Vec::new(),
        folder_removed: false,
        seconds: 0,
    };

    // Upload several at once. One-at-a-time left the connection idle between
    // files, which on a folder of small samples is most of the wall time.
    // Four matches the existing sync path and keeps memory predictable.
    const PARALLEL: usize = 4;

    let total_bytes: u64 = files.iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
    let started = std::time::Instant::now();
    let done_files = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let done_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let results = Arc::new(Mutex::new(Vec::<(bool, u64, Option<String>)>::new()));

    let tasks = futures_util::stream::iter(files.iter().cloned().map(|path| {
        let token = token.clone();
        let ws_id = ws.id.clone();
        let base = base.clone();
        let src = src.clone();
        let app = engine.app.clone();
        let done_files = done_files.clone();
        let done_bytes = done_bytes.clone();
        let results = results.clone();
        async move {
            let name = match path.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => return,
            };
            let size = path.metadata().map(|m| m.len()).unwrap_or(0);

            let rel_dir = path
                .parent()
                .and_then(|p| p.strip_prefix(&src).ok())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let folder_path = if rel_dir.is_empty() { base.clone() } else { format!("{base}/{rel_dir}") };

            let outcome = async {
                let sha = hash_file(&path).map_err(|e| format!("unreadable ({e})"))?;
                // Only Ok once the server has marked the file ready.
                api::upload_file(&token, &ws_id, &name, size, Some(&folder_path), &sha, &path).await?;
                Ok::<(), String>(())
            }
            .await;

            let entry = match outcome {
                Ok(()) => match std::fs::remove_file(&path) {
                    Ok(()) => (true, size, None),
                    Err(e) => (false, 0, Some(format!("{name}: uploaded but not deleted ({e})"))),
                },
                Err(e) => (false, 0, Some(format!("{name}: {e}"))),
            };
            results.lock().await.push(entry);

            let n = done_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let b = done_bytes.fetch_add(size, std::sync::atomic::Ordering::Relaxed) + size;

            // Estimate from throughput so far. Meaningless until a little has
            // moved, so it is only sent once past the first file.
            let elapsed = started.elapsed().as_secs_f64();
            let eta = if b > 0 && elapsed > 1.0 && total_bytes > b {
                let rate = b as f64 / elapsed;
                Some(((total_bytes - b) as f64 / rate).round() as u64)
            } else {
                None
            };

            let _ = app.emit(
                "archive-progress",
                serde_json::json!({
                    "current": n,
                    "total": total,
                    "name": name,
                    "bytes_done": b,
                    "bytes_total": total_bytes,
                    "eta_seconds": eta,
                }),
            );
        }
    }));

    use futures_util::StreamExt;
    tasks.buffer_unordered(PARALLEL).collect::<Vec<()>>().await;

    for (moved, bytes, err) in results.lock().await.iter() {
        if *moved {
            report.moved += 1;
            report.bytes_freed += bytes;
        } else {
            if err.as_deref().map(|e| e.contains("uploaded but not deleted")).unwrap_or(false) {
                // Safely stored, just not removed locally. Not a skip.
            } else {
                report.skipped += 1;
            }
            if let Some(e) = err { report.errors.push(e.clone()); }
        }
    }
    report.seconds = started.elapsed().as_secs();

    // Tidy up directories we emptied. remove_dir only succeeds on an empty one,
    // so anything still holding a skipped file is left alone.
    prune_empty_dirs(&src);
    report.folder_removed = !src.exists();

    log::info!(
        "[Archive] {} files moved to '{}', {} bytes freed, {} skipped",
        report.moved, report.workspace, report.bytes_freed, report.skipped
    );
    Ok(report)
}

/// True for files the user is not shown in Explorer and did not choose to move:
/// Windows marks album art and desktop.ini as Hidden or System attributes rather
/// than by name, so a leading-dot check (a Unix convention) missed them. A folder
/// showing three files was reporting five failures because of exactly this.
fn is_hidden(path: &Path) -> bool {
    if path
        .file_name()
        .map(|n| n.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
    {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
        if let Ok(md) = path.metadata() {
            let a = md.file_attributes();
            if a & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 {
                return true;
            }
        }
    }
    false
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if !is_hidden(&p) {
                collect_files(&p, out);
            }
        } else if p.is_file() && !is_hidden(&p) {
            out.push(p);
        }
    }
}

/// Files Windows and macOS generate to describe a folder. They are regenerated
/// on demand and describe contents that are now gone, so once everything real
/// has moved they are the only thing keeping an empty folder alive.
fn is_folder_metadata(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "thumbs.db"
        || n == "desktop.ini"
        || n == ".ds_store"
        || n == "folder.jpg"
        || n == "albumart.jpg"
        || (n.starts_with("albumart") && n.ends_with(".jpg"))
}

/// Remove directories we emptied, depth-first, including `dir` itself.
///
/// The first version left the folder behind: the hidden album art skipped
/// during the move was still inside, so `remove_dir` refused. Windows-generated
/// metadata is cleared when it is all that remains; a directory still holding
/// anything real is left exactly as it is.
fn prune_empty_dirs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut metadata_files = Vec::new();
    let mut has_real_content = false;

    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            prune_empty_dirs(&p);
            // Still there after pruning means it holds something.
            if p.exists() {
                has_real_content = true;
            }
        } else {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if is_folder_metadata(&name) {
                metadata_files.push(p);
            } else {
                has_real_content = true;
            }
        }
    }

    if has_real_content {
        return; // a real file is still here, so keep the folder and its metadata
    }
    for f in metadata_files {
        let _ = std::fs::remove_file(f);
    }
    let _ = std::fs::remove_dir(dir);
}

#[cfg(test)]
mod redact_token_tests {
    use super::redact_token;

    #[test]
    fn paths_from_the_server_never_leave_the_sync_folder() {
        assert!(super::is_safe_rel_path("Studio/FL Studio/Face Of Fire.flp"));
        assert!(super::is_safe_rel_path("Studio/a..b.wav"));
        for bad in [
            "Studio/../../AppData/x.bat",
            "Studio/..\\..\\Startup\\x.bat",
            "C:/Windows/x.dll",
            "Studio/C:x.bat",
            "/etc/passwd",
            "Studio//x.wav",
            "Studio/./x.wav",
            "",
        ] {
            assert!(!super::is_safe_rel_path(bad), "{bad}");
        }
    }

    #[test]
    fn takes_the_token_out_of_a_url() {
        let line = "error sending request for url (https://workspace.hardwavestudios.com/api/workspaces/2/events?token=eyJhbGciOiJIUzI1NiJ9.abc-DEF_123.sig)";
        let out = redact_token(line);
        assert!(out.contains("token=REDACTED"));
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(out.ends_with(')'), "the rest of the message survives: {out}");
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        assert_eq!(redact_token("nothing to hide here"), "nothing to hide here");
    }
}

#[cfg(test)]
mod overview_tests {
    use super::*;

    #[test]
    fn the_folder_list_counts_each_file_once_and_only_this_accounts_workspaces() {
        let root = std::env::temp_dir().join(format!("hw-overview-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Mine/Kicks")).unwrap();
        std::fs::create_dir_all(root.join("Someone else/Leads")).unwrap();
        std::fs::write(root.join("Mine/loose.txt"), b"12345").unwrap();
        std::fs::write(root.join("Mine/Kicks/kick.wav"), b"1234567890").unwrap();
        std::fs::write(root.join("Someone else/Leads/lead.wav"), b"123").unwrap();

        let own = vec!["Mine".to_string()];
        let o = build_overview(&root, &HashMap::new(), Some(&own));
        let names: Vec<&str> = o.rows.iter().map(|r| r.folder.as_str()).collect();
        assert_eq!(names, ["Mine", "Mine/Kicks"]);
        assert_eq!((o.rows[0].files, o.rows[0].bytes_here), (2, 15));
        assert_eq!((o.rows[1].files, o.rows[1].bytes_here), (1, 10));
        assert_eq!(o.bytes_here, 15);

        // Before the first pass has listed the workspaces, every folder shows.
        let all = build_overview(&root, &HashMap::new(), None);
        assert_eq!(all.rows.len(), 4);
        let _ = std::fs::remove_dir_all(&root);
    }
}
