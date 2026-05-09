//! The four read-only MCP tools the IDE bridge exposes, plus their
//! advertised descriptors.
//!
//! Out-of-scope tools (`openDiff`, `getDiagnostics`, `executeCode`, …) are
//! deliberately not advertised here — see `docs/protocol.md` §3.2 first-cut
//! scope and the OpenSpec `specs/mcp/spec.md` requirement
//! "out-of-scope tools are not advertised".
//!
//! Each tool function takes an immutable [`EditorState`] reference and
//! returns the structured JSON shape described in the spec, wrapped in the
//! standard MCP [`CallToolResult`] envelope (one text content block whose
//! body is the JSON-encoded structured result).

use serde_json::{Value, json};

use crate::mcp::state::EditorState;
use crate::protocol::{CallToolResult, Tool, ToolContent};

/// Names of the four read-only tools, in the order they are advertised.
pub const TOOL_NAMES: &[&str] = &[
    "getCurrentSelection",
    "getLatestSelection",
    "getOpenEditors",
    "getWorkspaceFolders",
];

/// Static descriptors for [`TOOL_NAMES`]. Built once on first access.
pub fn tools_list() -> Vec<Tool> {
    let empty_object_schema: Value = json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    });

    vec![
        Tool {
            name: "getCurrentSelection".to_string(),
            description: Some(
                "Returns the current text selection in the focused editor, if any.".to_string(),
            ),
            input_schema: empty_object_schema.clone(),
        },
        Tool {
            name: "getLatestSelection".to_string(),
            description: Some(
                "Returns the most recent selection observed by the editor, even if focus has moved away.".to_string(),
            ),
            input_schema: empty_object_schema.clone(),
        },
        Tool {
            name: "getOpenEditors".to_string(),
            description: Some(
                "Returns a snapshot of the currently open editors.".to_string(),
            ),
            input_schema: empty_object_schema.clone(),
        },
        Tool {
            name: "getWorkspaceFolders".to_string(),
            description: Some(
                "Returns the workspace folders currently open in the IDE.".to_string(),
            ),
            input_schema: empty_object_schema,
        },
    ]
}

/// Wrap a structured JSON value into the standard MCP [`CallToolResult`]
/// envelope (one text content block carrying the JSON-encoded body).
pub(crate) fn ok_result(structured: Value) -> CallToolResult {
    let text = serde_json::to_string(&structured)
        .unwrap_or_else(|_| "{\"success\":false,\"error\":\"encode\"}".to_string());
    CallToolResult {
        content: vec![ToolContent::Text { text }],
        is_error: None,
    }
}

/// `getCurrentSelection` — focused-editor selection or `{success:false}`.
pub fn tool_get_current_selection(state: &EditorState) -> CallToolResult {
    let body = match state.current_selection() {
        Some(sel) => json!({
            "success": true,
            "text": sel.text,
            "filePath": sel.file_path,
            "fileUrl": sel.file_url,
            "selection": {
                "start": { "line": sel.selection.start.line, "character": sel.selection.start.character },
                "end":   { "line": sel.selection.end.line,   "character": sel.selection.end.character },
                "isEmpty": sel.selection.is_empty,
            },
        }),
        None => json!({ "success": false }),
    };
    ok_result(body)
}

/// `getLatestSelection` — sticky last-seen selection or `{success:false}`.
pub fn tool_get_latest_selection(state: &EditorState) -> CallToolResult {
    let body = match state.latest_selection() {
        Some(sel) => json!({
            "success": true,
            "text": sel.text,
            "filePath": sel.file_path,
            "fileUrl": sel.file_url,
            "selection": {
                "start": { "line": sel.selection.start.line, "character": sel.selection.start.character },
                "end":   { "line": sel.selection.end.line,   "character": sel.selection.end.character },
                "isEmpty": sel.selection.is_empty,
            },
        }),
        None => json!({ "success": false }),
    };
    ok_result(body)
}

