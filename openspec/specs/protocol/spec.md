# protocol Specification

## Purpose
TBD - created by archiving change zed-claude-bridge. Update Purpose after archive.
## Requirements
### Requirement: JSON-RPC 2.0 envelope shapes

The crate SHALL define request, response, notification, and error envelope types
that round-trip JSON-RPC 2.0 verbatim. The `jsonrpc` field SHALL serialize to the
exact string `"2.0"`. Request/response objects SHALL carry an `id` whose JSON value
is one of: a JSON string, a JSON number, or `null`. Notifications SHALL omit the
`id` field entirely (not serialize it as `null`).

#### Scenario: Request round-trip preserves jsonrpc and id

- **WHEN** the input
  `{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}` is parsed into the
  request type and re-serialized with `serde_json::to_string`
- **THEN** the output JSON SHALL contain `"jsonrpc":"2.0"` and `"id":1`
- **AND** the `method` field SHALL equal `"ping"`

#### Scenario: Notification omits id field

- **WHEN** a notification value with method `"notifications/initialized"` is
  serialized
- **THEN** the resulting JSON object SHALL NOT contain the key `"id"`

#### Scenario: Error response carries code/message

- **WHEN** the input
  `{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"Method not found"}}`
  is parsed
- **THEN** the parsed error SHALL have `code = -32601`
- **AND** `message = "Method not found"`

### Requirement: RequestId accepts string, number, and null

The `id` field type SHALL deserialize all three of the JSON-RPC-permitted shapes
(string, integer, null) and SHALL preserve the variant on re-serialization.

#### Scenario: String id round-trip

- **WHEN** parsing `{"jsonrpc":"2.0","id":"abc","method":"ping"}` and
  re-serializing
- **THEN** the output JSON SHALL contain `"id":"abc"` (a string, not a number)

#### Scenario: Numeric id round-trip

- **WHEN** parsing `{"jsonrpc":"2.0","id":42,"method":"ping"}` and re-serializing
- **THEN** the output JSON SHALL contain `"id":42` (a number, not a string)

### Requirement: Lock-file JSON shape

The crate SHALL define a `LockFile` type whose serialized JSON contains exactly the
keys `pid`, `workspaceFolders`, `ideName`, `transport`, `runningInWindows`, and
`authToken`. Field naming SHALL use `#[serde(rename_all = "camelCase")]` (or
explicit per-field renames). `pid` SHALL be an unsigned integer; `workspaceFolders`
SHALL be a JSON array of strings; `ideName`, `transport`, and `authToken` SHALL be
JSON strings; `runningInWindows` SHALL be a JSON boolean.

#### Scenario: All six keys present after serialization

- **WHEN** a `LockFile` value with `pid=1`, `workspaceFolders=["/x"]`,
  `ideName="Zed"`, `transport="ws"`, `runningInWindows=false`,
  `authToken="00000000-0000-0000-0000-000000000000"` is serialized
- **THEN** the resulting JSON object's key set SHALL equal
  `{"pid","workspaceFolders","ideName","transport","runningInWindows","authToken"}`
- **AND** no other keys SHALL be present

#### Scenario: camelCase renaming is in effect

- **WHEN** any `LockFile` value is serialized
- **THEN** the JSON SHALL contain the literal key `"workspaceFolders"`
- **AND** SHALL NOT contain `"workspace_folders"`

### Requirement: MCP initialize result shape

The crate SHALL define an `InitializeResult` type that serializes to a JSON object
with keys `protocolVersion`, `capabilities`, and `serverInfo`. `capabilities`
SHALL be an object containing key `tools`, whose value is an object with key
`listChanged` (boolean). `serverInfo` SHALL contain keys `name` and `version`
(both strings).

#### Scenario: protocolVersion field emitted in camelCase

- **WHEN** an `InitializeResult` with `protocol_version = "2024-11-05"` is
  serialized
- **THEN** the JSON SHALL contain the literal key `"protocolVersion":"2024-11-05"`

#### Scenario: tools.listChanged present and boolean

- **WHEN** an `InitializeResult` is serialized
- **THEN** the JSON path `capabilities.tools.listChanged` SHALL exist and SHALL be
  a JSON boolean

### Requirement: MCP Tool descriptor shape

The crate SHALL define a `Tool` descriptor whose serialized JSON object contains
the keys `name` (string), `description` (string), and `inputSchema` (a JSON
object). The crate SHALL define a static list of exactly four `Tool` values whose
`name` fields are `getCurrentSelection`, `getLatestSelection`, `getOpenEditors`,
and `getWorkspaceFolders` (in any order).

