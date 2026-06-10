//! Pure wire types for the Claude Code IDE bridge.
//!
//! This module is **I/O-free**. It defines:
//!
//! - JSON-RPC 2.0 envelope types ([`Request`], [`Response`], [`Notification`], [`Error`]).
//! - The discovery [`LockFile`] shape that lives at `~/.claude/ide/<port>.lock`.
//! - MCP handshake and tool-call types ([`InitializeParams`], [`InitializeResult`],
//!   [`Tool`], [`CallToolParams`], [`CallToolResult`]).
//! - Notification payloads pushed by the IDE to the CLI
//!   ([`SelectionChangedParams`], [`AtMentionedParams`]).
//! - The internal Zed-extension ↔ sidecar [`IpcFrame`] enum (snake_case tagged union).
//!
//! Field names match the Claude Code wire format byte-for-byte (camelCase) — this
//! is intentional: every external rename is a `serde(rename = ...)` so the source
//! still uses Rust snake_case identifiers. The IPC frame enum, by contrast, uses
//! snake_case on the wire because it is *our* internal protocol.
//!
//! See `docs/protocol.md` for the canonical specification.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelope
// ---------------------------------------------------------------------------

/// JSON-RPC 2.0 protocol version literal — always the string `"2.0"`.
pub const JSONRPC_VERSION: &str = "2.0";

/// JSON-RPC request id. Per spec it is a string, number, or null.
///
/// We accept all three on the wire and round-trip them faithfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id, e.g. `1`.
    Number(i64),
    /// String id, e.g. `"abc-123"`.
    String(String),
    /// Null id, allowed by the spec for notifications-as-requests edge cases.
    Null,
}

/// A JSON-RPC 2.0 request.
///
/// `params` is left as raw [`serde_json::Value`] so we don't have to enumerate
/// every method at parse time — dispatch happens after deserialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request id; correlates with the response.
    pub id: RequestId,
    /// Method name, e.g. `"tools/list"`.
    pub method: String,
    /// Optional parameters, schema depends on `method`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response.
///
/// Exactly one of [`Response::result`] or [`Response::error`] is `Some`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request id this response correlates with.
    pub id: RequestId,
    /// Successful result payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error payload (mutually exclusive with `result`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

/// A JSON-RPC 2.0 notification — a request without an `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Method name, e.g. `"selection_changed"`.
    pub method: String,
    /// Optional parameters, schema depends on `method`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
    /// Numeric error code (see JSON-RPC spec for reserved ranges).
    pub code: i32,
    /// Short human-readable message.
    pub message: String,
    /// Optional structured details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC error codes we use.
pub mod error_code {
    /// Generic parse error (malformed JSON).
    pub const PARSE_ERROR: i32 = -32700;
    /// The JSON sent is not a valid Request object.
    pub const INVALID_REQUEST: i32 = -32600;
    /// The method does not exist or is not available.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Invalid method parameter(s).
    pub const INVALID_PARAMS: i32 = -32602;
    /// Internal JSON-RPC error.
    pub const INTERNAL_ERROR: i32 = -32603;
}

impl Response {
    /// Build a successful response for a given request id.
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response for a given request id.
    pub fn failure(id: RequestId, error: Error) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

impl Notification {
    /// Build a notification with the given method and params.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params: Some(params),
        }
    }
}

// ---------------------------------------------------------------------------
// Lock file (~/.claude/ide/<port>.lock)
// ---------------------------------------------------------------------------

/// JSON shape of `~/.claude/ide/<port>.lock`.
///
/// Field names are camelCase on the wire to match the Claude Code CLI's
/// expectations exactly — see `docs/protocol.md` §1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockFile {
    /// Sidecar process PID. Only a discovery hint — the CLI does not validate it.
    pub pid: u32,
    /// Absolute paths of the open workspace roots in the host editor.
    pub workspace_folders: Vec<PathBuf>,
    /// Free-form display name. We use `"Zed"`.
    pub ide_name: String,
    /// Transport identifier. Always the literal string `"ws"`.
    pub transport: String,
    /// `true` iff the host editor is running on Windows.
    pub running_in_windows: bool,
    /// Per-launch random UUID v4. Required in the
    /// `x-claude-code-ide-authorization` WebSocket request header.
    pub auth_token: String,
}

// ---------------------------------------------------------------------------
// MCP handshake & tool-call types
// ---------------------------------------------------------------------------

