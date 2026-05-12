# protocol Specification Delta — session-routing

## MODIFIED Requirements

### Requirement: IPC frame envelope shape

The crate SHALL define an `IpcFrame` enum that serializes with a
`type` discriminator (i.e.
`#[serde(tag = "type", rename_all = "snake_case")]`). The permitted
`type` values SHALL be exactly: `selection`, `at_mention`,
`workspace_folders`, `open_editors`, `ping`, `ack`, `log`, and
`ambiguous`.

For each frame, the JSON-level field set SHALL be:

- `selection`: `{type, file_path, line_start, line_end, text}`
- `at_mention`: `{type, file_path, line_start, line_end}` plus the
  optional fields `workspace_root` (string) and `client_id`
  (string). Both optional fields SHALL be serialized with
  `#[serde(default, skip_serializing_if = "Option::is_none")]`,
  i.e. omitted entirely when `None`. When present, `workspace_root`
  SHALL be a JSON string carrying an absolute filesystem path;
  `client_id` SHALL be the lowercase 36-character hyphenated form of
  a UUID v4 (the wire representation used by `uuid::Uuid::to_string`).
- `workspace_folders`: `{type, folders}` (folders is an array of
  strings)
- `open_editors`: `{type, editors}` (editors is an array of objects)
- `ping`: `{type}`
- `ack`: `{type}`
- `log`: `{type, level, message}`
- `ambiguous`: `{type, candidates}` where `candidates` is a JSON
  array of `AmbiguousCandidate` objects (see the **AmbiguousCandidate
  shape** requirement below). This variant is emitted only by the
  sidecar in reply to an ambiguous at-mention; helpers SHALL NOT
  send it.

#### Scenario: at_mention frame round-trips with exact field set (no optional fields)

- **WHEN** parsing
  `{"type":"at_mention","file_path":"/p/x.rs","line_start":3,"line_end":4}` and
  re-serializing
- **THEN** the resulting JSON SHALL have key set
  `{"type","file_path","line_start","line_end"}`
- **AND** `"type":"at_mention"` SHALL be preserved verbatim
- **AND** the keys `"workspace_root"` and `"client_id"` SHALL be
  absent

#### Scenario: at_mention frame with workspace_root round-trips

- **WHEN** parsing
  `{"type":"at_mention","file_path":"/p/x.rs","line_start":3,"line_end":4,"workspace_root":"/p"}`
  and re-serializing
- **THEN** the resulting JSON SHALL have key set
  `{"type","file_path","line_start","line_end","workspace_root"}`
- **AND** the `workspace_root` value SHALL equal the literal string
  `"/p"`

#### Scenario: at_mention frame with client_id round-trips

- **WHEN** parsing
  `{"type":"at_mention","file_path":"/p/x.rs","line_start":0,"line_end":0,"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479"}`
  and re-serializing
- **THEN** the resulting JSON SHALL have key set
  `{"type","file_path","line_start","line_end","client_id"}`
- **AND** the `client_id` value SHALL equal the literal string
  `"f47ac10b-58cc-4372-a567-0e02b2c3d479"`

#### Scenario: ambiguous frame round-trips

- **WHEN** parsing
  `{"type":"ambiguous","candidates":[{"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","label":"Session 1 — connected 2m ago","connected_at_ms_ago":120000,"last_activity_ms_ago":3000}]}`
  and re-serializing
- **THEN** the resulting JSON SHALL parse back into an `IpcFrame::Ambiguous`
  variant with exactly one candidate
- **AND** the candidate's `client_id` SHALL equal `"f47ac10b-58cc-4372-a567-0e02b2c3d479"`

#### Scenario: snake_case discriminator values

- **WHEN** a `WorkspaceFolders` variant is serialized
- **THEN** the JSON SHALL contain `"type":"workspace_folders"` (snake_case)

#### Scenario: Unknown discriminator does not panic

