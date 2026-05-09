//! MCP server logic — pure dispatch, no I/O.
//!
//! Layer position: this is layer 3 (depends on `protocol`).
//!
//! - [`state`] holds the in-memory editor state served by the four read-only
//!   tools (`getCurrentSelection`, `getLatestSelection`, `getOpenEditors`,
//!   `getWorkspaceFolders`).
//! - [`tools`] implements those four tools and a static [`tools::TOOLS_LIST`].
//! - [`server`] dispatches incoming JSON-RPC requests against the state.
//!
//! This module deliberately imports nothing from `std::fs`, `std::env`, or
//! `tokio::net` — it must remain pure to honour the layer-order rule.

pub mod server;
pub mod state;
pub mod tools;

pub use server::{McpResponse, dispatch};
pub use state::{EditorState, StoredSelection};
