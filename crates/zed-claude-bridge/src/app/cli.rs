//! Command-line interface for the `zed-claude-bridge` binary.
//!
//! Two operating modes:
//!
//! 1. **Daemon mode** (default, no subcommand): hosts the WebSocket MCP
//!    server and the IPC server. Started by the Zed extension on first use.
//! 2. **Helper mode** (`ipc-send` subcommand): one-shot helper used by the
//!    Zed extension (which runs inside a WASM sandbox without raw Unix-socket
//!    I/O). Connects to the daemon's IPC socket, writes one frame, exits.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Local sidecar that bridges Zed to Claude Code's `/ide` command.
///
/// One sidecar runs per Zed workspace. It hosts a localhost WebSocket MCP
/// server (so the Claude Code CLI can discover it via `~/.claude/ide/<port>.lock`)
/// and a Unix-domain-socket IPC channel (so the Zed extension can push
/// selections, at-mentions, and editor metadata into the same process).
#[derive(Debug, Parser)]
#[command(name = "zed-claude-bridge", version, about, long_about = None)]
pub struct Cli {
    /// Optional subcommand. When omitted, the binary runs in daemon mode and
    /// requires `--workspace` (and the other daemon flags).
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Daemon-mode flags (ignored when a subcommand is used).
    #[command(flatten)]
    pub daemon: DaemonArgs,
}

/// Subcommands. Only the IPC helper modes live here; daemon mode is the
/// default no-subcommand invocation.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Send an `at_mention` IPC frame to the running sidecar's socket and
    /// exit. Used by the Zed extension's "Send to Claude Code" action.
    IpcSendAtMention(IpcSendAtMentionArgs),

    /// Send a `workspace_folders` IPC frame and exit.
    IpcSendWorkspaceFolders(IpcSendWorkspaceFoldersArgs),
}

/// Daemon-mode arguments.
#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Workspace root directory of the Zed window this sidecar serves.
    /// Used to compute the IPC socket path and the initial lock-file
    /// `workspaceFolders` entry.
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,

    /// Run in the foreground (do not daemonize). Always `true` in this build;
    /// kept as an explicit flag so logs make the lifecycle clear.
    #[arg(long, default_value_t = true)]
    pub foreground: bool,

    /// Tracing log level: `error`, `warn`, `info`, `debug`, or `trace`. Honors
    /// `RUST_LOG` directives if a value such as `zed_claude_bridge=debug,info`
    /// is passed instead of a bare level word.
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Optional override for the IPC socket path. When omitted, the path is
    /// derived from `--workspace` via `ipc::socket_path`.
    #[arg(long, value_name = "PATH")]
    pub ipc_socket: Option<PathBuf>,

    /// Directory in which `<port>.lock` files are written. Defaults to
    /// `~/.claude/ide`. The directory is created with mode `0o700` if missing.
    #[arg(long, default_value = "~/.claude/ide", value_name = "DIR")]
    pub lock_dir: PathBuf,
}

/// Arguments for `ipc-send-at-mention`.
#[derive(Debug, Args)]
pub struct IpcSendAtMentionArgs {
    /// Workspace root used to derive the IPC socket path.
    #[arg(long, value_name = "DIR")]
    pub workspace: PathBuf,

    /// Optional override for the IPC socket path.
    #[arg(long, value_name = "PATH")]
    pub ipc_socket: Option<PathBuf>,

    /// Path of the file the at-mention refers to (absolute, recommended).
    #[arg(long, value_name = "PATH")]
    pub file_path: String,

    /// 0-indexed inclusive start line of the selection.
    #[arg(long)]
    pub line_start: u32,

    /// 0-indexed inclusive end line of the selection.
    #[arg(long)]
    pub line_end: u32,
}

/// Arguments for `ipc-send-workspace-folders`.
#[derive(Debug, Args)]
pub struct IpcSendWorkspaceFoldersArgs {
    /// Workspace root used to derive the IPC socket path.
    #[arg(long, value_name = "DIR")]
    pub workspace: PathBuf,

    /// Optional override for the IPC socket path.
    #[arg(long, value_name = "PATH")]
    pub ipc_socket: Option<PathBuf>,