- **WHEN** parsing `{"type":"made_up_kind","x":1}` as an `IpcFrame`
- **THEN** parsing SHALL return a typed error (or a designated `Unknown` variant)
- **AND** SHALL NOT panic

## ADDED Requirements

### Requirement: AmbiguousCandidate shape

The crate SHALL define an `AmbiguousCandidate` type whose serialized
JSON object contains exactly the keys `client_id` (string),
`label` (string), `connected_at_ms_ago` (non-negative integer), and
`last_activity_ms_ago` (non-negative integer).

`client_id` SHALL be the lowercase 36-character hyphenated UUID v4
form. `label` SHALL be a non-empty UTF-8 string suitable for display
in a list dialog. `connected_at_ms_ago` and `last_activity_ms_ago`
SHALL be the elapsed time in milliseconds since the relevant
sidecar-side `Instant` (computed at the moment the sidecar serializes
the `Ambiguous` reply).

#### Scenario: AmbiguousCandidate round-trips with exact field set

- **WHEN** parsing
  `{"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","label":"Session 1","connected_at_ms_ago":3000,"last_activity_ms_ago":1000}`
  as an `AmbiguousCandidate` and re-serializing
- **THEN** the resulting JSON object's key set SHALL equal
  `{"client_id","label","connected_at_ms_ago","last_activity_ms_ago"}`
- **AND** no other keys SHALL be present

#### Scenario: AmbiguousCandidate rejects negative durations

- **WHEN** parsing
  `{"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","label":"x","connected_at_ms_ago":-1,"last_activity_ms_ago":0}`
  as an `AmbiguousCandidate`
- **THEN** parsing SHALL return a typed error (the field type is an
  unsigned integer)

### Requirement: WebSocket workspace request header

The sidecar SHALL accept one optional HTTP request header on the
WebSocket upgrade, in addition to the existing
`x-claude-code-ide-authorization` header:

- `x-claude-code-workspace`: the absolute filesystem path of the
  Claude session's working directory. When present and non-empty,
  the sidecar SHALL canonicalize the value and use it as the
  client's `workspace_root` (see the `websocket` capability's
  **Workspace identification on connect** requirement).

The header SHALL be optional. Its absence SHALL NOT affect
authentication or alter the existing behaviour for clients that do
not supply it.

#### Scenario: Workspace header parsed and stored

- **WHEN** the WebSocket upgrade carries
  `x-claude-code-workspace: /Users/me/proj`
- **THEN** the resulting client registry entry's `workspace_root`
  SHALL be `Some(canonical("/Users/me/proj"))`

#### Scenario: Missing header does not break the handshake

- **WHEN** a WebSocket upgrade carries the auth header but no
  `x-claude-code-workspace` header
- **THEN** the handshake SHALL succeed
- **AND** the registry entry's `workspace_root` SHALL fall back to
  the documented defaults

### Requirement: `ipc-send-at-mention` CLI accepts workspace and client_id

The `ipc-send-at-mention` CLI helper SHALL accept two new optional
arguments:

- `--workspace-root <PATH>`: forwarded into the `at_mention` IPC
  frame's `workspace_root` field. Distinct from the existing
  `--workspace <DIR>` flag, which names the IPC socket scope.
- `--client-id <UUID>`: forwarded into the `at_mention` IPC frame's
  `client_id` field. Used internally by the helper on the second
  leg of a picker round-trip; end-users typically do not set it
  directly.

Both arguments SHALL be optional. When omitted, the corresponding
field SHALL be `None` in the emitted IPC frame.

#### Scenario: --workspace-root populates the frame field

- **WHEN** the user runs
  `zed-claude-bridge ipc-send-at-mention --workspace /tmp/ws
  --workspace-root /Users/me/proj --file-path /Users/me/proj/x.rs
  --cursor-row 5`
- **THEN** the IPC frame written to the socket SHALL parse as
  `IpcFrame::AtMention` with `workspace_root == Some(PathBuf::from("/Users/me/proj"))`

