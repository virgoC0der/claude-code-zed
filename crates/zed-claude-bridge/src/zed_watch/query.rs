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

/// The active editor in the worktree matching a session cwd: its path, the
/// primary selection's UTF-8 byte-offset range (cursor when start == end),
/// and the persisted unsaved buffer text when the editor is dirty.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveEditor {
    /// Absolute file path stored by Zed.
    pub path: PathBuf,
    /// `(start, end)` UTF-8 byte offsets from `editor_selections`; `None`
    /// when no selection row is persisted (v1 empty-selection behaviour).
    pub selection: Option<(u64, u64)>,
    /// `editors.contents` (BLOB, lossy UTF-8) when non-NULL — the text basis
    /// for offset conversion on dirty buffers.
    pub unsaved_contents: Option<String>,
}

/// Full active-editor lookup. See [`ActiveEditor`].
///
/// Candidate worktrees in the current session are scanned longest paths first
/// so the most specific (nested) worktree wins. We match in Rust rather than
/// SQL so the prefix test is path-component-aware (avoids `/a/b` matching
/// `/a/bc`). Returns `Ok(None)` when no open worktree in the current session
/// contains `cwd`, or the matched worktree has no active editor.
pub fn active_editor_for_cwd(
    conn: &Connection,
    cwd: &Path,
) -> Result<Option<ActiveEditor>, ZedWatchError> {
    let Some(session) = current_session(conn)? else {
        return Ok(None);
    };
    let cwd_str = cwd.to_string_lossy().to_string();

    let mut stmt = conn.prepare(
        "SELECT w.paths, e.path, s.start, s.\"end\", e.contents
         FROM workspaces w
         JOIN items   i ON i.workspace_id = w.workspace_id AND i.active = 1 AND i.kind = 'Editor'
         JOIN editors e ON e.item_id = i.item_id AND e.workspace_id = w.workspace_id
         LEFT JOIN editor_selections s
             ON s.editor_id = i.item_id AND s.workspace_id = i.workspace_id
         WHERE w.session_id = ?1
         ORDER BY length(w.paths) DESC",
    )?;
    let rows = stmt.query_map([&session], |row| {
        let paths: Option<String> = row.get(0)?;
        // editors.path / editors.contents are BLOBs; read as bytes and decode
        // lossily. A TEXT-literal SQL comparison would silently match nothing.
        let active: Vec<u8> = row.get(1)?;
        let start: Option<i64> = row.get(2)?;
        let end: Option<i64> = row.get(3)?;
        let contents: Option<Vec<u8>> = row.get(4)?;
        Ok((paths, active, start, end, contents))
    })?;

    for row in rows {
        let (paths, active_bytes, start, end, contents) = row?;
        let Some(paths) = paths else { continue };
        if cwd_matches_worktree(&cwd_str, &paths) {
            let active = String::from_utf8_lossy(&active_bytes).to_string();
            if active.is_empty() {
                continue;
            }
            let selection = match (start, end) {
                (Some(s), Some(e)) if s >= 0 && e >= 0 => Some((s as u64, e as u64)),
                _ => None,
            };
            let unsaved_contents = contents.map(|c| String::from_utf8_lossy(&c).to_string());
            return Ok(Some(ActiveEditor {
                path: PathBuf::from(active),
                selection,
                unsaved_contents,
            }));
        }
    }
    Ok(None)
}

/// Path-only view of [`active_editor_for_cwd`] (v1 API, kept for callers
/// that don't need selection data).
pub fn active_file_for_cwd(
    conn: &Connection,
    cwd: &Path,
) -> Result<Option<PathBuf>, ZedWatchError> {
    Ok(active_editor_for_cwd(conn, cwd)?.map(|e| e.path))
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
             CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB, contents BLOB);
             CREATE TABLE editor_selections (
                 item_id INTEGER, editor_id INTEGER, workspace_id INTEGER,
                 start INTEGER, \"end\" INTEGER
             );",
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

    /// Add a selection row for the `idx`-th seeded editor (1-based item_id).
    fn add_selection(conn: &Connection, item_id: i64, wsid: i64, start: i64, end: i64) {
        conn.execute(
            "INSERT INTO editor_selections (item_id, editor_id, workspace_id, start, \"end\")
             VALUES (?1, ?1, ?2, ?3, ?4)",
            rusqlite::params![item_id, wsid, start, end],
        )
        .unwrap();
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

    #[test]
    fn active_editor_carries_selection_offsets() {
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        add_selection(&conn, 1, 1, 5, 12);
        let e = active_editor_for_cwd(&conn, Path::new("/p"))
            .unwrap()
            .unwrap();
        assert_eq!(e.path, PathBuf::from("/p/main.rs"));
        assert_eq!(e.selection, Some((5, 12)));
        assert_eq!(e.unsaved_contents, None);
    }

    #[test]
    fn active_editor_without_selection_row_has_none() {
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        let e = active_editor_for_cwd(&conn, Path::new("/p"))
            .unwrap()
            .unwrap();
        assert_eq!(e.selection, None, "LEFT JOIN must still surface the file");
    }

    #[test]
    fn active_editor_reads_blob_contents_as_unsaved_text() {
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        conn.execute(
            "UPDATE editors SET contents = CAST('dirty buffer text' AS BLOB) WHERE item_id = 1",
            [],
        )
        .unwrap();
        let e = active_editor_for_cwd(&conn, Path::new("/p"))
            .unwrap()
            .unwrap();
        assert_eq!(e.unsaved_contents.as_deref(), Some("dirty buffer text"));
    }

    #[test]
    fn blob_path_never_matches_sql_text_literal_regression() {
        // Lesson from live calibration: editors.path is BLOB; a text-literal
        // WHERE clause silently matches nothing. Guard the lesson.
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM editors WHERE path = '/p/main.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "BLOB column must not equal a TEXT literal");
        // ...while our byte-reading query DOES find it:
        assert!(
            active_editor_for_cwd(&conn, Path::new("/p"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn active_file_wrapper_still_returns_path_only() {
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        assert_eq!(
            active_file_for_cwd(&conn, Path::new("/p")).unwrap(),
            Some(PathBuf::from("/p/main.rs"))
        );
    }
}
