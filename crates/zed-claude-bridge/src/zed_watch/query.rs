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
