# Commit plan — peer-cwd-discovery + session-routing

Per team-lead's directive (2026-05-11 dispatch): two commits, one per
OpenSpec change. Authored AFTER user's smoke PASS for task #16.

The team-lead provides the verbatim commit messages below; the
implementer stages the paths.

## Commit 1 — session-routing

**Message (verbatim from team-lead):**

```
feat(transport): per-client at-mention routing (session-routing)

Drop the broadcast notifier + single-client displacement policy
introduced in the v0.1 sidecar. Replace with an explicit
ClientRegistry of per-client mpsc channels and a Router that
picks exactly one at_mention recipient per IPC frame.

Routing rules (priority order): client_id override → workspace
uniqueness → ambiguous reply (picker via osascript on macOS;
MRA fallback on Linux) → singleton → drop with WARN.

Adds 76 tests. OpenSpec change: session-routing.
```

**Paths to stage (whole files):**

```
.zed/tasks.json
README.md
crates/zed-claude-bridge/src/app/cli.rs
crates/zed-claude-bridge/src/app/mod.rs
crates/zed-claude-bridge/src/app/picker.rs
crates/zed-claude-bridge/src/ipc/mod.rs
crates/zed-claude-bridge/src/ipc/server.rs
crates/zed-claude-bridge/src/protocol.rs
crates/zed-claude-bridge/src/transport/registry.rs
crates/zed-claude-bridge/src/transport/router.rs
crates/zed-claude-bridge/tests/ipc.rs
docs/protocol.md
openspec/changes/session-routing/
```

**Paths to stage (partial — `git add -p`):**

- `crates/zed-claude-bridge/src/transport/ws.rs` —
  EVERYTHING EXCEPT the peer-cwd hunks (the `cwd_resolver` import on
  line 30, the `cwd_resolver` field on `Transport`, the
  `TransportBuilder` struct + impl, the `Transport::builder` ctor,
  the `PEER_CWD_RESOLVER_TIMEOUT_MS` const, the resolver call inside
  `handle_connection` after auth, the `workspace_source: &'static str`
  parameter on `serve_authorized`, the `workspace_source` fields on
  the priority-3 and priority-4 DEBUG logs in `dispatch_text`, the
  4-priority-chain comment rewrites, the rustdoc updates that say
  "priority 4"). Everything related to ClientRegistry, ClientHandle,
  mpsc-replaces-broadcast, the per-connection pump's mpsc loop, the
  registry insert/remove on connect/disconnect — those stay in
  commit 1.

- `crates/zed-claude-bridge/src/transport/mod.rs` —
  the new `pub mod registry;` / `pub mod router;` lines AND the
  `pub use` re-exports for ClientRegistry, ClientHandle, etc. The
  `pub mod cwd_resolver;` line and the `pub use` re-exports for the
  resolver-related symbols go to commit 2.

- `crates/zed-claude-bridge/src/transport/mod.rs` — see above.

- `crates/zed-claude-bridge/tests/session_routing.rs` —
  EVERYTHING EXCEPT the `start_sidecar_with_mock_cwd_resolver` helper
  + `ws_upgrade_on_existing_stream` helper + the
  `at_mention_routes_via_peer_cwd_when_no_header_and_no_client_info`
  test + the `NoopCwdResolver` import + the
  `.with_cwd_resolver(Arc::new(NoopCwdResolver::new()))` injection
  in `start_sidecar` (those go to commit 2). The bulk of the file
  (the 9 pre-peer-cwd tests + the picker-related helpers) belongs
  to commit 1.

- `crates/zed-claude-bridge/tests/end_to_end.rs` —
  EVERYTHING EXCEPT the `NoopCwdResolver` import and the
  `.with_cwd_resolver(...)` line in `start_full_stack` (those go
  to commit 2). The bulk (the e2e harness + assertion) is
  session-routing.

- `crates/zed-claude-bridge/tests/handshake.rs` —
  All of session-routing's additions (the priority-3 / priority-4
  tests, the prior priority-1 tests). The `NoopCwdResolver`
  injection in `start_transport` and `start_transport_with_daemon_workspace`
  goes to commit 2 — though arguably it could go to commit 1 if we
  prefer to land it as part of the session-routing test harness.
  **Decision: ship the Noop injection in commit 2** because it only
  becomes necessary AFTER the peer-cwd accept-loop wiring lands. In
  commit 1's world, `Transport::new` returned a Transport with no
  resolver field; tests didn't need injection.

