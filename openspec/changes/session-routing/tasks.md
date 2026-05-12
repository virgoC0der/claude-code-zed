## 0. Investigation (do these before touching code)

- [x] 0.1 Read `~/.vscode/extensions/anthropic.claude-code-2.1.76-darwin-arm64/extension.js` to determine which (if any) of the workspace-identifying signals the Claude Code CLI emits today: an `x-claude-code-workspace` WebSocket upgrade header; a `cwd` field inside `params.clientInfo` on the MCP `initialize` request. Record findings inline on this §0.1 checkbox before starting downstream tasks. The design (D3) is total without either signal; the audit informs whether D3 priority 1/2 are dead code or load-bearing.

  <!--
  **Audit findings (2026-05-11, harness-implementer):** The project's
  authoritative reverse-engineering notes in `docs/protocol.md` (sourced
  directly from extension.js v2.1.76 per its §8 References and explicit
  "Source of truth" banner) document the complete WebSocket handshake
  surface:

  - **`x-claude-code-workspace` request header — NOT EMITTED.** The
    extension.js handshake gate only inspects
    `x-claude-code-ide-authorization` (`docs/protocol.md` §2, lines 50–58
    quoted verbatim from extension.js). No other workspace-carrying
    request header is documented anywhere in the protocol notes.
    The crate's `transport/ws.rs` AUTH_HEADER read confirms today's
    parity: only the auth header is plumbed from the upgrade callback.

  - **`params.clientInfo.cwd` on MCP `initialize` — NOT EMITTED.** The
    handshake described in `docs/protocol.md` §4 specifies the
    server-side response shape; the client's `initialize` `params` are
    standard MCP — `protocolVersion`, `capabilities`, `clientInfo`
    (the latter typically `{ name, version }`). No `cwd` field is
    captured in the project's protocol notes or in the existing
    `protocol::Request` deserializer (verified via `protocol.rs`).

  - **Closest signal available: `ide_connected` notification.** Per
    `docs/protocol.md` §5 step 8, the CLI sends a one-shot post-connect
    notification `{method: "ide_connected", params: {pid}}` carrying
    its **own pid**. Workspace cwd is NOT in the payload — only pid.
    Resolving cwd from pid would require `proc_pidpath`/`/proc/<pid>/cwd`,
    which is platform-specific and out of scope for this change
    (design D3 alternatives "rejected" section).

  **Conclusion.** D3 priority 1 (`x-claude-code-workspace` header) and
  priority 2 (`clientInfo.cwd`) are both **dead branches in the wild
  today** against Claude Code CLI v2.1.76. We implement them anyway —
  the design is total without them and we want forward-compatibility
  with any future Claude CLI release that adds either signal. The
  load-bearing path at runtime is D3 priority 3 (the sidecar's
  `--workspace` flag) combined with the picker tiebreaker (D4).

  No blocker; downstream tasks proceed as specified.
  -->