/// `getOpenEditors` — the most recent IPC `open_editors` snapshot.
pub fn tool_get_open_editors(state: &EditorState) -> CallToolResult {
    // Project camelCase on the wire (matches VSCode extension's tool result).
    let editors: Vec<Value> = state
        .open_editors()
        .iter()
        .map(|e| {
            let mut entry = serde_json::Map::new();
            entry.insert("uri".to_string(), Value::String(e.uri.clone()));
            entry.insert("isActive".to_string(), Value::Bool(e.is_active));
            entry.insert("isPinned".to_string(), Value::Bool(e.is_pinned));
            entry.insert("isPreview".to_string(), Value::Bool(e.is_preview));
            if let Some(d) = e.is_dirty {
                entry.insert("isDirty".to_string(), Value::Bool(d));
            }
            if let Some(lang) = &e.language_id {
                entry.insert("languageId".to_string(), Value::String(lang.clone()));
            }
            Value::Object(entry)
        })
        .collect();
    ok_result(Value::Array(editors))
}

/// `getWorkspaceFolders` — `{success, folders, rootPath, workspaceFile}`.
///
/// `rootPath` is the first folder's path or `null`; `workspaceFile` is
/// always `null` for Zed (no `.code-workspace` analogue).
pub fn tool_get_workspace_folders(state: &EditorState) -> CallToolResult {
    let folders: Vec<Value> = state
        .workspace_folders()
        .iter()
        .enumerate()
        .map(|(idx, p)| {
            let path_str = p.to_string_lossy().into_owned();
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path_str.clone());
            json!({
                "name": name,
                "uri": format!("file://{path_str}"),
                "path": path_str,
                "index": idx,
            })
        })
        .collect();

    let root_path: Value = match state.workspace_folders().first() {
        Some(p) => Value::String(p.to_string_lossy().into_owned()),
        None => Value::Null,
    };

    let body = json!({
        "success": true,
        "folders": folders,
        "rootPath": root_path,
        "workspaceFile": Value::Null,
    });
    ok_result(body)
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;
    use crate::mcp::state::StoredSelection;
    use crate::protocol::{OpenEditor, Position, Selection};
    use std::path::PathBuf;

    /// Pull the JSON-encoded structured body out of a [`CallToolResult`].
    fn structured(r: &CallToolResult) -> Value {
        assert_eq!(r.content.len(), 1);
        let ToolContent::Text { text } = &r.content[0];
        serde_json::from_str(text).expect("structured body is valid JSON")
    }

    // ----- tools_list ----------------------------------------------------

    #[test]
    fn tools_list_advertises_exactly_the_four_tools() {
        let list = tools_list();
        let names: Vec<&str> = list.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "getCurrentSelection",
                "getLatestSelection",
                "getOpenEditors",
                "getWorkspaceFolders",
            ]
        );
    }

    #[test]
    fn tools_list_does_not_contain_out_of_scope_tools() {
        let names: Vec<String> = tools_list().into_iter().map(|t| t.name).collect();
        for forbidden in [
            "openDiff",
            "getDiagnostics",
            "executeCode",
            "close_tab",
            "closeAllDiffTabs",
            "openFile",
            "checkDocumentDirty",
            "saveDocument",
        ] {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "out-of-scope tool {forbidden} must not be advertised"
            );
        }
    }

    #[test]
    fn tools_list_descriptors_have_object_schema() {
        for tool in tools_list() {
            assert_eq!(tool.input_schema["type"], "object");
            assert!(
                tool.description.is_some(),
                "{} missing description",
                tool.name
            );
        }
    }

    // ----- getCurrentSelection ------------------------------------------

    #[test]
    fn current_selection_empty_state_returns_failure() {
        let s = EditorState::new();
        let body = structured(&tool_get_current_selection(&s));
        assert_eq!(body["success"], Value::Bool(false));
    }

    #[test]
    fn current_selection_populated_state_returns_full_shape() {
        let mut s = EditorState::new();
        s.apply_selection(StoredSelection {
            text: "fn x(){}".to_string(),
            file_path: "/p/main.rs".to_string(),
            file_url: "file:///p/main.rs".to_string(),
            selection: Selection {
                start: Position {
                    line: 10,
                    character: 0,
                },
                end: Position {
                    line: 12,
                    character: 1,
                },
                is_empty: false,
            },
        });
        let body = structured(&tool_get_current_selection(&s));
        assert_eq!(body["success"], Value::Bool(true));
        assert_eq!(body["text"], "fn x(){}");
        assert_eq!(body["filePath"], "/p/main.rs");
        assert_eq!(body["fileUrl"], "file:///p/main.rs");
        assert_eq!(body["selection"]["start"]["line"], 10);
        assert_eq!(body["selection"]["end"]["line"], 12);
        assert_eq!(body["selection"]["end"]["character"], 1);
        assert_eq!(body["selection"]["isEmpty"], false);
    }

    // ----- getLatestSelection -------------------------------------------

    #[test]
    fn latest_selection_survives_focus_loss() {
        let mut s = EditorState::new();
        s.apply_selection(StoredSelection {
            text: "a".to_string(),
            file_path: "/p/a.rs".to_string(),
            file_url: "file:///p/a.rs".to_string(),
            selection: Selection {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 1,
                    character: 0,
                },
                is_empty: false,
            },
        });
        s.clear_current_selection();

        let cur = structured(&tool_get_current_selection(&s));
        assert_eq!(cur["success"], Value::Bool(false));

        let latest = structured(&tool_get_latest_selection(&s));
        assert_eq!(latest["success"], Value::Bool(true));
        assert_eq!(latest["filePath"], "/p/a.rs");
    }

    // ----- getOpenEditors -----------------------------------------------

    #[test]
    fn open_editors_empty_state_returns_empty_array() {
        let s = EditorState::new();
        let body = structured(&tool_get_open_editors(&s));
        assert_eq!(body, Value::Array(vec![]));
    }

    #[test]
    fn open_editors_returns_camel_case_entries() {
        let mut s = EditorState::new();
        s.set_open_editors(vec![OpenEditor {
            uri: "file:///p/a.rs".to_string(),
            is_active: true,
            is_pinned: false,
            is_preview: false,
            is_dirty: Some(true),
            language_id: Some("rust".to_string()),
        }]);
        let body = structured(&tool_get_open_editors(&s));
        let arr = body.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["uri"], "file:///p/a.rs");
        assert_eq!(arr[0]["isActive"], true);
        assert_eq!(arr[0]["isPinned"], false);
        assert_eq!(arr[0]["isPreview"], false);
        assert_eq!(arr[0]["isDirty"], true);
        assert_eq!(arr[0]["languageId"], "rust");
    }

    // ----- getWorkspaceFolders ------------------------------------------

    #[test]
    fn workspace_folders_empty_state_has_null_root() {
        let s = EditorState::new();
        let body = structured(&tool_get_workspace_folders(&s));
        assert_eq!(body["success"], Value::Bool(true));
        assert_eq!(body["rootPath"], Value::Null);
        assert_eq!(body["workspaceFile"], Value::Null);
        assert_eq!(body["folders"], Value::Array(vec![]));
    }

    #[test]
    fn workspace_folders_single_folder_populates_root_path() {
        let mut s = EditorState::new();
        s.set_workspace_folders(vec![PathBuf::from("/Users/me/proj")]);
        let body = structured(&tool_get_workspace_folders(&s));
        assert_eq!(body["success"], Value::Bool(true));
        assert_eq!(body["rootPath"], "/Users/me/proj");
        assert_eq!(body["workspaceFile"], Value::Null);
        let folders = body["folders"].as_array().expect("array");
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0]["name"], "proj");
        assert_eq!(folders[0]["path"], "/Users/me/proj");
        assert_eq!(folders[0]["uri"], "file:///Users/me/proj");
        assert_eq!(folders[0]["index"], 0);
    }
}