**Path filter NOT in commit 1 (stays for commit 2):**

- `.harness/project.md` (policy)
- `Cargo.toml` (workspace lint shift)
- `Cargo.lock` (libproc deps)
- `crates/zed-claude-bridge/Cargo.toml` (libproc / libc deps)
- `crates/zed-claude-bridge/src/app/lifecycle.rs` (run_daemon wiring; SHOULD really be in commit 1 for its session-routing bits and commit 2 for the resolver wiring — `git add -p` needed)
- `crates/zed-claude-bridge/src/transport/cwd_resolver.rs` (NEW)
- `crates/zed-claude-bridge/tests/cwd_resolver_reexport.rs` (NEW)
- `crates/zed-claude-bridge/tests/peer_cwd_discovery.rs` (NEW)
- `openspec/changes/peer-cwd-discovery/`

## Commit 2 — peer-cwd-discovery

**Message (verbatim from team-lead):**

```
feat(transport): peer-cwd discovery for cross-project routing (peer-cwd-discovery)

The session-routing change shipped a routing chain that relied on
Claude Code CLI v2.1.76 emitting workspace identifiers via
x-claude-code-workspace header or clientInfo.cwd. In practice
v2.1.76 emits neither, so the LaunchAgent deployment (sidecar
pinned to $HOME) routes every at_mention through the daemon
workspace fallback and drops them all on workspace mismatch.

This change inserts a new priority-2 step: on accept, resolve the
Claude client's cwd from its process via libproc (proc_listpids +
PROC_PIDFDSOCKETINFO + proc_pidinfo with PROC_PIDVNODEPATHINFO).
Pluggable CwdResolver trait keeps tests libproc-free.

Two narrow unsafe blocks added at FFI / POD-union boundaries in
the new transport::cwd_resolver module. Project policy amended
to scope this exception to that single file.

Adds 25 tests including a regression test (#14) that locks in
the field bug fix. OpenSpec change: peer-cwd-discovery.
```

**Paths to stage (whole files):**

```
.harness/project.md
Cargo.toml
Cargo.lock
crates/zed-claude-bridge/Cargo.toml
crates/zed-claude-bridge/src/transport/cwd_resolver.rs
crates/zed-claude-bridge/tests/cwd_resolver_reexport.rs
crates/zed-claude-bridge/tests/peer_cwd_discovery.rs
openspec/changes/peer-cwd-discovery/
```

**Paths to stage (partial — `git add -p`):**

- `crates/zed-claude-bridge/src/transport/ws.rs` — see commit 1 list inverted.
- `crates/zed-claude-bridge/src/transport/mod.rs` — only the
  `pub mod cwd_resolver;` + the `pub use cwd_resolver::{...}` blocks.
- `crates/zed-claude-bridge/src/app/lifecycle.rs` — only the
  `use crate::transport::{..., default_cwd_resolver};` line and the
  `Transport::with_daemon_workspace` → `Transport::builder(...).with_*().build()` refactor in `run_daemon`, plus the new INFO log line.
- `crates/zed-claude-bridge/tests/end_to_end.rs` — only the
  `NoopCwdResolver` import + injection.
- `crates/zed-claude-bridge/tests/handshake.rs` — Noop injection in
  both helpers (entire file's changes, since this is all the file
  was touched for in this change).
- `crates/zed-claude-bridge/tests/session_routing.rs` — Noop
  injection in `start_sidecar` + the new helper +
  `at_mention_routes_via_peer_cwd_when_no_header_and_no_client_info` test.

## Holding pattern

Per Q1/Q3: SMOKE FIRST. Do NOT run `git commit` until task #16 PASS.
If smoke PASS:
1. Stage commit 1's paths.
2. `git commit -F` with the message above.
3. Stage commit 2's paths.
4. `git commit -F` with the message above.
5. `git status` to confirm zero residue.
6. Send team-lead the two commit hashes for review.
7. Push only after team-lead PASS on the commits.
