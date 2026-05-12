//! Integration tests for the peer-process cwd resolver wiring.
//!
//! These tests exercise the `handle_connection → CwdResolver →
//! serve_authorized → registry.insert` chain end-to-end using a
//! `MockCwdResolver`. Real libproc is never invoked — CI on Linux
//! works without modification.
//!
//! See `openspec/changes/peer-cwd-discovery/specs/websocket/spec.md`
//! → **Peer-process cwd discovery** for the contracts asserted here.
//!
//! The clever bit: the mock is keyed on the **peer port** (the
//! client-side ephemeral port of the accepted TCP socket). To set
//! up the mock BEFORE the server can read it, we:
//!
//! 1. `TcpListener::bind("127.0.0.1:0")` — gets a bound address but
//!    the accept loop hasn't started.
//! 2. `TcpStream::connect(addr)` — TCP three-way handshake completes
//!    on the kernel level even before the server calls `.accept()`,
//!    because the kernel queues backlog. The stream now has a known
//!    `local_addr().port()`.
//! 3. Build the `MockCwdResolver` with the mapping
//!    `{ stream.local_addr().port() -> /tmp/expected-ws }` and feed
//!    it into the `Transport` via the builder.
//! 4. Spawn `transport.run(listener)` — accept loop pops the queued
//!    TCP connection and the resolver lookup hits the mock.
//! 5. Drive the WebSocket handshake on the connected stream; assert
//!    the registry entry's `workspace_root` matches.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use http::header::HeaderValue;
use zed_claude_bridge::mcp::EditorState;
use zed_claude_bridge::transport::{AuthToken, MockCwdResolver, Transport};

const TEST_TOKEN: &str = "peer-cwd-token-deadbeefcafe";

/// Build a Transport seeded with the given port→path mock map and
/// start its accept loop. Returns the bound `SocketAddr` and the
/// `Transport` handle (used for `registry().snapshot()`).
async fn start_transport_with_mock(
    map: HashMap<u16, PathBuf>,
    listener: TcpListener,
) -> (std::net::SocketAddr, Transport) {
    let addr = listener.local_addr().unwrap();
    let auth = AuthToken::new(TEST_TOKEN);
    let state = Arc::new(RwLock::new(EditorState::new()));

    let mut mock = MockCwdResolver::new();
    for (port, path) in map {
        mock.insert(port, path);
    }

    let transport = Transport::builder(auth, state.clone())
        .with_cwd_resolver(Arc::new(mock))
        .build();

    let server_handle = transport.clone();
    tokio::spawn(async move {
        let _ = server_handle.run(listener).await;
    });

    // Give the accept loop a beat to start polling.
    sleep(Duration::from_millis(20)).await;

    (addr, transport)
}

/// Drive the WS handshake on `stream` with the auth header but NO
/// `x-claude-code-workspace` header — so the priority-1 path doesn't
/// fire and the resolver (priority 2) is the only candidate.
async fn ws_handshake_authorized_no_workspace_header(
    stream: TcpStream,
    addr: std::net::SocketAddr,
) -> tokio_tungstenite::WebSocketStream<TcpStream> {
    let url = format!("ws://{addr}/");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "x-claude-code-ide-authorization",
        HeaderValue::from_str(TEST_TOKEN).unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("ws upgrade");
    ws
}

/// Send a minimal `initialize` (no `clientInfo.cwd`) so the server's
/// `dispatch_text` runs at least once and any priority-3 fallthrough
/// would be observable in the registry. We then wait for the reply
/// to ensure the server has processed it before we snapshot the
/// registry.
async fn drive_initialize(ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>) {
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "peer-cwd-test", "version": "0.0.0" }
        }
    });
    ws.send(Message::text(init.to_string())).await.unwrap();

    // Wait for the initialize reply (any text message) to ensure
    // the server has applied the priority chain. 1s budget — the
    // resolver itself is capped at 250 ms.
    let _reply = timeout(Duration::from_secs(1), ws.next())
        .await
        .expect("server replies within 1 second")
        .expect("stream has a frame")
        .expect("frame is Ok");
}

// ---------------------------------------------------------------------------
// Spec scenarios
// ---------------------------------------------------------------------------

