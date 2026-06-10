# openFile + Stable Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** (1) An `openFile` MCP tool so Claude can jump the user's Zed to a file (optionally positioned via a `startText` pattern); (2) a `--port` flag for a stable WebSocket port enabling `CLAUDE_CODE_SSE_PORT` auto-connect without `/ide`.

**Architecture:** `openFile` keeps `mcp/` pure via a new deferred-execution variant `McpResponse::OpenFile { id, args }` — the pure dispatcher validates args; the async transport layer (`ws.rs::dispatch_text`) executes it through a new `zed_cli/` module that spawns `zed -e <path>[:line:col]` (binary injectable for tests). `--port` adds `bind_fixed` beside `bind_random`, fail-fast on conflict.

**Tech Stack:** existing deps only (tokio::process, serde). No new crates.

**Design doc:** `docs/superpowers/specs/2026-06-10-openfile-and-stable-port-design.md`

**Branch:** `feat/openfile-and-stable-port` (stacked on `feat/zed-active-file-awareness`).

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/zed-claude-bridge/src/app/cli.rs` | Add `--port` to `DaemonArgs`. |
| `crates/zed-claude-bridge/src/transport/ws.rs` | Add `bind_fixed`; handle `McpResponse::OpenFile` in `dispatch_text`; `TransportBuilder::with_zed_bin`. |
| `crates/zed-claude-bridge/src/transport/mod.rs` | Re-export `bind_fixed`. |
| `crates/zed-claude-bridge/src/app/lifecycle.rs` | Choose `bind_fixed` vs `bind_random` from `args.port`. |
| `crates/zed-claude-bridge/src/protocol.rs` | `OpenFileArgs` wire type. |
| `crates/zed-claude-bridge/src/mcp/tools.rs` | Advertise `openFile` descriptor (5 tools now). |
| `crates/zed-claude-bridge/src/mcp/server.rs` | `McpResponse::OpenFile` variant + pure validation arm. |
| `crates/zed-claude-bridge/src/zed_cli/mod.rs` | NEW: text locate, path spec, spawn `zed`, response building. |
| `crates/zed-claude-bridge/src/lib.rs` | `pub mod zed_cli;` |
| `crates/zed-claude-bridge/tests/open_file.rs` | NEW: WS-level integration test with a fake `zed` script. |
| `scripts/com.virgoC0der.zed-claude-bridge.plist` | `--port 52840` in template. |
| `docs/protocol.md`, `openspec/specs/mcp/spec.md`, `README.md`, `.harness/project.md` | Spec/doc updates (openFile in scope; auto-connect guide; layer order). |

**Layer compliance:** `zed_cli/` sits between `mcp` and `transport`; depends only on `protocol`. All process-spawning I/O confined there. `mcp/` stays I/O-free (deferred variant). `thiserror` at the `zed_cli` boundary.

---

## Task 1: `--port` — fixed WebSocket port

**Files:**
- Modify: `crates/zed-claude-bridge/src/app/cli.rs` (DaemonArgs)
- Modify: `crates/zed-claude-bridge/src/transport/ws.rs` (add `bind_fixed` + tests)
- Modify: `crates/zed-claude-bridge/src/transport/mod.rs` (re-export)
- Modify: `crates/zed-claude-bridge/src/app/lifecycle.rs` (bind choice)
- Modify: `scripts/com.virgoC0der.zed-claude-bridge.plist` (template arg)

- [ ] **Step 1: Add the flag to `DaemonArgs`** (after the `zed_db_path` field):

```rust
    /// Fixed WebSocket port. When set, the sidecar binds exactly this port
    /// (failing fast if it is taken) instead of picking a random one in
    /// [10000, 65535]. Pair with `CLAUDE_CODE_SSE_PORT=<N>` in your shell or
    /// Zed terminal env so `claude` auto-connects without `/ide`.
    #[arg(long, value_name = "N")]
    pub port: Option<u16>,
