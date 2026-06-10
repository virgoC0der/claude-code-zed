# Selection Awareness v2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade the active-file watcher's pushed selection from the v1 empty placeholder to the user's REAL cursor/selection (0-indexed line + UTF-16 column + selected text), read from Zed's `editor_selections` table (UTF-8 byte offsets — calibrated empirically).

**Architecture:** `query.rs` gains a `LEFT JOIN editor_selections` and returns a richer `ActiveEditor { path, selection: Option<(u64,u64)>, unsaved_contents }`. `watcher.rs` converts byte offsets to wire `Position`s against a text basis (`editors.contents` for dirty buffers, else the on-disk file), fills `StoredSelection.text` with the selected bytes, and upgrades dedup from per-file to per-selection. The push pipeline (debounce, routing, EditorState) is unchanged.

**Tech Stack:** existing deps only.

**Design doc:** `docs/superpowers/specs/2026-06-10-selection-awareness-design.md`
**Branch:** `feat/selection-awareness` (from main; does NOT depend on PR #4).

**Calibrated facts (do not re-litigate):**
- `editor_selections.start/end` are **UTF-8 byte offsets**; `start == end` = cursor.
- `editors.path` and `editors.contents` are **BLOB** columns — SQL text-literal `=` comparison never matches; read as `Vec<u8>` (path already is in v1).
- Wire positions are **0-indexed**; `character` is UTF-16 code units (VSCode semantics, protocol.md §3.3).

---

## File Structure

| File | Change |
|------|--------|
| `crates/zed-claude-bridge/src/zed_watch/query.rs` | `ActiveEditor` struct; `active_editor_for_cwd`; `active_file_for_cwd` becomes a thin wrapper. |
| `crates/zed-claude-bridge/src/zed_watch/schema_probe.rs` | REQUIRED gains `editor_selections` columns; fixtures updated. |
| `crates/zed-claude-bridge/src/zed_watch/watcher.rs` | `position_at` / `selection_from_offsets` pure fns; `build_active_editor` takes the optional selection; `PushState` keyed on full `StoredSelection`; `refresh_once` wires it. |
| `crates/zed-claude-bridge/tests/zed_watch.rs` | Fixture DB gains a selection row; asserts converted positions + text. |
| `README.md`, `docs/protocol.md` | Selection awareness documented. |

---

## Task 1: query — `ActiveEditor` with selection offsets

**Files:**
- Modify: `crates/zed-claude-bridge/src/zed_watch/query.rs`
- Modify: `crates/zed-claude-bridge/src/zed_watch/schema_probe.rs`

- [ ] **Step 1: Extend the schema probe.** In `schema_probe.rs`, append to `REQUIRED`:

```rust
    ("editor_selections", "editor_id"),
    ("editor_selections", "workspace_id"),
    ("editor_selections", "start"),
    ("editor_selections", "end"),
```

Update the `good_db()` test fixture to also create:

```sql
CREATE TABLE editor_selections (
    item_id INTEGER, editor_id INTEGER, workspace_id INTEGER,
    start INTEGER, "end" INTEGER
);
```

and verify `probe_passes_on_good_schema` still passes / the missing-table test still fails (it now reports an `editor_selections` column if you remove that table — keep the existing assertions semantically intact, adjusting expected missing-column names if needed).

- [ ] **Step 2: Write the failing query tests.** In `query.rs` tests: extend the `db_with` fixture — add the `editor_selections` table to `execute_batch` (same DDL as above) and a new seeding helper so rows can optionally carry a selection:

```rust
    /// Add a selection row for the `idx`-th seeded editor (1-based item_id).
    fn add_selection(conn: &Connection, item_id: i64, wsid: i64, start: i64, end: i64) {
        conn.execute(
            "INSERT INTO editor_selections (item_id, editor_id, workspace_id, start, \"end\")
             VALUES (?1, ?1, ?2, ?3, ?4)",
            rusqlite::params![item_id, wsid, start, end],
        )
        .unwrap();
    }
```

New tests:

```rust
    #[test]
    fn active_editor_carries_selection_offsets() {
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        add_selection(&conn, 1, 1, 5, 12);
        let e = active_editor_for_cwd(&conn, Path::new("/p")).unwrap().unwrap();
        assert_eq!(e.path, PathBuf::from("/p/main.rs"));
        assert_eq!(e.selection, Some((5, 12)));
        assert_eq!(e.unsaved_contents, None);
    }

    #[test]
    fn active_editor_without_selection_row_has_none() {
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        let e = active_editor_for_cwd(&conn, Path::new("/p")).unwrap().unwrap();
        assert_eq!(e.selection, None, "LEFT JOIN must still surface the file");
    }

    #[test]
    fn active_editor_reads_blob_contents_as_unsaved_text() {
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        conn.execute(
            "UPDATE editors SET contents = CAST('dirty buffer text' AS BLOB) WHERE item_id = 1",
            [],
        )
        .unwrap();
        let e = active_editor_for_cwd(&conn, Path::new("/p")).unwrap().unwrap();
        assert_eq!(e.unsaved_contents.as_deref(), Some("dirty buffer text"));
    }

    #[test]
    fn blob_path_never_matches_sql_text_literal_regression() {
        // Lesson from live calibration: editors.path is BLOB; a text-literal
        // WHERE clause silently matches nothing. Guard the lesson.
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM editors WHERE path = '/p/main.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "BLOB column must not equal a TEXT literal");
        // ...while our byte-reading query DOES find it:
        assert!(active_editor_for_cwd(&conn, Path::new("/p")).unwrap().is_some());
    }

    #[test]
    fn active_file_wrapper_still_returns_path_only() {
        let conn = db_with(&[(1, "S", "/p", "/p/main.rs")]);
        assert_eq!(
            active_file_for_cwd(&conn, Path::new("/p")).unwrap(),
            Some(PathBuf::from("/p/main.rs"))
        );
    }
```

NOTE: `db_with`'s editors insert must now include a NULL `contents` column — extend the fixture DDL to `CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB, contents BLOB);` and keep the existing INSERT (3 columns named explicitly) or add `contents` as NULL.

- [ ] **Step 3: Run to verify failures** — `cargo test -p zed-claude-bridge zed_watch::query` → FAIL (`active_editor_for_cwd`/`ActiveEditor` not found).

- [ ] **Step 4: Implement.** In `query.rs`:

```rust
/// The active editor in the worktree matching a session cwd: its path, the
/// primary selection's UTF-8 byte-offset range (cursor when start == end),
/// and the persisted unsaved buffer text when the editor is dirty.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveEditor {
    /// Absolute file path stored by Zed.
    pub path: PathBuf,
    /// `(start, end)` UTF-8 byte offsets from `editor_selections`; `None`
    /// when no selection row is persisted (v1 empty-selection behaviour).
    pub selection: Option<(u64, u64)>,
    /// `editors.contents` (BLOB, lossy UTF-8) when non-NULL — the text basis
    /// for offset conversion on dirty buffers.
    pub unsaved_contents: Option<String>,
}
```

Rework `active_file_for_cwd`'s body into:

```rust
/// Full active-editor lookup. See [`ActiveEditor`].
pub fn active_editor_for_cwd(
    conn: &Connection,
    cwd: &Path,
) -> Result<Option<ActiveEditor>, ZedWatchError> {
    let Some(session) = current_session(conn)? else {
        return Ok(None);
    };
    let cwd_str = cwd.to_string_lossy().to_string();

    let mut stmt = conn.prepare(
        "SELECT w.paths, e.path, s.start, s.\"end\", e.contents
         FROM workspaces w
         JOIN items   i ON i.workspace_id = w.workspace_id AND i.active = 1 AND i.kind = 'Editor'
         JOIN editors e ON e.item_id = i.item_id AND e.workspace_id = w.workspace_id
         LEFT JOIN editor_selections s
             ON s.editor_id = i.item_id AND s.workspace_id = i.workspace_id
         WHERE w.session_id = ?1
         ORDER BY length(w.paths) DESC",
    )?;
    let rows = stmt.query_map([&session], |row| {
        let paths: Option<String> = row.get(0)?;
        // editors.path / editors.contents are BLOBs; read as bytes and decode
        // lossily. A TEXT-literal SQL comparison would silently match nothing.
        let active: Vec<u8> = row.get(1)?;
        let start: Option<i64> = row.get(2)?;
        let end: Option<i64> = row.get(3)?;
        let contents: Option<Vec<u8>> = row.get(4)?;
        Ok((paths, active, start, end, contents))
    })?;

    for row in rows {
        let (paths, active_bytes, start, end, contents) = row?;
        let Some(paths) = paths else { continue };
        if cwd_matches_worktree(&cwd_str, &paths) {
            let active = String::from_utf8_lossy(&active_bytes).to_string();
            if active.is_empty() {
                continue;
            }
            let selection = match (start, end) {
                (Some(s), Some(e)) if s >= 0 && e >= 0 => Some((s as u64, e as u64)),
                _ => None,
            };
            let unsaved_contents =
                contents.map(|c| String::from_utf8_lossy(&c).to_string());
            return Ok(Some(ActiveEditor {
                path: PathBuf::from(active),
                selection,
                unsaved_contents,
            }));
        }
    }
    Ok(None)
}

/// Path-only view of [`active_editor_for_cwd`] (v1 API, kept for callers
/// that don't need selection data).
pub fn active_file_for_cwd(
    conn: &Connection,
    cwd: &Path,
) -> Result<Option<PathBuf>, ZedWatchError> {
    Ok(active_editor_for_cwd(conn, cwd)?.map(|e| e.path))
}
```

(The old body is replaced; existing v1 tests keep passing through the wrapper.)

- [ ] **Step 5: Update the watcher's seed fixture.** `watcher.rs`'s `seed_db()` test helper creates the same tables — add `editor_selections` DDL and the `contents` column there too (its tests don't seed selections yet; Task 2 will).

- [ ] **Step 6: Run** — `cargo test -p zed-claude-bridge zed_watch && cargo clippy --workspace --all-targets -- -D warnings` → PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/zed-claude-bridge/src/zed_watch
git commit -m "feat(zed_watch): query returns ActiveEditor with selection offsets and unsaved contents"
```

---

## Task 2: watcher — offset→Position conversion, real selections, selection-level dedup

**Files:**
- Modify: `crates/zed-claude-bridge/src/zed_watch/watcher.rs`

- [ ] **Step 1: Write the failing conversion tests** (add to watcher.rs's test mod):

```rust
    // ----- position_at / selection_from_offsets ----------------------------

    #[test]
    fn position_at_ascii() {
        let basis = "fn a() {}\nfn main() {}\n";
        // offset 10 = start of line 2 (0-indexed line 1, char 0)
        let p = position_at(basis, 10);
        assert_eq!((p.line, p.character), (1, 0));
        // offset 13 = "main" start: line 1, char 3
        let p = position_at(basis, 13);
        assert_eq!((p.line, p.character), (1, 3));
    }

    #[test]
    fn position_at_multibyte_utf16_columns() {
        // '→' is 3 UTF-8 bytes / 1 UTF-16 unit; '你' is 3 bytes / 1 unit.
        let basis = "a→b你c\nx";
        // byte offset of 'c' = 1 + 3 + 1 + 3 = 8; utf16 col = 4
        let p = position_at(basis, 8);
        assert_eq!((p.line, p.character), (0, 4));
        // byte offset of 'x' = 10 (after \n at 9): line 1 char 0
        let p = position_at(basis, 10);
        assert_eq!((p.line, p.character), (1, 0));
    }

    #[test]
    fn selection_from_offsets_extracts_text_and_flags() {
        let basis = "hello\nworld\n";
        let (sel, text) = selection_from_offsets(basis, 6, 11).expect("in range");
        assert_eq!(text, "world");
        assert!(!sel.is_empty);
        assert_eq!((sel.start.line, sel.start.character), (1, 0));
        assert_eq!((sel.end.line, sel.end.character), (1, 5));
    }

    #[test]
    fn selection_from_offsets_cursor_is_empty() {
        let (sel, text) = selection_from_offsets("abc", 1, 1).expect("in range");
        assert!(sel.is_empty);
        assert_eq!(text, "");
        assert_eq!((sel.start.line, sel.start.character), (0, 1));
    }

    #[test]
    fn selection_from_offsets_out_of_range_degrades_to_none() {
        assert!(selection_from_offsets("abc", 0, 99).is_none());
        assert!(selection_from_offsets("abc", 99, 99).is_none());
        assert!(selection_from_offsets("abc", 2, 1).is_none(), "inverted range");
    }
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p zed-claude-bridge zed_watch::watcher` → FAIL.

- [ ] **Step 3: Implement the pure conversion fns** (in watcher.rs, near `build_active_editor`):

```rust
/// 0-indexed wire position of UTF-8 byte offset `off` in `basis`.
/// `character` counts UTF-16 code units from the line start (VSCode
/// semantics, protocol.md §3.3). Caller guarantees `off <= basis.len()`.
fn position_at(basis: &str, off: usize) -> Position {
    let bytes = basis.as_bytes();
    let before = &bytes[..off];
    let line = before.iter().filter(|b| **b == b'\n').count() as u32;
    let line_start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let col_text = String::from_utf8_lossy(&bytes[line_start..off]);
    let character = col_text.encode_utf16().count() as u32;
    Position { line, character }
}

/// Convert a `(start, end)` UTF-8 byte-offset range into a wire `Selection`
/// plus the selected text. Returns `None` when the range is out of bounds or
/// inverted (e.g. the DB and the text basis are momentarily out of sync) —
/// callers degrade to the v1 empty selection.
pub fn selection_from_offsets(basis: &str, start: u64, end: u64) -> Option<(Selection, String)> {
    let (s, e) = (start as usize, end as usize);
    if s > e || e > basis.len() {
        return None;
    }
    let text = String::from_utf8_lossy(&basis.as_bytes()[s..e]).to_string();
    Some((
        Selection {
            start: position_at(basis, s),
            end: position_at(basis, e),
            is_empty: s == e,
        },
        text,
    ))
}
```

- [ ] **Step 4: Thread real selections through `build_active_editor` and `refresh_once`.**

Change `build_active_editor`'s signature and body:

```rust
/// Build the `OpenEditor` + `StoredSelection` for `file`. When `selection`
/// carries a converted range + text, the stored selection is real; otherwise
/// it falls back to the v1 empty placeholder (file path only).
pub fn build_active_editor(
    file: &Path,
    selection: Option<(Selection, String)>,
) -> (OpenEditor, StoredSelection) {
```

with the `StoredSelection` constructed as:

```rust
    let (sel, text) = selection.unwrap_or((
        Selection {
            start: Position { line: 0, character: 0 },
            end: Position { line: 0, character: 0 },
            is_empty: true,
        },
        String::new(),
    ));
    let stored = StoredSelection {
        text,
        file_path: path_str.clone(),
        file_url: url.clone(),
        selection: sel,
    };
```

(keep the existing `OpenEditor` construction; update the two existing build_active_editor tests to pass `None` and keep their assertions).

In `refresh_once`: replace the `query::active_file_for_cwd` call with `query::active_editor_for_cwd`; derive the basis and selection:

```rust
        let editor = match query::active_editor_for_cwd(conn, cwd) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, client_id = %client.id, "active-editor query failed; skipping this client");
                continue;
            }
        };
        let active = editor.path.clone();
        // Selection basis: dirty-buffer contents first, else the on-disk file.
        let converted = match editor.selection {
            Some((s, e)) => {
                let basis = match &editor.unsaved_contents {
                    Some(c) => Some(c.clone()),
                    None => tokio::fs::read_to_string(&active).await.ok(),
                };
                basis.and_then(|b| selection_from_offsets(&b, s, e))
            }
            None => None,
        };
