//! What the sync leaves alone.
//!
//! Folders full of tool output (`node_modules`, `.git`) and files that only
//! exist while a program writes (`~$Song.docx`, `*.tmp`, browser downloads in
//! progress) would otherwise be uploaded, versioned and pulled onto every other
//! PC. A leading dot already hid most of them; these are the ones that slipped
//! through, plus the engine's own temporary files.

const EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".svn",
    ".hg",
    "__pycache__",
    "$recycle.bin",
    "system volume information",
];

const EXCLUDED_NAMES: &[&str] = &["thumbs.db", "desktop.ini", ".ds_store"];

const EXCLUDED_SUFFIXES: &[&str] = &[".tmp", ".crdownload", ".part", ".partial", ".download"];

/// Prefix of the file a download is written to before it replaces the real one.
pub const TEMP_PREFIX: &str = ".hwtmp-";

/// True when `rel_path` (either separator) is inside an excluded folder or is a
/// temporary or system file.
pub fn is_excluded(rel_path: &str) -> bool {
    let parts: Vec<&str> = rel_path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let Some((name, dirs)) = parts.split_last() else { return false };
    if dirs.iter().any(|d| EXCLUDED_DIRS.contains(&d.to_ascii_lowercase().as_str())) {
        return true;
    }
    let name = name.to_ascii_lowercase();
    EXCLUDED_DIRS.contains(&name.as_str())
        || EXCLUDED_NAMES.contains(&name.as_str())
        || name.starts_with("~$")
        || name.starts_with(".~lock.")
        || name.starts_with(TEMP_PREFIX)
        || EXCLUDED_SUFFIXES.iter().any(|s| name.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_folders_are_left_alone() {
        assert!(is_excluded("Studio/site/node_modules/react/index.js"));
        assert!(is_excluded("Studio\\code\\.git\\HEAD"));
        assert!(is_excluded("Studio/code/node_modules"));
    }

    #[test]
    fn files_that_only_exist_while_written_are_left_alone() {
        assert!(is_excluded("Studio/Docs/~$Contract.docx"));
        assert!(is_excluded("Studio/Bounces/render.wav.tmp"));
        assert!(is_excluded("Studio/Downloads/pack.zip.crdownload"));
        assert!(is_excluded("Studio/Bounces/.hwtmp-Face Of Fire.wav"));
        assert!(is_excluded("Studio/Thumbs.db"));
    }

    #[test]
    fn music_is_synced() {
        assert!(!is_excluded("Studio/Face Of Fire/Face Of Fire.flp"));
        assert!(!is_excluded("Studio/Face Of Fire (autosaved at 0h05).flp"));
        assert!(!is_excluded("Studio/Kicks/node kick.wav"));
        assert!(!is_excluded("Studio/Templates/tmp kick.wav"));
    }
}
