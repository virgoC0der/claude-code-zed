# Implementation Tasks

> Each task must leave the workspace in a buildable state (`cargo build --workspace`
> succeeds). Each task lists a concrete verification command/assertion. Tasks are
> ordered by the layer order in `.harness/project.md`:
> protocol → lockfile → mcp → transport → ipc → app → main.

## 1. Workspace scaffold

- [x] 1.1 Create `Cargo.toml` at the repo root declaring a workspace with members
      `crates/zed-claude-bridge` and `extension/zed-claude-code` and a shared
      `[workspace.package]` block (edition `2024`, MSRV `1.85`, license, repo URL).
      Add a `.gitignore` covering `target/`, `*.lock` for the WASM crate (the bin
      crate keeps its `Cargo.lock`). Verify: `cargo metadata --no-deps --format-version 1 | jq '.workspace_members | length'` returns `2`.
- [x] 1.2 Scaffold `crates/zed-claude-bridge/` with a `Cargo.toml` declaring `[lib]`
      and `[[bin]] name = "zed-claude-bridge"`, dependencies (`tokio` with `full`,
      `tokio-tungstenite` with `default-features = false`, `serde`, `serde_json`,
      `uuid` v1 (`v4`), `tracing`, `tracing-subscriber`, `thiserror`, `anyhow`,
      `xxhash-rust` with `xxh3`, `clap` derive). Place a stub `src/main.rs`
      (`fn main() { tracing_subscriber::fmt::init(); }`) and `src/lib.rs` with empty
      module declarations matching the layer plan
      (`pub mod protocol; pub mod lockfile; pub mod mcp; pub mod transport; pub mod ipc; pub mod app;`).
      Each module is an empty `mod.rs`. Verify: `cargo build --workspace` and
      `cargo clippy --workspace --all-targets -- -D warnings` succeed.
- [x] 1.3 Scaffold `extension/zed-claude-code/` with a `Cargo.toml` declaring
      `[lib] crate-type = ["cdylib"]`, dependency `zed_extension_api`, and an
      `extension.toml` with `id = "zed-claude-code"`, `name = "Send to Claude Code"`,
      `version` matching the bin crate, `authors`, `description`, plus an empty
      `src/lib.rs` exporting a stub `Extension` impl (or `zed_extension_api::register_extension!`
      macro with a placeholder type). Verify: `cargo build --workspace` succeeds; the
      extension manifest is parseable (`grep '^id =' extension/zed-claude-code/extension.toml`).
- [x] 1.4 Add a workspace-level `rustfmt.toml` matching house style (edition 2024,
      `max_width = 100`) and a top-level `README.md` placeholder linking to
      `docs/protocol.md`. Verify: `cargo fmt --all --check` succeeds.

## 2. Protocol module (pure data types, no I/O)

> Spec: `specs/protocol/spec.md` (every requirement must hold; the verifier will
> byte-match field names from `docs/protocol.md`).

- [x] 2.1 In `crates/zed-claude-bridge/src/protocol/jsonrpc.rs` define
      `JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcNotification`, and `JsonRpcError`
      enums/structs with `serde` derives. Support `id` as `Option<RequestId>` where
      `RequestId` is an untagged enum of string/number/null. Add unit tests round-
      tripping each shape. Verify: `cargo test -p zed-claude-bridge protocol::jsonrpc`
      passes.
- [x] 2.2 In `protocol/lockfile.rs` define `LockFile { pid, workspace_folders,
      ide_name, transport, running_in_windows, auth_token }` with `#[serde(rename_all
      = "camelCase")]`. Add a unit test that the serialized JSON contains the exact
      key set per `specs/lockfile/spec.md` ("Lock-file JSON payload"). Verify: `cargo
      test -p zed-claude-bridge protocol::lockfile` passes.
- [x] 2.3 In `protocol/mcp.rs` define `InitializeResult`, `ServerInfo`, `Capabilities
      { tools: ToolsCapability { list_changed: bool } }`, `Tool`, `ToolsListResult`,
      `ToolCallParams`, `ToolCallResult` (untyped JSON value for the `content`
      field). Add a unit test asserting the static `tools/list` payload contains
      exactly the four tool names from `specs/mcp/spec.md`. Verify: `cargo test`
      passes.
