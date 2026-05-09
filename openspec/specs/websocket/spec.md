# websocket Specification

## Purpose
TBD - created by archiving change zed-claude-bridge. Update Purpose after archive.
## Requirements
### Requirement: Loopback-only bind

The sidecar SHALL bind its WebSocket listener exclusively on the IPv4 loopback
address `127.0.0.1`. Binding `0.0.0.0`, `::`, or any non-loopback address is
forbidden.

#### Scenario: Listener bound to 127.0.0.1

- **WHEN** the sidecar has finished startup
- **THEN** `lsof` (or equivalent) SHALL report exactly one TCP listener owned by the
  sidecar process bound to `127.0.0.1:<port>`

### Requirement: Random port in 10000..=65535

The sidecar SHALL choose its listening port uniformly at random from the inclusive
range `10000..=65535`. On `EADDRINUSE` it SHALL retry with a freshly chosen port up
to 16 times before failing.

#### Scenario: Port falls within the allowed range

- **WHEN** the sidecar reports its bound port via logs
- **THEN** the port SHALL satisfy `10000 <= port <= 65535`

#### Scenario: Bind retries on collision

- **GIVEN** another process holds port `54321` on `127.0.0.1`
- **WHEN** the sidecar's RNG selects `54321` on its first try
- **THEN** the sidecar SHALL retry with another port and SHALL eventually succeed
  within 16 attempts

### Requirement: WebSocket auth header

The sidecar SHALL accept a WebSocket upgrade request only if it carries the request
header `x-claude-code-ide-authorization` whose value byte-equals the in-memory
auth token. Header lookup SHALL be case-insensitive (HTTP semantics). The auth
token comparison SHALL be constant-time to avoid leaking length/prefix.

#### Scenario: Valid auth token accepted

- **WHEN** a WebSocket upgrade request includes
  `x-claude-code-ide-authorization: <correct token>`
- **THEN** the upgrade SHALL succeed
- **AND** the connection SHALL receive subsequent JSON-RPC messages

#### Scenario: Missing auth header rejected

- **WHEN** a WebSocket upgrade request omits the
  `x-claude-code-ide-authorization` header
- **THEN** the server SHALL close the underlying connection with WebSocket close
  code `1008`
- **AND** SHALL log the rejection at WARN level without revealing the expected token

#### Scenario: Wrong auth token rejected

- **WHEN** a WebSocket upgrade request includes
  `x-claude-code-ide-authorization: <wrong token>`
- **THEN** the server SHALL close with WebSocket close code `1008`

### Requirement: Single-client policy

The sidecar SHALL allow at most one connected WebSocket client at a time. When a new
authorized connection is accepted while a prior client is still connected, the
sidecar SHALL close the prior connection (close code `1000`, reason
`"Disconnecting previous WebSocket client"`) before promoting the new one as the
active client.

#### Scenario: Second connection displaces the first

- **GIVEN** client A is connected and authenticated
- **WHEN** client B opens a fresh connection with a valid auth header
- **THEN** client A SHALL receive a WebSocket close frame with code `1000`
- **AND** client B SHALL be the unique active client thereafter
- **AND** subsequent notifications SHALL be delivered to client B only

### Requirement: JSON-RPC text frames

The sidecar SHALL exchange JSON-RPC 2.0 messages as WebSocket text frames, one JSON
object per frame. Messages MUST conform to JSON-RPC 2.0 envelope shapes (request,
response, notification). Binary frames SHALL be ignored with a WARN log.

#### Scenario: Request → response on the same connection

- **GIVEN** an authenticated client
- **WHEN** the client sends `{"jsonrpc":"2.0","id":1,"method":"ping"}`
- **THEN** the client SHALL receive a JSON text frame `{"jsonrpc":"2.0","id":1,"result":{}}`

#### Scenario: Binary frame rejected

- **WHEN** an authenticated client sends a binary frame
- **THEN** the sidecar SHALL log a WARN
- **AND** SHALL NOT send any response for that frame

### Requirement: Sidecar survives client disconnect

The sidecar process SHALL NOT exit when the connected WebSocket client disconnects
(clean close, EOF, or transport error); it SHALL continue running and SHALL accept
future authorized connections.

#### Scenario: Disconnect does not kill sidecar

- **GIVEN** the sidecar has been running for 5 seconds with a connected client
- **WHEN** the client closes the connection cleanly
- **THEN** the sidecar process SHALL still be running 1 second later
- **AND** a fresh authorized connection SHALL succeed

