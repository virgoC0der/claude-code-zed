//! WebSocket implementation: bind, auth, multi-client registry, request loop.
//!
//! Per the OpenSpec `session-routing` change, the prior single-client
//! displacement policy is REMOVED — multiple authorized clients
//! coexist freely. Outbound notifications are delivered to a specific
//! client by sending into that client's per-client mpsc channel
//! (registered in [`crate::transport::ClientRegistry`]).

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{RwLock, mpsc};
use tokio::time::{Instant, sleep};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{debug, error, info, warn};

use crate::mcp::{EditorState, McpResponse, dispatch};
use crate::protocol::{Notification as JsonRpcNotification, Request as JsonRpcRequest, RequestId};
use crate::transport::cwd_resolver::{CwdResolver, default_cwd_resolver};
use crate::transport::registry::{CLIENT_CHANNEL_CAPACITY, ClientHandle, ClientId, ClientRegistry};

/// Header name carrying the per-launch auth token. Matches the VSCode
/// extension's literal: `x-claude-code-ide-authorization`.
pub const AUTH_HEADER: &str = "x-claude-code-ide-authorization";

/// Optional header carrying the Claude session's workspace cwd. Used
/// as priority-1 input to the registry entry's `workspace_root`. See
/// the `protocol` capability's **WebSocket workspace request header**
/// requirement.
pub const WORKSPACE_HEADER: &str = "x-claude-code-workspace";

/// Lower bound (inclusive) of the port range we pick from.
pub const MIN_PORT: u16 = 10_000;
/// Upper bound (inclusive) of the port range we pick from.
pub const MAX_PORT: u16 = 65_535;

/// Maximum wall-clock budget for a single peer-cwd resolver call
/// from inside the accept loop. Per team-lead OQ2 decision: the
/// resolver itself is the expected path (libproc enumeration on
/// macOS typically completes in < 10 ms); this timeout is a safety
/// net for the case where libproc enumeration runs away (huge
/// process tree, transient kernel slowness, etc.). On timeout the
/// transport logs WARN and falls through to priority 3
/// (`clientInfo.cwd`) / priority 4 (`--workspace`); the WebSocket
/// connection stays open. The timeout does NOT close the underlying
/// blocking thread — `spawn_blocking` continues to completion and
/// drops its result silently, which is the correct behaviour for a
/// best-effort signal.
pub const PEER_CWD_RESOLVER_TIMEOUT_MS: u64 = 250;

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
/// editor state, and the multi-client registry.
///
/// Cloning is cheap — the registry is `Arc`-shared so every cloned
/// `Transport` (including the one passed into the IPC layer via
/// [`Transport::registry`]) sees the same live set of clients.
///
/// The optional `daemon_workspace` is the canonicalised value of the
/// sidecar's `--workspace` CLI flag. It is consumed as **priority 4**
/// in the post-`peer-cwd-discovery` per-client `workspace_root`
/// resolution chain (see `websocket/spec.md` "Workspace identification
/// on connect" rule 4 and the "Defaults to --workspace when no
/// client-side and no peer-cwd signal" scenario). When `None`, the
/// fallback never fires and a client with no header, no peer-cwd
/// signal, and no `clientInfo.cwd` keeps `workspace_root = None`.
#[derive(Clone)]
pub struct Transport {
    auth: AuthToken,
    state: SharedEditorState,
    registry: ClientRegistry,
    daemon_workspace: Option<PathBuf>,
    /// Resolver used at WebSocket-accept time to derive the peer
    /// process's cwd as priority 2 in the workspace-identification
    /// chain (between the `x-claude-code-workspace` header and the
    /// `clientInfo.cwd` field of MCP `initialize`).
    ///
    /// Production code threads in `default_cwd_resolver()` —
    /// `LibprocCwdResolver` on macOS, `NoopCwdResolver` elsewhere.
    /// Tests inject `MockCwdResolver` via
    /// [`TransportBuilder::with_cwd_resolver`]. See design D1 and
    /// `openspec/changes/peer-cwd-discovery/specs/websocket/spec.md`
    /// → **Peer-process cwd discovery**.
    ///
    cwd_resolver: Arc<dyn CwdResolver>,
}

