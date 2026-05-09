# lockfile Specification

## Purpose
TBD - created by archiving change zed-claude-bridge. Update Purpose after archive.
## Requirements
### Requirement: Lock-file location and naming

The sidecar SHALL publish a discovery file at `~/.claude/ide/<port>.lock`, where
`<port>` is the integer TCP port (decimal, no padding) on which the WebSocket server
is currently listening.

#### Scenario: Lock file path matches the listening port

- **WHEN** the sidecar successfully binds the WebSocket listener to port `54321` on
  `127.0.0.1`
- **THEN** a regular file SHALL exist at `~/.claude/ide/54321.lock` with the JSON
  payload described in `Requirement: Lock-file JSON payload`

#### Scenario: Lock-file directory is created on first run

- **WHEN** the sidecar starts and `~/.claude/ide/` does not exist
- **THEN** the sidecar SHALL create the directory with mode `0o700`
- **AND** then write the lock file inside it

### Requirement: Lock-file permissions

The sidecar SHALL ensure the lock-file's parent directory has POSIX mode `0o700` and
the lock file itself has POSIX mode `0o600`. Permissions MUST be verified after
every write.

#### Scenario: Newly created lock file has 0600 permissions

- **WHEN** the sidecar writes a fresh lock file
- **THEN** the file's POSIX mode bits SHALL be exactly `0o600`

#### Scenario: Parent directory has 0700 permissions

- **WHEN** the lock-file directory exists after sidecar startup
- **THEN** the directory's POSIX mode bits SHALL be exactly `0o700`

### Requirement: Lock-file JSON payload

The lock-file body SHALL be a single JSON object containing exactly the fields `pid`
(unsigned integer), `workspaceFolders` (array of absolute paths), `ideName` (string),
`transport` (string), `runningInWindows` (boolean), and `authToken` (string). The
sidecar SHALL set `ideName = "Zed"`, `transport = "ws"`, `runningInWindows = false`
on macOS/Linux, `pid` to its own process id, and `authToken` to the per-launch UUID
v4 the WebSocket server validates.

#### Scenario: Required fields present and well-typed

- **WHEN** a fresh lock file is written
- **THEN** parsing it as JSON SHALL succeed
- **AND** the result SHALL contain `pid`, `workspaceFolders`, `ideName`,
  `transport`, `runningInWindows`, and `authToken` with the types listed above

#### Scenario: ideName and transport have fixed values

- **WHEN** any lock file written by the sidecar is parsed
- **THEN** `ideName` SHALL equal `"Zed"`
- **AND** `transport` SHALL equal `"ws"`

#### Scenario: authToken matches the WebSocket gate

- **WHEN** a client connects with header `x-claude-code-ide-authorization` set to
  the `authToken` value read from the lock file
- **THEN** the WebSocket server SHALL accept the connection

### Requirement: Atomic write

The sidecar SHALL write the lock file atomically: it SHALL write the JSON to a
sibling temporary file with mode `0o600`, fsync, then `rename(2)` over the final
path.

#### Scenario: Concurrent reader never sees a partial file

- **WHEN** another process repeatedly opens and parses `~/.claude/ide/<port>.lock`
  while the sidecar rewrites it on workspace-folder change
- **THEN** every successful open SHALL yield either the previous valid JSON or the
  new valid JSON, never a partially written buffer

### Requirement: Workspace-folder updates rewrite the lock file

The sidecar SHALL rewrite the existing lock file in-place when its workspace folders
change (delivered over IPC as a `workspace_folders` frame), preserving the same
`port` and `authToken` and only updating the `workspaceFolders` field.

#### Scenario: Workspace change updates lock file but not port

- **GIVEN** the sidecar is running with port `54321` and lock file `54321.lock`
- **WHEN** the extension sends an IPC frame `{"type":"workspace_folders","folders":["/x"]}`
- **THEN** `~/.claude/ide/54321.lock` SHALL be rewritten with `workspaceFolders =
  ["/x"]`
- **AND** no new lock file at any other port SHALL be created
- **AND** `authToken` SHALL be unchanged

### Requirement: Stale-lock cleanup on startup

On startup, the sidecar SHALL scan `~/.claude/ide/*.lock`. For each entry, the
sidecar SHALL attempt a TCP connect to `127.0.0.1:<port>` (where `<port>` is parsed
from the filename). If the connect is refused (process gone), the sidecar SHALL
unlink the file. The sidecar MUST NOT delete lock files belonging to live processes.

#### Scenario: Dead lock file pruned

- **GIVEN** `~/.claude/ide/40000.lock` exists but no process is listening on
  `127.0.0.1:40000`
- **WHEN** the sidecar starts
- **THEN** `~/.claude/ide/40000.lock` SHALL be removed before the new sidecar writes
  its own lock file

#### Scenario: Live peer lock file preserved

- **GIVEN** `~/.claude/ide/40001.lock` exists and another sidecar process is
  actively listening on `127.0.0.1:40001`
- **WHEN** a second sidecar starts
- **THEN** `~/.claude/ide/40001.lock` SHALL remain on disk
- **AND** the new sidecar SHALL bind a different port and write its own lock file

### Requirement: Graceful-shutdown removal

On graceful shutdown (SIGINT, SIGTERM, or SIGHUP), the sidecar SHALL unlink its own
lock file before exit.

#### Scenario: SIGTERM removes lock file

- **GIVEN** the sidecar is running with lock file `~/.claude/ide/<port>.lock`
- **WHEN** the sidecar receives SIGTERM and exits with status 0
- **THEN** `~/.claude/ide/<port>.lock` SHALL no longer exist

