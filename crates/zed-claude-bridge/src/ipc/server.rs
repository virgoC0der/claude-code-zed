//! IPC server: accept loop on a `UnixListener`, line-delimited JSON parser,
//! frame dispatch, selection-debounce timer, and dedup.
//!
//! Per the OpenSpec `session-routing` change, outbound notifications are no
//! longer fanned out via a broadcast channel — they are routed to specific
//! WebSocket clients via [`crate::transport::ClientRegistry`] and the
//! [`crate::transport::router`] module's pure decision functions.
//!
//! Frame contract: see `docs/protocol.md` §6 + §9 and the OpenSpec
//! `specs/ipc/spec.md` / `specs/notifications/spec.md`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::mcp::state::{EditorState, StoredSelection};
use crate::protocol::{
    AtMentionedParams, IpcFrame, Notification as JsonRpcNotification, Position, Selection,
    SelectionChangedParams,
};
use crate::transport::registry::{ClientId, ClientRegistry};
use crate::transport::router::{RoutingDecision, route_at_mention, route_selection_changed};
use crate::transport::ws::canonicalize_or_keep_path;

/// Maximum line length accepted by the IPC parser (1 MiB). Lines longer than
/// this trigger an ERROR log and connection close.
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// Debounce window for `selection_changed` notifications. Reset on every
/// `selection` IPC frame; the notification is emitted only when this window
/// elapses without another frame arriving.
pub const SELECTION_DEBOUNCE: Duration = Duration::from_millis(300);

/// Timeout for per-client `mpsc::Sender::send_timeout`. Per the
/// `websocket` capability spec's **Outbound delivery via per-client
/// channel** requirement: if a send to a client's channel does not
/// complete within this window, the sidecar SHALL log a WARN and drop
/// the notification for that client only.
pub const PER_CLIENT_SEND_TIMEOUT: Duration = Duration::from_millis(50);

/// Errors the IPC server may emit.
#[derive(Debug, Error)]
pub enum IpcError {
    /// Underlying I/O error on the socket or filesystem.
    #[error("ipc I/O at {path:?}: {source}")]
    Io {
        /// Best-effort path that triggered the error.
        path: PathBuf,
        /// Wrapped I/O error.
        #[source]
        source: io::Error,
    },
}

/// Most recently emitted `selection_changed` payload, used for dedup
/// across IPC connections. `None` means no selection has been emitted
/// yet on this sidecar lifetime.
///
/// (Originally named `LastBroadcast` when the design used a
/// `tokio::sync::broadcast` channel; the channel is gone but the
/// dedup slot remains. Renamed to remove the residual.)
pub type LastEmittedSelection = Arc<Mutex<Option<StoredSelection>>>;

/// Inputs the [`IpcServer::run`] loop needs.
#[derive(Clone)]
pub struct IpcServer {
    /// Shared editor state pointer (read on `tools/call`).
    pub state: Arc<RwLock<EditorState>>,
    /// Shared registry of authorized WebSocket clients. The IPC layer
    /// snapshots it before each routing decision.
    pub registry: ClientRegistry,
    /// Dedup slot for `selection_changed` debouncing across all IPC
    /// connections.
    pub last_emitted_selection: LastEmittedSelection,
}

impl IpcServer {
    /// Build a fresh IPC server with an empty
    /// `last_emitted_selection` slot.
    pub fn new(state: Arc<RwLock<EditorState>>, registry: ClientRegistry) -> Self {
        Self {
            state,
            registry,
            last_emitted_selection: Arc::new(Mutex::new(None)),
        }
    }

    /// Bind a [`UnixListener`] at `socket_path`. If a stale file already
    /// exists at that path we unlink it first (per the spec).
    pub fn bind(socket_path: &Path) -> Result<UnixListener, IpcError> {
        if socket_path.exists() {
            std::fs::remove_file(socket_path).map_err(|source| IpcError::Io {
                path: socket_path.to_path_buf(),
                source,
            })?;
        }
        let listener = UnixListener::bind(socket_path).map_err(|source| IpcError::Io {
            path: socket_path.to_path_buf(),
            source,
        })?;
        info!(path = %socket_path.display(), "ipc listening");
        Ok(listener)
    }