/// Spec: **Peer-cwd applied when header is absent.**
///
/// Single client, no workspace header, mock returns
/// `Some(/tmp-some/expected-ws)`. Assert that after the handshake +
/// `initialize` round-trip, the registry entry's `workspace_root` is
/// the canonical form of that path.
#[tokio::test(flavor = "current_thread")]
async fn single_client_peer_cwd_populates_workspace_root_from_mock() {
    // 1. Bind the listener but don't start the server yet — we need
    //    to know the client's peer port BEFORE building the mock.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // 2. Connect a TCP stream. The kernel completes the three-way
    //    handshake and queues the connection in the listener's
    //    backlog; the server will pop it on its next `.accept()`.
    let stream = TcpStream::connect(addr).await.expect("tcp connect");
    let peer_port = stream.local_addr().unwrap().port();

    // 3. Set up the mock to return our expected cwd for THIS peer port.
    let tmp = TempDir::new().expect("tempdir");
    let expected_ws = tmp.path().to_path_buf();
    let mut map = HashMap::new();
    map.insert(peer_port, expected_ws.clone());

    // 4. Start the Transport with the seeded mock. The accept loop
    //    pops the queued TCP connection; `handle_connection` calls
    //    `cwd_resolver.resolve(peer)` where `peer.port() == peer_port`.
    let (_addr, transport) = start_transport_with_mock(map, listener).await;

    // 5. Drive the WS handshake (no workspace header).
    let mut ws = ws_handshake_authorized_no_workspace_header(stream, addr).await;
    drive_initialize(&mut ws).await;

    // 6. Snapshot the registry and assert.
    let snap = transport.registry().snapshot().await;
    assert_eq!(snap.len(), 1, "exactly one client should be registered");
    let entry = &snap[0];
    let expected_canonical = std::fs::canonicalize(&expected_ws).unwrap_or(expected_ws);
    assert_eq!(
        entry.workspace_root.as_deref(),
        Some(expected_canonical.as_path()),
        "workspace_root SHALL be the canonical form of the mock's mapped cwd"
    );

    // 7. Clean shutdown.
    let _ = ws.close(None).await;
}

/// Spec: **Mock resolver scripted by tests** — extended to two
/// distinct ports → two distinct cwds, exercised end-to-end through
/// the accept loop. Asserts both clients end up in the registry with
/// the resolver's per-port mapping.
#[tokio::test(flavor = "current_thread")]
async fn two_clients_get_distinct_workspace_roots_from_mock() {
    // 1. Bind one shared listener — both clients connect to the
    //    same sidecar.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // 2. Pre-connect two TCP streams. The kernel will queue both in
    //    the listener's backlog.
    let stream_a = TcpStream::connect(addr).await.expect("tcp a");
    let port_a = stream_a.local_addr().unwrap().port();
    let stream_b = TcpStream::connect(addr).await.expect("tcp b");
    let port_b = stream_b.local_addr().unwrap().port();
    assert_ne!(
        port_a, port_b,
        "OS must assign distinct ephemeral ports to the two clients"
    );

    // 3. Seed the mock with both mappings BEFORE the server runs.
    let tmp_a = TempDir::new().expect("tempdir a");
    let tmp_b = TempDir::new().expect("tempdir b");
    let mut map = HashMap::new();
    map.insert(port_a, tmp_a.path().to_path_buf());
    map.insert(port_b, tmp_b.path().to_path_buf());

    let (_addr, transport) = start_transport_with_mock(map, listener).await;

    // 4. Drive the handshake for BOTH clients (sequentially — the
    //    sidecar's accept loop will pop both from the queue).
    let mut ws_a = ws_handshake_authorized_no_workspace_header(stream_a, addr).await;
    drive_initialize(&mut ws_a).await;
    let mut ws_b = ws_handshake_authorized_no_workspace_header(stream_b, addr).await;
    drive_initialize(&mut ws_b).await;

    // 5. Snapshot the registry. Both entries should be present, each
    //    with the correct workspace.
    let snap = transport.registry().snapshot().await;
    assert_eq!(snap.len(), 2, "two clients should be registered");

    let canon_a =
        std::fs::canonicalize(tmp_a.path()).unwrap_or_else(|_| tmp_a.path().to_path_buf());
    let canon_b =
        std::fs::canonicalize(tmp_b.path()).unwrap_or_else(|_| tmp_b.path().to_path_buf());

    let registry_workspaces: Vec<PathBuf> = snap
        .iter()
        .filter_map(|c| c.workspace_root.clone())
        .collect();
    assert!(
        registry_workspaces.contains(&canon_a),
        "registry should contain client A's mock cwd; got {registry_workspaces:?}"
    );
    assert!(
        registry_workspaces.contains(&canon_b),
        "registry should contain client B's mock cwd; got {registry_workspaces:?}"
    );

    let _ = ws_a.close(None).await;
    let _ = ws_b.close(None).await;
}

