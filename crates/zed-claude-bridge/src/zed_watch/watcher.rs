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
use crate::protocol::{
    Notification as JsonRpcNotification, OpenEditor, Position, Selection, SelectionChangedParams,
};
use crate::transport::registry::{ClientId, ClientRegistry};
use crate::zed_watch::query;

/// Debounce window after a WAL change before we query (WAL is written often).
const DEBOUNCE: Duration = Duration::from_millis(400);
/// Poll interval used as a fallback when the file watcher can't be installed.
const POLL_FALLBACK: Duration = Duration::from_secs(2);
/// Per-client outbound send timeout (mirrors ipc::server's policy).
const SEND_TIMEOUT: Duration = Duration::from_millis(50);

/// 0-indexed wire position of UTF-8 byte offset `off` in `basis`.
/// `character` counts UTF-16 code units from the line start (VSCode
/// semantics, protocol.md §3.3). Caller guarantees `off <= basis.len()`.
fn position_at(basis: &str, off: usize) -> Position {
    let bytes = basis.as_bytes();
    let before = &bytes[..off];
    let line = before.iter().filter(|b| **b == b'\n').count() as u32;
    let line_start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let col_text = String::from_utf8_lossy(&bytes[line_start..off]);
    let character = col_text.encode_utf16().count() as u32;
    Position { line, character }
}

/// Convert a `(start, end)` UTF-8 byte-offset range into a wire `Selection`
/// plus the selected text. Returns `None` when the range is out of bounds or
/// inverted (e.g. the DB and the text basis are momentarily out of sync) —
/// callers degrade to the v1 empty selection.
pub fn selection_from_offsets(basis: &str, start: u64, end: u64) -> Option<(Selection, String)> {
    let (s, e) = (start as usize, end as usize);
    if s > e || e > basis.len() {
        return None;
    }
    let text = String::from_utf8_lossy(&basis.as_bytes()[s..e]).to_string();
    Some((
        Selection {
            start: position_at(basis, s),
            end: position_at(basis, e),
            is_empty: s == e,
        },
        text,
    ))
}

/// Build the `OpenEditor` + `StoredSelection` for `file`. When `selection`
/// carries a converted range + text, the stored selection is real; otherwise
/// it falls back to the v1 empty placeholder (file path only).
pub fn build_active_editor(
    file: &Path,
    selection: Option<(Selection, String)>,
) -> (OpenEditor, StoredSelection) {
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
    let (sel, text) = selection.unwrap_or((
        Selection {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
            is_empty: true,
        },
        String::new(),
    ));
    let stored = StoredSelection {
        text,
        file_path: path_str.clone(),
        file_url: url.clone(),
        selection: sel,
    };
    (editor, stored)
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

/// Tracks the last selection pushed per client, so we only push on change —
/// a re-push happens when the file OR the selection within it changes.
#[derive(Default)]
struct PushState {
    last: HashMap<ClientId, StoredSelection>,
}

/// Run one refresh cycle: for each connected client with a usable cwd, query
/// its active file; if it changed since the last push, update EditorState and
/// send a directed `selection_changed`.
///
/// `daemon_workspace` is the sidecar's own `--workspace` value; a client whose
/// `workspace_root` equals it (i.e. no more-specific cwd was resolved) is
/// skipped to avoid false matches on an over-broad root like `$HOME`.
///
/// Takes `&mut Connection` (not `&Connection`): `rusqlite::Connection` is
/// `Send` but not `Sync`, so a shared reference held across the `.await`s in
/// this fn would make the future non-`Send` and unusable with `tokio::spawn`.
/// A unique reference is `Send` whenever the referent is.
async fn refresh_once(
    conn: &mut rusqlite::Connection,
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
        let editor = match query::active_editor_for_cwd(conn, cwd) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, client_id = %client.id, "active-editor query failed; skipping this client");
                continue;
            }
        };
        let active = editor.path.clone();
        // Selection basis: dirty-buffer contents first, else the on-disk file.
        let converted = match editor.selection {
            Some((s, e)) => {
                let basis = match &editor.unsaved_contents {
                    Some(c) => Some(c.clone()),
                    None => tokio::fs::read_to_string(&active).await.ok(),
                };
                basis.and_then(|b| selection_from_offsets(&b, s, e))
            }
            None => None,
        };

        let (editor, selection) = build_active_editor(&active, converted);
        // Dedup: only push when this client's file or selection changed.
        if push_state.last.get(&client.id) == Some(&selection) {
            continue;
        }
        push_state.last.insert(client.id, selection.clone());
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
            Ok(Err(_)) => {
                debug!(client_id = %client.id, "client receiver gone; active-file push dropped")
            }
            Err(_) => {
                warn!(client_id = %client.id, "active-file push timed out; dropped for this client")
            }
        }
    }
    // Drop dedup entries for clients that have disconnected.
    let live: std::collections::HashSet<ClientId> = clients.iter().map(|c| c.id).collect();
    push_state.last.retain(|id, _| live.contains(id));
}

