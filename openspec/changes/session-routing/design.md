# Design: session-aware at-mention routing

## Context

Today the sidecar treats outbound JSON-RPC notifications as a flat
broadcast: a single `tokio::sync::broadcast::Sender<Notification>`
(`crates/zed-claude-bridge/src/transport/ws.rs:170,181`) fans out every
`at_mentioned` and `selection_changed` to every subscribed authorized
WebSocket client. A best-effort "single-client policy"
(`transport/ws.rs:286–301`) tries to keep the subscriber set at size 1
by displacing prior clients on each new authorized connect, but the
displacement is asynchronous — the new client subscribes to the
broadcast *before* the prior client's close-frame round-trip completes,
and the Claude CLI reconnects after a `1000` close, which makes the
race a steady state in any multi-session setup.

The recommended LaunchAgent deployment
(`README.md` "Option A: launch on macOS login (recommended)") pins one
sidecar to `$HOME`, so every Claude session under any project in `~/`
is served by the same WebSocket server. The single-client policy is
fundamentally incompatible with this setup — and even a per-project
sidecar would still see two `claude /ide` invocations from the same
cwd as two clients, both of which the user expects to coexist.

The Zed task system that drives `cmd-ctrl-c` exposes
`$ZED_FILE`, `$ZED_ROW`, `$ZED_SELECTED_TEXT`, and `$ZED_WORKTREE_ROOT`
to the spawned process. It does **not** expose terminal focus, the
focused-terminal PID, or any handle identifying which integrated Zed
terminal is foreground (confirmed via Zed's tasks docs and the
project's own README, `README.md` lines 204–210). The user has
explicitly chosen a **picker** (native chooser dialog) as the
tiebreak when the workspace alone cannot disambiguate — not a
most-recently-active heuristic, and not a hidden choice.

## Goals / Non-Goals

**Goals**

- Eliminate the at-mention fan-out: every `at_mentioned` notification
  produced by an IPC frame goes to **at most one** WebSocket client.
- Support multiple concurrent authorized clients per sidecar; never
  displace a prior client.
- When multiple registered clients share the matching workspace,
  prompt the user to pick which one receives the at-mention. The
  choice is presented through a native macOS list dialog
  (`osascript -e 'choose from list ...'`), invoked from the
  `ipc-send-at-mention` helper itself — not from the daemon.
- Make routing deterministic and explainable from logs alone.
- Preserve byte-for-byte wire compatibility with the existing JSON-RPC
  and MCP handshake. No spec break visible to the Claude Code CLI.
- Keep the design implementable today against signals the codebase
  actually has access to. No reliance on unreleased Zed APIs, no
  AppleScript inspection of Zed's frontmost window, no Accessibility
  entitlement on the daemon.

**Non-Goals**

- We do not aim to identify which Zed terminal pane has focus
  (impossible from outside Zed without a Zed API; the user-chosen
  picker is the documented substitute).
- We do not aim to spawn or manage `claude` processes from the
  sidecar.
- We do not aim to ship a picker UI on Linux in this iteration —
  Linux falls back to a deterministic most-recently-active choice
  with a WARN log, and a follow-up OpenSpec change can add `zenity`
  / `kdialog` later.
- We do not aim to expose a session-tag mechanism today. The prior
  draft of this design included an `x-claude-code-session-tag`
  header + matching CLI flag; the user has explicitly removed it
  because (a) Claude CLI v2.1.76 does not emit it, (b) Zed has no
  clean way to inject it into a hand-launched terminal, and (c) the
  picker mechanism is sufficient. If Claude upstream ever ships a
  workspace/session identifier we'll add it then in a follow-up.

## Decisions

### D1 — Replace the broadcast notifier with an explicit client registry

**Decision.** Introduce a `ClientRegistry` owned by `Transport` whose
state is a `HashMap<ClientId, ClientHandle>` behind an `RwLock`. Each
`ClientHandle` holds:

- `id: ClientId` — a `uuid::Uuid` minted on accept; opaque, used in
  logs and as the disambiguator on the picker round-trip.
- `tx: mpsc::Sender<Notification>` — a bounded (capacity 64)
  per-client channel. The connection task `recv`s from this channel
  and writes frames to its WebSocket peer.
- `workspace_root: Option<PathBuf>` — canonicalized; captured per D3.
- `last_activity: tokio::time::Instant` — updated on every inbound
  JSON-RPC frame from this client.
- `connected_at: Instant` — fixed; used to compute "connected Xm ago"
  in picker labels.

On WebSocket accept (after auth), we lock the registry for write,
insert the handle, and drop the lock before starting the serve loop.
On disconnect, the loop removes its own entry. The registry is
`Arc`-shared with the IPC layer so the router can read it.

**Alternatives considered.**

- *Keep the broadcast and add an in-task filter.* Rejected: the
  filter would need a side channel from "the publisher of this
  frame" to "every subscriber's decision logic", which is exactly
  what a registry models cleanly.
- *Single `mpsc` to a router task that does the routing.* Rejected:
  adds an extra hop and a single point of contention. The
  registry-read approach lets `selection_changed` and `at_mentioned`
  share the same primitive.

### D2 — Remove the single-client displacement policy

**Decision.** Delete the `active_client: Mutex<Option<Arc<Notify>>>`
field, the `notify_waiters()` call on insert, and the
`my_displace.notified()` branch in the serve `select!`. Every
authorized connection runs to completion (until peer close, EOF,
transport error, or sidecar shutdown). The websocket capability spec
is updated correspondingly (see `specs/websocket/spec.md`).

**Rationale.** The displacement is the proximate cause of the user's
"all sessions receive the mention" symptom (see Why in
`proposal.md`). It is also incompatible with the LaunchAgent
deployment.

### D3 — Workspace-cwd capture, in priority order

**Decision.** Each registry entry's `workspace_root` is computed at
WebSocket-accept time in this order:

1. **`x-claude-code-workspace` request header** on the upgrade. We
   add this read in `handle_connection`'s callback. *Investigation*:
   whether Claude CLI sends it today is recorded in task #1; either
   way this branch is total (it is silently no-op when the header
   is absent).
2. **`clientInfo.cwd` field** in the MCP `initialize` request. The
   MCP spec doesn't define this field but allows extensions; we
   tolerate it as an optional string parsed from `params.clientInfo`.
   Captured by `transport::ws::dispatch_text` after the initialize
   request is parsed, then propagated to the registry via a
   `registry.set_workspace(id, path)` setter so `mcp::dispatch`
   stays I/O-free.
3. **Sidecar's `--workspace` flag.** Used as the last-resort default.
   For the LaunchAgent case `--workspace = $HOME`, which is too
   broad to disambiguate — but the routing rules (D5/D7) degrade
   gracefully when many clients share `$HOME` as their default.

In all cases the path is canonicalized via `std::fs::canonicalize`
where possible; if canonicalization fails (path missing), we keep
the as-supplied value and log at debug.

**Alternatives considered.**

- *Peer-PID + `proc_pidpath`.* TCP loopback sockets do not carry
  peer credentials portably; `LOCAL_PEERPID` is Unix-domain-only.
  Rejected.
- *AppleScript / Accessibility entitlement to query Zed.* Rejected:
  permission prompt on first run, codesigning, breaks on Linux,
  fragile across Zed versions.

### D4 — Picker mechanism (the user's tiebreak)

The user requires a **picker** when more than one registered client
matches the workspace. Two implementation paths exist; the chosen
path is **(a)** with explicit reasoning below.

#### Choice (a) — `osascript choose from list` from the helper

**Decision.** When the IPC handler runs the router and the at-mention
yields >1 candidate, the sidecar replies on the **same IPC
connection** with a one-line JSON `IpcFrame::Ambiguous { candidates }`
frame, then leaves the connection open. The
`ipc-send-at-mention` helper reads this reply, invokes
`osascript -e 'choose from list {...} with prompt "..."'`
synchronously, captures the chosen list-entry label, looks up its
`client_id`, and writes a second `at_mention` frame on the same
connection with `--client-id <picked>`. The router sees the
client_id override (rule 1) and routes directly. If the user cancels
the dialog (`osascript` exits with "false"), the helper sends no
follow-up and exits zero — the at-mention is intentionally dropped.

**Why this path.**

- **The daemon cannot show UI from inside an async tokio task.** A
  LaunchAgent daemon can shell out to `osascript`, but the IPC
  handler must not block all routing on a synchronous subprocess —
  a stuck user dialog would freeze every other workspace's at-mention
  routing. Moving the dialog into the **helper** keeps each click
  isolated to its own helper process.
- **The Zed task spawns the helper synchronously** with `reveal: "never"`,
  but the helper's blocking call to `osascript` does not depend on a
  visible terminal — `osascript` talks directly to the macOS
  WindowServer. Zed sees the helper as a long-running process for
  the duration of the picker, which is fine (the task simply exits
  later than usual).
- **It uses signals already available.** The picker labels are built
  from registry data the sidecar already has (`connected_at`,
  `last_activity`, `clientInfo.name`/`version`). No new platform
  dependency beyond `osascript`, which ships with every macOS.

**Picker label format.** Each `AmbiguousCandidate` carries
`client_id` (UUID, hidden from the user), `label` (visible string),
and an opaque `metadata` map. The visible `label` is composed
sidecar-side as:

> `Session {N} — connected {duration} ago (last active {duration} ago)`

where `{N}` is the 1-based index in the candidates list (stable for
the lifetime of one picker round-trip), the durations are humanised
to seconds/minutes ("17s", "3m"), and `last active` is omitted when
it equals `connected at` to within 1 second. If the registry entry's
`clientInfo` carried `name`/`version` strings, they are appended in
parens: `… (claude-code 2.1.76)`. The user sees a list of distinct,
ordered, time-stamped sessions — the only useful distinction
available when both sessions share a cwd.

**Linux fallback.** On Linux the helper detects the platform at
compile time (`#[cfg(target_os = "macos")]`) and, when ambiguous,
emits a WARN line to stderr (consumed by Zed's task log) saying
`"ambiguous workspace match; picker not available on this platform; routing to most-recently-active candidate (<label>) as a fallback"`,
then sends the follow-up frame with `--client-id` set to the
most-recently-active candidate from the reply. This keeps Linux
unblocked at the cost of giving up determinism on the rare same-cwd
case; tracked as a follow-up OpenSpec change to add `zenity` /
`kdialog`.

#### Choice (b, rejected) — notification toast + MRA fallback

**Why rejected.** The user explicitly rejected MRA as the routing
target ("不是最近一次 JSON-RPC 活动"). Wrapping MRA in a notification
toast does not change the routing — it just admits to it after the
fact. We honour the user's choice and use a real picker on macOS.

### D5 — Routing rules (deterministic, total)

**Decision.** The router is a pure function
`Router::route_at_mention(snapshot, frame) -> RoutingDecision`. The
`RoutingDecision` enum is:

- `DirectClient(ClientId)` — frame's `client_id` matches a live
  client (rule 1).
- `WorkspaceUnique(ClientId)` — exactly one client matches the
  workspace (rule 2).
- `Ambiguous { candidates: Vec<AmbiguousCandidate> }` — workspace
  matches multiple clients (rule 3); IPC layer translates this into
  an `Ambiguous` reply frame.
- `Singleton(ClientId)` — exactly one client in the registry, any
  workspace (rule 4).
- `NoMatch { known_workspaces: Vec<PathBuf> }` — none of the above
  (rule 5).

Rule order applied in priority:

1. If `frame.client_id` is `Some(id)` AND that id exists in the
   registry → `DirectClient(id)`. If the id is set but the client
   has disconnected, fall through to rule 5 with a WARN logged
   distinctly so the user knows the picker selection became stale.
2. Else if `frame.workspace_root` is `Some(r)`:
   - Filter clients by `canonical(client.workspace_root) == canonical(r)`.
   - 1 match → `WorkspaceUnique(id)`.
   - ≥2 matches → `Ambiguous { candidates }`. The sidecar's IPC
     handler converts this into the `Ambiguous` reply frame; it
     does NOT emit a notification.
3. Else if the registry has exactly one client → `Singleton(id)`.
4. Else → `NoMatch { known_workspaces }`. IPC handler logs a WARN
   listing the file path, workspace_root, and known workspaces.

`selection_changed` uses a separate, simpler function:
`route_selection_changed(snapshot, file_path) -> Vec<ClientId>` that
returns all clients whose workspace is a prefix of `file_path`;
empty filter (no workspace prefix matches) returns ALL client ids
(preserves today's fan-out for unknown workspaces). The picker step
SHALL NOT apply to `selection_changed`.

**Alternatives considered.**

- *Tiebreak by most-recently-active.* Rejected by the user.
- *Tiebreak by a picker that runs from the daemon.* Rejected per
  D4 — daemon-side blocking subprocess would freeze unrelated
  routing.
- *Skip ambiguity handling and drop the event.* Rejected — the
  user must be able to send the at-mention; dropping is a poor UX.

### D6 — Per-client mpsc capacity and lag policy

**Decision.** Use `mpsc::channel(64)` per client. On `tx.send().await`
from the router, if the channel is full we apply a 50 ms timeout; if
the timeout fires, we log WARN with `client.id` and *drop the
notification for that client only*. This is consistent with today's
`broadcast::Sender` behaviour, which silently lags slow subscribers;
the difference is that we now know *which* client lagged.

### D7 — IPC frame shape backward compatibility

**Decision.** `IpcFrame::AtMention` adds two `#[serde(default,
skip_serializing_if = "Option::is_none")]` fields:
`workspace_root: Option<PathBuf>` and `client_id: Option<ClientId>`.
A pre-update CLI helper writing the old frame shape continues to
parse correctly; both new fields default to `None` and the router
falls back per D5's rule 3/4 (singleton direct route or WARN drop).

A NEW `IpcFrame::Ambiguous { candidates: Vec<AmbiguousCandidate> }`
variant joins the enum. The serialized discriminator is
`"type":"ambiguous"`. The wire shape is documented in
`specs/protocol/spec.md`.

`ClientId` on the IPC wire is the lowercase hex of the underlying
UUID (i.e. `"deadbeef-..."` 36-char form). This keeps the wire
shape human-readable; sidecar-side parsing uses `Uuid::parse_str`.

### D8 — Persistent IPC connection during picker

**Decision.** The IPC connection used by `ipc-send-at-mention` STAYS
OPEN after the first `at_mention` frame is sent. The helper's read
loop awaits either an `ack`-like terminal frame (e.g. routing
succeeded, signalled by an `Ack` reply) or an `Ambiguous` reply. The
sidecar SHALL NOT close the IPC connection after an `at_mention`
frame in this design — that is a behavioural change documented in
the IPC spec delta.

For backwards compatibility, the sidecar SHALL still tolerate
helpers that close immediately after writing an `at_mention`: if the
router yields `Ambiguous` and the IPC peer has already closed, the
sidecar logs a WARN and drops (it has no one to reply to). This
matches the no-match drop behaviour and remains total.

**Picker timeout.** The helper waits up to 60 seconds for the user
to interact with the picker. If `osascript` does not return in that
window (extremely unlikely; the dialog blocks the user, not the
helper, and macOS doesn't enforce timeouts) the helper logs an ERROR
and exits non-zero; the connection is closed without writing a
follow-up.

### D9 — Activity tracking

**Decision.** Bump `client.last_activity = Instant::now()` on every
inbound text frame in `serve_authorized`'s `ws.next()` branch, before
dispatch. This is **not** used as a routing tiebreaker (the picker
is the tiebreaker) — it is used:

- as the `last active` field in `AmbiguousCandidate.label`, so the
  user has temporal context when picking;
- as the deterministic fallback on Linux (D4 Linux fallback);
- in operator logs for understanding traffic patterns.

### D10 — Logging discipline

**Decision.** No logging of secrets (auth token already redacted).
Every routing decision logs at DEBUG with the chosen `client.id`
(UUID), the rule that fired (`"client-id-override" |
"workspace-unique" | "ambiguous-reply" | "singleton-registry" |
"dropped" | "stale-client-id"`), and the frame's `file_path`. On a
`dropped` decision the WARN level log additionally enumerates the
known workspaces in the registry so the user can spot a typo /
mismatch in their setup.

## Risks / Trade-offs

- **Helper now writes-then-reads on the IPC socket** — small
  behavioural change to the IPC protocol's bidirectionality.
  Mitigated by D8's tolerance for legacy helpers that close after
  writing. The new helper opts in via its `--workspace-root` flag,
  which a new helper always sends; old helpers that don't are by
  definition pre-this-change.
- **`osascript` subprocess from a foreground task** — adds a brief
  GUI dialog whenever ambiguity exists. The user explicitly asked
  for this. Trade-off: keystrokes-to-mention increases by one
  click in the multi-session case. Acceptable per the user's choice.
- **Linux behaviour differs from macOS.** Linux's same-cwd case
  falls back to MRA with a WARN — not "the same as macOS". This is
  an explicit follow-up; this iteration ships macOS-first.
  Mitigated by: documenting clearly in `README.md`; ensuring the
  WARN line is grep-friendly; making sure the eventual Linux
  picker addition does not require any protocol change (the helper
  already invokes platform-specific code at the `cfg`-boundary).
- **`std::fs::canonicalize` on the hot path.** Mitigated by
  canonicalizing once per client at accept time and once per IPC
  frame; this is at most one extra `stat` per routing decision.
- **Stale client_id from the picker.** If the chosen session
  disconnects in the ~1 second between picker close and follow-up
  frame, we drop with a distinct WARN. The helper does not retry;
  the user simply presses cmd-ctrl-c again. Acceptable for a rare
  edge case.
- **Removing the single-client policy is wire-visible.** Prior
  Claude clients that relied on being displaced are now coexisting.
  Intended fix; Claude CLI tolerates many concurrent peers fine
  (it has always sent its frames over its own connection).

## Migration Plan

This is a single-binary, single-deploy change. No data migration.

1. Roll out the new sidecar binary; users replace it via
   `cargo install --path crates/zed-claude-bridge` (per README).
2. The LaunchAgent restart is automatic on next login (or via
   `launchctl kickstart -k`).
3. Users may keep their existing `.zed/tasks.json` — the missing
   `--workspace-root` falls through to D5 rule 3/4 (singleton or
   no-match drop). For correct multi-session routing, users update
   `.zed/tasks.json` per the updated README.

Rollback: revert the binary. The wire protocol is a superset, so a
new IPC frame written by an old CLI helper to a downgraded sidecar
is still valid (the optional fields are simply absent on the wire).

## Open Questions

- **OQ1.** Does the Claude Code CLI v2.1.76 send any of:
  - `x-claude-code-workspace` request header,
  - `clientInfo.cwd` in `initialize`'s params,
  - or any other workspace hint?
  Resolution: read
  `~/.vscode/extensions/anthropic.claude-code-2.1.76-darwin-arm64/extension.js`
  (per `.harness/project.md` "Knowledge Sources" priority 2) before
  implementing D3 priority 1/2. The answer determines whether those
  branches are reachable in practice; either way the design works.
- **OQ2.** Picker label phrasing. The draft format
  `"Session N — connected Xm ago (last active Ys ago)"` is a strawman;
  the implementer should feel free to tighten it during task #6 if
  there is a clearer option. The wire shape (`label: String` per
  candidate) does not constrain phrasing.
- **OQ3.** Should we ship a Linux picker (`zenity --list` /
  `kdialog --menu`) in this same change, or in a follow-up? The plan
  defers to a follow-up to keep the diff small; if the team wants
  Linux parity, raise it and we'll extend `tasks.md` accordingly.
