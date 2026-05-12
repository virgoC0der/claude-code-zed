# websocket Specification Delta — peer-cwd-discovery

## MODIFIED Requirements

### Requirement: Workspace identification on connect

For every authorized client the sidecar SHALL attempt to resolve a
`workspace_root` value at WebSocket-accept time using the following
ordered sources, taking the first that produces a non-empty value:

1. The `x-claude-code-workspace` HTTP request header on the
   WebSocket upgrade. If present and non-empty, the value SHALL be
   canonicalized via `std::fs::canonicalize`; on canonicalization
   failure the raw value SHALL be kept and a DEBUG log emitted.
2. **Peer-process cwd discovery.** After the auth check has passed
   AND no value was supplied by priority 1, the sidecar SHALL invoke
   the configured `CwdResolver` (see the **Peer-process cwd discovery**
   requirement below) with the accepted TCP socket's peer `SocketAddr`.
   If the resolver returns `Some(path)`, the value SHALL be
   canonicalized via `std::fs::canonicalize`; on canonicalization
   failure the raw value SHALL be kept and a DEBUG log emitted.
3. A `cwd` string field inside the `clientInfo` object of the MCP
   `initialize` request's `params`, captured after the request is
   parsed in the transport layer and propagated to the registry
   entry by id.
4. The sidecar's own `--workspace` flag value as a last-resort
   default.

If none of (1)–(4) produce a value, `workspace_root` SHALL remain
`None`. The capture SHALL NOT block the accept loop on file-system
I/O for more than a single `canonicalize` call per client plus the
resolver's bounded budget per the **Peer-process cwd discovery**
requirement.

The sidecar's INFO log line emitted when a client is registered
SHALL carry a `workspace_source` field whose value is exactly one of
`"header"`, `"peer-cwd-libproc"`, `"client-info-cwd"`,
`"daemon-workspace"`, or `"pending-initialize"` — corresponding to
priorities 1, 2, 3, 4, or "still None at registry-insert time"
respectively.

#### Scenario: Workspace header takes priority over peer-cwd

- **GIVEN** a client sends an `x-claude-code-workspace: /a/proj`
  header AND the configured resolver would have returned `Some(/b/proj)`
  for that client's peer address
- **WHEN** the connection is registered
- **THEN** the registry entry's `workspace_root` SHALL be the
  canonical form of `/a/proj`
- **AND** the INFO log's `workspace_source` SHALL equal `"header"`

#### Scenario: Peer-cwd applied when header is absent

- **GIVEN** a client connects with no `x-claude-code-workspace`
  header AND the configured resolver returns `Some(/Users/me/proj)`
  for that client's peer address
- **WHEN** the connection is registered
- **THEN** the registry entry's `workspace_root` SHALL be the
  canonical form of `/Users/me/proj`
- **AND** the INFO log's `workspace_source` SHALL equal
  `"peer-cwd-libproc"`

#### Scenario: clientInfo.cwd used when header and peer-cwd both miss

- **GIVEN** a client connects with no workspace header
- **AND** the configured resolver returns `None`
- **AND** the client sends `initialize` with `clientInfo.cwd = "/Users/me/proj"`
- **WHEN** the `initialize` is processed
- **THEN** the registry entry's `workspace_root` SHALL be the
  canonical form of `/Users/me/proj`
- **AND** the DEBUG log's `workspace_source` SHALL equal
  `"client-info-cwd"`

#### Scenario: Defaults to --workspace when no client-side and no peer-cwd signal

- **GIVEN** the sidecar was launched with `--workspace /Users/me/p`
- **AND** a client connects with no workspace header
- **AND** the configured resolver returns `None`
- **AND** the client sends `initialize` with no `clientInfo.cwd`
- **WHEN** the connection is registered
- **THEN** the registry entry's `workspace_root` SHALL be the
  canonical form of `/Users/me/p`
- **AND** the DEBUG log's `workspace_source` SHALL equal
  `"daemon-workspace"`

## ADDED Requirements

### Requirement: Peer-process cwd discovery

The sidecar SHALL provide a `CwdResolver` abstraction whose
`resolve(peer: SocketAddr) -> Option<PathBuf>` method returns the
current working directory of the OS process that owns the peer end
of an accepted TCP loopback socket, when discoverable.