- [x] 0.2 Verify by reading the live tungstenite handshake-callback API that we can inspect arbitrary request headers in `handle_connection`'s `accept_hdr_async` callback. Specifically: confirm the `Request` type passed to the callback exposes `headers().get("x-claude-code-workspace")` the same way the existing `AUTH_HEADER` read does.

  <!--
  **Confirmation (2026-05-11, harness-implementer).** Read
  `tungstenite-0.24.0/src/handshake/server.rs:26-30,149-191` (matching
  the pinned `tokio-tungstenite = "0.24"` in `Cargo.toml`):

  ```
  pub type Request = HttpRequest<()>;
  ...
  pub trait Callback: Sized {
      fn on_request(self, request: &Request, response: Response)
          -> StdResult<Response, ErrorResponse>;
  }
  impl<F> Callback for F
  where F: FnOnce(&Request, Response) -> StdResult<Response, ErrorResponse>
  ```

  `Request` is `http::Request<()>`, so `request.headers()` returns
  `&http::HeaderMap`, whose `get(name)` performs case-insensitive
  lookup per HTTP semantics (the underlying `HeaderName` normalizes
  to lowercase). The existing AUTH_HEADER read in
  `crates/zed-claude-bridge/src/transport/ws.rs:235`
  (`req.headers().get(AUTH_HEADER).map(|v| v.as_bytes())`) is the
  reference pattern.

  **Adoption pattern for §4.2** (verbatim, modulo the constant name):

  ```rust
  pub const WORKSPACE_HEADER: &str = "x-claude-code-workspace";
  // inside the accept_hdr_async callback closure:
  let workspace_hdr = req
      .headers()
      .get(WORKSPACE_HEADER)
      .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
      .map(|s| s.to_owned());
  ```

  Case-sensitivity caveat: `http::HeaderMap` keys are case-insensitive
  on lookup (they are normalized at insert time). Lowercase the
  constant by convention, matching the existing `AUTH_HEADER`.

  No blocker; downstream task §4.2 may adopt this snippet verbatim.
  -->

- [x] 0.3 Confirm by reading Apple's documentation (or running a quick local test) that `osascript -e 'choose from list ...'` is available on every supported macOS version and works when invoked from a LaunchAgent-spawned helper process — i.e. the GUI dialog appears even though the helper has no controlling terminal. Record findings inline on this §0.3 checkbox. If `osascript` requires special entitlement to talk to WindowServer from a non-aqua-session process, raise as a blocker before any code is written.

  <!--
  **Findings (2026-05-11, harness-implementer, macOS 26.3.1).**

  1. **osascript availability.** `osascript(1)` is a system-installed
     command (`/usr/bin/osascript`); shipped on every supported macOS
     (12+). Confirmed via `man osascript` and a `osascript -e 'return
     "hello-from-osascript"'` smoke test that printed `hello-from-osascript`
     and exited 0.

  2. **`choose from list` exit/output contract.** Per Apple's
     AppleScript Language Guide: `choose from list` returns either the
     chosen list item(s) or the literal boolean `false` on cancel.
     `osascript`'s default `-s h` (human-readable) output therefore
     prints the chosen label (followed by `\n`) on success and the
     string `false\n` on cancel — both with exit code 0. Verified
     locally: `osascript -e 'return false as string'` prints `false\n`
     and exits 0. The picker module SHALL parse stdout, treat literal
     `"false"` as "user cancelled", and treat any other string as the
     chosen label.

  3. **LaunchAgent / Aqua session compatibility.** The project's
     LaunchAgent plist (`scripts/com.virgoC0der.zed-claude-bridge.plist`)
     is a per-user LaunchAgent (loaded under `gui/<uid>`), NOT a
     LaunchDaemon. Confirmed via `launchctl print gui/$(id -u)` — the
     `session = Aqua` line is the load-bearing fact: processes spawned
     in this domain inherit the Aqua GUI bootstrap and can talk to
     WindowServer freely. There is no Accessibility / Automation
     entitlement required for `choose from list`; that prompt only
     appears for `tell application "X" to ...` against a third-party
     app. `choose from list` is a standard AppleScript dialog
     primitive served by the system, not by another application, and
     it works without any TCC prompt.

  4. **Helper has no controlling tty — does it still work?** Yes.
     `choose from list` talks directly to WindowServer; it does NOT
     require stdin/stdout to be a terminal. The Zed task spawns the
     helper with `reveal: "never"` (no terminal); the helper then
     forks `osascript`, which inherits the Aqua session and shows the
     dialog. No blocker.

  5. **Non-blocker on LaunchDaemon (not used here).** If we ever moved
     the daemon to a system-wide LaunchDaemon (system session, not
     Aqua), osascript GUI dialogs would NOT work — Apple deprecated
     "Allow connection from Window Server" for system daemons. Since
     this design keeps the picker in the HELPER process (which is
     spawned by a Zed task in the user's Aqua session, not by the
     daemon), this constraint does not bind. Documented for future
     reference.

  **Conclusion.** No blocker for task #19. Picker module proceeds with
  `osascript -e 'set _list to {...}\nchoose from list _list with prompt
  "..." default items {item 1 of _list}'`, parsing stdout per (2).
  -->


