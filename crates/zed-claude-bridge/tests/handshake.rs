//! Integration tests for the WebSocket transport — auth gate, multi-client
//! registry, full MCP handshake, and "sidecar survives client disconnect".
//!
//! Each test stands up a `Transport` against an in-process `tokio` accept
//! loop on `127.0.0.1:0` (kernel-assigned port; the spec's `[10000,65535]`
//! range only applies to the production sidecar, not handshake unit tests).
//!
//! Per the OpenSpec `session-routing` change, the prior single-client
//! displacement policy is REMOVED. Tests that previously asserted the
//! displacement behaviour are inverted: a second authorized client SHALL
//! coexist with the first. A new test exercises three simultaneous
//! authorized clients.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http::header::HeaderValue;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use zed_claude_bridge::mcp::EditorState;
use zed_claude_bridge::protocol::Notification as JsonRpcNotification;
use zed_claude_bridge::transport::{AuthToken, NoopCwdResolver, Transport};

const TEST_TOKEN: &str = "test-token-deadbeefcafe";

/// Spawn a transport on `127.0.0.1:0` and return `(addr, transport)`.
///
/// Tests that exercise priority-3 (`clientInfo.cwd`) and priority-4
/// (daemon `--workspace`) of the workspace-identification chain
/// MUST inject `NoopCwdResolver` for the peer-cwd resolver
/// (priority 2). If they don't, the production default on macOS is
/// `LibprocCwdResolver`, which correctly resolves the test process's
/// cwd at WebSocket-accept time — and that pre-empts priority 3/4,
/// breaking the test's assumption that those branches will fire.
/// This is also the team-lead's hard rule: "Test isolation: any
/// test that lights up a real WS connection must use Mock or skip
/// the resolver entirely. No CI test should invoke libproc."
async fn start_transport() -> (std::net::SocketAddr, Transport) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let auth = AuthToken::new(TEST_TOKEN);
    let state = Arc::new(RwLock::new(EditorState::new()));
    let transport = Transport::builder(auth, state)
        .with_cwd_resolver(Arc::new(NoopCwdResolver::new()))
        .build();

    let server_handle = transport.clone();
    tokio::spawn(async move {
        let _ = server_handle.run(listener).await;
    });

    // Give the accept loop a beat to start polling.
    sleep(Duration::from_millis(20)).await;

    (addr, transport)
}

/// Spawn a transport with a `--workspace`-style daemon fallback so we
/// can exercise priority-4 of the workspace-identification chain.
///
/// Same `NoopCwdResolver` injection as [`start_transport`] above.
async fn start_transport_with_daemon_workspace(
    daemon_workspace: std::path::PathBuf,
) -> (std::net::SocketAddr, Transport) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let auth = AuthToken::new(TEST_TOKEN);
    let state = Arc::new(RwLock::new(EditorState::new()));
    let transport = Transport::builder(auth, state)
        .with_daemon_workspace(daemon_workspace)
        .with_cwd_resolver(Arc::new(NoopCwdResolver::new()))
        .build();

    let server_handle = transport.clone();
    tokio::spawn(async move {
        let _ = server_handle.run(listener).await;
    });

    sleep(Duration::from_millis(20)).await;

    (addr, transport)
}

