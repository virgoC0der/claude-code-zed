//! WS-level integration: tools/call openFile → fake zed binary receives the
//! positioned path spec, and the MCP reply mirrors the VSCode shapes.
//!
//! The connection/auth harness mirrors `tests/handshake.rs` — an
//! in-process `Transport` on `127.0.0.1:0` with `NoopCwdResolver`
//! injected (no CI test may invoke real libproc), plus a
//! `with_zed_bin` override pointing at a fake editor script.

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

use zed_claude_bridge::mcp::EditorState;
use zed_claude_bridge::transport::{AuthToken, NoopCwdResolver, Transport};

const TEST_TOKEN: &str = "test-token-deadbeefcafe";

/// Spawn a transport on `127.0.0.1:0` whose `openFile` tool launches
/// `zed_bin` instead of the real `zed` CLI; return `(addr, transport)`.
///
/// Same `NoopCwdResolver` injection rationale as `tests/handshake.rs`.
async fn start_transport_with_zed_bin(zed_bin: &str) -> (std::net::SocketAddr, Transport) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let auth = AuthToken::new(TEST_TOKEN);
    let state = Arc::new(RwLock::new(EditorState::new()));
    let transport = Transport::builder(auth, state)
        .with_cwd_resolver(Arc::new(NoopCwdResolver::new()))
        .with_zed_bin(zed_bin)
        .build();

    let server_handle = transport.clone();
    tokio::spawn(async move {
        let _ = server_handle.run(listener).await;
    });

    // Give the accept loop a beat to start polling.
    sleep(Duration::from_millis(20)).await;

    (addr, transport)
}

/// Open a TCP stream and drive a WebSocket client handshake against `addr`,
/// including the auth header.
async fn connect_authorized(addr: &std::net::SocketAddr) -> WebSocketStream<TcpStream> {
    let stream = TcpStream::connect(addr).await.expect("tcp connect");
    let url = format!("ws://{addr}/");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "x-claude-code-ide-authorization",
        HeaderValue::from_str(TEST_TOKEN).unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("client handshake");
    ws
}

async fn recv_text<S>(ws: &mut WebSocketStream<S>) -> String
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_file_tool_spawns_zed_and_replies() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let capture = tmp.path().join("argv.txt");
    let script = tmp.path().join("fake-zed.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$@\" > {}\nexit 0\n", capture.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let target = tmp.path().join("main.rs");
    std::fs::write(&target, "fn a() {}\nfn main() {}\n").unwrap();

    // 1. Start a Transport with .with_zed_bin(script) on a random port.
    let (addr, _t) = start_transport_with_zed_bin(script.to_str().unwrap()).await;

    // 2. Connect an authorized WS client; complete initialize.
    let mut ws = connect_authorized(&addr).await;
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}).to_string(),
    ))
    .await
    .unwrap();
    let resp = recv_text(&mut ws).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
    ))
    .await
    .unwrap();

    // 3. Send the openFile tools/call.
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "openFile",
                "arguments": {
                    "filePath": target.to_str().unwrap(),
                    "startText": "fn main"
                }
            }
        })
        .to_string(),
    ))
    .await
    .unwrap();

    // 4. Read the response frame for id 7 and assert the reply text.
    let resp = recv_text(&mut ws).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 7, "reply must correlate to tools/call (id=7)");
    assert_eq!(
        v["result"]["content"][0]["text"],
        "Opened file and positioned at \"fn main\""
    );

    // 5. Assert the fake zed received the positioned path spec.
    let argv = std::fs::read_to_string(&capture).unwrap();
    assert_eq!(argv.trim(), format!("-e {}:2:1", target.display()));

    let _ = ws.close(None).await;
}