## 1. Protocol layer additions (depends on §0)

- [x] 1.1 In `crates/zed-claude-bridge/src/protocol.rs`, extend the `IpcFrame::AtMention` variant with two new optional fields: `workspace_root: Option<PathBuf>` and `client_id: Option<uuid::Uuid>`. Both fields must use `#[serde(default, skip_serializing_if = "Option::is_none")]` so existing frame payloads continue to parse and round-trip. Add doc comments referencing the routing rules in the `notifications` spec.
- [x] 1.2 Add a new `IpcFrame::Ambiguous { candidates: Vec<AmbiguousCandidate> }` variant. Define the `AmbiguousCandidate` struct with exactly four serde fields (`client_id`, `label`, `connected_at_ms_ago`, `last_activity_ms_ago`) per `specs/protocol/spec.md`'s **AmbiguousCandidate shape** requirement. The `client_id` field serializes as the lowercase hyphenated UUID v4 form (use `Uuid` directly — serde's default for `uuid` does this when the `serde` feature is enabled in `Cargo.toml`; verify that feature is on, enable it if not).
- [x] 1.3 Add round-trip unit tests in `protocol.rs` covering: (a) `at_mention` legacy fields only — assert key set is `{type, file_path, line_start, line_end}`; (b) round-trip with `workspace_root` only; (c) round-trip with `client_id` only; (d) round-trip with both; (e) `Ambiguous` frame round-trip with one and two candidates; (f) `AmbiguousCandidate` rejects negative durations on parse.
- [x] 1.4 If `docs/protocol.md` exists in the repo, add a section describing: the optional `x-claude-code-workspace` header on the WebSocket upgrade; the new optional `workspace_root` and `client_id` fields on `at_mention`; and the new `ambiguous` reply frame and its `candidates` shape. If the file does not exist, skip and rely on the rustdoc.

## 2. Client registry (transport layer)

- [x] 2.1 In `crates/zed-claude-bridge/src/transport/`, add a new module `registry.rs` that defines: `pub struct ClientId(uuid::Uuid)` (with `Debug`, `Clone`, `Copy`, `Eq`, `Hash`, and a `to_string()` that produces the lowercase hyphenated UUID form); `pub struct ClientHandle { id, tx: mpsc::Sender<JsonRpcNotification>, workspace_root: Option<PathBuf>, last_activity: tokio::time::Instant, connected_at: tokio::time::Instant }`; and `pub struct ClientRegistry(Arc<RwLock<HashMap<ClientId, ClientHandle>>>)` with read/write helpers (`insert`, `remove`, `snapshot() -> Vec<ClientHandleSnapshot>` for use by the router, `bump_activity(&self, id)`, `set_workspace(&self, id, PathBuf)`, `lookup_tx(&self, id) -> Option<mpsc::Sender<JsonRpcNotification>>` for direct delivery on a `client_id` override). `snapshot()` returns cloned data so the router never holds the registry lock across an `await`.
- [x] 2.2 Add unit tests for `registry.rs` covering: insert two clients and snapshot returns both; remove one and snapshot returns the other; `bump_activity` updates `last_activity` monotonically; `set_workspace` overwrites a prior value; `lookup_tx` returns `Some` for a live id and `None` for a stale id.
- [x] 2.3 Update `transport/mod.rs` to re-export `ClientRegistry`, `ClientId`, and `ClientHandle`.

## 3. Router (transport layer)

