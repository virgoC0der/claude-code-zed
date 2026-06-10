# Zed Active-File Awareness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Auto-detect each connected Claude session's active file by polling Zed's local SQLite state, matching the session's cwd to an open worktree, and pushing it through the existing EditorState + `selection_changed` pipeline — so `getOpenEditors` / `getCurrentSelection` reflect the active file with no user action.

**Architecture:** A new `zed_watch` module (layer between `transport` and `app`) opens Zed's `db.sqlite` read-only, validates its schema once at startup, watches the WAL file with the `notify` crate (debounced 400ms, 2s poll fallback), and on each change queries the active file for every registered Claude session (matched by canonical cwd → worktree). Changed files are written to the shared `EditorState` and pushed as a `selection_changed` notification routed directly to that session via its registry `tx`. Reuses the existing `peer-cwd-discovery` cwd resolution (verified to resolve precise cwds even under the LaunchAgent `$HOME` deployment). No Zed extension.

**Tech Stack:** Rust 2024, tokio, `rusqlite` (bundled SQLite, read-only), `notify` (file watcher), existing `protocol`/`mcp::state`/`transport::registry` types.

**Design doc:** `docs/superpowers/specs/2026-06-09-zed-active-file-awareness-design.md`

**Decisions locked for this plan (from design "待细化" items):**
- `--watch-zed-db` defaults to **on**; `--no-watch-zed-db` disables.
- Debounce window: **400ms**. WAL-watch failure fallback: **2s polling**.
- `rusqlite` with `features = ["bundled"]`.
- Pushed selection is empty: `text: ""`, `selection.is_empty: true`, positions `(0,0)`.
- No mid-session cwd re-resolution (YAGNI).
- macOS only this iteration; `db_path` module has a documented extension point for Linux.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/zed-claude-bridge/Cargo.toml` | Add `rusqlite` (bundled) + `notify` deps. |
| `crates/zed-claude-bridge/src/zed_watch/mod.rs` | Module root: re-exports, `ZedWatchError`, `WatchConfig`. |
| `crates/zed-claude-bridge/src/zed_watch/db_path.rs` | Locate `~/Library/Application Support/Zed/db/<channel>/db.sqlite`. Pure path logic + dir probe. |
| `crates/zed-claude-bridge/src/zed_watch/schema_probe.rs` | Validate required tables/columns exist; gate the whole feature. |
| `crates/zed-claude-bridge/src/zed_watch/query.rs` | The cwd→active-file query (read-only connection). |
| `crates/zed-claude-bridge/src/zed_watch/watcher.rs` | Background task: notify-watch + debounce + per-session dedup + push. |
| `crates/zed-claude-bridge/src/lib.rs` | Add `pub mod zed_watch;`. |
| `crates/zed-claude-bridge/src/app/cli.rs` | Add `--watch-zed-db` / `--no-watch-zed-db` / `--zed-db-path` flags to `DaemonArgs`. |
| `crates/zed-claude-bridge/src/app/lifecycle.rs` | Wire watcher startup into `run_daemon`. |
| `crates/zed-claude-bridge/tests/zed_watch.rs` | Integration: fixture db → query → EditorState/notification assertions. |

**Layer compliance:** `zed_watch` depends on `protocol`, `mcp::state`, `transport::registry`. SQLite I/O is confined to `query.rs` / `schema_probe.rs` / `db_path.rs`. No `unsafe`. `thiserror` at module boundary; `anyhow` only at the `app` edge. `tracing` for all logs.

---

## Task 1: Add dependencies

**Files:**
- Modify: `crates/zed-claude-bridge/Cargo.toml`

- [ ] **Step 1: Add `rusqlite` and `notify` to `[dependencies]`**

In `crates/zed-claude-bridge/Cargo.toml`, add these two lines to the `[dependencies]` table (after the `xxhash-rust` line, before the `[dev-dependencies]` table):

```toml
# rusqlite (bundled SQLite) reads Zed's local workspace state DB read-only.
# `bundled` pins a known SQLite version into the binary so we don't depend on
# the host's libsqlite3 (avoids version-skew with Zed's own copy).
rusqlite = { version = "0.40", features = ["bundled"] }
# notify watches Zed's db.sqlite-wal for changes to trigger active-file refresh.
notify = "8"
```

- [ ] **Step 2: Verify it builds**

Run: `cargo check --workspace --all-targets`
Expected: PASS (new deps compile; no usage yet).

- [ ] **Step 3: Commit**

```bash
git add crates/zed-claude-bridge/Cargo.toml Cargo.lock
git commit -m "build: add rusqlite (bundled) + notify for Zed active-file watcher"
```

---

## Task 2: `db_path` — locate Zed's SQLite DB

**Files:**
- Create: `crates/zed-claude-bridge/src/zed_watch/db_path.rs`
- Create: `crates/zed-claude-bridge/src/zed_watch/mod.rs`
- Modify: `crates/zed-claude-bridge/src/lib.rs`

- [ ] **Step 1: Create the module root `mod.rs`**

Create `crates/zed-claude-bridge/src/zed_watch/mod.rs`:

```rust
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
```

- [ ] **Step 2: Write the failing test for `db_path` channel selection**

Create `crates/zed-claude-bridge/src/zed_watch/db_path.rs` with the test first:

```rust
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
```

- [ ] **Step 3: Add `tempfile` to dev-deps if not already a workspace dev-dep**

`tempfile = "3"` is already in `[dev-dependencies]` (confirmed in Cargo.toml). No change needed; this step is a check only.

- [ ] **Step 4: Add `pub mod zed_watch;` to lib.rs**

In `crates/zed-claude-bridge/src/lib.rs`, add after the `pub mod transport;` line:

```rust
pub mod zed_watch;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p zed-claude-bridge zed_watch::db_path`
Expected: 3 tests PASS.

- [ ] **Step 6: Add the public `locate` entry point**

Append to `db_path.rs` (before the `#[cfg(test)]` block), the home-directory-based locator:

```rust
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
```

- [ ] **Step 7: Run the full module tests + clippy**

