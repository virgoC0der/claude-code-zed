# Proposal: Zed ↔ Claude Code IDE Bridge

## Why

The Claude Code CLI ships first-class IDE integrations for VSCode and JetBrains: when a
user has the editor open and runs `/ide` in a Claude Code session, the CLI discovers the
editor, attaches over WebSocket, and the user can right-click on a selection to send an
`@<file>#L<start>-<end>` mention to the active Claude prompt. **Zed has no equivalent.**

Zed users are forced to copy/paste paths and line numbers manually, which is slow and
error-prone. We have a fully reverse-engineered protocol (`docs/protocol.md`, sourced
from VSCode extension v2.1.76) — enough to build a compatible bridge today without
guessing. This change delivers a minimal, end-to-end working bridge so a Zed user can
run `/ide` and use **Send to Claude Code** on a selection.

## What Changes

- Add a new Cargo workspace at the repository root with two members:
  - `crates/zed-claude-bridge` — sidecar binary (Rust, tokio + tokio-tungstenite).
  - `extension/zed-claude-code` — Zed extension (Rust → WASM).
- The sidecar:
  - Binds a random port in `10000..=65535` on `127.0.0.1`, hosts a JSON-RPC 2.0
    WebSocket server that speaks the MCP `2024-11-05` protocol.
  - Writes `~/.claude/ide/<port>.lock` with mode `0600` inside a `0700` parent dir, so
    the Claude Code CLI's `/ide` command can discover it.
  - Validates the per-launch UUID v4 auth token via the
    `x-claude-code-ide-authorization` request header; rejects mismatches with WS close
    `1008`. Single connected client at a time.
  - Implements the four read-only MCP tools — `getCurrentSelection`,
    `getLatestSelection`, `getOpenEditors`, `getWorkspaceFolders` — answered from the
    sidecar's last-known editor state.
  - Emits two outbound JSON-RPC notifications — `selection_changed` (300 ms debounced)
    and `at_mentioned` (one-shot, fired on user action).
  - Hosts a Unix-domain-socket IPC server at
    `$TMPDIR/zed-claude-bridge-<workspace-hash>.sock` that the Zed extension uses to
    push selection updates and trigger at-mentions.
  - Cleans up its own lock file on graceful shutdown (SIGINT/SIGTERM); on startup,
    prunes stale lock files belonging to dead processes.
- The Zed extension:
  - Registers a slash command and a context-menu / command-palette entry called
    **Send to Claude Code**.
  - Spawns the sidecar (if not already running for this workspace) and connects to its
    IPC socket.
  - On selection changes, pushes selection state over IPC; on user action, fires an
    `at_mention` IPC frame.
- Add `docs/protocol.md` references and a top-level `README.md` with build/install/use
  instructions.
- **Out of scope (explicit non-goals)**: `openDiff`, `getDiagnostics`, `executeCode`,
  multi-window targeting, Windows path handling, stdio MCP transport, `close_tab` /
  diff-tab management, `openFile`, `checkDocumentDirty`, `saveDocument`.

## Capabilities

### New Capabilities

- `protocol`: Pure data-type layer — serde-derived structs/enums for JSON-RPC 2.0
  envelopes, the lock-file JSON shape, MCP request/response payloads, IDE-bound
  notification params, and the Unix-socket IPC frame envelope. This is the
  byte-level contract; every other capability builds on it.
- `lockfile`: Discovery file at `~/.claude/ide/<port>.lock` — atomic write, correct
  permissions, stale-lock pruning, and graceful-shutdown cleanup.
- `websocket`: Localhost WebSocket transport — random-port bind, auth-header gate,
  single-client policy, JSON-RPC 2.0 framing.
- `mcp`: MCP server surface — `initialize`, `notifications/initialized`, `ping`,
  `tools/list`, `tools/call` dispatch, plus the four read-only tools that serve from
  last-known editor state.
- `notifications`: Outbound JSON-RPC notifications — `selection_changed` (debounced
  300 ms, skips `comment://` and `output://` URIs) and `at_mentioned` (one-shot).
- `ipc`: Unix-domain-socket bridge between the Zed extension and the sidecar — line-
  delimited JSON, typed message envelope.
- `zed-extension`: Zed extension that registers the slash command / context-menu
  action, locates or spawns the sidecar, and emits IPC frames on selection events.

### Modified Capabilities

(none — greenfield project)

## Impact

- New: Cargo workspace, two crates, one Zed extension manifest, one top-level README.
- Filesystem side effects: creates/removes `~/.claude/ide/<port>.lock` (mode 0600,
  parent 0700) and `$TMPDIR/zed-claude-bridge-<workspace-hash>.sock` per running
  workspace.
- Network side effects: binds a single TCP listener on `127.0.0.1:<random-port>`. No
  outbound traffic. No TLS.
- Dependencies: `tokio`, `tokio-tungstenite` (rustls disabled), `serde`,
  `serde_json`, `uuid`, `tracing`, `tracing-subscriber`, `thiserror`, `anyhow`,
  `xxhash-rust`, plus Zed's `zed_extension_api` for the WASM crate.
- Security: per-launch UUID v4 auth token, never logged at INFO level; loopback bind
  only; `unsafe` is forbidden.
- No impact on existing code (greenfield).
