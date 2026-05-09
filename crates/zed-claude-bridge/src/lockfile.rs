//! Manage the discovery directory `~/.claude/ide/` and the per-port lock files
//! the Claude Code CLI scans when running `/ide`.
//!
//! This module owns the I/O (file system + brief TCP probes for stale-lock
//! pruning). Wire types live in [`crate::protocol::LockFile`]; this layer is
//! responsible for serialization, atomic writes, permission verification, and
//! cleanup. See `docs/protocol.md` §1 and the OpenSpec
//! `specs/lockfile/spec.md` for the contract.
//!
//! Layer position: this is layer 2 (depends on `protocol`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::protocol::LockFile;

/// POSIX mode for the lock-file parent directory: rwx for owner only.
pub const DIR_MODE: u32 = 0o700;
/// POSIX mode for individual lock files: rw for owner only.
pub const FILE_MODE: u32 = 0o600;
/// Timeout for the TCP probe used by [`LockDir::prune_stale`].
const STALE_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Errors produced by the [`LockDir`] API.
#[derive(Debug, Error)]
pub enum LockfileError {
    /// Underlying filesystem I/O failed.
    #[error("lockfile I/O at {path:?}: {source}")]
    Io {
        /// Path that triggered the error (best effort).
        path: PathBuf,
        /// Wrapped I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The lock-file directory exists with the wrong POSIX mode.
    #[error("lockfile dir {path:?} has mode {found:#o}, expected {expected:#o}")]
    DirModeMismatch {
        /// Directory path.
        path: PathBuf,
        /// Mode bits we observed.
        found: u32,
        /// Mode bits we required.
        expected: u32,
    },
    /// A freshly written lock file does not have the required POSIX mode.
    #[error("lockfile {path:?} has mode {found:#o}, expected {expected:#o} after write")]
    FileModeMismatch {
        /// File path.
        path: PathBuf,
        /// Mode bits we observed.
        found: u32,
        /// Mode bits we required.
        expected: u32,
    },
    /// JSON encoding/decoding failed.
    #[error("lockfile JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Handle to the discovery directory, typically `~/.claude/ide/`.
///
/// The handle holds only the path; opening one is cheap. All methods are
/// idempotent against repeated calls in normal sidecar lifecycle (startup
/// prune → write → workspace updates → graceful shutdown remove).
#[derive(Debug, Clone)]
pub struct LockDir {
    path: PathBuf,
}

impl LockDir {
    /// Open (and create if missing) the discovery directory at `path`.
    ///
    /// Behaviour:
    /// - If the directory does not exist, it is created with mode `0o700`.
    /// - If it exists, its mode is verified to be exactly `0o700` (lower bits
    ///   only — we mask off file-type bits before comparison).
    ///
    /// Errors:
    /// - [`LockfileError::Io`] for filesystem failures.
    /// - [`LockfileError::DirModeMismatch`] if an existing dir has the wrong
    ///   permissions. Callers MAY choose to fix the mode and retry, but this
    ///   crate refuses to silently chmod a directory it did not create.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, LockfileError> {
        let path = path.into();
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                let mode = meta.permissions().mode() & 0o777;
                if mode != DIR_MODE {
                    return Err(LockfileError::DirModeMismatch {
                        path,
                        found: mode,
                        expected: DIR_MODE,
                    });
                }
                Ok(Self { path })
            }
            Ok(_) => Err(LockfileError::Io {
                path: path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "lock-file directory path exists but is not a directory",
                ),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Create directory and any missing parents, then chmod 0o700
                // to override umask. We use `create_dir_all` first because
                // PermissionsExt-via-DirBuilder is awkward across libc versions.
                fs::create_dir_all(&path).map_err(|source| LockfileError::Io {
                    path: path.clone(),
                    source,
                })?;
                let perms = std::fs::Permissions::from_mode(DIR_MODE);
                fs::set_permissions(&path, perms).map_err(|source| LockfileError::Io {
                    path: path.clone(),
                    source,
                })?;
                Ok(Self { path })
            }
            Err(source) => Err(LockfileError::Io { path, source }),
        }
    }

    /// Path of the discovery directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Compute the on-disk path for the lock file of the given port.
    pub fn lock_path(&self, port: u16) -> PathBuf {
        self.path.join(format!("{port}.lock"))
    }

    /// Atomically write the lock file for `port`.
    ///
    /// Sequence:
    /// 1. Serialize `body` to JSON.
    /// 2. Open `<port>.lock.tmp` with `O_CREAT | O_TRUNC | O_WRONLY` and mode
    ///    `0o600`.
    /// 3. Write the bytes, `fsync(2)`, then `rename(2)` to `<port>.lock`.
    /// 4. Verify the final file's mode is `0o600`.
    ///
    /// Concurrent readers will only ever see the previous valid JSON or the
    /// new one — never a partial buffer.
    pub fn write_lock(&self, port: u16, body: &LockFile) -> Result<(), LockfileError> {
        let final_path = self.lock_path(port);
        let tmp_path = self.path.join(format!("{port}.lock.tmp"));

        // Best-effort: if a leftover tmp from a crashed previous run exists,
        // remove it so `create_new` doesn't fail. We deliberately do NOT use
        // create_new here — using O_TRUNC is fine because the temp path is
        // unique to this port and we are the writer. The atomicity guarantee
        // is provided by the rename, not by the temp file's exclusivity.
        let bytes = serde_json::to_vec(body)?;
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(FILE_MODE)
                .open(&tmp_path)
                .map_err(|source| LockfileError::Io {
                    path: tmp_path.clone(),
                    source,
                })?;
            file.write_all(&bytes).map_err(|source| LockfileError::Io {
                path: tmp_path.clone(),
                source,
            })?;
            file.sync_all().map_err(|source| LockfileError::Io {
                path: tmp_path.clone(),
                source,
            })?;
            // Re-assert mode in case umask interfered (some platforms ignore
            // the OpenOptions::mode call when the file already existed).
            let perms = std::fs::Permissions::from_mode(FILE_MODE);
            fs::set_permissions(&tmp_path, perms).map_err(|source| LockfileError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        }

        fs::rename(&tmp_path, &final_path).map_err(|source| LockfileError::Io {
            path: final_path.clone(),
            source,
        })?;

        // Verify post-rename — defensive, since the spec mandates "verified
        // after every write".
        let meta = fs::metadata(&final_path).map_err(|source| LockfileError::Io {
            path: final_path.clone(),
            source,
        })?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != FILE_MODE {
            return Err(LockfileError::FileModeMismatch {
                path: final_path,
                found: mode,
                expected: FILE_MODE,
            });
        }
        debug!(port, path = %final_path.display(), "wrote lock file");
        Ok(())
    }

    /// Unlink the lock file for `port`. Idempotent: a missing file is OK.
    pub fn remove_lock(&self, port: u16) -> Result<(), LockfileError> {
        let path = self.lock_path(port);
        match fs::remove_file(&path) {
            Ok(()) => {
                debug!(port, path = %path.display(), "removed lock file");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(LockfileError::Io { path, source }),
        }
    }

    /// List ports for which a `<port>.lock` file currently exists in this dir.
    ///
    /// Filenames that don't match `<u16>.lock` are skipped with a `warn` log
    /// rather than failing the call — leftover artefacts from other tools
    /// must not break startup.
    pub fn list(&self) -> Result<Vec<u16>, LockfileError> {
        let mut ports = Vec::new();
        let entries = fs::read_dir(&self.path).map_err(|source| LockfileError::Io {
            path: self.path.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| LockfileError::Io {
                path: self.path.clone(),
                source,
            })?;
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                warn!(name = ?name, "skipping non-utf8 lock-dir entry");
                continue;
            };
            let Some(stem) = name_str.strip_suffix(".lock") else {
                continue;
            };
            match stem.parse::<u16>() {
                Ok(port) => ports.push(port),
                Err(_) => warn!(name = name_str, "skipping malformed lock filename"),
            }
        }
        ports.sort_unstable();
        Ok(ports)
    }

    /// Probe each known lock file's port; if `127.0.0.1:<port>` refuses the
    /// connection, treat the lock as stale and unlink it.
    ///
    /// Live ports — connection succeeds OR connection times out (still
    /// reachable, just busy) — are left alone. We use a short
    /// [`STALE_PROBE_TIMEOUT`] so startup isn't gated on a slow peer.
    pub async fn prune_stale(&self) -> Result<(), LockfileError> {
        for port in self.list()? {
            if probe_port_dead(port).await {
                let path = self.lock_path(port);
                match fs::remove_file(&path) {
                    Ok(()) => debug!(port, "pruned stale lock file"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Raced with another sidecar; ignore.
                    }
                    Err(source) => {
                        return Err(LockfileError::Io { path, source });
                    }
                }
            } else {
                debug!(port, "lock file is live; keeping");
            }
        }
        Ok(())
    }
}

