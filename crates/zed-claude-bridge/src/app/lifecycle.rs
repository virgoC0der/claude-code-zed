//! Process lifecycle: tracing init, lock-dir prep, port bind, lock-file
//! write, IPC bind, accept loops, signal handling, graceful shutdown.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::app::cli::{
    Cli, Command, DaemonArgs, IpcSendAtMentionArgs, IpcSendWorkspaceFoldersArgs, resolve_line_range,
};
use crate::app::picker;
use crate::ipc;
use crate::ipc::server::IpcServer;
use crate::lockfile::LockDir;
use crate::mcp::EditorState;
use crate::protocol::{IpcFrame, LockFile};
use crate::transport::{AuthToken, Transport, bind_random, default_cwd_resolver};

/// Top-level entrypoint called from `main.rs`.
///
/// Dispatches to either the daemon or one of the helper subcommands.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::IpcSendAtMention(args)) => run_ipc_send_at_mention(args).await,
        Some(Command::IpcSendWorkspaceFolders(args)) => run_ipc_send_workspace_folders(args).await,
        None => run_daemon(cli.daemon).await,
    }
}

/// Daemon-mode entrypoint: runs the WebSocket and IPC accept loops until
/// SIGINT/SIGTERM/SIGHUP, then cleans up.
async fn run_daemon(args: DaemonArgs) -> anyhow::Result<()> {
    init_tracing(&args.log_level)?;
    let workspace = args
        .workspace
        .clone()
        .context("daemon mode requires --workspace")?;
    info!(workspace = %workspace.display(), "zed-claude-bridge starting");

    // 1. Prepare the discovery directory and prune stale peers.
    let lock_dir_path = args.resolved_lock_dir();
    let lock_dir = LockDir::open(&lock_dir_path)
        .with_context(|| format!("opening lock dir at {}", lock_dir_path.display()))?;
    if let Err(e) = lock_dir.prune_stale().await {
        warn!(error = %e, "stale-lock pruning failed; continuing");
    }

    // 2. Generate the per-launch auth token. We log only that one was
    //    generated at info level; the token itself is debug-only and
    //    redacted in any `{:?}` output via `AuthToken`'s Debug impl.
    let auth = AuthToken::generate();
    debug!(token = auth.as_str(), "generated per-launch auth token");
    info!("auth token generated");

    // 3. Bind the WebSocket listener.
    let (ws_listener, port) = bind_random(16)
        .await
        .context("binding WebSocket listener")?;
    info!(port, "websocket listener bound on 127.0.0.1");

    // 4. Write the lock file so `/ide` can find us.
    let body = LockFile {
        pid: std::process::id(),
        workspace_folders: vec![workspace.clone()],
        ide_name: "Zed".to_string(),
        transport: "ws".to_string(),
        running_in_windows: false,
        auth_token: auth.as_str().to_string(),
    };
    lock_dir
        .write_lock(port, &body)
        .with_context(|| format!("writing lock file for port {port}"))?;

    // 5. Build shared state + transport (which owns the client
    //    registry). Production wiring threads in BOTH:
    //    - the daemon's `--workspace` flag as the **priority-4**
    //      last-resort fallback (websocket spec "Defaults to
    //      --workspace when no client-side and no peer-cwd signal"
    //      scenario), and
    //    - the platform-default `CwdResolver` for **priority 2**
    //      (peer-process cwd discovery). On macOS this is
    //      `LibprocCwdResolver` (libproc-backed); on every other
    //      platform it is `NoopCwdResolver` (returns None for every
    //      peer, preserving today's behaviour).
    //
    //    We use the explicit `Transport::builder(...)` chain rather
    //    than the legacy `Transport::with_daemon_workspace` so the
    //    resolver wiring is greppable from this single call site
    //    — easier to audit and to swap in a custom resolver from a
    //    test or a debug binary.
    let state = Arc::new(RwLock::new(EditorState::new()));
    let cwd_resolver = default_cwd_resolver();
    info!(
        target_os = std::env::consts::OS,
        "cwd resolver configured (peer-process cwd discovery, priority 2)"
    );
    let transport = Transport::builder(auth, state.clone())
        .with_daemon_workspace(workspace.clone())
        .with_cwd_resolver(cwd_resolver)
        .build();
    let registry = transport.registry();

    // 6. Bind the IPC socket.
    let socket_path = args
        .ipc_socket
        .clone()
        .unwrap_or_else(|| ipc::socket_path(&workspace));
    let ipc_listener = IpcServer::bind(&socket_path)
        .with_context(|| format!("binding IPC socket at {}", socket_path.display()))?;
    let ipc_server = IpcServer::new(state, registry);

    // 7. Drive both accept loops on background tasks. Errors from these are
    //    logged but do not abort the process — accept loops are infinite.
    let ws_handle = tokio::spawn(async move {
        if let Err(e) = transport.run(ws_listener).await {
            error!(error = %e, "websocket accept loop exited with error");
        }
    });
    let ipc_handle = tokio::spawn(async move {
        if let Err(e) = ipc_server.run(ipc_listener).await {
            error!(error = %e, "ipc accept loop exited with error");
        }
    });

    // 8. Wait for SIGINT / SIGTERM / SIGHUP.
    let shutdown = wait_for_shutdown_signal().await;
    info!(reason = shutdown, "shutdown signal received; cleaning up");

    // 9. Stop accepting and unlink lock + socket files. Best-effort.
    ws_handle.abort();
    ipc_handle.abort();
    if let Err(e) = lock_dir.remove_lock(port) {
        warn!(error = %e, port, "failed to remove lock file; continuing");
    }
    if let Err(e) = std::fs::remove_file(&socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(error = %e, path = %socket_path.display(), "failed to remove ipc socket file");
        }
    }

    // 10. Allow in-flight connection tasks a moment to flush close frames.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        // We do not have explicit per-connection handles to await — the
        // accept-loop tasks have been aborted, and per-connection tasks
        // are independent. The 2s grace gives them time to flush before
        // the runtime shuts down (when `main` returns).
        sleep(Duration::from_millis(50)).await;
    })
    .await;

    info!("zed-claude-bridge stopped cleanly");
    Ok(())
}

