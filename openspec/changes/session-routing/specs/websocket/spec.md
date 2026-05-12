# websocket Specification Delta — session-routing

## REMOVED Requirements

### Requirement: Single-client policy

**Reason**: The single-client displacement policy is the proximate
cause of the user-visible bug "at-mention reaches all sessions": a
new authorized client subscribes to the outbound notifier *before*
the prior client's displacement close-frame round-trip completes,
and the Claude CLI auto-reconnects after the `1000` close, which
sustains the race indefinitely. The policy is also incompatible with
the recommended LaunchAgent deployment, where one sidecar at `$HOME`
must serve many Claude sessions from many projects simultaneously.

**Migration**: Multi-client coexistence is now first-class — see
**Multi-client registry** below. No on-the-wire migration needed for
Claude clients; they continue to connect normally and are no longer
forcibly closed when a second client appears. Any IDE-side code that
relied on observing a `1000` close with reason `"Disconnecting previous
WebSocket client"` must be updated to tolerate its absence.

## ADDED Requirements

### Requirement: Multi-client registry

The sidecar SHALL maintain an in-memory registry of every currently
connected, authorized WebSocket client. Each entry SHALL hold at
least:

- a per-connection opaque identifier (UUID v4) used in logs and as
  the routing-override key carried by IPC `at_mention` frames after
  a picker round-trip,
- a bounded per-client outbound channel (`tokio::sync::mpsc`,
  capacity 64),
- a `workspace_root: Option<PathBuf>` captured per the
  **Workspace identification on connect** requirement,
- a `last_activity: Instant` initialised at connect time and
  updated per **Per-client activity tracking**,
- a `connected_at: Instant` fixed at connect time, used to build
  human-readable labels for the picker candidate list.

The sidecar SHALL accept any number of concurrent authorized
clients; there is no maximum and no displacement. On connection
loss (clean close, EOF, transport error), the sidecar SHALL remove
that entry from the registry and SHALL continue to serve every
remaining client uninterrupted.

#### Scenario: Two authorized clients coexist

- **GIVEN** authorized client A is connected
- **WHEN** authorized client B opens a fresh connection with a valid
  auth header
- **THEN** A SHALL NOT receive a WebSocket close frame
- **AND** both A and B SHALL appear in the registry simultaneously
- **AND** both SHALL be able to send and receive JSON-RPC frames
  independently

#### Scenario: Client disconnect leaves the registry consistent

- **GIVEN** the registry has clients A, B, and C
- **WHEN** client B's WebSocket disconnects (clean close)
- **THEN** the registry SHALL contain exactly A and C immediately
  after the disconnect
- **AND** A's and C's connections SHALL continue uninterrupted

### Requirement: Workspace identification on connect

For every authorized client the sidecar SHALL attempt to resolve a
`workspace_root` value at WebSocket-accept time using the following
ordered sources, taking the first that produces a non-empty value:

1. The `x-claude-code-workspace` HTTP request header on the
   WebSocket upgrade. If present and non-empty, the value SHALL be
   canonicalized via `std::fs::canonicalize`; on canonicalization
   failure the raw value SHALL be kept and a DEBUG log emitted.
2. A `cwd` string field inside the `clientInfo` object of the MCP
   `initialize` request's `params`, captured after the request is
   parsed in the transport layer and propagated to the registry
   entry by id.
3. The sidecar's own `--workspace` flag value as a last-resort
   default.

If none of (1)–(3) produce a value, `workspace_root` SHALL remain
`None`. The capture SHALL NOT block the accept loop on file-system
I/O for more than a single `canonicalize` call per client.

#### Scenario: Workspace header takes priority

- **GIVEN** a client sends both an `x-claude-code-workspace: /a/proj`
  header AND an `initialize` with `clientInfo.cwd = "/b/proj"`
- **WHEN** the connection is registered
- **THEN** the registry entry's `workspace_root` SHALL be the
  canonical form of `/a/proj`

#### Scenario: clientInfo.cwd used when header absent

- **GIVEN** a client connects with no workspace header and sends
  `initialize` with `clientInfo.cwd = "/Users/me/proj"`
- **WHEN** the `initialize` is processed
- **THEN** the registry entry's `workspace_root` SHALL be the
  canonical form of `/Users/me/proj`

#### Scenario: Defaults to --workspace when no client-side signal

- **GIVEN** the sidecar was launched with `--workspace /Users/me/p`
- **AND** a client connects with no workspace header and no
  `clientInfo.cwd`
- **WHEN** the connection is registered
- **THEN** the registry entry's `workspace_root` SHALL be the
  canonical form of `/Users/me/p`

### Requirement: Per-client activity tracking

For each connected client the sidecar SHALL update
`last_activity = Instant::now()` whenever an inbound WebSocket text
frame is received from that client (before JSON-RPC dispatch).
Outbound frames (sidecar → client) SHALL NOT bump `last_activity`.

This timestamp SHALL NOT be used as an automatic tiebreaker for
at-mention routing — the picker is the tiebreaker on macOS, and on
Linux the most-recently-active candidate is selected only as the
documented fallback when no picker UI is available on the platform.
The timestamp is otherwise used to build human-readable picker
candidate labels (see the `notifications` capability).

#### Scenario: Inbound JSON-RPC bumps activity

- **GIVEN** client A's `last_activity` is `T0`
- **WHEN** A sends a JSON-RPC request at wall-clock time `T0 + 5s`
- **THEN** A's `last_activity` SHALL be updated to a value `>= T0 + 5s`

#### Scenario: Sidecar-originated notification does not bump activity

- **GIVEN** clients A and B are both registered
- **AND** A's `last_activity = T0`
- **WHEN** the sidecar routes a `selection_changed` notification to
  A at `T0 + 5s`
- **THEN** A's `last_activity` SHALL remain `T0`

### Requirement: Outbound delivery via per-client channel

Outbound notifications (`at_mentioned`, `selection_changed`) SHALL
be delivered to a specific client by sending into that client's
per-client mpsc channel, never via a fan-out primitive. The
connection task SHALL receive from this channel and write each
notification as a single JSON-RPC text frame to its peer.

If a `tx.send` to a client's channel does not complete within 50 ms,
the sidecar SHALL log a WARN containing that client's registry id and
SHALL drop that single notification for that client only. The
connection SHALL NOT be closed and other clients' deliveries SHALL
NOT be affected.

#### Scenario: Single-recipient delivery

- **GIVEN** clients A and B are both registered
- **AND** the router selects A as the unique recipient for an
  outbound `at_mentioned`
- **WHEN** the notification is dispatched
- **THEN** A SHALL receive exactly one `at_mentioned` text frame
- **AND** B SHALL NOT receive any frame produced by that dispatch

#### Scenario: Slow client does not stall the router

- **GIVEN** client A is connected but not reading frames
- **AND** A's outbound channel has been filled past capacity for >50 ms
- **WHEN** the router attempts to deliver another notification to A
- **THEN** the dispatch SHALL log a WARN tagged with A's registry id
- **AND** the notification for A SHALL be dropped
- **AND** other clients SHALL still receive their own deliveries
  without delay