/// Spec: **Workspace header takes priority over peer-cwd.**
///
/// Header WINS even when the resolver would have returned a value.
/// We seed the mock with `port -> /tmp-mock`, but the client sends
/// `x-claude-code-workspace: /tmp-header`. The registry must show
/// the header path, NOT the mock path.
#[tokio::test(flavor = "current_thread")]
async fn workspace_header_wins_over_peer_cwd_resolver() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let stream = TcpStream::connect(addr).await.expect("tcp connect");
    let peer_port = stream.local_addr().unwrap().port();

    // Mock value DIFFERS from header value.
    let tmp_mock = TempDir::new().expect("tempdir mock");
    let tmp_header = TempDir::new().expect("tempdir header");
    let mut map = HashMap::new();
    map.insert(peer_port, tmp_mock.path().to_path_buf());

    let (_addr, transport) = start_transport_with_mock(map, listener).await;

    // Drive a handshake that DOES carry the workspace header.
    let mut req = format!("ws://{addr}/").into_client_request().unwrap();
    req.headers_mut().insert(
        "x-claude-code-ide-authorization",
        HeaderValue::from_str(TEST_TOKEN).unwrap(),
    );
    req.headers_mut().insert(
        "x-claude-code-workspace",
        HeaderValue::from_str(tmp_header.path().to_str().unwrap()).unwrap(),
    );
    let (mut ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("ws upgrade");
    drive_initialize(&mut ws).await;

    let snap = transport.registry().snapshot().await;
    assert_eq!(snap.len(), 1);
    let entry = &snap[0];
    let canon_header = std::fs::canonicalize(tmp_header.path())
        .unwrap_or_else(|_| tmp_header.path().to_path_buf());
    let canon_mock =
        std::fs::canonicalize(tmp_mock.path()).unwrap_or_else(|_| tmp_mock.path().to_path_buf());
    assert_eq!(
        entry.workspace_root.as_deref(),
        Some(canon_header.as_path()),
        "workspace_root SHALL be the canonical form of the header value (priority 1)"
    );
    assert_ne!(
        entry.workspace_root.as_deref(),
        Some(canon_mock.as_path()),
        "workspace_root MUST NOT come from the resolver when the header is present"
    );

    let _ = ws.close(None).await;
}

/// Spec: **Resolver does not stall the accept loop.**
///
/// The first connection's resolver-call wait must not block the
/// second connection's handshake. With the `MockCwdResolver` this is
/// trivially true (the mock is synchronous), so to actually exercise
/// the non-blocking guarantee we'd need an async-pause mock. For now
/// this test is a behavioural canary: two clients connect, the
/// second client's handshake completes within a tight time budget
/// (1 second; the resolver itself is bounded at 250 ms).
#[tokio::test(flavor = "current_thread")]
async fn second_connection_handshake_does_not_block_on_first_resolver() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let stream_a = TcpStream::connect(addr).await.expect("tcp a");
    let port_a = stream_a.local_addr().unwrap().port();
    let stream_b = TcpStream::connect(addr).await.expect("tcp b");
    let port_b = stream_b.local_addr().unwrap().port();

    let tmp_a = TempDir::new().expect("tempdir a");
    let tmp_b = TempDir::new().expect("tempdir b");
    let mut map = HashMap::new();
    map.insert(port_a, tmp_a.path().to_path_buf());
    map.insert(port_b, tmp_b.path().to_path_buf());

    let (_addr, _transport) = start_transport_with_mock(map, listener).await;

    // Both handshakes must complete well inside a single
    // resolver-budget. With current_thread runtime and synchronous
    // mock, anything blocking the accept loop would manifest as the
    // second handshake hanging.
    let both = timeout(Duration::from_secs(2), async {
        let mut ws_a = ws_handshake_authorized_no_workspace_header(stream_a, addr).await;
        let mut ws_b = ws_handshake_authorized_no_workspace_header(stream_b, addr).await;
        drive_initialize(&mut ws_a).await;
        drive_initialize(&mut ws_b).await;
        (ws_a, ws_b)
    })
    .await
    .expect("both handshakes complete within 2s — accept loop did not stall");

    let (mut ws_a, mut ws_b) = both;
    let _ = ws_a.close(None).await;
    let _ = ws_b.close(None).await;
}

/// Sanity check: the `initialize` reply parses as JSON-RPC. This
/// guards against accidental regressions in the dispatch path that
/// might otherwise be masked by the registry-snapshot assertions.
#[tokio::test(flavor = "current_thread")]
async fn initialize_response_is_valid_json_rpc() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stream = TcpStream::connect(addr).await.expect("tcp connect");

    // No mock seed needed — we just want to confirm the dispatch path.
    let map = HashMap::new();
    let (_addr, _transport) = start_transport_with_mock(map, listener).await;

    let mut ws = ws_handshake_authorized_no_workspace_header(stream, addr).await;

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "peer-cwd-test", "version": "0.0.0" }
        }
    });
    ws.send(Message::text(init.to_string())).await.unwrap();

    let reply = timeout(Duration::from_secs(1), ws.next())
        .await
        .expect("reply within 1s")
        .expect("stream has frame")
        .expect("frame is Ok");

    match reply {
        Message::Text(s) => {
            let v: Value = serde_json::from_str(&s).expect("JSON-RPC reply parses as JSON");
            assert_eq!(v["jsonrpc"], "2.0");
            assert_eq!(v["id"], 1);
        }
        other => panic!("expected text reply, got {other:?}"),
    }

    let _ = ws.close(None).await;
}