```

- [ ] **Step 2: Write the failing tests for `bind_fixed`** in `ws.rs` (add to the existing `#[cfg(test)] mod tests`; create one if the file has none):

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn bind_fixed_binds_the_requested_port() {
        // Grab a free port from the OS, release it, then bind it fixed.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let (listener, got) = bind_fixed(port).await.expect("bind_fixed");
        assert_eq!(got, port);
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bind_fixed_fails_fast_when_port_taken() {
        let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = holder.local_addr().unwrap().port();
        let err = bind_fixed(port).await.expect_err("must fail while held");
        assert!(matches!(err, TransportError::Io(_)));
    }
```

- [ ] **Step 3: Run to verify failure** — `cargo test -p zed-claude-bridge bind_fixed` → FAIL (`bind_fixed` not found).

- [ ] **Step 4: Implement `bind_fixed`** in `ws.rs`, directly below `bind_random` (mirror its imports/style):

```rust
/// Bind exactly `port` on IPv4 loopback. Unlike [`bind_random`], any bind
/// failure (including `AddrInUse`) is returned immediately — a
/// user-specified fixed port is explicit intent, so we fail fast rather
/// than silently fall back to a random port.
pub async fn bind_fixed(port: u16) -> Result<(TcpListener, u16), TransportError> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    match TcpListener::bind(addr).await {
        Ok(listener) => Ok((listener, port)),
        Err(e) => Err(TransportError::Io(e)),
    }
}
```

Re-export in `transport/mod.rs`: change the `pub use ws::{...}` line to include `bind_fixed`.

- [ ] **Step 5: Wire into `run_daemon`** (`lifecycle.rs`, step "3. Bind the WebSocket listener"). Replace:

```rust
    let (ws_listener, port) = bind_random(16)
        .await
        .context("binding WebSocket listener")?;
```

with:

```rust
    let (ws_listener, port) = match args.port {
        Some(p) => bind_fixed(p)
            .await
            .with_context(|| format!("binding fixed WebSocket port {p} (is another sidecar running?)"))?,
        None => bind_random(16).await.context("binding WebSocket listener")?,
    };
```

and add `bind_fixed` to the existing `use crate::transport::{...}` import.

- [ ] **Step 6: CLI parse test** in `app/cli.rs`'s test mod (mirror existing parse tests):

```rust
    #[test]
    fn daemon_port_flag_parses() {
        let cli = Cli::parse_from(["zed-claude-bridge", "--workspace", "/w", "--port", "52840"]);
        assert_eq!(cli.daemon.port, Some(52840));
    }

    #[test]
    fn daemon_port_flag_defaults_to_none() {
        let cli = Cli::parse_from(["zed-claude-bridge", "--workspace", "/w"]);
        assert_eq!(cli.daemon.port, None);
    }
```

- [ ] **Step 7: plist template** — in `scripts/com.virgoC0der.zed-claude-bridge.plist`, inside `ProgramArguments` after the `--workspace`/`__HOME__` pair, add:

```xml
        <!--
          Stable port so CLAUDE_CODE_SSE_PORT=52840 auto-connects `claude`
          without /ide. Change or remove if 52840 collides on your machine.
        -->
        <string>--port</string>
        <string>52840</string>
```

- [ ] **Step 8: Run** — `cargo test -p zed-claude-bridge bind_fixed && cargo test -p zed-claude-bridge app::cli && cargo clippy --workspace --all-targets -- -D warnings` → PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/zed-claude-bridge/src scripts/com.virgoC0der.zed-claude-bridge.plist
git commit -m "feat(transport,cli): --port flag binds a fixed WebSocket port (fail-fast)"
```

---

## Task 2: `OpenFileArgs` wire type

**Files:**
- Modify: `crates/zed-claude-bridge/src/protocol.rs` (add near `SelectionChangedParams`/`AtMentionedParams`)