```

and build with `build_active_editor(&active, converted)`.

Upgrade dedup — `PushState`:

```rust
struct PushState {
    last: HashMap<ClientId, StoredSelection>,
}
```

with the comparison/insert keyed on the full `StoredSelection` (the `selection` clone made for the notification params serves as the dedup value; insert before pushing, same place the old path-insert lived).

- [ ] **Step 5: Update/extend the refresh tests.** Existing 3 refresh tests: `seed_db()` has no selection rows → pushes still happen with empty selection (assertions unchanged except `build_active_editor(...)` callers). Add:

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn refresh_pushes_real_selection_from_db() {
        let conn = seed_db();
        // Selection over bytes 3..8 of the on-disk basis — write a real file
        // so the disk fallback basis exists.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("main.rs");
        std::fs::write(&file, "abc\ndefgh\n").unwrap();
        let file_str = file.to_str().unwrap();
        conn.execute(
            "UPDATE editors SET path = CAST(?1 AS BLOB) WHERE item_id = 1",
            rusqlite::params![file_str],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO editor_selections (item_id, editor_id, workspace_id, start, \"end\")
             VALUES (1, 1, 1, 4, 9)",
            [],
        )
        .unwrap();

        let reg = ClientRegistry::new();
        let (_id, mut rx) = register(&reg, "/proj").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();
        refresh_once(&conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;

        let notif = rx.try_recv().expect("selection_changed queued");
        let params = notif.params.expect("params");
        assert_eq!(params["text"], "defgh");
        assert_eq!(params["selection"]["start"]["line"], 1);
        assert_eq!(params["selection"]["start"]["character"], 0);
        assert_eq!(params["selection"]["end"]["character"], 5);
        assert_eq!(params["selection"]["isEmpty"], false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_repushes_when_only_selection_changes() {
        // Same file, selection moves → dedup must NOT swallow the second push.
        let conn = seed_db();
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("main.rs");
        std::fs::write(&file, "abc\ndefgh\n").unwrap();
        conn.execute(
            "UPDATE editors SET path = CAST(?1 AS BLOB) WHERE item_id = 1",
            rusqlite::params![file.to_str().unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO editor_selections (item_id, editor_id, workspace_id, start, \"end\")
             VALUES (1, 1, 1, 0, 3)",
            [],
        )
        .unwrap();

        let reg = ClientRegistry::new();
        let (_id, mut rx) = register(&reg, "/proj").await;
        let state = Arc::new(RwLock::new(EditorState::new()));
        let mut ps = PushState::default();
        refresh_once(&conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        assert!(rx.try_recv().is_ok(), "first push");

        conn.execute("UPDATE editor_selections SET start = 4, \"end\" = 9", [])
            .unwrap();
        refresh_once(&conn, &reg, &state, Path::new("/daemon-ws"), &mut ps).await;
        let second = rx.try_recv().expect("selection move must re-push");
        assert_eq!(second.params.unwrap()["text"], "defgh");
    }
```

