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

/// Build a Zed-shaped DB whose active editor points at `active_file`, a REAL
/// on-disk file written with `"hello\nworld\n"` (the disk basis for offset
/// conversion), carrying one selection row over bytes 6..11 (`"world"`).
fn build_db(path: &Path, active_file: &Path) {
    std::fs::write(active_file, "hello\nworld\n").unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY, paths TEXT,
             timestamp TEXT, session_id TEXT);
         CREATE TABLE items (item_id INTEGER, workspace_id INTEGER, kind TEXT, active INTEGER);
         CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB, contents BLOB);
         CREATE TABLE editor_selections (
             item_id INTEGER, editor_id INTEGER, workspace_id INTEGER,
             start INTEGER, \"end\" INTEGER
         );
         INSERT INTO workspaces VALUES (1, '/proj', '2026-06-09 01:00:00', 'S');
         INSERT INTO items VALUES (1, 1, 'Editor', 1);
         INSERT INTO editor_selections (item_id, editor_id, workspace_id, start, \"end\")
             VALUES (1, 1, 1, 6, 11);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO editors (item_id, workspace_id, path, contents)
         VALUES (1, 1, CAST(?1 AS BLOB), NULL)",
        rusqlite::params![active_file.to_str().unwrap()],
    )
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn read_only_query_against_real_file_finds_active() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("db.sqlite");
    let active_file = tmp.path().join("active.rs");
    build_db(&db, &active_file);

    // Read-only open + schema probe both succeed against the real file.
    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    schema_probe::probe(&conn).unwrap();

    let got = query::active_file_for_cwd(&conn, Path::new("/proj")).unwrap();
    assert_eq!(got, Some(active_file));
}

#[tokio::test(flavor = "current_thread")]
async fn registry_client_receives_active_file_state() {
    // This mirrors the watcher's refresh: query -> EditorState -> notify.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("db.sqlite");
    let active_file = tmp.path().join("active.rs");
    build_db(&db, &active_file);

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
    assert_eq!(active, active_file);

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

#[tokio::test(flavor = "current_thread")]
async fn selection_offsets_flow_to_wire_positions() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("db.sqlite");
    let active_file = tmp.path().join("active.rs");
    build_db(&db, &active_file);

    let conn =
        Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let editor = query::active_editor_for_cwd(&conn, Path::new("/proj"))
        .unwrap()
        .unwrap();
    assert_eq!(editor.selection, Some((6, 11)));
    let basis = std::fs::read_to_string(&editor.path).unwrap();
    let (sel, text) =
        zed_claude_bridge::zed_watch::watcher::selection_from_offsets(&basis, 6, 11).unwrap();
    assert_eq!(text, "world");
    assert_eq!((sel.start.line, sel.start.character), (1, 0));
    assert_eq!((sel.end.line, sel.end.character), (1, 5));
}
