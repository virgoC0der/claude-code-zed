# claude-code-zed

A local sidecar binary plus a project-local Zed task that bring Claude
Code's `/ide` integration to [Zed](https://zed.dev). When the sidecar is
running for your Zed window, the Claude Code CLI's `/ide` command will
discover Zed the same way it discovers VS Code or JetBrains, and a
`cmd-ctrl-c` keybinding delivers `@<file>#L<m>-<n>` style at-mentions
from the editor selection straight into the active Claude Code prompt.

The on-the-wire protocol is reverse-engineered from Anthropic's official
VSCode extension (v2.1.76) and documented byte-for-byte in
[`docs/protocol.md`](docs/protocol.md). The OpenSpec change that drove the
initial implementation is archived at
[`openspec/changes/archive/2026-05-09-zed-claude-bridge/`](openspec/changes/archive/2026-05-09-zed-claude-bridge/);
the active change adding session-aware at-mention routing is at
[`openspec/changes/session-routing/`](openspec/changes/session-routing/).

## Status

- **Sidecar binary** (`zed-claude-bridge`): functional. WebSocket MCP
  server, lock-file discovery, IPC socket, signal-handled lifecycle,
  selection debounce + dedup, an `ipc-send-at-mention` helper that
  derives the line range from `$ZED_SELECTED_TEXT` (or falls back to
  `$ZED_ROW`), and **session-aware at-mention routing** — multiple
  concurrent `claude /ide` sessions are first-class. The sidecar
  routes each at-mention to exactly one Claude session (no broadcast);
  when two sessions share a workspace, a native macOS picker
  disambiguates. See [*Multiple Claude sessions*](#multiple-claude-sessions) below.
- **At-mention trigger**: a project-local Zed *task* (`.zed/tasks.json`)
  bound to `cmd-ctrl-c` (`.zed/keymap.json`). The task receives
  `$ZED_FILE`, `$ZED_ROW`, `$ZED_SELECTED_TEXT`, and
  `$ZED_WORKTREE_ROOT` from Zed and forwards them to the sidecar over
  IPC. The worktree root is **load-bearing** for routing — see
  [*Why a task*](#why-a-task-not-a-slash-command-or-context-menu-action) below.
- **No Zed extension.** Zed's `zed_extension_api` (≤ 0.7) exposes
  neither the editor's primary selection nor a context-menu hook to
  extensions, so an extension cannot implement a "Send selection to
  Claude Code" command on its own. Selection capture is delegated to
  Zed's built-in task system instead. An earlier iteration of this
  repo shipped a `zed-claude-code` extension as a slash-command
  scaffold; it has been removed. The deferred spec stub at
  [`openspec/specs/zed-extension/`](openspec/specs/zed-extension/)
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

You have two options:

### Option A: launch on macOS login (recommended)

Install the bundled LaunchAgent. It runs a single sidecar pinned to your
home directory (`--workspace "$HOME"`), which Claude Code's `/ide` will
match against any project under `~/`:

```bash
cargo install --path crates/zed-claude-bridge      # ensure binary on PATH
chmod 700 ~/.claude/ide                            # one-time, sidecar requires 0700
./scripts/install-launchd.sh                       # install + start the agent
```

Logs at `~/Library/Logs/zed-claude-bridge.log`. Uninstall with
`./scripts/uninstall-launchd.sh`.

### Option B: run by hand

Per-window manual launch, useful when you want one sidecar per project:

```bash
cargo run -p zed-claude-bridge -- --workspace /path/to/your/project
```

If you go this route, edit `.zed/tasks.json` so `--workspace` is
`$ZED_WORKTREE_ROOT` (the original behaviour) instead of `$HOME`.

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
    [--workspace-root <PATH>] [--client-id <UUID>] \
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

Two flags drive session-aware routing (see
[*Multiple Claude sessions*](#multiple-claude-sessions) below):

- `--workspace-root <PATH>` — the Zed worktree from which this
  at-mention was triggered. Distinct from `--workspace` (which names
  the IPC socket scope). Populated by the Zed task from
  `$ZED_WORKTREE_ROOT`.
- `--client-id <UUID>` — direct-route override. **Set internally by
  the helper on the picker's follow-up leg**, not by end users.
  Malformed UUIDs are rejected at parse time with a non-zero exit.

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

`$ZED_WORKTREE_ROOT` is **load-bearing** for session-aware routing
(see [*Multiple Claude sessions*](#multiple-claude-sessions) below):
it's forwarded into the IPC frame's `workspace_root` field so the
sidecar can pick which `claude /ide` session should receive the
at-mention. Triggering the task outside a worktree (which leaves
`$ZED_WORKTREE_ROOT` empty) makes the workspace_root field empty
too — routing then falls back to the singleton rule (if only one
session is connected, it receives) or the no-match drop (if multiple
sessions are connected and none can be uniquely identified). In
practice every file you'd want to at-mention lives inside a worktree,
so this is a soft edge case, not a footgun.

## Multiple Claude sessions

The sidecar supports multiple concurrent `claude /ide` sessions
against the same WebSocket server. This matters for the LaunchAgent
deployment (one sidecar at `$HOME` serving every project under it),
and for users who run two `claude /ide` terminals against the same
project. Each at-mention goes to **exactly one** Claude session, not
all of them — the prior single-client / displacement policy is
removed.

### Routing rules

When you press `cmd-ctrl-c` in Zed, the sidecar inspects the
registered WebSocket clients and routes the at-mention via this
priority cascade:

1. **Direct client ID override.** Set internally by the helper on
   the second leg of a picker round-trip; you never set this by
   hand.
2. **Workspace match — unique.** If your `.zed/tasks.json` sent
   `--workspace-root "$ZED_WORKTREE_ROOT"` and exactly one connected
   Claude session is in that workspace, it receives.
3. **Workspace match — ambiguous.** If two or more Claude sessions
   are in the same workspace, the sidecar pops up a native macOS
   picker (`osascript choose from list`) so you pick which session
   should receive. On Linux, the sidecar falls back to the
   most-recently-active session and logs a WARN — a native Linux
   picker is a documented follow-up (see [*Linux follow-up*](#linux-follow-up)).
4. **Singleton registry.** If only one Claude session is connected,
   it receives regardless of workspace.
5. **No match.** Otherwise the at-mention is dropped with a WARN
   log naming the file path, the requested workspace, and the
   workspaces of every connected session.

`selection_changed` notifications (the read-only state push that
keeps Claude's `getCurrentSelection` view in sync) use a separate,
simpler rule: the sidecar matches the file's path against the
longest registered workspace prefix and delivers to every Claude
session in that workspace. If no workspace prefixes the path, the
notification fans out to all sessions — preserving pre-routing
behaviour for files opened outside any registered worktree. The
picker round-trip does NOT apply to selection_changed.

### Workspace identification

Each connected Claude session's "workspace" is resolved in this
priority order:

1. The optional `x-claude-code-workspace` request header on the
   WebSocket upgrade. Claude Code v2.1.76 does not emit this header
   today — it's included for forward compatibility.
2. A `cwd` field inside `params.clientInfo` on the MCP `initialize`
   request, if present.
3. The sidecar's own `--workspace` flag. For the LaunchAgent at
   `$HOME` this is the home directory — too broad to disambiguate
   on its own, but combined with the picker (rule 3 above) it
   degrades gracefully.

The resolved value is canonicalized via `std::fs::canonicalize` on
both the WebSocket side and the IPC frame side, so symlinks (e.g.
macOS's `/var → /private/var`) don't cause silent routing
mismatches.

### LaunchAgent interaction

The LaunchAgent option (single sidecar pinned to `$HOME`) is the
primary beneficiary of session-aware routing. Before this change,
one sidecar at `$HOME` worked for one session at a time; now it
serves every Claude session under `~/` simultaneously, with each
at-mention routed to the right session via the rules above. No
configuration change beyond installing the agent is required.

### Troubleshooting: my at-mention went nowhere

If pressing `cmd-ctrl-c` produced no at-mention in any Claude
session, tail the sidecar log:

```bash
tail -f ~/Library/Logs/zed-claude-bridge.log     # LaunchAgent option A
# or check stderr for the foreground sidecar (Option B)
```

You'll see one of these WARN patterns:

- `"no matching client; dropping at_mention"` — none of the
  connected Claude sessions matched your worktree. The log line
  emits both the raw and canonical forms of the workspace it tried
  to match plus the set of known sessions' workspaces. Likely
  cause: your `$ZED_WORKTREE_ROOT` differs from what `claude /ide`
  joined from. Re-open Claude Code in the correct directory.
- `"stale client_id; dropping at_mention"` — the helper picked a
  session via the picker, but the session disconnected before the
  follow-up frame arrived. Rare; press `cmd-ctrl-c` again.
- `"ambiguous match but peer disconnected before Ambiguous reply
  could be written"` — the IPC helper closed before the picker
  could be presented. Indicates a misconfigured task or a helper
  that exits early; check `.zed/tasks.json` against the version in
  this repo.

If you see no WARN line at all, the helper itself never wrote a
frame to the IPC socket — verify the sidecar is running
(`launchctl list | grep zed-claude-bridge` or `pgrep -f
zed-claude-bridge`) and that `.zed/tasks.json` exists in the
project root.

### Linux follow-up

The native picker for ambiguous routing is implemented for macOS
only in this iteration via `osascript choose from list`. On Linux
the sidecar falls back to routing to the most-recently-active
Claude session (smallest `last_activity_ms_ago`) and emits a WARN
log so users can spot the implicit choice. A native Linux picker
using `zenity --list` or `kdialog --menu` is intentionally
out-of-scope for this change and tracked as a follow-up OpenSpec
change. The protocol's `Ambiguous` reply frame already carries
everything a Linux picker would need (per-candidate labels +
`client_id`); only the helper-side dialog implementation is
missing.

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

Test inventory (**185 tests** total at last count, all in the host
workspace):

- 144 unit tests across `protocol`, `lockfile`, `mcp`, `transport`
  (registry, router, ws), `ipc::server` (including the slow-client
  backpressure tests), `app::cli` (line-range resolution and the
  `--workspace-root` / `--client-id` parse tests), and `app::picker`
  (AppleScript escape + macOS osascript stdout parsing).
- 15 WebSocket integration tests in `tests/handshake.rs` (auth gate,
  multi-client coexistence, registry-driven outbound, workspace
  header capture, `clientInfo.cwd` priority-2 capture, daemon
  `--workspace` priority-3 fallback).
- 14 IPC integration tests in `tests/ipc.rs`.
- 1 full-stack end-to-end test in `tests/end_to_end.rs` (IPC → sidecar →
  WebSocket → MCP client, with at_mention 0 → 1-indexed conversion
  verified on the wire).
- 2 lock-file integration tests in `tests/lockfile.rs`.
- 9 session-routing end-to-end tests in `tests/session_routing.rs`
  (distinct-workspace routing, canonicalization symmetry, stale
  client_id, legacy-helper disconnect, singleton fallback, no-match
  drop with WARN, Ambiguous reply with two candidates, picker
  follow-up routes to the picked client).

## Specs

- [`docs/protocol.md`](docs/protocol.md) — reverse-engineered wire format.
- [`openspec/specs/`](openspec/specs/) — per-module Given/When/Then
  scenarios (`protocol`, `lockfile`, `mcp`, `websocket`, `ipc`,
  `notifications`, `zed-extension`).
- [`openspec/changes/session-routing/`](openspec/changes/session-routing/)
  — active OpenSpec change deltas for session-aware at-mention
  routing (this iteration).
- [`.harness/project.md`](.harness/project.md) — workspace conventions
  (commands, layer order, error handling, secrets policy).

## License

Dual-licensed under MIT or Apache-2.0, at your option.
