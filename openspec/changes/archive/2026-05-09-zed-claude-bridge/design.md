# Design: Zed ↔ Claude Code IDE Bridge

## Context

Greenfield project at `/Users/sx.chen/Code/personal/claude-code-zed`. The repository
currently contains only `docs/protocol.md` and `.harness/project.md`. We need to
deliver an end-to-end working bridge that mirrors Anthropic's VSCode/JetBrains Claude
Code plugin for Zed, using the protocol fully reverse-engineered from VSCode extension
v2.1.76 (see `docs/protocol.md` — that document is **the spec**; this design document
maps it onto Rust modules and does not re-derive it).

Key constraints inherited from `.harness/project.md`:

- Rust 2024 edition, MSRV 1.85, `cargo` workspace.
- `tokio` runtime; `tokio-tungstenite` for WebSocket (rustls disabled).
- Strict layer order: `protocol` → `lockfile` → `mcp` → `transport` → `ipc` → `app` →
  `main`.
- All wire types live in `protocol/`; I/O is forbidden in `protocol/` and `mcp/`.
- `unsafe` is forbidden. `tracing` for logs (no `println!` outside `main.rs`/tests).
- Auth token: per-launch UUID v4, never logged at INFO.
- Lock-file permissions: file `0600`, parent dir `0700`, verified on every write.

The Zed extension cannot host a TCP server (WASM sandbox), so all network and
filesystem behavior lives in the sidecar; the extension is a thin IPC client.

## Goals / Non-Goals

**Goals:**

- A Zed user with the extension installed and the sidecar running can execute `/ide`
  in Claude Code CLI and have it discover Zed.
- Right-click → **Send to Claude Code** on a selection delivers `@<rel-path>#L<a>-<b>`
  to the active Claude Code prompt (via the `at_mentioned` JSON-RPC notification).
- The four read-only MCP tools (`getCurrentSelection`, `getLatestSelection`,
  `getOpenEditors`, `getWorkspaceFolders`) return correct, last-known editor state.
- The system is buildable and testable from `cargo` alone — no shell scripts in the
  critical path, no hidden state.
- Workspace remains buildable (`cargo build --workspace` succeeds and `cargo test
  --workspace` passes) after every implementation task.

**Non-Goals:**

- `openDiff`, `getDiagnostics`, `executeCode`, `close_tab`, `closeAllDiffTabs`,
  `openFile`, `checkDocumentDirty`, `saveDocument` MCP tools.
- Multi-window targeting / workspace selection from CLI side.
- Windows path handling (we target macOS + Linux only in this iteration).
- TLS / non-loopback bind. The sidecar is `127.0.0.1` only.
- stdio MCP transport.
- A real Zed-driven UI test harness (we will use mock/stub editor state in tests).

## Decisions

### D1 — Cargo workspace with two members

```
claude-code-zed/
├── Cargo.toml                          # [workspace]
├── crates/
│   └── zed-claude-bridge/              # sidecar binary
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs                 # entrypoint only
│           ├── lib.rs                  # re-exports for integration tests
│           ├── protocol/               # pure data types + serde
│           │   ├── mod.rs
│           │   ├── jsonrpc.rs          # JSON-RPC 2.0 envelopes
│           │   ├── lockfile.rs         # lock-file JSON shape
│           │   ├── mcp.rs              # initialize/tools-list/tools-call payloads
│           │   ├── notifications.rs    # selection_changed, at_mentioned
│           │   └── ipc.rs              # IPC frame envelope
│           ├── lockfile/               # ~/.claude/ide/<port>.lock I/O
│           │   └── mod.rs
│           ├── mcp/                    # MCP server logic (no I/O)
│           │   ├── mod.rs
│           │   ├── server.rs           # request dispatch
│           │   ├── tools.rs            # tool implementations over EditorState
│           │   └── state.rs            # last-known editor state
│           ├── transport/              # WebSocket
│           │   ├── mod.rs
│           │   └── ws.rs               # accept loop, auth, single-client policy
│           ├── ipc/                    # Unix-socket bridge to extension
│           │   ├── mod.rs
│           │   └── server.rs
│           └── app/                    # wiring
│               ├── mod.rs
│               ├── cli.rs              # CLI args via clap
│               └── lifecycle.rs        # signals, startup, shutdown
│       └── tests/
│           ├── handshake.rs            # MCP handshake round-trip
│           └── at_mention.rs           # IPC → notification round-trip
└── extension/
    └── zed-claude-code/
        ├── Cargo.toml
        ├── extension.toml              # Zed extension manifest
        └── src/
            └── lib.rs                  # registers commands; talks IPC
```

Rationale: matches the layer order in `.harness/project.md` exactly. Each layer
imports only the layers strictly below it. `protocol/` is leaf-pure; `app/` is the
only place that wires everything.

