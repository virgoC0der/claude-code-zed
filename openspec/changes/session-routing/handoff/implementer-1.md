# Handoff: session-routing implementer (1 → 2)

**From:** harness-implementer (instance 1)
**Date:** 2026-05-11
**Reason:** Context fatigue — 11 tasks completed, ~7 substantial files rewritten, much
re-reading. A fresh implementer with focused context will be more effective on the
remaining end-to-end test work and final verification.

## Current state of the change

### Tasks completed and PASSED by verifier
- #1 — Audit Claude Code v2.1.76 for workspace-identifying signals.
- #2 — Verify tungstenite accept_hdr callback exposes custom request headers.
- #3 — Extend IpcFrame with workspace_root, client_id, and Ambiguous variant.
- #4 — Document optional WebSocket headers and new IPC frames in docs/protocol.md.
- #5 — Add ClientRegistry module under transport/.
- #19 — Implement picker module (macOS osascript, Linux MRA fallback).
- #20 — Verify osascript availability and behaviour from LaunchAgent-spawned helper.

### Tasks completed and awaiting verifier review (after team-lead reverted #6 once)
- #6 — Add Router module with route_at_mention and route_selection_changed.
  - **History:** Verifier FAILed attempt 1 because `route_selection_changed` did
    any-prefix match instead of the spec-required longest-prefix. Re-submitted as
    attempt 2/3 with the fix and a docs/protocol.md wording correction folded in.
- #7 — Rewire Transport: drop broadcast/displacement, adopt registry+router.
- #8 — IPC server: route at_mention via Router; emit Ambiguous reply; route
  selection_changed.
- #9 — CLI: add --workspace-root and --client-id; implement picker round-trip.
- #10 — Wire registry through app::lifecycle::run_daemon.
- #11 — Update .zed/tasks.json to pass --workspace-root $ZED_WORKTREE_ROOT.

### Tasks NOT yet started
- #12 — End-to-end test: routing across distinct workspaces.
- #13 — End-to-end test: stale client_id and legacy-helper disconnect tolerance.
- #14 — End-to-end test: singleton fallback and no-match drop.
- #15 — End-to-end test: ambiguous reply, follow-up frame routes via client_id.
- #16 — Test: slow-client backpressure drops without stalling other clients.
- #17 — README: document multi-session routing, --workspace-root, troubleshooting.
- #18 — Verify: fmt, clippy, check, test, manual smoke. **Final task.**