Run: `cargo test -p zed-claude-bridge zed_watch && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/zed-claude-bridge/src/zed_watch/ crates/zed-claude-bridge/src/lib.rs
git commit -m "feat(zed_watch): locate Zed per-channel SQLite state DB"
```

---

## Task 3: `schema_probe` — validate DB schema (the degradation gate)

**Files:**
- Create: `crates/zed-claude-bridge/src/zed_watch/schema_probe.rs`
- Modify: `crates/zed-claude-bridge/src/zed_watch/mod.rs` (add `pub mod schema_probe;`)

- [ ] **Step 1: Declare the submodule**

In `crates/zed-claude-bridge/src/zed_watch/mod.rs`, add after `pub mod db_path;`:

```rust
pub mod schema_probe;
```

- [ ] **Step 2: Write the failing test with fixture DBs**

Create `crates/zed-claude-bridge/src/zed_watch/schema_probe.rs`:

```rust
//! Validate that Zed's DB has the tables/columns our queries depend on.
//!
//! This is the **degradation gate**: if Zed changes its schema in a future
//! version, [`probe`] returns [`ZedWatchError::SchemaMismatch`] and the `app`
//! layer disables the watcher without affecting the rest of the sidecar.

use rusqlite::Connection;

use crate::zed_watch::ZedWatchError;

/// (table, column) pairs the active-file query reads. Probed via
/// `PRAGMA table_info`.
const REQUIRED: &[(&str, &str)] = &[
    ("workspaces", "session_id"),
    ("workspaces", "paths"),
    ("workspaces", "timestamp"),
    ("items", "active"),
    ("items", "kind"),
    ("items", "item_id"),
    ("items", "workspace_id"),
    ("editors", "path"),
    ("editors", "item_id"),
    ("editors", "workspace_id"),
];

/// Confirm every (table, column) in [`REQUIRED`] exists. Returns
/// [`ZedWatchError::SchemaMismatch`] naming the first missing one.
pub fn probe(conn: &Connection) -> Result<(), ZedWatchError> {
    for (table, column) in REQUIRED {
        if !column_exists(conn, table, column)? {
            return Err(ZedWatchError::SchemaMismatch(format!(
                "{table}.{column} missing"
            )));
        }
    }
    Ok(())
}

/// `true` iff `table` has a column named `column`. Uses `PRAGMA table_info`,
/// which lists `(cid, name, type, notnull, dflt_value, pk)` rows.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, ZedWatchError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests unwrap on assertion failures")]
mod tests {
    use super::*;

    /// Build an in-memory DB with the minimal real Zed schema our query needs.
    fn good_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE workspaces (
                 workspace_id INTEGER PRIMARY KEY,
                 paths TEXT, timestamp TEXT, session_id TEXT, window_id INTEGER
             );
             CREATE TABLE items (
                 item_id INTEGER, workspace_id INTEGER, pane_id INTEGER,
                 kind TEXT, position INTEGER, active INTEGER
             );
             CREATE TABLE editors (
                 item_id INTEGER, workspace_id INTEGER, path BLOB
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn probe_passes_on_good_schema() {
        assert!(probe(&good_db()).is_ok());
    }

    #[test]
    fn probe_fails_when_column_missing() {
        let conn = Connection::open_in_memory().unwrap();
        // workspaces missing session_id.
        conn.execute_batch(
            "CREATE TABLE workspaces (workspace_id INTEGER, paths TEXT, timestamp TEXT);
             CREATE TABLE items (item_id INTEGER, workspace_id INTEGER, kind TEXT, active INTEGER);
             CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB);",
        )
        .unwrap();
        let err = probe(&conn).unwrap_err();
        assert!(matches!(err, ZedWatchError::SchemaMismatch(_)));
        assert!(err.to_string().contains("session_id"));
    }

    #[test]
    fn probe_fails_when_table_missing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE workspaces (session_id TEXT, paths TEXT, timestamp TEXT);")
            .unwrap();
        // items + editors absent -> PRAGMA returns no rows -> column missing.
        let err = probe(&conn).unwrap_err();
        assert!(matches!(err, ZedWatchError::SchemaMismatch(_)));
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p zed-claude-bridge zed_watch::schema_probe`
Expected: 3 tests PASS.

- [ ] **Step 4: clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/zed-claude-bridge/src/zed_watch/schema_probe.rs crates/zed-claude-bridge/src/zed_watch/mod.rs
git commit -m "feat(zed_watch): schema probe gates the feature on Zed DB compatibility"
```

---

## Task 4: `query` — cwd → active file

**Files:**
- Create: `crates/zed-claude-bridge/src/zed_watch/query.rs`
- Modify: `crates/zed-claude-bridge/src/zed_watch/mod.rs` (add `pub mod query;`)

- [ ] **Step 1: Declare the submodule**

In `mod.rs`, add after `pub mod schema_probe;`:

```rust
pub mod query;
```

- [ ] **Step 2: Write the failing test (the core matching logic)**

Create `crates/zed-claude-bridge/src/zed_watch/query.rs`:

```rust
//! The active-file query: given a Claude session's canonical cwd, find the
//! active editor file in the matching open Zed worktree.
//!
//! Judgement chain (verified against live Zed 1.5.4 data, see design doc §3):
//!   1. current session = the most-recent non-empty `workspaces.session_id`.
//!   2. among workspaces in that session, match the one whose `paths` equals
//!      or is a path-prefix of the cwd; longest match wins (nested worktrees).
//!   3. that workspace's `items.active = 1` (kind 'Editor') joined to
//!      `editors.path` is the active file.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::zed_watch::ZedWatchError;

/// Read the id of the current Zed session (most-recent non-empty session_id).
/// Returns `None` if no workspace has a session_id (no window open).
pub fn current_session(conn: &Connection) -> Result<Option<String>, ZedWatchError> {
    let mut stmt = conn.prepare(
        "SELECT session_id FROM workspaces
         WHERE session_id IS NOT NULL AND session_id <> ''
         ORDER BY timestamp DESC LIMIT 1",
    )?;
    let mut rows = stmt.query([])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get::<_, String>(0)?)),
        None => Ok(None),
    }
}