    /// Accept loop. Spawns a per-connection task on every successful accept.
    pub async fn run(self, listener: UnixListener) -> Result<(), IpcError> {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    debug!("ipc client connected");
                    let me = self.clone();
                    tokio::spawn(async move {
                        me.handle_connection(stream).await;
                    });
                }
                Err(e) => {
                    warn!(error = %e, "ipc accept failed; continuing");
                    sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    async fn handle_connection(&self, stream: UnixStream) {
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let writer = Arc::new(Mutex::new(write_half));

        // Per-connection debounce state.
        let pending: Arc<Mutex<Option<StoredSelection>>> = Arc::new(Mutex::new(None));
        let mut debounce_handle: Option<tokio::task::JoinHandle<()>> = None;

        let mut buf = Vec::with_capacity(4096);
        loop {
            buf.clear();
            match read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES).await {
                ReadOutcome::Line(line) => {
                    if let Some(ack) = self
                        .handle_line(&line, &pending, &mut debounce_handle, &writer)
                        .await
                    {
                        let mut w = writer.lock().await;
                        let payload = format!("{ack}\n");
                        if let Err(e) = w.write_all(payload.as_bytes()).await {
                            debug!(error = %e, "ipc write failed; client gone");
                            break;
                        }
                    }
                }
                ReadOutcome::OversizedLine => {
                    error!(
                        max = MAX_LINE_BYTES,
                        "ipc line exceeded the {MAX_LINE_BYTES}-byte limit; closing connection"
                    );
                    break;
                }
                ReadOutcome::Error(e) => {
                    debug!(error = %e, "ipc read error; closing connection");
                    break;
                }
                ReadOutcome::Eof => {
                    debug!("ipc client disconnected");
                    break;
                }
            }
        }

        // Cancel any pending debounce timer for this connection.
        if let Some(h) = debounce_handle.take() {
            h.abort();
        }
    }

    /// Returns `Some(json_text)` if the caller should write an `ack`-style
    /// reply on the IPC connection (e.g. for `ping`); `None` otherwise.
    ///
    /// `writer` is passed in so the `at_mention` arm can write an
    /// `Ambiguous` reply on the same connection if routing yields
    /// multiple candidates, without closing.
    async fn handle_line(
        &self,
        line: &str,
        pending: &Arc<Mutex<Option<StoredSelection>>>,
        debounce_handle: &mut Option<tokio::task::JoinHandle<()>>,
        writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    ) -> Option<String> {
        let parsed: IpcFrame = match serde_json::from_str(line) {
            Ok(f) => f,
            Err(e) => {
                // Best-effort: peek at the type field to log "unknown type"
                // separately from outright malformed JSON.
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if let Some(t) = v.get("type").and_then(Value::as_str) {
                        warn!(
                            r#type = t,
                            "ignoring IPC frame with unknown or unsupported type"
                        );
                        return None;
                    }
                }
                warn!(error = %e, "malformed IPC line; ignoring");
                return None;
            }
        };

