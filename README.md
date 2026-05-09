# claude-code-zed

A local sidecar binary plus a Zed extension that bring Claude Code's
`/ide` integration to [Zed](https://zed.dev). When the sidecar is running
for your Zed window, the Claude Code CLI's `/ide` command will discover Zed
the same way it discovers VS Code or JetBrains, and the **Send to Claude
Code** slash command in Zed delivers `@<file>#L<a>-<b>` style at-mentions
to the active Claude Code prompt.

The on-the-wire protocol is reverse-engineered from Anthropic's official
VSCode extension (v2.1.76) and documented byte-for-byte in
[`docs/protocol.md`](docs/protocol.md). The OpenSpec change that drove this
implementation is at
[`openspec/changes/zed-claude-bridge/`](openspec/changes/zed-claude-bridge/).

## Status

- **Sidecar binary** (`zed-claude-bridge`): functional. WebSocket MCP
  server, lock-file discovery, IPC socket, signal-handled lifecycle,
  selection debounce + dedup.
- **Zed extension** (`zed-claude-code`): registers a `/send-to-claude`
  slash command; right-click context-menu entry is *not* wired (Zed's
  current `zed_extension_api` 0.7 has no editor-action hook).

## Repository layout

```
claude-code-zed/
├── crates/zed-claude-bridge/        Sidecar binary (Rust, host target)
├── extension/zed-claude-code/       Zed extension (Rust → wasm32-wasip1)
├── docs/protocol.md                 Wire-format spec (source of truth)
├── openspec/changes/                Active OpenSpec change(s)
├── scripts/smoke.sh                 Manual end-to-end smoke harness
└── .harness/                        Project conventions for AI agents
```

## Prerequisites

- Rust toolchain ≥ 1.85 (workspace edition is 2024).
- macOS or Linux. Windows path handling is out of scope for this iteration.
- For the manual smoke harness only: [`jq`](https://jqlang.org/) and
  [`websocat`](https://github.com/vi/websocat).

## Build

```bash
cargo fetch
cargo build --workspace                  # debug
cargo build --workspace --release        # optimized
```

The Zed extension is excluded from the host workspace because it targets
`wasm32-wasip1`; build it separately:

```bash
rustup target add wasm32-wasip1
cd extension/zed-claude-code
cargo build --target wasm32-wasip1 --release
```

## Run the sidecar

The sidecar is one process per Zed window/workspace. Start it pointing at
your project root:

```bash
cargo run -p zed-claude-bridge -- --workspace /path/to/your/project
```

What happens at startup:

1. Opens (or creates with mode `0o700`) the lock-file directory at
   `~/.claude/ide/` and prunes any lock files whose listening port refuses
   TCP connections.
2. Generates a per-launch UUID v4 auth token (logged at `debug` only;
   never at `info`).
3. Binds a WebSocket listener on `127.0.0.1:<random-port>` in
   `[10000, 65535]`.
4. Writes `~/.claude/ide/<port>.lock` (mode `0o600`) so the Claude Code
   CLI can find Zed.
5. Binds the IPC socket at `$TMPDIR/zed-claude-bridge-<workspace-hash>.sock`.
6. Awaits `SIGINT`/`SIGTERM`/`SIGHUP`. On any of them the lock file and
   socket are removed cleanly.

CLI flags:

| Flag           | Default            | What it does                                         |
| -------------- | ------------------ | ---------------------------------------------------- |
| `--workspace`  | *(required)*       | Workspace root; drives the IPC socket name           |
| `--foreground` | `true`             | Run in the foreground (always true in this build)    |
| `--log-level`  | `info`             | Tracing filter (also accepts full `EnvFilter` strings) |
| `--ipc-socket` | derived            | Override the IPC socket path                         |
| `--lock-dir`   | `~/.claude/ide`    | Override the lock-file directory                     |

Helper subcommands used by the Zed extension (you usually don't run these
by hand):

```bash
zed-claude-bridge ipc-send-at-mention      --workspace <ROOT> --file-path <P> --line-start <L0> --line-end <L1>
zed-claude-bridge ipc-send-workspace-folders --workspace <ROOT> --folder <P> [--folder <P> ...]
```

## Use `/ide` from Claude Code

1. Start the sidecar (above).
2. Run `claude` (the Claude Code CLI) in any directory. Inside the prompt,
   type `/ide`. The CLI will scan `~/.claude/ide/*.lock`, connect to your
   sidecar's WebSocket, and complete the MCP handshake. You should see
   "Zed" listed as a connected IDE.
3. The four read-only MCP tools become available:
   `getCurrentSelection`, `getLatestSelection`, `getOpenEditors`,
   `getWorkspaceFolders`.

## Install the Zed extension (dev mode)

1. Build the wasm artifact (see above).
2. In Zed: open the command palette → `zed: install dev extension` → pick
   the `extension/zed-claude-code/` directory.
3. The slash command `/send-to-claude` will appear in the assistant panel.
   Usage:

   ```
   /send-to-claude <file> <start-line> <end-line>
   ```

   `<file>` may be relative to the worktree root or absolute. Lines are
   **0-indexed** (matching the underlying IPC frame format). The
   extension forwards the request to the sidecar via a helper-binary
   invocation; if the sidecar isn't running it'll spawn one and retry up
   to 5 times with exponential backoff.

## Tests and verification

The full per-`.harness/project.md` verification suite:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets
cargo test --workspace
```

For a hands-on end-to-end check (requires `jq` and `websocat`):

```bash
bash scripts/smoke.sh
```

Test inventory (host workspace only):

- 72 unit tests across `protocol`, `lockfile`, `mcp`, `transport`, `ipc`,
  `app::cli`.
- 7 WebSocket integration tests in `tests/handshake.rs`.
- 14 IPC integration tests in `tests/ipc.rs`.
- 1 full-stack end-to-end test in `tests/end_to_end.rs` (extension → IPC
  → sidecar → WebSocket → MCP client, with at_mention 0 → 1-indexed
  conversion verified on the wire).
- 2 lock-file integration tests in `tests/lockfile.rs`.
- 8 unit tests inside the Zed extension crate (run from inside
  `extension/zed-claude-code/`).

## Specs

- [`docs/protocol.md`](docs/protocol.md) — reverse-engineered wire format.
- [`openspec/changes/zed-claude-bridge/specs/`](openspec/changes/zed-claude-bridge/specs/)
  — per-module Given/When/Then scenarios that drove the implementation.
- [`.harness/project.md`](.harness/project.md) — workspace conventions
  (commands, layer order, error handling, secrets policy).

## License

Dual-licensed under MIT or Apache-2.0, at your option.