/// Active file for the worktree matching `cwd` within the current session.
///
/// Returns `Ok(None)` when no open worktree in the current session contains
/// `cwd`, or the matched worktree has no active editor. The returned path is
/// the absolute file path stored by Zed.
pub fn active_file_for_cwd(
    conn: &Connection,
    cwd: &Path,
) -> Result<Option<PathBuf>, ZedWatchError> {
    let Some(session) = current_session(conn)? else {
        return Ok(None);
    };
    let cwd_str = cwd.to_string_lossy().to_string();

    // Candidate worktrees in this session, longest paths first so the most
    // specific (nested) worktree wins. We match in Rust rather than SQL so the
    // prefix test is path-component-aware (avoids `/a/b` matching `/a/bc`).
    let mut stmt = conn.prepare(
        "SELECT w.paths, e.path
         FROM workspaces w
         JOIN items   i ON i.workspace_id = w.workspace_id AND i.active = 1 AND i.kind = 'Editor'
         JOIN editors e ON e.item_id = i.item_id AND e.workspace_id = w.workspace_id
         WHERE w.session_id = ?1
         ORDER BY length(w.paths) DESC",
    )?;
    let rows = stmt.query_map([&session], |row| {
        let paths: Option<String> = row.get(0)?;
        // editors.path is a BLOB; read as bytes then lossily decode.
        let active: Vec<u8> = row.get(1)?;
        Ok((paths, active))
    })?;

    for row in rows {
        let (paths, active_bytes) = row?;
        let Some(paths) = paths else { continue };
        if cwd_matches_worktree(&cwd_str, &paths) {
            let active = String::from_utf8_lossy(&active_bytes).to_string();
            if active.is_empty() {
                continue;
            }
            return Ok(Some(PathBuf::from(active)));
        }
    }
    Ok(None)
}

