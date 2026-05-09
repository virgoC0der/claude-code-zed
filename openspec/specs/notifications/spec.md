# notifications Specification

## Purpose
TBD - created by archiving change zed-claude-bridge. Update Purpose after archive.
## Requirements
### Requirement: selection_changed notification shape

When the sidecar's tracked selection changes, it SHALL emit a JSON-RPC notification
to the connected MCP client with method `selection_changed` and params containing
`text`, `filePath`, `fileUrl`, and `selection: {start: {line, character}, end:
{line, character}, isEmpty}`. Line and character values SHALL be 0-indexed.

#### Scenario: Notification carries 0-indexed coordinates

- **GIVEN** an authenticated client is connected
- **WHEN** the extension sends a `selection` IPC frame with `lineStart = 10`,
  `lineEnd = 12` for `/p/main.rs`
- **AND** the 300 ms debounce window elapses with no further IPC frames
- **THEN** the client SHALL receive a frame
  `{"jsonrpc":"2.0","method":"selection_changed","params":{...}}`
- **AND** `params.selection.start.line` SHALL equal `10`
- **AND** `params.selection.end.line` SHALL equal `12`
- **AND** `params.filePath` SHALL equal `/p/main.rs`

### Requirement: 300 ms debounce on selection_changed

The sidecar SHALL debounce `selection_changed` notifications by 300 milliseconds:
each incoming `selection` IPC frame SHALL reset a per-client timer; the
notification SHALL fire only after the timer expires without another frame
arriving. While the timer is pending, intermediate frames SHALL only update
in-memory state.

#### Scenario: Rapid IPC frames coalesce into one notification

- **GIVEN** an authenticated client is connected
- **WHEN** the extension sends three `selection` IPC frames at intervals of 100 ms
  for the same file with three different ranges
- **AND** then sends nothing for 400 ms
- **THEN** the client SHALL receive exactly one `selection_changed` notification
- **AND** the notification's `params.selection` SHALL match the third (most recent)
  frame

#### Scenario: Idle period below 300 ms suppresses delivery

- **GIVEN** an authenticated client is connected
- **WHEN** the extension sends a `selection` IPC frame and then a second one 200 ms
  later
- **AND** then sends nothing for another 200 ms
- **THEN** within 250 ms of the first frame, the client SHALL NOT have received any
  `selection_changed` notification

### Requirement: selection_changed deduplicates identical state

The sidecar SHALL NOT send a second `selection_changed` notification when, at debounce-
timer expiry, the pending selection's `text`, `filePath`, and `selection` range are
byte-equal to the last `selection_changed` previously sent on this connection.

#### Scenario: Identical selection emitted twice yields one notification

- **GIVEN** the client has already received a `selection_changed` for `/p/a.rs#L5-5`
- **WHEN** the extension sends a fresh `selection` IPC frame describing exactly the
  same `/p/a.rs#L5-5`
- **AND** the 300 ms window elapses
- **THEN** the client SHALL NOT receive a second `selection_changed`

### Requirement: Skip comment:// and output:// URIs

The sidecar SHALL NOT emit `selection_changed` when the IPC frame's `filePath` URI
scheme is `comment` or `output`.

#### Scenario: comment:// selection is ignored

- **WHEN** the extension sends a `selection` IPC frame whose `filePath` starts with
  `comment://`
- **THEN** no `selection_changed` notification SHALL be sent during or after the
  debounce window

### Requirement: at_mentioned notification shape

When the sidecar receives an `at_mention` IPC frame, it SHALL immediately (without
debounce) emit a JSON-RPC notification with method `at_mentioned` and params
`{filePath, lineStart, lineEnd}`. `lineStart` and `lineEnd` SHALL be **1-indexed**
(the sidecar SHALL add 1 to the 0-indexed values it receives over IPC).

#### Scenario: at_mention IPC frame produces 1-indexed notification

- **GIVEN** an authenticated client is connected
- **WHEN** the extension sends
  `{"type":"at_mention","filePath":"/p/x.rs","lineStart":9,"lineEnd":19}`
- **THEN** within 50 ms the client SHALL receive
  `{"jsonrpc":"2.0","method":"at_mentioned","params":{"filePath":"/p/x.rs","lineStart":10,"lineEnd":20}}`

### Requirement: at_mentioned bypasses the debounce

The `at_mentioned` notification SHALL NOT be subject to the selection_changed
debounce; multiple successive at-mentions SHALL each result in their own
notification.

#### Scenario: Two at-mentions yield two notifications

- **WHEN** the extension sends two `at_mention` IPC frames 50 ms apart
- **THEN** the client SHALL receive two `at_mentioned` notifications, in the order
  the IPC frames arrived

### Requirement: Notifications dropped when no client is connected

The sidecar SHALL drop (not buffer) both `selection_changed` and `at_mentioned`
notifications when no MCP client is currently connected. The sidecar's in-memory
`EditorState` SHALL still be updated.

#### Scenario: at_mention with no client is recorded but not sent

- **GIVEN** no WebSocket client is connected
- **WHEN** the extension sends an `at_mention` IPC frame
- **THEN** the sidecar SHALL NOT panic or queue the notification
- **AND** the next subsequent connection by a client SHALL NOT receive the
  previously dropped `at_mentioned`