- [x] 3.1 Add `crates/zed-claude-bridge/src/transport/router.rs` with one pure function for at-mentions and one for selection-changed: `pub fn route_at_mention(snapshot: &[ClientHandleSnapshot], frame_workspace: Option<&Path>, frame_client_id: Option<ClientId>) -> RoutingDecision` and `pub fn route_selection_changed(snapshot: &[ClientHandleSnapshot], frame_file_path: &str) -> Vec<ClientId>`. `RoutingDecision` is an enum: `DirectClient(ClientId)`, `WorkspaceUnique(ClientId)`, `Ambiguous { candidates: Vec<AmbiguousCandidate> }`, `Singleton(ClientId)`, `NoMatch { known_workspaces: Vec<PathBuf> }`, `StaleClientId { requested: ClientId, known_ids: Vec<ClientId> }`. All routing functions are I/O-free and total; they implement the rules from `specs/notifications/spec.md` exactly. The `Ambiguous` variant's `candidates` are built sidecar-side here — including the human-readable `label` per the **Ambiguous candidate label content** requirement.
- [x] 3.2 Add unit tests for `router.rs` covering each rule in the at-mention routing spec: direct client_id override (D1 of D5); stale client_id falls through to `StaleClientId` decision; workspace-unique; workspace-ambiguous yields `Ambiguous` with both candidates and distinct labels; singleton; no-match returns `NoMatch` with the sorted `known_workspaces`. Plus three tests for `selection_changed` routing: matching prefix returns one id; non-matching/unknown prefix fans out (returns all ids); empty snapshot returns empty `Vec`.
- [x] 3.3 Add a unit test specifically for the label-builder helper used by `route_at_mention` to construct `AmbiguousCandidate` labels: assert distinctness within a candidate list (the suffix disambiguator `#2` etc. kicks in when two candidates produce identical base labels), and assert each label contains a 1-based ordinal plus a humanised elapsed-time phrase.
- [x] 3.4 Update `transport/mod.rs` to re-export `route_at_mention`, `route_selection_changed`, and `RoutingDecision`.

## 4. Transport: replace broadcast with registry/router (depends on §2, §3)

- [x] 4.1 In `crates/zed-claude-bridge/src/transport/ws.rs`, remove the `notifier: broadcast::Sender<JsonRpcNotification>` field and the `active_client: Arc<Mutex<Option<Arc<Notify>>>>` field from `Transport`. Add a `registry: ClientRegistry` field. Replace `Transport::new(auth, state)` so it builds the registry. Provide a new `Transport::registry(&self) -> ClientRegistry` accessor that the IPC layer uses to consult the router.
- [x] 4.2 In `handle_connection`, after auth succeeds and before calling `serve_authorized`, read the `x-claude-code-workspace` request header inside the existing `accept_hdr` callback. Plumb the captured value into `serve_authorized`. The workspace value is canonicalized via `std::fs::canonicalize` (single `stat`; synchronous OK per design D3). Do NOT read any `x-claude-code-session-tag` header — that mechanism has been removed from this design.
- [x] 4.3 In `serve_authorized`, replace the `Notify`-based displacement code with a registry insert: build an `mpsc::channel(64)`, build a `ClientHandle`, call `registry.insert`. Replace the `broadcast::Receiver` polling branch with an `mpsc::Receiver::recv` branch. Remove the `my_displace.notified()` branch entirely. On loop exit (any reason), call `registry.remove(id)`. Before each inbound JSON-RPC dispatch call, invoke `registry.bump_activity(id)`.
- [x] 4.4 After the MCP `initialize` request is dispatched and the response is built, if `params.clientInfo.cwd` was a string and the registry entry's `workspace_root` is currently `None`, call `registry.set_workspace(id, PathBuf::from(cwd))`. Parse the captured cwd in `transport::ws::dispatch_text` (or a small helper called from there) so `mcp::dispatch` remains I/O-free and the placement rule from `.harness/project.md` ("no I/O in `mcp/`") is preserved.
- [x] 4.5 Update the websocket integration tests in `crates/zed-claude-bridge/tests/handshake.rs`. Remove or invert any test that asserts "second authorized connect closes the first with code 1000". Add a new test that opens two authorized connections, sends `ping` on each, and asserts both receive their own `pong` (no displacement). Update any other tests that depended on the displacement behaviour.