/// Helper: connect to the daemon's IPC socket and write one frame.
///
/// Errors here are treated as fatal — the calling Zed extension uses the
/// non-zero exit code as its signal to spawn the daemon and retry.
async fn send_one_frame(socket_path: &std::path::Path, frame: &IpcFrame) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to IPC socket at {}", socket_path.display()))?;
    let mut bytes = serde_json::to_vec(frame).context("serialising IPC frame")?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .await
        .context("writing IPC frame")?;
    stream.flush().await.context("flushing IPC frame")?;
    stream.shutdown().await.ok();
    // Give the kernel a moment to deliver the bytes before the process exits.
    // Without this delay, in-flight Unix-socket writes can be discarded when
    // the connection's last reference drops (observed on macOS launchd-spawned
    // and shell-spawned binaries alike).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Ok(())
}

async fn run_ipc_send_at_mention(args: IpcSendAtMentionArgs) -> anyhow::Result<()> {
    let socket_path = args
        .ipc_socket
        .clone()
        .unwrap_or_else(|| ipc::socket_path(&args.workspace));
    let (line_start, line_end) =
        resolve_line_range(&args).map_err(|msg| anyhow::anyhow!("{msg}"))?;
    let initial_frame = IpcFrame::AtMention {
        file_path: args.file_path.clone(),
        line_start,
        line_end,
        workspace_root: args.workspace_root.clone(),
        client_id: args.client_id,
    };

    // Open the connection and keep it for the picker round-trip (if any).
    let stream = UnixStream::connect(&socket_path)
        .await
        .with_context(|| format!("connecting to IPC socket at {}", socket_path.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Write the first frame.
    write_frame(&mut write_half, &initial_frame).await?;

    // The sidecar always writes one reply on the same connection:
    // `IpcFrame::Ack` for direct routes (DirectClient /
    // WorkspaceUnique / Singleton), or `IpcFrame::Ambiguous` when
    // the workspace match was non-unique. Reading the reply lets us
    // close as soon as the sidecar confirms routing — without it we
    // would block for the full timeout below on every send.
    let reply_line = read_one_line_with_timeout(&mut reader, std::time::Duration::from_millis(500))
        .await
        .ok()
        .flatten();

    if let Some(line) = reply_line {
        match serde_json::from_str::<IpcFrame>(&line) {
            Ok(IpcFrame::Ack) => {
                // Direct route succeeded; nothing more to do.
            }
            Ok(IpcFrame::Ambiguous { candidates }) => {
                // Picker round-trip. On macOS this presents a native
                // `choose from list` dialog; on other platforms it
                // falls back to most-recently-active. Cancellation
                // yields None, which we treat as an intentional drop
                // (no follow-up frame; exit 0).
                if let Some(picked_uuid) = picker::pick_candidate(&candidates) {
                    let followup = IpcFrame::AtMention {
                        file_path: args.file_path,
                        line_start,
                        line_end,
                        workspace_root: args.workspace_root,
                        client_id: Some(picked_uuid),
                    };
                    write_frame(&mut write_half, &followup).await?;
                } else {
                    debug!("picker cancelled; sending no follow-up frame");
                }
            }
            Ok(other) => {
                debug!(?other, "ignoring unexpected IPC reply");
            }
            Err(e) => {
                debug!(error = %e, line = %line, "ignoring unparseable IPC reply");
            }
        }
    }

    // Cleanly close. The launchd-spawned-helper drain workaround
    // (`send_one_frame`'s 50ms sleep) is not needed here: the read
    // of `Ack` / `Ambiguous` above proves the sidecar has already
    // consumed our write, so there are no in-flight bytes to lose.
    let _ = write_half.flush().await;
    let _ = write_half.shutdown().await;
    Ok(())
}

/// Serialize `frame` as a single line-delimited JSON record and write
/// it to `stream`. Flushes after the write so the daemon sees the
/// bytes promptly.
async fn write_frame(
    stream: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &IpcFrame,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(frame).context("serialising IPC frame")?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .await
        .context("writing IPC frame")?;
    stream.flush().await.context("flushing IPC frame")?;
    Ok(())
}

/// Read one `\n`-terminated line from `reader`, with a hard timeout.
/// Returns `Ok(None)` on EOF, `Ok(Some(line))` on success, and
/// `Err(_)` only on the timeout case.
async fn read_one_line_with_timeout<R>(
    reader: &mut BufReader<R>,
    within: std::time::Duration,
) -> anyhow::Result<Option<String>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    let res = tokio::time::timeout(within, reader.read_line(&mut line)).await;
    match res {
        Ok(Ok(0)) => Ok(None), // EOF
        Ok(Ok(_)) => {
            // Strip trailing newline (and optional \r).
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            Ok(Some(line))
        }
        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
        Err(_) => Err(anyhow::anyhow!("read timed out after {:?}", within)),
    }
}

async fn run_ipc_send_workspace_folders(args: IpcSendWorkspaceFoldersArgs) -> anyhow::Result<()> {
    let socket_path = args
        .ipc_socket
        .clone()
        .unwrap_or_else(|| ipc::socket_path(&args.workspace));
    let frame = IpcFrame::WorkspaceFolders {
        folders: args.folders,
    };
    send_one_frame(&socket_path, &frame).await
}

/// Initialise the global `tracing` subscriber from `--log-level`.
fn init_tracing(level_or_filter: &str) -> anyhow::Result<()> {
    let env_filter = EnvFilter::try_new(level_or_filter)
        .or_else(|_| EnvFilter::try_new(format!("zed_claude_bridge={level_or_filter},info")))
        .context("parsing --log-level as tracing filter")?;
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .finish();
    // `try_init` is the right call from a binary so we don't double-init in
    // tests that exercise this function.
    let _ = tracing::subscriber::set_global_default(subscriber);
    Ok(())
}

/// Wait for the first of SIGINT / SIGTERM / SIGHUP. Returns the name of the
/// signal that fired so the lifecycle log makes the cause clear.
async fn wait_for_shutdown_signal() -> &'static str {
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "could not install SIGINT handler; only SIGTERM/SIGHUP active");
            return wait_for_term_or_hup().await;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "could not install SIGTERM handler");
            // Fall back to SIGINT only.
            sigint.recv().await;
            return "SIGINT";
        }
    };
    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "could not install SIGHUP handler");
            tokio::select! {
                _ = sigint.recv() => return "SIGINT",
                _ = sigterm.recv() => return "SIGTERM",
            }
        }
    };
    tokio::select! {
        _ = sigint.recv() => "SIGINT",
        _ = sigterm.recv() => "SIGTERM",
        _ = sighup.recv() => "SIGHUP",
    }
}

async fn wait_for_term_or_hup() -> &'static str {
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "could not install SIGTERM handler; sleeping forever");
            std::future::pending::<()>().await;
            return "SIGTERM";
        }
    };
    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "could not install SIGHUP handler; SIGTERM only");
            sigterm.recv().await;
            return "SIGTERM";
        }
    };
    tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sighup.recv() => "SIGHUP",
    }
}