/// Builder for [`Transport`].
///
/// Constructed via [`Transport::builder`]. Fluent setters let tests
/// (and `app/lifecycle.rs`) inject custom `daemon_workspace` and
/// `cwd_resolver` values without forcing a constructor matrix on every
/// combination. See design.md D6.
///
/// The two legacy constructors [`Transport::new`] and
/// [`Transport::with_daemon_workspace`] survive unchanged for callers
/// that don't need a custom resolver — they delegate to this builder.
#[must_use = "TransportBuilder does nothing until `build()` is called"]
pub struct TransportBuilder {
    auth: AuthToken,
    state: SharedEditorState,
    daemon_workspace: Option<PathBuf>,
    cwd_resolver: Arc<dyn CwdResolver>,
}

impl TransportBuilder {
    /// Set the sidecar's `--workspace` fallback path. Canonicalised
    /// at `build()` time; failures keep the raw path with a DEBUG
    /// log. This corresponds to priority 4 in the
    /// workspace-identification chain.
    pub fn with_daemon_workspace(mut self, daemon_workspace: PathBuf) -> Self {
        self.daemon_workspace = Some(daemon_workspace);
        self
    }

    /// Override the peer-cwd resolver. Production code uses the
    /// platform default ([`default_cwd_resolver`]); tests pass a
    /// [`MockCwdResolver`](crate::transport::cwd_resolver::MockCwdResolver)
    /// so they can script the cwd returned per peer port without
    /// invoking real libproc.
    pub fn with_cwd_resolver(mut self, cwd_resolver: Arc<dyn CwdResolver>) -> Self {
        self.cwd_resolver = cwd_resolver;
        self
    }

    /// Finalise the builder and construct a [`Transport`].
    pub fn build(self) -> Transport {
        let canonical_daemon_workspace =
            self.daemon_workspace.map(|p| canonicalize_or_keep_path(&p));
        Transport {
            auth: self.auth,
            state: self.state,
            registry: ClientRegistry::new(),
            daemon_workspace: canonical_daemon_workspace,
            cwd_resolver: self.cwd_resolver,
        }
    }
}

impl Transport {
    /// Begin constructing a `Transport` with full builder access to
    /// the daemon-workspace fallback and the peer-cwd resolver.
    ///
    /// The builder's `cwd_resolver` defaults to
    /// [`default_cwd_resolver`] (`LibprocCwdResolver` on macOS,
    /// `NoopCwdResolver` elsewhere); the `daemon_workspace` defaults
    /// to `None` (no priority-4 fallback). Both can be overridden
    /// with the builder's fluent setters. See design.md D6.
    pub fn builder(auth: AuthToken, state: SharedEditorState) -> TransportBuilder {
        TransportBuilder {
            auth,
            state,
            daemon_workspace: None,
            cwd_resolver: default_cwd_resolver(),
        }
    }

    /// Build a fresh transport with an empty client registry and no
    /// daemon workspace fallback.
    ///
    /// For production use prefer [`Transport::with_daemon_workspace`]
    /// which threads the sidecar's `--workspace` flag in as priority
    /// 4 in the workspace-identification chain. Kept for tests that
    /// don't care about that fallback.
    ///
    /// Equivalent to `Transport::builder(auth, state).build()` — the
    /// platform default `cwd_resolver` is used.
    pub fn new(auth: AuthToken, state: SharedEditorState) -> Self {
        Self::builder(auth, state).build()
    }

    /// Build a fresh transport that uses `daemon_workspace` as the
    /// fallback `workspace_root` for clients that send neither the
    /// `x-claude-code-workspace` header, nor a peer-cwd signal from
    /// the resolver, nor `clientInfo.cwd` (priority 4 in the
    /// websocket capability spec).
    ///
    /// The path is canonicalised once at construction time; failures
    /// keep the raw path with a DEBUG log (single `stat`).
    ///
    /// Equivalent to
    /// `Transport::builder(auth, state).with_daemon_workspace(p).build()`.
    pub fn with_daemon_workspace(
        auth: AuthToken,
        state: SharedEditorState,
        daemon_workspace: PathBuf,
    ) -> Self {
        Self::builder(auth, state)
            .with_daemon_workspace(daemon_workspace)
            .build()
    }

