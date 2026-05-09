//! In-memory editor state served by the MCP read-only tools.
//!
//! The IPC layer (task #6) feeds this state via `apply_*` methods; the MCP
//! tools read from it. There is no I/O in this file.

use std::path::PathBuf;

use crate::protocol::{OpenEditor, Selection};

/// A snapshot of an editor selection rich enough to satisfy
/// `getCurrentSelection` / `getLatestSelection`.
///
/// Values are mirrored verbatim into the MCP tool result; field naming and
/// indexing semantics follow `docs/protocol.md` §3.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSelection {
    /// Selected text (may be empty).
    pub text: String,
    /// Absolute path or URI of the document the selection lives in.
    pub file_path: String,
    /// `file://` URL (or scheme-prefixed URI) for the document.
    pub file_url: String,
    /// 0-indexed start/end positions plus the `is_empty` flag.
    pub selection: Selection,
}

/// All editor-side state the MCP tools serve.
///
/// All fields are owned and mutated only via the `apply_*` / `clear_*` /
/// `set_*` methods on `&mut self` so we can reason about update points
/// without needing interior mutability.
#[derive(Debug, Default, Clone)]
pub struct EditorState {
    /// Selection while the editor has focus. Cleared on focus loss.
    current_selection: Option<StoredSelection>,
    /// Last-seen selection regardless of focus. Sticky; only replaced by a
    /// later [`Self::apply_selection`] call.
    latest_selection: Option<StoredSelection>,
    /// Most recent `open_editors` snapshot from the IPC layer.
    open_editors: Vec<OpenEditor>,
    /// Most recent `workspace_folders` snapshot from the IPC layer.
    workspace_folders: Vec<PathBuf>,
}

impl EditorState {
    /// Build an empty state — equivalent to [`Default::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only view of the focused selection, if any.
    pub fn current_selection(&self) -> Option<&StoredSelection> {
        self.current_selection.as_ref()
    }

    /// Read-only view of the last-seen selection (focused or not).
    pub fn latest_selection(&self) -> Option<&StoredSelection> {
        self.latest_selection.as_ref()
    }

    /// Read-only view of the most-recent open-editors snapshot.
    pub fn open_editors(&self) -> &[OpenEditor] {
        &self.open_editors
    }

    /// Read-only view of the most-recent workspace-folders snapshot.
    pub fn workspace_folders(&self) -> &[PathBuf] {
        &self.workspace_folders
    }

    /// Record a new selection. Updates both `current_selection` (focused
    /// view) and `latest_selection` (sticky view).
    pub fn apply_selection(&mut self, selection: StoredSelection) {
        self.latest_selection = Some(selection.clone());
        self.current_selection = Some(selection);
    }

    /// Clear `current_selection` only. Used on editor focus loss; the
    /// `latest_selection` stays unchanged.
    pub fn clear_current_selection(&mut self) {
        self.current_selection = None;
    }

    /// Replace the open-editors snapshot.
    pub fn set_open_editors(&mut self, editors: Vec<OpenEditor>) {
        self.open_editors = editors;
    }

    /// Replace the workspace-folders snapshot.
    pub fn set_workspace_folders(&mut self, folders: Vec<PathBuf>) {
        self.workspace_folders = folders;
    }
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;
    use crate::protocol::Position;

    fn sel(text: &str, line_start: u32, line_end: u32) -> StoredSelection {
        StoredSelection {
            text: text.to_string(),
            file_path: "/p/main.rs".to_string(),
            file_url: "file:///p/main.rs".to_string(),
            selection: Selection {
                start: Position {
                    line: line_start,
                    character: 0,
                },
                end: Position {
                    line: line_end,
                    character: 0,
                },
                is_empty: text.is_empty(),
            },
        }
    }

    #[test]
    fn default_state_is_empty() {
        let s = EditorState::new();
        assert!(s.current_selection().is_none());
        assert!(s.latest_selection().is_none());
        assert!(s.open_editors().is_empty());
        assert!(s.workspace_folders().is_empty());
    }

    #[test]
    fn apply_selection_sets_both_current_and_latest() {
        let mut s = EditorState::new();
        let v = sel("fn x(){}", 10, 12);
        s.apply_selection(v.clone());
        assert_eq!(s.current_selection(), Some(&v));
        assert_eq!(s.latest_selection(), Some(&v));
    }

    #[test]
    fn clear_current_keeps_latest() {
        let mut s = EditorState::new();
        let v = sel("hello", 1, 1);
        s.apply_selection(v.clone());
        s.clear_current_selection();
        assert!(s.current_selection().is_none());
        assert_eq!(s.latest_selection(), Some(&v));
    }

    #[test]
    fn set_open_editors_replaces_list() {
        let mut s = EditorState::new();
        s.set_open_editors(vec![OpenEditor {
            uri: "file:///a.rs".to_string(),
            is_active: true,
            is_pinned: false,
            is_preview: false,
            is_dirty: None,
            language_id: Some("rust".to_string()),
        }]);
        assert_eq!(s.open_editors().len(), 1);

        s.set_open_editors(vec![]);
        assert!(s.open_editors().is_empty());
    }

    #[test]
    fn set_workspace_folders_replaces_list() {
        let mut s = EditorState::new();
        s.set_workspace_folders(vec![PathBuf::from("/x")]);
        assert_eq!(s.workspace_folders(), &[PathBuf::from("/x")] as &[PathBuf]);
        s.set_workspace_folders(vec![]);
        assert!(s.workspace_folders().is_empty());
    }
}