use crate::zed_watch::{WatchConfig, ZedWatchError, schema_probe};

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
            Ok(mut conn) => {
                refresh_once(
                    &mut conn,
                    &registry,
                    &state,
                    &daemon_workspace,
                    &mut push_state,
                )
                .await;
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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests unwrap on assertion failures")]
mod tests {
    use super::*;

    #[test]
    fn build_active_editor_sets_active_and_empty_selection() {
        let (editor, sel) = build_active_editor(Path::new("/p/main.rs"), None);
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
        let (editor, _) = build_active_editor(Path::new("/p/x.zig"), None);
        assert_eq!(editor.language_id.as_deref(), Some("zig"));
    }

    // ----- position_at / selection_from_offsets ----------------------------

    #[test]
    fn position_at_ascii() {
        let basis = "fn a() {}\nfn main() {}\n";
        // offset 10 = start of line 2 (0-indexed line 1, char 0)
        let p = position_at(basis, 10);
        assert_eq!((p.line, p.character), (1, 0));
        // offset 13 = "main" start: line 1, char 3
        let p = position_at(basis, 13);
        assert_eq!((p.line, p.character), (1, 3));
    }

    #[test]
    fn position_at_multibyte_utf16_columns() {
        // '→' is 3 UTF-8 bytes / 1 UTF-16 unit; '你' is 3 bytes / 1 unit.
        let basis = "a→b你c\nx";
        // byte offset of 'c' = 1 + 3 + 1 + 3 = 8; utf16 col = 4
        let p = position_at(basis, 8);
        assert_eq!((p.line, p.character), (0, 4));
        // byte offset of 'x' = 10 (after \n at 9): line 1 char 0
        let p = position_at(basis, 10);
        assert_eq!((p.line, p.character), (1, 0));
    }

    #[test]
    fn selection_from_offsets_extracts_text_and_flags() {
        let basis = "hello\nworld\n";
        let (sel, text) = selection_from_offsets(basis, 6, 11).expect("in range");
        assert_eq!(text, "world");
        assert!(!sel.is_empty);
        assert_eq!((sel.start.line, sel.start.character), (1, 0));
        assert_eq!((sel.end.line, sel.end.character), (1, 5));
    }

    #[test]
    fn selection_from_offsets_cursor_is_empty() {
        let (sel, text) = selection_from_offsets("abc", 1, 1).expect("in range");
        assert!(sel.is_empty);
        assert_eq!(text, "");
        assert_eq!((sel.start.line, sel.start.character), (0, 1));
    }

    #[test]
    fn selection_from_offsets_out_of_range_degrades_to_none() {
        assert!(selection_from_offsets("abc", 0, 99).is_none());
        assert!(selection_from_offsets("abc", 99, 99).is_none());
        assert!(
            selection_from_offsets("abc", 2, 1).is_none(),
            "inverted range"
        );
    }

    use crate::transport::registry::{CLIENT_CHANNEL_CAPACITY, ClientHandle};
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    fn seed_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
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
             INSERT INTO editors VALUES (1, 1, CAST('/proj/main.rs' AS BLOB), NULL);",
        )
        .unwrap();
        conn
    }

    async fn register(
        reg: &ClientRegistry,
        cwd: &str,
    ) -> (ClientId, mpsc::Receiver<JsonRpcNotification>) {
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
        let mut conn = seed_db();
        let reg = ClientRegistry::new();
        let (_id, mut rx) = register(&reg, "/proj").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();

        refresh_once(&mut conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;

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
        let mut conn = seed_db();
        let reg = ClientRegistry::new();
        let (_id, mut rx) = register(&reg, "/proj").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();

        refresh_once(&mut conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        let _ = rx.try_recv().expect("first push");
        // Second cycle with no DB change -> no new push.
        refresh_once(&mut conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        assert!(rx.try_recv().is_err(), "no second push for unchanged file");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_pushes_real_selection_from_db() {
        let mut conn = seed_db();
        // Selection over bytes 4..9 of the on-disk basis — write a real file
        // so the disk fallback basis exists.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("main.rs");
        std::fs::write(&file, "abc\ndefgh\n").unwrap();
        let file_str = file.to_str().unwrap();
        conn.execute(
            "UPDATE editors SET path = CAST(?1 AS BLOB) WHERE item_id = 1",
            rusqlite::params![file_str],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO editor_selections (item_id, editor_id, workspace_id, start, \"end\")
             VALUES (1, 1, 1, 4, 9)",
            [],
        )
        .unwrap();

        let reg = ClientRegistry::new();
        let (_id, mut rx) = register(&reg, "/proj").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();
        refresh_once(&mut conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;

        let notif = rx.try_recv().expect("selection_changed queued");
        let params = notif.params.expect("params");
        assert_eq!(params["text"], "defgh");
        assert_eq!(params["selection"]["start"]["line"], 1);
        assert_eq!(params["selection"]["start"]["character"], 0);
        assert_eq!(params["selection"]["end"]["character"], 5);
        assert_eq!(params["selection"]["isEmpty"], false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_repushes_when_only_selection_changes() {
        // Same file, selection moves → dedup must NOT swallow the second push.
        let mut conn = seed_db();
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("main.rs");
        std::fs::write(&file, "abc\ndefgh\n").unwrap();
        conn.execute(
            "UPDATE editors SET path = CAST(?1 AS BLOB) WHERE item_id = 1",
            rusqlite::params![file.to_str().unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO editor_selections (item_id, editor_id, workspace_id, start, \"end\")
             VALUES (1, 1, 1, 0, 3)",
            [],
        )
        .unwrap();

        let reg = ClientRegistry::new();
        let (_id, mut rx) = register(&reg, "/proj").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();
        refresh_once(&mut conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        assert!(rx.try_recv().is_ok(), "first push");

        conn.execute("UPDATE editor_selections SET start = 4, \"end\" = 9", [])
            .unwrap();
        refresh_once(&mut conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        let second = rx.try_recv().expect("selection move must re-push");
        assert_eq!(second.params.unwrap()["text"], "defgh");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_skips_client_with_daemon_workspace_cwd() {
        let mut conn = seed_db();
        let reg = ClientRegistry::new();
        // Client cwd == daemon workspace -> skipped.
        let (_id, mut rx) = register(&reg, "/daemon-ws").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();

        refresh_once(&mut conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        assert!(
            rx.try_recv().is_err(),
            "over-broad cwd client must be skipped"
        );
    }
}
