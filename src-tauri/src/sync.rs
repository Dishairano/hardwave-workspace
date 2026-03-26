//! Bidirectional file sync engine.
//!
//! Watches `~/Hardwave/` for local changes and periodically polls the
//! Workspace API for remote changes. Uses SHA-256 hashes to detect diffs.

use crate::api;
use crate::models::{SyncEntry, SyncFileProgress, SyncStatus};
use notify::{RecommendedWatcher, RecursiveMode, Watcher, Event, EventKind};
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tauri::Emitter;

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

/// Compute SHA-256 of a file.
fn hash_file(path: &Path) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| format!("Read error: {}", e))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
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
        }
    }

    pub async fn set_token(&self, token: Option<String>) {
        *self.token.write().await = token;
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

    /// Start the sync loop. Call this once on app startup.
    pub async fn start(self: Arc<Self>) {
        let root = sync_root();
        let _ = std::fs::create_dir_all(&root);

        // Start file watcher for instant local change detection
        let engine = Arc::clone(&self);
        let (fs_tx, mut fs_rx) = mpsc::channel::<String>(256);

        let watch_root = root.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Handle::current();
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
                Err(_) => return,
            };
            let _ = watcher.watch(&root, RecursiveMode::Recursive);
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

        // Periodic full sync loop
        let engine_poll = Arc::clone(&self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
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
            None => return Ok(()), // Not logged in
        };

        let root = sync_root();
        let full_path = root.join(rel_path);

        let mut index = self.index.lock().await;

        if !full_path.exists() {
            // File deleted locally — remove from index
            // TODO: delete from remote
            index.remove(rel_path);
            write_index(&index);
            return Ok(());
        }

        let meta = std::fs::metadata(&full_path).map_err(|e| e.to_string())?;
        let sha = hash_file(&full_path)?;

        // Check if file changed since last sync
        if let Some(entry) = index.get(rel_path) {
            if entry.sha256 == sha {
                return Ok(()); // No change
            }
        }

        // Determine workspace from folder structure:
        // ~/Hardwave/<workspace_name>/path/to/file
        let parts: Vec<&str> = rel_path.splitn(2, '/').collect();
        if parts.len() < 2 {
            return Ok(()); // File at root level — skip
        }
        let workspace_name = parts[0];
        let file_path_in_ws = parts[1];

        // Find workspace ID by name
        let workspaces = api::list_workspaces(&token).await?;
        let ws = workspaces.iter().find(|w| w.name == workspace_name);
        let ws_id = match ws {
            Some(w) => w.id.clone(),
            None => return Ok(()), // Workspace not found — skip
        };

        let filename = full_path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let folder = Path::new(file_path_in_ws).parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty());

        self.update_status("syncing", None).await;

        let _ = self.app.emit("sync:file", SyncFileProgress {
            rel_path: rel_path.to_string(),
            direction: "upload".into(),
            percent: 0,
            bytes_done: 0,
            bytes_total: meta.len(),
        });

        // Initiate upload
        let upload = api::init_upload(&token, &ws_id, filename, meta.len(), folder, &sha).await?;

        // Read file and upload to S3
        let data = std::fs::read(&full_path).map_err(|e| e.to_string())?;
        api::upload_to_s3(&upload.upload_url, data).await?;

        // Confirm upload
        api::register_upload(&token, &ws_id, &upload.file_id).await?;

        let _ = self.app.emit("sync:file", SyncFileProgress {
            rel_path: rel_path.to_string(),
            direction: "upload".into(),
            percent: 100,
            bytes_done: meta.len(),
            bytes_total: meta.len(),
        });

        // Update index
        index.insert(rel_path.to_string(), SyncEntry {
            rel_path: rel_path.to_string(),
            sha256: sha,
            modified: chrono::Utc::now().to_rfc3339(),
            size: meta.len(),
            remote_id: Some(upload.file_id),
            workspace_id: Some(ws_id),
        });
        write_index(&index);

        self.update_status("idle", None).await;
        Ok(())
    }

    /// Full bidirectional sync — compare local index with remote state.
    async fn full_sync(&self) -> Result<(), String> {
        let token = self.token.read().await.clone();
        let token = match token {
            Some(t) => t,
            None => return Ok(()),
        };

        self.update_status("syncing", None).await;

        let root = sync_root();
        let workspaces = api::list_workspaces(&token).await?;
        eprintln!("[Sync] Full sync: {} workspace(s)", workspaces.len());

        for ws in &workspaces {
            let ws_dir = root.join(&ws.name);
            let _ = std::fs::create_dir_all(&ws_dir);

            let remote_files = match api::list_files(&token, &ws.id).await {
                Ok(f) => {
                    eprintln!("[Sync] Workspace '{}': {} remote file(s)", ws.name, f.len());
                    f
                }
                Err(e) => {
                    eprintln!("[Sync] Failed to list files for '{}': {}", ws.name, e);
                    continue;
                }
            };
            let mut index = self.index.lock().await;

            // Check for remote files not in local index → download
            for rf in &remote_files {
                let folder = rf.folder_path.as_deref().unwrap_or("/").trim_matches('/');
                let rel_path = if folder.is_empty() {
                    format!("{}/{}", ws.name, rf.name)
                } else {
                    format!("{}/{}/{}", ws.name, folder, rf.name)
                };

                let needs_download = match index.get(&rel_path) {
                    Some(entry) => {
                        // If remote has a different hash, download it
                        rf.sha256.as_deref() != Some(&entry.sha256)
                    }
                    None => true, // Not in index — new remote file
                };

                if needs_download {
                    eprintln!("[Sync] Downloading: {}", rel_path);
                    let local_path = root.join(&rel_path);
                    if let Some(parent) = local_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }

                    // Download
                    match api::get_download_url(&token, &ws.id, &rf.id).await {
                        Ok(url) => {
                            let _ = self.app.emit("sync:file", SyncFileProgress {
                                rel_path: rel_path.clone(),
                                direction: "download".into(),
                                percent: 0,
                                bytes_done: 0,
                                bytes_total: rf.size,
                            });

                            let client = reqwest::Client::new();
                            if let Ok(res) = client.get(&url).send().await {
                                if let Ok(bytes) = res.bytes().await {
                                    let _ = std::fs::write(&local_path, &bytes);
                                    let sha = {
                                        let mut h = Sha256::new();
                                        h.update(&bytes);
                                        hex::encode(h.finalize())
                                    };

                                    index.insert(rel_path.clone(), SyncEntry {
                                        rel_path: rel_path.clone(),
                                        sha256: sha,
                                        modified: chrono::Utc::now().to_rfc3339(),
                                        size: rf.size,
                                        remote_id: Some(rf.id.clone()),
                                        workspace_id: Some(ws.id.clone()),
                                    });

                                    let _ = self.app.emit("sync:file", SyncFileProgress {
                                        rel_path: rel_path.clone(),
                                        direction: "download".into(),
                                        percent: 100,
                                        bytes_done: rf.size,
                                        bytes_total: rf.size,
                                    });
                                }
                            }
                        }
                        Err(_) => continue,
                    }
                }
            }

            // Check for local files not on remote → upload
            let local_files = scan_local(&ws_dir);
            for (local_rel, local_path, size) in &local_files {
                let full_rel = format!("{}/{}", ws.name, local_rel);
                if index.contains_key(&full_rel) {
                    continue; // Already synced
                }
                // Upload new local file
                let sha = match hash_file(local_path) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                let filename = local_path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                let folder = Path::new(local_rel).parent()
                    .and_then(|p| p.to_str())
                    .filter(|s| !s.is_empty());

                match api::init_upload(&token, &ws.id, filename, *size, folder, &sha).await {
                    Ok(upload) => {
                        if let Ok(data) = std::fs::read(local_path) {
                            if api::upload_to_s3(&upload.upload_url, data).await.is_ok() {
                                let _ = api::register_upload(&token, &ws.id, &upload.file_id).await;
                                index.insert(full_rel.clone(), SyncEntry {
                                    rel_path: full_rel,
                                    sha256: sha,
                                    modified: chrono::Utc::now().to_rfc3339(),
                                    size: *size,
                                    remote_id: Some(upload.file_id),
                                    workspace_id: Some(ws.id.clone()),
                                });
                            }
                        }
                    }
                    Err(_) => continue,
                }
            }

            write_index(&index);
        }

        let mut status = self.status.write().await;
        status.state = "idle".into();
        status.last_sync = Some(chrono::Utc::now().to_rfc3339());
        status.error = None;
        let _ = self.app.emit("sync:status", status.clone());

        Ok(())
    }
}