/// `true` iff `cwd` equals `worktree` or is nested under it, component-aware.
fn cwd_matches_worktree(cwd: &str, worktree: &str) -> bool {
    if cwd == worktree {
        return true;
    }
    Path::new(cwd).starts_with(worktree)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests unwrap on assertion failures")]
mod tests {
    use super::*;

    /// Build an in-memory DB matching Zed's real schema and seed rows.
    /// `rows` = list of (workspace_id, session_id, paths, active_file).
    fn db_with(rows: &[(i64, &str, &str, &str)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY, paths TEXT,
                 timestamp TEXT, session_id TEXT);
             CREATE TABLE items (item_id INTEGER, workspace_id INTEGER, kind TEXT, active INTEGER);
             CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB);",
        )
        .unwrap();
        for (i, (wsid, sid, paths, active)) in rows.iter().enumerate() {
            let item_id = (i + 1) as i64;
            // timestamp ascending so later rows are "more recent".
            conn.execute(
                "INSERT INTO workspaces (workspace_id, paths, timestamp, session_id)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![wsid, paths, format!("2026-06-09 0{i}:00:00"), sid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO items (item_id, workspace_id, kind, active) VALUES (?1, ?2, 'Editor', 1)",
                rusqlite::params![item_id, wsid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO editors (item_id, workspace_id, path) VALUES (?1, ?2, ?3)",
                rusqlite::params![item_id, wsid, active.as_bytes()],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn current_session_picks_most_recent_non_empty() {
        let conn = db_with(&[
            (1, "sess-old", "/a", "/a/x.rs"),
            (2, "sess-new", "/b", "/b/y.rs"),
        ]);
        assert_eq!(current_session(&conn).unwrap().as_deref(), Some("sess-new"));
    }

    #[test]
    fn current_session_none_when_all_empty() {
        let conn = db_with(&[(1, "", "/a", "/a/x.rs")]);
        assert_eq!(current_session(&conn).unwrap(), None);
    }

    #[test]
    fn exact_cwd_match_returns_active_file() {
        let conn = db_with(&[(1, "S", "/Users/me/proj", "/Users/me/proj/main.rs")]);
        let got = active_file_for_cwd(&conn, Path::new("/Users/me/proj")).unwrap();
        assert_eq!(got, Some(PathBuf::from("/Users/me/proj/main.rs")));
    }

    #[test]
    fn cwd_under_worktree_matches_parent() {
        let conn = db_with(&[(1, "S", "/Users/me/proj", "/Users/me/proj/sub/a.rs")]);
        let got = active_file_for_cwd(&conn, Path::new("/Users/me/proj/sub/deep")).unwrap();
        assert_eq!(got, Some(PathBuf::from("/Users/me/proj/sub/a.rs")));
    }

    #[test]
    fn nested_worktrees_longest_prefix_wins() {
        let conn = db_with(&[
            (1, "S", "/a", "/a/outer.rs"),
            (2, "S", "/a/inner", "/a/inner/deep.rs"),
        ]);
        let got = active_file_for_cwd(&conn, Path::new("/a/inner/x")).unwrap();
        assert_eq!(got, Some(PathBuf::from("/a/inner/deep.rs")));
    }

    #[test]
    fn sibling_prefix_does_not_false_match() {
        // cwd "/a/bc" must NOT match worktree "/a/b".
        let conn = db_with(&[(1, "S", "/a/b", "/a/b/x.rs")]);
        let got = active_file_for_cwd(&conn, Path::new("/a/bc")).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn worktree_from_other_session_is_ignored() {
        // Current session = "S2" (more recent). cwd matches a workspace that
        // belongs to the OLD session "S1" only -> no match.
        let conn = db_with(&[
            (1, "S1", "/closed/proj", "/closed/proj/old.rs"),
            (2, "S2", "/open/proj", "/open/proj/new.rs"),
        ]);
        assert_eq!(
            active_file_for_cwd(&conn, Path::new("/closed/proj")).unwrap(),
            None
        );
        assert_eq!(
            active_file_for_cwd(&conn, Path::new("/open/proj")).unwrap(),
            Some(PathBuf::from("/open/proj/new.rs"))
        );
    }

    #[test]
    fn no_match_returns_none() {
        let conn = db_with(&[(1, "S", "/a", "/a/x.rs")]);
        assert_eq!(active_file_for_cwd(&conn, Path::new("/z")).unwrap(), None);
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p zed-claude-bridge zed_watch::query`
Expected: 8 tests PASS.

- [ ] **Step 4: clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/zed-claude-bridge/src/zed_watch/query.rs crates/zed-claude-bridge/src/zed_watch/mod.rs
git commit -m "feat(zed_watch): cwd→active-file query with session + longest-prefix matching"
```

---

## Task 5: `watcher` — push active files into EditorState + notifications

**Files:**
- Create: `crates/zed-claude-bridge/src/zed_watch/watcher.rs`
- Modify: `crates/zed-claude-bridge/src/zed_watch/mod.rs` (add `pub mod watcher;`)

This task has the most moving parts. We split it: first a pure "compute what to push" function (unit-testable without files/sockets), then the I/O driver around it.

- [ ] **Step 1: Declare the submodule**

In `mod.rs`, add after `pub mod query;`:

```rust
pub mod watcher;
```

- [ ] **Step 2: Write the failing test for the pure push-builder**

Create `crates/zed-claude-bridge/src/zed_watch/watcher.rs`:

```rust
//! Background watcher: on each Zed DB change, refresh every connected Claude
//! session's active file and push it through EditorState + a directed
//! `selection_changed` notification.
//!
//! Split into a pure `build_active_editor` / push-decision layer (unit-tested)
//! and the I/O driver `run` (notify-watch + debounce + per-session dedup).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::mcp::EditorState;
use crate::mcp::state::StoredSelection;
use crate::protocol::{Notification as JsonRpcNotification, OpenEditor, Position, Selection, SelectionChangedParams};
use crate::transport::registry::{ClientId, ClientRegistry};
use crate::zed_watch::query;

/// Debounce window after a WAL change before we query (WAL is written often).
const DEBOUNCE: Duration = Duration::from_millis(400);
/// Poll interval used as a fallback when the file watcher can't be installed.
const POLL_FALLBACK: Duration = Duration::from_secs(2);
/// Per-client outbound send timeout (mirrors ipc::server's policy).
const SEND_TIMEOUT: Duration = Duration::from_millis(50);

/// Build the `OpenEditor` + empty `StoredSelection` representing `file` as the
/// active editor. The selection is empty (we only convey the file path; MVP).
pub fn build_active_editor(file: &Path) -> (OpenEditor, StoredSelection) {
    let path_str = file.to_string_lossy().to_string();
    let url = format!("file://{path_str}");
    let language_id = file
        .extension()
        .and_then(|e| e.to_str())
        .map(language_for_extension);

    let editor = OpenEditor {
        uri: url.clone(),
        is_active: true,
        is_pinned: false,
        is_preview: false,
        is_dirty: None,
        language_id,
    };
    let selection = StoredSelection {
        text: String::new(),
        file_path: path_str,
        file_url: url,
        selection: Selection {
            start: Position { line: 0, character: 0 },
            end: Position { line: 0, character: 0 },
            is_empty: true,
        },
    };
    (editor, selection)
}

/// Map a file extension to a best-effort MCP `languageId`. Unknown extensions
/// fall back to the extension itself, matching VSCode's permissive behaviour.
fn language_for_extension(ext: &str) -> String {
    match ext {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "md" => "markdown",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        other => other,
    }
    .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests unwrap on assertion failures")]
mod tests {
    use super::*;

    #[test]
    fn build_active_editor_sets_active_and_empty_selection() {
        let (editor, sel) = build_active_editor(Path::new("/p/main.rs"));
        assert_eq!(editor.uri, "file:///p/main.rs");
        assert!(editor.is_active);
        assert_eq!(editor.language_id.as_deref(), Some("rust"));
        assert_eq!(sel.file_path, "/p/main.rs");
        assert_eq!(sel.file_url, "file:///p/main.rs");
        assert!(sel.selection.is_empty);
        assert_eq!(sel.text, "");
    }

    #[test]
    fn unknown_extension_falls_back_to_extension() {
        let (editor, _) = build_active_editor(Path::new("/p/x.zig"));
        assert_eq!(editor.language_id.as_deref(), Some("zig"));
    }
}
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p zed-claude-bridge zed_watch::watcher`
Expected: 2 tests PASS.

- [ ] **Step 4: Add the per-session refresh function (with dedup state)**

Append to `watcher.rs` (before the `#[cfg(test)]` block):

```rust
/// Tracks the last active file pushed per client, so we only push on change.
#[derive(Default)]
struct PushState {
    last: HashMap<ClientId, PathBuf>,
}

/// Run one refresh cycle: for each connected client with a usable cwd, query
/// its active file; if it changed since the last push, update EditorState and
/// send a directed `selection_changed`.
///
/// `daemon_workspace` is the sidecar's own `--workspace` value; a client whose
/// `workspace_root` equals it (i.e. no more-specific cwd was resolved) is
/// skipped to avoid false matches on an over-broad root like `$HOME`.
async fn refresh_once(
    conn: &rusqlite::Connection,
    registry: &ClientRegistry,
    state: &Arc<RwLock<EditorState>>,
    daemon_workspace: &Path,
    push_state: &mut PushState,
) {
    let clients = registry.snapshot().await;
    for client in &clients {
        let Some(cwd) = client.workspace_root.as_deref() else {
            continue;
        };
        // Skip the over-broad default: if the client's cwd is just the daemon
        // workspace (e.g. $HOME under the LaunchAgent), we can't disambiguate.
        if cwd == daemon_workspace {
            debug!(client_id = %client.id, "client cwd equals daemon workspace; skipping active-file push");
            continue;
        }
        let active = match query::active_file_for_cwd(conn, cwd) {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, client_id = %client.id, "active-file query failed; skipping this client");
                continue;
            }
        };
        // Dedup: only push when this client's active file changed.
        if push_state.last.get(&client.id) == Some(&active) {
            continue;
        }
        push_state.last.insert(client.id, active.clone());

        let (editor, selection) = build_active_editor(&active);
        {
            let mut s = state.write().await;
            s.set_open_editors(vec![editor]);
            s.apply_selection(selection.clone());
        }
        let params = SelectionChangedParams {
            text: selection.text,
            file_path: selection.file_path,
            file_url: selection.file_url,
            selection: selection.selection,
        };
        let payload = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let notif = JsonRpcNotification::new("selection_changed", payload);
        match tokio::time::timeout(SEND_TIMEOUT, client.tx.send(notif)).await {
            Ok(Ok(())) => {
                debug!(client_id = %client.id, file = %active.display(), "pushed active file");
            }
            Ok(Err(_)) => debug!(client_id = %client.id, "client receiver gone; active-file push dropped"),
            Err(_) => warn!(client_id = %client.id, "active-file push timed out; dropped for this client"),
        }
    }
    // Drop dedup entries for clients that have disconnected.
    let live: std::collections::HashSet<ClientId> = clients.iter().map(|c| c.id).collect();
    push_state.last.retain(|id, _| live.contains(id));
}
```

- [ ] **Step 5: Write the failing integration-style test for `refresh_once`**

Still in `watcher.rs`, add these tests inside the existing `mod tests` block (append after the two existing tests). They use a real in-memory SQLite + a real `ClientRegistry`:

```rust
    use crate::transport::registry::{ClientHandle, CLIENT_CHANNEL_CAPACITY};
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    fn seed_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY, paths TEXT,
                 timestamp TEXT, session_id TEXT);
             CREATE TABLE items (item_id INTEGER, workspace_id INTEGER, kind TEXT, active INTEGER);
             CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB);
             INSERT INTO workspaces VALUES (1, '/proj', '2026-06-09 01:00:00', 'S');
             INSERT INTO items VALUES (1, 1, 'Editor', 1);
             INSERT INTO editors VALUES (1, 1, CAST('/proj/main.rs' AS BLOB));",
        )
        .unwrap();
        conn
    }

    async fn register(reg: &ClientRegistry, cwd: &str) -> (ClientId, mpsc::Receiver<JsonRpcNotification>) {
        let (tx, rx) = mpsc::channel(CLIENT_CHANNEL_CAPACITY);
        let now = Instant::now();
        let h = ClientHandle {
            id: ClientId::new(),
            tx,
            workspace_root: Some(PathBuf::from(cwd)),
            last_activity: now,
            connected_at: now,
        };
        let id = h.id;
        reg.insert(h).await;
        (id, rx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_pushes_active_file_to_matching_client() {
        let conn = seed_db();
        let reg = ClientRegistry::new();
        let (_id, mut rx) = register(&reg, "/proj").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();

        refresh_once(&conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;

        // EditorState updated.
        let s = state.read().await;
        assert_eq!(s.open_editors().len(), 1);
        assert_eq!(s.open_editors()[0].uri, "file:///proj/main.rs");
        drop(s);
        // Notification delivered.
        let notif = rx.try_recv().expect("a selection_changed should be queued");
        assert_eq!(notif.method, "selection_changed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_dedups_unchanged_active_file() {
        let conn = seed_db();
        let reg = ClientRegistry::new();
        let (_id, mut rx) = register(&reg, "/proj").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();

        refresh_once(&conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        let _ = rx.try_recv().expect("first push");
        // Second cycle with no DB change -> no new push.
        refresh_once(&conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        assert!(rx.try_recv().is_err(), "no second push for unchanged file");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_skips_client_with_daemon_workspace_cwd() {
        let conn = seed_db();
        let reg = ClientRegistry::new();
        // Client cwd == daemon workspace -> skipped.
        let (_id, mut rx) = register(&reg, "/daemon-ws").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();

        refresh_once(&conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        assert!(rx.try_recv().is_err(), "over-broad cwd client must be skipped");
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p zed-claude-bridge zed_watch::watcher`
Expected: 5 tests PASS (2 pure + 3 refresh).

- [ ] **Step 7: Add the `run` I/O driver**

Append to `watcher.rs` (before the `#[cfg(test)]` block). This opens the DB read-only, probes schema, installs the notify watcher with a debounce, and falls back to polling:

```rust
use crate::zed_watch::{schema_probe, ZedWatchError, WatchConfig};

/// Open `db.sqlite` read-only. WAL-mode readers don't block Zed's writes.
fn open_ro(db_path: &Path) -> Result<rusqlite::Connection, ZedWatchError> {
    use rusqlite::OpenFlags;
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    Ok(conn)
}

/// Start the watcher. Returns `Err` (logged-and-disabled by caller) if the DB
/// can't be located/opened or the schema doesn't match. On success this runs
/// until the task is aborted at shutdown.
pub async fn run(
    config: WatchConfig,
    registry: ClientRegistry,
    state: Arc<RwLock<EditorState>>,
    daemon_workspace: PathBuf,
) -> Result<(), ZedWatchError> {
    let db_path = match config.db_path {
        Some(p) => p,
        None => crate::zed_watch::db_path::locate()?,
    };
    info!(db = %db_path.display(), "zed active-file watcher starting");

    // Probe once up front; bail (feature disabled) on mismatch.
    {
        let conn = open_ro(&db_path)?;
        schema_probe::probe(&conn)?;
    }

    // notify watches the WAL file; if install fails we poll instead.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(8);
    let wal_path = wal_sibling(&db_path);
    let _watcher_guard = install_watcher(&wal_path, tx.clone());
    if _watcher_guard.is_none() {
        warn!("file watcher unavailable; falling back to polling");
    }

    let mut push_state = PushState::default();
    loop {
        // Wait for either a change signal (debounced) or the poll timeout.
        let changed = tokio::select! {
            v = rx.recv() => v.is_some(),
            _ = tokio::time::sleep(POLL_FALLBACK) => false,
        };
        if changed {
            // Coalesce a burst of WAL writes into one refresh.
            tokio::time::sleep(DEBOUNCE).await;
            while rx.try_recv().is_ok() {}
        }
        match open_ro(&db_path) {
            Ok(conn) => {
                refresh_once(&conn, &registry, &state, &daemon_workspace, &mut push_state).await;
            }
            Err(e) => warn!(error = %e, "reopening Zed DB failed; will retry"),
        }
    }
}

/// `db.sqlite` -> `db.sqlite-wal` sibling path.
fn wal_sibling(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

/// Install a `notify` recommended watcher on `wal_path`, forwarding raw events
/// as unit signals into `tx`. Returns the watcher guard (kept alive by the
/// caller) or `None` if installation failed.
fn install_watcher(
    wal_path: &Path,
    tx: tokio::sync::mpsc::Sender<()>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.try_send(());
        }
    })
    .ok()?;
    // Watch the parent dir: WAL files are recreated, which a file-level watch
    // can miss after checkpoint. Directory watch survives recreation.
    let dir = wal_path.parent()?;
    watcher.watch(dir, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}
```

> Note on `JsonRpcNotification`: `protocol::Notification` has `pub method: String` and `pub params: Option<Value>` (confirmed `protocol.rs:83-91`). `Notification::new(method, params: Value)` wraps the `Value` into `Some` internally (matches `ipc/server.rs:451` usage). The test in Step 5 reads `notif.method` — valid.

- [ ] **Step 8: Export `run` and helpers from the module**

In `crates/zed-claude-bridge/src/zed_watch/mod.rs`, add after the submodule declarations:

```rust
pub use watcher::run;
```

- [ ] **Step 9: Run all zed_watch tests + clippy**

Run: `cargo test -p zed-claude-bridge zed_watch && cargo clippy --workspace --all-targets -- -D warnings`
Expected: all PASS.

- [ ] **Step 10: Commit**

```bash
git add crates/zed-claude-bridge/src/zed_watch/watcher.rs crates/zed-claude-bridge/src/zed_watch/mod.rs
git commit -m "feat(zed_watch): watcher pushes per-session active file via debounced WAL watch"
```

---

## Task 6: CLI flags

**Files:**
- Modify: `crates/zed-claude-bridge/src/app/cli.rs:48-75` (the `DaemonArgs` struct)

- [ ] **Step 1: Add the three flags to `DaemonArgs`**

In `crates/zed-claude-bridge/src/app/cli.rs`, add these fields inside `pub struct DaemonArgs` (after the `lock_dir` field, before the closing `}`):

```rust
    /// Watch Zed's local SQLite state to auto-detect each Claude session's
    /// active file (pushed via `getOpenEditors` / `getCurrentSelection`).
    /// On by default; disable with `--no-watch-zed-db`.
    #[arg(long = "watch-zed-db", default_value_t = true, action = clap::ArgAction::Set)]
    pub watch_zed_db: bool,

    /// Override the auto-detected path to Zed's `db.sqlite`. Mainly for tests
    /// and non-standard installs.
    #[arg(long, value_name = "PATH")]
    pub zed_db_path: Option<PathBuf>,
```

> `--no-watch-zed-db` works automatically: with `action = Set` and a bool, clap accepts `--watch-zed-db=false`. To get the literal `--no-watch-zed-db` spelling, we instead use a negation flag. Replace the `watch_zed_db` field above with this pair if the `--no-` spelling is required:
>
> ```rust
>     /// Disable the Zed active-file watcher (on by default).
>     #[arg(long = "no-watch-zed-db", default_value_t = false)]
>     pub no_watch_zed_db: bool,
> ```
>
> Then compute `watch_zed_db = !args.no_watch_zed_db` in lifecycle. **Choose the negation-flag form** (it gives the `--no-watch-zed-db` spelling the design specifies). Use `no_watch_zed_db: bool` as the actual field and delete the `watch_zed_db` field.

- [ ] **Step 2: Settle on the field (negation flag)**

Final `DaemonArgs` additions (this is what to actually write — the two fields):

```rust
    /// Disable the Zed active-file watcher. The watcher is ON by default;
    /// pass `--no-watch-zed-db` to turn it off.
    #[arg(long = "no-watch-zed-db", default_value_t = false)]
    pub no_watch_zed_db: bool,

    /// Override the auto-detected path to Zed's `db.sqlite`. Mainly for tests
    /// and non-standard installs.
    #[arg(long, value_name = "PATH")]
    pub zed_db_path: Option<PathBuf>,
```

- [ ] **Step 3: Verify the CLI parses**

Run: `cargo build -p zed-claude-bridge && ./target/debug/zed-claude-bridge --help 2>&1 | grep -E "no-watch-zed-db|zed-db-path"`
Expected: both flags appear in help output.

- [ ] **Step 4: Run existing CLI tests**

Run: `cargo test -p zed-claude-bridge app::cli`
Expected: existing CLI tests still PASS (new optional flags don't break them).

- [ ] **Step 5: Commit**

```bash
git add crates/zed-claude-bridge/src/app/cli.rs
git commit -m "feat(cli): add --no-watch-zed-db and --zed-db-path daemon flags"
```

---

## Task 7: Wire the watcher into `run_daemon`

**Files:**
- Modify: `crates/zed-claude-bridge/src/app/lifecycle.rs:118-131` (after IPC server build, alongside the accept-loop spawns)

- [ ] **Step 1: Import the watcher pieces**

In `crates/zed-claude-bridge/src/app/lifecycle.rs`, add to the imports near the top (after `use crate::transport::...`):

```rust
use crate::zed_watch::{self, WatchConfig};
```

- [ ] **Step 2: Spawn the watcher task after the IPC accept loop spawn**

In `run_daemon`, immediately after the `ipc_handle = tokio::spawn(...)` block (around line 131) and before "8. Wait for shutdown", insert:

```rust
    // 7b. Optionally start the Zed active-file watcher. Failure to start
    //     (DB not found / schema mismatch / non-macOS) disables the feature
    //     with a WARN but never aborts the sidecar.
    let watch_handle = if args.no_watch_zed_db {
        info!("zed active-file watcher disabled via --no-watch-zed-db");
        None
    } else {
        let config = WatchConfig {
            db_path: args.zed_db_path.clone(),
        };
        let registry_for_watch = registry.clone();
        let state_for_watch = state_for_watch_clone.clone();
        let daemon_ws = workspace.clone();
        Some(tokio::spawn(async move {
            if let Err(e) =
                zed_watch::run(config, registry_for_watch, state_for_watch, daemon_ws).await
            {
                warn!(error = %e, "zed active-file watcher disabled");
            }
        }))
    };
```

- [ ] **Step 3: Provide the `state` clone the watcher needs**

The existing code moves `state` into `IpcServer::new(state, registry)` at line 118. The watcher also needs `state`. Clone it before that move. Change line 99 area and line 118:

Find (around line 99):
```rust
    let state = Arc::new(RwLock::new(EditorState::new()));
```
Leave as is, and find (around line 118):
```rust
    let ipc_server = IpcServer::new(state, registry);
```
Replace with:
```rust
    let state_for_watch_clone = state.clone();
    let ipc_server = IpcServer::new(state, registry.clone());
```

> `registry` is also used by the watcher (Step 2 clones it). It was previously moved into `IpcServer::new`. Cloning it here (`registry.clone()`) keeps the original available for the watcher spawn. `ClientRegistry` is `Clone` (Arc-backed) — confirmed in `registry.rs:148`.

- [ ] **Step 4: Abort the watcher on shutdown**

In the shutdown section (around line 138, where `ws_handle.abort()` and `ipc_handle.abort()` are called), add after them:

```rust
    if let Some(h) = watch_handle {
        h.abort();
    }
```

- [ ] **Step 5: Build and run existing tests**

Run: `cargo build -p zed-claude-bridge && cargo test -p zed-claude-bridge`
Expected: PASS (no regressions; watcher wiring compiles).

- [ ] **Step 6: clippy + fmt**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/zed-claude-bridge/src/app/lifecycle.rs
git commit -m "feat(app): start Zed active-file watcher in daemon mode (graceful disable on failure)"
```

---

## Task 8: End-to-end integration test

**Files:**
- Create: `crates/zed-claude-bridge/tests/zed_watch.rs`

- [ ] **Step 1: Write the integration test**

Create `crates/zed-claude-bridge/tests/zed_watch.rs`. It builds a real on-disk SQLite DB, points the watcher's query layer at it, registers a fake client in a real `ClientRegistry`, and asserts the active file flows into `EditorState` and a notification is queued. This exercises `open_ro` → `schema_probe` → `query` → `refresh` path against a real file (not in-memory), which the unit tests don't cover.

```rust
//! End-to-end: a real on-disk Zed-shaped SQLite DB → watcher refresh →
//! EditorState updated + selection_changed queued for the matching client.
//!
//! We call the crate's public query + a locally-reconstructed refresh rather
//! than the private `refresh_once`, exercising the read-only open path against
//! a real file. The watcher's `run` loop itself (notify/debounce) is covered
//! by manual smoke testing; here we verify the data path deterministically.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::time::Instant;

use zed_claude_bridge::mcp::EditorState;
use zed_claude_bridge::protocol::Notification as JsonRpcNotification;
use zed_claude_bridge::transport::registry::{
    CLIENT_CHANNEL_CAPACITY, ClientHandle, ClientId, ClientRegistry,
};
use zed_claude_bridge::zed_watch::{query, schema_probe};

fn build_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY, paths TEXT,
             timestamp TEXT, session_id TEXT);
         CREATE TABLE items (item_id INTEGER, workspace_id INTEGER, kind TEXT, active INTEGER);
         CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB);
         INSERT INTO workspaces VALUES (1, '/proj', '2026-06-09 01:00:00', 'S');
         INSERT INTO items VALUES (1, 1, 'Editor', 1);
         INSERT INTO editors VALUES (1, 1, CAST('/proj/active.rs' AS BLOB));",
    )
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn read_only_query_against_real_file_finds_active() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("db.sqlite");
    build_db(&db);

    // Read-only open + schema probe both succeed against the real file.
    let conn = Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    schema_probe::probe(&conn).unwrap();

    let got = query::active_file_for_cwd(&conn, Path::new("/proj")).unwrap();
    assert_eq!(got, Some(PathBuf::from("/proj/active.rs")));
}

#[tokio::test(flavor = "current_thread")]
async fn registry_client_receives_active_file_state() {
    // This mirrors the watcher's refresh: query -> EditorState -> notify.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("db.sqlite");
    build_db(&db);

    let reg = ClientRegistry::new();
    let (tx, mut rx) = mpsc::channel::<JsonRpcNotification>(CLIENT_CHANNEL_CAPACITY);
    let now = Instant::now();
    let id = ClientId::new();
    reg.insert(ClientHandle {
        id,
        tx,
        workspace_root: Some(PathBuf::from("/proj")),
        last_activity: now,
        connected_at: now,
    })
    .await;

    let state = Arc::new(RwLock::new(EditorState::new()));

    // Drive the same logic the watcher does, via the public query API.
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let clients = reg.snapshot().await;
    let client = &clients[0];
    let cwd = client.workspace_root.clone().unwrap();
    let active = query::active_file_for_cwd(&conn, &cwd).unwrap().unwrap();
    assert_eq!(active, PathBuf::from("/proj/active.rs"));

    // Simulate the EditorState write + notification the watcher performs.
    {
        let mut s = state.write().await;
        s.set_open_editors(vec![zed_claude_bridge::protocol::OpenEditor {
            uri: format!("file://{}", active.display()),
            is_active: true,
            is_pinned: false,
            is_preview: false,
            is_dirty: None,
            language_id: Some("rust".to_string()),
        }]);
    }
    let s = state.read().await;
    assert_eq!(s.open_editors().len(), 1);
    assert!(s.open_editors()[0].is_active);
    drop(s);

    let notif = JsonRpcNotification::new("selection_changed", serde_json::Value::Null);
    client.tx.send(notif).await.unwrap();
    let received = rx.recv().await.unwrap();
    assert_eq!(received.method, "selection_changed");
}
```

> If `JsonRpcNotification`'s field is not named `method`, or `OpenEditor` is not re-exported from `zed_claude_bridge::protocol`, adjust imports/asserts. Both are confirmed: `protocol::Notification.method` (`protocol.rs:83`) and `protocol::OpenEditor` (`protocol.rs:373`) are `pub`.

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p zed-claude-bridge --test zed_watch`
Expected: 2 tests PASS.

- [ ] **Step 3: Full suite + clippy + fmt**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all PASS (existing 185 tests + the new ~20 zed_watch tests).

- [ ] **Step 4: Commit**

```bash
git add crates/zed-claude-bridge/tests/zed_watch.rs
git commit -m "test(zed_watch): end-to-end active-file flow against a real on-disk DB"
```

---

## Task 9: Manual smoke + docs

**Files:**
- Modify: `README.md` (document the watcher under a new "Active-file awareness" subsection)
- Modify: `docs/protocol.md` §9 (note the new internal source of `selection_changed` / `open_editors`)

- [ ] **Step 1: Manual smoke test (real Zed + real Claude)**

With Zed open on this repo and a `claude /ide` session connected:

```bash
# Rebuild + reinstall the sidecar with the watcher.
cargo install --path crates/zed-claude-bridge
# Restart the LaunchAgent (or the foreground sidecar) so the new binary runs.
./scripts/uninstall-launchd.sh && ./scripts/install-launchd.sh
# Tail logs and confirm the watcher started + pushes on file switch.
tail -f ~/Library/Logs/zed-claude-bridge.log | grep -E "active-file|watcher"
```

In Zed, switch the active file. Within ~1s you should see a `pushed active file` DEBUG line (run the sidecar with `--log-level debug` to see it). In the Claude session, `getCurrentSelection` / `getOpenEditors` should now report the file you switched to.

Expected: log shows `zed active-file watcher starting` then `pushed active file file=<the file you switched to>`.

- [ ] **Step 2: Document in README**

In `README.md`, add a new subsection after "Usage: send a selection from Zed to Claude Code" titled `### Active-file awareness (automatic)`. Content:

```markdown
### Active-file awareness (automatic)

Beyond the explicit `cmd-ctrl-c` at-mention, the sidecar can keep Claude
aware of *which file you're currently editing* — no keypress required,
matching the JetBrains plugin's behaviour.

This is on by default. The sidecar watches Zed's local SQLite state
(`~/Library/Application Support/Zed/db/<channel>/db.sqlite`) and, whenever
the active editor changes, resolves each connected Claude session's active
file by matching the session's working directory to the open Zed worktree.
The result flows into the same `getOpenEditors` / `getCurrentSelection`
MCP tools Claude already reads.

- **Latency:** near-real-time (Zed flushes state to SQLite within ~1s; the
  watcher debounces ~400ms on top).
- **Scope:** the active file only (not every open tab).
- **Multi-session:** each Claude session is matched independently by its
  own cwd, so two sessions in different projects each see their own file.
- **Disable:** pass `--no-watch-zed-db` to the sidecar.
- **Robustness:** if Zed's DB schema changes in a future version, the
  watcher disables itself with a WARN and the rest of the sidecar
  (at-mentions, `/ide` discovery) is unaffected.

> This reads Zed's private SQLite schema, which is an implementation detail
> rather than a public API. A schema probe at startup guards against Zed
> version changes.
```

- [ ] **Step 3: Note the new notification source in protocol.md**

In `docs/protocol.md` §9, append a short paragraph:

```markdown
**Internal source: active-file watcher (Zed-sidecar extension).** In addition
to IPC-frame-driven `selection_changed` notifications (from the `cmd-ctrl-c`
task), the sidecar may emit `selection_changed` / refresh `getOpenEditors`
from its Zed SQLite watcher (`src/zed_watch/`). These notifications carry an
empty selection (`isEmpty: true`, no text) and convey only the active file
path. They are routed directly to the single Claude session whose cwd matches
the file's worktree — never broadcast. Upstream Claude Code sees a wire shape
identical to §3.3.
```

- [ ] **Step 4: Commit**

```bash
git add README.md docs/protocol.md
git commit -m "docs: document automatic Zed active-file awareness"
```

---

## Self-Review Notes

**Spec coverage** (design doc → task):
- §3 judgement chain (session_id + longest-prefix cwd match) → Task 4 (`query.rs`).
- §4 数据流 / EditorState write + selection_changed push → Task 5 (`watcher.rs`).
- §4 组件 db_path/schema_probe/query/watcher → Tasks 2–5.
- §4 决策 1 (rusqlite read-only bundled) → Task 1 + Task 5 `open_ro`.
- §4 决策 2 (notify + debounce + poll fallback) → Task 5 `run`/`install_watcher`.
- §4 决策 3 (per-session dedup) → Task 5 `PushState`.
- §4 决策 4 (cwd source + $HOME skip) → Task 5 `refresh_once` daemon-workspace skip.
- §4 决策 5 (empty selection MVP) → Task 5 `build_active_editor`.
- §4 版本探测 + 优雅降级 → Task 3 + Task 7 graceful-disable wiring.
- §4 CLI flags → Task 6.
- §5 error_handling/logging/no-unsafe → enforced throughout (thiserror in mod.rs, tracing, no unsafe).
- §6 test strategy (unit per module + integration, fixture DB, macOS CI) → Tasks 2–5 inline tests + Task 8.
- §7 non-goals respected (active file only, no focus/title, no Linux path, no selection text).

**Placeholder scan:** No TBD/TODO. Every code step shows complete code. Task 6's negation-flag ambiguity is resolved explicitly in Step 2 ("this is what to actually write").

**Type consistency:** `active_file_for_cwd(conn, &Path) -> Result<Option<PathBuf>, ZedWatchError>` used identically in Tasks 4, 5, 8. `build_active_editor(&Path) -> (OpenEditor, StoredSelection)` consistent. `refresh_once(conn, registry, state, daemon_workspace, push_state)` signature stable across Task 5 steps. `WatchConfig { db_path: Option<PathBuf> }` consistent between mod.rs (Task 2), watcher `run` (Task 5), lifecycle (Task 7). `JsonRpcNotification::new(method, params)` matches `protocol.rs:143`. Field `Notification.method` confirmed public.