        match parsed {
            IpcFrame::Selection {
                file_path,
                line_start,
                line_end,
                text,
            } => {
                let stored = build_stored_selection(&file_path, line_start, line_end, text);
                // Update shared editor state immediately (synchronous read for
                // the MCP getCurrentSelection tool).
                {
                    let mut s = self.state.write().await;
                    s.apply_selection(stored.clone());
                }
                // Skip comment:// / output:// schemes.
                if has_skip_scheme(&file_path) {
                    debug!(path = %file_path, "ignoring selection from comment:// or output:// scheme");
                    return None;
                }
                // Update pending and (re)schedule the debounce timer.
                {
                    let mut p = pending.lock().await;
                    *p = Some(stored);
                }
                self.reset_debounce(pending.clone(), debounce_handle);
                None
            }
            IpcFrame::AtMention {
                file_path,
                line_start,
                line_end,
                workspace_root,
                client_id,
            } => {
                // Per spec: IPC at_mention is 0-indexed; notification is 1-indexed.
                let params = AtMentionedParams {
                    file_path: file_path.clone(),
                    line_start: line_start.saturating_add(1),
                    line_end: line_end.saturating_add(1),
                };
                let payload = serde_json::to_value(&params).unwrap_or(Value::Null);
                let notif = JsonRpcNotification::new("at_mentioned", payload);

                // Canonicalise the frame's `workspace_root` before
                // handing it to the router so the comparison against
                // registered client workspaces is canonical-vs-canonical
                // (per `notifications/spec.md` "Workspace match — unique"
                // rule wording: `canonical(client.workspace_root) ==
                // canonical(r)`). The WS handshake side already
                // canonicalises the `x-claude-code-workspace` header
                // (`transport/ws.rs::handle_connection`); without doing
                // the same here, a non-canonical `--workspace-root`
                // value (e.g. `/var/folders/...` on macOS where
                // canonicalize resolves the `/var → /private/var`
                // symlink) would silently fail to match an otherwise
                // identical client workspace.
                let workspace_root_canonical =
                    workspace_root.as_deref().map(canonicalize_or_keep_path);
                let snapshot = self.registry.snapshot().await;
                let frame_client_id = client_id.map(ClientId::from);
                let decision = route_at_mention(
                    &snapshot,
                    workspace_root_canonical.as_deref(),
                    frame_client_id,
                    tokio::time::Instant::now(),
                );

                // Reply policy on the same IPC connection:
                //   - Ambiguous → write `Ambiguous { candidates }` and
                //     wait for the helper's follow-up (picker round-trip).
                //   - Everything else (DirectClient / WorkspaceUnique /
                //     Singleton / StaleClientId / NoMatch) → write `Ack`
                //     so the helper exits immediately without waiting
                //     for its read timeout.
                //
                // Without an Ack on the drop paths the helper would
                // sit on its 500ms timeout every time no client is
                // registered, which felt like end-to-end lag to users.
                let reply_frame: IpcFrame = match decision {
                    RoutingDecision::DirectClient(id) => {
                        debug!(
                            client_id = %id,
                            rule = "client-id-override",
                            file_path = %file_path,
                            "routing at_mention"
                        );
                        deliver_to(&snapshot, id, notif).await;
                        IpcFrame::Ack
                    }
                    RoutingDecision::WorkspaceUnique(id) => {
                        debug!(
                            client_id = %id,
                            rule = "workspace-unique",
                            file_path = %file_path,
                            "routing at_mention"
                        );
                        deliver_to(&snapshot, id, notif).await;
                        IpcFrame::Ack
                    }
                    RoutingDecision::Singleton(id) => {
                        debug!(
                            client_id = %id,
                            rule = "singleton-registry",
                            file_path = %file_path,
                            "routing at_mention"
                        );
                        deliver_to(&snapshot, id, notif).await;
                        IpcFrame::Ack
                    }
                    RoutingDecision::Ambiguous { candidates } => {
                        debug!(
                            count = candidates.len(),
                            rule = "ambiguous-reply",
                            file_path = %file_path,
                            "writing Ambiguous IPC reply (awaiting follow-up)"
                        );
                        IpcFrame::Ambiguous { candidates }
                    }
                    RoutingDecision::StaleClientId {
                        requested,
                        known_ids,
                    } => {
                        let known_ids_str: Vec<String> =
                            known_ids.iter().map(ClientId::to_string).collect();
                        warn!(
                            stale_client_id = %requested,
                            known_ids = ?known_ids_str,
                            file_path = %file_path,
                            "stale client_id; dropping at_mention"
                        );
                        IpcFrame::Ack
                    }
                    RoutingDecision::NoMatch { known_workspaces } => {
                        let known: Vec<String> = known_workspaces
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect();
                        warn!(
                            file_path = %file_path,
                            workspace_root_raw = ?workspace_root.as_ref().map(|p| p.display().to_string()),
                            workspace_root_canonical = ?workspace_root_canonical.as_ref().map(|p| p.display().to_string()),
                            known_workspaces = ?known,
                            "no matching client; dropping at_mention"
                        );
                        IpcFrame::Ack
                    }
                };

                let line = match serde_json::to_string(&reply_frame) {
                    Ok(s) => s,
                    Err(e) => {
                        error!(error = %e, "failed to serialise IPC reply");
                        return None;
                    }
                };
                let mut w = writer.lock().await;
                let payload = format!("{line}\n");
                if let Err(e) = w.write_all(payload.as_bytes()).await {
                    // For Ambiguous, preserve the historical WARN: a
                    // helper that disconnected before reading drops
                    // the at-mention. For Ack it's fine — the helper
                    // may have already closed if it bailed early.
                    if matches!(reply_frame, IpcFrame::Ambiguous { .. }) {
                        warn!(
                            error = %e,
                            file_path = %file_path,
                            "ambiguous match but peer disconnected before Ambiguous reply could be written"
                        );
                    } else {
                        debug!(error = %e, "ack write failed; helper already gone");
                    }
                }
                None
            }
            IpcFrame::WorkspaceFolders { folders } => {
                let mut s = self.state.write().await;
                s.set_workspace_folders(folders);
                None
            }
            IpcFrame::OpenEditors { editors } => {
                let mut s = self.state.write().await;
                s.set_open_editors(editors);
                None
            }
            IpcFrame::Ping => Some(r#"{"type":"ack"}"#.to_string()),
            // Helpers never send `Ambiguous` — only the sidecar emits it.
            // Reject defensively in case a malformed test or a future
            // refactor sends one inbound. Logged at debug because this is
            // a protocol-shape mistake on the peer side, not a routing
            // event.
            IpcFrame::Ambiguous { .. } => {
                debug!(
                    "ignoring inbound IpcFrame::Ambiguous from helper (helpers should never send this variant)"
                );
                None
            }
            IpcFrame::Ack | IpcFrame::Log { .. } => None,
        }
    }

