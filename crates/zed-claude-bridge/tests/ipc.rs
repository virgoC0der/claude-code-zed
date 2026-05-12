//! Integration tests for the IPC server.
//!
//! Exercises:
//! - `ping` is acknowledged with `{"type":"ack"}\n`.
//! - `at_mention` produces a 1-indexed `at_mentioned` notification within 50 ms.
//! - Three rapid `selection` frames coalesce into one `selection_changed`
//!   carrying the third frame's content.
//! - Identical `selection_changed` payloads are deduplicated.
//! - `comment://` and `output://` URIs never produce `selection_changed`.
//! - Two concurrent IPC clients share the same `EditorState` (last writer wins).
//! - A line >1 MiB closes the connection but leaves the server accepting more.
//! - Unknown IPC `type` values are ignored.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{RwLock, mpsc};
use tokio::time::timeout;

use zed_claude_bridge::ipc::server::IpcServer;
use zed_claude_bridge::mcp::EditorState;
use zed_claude_bridge::protocol::Notification as JsonRpcNotification;
use zed_claude_bridge::transport::{
    CLIENT_CHANNEL_CAPACITY, ClientHandle, ClientId, ClientRegistry,
};

/// Spin up an `IpcServer` against a fresh socket inside a `TempDir`.
///
/// Returns the socket path, an mpsc receiver representing the single
/// "fake WebSocket client" registered with the server's
/// [`ClientRegistry`] (so the tests can observe routed notifications),
/// the shared state handle, and the `TempDir` itself (kept alive for
/// the lifetime of the test).
///
/// The fake client is registered with no `workspace_root` — so the
/// router's "singleton registry" rule (or "no workspace_root in
/// frame") always fires, mirroring the pre-routing fan-out behaviour
/// these tests originally asserted.
async fn start_server() -> (
    std::path::PathBuf,
    mpsc::Receiver<JsonRpcNotification>,
    Arc<RwLock<EditorState>>,
    TempDir,
) {
    let tmp = TempDir::new().unwrap();
    let socket_path = tmp.path().join("ipc.sock");
    let state = Arc::new(RwLock::new(EditorState::new()));
    let registry = ClientRegistry::new();

    // Pre-register a "fake WebSocket client" so the IPC server has
    // somewhere to route to. The router's singleton rule fires when
    // the IPC frame has no workspace_root (or it has one but no
    // workspace_root matches yet only one client is registered, which
    // is also a singleton route).
    let (tx, rx) = mpsc::channel::<JsonRpcNotification>(CLIENT_CHANNEL_CAPACITY);
    let now = tokio::time::Instant::now();
    let handle = ClientHandle {
        id: ClientId::new(),
        tx,
        workspace_root: None,
        last_activity: now,
        connected_at: now,
    };
    registry.insert(handle).await;

    let server = IpcServer::new(state.clone(), registry);
    let listener = IpcServer::bind(&socket_path).expect("bind");
    let server_clone = server.clone();
    tokio::spawn(async move {
        let _ = server_clone.run(listener).await;
    });
    // Give the accept loop a beat to start.
    tokio::time::sleep(Duration::from_millis(20)).await;
    (socket_path, rx, state, tmp)
}

async fn connect(path: &std::path::Path) -> UnixStream {
    UnixStream::connect(path).await.expect("connect")
}

async fn write_line(stream: &mut UnixStream, value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();
}

async fn read_line(stream: &mut UnixStream) -> Option<String> {
    let mut buf = String::new();
    let mut reader = BufReader::new(stream);
    let n = reader.read_line(&mut buf).await.ok()?;
    if n == 0 {
        None
    } else {
        Some(buf.trim_end_matches('\n').to_string())
    }
}

/// A persistent [`BufReader`] wrapper for tests that read multiple lines
/// from the same connection. Using a fresh `BufReader` per read would
/// discard buffered bytes between calls, which masks bugs.
async fn read_line_buffered<R>(reader: &mut BufReader<R>) -> Option<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = String::new();
    let n = reader.read_line(&mut buf).await.ok()?;
    if n == 0 {
        None
    } else {
        Some(buf.trim_end_matches('\n').to_string())
    }
}

async fn next_notification(
    rx: &mut mpsc::Receiver<JsonRpcNotification>,
    within: Duration,
) -> Option<JsonRpcNotification> {
    timeout(within, rx.recv()).await.ok().flatten()
}

// ---------------------------------------------------------------------------
// Ping ack
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn ping_is_acknowledged_with_ack() {
    let (path, _rx, _state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    write_line(&mut s, json!({"type": "ping"})).await;

    let line = timeout(Duration::from_millis(200), read_line(&mut s))
        .await
        .expect("ack arrives within 200ms")
        .expect("line received");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], "ack");
}