- [x] 2.4 In `protocol/notifications.rs` define `EditorPosition`, `EditorRange`,
      `EditorSelection`, plus `SelectionChangedParams` and `AtMentionedParams`. Tests:
      (a) sample `selection_changed` JSON deserializes correctly; (b) building an
      `at_mentioned` payload from `(file, 9, 19)` produces JSON whose `lineStart =
      10`, `lineEnd = 20` (1-indexed). Verify: `cargo test` passes.
- [x] 2.5 In `protocol/ipc.rs` define a tagged enum `IpcFrame` with
      `#[serde(tag = "type", rename_all = "snake_case")]` covering variants
      `Selection { ... }`, `AtMention { ... }`, `WorkspaceFolders { folders }`,
      `OpenEditors { editors }`, `Ping`, plus reply variants `Ack`, `Log { level,
      message }`. Tests: each variant round-trips through `serde_json`; unknown tag
      deserializes as a custom `Unknown` fallback (or fails with a typed error the
      caller can ignore). Verify: `cargo test` passes.
- [x] 2.6 Wire all submodules into `protocol/mod.rs` with `pub mod ...`. Verify:
      `cargo clippy --workspace --all-targets -- -D warnings` succeeds.

## 3. Lock-file module

- [x] 3.1 In `lockfile/mod.rs` implement `LockDir::open(path: &Path) -> Result<Self>`
      that creates the directory with mode `0o700` if missing and verifies the mode
      otherwise. Define a `thiserror`-based `LockfileError` for I/O / permission
      failures. Verify (unit test, tempfile dir): newly created dir has mode `0o700`.
- [x] 3.2 Implement `LockDir::write_lock(&self, port: u16, body: &LockFile) ->
      Result<()>` performing atomic write: write to `<port>.lock.tmp` with mode
      `0o600`, fsync, rename. Verify (unit test): after `write_lock` the mode is
      `0o600` and the parsed JSON round-trips.
- [x] 3.3 Implement `LockDir::remove_lock(&self, port: u16) -> Result<()>` that
      unlinks the file (idempotent: missing is OK). Implement `LockDir::list(&self)
      -> Result<Vec<u16>>` returning ports parsed from existing `<port>.lock`
      filenames (skipping malformed names with a warn log).
- [x] 3.4 Implement `LockDir::prune_stale(&self) -> Result<()>` that for each port
      returned by `list()`, attempts a TCP connect to `127.0.0.1:<port>` with a 250
      ms timeout, and unlinks the lock file on `ECONNREFUSED`. Live ports are kept.
      Verify (integration test using a `TcpListener` we hold alive on a known port,
      and another stale lock pointing at an unbound port): only the stale one is
      pruned.
- [x] 3.5 Add an integration test under `crates/zed-claude-bridge/tests/lockfile.rs`
      driving the full sequence (create dir → write → list → prune → remove) inside
      a `tempfile::TempDir`. Verify: `cargo test -p zed-claude-bridge --test lockfile`
      passes.

## 4. MCP module (pure dispatch over EditorState)

- [x] 4.1 In `mcp/state.rs` define `EditorState { current_selection, latest_selection,
      open_editors, workspace_folders }` with `Default` and methods:
      `apply_selection`, `clear_current_selection`, `set_open_editors`,
      `set_workspace_folders`. No I/O. All updates take `&mut self`.
- [x] 4.2 In `mcp/tools.rs` implement four tool functions
      `tool_get_current_selection(state) -> ToolCallResult`,
      `tool_get_latest_selection`, `tool_get_open_editors`,
      `tool_get_workspace_folders`. Each returns the JSON shape required by
      `specs/mcp/spec.md`. Provide a static `TOOLS_LIST: &[Tool]` describing all
      four. Verify (unit tests, one per tool): empty state → `success: false` for
      selection tools; populated state → matching JSON.
- [x] 4.3 In `mcp/server.rs` implement `dispatch(state: &EditorState, req:
      JsonRpcRequest) -> McpResponse` (an enum: `Response`, `NoReply`, `Error`).
      Handle `initialize`, `notifications/initialized`, `ping`, `tools/list`,
      `tools/call`, and the catch-all `-32601`. Verify (unit tests): handshake
      returns version `"2024-11-05"`; unknown method returns `-32601`; unknown tool
      name returns `-32602`.