**Alternative considered:** single-crate sidecar with feature flags. Rejected — the
WASM extension has very different dependency constraints (no tokio, restricted std)
and must be a separate crate.

### D2 — Async runtime, dependencies, TLS

- `tokio` (multi-thread runtime, `rt-multi-thread` + `macros` + `signal` + `net` +
  `time` + `sync` + `io-util`).
- `tokio-tungstenite` with `default-features = false` (no rustls, no native-tls — we
  bind loopback plaintext only).
- `serde` + `serde_json` for JSON; `uuid` v1 with `v4` for the auth token.
- `tracing` + `tracing-subscriber` (env-filter) for logging.
- `thiserror` for typed module errors; `anyhow::Result` only at the binary edge.
- `xxhash-rust` for the workspace-hash IPC socket name.
- `clap` (derive) for `app/cli.rs`.
- WASM extension uses `zed_extension_api` only; no tokio, no tungstenite. It connects
  to the IPC socket via blocking `std::os::unix::net::UnixStream` (the WASM API does
  expose synchronous host functions for sockets — if not, we wire through a
  Zed-provided `process` API to spawn a helper binary; **see open question OQ1**).

### D3 — Editor state model

```rust
// protocol/notifications.rs (shared shape)
pub struct EditorPosition { pub line: u32, pub character: u32 }
pub struct EditorRange    { pub start: EditorPosition, pub end: EditorPosition, pub is_empty: bool }
pub struct EditorSelection {
    pub text: String,
    pub file_path: String,
    pub file_url: String,           // file:// URL
    pub selection: EditorRange,
}
```

The sidecar holds a single `Arc<RwLock<EditorState>>` that contains:

- `current_selection: Option<EditorSelection>` (cleared on focus loss).
- `latest_selection: Option<EditorSelection>` (sticky, updated on every change).
- `open_editors: Vec<OpenEditor>`.
- `workspace_folders: Vec<WorkspaceFolder>`.

All MCP tools read from this state; selection-related IPC frames write to it.

### D4 — Lock-file lifecycle

1. On startup, scan `~/.claude/ide/*.lock`. For each, attempt a TCP connect to its
   `127.0.0.1:<port>`. If the connect refuses (process gone), `unlink` the file. We do
   not perform the WS handshake — TCP refusal is a sufficient liveness signal and
   avoids touching other live sidecars.
2. Bind a TCP listener on a random port in `10000..=65535`; retry up to 16 times on
   `EADDRINUSE`. The chosen port is the lock-file's name.
3. Generate a UUID v4 auth token. Write the JSON to `~/.claude/ide/<port>.lock` with
   `OpenOptions::mode(0o600)`; ensure the parent dir is `0o700`. Use a `.tmp` rename
   for atomicity.
4. On `workspace_folders` IPC update, rewrite the lock file in place (same port, same
   token).
5. On graceful shutdown (SIGINT/SIGTERM/SIGHUP), `unlink` the lock file. We register
   `tokio::signal::unix` handlers; on Windows-target builds we'd use ctrl_c, but
   Windows is out of scope.

### D5 — WebSocket transport

- Accept loop on the bound listener; for each TCP connection, perform the WS upgrade.
- Validate `x-claude-code-ide-authorization` against the in-memory auth token. On
  mismatch, send WS close `1008` and drop. Log at WARN with token redacted.
- Single-client policy: when a new authorized connection arrives, send the existing
  client a close frame with code `1000` and reason `"Disconnecting previous WebSocket
  client"`, then accept the new one.
- Per-connection task owns the read half (parses JSON-RPC, dispatches to MCP) and
  shares an `mpsc::Sender<ServerMessage>` for outbound notifications/responses.
- Notifications (`selection_changed`, `at_mentioned`) are pushed by the IPC handler
  through a broadcast channel that the connected client task subscribes to.

### D6 — MCP dispatch

Implemented in `mcp/server.rs` as a pure function over `(EditorState, JsonRpcRequest)
→ JsonRpcResponse | Notification`. No I/O. Methods supported:

- `initialize` — return `protocolVersion: "2024-11-05"`,
  `capabilities.tools.listChanged = false`, `serverInfo` `{name:
  "zed-claude-bridge", version: env!("CARGO_PKG_VERSION")}`.
- `notifications/initialized` — accept, no response.
- `ping` — `{}` result.
- `tools/list` — return four tool schemas (built statically).
- `tools/call` — dispatch by `params.name`; unknown name returns error `-32602`.
- Anything else (`resources/*`, `prompts/*`, …) returns error `-32601` (Method not
  found).

### D7 — Notifications

- `selection_changed`: gated by a `tokio::time::sleep` debounce. The IPC handler
  cancels and resets a 300 ms timer on every selection IPC frame. When the timer
  fires, if the selection's text or range differs from the last sent one, fire the
  notification. Skip when `file_path` URI scheme is `comment` or `output`.
