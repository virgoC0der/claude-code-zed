# Spec Delta: zed-extension

> **Note:** This capability has been superseded by the `.zed/tasks.json` flow
> described in the proposal. Zed's `zed_extension_api` (≤ 0.7) does not expose
> the editor's primary selection or a context-menu hook to extensions, so the
> requirements below are deferred indefinitely. The `extension/zed-claude-code/`
> crate that originally held this scaffold has been removed; selection capture
> now goes through Zed's built-in task system, which hands `$ZED_FILE`,
> `$ZED_ROW`, `$ZED_SELECTED_TEXT`, and `$ZED_WORKTREE_ROOT` to the
> `zed-claude-bridge ipc-send-at-mention` helper. The text below is preserved
> for spec-validate compatibility and as a reference for any future Zed API
> change that would let us revive an extension-driven flow.

## ADDED Requirements

### Requirement: Extension manifest

The Zed extension SHALL ship an `extension.toml` at
`extension/zed-claude-code/extension.toml` that declares the extension `id =
"zed-claude-code"`, a human-readable `name`, a `version` matching the workspace
crate version, and a target compatible with the current stable Zed extension API.

#### Scenario: extension.toml parses cleanly

- **WHEN** Zed loads the extension at `extension/zed-claude-code/`
- **THEN** Zed SHALL accept the `extension.toml` without manifest errors
- **AND** the loaded extension SHALL report id `"zed-claude-code"`

### Requirement: Send-to-Claude-Code action

The extension SHALL register a user-invokable action, available from the
command-palette and the editor context menu (right-click on a selection), labelled
**Send to Claude Code**, that fires when the user has a non-empty primary
selection.

#### Scenario: Action present in command palette

- **WHEN** the user opens Zed's command palette
- **THEN** the entry **Send to Claude Code** SHALL be listed
- **AND** invoking it on an editor with a selection SHALL trigger the at-mention
  flow

#### Scenario: Action present in editor context menu

- **WHEN** the user right-clicks on a non-empty editor selection
- **THEN** the context menu SHALL include the **Send to Claude Code** entry

### Requirement: Sidecar discovery and spawn

When the extension is asked to send a selection, it SHALL connect to the sidecar's
IPC socket at the documented path (`$TMPDIR/zed-claude-bridge-<workspace-hash>.sock`).
If the connect fails (no such file, or `ECONNREFUSED`), the extension SHALL spawn
the sidecar binary (`zed-claude-bridge`) with `--workspace <abs-workspace-root>`
and SHALL retry the connection up to 5 times with exponential backoff (initial
delay 50 ms, max 1 s).

#### Scenario: Sidecar already running

- **GIVEN** the sidecar is running and its IPC socket exists
- **WHEN** the user invokes **Send to Claude Code**
- **THEN** the extension SHALL connect to the existing socket without spawning a
  new sidecar process

#### Scenario: Sidecar not yet running

- **GIVEN** no sidecar is running for the current workspace
- **WHEN** the user invokes **Send to Claude Code**
- **THEN** the extension SHALL spawn `zed-claude-bridge --workspace <root>`
- **AND** SHALL retry IPC connect with backoff until success or 5 attempts
- **AND** on success SHALL deliver the `at_mention` IPC frame

### Requirement: at_mention frame on user action

The extension SHALL send exactly one IPC frame
`{"type":"at_mention","filePath":"<abs path>","lineStart":<L0>,"lineEnd":<L1>}`
when the **Send to Claude Code** action fires with a non-empty selection, where
`L0` and `L1` MUST be 0-indexed editor line numbers (start and end of the primary
selection, inclusive).

#### Scenario: Single-line selection

- **GIVEN** the user has a primary selection on line 5 (0-indexed) only
- **WHEN** they invoke **Send to Claude Code**
- **THEN** the extension SHALL send
  `{"type":"at_mention","filePath":"<abs path>","lineStart":5,"lineEnd":5}\n`
  to the sidecar's IPC socket

#### Scenario: Multi-line selection

- **GIVEN** the user has a primary selection from line 9 to line 19 (0-indexed)
- **WHEN** they invoke **Send to Claude Code**
- **THEN** the extension SHALL send
  `{"type":"at_mention","filePath":"<abs path>","lineStart":9,"lineEnd":19}\n`

### Requirement: workspace_folders frame on activation

When the extension activates for a workspace, it SHALL send one
`{"type":"workspace_folders","folders":[<abs paths>]}` frame to the sidecar
containing the absolute paths of the current workspace roots.

#### Scenario: Activation publishes workspace folders

- **WHEN** the extension activates for a workspace whose root is `/Users/me/proj`
- **THEN** the extension SHALL send
  `{"type":"workspace_folders","folders":["/Users/me/proj"]}\n`

### Requirement: No-op on empty selection

The extension SHALL NOT send an `at_mention` IPC frame when the user invokes
**Send to Claude Code** with no selection (caret only / empty range), and SHALL
surface a short user-visible message (status bar, notification, or similar)
indicating that a selection is required.

#### Scenario: Caret with no selection

- **GIVEN** the editor has only a caret (no highlighted range)
- **WHEN** the user invokes **Send to Claude Code**
- **THEN** the extension SHALL NOT send any IPC frame
- **AND** SHALL inform the user that a selection is required