- [ ] **Step 1: Write the failing serde tests** (in protocol.rs's test mod):

```rust
    #[test]
    fn open_file_args_minimal_applies_defaults() {
        let v: OpenFileArgs = serde_json::from_str(r#"{"filePath":"/a/b.rs"}"#).unwrap();
        assert_eq!(v.file_path, "/a/b.rs");
        assert!(!v.preview);
        assert!(v.start_text.is_none());
        assert!(v.end_text.is_none());
        assert!(!v.select_to_end_of_line);
        assert!(v.make_frontmost, "makeFrontmost defaults to true");
    }

    #[test]
    fn open_file_args_full_roundtrip_camel_case() {
        let json = r#"{"filePath":"/p","preview":true,"startText":"fn main","endText":"}","selectToEndOfLine":true,"makeFrontmost":false}"#;
        let v: OpenFileArgs = serde_json::from_str(json).unwrap();
        assert_eq!(v.start_text.as_deref(), Some("fn main"));
        assert!(!v.make_frontmost);
        let back = serde_json::to_value(&v).unwrap();
        assert_eq!(back["filePath"], "/p");
        assert_eq!(back["startText"], "fn main");
    }

    #[test]
    fn open_file_args_missing_file_path_fails() {
        assert!(serde_json::from_str::<OpenFileArgs>("{}").is_err());
    }
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p zed-claude-bridge open_file_args` → FAIL.

- [ ] **Step 3: Implement the type**:

```rust
/// Arguments for the `openFile` MCP tool (§3.2). Wire field names are
/// camelCase; serde defaults mirror the VSCode extension's schema
/// (`preview=false`, `selectToEndOfLine=false`, `makeFrontmost=true`).
///
/// Zed-capability notes: `preview`, `endText`, `selectToEndOfLine`, and
/// `makeFrontmost=false` are ACCEPTED for wire compatibility but have no
/// effect — the `zed` CLI can only position a cursor (`path:line:col`) in a
/// focused window; it cannot set selections or open in the background.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFileArgs {
    /// Path to the file to open (absolute, or relative to the first
    /// workspace folder).
    pub file_path: String,
    /// Preview-mode hint. Ignored (see type docs).
    #[serde(default)]
    pub preview: bool,
    /// Text pattern locating the cursor position (first occurrence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_text: Option<String>,
    /// Selection end pattern. Ignored (see type docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_text: Option<String>,
    /// Ignored (see type docs).
    #[serde(default)]
    pub select_to_end_of_line: bool,
    /// Focus hint. Effectively always true with the `zed` CLI.
    #[serde(default = "default_true")]
    pub make_frontmost: bool,
}

fn default_true() -> bool {
    true
}
```

- [ ] **Step 4: Run** — `cargo test -p zed-claude-bridge open_file_args && cargo clippy --workspace --all-targets -- -D warnings` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/zed-claude-bridge/src/protocol.rs
git commit -m "feat(protocol): OpenFileArgs wire type for the openFile tool"
```

---

## Task 3: mcp — advertise `openFile`, defer execution

**Files:**
- Modify: `crates/zed-claude-bridge/src/mcp/tools.rs` (descriptor + test updates)
- Modify: `crates/zed-claude-bridge/src/mcp/server.rs` (variant + dispatch arm + test updates)

- [ ] **Step 1: Update existing tests to the new contract** (they will fail until Steps 3-4):

In `tools.rs` tests: rename/adjust `tools_list_advertises_exactly_the_four_tools` to assert **5** names ending with `"openFile"`; REMOVE `"openFile"` from the forbidden list in `tools_list_does_not_contain_out_of_scope_tools`.

In `server.rs` tests: `tools_list_advertises_exactly_four_tools` → assert `tools.len() == 5` and `names.contains(&"openFile")`; REMOVE `"openFile"` from `tools_list_omits_out_of_scope_names`. Add the two new dispatch tests:

```rust
    #[test]
    fn tools_call_open_file_returns_deferred_variant() {
        let state = EditorState::new();
        let resp = dispatch(
            &state,
            req(
                10,
                "tools/call",
                Some(json!({"name":"openFile","arguments":{"filePath":"/p/a.rs","startText":"fn main"}})),
            ),
        );
        match resp {
            McpResponse::OpenFile { id, args } => {
                assert_eq!(id, RequestId::Number(10));
                assert_eq!(args.file_path, "/p/a.rs");
                assert_eq!(args.start_text.as_deref(), Some("fn main"));
                assert!(args.make_frontmost);
            }
            other => panic!("expected OpenFile, got {other:?}"),
        }
    }

    #[test]
    fn tools_call_open_file_missing_file_path_is_invalid_params() {
        let state = EditorState::new();
        let resp = dispatch(
            &state,
            req(11, "tools/call", Some(json!({"name":"openFile","arguments":{}}))),
        );
        assert_eq!(err_code(resp), error_code::INVALID_PARAMS);
    }
```

- [ ] **Step 2: Run to verify failures** — `cargo test -p zed-claude-bridge mcp` → FAIL (count mismatch, missing variant).

- [ ] **Step 3: Add the descriptor** in `tools.rs`: append `"openFile"` to `TOOL_NAMES`; append to `tools_list()` (after getWorkspaceFolders):

```rust
        Tool {
            name: "openFile".to_string(),
            description: Some(
                "Open a file in the editor, optionally jumping to the position of a text pattern.".to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "Path to the file to open" },
                    "preview": { "type": "boolean", "default": false },
                    "startText": { "type": "string", "description": "Text pattern; the cursor is positioned at its first occurrence" },
                    "endText": { "type": "string", "description": "Accepted for compatibility; Zed cannot set selections" },
                    "selectToEndOfLine": { "type": "boolean", "default": false },
                    "makeFrontmost": { "type": "boolean", "default": true }
                },
                "required": ["filePath"],
                "additionalProperties": false
            }),
        },
