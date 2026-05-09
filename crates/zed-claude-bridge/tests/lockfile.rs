//! Integration tests for [`zed_claude_bridge::lockfile::LockDir`].
//!
//! These exercise the full `open → write → list → prune → remove` sequence
//! inside a `tempfile::TempDir`, with a real `tokio::net::TcpListener` for
//! the `prune_stale` probe.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use tempfile::TempDir;
use tokio::net::TcpListener;

use zed_claude_bridge::lockfile::{DIR_MODE, FILE_MODE, LockDir};
use zed_claude_bridge::protocol::LockFile;

fn body_for(port: u16) -> LockFile {
    LockFile {
        pid: 1,
        workspace_folders: vec![PathBuf::from("/tmp/integration-ws")],
        ide_name: "Zed".to_string(),
        transport: "ws".to_string(),
        running_in_windows: false,
        auth_token: format!("token-{port}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn full_lifecycle_open_write_list_prune_remove() {
    let tmp = TempDir::new().unwrap();
    let dir = LockDir::open(tmp.path().join("ide")).unwrap();

    // Bind a real listener so we can register a "live" port whose lock file
    // must NOT be pruned. Port 0 → kernel-assigned ephemeral.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let live_port = listener.local_addr().unwrap().port();

    // Stale port: pick something that's almost certainly unbound. We loop
    // until we find a port nothing is listening on (using `connect` to test).
    let stale_port: u16 = {
        let mut candidate = 49_999_u16;
        loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", candidate)).await {
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => break candidate,
                _ => candidate -= 1,
            }
            assert!(candidate > 10_000, "could not find an unbound port");
        }
    };

    dir.write_lock(live_port, &body_for(live_port)).unwrap();
    dir.write_lock(stale_port, &body_for(stale_port)).unwrap();

    let live_path = dir.lock_path(live_port);
    let stale_path = dir.lock_path(stale_port);
    assert!(live_path.exists());
    assert!(stale_path.exists());
    // Permissions enforced.
    assert_eq!(
        fs::metadata(&live_path).unwrap().permissions().mode() & 0o777,
        FILE_MODE
    );
    assert_eq!(
        fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
        DIR_MODE
    );

    let listed = dir.list().unwrap();
    assert!(listed.contains(&live_port));
    assert!(listed.contains(&stale_port));

    dir.prune_stale().await.unwrap();

    assert!(
        live_path.exists(),
        "live peer's lock file must NOT be pruned"
    );
    assert!(
        !stale_path.exists(),
        "stale lock file must be pruned (port was unbound)"
    );

    // Cleanly remove our own (live) lock file.
    dir.remove_lock(live_port).unwrap();
    assert!(!live_path.exists());
    // Idempotency: removing again is a no-op.
    dir.remove_lock(live_port).unwrap();

    drop(listener);
}

#[tokio::test(flavor = "current_thread")]
async fn workspace_update_rewrites_in_place_same_port() {
    let tmp = TempDir::new().unwrap();
    let dir = LockDir::open(tmp.path().join("ide")).unwrap();

    let port: u16 = 54321;
    let mut body = body_for(port);
    dir.write_lock(port, &body).unwrap();
    let original_token = body.auth_token.clone();

    body.workspace_folders = vec![PathBuf::from("/x"), PathBuf::from("/y")];
    dir.write_lock(port, &body).unwrap();

    // Same port, same token, updated folders.
    let on_disk: LockFile =
        serde_json::from_str(&fs::read_to_string(dir.lock_path(port)).unwrap()).unwrap();
    assert_eq!(on_disk.auth_token, original_token);
    assert_eq!(
        on_disk.workspace_folders,
        vec![PathBuf::from("/x"), PathBuf::from("/y")]
    );

    // No other lock files should have appeared.
    let listed = dir.list().unwrap();
    assert_eq!(listed, vec![port]);
}
