//! Heartbeat to Workspace: this computer's sync state, every few minutes, so the
//! Home page can say per computer whether the work is backed up (redesign phase 3,
//! POST /api/devices/heartbeat). It sends counts and the sync state, never file
//! names or paths.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::Manager;

use crate::models::SyncStatus;
use crate::AppState;

const FIRST_BEAT_SECS: u64 = 60;
const EVERY_SECS: u64 = 300;

fn id_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("hardwave").join("device-id"))
}

fn valid_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// 128 random bits as hex. RandomState is seeded from the operating system's
/// random source, which is all an install id needs; no extra crate.
fn random_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut out = String::with_capacity(33);
    for i in 0..2u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(nanos ^ i);
        out.push_str(&format!("{:016x}", h.finish()));
        if i == 0 {
            out.push('-');
        }
    }
    out
}

/// This install's id, kept in the app's data folder so the same computer stays
/// one row on the server across restarts and updates.
pub fn device_id() -> String {
    if let Some(p) = id_path() {
        if let Ok(s) = std::fs::read_to_string(&p) {
            let s = s.trim().to_string();
            if valid_id(&s) {
                return s;
            }
        }
        let id = random_id();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, &id);
        return id;
    }
    random_id()
}

pub fn device_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    let name: String = raw.chars().filter(|c| !c.is_control()).take(120).collect();
    if name.trim().is_empty() { "This computer".into() } else { name.trim().to_string() }
}

fn platform() -> &'static str {
    if cfg!(target_os = "windows") { "windows" } else if cfg!(target_os = "macos") { "macos" } else { "linux" }
}

fn body(id: &str, status: &SyncStatus) -> serde_json::Value {
    serde_json::json!({
        "deviceId": id,
        "name": device_name(),
        "platform": platform(),
        "appVersion": env!("CARGO_PKG_VERSION"),
        "status": {
            "state": status.state,
            "filesPending": status.files_pending,
            "filesTotal": status.files_total,
            "bytesTotal": status.bytes_total,
            "bytesSynced": status.bytes_synced,
            "lastSync": status.last_sync,
            "error": status.error,
        }
    })
}

async fn send(token: &str, id: &str, status: &SyncStatus) -> Result<(), String> {
    let url = format!("{}/devices/heartbeat", crate::api::ws_base());
    let res = crate::api::http_client()
        .post(&url)
        .bearer_auth(token)
        .json(&body(id, status))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if res.status().is_success() { Ok(()) } else { Err(format!("heartbeat {}", res.status())) }
}

/// Runs for the life of the app. Signed out or no engine yet: skip the beat.
/// A failed beat is only logged: the server shows "last seen", which is honest.
pub async fn run(handle: tauri::AppHandle) {
    let id = device_id();
    tokio::time::sleep(std::time::Duration::from_secs(FIRST_BEAT_SECS)).await;
    loop {
        let state = handle.state::<AppState>();
        let token = state.api_token.lock().await.clone();
        let engine = state.sync_engine.lock().await.clone();
        if let (Some(token), Some(engine)) = (token, engine) {
            let status = engine.get_status().await;
            if let Err(e) = send(&token, &id, &status).await {
                log::warn!("[device] {}", e);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(EVERY_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_valid_and_differ() {
        let a = random_id();
        let b = random_id();
        assert!(valid_id(&a), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn rejects_bad_ids() {
        assert!(!valid_id("../../x"));
        assert!(!valid_id("short"));
    }
}