## Verification status (snapshot at handoff)

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo check --workspace --all-targets`, and `cargo test --workspace` are all
**green** at the moment of this handoff. Counts:
- 139 unit tests (lib).
- 29 integration tests across `tests/end_to_end.rs` (1), `tests/handshake.rs` (12),
  `tests/ipc.rs` (14), `tests/lockfile.rs` (2).

## Key files modified

### Source (Rust)
- `crates/zed-claude-bridge/Cargo.toml` — added `serde` feature to `uuid` dep.
- `crates/zed-claude-bridge/src/protocol.rs` — extended `IpcFrame::AtMention`; added
  `IpcFrame::Ambiguous { candidates }` and `AmbiguousCandidate` struct.
- `crates/zed-claude-bridge/src/transport/registry.rs` — NEW.
- `crates/zed-claude-bridge/src/transport/router.rs` — NEW.
- `crates/zed-claude-bridge/src/transport/mod.rs` — re-exports.
- `crates/zed-claude-bridge/src/transport/ws.rs` — full rewrite (registry/router based).
- `crates/zed-claude-bridge/src/ipc/server.rs` — full rewrite (registry/router based).
- `crates/zed-claude-bridge/src/app/cli.rs` — added `--workspace-root` and `--client-id`.
- `crates/zed-claude-bridge/src/app/lifecycle.rs` — wired registry; picker round-trip.
- `crates/zed-claude-bridge/src/app/picker.rs` — NEW (macOS osascript + non-macOS MRA).
- `crates/zed-claude-bridge/src/app/mod.rs` — re-exports `pick_candidate`.

### Tests (Rust)
- `crates/zed-claude-bridge/tests/handshake.rs` — full rewrite (12 tests, displacement
  test inverted to coexistence; workspace header capture tests added).
- `crates/zed-claude-bridge/tests/ipc.rs` — harness switched from `broadcast::Receiver`
  to a registry-registered fake client with `mpsc::Receiver` (14 tests unchanged in
  behaviour).
- `crates/zed-claude-bridge/tests/end_to_end.rs` — single 3-line swap (notifier →
  registry).

### Docs / config
- `docs/protocol.md` — added §9 routing semantics; updated §2 (workspace header) and
  §6 (IPC frames); REMOVED single-client policy callout.
- `.zed/tasks.json` — added `--workspace-root "$WSR"` and `WSR: "$ZED_WORKTREE_ROOT"`.
- `openspec/changes/session-routing/tasks.md` — §0.1, §0.2, §0.3 boxes checked with
  inline finding blocks.

## Important decisions / deviations recorded

1. **No `session_tag` anywhere.** The team-lead's mandate and the design+spec are
   explicit: `session_tag` / `x-claude-code-session-tag` is REMOVED. Several task
   descriptions (TaskGet output for #5, #6, #9) mention it — those descriptions are
   stale planner drafts and must be ignored in favour of the design + spec + team-lead
   mandate. I documented this rationale in every relevant SendMessage to the verifier.

2. **Tasks #6 enum shape.** The TaskGet description for #6 listed variants
   `Tagged / WorkspaceMostRecent / NoMatch`. The correct enum per the design and
   `tasks.md §3.1` is `DirectClient / StaleClientId / WorkspaceUnique / Ambiguous /
   Singleton / NoMatch`. I implemented the correct one. Verifier flagged a real bug
   on selection_changed (longest-prefix) but did NOT flag the enum shape — confirming
   the design+spec are authoritative.

3. **`route_selection_changed` is longest-prefix.** Spec lines 213–216 of
   `notifications/spec.md` say so explicitly. Fixed on verifier feedback.

4. **Lifecycle does NOT eagerly populate `--workspace` as every client's
   `workspace_root`.** Each client's `workspace_root` is captured per-connection from
   the `x-claude-code-workspace` header or `clientInfo.cwd`. The CLI's `--workspace`
   names the IPC socket scope (per `DaemonArgs::workspace` docs). I called this out
   in the task-#10 message; if the verifier reads the spec as requiring a daemon-wide
   default, that needs a fix in `Transport::new`.

5. **IPC `Ambiguous` reply is written on the same connection.** The IPC server's
   per-connection read loop was already line-based; my changes added the
   `writer.write_all` call inside the `AtMention` arm to write the reply without
   closing. Tolerates legacy helpers that close immediately: log WARN, drop.

6. **`pick_fallback_mra` is `#[cfg(any(test, not(target_os = "macos")))]`.** The
   helper is only called in production on non-macOS, but we want tests to drive it on
   every platform without clippy `dead_code` errors. The cfg gate solves both.

## What the next implementer needs to do

### Task #12 — `tests/session_routing.rs::two_workspaces_route_independently`
Open two authorized WS clients with distinct `x-claude-code-workspace` headers
(`/tmp/ws-a` and `/tmp/ws-b`). Open an IPC connection, write an `at_mention` with
`workspace_root=/tmp/ws-a`. Assert:
- A's WS receives the `at_mentioned` JSON-RPC frame within 200 ms.
- B's WS receives nothing during a 500 ms window.

Pattern: use `start_full_stack()` from `tests/end_to_end.rs` (rename to a shared
helper if needed). The harness already exists; you'll need to connect two WS clients
and observe each independently.

### Task #13 — `tests/session_routing.rs::stale_client_id_and_legacy_helper_disconnect`
Two scenarios in one file (or two tests):
1. Open one authorized client. Send an `at_mention` with `client_id` set to a UUID
   that does not exist. Assert no notification is delivered AND a `tracing` test
   subscriber captures a WARN line with the stale id.
2. Open two authorized clients with the SAME workspace. Open an IPC connection, write
   an `at_mention` for that workspace, IMMEDIATELY close the IPC connection. Assert
   the sidecar does not panic, does not deliver any `at_mentioned`, and logs a WARN
   ("ambiguous match but peer disconnected" — exact wording in `ipc/server.rs`).

For log capture, use `tracing-subscriber`'s test-writer pattern. Search the repo for
prior tests that may already use it, or use the
`tracing_subscriber::fmt::TestWriter` + `with_default(subscriber, || ...)` pattern.

### Task #14 — `tests/session_routing.rs::singleton_fallback_and_no_match_drop`
1. Singleton: one authorized client with NO workspace header. Send `at_mention` with
   NO `workspace_root`. Assert the client receives the notification (singleton rule
   fires).
2. No match: two authorized clients with workspaces `/tmp/ws-a` and `/tmp/ws-b`.
   Send `at_mention` with `workspace_root=/tmp/ws-c`. Assert no client receives
   anything within 500 ms AND the sidecar logs a WARN listing `/tmp/ws-c` and the
   set `{/tmp/ws-a, /tmp/ws-b}`.

