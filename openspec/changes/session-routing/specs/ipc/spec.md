# ipc Specification Delta — session-routing

## MODIFIED Requirements

### Requirement: Supported IPC message types

The sidecar SHALL accept IPC frames whose `type` field is one of
`selection`, `at_mention`, `workspace_folders`, `open_editors`, or
`ping`. Unknown `type` values SHALL be logged at WARN level and
otherwise ignored.

The `at_mention` frame SHALL accept two new optional fields beyond
`file_path`, `line_start`, and `line_end`:

- `workspace_root` (string, optional): an absolute filesystem path
  identifying the workspace whose `cmd-ctrl-c` produced this
  at-mention. When supplied, the sidecar's router SHALL use it to
  pick the recipient WebSocket client per the **notifications**
  capability's at-mention routing requirement.
- `client_id` (string, optional): the lowercase hex form of a UUID
  identifying a registry entry. When supplied, the router SHALL
  bypass workspace matching and route directly to the matching
  registered client. This field is set only on the second leg of a
  picker round-trip (see **Ambiguous reply frame** below); helpers
  SHALL NOT set it on a first-leg `at_mention`.

Both fields SHALL be optional. A frame omitting them SHALL remain
valid and SHALL be processed using the router's no-hint fallback
rules.

#### Scenario: selection frame updates EditorState

- **WHEN** the extension sends
  `{"type":"selection","file_path":"/p/a.rs","line_start":3,"line_end":4,"text":"x"}`
- **THEN** subsequent `getCurrentSelection` calls SHALL reflect this state per
  `mcp` spec

#### Scenario: at_mention frame triggers routed notification

- **WHEN** the extension sends
  `{"type":"at_mention","file_path":"/p/a.rs","line_start":3,"line_end":4,"workspace_root":"/p"}`
- **AND** exactly one registered client has workspace `/p`
- **THEN** the sidecar SHALL produce exactly one `at_mentioned` JSON-RPC
  notification delivered to that single WebSocket client per the
  `notifications` spec's routing rules
- **AND** SHALL NOT fan the notification out to additional clients

#### Scenario: at_mention frame without workspace_root falls back to router defaults

- **WHEN** the extension sends
  `{"type":"at_mention","file_path":"/p/a.rs","line_start":3,"line_end":4}`
- **THEN** the sidecar SHALL still emit a routed `at_mentioned`
  notification (zero or one recipient) per the `notifications`
  spec's no-hint rules

#### Scenario: at_mention frame with client_id routes directly

- **GIVEN** the registry has two clients A and B, both with
  `workspace_root=/p`
- **WHEN** the helper sends
  `{"type":"at_mention","file_path":"/p/a.rs","line_start":3,"line_end":4,"workspace_root":"/p","client_id":"<B's UUID>"}`
- **THEN** the resulting `at_mentioned` SHALL be delivered to B only
- **AND** SHALL NOT be delivered to A

#### Scenario: at_mention frame with stale client_id drops and logs

- **GIVEN** the registry has only client A (id `<A>`)
- **WHEN** the helper sends an `at_mention` with `client_id =
  <B>` (where `<B>` is not in the registry)
- **THEN** the sidecar SHALL NOT deliver the notification to A
- **AND** SHALL log a WARN distinctly identifying the stale
  `client_id` and the registry's current ids

#### Scenario: Unknown type ignored

- **WHEN** the extension sends `{"type":"unknown","foo":"bar"}`
- **THEN** the sidecar SHALL log at WARN
- **AND** SHALL NOT close the IPC connection
- **AND** SHALL continue processing subsequent frames

## ADDED Requirements

### Requirement: Ambiguous reply frame

The sidecar SHALL reply on the same IPC connection with a single
line-delimited JSON `ambiguous` frame whenever the router determines
that an `at_mention` IPC frame matches more than one registered
WebSocket client by workspace AND the frame carries no `client_id`
override. The frame SHALL have the following shape:

```
{
  "type": "ambiguous",
  "candidates": [
    {
      "client_id": "<UUID hex>",
      "label": "<human-readable label>",
      "connected_at_ms_ago": <integer>,
      "last_activity_ms_ago": <integer>
    },
    …
  ]
}
```