    fn reset_debounce(
        &self,
        pending: Arc<Mutex<Option<StoredSelection>>>,
        debounce_handle: &mut Option<tokio::task::JoinHandle<()>>,
    ) {
        if let Some(h) = debounce_handle.take() {
            h.abort();
        }
        let registry = self.registry.clone();
        let last_emitted_selection = self.last_emitted_selection.clone();
        let handle = tokio::spawn(async move {
            sleep(SELECTION_DEBOUNCE).await;
            let snapshot = {
                let mut p = pending.lock().await;
                p.take()
            };
            let Some(snapshot) = snapshot else {
                return;
            };
            // Dedup against the most recently emitted selection.
            let mut last = last_emitted_selection.lock().await;
            if last.as_ref() == Some(&snapshot) {
                debug!("selection unchanged from last emit; skipping");
                return;
            }
            *last = Some(snapshot.clone());
            drop(last);

            let file_path = snapshot.file_path.clone();
            let params = SelectionChangedParams {
                text: snapshot.text,
                file_path: snapshot.file_path,
                file_url: snapshot.file_url,
                selection: snapshot.selection,
            };
            let payload = serde_json::to_value(&params).unwrap_or(Value::Null);
            let notif = JsonRpcNotification::new("selection_changed", payload);

            // Workspace-aware routing per the **selection_changed
            // routing within workspace** requirement. The router's
            // longest-prefix match relies on `Path::starts_with`,
            // which is component-aware but does not normalise
            // symlinks — so a non-canonical file path could fail to
            // match an otherwise-matching canonical workspace prefix.
            // Canonicalise here for symmetry with the WS-side header
            // capture; `canonicalize_or_keep_path` falls back to the
            // raw value if the file doesn't exist (e.g. an unsaved
            // buffer's filename), preserving the existing total
            // behaviour. The notification's `filePath` field still
            // carries the editor-supplied path verbatim — the
            // canonicalisation is for ROUTING only.
            let file_path_canonical = canonicalize_or_keep_path(std::path::Path::new(&file_path));
            let registry_snapshot = registry.snapshot().await;
            let recipients =
                route_selection_changed(&registry_snapshot, &file_path_canonical.to_string_lossy());
            for id in recipients {
                deliver_to(&registry_snapshot, id, notif.clone()).await;
            }
        });
        *debounce_handle = Some(handle);
    }
}

