# Manual smoke procedure — peer-cwd-discovery (task #16)

This is a **human-interactive** step. CI cannot validate
LaunchAgent + Claude CLI + Zed end-to-end. The implementer prepared
this procedure; the user runs it on their macOS machine and pastes
the results into `smoke-results.md` (alongside this file).

## Prerequisites

- macOS (any version that runs `claude-code` v2.1.76+).
- Zed installed with the project-local task at
  `.zed/tasks.json` ("Send selection to Claude Code") bound to
  `cmd-ctrl-c` in `.zed/keymap.json`.
- LaunchAgent plist installed at
  `~/Library/LaunchAgents/com.virgoC0der.zed-claude-bridge.plist`
  (per the existing deployment — `scripts/com.virgoC0der.zed-claude-bridge.plist`).
- Two project directories to use as the two distinct cwds. Suggested:
  - `~/proj-a` (anything; e.g. clone of a small repo)
  - `~/proj-b` (anything; e.g. a different repo or a tempdir with
    a single `main.rs` inside).

## Procedure

```bash
# 1. Install the new sidecar binary (overwrites ~/.cargo/bin/zed-claude-bridge).
cargo install --path /Users/sx.chen/Code/personal/claude-code-zed/crates/zed-claude-bridge --force

# 2. Restart the LaunchAgent so it picks up the new binary.
launchctl kickstart -k "gui/$(id -u)/com.virgoC0der.zed-claude-bridge"

# 3. Confirm the sidecar is running and tail its log in a third terminal
#    so you can watch the per-client log lines fire in real time.
tail -F ~/Library/Logs/zed-claude-bridge.log
```

In two separate terminals:

```bash
# Terminal A
cd ~/proj-a
claude /ide
# (wait for Claude to connect to the sidecar)
```

```bash
# Terminal B
cd ~/proj-b
claude /ide
# (wait for Claude to connect to the sidecar)
```

Each `claude /ide` connection triggers one INFO line in the
sidecar log:

```
authorized websocket client registered
    client_id=<UUID>
    workspace=<canonical-cwd>
    workspace_source="peer-cwd-libproc"
```

Both lines should appear — one per `claude /ide` invocation — with
`workspace_source="peer-cwd-libproc"` and `workspace=` set to
the *project's* canonical path (NOT `$HOME`).

## At-mention round-trip

In Zed (project A's window):

1. Open any file in `~/proj-a`.
2. Click somewhere in the file (or select a small range).
3. Press `cmd-ctrl-c`.

Expected behaviour:
- Zed's task "Send selection to Claude Code" fires.
- The sidecar's IPC dispatches the `at_mention` frame.
- The Claude session in **Terminal A** receives an
  `@<path>#L<row>-<row>` token in its input prompt.
- The Claude session in **Terminal B** receives nothing.
- NO picker dialog appears in Zed.

Repeat from Zed's window for `~/proj-b` → at-mention lands in
Terminal B only.

## Log evidence to capture

Tail the sidecar log:

```bash
tail -F ~/Library/Logs/zed-claude-bridge.log
```

Capture these line types (paste into `smoke-results.md`):

1. **Two `authorized websocket client registered` INFO lines**, one
   per session, each with `workspace_source="peer-cwd-libproc"` and
   the correct project path in `workspace=`.
2. **Two routing-success DEBUG lines** of the form
   `routed at_mention via workspace-unique rule` (the router's
   per-decision DEBUG log). One per `cmd-ctrl-c`.
3. **Zero `no matching client; dropping at_mention` WARN lines.**
   If you see one, the bug isn't fixed.

## Optional verification commands

```bash
# Confirm both lock files exist and carry the new auth tokens.
ls -la ~/.claude/ide/*.lock
jq . ~/.claude/ide/*.lock

# Confirm the sidecar's PID and connected fds (sanity).
pgrep -fl zed-claude-bridge
```

## Acceptance

- Both at-mentions are delivered to the correct sessions on the
  first try.
- Log evidence (1)+(2) above is captured.
- No picker dialog.
- No `no matching client` WARN.

## Rollback

If anything goes wrong:

```bash
# Reinstall the previous binary from origin/main, or from a known-good
# commit on a feature branch:
git checkout <previous-good-commit> -- crates/zed-claude-bridge/
cargo install --path crates/zed-claude-bridge --force
launchctl kickstart -k "gui/$(id -u)/com.virgoC0der.zed-claude-bridge"
```

The wire formats (lock-file JSON, JSON-RPC, IPC frames) are
unchanged by this change — rollback is safe.
