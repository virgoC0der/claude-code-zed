//! JSON-RPC dispatch for the IDE-side MCP server.
//!
//! [`dispatch`] is a pure function: given the current [`EditorState`] and a
//! parsed [`Request`], it returns one of:
//!
//! - [`McpResponse::Reply`] — send this JSON-RPC [`Response`] back to the
//!   client.
//! - [`McpResponse::NoReply`] — the request was a notification (e.g.
//!   `notifications/initialized`); send nothing.
//!
//! No I/O, no async, no allocations beyond what serde requires.

use serde_json::{Value, json};

use crate::mcp::state::EditorState;
use crate::mcp::tools as mcp_tools;
use crate::protocol::{
    CallToolParams, Error as JsonRpcError, InitializeResult, Request, Response, ServerCapabilities,
    ServerInfo, ToolsCapability, ToolsListResult, error_code,
};

/// MCP protocol version this server speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Server display name used in `initialize` `serverInfo.name`.
pub const SERVER_NAME: &str = "zed-claude-bridge";

/// Outcome of [`dispatch`] — either a JSON-RPC reply, or no reply (for
/// notifications).
#[derive(Debug, Clone, PartialEq)]
pub enum McpResponse {
    /// Send this JSON-RPC response back to the client.
    Reply(Response),
    /// The request was a notification; do not write anything to the wire.
    NoReply,
}

/// Dispatch a parsed JSON-RPC request against the editor state.
///
/// This function is total: every input produces a defined [`McpResponse`].
/// Unknown methods yield JSON-RPC error `-32601`; unknown tool names within
/// `tools/call` yield `-32602`.
pub fn dispatch(state: &EditorState, req: Request) -> McpResponse {
    match req.method.as_str() {
        "initialize" => McpResponse::Reply(Response::success(
            req.id,
            initialize_result()
                .and_then(|v| serde_json::to_value(v).ok())
                .unwrap_or(Value::Null),
        )),
        "notifications/initialized" => McpResponse::NoReply,
        "ping" => McpResponse::Reply(Response::success(req.id, json!({}))),
        "tools/list" => McpResponse::Reply(Response::success(req.id, tools_list_value())),
        "tools/call" => McpResponse::Reply(handle_tools_call(state, req)),
        _ => McpResponse::Reply(Response::failure(
            req.id,
            JsonRpcError {
                code: error_code::METHOD_NOT_FOUND,
                message: format!("Method not found: {}", req.method),
                data: None,
            },
        )),
    }
}

fn initialize_result() -> Option<InitializeResult> {
    Some(InitializeResult {
        protocol_version: PROTOCOL_VERSION.to_string(),
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability {
                list_changed: false,
            }),
        },
        server_info: ServerInfo {
            name: SERVER_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    })
}

fn tools_list_value() -> Value {
    let result = ToolsListResult {
        tools: mcp_tools::tools_list(),
    };
    // Falling back to an empty list is defensive; serialization of
    // `ToolsListResult` cannot fail with the static input we pass here, but
    // we still avoid `unwrap` per the workspace lint.
    serde_json::to_value(result).unwrap_or_else(|_| json!({"tools": []}))
}

