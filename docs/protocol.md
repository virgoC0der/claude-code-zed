# Claude Code IDE Bridge Protocol — Reverse-Engineered

> **Source of truth**: `~/.vscode/extensions/anthropic.claude-code-2.1.76-darwin-arm64/extension.js`
> (Anthropic VSCode extension v2.1.76, darwin-arm64, Claude Code CLI v2.1.137).
> All snippets below are quoted verbatim from that file.

This document captures everything we need to build a Zed-side compatible implementation. We will not invent or guess fields — if it isn't here, it's not implemented in the first cut.

---

## 1. Discovery: the lock file

**Location**: `~/.claude/ide/<port>.lock`

**Permissions**: parent directory `0700`, file `0600` (mode `448` and `384` in decimal — see `Z9.mkdirSync(v, {recursive:!0, mode:448})` and `Z9.writeFileSync(j, ..., {mode:384})`).

**Filename**: the integer port the WebSocket server is listening on, with `.lock` suffix. Port is chosen with `Math.floor(Math.random()*55536)+10000` (range `10000..=65535`).

**JSON shape** (extracted from `xM` function):
```json
{
  "pid": 12345,
  "workspaceFolders": ["/Users/me/Code/my-project"],
  "ideName": "Visual Studio Code",
  "transport": "ws",
  "runningInWindows": false,
  "authToken": "f47ac10b-58cc-4372-a567-0e02b2c3d479"
}
```

Field semantics:
- `pid`: `process.ppid` in the VSCode source (the parent of the renderer). For our Zed sidecar, use the sidecar's own PID — it's only used as a hint, the CLI doesn't validate.
- `workspaceFolders`: absolute paths of open workspace roots. Required for `/ide`'s "trust" prompt to display sensibly.
- `ideName`: free-form display string. Use `"Zed"`.
- `transport`: must be `"ws"` (we do not implement stdio transport).
- `runningInWindows`: boolean derived from `process.platform === "win32"`.
- `authToken`: a **per-launch random UUID v4** the client must echo in the `x-claude-code-ide-authorization` WebSocket request header.

**Lifecycle**: write the lock file as soon as the WebSocket server has bound a port. Update (overwrite) it whenever workspace folders change. Delete it on graceful shutdown.

The Claude Code CLI also reads the env var `CLAUDE_CODE_SSE_PORT` if set — in VSCode, the extension calls `Al(z, "CLAUDE_CODE_SSE_PORT", String(K))` to inject it into terminals it spawns. For our case, we will scan the lock-file directory; setting the env var is optional polish.

---

## 2. WebSocket transport

**URL**: `ws://127.0.0.1:<port>` — IPv4 loopback only. Do not bind `0.0.0.0` or `::`.

**Auth**: every incoming connection must carry header `x-claude-code-ide-authorization: <authToken>` matching the lock file. From extension.js:
```js
q.on("connection", function(W, M){
  if (M.headers["x-claude-code-ide-authorization"] !== Z) {
    j.error("Unauthorized WebSocket connection attempt");
    W.close(1008, "Unauthorized");
    return;
  }
  ...
});
```
Reject with WS close code `1008`.

**Single-client policy**: VSCode disconnects the previous client when a new one connects ("Disconnecting previous WebSocket client"). We will mirror that.

**Frame format**: text frames carrying JSON-RPC 2.0. One JSON object per frame.

---

## 3. JSON-RPC roles

The IDE (us) is the **MCP server**. The Claude Code CLI is the **MCP client**.

Both sides also send unsolicited notifications — for our use case, the IDE sends `selection_changed` and `at_mentioned` to the CLI.

### 3.1 Standard MCP requests handled by the IDE

`tools/list` → returns the list of MCP tools the IDE exposes.

`tools/call` with `params: { name, arguments }` → dispatches to the tool implementation.

Other MCP plumbing the CLI may send:
- `initialize` (handshake) — respond with server capabilities.
- `ping` — respond with empty result.
- `resources/*`, `prompts/*` — we do NOT advertise these, so the CLI shouldn't call them; if it does, return JSON-RPC error `-32601` (Method not found).

### 3.2 Tools exposed by the IDE

From a `tool(...)` registration block in extension.js:

