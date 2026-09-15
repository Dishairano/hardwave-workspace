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
    /// Local file mtime (unix seconds) recorded when this entry was written.
    /// Lets a later pass skip re-hashing an unchanged file (size + mtime match)
    /// instead of SHA-256'ing every file every cycle. Defaulted so older index
    /// files (which lack it) still load and simply fall back to hashing once.
    #[serde(default)]
    pub mtime: Option<i64>,
    /// Whether `sha256` has been checked against the bytes on this disk.
    ///
    /// False only for a match recorded from metadata against a server file,
    /// without reading the local file (`sync::trust_without_hash`). Such an
    /// entry still skips re-upload, but Free Up Space hashes the file before it
    /// discards any bytes. Defaults to true: every entry written before this
    /// flag existed came from a real hash or from bytes downloaded from the
    /// server.
    #[serde(default = "verified_by_default")]
    pub verified: bool,
}

fn verified_by_default() -> bool {
    true
}

/// State pushed to the frontend via events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    pub state: String, // "idle", "syncing", "error", "paused"
    pub files_pending: u32,
    pub files_synced: u32,
    pub last_sync: Option<String>,
    pub error: Option<String>,
    /// Honest totals so the UI never shows a fake 100%. `files_total` /
    /// `bytes_total` are what is actually on disk under the sync root;
    /// `files_synced` / `bytes_synced` are what has a remote id (uploaded).
    /// The gap between them is real, unbacked-up data — the number the user
    /// most needs to see. Defaulted so older persisted/serialized shapes still
    /// deserialize.
    #[serde(default)]
    pub files_total: u32,
    #[serde(default)]
    pub bytes_synced: u64,
    #[serde(default)]
    pub bytes_total: u64,
    /// The file currently moving, if any, and its percent — for a live line.
    #[serde(default)]
    pub current_file: Option<String>,
    #[serde(default)]
    pub current_percent: u32,
}

#[cfg(test)]
mod sync_entry_tests {
    use super::*;

    #[test]
    fn index_entries_from_before_the_flag_load_as_verified() {
        let json = r#"{"rel_path":"ws/kick.wav","sha256":"ab","modified":"t","size":1,"remote_id":"1","workspace_id":"2"}"#;
        let entry: SyncEntry = serde_json::from_str(json).unwrap();
        assert!(entry.verified);
    }
}
