# notifications Specification Delta — session-routing

## MODIFIED Requirements

### Requirement: at_mentioned notification shape

When the sidecar receives an `at_mention` IPC frame, it SHALL
immediately (without debounce) determine the recipient by running
the router (see the **at_mentioned routing** requirement below).

If the router yields a single recipient client, the sidecar SHALL
emit a JSON-RPC notification with method `at_mentioned` and params
`{filePath, lineStart, lineEnd}` to that client only. `lineStart`
and `lineEnd` SHALL be **1-indexed** (the sidecar SHALL add 1 to the
0-indexed values it receives over IPC).

If the router yields multiple candidate clients, the sidecar SHALL
NOT emit any `at_mentioned` notification for this event. Instead, it
SHALL emit an `Ambiguous` reply frame on the IPC connection per the
`ipc` capability's **Ambiguous reply frame** requirement and await a
follow-up `at_mention` frame carrying a `client_id` override.

The notification SHALL NOT be broadcast to multiple clients under
any circumstances.

#### Scenario: at_mention IPC frame produces 1-indexed notification on the routed client

- **GIVEN** an authenticated client is the unique registered client
- **WHEN** the extension sends
  `{"type":"at_mention","file_path":"/p/x.rs","line_start":9,"line_end":19}`
- **THEN** within 50 ms that client SHALL receive
  `{"jsonrpc":"2.0","method":"at_mentioned","params":{"filePath":"/p/x.rs","lineStart":10,"lineEnd":20}}`

#### Scenario: at_mention is never fanned out

- **GIVEN** the registry contains two authorized clients A and B in
  the same workspace
- **WHEN** the extension sends an `at_mention` IPC frame for that
  workspace (without `client_id`)
- **THEN** neither A nor B SHALL receive an `at_mentioned`
  notification before a follow-up disambiguation frame is sent
- **AND** the sidecar SHALL respond on the IPC connection with an
  `Ambiguous` reply listing both A and B as candidates

### Requirement: Notifications dropped when no client is connected

The sidecar SHALL drop (not buffer) both `selection_changed` and
`at_mentioned` notifications when no MCP client is currently
connected. The sidecar's in-memory `EditorState` SHALL still be
updated.

For `at_mentioned`, the sidecar SHALL additionally drop the
notification (with a WARN log carrying the file path, the frame's
`workspace_root` if any, and the list of registered workspaces) when
clients are connected but **no client matches the routing rules**.
This is preferable to fan-out, which is the bug this change fixes.

#### Scenario: at_mention with no client is recorded but not sent

- **GIVEN** no WebSocket client is connected
- **WHEN** the extension sends an `at_mention` IPC frame
- **THEN** the sidecar SHALL NOT panic or queue the notification
- **AND** the next subsequent connection by a client SHALL NOT receive the
  previously dropped `at_mentioned`

#### Scenario: at_mention with no matching client drops and logs

- **GIVEN** the registry contains client A with
  `workspace_root = /Users/me/proj-a`
- **WHEN** the extension sends
  `{"type":"at_mention","file_path":"/Users/me/proj-b/x.rs","line_start":1,"line_end":1,"workspace_root":"/Users/me/proj-b"}`
- **THEN** A SHALL NOT receive an `at_mentioned` notification
- **AND** the sidecar SHALL log a WARN containing the frame's
  `workspace_root` and the registry's known workspaces

## ADDED Requirements

### Requirement: at_mentioned routing

The sidecar SHALL route each `at_mentioned` notification produced
from an IPC `at_mention` frame to exactly one (or zero) WebSocket
clients, using the following ordered rules. The first rule that
produces a unique recipient SHALL be used; rules that produce zero
or ambiguous candidates SHALL fall through to the next rule (or
trigger the ambiguous-reply path per rule 3). The chosen rule and
the recipient's registry id SHALL be logged at DEBUG. The sidecar
SHALL NOT use any heuristic tiebreaker (such as most-recently-active)
to silently disambiguate within a workspace — disambiguation among
workspace-matching candidates SHALL be deferred to the picker
round-trip orchestrated by the helper.

1. **Direct client_id override.** If the frame's `client_id` is
   `Some(id)` AND a registered client has that id, that client is
   the recipient. If the id is set but the corresponding client has
   disconnected, the router SHALL fall through to rule 5 with a
   WARN logged distinctly to indicate that the picker selection
   became stale.
2. **Workspace match — unique.** If the frame's `workspace_root`
   is `Some(r)`, filter the registry to clients whose canonical
   `workspace_root` equals `canonical(r)`. If exactly one matches,
   that client is the recipient.
3. **Workspace match — ambiguous.** Under the same filter as (2),
   if more than one client matches, the router SHALL return an
   `Ambiguous` decision listing every matching client as a
   candidate. The IPC layer SHALL translate this decision into the
   `Ambiguous` reply frame defined by the `ipc` capability. No
   `at_mentioned` SHALL be emitted for this event until a follow-up
   IPC frame with `client_id` is received.
4. **Singleton registry.** If the registry contains exactly one
   client (regardless of workspace), that client is the recipient.