/// Deliver `notif` to the client with id `target` via its per-client
/// mpsc channel, with the per-client send timeout from the spec. On
/// timeout or send error, log a WARN tagged with the client id and
/// drop the notification for that client only.
async fn deliver_to(
    snapshot: &[crate::transport::registry::ClientHandleSnapshot],
    target: ClientId,
    notif: JsonRpcNotification,
) {
    let Some(handle) = snapshot.iter().find(|c| c.id == target) else {
        // The client disconnected between snapshot and dispatch. The
        // router's stale-id path covers explicit overrides; this
        // branch covers the race where a singleton/workspace-unique
        // client drops mid-dispatch.
        debug!(
            client_id = %target,
            "target client disappeared between snapshot and dispatch; skipping"
        );
        return;
    };
    match tokio::time::timeout(PER_CLIENT_SEND_TIMEOUT, handle.tx.send(notif)).await {
        Ok(Ok(())) => { /* delivered */ }
        Ok(Err(_)) => {
            // Receiver gone (client task exited). Same outcome as the
            // race above; log at debug.
            debug!(
                client_id = %target,
                "client receiver gone; notification dropped"
            );
        }
        Err(_) => {
            warn!(
                client_id = %target,
                timeout_ms = PER_CLIENT_SEND_TIMEOUT.as_millis() as u64,
                "client outbound channel full beyond timeout; dropping notification for this client only"
            );
        }
    }
}

enum ReadOutcome {
    Line(String),
    OversizedLine,
    Error(io::Error),
    Eof,
}

/// Read bytes from `reader` into `buf` up to (and including) the next `\n`,
/// rejecting any line that exceeds `max` bytes (excluding the terminator).
///
/// Returns:
/// - [`ReadOutcome::Line`] with the trimmed UTF-8 line (newline stripped).
/// - [`ReadOutcome::OversizedLine`] if the in-progress line crosses `max`
///   bytes — the reader's stream is left in an indeterminate position; the
///   caller MUST close the connection.
/// - [`ReadOutcome::Error`] on any I/O error other than EOF.
/// - [`ReadOutcome::Eof`] when the peer has closed cleanly and no bytes
///   remain in `buf`.
async fn read_line_bounded<R>(reader: &mut R, buf: &mut Vec<u8>, max: usize) -> ReadOutcome
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    // `read_until` will append up to (and including) the delimiter or EOF.
    // It does NOT bound the buffer; we have to enforce `max` ourselves by
    // checking after the read.
    match reader.read_until(b'\n', buf).await {
        Ok(0) => ReadOutcome::Eof,
        Ok(_) => {
            // If we exceeded the limit, signal oversize. Note that
            // `read_until` could have read more than `max` bytes — the
            // BufReader's internal chunk size is the granularity.
            let payload_len = if buf.last() == Some(&b'\n') {
                buf.len() - 1
            } else {
                buf.len()
            };
            if payload_len > max {
                return ReadOutcome::OversizedLine;
            }
            // Drop trailing `\n` (and an optional preceding `\r`).
            let end = if buf.last() == Some(&b'\n') {
                buf.len() - 1
            } else {
                buf.len()
            };
            let end = if end > 0 && buf[end - 1] == b'\r' {
                end - 1
            } else {
                end
            };
            match std::str::from_utf8(&buf[..end]) {
                Ok(s) => ReadOutcome::Line(s.to_string()),
                Err(e) => ReadOutcome::Error(io::Error::new(io::ErrorKind::InvalidData, e)),
            }
        }
        Err(e) => ReadOutcome::Error(e),
    }
}