- `at_mentioned`: fired immediately, without debounce, when an `at_mention` IPC frame
  arrives. Lines in the payload are 1-indexed (per protocol §3.3). The IPC frame
  carries 0-indexed lines (raw editor positions); we add 1 when building the
  notification.

### D8 — IPC

- Unix-domain-socket listener at `$TMPDIR/zed-claude-bridge-<workspace-hash>.sock`,
  where `<workspace-hash> = xxh3_64_hex(canonicalized_workspace_root)`.
- Line-delimited JSON. One frame per line. Use `tokio::io::AsyncBufReadExt::lines`.
- Multiple concurrent IPC clients allowed (Zed could open the same workspace from two
  windows, edge case). Each frame is parsed and applied to `EditorState` under the
  write lock.
- Frame envelope: `{"type": "...", ...}`. Types: `selection`, `at_mention`,
  `workspace_folders`, `open_editors`, `ping`. Sidecar replies (rare, mostly for
  diagnostics): `{"type":"ack"}`, `{"type":"log",...}`.

### D9 — Zed extension surface

- `extension.toml` declares one slash command (`/claude-send`) and one context-menu
  action (`zed-claude-code:send-selection`).
- `src/lib.rs` implements `zed_extension_api::Extension`.
- On activation: compute workspace hash, look up the IPC socket path, attempt
  connect; if connect fails, spawn the sidecar binary (path resolved from `which
  zed-claude-bridge` or a configured setting) and retry with backoff.
- Selection events: when the user invokes the command, the extension serializes a
  `selection` (or `at_mention`) frame and writes one line to the socket. No persistent
  TCP state is required from the extension's point of view.

### D10 — CLI flags

```
zed-claude-bridge [--workspace <path>] [--foreground] [--log-level <lvl>]
                  [--ipc-socket <path>] [--lock-dir <path>]
```

`--workspace` is required when running standalone (used to derive the IPC socket
hash). Defaults: `--foreground=false` (daemonize via simple double-fork? — see
**OQ2**), `--log-level=info`, `--ipc-socket` derived from `$TMPDIR` + workspace hash,
`--lock-dir=~/.claude/ide`.

## Risks / Trade-offs

- **[Risk] Zed WASM extension API may not expose Unix-socket I/O directly.** →
  Mitigation: fall back to a tiny helper sub-binary launched by the extension that
  bridges stdin/stdout to the socket. Decision deferred to OQ1.
- **[Risk] Random-port collisions or port-reuse races on rapid restarts.** →
  Mitigation: 16-retry bind loop + TCP-connect liveness check before deleting stale
  lock files.
- **[Risk] Lock-file directory permissions on a freshly minted machine.** →
  Mitigation: always `mkdir -p` with mode `0700`; if existing dir has wider mode, log
  a WARN and continue (re-permissioning the user's `~/.claude` is too invasive).
- **[Risk] Auth token leaking via logs.** → Mitigation: token typed as
  `secrecy::SecretString` (or a small custom newtype that overrides `Debug`); never
  formatted at INFO level — only at DEBUG, and even then redacted to a prefix.
- **[Risk] Single-client policy race when two CLIs reconnect simultaneously.** →
  Mitigation: serialize new-connection handling through a single tokio task; the
  prior client is sent a close frame and the new connection is fully spun up before
  the listener accepts the next.
- **[Trade-off] No TLS, loopback only.** → Adequate because the auth token is a
  64-bit-equivalent secret on a 127.0.0.1 socket; consistent with VSCode plugin.
- **[Trade-off] Polling-style stale-lock cleanup at startup.** → Linear in the number
  of files, but the directory is small in practice; acceptable.

## Migration Plan

This is a greenfield project, so there is nothing to migrate. Rollback is `rm -rf`
the workspace.

## Open Questions

- **OQ1**: Does Zed's `zed_extension_api` (current crate version targeted by Zed
  stable on macOS) expose Unix-domain-socket I/O from inside WASM, or do we need a
  helper sub-binary spawned by the extension to bridge to the sidecar's IPC socket?
  *Captured for the implementer; the IPC frame format is decided either way.*
- **OQ2**: Should the sidecar daemonize on launch (double-fork) or always run as a
  tokio task in foreground supervised by Zed/launchd? *Tentative: keep it
  foreground-only in the first cut; supervision is the user's responsibility. We can
  add `--foreground` as the default and revisit later.*
- **OQ3**: Lock-file `pid` field — protocol doc says "the CLI doesn't validate"; we
  use the sidecar's own PID. Confirm this is acceptable to all CLI versions in the
  wild (≥ 2.1.x). *Spec-level acceptance; not a code blocker.*
