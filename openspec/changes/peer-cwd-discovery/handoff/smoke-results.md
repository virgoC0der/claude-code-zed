# Manual smoke results — peer-cwd-discovery (task #16)

Run the procedure in `smoke-procedure.md`, paste the relevant log
lines into the sections below, and check the boxes. The implementer
fills in `RUN_DATE`, `MACOS_VERSION`, `CLAUDE_VERSION`, etc.

## Environment

- Date: _______________________
- Host: macOS _______________________
- Binary version: `git rev-parse --short HEAD` → _______________________
- Claude CLI: `claude --version` → _______________________
- Zed version: _______________________
- LaunchAgent plist hash: `shasum ~/Library/LaunchAgents/com.virgoC0der.zed-claude-bridge.plist` → _______________________

## Session A — `~/proj-a`

### Registration INFO line

```
<paste the "authorized websocket client registered" line for terminal A here>
```

- [ ] `workspace_source="peer-cwd-libproc"`
- [ ] `workspace=<canonical /Users/.../proj-a>` (NOT `$HOME`)

### at_mention routing DEBUG/INFO trace (cmd-ctrl-c #1 in proj-a)

```
<paste the IPC at_mention + routing decision lines here>
```

- [ ] Claude session in Terminal A received `@…#L…`
- [ ] Claude session in Terminal B received NOTHING
- [ ] No picker dialog in Zed

## Session B — `~/proj-b`

### Registration INFO line

```
<paste the "authorized websocket client registered" line for terminal B here>
```

- [ ] `workspace_source="peer-cwd-libproc"`
- [ ] `workspace=<canonical /Users/.../proj-b>` (NOT `$HOME`)

### at_mention routing DEBUG/INFO trace (cmd-ctrl-c #2 in proj-b)

```
<paste the IPC at_mention + routing decision lines here>
```

- [ ] Claude session in Terminal B received `@…#L…`
- [ ] Claude session in Terminal A received NOTHING
- [ ] No picker dialog in Zed

## Negative-case check

Confirm the WARN that the pre-change behaviour produced does NOT
appear:

```bash
grep "no matching client" ~/Library/Logs/zed-claude-bridge.log | tail -5
```

- [ ] Zero hits during the smoke window.

## Acceptance

- [ ] All six `[ ]` boxes above checked.
- [ ] Both at-mentions delivered first try, no picker, no drops.
- [ ] Log evidence captured.

## Free-form notes

(Anything weird, slow, unexpected — paste here.)