The `candidates` array SHALL list every registered client that matches
the frame's `workspace_root` (canonical-path equality), in stable
order — sort key: `connected_at` ascending (oldest first). The
`label` field SHALL be a non-empty string built sidecar-side suitable
for display in a list dialog (e.g. `"Session 1 — connected 2m ago"`).
The `connected_at_ms_ago` and `last_activity_ms_ago` integers SHALL
be the elapsed milliseconds since each event, computed against the
sidecar's current `Instant`.

The sidecar SHALL keep the IPC connection open after writing the
`ambiguous` reply, awaiting a follow-up frame from the helper. The
sidecar SHALL NOT emit any `at_mentioned` notification for this
event until the helper writes a follow-up `at_mention` whose
`client_id` matches one of the listed candidates (or fails to do so
within a reasonable client-side timeout, in which case the helper
simply closes the connection).

If the helper closes the IPC connection without sending a follow-up,
the sidecar SHALL log a DEBUG-level note (the user cancelled the
picker) and SHALL NOT route the at-mention.

#### Scenario: Two clients in same workspace produce an ambiguous reply

- **GIVEN** the registry has clients A and B, both with
  `workspace_root=/p`
- **WHEN** the helper sends
  `{"type":"at_mention","file_path":"/p/a.rs","line_start":0,"line_end":0,"workspace_root":"/p"}`
- **THEN** the sidecar SHALL write a single line on the same IPC
  connection whose JSON body has `"type":"ambiguous"`
- **AND** the body's `candidates` array SHALL contain exactly two
  entries, with `client_id` values equal to A's and B's UUIDs
- **AND** the sidecar SHALL NOT emit any `at_mentioned` notification
  for this event yet

#### Scenario: Follow-up frame routes to the picked client

- **GIVEN** the sidecar has just written an `ambiguous` reply listing
  clients A and B on the IPC connection
- **WHEN** the helper writes a follow-up
  `{"type":"at_mention","file_path":"/p/a.rs","line_start":0,"line_end":0,"workspace_root":"/p","client_id":"<B's UUID>"}`
  on the same connection
- **THEN** B SHALL receive exactly one `at_mentioned` JSON-RPC
  notification
- **AND** A SHALL NOT receive any frame for this event

#### Scenario: Helper closes without follow-up after ambiguous reply

- **GIVEN** the sidecar has just written an `ambiguous` reply on the
  IPC connection
- **WHEN** the helper closes the IPC connection without writing a
  follow-up frame
- **THEN** the sidecar SHALL NOT emit any `at_mentioned` notification
- **AND** the sidecar SHALL log a DEBUG message noting the cancellation

### Requirement: IPC connection lifetime for at_mention round-trips

The sidecar SHALL NOT close an IPC connection after handling an
`at_mention` frame. The same connection SHALL remain available for
subsequent frames from the same helper process, including the
follow-up frame that disambiguates an `ambiguous` reply.

The sidecar SHALL continue to tolerate legacy helpers that close
their IPC connection immediately after writing a single
`at_mention`: if the router yields an ambiguity and the IPC peer has
already gone, the sidecar SHALL log a WARN and drop the at-mention
gracefully (no panic, no leaked resources).

#### Scenario: Helper reuses one connection for picker round-trip

- **WHEN** a helper opens an IPC connection, writes an
  `at_mention` frame, reads an `ambiguous` reply, writes a
  follow-up `at_mention` with `client_id`, then closes
- **THEN** the sidecar SHALL accept all reads/writes on the single
  connection without errors
- **AND** SHALL deliver exactly one `at_mentioned` notification (to
  the picked client)

#### Scenario: Legacy helper closing immediately is tolerated

- **GIVEN** the registry has two clients in the same workspace
- **WHEN** a helper writes an `at_mention` frame without `client_id`
  and immediately closes the IPC connection
- **THEN** the sidecar SHALL NOT panic
- **AND** SHALL log a WARN indicating an ambiguous match could not
  be resolved because the peer disconnected
- **AND** SHALL NOT deliver any `at_mentioned` notification for this
  event
