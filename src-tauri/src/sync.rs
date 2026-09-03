//! Bidirectional file sync engine.
//!
//! Watches `~/Hardwave/` for local changes and periodically polls the
//! Workspace API for remote changes. Uses SHA-256 hashes to detect diffs.

use crate::api;
use crate::models::{SyncEntry, SyncStatus};
use notify::{RecursiveMode, Watcher, Event, EventKind};
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Validate that a path component is safe (no path traversal).
fn is_safe_component(name: &str) -> bool {
    !name.contains("..") && !name.contains('/') && !name.contains('\\') && !name.contains('\0')
}

/// Validate that a relative path is safe (allows `/` separators, rejects traversal).
fn is_safe_rel_path(path: &str) -> bool {
    !path.contains("..") && !path.contains('\\') && !path.contains('\0')
}

/// Scan the sync root and return all files with their hashes.
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
        if path.is_dir() {
            scan_dir_recursive(root, &path, out);
        } else if let Ok(meta) = path.metadata() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
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
            })),
            index: Arc::new(Mutex::new(index)),
            app,
            paused: Arc::new(RwLock::new(false)),
            sync_active: AtomicBool::new(false),
            sse_handles: Mutex::new(Vec::new()),
            sse_trigger: Arc::new(tokio::sync::Notify::new()),
        }
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
                            eprintln!("[Sync] Skipping workspace with unsafe name: {}", ws.name);
                            continue;
                        }
                        let ws_dir = root.join(&ws.name);
                        let _ = std::fs::create_dir_all(&ws_dir);
                        eprintln!("[Sync] Created workspace folder: {}", ws_dir.display());
                        if let Ok(folders) = api::list_folders(&t, &ws.id).await {
                            for f in &folders {
                                let folder_path = f.path.as_deref()
                                    .unwrap_or(&f.name)
                                    .trim_matches('/');
                                if !folder_path.is_empty() && is_safe_rel_path(folder_path) {
                                    let dir = ws_dir.join(folder_path);
                                    let _ = std::fs::create_dir_all(&dir);
                                    eprintln!("[Sync] Created subfolder: {}", dir.display());
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
        self.status.read().await.clone()
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

    /// Emit a conflict warning to the webview.
    fn emit_conflict(&self, rel_path: &str, local_size: u64, remote_size: u64) {
        if let Some(win) = self.app.get_webview_window("main") {
            crate::safe_eval(&win, "__HW_SYNC_CONFLICTS__", &serde_json::json!({
                "rel_path": rel_path,
                "local_size": local_size,
                "remote_size": remote_size,
                "time": chrono::Utc::now().timestamp_millis(),
            }));
        }
        eprintln!("[Sync] CONFLICT: '{}' exists locally ({} bytes) but differs from remote ({} bytes)", rel_path, local_size, remote_size);
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
                    let fetcher: crate::hydration::Fetcher = std::sync::Arc::new(move |identity: String, offset: u64, length: u64| {
                        let token = Arc::clone(&token);
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

                            let status = ranged.status();
                            if status.as_u16() == 206 {
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
                            let start = (offset as usize).min(all.len());
                            let stop = (start + length as usize).min(all.len());
                            Ok(all[start..stop].to_vec())
                        })
                    });

                    match crate::hydration::connect(&root, fetcher) {
                        Ok(conn) => {
                            eprintln!("[Sync] Files On-Demand active at {}", root.display());
                            // The OS connection lives until CfDisconnectSyncRoot
                            // is called, which we never do while running. Park the
                            // handle rather than dropping it so a future clean
                            // shutdown has something to close.
                            static HYDRATION: std::sync::OnceLock<crate::hydration::Connection> =
                                std::sync::OnceLock::new();
                            let _ = HYDRATION.set(conn);
                        }
                        Err(e) => eprintln!("[Sync] hydration connect failed: {e} — falling back to full downloads"),
                    }
                }
                Err(e) => eprintln!("[Sync] sync-root registration failed: {e} — falling back to full downloads"),
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
                    eprintln!("[Sync] Failed to create file watcher: {}", e);
                    return;
                }
            };
            if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
                eprintln!("[Sync] Failed to start watching '{}': {}", root.display(), e);
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
                if *engine_fs.paused.read().await {
                    continue;
                }
                // Debounce: wait a bit for writes to finish
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                let _ = engine_fs.sync_local_file(&rel_path).await;
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
                        eprintln!("[Sync] SSE trigger: running immediate sync");
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
    async fn sync_local_file(&self, rel_path: &str) -> Result<(), String> {
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

        let meta = std::fs::metadata(&full_path).map_err(|e| e.to_string())?;
        let sha = hash_file(&full_path)?;

        // Check hash against index (brief lock)
        {
            let index = self.index.lock().await;
            if let Some(entry) = index.get(rel_path) {
                if entry.sha256 == sha {
                    return Ok(());
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
                    let u = api::init_upload(&token, &ws_id, &filename, file_size, folder.as_deref(), &sha).await?;
                    api::upload_to_s3(&u.upload_url, &full_path).await?;
                    api::register_upload(&token, &ws_id, &u.file_id).await?;
                    Ok::<_, String>((u.file_id, ws_id))
                })
            }
        }, 2).await?;

        self.emit_file_progress(rel_path, "upload", 100);

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
            eprintln!("[Sync] Full sync already in progress, skipping");
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
        eprintln!("[Sync] Full sync: {} workspace(s)", workspaces.len());

        for ws in &workspaces {
            if !is_safe_component(&ws.name) {
                eprintln!("[Sync] Skipping workspace with unsafe name: {}", ws.name);
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
                    eprintln!("[Sync] Workspace '{}': {} remote file(s)", ws.name, f.len());
                    f
                }
                Err(e) => {
                    eprintln!("[Sync] Failed to list files for '{}': {}", ws.name, e);
                    continue;
                }
            };

            // ── Download phase ─────────────────────────────────────
            // Determine what needs downloading (index lock held briefly)
            let to_download: Vec<(String, api::WorkspaceFile)> = {
                let idx = self.index.lock().await;
                remote_files.iter().filter_map(|rf| {
                    let folder = rf.folder_path.as_deref().unwrap_or("/").trim_matches('/');
                    let rel_path = if folder.is_empty() {
                        format!("{}/{}", ws.name, rf.name)
                    } else {
                        format!("{}/{}/{}", ws.name, folder, rf.name)
                    };
                    let should = match idx.get(&rel_path) {
                        Some(e) => rf.sha256.as_deref() != Some(&e.sha256),
                        None => true,
                    };
                    should.then(|| (rel_path, rf.clone()))
                }).collect()
            };

            // Process downloads without holding the index lock
            let mut downloaded: Vec<SyncEntry> = Vec::new();
            for (rel_path, rf) in &to_download {
                let local_path = root.join(rel_path);

                // Conflict: file exists locally but not indexed
                if local_path.exists() {
                    let local_hash = hash_file(&local_path).unwrap_or_default();
                    if rf.sha256.as_deref() == Some(&local_hash) {
                        eprintln!("[Sync] Already exists (same hash): {}", rel_path);
                        downloaded.push(SyncEntry {
                            rel_path: rel_path.clone(),
                            sha256: local_hash,
                            modified: chrono::Utc::now().to_rfc3339(),
                            size: local_path.metadata().map(|m| m.len()).unwrap_or(0),
                            remote_id: Some(rf.id.clone()),
                            workspace_id: Some(ws.id.clone()),
                        });
                        continue;
                    }
                    self.emit_conflict(rel_path, local_path.metadata().map(|m| m.len()).unwrap_or(0), rf.size);
                    continue;
                }

                if let Some(parent) = local_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }

                // Files On-Demand: create a placeholder rather than pulling the
                // bytes down. It shows in Explorer at full size for ~1 KB of
                // disk, and Windows calls our hydration handler if anything
                // actually opens it. Falls through to a real download when the
                // platform does not support it (non-Windows, or pre-1709).
                if crate::cloudfiles::is_supported() {
                    let dir = rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                    let ph = crate::cloudfiles::RemoteFile {
                        rel_path: rel_path.clone(),
                        size: rf.size,
                        identity: format!("{}/{}", ws.id, rf.id),
                    };
                    match crate::cloudfiles::create_placeholders(&root, dir, &[ph]) {
                        Ok(n) if n > 0 => {
                            eprintln!("[Sync] Placeholder: {}", rel_path);
                            downloaded.push(SyncEntry {
                                rel_path: rel_path.clone(),
                                // The remote hash is authoritative until the
                                // file is hydrated and edited locally.
                                sha256: rf.sha256.clone().unwrap_or_default(),
                                modified: chrono::Utc::now().to_rfc3339(),
                                size: rf.size,
                                remote_id: Some(rf.id.clone()),
                                workspace_id: Some(ws.id.clone()),
                            });
                            self.emit_file_progress(rel_path, "placeholder", 100);
                            continue;
                        }
                        Ok(_) => eprintln!("[Sync] Placeholder skipped for '{}'", rel_path),
                        Err(e) => eprintln!("[Sync] Placeholder failed for '{}': {} — downloading instead", rel_path, e),
                    }
                }

                eprintln!("[Sync] Downloading: {}", rel_path);
                self.emit_file_progress(rel_path, "download", 0);
                // Download with retry
                let dl = api::with_retry({
                    let token = token.to_string();
                    let ws_id = ws.id.clone();
                    let rf_id = rf.id.clone();
                    let rel_path = rel_path.clone();
                    move || {
                        let token = token.clone();
                        let ws_id = ws_id.clone();
                        let rf_id = rf_id.clone();
                        let rel_path = rel_path.clone();
                        Box::pin(async move {
                            let url = api::get_download_url(&token, &ws_id, &rf_id).await?;
                            let bytes = api::http_client().get(&url).send().await
                                .map_err(|e| format!("Download failed: {}", e))?
                                .bytes().await
                                .map_err(|e| format!("Read body failed: {}", e))?;
                            let sha = {
                                let mut h = Sha256::new();
                                h.update(&bytes);
                                hex::encode(h.finalize())
                            };
                            let local_path = sync_root().join(&rel_path);
                            std::fs::write(&local_path, &bytes)
                                .map_err(|e| format!("Write failed: {}", e))?;
                            Ok((sha, bytes.len() as u64))
                        })
                    }
                }, 2).await;

                match dl {
                    Ok((sha, size)) => {
                        downloaded.push(SyncEntry {
                            rel_path: rel_path.clone(),
                            sha256: sha,
                            modified: chrono::Utc::now().to_rfc3339(),
                            size,
                            remote_id: Some(rf.id.clone()),
                            workspace_id: Some(ws.id.clone()),
                        });
                        self.emit_file_progress(rel_path, "download", 100);
                    }
                    Err(e) => {
                        eprintln!("[Sync] Download failed for '{}': {}", rel_path, e);
                    }
                }
            }

            // Update index with downloaded files (brief lock)
            if !downloaded.is_empty() {
                let mut idx = self.index.lock().await;
                for e in &downloaded {
                    idx.insert(e.rel_path.clone(), e.clone());
                }
                write_index(&idx);
            }

            // ── Upload phase ────────────────────────────────────────
            let local_files = scan_local(&ws_dir);

            // Determine what needs uploading (brief index lock)
            let to_upload: Vec<(String, PathBuf, u64, String)> = {
                let idx = self.index.lock().await;
                local_files.iter().filter_map(|(local_rel, local_path, size)| {
                    let full_rel = format!("{}/{}", ws.name, local_rel);
                    // Presence in the index used to be enough to skip a file, so
                    // editing something that had already synced never uploaded
                    // again: open a project from the workspace, save it, and the
                    // server kept the old version forever. Compare contents.
                    //
                    // Size is checked first because it is free; the hash only
                    // runs when the size matches, which is the common case.
                    if let Some(entry) = idx.get(&full_rel) {
                        if entry.size == *size {
                            match hash_file(local_path) {
                                Ok(h) if h == entry.sha256 => return None, // unchanged
                                Ok(h) => {
                                    eprintln!("[Sync] Modified, re-uploading: {}", full_rel);
                                    return Some((full_rel, local_path.clone(), *size, h));
                                }
                                // Unreadable (locked by the DAW, say). Leave it
                                // for the next pass rather than uploading junk.
                                Err(_) => return None,
                            }
                        }
                        eprintln!("[Sync] Size changed, re-uploading: {}", full_rel);
                    }
                    let sha = hash_file(local_path).ok()?;
                    Some((full_rel, local_path.clone(), *size, sha))
                }).collect()
            };

            // Upload in parallel batches of 4 (no index lock held)
            let mut uploaded: Vec<SyncEntry> = Vec::new();
            for batch in to_upload.chunks(4) {
                let mut tasks = Vec::new();
                for (full_rel, local_path, size, sha) in batch {
                    let token = token.to_string();
                    let ws_id = ws.id.clone();
                    let ws_name = ws.name.clone();
                    let full_rel = full_rel.clone();
                    let local_path = local_path.clone();
                    let size = *size;
                    let sha = sha.clone();

                    tasks.push(tokio::spawn(async move {
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
                                    let up = api::init_upload(&token, &ws_id, &filename, size, folder.as_deref(), &sha).await?;
                                    api::upload_to_s3(&up.upload_url, &local_path).await?;
                                    api::register_upload(&token, &ws_id, &up.file_id).await?;
                                    Ok::<_, String>((full_rel.clone(), sha.clone(), size, up.file_id, ws_id.clone()))
                                })
                            }
                        }, 2).await?;
                        Ok::<_, String>(upload)
                    }));
                }

                let results = futures_util::future::join_all(tasks).await;
                for result in results {
                    match result {
                        Ok(Ok((full_rel, sha, size, file_id, ws_id))) => {
                            uploaded.push(SyncEntry {
                                rel_path: full_rel,
                                sha256: sha,
                                modified: chrono::Utc::now().to_rfc3339(),
                                size,
                                remote_id: Some(file_id),
                                workspace_id: Some(ws_id),
                            });
                        }
                        Ok(Err(e)) => eprintln!("[Sync] Upload failed: {}", e),
                        Err(e) => eprintln!("[Sync] Upload task panicked: {}", e),
                    }
                }
            }

            // Update index with uploaded files (brief lock)
            if !uploaded.is_empty() {
                let mut idx = self.index.lock().await;
                for e in &uploaded {
                    idx.insert(e.rel_path.clone(), e.clone());
                }
                write_index(&idx);
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

            let url = format!(
                "{}/workspaces/{}/events?token={}",
                api::ws_base(), workspace_id, token
            );

            let mut backoff_ms = 2_000u64;
            loop {
                eprintln!("[SSE] Connecting to workspace {}", workspace_id);
                match sse_client.get(&url).send().await {
                    Err(e) => {
                        eprintln!("[SSE] Connect error: {e}");
                    }
                    Ok(resp) if !resp.status().is_success() => {
                        eprintln!("[SSE] Auth error {}: stopping SSE for workspace {}", resp.status(), workspace_id);
                        return; // 401/403 — don't retry, token is gone
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
                                            eprintln!("[SSE] Event: {}", kind);
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
                        eprintln!("[SSE] Stream ended for workspace {}", workspace_id);
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

/// Convert every already-uploaded local file into a dehydrated placeholder.
///
/// The OneDrive "Free up space" behaviour. Files stay visible and openable;
/// only their bytes go, and opening one pulls it back.
///
/// Deliberately conservative: a file is only touched when the sync index says
/// the server holds it AND the local hash still matches what was uploaded. An
/// unsynced or locally-modified file is skipped, because dehydrating one would
/// destroy the only copy.
pub fn free_up_space() -> Result<(u32, u64), String> {
    if !crate::cloudfiles::is_supported() {
        return Err("Files On-Demand is not available on this system".into());
    }
    let root = sync_root();
    let index = read_index();
    let mut freed_files = 0u32;
    let mut freed_bytes = 0u64;
    let mut skipped = 0u32;

    for (rel_path, entry) in index.iter() {
        let path = root.join(rel_path);
        if !path.is_file() {
            continue;
        }
        // Already dehydrated: nothing to reclaim.
        if crate::cloudfiles::is_placeholder(&path) {
            continue;
        }
        // Without both halves of the identity we cannot ask the server for the
        // bytes again, so discarding them locally would lose the file.
        let (Some(ws_id), Some(file_id)) = (&entry.workspace_id, &entry.remote_id) else {
            skipped += 1;
            continue;
        };
        // Modified since upload? Leave it: the server copy is stale, so the
        // local bytes are the only current ones.
        match hash_file(&path) {
            Ok(h) if h == entry.sha256 => {}
            _ => { skipped += 1; continue; }
        }

        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
        match crate::cloudfiles::dehydrate(&path, &format!("{}/{}", ws_id, file_id)) {
            Ok(()) => { freed_files += 1; freed_bytes += size; }
            Err(e) => {
                eprintln!("[FreeSpace] {rel_path}: {e}");
                skipped += 1;
            }
        }
    }
    eprintln!("[FreeSpace] dehydrated {freed_files} files, {freed_bytes} bytes, skipped {skipped}");
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
    };

    for (i, path) in files.iter().enumerate() {
        let name = match path.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => { report.skipped += 1; continue; }
        };
        let size = path.metadata().map(|m| m.len()).unwrap_or(0);

        // Mirror the local tree under a folder named after the source.
        let rel_dir = path
            .parent()
            .and_then(|p| p.strip_prefix(&src).ok())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let folder_path = if rel_dir.is_empty() {
            base.clone()
        } else {
            format!("{}/{}", base, rel_dir)
        };

        let _ = engine.app.emit(
            "archive-progress",
            serde_json::json!({ "current": i + 1, "total": total, "name": name }),
        );

        let sha = match hash_file(path) {
            Ok(h) => h,
            Err(e) => {
                report.skipped += 1;
                report.errors.push(format!("{name}: unreadable ({e})"));
                continue;
            }
        };

        let outcome = async {
            let up = api::init_upload(&token, &ws.id, &name, size, Some(&folder_path), &sha).await?;
            api::upload_to_s3(&up.upload_url, path).await?;
            // Only returns Ok once the server has confirmed the object exists.
            api::register_upload(&token, &ws.id, &up.file_id).await?;
            Ok::<(), String>(())
        }
        .await;

        match outcome {
            Ok(()) => match std::fs::remove_file(path) {
                Ok(()) => {
                    report.moved += 1;
                    report.bytes_freed += size;
                }
                Err(e) => {
                    // Safely in the cloud, just not removed here. Not a failure.
                    report.errors.push(format!("{name}: uploaded but not deleted ({e})"));
                }
            },
            Err(e) => {
                report.skipped += 1;
                report.errors.push(format!("{name}: {e}"));
            }
        }
    }

    // Tidy up directories we emptied. remove_dir only succeeds on an empty one,
    // so anything still holding a skipped file is left alone.
    prune_empty_dirs(&src);
    report.folder_removed = !src.exists();

    eprintln!(
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