/// Parameters for the MCP `initialize` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Highest MCP protocol version the client speaks.
    pub protocol_version: String,
    /// Client-advertised capabilities — opaque pass-through.
    #[serde(default)]
    pub capabilities: Value,
    /// Client name + version.
    #[serde(default)]
    pub client_info: Option<ServerInfo>,
}

/// Result of a successful MCP `initialize` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// MCP protocol version we agree to. Currently `"2024-11-05"`.
    pub protocol_version: String,
    /// Capabilities the server advertises.
    pub capabilities: ServerCapabilities,
    /// Server name + version.
    pub server_info: ServerInfo,
}

/// MCP server capabilities. We only advertise a static `tools` capability
/// for now; the rest of the spec (resources, prompts, sampling, …) is
/// out of scope per `docs/protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// `tools` capability. `listChanged: false` means we do not push
    /// `tools/list_changed` notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
}

/// `tools` server capability descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    /// `true` iff the server emits `notifications/tools/list_changed`.
    pub list_changed: bool,
}

/// Generic name + version pair, used for both `serverInfo` and `clientInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Display name, e.g. `"zed-claude-bridge"`.
    pub name: String,
    /// Semver string, e.g. `"0.1.0"`.
    pub version: String,
}

/// One entry in the `tools/list` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// Tool name, e.g. `"getCurrentSelection"`.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments object.
    pub input_schema: Value,
}

/// Wrapper for the `tools/list` response payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolsListResult {
    /// All tools the server exposes.
    pub tools: Vec<Tool>,
}

/// Parameters for an MCP `tools/call` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallToolParams {
    /// Tool name being invoked.
    pub name: String,
    /// JSON-encoded arguments object. Tool-specific schema.
    #[serde(default)]
    pub arguments: Value,
}

/// Result of an MCP `tools/call` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    /// Pieces of content returned to the model. We always return one
    /// `text` entry whose body is the JSON-encoded structured result.
    pub content: Vec<ToolContent>,
    /// `true` iff the tool call ended in an error the model should see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// One content block in a [`CallToolResult`]. The MCP spec allows several
/// kinds (`text`, `image`, …); we only emit `text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolContent {
    /// Plain text content.
    #[serde(rename = "text")]
    Text {
        /// The text payload — typically a JSON-encoded structured result.
        text: String,
    },
}

// ---------------------------------------------------------------------------
// Notification payloads (IDE → CLI)
// ---------------------------------------------------------------------------

/// 0-indexed buffer position. Matches VSCode's `Position` shape exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// 0-indexed line number.
    pub line: u32,
    /// 0-indexed column (UTF-16 code unit offset, per VSCode semantics).
    pub character: u32,
}

/// Selection range — a half-open `[start, end)` interval, 0-indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    /// Inclusive start position.
    pub start: Position,
    /// Exclusive end position.
    pub end: Position,
    /// `true` iff `start == end`.
    pub is_empty: bool,
}

/// Params for the `selection_changed` notification.
///
/// See `docs/protocol.md` §3.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionChangedParams {
    /// The currently-selected text (may be empty).
    pub text: String,
    /// Absolute path or URI of the document. For untitled buffers, this
    /// is the URI string (e.g. `"untitled:Untitled-1"`).
    pub file_path: String,
    /// `file://` URL (or scheme-prefixed URI) for the document.
    pub file_url: String,
    /// The primary selection (multi-cursor secondary selections are ignored).
    pub selection: Selection,
}

/// Params for the `at_mentioned` notification.
///
/// Lines are **1-indexed** in this payload (the CLI uses them directly when
/// rendering `@path#L<start>-<end>`). This is intentional and matches the
/// VSCode extension's `+1` adjustment — see `docs/protocol.md` §3.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtMentionedParams {
    /// File path the at-mention refers to. May be relative or absolute.
    pub file_path: String,
    /// 1-indexed inclusive start line.
    pub line_start: u32,
    /// 1-indexed inclusive end line.
    pub line_end: u32,
}

impl AtMentionedParams {
    /// Construct from 0-indexed editor positions; the wire payload is 1-indexed.
    ///
    /// Mirrors the VSCode extension's `q = N.start.line + 1` adjustment
    /// (`docs/protocol.md` §3.3): given `(file, 9, 19)` you get `lineStart=10,
    /// lineEnd=20`, which renders as `@file#L10-20` in the Claude prompt.
    pub fn new(file_path: String, line_start_zero: u32, line_end_zero: u32) -> Self {
        Self {
            file_path,
            line_start: line_start_zero.saturating_add(1),
            line_end: line_end_zero.saturating_add(1),
        }
    }
}