| Tool name | Args | Returns | First-cut scope |
|-----------|------|---------|-----------------|
| `getCurrentSelection` | `{}` | `{success, text, filePath, fileUrl, selection: {start, end, isEmpty}}` | **YES** |
| `getLatestSelection` | `{}` | same shape (last seen, even if editor not focused) | **YES** |
| `getOpenEditors` | `{}` | array of `{uri, isActive, isPinned, isPreview, isDirty?, languageId?}` | **YES** |
| `getWorkspaceFolders` | `{}` | `{success, folders: [{name, uri, path, index}], rootPath, workspaceFile}` | **YES** |
| `checkDocumentDirty` | `{filePath}` | `{success, isDirty}` | optional |
| `saveDocument` | `{filePath}` | `{success}` | optional |
| `openFile` | `{filePath, preview?, startText?, endText?, selectToEndOfLine?, makeFrontmost?}` | `{success, filePath, fileUrl, ...}` | optional |
| `openDiff` | `{originalFilePath, newFilePath, edits, supportMultiEdits}` | diff UI events | NO (later) |
| `close_tab` | `{...}` | tab close result | NO (later) |
| `closeAllDiffTabs` | `{}` | result | NO (later) |
| `getDiagnostics` | `{uri?}` | array of `{uri, linesInFile, diagnostics:[...]}` | NO (later) |
| `executeCode` | `{code}` (Jupyter) | execution result | NO (later) |

### 3.3 Notifications sent by the IDE to the CLI

**`selection_changed`** (debounced 300 ms; only fired when text/range changes):
```json
{
  "jsonrpc": "2.0",
  "method": "selection_changed",
  "params": {
    "text": "fn main() { ... }",
    "filePath": "/Users/me/proj/src/main.rs",
    "fileUrl": "file:///Users/me/proj/src/main.rs",
    "selection": {
      "start": {"line": 10, "character": 0},
      "end":   {"line": 12, "character": 1},
      "isEmpty": false
    }
  }
}
```
Skip when `document.uri.scheme` is `"comment"` or `"output"`.

**`at_mentioned`** (one-shot, fired by an explicit user action like "Insert at-mention" / "Send to Claude Code"):
```json
{
  "jsonrpc": "2.0",
  "method": "at_mentioned",
  "params": {
    "filePath": "/relative/or/abs/path.rs",
    "lineStart": 10,
    "lineEnd": 20
  }
}
```

NB: in the VSCode terminal-mode flow (`claude-vscode.insertAtMention`), the extension constructs the at-mention text **client-side** as `@<relativePath>#<startLine>-<endLine>` and feeds it to the terminal directly — it does not rely on a JSON-RPC roundtrip:
```js
let V = T0.workspace.asRelativePath(K.fileName);
let q = N.start.line + 1, B = N.end.line + 1;
let x = q !== B ? `@${V}#L${q}-${B}` : `@${V}#L${q}`;
```
For Claude Code's `useTerminal=true` mode, the bridge fires the `at_mentioned` JSON-RPC notification, and the CLI side renders `@path#L10-20` into the prompt. Both code paths exist in the bundle. We will implement the JSON-RPC notification path.

Lines are **1-indexed** in the at-mention string format (`+1` adjustment in extension.js). Selection ranges in `selection_changed` payloads are **0-indexed** (raw VSCode positions).

---

## 4. Handshake / initialize

