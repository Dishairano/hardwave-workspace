use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    pub success: bool,
    pub token: Option<String>,
    pub user: Option<User>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub email: String,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
}

/// A tracked file in the local sync folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEntry {
    /// Relative path from the sync root (e.g. "My Workspace/kicks/kick01.wav").
    pub rel_path: String,
    /// SHA-256 hash of the local file contents.
    pub sha256: String,
    /// Last modified timestamp (UTC).
    pub modified: String,
    /// File size in bytes.
    pub size: u64,
    /// Remote file ID in the Workspace backend (None if not yet uploaded).
    pub remote_id: Option<String>,
    /// Workspace ID this file belongs to.
    pub workspace_id: Option<String>,
}

/// State pushed to the frontend via events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    pub state: String, // "idle", "syncing", "error", "paused"
    pub files_pending: u32,
    pub files_synced: u32,
    pub last_sync: Option<String>,
    pub error: Option<String>,
}

/// Progress event for individual file sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncFileProgress {
    pub rel_path: String,
    pub direction: String, // "upload" or "download"
    pub percent: u8,
    pub bytes_done: u64,
    pub bytes_total: u64,
}