```

(Note: the shared `empty_object_schema` clones stay as-is for the first four tools.)

- [ ] **Step 4: Add the deferred variant + dispatch arm** in `server.rs`:

Add imports: `use crate::protocol::{OpenFileArgs, RequestId};` (merge into the existing `use crate::protocol::{...}`).

Extend the enum:

```rust
    /// `tools/call openFile` passed pure validation; the I/O-capable caller
    /// (the transport layer) must launch the editor via `zed_cli` and build
    /// the reply itself. Keeps this module I/O-free per the layer rules.
    OpenFile {
        /// JSON-RPC id to answer with.
        id: RequestId,
        /// Validated tool arguments.
        args: OpenFileArgs,
    },
```

Change `handle_tools_call` to return `McpResponse` (and the `"tools/call"` arm in `dispatch` to `=> handle_tools_call(state, req),`). Wrap every existing `return Response::...`/final-expression site in `McpResponse::Reply(...)`. Insert the openFile arm in the tool-name match, before the catch-all:

```rust
        "openFile" => {
            let args_value = params.arguments.clone().unwrap_or_else(|| json!({}));
            return match serde_json::from_value::<OpenFileArgs>(args_value) {
                Ok(args) => McpResponse::OpenFile { id: req.id, args },
                Err(e) => McpResponse::Reply(Response::failure(
                    req.id,
                    JsonRpcError {
                        code: error_code::INVALID_PARAMS,
                        message: format!("Invalid openFile arguments: {e}"),
                        data: None,
                    },
                )),
            };
        }
```

(Check `CallToolParams`'s arguments field name in protocol.rs — it is the `arguments` member; adapt if it's an `Option<Value>` vs `Value`.)

- [ ] **Step 5: Run** — `cargo test -p zed-claude-bridge mcp && cargo clippy --workspace --all-targets -- -D warnings` → PASS. The ws.rs match on `McpResponse` will NOT compile yet if it's exhaustive — if so, add a temporary arm there returning an internal-error reply, marked `// replaced in Task 5`, OR proceed to implement the minimal arm now (Task 5 replaces it). Record what you did in deviations.

- [ ] **Step 6: Commit**

```bash
git add crates/zed-claude-bridge/src/mcp
git commit -m "feat(mcp): advertise openFile and defer execution to the transport layer"
```

---

## Task 4: `zed_cli` module

**Files:**
- Create: `crates/zed-claude-bridge/src/zed_cli/mod.rs`
- Modify: `crates/zed-claude-bridge/src/lib.rs` (add `pub mod zed_cli;`)

- [ ] **Step 1: Create the module with pure helpers + failing tests first.** Full file:

