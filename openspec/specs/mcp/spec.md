# mcp Specification

## Purpose
TBD - created by archiving change zed-claude-bridge. Update Purpose after archive.
## Requirements
### Requirement: MCP initialize handshake

The sidecar SHALL respond to a JSON-RPC `initialize` request with a JSON object whose
`result` contains `protocolVersion = "2024-11-05"`, `capabilities.tools.listChanged
= false`, and `serverInfo` with `name = "zed-claude-bridge"` and `version` set to
the crate's `CARGO_PKG_VERSION`.

#### Scenario: Initialize returns protocol version 2024-11-05

- **WHEN** an authenticated client sends
  `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}`
- **THEN** the response `result.protocolVersion` SHALL equal `"2024-11-05"`
- **AND** the response `result.capabilities.tools.listChanged` SHALL equal `false`
- **AND** the response `result.serverInfo.name` SHALL equal `"zed-claude-bridge"`

### Requirement: notifications/initialized accepted

The sidecar SHALL accept a JSON-RPC notification with method
`notifications/initialized`, returning no response.

#### Scenario: Initialized notification produces no reply

- **GIVEN** the client has just received the `initialize` response
- **WHEN** the client sends
  `{"jsonrpc":"2.0","method":"notifications/initialized"}`
- **THEN** the sidecar SHALL NOT send any frame in response
- **AND** the connection SHALL remain open

### Requirement: ping returns empty result

The sidecar SHALL respond to a JSON-RPC `ping` request with `result = {}`.

#### Scenario: ping → empty result

- **WHEN** the client sends `{"jsonrpc":"2.0","id":99,"method":"ping"}`
- **THEN** the response SHALL equal `{"jsonrpc":"2.0","id":99,"result":{}}`

### Requirement: tools/list advertises four tools

The sidecar SHALL respond to `tools/list` with a list containing exactly four tool
descriptors with names `getCurrentSelection`, `getLatestSelection`, `getOpenEditors`,
and `getWorkspaceFolders`. Each descriptor MUST include `name`, `description`, and
`inputSchema` (a JSON Schema object).

#### Scenario: tools/list contains the four read-only tools

- **WHEN** the client sends `{"jsonrpc":"2.0","id":2,"method":"tools/list"}`
- **THEN** the response `result.tools` SHALL be an array of length 4
- **AND** the set of `result.tools[*].name` SHALL equal
  `{"getCurrentSelection","getLatestSelection","getOpenEditors","getWorkspaceFolders"}`

#### Scenario: out-of-scope tools are not advertised

- **WHEN** the client inspects the `tools/list` response
- **THEN** no tool with name `openDiff`, `getDiagnostics`, `executeCode`,
  `close_tab`, `closeAllDiffTabs`, `openFile`, `checkDocumentDirty`, or
  `saveDocument` SHALL appear

### Requirement: getCurrentSelection returns the focused selection

`tools/call` with `name = "getCurrentSelection"` SHALL return
`{success, text, filePath, fileUrl, selection: {start, end, isEmpty}}` reflecting
the most recent IPC `selection` frame received while the editor was focused.
`selection.start` and `selection.end` are 0-indexed editor positions
(`{line, character}`). When no current selection is known, the result SHALL be
`{success: false}`.

#### Scenario: With a known selection

- **GIVEN** the extension has sent
  `{"type":"selection","filePath":"/p/main.rs","lineStart":10,"lineEnd":12,"text":"fn x(){}"}`
- **WHEN** the client calls `tools/call` with `name = "getCurrentSelection"`
- **THEN** the response `result` SHALL have `success = true`
- **AND** `result.text` SHALL equal `"fn x(){}"`
- **AND** `result.filePath` SHALL equal `"/p/main.rs"`
- **AND** `result.selection.start.line` SHALL equal `10`
- **AND** `result.selection.end.line` SHALL equal `12`

#### Scenario: With no selection

- **GIVEN** no `selection` IPC frame has ever been received
- **WHEN** the client calls `tools/call` with `name = "getCurrentSelection"`
- **THEN** the response `result.success` SHALL equal `false`

### Requirement: getLatestSelection returns the last-seen selection

`tools/call` with `name = "getLatestSelection"` SHALL return the most recent
selection ever observed for this sidecar, regardless of focus. It SHALL persist
through focus loss; only a subsequent `selection` IPC frame replaces it.

#### Scenario: Latest selection survives focus loss

- **GIVEN** the extension previously sent a `selection` frame for `/p/a.rs#L1-2`
- **AND** the editor has since been unfocused (current selection cleared)
- **WHEN** the client calls `tools/call` with `name = "getLatestSelection"`
- **THEN** the response SHALL still describe `/p/a.rs#L1-2`

### Requirement: getOpenEditors returns last-known open editors

`tools/call` with `name = "getOpenEditors"` SHALL return an array of editor entries
each containing at minimum `uri`, `isActive`, `isPinned`, and `isPreview`, with
optional `isDirty` and `languageId`. The list SHALL reflect the most recent
`open_editors` IPC frame.

#### Scenario: Returns the IPC-supplied editor list

- **GIVEN** the extension sent
  `{"type":"open_editors","editors":[{"uri":"file:///p/a.rs","isActive":true,"isPinned":false,"isPreview":false}]}`
- **WHEN** the client calls `tools/call` with `name = "getOpenEditors"`
- **THEN** the response `result` SHALL be a single-element array whose element has
  `uri = "file:///p/a.rs"` and `isActive = true`

### Requirement: getWorkspaceFolders returns last-known folders

`tools/call` with `name = "getWorkspaceFolders"` SHALL return
`{success, folders, rootPath, workspaceFile}` where `folders` is an array of
`{name, uri, path, index}`. `rootPath` SHALL equal the first folder's `path` when at
least one folder is present, otherwise `null`. `workspaceFile` SHALL be `null` (Zed
does not have VSCode-style multi-root `.code-workspace` files).

#### Scenario: Single workspace folder

- **GIVEN** the extension sent
  `{"type":"workspace_folders","folders":["/Users/me/proj"]}`
- **WHEN** the client calls `tools/call` with `name = "getWorkspaceFolders"`
- **THEN** the response `result.success` SHALL equal `true`
- **AND** `result.folders[0].path` SHALL equal `"/Users/me/proj"`
- **AND** `result.rootPath` SHALL equal `"/Users/me/proj"`
- **AND** `result.workspaceFile` SHALL equal `null`

### Requirement: Unknown tool returns -32602

`tools/call` with a `name` that is not one of the four advertised tools SHALL return
a JSON-RPC error with `code = -32602` (Invalid params).

#### Scenario: Unknown tool name

- **WHEN** the client sends
  `{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"openDiff","arguments":{}}}`
- **THEN** the response SHALL contain `error.code = -32602`

### Requirement: Unimplemented MCP methods return -32601

The sidecar SHALL respond with JSON-RPC error `code = -32601` (Method not found) for
any MCP method other than `initialize`, `notifications/initialized`, `ping`,
`tools/list`, and `tools/call` (e.g. `resources/list`, `prompts/list`).

#### Scenario: resources/list rejected

- **WHEN** the client sends `{"jsonrpc":"2.0","id":3,"method":"resources/list"}`
- **THEN** the response SHALL contain `error.code = -32601`