- [x] 4.4 Add property-style tests that for each method in `{initialize, ping,
      tools/list, tools/call (each tool)}`, the response satisfies
      `serde_json::from_str` on the expected output type. Verify: `cargo test -p
      zed-claude-bridge mcp::` passes.

## 5. WebSocket transport

- [x] 5.1 In `transport/ws.rs` implement `bind_random(retry: usize) -> Result<(TcpListener,
      u16)>` that picks a random port in `10000..=65535` and retries on
      `EADDRINUSE`. Verify (unit test, holding an existing listener on a known port
      that the RNG might select): a fresh bind eventually succeeds without panicking.
- [x] 5.2 Implement the per-connection async task: read the HTTP upgrade request,
      pull `x-claude-code-ide-authorization`, constant-time-compare against the
      auth token, and on mismatch send WS close `1008` and drop. On match, complete
      the upgrade. Verify (integration test): a request without the header receives
      close `1008`; a request with the wrong token also gets `1008`; correct token
      upgrades to WS.
- [x] 5.3 Implement single-client policy: a `tokio::sync::Mutex<Option<ClientHandle>>`
      where each new authorized connection sends a close frame (code `1000`, reason
      `"Disconnecting previous WebSocket client"`) to the prior client before
      replacing it. Verify (integration test): connect A, then connect B; assert A
      receives close `1000` and B is the active one.
- [x] 5.4 Implement the request loop: receive text frames, parse JSON-RPC, dispatch
      via `mcp::server::dispatch`, send the response on the same connection. Binary
      frames produce a WARN log and no reply. Implement `notify_all(notification:
      JsonRpcNotification)` wired through a `broadcast::Sender` so the IPC layer can
      push outbound notifications. Verify (integration test): `tools/list` round-
      trip; pushing a notification via the broadcast sender delivers it as a text
      frame to the connected client.
- [x] 5.5 Add an integration test under `tests/handshake.rs` that drives the full
      handshake (`initialize` → `notifications/initialized` → `tools/list`) over a
      real `tokio-tungstenite` client against an in-process server. Verify: `cargo
      test -p zed-claude-bridge --test handshake` passes.

## 6. IPC module

- [x] 6.1 In `ipc/mod.rs` implement `socket_path(workspace_root: &Path) -> PathBuf`
      computing `$TMPDIR/zed-claude-bridge-<xxh3-hex>.sock`. Unit test: stable
      output for a fixed input path; uses `/tmp` when `TMPDIR` is unset (honored via
      a `TmpDir` indirection that the test can override).
- [x] 6.2 Implement `IpcServer::start(socket_path, state, notifier)` that unlinks
      any stale file at `socket_path`, binds a `UnixListener`, and per-connection
      reads `\n`-delimited JSON, parsing each line into `IpcFrame`. Apply
      `Selection`, `WorkspaceFolders`, `OpenEditors` directly to `EditorState`; for
      `AtMention` push an `at_mentioned` notification through the notifier; for
      `Ping` respond with `Ack`. Reject lines >1 MiB. Verify (integration test):
      sending each frame type produces the documented effect on a fake EditorState
      and a fake notifier.
- [x] 6.3 Wire `selection_changed` debouncing here: each `Selection` frame resets a
      300 ms `tokio::time::sleep`; on expiry, if the new selection differs from
      `last_sent`, push a `selection_changed` notification. Skip when the URI scheme
      is `comment` or `output`. Verify (integration test using `tokio::time::pause`
      and `advance`): three rapid frames coalesce to one notification carrying the
      third frame's content; identical replays produce zero additional notifications.
- [x] 6.4 Verify robustness: integration test that closes an IPC connection mid-
      frame (no trailing `\n`) and confirms the server keeps running and accepts a
      fresh connection. Verify: `cargo test -p zed-claude-bridge --test ipc` passes.

## 7. App layer (CLI + lifecycle)

- [x] 7.1 In `app/cli.rs` define a `clap` derive struct with `--workspace
      <PathBuf>`, `--foreground` (bool, default true), `--log-level <String>`,
      `--ipc-socket <PathBuf>` (optional override), `--lock-dir <PathBuf>`
      (defaults to `~/.claude/ide`). Verify: `cargo run -p zed-claude-bridge -- --help`
      prints the expected flags.
