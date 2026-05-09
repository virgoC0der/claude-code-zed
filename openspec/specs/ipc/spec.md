# ipc Specification

## Purpose
TBD - created by archiving change zed-claude-bridge. Update Purpose after archive.
## Requirements
### Requirement: Unix-socket location

The sidecar SHALL host a Unix-domain-socket listener at
`$TMPDIR/zed-claude-bridge-<workspace-hash>.sock`, where `<workspace-hash>` is the
lowercase hexadecimal `xxh3_64` digest of the canonicalized absolute path of the
workspace root. If `$TMPDIR` is unset, `/tmp` SHALL be used.

#### Scenario: Socket path matches the documented format

- **GIVEN** the sidecar was started with `--workspace /Users/me/proj`
- **WHEN** the sidecar has finished startup
- **THEN** the file at `$TMPDIR/zed-claude-bridge-<hash>.sock` SHALL exist
- **AND** the file SHALL be a Unix-domain socket (S_IFSOCK)
- **AND** `<hash>` SHALL equal the lowercase hex `xxh3_64` of
  `/Users/me/proj` (after canonicalization)

#### Scenario: Stale socket file is removed on bind

- **GIVEN** a stale (non-listening) socket file exists at the target path
- **WHEN** the sidecar starts
- **THEN** the sidecar SHALL unlink the stale file before binding the new listener

### Requirement: Line-delimited JSON frames

IPC frames SHALL be encoded as UTF-8 JSON objects, one frame per line, terminated by
a single `\n` (`0x0A`) byte. Lines longer than 1 MiB SHALL be rejected with an
error log and the connection SHALL be closed.

#### Scenario: Frames separated by newline are parsed independently

- **WHEN** the extension writes
  `{"type":"ping"}\n{"type":"ping"}\n` to the socket in a single `write()`
- **THEN** the sidecar SHALL process both frames as separate `ping` messages

#### Scenario: Oversized line closes the connection

- **WHEN** the extension writes a single line of `2_000_000` bytes (no `\n`)
- **THEN** the sidecar SHALL log an ERROR
- **AND** SHALL close the IPC connection without affecting the WebSocket server or
  other IPC clients

### Requirement: Supported IPC message types

The sidecar SHALL accept IPC frames whose `type` field is one of `selection`,
`at_mention`, `workspace_folders`, `open_editors`, or `ping`. Unknown `type` values
SHALL be logged at WARN level and otherwise ignored.

#### Scenario: selection frame updates EditorState

- **WHEN** the extension sends
  `{"type":"selection","filePath":"/p/a.rs","lineStart":3,"lineEnd":4,"text":"x"}`
- **THEN** subsequent `getCurrentSelection` calls SHALL reflect this state per
  `mcp` spec

#### Scenario: at_mention frame triggers notification

- **WHEN** the extension sends
  `{"type":"at_mention","filePath":"/p/a.rs","lineStart":3,"lineEnd":4}`
- **THEN** the sidecar SHALL produce the corresponding `at_mentioned` JSON-RPC
  notification per `notifications` spec

#### Scenario: Unknown type ignored

- **WHEN** the extension sends `{"type":"unknown","foo":"bar"}`
- **THEN** the sidecar SHALL log at WARN
- **AND** SHALL NOT close the IPC connection
- **AND** SHALL continue processing subsequent frames

### Requirement: Replies and diagnostics

The sidecar MAY send `{"type":"ack"}` in response to any frame, and SHALL respond to
`{"type":"ping"}` with `{"type":"ack"}`. The sidecar MAY also emit
`{"type":"log","level":"<level>","message":"<text>"}` frames for diagnostics. No
response is required for `selection`, `at_mention`, `workspace_folders`, or
`open_editors`.

#### Scenario: ping is acknowledged

- **WHEN** the extension sends `{"type":"ping"}\n`
- **THEN** the sidecar SHALL write `{"type":"ack"}\n` back on the same connection
  within 50 ms

### Requirement: Multiple concurrent IPC clients

The sidecar SHALL accept multiple concurrent IPC connections on the same socket and
SHALL apply each frame to a single shared `EditorState` under appropriate
synchronization. There is no per-connection isolation.

#### Scenario: Two extensions connected simultaneously

- **GIVEN** two IPC clients A and B are both connected
- **WHEN** A sends a `selection` frame for `/p/a.rs`
- **AND** then B sends a `selection` frame for `/p/b.rs`
- **THEN** the sidecar's last-known selection SHALL describe `/p/b.rs` (last writer
  wins)

### Requirement: Robust to client disconnects

When an IPC client disconnects (clean close or transport error), the sidecar SHALL
NOT exit and SHALL continue accepting new IPC connections.

#### Scenario: IPC client crash leaves sidecar healthy

- **GIVEN** an IPC client is connected
- **WHEN** the IPC client process is killed with SIGKILL
- **THEN** the sidecar SHALL still be running 1 second later
- **AND** a fresh IPC connection SHALL succeed

