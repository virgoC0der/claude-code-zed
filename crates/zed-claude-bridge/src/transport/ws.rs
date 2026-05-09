//! WebSocket implementation: bind, auth, single-client policy, request loop.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify, RwLock, broadcast};
use tokio::time::sleep;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{debug, error, info, warn};

use crate::mcp::{EditorState, McpResponse, dispatch};
use crate::protocol::{Notification as JsonRpcNotification, Request as JsonRpcRequest};

/// Header name carrying the per-launch auth token. Matches the VSCode
/// extension's literal: `x-claude-code-ide-authorization`.
pub const AUTH_HEADER: &str = "x-claude-code-ide-authorization";

/// Lower bound (inclusive) of the port range we pick from.
pub const MIN_PORT: u16 = 10_000;
/// Upper bound (inclusive) of the port range we pick from.
pub const MAX_PORT: u16 = 65_535;

const AUTH_MISSING: u8 = 0;
const AUTH_WRONG: u8 = 1;
const AUTH_OK: u8 = 2;

/// Per-launch auth secret. Wraps a [`String`] but redacts on [`std::fmt::Debug`]
/// so accidental `info!(?token)` cannot leak the value.
#[derive(Clone)]
pub struct AuthToken(Arc<String>);

impl AuthToken {
    /// Build an [`AuthToken`] from a freshly minted UUID v4 (or any string the
    /// caller has already chosen — tests use deterministic values).
    pub fn new(value: impl Into<String>) -> Self {
        Self(Arc::new(value.into()))
    }

    /// Generate a fresh random token using UUID v4.
    pub fn generate() -> Self {
        Self::new(uuid::Uuid::new_v4().to_string())
    }

    /// Borrow the underlying string. Avoid passing this through an `info!`
    /// invocation — debug-level only.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Borrow the bytes used for constant-time comparison.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact: never expose the secret in any tracing output.
        f.debug_tuple("AuthToken").field(&"<redacted>").finish()
    }
}

/// Errors produced by the transport layer.
#[derive(Debug, Error)]
pub enum TransportError {
    /// All bind attempts collided; the OS held no free port we tried.
    #[error("could not bind a port in {min}..={max} after {attempts} attempts")]
    BindExhausted {
        /// Lower bound we tried.
        min: u16,
        /// Upper bound we tried.
        max: u16,
        /// Number of attempts made.
        attempts: usize,
    },
    /// Underlying I/O error.
    #[error("transport I/O: {0}")]
    Io(#[from] io::Error),
    /// JSON parse error from a client frame.
    #[error("JSON parse: {0}")]
    Json(#[from] serde_json::Error),
    /// WebSocket protocol-level error.
    #[error("websocket: {0}")]
    Ws(#[from] WsError),
}

/// Pick a random port in `[MIN_PORT, MAX_PORT]` and bind a TCP listener on
/// `127.0.0.1`. Retries up to `max_retries` times on `EADDRINUSE`.
///
/// Returns the listener and the chosen port.
///
/// Errors:
/// - [`TransportError::BindExhausted`] when every retry hit `EADDRINUSE`.
/// - [`TransportError::Io`] for any other I/O failure on the first attempt.
pub async fn bind_random(max_retries: usize) -> Result<(TcpListener, u16), TransportError> {
    let attempts = max_retries.max(1);
    let mut last_err: Option<io::Error> = None;
    for _ in 0..attempts {
        let port = random_port_in_range();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok((listener, port)),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                debug!(port, "port in use; retrying");
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(TransportError::Io(e)),
        }
    }
    if let Some(e) = last_err {
        debug!(error = %e, "bind exhausted; last error logged at debug");
    }
    Err(TransportError::BindExhausted {
        min: MIN_PORT,
        max: MAX_PORT,
        attempts,
    })
}

fn random_port_in_range() -> u16 {
    // Use UUID v4's randomness rather than pulling in a new dep. We take 4
    // bytes to seed a u32 and modulo it into [MIN_PORT, MAX_PORT].
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let r = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let span = (MAX_PORT - MIN_PORT) as u32 + 1;
    MIN_PORT + (r % span) as u16
}

/// Constant-time byte slice comparison. Returns `true` iff `a` and `b` are
/// element-wise equal AND have the same length. Designed to avoid leaking
/// length/prefix to a side-channel observer.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // We still scan one slice so timing is not visibly different from a
        // mismatching same-length compare. Length is not secret — header
        // length is observable to an attacker — so this is belt-and-braces.
        let mut diff: u8 = 1;
        for byte in a.iter().chain(b.iter()) {
            diff |= *byte;
        }
        return diff == 0 && a.is_empty() && b.is_empty();
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Shared editor state pointer used by the transport's request loop.
pub type SharedEditorState = Arc<RwLock<EditorState>>;

/// The transport server: owns the listener, the auth secret, the shared
/// editor state, the active-client slot, and the broadcast notifier.
#[derive(Clone)]
pub struct Transport {
    auth: AuthToken,
    state: SharedEditorState,
    notifier: broadcast::Sender<JsonRpcNotification>,
    /// When the active client is being displaced, the master fires this
    /// notify so the displaced task knows to send close 1000 and exit.
    active_client: Arc<Mutex<Option<Arc<Notify>>>>,
}

impl Transport {
    /// Build a fresh transport with a broadcast capacity for outbound
    /// notifications. Capacity 64 is generous — selection_changed fires at
    /// most once per debounce window (300 ms).
    pub fn new(auth: AuthToken, state: SharedEditorState) -> Self {
        let (notifier, _rx) = broadcast::channel::<JsonRpcNotification>(64);
        Self {
            auth,
            state,
            notifier,
            active_client: Arc::new(Mutex::new(None)),
        }
    }

    /// Return a clone of the broadcast sender. The IPC layer publishes
    /// outbound notifications through this handle; every authorized client
    /// task subscribes and forwards them to its WebSocket peer.
    pub fn notifier(&self) -> broadcast::Sender<JsonRpcNotification> {
        self.notifier.clone()
    }

    /// Run the accept loop on `listener` until the listener is dropped or
    /// the OS reports a fatal error. Each accepted TCP connection is handled
    /// on a dedicated tokio task.
    pub async fn run(self, listener: TcpListener) -> Result<(), TransportError> {
        info!(addr = %listener.local_addr()?, "websocket accept loop ready");
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(p) => p,
                Err(e) => {
                    // Treat per-connection errors as warnings; only return
                    // on truly fatal listener errors (e.g. fd exhaustion the
                    // kernel is signaling consistently). For now, keep the
                    // loop alive — losing the listener is not recoverable
                    // and would manifest as the OS returning errors forever,
                    // which we surface via this warn log.
                    warn!(error = %e, "accept failed; continuing");
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            debug!(peer = %peer, "tcp accepted");
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.handle_connection(stream).await {
                    debug!(error = %e, "connection handler ended with error");
                }
            });
        }
    }

