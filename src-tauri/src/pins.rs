//! What stays on this PC, per folder: always, when opened, or online only.
//!
//! The choice is the producer's (the This PC page, and the first start). It is
//! stored here and applied in two ways: the pin state Windows shows in Explorer
//! (cloudfiles::set_pin), and the bytes themselves, which the engine brings in
//! for "always" and frees for "online only".

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Every file in the folder is kept on this PC.
    Always,
    /// Files come down when something opens them, and stay until space is freed.
    WhenOpened,
    /// Files take no space until opened, and Free Up Space takes them back.
    Online,
}

impl Mode {
    pub fn pin(self) -> crate::cloudfiles::Pin {
        match self {
            Mode::Always => crate::cloudfiles::Pin::Pinned,
            Mode::WhenOpened => crate::cloudfiles::Pin::Unspecified,
            Mode::Online => crate::cloudfiles::Pin::Unpinned,
        }
    }
}

fn store_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hardwave")
        .join("workspace-folder-modes.json")
}

/// Folder (relative to the sync root, `/` separators) to its mode.
pub fn load() -> HashMap<String, Mode> {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(modes: &HashMap<String, Mode>) {
    let path = store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(modes) {
        let _ = std::fs::write(&path, json);
    }
}

/// The mode that governs a file: the one set on its nearest folder, or
/// "when opened" when none is.
pub fn mode_for(rel_path: &str, modes: &HashMap<String, Mode>) -> Mode {
    let mut best: Option<(&str, Mode)> = None;
    for (folder, mode) in modes {
        let inside = rel_path == folder
            || (rel_path.len() > folder.len()
                && rel_path.starts_with(folder.as_str())
                && rel_path.as_bytes()[folder.len()] == b'/');
        if inside && best.is_none_or(|(b, _)| folder.len() > b.len()) {
            best = Some((folder, *mode));
        }
    }
    best.map(|(_, m)| m).unwrap_or(Mode::WhenOpened)
}

/// Setting a folder replaces whatever its subfolders had: the producer chose
/// for the whole folder.
pub fn set(modes: &mut HashMap<String, Mode>, folder: &str, mode: Mode) {
    let prefix = format!("{folder}/");
    modes.retain(|f, _| !f.starts_with(&prefix));
    modes.remove(folder);
    // Stored only where it differs from what the folder inherits. Dropping every "When opened"
    // left a subfolder of an "Always" folder on Always, so the producer could not change it.
    if mode_for(folder, modes) != mode {
        modes.insert(folder.to_string(), mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_folder_decides() {
        let mut m = HashMap::new();
        m.insert("Studio/FL Studio".to_string(), Mode::Always);
        m.insert("Studio/FL Studio/Old".to_string(), Mode::Online);
        assert_eq!(
            mode_for("Studio/FL Studio/Face Of Fire.flp", &m),
            Mode::Always
        );
        assert_eq!(mode_for("Studio/FL Studio/Old/2019.flp", &m), Mode::Online);
        assert_eq!(mode_for("Studio/Sounds/kick.wav", &m), Mode::WhenOpened);
    }

    #[test]
    fn when_opened_under_an_always_folder_takes_effect() {
        let mut m = HashMap::new();
        set(&mut m, "Studio", Mode::Always);
        set(&mut m, "Studio/FL", Mode::WhenOpened);
        assert_eq!(mode_for("Studio/FL/a.flp", &m), Mode::WhenOpened);
        assert_eq!(mode_for("Studio/Sounds/kick.wav", &m), Mode::Always);
        // Back to what it inherits: nothing left to store for the subfolder.
        set(&mut m, "Studio/FL", Mode::Always);
        assert_eq!(m.len(), 1);
        set(&mut m, "Studio", Mode::WhenOpened);
        assert!(m.is_empty());
    }

    #[test]
    fn a_folder_with_a_longer_name_is_not_inside() {
        let mut m = HashMap::new();
        m.insert("Studio/FL".to_string(), Mode::Always);
        assert_eq!(mode_for("Studio/FL Studio/a.flp", &m), Mode::WhenOpened);
        assert_eq!(mode_for("Studio/FL/a.flp", &m), Mode::Always);
    }

    #[test]
    fn setting_a_folder_clears_its_subfolders() {
        let mut m = HashMap::new();
        m.insert("Studio/FL Studio/Old".to_string(), Mode::Online);
        set(&mut m, "Studio/FL Studio", Mode::Always);
        assert_eq!(m.len(), 1);
        set(&mut m, "Studio/FL Studio", Mode::WhenOpened);
        assert!(m.is_empty());
    }
}