    /// Return a clone of the multi-client registry. The IPC layer
    /// consults it to route outbound `at_mentioned` and
    /// `selection_changed` notifications to specific clients.
    pub fn registry(&self) -> ClientRegistry {
        self.registry.clone()
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
                if let Err(e) = me.handle_connection(stream, peer).await {
                    debug!(error = %e, "connection handler ended with error");
                }
            });
        }
    }

    async fn handle_connection(
        &self,
        stream: TcpStream,
        peer: SocketAddr,
    ) -> Result<(), TransportError> {
        let auth_status = Arc::new(AtomicU8::new(AUTH_MISSING));
        let expected_bytes: Vec<u8> = self.auth.as_bytes().to_vec();
        // The `x-claude-code-workspace` header, if the client supplied
        // one. Captured inside the callback closure and read out after
        // the handshake completes.
        let workspace_header = Arc::new(std::sync::Mutex::new(None::<String>));

        let auth_status_for_cb = auth_status.clone();
        let workspace_for_cb = workspace_header.clone();
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

                // Optional workspace header. Per the `protocol` capability
                // spec, presence does not affect the handshake — we record
                // it for later use only if auth passed.
                if status == AUTH_OK {
                    if let Some(v) = req.headers().get(WORKSPACE_HEADER) {
                        if let Ok(s) = std::str::from_utf8(v.as_bytes()) {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                if let Ok(mut guard) = workspace_for_cb.lock() {
                                    *guard = Some(trimmed.to_owned());
                                }
                            }
                        }
                    }
                }

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

        // Resolve the registry entry's initial `workspace_root` using
        // priorities 1 and 2 of the four-step chain (priority 3 and 4
        // fire lazily in `dispatch_text` after `initialize` arrives —
        // see the comment in `serve_authorized` for the full picture).
        //
        //   Priority 1: `x-claude-code-workspace` request header.
        //   Priority 2: peer-process cwd via `self.cwd_resolver`.
        //
        // The header capture below is synchronous (a single stat per
        // accept on a path the user supplied). The resolver call may
        // run libproc on macOS via `spawn_blocking`; we wrap it in a
        // 250 ms `tokio::time::timeout` per team-lead OQ2 as a
        // safety net (the resolver is the expected path, the timeout
        // is the cliff for runaway libproc enumeration). Timeout →
        // WARN + fall through to priority 3 / 4 (no WebSocket close).
        let (initial_workspace, source): (Option<PathBuf>, &'static str) = if let Some(raw) =
            workspace_header.lock().ok().and_then(|g| g.clone())
        {
            let canonical = canonicalize_or_keep(&raw);
            (Some(canonical), "header")
        } else {
            let resolver_timeout = Duration::from_millis(PEER_CWD_RESOLVER_TIMEOUT_MS);
            match tokio::time::timeout(resolver_timeout, self.cwd_resolver.resolve(peer)).await {
                Ok(Some(p)) => {
                    let canonical = canonicalize_or_keep_path(&p);
                    (Some(canonical), "peer-cwd-libproc")
                }
                Ok(None) => {
                    debug!(
                        peer = %peer,
                        "peer-cwd resolver returned None; falling through to priority 3/4"
                    );
                    (None, "pending-initialize")
                }
                Err(_elapsed) => {
                    warn!(
                        peer = %peer,
                        timeout_ms = PEER_CWD_RESOLVER_TIMEOUT_MS,
                        "cwd resolver timed out; falling through"
                    );
                    (None, "pending-initialize")
                }
            }
        };

        self.serve_authorized(ws, initial_workspace, source).await
    }

    async fn serve_authorized(
        &self,
        mut ws: WebSocketStream<TcpStream>,
        initial_workspace: Option<PathBuf>,
        workspace_source: &'static str,
    ) -> Result<(), TransportError> {
        // Build the registry entry. Per the `websocket` capability:
        // - id is a fresh UUID v4;
        // - tx is a per-client mpsc::channel(CLIENT_CHANNEL_CAPACITY);
        // - workspace_root resolves in the 4-priority order
        //   (post `peer-cwd-discovery`):
        //     1. `x-claude-code-workspace` header (captured into
        //        `initial_workspace` by `handle_connection`);
        //     2. peer-process cwd via `CwdResolver` (also captured
        //        into `initial_workspace` by `handle_connection`
        //        when priority 1 didn't fire);
        //     3. `clientInfo.cwd` from the MCP `initialize` request
        //        (captured later in `dispatch_text`, LAZY);
        //     4. the sidecar's `--workspace` flag, via
        //        `self.daemon_workspace`, also applied LAZILY in
        //        `dispatch_text` after the `initialize` handshake
        //        — see the comment block there. The lazy ordering
        //        avoids a brittle path-identity comparison.
        // - workspace_source carries the priority tag that fired:
        //   "header", "peer-cwd-libproc", or "pending-initialize"
        //   here at registration; "client-info-cwd" or
        //   "daemon-workspace" later in `dispatch_text` (those
        //   surface in its DEBUG log).
        // - last_activity = connected_at = Instant::now().
        let id = ClientId::new();
        let (tx, mut rx) = mpsc::channel::<JsonRpcNotification>(CLIENT_CHANNEL_CAPACITY);
        let now = Instant::now();
        let handle = ClientHandle {
            id,
            tx,
            workspace_root: initial_workspace.clone(),
            last_activity: now,
            connected_at: now,
        };
        self.registry.insert(handle).await;
        info!(
            client_id = %id,
            workspace = ?initial_workspace,
            workspace_source,
            "authorized websocket client registered"
        );

        let outcome = loop {
            tokio::select! {
                // Outbound: per-client notification pushed by the IPC layer.
                msg = rx.recv() => {
                    match msg {
                        Some(notif) => {
                            let frame = match serde_json::to_string(&notif) {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!(error = %e, "failed to encode outbound notification");
                                    continue;
                                }
                            };
                            if let Err(e) = ws.send(Message::Text(frame)).await {
                                debug!(client_id = %id, error = %e, "send failed; client gone");
                                break Ok(());
                            }
                        }
                        None => {
                            // Channel closed (our Sender side dropped, e.g. registry removed us).
                            debug!(client_id = %id, "outbound channel closed");
                            break Ok(());
                        }
                    }
                }
                // Inbound: a frame from the client.
                incoming = ws.next() => {
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            // Bump activity BEFORE dispatch per the spec.
                            self.registry.bump_activity(id).await;
                            if let Some(reply) = self.dispatch_text(id, &text).await {
                                if let Err(e) = ws.send(Message::Text(reply)).await {
                                    debug!(client_id = %id, error = %e, "send failed during reply; client gone");
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
                                debug!(client_id = %id, error = %e, "pong send failed");
                                break Ok(());
                            }
                        }
                        Some(Ok(Message::Pong(_))) => { /* unsolicited pong: ignore */ }
                        Some(Ok(Message::Close(frame))) => {
                            debug!(client_id = %id, ?frame, "client requested close");
                            let _ = ws.close(None).await;
                            break Ok(());
                        }
                        Some(Ok(Message::Frame(_))) => { /* raw frame; ignore */ }
                        Some(Err(e)) => {
                            debug!(client_id = %id, error = %e, "websocket read error; closing");
                            break Ok(());
                        }
                        None => {
                            debug!(client_id = %id, "client EOF");
                            break Ok(());
                        }
                    }
                }
            }
        };

        self.registry.remove(id).await;
        info!(
            client_id = %id,
            "websocket client disconnected; sidecar continues running"
        );
        outcome
    }

    /// Parse a JSON-RPC text frame, dispatch it via [`crate::mcp::dispatch`],
    /// and possibly extract `params.clientInfo.cwd` on the `initialize`
    /// request to populate the registry entry's workspace_root.
    ///
    /// Returns `Some(json_text)` if the caller should write a reply on
    /// the WebSocket; `None` otherwise.
    async fn dispatch_text(&self, id: ClientId, text: &str) -> Option<String> {
        let req: JsonRpcRequest = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                warn!(client_id = %id, error = %e, "malformed JSON-RPC frame; ignoring");
                return None;
            }
        };

        // Workspace-identification priority resolution at `initialize`
        // time. The full 4-priority chain (post `peer-cwd-discovery`):
        //
        // 1. `x-claude-code-workspace` header — applied at
        //    handshake time in `handle_connection`.
        // 2. Peer-process cwd via `CwdResolver` — also applied at
        //    handshake time in `handle_connection`, when the header
        //    didn't fire.
        // 3. `clientInfo.cwd` from MCP `initialize` — applied HERE
        //    (LAZY: requires the request body, which arrives after
        //    the handshake).
        // 4. The sidecar's `--workspace` daemon fallback — also
        //    applied HERE, only when (3) didn't fire.
        //
        // Priorities 1 and 2 already populated `workspace_root` at
        // handshake time. We detect that case via the
        // `if current.is_none()` gate below: if peer-cwd
        // (priority 2) already set it, priorities 3 and 4 stay
        // no-op. Otherwise priority 3 wins if `clientInfo.cwd` is
        // present; otherwise priority 4 fires if the daemon has a
        // `--workspace` configured. This closes the spec's
        // "clientInfo.cwd used when header and peer-cwd both miss"
        // and "Defaults to --workspace when no client-side and no
        // peer-cwd signal" scenarios.
        //
        // Doing the daemon fallback HERE (rather than at
        // registry-insert time) avoids a brittle path-identity
        // comparison between the current value and
        // `self.daemon_workspace`: priority 3 arrives before
        // priority 4 in the same critical section, so there is no
        // shadowing.
        //
        // Parsing is done in this layer (not `mcp/`) so the
        // placement rule "no I/O in `mcp/`" is preserved.
        if req.method == "initialize" {
            let snap = self.registry.snapshot().await;
            let current = snap
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.workspace_root.clone());

            if current.is_none() {
                // Priority 3: clientInfo.cwd.
                let cwd = extract_initialize_cwd(req.params.as_ref());
                if let Some(cwd) = cwd {
                    let canonical = canonicalize_or_keep(&cwd);
                    if self.registry.set_workspace(id, canonical.clone()).await {
                        debug!(
                            client_id = %id,
                            workspace = %canonical.display(),
                            workspace_source = "client-info-cwd",
                            "captured workspace_root from clientInfo.cwd (priority 3)"
                        );
                    }
                } else if let Some(daemon) = self.daemon_workspace.clone() {
                    // Priority 4: daemon `--workspace` fallback. Only
                    // fires when none of priorities 1-3 produced a
                    // value.
                    if self.registry.set_workspace(id, daemon.clone()).await {
                        debug!(
                            client_id = %id,
                            workspace = %daemon.display(),
                            workspace_source = "daemon-workspace",
                            "applied workspace_root from daemon --workspace fallback (priority 4)"
                        );
                    }
                }
            }
        }

        let state_guard = self.state.read().await;
        let outcome = dispatch(&state_guard, req);
        drop(state_guard);
        match outcome {
            McpResponse::Reply(resp) => match serde_json::to_string(&resp) {
                Ok(s) => Some(s),
                Err(e) => {
                    error!(client_id = %id, error = %e, "failed to encode JSON-RPC response");
                    None
                }
            },
            McpResponse::NoReply => None,
        }
    }
}

