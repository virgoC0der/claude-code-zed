//! End-to-end tests for session-aware at-mention routing.
//!
//! Builds the full sidecar stack inline (per the pattern in
//! `tests/end_to_end.rs`) and exercises the multi-client routing
//! scenarios that single-client tests cannot:
//!
//! - Two clients with distinct workspaces → at_mention routes to
//!   exactly one (this file: task #12).
//! - Same workspace + no client_id → sidecar replies with an
//!   `IpcFrame::Ambiguous` and awaits a follow-up frame (task #15).
//! - `client_id` override, stale `client_id`, no-match drop,
//!   legacy-helper disconnect, singleton fallback (tasks #13-#14).
//!
//! The wire formats and the cross-layer data flow are exercised
//! exactly as the binary uses them — no shortcuts, no
//! `Transport::registry`-style direct injection. The IPC frame is
//! serialised, written to the Unix socket, and the sidecar's IPC
//! accept loop dispatches it through the registered router.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
// The log-capture tests (#13, #14) deliberately hold a
// `std::sync::MutexGuard` across `.await` points. The guard
// serializes log-capture-using tests against each other so a single
// shared `tracing` subscriber's output buffer isn't raced. No async
// task ever tries to re-acquire the guard, so the await_holding_lock
// deadlock scenario the lint guards against is structurally
// impossible here. See the LOG_TEST_MUTEX comment block.
#![allow(clippy::await_holding_lock)]

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http::header::HeaderValue;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use zed_claude_bridge::ipc::server::IpcServer;
use zed_claude_bridge::mcp::EditorState;
use zed_claude_bridge::transport::{AuthToken, NoopCwdResolver, Transport, bind_random};

const TEST_TOKEN: &str = "session-routing-token-deadbeefcafe";

// ---------------------------------------------------------------------------
// Log capture infrastructure
// ---------------------------------------------------------------------------
//
// Several tests below need to assert that the sidecar emitted a specific
// WARN/DEBUG line (e.g. "stale client_id; dropping at_mention" or
// "ambiguous match but peer disconnected"). The WARN events fire from
// background tokio tasks spawned by the IPC accept loop — NOT from the
// test's stack frame — so `tracing::subscriber::with_default(...)`
// (a thread-local guard) does not catch them under the current_thread
// runtime, where spawned tasks may run on the same OS thread but live
// outside the with_default lexical block.
//
// Design:
// 1. A single GLOBAL tracing subscriber is installed once per test
//    binary (via `OnceLock`), writing every event into a shared
//    `Arc<Mutex<Vec<u8>>>` buffer.
// 2. cargo test runs integration tests CONCURRENTLY across OS threads
//    by default. A naive shared-buffer design would race — test A's
//    `reset_logs` could clobber test B's expected log line, OR test
//    A's leftover spawned-task log events could pollute test B's
//    assertions.
// 3. To serialise log-capture-using tests without forcing all tests
//    to run single-threaded, every test that needs log capture must
//    acquire `LOG_TEST_MUTEX` and HOLD the guard for its entire
//    duration. Tests that DON'T use log capture (e.g. tasks #12's
//    positive-routing tests) run freely in parallel with these and
//    with each other.
// 4. The guard is returned by `install_log_capture()` alongside the
//    buffer handle; tests own the guard as a local. When the test
//    function returns, the guard drops and the next log-capture test
//    can proceed.
//
// Cross-binary isolation is automatic: different integration test
// files run in different processes.

/// Shared in-memory write target for the test subscriber. All log
/// events captured anywhere in this test binary land here as
/// newline-delimited UTF-8 text formatted by `tracing_subscriber::fmt`.
type CapturedLogs = Arc<Mutex<Vec<u8>>>;

static LOG_CAPTURE: OnceLock<CapturedLogs> = OnceLock::new();

/// Per-process mutex that serializes log-capture-using tests. Held
/// for the duration of each such test so the shared buffer is only
/// touched by one test at a time. Tests that don't take this guard
/// can run freely in parallel with anything.
static LOG_TEST_MUTEX: Mutex<()> = Mutex::new(());

/// Newtype around `Arc<Mutex<Vec<u8>>>` that implements `io::Write` so
/// it can be plugged into `tracing_subscriber::fmt::Subscriber::with_writer`.
#[derive(Clone)]
struct CapturedWriter(CapturedLogs);