5. **No match.** Otherwise, no notification is sent; the sidecar
   SHALL log a WARN per the **Notifications dropped when no client
   is connected** requirement.

#### Scenario: client_id override wins over workspace match

- **GIVEN** the registry has clients A (`workspace_root=/p`) and B
  (`workspace_root=/q`)
- **WHEN** the helper sends an `at_mention` with
  `workspace_root="/p"` AND `client_id="<B's UUID>"`
- **THEN** the recipient SHALL be B
- **AND** A SHALL NOT receive the notification

#### Scenario: Workspace match picks the lone matching client

- **GIVEN** the registry has clients A (`workspace_root=/p`) and B
  (`workspace_root=/q`)
- **WHEN** the extension sends an `at_mention` with
  `workspace_root="/p"` (no client_id)
- **THEN** the recipient SHALL be A

#### Scenario: Same workspace yields an ambiguous decision

- **GIVEN** the registry has clients A and B both with
  `workspace_root=/p`
- **WHEN** the extension sends an `at_mention` with
  `workspace_root="/p"` (no client_id)
- **THEN** no `at_mentioned` notification SHALL be delivered to any
  client until a follow-up disambiguation frame arrives
- **AND** the sidecar SHALL emit an `Ambiguous` IPC reply frame on
  the same connection, listing both A and B in `candidates`

#### Scenario: Singleton registry routes regardless of workspace

- **GIVEN** the registry has exactly one client A with
  `workspace_root=/p`
- **WHEN** the extension sends an `at_mention` with
  `workspace_root="/q"` (does not match A's workspace, no client_id)
- **THEN** the recipient SHALL be A
- **AND** the DEBUG log entry SHALL identify the rule as
  "singleton registry"

#### Scenario: No matching client drops with a WARN

- **GIVEN** the registry has clients A and B, with `workspace_root`
  values `/p` and `/q` respectively
- **WHEN** the extension sends an `at_mention` with
  `workspace_root="/r"` (matches neither A nor B, no client_id)
- **THEN** no `at_mentioned` notification SHALL be delivered to any
  client
- **AND** the sidecar SHALL log a WARN containing `/r` and the set
  `{/p, /q}`

#### Scenario: Stale client_id falls through to no-match drop

- **GIVEN** the registry has only client A
- **WHEN** the helper sends an `at_mention` with
  `client_id = <some other UUID>`
- **THEN** A SHALL NOT receive the notification
- **AND** the sidecar SHALL log a WARN distinctly identifying the
  stale client_id and noting the registry's current ids

### Requirement: Ambiguous candidate label content

When the router yields an `Ambiguous` decision, the sidecar SHALL
build each candidate's `label` string sidecar-side so the helper
need not embed any presentation logic. Labels SHALL satisfy the
following constraints:

- non-empty UTF-8;
- distinct within the candidate list (the sidecar SHALL append a
  disambiguating suffix such as ` #2`, ` #3` if two candidates
  produce identical base labels);
- human-readable, containing at least the candidate's 1-based index
  within the candidate list and a human-readable "connected X ago"
  duration.

The exact wording is implementation-defined and SHALL be documented
in `README.md` so the user knows what to expect in the picker dialog.

#### Scenario: Labels are distinct and human-readable

- **GIVEN** the router emits an Ambiguous decision with two
  candidates whose `connected_at` differ by 90 seconds
- **WHEN** the sidecar writes the `Ambiguous` reply frame
- **THEN** the two `label` strings SHALL differ from each other
- **AND** each SHALL contain a 1-based ordinal (e.g. `1` and `2`)
- **AND** each SHALL contain a human-readable elapsed-time phrase
  (e.g. `"1m ago"`, `"3m ago"`)

### Requirement: selection_changed routing within workspace

The sidecar SHALL route `selection_changed` notifications produced
by a debounced `selection` IPC frame to each registered client whose
`workspace_root` canonically equals the IPC frame's source
workspace, when that workspace is known. If the workspace cannot be
determined for a selection frame, the sidecar SHALL deliver the
notification to **every** registered client (preserving prior
behaviour). At-mention routing rules SHALL NOT apply to
`selection_changed` — there SHALL be no picker round-trip for
selection updates.

A `selection_changed`'s workspace SHALL be inferred from the IPC
frame's `file_path` by matching the longest prefix among the
registered workspace roots; if no prefix matches, the workspace is
considered unknown.

#### Scenario: selection_changed reaches only matching workspaces

- **GIVEN** the registry has clients A (`workspace_root=/p`) and B
  (`workspace_root=/q`)
- **WHEN** the extension sends a `selection` IPC frame for
  `file_path = /p/main.rs` and the 300 ms debounce elapses
- **THEN** A SHALL receive the `selection_changed` notification
- **AND** B SHALL NOT receive it

#### Scenario: selection_changed with unknown workspace fans out

- **GIVEN** the registry has clients A (`workspace_root=/p`) and B
  (`workspace_root=/q`)
- **WHEN** the extension sends a `selection` IPC frame for
  `file_path = /unrelated/path/main.rs` and the 300 ms debounce
  elapses
- **THEN** both A and B SHALL receive the `selection_changed`
  notification
