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

    /// Disable the Zed active-file watcher. The watcher is ON by default;
    /// pass `--no-watch-zed-db` to turn it off.
    #[arg(long = "no-watch-zed-db", default_value_t = false)]
    pub no_watch_zed_db: bool,

    /// Override the auto-detected path to Zed's `db.sqlite`. Mainly for tests
    /// and non-standard installs.
    #[arg(long, value_name = "PATH")]
    pub zed_db_path: Option<PathBuf>,
}

/// Arguments for `ipc-send-at-mention`.
///
/// Three ways to specify the line range, in priority order:
///
/// 1. **Explicit:** `--line-start N --line-end M` (0-indexed, inclusive).
///    Both required together; takes precedence over everything else.
/// 2. **Text-derived:** `--text "<selection>"` plus `--file-path`. The
///    sidecar reads the file, finds the first occurrence of `<selection>`,
///    and computes a 0-indexed line range covering it. Useful when the
///    caller (e.g. a Zed task) only has access to `$ZED_SELECTED_TEXT`.
/// 3. **Cursor fallback:** `--cursor-row N` (**1-indexed**, matching Zed's
///    `$ZED_ROW`). Used when no selection text is available, or when the
///    text wasn't found in the file. The single row becomes both
///    `line_start` and `line_end` (still emitted as 0-indexed on the wire).
///
/// At least one of (`--line-start`+`--line-end`), `--text`, or
/// `--cursor-row` must be supplied; otherwise the CLI exits with an error.
///
/// **Session routing.** Two optional flags drive the
/// `session-routing` change's at-mention router:
///
/// - `--workspace-root <PATH>`: the absolute path of the Zed worktree
///   from which this at-mention was triggered. Typically populated
///   from `$ZED_WORKTREE_ROOT` by the Zed task. **Distinct from
///   `--workspace`**: `--workspace` names the IPC socket scope (the
///   directory whose hash forms the socket file name), while
///   `--workspace-root` is forwarded into the IPC frame's
///   `workspace_root` field so the sidecar's router can pick a
///   recipient WebSocket client by workspace.
/// - `--client-id <UUID>`: the registry id of a specific WebSocket
///   client to route directly to. **Normally set internally by the
///   helper on the picker's follow-up leg** (i.e. after a macOS
///   `osascript choose from list` dialog), not by end users.
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

    /// 0-indexed inclusive start line of the selection. When provided,
    /// `--line-end` is also required and overrides `--text` / `--cursor-row`.
    #[arg(long, requires = "line_end")]
    pub line_start: Option<u32>,

    /// 0-indexed inclusive end line of the selection. When provided,
    /// `--line-start` is also required and overrides `--text` / `--cursor-row`.
    #[arg(long, requires = "line_start")]
    pub line_end: Option<u32>,

    /// Selected text (typically `$ZED_SELECTED_TEXT`). The sidecar locates
    /// its first occurrence in `--file-path` to derive the line range.
    /// Falls back to `--cursor-row` if the text is missing or the file is
    /// unreadable.
    #[arg(long, value_name = "STRING")]
    pub text: Option<String>,

    /// **1-indexed** caret row (typically `$ZED_ROW`). Used when no other
    /// range information is available, or as a fallback when `--text`
    /// can't be located.
    #[arg(long, value_name = "N")]
    pub cursor_row: Option<u32>,

    /// Zed worktree root for session-aware at-mention routing. Forwarded
    /// into the IPC frame's `workspace_root` field. Distinct from
    /// `--workspace` (see the struct's rustdoc). Typically populated
    /// from `$ZED_WORKTREE_ROOT`.
    #[arg(long, value_name = "PATH")]
    pub workspace_root: Option<PathBuf>,

    /// Direct-route override: the registry id of a specific WebSocket
    /// client to deliver this at-mention to. Bypasses workspace
    /// matching. Set internally by the helper on the picker's
    /// follow-up leg; end users do not normally set it. Malformed
    /// UUIDs are rejected at parse time with a typed error.
    #[arg(long, value_name = "UUID", value_parser = clap::value_parser!(uuid::Uuid))]
    pub client_id: Option<uuid::Uuid>,
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