- [x] 7.2 In `app/lifecycle.rs` implement `run(cli: Cli) -> anyhow::Result<()>`
      that: (a) initializes tracing with the chosen level; (b) opens the lock dir
      and prunes stale; (c) generates the per-launch UUID v4 auth token (wrapped in
      a `Secret` newtype with redacting `Debug`); (d) binds the WS listener and
      writes the lock file; (e) starts the IPC server bound to the workspace's
      socket; (f) drives an accept loop and a signal handler awaiting
      `tokio::signal::unix` `SIGINT|SIGTERM|SIGHUP`. On any signal: stop accepting,
      delete the lock file, await in-flight tasks with a 2-second deadline, then
      return.
- [x] 7.3 Wire `crates/zed-claude-bridge/src/main.rs` to a `#[tokio::main]` async
      `main` that parses `Cli`, calls `app::lifecycle::run(cli).await`, and converts
      its `Result` to a process exit code. No business logic here. Verify: `cargo
      run -p zed-claude-bridge -- --workspace /tmp/test-ws --foreground` starts up
      and writes a lock file under `~/.claude/ide/`; SIGTERM removes it cleanly.

## 8. Zed extension implementation

- [x] 8.1 In `extension/zed-claude-code/src/lib.rs` implement the
      `zed_extension_api::Extension` trait (or call the registration macro the
      current API uses). Read the workspace root from the Zed-provided context;
      compute the IPC socket path using the same `xxh3` formula as the sidecar.
      Verify: `cargo build --workspace` succeeds; the WASM target builds.
- [x] 8.2 Register a slash command and a context-menu action **Send to Claude
      Code** (declared in `extension.toml` per the Zed extension schema). Stub the
      handler to log "send-selection invoked". Verify: loading the extension into a
      local Zed dev build shows the command in the palette (manual smoke; recorded
      as a checklist item in the test plan).
- [x] 8.3 Implement IPC client logic: open the Unix socket, write one `at_mention`
      frame (followed by `\n`) carrying `{filePath, lineStart, lineEnd}` derived
      from the primary selection (0-indexed); close. If `connect()` fails, spawn the
      sidecar binary (`zed-claude-bridge --workspace <root>`) using the API the Zed
      extension exposes for spawning processes, then retry up to 5 times with
      exponential backoff (50 ms, 100, 200, 400, 800 ms). On empty selection,
      surface a user-visible message and skip the IPC write.
- [x] 8.4 On extension activation, send a `workspace_folders` IPC frame with the
      current workspace roots (absolute paths). Send a `selection` IPC frame on
      every selection change so the sidecar can debounce and fire
      `selection_changed`. Verify (manual): the sidecar logs frames at DEBUG.

## 9. End-to-end smoke test

- [x] 9.1 Add a Rust integration test `tests/end_to_end.rs` that spawns the sidecar
      in-process (via `app::lifecycle::run` on a tokio task with a `TempDir`-based
      lock dir), connects a `tokio-tungstenite` client with the auth token read
      from the freshly written lock file, drives the full MCP handshake, calls
      `tools/list`, then opens an IPC connection and sends an `at_mention` frame —
      asserting the WS client receives a matching `at_mentioned` JSON-RPC
      notification with 1-indexed lines. Verify: `cargo test -p zed-claude-bridge
      --test end_to_end` passes.
- [x] 9.2 Add a shell-level smoke harness (`scripts/smoke.sh`, executable) that
      starts the binary with `--workspace /tmp/test-ws --foreground`, polls for the
      lock file, uses `jq` + `websocat` to issue `tools/list`, asserts the response
      contains `getCurrentSelection`, then sends SIGTERM and asserts the lock file
      is removed. Verify: `bash scripts/smoke.sh` exits with status 0 (manual run;
      the script is documented in README).

## 10. Documentation & verification

- [x] 10.1 Replace the placeholder `README.md` with build/install/use instructions:
      how to build the workspace, install the Zed extension in dev mode, run the
      sidecar, and use `/ide`. Reference `docs/protocol.md` and the OpenSpec change
      directory. Verify: `markdown-link-check README.md` (best-effort, optional)
      reports no broken internal links.
- [x] 10.2 Run the full verification suite from `.harness/project.md` and capture
      the output: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
      -- -D warnings`, `cargo check --workspace --all-targets`, `cargo test
      --workspace`. Verify: every command exits 0.
- [x] 10.3 Run `openspec validate zed-claude-bridge --strict` and ensure no
      validation errors. Verify: command exits 0.