/// Best-effort path canonicalisation. Returns the canonical form on
/// success; on failure (path doesn't exist yet, permission denied, …)
/// keeps the as-supplied path and logs at debug. Single `stat` per
/// invocation; safe to call synchronously inside an async fn per
/// design D3.
fn canonicalize_or_keep(raw: &str) -> PathBuf {
    canonicalize_or_keep_path(std::path::Path::new(raw))
}

/// Same as [`canonicalize_or_keep`] but operates on an already-typed
/// `&Path`. Used by `Transport::with_daemon_workspace` to canonicalise
/// the sidecar's `--workspace` flag once at construction time, and by
/// the IPC server to canonicalise the `at_mention` frame's
/// `workspace_root` so router-side `PathBuf::eq` compares
/// canonical-against-canonical (per `notifications/spec.md` "Workspace
/// match — unique" wording: `canonical(client.workspace_root) ==
/// canonical(r)`).
pub(crate) fn canonicalize_or_keep_path(p: &std::path::Path) -> PathBuf {
    match std::fs::canonicalize(p) {
        Ok(c) => c,
        Err(e) => {
            debug!(
                path = %p.display(),
                error = %e,
                "canonicalize failed; keeping as-supplied"
            );
            p.to_path_buf()
        }
    }
}

/// Extract `params.clientInfo.cwd` from an `initialize` request, if
/// present, non-empty, and a JSON string.
fn extract_initialize_cwd(params: Option<&serde_json::Value>) -> Option<String> {
    let p = params?;
    let cwd = p.get("clientInfo")?.get("cwd")?.as_str()?;
    let trimmed = cwd.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

// Silence "unused import" if RequestId is not yet used elsewhere in
// the module. (Kept for future at-mention / id correlation work.)
#[allow(dead_code)]
fn _request_id_marker(_: RequestId) {}

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

    /// Acceptance criterion for tasks.md §4.1: the new builder API
    /// compiles, returns a real [`Transport`], and accepts a custom
    /// [`MockCwdResolver`] without forcing the caller through the
    /// legacy constructors.
    #[test]
    fn transport_builder_compiles_with_mock_cwd_resolver() {
        use crate::transport::cwd_resolver::MockCwdResolver;
        let auth = AuthToken::new("test-token");
        let state = SharedEditorState::default();
        let mock = Arc::new(MockCwdResolver::new());
        let _transport = Transport::builder(auth, state)
            .with_cwd_resolver(mock)
            .build();
    }

    /// Legacy `Transport::new` keeps its signature and behaviour:
    /// no daemon-workspace fallback, platform-default resolver.
    #[test]
    fn transport_new_legacy_constructor_still_compiles() {
        let auth = AuthToken::new("test-token");
        let state = SharedEditorState::default();
        let _transport = Transport::new(auth, state);
    }

    /// Legacy `Transport::with_daemon_workspace` keeps its signature
    /// and behaviour: canonicalised daemon-workspace fallback,
    /// platform-default resolver.
    #[test]
    fn transport_with_daemon_workspace_legacy_constructor_still_compiles() {
        let auth = AuthToken::new("test-token");
        let state = SharedEditorState::default();
        let dir = std::env::temp_dir();
        let _transport = Transport::with_daemon_workspace(auth, state, dir);
    }

    /// Builder fluent chain: daemon workspace AND custom resolver
    /// simultaneously. Exercises the cross-product that the
    /// legacy constructors don't cover.
    #[test]
    fn transport_builder_with_daemon_workspace_and_resolver() {
        use crate::transport::cwd_resolver::MockCwdResolver;
        let auth = AuthToken::new("test-token");
        let state = SharedEditorState::default();
        let dir = std::env::temp_dir();
        let resolver: Arc<dyn CwdResolver> = Arc::new(MockCwdResolver::new());
        let _transport = Transport::builder(auth, state)
            .with_daemon_workspace(dir)
            .with_cwd_resolver(resolver)
            .build();
    }

    #[test]
    fn extract_initialize_cwd_handles_present_and_absent() {
        // Present & non-empty: returns the string.
        let v = serde_json::json!({
            "clientInfo": { "name": "claude", "cwd": "/Users/me/proj" }
        });
        assert_eq!(
            extract_initialize_cwd(Some(&v)),
            Some("/Users/me/proj".to_string())
        );

        // Whitespace-only string trims to empty → None.
        let v = serde_json::json!({"clientInfo": {"cwd": "   "}});
        assert!(extract_initialize_cwd(Some(&v)).is_none());

        // Missing field → None.
        let v = serde_json::json!({"clientInfo": {"name": "claude"}});
        assert!(extract_initialize_cwd(Some(&v)).is_none());

        // Missing clientInfo object → None.
        let v = serde_json::json!({"otherKey": 1});
        assert!(extract_initialize_cwd(Some(&v)).is_none());

        // None params → None.
        assert!(extract_initialize_cwd(None).is_none());

        // Non-string cwd (e.g. integer) → None.
        let v = serde_json::json!({"clientInfo": {"cwd": 42}});
        assert!(extract_initialize_cwd(Some(&v)).is_none());
    }
}