```rust
//! Launch the `zed` CLI — the I/O backend for the `openFile` MCP tool.
//!
//! Layer position: between `mcp` and `transport`; depends only on
//! `protocol`. All process-spawning I/O for driving the Zed editor lives
//! here and nowhere else.
//!
//! Capability notes (Zed 1.5 CLI): `zed -e <path>[:line:col]` opens a file
//! in an existing window and positions the cursor (1-indexed). It cannot
//! set selections or open without focusing — see `OpenFileArgs`' docs for
//! how the unsupported wire fields degrade.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use crate::protocol::{CallToolResult, OpenFileArgs, ToolContent};

/// Default editor binary, resolved via `PATH`.
pub const DEFAULT_ZED_BIN: &str = "zed";

/// How long we wait for the `zed` CLI to exit (it hands off to the app and
/// returns quickly; 5s is generous).
const ZED_SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from driving the `zed` CLI.
#[derive(Debug, thiserror::Error)]
pub enum ZedCliError {
    /// The binary could not be spawned (missing from PATH, not executable).
    #[error("failed to spawn {bin}: {source}")]
    Spawn {
        /// Binary name or path we tried to run.
        bin: String,
        /// Underlying OS error.
        source: std::io::Error,
    },
    /// The CLI ran but exited non-zero.
    #[error("{bin} exited with status {status}")]
    NonZero {
        /// Binary name or path.
        bin: String,
        /// The non-success exit status.
        status: std::process::ExitStatus,
    },
    /// The CLI did not exit within [`ZED_SPAWN_TIMEOUT`].
    #[error("{bin} timed out after {timeout_ms} ms")]
    Timeout {
        /// Binary name or path.
        bin: String,
        /// The timeout that elapsed, in milliseconds.
        timeout_ms: u64,
    },
}

/// Locate `needle` in `haystack`; return the 1-indexed (line, byte-column)
/// of the match START. Byte columns are exact for ASCII and a close
/// approximation otherwise (Zed treats the column as a character offset).
pub fn locate_text(haystack: &str, needle: &str) -> Option<(u32, u32)> {
    let idx = haystack.find(needle)?;
    let before = &haystack[..idx];
    let line = before.bytes().filter(|b| *b == b'\n').count() as u32 + 1;
    let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let col = (idx - line_start) as u32 + 1;
    Some((line, col))
}

/// Build the `zed -e` path-spec argument: `path[:line:col]`.
pub fn path_spec(path: &Path, position: Option<(u32, u32)>) -> String {
    match position {
        Some((line, col)) => format!("{}:{line}:{col}", path.display()),
        None => path.display().to_string(),
    }
}

/// Resolve `file_path` against `base` when relative (VSCode parity: the
/// base is the first workspace folder).
pub fn resolve_path(file_path: &str, base: Option<&Path>) -> PathBuf {
    let p = Path::new(file_path);
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Some(b) = base {
        b.join(p)
    } else {
        p.to_path_buf()
    }
}

/// Execute an `openFile` request end to end: resolve the path, check
/// existence, locate `startText` (if any), spawn `bin -e <spec>`, and build
/// a [`CallToolResult`] mirroring the VSCode extension's response shapes.
pub async fn open_file(
    bin: &str,
    args: &OpenFileArgs,
    workspace_base: Option<&Path>,
) -> CallToolResult {
    let path = resolve_path(&args.file_path, workspace_base);
    if !path.is_file() {
        return text_result(
            json!({
                "success": false,
                "message": format!("File not found: {}", path.display()),
            })
            .to_string(),
        );
    }

    // startText → 1-indexed cursor position. Read failures degrade to a
    // plain open at the path (tracked so the message stays honest).
    let mut text_missing = false;
    let position = match &args.start_text {
        Some(t) => match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                let pos = locate_text(&contents, t);
                text_missing = pos.is_none();
                pos
            }
            Err(_) => {
                text_missing = true;
                None
            }
        },
        None => None,
    };

    let spec = path_spec(&path, position);
    if let Err(e) = run_zed(bin, &spec).await {
        return text_result(
            json!({
                "success": false,
                "message": format!("Failed to launch zed: {e}"),
            })
            .to_string(),
        );
    }

    let message = match (&args.start_text, position) {
        (Some(t), Some(_)) => format!("Opened file and positioned at \"{t}\""),
        (Some(t), None) if text_missing => {
            format!("Opened file, but text \"{t}\" not found")
        }
        (Some(t), None) => format!("Opened file, but text \"{t}\" could not be located"),
        (None, _) => json!({
            "success": true,
            "filePath": path.display().to_string(),
            "fileUrl": format!("file://{}", path.display()),
            "message": format!("Opened file: {}", path.display()),
        })
        .to_string(),
    };
    text_result(message)
}

/// Spawn `bin -e <spec>` and wait (bounded) for it to exit successfully.
async fn run_zed(bin: &str, spec: &str) -> Result<(), ZedCliError> {
    let mut child = tokio::process::Command::new(bin)
        .arg("-e")
        .arg(spec)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| ZedCliError::Spawn {
            bin: bin.to_string(),
            source: e,
        })?;
    match tokio::time::timeout(ZED_SPAWN_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(ZedCliError::NonZero {
            bin: bin.to_string(),
            status,
        }),
        Ok(Err(e)) => Err(ZedCliError::Spawn {
            bin: bin.to_string(),
            source: e,
        }),
        Err(_) => {
            let _ = child.start_kill();
            Err(ZedCliError::Timeout {
                bin: bin.to_string(),
                timeout_ms: ZED_SPAWN_TIMEOUT.as_millis() as u64,
            })
        }
    }
}

fn text_result(text: String) -> CallToolResult {
    CallToolResult {
        content: vec![ToolContent::Text { text }],
        is_error: None,
    }
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // ----- locate_text ----------------------------------------------------

    #[test]
    fn locate_text_first_line() {
        assert_eq!(locate_text("fn main() {}", "fn main"), Some((1, 1)));
    }

    #[test]
    fn locate_text_later_line_and_column() {
        let s = "fn a() {}\nfn main() {}\n";
        assert_eq!(locate_text(s, "fn main"), Some((2, 1)));
        assert_eq!(locate_text(s, "main"), Some((2, 4)));
    }

    #[test]
    fn locate_text_not_found_and_empty() {
        assert_eq!(locate_text("abc", "zzz"), None);
        assert_eq!(locate_text("", "x"), None);
    }

    // ----- path_spec / resolve_path ----------------------------------------

    #[test]
    fn path_spec_with_and_without_position() {
        let p = Path::new("/a/b.rs");
        assert_eq!(path_spec(p, Some((2, 4))), "/a/b.rs:2:4");
        assert_eq!(path_spec(p, None), "/a/b.rs");
    }

    #[test]
    fn resolve_path_relative_joins_base_absolute_passes_through() {
        assert_eq!(
            resolve_path("src/x.rs", Some(Path::new("/w"))),
            PathBuf::from("/w/src/x.rs")
        );
        assert_eq!(
            resolve_path("/abs/x.rs", Some(Path::new("/w"))),
            PathBuf::from("/abs/x.rs")
        );
        assert_eq!(resolve_path("rel.rs", None), PathBuf::from("rel.rs"));
    }

    // ----- open_file with a fake binary ------------------------------------

    /// Write an executable shell script that records its argv and exits 0.
    fn fake_zed(dir: &Path, capture: &Path, exit_code: i32) -> PathBuf {
        let script = dir.join("fake-zed.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" > {}\nexit {exit_code}\n", capture.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn args_for(path: &str, start_text: Option<&str>) -> OpenFileArgs {
        OpenFileArgs {
            file_path: path.to_string(),
            preview: false,
            start_text: start_text.map(String::from),
            end_text: None,
            select_to_end_of_line: false,
            make_frontmost: true,
        }
    }

    fn result_text(r: &CallToolResult) -> &str {
        let ToolContent::Text { text } = &r.content[0];
        text
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_positions_at_start_text() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);
        let target = tmp.path().join("main.rs");
        std::fs::write(&target, "fn a() {}\nfn main() {}\n").unwrap();

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for(target.to_str().unwrap(), Some("fn main")),
            None,
        )
        .await;
        assert_eq!(
            result_text(&r),
            "Opened file and positioned at \"fn main\""
        );
        let argv = std::fs::read_to_string(&capture).unwrap();
        assert_eq!(argv.trim(), format!("-e {}:2:1", target.display()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_without_start_text_returns_success_json() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);
        let target = tmp.path().join("x.rs");
        std::fs::write(&target, "x").unwrap();

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for(target.to_str().unwrap(), None),
            None,
        )
        .await;
        let body: serde_json::Value = serde_json::from_str(result_text(&r)).unwrap();
        assert_eq!(body["success"], true);
        assert_eq!(body["filePath"], target.to_str().unwrap());
        let argv = std::fs::read_to_string(&capture).unwrap();
        assert_eq!(argv.trim(), format!("-e {}", target.display()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_start_text_not_found_still_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);
        let target = tmp.path().join("x.rs");
        std::fs::write(&target, "nothing here").unwrap();

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for(target.to_str().unwrap(), Some("absent")),
            None,
        )
        .await;
        assert_eq!(
            result_text(&r),
            "Opened file, but text \"absent\" not found"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_missing_file_does_not_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);

        let r = open_file(bin.to_str().unwrap(), &args_for("/no/such/file.rs", None), None).await;
        let body: serde_json::Value = serde_json::from_str(result_text(&r)).unwrap();
        assert_eq!(body["success"], false);
        assert!(body["message"].as_str().unwrap().starts_with("File not found"));
        assert!(!capture.exists(), "fake zed must not have been spawned");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_nonzero_exit_reports_launch_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 3);
        let target = tmp.path().join("x.rs");
        std::fs::write(&target, "x").unwrap();

        let r = open_file(
            bin.to_str().unwrap(),
            &args_for(target.to_str().unwrap(), None),
            None,
        )
        .await;
        let body: serde_json::Value = serde_json::from_str(result_text(&r)).unwrap();
        assert_eq!(body["success"], false);
        assert!(body["message"].as_str().unwrap().starts_with("Failed to launch zed"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn open_file_relative_path_resolves_against_base() {
        let tmp = tempfile::tempdir().unwrap();
        let capture = tmp.path().join("argv.txt");
        let bin = fake_zed(tmp.path(), &capture, 0);
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn x() {}").unwrap();

        let r = open_file(bin.to_str().unwrap(), &args_for("src/lib.rs", None), Some(tmp.path())).await;
        let body: serde_json::Value = serde_json::from_str(result_text(&r)).unwrap();
        assert_eq!(body["success"], true);
        assert!(
            body["filePath"].as_str().unwrap().ends_with("/src/lib.rs"),
            "relative path resolved against base"
        );
    }
}
```