Standard MCP `initialize` request from the CLI. Minimal valid response:
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "2024-11-05",
    "capabilities": {
      "tools": {"listChanged": false}
    },
    "serverInfo": {
      "name": "zed-claude-bridge",
      "version": "0.1.0"
    }
  }
}
```

Then the CLI sends `notifications/initialized` (no response expected) and `tools/list`.

---

## 5. CLI-side discovery (`/ide` command)

Verified by reading the CLI binary's `dUH` (discovery) and `wt9` (post-connect notification) functions in `/Users/sx.chen/.local/share/claude/versions/2.1.137`.

The CLI:
1. Lists `~/.claude/ide/*.lock` (also `$CLAUDE_CONFIG_DIR/.claude/ide/` if set; WSL has additional paths).
2. Parses each lock. **Marks a lock as "valid for this CLI" iff** any of its `workspaceFolders` is a path-prefix of the CLI's cwd (NFC-normalized; case-insensitive on Windows). This is `O === X || O.startsWith(X + path.sep)` where `O` is cwd, `X` is one workspace folder.
3. Then applies a **liveness check**: `Y.pid` must be a live process (`zt9(Y.pid)`); if `process.ppid !== Y.pid`, the pid must additionally be in an allow-set. Stale locks are unlinked.
4. **Env-var fast path**: if `CLAUDE_CODE_SSE_PORT=N` is set and exactly one valid lock has port `N`, use it without further selection.
5. **Auto-connect** triggers when any of: `autoConnectIde` setting true, `--ide` flag, `process.env.CLAUDE_CODE_SSE_PORT` set, `process.env.CLAUDE_CODE_AUTO_CONNECT_IDE` truthy, or running in an IDE-spawned terminal (`yG()`).
6. Connects `ws://127.0.0.1:<port>` with `x-claude-code-ide-authorization: <authToken>`.
7. Performs MCP `initialize` handshake.
8. **Immediately sends a notification** `{method:"ide_connected", params:{pid: <CLI process pid>}}`. This is the IDE's free hint about *which* CLI just connected — store it in EditorState if you want to route per-client (e.g. by `/proc/<pid>/cwd`).
9. Subscribes to server-side notifications (passive — JSON-RPC notifications are pushed, not polled).

**Implication for multi-window UX**: if every Zed window runs its own sidecar with its own lock and the **correct `workspaceFolders`**, the CLI will pick the right one automatically. We do not need an explicit selector. The single failure mode is when two workspace paths are nested — but the prefix rule resolves that deterministically (longer match wins is NOT what the CLI does; it picks the first matching lock in directory iteration order, so prefer one-sidecar-per-window).

---

## 6. Internal IPC: Zed extension ↔ sidecar

This is **our** invention, not part of Claude Code. We need it because the Zed WASM extension cannot host a TCP server.

**Transport**: Unix domain socket at `$TMPDIR/zed-claude-bridge-<workspace-hash>.sock`, where `<workspace-hash>` is `xxhash3(workspace_root_abs_path)` hex.

**Frame format**: line-delimited JSON (`\n`-terminated). Each line is one message.

**Messages from extension → sidecar**:
```json
{"type": "selection", "filePath": "...", "lineStart": 10, "lineEnd": 20, "text": "..."}
{"type": "at_mention", "filePath": "...", "lineStart": 10, "lineEnd": 20}
{"type": "workspace_folders", "folders": ["/abs/path"]}
{"type": "open_editors", "editors": [{"uri": "...", "isActive": true, ...}]}
{"type": "ping"}
```

**Messages from sidecar → extension** (mostly for diagnostics):
```json
{"type": "ack"}
{"type": "log", "level": "info", "message": "..."}
```

The sidecar maintains last-known editor state in memory and serves it when MCP `tools/call` requests come in from the CLI.

---

## 7. Edge cases & gotchas

- **Stale lock files**: on startup, scan `~/.claude/ide/*.lock`, try to connect to each — if connection fails (process gone), delete the file. Avoids the dir filling up.
- **Port conflicts**: random port in `10000..=65535`; retry on EADDRINUSE.
- **Workspace folder updates**: when Zed's workspace changes, rewrite the lock file (same port).
- **Auth token rotation**: token is generated **once per sidecar process lifetime**. Rotating mid-session would break the connected CLI client.
- **EOF handling**: when the CLI disconnects, the sidecar must NOT exit — it stays alive waiting for `/ide` to be re-run.
- **Comment / output URIs**: ignore selections from `output:`-scheme buffers.
- **Untitled buffers**: `filePath` becomes the URI (`untitled:Untitled-1`); `fileUrl` is the same. Don't try to read the file from disk.
- **Multi-cursor selections**: extension.js uses `editor.selection` (the primary selection only). Ignore secondary selections.
- **Debounce**: 300 ms for `selection_changed`. Cancel pending timer on each new event.

---

## 8. References

- VSCode extension source: `~/.vscode/extensions/anthropic.claude-code-2.1.76-darwin-arm64/extension.js`
- Lock file generator: function `xM` in extension.js (search for `xM(v,U)`)
- WebSocket auth gate: search for `x-claude-code-ide-authorization`
- Selection notification: search for `method:"selection_changed"` and `method:"at_mentioned"`
- Tool registrations: search for `x.tool("getOpenEditors"`
- MCP spec: https://modelcontextprotocol.io/specification/2024-11-05
