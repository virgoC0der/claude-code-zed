//! WebSocket accept loop, auth gate, and the per-connection request/response
//! pump.
//!
//! Layer position: this is layer 4 — depends on `protocol` and `mcp`.
//!
//! Public API:
//!
//! - [`bind_random`] — pick a free `127.0.0.1:<port>` listener with `<port> ∈
//!   [10000, 65535]`.
//! - [`Transport`] — owns the accept loop, the active-client slot, and the
//!   broadcast notifier the IPC layer pushes through.
//! - [`AuthToken`] — newtype around the per-launch UUID v4 secret with a
//!   redacting [`std::fmt::Debug`] impl, so accidental `info!` doesn't leak it.
//!
//! See the OpenSpec `specs/websocket/spec.md` for the contract this module
//! implements.

pub mod ws;

pub use ws::{AuthToken, Transport, TransportError, bind_random};
