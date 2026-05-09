//! Integration tests for the WebSocket transport — auth gate, single-client
//! policy, full MCP handshake, and "sidecar survives client disconnect".
//!
//! Each test stands up a `Transport` against an in-process `tokio` accept
//! loop on `127.0.0.1:0` (kernel-assigned port; the spec's `[10000,65535]`
//! range only applies to the production sidecar, not handshake unit tests).

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
use zed_claude_bridge::transport::{AuthToken, Transport};

const TEST_TOKEN: &str = "test-token-deadbeefcafe";

/// Spawn a transport on `127.0.0.1:0` and return `(addr, transport)`.
async fn start_transport() -> (std::net::SocketAddr, Transport) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let auth = AuthToken::new(TEST_TOKEN);
    let state = Arc::new(RwLock::new(EditorState::new()));
    let transport = Transport::new(auth, state);

    let server_handle = transport.clone();
    tokio::spawn(async move {
        let _ = server_handle.run(listener).await;
    });

    // Give the accept loop a beat to start polling.
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
    assert_eq!(tools.len(), 4);

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
// Single-client policy
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn second_authorized_client_displaces_first_with_close_1000() {
    let (addr, _t) = start_transport().await;
    // Client A connects.
    let mut a = connect_with_token(&addr, Some(TEST_TOKEN)).await;
    // Drive a request so we know A is fully active.
    a.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut a).await;

    // Client B connects.
    let mut b = connect_with_token(&addr, Some(TEST_TOKEN)).await;

    // A should receive close 1000.
    let frame = timeout(Duration::from_secs(2), a.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match frame {
        Message::Close(Some(f)) => {
            assert_eq!(f.code, CloseCode::Normal, "want 1000 (Normal)");
            assert_eq!(f.reason, "Disconnecting previous WebSocket client");
        }
        other => panic!("expected close 1000, got {other:?}"),
    }

    // B is the active client; ping works.
    b.send(Message::Text(
        json!({"jsonrpc":"2.0","id":42,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(&mut b).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 42);
    assert_eq!(v["result"], json!({}));

    let _ = b.close(None).await;
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
// Notifier broadcast
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn ipc_notification_via_broadcast_reaches_active_client() {
    let (addr, transport) = start_transport().await;
    let notifier = transport.notifier();

    let mut ws = connect_with_token(&addr, Some(TEST_TOKEN)).await;

    // Drive a ping so we can confirm the client task is in its select loop
    // (a subscriber exists).
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
    ))
    .await
    .unwrap();
    let _ = recv_text(&mut ws).await;

    // Push a notification through the broadcast.
    use zed_claude_bridge::protocol::Notification;
    let notif = Notification::new(
        "at_mentioned",
        json!({"filePath": "/p/x.rs", "lineStart": 10, "lineEnd": 20}),
    );
    notifier.send(notif).expect("at least one subscriber");

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