#### Scenario: --client-id populates the frame field

- **WHEN** the user runs
  `zed-claude-bridge ipc-send-at-mention --workspace /tmp/ws
  --client-id f47ac10b-58cc-4372-a567-0e02b2c3d479
  --file-path /x.rs --cursor-row 1`
- **THEN** the IPC frame written to the socket SHALL parse as
  `IpcFrame::AtMention` with `client_id == Some(Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479"))`

#### Scenario: --client-id rejects malformed UUIDs

- **WHEN** the user runs
  `zed-claude-bridge ipc-send-at-mention --workspace /tmp/ws
  --client-id not-a-uuid --file-path /x.rs --cursor-row 1`
- **THEN** the CLI SHALL exit non-zero with a typed parse error
- **AND** SHALL NOT write any IPC frame

#### Scenario: Omitting both yields None for both

- **WHEN** the user runs
  `zed-claude-bridge ipc-send-at-mention --workspace /tmp/ws
  --file-path /x.rs --cursor-row 1`
- **THEN** the IPC frame SHALL parse with both `workspace_root` and
  `client_id` equal to `None`

### Requirement: `ipc-send-at-mention` picker round-trip behaviour

The `ipc-send-at-mention` helper SHALL keep its IPC connection open
after writing the first `at_mention` frame, and SHALL read at most
one reply line from the connection before deciding whether to write
a follow-up frame. Behaviour:

- If the reply is parsed as an `IpcFrame::Ambiguous { candidates }`,
  the helper SHALL on **macOS** invoke `osascript -e 'choose from
  list {...} with prompt "..."'` synchronously, where the list items
  are the candidates' `label` strings in the order received. The
  helper SHALL then write a second `at_mention` frame on the same
  IPC connection carrying the picked candidate's `client_id`. If
  the user cancels the dialog, the helper SHALL close the connection
  without writing a follow-up and SHALL exit with status 0.
- If the reply is parsed as an `IpcFrame::Ambiguous { candidates }`
  on **non-macOS platforms**, the helper SHALL log a WARN to stderr
  indicating no platform picker is available and SHALL write a
  follow-up `at_mention` frame whose `client_id` matches the
  candidate with the smallest `last_activity_ms_ago` value
  (i.e. the most recently active session). Picker support for those
  platforms is a documented follow-up.
- If the IPC reply is anything else (or the peer closes without a
  reply within an implementation-defined short timeout, e.g. 500 ms),
  the helper SHALL exit with status 0 (the sidecar already either
  routed successfully or dropped the at-mention; the helper's job
  is done).

#### Scenario: macOS picker round-trip resolves to a follow-up frame

- **GIVEN** `ipc-send-at-mention` is running on macOS and has just
  read an `ambiguous` reply with two candidates
- **WHEN** the user picks the candidate labelled `"Session 2 — connected 30s ago"`
- **THEN** the helper SHALL write a single follow-up line on the
  same IPC connection parseable as an `IpcFrame::AtMention` whose
  `client_id` equals that candidate's `client_id`

#### Scenario: macOS picker cancellation sends no follow-up

- **GIVEN** `ipc-send-at-mention` is running on macOS and has just
  read an `ambiguous` reply
- **WHEN** the user dismisses the `osascript` dialog (clicking
  Cancel)
- **THEN** the helper SHALL NOT write a follow-up IPC frame
- **AND** SHALL exit with status 0

#### Scenario: Non-macOS fallback writes follow-up with most-recently-active candidate

- **GIVEN** `ipc-send-at-mention` is running on Linux and has just
  read an `ambiguous` reply with candidates A (`last_activity_ms_ago = 60000`)
  and B (`last_activity_ms_ago = 1000`)
- **WHEN** the helper processes the reply
- **THEN** the helper SHALL log a WARN to stderr indicating no
  platform picker is available
- **AND** SHALL write a follow-up `at_mention` whose `client_id`
  equals B's `client_id`