/// Arguments for the `openFile` MCP tool (§3.2). Wire field names are
/// camelCase; serde defaults mirror the VSCode extension's schema
/// (`preview=false`, `selectToEndOfLine=false`, `makeFrontmost=true`).
///
/// Zed-capability notes: `preview`, `endText`, `selectToEndOfLine`, and
/// `makeFrontmost=false` are ACCEPTED for wire compatibility but have no
/// effect — the `zed` CLI can only position a cursor (`path:line:col`) in a
/// focused window; it cannot set selections or open in the background.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFileArgs {
    /// Path to the file to open (absolute, or relative to the first
    /// workspace folder).
    pub file_path: String,
    /// Preview-mode hint. Ignored (see type docs).
    #[serde(default)]
    pub preview: bool,
    /// Text pattern locating the cursor position (first occurrence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_text: Option<String>,
    /// Selection end pattern. Ignored (see type docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_text: Option<String>,
    /// Ignored (see type docs).
    #[serde(default)]
    pub select_to_end_of_line: bool,
    /// Focus hint. Effectively always true with the `zed` CLI.
    #[serde(default = "default_true")]
    pub make_frontmost: bool,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Internal IPC frames (Zed extension ↔ sidecar)
// ---------------------------------------------------------------------------

/// One open editor descriptor, sent from the Zed extension to the sidecar
/// so the sidecar can answer `getOpenEditors` MCP calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OpenEditor {
    /// `file://` URI (or `untitled:` URI for unsaved buffers).
    pub uri: String,
    /// `true` iff this editor currently has focus.
    pub is_active: bool,
    /// `true` iff the tab is pinned in the UI.
    #[serde(default)]
    pub is_pinned: bool,
    /// `true` iff the tab is in preview mode (single-click open).
    #[serde(default)]
    pub is_preview: bool,
    /// `true` iff the buffer has unsaved changes. `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_dirty: Option<bool>,
    /// Editor-reported language id (e.g. `"rust"`). `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
}

/// One frame on the internal Unix-socket IPC channel.
///
/// Field names are **snake_case** on the wire because this protocol is
/// internal to this project — unlike the camelCase Claude Code wire formats
/// above.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcFrame {
    /// A new selection was made in the editor.
    ///
    /// Lines are **0-indexed**, matching `selection_changed` notification
    /// semantics (`docs/protocol.md` §3.3 — "Selection ranges in
    /// `selection_changed` payloads are 0-indexed").
    Selection {
        /// Path of the file the selection lives in. The sidecar skips
        /// frames whose URI scheme is `comment` or `output`.
        file_path: String,
        /// 0-indexed inclusive start line.
        line_start: u32,
        /// 0-indexed inclusive end line.
        line_end: u32,
        /// Selected text content.
        text: String,
    },
    /// User invoked "Send to Claude Code" (or similar) on a range.
    ///
    /// Lines are **0-indexed** on this internal IPC channel (matching how
    /// editors report selection rows). The sidecar adds `+1` before forwarding
    /// the values into the [`AtMentionedParams`] notification, which is
    /// **1-indexed** per the Claude Code wire format.
    ///
    /// Two optional fields drive the session-routing rules documented in
    /// the `notifications` capability spec:
    /// - `workspace_root`: the Zed worktree root from which the at-mention
    ///   was triggered (populated by the Zed task from `$ZED_WORKTREE_ROOT`).
    ///   The sidecar matches it against each registered WebSocket client's
    ///   workspace to pick a recipient.
    /// - `client_id`: the registry id of a specific WebSocket client to
    ///   route directly to. Populated only on the second leg of a picker
    ///   round-trip (see `ipc-send-at-mention` picker round-trip behaviour
    ///   in the `protocol` capability spec).
    ///
    /// Both fields are omitted entirely on the wire when `None`.
    AtMention {
        /// Path of the file the at-mention refers to.
        file_path: String,
        /// 0-indexed inclusive start line.
        line_start: u32,
        /// 0-indexed inclusive end line.
        line_end: u32,
        /// Optional Zed worktree root for workspace-based routing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_root: Option<PathBuf>,
        /// Optional direct-route override; bypasses workspace matching.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<Uuid>,
    },
    /// Workspace roots changed; sidecar should rewrite the lock file.
    WorkspaceFolders {
        /// New list of absolute workspace folder paths.
        folders: Vec<PathBuf>,
    },
    /// Full snapshot of currently-open editors.
    OpenEditors {
        /// Editors visible in the host window.
        editors: Vec<OpenEditor>,
    },
    /// Liveness ping — the sidecar should reply with an ack frame.
    Ping,
    /// Sidecar's reply to an inbound IPC frame (e.g. ack of a `Ping`).
    Ack,
    /// Diagnostic log line forwarded from sidecar to extension. Off the
    /// hot path; intended for surfacing sidecar-side errors in Zed's UI.
    Log {
        /// Severity ("trace" / "debug" / "info" / "warn" / "error").
        level: String,
        /// Human-readable message.
        message: String,
    },
    /// Sidecar's reply to an at-mention whose workspace match was
    /// non-unique. Carries the candidate WebSocket clients so the helper
    /// can present a picker (macOS `osascript choose from list`) and
    /// then write a follow-up `AtMention` frame on the same IPC
    /// connection with a `client_id` set.
    ///
    /// This variant is emitted **only by the sidecar**. Helpers SHALL NOT
    /// send it. See the `notifications` capability spec for routing
    /// semantics and the `protocol` capability spec for the
    /// `AmbiguousCandidate` shape.
    Ambiguous {
        /// One candidate per WebSocket client whose workspace matched.
        /// Order is stable for the lifetime of one picker round-trip
        /// (sidecar-side iteration order at the moment of routing).
        candidates: Vec<AmbiguousCandidate>,
    },
}