/// Returns `true` iff `127.0.0.1:<port>` actively refuses the connection
/// (i.e. nothing is listening). Any other outcome — success, timeout, or
/// other error — is treated as "alive, do not delete" so we err on the side
/// of preserving lock files for live peers.
async fn probe_port_dead(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    match timeout(STALE_PROBE_TIMEOUT, TcpStream::connect(&addr)).await {
        Ok(Ok(_stream)) => false, // alive
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => true,
        Ok(Err(_)) => false,    // other I/O error: be conservative, keep file
        Err(_elapsed) => false, // timed out: be conservative, keep file
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn sample_body(port: u16) -> LockFile {
        LockFile {
            pid: 4242,
            workspace_folders: vec![PathBuf::from("/tmp/test-ws")],
            ide_name: "Zed".to_string(),
            transport: "ws".to_string(),
            running_in_windows: false,
            auth_token: format!("token-{port}"),
        }
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn open_creates_dir_with_0700() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("ide");
        assert!(!target.exists());

        let lock_dir = LockDir::open(&target).expect("open creates dir");
        assert_eq!(lock_dir.path(), target.as_path());
        assert!(target.is_dir());
        assert_eq!(mode_of(&target), DIR_MODE);
    }

    #[test]
    fn open_succeeds_when_existing_dir_has_correct_mode() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("ide");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, std::fs::Permissions::from_mode(DIR_MODE)).unwrap();

        let lock_dir = LockDir::open(&target).expect("open accepts pre-existing 0700 dir");
        assert_eq!(lock_dir.path(), target.as_path());
    }

    #[test]
    fn open_rejects_existing_dir_with_wrong_mode() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("ide");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        match LockDir::open(&target) {
            Err(LockfileError::DirModeMismatch {
                found, expected, ..
            }) => {
                assert_eq!(found, 0o755);
                assert_eq!(expected, DIR_MODE);
            }
            other => panic!("expected DirModeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn write_lock_produces_0600_file_with_round_tripping_json() {
        let tmp = TempDir::new().unwrap();
        let lock_dir = LockDir::open(tmp.path().join("ide")).unwrap();

        let body = sample_body(54321);
        lock_dir.write_lock(54321, &body).expect("write_lock");

        let path = lock_dir.lock_path(54321);
        assert!(path.exists());
        assert_eq!(mode_of(&path), FILE_MODE);

        let on_disk = fs::read_to_string(&path).unwrap();
        let parsed: LockFile = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(parsed, body);
    }

    #[test]
    fn write_lock_overwrites_existing_file_atomically() {
        // We cannot easily test "no partial file ever observed" deterministically
        // without a contrived race harness. Instead we assert that repeated
        // overwrites yield the latest body each time and the temp sibling is
        // gone (rename consumed it).
        let tmp = TempDir::new().unwrap();
        let lock_dir = LockDir::open(tmp.path().join("ide")).unwrap();

        let mut body = sample_body(40000);
        lock_dir.write_lock(40000, &body).unwrap();
        body.workspace_folders = vec![PathBuf::from("/tmp/other-ws")];
        lock_dir.write_lock(40000, &body).unwrap();

        let path = lock_dir.lock_path(40000);
        let parsed: LockFile = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            parsed.workspace_folders,
            vec![PathBuf::from("/tmp/other-ws")]
        );

        let tmp_sibling = lock_dir.path().join("40000.lock.tmp");
        assert!(
            !tmp_sibling.exists(),
            "tmp sibling should be consumed by rename"
        );
    }

    #[test]
    fn remove_lock_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let lock_dir = LockDir::open(tmp.path().join("ide")).unwrap();
        lock_dir.write_lock(50000, &sample_body(50000)).unwrap();
        assert!(lock_dir.lock_path(50000).exists());

        lock_dir.remove_lock(50000).expect("first remove");
        assert!(!lock_dir.lock_path(50000).exists());

        // Removing again must succeed (idempotent).
        lock_dir.remove_lock(50000).expect("second remove no-ops");
    }

    #[test]
    fn list_returns_sorted_ports_skipping_garbage() {
        let tmp = TempDir::new().unwrap();
        let lock_dir = LockDir::open(tmp.path().join("ide")).unwrap();
        lock_dir.write_lock(20000, &sample_body(20000)).unwrap();
        lock_dir.write_lock(10001, &sample_body(10001)).unwrap();
        lock_dir.write_lock(30000, &sample_body(30000)).unwrap();
        // Garbage filenames in the same dir.
        fs::write(lock_dir.path().join("README.txt"), "ignore me").unwrap();
        fs::write(lock_dir.path().join("not-a-port.lock"), "{}").unwrap();

        let ports = lock_dir.list().unwrap();
        assert_eq!(ports, vec![10001, 20000, 30000]);
    }
}