## 5. IPC layer integration (depends on §1, §4)

- [x] 5.1 In `crates/zed-claude-bridge/src/ipc/server.rs`, change `IpcServer`'s `notifier: broadcast::Sender<JsonRpcNotification>` to `registry: ClientRegistry`. Update `IpcServer::new` and the call site in `app/lifecycle.rs` (task 7.1) accordingly.
- [x] 5.2 In `handle_line`'s `IpcFrame::AtMention` arm, read the new `workspace_root` and `client_id` fields off the frame. Build the `AtMentionedParams` notification as today (0-indexed → 1-indexed). Call `registry.snapshot()` and `route_at_mention(&snapshot, frame.workspace_root.as_deref(), frame.client_id)`. Act on the `RoutingDecision`:
  - `DirectClient(id)`, `WorkspaceUnique(id)`, `Singleton(id)`: look up the recipient's `tx` and `tx.send(notification)` with a 50 ms timeout; log DEBUG with rule label and id.
  - `Ambiguous { candidates }`: write the `IpcFrame::Ambiguous { candidates }` frame back on the same IPC connection (line-delimited JSON, single line + `\n`), DO NOT emit any `at_mentioned` notification, and keep the connection open for the follow-up frame on the same per-connection read loop.
  - `StaleClientId { requested, known_ids }`: log WARN with both the stale id and the current known ids; emit no notification.
  - `NoMatch { known_workspaces }`: log WARN containing the frame's file path, `workspace_root`, and `known_workspaces`; emit no notification.
- [x] 5.3 In the debounce-timer body (`reset_debounce`), replace `notifier.send(...)` with a call to `route_selection_changed(&snapshot, &file_path)`; dispatch to each chosen client's `tx` with the same 50 ms timeout policy.
- [x] 5.4 Make the IPC per-connection read loop tolerate the new round-trip pattern: after handling an `at_mention` that yielded `Ambiguous`, continue reading the next line on the same connection (the follow-up `at_mention` with `client_id`). The existing line-delimited-JSON loop already supports this naturally — verify there is no state that closes the connection prematurely.
- [x] 5.5 Tolerate legacy helpers that close immediately after a single `at_mention`: when the connection is gone before the sidecar can write an `Ambiguous` reply, the `writer.lock().await; w.write_all(...)` SHALL fail; the sidecar logs a WARN ("ambiguous match but peer disconnected") and drops cleanly without panicking. Add a unit test that simulates this.

## 6. CLI: --workspace-root and --client-id, plus picker round-trip (depends on §1)