    /// One or more absolute folder paths to publish as the workspace roots.
    #[arg(long = "folder", value_name = "PATH", required = true)]
    pub folders: Vec<PathBuf>,
}

impl DaemonArgs {
    /// Resolve `~` at the start of `--lock-dir` against `$HOME`. Other
    /// path components pass through untouched.
    pub fn resolved_lock_dir(&self) -> PathBuf {
        expand_tilde(&self.lock_dir)
    }
}

/// Expand a leading `~` or `~/...` against `$HOME`. Returns the input
/// unchanged if `$HOME` is unset or the path does not start with `~`.
pub(crate) fn expand_tilde(path: &std::path::Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    let home = match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => return path.to_path_buf(),
    };
    if let Some(rest) = s.strip_prefix("~/") {
        return home.join(rest);
    }
    if s == "~" {
        return home;
    }
    path.to_path_buf()
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;

    #[test]
    fn parses_daemon_mode_with_workspace() {
        let cli =
            Cli::try_parse_from(["zed-claude-bridge", "--workspace", "/tmp/ws"]).expect("parse");
        assert!(cli.command.is_none(), "no subcommand for daemon mode");
        assert_eq!(cli.daemon.workspace, Some(PathBuf::from("/tmp/ws")));
        assert!(cli.daemon.foreground);
        assert_eq!(cli.daemon.log_level, "info");
        assert_eq!(cli.daemon.lock_dir, PathBuf::from("~/.claude/ide"));
    }

    #[test]
    fn parses_daemon_mode_with_all_flags() {
        let cli = Cli::try_parse_from([
            "zed-claude-bridge",
            "--workspace",
            "/x",
            "--log-level",
            "debug",
            "--ipc-socket",
            "/tmp/sock",
            "--lock-dir",
            "/tmp/locks",
        ])
        .expect("parse");
        assert!(cli.command.is_none());
        assert_eq!(cli.daemon.log_level, "debug");
        assert_eq!(
            cli.daemon.ipc_socket.as_deref(),
            Some(std::path::Path::new("/tmp/sock"))
        );
        assert_eq!(cli.daemon.lock_dir, PathBuf::from("/tmp/locks"));
    }

    #[test]
    fn parses_ipc_send_at_mention() {
        let cli = Cli::try_parse_from([
            "zed-claude-bridge",
            "ipc-send-at-mention",
            "--workspace",
            "/p",
            "--file-path",
            "/p/a.rs",
            "--line-start",
            "9",
            "--line-end",
            "19",
        ])
        .expect("parse");
        match cli.command.expect("subcommand present") {
            Command::IpcSendAtMention(a) => {
                assert_eq!(a.workspace, PathBuf::from("/p"));
                assert_eq!(a.file_path, "/p/a.rs");
                assert_eq!(a.line_start, 9);
                assert_eq!(a.line_end, 19);
            }
            other => panic!("wrong subcommand: {other:?}"),
        }
    }

    #[test]
    fn parses_ipc_send_workspace_folders() {
        let cli = Cli::try_parse_from([
            "zed-claude-bridge",
            "ipc-send-workspace-folders",
            "--workspace",
            "/p",
            "--folder",
            "/p",
            "--folder",
            "/q",
        ])
        .expect("parse");
        match cli.command.expect("subcommand present") {
            Command::IpcSendWorkspaceFolders(a) => {
                assert_eq!(a.folders, vec![PathBuf::from("/p"), PathBuf::from("/q")]);
            }
            other => panic!("wrong subcommand: {other:?}"),
        }
    }

    #[test]
    fn expand_tilde_uses_home_when_set() {
        let home = std::env::var_os("HOME");
        let p = expand_tilde(std::path::Path::new("~/foo"));
        match home {
            Some(h) => assert_eq!(p, PathBuf::from(h).join("foo")),
            None => assert_eq!(p, PathBuf::from("~/foo")),
        }
    }

    #[test]
    fn expand_tilde_passes_through_when_no_tilde() {
        let p = expand_tilde(std::path::Path::new("/abs/path"));
        assert_eq!(p, PathBuf::from("/abs/path"));
    }
}
