# Tasks — peer-cwd-discovery

> Numbered checklist for the implementer. Order is dependency-driven;
> read each task's "Acceptance criteria" before claiming completion.

## 1. Dependency addition

- [x] 1.1 Add a macOS-gated dependency to `crates/zed-claude-bridge/Cargo.toml`:

  ```toml
  [target.'cfg(target_os = "macos")'.dependencies]
  libproc = "0.14"
  ```

  Use the `libproc` crate's latest 0.14.x patch (currently `0.14.11`).
  Do NOT pull in `darwin-libproc` or `netstat2` — pre-research rejected
  those alternatives.

  **Acceptance criteria:** `cargo check --workspace --all-targets`
  succeeds on macOS. On a non-macOS check the `libproc` dependency is
  not pulled in (verify by reading the lockfile diff: no `libproc`
  entry under a non-macOS host's `cargo update` run).

## 2. `CwdResolver` trait + Noop + Mock impls (depends on §1)

- [x] 2.1 Create a new module
  `crates/zed-claude-bridge/src/transport/cwd_resolver.rs` containing:

  - the `CwdResolver` trait per design D1, marked `Send + Sync + Debug`,
    using `#[async_trait::async_trait]` if it simplifies the dyn-trait
    object (alternatively, hand-write the `Pin<Box<dyn Future>>` return
    type — implementer's choice, but document the choice in a comment);
  - `NoopCwdResolver` (always returns `None`) under
    `#[cfg(not(target_os = "macos"))]` AND as a `#[cfg(any(test, target_os = "macos"))]` variant available for tests on macOS;
  - `MockCwdResolver { map: std::collections::HashMap<u16, std::path::PathBuf> }`
    with a public `new()` and a public `insert(port, path)` setter;
  - a `default_cwd_resolver() -> std::sync::Arc<dyn CwdResolver>`
    helper that returns the platform default
    (`LibprocCwdResolver` on macOS, `NoopCwdResolver` elsewhere). The
    `LibprocCwdResolver` reference here may be a forward-declared
    `pub(crate) struct` stub if §3 hasn't landed yet — but ideally the
    two land together.

  **Acceptance criteria:**
  - `cargo clippy --workspace --all-targets -- -D warnings` succeeds.
  - Inline unit tests:
    - `NoopCwdResolver::resolve(any_addr)` returns `None`.
    - `MockCwdResolver` with `{42321 -> /tmp/ws-a}` returns
      `Some(/tmp/ws-a)` for `127.0.0.1:42321` and `None` for
      `127.0.0.1:99999`.

- [x] 2.2 Re-export `CwdResolver`, `NoopCwdResolver`, `MockCwdResolver`,
  and `default_cwd_resolver` from `crates/zed-claude-bridge/src/transport/mod.rs`.

  **Acceptance criteria:**
  - `use zed_claude_bridge::transport::CwdResolver;` compiles from
    an integration test under `tests/`.

## 3. `LibprocCwdResolver` (macOS) (depends on §1, §2)

- [x] 3.1 Implement `LibprocCwdResolver` in
  `crates/zed-claude-bridge/src/transport/cwd_resolver.rs` under
  `#[cfg(target_os = "macos")]`.

  Per design D2:
  - `resolve()` wraps a synchronous `resolve_blocking(peer_port: u16)`
    call in `tokio::task::spawn_blocking(...)`.
  - `resolve_blocking` enumerates PIDs via `libproc::proc_pid::listpids(ProcType::ProcAllPIDS)`,
    walks each PID's fd table via `libproc::proc_pid::listpidinfo::<ListFDs>`,
    matches the fd whose `ProcFDType::Socket` info contains the peer
    port as the LOCAL port (the kernel's perspective on the Claude
    process's socket).
  - On match, calls `libproc::proc_pid::pidinfo::<VNodePathInfo>(pid, 0)`
    and returns the `pvi_cdir.vip_path` C-string as a `PathBuf`.
  - Returns `None` on every failure path; never panics.
  - All conversions from `[i8; N]` C-string buffers to `&[u8]` or
    `&CStr` MUST use safe Rust APIs (e.g. `bytemuck::cast_slice` on
    the buffer if the implementer adds `bytemuck`, or a manual
    element-wise byte cast via `iter().map(|&b| b as u8).collect::<Vec<u8>>()`).
    **Our code does NOT contain `unsafe` blocks.** If the implementer
    finds that a specific libproc 0.14 entry point requires `unsafe`
    from the caller, STOP and raise it as a blocker.

  **Acceptance criteria:**
  - Compiles cleanly on macOS with
    `cargo clippy --workspace --all-targets -- -D warnings`.
  - No `unsafe` token appears in `cwd_resolver.rs` (verify with
    `grep -n unsafe crates/zed-claude-bridge/src/transport/cwd_resolver.rs`
    returning no matches).
  - A manual smoke test (recorded in `manual-smoke-procedure.md` if
    introduced, or inline in this task's close-out): start the sidecar
    foreground in a tempdir; from a different tempdir run
    `websocat -H "x-claude-code-ide-authorization: <token>" ws://127.0.0.1:<port>`
    and observe the sidecar's log:
    `workspace_source="peer-cwd-libproc" workspace="<websocat's tempdir>"`.

## 4. `Transport` builder + accept-loop integration (depends on §2, §3)

- [x] 4.1 In `crates/zed-claude-bridge/src/transport/ws.rs`:

  - Introduce a `TransportBuilder` struct that owns `auth`, `state`,
    `daemon_workspace: Option<PathBuf>`, `cwd_resolver: Arc<dyn CwdResolver>`.
  - Keep `Transport::new(auth, state)` and
    `Transport::with_daemon_workspace(auth, state, daemon_workspace)`
    public; both delegate to a private `builder` API that defaults
    `cwd_resolver` to `default_cwd_resolver()`.
  - Add `Transport::builder(auth, state) -> TransportBuilder` and
    `TransportBuilder::with_daemon_workspace(self, p)`,
    `TransportBuilder::with_cwd_resolver(self, r)`, `build()`.
  - Add a `cwd_resolver: Arc<dyn CwdResolver>` field on `Transport`.

  **Acceptance criteria:**
  - All existing call sites compile without modification (the two
    legacy constructors keep their signatures).
  - A new doctest or unit test demonstrates:
    `Transport::builder(auth, state).with_cwd_resolver(Arc::new(MockCwdResolver::new())).build()` compiles.

- [x] 4.2 In `Transport::run`, capture the peer `SocketAddr` returned
  by `listener.accept()` and pass it into `handle_connection(stream,
  peer)`. (Today's code already binds `peer` in the match arm — just
  thread it down.)

  In `handle_connection`, after auth succeeds AND after the
  workspace-header callback has populated `workspace_header`, but
  BEFORE `serve_authorized` is called:

  - If the workspace header WAS set, use it (priority 1) and label
    the source `"header"`.
  - Otherwise, call `self.cwd_resolver.resolve(peer).await`. If it
    returns `Some(p)`, canonicalize via `canonicalize_or_keep_path(&p)`
    and set `initial_workspace = Some(canonical)`; label the source
    `"peer-cwd-libproc"`.
  - Otherwise, leave `initial_workspace = None`; label the source
    `"pending-initialize"`.

  Pass the resolved source tag into `serve_authorized` as a new
  parameter (e.g. `initial_source: &'static str`) so the INFO log
  emits the correct value.

  **Acceptance criteria:**
  - The existing INFO line `"authorized websocket client registered"`
    now carries `workspace_source` reflecting the priority that fired
    (verified by reading the log output of a new integration test —
    see §6.1).
  - The accept loop is non-blocking under load: a second TCP
    connection is accepted while the first resolver call is still
    running. Add an `assert!` or `tokio::time::timeout`-bounded test
    that proves this if practical; otherwise document it in the
    test's comment and rely on `spawn_blocking` semantics.

- [x] 4.3 In `dispatch_text`, update the priority-3 / priority-4
  branches' DEBUG log lines to carry an explicit `workspace_source`
  field:

  - Priority 3 (clientInfo.cwd) → `workspace_source = "client-info-cwd"`.
  - Priority 4 (daemon-workspace) → `workspace_source = "daemon-workspace"`.

  The control flow does NOT change. The only difference from today is
  that `initial_workspace` may already be `Some(_)` because peer-cwd
  filled it in at accept time — that case is already handled by the
  existing `if current.is_none() { ... }` gate.

  **Acceptance criteria:**
  - `grep -n 'workspace_source' crates/zed-claude-bridge/src/transport/ws.rs`
    shows the new tags in both branches.
  - Existing session-routing integration tests still pass (no
    behavioural drift).

## 5. App-level wiring (depends on §4)

- [x] 5.1 In `crates/zed-claude-bridge/src/app/lifecycle.rs`'s
  `run_daemon`, swap the existing
  `Transport::new(...)` / `Transport::with_daemon_workspace(...)`
  call for a builder invocation that explicitly threads in
  `default_cwd_resolver()`. (The legacy constructors still work, but
  threading the call here makes the production wiring explicit and
  greppable.)

  **Acceptance criteria:**
  - `cargo build --workspace --release` succeeds.
  - The sidecar starts with the LaunchAgent plist unchanged.
  - Optional: a startup INFO log line records the resolver kind,
    e.g. `info!(resolver = std::any::type_name::<...>(), "cwd resolver configured")`,
    to make field debugging trivial.

## 6. Tests (depends on §2, §3, §4)

- [x] 6.1 Add `crates/zed-claude-bridge/tests/peer_cwd_discovery.rs`
  exercising the full handle_connection → resolver → registry chain.

  - Build a `MockCwdResolver`, get the test's about-to-be-used local
    port via `TcpStream::connect(addr).local_addr()?.port()` BEFORE
    starting the WebSocket handshake, insert
    `port -> /tmp/expected-ws` into the mock, then issue the
    handshake.
  - Open the WebSocket WITHOUT the `x-claude-code-workspace`
    header and WITHOUT a `clientInfo.cwd`.
  - After the handshake completes, send `initialize` with a default
    `clientInfo` to drive the connection through the registry insert
    + first `dispatch_text` pass.
  - Use `Transport::registry().snapshot().await` (the test owns the
    Transport handle) to read the registered client's
    `workspace_root`. Assert it equals `canonical(/tmp/expected-ws)`.

  Run a parallel two-client variant: two distinct local ports, two
  distinct mapped cwds, snapshot returns two registry entries each
  with the correct `workspace_root`.

  **Acceptance criteria:**
  - `cargo test --workspace -- peer_cwd_discovery` passes.
  - The single-client test asserts the registry entry's workspace
    matches the mock's mapping.
  - The two-client test asserts both entries' workspaces match.

- [x] 6.2 Add a regression test inside
  `crates/zed-claude-bridge/tests/session_routing.rs`:

  - `at_mention_routes_via_peer_cwd_when_no_header_and_no_client_info`.
  - Setup: in-process sidecar built with a `MockCwdResolver`
    containing two port → cwd mappings (one per client). Both WS
    clients connect WITHOUT a workspace header.
  - Send an `at_mention` IPC frame whose `workspace_root` matches
    one of the two mapped cwds.
  - Assert: that client's WebSocket receives the `at_mentioned`
    frame within 200 ms; the other client receives nothing within
    500 ms.

  This is the **end-to-end proof that the field bug is fixed**.

  **Acceptance criteria:**
  - The test passes on macOS and Linux (the `MockCwdResolver` is
    cross-platform — no real libproc invoked).
  - Adding this test does NOT break any existing
    `session_routing.rs` test.

## 7. Verification

- [x] 7.1 `cargo fmt --all --check` — resolve any drift.
- [x] 7.2 `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] 7.3 `cargo check --workspace --all-targets`.
- [x] 7.4 `cargo test --workspace`. Confirm new tests from §2.1,
  §6.1, §6.2 pass alongside the existing suite. Confirm no
  regressions.
- [ ] 7.5 Manual smoke on the user's machine (macOS, LaunchAgent
  deployment):
  - `cargo install --path crates/zed-claude-bridge`,
  - `launchctl kickstart -k gui/$(id -u)/com.virgoC0der.zed-claude-bridge`,
  - open two `claude /ide` sessions in two different project cwds,
  - press `cmd-ctrl-c` in Zed for a file in each project,
  - observe the at-mention lands in the correct Claude session every
    time, with no picker dialog,
  - tail `~/Library/Logs/zed-claude-bridge.log` and confirm
    `workspace_source="peer-cwd-libproc"` for both connections.

  Record results in the change's `handoff/smoke-results.md` once the
  smoke completes.
