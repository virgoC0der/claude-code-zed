# Proposal: Route at-mentions to the focused Claude session

## Why

When a user has multiple `claude /ide` sessions open in the same working
directory (e.g. two terminals, two `claude /ide` connections), pressing
`cmd-ctrl-c` in Zed currently delivers the `@<file>#L<m>-<n>` at-mention to
**every** connected session at once. The user expects the mention to land
in **only the one Claude session whose terminal they are actively using**,
matching how the VSCode "Send to Claude Code" action behaves.

The proximate cause is two design choices in today's sidecar:

1. The outbound notifier is a `tokio::sync::broadcast::Sender` — every
   subscribed WebSocket client receives every notification
   (`crates/zed-claude-bridge/src/transport/ws.rs:170`, `181`, `303`).
   There is no per-client filter.
2. The single-client policy that was meant to enforce one active client
   at a time (`transport/ws.rs:286–301`) is racy: a new client is
   subscribed to the broadcast *before* the prior client's displacement
   close-frame round-trip completes, so a notification fired in that
   window is delivered to **both** the displaced and the new client.
   In practice the Claude CLI auto-reconnects after a 1000-close, which
   sustains the race indefinitely — the user observes "all sessions
   receive the mention" because every session is briefly subscribed.

The single-client policy is also incompatible with the now-recommended
LaunchAgent deployment (`README.md` "Option A"), where one sidecar at
`$HOME` serves Claude sessions for many projects simultaneously. We need
to support multiple concurrent clients *by design*, and route each
at-mention deliberately to the one session whose workspace matches the
event source — falling back to a user-facing picker when the workspace
alone cannot disambiguate.

## What Changes

- **BREAKING (`websocket` capability):** Remove the single-client
  displacement policy. The sidecar will accept many concurrent
  authorized WebSocket clients. Each is tracked in an explicit registry
  with per-client metadata (workspace, last-activity timestamp,
  optional `clientInfo.name`/`version`/`cwd` from MCP `initialize`).
- **BREAKING (internal):** Replace the
  `tokio::sync::broadcast::Sender<Notification>` notifier with a
  registry-backed router that picks **at most one recipient** per
  outbound `at_mentioned` event. `selection_changed` is delivered to
  clients whose workspace matches the editor that produced the
  selection (fans out within a workspace; falls back to all clients
  when the workspace is unknown — preserving today's behaviour for the
  read-only state push).
- **New IPC fields:** `IpcFrame::AtMention` gains two optional fields:
  - `workspace_root: Option<PathBuf>` — populated by the Zed task from
    `$ZED_WORKTREE_ROOT` so the router can match by workspace.
  - `client_id: Option<ClientId>` — set only on the second leg of a
    picker round-trip (see below). The router treats this as a
    "route directly to this registered client" override, bypassing the
    normal workspace matching.
- **New IPC reply frame:** `IpcFrame::Ambiguous { candidates: Vec<AmbiguousCandidate> }`,
  where each candidate carries the registry `client_id`, `connected_at`
  age in seconds, and a `label` string (e.g. `"Claude session 1 (connected 2m ago)"`).
  Sent by the sidecar in response to an `at_mention` whose workspace
  match yields >1 candidate. The IPC connection stays open after the
  reply so the helper can write a follow-up disambiguated frame on the
  same connection.
- **CLI helper round-trip:** the `ipc-send-at-mention` helper:
  1. Sends its first `at_mention` frame with `--workspace-root` (no
     `--client-id`).
  2. If the sidecar replies `ambiguous`, the helper invokes
     `osascript -e 'choose from list ...'` (macOS) to present a native
     list dialog. On Linux, the helper logs a WARN
     `"ambiguous workspace match, no picker available on this platform"`
     and sends a second frame with `--client-id` set to the
     most-recently-active candidate (deterministic fallback so users
     are never blocked; tracked as a follow-up to add a Linux picker).
  3. Writes a second `at_mention` frame on the same IPC connection
     with `--client-id <picked>`. The router routes directly to that
     client.
  4. If the user cancels the picker, the helper sends no second frame
     and exits 0 (the at-mention is intentionally dropped).
- **CLI flag:** `--workspace-root <PATH>` on `ipc-send-at-mention`,
  distinct from the existing `--workspace <DIR>` (which still names
  the IPC-socket scope). No `--session-tag` / no
  `x-claude-code-session-tag` header — those have been intentionally
  removed from this design (see "Notable removals" below).
- **Workspace identification on connect:** the sidecar derives each
  client's workspace cwd in priority order:
  1. `x-claude-code-workspace` WebSocket request header, if Claude
     emits one (see Investigation).
  2. `cwd` field inside `params.clientInfo` on the MCP `initialize`
     request, if present.
  3. The sidecar's own `--workspace` flag (used as a last-resort
     default; for the LaunchAgent it is `$HOME`, which is too broad
     to disambiguate but routing degrades gracefully — see Rules
     below).
- **Routing rules (deterministic, total):** when an at-mention is
  emitted, the router decides among:
  1. **Direct client_id override.** If the frame carries
     `client_id = Some(id)` and that id exists in the registry → route
     to it. (If the id is stale, fall through to rule 5 with a WARN
     noting the disconnect.)
  2. **Workspace match — unique.** If the frame's `workspace_root` is
     `Some(r)` and exactly one registered client has a canonical
     `workspace_root` equal to `canonical(r)` → route to it.
  3. **Workspace match — multiple candidates → picker.** If the
     frame's `workspace_root` is `Some(r)` and >1 registered clients
     match, the sidecar SHALL reply on the IPC connection with an
     `Ambiguous` frame and NOT emit a notification. Routing for this
     event is suspended until the helper writes a follow-up frame
     with `client_id` (rule 1).
  4. **Singleton registry.** If the registry contains exactly one
     client (regardless of workspace) → route to it.
  5. **No match.** Otherwise, the sidecar logs a WARN containing the
     frame's file path, workspace_root, and the set of known
     workspaces in the registry, and drops the at-mention.
