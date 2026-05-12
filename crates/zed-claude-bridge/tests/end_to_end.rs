//! Full-stack integration test: extension → IPC → sidecar → WebSocket → CLI.
//!
//! We don't spawn a separate process; instead we build the whole sidecar
//! library stack inline (bypassing the CLI/argv layer and the global
//! tracing subscriber, which the unit/integration test harness can't share
//! cleanly across concurrent tests). The wire formats and the cross-layer
//! data flow are still exercised exactly as the binary uses them.

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
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use zed_claude_bridge::ipc::server::IpcServer;
use zed_claude_bridge::lockfile::LockDir;
use zed_claude_bridge::mcp::EditorState;
use zed_claude_bridge::protocol::LockFile;
use zed_claude_bridge::transport::{AuthToken, NoopCwdResolver, Transport, bind_random};

/// Stand up the full sidecar stack against a `TempDir` for the lock dir
/// and a `TempDir`-derived path for the IPC socket. Returns:
///
/// - the path to the freshly written lock file (so the test reads the
///   token + port from disk, just like the Claude Code CLI does);
/// - the path to the IPC socket;
/// - the bound port (for direct verification against the lock file).
async fn start_full_stack() -> (std::path::PathBuf, std::path::PathBuf, u16, TempDir) {
    let tmp = TempDir::new().unwrap();
    let lock_dir_path = tmp.path().join("ide");
    let socket_path = tmp.path().join("ipc.sock");

    // 1. Lock dir + auth token + WS bind, mirroring `app::lifecycle::run_daemon`.
    let lock_dir = LockDir::open(&lock_dir_path).expect("open lock dir");
    let auth = AuthToken::new("e2e-token-deadbeef-1234567890abcd");
    let (ws_listener, port) = bind_random(16).await.expect("bind ws");

    // 2. Write the lock file. Use a fake workspace path so it doesn't
    //    matter whether the path exists on this machine.
    let body = LockFile {
        pid: std::process::id(),
        workspace_folders: vec![tmp.path().to_path_buf()],
        ide_name: "Zed".to_string(),
        transport: "ws".to_string(),
        running_in_windows: false,
        auth_token: auth.as_str().to_string(),
    };
    lock_dir.write_lock(port, &body).expect("write lock");

    // 3. IPC server + transport (sharing the client registry).
    //    `NoopCwdResolver` is injected so the test process's
    //    real cwd is NOT auto-resolved as a priority-2
    //    workspace; without this, the new peer-cwd-discovery
    //    behaviour would silently fill `workspace_root` with
    //    the test runner's cwd, defeating any test that
    //    expects priority-3 / priority-4 to fire.
    let state = Arc::new(RwLock::new(EditorState::new()));
    let transport = Transport::builder(auth, state.clone())
        .with_cwd_resolver(Arc::new(NoopCwdResolver::new()))
        .build();
    let registry = transport.registry();
    let ipc_server = IpcServer::new(state, registry);
    let ipc_listener = IpcServer::bind(&socket_path).expect("bind ipc");

    // 4. Spawn both accept loops.
    tokio::spawn(async move {
        let _ = transport.run(ws_listener).await;
    });
    tokio::spawn(async move {
        let _ = ipc_server.run(ipc_listener).await;
    });
    sleep(Duration::from_millis(20)).await;

    (lock_dir.lock_path(port), socket_path, port, tmp)
}

async fn ws_connect_with_token(
    addr: std::net::SocketAddr,
    token: &str,
) -> WebSocketStream<TcpStream> {
    let stream = TcpStream::connect(addr).await.expect("ws tcp");
    let mut req = format!("ws://{addr}/").into_client_request().unwrap();
    req.headers_mut().insert(
        "x-claude-code-ide-authorization",
        HeaderValue::from_str(token).unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::client_async(req, stream)
        .await
        .expect("ws upgrade");
    ws
}

async fn send_text(ws: &mut WebSocketStream<TcpStream>, v: Value) {
    ws.send(Message::Text(v.to_string())).await.unwrap();
}

async fn recv_text(ws: &mut WebSocketStream<TcpStream>) -> String {
    loop {
        let msg = timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("recv timeout")
            .expect("stream had a frame")
            .expect("frame ok");
        match msg {
            Message::Text(s) => return s,
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn full_stack_handshake_then_at_mention_round_trip() {
    let (lock_path, socket_path, port, _tmp) = start_full_stack().await;

    // 1. Read the lock file we just wrote — exactly like the Claude Code
    //    CLI's `/ide` command does.
    let raw = std::fs::read_to_string(&lock_path).expect("read lock");
    let parsed: LockFile = serde_json::from_str(&raw).expect("lock JSON");
    assert_eq!(parsed.transport, "ws");
    assert_eq!(parsed.ide_name, "Zed");

    // 2. Connect with the auth header from the lock file.
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut ws = ws_connect_with_token(addr, &parsed.auth_token).await;

    // 3. Drive the full MCP handshake.
    send_text(
        &mut ws,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await;
    let resp = recv_text(&mut ws).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["protocolVersion"], "2024-11-05");

    send_text(
        &mut ws,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;

    send_text(
        &mut ws,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let resp = recv_text(&mut ws).await;
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["id"], 2);
    let names: Vec<String> = v["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for required in [
        "getCurrentSelection",
        "getLatestSelection",
        "getOpenEditors",
        "getWorkspaceFolders",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "missing {required} in tools/list"
        );
    }

    // 4. Open the IPC socket and write an at_mention frame with 0-indexed
    //    line numbers. The sidecar is supposed to forward an at_mentioned
    //    JSON-RPC notification with **1-indexed** lines (lineStart+1,
    //    lineEnd+1) over the WebSocket within ~50ms.
    let mut ipc = UnixStream::connect(&socket_path)
        .await
        .expect("ipc connect");
    let frame = json!({
        "type": "at_mention",
        "file_path": "/p/x.rs",
        "line_start": 9,
        "line_end": 19,
    });
    let mut buf = serde_json::to_vec(&frame).unwrap();
    buf.push(b'\n');
    ipc.write_all(&buf).await.unwrap();
    ipc.shutdown().await.ok();

    // 5. Receive the notification on the WS.
    let notif = timeout(Duration::from_millis(500), recv_text(&mut ws))
        .await
        .expect("at_mentioned arrives");
    let v: Value = serde_json::from_str(&notif).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["method"], "at_mentioned");
    assert_eq!(v["params"]["filePath"], "/p/x.rs");
    assert_eq!(v["params"]["lineStart"], 10);
    assert_eq!(v["params"]["lineEnd"], 20);
    assert!(v.get("id").is_none(), "notifications carry no id");

    let _ = ws.close(None).await;
}