/// Resolve a 0-indexed inclusive line range from the various ways
/// `ipc-send-at-mention` accepts one.
///
/// Priority:
/// 1. Explicit `--line-start` + `--line-end` (taken verbatim, must be valid).
/// 2. `--text`: read `file_path`, find the first occurrence; the start line
///    is the number of `\n` characters preceding the match (0-indexed); the
///    end line is `start + (number of \n in the matched text)`.
/// 3. `--cursor-row` (1-indexed) → both bounds become `cursor_row - 1`.
///
/// If `--text` is supplied but the file can't be read or the text isn't
/// found, falls back to `--cursor-row`. If nothing yields a range, returns
/// an `Err` with a user-facing message.
pub fn resolve_line_range(args: &IpcSendAtMentionArgs) -> Result<(u32, u32), String> {
    // 1. Explicit line range wins.
    if let (Some(start), Some(end)) = (args.line_start, args.line_end) {
        if start > end {
            return Err(format!(
                "--line-start ({start}) must be <= --line-end ({end})",
            ));
        }
        return Ok((start, end));
    }

    // 2. Try `--text`.
    if let Some(text) = args.text.as_deref()
        && !text.is_empty()
    {
        match find_text_line_range(std::path::Path::new(&args.file_path), text) {
            Ok(Some(range)) => return Ok(range),
            Ok(None) => {
                tracing::debug!(
                    file_path = %args.file_path,
                    "selection text not found in file; falling back to --cursor-row"
                );
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    file_path = %args.file_path,
                    "could not read file to locate selection text; falling back to --cursor-row"
                );
            }
        }
    }

    // 3. Fall back to cursor row (1-indexed → 0-indexed).
    if let Some(row_one) = args.cursor_row {
        if row_one == 0 {
            return Err("--cursor-row must be >= 1 (it is 1-indexed)".to_string());
        }
        let row_zero = row_one - 1;
        return Ok((row_zero, row_zero));
    }

    Err("no line range provided: pass --line-start/--line-end, --text, or --cursor-row".to_string())
}

/// Find the first byte occurrence of `needle` in the file at `path` and
/// translate it into a 0-indexed inclusive `(line_start, line_end)` range.
///
/// Returns `Ok(None)` when the file is readable but does not contain the
/// needle. Returns `Err` only on I/O errors (so callers can fall back).
fn find_text_line_range(
    path: &std::path::Path,
    needle: &str,
) -> std::io::Result<Option<(u32, u32)>> {
    if needle.is_empty() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)?;
    let Some(byte_offset) = contents.find(needle) else {
        return Ok(None);
    };
    // Count `\n` bytes before the match → 0-indexed start line.
    let start_line = count_newlines(&contents[..byte_offset]);
    // The match itself may span multiple lines; count `\n` inside the
    // matched substring to derive the end line.
    let end_line = start_line + count_newlines(needle);
    // Saturate at u32::MAX to keep the wire types intact even for absurd
    // inputs; downstream consumers never expect such values in practice.
    let start_u32 = u32::try_from(start_line).unwrap_or(u32::MAX);
    let end_u32 = u32::try_from(end_line).unwrap_or(u32::MAX);
    Ok(Some((start_u32, end_u32)))
}

