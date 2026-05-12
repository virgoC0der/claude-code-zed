# Manual macOS smoke procedure — session-routing

Per `tasks.md` §10.5, this is the human-loop smoke test that closes
out the session-routing change. It complements the
machine-verifiable proxy that already runs as part of `cargo test
--workspace` (the 9 tests in `crates/zed-claude-bridge/tests/session_routing.rs`
exercise every routing rule via real IPC and real WebSocket
connections — what's left is verifying that the visible-to-user
parts — the native picker dialog appearing in response to a real
`cmd-ctrl-c` keystroke from Zed — also work).

The implementer cannot run this themselves (it requires opening
GUI Zed and live `claude /ide` terminal sessions; an AI agent
cannot drive macOS UI nor type in interactive Claude prompts).
This document is for the team-lead to execute and attach a
pass/fail verdict to the task close-out.

## Prerequisites

- macOS 12+ (verified: macOS 26.3.1 per task #20 audit).
- `zed-claude-bridge` binary installed on `PATH`. To use the
  current-HEAD binary:
  ```bash
  cargo install --path crates/zed-claude-bridge --force
  ```
- The LaunchAgent installed and running (`./scripts/install-launchd.sh`),
  OR a manually-launched sidecar.
- A Zed window with `.zed/tasks.json` and `.zed/keymap.json` from
  this repo copied into the project root.
- Two terminal applications open (iTerm, Terminal.app, ghostty, etc.).
- The Claude Code CLI installed; `claude --version` works.

## Procedure — Scenario A: ambiguous workspace, picker fires

1. Pick a project directory you want to use, e.g. `~/Code/some-project`.
2. Open that project in Zed.
3. **Terminal 1**: `cd ~/Code/some-project && claude /ide`. Verify the
   `/ide` command reports "Zed" as the connected IDE. Leave the
   prompt sitting at idle.
4. **Terminal 2**: `cd ~/Code/some-project && claude /ide`. Same
   verification. You now have two Claude sessions in the SAME
   workspace.
5. **In Zed**: open any source file in the project. Select a few
   lines. Press `cmd-ctrl-c`.
6. **EXPECTED**: a native macOS list-chooser dialog pops up titled
   "Send selection to which Claude session?" with TWO entries
   labelled something like:
   - `Session 1 — connected 30s ago`
   - `Session 2 — connected 12s ago`
   (The exact durations depend on when you ran step 3/4.)
7. Click one of the entries — say, Session 1.
8. **EXPECTED**: the `@<path>#L<m>-<n>` text appears in Terminal 1's
   Claude prompt (Session 1). Terminal 2's prompt should be
   unchanged.

### Capture for close-out

Tail the sidecar log during the test:
```bash
tail -f ~/Library/Logs/zed-claude-bridge.log
```
Expected log lines in sequence:
- `INFO` "authorized websocket client registered" (×2, one per Claude
  session, each with `workspace_source="header"` or
  `="daemon-flag-fallback"` depending on whether you used Option A or B).
- `DEBUG` "writing Ambiguous IPC reply (awaiting follow-up)"
  with `rule="ambiguous-reply"` and `count=2`.
- `DEBUG` "routing at_mention" with `rule="client-id-override"`
  (after you click the picker entry).

Attach the relevant lines (timestamps + the three DEBUG/INFO entries
above) to the task close-out.

## Procedure — Scenario B: workspace-unique, no picker

1. Open Zed in a project directory `~/Code/project-A`.
2. **Terminal 1**: `cd ~/Code/project-A && claude /ide`.
3. **Terminal 2**: `cd ~/Code/UNRELATED-project-B && claude /ide`.
   (Note: this terminal joins a DIFFERENT workspace.)
4. **In Zed (project-A)**: select text, press `cmd-ctrl-c`.
5. **EXPECTED**: NO picker dialog appears. The at-mention silently
   lands in Terminal 1's Claude prompt (the project-A session).
6. **EXPECTED**: Terminal 2's prompt is unchanged.

### Capture for close-out

Expected log line:
- `DEBUG` "routing at_mention" with `rule="workspace-unique"`,
  identifying the chosen client by `client_id`.

Attach the line to the task close-out.

## Procedure — Scenario C (optional): no-match drop

1. Open Zed in `~/Code/project-A`. Make sure NO Claude session is
   connected from `~/Code/project-A` — but one IS connected from
   somewhere else, e.g. `~/Code/project-B`.
2. **In Zed**: press `cmd-ctrl-c` on a selection.
3. **EXPECTED**: no at-mention appears in any Claude session.
4. **EXPECTED**: the log shows a WARN line "no matching client;
   dropping at_mention" listing `workspace_root_raw`,
   `workspace_root_canonical`, and the set of `known_workspaces`.

This scenario proves the no-match troubleshooting path documented
in the README.

## Cancellation path (verify picker dismissal works)

Repeat Scenario A. At step 7, instead of clicking an entry, click
"Cancel" in the picker dialog.

**EXPECTED**: no at-mention appears in any Claude session. The
sidecar log shows the Ambiguous reply was written (rule
"ambiguous-reply") but no follow-up at_mention frame arrives — the
helper exited 0 after the user cancelled.

## Failure modes

If any scenario fails, capture the full sidecar log
(`~/Library/Logs/zed-claude-bridge.log`) and the Zed task output
from Zed's task console (View → Open Tasks → recent run), and
attach to the task close-out for triage.

Most likely failure modes (already covered by the 9 automated
integration tests, so this is a sanity-check that the binary on
disk matches what `cargo test` ran against):

- Picker dialog never appears → `osascript` issue or LaunchAgent
  not in Aqua session (task #20 verified this works on macOS
  26.3.1).
- At-mention lands in the wrong session → router bug; check the
  log's `rule="..."` tag for the decision that fired.
- Both sessions receive the at-mention → spec violation; this
  would indicate the rewire to per-client mpsc is incomplete.
  Should be impossible given the test coverage.

## Sign-off

Once all three scenarios (A, B, optionally C) pass:
- The session-routing change is verified end-to-end on a real
  system.
- Attach the captured log excerpts to task #18's close-out.
- Notify the verifier / team-lead via SendMessage.

The change is then ready for OpenSpec archival.