fn build_stored_selection(
    file_path: &str,
    line_start: u32,
    line_end: u32,
    text: String,
) -> StoredSelection {
    let file_url = if file_path.starts_with("file://")
        || file_path.starts_with("untitled:")
        || file_path.starts_with("comment:")
        || file_path.starts_with("output:")
    {
        file_path.to_string()
    } else {
        format!("file://{file_path}")
    };
    StoredSelection {
        text,
        file_path: file_path.to_string(),
        file_url,
        selection: Selection {
            start: Position {
                line: line_start,
                character: 0,
            },
            end: Position {
                line: line_end,
                character: 0,
            },
            is_empty: false,
        },
    }
}

fn has_skip_scheme(file_path: &str) -> bool {
    file_path.starts_with("comment:") || file_path.starts_with("output:")
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    //! Unit tests for the per-client backpressure path in `deliver_to`.
    //!
    //! These tests exercise the spec's "Slow client does not stall the
    //! router" requirement from `websocket/spec.md` directly at the
    //! `deliver_to` function boundary — no TCP, no IPC socket, no
    //! WebSocket. The full e2e pipeline is exercised separately by
    //! the integration tests in `tests/session_routing.rs`. Per the
    //! tasks.md §8.8 allowance ("may be a unit test on the per-client
    //! mpsc helper from §4.3 if the e2e variant is too flaky"), this
    //! is the unit-test approach the verifier explicitly recommended:
    //! a slow WS reader is hard to construct portably; the timeout
    //! contract lives entirely in `deliver_to`'s body.
    //!
    //! Why these tests sit here rather than in the integration crate:
    //! `deliver_to` is private to this module. Keeping the tests
    //! co-located avoids needing to `pub(crate)`-ify it just for
    //! testing.

    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tokio::sync::mpsc;
    use tokio::time::Instant as TokioInstant;

    use crate::protocol::Notification as JsonRpcNotification;
    use crate::transport::registry::{CLIENT_CHANNEL_CAPACITY, ClientHandleSnapshot, ClientId};

    /// Build a fresh `(ClientHandleSnapshot, mpsc::Receiver)` pair
    /// for the test fixture. The snapshot's `tx` is the production
    /// `mpsc::Sender<JsonRpcNotification>` exactly as the registry
    /// would produce it.
    fn make_snapshot() -> (ClientHandleSnapshot, mpsc::Receiver<JsonRpcNotification>) {
        let (tx, rx) = mpsc::channel::<JsonRpcNotification>(CLIENT_CHANNEL_CAPACITY);
        let now = TokioInstant::now();
        let snap = ClientHandleSnapshot {
            id: ClientId::new(),
            tx,
            workspace_root: None,
            last_activity: now,
            connected_at: now,
        };
        (snap, rx)
    }

    /// Build a `JsonRpcNotification` suitable for the tests below.
    /// The actual payload doesn't matter — only the delivery
    /// timing/outcome does.
    fn test_notification() -> JsonRpcNotification {
        JsonRpcNotification::new(
            "at_mentioned",
            serde_json::json!({"filePath":"/x.rs","lineStart":1,"lineEnd":1}),
        )
    }

    /// Thread-local log capture for use with `tracing::subscriber::with_default`.
    /// The slow-client test needs to assert that the WARN line was emitted
    /// AND tagged with the slow client's ClientId. This is captured per-test
    /// via the thread-local `with_default` guard (NOT a global subscriber)
    /// because `deliver_to` runs entirely on the same thread as the test's
    /// stack frame under `#[tokio::test(flavor = "current_thread")]` — there
    /// are no spawned tokio tasks between the test and the WARN emit.
    #[derive(Clone)]
    struct Cap(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Cap {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::ErrorKind::Other)?
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Cap {
        type Writer = Cap;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // -----------------------------------------------------------------
    // Spec: "Outbound delivery via per-client channel" / tasks.md §8.8.
    // -----------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn deliver_to_succeeds_immediately_when_channel_has_capacity() {
        // Baseline: a healthy client receives the notification well
        // under PER_CLIENT_SEND_TIMEOUT. Calibration for the slow-
        // client test's "≥ 50ms" assertion.
        let (snap, mut rx) = make_snapshot();
        let target = snap.id;
        let snapshot = vec![snap];

        let start = Instant::now();
        deliver_to(&snapshot, target, test_notification()).await;
        let elapsed = start.elapsed();

        // A receiver with empty buffer accepts the send instantly; the
        // tokio runtime's wake-up latency is well under 10ms on every
        // platform we care about.
        assert!(
            elapsed < Duration::from_millis(10),
            "healthy send SHALL complete promptly, got {elapsed:?}"
        );
        // The notification arrived on the receiver side.
        let received = rx.try_recv().expect("notification SHALL be on rx");
        assert_eq!(received.method, "at_mentioned");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deliver_to_skips_silently_when_target_not_in_snapshot() {
        // Defence-in-depth path: the race where a singleton client
        // drops between `registry.snapshot()` and `deliver_to`.
        // The `Some(handle) = ... else` arm fires and returns silently
        // (DEBUG-level log only). No panic.
        let snapshot: Vec<ClientHandleSnapshot> = Vec::new();
        let phantom = ClientId::new();
        let start = Instant::now();
        deliver_to(&snapshot, phantom, test_notification()).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(10),
            "missing-target path SHALL return promptly, got {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deliver_to_drops_silently_when_receiver_gone() {
        // The send-error path: receiver dropped (mimics a client task
        // that has exited). `send` returns `Err` immediately; the
        // function logs DEBUG and returns. No timeout, no panic.
        let (snap, rx) = make_snapshot();
        let target = snap.id;
        drop(rx); // Receiver gone before deliver_to fires.
        let snapshot = vec![snap];

        let start = Instant::now();
        deliver_to(&snapshot, target, test_notification()).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(10),
            "send-error path SHALL return promptly, got {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deliver_to_slow_client_times_out_and_logs_warn_with_client_id() {
        // Spec scenario (websocket/spec.md "Slow client does not stall
        // the router"; tasks.md §8.8 clauses a and c):
        //   GIVEN client A's outbound mpsc has been filled past capacity
        //         and is not being drained.
        //   WHEN  the router attempts to deliver another notification to A.
        //   THEN  the dispatch SHALL log a WARN tagged with A's ClientId.
        //   AND   the notification for A SHALL be dropped (no panic, no
        //         stall longer than ~50ms over the timeout itself).
        //
        // We fill the channel by sending `CLIENT_CHANNEL_CAPACITY`
        // notifications eagerly (the receiver is never drained). The
        // 65th send via `deliver_to` will block waiting for capacity;
        // the 50ms timeout fires; `deliver_to` returns; WARN is
        // emitted.
        let (snap, _rx_kept_for_lifetime) = make_snapshot();
        let target = snap.id;
        let target_str = target.to_string();

        // Pre-fill the mpsc to capacity. The 64th `try_send` will
        // succeed (mpsc capacity 64 = 64 buffered messages); the
        // 65th attempt via `deliver_to` should hit Full and wait.
        for i in 0..CLIENT_CHANNEL_CAPACITY {
            snap.tx
                .try_send(test_notification())
                .unwrap_or_else(|e| panic!("pre-fill #{i} SHALL succeed; got {e:?}"));
        }
        // Sanity: one more try_send returns Full.
        assert!(
            matches!(
                snap.tx.try_send(test_notification()),
                Err(mpsc::error::TrySendError::Full(_))
            ),
            "channel SHALL be full after {CLIENT_CHANNEL_CAPACITY} sends"
        );

        let snapshot = vec![snap];

        // Capture tracing events from this test's thread. We use
        // `set_default` (rather than `with_default`) because the
        // closure form of `with_default` doesn't allow `.await`
        // inside it — and `deliver_to` is async. `set_default`
        // returns a guard that holds the thread-local subscriber
        // installed across `.await` points; under
        // `#[tokio::test(flavor = "current_thread")]` the runtime
        // never moves the future across threads, so the
        // thread-local persists.
        let cap = Cap(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let start = Instant::now();
        deliver_to(&snapshot, target, test_notification()).await;
        let elapsed = start.elapsed();

        // (a) The send must have hit the timeout, not completed
        // instantly. The function's `PER_CLIENT_SEND_TIMEOUT` is
        // 50ms; we allow a generous upper bound to absorb CI jitter.
        assert!(
            elapsed >= Duration::from_millis(45),
            "deliver_to SHALL wait at least PER_CLIENT_SEND_TIMEOUT-ish before timing out; got {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "deliver_to SHALL NOT block much beyond the timeout; got {elapsed:?}"
        );

        // (b) The WARN line must be present, tagged with the slow
        // client's ClientId, and contain the spec's exact message
        // text.
        drop(_guard); // Restore the prior subscriber (if any).
        let captured = cap.0.lock().expect("cap lock");
        let text = String::from_utf8_lossy(&captured);
        assert!(
            text.contains(
                "client outbound channel full beyond timeout; dropping notification for this client only"
            ),
            "WARN SHALL contain the spec's exact message; logs:\n{text}"
        );
        assert!(
            text.contains(&target_str),
            "WARN SHALL be tagged with the slow client's ClientId ({target_str}); logs:\n{text}"
        );
        assert!(
            text.contains("WARN"),
            "log line SHALL be at WARN level; logs:\n{text}"
        );

        // (c) Sidecar otherwise healthy: deliver_to returned without
        // panicking. The fact that this line executes is the proof.
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deliver_to_healthy_client_succeeds_while_slow_client_in_snapshot() {
        // Spec scenario (websocket/spec.md "Slow client does not stall
        // the router"; tasks.md §8.8 clause b):
        //   GIVEN clients A and B coexist; A's channel is full, B's is empty.
        //   WHEN  the router attempts to deliver to B.
        //   THEN  B SHALL receive the notification within the timeout
        //         budget (i.e. not blocked by A's slowness).
        //
        // This is the load-bearing isolation guarantee: per-client mpsc
        // channels mean one slow client cannot stall delivery to others.
        let (slow_snap, _slow_rx_kept) = make_snapshot();
        let (healthy_snap, mut healthy_rx) = make_snapshot();
        let healthy_target = healthy_snap.id;

        // Fill A's channel.
        for _ in 0..CLIENT_CHANNEL_CAPACITY {
            slow_snap
                .tx
                .try_send(test_notification())
                .expect("pre-fill A");
        }

        let snapshot = vec![slow_snap, healthy_snap];

        // Deliver to B. Should succeed quickly (B's channel is empty),
        // independent of A's congestion.
        let start = Instant::now();
        deliver_to(&snapshot, healthy_target, test_notification()).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(10),
            "healthy client's delivery SHALL NOT be blocked by another client's slowness; got {elapsed:?}"
        );

        let received = healthy_rx
            .try_recv()
            .expect("healthy client SHALL receive notification");
        assert_eq!(received.method, "at_mentioned");

        // Defense-in-depth: A's channel is still full (we never drained),
        // proving the slow client wasn't accidentally healed by the
        // healthy-side delivery.
        assert!(
            matches!(
                snapshot[0].tx.try_send(test_notification()),
                Err(mpsc::error::TrySendError::Full(_))
            ),
            "slow client's channel SHALL remain full"
        );
    }
}