fn count_newlines(s: &str) -> usize {
    s.bytes().filter(|b| *b == b'\n').count()
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
                assert_eq!(a.line_start, Some(9));
                assert_eq!(a.line_end, Some(19));
                assert!(a.text.is_none());
                assert!(a.cursor_row.is_none());
            }
            other => panic!("wrong subcommand: {other:?}"),
        }
    }

    #[test]
    fn parses_ipc_send_at_mention_with_text_and_cursor() {
        let cli = Cli::try_parse_from([
            "zed-claude-bridge",
            "ipc-send-at-mention",
            "--workspace",
            "/p",
            "--file-path",
            "/p/a.rs",
            "--text",
            "fn foo() {}",
            "--cursor-row",
            "7",
        ])
        .expect("parse");
        match cli.command.expect("subcommand present") {
            Command::IpcSendAtMention(a) => {
                assert!(a.line_start.is_none());
                assert!(a.line_end.is_none());
                assert_eq!(a.text.as_deref(), Some("fn foo() {}"));
                assert_eq!(a.cursor_row, Some(7));
            }
            other => panic!("wrong subcommand: {other:?}"),
        }
    }

    #[test]
    fn parses_ipc_send_at_mention_with_only_cursor() {
        let cli = Cli::try_parse_from([
            "zed-claude-bridge",
            "ipc-send-at-mention",
            "--workspace",
            "/p",
            "--file-path",
            "/p/a.rs",
            "--cursor-row",
            "1",
        ])
        .expect("parse");
        match cli.command.expect("subcommand present") {
            Command::IpcSendAtMention(a) => {
                assert!(a.line_start.is_none());
                assert!(a.line_end.is_none());
                assert!(a.text.is_none());
                assert_eq!(a.cursor_row, Some(1));
            }
            other => panic!("wrong subcommand: {other:?}"),
        }
    }

    #[test]
    fn rejects_line_start_without_line_end() {
        // `--line-start` requires `--line-end`. clap should error.
        let res = Cli::try_parse_from([
            "zed-claude-bridge",
            "ipc-send-at-mention",
            "--workspace",
            "/p",
            "--file-path",
            "/p/a.rs",
            "--line-start",
            "9",
        ]);
        assert!(res.is_err(), "expected error when --line-end is missing");
    }

    #[test]
    fn parses_ipc_send_at_mention_with_workspace_root() {
        // Spec scenario: "--workspace-root populates the frame field".
        let cli = Cli::try_parse_from([
            "zed-claude-bridge",
            "ipc-send-at-mention",
            "--workspace",
            "/tmp/ws",
            "--workspace-root",
            "/Users/me/proj",
            "--file-path",
            "/Users/me/proj/x.rs",
            "--cursor-row",
            "5",
        ])
        .expect("parse");
        match cli.command.expect("subcommand present") {
            Command::IpcSendAtMention(a) => {
                assert_eq!(
                    a.workspace_root.as_deref(),
                    Some(std::path::Path::new("/Users/me/proj"))
                );
                assert!(a.client_id.is_none());
            }
            other => panic!("wrong subcommand: {other:?}"),
        }
    }

    #[test]
    fn parses_ipc_send_at_mention_with_valid_client_id() {
        // Spec scenario: "--client-id populates the frame field".
        let cli = Cli::try_parse_from([
            "zed-claude-bridge",
            "ipc-send-at-mention",
            "--workspace",
            "/tmp/ws",
            "--client-id",
            "f47ac10b-58cc-4372-a567-0e02b2c3d479",
            "--file-path",
            "/x.rs",
            "--cursor-row",
            "1",
        ])
        .expect("parse");
        match cli.command.expect("subcommand present") {
            Command::IpcSendAtMention(a) => {
                assert_eq!(
                    a.client_id,
                    Some(
                        uuid::Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479")
                            .expect("uuid")
                    )
                );
                assert!(a.workspace_root.is_none());
            }
            other => panic!("wrong subcommand: {other:?}"),
        }
    }

    #[test]
    fn rejects_ipc_send_at_mention_with_malformed_client_id() {
        // Spec scenario: "--client-id rejects malformed UUIDs".
        // clap's value_parser!(uuid::Uuid) rejects with a typed error
        // BEFORE any IPC frame is written.
        let res = Cli::try_parse_from([
            "zed-claude-bridge",
            "ipc-send-at-mention",
            "--workspace",
            "/tmp/ws",
            "--client-id",
            "not-a-uuid",
            "--file-path",
            "/x.rs",
            "--cursor-row",
            "1",
        ]);
        assert!(
            res.is_err(),
            "malformed --client-id SHALL reject at parse time"
        );
    }

    #[test]
    fn ipc_send_at_mention_omits_routing_fields_when_not_provided() {
        // Spec scenario: "Omitting both yields None for both".
        let cli = Cli::try_parse_from([
            "zed-claude-bridge",
            "ipc-send-at-mention",
            "--workspace",
            "/tmp/ws",
            "--file-path",
            "/x.rs",
            "--cursor-row",
            "1",
        ])
        .expect("parse");
        match cli.command.expect("subcommand present") {
            Command::IpcSendAtMention(a) => {
                assert!(a.workspace_root.is_none());
                assert!(a.client_id.is_none());
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

    // ---- resolve_line_range ------------------------------------------------

    fn make_args(
        file_path: &str,
        line_start: Option<u32>,
        line_end: Option<u32>,
        text: Option<&str>,
        cursor_row: Option<u32>,
    ) -> IpcSendAtMentionArgs {
        IpcSendAtMentionArgs {
            workspace: PathBuf::from("/tmp/ws"),
            ipc_socket: None,
            file_path: file_path.to_string(),
            line_start,
            line_end,
            text: text.map(String::from),
            cursor_row,
            workspace_root: None,
            client_id: None,
        }
    }

    #[test]
    fn resolver_explicit_lines_take_priority_over_text_and_cursor() {
        // Even with text + cursor_row supplied, explicit lines win.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn other() {}\nfn foo() {}\n").expect("write");

        let args = make_args(
            file.to_str().expect("utf8 path"),
            Some(2),
            Some(4),
            Some("fn foo() {}"),
            Some(99),
        );
        let (start, end) = resolve_line_range(&args).expect("resolver should succeed");
        assert_eq!(start, 2);
        assert_eq!(end, 4);
    }

    #[test]
    fn resolver_explicit_lines_reject_inverted_range() {
        let args = make_args("/nonexistent", Some(5), Some(3), None, None);
        let err = resolve_line_range(&args).expect_err("inverted range should error");
        assert!(err.contains("--line-start"), "got: {err}");
    }

    #[test]
    fn resolver_text_found_yields_correct_line_range_single_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        // Text content (0-indexed lines):
        //   0: line zero
        //   1: line one with target inside
        //   2: line two
        std::fs::write(&file, "line zero\nline one with target inside\nline two\n").expect("write");

        let args = make_args(
            file.to_str().expect("utf8 path"),
            None,
            None,
            Some("target"),
            Some(99), // ignored: text was found
        );
        let (start, end) = resolve_line_range(&args).expect("text should be found");
        assert_eq!(start, 1);
        assert_eq!(end, 1);
    }

    #[test]
    fn resolver_text_found_yields_correct_line_range_multi_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "header\nfn foo() {\n    bar();\n}\nfooter\n").expect("write");

        // Match spans 3 newlines → start..start+3.
        let needle = "fn foo() {\n    bar();\n}";
        let args = make_args(
            file.to_str().expect("utf8 path"),
            None,
            None,
            Some(needle),
            None,
        );
        let (start, end) = resolve_line_range(&args).expect("multi-line text should resolve");
        assert_eq!(start, 1);
        assert_eq!(end, 3);
    }

    #[test]
    fn resolver_text_not_found_falls_back_to_cursor_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "line zero\nline one\n").expect("write");

        let args = make_args(
            file.to_str().expect("utf8 path"),
            None,
            None,
            Some("absent text"),
            Some(2), // 1-indexed → 0-indexed 1
        );
        let (start, end) = resolve_line_range(&args).expect("should fall back to cursor row");
        assert_eq!(start, 1);
        assert_eq!(end, 1);
    }

    #[test]
    fn resolver_unreadable_file_falls_back_to_cursor_row() {
        let args = make_args(
            "/this/path/definitely/does/not/exist.rs",
            None,
            None,
            Some("any text"),
            Some(5),
        );
        let (start, end) =
            resolve_line_range(&args).expect("unreadable file → fall back to cursor row");
        assert_eq!(start, 4); // 5 (1-indexed) → 4 (0-indexed)
        assert_eq!(end, 4);
    }

    #[test]
    fn resolver_cursor_only_translates_one_indexed_to_zero_indexed() {
        let args = make_args("/whatever", None, None, None, Some(10));
        let (start, end) = resolve_line_range(&args).expect("cursor row only is enough");
        assert_eq!(start, 9);
        assert_eq!(end, 9);
    }

    #[test]
    fn resolver_cursor_zero_is_rejected() {
        let args = make_args("/whatever", None, None, None, Some(0));
        let err = resolve_line_range(&args).expect_err("cursor 0 invalid");
        assert!(err.contains("1-indexed"), "got: {err}");
    }

    #[test]
    fn resolver_no_inputs_returns_error() {
        let args = make_args("/whatever", None, None, None, None);
        let err = resolve_line_range(&args).expect_err("no inputs → error");
        assert!(err.contains("no line range"), "got: {err}");
    }

    #[test]
    fn resolver_empty_text_falls_back_to_cursor() {
        let args = make_args("/whatever", None, None, Some(""), Some(3));
        let (start, end) = resolve_line_range(&args).expect("empty text → cursor row");
        assert_eq!(start, 2);
        assert_eq!(end, 2);
    }
}