fn handle_tools_call(state: &EditorState, req: Request) -> Response {
    let params: CallToolParams = match req.params.clone() {
        Some(v) => match serde_json::from_value(v) {
            Ok(p) => p,
            Err(e) => {
                return Response::failure(
                    req.id,
                    JsonRpcError {
                        code: error_code::INVALID_PARAMS,
                        message: format!("Invalid tools/call params: {e}"),
                        data: None,
                    },
                );
            }
        },
        None => {
            return Response::failure(
                req.id,
                JsonRpcError {
                    code: error_code::INVALID_PARAMS,
                    message: "tools/call requires params".to_string(),
                    data: None,
                },
            );
        }
    };

    let result = match params.name.as_str() {
        "getCurrentSelection" => mcp_tools::tool_get_current_selection(state),
        "getLatestSelection" => mcp_tools::tool_get_latest_selection(state),
        "getOpenEditors" => mcp_tools::tool_get_open_editors(state),
        "getWorkspaceFolders" => mcp_tools::tool_get_workspace_folders(state),
        other => {
            return Response::failure(
                req.id,
                JsonRpcError {
                    code: error_code::INVALID_PARAMS,
                    message: format!("Unknown tool: {other}"),
                    data: None,
                },
            );
        }
    };

    let value = serde_json::to_value(result).unwrap_or_else(|_| json!({"content":[]}));
    Response::success(req.id, value)
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
    use crate::protocol::{Position, RequestId, Selection};
    use std::path::PathBuf;

    fn req(id: i64, method: &str, params: Option<Value>) -> Request {
        Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Number(id),
            method: method.to_string(),
            params,
        }
    }

    fn note(method: &str, params: Option<Value>) -> Request {
        // For notifications-as-requests; dispatch only looks at method.
        Request {
            jsonrpc: "2.0".to_string(),
            id: RequestId::Null,
            method: method.to_string(),
            params,
        }
    }

    fn reply(resp: McpResponse) -> Response {
        match resp {
            McpResponse::Reply(r) => r,
            McpResponse::NoReply => panic!("expected a Reply, got NoReply"),
        }
    }

    fn ok_value(resp: McpResponse) -> Value {
        let r = reply(resp);
        assert!(
            r.error.is_none(),
            "expected success, got error: {:?}",
            r.error
        );
        r.result.expect("result")
    }

    fn err_code(resp: McpResponse) -> i32 {
        let r = reply(resp);
        r.error.expect("error present").code
    }

    fn structured_from_call_result(call_result: &Value) -> Value {
        let content = call_result
            .get("content")
            .and_then(|c| c.as_array())
            .expect("content array");
        let text = content[0]
            .get("text")
            .and_then(|t| t.as_str())
            .expect("text field");
        serde_json::from_str(text).expect("JSON body")
    }

    // ----- handshake / ping ---------------------------------------------

    #[test]
    fn initialize_returns_protocol_version_2024_11_05() {
        let state = EditorState::new();
        let resp = dispatch(&state, req(1, "initialize", Some(json!({}))));
        let v = ok_value(resp);
        assert_eq!(v["protocolVersion"], "2024-11-05");
        assert_eq!(v["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(v["serverInfo"]["name"], "zed-claude-bridge");
        // version string must match cargo's pkg version
        assert_eq!(v["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn notifications_initialized_yields_no_reply() {
        let state = EditorState::new();
        let resp = dispatch(&state, note("notifications/initialized", None));
        assert_eq!(resp, McpResponse::NoReply);
    }

    #[test]
    fn ping_returns_empty_object() {
        let state = EditorState::new();
        let resp = dispatch(&state, req(99, "ping", None));
        let v = ok_value(resp);
        assert_eq!(v, json!({}));
    }

    // ----- tools/list ----------------------------------------------------

    #[test]
    fn tools_list_advertises_exactly_four_tools() {
        let state = EditorState::new();
        let resp = dispatch(&state, req(2, "tools/list", None));
        let v = ok_value(resp);
        let tools = v["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 4);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"getCurrentSelection"));
        assert!(names.contains(&"getLatestSelection"));
        assert!(names.contains(&"getOpenEditors"));
        assert!(names.contains(&"getWorkspaceFolders"));
    }

    #[test]
    fn tools_list_omits_out_of_scope_names() {
        let state = EditorState::new();
        let resp = dispatch(&state, req(2, "tools/list", None));
        let v = ok_value(resp);
        let tools = v["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
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
                !names.contains(&forbidden),
                "out-of-scope tool {forbidden} must not be in tools/list"
            );
        }
    }

    // ----- tools/call ----------------------------------------------------

    #[test]
    fn tools_call_get_current_selection_empty_returns_success_false() {
        let state = EditorState::new();
        let resp = dispatch(
            &state,
            req(
                3,
                "tools/call",
                Some(json!({"name": "getCurrentSelection", "arguments": {}})),
            ),
        );
        let v = ok_value(resp);
        let body = structured_from_call_result(&v);
        assert_eq!(body["success"], false);
    }

    #[test]
    fn tools_call_get_current_selection_with_state_returns_full_shape() {
        let mut state = EditorState::new();
        state.apply_selection(StoredSelection {
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
        let resp = dispatch(
            &state,
            req(
                4,
                "tools/call",
                Some(json!({"name": "getCurrentSelection", "arguments": {}})),
            ),
        );
        let body = structured_from_call_result(&ok_value(resp));
        assert_eq!(body["success"], true);
        assert_eq!(body["text"], "fn x(){}");
        assert_eq!(body["filePath"], "/p/main.rs");
        assert_eq!(body["selection"]["start"]["line"], 10);
        assert_eq!(body["selection"]["end"]["line"], 12);
    }

    #[test]
    fn tools_call_get_workspace_folders_with_state() {
        let mut state = EditorState::new();
        state.set_workspace_folders(vec![PathBuf::from("/Users/me/proj")]);
        let resp = dispatch(
            &state,
            req(
                5,
                "tools/call",
                Some(json!({"name": "getWorkspaceFolders", "arguments": {}})),
            ),
        );
        let body = structured_from_call_result(&ok_value(resp));
        assert_eq!(body["success"], true);
        assert_eq!(body["rootPath"], "/Users/me/proj");
        assert_eq!(body["workspaceFile"], Value::Null);
        assert_eq!(body["folders"][0]["path"], "/Users/me/proj");
    }

    #[test]
    fn tools_call_unknown_tool_returns_minus_32602() {
        let state = EditorState::new();
        let resp = dispatch(
            &state,
            req(
                6,
                "tools/call",
                Some(json!({"name": "openDiff", "arguments": {}})),
            ),
        );
        assert_eq!(err_code(resp), error_code::INVALID_PARAMS);
    }

    #[test]
    fn tools_call_missing_params_returns_minus_32602() {
        let state = EditorState::new();
        let resp = dispatch(&state, req(7, "tools/call", None));
        assert_eq!(err_code(resp), error_code::INVALID_PARAMS);
    }

    // ----- unknown method ------------------------------------------------

    #[test]
    fn unknown_method_returns_minus_32601() {
        let state = EditorState::new();
        let resp = dispatch(&state, req(8, "resources/list", None));
        assert_eq!(err_code(resp), error_code::METHOD_NOT_FOUND);
    }

    #[test]
    fn prompts_list_returns_minus_32601() {
        let state = EditorState::new();
        let resp = dispatch(&state, req(9, "prompts/list", None));
        assert_eq!(err_code(resp), error_code::METHOD_NOT_FOUND);
    }
}