### Task #15 — `tests/session_routing.rs::ambiguous_reply_and_followup_routes`
Two authorized clients both with `workspace_root=/tmp/ws`. Send `at_mention` for
`/tmp/ws` from a test IPC client (no `client_id`). Assert:
- Neither WS client receives `at_mentioned` within 500 ms.
- The test IPC client reads back exactly one line that parses as
  `IpcFrame::Ambiguous` with two candidates whose `client_id` values match the two
  connected WS clients' registry ids.
- Labels are distinct and contain ordinals/durations per §3.3.

Then have the test IPC client write a follow-up `at_mention` with `client_id` set to
one of the two candidates. Assert exactly the chosen WS client receives the
`at_mentioned` within 200 ms; the other receives nothing.

### Task #16 — slow-client backpressure
Fill client A's per-client mpsc beyond capacity (64) without draining (don't read
from A's WS). Send another routed notification destined for A AND one for B. Assert:
- A's send times out (50 ms) and the sidecar logs a WARN tagged with A's `ClientId`.
- B's send succeeds.
- The sidecar process is otherwise healthy (no panic, ipc still accepts).

For this you may either:
- Spin up a real WS server + slow-reader pattern (set up TCP socket connection but
  never call `ws.next()`), OR
- Cover the slow-client logic at the unit-test level by directly invoking the
  `deliver_to` function with a deliberately-full `mpsc::Receiver`.

The task description allows either approach.

### Task #17 — README documentation
Add a "Multiple Claude sessions" section to README.md per the task brief. Reference
the new routing rules, `--workspace-root` flag, macOS picker, Linux fallback, and
troubleshooting (look for WARN logs in `~/Library/Logs/zed-claude-bridge.log`). Also
update the existing "Why a task, not a slash command" section to note that
`$ZED_WORKTREE_ROOT` is load-bearing.

### Task #18 — Final verification
Run all four checks: fmt, clippy, check, test. Then do a manual macOS smoke per task
brief §10.5: start the sidecar, open two `claude /ide` sessions in separate
terminals at the same cwd, trigger `cmd-ctrl-c` in Zed, observe the native picker,
verify the at-mention lands in only the chosen Claude session. Capture relevant log
lines and attach to the task close-out.

## Project conventions (repeat from project.md so the next implementer doesn't have to re-read)

- Rust 1.85+, edition 2024, single-crate workspace at `crates/zed-claude-bridge`.
- Commands (use VERBATIM):
  - format: `cargo fmt --all`
  - format_check: `cargo fmt --all --check`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - typecheck: `cargo check --workspace --all-targets`
  - test: `cargo test --workspace`
- Layer order: `protocol → lockfile → mcp → transport → ipc → app → main`.
- `thiserror` at module boundaries; `anyhow::Result` only at `main.rs` and CLI handlers.
- `tracing` for logs, never `println!`/`eprintln!` outside `main.rs` and tests.
- WebSocket lib: `tokio-tungstenite` with `rustls` disabled.
- `unsafe` forbidden.
- File perms: lock files 0600, lock dir 0700.

## Watch out for

1. **`tracing-subscriber` global state.** Tests that install a subscriber must use
   `tracing::subscriber::with_default` (not `set_global_default`) so they don't
   conflict with parallel tests. Look for the existing pattern in the test suite
   before you add it — there may already be one. If not, the docs at
   <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/> show the test
   pattern.

2. **`Path::starts_with` vs `str::starts_with`.** Always use the former for path
   prefix matching (component-aware: `/p` is NOT a prefix of `/page/x.rs`). Router
   already does this; tests for #14 and #15 will need to follow suit.

3. **`Instant::checked_sub`.** Used in router tests to construct snapshots with
   past timestamps. The unit test pattern in `transport/router.rs` and
   `transport/registry.rs` test modules is the reference.

4. **The IPC server's `writer: Arc<Mutex<OwnedWriteHalf>>` pattern.** Already in
   place; task #15's follow-up frame just needs to write to the same socket
   connection — the test IPC client should reuse the connection.

5. **macOS `osascript` will actually try to pop a dialog.** Don't invoke the real
   `pick_candidate` in end-to-end tests — instead, have the test IPC client write
   the follow-up frame directly (simulating what the helper would do). Task #15 is
   explicit about this: "no real osascript involved".

## Files NOT touched (yet, by anyone)

- README.md — task #17 wants additions.
- `crates/zed-claude-bridge/tests/session_routing.rs` — does not exist yet; tasks
  #12–#15 create it.

Good luck. The hard rewire is done; the remaining work is "exercise the contracts
via end-to-end tests".