(Adapt `seed_db`'s `/proj/main.rs` constants if the UPDATE approach conflicts — the point is: real file on disk as basis, BLOB path update, selection rows. Record adaptations in deviations.)

- [ ] **Step 6: Run** — `cargo test -p zed-claude-bridge zed_watch && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check` → PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/zed-claude-bridge/src/zed_watch
git commit -m "feat(zed_watch): convert byte-offset selections to wire positions and push real selections"
```

---

## Task 3: integration test — selection end to end

**Files:**
- Modify: `crates/zed-claude-bridge/tests/zed_watch.rs`

- [ ] **Step 1: Extend `build_db`** — add the `editor_selections` table (same DDL as unit fixtures, plus the `contents BLOB` column on editors) and one selection row `(1, 1, 1, 6, 11)`; make the editors row point at a REAL temp file written with `"hello\nworld\n"` (the existing test writes `/proj/active.rs` as a constant — switch to a tempdir file so the disk basis exists; keep the cwd/worktree alignment consistent).

- [ ] **Step 2: New test** using the public APIs:

```rust
#[tokio::test(flavor = "current_thread")]
async fn selection_offsets_flow_to_wire_positions() {
    // build_db as extended above; then:
    let conn = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let editor = query::active_editor_for_cwd(&conn, Path::new("/proj")).unwrap().unwrap();
    assert_eq!(editor.selection, Some((6, 11)));
    let basis = std::fs::read_to_string(&editor.path).unwrap();
    let (sel, text) = zed_claude_bridge::zed_watch::watcher::selection_from_offsets(&basis, 6, 11).unwrap();
    assert_eq!(text, "world");
    assert_eq!((sel.start.line, sel.start.character), (1, 0));
    assert_eq!((sel.end.line, sel.end.character), (1, 5));
}
```

(Adjust paths/imports to the file's existing helpers; `selection_from_offsets` is `pub` per Task 2.)

- [ ] **Step 3: Run** — `cargo test -p zed-claude-bridge --test zed_watch && cargo test --workspace` → PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/zed-claude-bridge/tests/zed_watch.rs
git commit -m "test(zed_watch): end-to-end selection offsets to wire positions"
```

---

## Task 4: docs

**Files:**
- Modify: `README.md` (Active-file awareness subsection)
- Modify: `docs/protocol.md` (§9 internal-source paragraph)

- [ ] **Step 1: README** — in the `### Active-file awareness (automatic)` subsection, replace the `**Scope:** the active file only (not every open tab).` bullet with:

```markdown
- **Scope:** the active file plus your cursor/selection — the pushed
  `selection_changed` carries the 0-indexed selection range AND the selected
  text, so Claude knows which lines you're looking at without any keypress.
  Dirty buffers use Zed's persisted unsaved contents as the conversion basis.
```

- [ ] **Step 2: protocol.md** — in the §9 paragraph "Internal source: active-file watcher", replace the sentence about empty selections with:

```markdown
These notifications carry the active editor's primary selection converted
from Zed's persisted UTF-8 byte offsets (`editor_selections`) to 0-indexed
wire positions (UTF-16 columns), including the selected text. When no
selection row is persisted or the offsets are momentarily out of sync with
the text basis, the notification degrades to an empty selection (file path
only).
```

- [ ] **Step 3: Full verification** — `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` → PASS.

- [ ] **Step 4: Commit**

```bash
git add README.md docs/protocol.md
git commit -m "docs: selection awareness v2 (real cursor/selection + text)"
```

---

## Self-Review Notes

**Spec coverage:** D1 ActiveEditor + LEFT JOIN + contents → Task 1; D2 byte-offset conversion + UTF-16 columns + selected text + out-of-range degrade + dirty-buffer basis → Task 2; D3 dedup on full StoredSelection → Task 2 Step 4 + repush test; D4 unchanged pipeline → no transport/mcp edits anywhere; BLOB regression lesson → Task 1 `blob_path_never_matches_sql_text_literal_regression`; schema gate extension → Task 1 Step 1.

**Placeholder scan:** clean — every step carries code; the two "adapt" notes (seed_db constants, integration helpers) point at concrete existing code the implementer must read, with deviations recorded.

**Type consistency:** `ActiveEditor { path: PathBuf, selection: Option<(u64,u64)>, unsaved_contents: Option<String> }` used identically in Tasks 1/2/3. `selection_from_offsets(&str, u64, u64) -> Option<(Selection, String)>` (pub) consistent in Tasks 2/3. `build_active_editor(&Path, Option<(Selection, String)>)` consistent within Task 2. `position_at` private. Wire assertions use camelCase `isEmpty` matching `Selection`'s serde rename.