- **Activity tracking:** every inbound WebSocket text frame (JSON-RPC
  request or notification) bumps the client's `last_activity` to
  `now`. This is used only to label picker candidates with "most
  recently active" / "connected at"; **it is NOT a tiebreaker** for
  routing — the picker is the tiebreaker per the user's explicit
  decision.
- **Investigation deliverable (decided before implementation starts):**
  audit the VSCode Claude Code extension v2.1.76 and the running
  Claude CLI to confirm whether `clientInfo.cwd` and/or an
  `x-claude-code-workspace` header are emitted. If neither is present
  today, ship with the IPC-folder-hint + `--workspace` defaults; the
  design is total either way (the singleton/picker fallbacks fire
  cleanly when workspace is unknown).

### Notable removals from the prior draft

- **`session_tag` / `x-claude-code-session-tag` header — removed.**
  Claude CLI v2.1.76 does not emit it, Zed has no clean way to inject
  it into a manually-launched terminal, and the picker tiebreak makes
  it unnecessary. If a future Claude release ships a workspace or
  session identifier, we'll add it then in a follow-up OpenSpec
  change. This shrinks the surface area and removes dead-code.
- **Most-recently-active tiebreaker — replaced by picker.** Within a
  matching workspace, ambiguity is resolved by user choice, not
  heuristic. MRA survives only as the **deterministic fallback on
  Linux**, where we have no native picker today; flagged as a
  follow-up.

## Capabilities

### New Capabilities

(none — this change modifies existing capabilities only)

### Modified Capabilities

- `websocket`: removes the single-client policy; adds the per-client
  registry, workspace capture on upgrade, and activity tracking. The
  auth-token, loopback-bind, random-port, and JSON-RPC text-frame
  requirements are unchanged.
- `ipc`: extends the `at_mention` IPC frame envelope with optional
  `workspace_root` and `client_id` fields; adds a new outbound
  `ambiguous` frame type sent from sidecar to helper on the same
  connection when the workspace match is non-unique.
- `notifications`: adds routing semantics — `at_mentioned` is
  delivered to **exactly one** client per disambiguated IPC frame (or
  zero with a WARN log), never broadcast. `selection_changed` is
  delivered to every client whose workspace matches the source
  selection (or to all clients if the workspace is unknown —
  preserves today's behaviour).
- `protocol`: adds the optional `workspaceRoot` / `clientId` fields
  on `at_mention` IPC frames; adds the `ambiguous` IPC frame variant
  and `AmbiguousCandidate` shape; adds the optional
  `x-claude-code-workspace` request header on the WebSocket upgrade.

## Impact

- **Affected source files (project-relative):**
  - `crates/zed-claude-bridge/src/transport/ws.rs` — replace
    `broadcast::Sender` with the new `ClientRegistry`; remove
    `active_client` mutex + displacement; add per-client metadata
    capture on `handle_connection` and per-frame activity bumps inside
    `serve_authorized`.
  - `crates/zed-claude-bridge/src/transport/mod.rs` — expose the new
    `ClientRegistry` and `Router` types.
  - `crates/zed-claude-bridge/src/ipc/server.rs` — invoke
    `Router::route_at_mention(...)` for at-mentions; emit the
    `Ambiguous` reply frame back to the IPC caller when the router
    yields multiple candidates; route `selection_changed` to
    workspace-matching clients.
  - `crates/zed-claude-bridge/src/protocol.rs` — extend
    `IpcFrame::AtMention` with `workspace_root: Option<PathBuf>` and
    `client_id: Option<ClientId>`; add a new `IpcFrame::Ambiguous {
    candidates: Vec<AmbiguousCandidate> }` variant.
  - `crates/zed-claude-bridge/src/app/cli.rs` — add
    `--workspace-root` and `--client-id` to `IpcSendAtMentionArgs`;
    add the picker round-trip logic to `run_ipc_send_at_mention`
    (read the optional `ambiguous` reply line; invoke `osascript`;
    write the follow-up frame).
  - `crates/zed-claude-bridge/src/app/lifecycle.rs` — instantiate the
    new registry/router; wire it through `Transport::new` and
    `IpcServer::new`.
  - `.zed/tasks.json` — set `--workspace-root "$ZED_WORKTREE_ROOT"`
    on the `Send selection to Claude Code` task.
  - `README.md` — add a "Multiple Claude sessions" section
    documenting the routing rules, the picker UX (macOS), the Linux
    fallback, and the troubleshooting path when an at-mention is
    dropped.
- **Tests added:** routing-table unit tests; end-to-end tests
  asserting (a) two clients in distinct workspaces each receive only
  their own at-mention, (b) two clients in same workspace trigger an
  `ambiguous` reply on the IPC connection, (c) a follow-up frame with
  `client_id` routes correctly, (d) singleton registry routes any
  at-mention, (e) no-match drops with WARN, (f) `selection_changed`
  fans out within a workspace.
- **Dependencies:** no new external crates. The picker call uses
  `osascript`, present on every macOS.
- **No protocol break on the WebSocket wire:** the JSON-RPC handshake
  and tool list are byte-identical. Only the per-connection lifetime
  semantics change (no more 1000-close on a second connect).
- **Deployment story unchanged:** LaunchAgent at `$HOME` continues to
  work and is in fact the main beneficiary — it can now serve
  multiple workspaces correctly, and within-workspace ambiguity is
  resolved by the picker rather than a hidden heuristic.