Note: `tokio` needs the `process` feature — it is already enabled for dev-dependencies; check `[dependencies]` tokio features and ADD `"process"` if missing.

- [ ] **Step 2: Register the module** — in `lib.rs`, after `pub mod transport;` add `pub mod zed_cli;` (alphabetical placement also fine; match file's ordering comment).

- [ ] **Step 3: Run** — `cargo test -p zed-claude-bridge zed_cli && cargo clippy --workspace --all-targets -- -D warnings` → all PASS (7 unit + 6 async tests).

- [ ] **Step 4: Commit**

```bash
git add crates/zed-claude-bridge/src/zed_cli crates/zed-claude-bridge/src/lib.rs crates/zed-claude-bridge/Cargo.toml
git commit -m "feat(zed_cli): launch zed CLI to open files at located positions"
```

---

## Task 5: transport — execute the deferred openFile

**Files:**
- Modify: `crates/zed-claude-bridge/src/transport/ws.rs` (builder option + dispatch arm)
- Create: `crates/zed-claude-bridge/tests/open_file.rs`

- [ ] **Step 1: Add the injectable zed binary to the transport.** In `ws.rs`: add a `zed_bin: String` field (default `crate::zed_cli::DEFAULT_ZED_BIN.to_string()`) wherever the connection context carries `cwd_resolver`/`daemon_workspace` (the same struct(s) — follow how `with_cwd_resolver` threads through `TransportBuilder` → connection task). Add the builder method:

```rust
    /// Override the editor binary used by the `openFile` tool. Tests inject
    /// a fake script here; production uses the default (`"zed"` on PATH).
    pub fn with_zed_bin(mut self, bin: impl Into<String>) -> Self {
        self.zed_bin = bin.into();
        self
    }
```

(Adapt field/struct names to the actual builder layout; record any adaptation in deviations.)

- [ ] **Step 2: Handle the variant in `dispatch_text`** (ws.rs ~line 715, where `McpResponse` is matched). Add the arm:

```rust
            McpResponse::OpenFile { id, args } => {
                // Relative paths resolve against the first workspace folder
                // (VSCode parity), falling back to the daemon workspace.
                let base = state_guard
                    .workspace_folders()
                    .first()
                    .cloned()
                    .or_else(|| self.daemon_workspace.clone());
                drop(state_guard);
                let result = crate::zed_cli::open_file(&self.zed_bin, &args, base.as_deref()).await;
                let resp = Response::success(
                    id,
                    serde_json::to_value(result).unwrap_or_else(|_| json!({"content": []})),
                );
                serde_json::to_string(&resp).ok()
            }
```

IMPORTANT adaptation notes: (a) the existing code may hold the state read-guard differently — ensure the guard is dropped before the `.await` (clippy `await_holding_lock` / runtime correctness); (b) `self.daemon_workspace` — use the actual field that `with_daemon_workspace` populates; (c) if Task 3 left a temporary arm here, replace it.

- [ ] **Step 3: Integration test** — create `crates/zed-claude-bridge/tests/open_file.rs`. Mirror the WebSocket setup helpers from `crates/zed-claude-bridge/tests/handshake.rs` (auth header, initialize handshake). Test body:

```rust
//! WS-level integration: tools/call openFile → fake zed binary receives the
//! positioned path spec, and the MCP reply mirrors the VSCode shapes.

// Mirror the setup/connect helpers from tests/handshake.rs (auth token,
// Transport::builder(...).with_zed_bin(...), tokio-tungstenite client).

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

    // 1. Start a Transport with .with_zed_bin(script) on a random port,
    //    using the same builder wiring as handshake.rs tests.
    // 2. Connect an authorized WS client; complete initialize.
    // 3. Send: {"jsonrpc":"2.0","id":7,"method":"tools/call","params":{
    //      "name":"openFile","arguments":{
    //        "filePath": <target abs path>, "startText":"fn main"}}}
    // 4. Read the response frame for id 7 and assert:
    //      result.content[0].text == "Opened file and positioned at \"fn main\""
    // 5. Assert the capture file's content:
    //      format!("-e {}:2:1", target.display())
}
```

Write the helper-dependent parts (steps 1-4 comments above) as REAL code by copying the connect/auth pattern from `tests/handshake.rs` — read that file first; do not invent a new harness.

- [ ] **Step 4: Run** — `cargo test -p zed-claude-bridge --test open_file && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings` → all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/zed-claude-bridge/src/transport crates/zed-claude-bridge/tests/open_file.rs
git commit -m "feat(transport): execute deferred openFile via zed_cli with injectable binary"
```

---

## Task 6: specs + docs

**Files:**
- Modify: `docs/protocol.md` (§3.2 table row)
- Modify: `openspec/specs/mcp/spec.md` (openFile moves in scope)
- Modify: `README.md` (two new subsections)
- Modify: `.harness/project.md` (layer order)

- [ ] **Step 1: protocol.md** — §3.2 table: change the `openFile` row's "First-cut scope" cell from `optional` to `**YES** — positions the cursor only (`zed -e path:line:col`); selection-related args are accepted but ignored (Zed CLI limitation)`.

- [ ] **Step 2: openspec mcp spec** — read `openspec/specs/mcp/spec.md`; update the requirement that forbids advertising out-of-scope tools so `openFile` is no longer in the forbidden set, and add a scenario in the file's existing Given/When/Then style:

```markdown
#### Scenario: openFile positions the editor via the zed CLI
- GIVEN a connected MCP client and a file `/w/src/main.rs` containing `fn main`
- WHEN the client calls `tools/call` `openFile` with `{"filePath": "/w/src/main.rs", "startText": "fn main"}`
- THEN the sidecar spawns `zed -e /w/src/main.rs:<line>:<col>` with the located 1-indexed position
- AND the reply's text content is `Opened file and positioned at "fn main"`
- AND selection-related arguments (`endText`, `selectToEndOfLine`, `makeFrontmost:false`, `preview`) are accepted without error and ignored
```

- [ ] **Step 3: README** — add under the existing usage sections:

```markdown
### Let Claude open files in your Zed (openFile)

The sidecar advertises the `openFile` MCP tool. When Claude wants to show
you code, it can jump your Zed window straight to the file — positioned at
a text match — via `zed -e <path>:<line>:<col>`.

Capability note: the Zed CLI can position the cursor but cannot create a
selection or open files in the background, so `startText` positions the
cursor at the match and `endText` / `selectToEndOfLine` /
`makeFrontmost:false` / `preview` are accepted (for protocol compatibility)
but ignored. Requires the `zed` CLI on `PATH` (Zed: "Install CLI").

### Auto-connect from any terminal (stable port)

Pass `--port <N>` to pin the sidecar's WebSocket port (the LaunchAgent
template pins `52840`). With a fixed port, set:

```bash
export CLAUDE_CODE_SSE_PORT=52840   # shell rc: every `claude` auto-connects
```

or per-project for Zed's built-in terminal (`.zed/settings.json`):

```json
{ "terminal": { "env": { "CLAUDE_CODE_SSE_PORT": "52840" } } }
```

`claude` then connects to the sidecar on startup — no `/ide` needed. If the
fixed port is taken at startup the sidecar exits with a clear error rather
than silently moving (check the log; pick another port).
```

- [ ] **Step 4: .harness/project.md** — in `layer_order`, insert between `mcp/` (3) and `transport/` (4):

```markdown
  4. `zed_cli/` — drive the Zed editor via its CLI (`zed -e`); the only module allowed to spawn editor processes (depends on protocol)
```

renumbering the later entries (transport 5, ipc 6, app 7, main.rs 8). Add a placement rule: `tokio::process spawning of the editor CLI lives only in zed_cli/.`

- [ ] **Step 5: Full verification** — `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` → all PASS.

- [ ] **Step 6: Commit**

```bash
git add docs/protocol.md openspec/specs/mcp/spec.md README.md .harness/project.md
git commit -m "docs: openFile in scope + stable-port auto-connect guide"
```

---

## Self-Review Notes

**Spec coverage:** D1 deferred variant → Task 3; D2 zed_cli module → Task 4; D3 honest degradation messages → Tasks 2 (docs on type) & 4 (message strings); D4 error shapes → Task 4 (`File not found`/`Failed to launch zed`); D5 spec updates → Tasks 3 (tests) & 6 (docs/specs); D6 bind_fixed fail-fast → Task 1; D7 plist port → Task 1 Step 7; D8 README auto-connect → Task 6 Step 3.

**Placeholder scan:** Task 5's integration test contains commented step descriptions for the parts that MUST be copied from `tests/handshake.rs` (existing harness, explicitly instructed to read & reuse) — intentional, with full assertions specified. All other code complete.

**Type consistency:** `OpenFileArgs` fields used in Task 3 tests, Task 4 (`args_for`) and Task 5 JSON match Task 2's definition. `open_file(bin, &args, base) -> CallToolResult` identical across Tasks 4/5. `bind_fixed(port) -> Result<(TcpListener, u16), TransportError>` matches Task 1 Steps 2/4/5. `McpResponse::OpenFile { id: RequestId, args: OpenFileArgs }` consistent across Tasks 3/5.