// ---------------------------------------------------------------------------
// at_mention → 1-indexed at_mentioned
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn at_mention_produces_one_indexed_notification_within_50ms() {
    let (path, mut rx, _state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    write_line(
        &mut s,
        json!({"type":"at_mention","file_path":"/p/x.rs","line_start":9,"line_end":19}),
    )
    .await;

    let notif = next_notification(&mut rx, Duration::from_millis(50))
        .await
        .expect("notification");
    assert_eq!(notif.method, "at_mentioned");
    let params = notif.params.expect("params");
    assert_eq!(params["filePath"], "/p/x.rs");
    assert_eq!(params["lineStart"], 10);
    assert_eq!(params["lineEnd"], 20);
}

#[tokio::test(flavor = "current_thread")]
async fn two_at_mentions_yield_two_notifications() {
    let (path, mut rx, _state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    write_line(
        &mut s,
        json!({"type":"at_mention","file_path":"/a","line_start":0,"line_end":0}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    write_line(
        &mut s,
        json!({"type":"at_mention","file_path":"/b","line_start":1,"line_end":2}),
    )
    .await;

    let n1 = next_notification(&mut rx, Duration::from_millis(200))
        .await
        .expect("first");
    let n2 = next_notification(&mut rx, Duration::from_millis(200))
        .await
        .expect("second");
    assert_eq!(n1.method, "at_mentioned");
    assert_eq!(n2.method, "at_mentioned");
    assert_eq!(n1.params.unwrap()["filePath"], "/a");
    assert_eq!(n2.params.unwrap()["filePath"], "/b");
}

// ---------------------------------------------------------------------------
// selection debounce + dedup
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn three_rapid_selections_collapse_to_one_selection_changed() {
    let (path, mut rx, _state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    write_line(
        &mut s,
        json!({"type":"selection","file_path":"/p/m.rs","line_start":1,"line_end":1,"text":"a"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    write_line(
        &mut s,
        json!({"type":"selection","file_path":"/p/m.rs","line_start":2,"line_end":3,"text":"bb"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    write_line(
        &mut s,
        json!({"type":"selection","file_path":"/p/m.rs","line_start":4,"line_end":7,"text":"ccc"}),
    )
    .await;
    // Wait through the 300ms debounce.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // We must see exactly one notification carrying the THIRD frame.
    let notif = next_notification(&mut rx, Duration::from_millis(50))
        .await
        .expect("debounced selection_changed");
    assert_eq!(notif.method, "selection_changed");
    let params = notif.params.expect("params");
    assert_eq!(params["text"], "ccc");
    assert_eq!(params["selection"]["start"]["line"], 4);
    assert_eq!(params["selection"]["end"]["line"], 7);

    // No further notification should arrive in another debounce window.
    let extra = next_notification(&mut rx, Duration::from_millis(400)).await;
    assert!(
        extra.is_none(),
        "no additional selection_changed expected, got {extra:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn idle_under_300ms_suppresses_delivery() {
    let (path, mut rx, _state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    write_line(
        &mut s,
        json!({"type":"selection","file_path":"/p/m.rs","line_start":1,"line_end":2,"text":"x"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    write_line(
        &mut s,
        json!({"type":"selection","file_path":"/p/m.rs","line_start":3,"line_end":4,"text":"y"}),
    )
    .await;
    // Within 250ms of the FIRST frame (we waited 200ms for the 2nd frame; now
    // wait only another 50ms — the debounce can't have elapsed yet because
    // the second frame reset the timer 50ms ago).
    let early = next_notification(&mut rx, Duration::from_millis(50)).await;
    assert!(
        early.is_none(),
        "expected no notification yet, got {early:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn identical_selection_emitted_twice_yields_one_notification() {
    let (path, mut rx, _state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    let frame =
        json!({"type":"selection","file_path":"/p/a.rs","line_start":5,"line_end":5,"text":"x"});
    write_line(&mut s, frame.clone()).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let n1 = next_notification(&mut rx, Duration::from_millis(50))
        .await
        .expect("first selection_changed");
    assert_eq!(n1.method, "selection_changed");

    // Second identical IPC frame should NOT yield another notification.
    write_line(&mut s, frame).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let extra = next_notification(&mut rx, Duration::from_millis(50)).await;
    assert!(extra.is_none(), "expected dedup, got {extra:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn comment_and_output_uris_skip_selection_changed() {
    let (path, mut rx, _state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    write_line(
        &mut s,
        json!({"type":"selection","file_path":"comment://abc","line_start":0,"line_end":0,"text":""}),
    )
    .await;
    write_line(
        &mut s,
        json!({"type":"selection","file_path":"output://stderr","line_start":0,"line_end":0,"text":""}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let extra = next_notification(&mut rx, Duration::from_millis(50)).await;
    assert!(
        extra.is_none(),
        "expected no notification for comment://output:// URIs, got {extra:?}"
    );
}

// ---------------------------------------------------------------------------
// State updates
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn workspace_folders_frame_updates_state() {
    let (path, _rx, state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    write_line(
        &mut s,
        json!({"type":"workspace_folders","folders":["/x","/y"]}),
    )
    .await;
    // Give the server a beat to apply.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let st = state.read().await;
    assert_eq!(st.workspace_folders().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn open_editors_frame_updates_state() {
    let (path, _rx, state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    write_line(
        &mut s,
        json!({
            "type":"open_editors",
            "editors":[
                {"uri":"file:///a.rs","is_active":true,"is_pinned":false,"is_preview":false}
            ]
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let st = state.read().await;
    assert_eq!(st.open_editors().len(), 1);
    assert_eq!(st.open_editors()[0].uri, "file:///a.rs");
}

// ---------------------------------------------------------------------------
// Multiple concurrent clients
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn two_concurrent_clients_share_state_last_writer_wins() {
    let (path, _rx, state, _tmp) = start_server().await;
    let mut a = connect(&path).await;
    let mut b = connect(&path).await;
    write_line(
        &mut a,
        json!({"type":"selection","file_path":"/p/a.rs","line_start":0,"line_end":0,"text":"a"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    write_line(
        &mut b,
        json!({"type":"selection","file_path":"/p/b.rs","line_start":0,"line_end":0,"text":"b"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let st = state.read().await;
    let cur = st.current_selection().expect("selection set");
    assert_eq!(cur.file_path, "/p/b.rs", "last writer should win");
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn oversized_line_closes_connection_but_server_keeps_accepting() {
    let (path, _rx, _state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    // Write 1.1 MiB of garbage with no trailing newline. The server should
    // log ERROR and close the connection.
    let big = vec![b'A'; (1 << 20) + 100];
    let _ = s.write_all(&big).await;
    let _ = s.shutdown().await;
    drop(s);

    // A fresh connection must succeed.
    let mut s2 = connect(&path).await;
    write_line(&mut s2, json!({"type":"ping"})).await;
    let line = timeout(Duration::from_millis(200), read_line(&mut s2))
        .await
        .expect("ack arrives")
        .expect("line received");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], "ack");
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_type_is_logged_and_ignored() {
    let (path, mut rx, _state, _tmp) = start_server().await;
    let mut s = connect(&path).await;
    write_line(&mut s, json!({"type":"unknown","foo":"bar"})).await;
    // Subsequent ping should still work.
    write_line(&mut s, json!({"type":"ping"})).await;
    let line = timeout(Duration::from_millis(200), read_line(&mut s))
        .await
        .expect("ack arrives")
        .expect("line received");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], "ack");

    // No notification should have been emitted.
    let extra = next_notification(&mut rx, Duration::from_millis(50)).await;
    assert!(extra.is_none(), "no notifications expected, got {extra:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn frames_separated_by_newline_in_one_write_parsed_independently() {
    let (path, _rx, _state, _tmp) = start_server().await;
    let s = connect(&path).await;
    let (read_half, mut write_half) = s.into_split();
    let mut reader = BufReader::new(read_half);

    let mut buf = serde_json::to_vec(&json!({"type":"ping"})).unwrap();
    buf.push(b'\n');
    let mut buf2 = serde_json::to_vec(&json!({"type":"ping"})).unwrap();
    buf2.push(b'\n');
    buf.extend_from_slice(&buf2);
    write_half.write_all(&buf).await.unwrap();

    let l1 = timeout(Duration::from_millis(200), read_line_buffered(&mut reader))
        .await
        .unwrap()
        .unwrap();
    let l2 = timeout(Duration::from_millis(200), read_line_buffered(&mut reader))
        .await
        .unwrap()
        .unwrap();
    let v1: Value = serde_json::from_str(&l1).unwrap();
    let v2: Value = serde_json::from_str(&l2).unwrap();
    assert_eq!(v1["type"], "ack");
    assert_eq!(v2["type"], "ack");
}

#[tokio::test(flavor = "current_thread")]
async fn client_disconnect_does_not_kill_server() {
    let (path, _rx, _state, _tmp) = start_server().await;
    {
        let mut s = connect(&path).await;
        write_line(&mut s, json!({"type":"ping"})).await;
        // Drop without graceful shutdown to simulate SIGKILL'd client.
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Fresh connection still works.
    let mut s = connect(&path).await;
    write_line(&mut s, json!({"type":"ping"})).await;
    let line = timeout(Duration::from_millis(200), read_line(&mut s))
        .await
        .expect("ack")
        .expect("line");
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], "ack");
}