#### Scenario: Static tool list contains exactly the four names

- **WHEN** the static `Tool` list is serialized
- **THEN** the resulting JSON array SHALL have length 4
- **AND** the multiset of `tools[*].name` SHALL equal
  `{"getCurrentSelection","getLatestSelection","getOpenEditors","getWorkspaceFolders"}`

#### Scenario: inputSchema is a JSON object, not a string

- **WHEN** any `Tool` value is serialized
- **THEN** `inputSchema` in the JSON SHALL be a JSON object (`{...}`), not a
  JSON string

### Requirement: selection_changed and at_mentioned param shapes

The crate SHALL define types whose serialization yields:

- For `selection_changed` params: an object with keys `text` (string), `filePath`
  (string), `fileUrl` (string), and `selection` (object with `start`, `end`, and
  `isEmpty`). `start` and `end` SHALL each be objects with keys `line` and
  `character` (both integers, 0-indexed).
- For `at_mentioned` params: an object with keys `filePath` (string),
  `lineStart` (integer, **1-indexed**), and `lineEnd` (integer, **1-indexed**).

The conversion from internal 0-indexed editor coordinates to the 1-indexed
`at_mentioned` payload SHALL be performed by the `at_mentioned` constructor (the
caller passes 0-indexed inputs; the constructor adds 1).

#### Scenario: selection_changed fields are 0-indexed and camelCase

- **WHEN** a `SelectionChangedParams` representing lines 10..12 of `/p/x.rs` is
  serialized
- **THEN** the JSON SHALL contain `"filePath":"/p/x.rs"`,
  `"selection":{...,"start":{"line":10,...},"end":{"line":12,...},"isEmpty":...}`
- **AND** the JSON SHALL contain the literal key `"isEmpty"` (not `"is_empty"`)

#### Scenario: at_mentioned constructor adds 1 to convert to 1-indexed

- **WHEN** the `at_mentioned` constructor is invoked with `(file = "/p/x.rs",
  start = 9, end = 19)` (0-indexed)
- **AND** the result is serialized
- **THEN** the JSON SHALL contain `"lineStart":10` and `"lineEnd":20`

### Requirement: IPC frame envelope shape

The crate SHALL define an `IpcFrame` enum that serializes with a `type`
discriminator (i.e. `#[serde(tag = "type", rename_all = "snake_case")]`). The
permitted `type` values SHALL be exactly: `selection`, `at_mention`,
`workspace_folders`, `open_editors`, `ping`, `ack`, and `log`.

For each frame, the JSON-level field set SHALL be:

- `selection`: `{type, filePath, lineStart, lineEnd, text}`
- `at_mention`: `{type, filePath, lineStart, lineEnd}`
- `workspace_folders`: `{type, folders}` (folders is an array of strings)
- `open_editors`: `{type, editors}` (editors is an array of objects)
- `ping`: `{type}`
- `ack`: `{type}`
- `log`: `{type, level, message}`

#### Scenario: at_mention frame round-trips with exact field set

- **WHEN** parsing
  `{"type":"at_mention","filePath":"/p/x.rs","lineStart":3,"lineEnd":4}` and
  re-serializing
- **THEN** the resulting JSON SHALL have key set
  `{"type","filePath","lineStart","lineEnd"}`
- **AND** `"type":"at_mention"` SHALL be preserved verbatim

#### Scenario: snake_case discriminator values

- **WHEN** a `WorkspaceFolders` variant is serialized
- **THEN** the JSON SHALL contain `"type":"workspace_folders"` (snake_case)

#### Scenario: Unknown discriminator does not panic

- **WHEN** parsing `{"type":"made_up_kind","x":1}` as an `IpcFrame`
- **THEN** parsing SHALL return a typed error (or a designated `Unknown` variant)
- **AND** SHALL NOT panic

### Requirement: Module placement rule

All wire types described in this spec SHALL live under
`crates/zed-claude-bridge/src/protocol/` and SHALL NOT import any I/O modules
(no `std::fs`, no `std::env`, no `tokio::net`, no `tokio::io`, no
`std::os::unix::net`).

#### Scenario: protocol module has no I/O dependencies

- **WHEN** the contents of `crates/zed-claude-bridge/src/protocol/` are scanned
- **THEN** no file SHALL contain any of the substrings `std::fs`, `std::env`,
  `tokio::net`, `tokio::io`, `tokio::fs`, or `std::os::unix::net`

