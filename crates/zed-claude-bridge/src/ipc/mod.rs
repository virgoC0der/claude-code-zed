//! Unix-domain-socket IPC server consumed by the Zed extension.
//!
//! Layer position: this is layer 5 — depends on `protocol`, `mcp`, and
//! `transport` (specifically [`crate::transport::ClientRegistry`] for
//! per-client outbound delivery and the [`crate::transport::router`]
//! module for at-mention / selection-changed routing decisions).
//!
//! The Zed extension cannot host a TCP server in WASM, so the sidecar
//! exposes a per-workspace Unix socket. The socket path is derived from the
//! workspace root via [`socket_path`] so the extension can locate the
//! sidecar without configuration.

pub mod server;

use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::xxh3_64;

/// Compute `$TMPDIR/zed-claude-bridge-<xxh3-hex>.sock` for the given
/// workspace root.
///
/// The hash input is the canonicalized absolute path of `workspace_root`.
/// If canonicalization fails (path missing or unreadable), the raw bytes
/// of the path passed in are hashed instead — keeping the function total.
///
/// Falls back to `/tmp` when `TMPDIR` is unset or empty (per the spec).
pub fn socket_path(workspace_root: &Path) -> PathBuf {
    let canonical =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let bytes = canonical.as_os_str().as_encoded_bytes();
    let hex = format!("{:016x}", xxh3_64(bytes));

    let tmp = std::env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    tmp.join(format!("zed-claude-bridge-{hex}.sock"))
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;

    /// `socket_path` is deterministic for a given input path.
    #[test]
    fn socket_path_is_stable_for_same_input() {
        let p = PathBuf::from("/Users/me/proj");
        let a = socket_path(&p);
        let b = socket_path(&p);
        assert_eq!(a, b);
    }

    /// File name pattern is exactly `zed-claude-bridge-<16 hex>.sock`.
    #[test]
    fn socket_path_filename_matches_documented_pattern() {
        let p = PathBuf::from("/Users/me/proj");
        let s = socket_path(&p);
        let name = s
            .file_name()
            .and_then(|o| o.to_str())
            .expect("file name utf8");
        assert!(name.starts_with("zed-claude-bridge-"));
        assert!(name.ends_with(".sock"));
        let middle = name
            .strip_prefix("zed-claude-bridge-")
            .and_then(|s| s.strip_suffix(".sock"))
            .expect("strip");
        assert_eq!(middle.len(), 16);
        assert!(
            middle
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    /// Different workspace paths yield different socket paths.
    #[test]
    fn socket_path_differs_for_different_inputs() {
        let a = socket_path(Path::new("/Users/me/aaa"));
        let b = socket_path(Path::new("/Users/me/bbb"));
        assert_ne!(a, b);
    }

    /// The path lives under either `$TMPDIR` (if set) or `/tmp`. We do not
    /// mutate `$TMPDIR` in a test (env mutation is `unsafe` in edition 2024
    /// and forbidden by the workspace lint); instead we just check that the
    /// returned path's parent is one of those two.
    #[test]
    fn socket_path_lives_under_tmpdir_or_tmp() {
        let s = socket_path(Path::new("/Users/me/proj"));
        let parent = s.parent().expect("has parent");
        let tmpdir = std::env::var_os("TMPDIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        match tmpdir {
            Some(t) => assert_eq!(parent, t),
            None => assert_eq!(parent, Path::new("/tmp")),
        }
    }
}
