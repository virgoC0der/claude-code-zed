//! Zed active-file awareness: polls Zed's local SQLite workspace state to
//! learn each connected Claude session's active file, and pushes it through
//! the existing EditorState + `selection_changed` pipeline.
//!
//! Layer position: sits above `transport` (it reads the client registry) and
//! below `app` (which starts the watcher). SQLite I/O is confined to the
//! `query` / `schema_probe` / `db_path` submodules. No `unsafe`.
//!
//! macOS only this iteration. `db_path` documents the Linux extension point.

pub mod db_path;
pub mod query;
pub mod schema_probe;

use std::path::PathBuf;

/// Errors at the `zed_watch` module boundary. The `app` layer maps these into
/// "feature silently disabled" — none are fatal to the sidecar.
#[derive(Debug, thiserror::Error)]
pub enum ZedWatchError {
    /// Zed's DB could not be located (Zed not installed, non-macOS, or a
    /// non-standard install with no override given).
    #[error("Zed state DB not found: {0}")]
    DbNotFound(String),

    /// The DB exists but its schema doesn't match what the queries expect
    /// (likely a Zed version change). Feature is disabled.
    #[error("Zed state DB schema mismatch: {0}")]
    SchemaMismatch(String),

    /// A SQLite-level error during open or query.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Configuration for the watcher, assembled by the `app` layer from CLI flags.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Resolved path to `db.sqlite`. When `None`, [`db_path::locate`] is used.
    pub db_path: Option<PathBuf>,
}