/// One candidate inside an [`IpcFrame::Ambiguous`] reply.
///
/// The sidecar builds these from its `ClientRegistry` snapshot at the
/// moment it decides routing is ambiguous. The four fields are exactly
/// what the helper needs to render a picker label and to write a
/// follow-up `AtMention` frame with the chosen client.
///
/// Wire field set (no extras, no missing): `client_id`, `label`,
/// `connected_at_ms_ago`, `last_activity_ms_ago`. See the `protocol`
/// capability spec, **AmbiguousCandidate shape** requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmbiguousCandidate {
    /// Registry id of the candidate WebSocket client. Serializes as
    /// the lowercase 36-character hyphenated UUID v4 form (via the
    /// `uuid` crate's `serde` feature). Used as the value of
    /// `--client-id` on the helper's follow-up frame.
    pub client_id: Uuid,
    /// Human-readable description shown to the user in the picker.
    /// Constructed sidecar-side from the registry entry's metadata
    /// (e.g. `"Session 2 — connected 30s ago (last active 3s ago)"`).
    /// Guaranteed distinct within one `candidates` list.
    pub label: String,
    /// Milliseconds since the client's WebSocket upgrade completed.
    /// Used by the helper on platforms without a native picker (Linux
    /// today) and as picker label context. Non-negative by type —
    /// JSON values like `-1` fail to parse with a typed error.
    pub connected_at_ms_ago: u64,
    /// Milliseconds since the client last sent a JSON-RPC frame. Used
    /// as the Linux fallback's "most-recently-active" key (smallest
    /// value wins). Non-negative by type.
    pub last_activity_ms_ago: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Strip insignificant whitespace by re-serializing through `Value`.
    fn canonical(s: &str) -> String {
        let v: Value = serde_json::from_str(s).expect("test fixture must be valid JSON");
        serde_json::to_string(&v).expect("re-serialize")
    }

    fn roundtrip<T>(fixture: &str)
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let parsed: T = serde_json::from_str(fixture).expect("fixture deserializes");
        let serialized = serde_json::to_string(&parsed).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&serialized));
    }

    // ----- lock file ------------------------------------------------------

    #[test]
    fn lockfile_matches_protocol_md_example_byte_for_byte() {
        // Verbatim from docs/protocol.md §1.
        let fixture = r#"{
            "pid": 12345,
            "workspaceFolders": ["/Users/me/Code/my-project"],
            "ideName": "Visual Studio Code",
            "transport": "ws",
            "runningInWindows": false,
            "authToken": "f47ac10b-58cc-4372-a567-0e02b2c3d479"
        }"#;

        let parsed: LockFile = serde_json::from_str(fixture).expect("fixture parses");

        assert_eq!(parsed.pid, 12345);
        assert_eq!(
            parsed.workspace_folders,
            vec![PathBuf::from("/Users/me/Code/my-project")]
        );
        assert_eq!(parsed.ide_name, "Visual Studio Code");
        assert_eq!(parsed.transport, "ws");
        assert!(!parsed.running_in_windows);
        assert_eq!(parsed.auth_token, "f47ac10b-58cc-4372-a567-0e02b2c3d479");

        // Round-trip preserves shape.
        let serialized = serde_json::to_value(&parsed).expect("serialize");
        let expected: Value = serde_json::from_str(fixture).expect("expected parses");
        assert_eq!(serialized, expected);
    }

    #[test]
    fn lockfile_rejects_snake_case_fields() {
        // Defence-in-depth: confirm we did not accidentally accept snake_case
        // (which would diverge from Claude Code's wire format).
        let bad = r#"{
            "pid": 1,
            "workspace_folders": ["/x"],
            "ide_name": "Zed",
            "transport": "ws",
            "running_in_windows": false,
            "auth_token": "t"
        }"#;
        assert!(serde_json::from_str::<LockFile>(bad).is_err());
    }

    // ----- selection_changed --------------------------------------------

    #[test]
    fn selection_changed_roundtrip() {
        // Verbatim from docs/protocol.md §3.3.
        let fixture = r#"{
            "text": "fn main() { ... }",
            "filePath": "/Users/me/proj/src/main.rs",
            "fileUrl": "file:///Users/me/proj/src/main.rs",
            "selection": {
                "start": {"line": 10, "character": 0},
                "end":   {"line": 12, "character": 1},
                "isEmpty": false
            }
        }"#;
        roundtrip::<SelectionChangedParams>(fixture);

        let parsed: SelectionChangedParams = serde_json::from_str(fixture).expect("fixture parses");
        assert_eq!(parsed.file_path, "/Users/me/proj/src/main.rs");
        assert_eq!(parsed.file_url, "file:///Users/me/proj/src/main.rs");
        assert_eq!(parsed.selection.start.line, 10);
        assert_eq!(parsed.selection.end.character, 1);
        assert!(!parsed.selection.is_empty);
    }

    // ----- at_mentioned --------------------------------------------------

    #[test]
    fn at_mentioned_roundtrip() {
        // Verbatim from docs/protocol.md §3.3.
        let fixture = r#"{
            "filePath": "/relative/or/abs/path.rs",
            "lineStart": 10,
            "lineEnd": 20
        }"#;
        roundtrip::<AtMentionedParams>(fixture);

        let parsed: AtMentionedParams = serde_json::from_str(fixture).expect("fixture parses");
        assert_eq!(parsed.file_path, "/relative/or/abs/path.rs");
        assert_eq!(parsed.line_start, 10);
        assert_eq!(parsed.line_end, 20);
    }

    // ----- openFile args ---------------------------------------------------

    #[test]
    fn open_file_args_minimal_applies_defaults() {
        let v: OpenFileArgs = serde_json::from_str(r#"{"filePath":"/a/b.rs"}"#).unwrap();
        assert_eq!(v.file_path, "/a/b.rs");
        assert!(!v.preview);
        assert!(v.start_text.is_none());
        assert!(v.end_text.is_none());
        assert!(!v.select_to_end_of_line);
        assert!(v.make_frontmost, "makeFrontmost defaults to true");
    }

    #[test]
    fn open_file_args_full_roundtrip_camel_case() {
        let json = r#"{"filePath":"/p","preview":true,"startText":"fn main","endText":"}","selectToEndOfLine":true,"makeFrontmost":false}"#;
        let v: OpenFileArgs = serde_json::from_str(json).unwrap();
        assert_eq!(v.start_text.as_deref(), Some("fn main"));
        assert!(!v.make_frontmost);
        let back = serde_json::to_value(&v).unwrap();
        assert_eq!(back["filePath"], "/p");
        assert_eq!(back["startText"], "fn main");
    }

    #[test]
    fn open_file_args_missing_file_path_fails() {
        assert!(serde_json::from_str::<OpenFileArgs>("{}").is_err());
    }

    // ----- JSON-RPC envelope --------------------------------------------

    #[test]
    fn jsonrpc_request_with_numeric_id_roundtrip() {
        let fixture = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req: Request = serde_json::from_str(fixture).expect("parses");
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, RequestId::Number(1));
        assert_eq!(req.method, "tools/list");
        assert!(req.params.is_none());

        let back = serde_json::to_string(&req).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));
    }

    #[test]
    fn jsonrpc_request_with_string_id_and_params() {
        let fixture = r#"{
            "jsonrpc":"2.0",
            "id":"abc-123",
            "method":"tools/call",
            "params":{"name":"getCurrentSelection","arguments":{}}
        }"#;
        let req: Request = serde_json::from_str(fixture).expect("parses");
        assert_eq!(req.id, RequestId::String("abc-123".to_string()));
        assert_eq!(req.method, "tools/call");
        let params = req.params.as_ref().expect("params present");
        assert_eq!(params["name"], json!("getCurrentSelection"));
    }

    #[test]
    fn jsonrpc_response_success_omits_error() {
        let resp = Response::success(RequestId::Number(7), json!({"ok": true}));
        let s = serde_json::to_string(&resp).expect("serializes");
        assert!(s.contains("\"result\""));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn jsonrpc_response_error_omits_result() {
        let resp = Response::failure(
            RequestId::Number(7),
            Error {
                code: error_code::METHOD_NOT_FOUND,
                message: "Method not found".to_string(),
                data: None,
            },
        );
        let s = serde_json::to_string(&resp).expect("serializes");
        assert!(s.contains("\"error\""));
        assert!(!s.contains("\"result\""));
        assert!(s.contains("-32601"));
    }

    #[test]
    fn jsonrpc_notification_has_no_id() {
        let n = Notification::new(
            "at_mentioned",
            json!({"filePath":"/a","lineStart":1,"lineEnd":2}),
        );
        let s = serde_json::to_string(&n).expect("serializes");
        assert!(!s.contains("\"id\""));
        assert!(s.contains("\"method\":\"at_mentioned\""));
    }

    // ----- MCP handshake -------------------------------------------------

    #[test]
    fn initialize_result_matches_expected_shape() {
        // Verbatim shape from docs/protocol.md §4 (just the result body).
        let fixture = r#"{
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {"listChanged": false}
            },
            "serverInfo": {
                "name": "zed-claude-bridge",
                "version": "0.1.0"
            }
        }"#;
        roundtrip::<InitializeResult>(fixture);

        let parsed: InitializeResult = serde_json::from_str(fixture).expect("parses");
        assert_eq!(parsed.protocol_version, "2024-11-05");
        assert_eq!(parsed.server_info.name, "zed-claude-bridge");
        let tools = parsed.capabilities.tools.expect("tools cap present");
        assert!(!tools.list_changed);
    }

    #[test]
    fn tools_list_result_roundtrip() {
        let fixture = r#"{
            "tools": [
                {
                    "name": "getCurrentSelection",
                    "description": "Returns the active editor selection.",
                    "inputSchema": {"type":"object","properties":{}}
                }
            ]
        }"#;
        roundtrip::<ToolsListResult>(fixture);

        let parsed: ToolsListResult = serde_json::from_str(fixture).expect("parses");
        assert_eq!(parsed.tools.len(), 1);
        assert_eq!(parsed.tools[0].name, "getCurrentSelection");
    }

    #[test]
    fn call_tool_result_text_content() {
        let fixture = r#"{
            "content": [
                {"type":"text","text":"hello"}
            ]
        }"#;
        roundtrip::<CallToolResult>(fixture);

        let parsed: CallToolResult = serde_json::from_str(fixture).expect("parses");
        assert_eq!(parsed.content.len(), 1);
        match &parsed.content[0] {
            ToolContent::Text { text } => assert_eq!(text, "hello"),
        }
        assert!(parsed.is_error.is_none());
    }

    // ----- IPC frames ----------------------------------------------------

    #[test]
    fn ipc_selection_frame_uses_snake_case_tag() {
        // Verbatim from docs/protocol.md §6.
        let fixture =
            r#"{"type":"selection","file_path":"/x","line_start":10,"line_end":20,"text":"hi"}"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        match &f {
            IpcFrame::Selection {
                file_path,
                line_start,
                line_end,
                text,
            } => {
                assert_eq!(file_path, "/x");
                assert_eq!(*line_start, 10);
                assert_eq!(*line_end, 20);
                assert_eq!(text, "hi");
            }
            _ => panic!("wrong variant"),
        }
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));
    }

    #[test]
    fn ipc_at_mention_frame_roundtrip() {
        // Legacy fields only — both optional fields absent on the wire.
        let fixture = r#"{"type":"at_mention","file_path":"/x","line_start":1,"line_end":3}"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        match &f {
            IpcFrame::AtMention {
                file_path,
                line_start,
                line_end,
                workspace_root,
                client_id,
            } => {
                assert_eq!(file_path, "/x");
                assert_eq!(*line_start, 1);
                assert_eq!(*line_end, 3);
                assert!(workspace_root.is_none());
                assert!(client_id.is_none());
            }
            _ => panic!("wrong variant"),
        }
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));

        // Defence-in-depth: the serialized form MUST omit workspace_root
        // and client_id when both are None.
        let v: Value = serde_json::from_str(&back).expect("parse back");
        let obj = v.as_object().expect("object");
        let keys: std::collections::BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["type", "file_path", "line_start", "line_end"]
                .iter()
                .copied()
                .collect();
        assert_eq!(keys, expected, "legacy frame has unexpected keys: {keys:?}");
    }

    #[test]
    fn ipc_at_mention_frame_with_workspace_root_only_roundtrip() {
        let fixture = r#"{"type":"at_mention","file_path":"/p/x.rs","line_start":3,"line_end":4,"workspace_root":"/p"}"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        match &f {
            IpcFrame::AtMention {
                file_path,
                line_start,
                line_end,
                workspace_root,
                client_id,
            } => {
                assert_eq!(file_path, "/p/x.rs");
                assert_eq!(*line_start, 3);
                assert_eq!(*line_end, 4);
                assert_eq!(workspace_root.as_deref(), Some(std::path::Path::new("/p")));
                assert!(client_id.is_none());
            }
            _ => panic!("wrong variant"),
        }
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));

        // The serialized form MUST contain workspace_root and MUST NOT
        // contain client_id.
        let v: Value = serde_json::from_str(&back).expect("parse back");
        let obj = v.as_object().expect("object");
        assert!(obj.contains_key("workspace_root"));
        assert!(!obj.contains_key("client_id"));
    }

    #[test]
    fn ipc_at_mention_frame_with_client_id_only_roundtrip() {
        // The wire form is the lowercase hyphenated UUID v4 (uuid crate's
        // serde feature default).
        let fixture = r#"{"type":"at_mention","file_path":"/p/x.rs","line_start":0,"line_end":0,"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479"}"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        match &f {
            IpcFrame::AtMention {
                file_path,
                line_start,
                line_end,
                workspace_root,
                client_id,
            } => {
                assert_eq!(file_path, "/p/x.rs");
                assert_eq!(*line_start, 0);
                assert_eq!(*line_end, 0);
                assert!(workspace_root.is_none());
                assert_eq!(
                    *client_id,
                    Some(Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").expect("uuid"))
                );
            }
            _ => panic!("wrong variant"),
        }
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));

        let v: Value = serde_json::from_str(&back).expect("parse back");
        let obj = v.as_object().expect("object");
        assert!(!obj.contains_key("workspace_root"));
        assert_eq!(
            obj.get("client_id").and_then(|v| v.as_str()),
            Some("f47ac10b-58cc-4372-a567-0e02b2c3d479")
        );
    }

    #[test]
    fn ipc_at_mention_frame_with_both_optional_fields_roundtrip() {
        let fixture = r#"{"type":"at_mention","file_path":"/p/x.rs","line_start":1,"line_end":2,"workspace_root":"/p","client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479"}"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        match &f {
            IpcFrame::AtMention {
                workspace_root,
                client_id,
                ..
            } => {
                assert_eq!(workspace_root.as_deref(), Some(std::path::Path::new("/p")));
                assert_eq!(
                    *client_id,
                    Some(Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").expect("uuid"))
                );
            }
            _ => panic!("wrong variant"),
        }
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));
    }

    #[test]
    fn ipc_ambiguous_frame_one_candidate_roundtrip() {
        let fixture = r#"{"type":"ambiguous","candidates":[{"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","label":"Session 1 — connected 2m ago","connected_at_ms_ago":120000,"last_activity_ms_ago":3000}]}"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        match &f {
            IpcFrame::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 1);
                let c = &candidates[0];
                assert_eq!(
                    c.client_id,
                    Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").expect("uuid")
                );
                assert_eq!(c.label, "Session 1 — connected 2m ago");
                assert_eq!(c.connected_at_ms_ago, 120000);
                assert_eq!(c.last_activity_ms_ago, 3000);
            }
            _ => panic!("wrong variant"),
        }
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));
    }

    #[test]
    fn ipc_ambiguous_frame_two_candidates_roundtrip() {
        let fixture = r#"{"type":"ambiguous","candidates":[
            {"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","label":"Session 1","connected_at_ms_ago":1000,"last_activity_ms_ago":100},
            {"client_id":"00000000-0000-4000-8000-000000000000","label":"Session 2","connected_at_ms_ago":2000,"last_activity_ms_ago":50}
        ]}"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        match &f {
            IpcFrame::Ambiguous { candidates } => {
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates[0].label, "Session 1");
                assert_eq!(candidates[1].label, "Session 2");
                assert!(candidates[1].last_activity_ms_ago < candidates[0].last_activity_ms_ago);
            }
            _ => panic!("wrong variant"),
        }
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));
    }

    #[test]
    fn ambiguous_candidate_has_exactly_four_keys() {
        let fixture = r#"{"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","label":"Session 1","connected_at_ms_ago":3000,"last_activity_ms_ago":1000}"#;
        let c: AmbiguousCandidate = serde_json::from_str(fixture).expect("parses");
        let back = serde_json::to_string(&c).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));

        let v: Value = serde_json::from_str(&back).expect("parse back");
        let obj = v.as_object().expect("object");
        let keys: std::collections::BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> = [
            "client_id",
            "label",
            "connected_at_ms_ago",
            "last_activity_ms_ago",
        ]
        .iter()
        .copied()
        .collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn ambiguous_candidate_rejects_negative_durations() {
        // u64 rejects negative integers at parse time — typed error, no panic.
        let bad = r#"{"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","label":"x","connected_at_ms_ago":-1,"last_activity_ms_ago":0}"#;
        let err = serde_json::from_str::<AmbiguousCandidate>(bad)
            .expect_err("negative connected_at_ms_ago must reject");
        // Defence-in-depth: error references the offending field name.
        let msg = err.to_string();
        assert!(
            msg.contains("connected_at_ms_ago") || msg.contains("invalid"),
            "error message should reference the field or an 'invalid' indicator, got: {msg}"
        );

        // Symmetric: also rejects negative last_activity_ms_ago.
        let bad2 = r#"{"client_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","label":"x","connected_at_ms_ago":0,"last_activity_ms_ago":-2}"#;
        assert!(serde_json::from_str::<AmbiguousCandidate>(bad2).is_err());
    }

    #[test]
    fn ipc_workspace_folders_frame_roundtrip() {
        let fixture = r#"{"type":"workspace_folders","folders":["/a","/b"]}"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        match &f {
            IpcFrame::WorkspaceFolders { folders } => {
                assert_eq!(folders.len(), 2);
                assert_eq!(folders[0], PathBuf::from("/a"));
            }
            _ => panic!("wrong variant"),
        }
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));
    }

    #[test]
    fn ipc_open_editors_frame_roundtrip() {
        let fixture = r#"{
            "type":"open_editors",
            "editors":[
                {"uri":"file:///a.rs","is_active":true,"is_pinned":false,"is_preview":false,"is_dirty":true,"language_id":"rust"}
            ]
        }"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        match &f {
            IpcFrame::OpenEditors { editors } => {
                assert_eq!(editors.len(), 1);
                assert_eq!(editors[0].uri, "file:///a.rs");
                assert!(editors[0].is_active);
                assert_eq!(editors[0].is_dirty, Some(true));
                assert_eq!(editors[0].language_id.as_deref(), Some("rust"));
            }
            _ => panic!("wrong variant"),
        }
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));
    }

    #[test]
    fn ipc_ping_frame_roundtrip() {
        let fixture = r#"{"type":"ping"}"#;
        let f: IpcFrame = serde_json::from_str(fixture).expect("parses");
        assert!(matches!(f, IpcFrame::Ping));
        let back = serde_json::to_string(&f).expect("serializes");
        assert_eq!(canonical(fixture), canonical(&back));
    }

    #[test]
    fn ipc_ack_frame_roundtrip() {
        let s = serde_json::to_string(&IpcFrame::Ack).expect("serialize Ack");
        assert_eq!(s, r#"{"type":"ack"}"#);
        let back: IpcFrame = serde_json::from_str(&s).expect("parse Ack");
        assert_eq!(back, IpcFrame::Ack);
    }

    #[test]
    fn ipc_log_frame_roundtrip() {
        let f = IpcFrame::Log {
            level: "info".into(),
            message: "hello".into(),
        };
        let s = serde_json::to_string(&f).expect("serialize Log");
        let v: Value = serde_json::from_str(&s).expect("parse json");
        assert_eq!(v["type"], "log");
        assert_eq!(v["level"], "info");
        assert_eq!(v["message"], "hello");
        let back: IpcFrame = serde_json::from_str(&s).expect("parse Log");
        assert_eq!(back, f);
    }

    #[test]
    fn at_mentioned_constructor_one_indexes() {
        let p = AtMentionedParams::new("src/lib.rs".into(), 9, 19);
        assert_eq!(p.line_start, 10);
        assert_eq!(p.line_end, 20);
        assert_eq!(p.file_path, "src/lib.rs");
        let v = serde_json::to_value(&p).expect("serialize");
        assert_eq!(v["lineStart"], 10);
        assert_eq!(v["lineEnd"], 20);
        assert_eq!(v["filePath"], "src/lib.rs");
    }
}
