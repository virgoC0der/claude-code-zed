//! Locate Zed's per-channel SQLite state DB.
//!
//! Path shape (macOS): `~/Library/Application Support/Zed/db/<channel>/db.sqlite`
//! where `<channel>` is `0-stable` (release), `0-preview`, `0-dev`, etc.
//!
//! Linux extension point (not implemented this iteration): Zed uses
//! `$XDG_DATA_HOME/Zed/db/<channel>/db.sqlite` or
//! `~/.local/share/zed/...`. Add a `cfg(target_os = "linux")` branch to
//! [`zed_data_dir`] when Linux support is added.

use std::path::{Path, PathBuf};

use crate::zed_watch::ZedWatchError;

/// The `db.sqlite` filename inside a channel directory.
const DB_FILE: &str = "db.sqlite";

/// Pick the best channel subdirectory under `db_root`. Prefers `0-stable`;
/// otherwise the lexically-greatest `0-*` directory that contains a
/// `db.sqlite`. Returns the full path to `db.sqlite`, or `None` if no channel
/// directory holds one.
pub fn pick_channel_db(db_root: &Path) -> Option<PathBuf> {
    // Preference order: 0-stable first, then any other 0-* by reverse sort.
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(db_root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("0-"))
        })
        .collect();
    candidates.sort();
    // Stable wins if present; else greatest-named channel.
    let stable = candidates
        .iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some("0-stable"))
        .cloned();
    let chosen = stable.or_else(|| candidates.into_iter().next_back());
    let db = chosen?.join(DB_FILE);
    db.is_file().then_some(db)
}

/// Locate Zed's `db.sqlite` on this host. macOS only; returns
/// [`ZedWatchError::DbNotFound`] on every other platform (the Linux branch is
/// a documented future extension point — see the module docs).
pub fn locate() -> Result<PathBuf, ZedWatchError> {
    let root = zed_data_dir()?.join("db");
    pick_channel_db(&root)
        .ok_or_else(|| ZedWatchError::DbNotFound(format!("no channel db under {}", root.display())))
}

/// The platform-specific Zed application-data directory.
#[cfg(target_os = "macos")]
fn zed_data_dir() -> Result<PathBuf, ZedWatchError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| ZedWatchError::DbNotFound("HOME not set".to_string()))?;
    Ok(PathBuf::from(home).join("Library/Application Support/Zed"))
}

#[cfg(not(target_os = "macos"))]
fn zed_data_dir() -> Result<PathBuf, ZedWatchError> {
    Err(ZedWatchError::DbNotFound(
        "Zed active-file watcher is macOS-only this iteration".to_string(),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests unwrap on assertion failures")]
mod tests {
    use super::*;

    #[test]
    fn picks_stable_when_multiple_channels_present() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for ch in ["0-dev", "0-stable", "0-preview"] {
            let dir = root.join(ch);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("db.sqlite"), b"").unwrap();
        }
        let got = pick_channel_db(root).unwrap();
        assert_eq!(got, root.join("0-stable").join("db.sqlite"));
    }

    #[test]
    fn falls_back_to_greatest_channel_when_no_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for ch in ["0-dev", "0-preview"] {
            let dir = root.join(ch);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("db.sqlite"), b"").unwrap();
        }
        let got = pick_channel_db(root).unwrap();
        // sort() => ["0-dev","0-preview"]; next_back() => "0-preview".
        assert_eq!(got, root.join("0-preview").join("db.sqlite"));
    }

    #[test]
    fn none_when_channel_dir_has_no_db_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("0-stable")).unwrap();
        assert!(pick_channel_db(tmp.path()).is_none());
    }
}
