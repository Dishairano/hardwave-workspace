//! A file changed on this PC and in Workspace before the two synced.
//!
//! Before this, the engine had no answer: a remote change to a file that also
//! existed here was never downloaded (it only logged "CONFLICT" when the sizes
//! differed), and a local change was uploaded over whatever the other PC had
//! saved. The decision is now made per file from three facts: what was last
//! synced (the index), what is on disk, and what Workspace holds. When both
//! sides moved, the file waits for the producer to choose, and nothing is
//! uploaded or overwritten until then.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// What the index remembers about the last synced version.
#[derive(Clone, Copy, Debug)]
pub struct Indexed<'a> {
    pub sha: &'a str,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// What is at the file's path on this PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Local {
    Missing,
    /// A cloud-only placeholder: no local bytes, nothing to lose.
    Placeholder,
    File { size: u64, mtime: Option<i64> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteAction {
    /// Nothing to do from the remote side (a local change is the upload scan's job).
    Skip,
    /// Bring Workspace's version here: a placeholder or a download.
    Fetch,
    /// The placeholder describes an older version; replace it with the current one.
    RefreshPlaceholder,
    /// This PC already has exactly Workspace's version; only the index is missing it.
    Index,
    /// Cannot tell from size and time; hash the local file and ask `after_hash`.
    NeedsHash,
    /// Both sides changed. Wait for the producer.
    Conflict,
}

/// Decide what a remote file means for this PC, without reading the file.
/// `trusted` is `sync::trust_without_hash` for an unindexed local file.
pub fn plan_remote(
    entry: Option<Indexed>,
    local: Local,
    remote_sha: Option<&str>,
    remote_size: u64,
    trusted: bool,
) -> RemoteAction {
    // 5,816 server files have no checksum (older web uploads). Their version
    // cannot be compared, so an indexed one counts as unchanged: guessing
    // "changed" would download it again on every pass.
    let remote_changed = match (entry, remote_sha) {
        (Some(e), Some(r)) => r != e.sha,
        (Some(_), None) => false,
        (None, _) => true,
    };
    match local {
        Local::Missing => {
            if remote_changed { RemoteAction::Fetch } else { RemoteAction::Skip }
        }
        Local::Placeholder => {
            if remote_changed { RemoteAction::RefreshPlaceholder } else { RemoteAction::Skip }
        }
        Local::File { size, mtime } => match entry {
            None if trusted => RemoteAction::Index,
            None if size != remote_size => RemoteAction::Conflict,
            // Same size and nothing to compare against: take it as the same file.
            None if remote_sha.is_none_or(str::is_empty) => RemoteAction::Index,
            None => RemoteAction::NeedsHash,
            Some(_) if !remote_changed => RemoteAction::Skip,
            // Same size and time as when it was last synced: untouched here, so
            // Workspace's newer version simply replaces it.
            Some(e) if size == e.size && mtime.is_some() && mtime == e.mtime => RemoteAction::Fetch,
            Some(_) => RemoteAction::NeedsHash,
        },
    }
}

/// The decision once the local file's hash is known.
pub fn after_hash(entry: Option<Indexed>, local_sha: &str, remote_sha: Option<&str>) -> RemoteAction {
    if remote_sha == Some(local_sha) {
        return RemoteAction::Index;
    }
    match entry {
        Some(e) if e.sha == local_sha => RemoteAction::Fetch,
        _ => RemoteAction::Conflict,
    }
}

/// May the upload scan send this local change? Not when Workspace moved on
/// since the last sync: that is a conflict, decided by the download side.
pub fn may_upload(entry_sha: Option<&str>, remote_sha: Option<Option<&str>>) -> bool {
    match (entry_sha, remote_sha) {
        // Not in Workspace at this path: a new file.
        (_, None) => true,
        // In Workspace but never synced here: the download side decides first.
        (None, Some(_)) => false,
        // No checksum in Workspace to compare with: upload as before.
        (Some(_), Some(None)) => true,
        (Some(e), Some(r)) => r == Some(e),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Conflict {
    pub rel_path: String,
    pub workspace_id: String,
    pub remote_id: String,
    pub local_size: u64,
    pub local_mtime: Option<i64>,
    pub remote_size: u64,
    pub remote_sha: String,
    pub remote_updated_at: Option<String>,
    pub detected_at: String,
}

/// What the producer keeps. Sent by the window as "keep_local", "keep_remote" or "keep_both".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum Choice {
    /// Upload this PC's file as the newest version.
    #[serde(rename = "keep_local")]
    Local,
    /// Take Workspace's version; this PC's file goes to the Recycle Bin.
    #[serde(rename = "keep_remote")]
    Remote,
    /// Keep this PC's file under a new name and take Workspace's version.
    #[serde(rename = "keep_both")]
    Both,
}

fn store_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hardwave")
        .join("workspace-conflicts.json")
}

pub fn load() -> HashMap<String, Conflict> {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(all: &HashMap<String, Conflict>) {
    let path = store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(all) {
        let _ = std::fs::write(&path, json);
    }
}

/// "Face Of Fire.flp" kept from this PC becomes "Face Of Fire (DESKTOP-1).flp",
/// then "(DESKTOP-1 2)" and so on while the name is taken.
pub fn kept_name(file_name: &str, device: &str, taken: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = match file_name.rfind('.') {
        Some(i) if i > 0 => (&file_name[..i], &file_name[i..]),
        _ => (file_name, ""),
    };
    let device: String = device.chars().filter(|c| !"\\/:*?\"<>|".contains(*c)).collect();
    let device = if device.trim().is_empty() { "this PC".to_string() } else { device };
    for n in 1..1000 {
        let tag = if n == 1 { device.clone() } else { format!("{device} {n}") };
        let name = format!("{stem} ({tag}){ext}");
        if !taken(&name) {
            return name;
        }
    }
    format!("{stem} ({device} {}){ext}", chrono::Utc::now().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    const E: Indexed = Indexed { sha: "aaa", size: 10, mtime: Some(100) };

    #[test]
    fn a_new_remote_file_is_fetched() {
        assert_eq!(plan_remote(None, Local::Missing, Some("bbb"), 5, false), RemoteAction::Fetch);
    }

    #[test]
    fn untouched_here_and_changed_there_is_fetched() {
        let local = Local::File { size: 10, mtime: Some(100) };
        assert_eq!(plan_remote(Some(E), local, Some("bbb"), 12, false), RemoteAction::Fetch);
    }

    #[test]
    fn unchanged_remote_leaves_a_local_edit_to_the_upload_scan() {
        let local = Local::File { size: 11, mtime: Some(200) };
        assert_eq!(plan_remote(Some(E), local, Some("aaa"), 10, false), RemoteAction::Skip);
    }

    #[test]
    fn changed_on_both_sides_is_a_conflict_after_hashing() {
        let local = Local::File { size: 11, mtime: Some(200) };
        assert_eq!(plan_remote(Some(E), local, Some("bbb"), 12, false), RemoteAction::NeedsHash);
        assert_eq!(after_hash(Some(E), "ccc", Some("bbb")), RemoteAction::Conflict);
    }

    #[test]
    fn touched_but_identical_is_fetched_not_a_conflict() {
        // Saved again without a change: new mtime, same bytes as last sync.
        assert_eq!(after_hash(Some(E), "aaa", Some("bbb")), RemoteAction::Fetch);
    }

    #[test]
    fn the_same_edit_on_both_sides_is_only_indexed() {
        assert_eq!(after_hash(Some(E), "bbb", Some("bbb")), RemoteAction::Index);
        assert_eq!(after_hash(None, "bbb", Some("bbb")), RemoteAction::Index);
    }

    #[test]
    fn an_old_copy_on_a_new_pc_is_a_conflict_not_an_overwrite() {
        let local = Local::File { size: 9, mtime: Some(50) };
        assert_eq!(plan_remote(None, local, Some("bbb"), 12, false), RemoteAction::Conflict);
        let same_size = Local::File { size: 12, mtime: Some(50) };
        assert_eq!(plan_remote(None, same_size, Some("bbb"), 12, false), RemoteAction::NeedsHash);
        assert_eq!(after_hash(None, "ccc", Some("bbb")), RemoteAction::Conflict);
    }

    #[test]
    fn a_placeholder_is_refreshed_never_a_conflict() {
        assert_eq!(plan_remote(Some(E), Local::Placeholder, Some("bbb"), 12, false), RemoteAction::RefreshPlaceholder);
        assert_eq!(plan_remote(Some(E), Local::Placeholder, Some("aaa"), 10, false), RemoteAction::Skip);
    }

    #[test]
    fn uploads_wait_when_workspace_moved_on() {
        assert!(may_upload(Some("aaa"), None));
        assert!(may_upload(Some("aaa"), Some(Some("aaa"))));
        assert!(!may_upload(Some("aaa"), Some(Some("bbb"))));
        assert!(!may_upload(None, Some(Some("bbb"))));
        assert!(may_upload(None, None));
    }

    #[test]
    fn a_server_file_without_checksum_never_loops() {
        let local = Local::File { size: 10, mtime: Some(100) };
        assert_eq!(plan_remote(Some(E), local, None, 10, false), RemoteAction::Skip);
        assert_eq!(plan_remote(Some(E), Local::Placeholder, None, 10, false), RemoteAction::Skip);
        assert_eq!(plan_remote(None, local, None, 10, false), RemoteAction::Index);
        assert_eq!(plan_remote(None, local, None, 12, false), RemoteAction::Conflict);
        assert!(may_upload(Some("aaa"), Some(None)));
        assert!(!may_upload(None, Some(None)));
    }

    #[test]
    fn a_kept_copy_gets_the_pc_name() {
        assert_eq!(kept_name("Face Of Fire.flp", "DESKTOP-1", |_| false), "Face Of Fire (DESKTOP-1).flp");
        let taken = |n: &str| n == "Kick (Laptop).wav";
        assert_eq!(kept_name("Kick.wav", "Laptop", taken), "Kick (Laptop 2).wav");
        assert_eq!(kept_name("README", "", |_| false), "README (this PC)");
        assert_eq!(kept_name("a.wav", "bad:name", |_| false), "a (badname).wav");
    }
}
