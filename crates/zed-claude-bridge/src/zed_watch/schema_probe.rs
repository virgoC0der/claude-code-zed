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
