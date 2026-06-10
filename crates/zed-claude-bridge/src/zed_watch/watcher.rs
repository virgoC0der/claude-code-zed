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

    use crate::transport::registry::{CLIENT_CHANNEL_CAPACITY, ClientHandle};
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
        assert!(
            rx.try_recv().is_err(),
            "over-broad cwd client must be skipped"
        );
    }
}