/// Open a TCP stream and drive a WebSocket client handshake against `addr`,
/// optionally including the auth header.
async fn connect_with_token(
    addr: &std::net::SocketAddr,
    token: Option<&str>,
) -> WebSocketStream<TcpStream> {
    let stream = TcpStream::connect(addr).await.expect("tcp connect");
    let url = format!("ws://{addr}/");
    let mut req = url.into_client_request().unwrap();
    if let Some(tok) = token {
        req.headers_mut().insert(
            "x-claude-code-ide-authorization",
            HeaderValue::from_str(tok).unwrap(),
        );
    }
    let (ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("client handshake");
    ws
}

// ---------------------------------------------------------------------------
// Auth gate
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn missing_auth_header_is_closed_with_1008() {
    let (addr, _t) = start_transport().await;
    let mut ws = connect_with_token(&addr, None).await;

    let msg = timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("close arrives within 2s")
        .expect("stream had a frame")
        .expect("frame is Ok");

    match msg {
        Message::Close(Some(frame)) => {
            assert_eq!(frame.code, CloseCode::Policy, "want 1008 (Policy)");
        }
        other => panic!("expected close frame, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_auth_token_is_closed_with_1008() {
    let (addr, _t) = start_transport().await;
    let mut ws = connect_with_token(&addr, Some("bogus")).await;

    let msg = timeout(Duration::from_secs(2), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    match msg {
        Message::Close(Some(frame)) => assert_eq!(frame.code, CloseCode::Policy),
        other => panic!("expected close frame, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Full MCP handshake
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn full_mcp_handshake_initialize_then_tools_list() {
    let (addr, _t) = start_transport().await;
    let mut ws = connect_with_token(&addr, Some(TEST_TOKEN)).await;

    // initialize
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(&mut ws).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(v["result"]["serverInfo"]["name"], "zed-claude-bridge");

    // notifications/initialized — no reply expected; we then send tools/list
    // and verify the FIRST response we get is for that, not a stray reply.
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
    ))
    .await
    .unwrap();

    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(&mut ws).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 2, "next reply must correlate to tools/list (id=2)");
    let tools = v["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);

    // ping — empty result.
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":3,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(&mut ws).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 3);
    assert_eq!(v["result"], json!({}));

    let _ = ws.close(None).await;
}

// ---------------------------------------------------------------------------
// Multi-client coexistence (inverted from the prior single-client policy)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn second_authorized_client_coexists_with_first() {
    // Spec scenario: "Two authorized clients coexist" (websocket capability).
    let (addr, _t) = start_transport().await;

    // Client A connects and pings.
    let mut a = connect_with_token(&addr, Some(TEST_TOKEN)).await;
    a.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut a).await;

    // Client B connects.
    let mut b = connect_with_token(&addr, Some(TEST_TOKEN)).await;

    // Per the removed Single-client policy, A must NOT receive a close
    // frame. Wait 200 ms then ensure A can still ping/pong.
    sleep(Duration::from_millis(200)).await;
    a.send(Message::Text(
        json!({"jsonrpc":"2.0","id":2,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(&mut a).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["id"], 2,
        "A SHALL still receive its own ping reply after B connected"
    );

    // B also pings independently.
    b.send(Message::Text(
        json!({"jsonrpc":"2.0","id":42,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(&mut b).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 42);
    assert_eq!(v["result"], json!({}));

    let _ = a.close(None).await;
    let _ = b.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn three_authorized_clients_coexist() {
    // Defensive: extend the two-client scenario to three concurrent
    // authorized clients to catch any subtle "second" vs "Nth" bugs in
    // the registry's insert logic.
    let (addr, _t) = start_transport().await;

    let mut a = connect_with_token(&addr, Some(TEST_TOKEN)).await;
    let mut b = connect_with_token(&addr, Some(TEST_TOKEN)).await;
    let mut c = connect_with_token(&addr, Some(TEST_TOKEN)).await;

    // Give the registry inserts a beat.
    sleep(Duration::from_millis(50)).await;

    for (i, ws) in [&mut a, &mut b, &mut c].into_iter().enumerate() {
        let id = i as i64 + 1;
        ws.send(Message::Text(
            json!({"jsonrpc":"2.0","id":id,"method":"ping"}).to_string(),
        ))
        .await
        .unwrap();
        let resp = recv_text(ws).await;
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["id"], id, "client {} SHALL receive its own reply", id);
    }

    let _ = a.close(None).await;
    let _ = b.close(None).await;
    let _ = c.close(None).await;
}

// ---------------------------------------------------------------------------
// Binary frame ignored
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn binary_frame_produces_no_reply() {
    let (addr, _t) = start_transport().await;
    let mut ws = connect_with_token(&addr, Some(TEST_TOKEN)).await;

    // Send a binary frame followed by a text ping; we should only see the
    // ping response, never a reply for the binary frame.
    ws.send(Message::Binary(vec![1, 2, 3, 4])).await.unwrap();
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":7,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();

    let resp = recv_text(&mut ws).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 7, "first text reply must be the ping (id=7)");

    let _ = ws.close(None).await;
}

// ---------------------------------------------------------------------------
// Registry-driven outbound notification
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn registry_routed_notification_reaches_the_target_client() {
    // Replaces the pre-routing `ipc_notification_via_broadcast_reaches_active_client`
    // test. We push a notification directly into the registry handle's
    // `tx` channel and verify the connected client receives it as a
    // text frame on its WebSocket.
    let (addr, transport) = start_transport().await;
    let registry = transport.registry();

    let mut ws = connect_with_token(&addr, Some(TEST_TOKEN)).await;

    // Drive a ping so we know the client task is in its select loop
    // (i.e. its mpsc::Receiver is being polled).
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut ws).await;

    // Locate the (only) client in the registry and grab its `tx`.
    let snap = registry.snapshot().await;
    assert_eq!(snap.len(), 1, "exactly one client should be registered");
    let tx = snap[0].tx.clone();

    // Push the notification.
    let notif = JsonRpcNotification::new(
        "at_mentioned",
        json!({"filePath": "/p/x.rs", "lineStart": 10, "lineEnd": 20}),
    );
    tx.send(notif).await.expect("registry tx must accept");

    // Receive it as a text frame.
    let frame = recv_text(&mut ws).await;
    let v: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(v["method"], "at_mentioned");
    assert_eq!(v["params"]["filePath"], "/p/x.rs");
    assert_eq!(v["params"]["lineStart"], 10);
    assert!(v.get("id").is_none(), "notifications carry no id");

    let _ = ws.close(None).await;
}

// ---------------------------------------------------------------------------
// Sidecar survives disconnect
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn sidecar_keeps_running_after_client_disconnect() {
    let (addr, _t) = start_transport().await;
    {
        let mut ws = connect_with_token(&addr, Some(TEST_TOKEN)).await;
        ws.send(Message::Text(
            json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
        ))
        .await
        .unwrap();
        let _ = recv_text(&mut ws).await;
        // Clean close.
        let _ = ws.close(None).await;
    }

    // Give the server a moment to vacate the slot.
    sleep(Duration::from_millis(50)).await;

    // A fresh connection must succeed.
    let mut ws2 = connect_with_token(&addr, Some(TEST_TOKEN)).await;
    ws2.send(Message::Text(
        json!({"jsonrpc":"2.0","id":99,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(&mut ws2).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 99);

    let _ = ws2.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn client_removed_from_registry_on_disconnect() {
    // Spec scenario: "Client disconnect leaves the registry consistent"
    // (websocket capability).
    let (addr, transport) = start_transport().await;
    let registry = transport.registry();

    let mut a = connect_with_token(&addr, Some(TEST_TOKEN)).await;
    let mut b = connect_with_token(&addr, Some(TEST_TOKEN)).await;

    // Drive a ping on each so the registry inserts have completed.
    for (id, ws) in [(1, &mut a), (2, &mut b)] {
        ws.send(Message::Text(
            json!({"jsonrpc":"2.0","id":id,"method":"ping"}).to_string(),
        ))
        .await
        .unwrap();
        let _ = recv_text(ws).await;
    }
    assert_eq!(registry.len().await, 2, "both clients registered");

    // A closes cleanly.
    let _ = a.close(None).await;
    drop(a);

    // Give the server a moment to remove the entry.
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        registry.len().await,
        1,
        "registry SHALL contain only B after A's disconnect"
    );

    // B still works.
    b.send(Message::Text(
        json!({"jsonrpc":"2.0","id":3,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(&mut b).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 3);

    let _ = b.close(None).await;
}

// ---------------------------------------------------------------------------
// Workspace header capture
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn workspace_header_populates_registry_workspace_root() {
    // Spec scenario: "Workspace header parsed and stored" (protocol /
    // websocket capabilities). We can't easily canonicalize the path
    // to a known file system entity inside a test, but we can assert
    // that the registry entry's `workspace_root` is Some(...) after a
    // client supplies the header — i.e. the read happens at all.
    let (addr, transport) = start_transport().await;
    let registry = transport.registry();

    let stream = TcpStream::connect(&addr).await.expect("tcp connect");
    let url = format!("ws://{addr}/");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "x-claude-code-ide-authorization",
        HeaderValue::from_str(TEST_TOKEN).unwrap(),
    );
    req.headers_mut().insert(
        "x-claude-code-workspace",
        HeaderValue::from_str("/tmp/ws-routing-test-handshake").unwrap(),
    );
    let (mut ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("client handshake");

    // Drive a ping so the insert has completed.
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut ws).await;

    let snap = registry.snapshot().await;
    assert_eq!(snap.len(), 1);
    let ws_root = snap[0]
        .workspace_root
        .as_deref()
        .expect("workspace header SHALL populate the registry entry's workspace_root");
    // canonicalize may have failed (the path doesn't exist on the
    // test host); accept either the canonical form or the raw value.
    assert!(
        ws_root
            .to_string_lossy()
            .contains("ws-routing-test-handshake"),
        "workspace_root SHALL include the supplied header value, got {ws_root:?}"
    );

    let _ = ws.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn no_workspace_header_leaves_workspace_root_none() {
    // Spec scenario: "Missing header does not break the handshake"
    let (addr, transport) = start_transport().await;
    let registry = transport.registry();

    let mut ws = connect_with_token(&addr, Some(TEST_TOKEN)).await;
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut ws).await;

    let snap = registry.snapshot().await;
    assert_eq!(snap.len(), 1);
    assert!(
        snap[0].workspace_root.is_none(),
        "workspace_root SHALL remain None when no header was sent"
    );

    let _ = ws.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn client_info_cwd_populates_workspace_root_when_header_absent() {
    // Spec scenario: "clientInfo.cwd used when header absent"
    let (addr, transport) = start_transport().await;
    let registry = transport.registry();

    let mut ws = connect_with_token(&addr, Some(TEST_TOKEN)).await;

    // Send initialize with clientInfo.cwd.
    ws.send(Message::Text(
        json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params": {
                "clientInfo": {
                    "name": "claude",
                    "cwd": "/tmp/ws-cwd-from-initialize"
                }
            }
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut ws).await;

    // Allow the set_workspace call to complete.
    sleep(Duration::from_millis(50)).await;
    let snap = registry.snapshot().await;
    assert_eq!(snap.len(), 1);
    let ws_root = snap[0]
        .workspace_root
        .as_deref()
        .expect("clientInfo.cwd SHALL populate workspace_root");
    assert!(
        ws_root.to_string_lossy().contains("ws-cwd-from-initialize"),
        "workspace_root SHALL include the cwd value, got {ws_root:?}"
    );

    let _ = ws.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_workspace_is_used_as_fallback_when_no_client_side_signal() {
    // Spec scenario (websocket/spec.md L104-111):
    //   GIVEN the sidecar was launched with `--workspace /Users/me/p`
    //   AND a client connects with no workspace header and no clientInfo.cwd
    //   WHEN the connection is registered
    //   THEN the registry entry's workspace_root SHALL be the canonical
    //        form of /Users/me/p.
    //
    // Implementation note: the daemon fallback is applied lazily inside
    // the `initialize` dispatch (after priority-2 clientInfo.cwd has had
    // its chance) — this avoids a brittle path-identity comparison
    // between an eager fallback and the priority-2 capture. Per the
    // verifier-approved Option A: the registry entry briefly has
    // `workspace_root = None` between connect and initialize, but no
    // routing happens during that window (Claude Code sends `initialize`
    // as its first frame post-handshake).
    //
    // We use the test's own tempdir (an existing on-disk path) so the
    // canonicalize call succeeds; the assertion compares against the
    // canonicalised form, not the raw input.
    let tmp = tempfile::tempdir().expect("tempdir");
    let daemon_workspace = tmp.path().to_path_buf();

    let (addr, transport) = start_transport_with_daemon_workspace(daemon_workspace.clone()).await;
    let registry = transport.registry();

    let mut ws = connect_with_token(&addr, Some(TEST_TOKEN)).await;
    // Drive a real `initialize` with NO clientInfo (so priority 2 doesn't
    // fire) — this exercises the priority-3 fallback path.
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}).to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut ws).await;

    // Give the set_workspace call a beat to land.
    sleep(Duration::from_millis(50)).await;

    let snap = registry.snapshot().await;
    assert_eq!(snap.len(), 1);
    let ws_root = snap[0]
        .workspace_root
        .as_deref()
        .expect("daemon --workspace fallback SHALL populate workspace_root after initialize");
    let expected = std::fs::canonicalize(&daemon_workspace).expect("canonicalize tempdir");
    assert_eq!(
        ws_root, expected,
        "workspace_root SHALL be the canonical form of the daemon's --workspace"
    );

    let _ = ws.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn client_info_cwd_overrides_daemon_workspace_fallback() {
    // Defence-in-depth for the priority ordering in `dispatch_text`:
    // a daemon `--workspace` fallback SHALL NOT shadow a priority-2
    // `clientInfo.cwd` signal. Without the priority-aware overwrite
    // logic, the registry entry would silently stay on the fallback
    // because it is no longer `None` at registry-insert time.
    let tmp = tempfile::tempdir().expect("tempdir");
    let daemon_workspace = tmp.path().to_path_buf();

    let (addr, transport) = start_transport_with_daemon_workspace(daemon_workspace.clone()).await;
    let registry = transport.registry();

    let mut ws = connect_with_token(&addr, Some(TEST_TOKEN)).await;
    ws.send(Message::Text(
        json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params": {
                "clientInfo": {
                    "name": "claude",
                    "cwd": "/tmp/ws-cwd-priority-2-wins"
                }
            }
        })
        .to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut ws).await;

    // Allow the set_workspace call to complete.
    sleep(Duration::from_millis(50)).await;
    let snap = registry.snapshot().await;
    assert_eq!(snap.len(), 1);
    let ws_root = snap[0]
        .workspace_root
        .as_deref()
        .expect("workspace_root SHALL be set");
    // Must not equal the daemon fallback; must mention the
    // priority-2 path.
    let daemon_canonical = std::fs::canonicalize(&daemon_workspace).expect("canonicalize");
    assert_ne!(
        ws_root, daemon_canonical,
        "priority-2 clientInfo.cwd SHALL overwrite the priority-3 daemon fallback"
    );
    assert!(
        ws_root.to_string_lossy().contains("ws-cwd-priority-2-wins"),
        "workspace_root SHALL reflect the clientInfo.cwd value, got {ws_root:?}"
    );

    let _ = ws.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn header_overrides_daemon_workspace_fallback() {
    // Spec scenario "Workspace header takes priority" — the
    // x-claude-code-workspace header is priority 1, the daemon
    // --workspace flag is priority 3; the header wins. We can't
    // easily send both a header AND a clientInfo.cwd from a single
    // client task, but header-vs-daemon is the priority-1-vs-3 case
    // that directly exercises the fallback's precedence.
    let tmp = tempfile::tempdir().expect("tempdir");
    let daemon_workspace = tmp.path().to_path_buf();

    let (addr, transport) = start_transport_with_daemon_workspace(daemon_workspace.clone()).await;
    let registry = transport.registry();

    // Build a request with both the auth header and the workspace header.
    let stream = TcpStream::connect(&addr).await.expect("tcp connect");
    let url = format!("ws://{addr}/");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "x-claude-code-ide-authorization",
        HeaderValue::from_str(TEST_TOKEN).unwrap(),
    );
    req.headers_mut().insert(
        "x-claude-code-workspace",
        HeaderValue::from_str("/tmp/ws-header-priority-1-wins").unwrap(),
    );
    let (mut ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("client handshake");

    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut ws).await;

    let snap = registry.snapshot().await;
    assert_eq!(snap.len(), 1);
    let ws_root = snap[0]
        .workspace_root
        .as_deref()
        .expect("workspace_root SHALL be set");
    let daemon_canonical = std::fs::canonicalize(&daemon_workspace).expect("canonicalize");
    assert_ne!(
        ws_root, daemon_canonical,
        "priority-1 header SHALL overwrite the priority-3 daemon fallback"
    );
    assert!(
        ws_root
            .to_string_lossy()
            .contains("ws-header-priority-1-wins"),
        "workspace_root SHALL reflect the header value, got {ws_root:?}"
    );

    let _ = ws.close(None).await;
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

async fn recv_text<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let msg = timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("recv times out")
            .expect("stream had a frame")
            .expect("frame ok");
        match msg {
            Message::Text(s) => return s,
            // Skip control frames.
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => panic!("unexpected close while waiting for text"),
            Message::Binary(_) | Message::Frame(_) => panic!("unexpected non-text frame"),
        }
    }
}
