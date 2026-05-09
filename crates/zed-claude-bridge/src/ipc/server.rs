//! IPC server: accept loop on a `UnixListener`, line-delimited JSON parser,
//! frame dispatch, selection-debounce timer, and dedup.
//!
//! Frame contract: see `docs/protocol.md` §6 and the OpenSpec
//! `specs/ipc/spec.md` / `specs/notifications/spec.md`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::mcp::state::{EditorState, StoredSelection};
use crate::protocol::{
    AtMentionedParams, IpcFrame, Notification as JsonRpcNotification, Position, Selection,
    SelectionChangedParams,
};

/// Maximum line length accepted by the IPC parser (1 MiB). Lines longer than
/// this trigger an ERROR log and connection close.
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// Debounce window for `selection_changed` notifications. Reset on every
/// `selection` IPC frame; the notification is emitted only when this window
/// elapses without another frame arriving.
pub const SELECTION_DEBOUNCE: Duration = Duration::from_millis(300);

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

/// Last-broadcast `selection_changed` payload, used for dedup across IPC
/// connections. `None` means no selection has been broadcast yet on this
/// sidecar lifetime.
pub type LastBroadcast = Arc<Mutex<Option<StoredSelection>>>;

/// Inputs the [`run`] loop needs.
#[derive(Clone)]
pub struct IpcServer {
    pub state: Arc<RwLock<EditorState>>,
    pub notifier: broadcast::Sender<JsonRpcNotification>,
    pub last_broadcast: LastBroadcast,
}

impl IpcServer {
    /// Build a fresh IPC server with an empty `last_broadcast` slot.
    pub fn new(
        state: Arc<RwLock<EditorState>>,
        notifier: broadcast::Sender<JsonRpcNotification>,
    ) -> Self {
        Self {
            state,
            notifier,
            last_broadcast: Arc::new(Mutex::new(None)),
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
            // Cap the buffer length explicitly: read_until won't grow past
            // MAX_LINE_BYTES + 1 because we abort the read once the limit is
            // exceeded. We use a small loop reading up to 8 KiB at a time
            // through `fill_buf`-style semantics provided by `read_until`,
            // which respects the inner BufReader's chunk size.
            match read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES).await {
                ReadOutcome::Line(line) => {
                    if let Some(ack) = self
                        .handle_line(&line, &pending, &mut debounce_handle)
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

    /// Returns `Some(json_text)` if the caller should write a reply (e.g.
    /// for `ping`); `None` otherwise.
    async fn handle_line(
        &self,
        line: &str,
        pending: &Arc<Mutex<Option<StoredSelection>>>,
        debounce_handle: &mut Option<tokio::task::JoinHandle<()>>,
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
            } => {
                // Per spec: IPC at_mention is 0-indexed; notification is 1-indexed.
                let params = AtMentionedParams {
                    file_path,
                    line_start: line_start.saturating_add(1),
                    line_end: line_end.saturating_add(1),
                };
                let payload = serde_json::to_value(&params).unwrap_or(Value::Null);
                let _ = self
                    .notifier
                    .send(JsonRpcNotification::new("at_mentioned", payload));
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
            IpcFrame::Ping => Some(
                serde_json::to_string(&IpcFrame::Ack)
                    .unwrap_or_else(|_| String::from(r#"{"type":"ack"}"#)),
            ),
            // Inbound Ack/Log are no-ops — Ack is only ever produced by the
            // sidecar; Log is reserved for sidecar→extension diagnostics and
            // is never consumed inbound today.
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
        let notifier = self.notifier.clone();
        let last_broadcast = self.last_broadcast.clone();
        let handle = tokio::spawn(async move {
            sleep(SELECTION_DEBOUNCE).await;
            let snapshot = {
                let mut p = pending.lock().await;
                p.take()
            };
            let Some(snapshot) = snapshot else {
                return;
            };
            // Dedup against the last broadcast.
            let mut last = last_broadcast.lock().await;
            if last.as_ref() == Some(&snapshot) {
                debug!("selection unchanged from last broadcast; skipping");
                return;
            }
            *last = Some(snapshot.clone());
            drop(last);

            let params = SelectionChangedParams {
                text: snapshot.text,
                file_path: snapshot.file_path,
                file_url: snapshot.file_url,
                selection: snapshot.selection,
            };
            let payload = serde_json::to_value(&params).unwrap_or(Value::Null);
            let _ = notifier.send(JsonRpcNotification::new("selection_changed", payload));
        });
        *debounce_handle = Some(handle);
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
/// - [`ReadOutcome::Eof`] on a clean EOF before any bytes were read.
/// - [`ReadOutcome::Error`] on transport error.
async fn read_line_bounded<R>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
    max: usize,
) -> ReadOutcome
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    loop {
        let mut byte = [0u8; 1];
        match reader.read(&mut byte).await {
            Ok(0) => {
                if buf.is_empty() {
                    return ReadOutcome::Eof;
                }
                // Final partial line without terminating `\n`. Honour size
                // limit just like a complete line.
                if buf.len() > max {
                    return ReadOutcome::OversizedLine;
                }
                return match std::str::from_utf8(buf) {
                    Ok(s) => ReadOutcome::Line(s.to_string()),
                    Err(e) => ReadOutcome::Error(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid UTF-8: {e}"),
                    )),
                };
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return match std::str::from_utf8(buf) {
                        Ok(s) => ReadOutcome::Line(s.to_string()),
                        Err(e) => ReadOutcome::Error(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid UTF-8: {e}"),
                        )),
                    };
                }
                if buf.len() >= max {
                    return ReadOutcome::OversizedLine;
                }
                buf.push(byte[0]);
            }
            Err(e) => return ReadOutcome::Error(e),
        }
    }
}

fn has_skip_scheme(file_path: &str) -> bool {
    file_path.starts_with("comment://") || file_path.starts_with("output://")
}

fn build_stored_selection(
    file_path: &str,
    line_start: u32,
    line_end: u32,
    text: String,
) -> StoredSelection {
    // Treat anything starting with `/` as a POSIX absolute path that needs
    // a `file://` prefix; everything else is presumed to already be a
    // scheme-prefixed URI (e.g. `untitled:Untitled-1`, `comment://foo`).
    let file_url = if file_path.starts_with('/') {
        format!("file://{file_path}")
    } else {
        file_path.to_string()
    };
    StoredSelection {
        text: text.clone(),
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
            is_empty: text.is_empty(),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests (pure helpers; integration tests live under tests/ipc.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;

    #[test]
    fn skip_scheme_detects_comment_and_output() {
        assert!(has_skip_scheme("comment://abc"));
        assert!(has_skip_scheme("output://stderr"));
        assert!(!has_skip_scheme("file:///x"));
        assert!(!has_skip_scheme("/abs/path"));
        assert!(!has_skip_scheme(""));
    }

    #[test]
    fn build_stored_selection_synthesizes_file_url_from_abs_path() {
        let s = build_stored_selection("/p/main.rs", 10, 12, "hello".to_string());
        assert_eq!(s.file_path, "/p/main.rs");
        assert_eq!(s.file_url, "file:///p/main.rs");
        assert_eq!(s.selection.start.line, 10);
        assert_eq!(s.selection.end.line, 12);
        assert!(!s.selection.is_empty);
    }

    #[test]
    fn build_stored_selection_passes_through_uri() {
        let s = build_stored_selection("untitled:Untitled-1", 0, 0, String::new());
        assert_eq!(s.file_url, "untitled:Untitled-1");
        assert!(s.selection.is_empty);
    }
}