The sidecar SHALL ship two production implementations and one test
double:

- `LibprocCwdResolver` SHALL be the default `CwdResolver` on
  `target_os = "macos"`. It SHALL use the macOS `libproc.dylib`
  surface (via the `libproc` crate) to enumerate the per-process
  file-descriptor tables, find the PID whose socket fd-table contains
  the peer's ephemeral port, and read that PID's cwd via
  `proc_pidinfo(_, PROC_PIDVNODEPATHINFO, _, _)`. The implementation
  SHALL invoke `libproc` inside `tokio::task::spawn_blocking` so the
  WebSocket accept loop is not stalled by synchronous syscalls.
- `NoopCwdResolver` SHALL be the default `CwdResolver` on every other
  `target_os`. Its `resolve` SHALL return `None` for every input.
- A `MockCwdResolver` (test-only or `#[cfg(test)]`-gated) SHALL be
  provided whose `resolve` consults an injected
  `HashMap<u16, PathBuf>` keyed on the peer port. Returns `Some(_)`
  iff the port is mapped; `None` otherwise.

The resolver SHALL be injected into `Transport` via a builder method
`Transport::builder(...).with_cwd_resolver(Arc<dyn CwdResolver>)`.
The crate SHALL expose a `default_cwd_resolver()` constructor that
returns the platform-appropriate default behind an
`Arc<dyn CwdResolver>`.

Resolver failures (process gone, permission denied, empty cwd,
platform unsupported) SHALL NOT panic, SHALL NOT propagate as an
error, and SHALL NOT close the WebSocket connection. They SHALL
return `Option::None` so the workspace-identification chain falls
through to the next priority.

The sidecar's code base SHALL NOT contain any `unsafe` block
introduced by this requirement. The `libproc` crate may use `unsafe`
internally to call macOS APIs; that is an external-crate concern and
out of scope of the project's `unsafe`-forbidden rule.

#### Scenario: macOS resolver returns the Claude CLI's cwd

- **GIVEN** the sidecar is running on macOS with the default
  `LibprocCwdResolver`
- **AND** a Claude CLI process owned by the same UID is connected to
  the sidecar from peer address `127.0.0.1:<port>`
- **AND** that Claude CLI's cwd is `/Users/me/proj`
- **WHEN** the sidecar calls `resolver.resolve(127.0.0.1:<port>)`
- **THEN** the result SHALL be `Some(PathBuf::from("/Users/me/proj"))`
  (or its canonical equivalent)

#### Scenario: Non-macOS Noop resolver always returns None

- **GIVEN** the sidecar is running on a non-macOS target with the
  default `NoopCwdResolver`
- **WHEN** the sidecar calls `resolver.resolve(any_peer_addr)`
- **THEN** the result SHALL be `None`

#### Scenario: Mock resolver scripted by tests

- **GIVEN** a test builds a `MockCwdResolver` containing the mapping
  `42321 -> /tmp/ws-a`
- **WHEN** the test calls `resolver.resolve(127.0.0.1:42321)`
- **THEN** the result SHALL be `Some(PathBuf::from("/tmp/ws-a"))`
- **AND** when called with `127.0.0.1:99999` (an unmapped port) the
  result SHALL be `None`

#### Scenario: Resolver failure falls through to next priority

- **GIVEN** a client connects with no `x-claude-code-workspace`
  header AND the resolver's `resolve` returns `None`
- **AND** the client subsequently sends `initialize` with
  `clientInfo.cwd = "/Users/me/proj"`
- **WHEN** the connection is registered and the `initialize` is
  processed
- **THEN** the registry entry's `workspace_root` SHALL be the
  canonical form of `/Users/me/proj`
- **AND** the WebSocket connection SHALL remain healthy

#### Scenario: Resolver does not stall the accept loop

- **GIVEN** the sidecar is running with the default
  `LibprocCwdResolver`
- **WHEN** a second TCP connection is accepted while the resolver is
  still processing the first one
- **THEN** the second connection SHALL begin its WebSocket upgrade
  handshake without waiting for the first resolver call to complete