impl Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self.0.lock().map_err(|_| io::ErrorKind::Other)?;
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `MakeWriter` implementation that hands out a fresh `CapturedWriter`
/// per event. The clone is cheap (Arc).
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedWriter {
    type Writer = CapturedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install the test subscriber once per process (idempotent) AND
/// acquire the global log-capture mutex. Returns the shared log
/// buffer AND the mutex guard.
///
/// The mutex guard MUST be held for the duration of the test (assign
/// it to a local with a leading `_` to silence the unused-binding
/// warning). When the test returns, the guard drops and the next
/// log-capture test can proceed. Tests that don't acquire this
/// guard run in parallel with everything; tests that DO acquire it
/// serialize against each other.
///
/// The subscriber accepts events at all levels and from all targets,
/// formatted with `tracing_subscriber::fmt`. After acquiring the
/// guard, callers should `reset_logs(&buf)` to clear any leftover
/// bytes from a prior test, then run their scenario.
fn install_log_capture() -> (CapturedLogs, std::sync::MutexGuard<'static, ()>) {
    let buf = LOG_CAPTURE
        .get_or_init(|| {
            let buf: CapturedLogs = Arc::new(Mutex::new(Vec::new()));
            let writer = CapturedWriter(buf.clone());
            let subscriber = tracing_subscriber::fmt()
                .with_writer(writer)
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .with_target(true)
                .finish();
            // If a global subscriber was already installed by some
            // earlier test or by accident, the set_global_default
            // call returns Err — that's fine; we still keep our
            // buffer handle and the existing subscriber stays. The
            // tests will fail loudly via missing-log assertions in
            // that case, which is the correct signal.
            let _ = tracing::subscriber::set_global_default(subscriber);
            buf
        })
        .clone();
    // Acquire the test-serialization mutex. If it's poisoned (a prior
    // log-capture test panicked while holding it) we recover the
    // guard from the poison — the buffer's contents may be stale, but
    // `reset_logs` will clear them at the start of the next test.
    let guard = LOG_TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    (buf, guard)
}

/// Convenience: clear the shared log buffer at the start of a test.
fn reset_logs(buf: &CapturedLogs) {
    if let Ok(mut g) = buf.lock() {
        g.clear();
    }
}

/// Snapshot the current log buffer as a UTF-8 string. Lossy decode is
/// fine — the lines we assert on are pure ASCII produced by
/// `tracing_subscriber::fmt` plus user-supplied path components.
fn captured_text(buf: &CapturedLogs) -> String {
    let g = buf.lock().expect("log buffer poisoned");
    String::from_utf8_lossy(&g).into_owned()
}

/// Wait up to `within` for the buffer to contain a substring match
/// for `needle`. Returns the snapshot at the moment it matched, or
/// `None` if it never did. Polls at 25 ms intervals — cheaper than
/// signaling and fine for test-scale log volumes.
async fn wait_for_log_substring(
    buf: &CapturedLogs,
    needle: &str,
    within: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let snapshot = captured_text(buf);
        if snapshot.contains(needle) {
            return Some(snapshot);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Stand up an in-process sidecar (WS + IPC) sharing one
/// `ClientRegistry`. Returns the WS listener's address, the IPC
/// socket path, and the temp directory (kept alive for the lifetime
/// of the test).
async fn start_sidecar() -> (std::net::SocketAddr, PathBuf, TempDir) {
    let tmp = TempDir::new().unwrap();
    let socket_path = tmp.path().join("ipc.sock");

    let auth = AuthToken::new(TEST_TOKEN);
    let (ws_listener, _port) = bind_random(16).await.expect("bind ws");
    let addr = ws_listener.local_addr().expect("local addr");

    // Build shared state + transport + IPC server. No daemon
    // workspace fallback in this harness — tests that need it
    // construct their own transport via the relevant helper.
    //
    // `NoopCwdResolver` is injected so the test process's real
    // cwd is NOT auto-resolved as a priority-2 workspace. The
    // session-routing tests pre-date the peer-cwd-discovery
    // change and assume the registry's workspace_root is driven
    // by the priority-1 header (when present) or stays None
    // otherwise; production's `LibprocCwdResolver` would
    // override that assumption on macOS. The session-routing
    // regression test added by task #14 builds its own harness
    // with `MockCwdResolver`.
    let state = Arc::new(RwLock::new(EditorState::new()));
    let transport = Transport::builder(auth, state.clone())
        .with_cwd_resolver(Arc::new(NoopCwdResolver::new()))
        .build();
    let registry = transport.registry();
    let ipc_server = IpcServer::new(state, registry);
    let ipc_listener = IpcServer::bind(&socket_path).expect("bind ipc");

    // Spawn both accept loops.
    tokio::spawn(async move {
        let _ = transport.run(ws_listener).await;
    });
    tokio::spawn(async move {
        let _ = ipc_server.run(ipc_listener).await;
    });
    sleep(Duration::from_millis(20)).await;

    (addr, socket_path, tmp)
}

/// Stand up an in-process sidecar identical to [`start_sidecar`],
/// EXCEPT:
///
/// 1. The caller passes in a pre-bound [`tokio::net::TcpListener`] so
///    they can pre-connect TCP streams against the known address
///    before the accept loop starts. This is the
///    pre-connect-then-seed pattern from
///    `tests/peer_cwd_discovery.rs` — it's needed because the
///    `MockCwdResolver` is keyed on the client's local ephemeral
///    port, which the OS only assigns at `TcpStream::connect` time.
/// 2. The `Transport` is built with a `MockCwdResolver` seeded from
///    `port_to_workspace` instead of the production `NoopCwdResolver`.
///    Each entry maps a client peer port to the cwd the resolver
///    will return at WebSocket-accept time — which becomes the
///    registry entry's `workspace_root` via priority 2.
///
/// Mirrors [`start_sidecar`] for everything else: shared
/// `EditorState`, shared `ClientRegistry`, IPC server spawned, 20 ms
/// warm-up sleep before returning.
async fn start_sidecar_with_mock_cwd_resolver(
    ws_listener: tokio::net::TcpListener,
    port_to_workspace: std::collections::HashMap<u16, PathBuf>,
) -> (std::net::SocketAddr, PathBuf, TempDir) {
    let tmp = TempDir::new().unwrap();
    let socket_path = tmp.path().join("ipc.sock");
    let addr = ws_listener.local_addr().expect("local addr");

    let auth = AuthToken::new(TEST_TOKEN);
    let state = Arc::new(RwLock::new(EditorState::new()));

    let mut mock = zed_claude_bridge::transport::MockCwdResolver::new();
    for (port, path) in port_to_workspace {
        mock.insert(port, path);
    }

    let transport = Transport::builder(auth, state.clone())
        .with_cwd_resolver(Arc::new(mock))
        .build();
    let registry = transport.registry();
    let ipc_server = IpcServer::new(state, registry);
    let ipc_listener = IpcServer::bind(&socket_path).expect("bind ipc");

    tokio::spawn(async move {
        let _ = transport.run(ws_listener).await;
    });
    tokio::spawn(async move {
        let _ = ipc_server.run(ipc_listener).await;
    });
    sleep(Duration::from_millis(20)).await;

    (addr, socket_path, tmp)
}

/// Drive a WebSocket handshake on an EXISTING `TcpStream` (the test
/// has pre-connected the stream to capture its local ephemeral port
/// before the sidecar's accept loop has even run). Authorised but
/// supplies NO workspace header — used by the peer-cwd regression
/// test to force the priority-2 path.
async fn ws_upgrade_on_existing_stream(
    stream: TcpStream,
    addr: std::net::SocketAddr,
) -> WebSocketStream<TcpStream> {
    let mut req = format!("ws://{addr}/").into_client_request().unwrap();
    req.headers_mut().insert(
        "x-claude-code-ide-authorization",
        HeaderValue::from_str(TEST_TOKEN).unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("ws upgrade");
    ws
}

/// Open an authorized WebSocket connection that supplies the given
/// `x-claude-code-workspace` header. The header value is plumbed
/// into the registry's `workspace_root` via the priority-1 path in
/// `Transport::handle_connection`.
async fn ws_connect_with_workspace(
    addr: std::net::SocketAddr,
    workspace_header: &std::path::Path,
) -> WebSocketStream<TcpStream> {
    let stream = TcpStream::connect(addr).await.expect("ws tcp");
    let mut req = format!("ws://{addr}/").into_client_request().unwrap();
    req.headers_mut().insert(
        "x-claude-code-ide-authorization",
        HeaderValue::from_str(TEST_TOKEN).unwrap(),
    );
    req.headers_mut().insert(
        "x-claude-code-workspace",
        HeaderValue::from_str(workspace_header.to_str().expect("utf8 workspace")).unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("ws upgrade");
    ws
}

/// Open an authorized WebSocket connection with NO workspace header
/// and (by virtue of an empty `initialize.params`) no
/// `clientInfo.cwd`. Used by tests that exercise the router's
/// rule-4 singleton path or "no priority signal" fallbacks.
///
/// The registry entry's `workspace_root` is left `None`. The harness's
/// `start_sidecar()` does not configure a daemon `--workspace`
/// fallback, so priority-3 also doesn't fire — the entry stays
/// `None` after the `initialize` handshake.
async fn ws_connect_without_workspace(addr: std::net::SocketAddr) -> WebSocketStream<TcpStream> {
    let stream = TcpStream::connect(addr).await.expect("ws tcp");
    let mut req = format!("ws://{addr}/").into_client_request().unwrap();
    req.headers_mut().insert(
        "x-claude-code-ide-authorization",
        HeaderValue::from_str(TEST_TOKEN).unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("ws upgrade");
    ws
}

/// Receive the next text frame from `ws`, skipping ping/pong. Errors
/// (timeouts, unexpected close, binary frames) panic the test.
async fn recv_text(ws: &mut WebSocketStream<TcpStream>, within: Duration) -> Option<String> {
    loop {
        let msg = match timeout(within, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => panic!("ws read error: {e:?}"),
            Ok(None) => return None, // EOF
            Err(_) => return None,   // timeout
        };
        match msg {
            Message::Text(s) => return Some(s),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return None,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

/// Try to receive a text frame within `within`; assert that none
/// arrives. Skips ping/pong control frames.
async fn assert_no_text_within(ws: &mut WebSocketStream<TcpStream>, within: Duration, label: &str) {
    match recv_text(ws, within).await {
        None => { /* expected — nothing arrived */ }
        Some(frame) => panic!("expected no frame on {label} within {within:?}, but got: {frame}"),
    }
}

/// Send a single IPC `at_mention` frame on a fresh Unix-domain-socket
/// connection. Closes the connection after the write (legacy-helper
/// semantics — task #15 reuses the same connection for a picker
/// round-trip; this helper is for the simple one-shot case).
async fn send_ipc_at_mention(
    socket_path: &std::path::Path,
    file_path: &str,
    line_start: u32,
    line_end: u32,
    workspace_root: Option<&std::path::Path>,
    client_id: Option<uuid::Uuid>,
) {
    let mut stream = UnixStream::connect(socket_path).await.expect("ipc connect");
    let mut frame = serde_json::json!({
        "type": "at_mention",
        "file_path": file_path,
        "line_start": line_start,
        "line_end": line_end,
    });
    if let Some(w) = workspace_root {
        frame["workspace_root"] = serde_json::Value::String(w.display().to_string());
    }
    if let Some(id) = client_id {
        frame["client_id"] = serde_json::Value::String(id.hyphenated().to_string());
    }
    let mut bytes = serde_json::to_vec(&frame).expect("serialise");
    bytes.push(b'\n');
    stream.write_all(&bytes).await.expect("ipc write");
    stream.flush().await.expect("ipc flush");
    stream.shutdown().await.ok();
    // Small drain so the kernel delivers the bytes before the test
    // takes its first assertion.
    sleep(Duration::from_millis(20)).await;
}

/// Drive each connected client through one `ping` so the WS server's
/// `serve_authorized` finishes inserting it into the registry before
/// the test reads `at_mention` results. Without this priming step the
/// IPC server's first `registry.snapshot()` might race the inserts
/// and miss a client.
async fn prime_ws_client(ws: &mut WebSocketStream<TcpStream>, ping_id: i64) {
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":ping_id,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(ws, Duration::from_secs(2))
        .await
        .expect("ping reply");
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], ping_id, "ping reply must echo our id");
}

// ---------------------------------------------------------------------------
// Task #12: routing across distinct workspaces
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn at_mention_routes_to_workspace_matching_client_only() {
    // Spec scenario (notifications/spec.md, "Workspace match picks the
    // lone matching client"; tasks.md §8.1):
    //   GIVEN two authorized clients A and B with distinct workspaces
    //         (via x-claude-code-workspace header).
    //   WHEN  an `at_mention` IPC frame with workspace_root=ws_a is sent.
    //   THEN  A receives the `at_mentioned` notification within 200 ms.
    //   AND   B receives no frame during a 500 ms window after dispatch.
    //
    // The header value and the IPC frame's workspace_root both carry
    // the raw tempdir path. The WS handshake canonicalises the header
    // via `transport/ws.rs::handle_connection`, and the IPC server
    // canonicalises the frame's workspace_root via
    // `transport/ws::canonicalize_or_keep_path` (called from
    // `ipc/server.rs::handle_line`'s at_mention arm). The router then
    // compares two canonical paths and they match. This test exercises
    // the real production canonicalization path on both sides — no
    // pre-canonicalize workaround.
    let ws_a_dir = TempDir::new().expect("tempdir a");
    let ws_b_dir = TempDir::new().expect("tempdir b");
    let ws_a = ws_a_dir.path().to_path_buf();
    let ws_b = ws_b_dir.path().to_path_buf();

    let (addr, socket_path, _tmp) = start_sidecar().await;

    // Open the two WS clients with the corresponding workspace headers.
    let mut client_a = ws_connect_with_workspace(addr, &ws_a).await;
    let mut client_b = ws_connect_with_workspace(addr, &ws_b).await;

    // Prime each so the registry insert is complete. The ping reply
    // also confirms the WS upgrade succeeded and the connection is
    // serving frames.
    prime_ws_client(&mut client_a, 1).await;
    prime_ws_client(&mut client_b, 2).await;

    // Send the IPC at_mention with workspace_root = ws_a.
    let file_path = format!("{}/main.rs", ws_a.display());
    send_ipc_at_mention(&socket_path, &file_path, 9, 19, Some(&ws_a), None).await;

    // Assert A receives the at_mentioned frame within 200 ms. Match
    // on `method` first — production code never sends `id` on
    // notifications.
    let frame = timeout(
        Duration::from_millis(200),
        recv_text(&mut client_a, Duration::from_millis(200)),
    )
    .await
    .expect("at_mentioned within 200ms")
    .expect("stream had a text frame");
    let v: Value = serde_json::from_str(&frame).expect("json");
    assert_eq!(v["method"], "at_mentioned", "A SHALL receive at_mentioned");
    assert_eq!(v["params"]["filePath"], file_path);
    assert_eq!(v["params"]["lineStart"], 10, "1-indexed start");
    assert_eq!(v["params"]["lineEnd"], 20, "1-indexed end");
    assert!(v.get("id").is_none(), "notifications carry no id");

    // Assert B receives nothing during a 500 ms window.
    assert_no_text_within(&mut client_b, Duration::from_millis(500), "client B").await;

    // Cleanup. Closing the streams drops the registry entries via the
    // serve_authorized loop's `registry.remove(id).await` on loop exit.
    let _ = client_a.close(None).await;
    let _ = client_b.close(None).await;
}

/// End-to-end proof that the field bug recorded in
/// `openspec/changes/peer-cwd-discovery/proposal.md` is fixed.
///
/// Pre-`peer-cwd-discovery` symptom (from the user's live log,
/// `~/Library/Logs/zed-claude-bridge.log`):
/// ```
/// applied workspace_root from daemon --workspace fallback (priority 3)
///     client_id=44a79a8c… workspace=/Users/sx.chen
/// no matching client; dropping at_mention …
///     workspace_root_canonical=Some("/Users/sx.chen/Code/personal/claude-code-zed")
///     known_workspaces=["/Users/sx.chen"]
/// ```
/// Two Claude sessions both end up with `workspace_root = $HOME`
/// (because none of the pre-change priority-1/2/3 signals fired
/// against Claude CLI v2.1.76, and the LaunchAgent's `--workspace`
/// is `$HOME`). When the user `cmd-ctrl-c`s a file in a specific
/// project, the IPC frame's `workspace_root` is the project path —
/// which matches NEITHER registered client. The router drops the
/// at-mention.
///
/// Post-`peer-cwd-discovery`: priority 2 fires at WebSocket-accept
/// time and gives each client its own peer-process cwd. The router
/// finds the unique match and delivers to exactly that client.
///
/// This regression test stages the exact scenario in-process with
/// `MockCwdResolver` standing in for `LibprocCwdResolver` (the mock
/// avoids real libproc so CI on Linux works without modification).
///
/// Spec scenarios exercised:
/// - "Peer-cwd applied when header is absent" (per-client priority-2 fire)
/// - "Workspace match — unique" (router rule that the field bug broke)
#[tokio::test(flavor = "current_thread")]
async fn at_mention_routes_via_peer_cwd_when_no_header_and_no_client_info() {
    // 1. Bind the WS listener but DON'T start the sidecar yet —
    //    we need to know each client's peer port BEFORE building
    //    the mock resolver. `bind_random` selects a port in
    //    [MIN_PORT, MAX_PORT] and returns the bound listener; we
    //    hand it off to `start_sidecar_with_mock_cwd_resolver` below
    //    so the listener identity (and the `addr`) is preserved.
    let (ws_listener, _port) = bind_random(16).await.expect("bind ws");
    let addr = ws_listener.local_addr().expect("local addr");

    // 2. Pre-connect both TCP streams. The kernel queues the
    //    completed three-way handshakes in the listener's backlog
    //    until the not-yet-spawned accept loop pops them. Each
    //    stream's `local_addr().port()` is its ephemeral local
    //    port — the very value the server's `peer.port()` will see.
    let stream_a = TcpStream::connect(addr).await.expect("tcp a");
    let port_a = stream_a.local_addr().expect("local a").port();
    let stream_b = TcpStream::connect(addr).await.expect("tcp b");
    let port_b = stream_b.local_addr().expect("local b").port();
    assert_ne!(
        port_a, port_b,
        "OS must assign distinct ephemeral ports to the two clients"
    );

    // 3. Distinct workspace cwds for the two clients. These are the
    //    paths the `LibprocCwdResolver` would have returned in
    //    production by reading each Claude process's
    //    `proc_vnodepathinfo`; here the `MockCwdResolver` returns
    //    them deterministically.
    let ws_a_dir = TempDir::new().expect("tempdir a");
    let ws_b_dir = TempDir::new().expect("tempdir b");
    let ws_a = ws_a_dir.path().to_path_buf();
    let ws_b = ws_b_dir.path().to_path_buf();
    let mut port_to_workspace = std::collections::HashMap::new();
    port_to_workspace.insert(port_a, ws_a.clone());
    port_to_workspace.insert(port_b, ws_b.clone());

    // 4. Stand up the full sidecar with the seeded mock and the
    //    pre-bound listener (which has both pending TCP connections
    //    in its backlog).
    let (_addr, socket_path, _tmp) =
        start_sidecar_with_mock_cwd_resolver(ws_listener, port_to_workspace).await;

    // 5. Drive both WS handshakes on the existing streams.
    //    `handle_connection` will:
    //    - skip priority 1 (no header), and
    //    - hit priority 2 (the mock returns the per-port cwd),
    //    - canonicalise that cwd into the registry entry.
    let mut client_a = ws_upgrade_on_existing_stream(stream_a, addr).await;
    let mut client_b = ws_upgrade_on_existing_stream(stream_b, addr).await;
    prime_ws_client(&mut client_a, 1).await;
    prime_ws_client(&mut client_b, 2).await;

    // 6. Send an at_mention IPC frame whose `workspace_root` matches
    //    `ws_a`. Pre-change, neither client would have had a
    //    matching workspace_root (both would be None or $HOME) and
    //    the router would have dropped the frame. Post-change, A
    //    matches and B does not — deterministic delivery.
    let file_path = format!("{}/main.rs", ws_a.display());
    send_ipc_at_mention(&socket_path, &file_path, 9, 19, Some(&ws_a), None).await;

    // 7. Assert A receives the at_mentioned frame within 200 ms.
    let frame = timeout(
        Duration::from_millis(200),
        recv_text(&mut client_a, Duration::from_millis(200)),
    )
    .await
    .expect("at_mentioned within 200ms")
    .expect("client A had a text frame");
    let v: Value = serde_json::from_str(&frame).expect("json");
    assert_eq!(
        v["method"], "at_mentioned",
        "A SHALL receive at_mentioned (its peer-cwd matched the frame's workspace_root)"
    );
    assert_eq!(v["params"]["filePath"], file_path);
    assert_eq!(v["params"]["lineStart"], 10, "1-indexed start");
    assert_eq!(v["params"]["lineEnd"], 20, "1-indexed end");
    assert!(v.get("id").is_none(), "notifications carry no id");

    // 8. Assert B receives nothing during a 500 ms window. (The
    //    router routes via workspace match, not broadcast.)
    assert_no_text_within(&mut client_b, Duration::from_millis(500), "client B").await;

    let _ = client_a.close(None).await;
    let _ = client_b.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn at_mention_routes_when_frame_workspace_root_is_non_canonical() {
    // Regression protection for the canonicalization asymmetry fix.
    //
    // Setup: the WS client supplies an ALREADY-CANONICAL workspace
    // header (`/private/var/folders/.../ws-a` on macOS). The IPC
    // frame supplies the NON-canonical sibling form
    // (`/var/folders/.../ws-a`, which on macOS resolves through the
    // `/var → /private/var` symlink). The router's `PathBuf::eq` is
    // byte-for-byte equality; without the IPC-side canonicalise call
    // in `ipc/server.rs`, these would NOT match and the at_mention
    // would silently drop.
    //
    // We construct the symlink case directly to avoid depending on
    // any host-specific path layout: a tempdir + a symlink to it
    // in the same parent. The symlinked path is the "non-canonical"
    // form; `std::fs::canonicalize` resolves the symlink, giving us
    // the "canonical" form.
    let parent = TempDir::new().expect("tempdir parent");
    let real_dir = parent.path().join("real");
    std::fs::create_dir(&real_dir).expect("create real dir");
    let symlink_path = parent.path().join("symlinked");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_dir, &symlink_path).expect("create symlink");
    #[cfg(not(unix))]
    {
        // Symlink creation on Windows requires admin / dev mode.
        // Skip this test on non-Unix targets.
        return;
    }
    let canonical = std::fs::canonicalize(&symlink_path).expect("canonicalize");
    assert_ne!(
        canonical, symlink_path,
        "test precondition: symlinked form must canonicalize to a different path"
    );

    let (addr, socket_path, _tmp) = start_sidecar().await;

    // Client supplies the CANONICAL path via the WS header so the
    // registry entry stores the canonical form (the WS-side already
    // canonicalises, so the result is the same either way — we just
    // pick canonical here to make the asymmetry-protection test
    // unambiguous).
    let mut client = ws_connect_with_workspace(addr, &canonical).await;
    prime_ws_client(&mut client, 1).await;

    // IPC frame sends the SYMLINKED (non-canonical) form. Without
    // the IPC-side canonicalise fix, the router's PathBuf::eq sees
    // `<canonical>` vs `<symlinked>` and returns false → NoMatch →
    // notification dropped → recv_text times out.
    let file_path = format!("{}/x.rs", symlink_path.display());
    send_ipc_at_mention(&socket_path, &file_path, 0, 0, Some(&symlink_path), None).await;

    let frame = timeout(
        Duration::from_millis(200),
        recv_text(&mut client, Duration::from_millis(200)),
    )
    .await
    .expect("at_mentioned within 200ms — canonicalize asymmetry must be fixed")
    .expect("stream had a text frame");
    let v: Value = serde_json::from_str(&frame).expect("json");
    assert_eq!(v["method"], "at_mentioned");
    // The notification's filePath carries the editor-supplied (raw)
    // path verbatim; canonicalization is for ROUTING only.
    assert_eq!(v["params"]["filePath"], file_path);

    let _ = client.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn at_mention_routes_to_workspace_b_when_workspace_root_is_b() {
    // Symmetric mirror of the above: workspace_root=ws_b → only B
    // receives. Locks in the "B wins" half of the routing-table
    // (the previous test only proved A's side). Raw tempdir paths
    // on both sides; production canonicalises them.
    let ws_a_dir = TempDir::new().expect("tempdir a");
    let ws_b_dir = TempDir::new().expect("tempdir b");
    let ws_a = ws_a_dir.path().to_path_buf();
    let ws_b = ws_b_dir.path().to_path_buf();

    let (addr, socket_path, _tmp) = start_sidecar().await;

    let mut client_a = ws_connect_with_workspace(addr, &ws_a).await;
    let mut client_b = ws_connect_with_workspace(addr, &ws_b).await;
    prime_ws_client(&mut client_a, 1).await;
    prime_ws_client(&mut client_b, 2).await;

    let file_path = format!("{}/lib.rs", ws_b.display());
    send_ipc_at_mention(&socket_path, &file_path, 0, 0, Some(&ws_b), None).await;

    let frame = timeout(
        Duration::from_millis(200),
        recv_text(&mut client_b, Duration::from_millis(200)),
    )
    .await
    .expect("at_mentioned within 200ms")
    .expect("stream had a text frame");
    let v: Value = serde_json::from_str(&frame).expect("json");
    assert_eq!(v["method"], "at_mentioned", "B SHALL receive at_mentioned");
    assert_eq!(v["params"]["filePath"], file_path);

    assert_no_text_within(&mut client_a, Duration::from_millis(500), "client A").await;

    let _ = client_a.close(None).await;
    let _ = client_b.close(None).await;
}

// ---------------------------------------------------------------------------
// Task #13: stale client_id and legacy-helper disconnect tolerance
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn at_mention_with_stale_client_id_logs_warn_and_drops() {
    // Spec scenario (notifications/spec.md "Stale client_id falls through
    // to no-match drop"; tasks.md §8.6):
    //   GIVEN one authorized WS client is registered.
    //   WHEN  the helper sends an `at_mention` with `client_id` set to a
    //         UUID that does NOT exist in the registry.
    //   THEN  no `at_mentioned` notification is delivered to the live client.
    //   AND   the sidecar logs a WARN distinctly identifying the stale id
    //         and the registry's current ids ("stale client_id; dropping
    //         at_mention" — exact wording in `ipc/server.rs::handle_line`).
    // Acquire the log-capture mutex for the duration of this test.
    // `_log_guard` is held until the test returns; tests that don't
    // need log capture can run in parallel with this one.
    let (log_buf, _log_guard) = install_log_capture();
    reset_logs(&log_buf);

    let ws_dir = TempDir::new().expect("tempdir");
    let ws_root = ws_dir.path().to_path_buf();

    let (addr, socket_path, _tmp) = start_sidecar().await;

    let mut client = ws_connect_with_workspace(addr, &ws_root).await;
    prime_ws_client(&mut client, 1).await;

    // Construct a UUID that has effectively-zero chance of colliding
    // with the live client's ClientId (which is also a UUID v4).
    let stale = uuid::Uuid::parse_str("deadbeef-dead-4dad-beef-deadbeefdead").expect("uuid");

    let file_path = format!("{}/x.rs", ws_root.display());
    send_ipc_at_mention(&socket_path, &file_path, 0, 0, Some(&ws_root), Some(stale)).await;

    // (a) The live client SHALL receive nothing within 500 ms.
    assert_no_text_within(
        &mut client,
        Duration::from_millis(500),
        "live client (stale client_id should not reach it)",
    )
    .await;

    // (b) A WARN line with the stale id SHALL appear in the captured
    // logs. The exact format is determined by tracing_subscriber::fmt
    // — we assert on the message body + the stale_client_id field's
    // value (the literal UUID string). Both must appear; either alone
    // would not prove the scenario fired.
    let snapshot = wait_for_log_substring(
        &log_buf,
        "stale client_id; dropping at_mention",
        Duration::from_millis(500),
    )
    .await
    .expect("WARN 'stale client_id' SHALL be logged");
    assert!(
        snapshot.contains("deadbeef-dead-4dad-beef-deadbeefdead"),
        "WARN line SHALL include the stale UUID; logs:\n{snapshot}"
    );

    let _ = client.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn at_mention_ambiguous_with_immediate_peer_disconnect_logs_warn_and_drops() {
    // Spec scenario (ipc/spec.md "Legacy helper closing immediately is
    // tolerated"; tasks.md §8.7):
    //   GIVEN two authorized WS clients A and B in the SAME workspace.
    //   WHEN  a helper opens an IPC connection, writes an `at_mention`
    //         frame for that workspace (NO client_id), and IMMEDIATELY
    //         closes the connection (does NOT read the Ambiguous reply).
    //   THEN  the sidecar SHALL NOT panic.
    //   AND   neither A nor B receives an `at_mentioned` notification.
    //   AND   the sidecar logs a WARN indicating the peer disconnected
    //         ("ambiguous match but peer disconnected before Ambiguous
    //         reply could be written" — exact wording in
    //         `ipc/server.rs::handle_line`'s Ambiguous arm).
    // Acquire the log-capture mutex for the duration of this test.
    // `_log_guard` is held until the test returns; tests that don't
    // need log capture can run in parallel with this one.
    let (log_buf, _log_guard) = install_log_capture();
    reset_logs(&log_buf);

    let ws_dir = TempDir::new().expect("tempdir");
    let ws_root = ws_dir.path().to_path_buf();

    let (addr, socket_path, _tmp) = start_sidecar().await;

    // Both clients in the SAME workspace. The router will return
    // RoutingDecision::Ambiguous when an `at_mention` for this
    // workspace arrives without a client_id override — that's the
    // arm whose write-back-to-helper failure path we're testing.
    let mut client_a = ws_connect_with_workspace(addr, &ws_root).await;
    let mut client_b = ws_connect_with_workspace(addr, &ws_root).await;
    prime_ws_client(&mut client_a, 1).await;
    prime_ws_client(&mut client_b, 2).await;

    // Write the at_mention frame and drop the connection immediately
    // — no shutdown handshake, no 20ms grace. We want the sidecar's
    // Ambiguous-reply write attempt to fail because the peer is gone.
    let file_path = format!("{}/x.rs", ws_root.display());
    {
        let mut stream = UnixStream::connect(&socket_path)
            .await
            .expect("ipc connect");
        let frame = serde_json::json!({
            "type": "at_mention",
            "file_path": file_path,
            "line_start": 0,
            "line_end": 0,
            "workspace_root": ws_root.display().to_string(),
        });
        let mut bytes = serde_json::to_vec(&frame).expect("serialise");
        bytes.push(b'\n');
        stream.write_all(&bytes).await.expect("ipc write");
        stream.flush().await.expect("ipc flush");
        // Drop the stream RIGHT NOW. The kernel will deliver the
        // buffered bytes to the sidecar, but by the time the sidecar
        // processes the frame and tries to write the Ambiguous reply,
        // both halves of the UnixStream are dead.
    }

    // (a) The sidecar SHALL NOT panic — verified implicitly: if it
    //     panicked, the IPC accept loop would die and our subsequent
    //     "no frame on A/B" assertions wouldn't be meaningful. To prove
    //     the IPC server is still healthy, connect again and send a
    //     ping; expect an ack.
    {
        let mut stream2 = UnixStream::connect(&socket_path)
            .await
            .expect("ipc still accepts after the disconnect scenario");
        let mut bytes =
            serde_json::to_vec(&serde_json::json!({"type": "ping"})).expect("serialise");
        bytes.push(b'\n');
        stream2.write_all(&bytes).await.expect("ipc write ping");
        stream2.flush().await.expect("ipc flush");
        let mut buf = [0u8; 64];
        let n = timeout(
            Duration::from_millis(500),
            tokio::io::AsyncReadExt::read(&mut stream2, &mut buf),
        )
        .await
        .expect("ping ack within 500ms")
        .expect("read ok");
        let reply = std::str::from_utf8(&buf[..n]).expect("ack utf8");
        assert!(reply.contains("\"ack\""), "expected ack, got {reply:?}");
        let _ = stream2.shutdown().await;
    }

    // (b) Neither A nor B SHALL receive a frame within 500 ms.
    assert_no_text_within(&mut client_a, Duration::from_millis(500), "client A").await;
    assert_no_text_within(&mut client_b, Duration::from_millis(500), "client B").await;

    // (c) The sidecar SHALL log the peer-disconnect WARN.
    let snapshot = wait_for_log_substring(
        &log_buf,
        "ambiguous match but peer disconnected before Ambiguous reply could be written",
        Duration::from_millis(500),
    )
    .await
    .expect("WARN 'ambiguous match but peer disconnected' SHALL be logged");
    // Belt-and-braces: the snapshot must show the WARN level too,
    // so we're not matching a stray INFO/DEBUG line that happens to
    // contain the substring.
    assert!(
        snapshot.contains(" WARN ") || snapshot.contains("WARN "),
        "log line SHALL be at WARN level; logs:\n{snapshot}"
    );

    let _ = client_a.close(None).await;
    let _ = client_b.close(None).await;
}

// ---------------------------------------------------------------------------
// Task #14: singleton fallback and no-match drop
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn at_mention_singleton_fallback_delivers_to_lone_client() {
    // Spec scenario (notifications/spec.md "Singleton registry routes
    // regardless of workspace"; tasks.md §8.4):
    //   GIVEN one authorized WS client with NO workspace header (and
    //         the harness configures no daemon --workspace fallback,
    //         so `workspace_root` stays `None`).
    //   WHEN  an `at_mention` IPC frame with NO `workspace_root` is sent.
    //   THEN  the lone client SHALL receive the `at_mentioned` notification
    //         within 200 ms via the router's rule 4 (singleton-registry).
    //
    // The DEBUG log emitted by `ipc/server.rs`'s singleton arm tags
    // the rule as `rule="singleton-registry"`. We don't assert on
    // that here (it's a DEBUG line, not WARN, and the spec doesn't
    // mandate observable proof at this layer) — the positive
    // delivery within 200ms is the load-bearing assertion.
    let (addr, socket_path, _tmp) = start_sidecar().await;

    let mut client = ws_connect_without_workspace(addr).await;
    prime_ws_client(&mut client, 1).await;

    // IPC at_mention with NO workspace_root, NO client_id.
    let file_path = "/some/path/lib.rs";
    send_ipc_at_mention(&socket_path, file_path, 4, 7, None, None).await;

    let frame = timeout(
        Duration::from_millis(200),
        recv_text(&mut client, Duration::from_millis(200)),
    )
    .await
    .expect("at_mentioned within 200ms via singleton rule")
    .expect("stream had a text frame");
    let v: Value = serde_json::from_str(&frame).expect("json");
    assert_eq!(v["method"], "at_mentioned");
    assert_eq!(v["params"]["filePath"], file_path);
    assert_eq!(v["params"]["lineStart"], 5, "1-indexed start (4 + 1)");
    assert_eq!(v["params"]["lineEnd"], 8, "1-indexed end (7 + 1)");
    assert!(v.get("id").is_none(), "notifications carry no id");

    let _ = client.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn at_mention_no_match_drops_with_warn_listing_known_workspaces() {
    // Spec scenario (notifications/spec.md "No matching client drops
    // with a WARN"; tasks.md §8.5):
    //   GIVEN two authorized WS clients A and B with distinct
    //         workspaces ws_a and ws_b (real on-disk tempdirs so the
    //         WS-side canonicalize succeeds, mirroring production).
    //   WHEN  an `at_mention` IPC frame with `workspace_root=ws_c`
    //         (a third, unrelated tempdir) is sent — matches neither
    //         A nor B.
    //   THEN  neither client receives an `at_mentioned` within 500 ms.
    //   AND   the sidecar logs a WARN at the NoMatch arm containing:
    //         - the file path
    //         - `ws_c` (in either the raw or canonical workspace_root
    //           field — both fire on the post-#12-fix code path)
    //         - both `ws_a` and `ws_b` in the `known_workspaces` set
    // Acquire the log-capture mutex for the duration of this test.
    // `_log_guard` is held until the test returns; tests that don't
    // need log capture can run in parallel with this one.
    let (log_buf, _log_guard) = install_log_capture();
    reset_logs(&log_buf);

    let ws_a_dir = TempDir::new().expect("tempdir a");
    let ws_b_dir = TempDir::new().expect("tempdir b");
    let ws_c_dir = TempDir::new().expect("tempdir c");
    let ws_a = ws_a_dir.path().to_path_buf();
    let ws_b = ws_b_dir.path().to_path_buf();
    let ws_c = ws_c_dir.path().to_path_buf();

    // The WS-side canonicalises the headers; the IPC-side
    // canonicalises the frame's workspace_root (per the task #12
    // canonicalize-symmetry fix). The known_workspaces field in the
    // WARN log shows the registry's canonical forms. To compare
    // robustly across macOS's /var → /private/var quirk, pre-compute
    // the canonical forms we expect to see in the log AND assert
    // against the raw form for ws_c (which appears verbatim in
    // workspace_root_raw).
    let ws_a_canonical = std::fs::canonicalize(&ws_a).expect("canonicalize a");
    let ws_b_canonical = std::fs::canonicalize(&ws_b).expect("canonicalize b");
    let ws_c_raw = ws_c.display().to_string();

    let (addr, socket_path, _tmp) = start_sidecar().await;

    let mut client_a = ws_connect_with_workspace(addr, &ws_a).await;
    let mut client_b = ws_connect_with_workspace(addr, &ws_b).await;
    prime_ws_client(&mut client_a, 1).await;
    prime_ws_client(&mut client_b, 2).await;

    // IPC at_mention pointing at a workspace neither client has.
    let file_path = format!("{}/x.rs", ws_c.display());
    send_ipc_at_mention(&socket_path, &file_path, 0, 0, Some(&ws_c), None).await;

    // (a) Neither client receives a frame within 500 ms.
    assert_no_text_within(&mut client_a, Duration::from_millis(500), "client A").await;
    assert_no_text_within(&mut client_b, Duration::from_millis(500), "client B").await;

    // (b) The sidecar logs a NoMatch WARN. The exact message string
    // is the load-bearing assertion; the surrounding tracing fields
    // are positional matches.
    let snapshot = wait_for_log_substring(
        &log_buf,
        "no matching client; dropping at_mention",
        Duration::from_millis(500),
    )
    .await
    .expect("WARN 'no matching client; dropping at_mention' SHALL be logged");

    // (c) The WARN line MUST include the file path so an operator
    // can identify which user action was dropped.
    assert!(
        snapshot.contains(&file_path),
        "WARN SHALL include the file path; logs:\n{snapshot}"
    );

    // (d) The WARN line MUST include ws_c — appears in
    // `workspace_root_raw` per the #12 canonicalize-fix log upgrade.
    // We assert on the raw form because the canonical form may
    // differ on macOS (the canonical form should also appear in the
    // canonical field but is more host-dependent to assert on).
    assert!(
        snapshot.contains(&ws_c_raw),
        "WARN SHALL include the frame's workspace_root in the raw form ({ws_c_raw}); logs:\n{snapshot}"
    );

    // (e) The WARN line MUST list both A's and B's workspaces in
    // `known_workspaces`. The registry stores canonical forms (set by
    // the WS-side header capture) so the log's known_workspaces
    // shows canonical paths.
    let ws_a_canonical_str = ws_a_canonical.display().to_string();
    let ws_b_canonical_str = ws_b_canonical.display().to_string();
    assert!(
        snapshot.contains(&ws_a_canonical_str),
        "WARN's known_workspaces SHALL include A's canonical workspace ({ws_a_canonical_str}); logs:\n{snapshot}"
    );
    assert!(
        snapshot.contains(&ws_b_canonical_str),
        "WARN's known_workspaces SHALL include B's canonical workspace ({ws_b_canonical_str}); logs:\n{snapshot}"
    );

    // (f) Belt-and-braces: the line MUST be at WARN level (defends
    // against fmt-format drift if the message ever appears at a
    // different level).
    assert!(
        snapshot.contains(" WARN ") || snapshot.contains("WARN "),
        "log line SHALL be at WARN level; logs:\n{snapshot}"
    );

    let _ = client_a.close(None).await;
    let _ = client_b.close(None).await;
}

// ---------------------------------------------------------------------------
// Task #15: ambiguous reply, follow-up frame routes via client_id
// ---------------------------------------------------------------------------

/// Open a persistent IPC connection and write a single at_mention
/// frame. Returns the open stream split into a buffered reader and
/// the write half so the caller can drive a multi-frame round-trip
/// (e.g. read the `Ambiguous` reply and write a follow-up with
/// `client_id`).
///
/// CRITICAL for task #15 / `ipc/spec.md` "IPC connection lifetime for
/// at_mention round-trips": the connection MUST stay open after the
/// first write so the sidecar can write the Ambiguous reply back and
/// the helper can write its picker-selected follow-up. The simpler
/// `send_ipc_at_mention` helper closes after one write and is NOT
/// suitable for the picker round-trip.
async fn ipc_open_and_write_at_mention(
    socket_path: &std::path::Path,
    file_path: &str,
    line_start: u32,
    line_end: u32,
    workspace_root: Option<&std::path::Path>,
    client_id: Option<uuid::Uuid>,
) -> (
    tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let stream = UnixStream::connect(socket_path).await.expect("ipc connect");
    let (read_half, mut write_half) = stream.into_split();
    let mut frame = serde_json::json!({
        "type": "at_mention",
        "file_path": file_path,
        "line_start": line_start,
        "line_end": line_end,
    });
    if let Some(w) = workspace_root {
        frame["workspace_root"] = serde_json::Value::String(w.display().to_string());
    }
    if let Some(id) = client_id {
        frame["client_id"] = serde_json::Value::String(id.hyphenated().to_string());
    }
    let mut bytes = serde_json::to_vec(&frame).expect("serialise");
    bytes.push(b'\n');
    write_half.write_all(&bytes).await.expect("ipc write");
    write_half.flush().await.expect("ipc flush");
    (tokio::io::BufReader::new(read_half), write_half)
}

/// Read one `\n`-terminated line from the IPC connection, with a
/// bounded timeout. Returns `Some(line_without_trailing_newline)` on
/// success, `None` on timeout / EOF.
async fn ipc_read_line(
    reader: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    within: Duration,
) -> Option<String> {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    match timeout(within, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => None, // EOF
        Ok(Ok(_)) => {
            // Strip trailing newline (and optional \r before it).
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            Some(line)
        }
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Write a follow-up at_mention frame on an already-open IPC
/// connection. Used by the picker round-trip's second leg.
async fn ipc_write_at_mention_followup(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    file_path: &str,
    line_start: u32,
    line_end: u32,
    workspace_root: &std::path::Path,
    client_id: uuid::Uuid,
) {
    let frame = serde_json::json!({
        "type": "at_mention",
        "file_path": file_path,
        "line_start": line_start,
        "line_end": line_end,
        "workspace_root": workspace_root.display().to_string(),
        "client_id": client_id.hyphenated().to_string(),
    });
    let mut bytes = serde_json::to_vec(&frame).expect("serialise");
    bytes.push(b'\n');
    write_half.write_all(&bytes).await.expect("ipc write");
    write_half.flush().await.expect("ipc flush");
}

#[tokio::test(flavor = "current_thread")]
async fn ambiguous_workspace_yields_ambiguous_reply_with_two_candidates() {
    // Spec scenario (ipc/spec.md "Two clients in same workspace produce
    // an ambiguous reply"; tasks.md §8.2):
    //   GIVEN two authorized WS clients A and B both with the SAME
    //         workspace (via x-claude-code-workspace header).
    //   WHEN  a helper opens an IPC connection and writes an
    //         `at_mention` for that workspace with NO client_id.
    //   THEN  neither WS client receives an `at_mentioned` within 500ms.
    //   AND   the sidecar writes ONE line on the SAME IPC connection
    //         parseable as `IpcFrame::Ambiguous` with exactly two
    //         candidates whose `client_id`s are non-equal UUIDs.
    //   AND   the two `label` strings are distinct, non-empty, and each
    //         contains a 1-based ordinal + a humanised elapsed phrase
    //         (per the "Ambiguous candidate label content" requirement
    //         from notifications/spec.md and router::build_label).
    let ws_dir = TempDir::new().expect("tempdir");
    let ws = ws_dir.path().to_path_buf();

    let (addr, socket_path, _tmp) = start_sidecar().await;

    let mut client_a = ws_connect_with_workspace(addr, &ws).await;
    let mut client_b = ws_connect_with_workspace(addr, &ws).await;
    prime_ws_client(&mut client_a, 1).await;
    prime_ws_client(&mut client_b, 2).await;

    // Open a PERSISTENT IPC connection and write the first frame.
    let file_path = format!("{}/x.rs", ws.display());
    let (mut reader, write_half) =
        ipc_open_and_write_at_mention(&socket_path, &file_path, 0, 0, Some(&ws), None).await;

    // (a) Read exactly one line from the SAME connection — the
    // sidecar's Ambiguous reply. Bounded timeout per the team-lead
    // spec note "expect a reply within ~50ms; 500ms is a generous bound".
    let reply_line = ipc_read_line(&mut reader, Duration::from_millis(500))
        .await
        .expect("Ambiguous reply line within 500ms");
    let reply_v: Value = serde_json::from_str(&reply_line).expect("Ambiguous reply parses as JSON");
    assert_eq!(
        reply_v["type"], "ambiguous",
        "reply frame's `type` SHALL be 'ambiguous'"
    );
    let candidates = reply_v["candidates"]
        .as_array()
        .expect("`candidates` SHALL be a JSON array");
    assert_eq!(
        candidates.len(),
        2,
        "two clients in the same workspace SHALL produce two candidates"
    );

    // (b) Each candidate carries a valid UUID v4 string. The two
    // UUIDs are non-equal. The `client_id` field's wire form is the
    // lowercase 36-character hyphenated UUID (per protocol/spec.md
    // "AmbiguousCandidate shape").
    let id0 = candidates[0]["client_id"]
        .as_str()
        .expect("candidate[0].client_id SHALL be a string");
    let id1 = candidates[1]["client_id"]
        .as_str()
        .expect("candidate[1].client_id SHALL be a string");
    let id0_uuid = uuid::Uuid::parse_str(id0).expect("candidate[0].client_id SHALL parse as UUID");
    let id1_uuid = uuid::Uuid::parse_str(id1).expect("candidate[1].client_id SHALL parse as UUID");
    assert_ne!(
        id0_uuid, id1_uuid,
        "the two candidates SHALL have distinct client_ids"
    );

    // (c) Labels are distinct, non-empty, contain a 1-based ordinal
    // and a humanised elapsed-time phrase (`Ns` / `Nms` / `Nm` / `Nh`).
    // Lock in the router's `build_label` contract from #6 §3.3.
    let label0 = candidates[0]["label"]
        .as_str()
        .expect("candidate[0].label SHALL be a string");
    let label1 = candidates[1]["label"]
        .as_str()
        .expect("candidate[1].label SHALL be a string");
    assert!(!label0.is_empty() && !label1.is_empty(), "labels non-empty");
    assert_ne!(label0, label1, "the two labels SHALL differ");
    assert!(
        label0.contains('1'),
        "label0 SHALL include 1-based ordinal '1'; got: {label0}"
    );
    assert!(
        label1.contains('2'),
        "label1 SHALL include 1-based ordinal '2'; got: {label1}"
    );
    let has_unit = |s: &str| s.contains("ms") || s.contains('s') || s.contains('m');
    assert!(
        has_unit(label0),
        "label0 SHALL include an elapsed-time unit; got: {label0}"
    );
    assert!(
        has_unit(label1),
        "label1 SHALL include an elapsed-time unit; got: {label1}"
    );

    // (d) Both `connected_at_ms_ago` and `last_activity_ms_ago` are
    // non-negative integers (the protocol layer's u64 type rejects
    // negatives at parse time, so a successful parse plus a positive
    // assertion is the right test).
    for (i, c) in candidates.iter().enumerate() {
        let c_at = c["connected_at_ms_ago"]
            .as_u64()
            .unwrap_or_else(|| panic!("candidate[{i}].connected_at_ms_ago SHALL be u64"));
        let a_ago = c["last_activity_ms_ago"]
            .as_u64()
            .unwrap_or_else(|| panic!("candidate[{i}].last_activity_ms_ago SHALL be u64"));
        // Bonus reasonableness check: the timestamps were taken < 1s
        // ago at registry insert, so values are well below 60_000ms.
        assert!(
            c_at < 60_000,
            "candidate[{i}].connected_at_ms_ago seems unreasonable: {c_at}"
        );
        assert!(
            a_ago < 60_000,
            "candidate[{i}].last_activity_ms_ago seems unreasonable: {a_ago}"
        );
    }

    // (e) Neither WS client SHALL have received an `at_mentioned` yet
    // — the sidecar is awaiting our follow-up.
    assert_no_text_within(&mut client_a, Duration::from_millis(500), "client A").await;
    assert_no_text_within(&mut client_b, Duration::from_millis(500), "client B").await;

    // Clean up: drop the IPC connection (no follow-up). The sidecar's
    // read loop will see EOF on the next read_line and exit gracefully.
    drop(write_half);
    drop(reader);

    let _ = client_a.close(None).await;
    let _ = client_b.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn ambiguous_followup_with_client_id_routes_to_picked_client_only() {
    // Spec scenario (ipc/spec.md "Follow-up frame routes to the picked
    // client"; tasks.md §8.3):
    //   GIVEN the sidecar has just written an Ambiguous reply listing
    //         two candidates A and B on the IPC connection.
    //   WHEN  the helper writes a follow-up `at_mention` on the SAME
    //         IPC connection with `client_id` set to ONE of the two
    //         candidates.
    //   THEN  the chosen WS client SHALL receive `at_mentioned` within
    //         200ms.
    //   AND   the OTHER WS client SHALL receive nothing within 500ms.
    //
    // The test cannot a priori map "candidates[0]" to client_a or
    // client_b — that mapping is determined by the registry's
    // HashMap iteration order, which is randomised across runs.
    // We assert exactly-one-delivered: the candidate UUID we picked
    // identifies ONE WS client, and only that client should receive
    // the notification. Whether it happens to be client_a or
    // client_b is irrelevant to the spec.
    let ws_dir = TempDir::new().expect("tempdir");
    let ws = ws_dir.path().to_path_buf();

    let (addr, socket_path, _tmp) = start_sidecar().await;

    let mut client_a = ws_connect_with_workspace(addr, &ws).await;
    let mut client_b = ws_connect_with_workspace(addr, &ws).await;
    prime_ws_client(&mut client_a, 1).await;
    prime_ws_client(&mut client_b, 2).await;

    // First leg: same as the previous test — open connection, send
    // initial at_mention, read the Ambiguous reply. Extract one of
    // the candidate UUIDs for the follow-up.
    let file_path = format!("{}/main.rs", ws.display());
    let (mut reader, mut write_half) =
        ipc_open_and_write_at_mention(&socket_path, &file_path, 9, 19, Some(&ws), None).await;

    let reply_line = ipc_read_line(&mut reader, Duration::from_millis(500))
        .await
        .expect("Ambiguous reply within 500ms");
    let reply_v: Value = serde_json::from_str(&reply_line).expect("Ambiguous reply parses");
    assert_eq!(reply_v["type"], "ambiguous");
    let candidates = reply_v["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 2);

    // Pick the first candidate. Deterministic within one test run,
    // but maps to either A or B non-deterministically across runs.
    let picked_uuid_str = candidates[0]["client_id"]
        .as_str()
        .expect("picked client_id");
    let picked_uuid = uuid::Uuid::parse_str(picked_uuid_str).expect("uuid");
    let other_uuid_str = candidates[1]["client_id"]
        .as_str()
        .expect("other client_id");
    assert_ne!(picked_uuid_str, other_uuid_str);

    // Second leg: write the follow-up frame on the SAME connection
    // (`write_half` is still alive). This is the spec contract from
    // ipc/spec.md "IPC connection lifetime for at_mention round-trips".
    ipc_write_at_mention_followup(&mut write_half, &file_path, 9, 19, &ws, picked_uuid).await;

    // (a) Exactly ONE of {client_a, client_b} SHALL receive the
    // at_mentioned within 200ms. We don't know in advance which.
    let a_frame_within_200ms = recv_text(&mut client_a, Duration::from_millis(200)).await;
    let b_frame_within_200ms = recv_text(&mut client_b, Duration::from_millis(200)).await;

    let received_count =
        a_frame_within_200ms.is_some() as usize + b_frame_within_200ms.is_some() as usize;
    assert_eq!(
        received_count,
        1,
        "exactly one client SHALL receive the routed at_mentioned (got a={:?}, b={:?})",
        a_frame_within_200ms.as_deref(),
        b_frame_within_200ms.as_deref()
    );

    let received_frame = a_frame_within_200ms
        .or(b_frame_within_200ms)
        .expect("one frame");
    let v: Value = serde_json::from_str(&received_frame).expect("json");
    assert_eq!(v["method"], "at_mentioned");
    assert_eq!(v["params"]["filePath"], file_path);
    assert_eq!(v["params"]["lineStart"], 10, "1-indexed (9+1)");
    assert_eq!(v["params"]["lineEnd"], 20, "1-indexed (19+1)");

    // (b) Neither client SHALL receive a SECOND frame within 500ms.
    // We've already drained 200ms from each above; the "exactly one
    // received" invariant + "no further frames" together prove the
    // spec scenario. We don't need to identify the loser — neither
    // side should produce more output.
    assert_no_text_within(
        &mut client_a,
        Duration::from_millis(500),
        "client A (post-follow-up tail check)",
    )
    .await;
    assert_no_text_within(
        &mut client_b,
        Duration::from_millis(500),
        "client B (post-follow-up tail check)",
    )
    .await;

    drop(write_half);
    drop(reader);

    let _ = client_a.close(None).await;
    let _ = client_b.close(None).await;
}
