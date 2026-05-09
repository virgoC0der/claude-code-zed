# claude-code-zed

A local sidecar binary plus a project-local Zed task that bring Claude
Code's `/ide` integration to [Zed](https://zed.dev). When the sidecar is
running for your Zed window, the Claude Code CLI's `/ide` command will
discover Zed the same way it discovers VS Code or JetBrains, and a
`cmd-ctrl-c` keybinding delivers `@<file>#L<m>-<n>` style at-mentions
from the editor selection straight into the active Claude Code prompt.

The on-the-wire protocol is reverse-engineered from Anthropic's official
VSCode extension (v2.1.76) and documented byte-for-byte in
[`docs/protocol.md`](docs/protocol.md). The OpenSpec change that drove this
implementation is at
[`openspec/changes/zed-claude-bridge/`](openspec/changes/zed-claude-bridge/).

## Status

- **Sidecar binary** (`zed-claude-bridge`): functional. WebSocket MCP
  server, lock-file discovery, IPC socket, signal-handled lifecycle,
  selection debounce + dedup, and an `ipc-send-at-mention` helper that
  derives the line range from `$ZED_SELECTED_TEXT` (or falls back to
  `$ZED_ROW`).
- **At-mention trigger**: a project-local Zed *task* (`.zed/tasks.json`)
  bound to `cmd-ctrl-c` (`.zed/keymap.json`). The task receives
  `$ZED_FILE`, `$ZED_ROW`, `$ZED_SELECTED_TEXT`, and
  `$ZED_WORKTREE_ROOT` from Zed and forwards them to the sidecar over
  IPC.
- **No Zed extension.** Zed's `zed_extension_api` (≤ 0.7) exposes
  neither the editor's primary selection nor a context-menu hook to
  extensions, so an extension cannot implement a "Send selection to
  Claude Code" command on its own. Selection capture is delegated to
  Zed's built-in task system instead. An earlier iteration of this
  repo shipped a `zed-claude-code` extension as a slash-command
  scaffold; it has been removed. The deferred spec stub at
  `openspec/changes/zed-claude-bridge/specs/zed-extension/spec.md`
  remains as a reference for any future Zed API change that would let
  us revisit the extension-driven flow.

## Repository layout

```
claude-code-zed/
├── crates/zed-claude-bridge/        Sidecar binary (Rust, host target)
├── .zed/                            Project-local Zed task + keymap
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

There is only one host crate (`crates/zed-claude-bridge`); the workspace
no longer ships a WASM Zed extension. See the *Status* section above for
why.

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

Helper subcommands used by the Zed task (you usually don't run these by
hand):

```bash
# Forward an at-mention. Three ways to specify the line range, in
# priority order:
#   1. explicit:  --line-start <L0> --line-end <L1>          (0-indexed)
#   2. from text: --text <STRING>  (located inside --file-path)
#   3. cursor:    --cursor-row <N>                           (1-indexed)
zed-claude-bridge ipc-send-at-mention \
    --workspace <ROOT> --file-path <P> \
    [--text <STRING>] [--cursor-row <N>] \
    [--line-start <L0> --line-end <L1>]

zed-claude-bridge ipc-send-workspace-folders \
    --workspace <ROOT> --folder <P> [--folder <P> ...]
```

Resolution: explicit `--line-start`/`--line-end` win; otherwise `--text`
is located in the file and the matching line range is computed
(0-indexed, multi-line aware); if the text isn't found or the file
isn't readable, the helper falls back to `--cursor-row` (1-indexed,
matching Zed's `$ZED_ROW`); if no usable input is provided the helper
exits non-zero.

## Use `/ide` from Claude Code

1. Start the sidecar (above).
2. Run `claude` (the Claude Code CLI) in any directory. Inside the prompt,
   type `/ide`. The CLI will scan `~/.claude/ide/*.lock`, connect to your
   sidecar's WebSocket, and complete the MCP handshake. You should see
   "Zed" listed as a connected IDE.
3. The four read-only MCP tools become available:
   `getCurrentSelection`, `getLatestSelection`, `getOpenEditors`,
   `getWorkspaceFolders`.

## Usage: send a selection from Zed to Claude Code

The full end-to-end flow uses Zed's built-in *task* system (no extension
required for at-mentions):

1. **Install the binary so Zed can find it on `PATH`.**

   ```bash
   cargo install --path crates/zed-claude-bridge
   # or, after `cargo build --workspace --release`, copy the binary:
   #   cp target/release/zed-claude-bridge ~/.local/bin/
   ```

2. **Start the sidecar for your project workspace.**

   ```bash
   cd /path/to/your/project
   zed-claude-bridge --workspace "$(pwd)"
   ```

3. **Copy this repo's `.zed/tasks.json` and `.zed/keymap.json` into your
   project root.** If you already have one of those files, merge: append
   the `Send selection to Claude Code` entry to your `tasks.json` array
   and add the `cmd-ctrl-c` binding to your `keymap.json` array.

   ```bash
   mkdir -p .zed
   cp /path/to/claude-code-zed/.zed/tasks.json   .zed/
   cp /path/to/claude-code-zed/.zed/keymap.json  .zed/
   ```

4. **In Zed**, open any file in this project, select some text, and press
   `cmd-ctrl-c`. The task spawns silently (no terminal pop-up because
   `reveal: "never"`) and the sidecar forwards an `at_mention` IPC frame
   over the local socket.

5. **In your `claude /ide` terminal session**, the prompt receives
   `@<path>#L<m>-<n>` — exactly as VSCode's "Send to Claude Code" would.

If no text is selected, the task still fires and the at-mention falls
back to a single-line range at the caret row (`$ZED_ROW`).

### Why a task, not a slash command or context-menu action?

Zed's extension API (`zed_extension_api` 0.7) does not currently expose
the editor's selection or a context-menu hook to extensions. The task
system, on the other hand, hands `$ZED_FILE`, `$ZED_ROW`,
`$ZED_SELECTED_TEXT`, and `$ZED_WORKTREE_ROOT` to whatever shell
command the task spawns — exactly what we need. Once Zed's extension
API grows a selection accessor we can revisit.

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

Test inventory (**109 tests** total at last count, all in the host
workspace):

- 85 unit tests across `protocol`, `lockfile`, `mcp`, `transport`, `ipc`,
  `app::cli` (the latter cover the `--text` / `--cursor-row` /
  `--line-start`/`--line-end` resolution priority for
  `ipc-send-at-mention`).
- 7 WebSocket integration tests in `tests/handshake.rs`.
- 14 IPC integration tests in `tests/ipc.rs`.
- 1 full-stack end-to-end test in `tests/end_to_end.rs` (IPC → sidecar →
  WebSocket → MCP client, with at_mention 0 → 1-indexed conversion
  verified on the wire).
- 2 lock-file integration tests in `tests/lockfile.rs`.

## Specs

- [`docs/protocol.md`](docs/protocol.md) — reverse-engineered wire format.
- [`openspec/changes/zed-claude-bridge/specs/`](openspec/changes/zed-claude-bridge/specs/)
  — per-module Given/When/Then scenarios that drove the implementation.
- [`.harness/project.md`](.harness/project.md) — workspace conventions
  (commands, layer order, error handling, secrets policy).

## License

Dual-licensed under MIT or Apache-2.0, at your option.