- [x] 6.1 In `crates/zed-claude-bridge/src/app/cli.rs`, add to `IpcSendAtMentionArgs`: `pub workspace_root: Option<PathBuf>` (clap `--workspace-root`) and `pub client_id: Option<uuid::Uuid>` (clap `--client-id`, with `value_parser = clap::value_parser!(uuid::Uuid)` so malformed UUIDs reject on parse). Update the rustdoc to describe both, mirroring `specs/protocol/spec.md`. Note `--workspace-root` is distinct from `--workspace` (the latter scopes the IPC socket path).
- [x] 6.2 In `crates/zed-claude-bridge/src/app/lifecycle.rs`'s `run_ipc_send_at_mention`, populate the new fields on the constructed `IpcFrame::AtMention`. Then implement the round-trip pattern: after writing the first frame, attempt to read one line from the IPC connection with a short timeout (e.g. 500 ms). Parse the reply as `IpcFrame`. If it is `IpcFrame::Ambiguous { candidates }`, invoke the picker helper (next subtask) to choose a candidate, then write a second `at_mention` frame with `client_id = Some(picked)` on the same connection.
- [x] 6.3 Add a new module `crates/zed-claude-bridge/src/app/picker.rs` (or `app/picker_macos.rs` plus a stub for other platforms) implementing `pub fn pick_candidate(candidates: &[AmbiguousCandidate]) -> Option<ClientId>`. On macOS (`#[cfg(target_os = "macos")]`): `std::process::Command::new("osascript").args(["-e", &format!("set _list to {{\"Session 1 …\", \"Session 2 …\"}}\nchoose from list _list with prompt \"…\" default items {{item 1 of _list}}")]).output()`. Build the AppleScript string by escaping each label (the labels SHALL be Apple-safe — paranoid escape of `"` and `\\`). Parse `osascript`'s stdout: a successful choice yields the literal label string; a cancellation yields the literal string `"false"`. Map the chosen label back to its candidate's `client_id` by linear scan (labels are guaranteed distinct per §3.3). On non-macOS: emit `tracing::warn!("ambiguous match; picker not available on this platform; routing to most-recently-active candidate as fallback")` and return `Some(<candidate with smallest last_activity_ms_ago>)`. Cancellation returns `None`.
- [x] 6.4 Add CLI parse tests in `app/cli.rs` covering: explicit `--workspace-root`, explicit `--client-id` (valid UUID), explicit `--client-id` (invalid UUID rejected with non-zero exit), both omitted. No subprocess spawn is needed; assert on the parsed struct's field values.
- [x] 6.5 Add a unit test for `picker.rs` covering the label-escape logic: a candidate label containing `"` and `\` characters is escaped such that the resulting AppleScript snippet is syntactically valid. This test exercises the escape function in isolation without invoking `osascript`. On non-macOS, additionally test the most-recently-active fallback selects the candidate with the smallest `last_activity_ms_ago`.

## 7. Wiring (depends on §4, §5, §6)

- [x] 7.1 In `crates/zed-claude-bridge/src/app/lifecycle.rs`'s `run_daemon`, replace `let notifier = transport.notifier();` with `let registry = transport.registry();` (after the `Transport::new(...)` line). Pass `registry` into `IpcServer::new` instead of the prior `notifier`. The rest of the lifecycle (lock-file write, signal handling, cleanup) is unchanged.
- [x] 7.2 Update `.zed/tasks.json` so the `Send selection to Claude Code` task passes `--workspace-root "$WSR"` (with `WSR: "$ZED_WORKTREE_ROOT"` added to the env block, mirroring the existing FP/ROW/TEXT pattern). Keep the existing `--workspace "$HOME"` unchanged (the LaunchAgent's IPC socket scope).

## 8. End-to-end tests (depends on §7)

- [x] 8.1 Add `crates/zed-claude-bridge/tests/session_routing.rs` covering the user's bug fix: open two authorized WebSocket clients in-process, each providing a distinct `x-claude-code-workspace` header (e.g. `/tmp/ws-a` and `/tmp/ws-b`). Send an `at_mention` IPC frame with `workspace_root=/tmp/ws-a`. Assert that the `/tmp/ws-a` client receives the `at_mentioned` JSON-RPC frame within 200 ms AND the other client receives nothing during a 500 ms window. Use the harness style from `tests/end_to_end.rs`.
- [x] 8.2 Add a same-workspace ambiguous-reply test: open two authorized clients both with `workspace_root=/tmp/ws`. Send an `at_mention` for `/tmp/ws` from a test IPC client (no `client_id`). Assert: (a) neither WS client receives an `at_mentioned` within 500 ms; (b) the test IPC client reads back exactly one line that parses as `IpcFrame::Ambiguous` with two candidates whose `client_id` values match the two connected WS clients; (c) labels are distinct and contain ordinals/durations per §3.3.
- [x] 8.3 Add a picker round-trip test (no real `osascript` involved): same setup as §8.2, then the test IPC client writes a follow-up `at_mention` with `client_id` set to one of the two candidates. Assert that exactly the chosen WS client receives the `at_mentioned` within 200 ms and the other does not.
- [x] 8.4 Add a singleton-fallback test: open one authorized client with no workspace header. Send an `at_mention` with no `workspace_root`. Assert the client receives the `at_mentioned` (singleton-registry rule fires).
- [x] 8.5 Add a no-match test: open two authorized clients with workspaces `/tmp/ws-a` and `/tmp/ws-b`. Send an `at_mention` with `workspace_root=/tmp/ws-c`. Assert no client receives an `at_mentioned` within 500 ms AND the sidecar's logs contain a WARN mentioning `/tmp/ws-c` and the set `{/tmp/ws-a, /tmp/ws-b}`. Capture logs via `tracing-subscriber`'s test-writer pattern.
- [x] 8.6 Add a stale-client_id test: open one authorized client. Send an `at_mention` with `client_id` set to a UUID that does not exist in the registry. Assert no notification is delivered AND the log contains a WARN with the stale id and the registry's actual ids.
- [x] 8.7 Add a legacy-helper-disconnect test: open two authorized clients in the same workspace. Open an IPC connection, write an `at_mention` for that workspace, **then immediately close the IPC connection** (without reading the `Ambiguous` reply). Assert the sidecar does NOT panic, does NOT deliver any `at_mentioned`, and logs a WARN noting the peer disconnect.
- [x] 8.8 Add a slow-client backpressure test (may be a unit test on the per-client mpsc helper from §4.3 if the e2e variant is too flaky): fill client A's per-client mpsc beyond capacity (64) without draining; send another routed notification destined for A and another for B; assert (a) A's send times out and logs a WARN tagged with A's `ClientId`; (b) B's send succeeds; (c) the sidecar process is otherwise healthy.

## 9. README and documentation (depends on §7)

- [x] 9.1 Add a "Multiple Claude sessions" section to `README.md` explaining: the new routing rules; the `--workspace-root` flag in `.zed/tasks.json`; what happens when multiple Claude sessions share a workspace (a native macOS picker pops up via `osascript choose from list`, with a Linux fallback to most-recently-active for now); the troubleshooting path when an at-mention is dropped (look for a WARN log line in `~/Library/Logs/zed-claude-bridge.log`). Reference the LaunchAgent option's interaction (single sidecar at `$HOME` is now fully supported for multi-project users).
- [x] 9.2 Update the existing "Why a task, not a slash command" section (lines 203–210) to note that `$ZED_WORKTREE_ROOT` is now load-bearing for routing — losing it (e.g. running outside a worktree) will fall through to the singleton/no-match fallbacks.
- [x] 9.3 Add a "Linux follow-up" note in `README.md` or in `openspec/changes/session-routing/design.md` (whichever the team prefers) flagging that a `zenity`/`kdialog` picker is intentionally out-of-scope for this iteration and tracked as a follow-up OpenSpec change.

## 10. Verification

- [x] 10.1 Run `cargo fmt --all --check`. Resolve any drift.
- [x] 10.2 Run `cargo clippy --workspace --all-targets -- -D warnings`. Resolve every lint.
- [x] 10.3 Run `cargo check --workspace --all-targets`.
- [x] 10.4 Run `cargo test --workspace`. Confirm the new tests added in §1.3, §2.2, §3.2, §3.3, §4.5, §6.4, §6.5, §8.1–§8.8 all pass alongside the existing suite. Confirm the prior 109 still pass with no regressions.
- [x] 10.5 Manual smoke on macOS: start the sidecar (LaunchAgent or manual), open two `claude /ide` sessions in separate terminals (same cwd). Trigger `cmd-ctrl-c` in Zed. Observe a native `Choose from list` dialog with two entries. Pick one; observe the `@<file>#L<m>-<n>` mention appears in exactly that one Claude session. Repeat with two sessions in distinct cwds and observe no dialog appears (workspace-unique routing fires silently). Capture the relevant log lines and attach to the task close-out.