    async fn handle_connection(&self, stream: TcpStream) -> Result<(), TransportError> {
        let auth_status = Arc::new(AtomicU8::new(AUTH_MISSING));
        let expected_bytes: Vec<u8> = self.auth.as_bytes().to_vec();

        let auth_status_for_cb = auth_status.clone();
        let callback =
            move |req: &Request, response: Response| -> Result<Response, ErrorResponse> {
                // Header lookup via `http::HeaderMap` — case-insensitive.
                let provided = req.headers().get(AUTH_HEADER).map(|v| v.as_bytes());
                let status = match provided {
                    None => AUTH_MISSING,
                    Some(bytes) => {
                        if constant_time_eq(bytes, &expected_bytes) {
                            AUTH_OK
                        } else {
                            AUTH_WRONG
                        }
                    }
                };
                auth_status_for_cb.store(status, Ordering::SeqCst);
                // Always accept the upgrade so we can send a WS close frame
                // (per docs/protocol.md §2 — "VSCode" path closes 1008 from
                // inside the WebSocket, not at the HTTP layer).
                Ok(response)
            };

        let mut ws = match tokio_tungstenite::accept_hdr_async(stream, callback).await {
            Ok(s) => s,
            Err(e) => {
                debug!(error = %e, "ws handshake failed");
                return Err(TransportError::Ws(e));
            }
        };

        let auth = auth_status.load(Ordering::SeqCst);
        if auth != AUTH_OK {
            warn!(
                kind = match auth {
                    AUTH_MISSING => "missing-header",
                    AUTH_WRONG => "wrong-token",
                    _ => "unknown",
                },
                "rejecting unauthenticated WebSocket client"
            );
            let _ = ws
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy, // 1008
                    reason: "Unauthorized".into(),
                })))
                .await;
            // Drain to flush the close frame before drop.
            let _ = ws.close(None).await;
            return Ok(());
        }

        debug!("websocket client authorized");
        self.serve_authorized(ws).await
    }

    async fn serve_authorized(
        &self,
        mut ws: WebSocketStream<TcpStream>,
    ) -> Result<(), TransportError> {
        // Single-client policy: acquire the slot, displacing any prior
        // active client. The displaced task receives a Notify ping and
        // sends close 1000 itself.
        let my_displace = Arc::new(Notify::new());
        let prior = {
            let mut guard = self.active_client.lock().await;
            guard.replace(my_displace.clone())
        };
        if let Some(prior_notify) = prior {
            debug!("displacing previous active client");
            prior_notify.notify_waiters();
        }

        let mut rx = self.notifier.subscribe();
        info!("active websocket client connected");

        let outcome = loop {
            tokio::select! {
                // Outbound: notification published by the IPC layer.
                broadcast_msg = rx.recv() => {
                    match broadcast_msg {
                        Ok(notif) => {
                            let frame = match serde_json::to_string(&notif) {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!(error = %e, "failed to encode outbound notification");
                                    continue;
                                }
                            };
                            if let Err(e) = ws.send(Message::Text(frame)).await {
                                debug!(error = %e, "send failed; client gone");
                                break Ok(());
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "client lagged on broadcast");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // Notifier dropped — server is shutting down.
                            break Ok(());
                        }
                    }
                }
                // Displacement: a newer client took the slot.
                _ = my_displace.notified() => {
                    info!("displaced by a newer authorized client");
                    let _ = ws.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Normal, // 1000
                        reason: "Disconnecting previous WebSocket client".into(),
                    }))).await;
                    let _ = ws.close(None).await;
                    break Ok(());
                }
                // Inbound: a frame from the client.
                incoming = ws.next() => {
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            if let Some(reply) = self.dispatch_text(&text).await {
                                if let Err(e) = ws.send(Message::Text(reply)).await {
                                    debug!(error = %e, "send failed during reply; client gone");
                                    break Ok(());
                                }
                            }
                        }
                        Some(Ok(Message::Binary(_))) => {
                            warn!("ignoring binary websocket frame");
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            // Respond at the WebSocket protocol level. JSON-RPC
                            // pings are a separate thing handled in dispatch.
                            if let Err(e) = ws.send(Message::Pong(payload)).await {
                                debug!(error = %e, "pong send failed");
                                break Ok(());
                            }
                        }
                        Some(Ok(Message::Pong(_))) => { /* unsolicited pong: ignore */ }
                        Some(Ok(Message::Close(frame))) => {
                            debug!(?frame, "client requested close");
                            let _ = ws.close(None).await;
                            break Ok(());
                        }
                        Some(Ok(Message::Frame(_))) => { /* raw frame; ignore */ }
                        Some(Err(e)) => {
                            debug!(error = %e, "websocket read error; closing");
                            break Ok(());
                        }
                        None => {
                            debug!("client EOF");
                            break Ok(());
                        }
                    }
                }
            }
        };

        // Vacate the slot only if we still own it. If a newer client took
        // over while we were processing, do not clear their slot.
        {
            let mut guard = self.active_client.lock().await;
            if guard
                .as_ref()
                .map(|n| Arc::ptr_eq(n, &my_displace))
                .unwrap_or(false)
            {
                *guard = None;
            }
        }
        info!("websocket client disconnected; sidecar continues running");
        outcome
    }

    async fn dispatch_text(&self, text: &str) -> Option<String> {
        let req: JsonRpcRequest = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "malformed JSON-RPC frame; ignoring");
                return None;
            }
        };
        let state_guard = self.state.read().await;
        let outcome = dispatch(&state_guard, req);
        drop(state_guard);
        match outcome {
            McpResponse::Reply(resp) => match serde_json::to_string(&resp) {
                Ok(s) => Some(s),
                Err(e) => {
                    error!(error = %e, "failed to encode JSON-RPC response");
                    None
                }
            },
            McpResponse::NoReply => None,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;

    #[test]
    fn auth_token_debug_is_redacted() {
        let t = AuthToken::new("super-secret-1234");
        let dbg = format!("{:?}", t);
        assert!(
            !dbg.contains("super-secret"),
            "Debug must not leak the token, got: {dbg}"
        );
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn constant_time_eq_matches_for_equal_inputs() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects_mismatch() {
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b""));
    }

    #[test]
    fn random_port_is_within_allowed_range() {
        for _ in 0..256 {
            let p = random_port_in_range();
            assert!(
                (MIN_PORT..=MAX_PORT).contains(&p),
                "port {p} outside [{MIN_PORT}, {MAX_PORT}]"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bind_random_returns_loopback_listener_in_range() {
        let (listener, port) = bind_random(16).await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        assert!(addr.ip().is_loopback(), "must bind loopback only");
        assert!(matches!(addr.ip(), IpAddr::V4(_)), "must be IPv4");
        assert_eq!(addr.port(), port);
        assert!((MIN_PORT..=MAX_PORT).contains(&port));
    }
}
