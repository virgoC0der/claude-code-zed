//! WebSocket accept loop, auth gate, and the per-connection request/response
//! pump.
//!
//! Layer position: this is layer 4 — depends on `protocol` and `mcp`.
//!
//! Public API:
//!
//! - [`bind_random`] — pick a free `127.0.0.1:<port>` listener with `<port> ∈
//!   [10000, 65535]`.
//! - [`Transport`] — owns the accept loop and the multi-client
//!   [`ClientRegistry`]. Outbound notifications are delivered
//!   per-client via the registry's per-client mpsc channels (see
//!   [`ClientRegistry::lookup_tx`] and the [`router`] module that the
//!   IPC layer drives).
//! - [`AuthToken`] — newtype around the per-launch UUID v4 secret with a
//!   redacting [`std::fmt::Debug`] impl, so accidental `info!` doesn't leak it.
//!
//! See the OpenSpec `specs/websocket/spec.md` for the contract this module
//! implements.

pub mod cwd_resolver;
pub mod registry;
pub mod router;
pub mod ws;

#[cfg(target_os = "macos")]
pub use cwd_resolver::LibprocCwdResolver;
pub use cwd_resolver::{
    BoxResolveFuture, CwdResolver, MockCwdResolver, NoopCwdResolver, default_cwd_resolver,
};
pub use registry::{
    CLIENT_CHANNEL_CAPACITY, ClientHandle, ClientHandleSnapshot, ClientId, ClientRegistry,
};
pub use router::{RoutingDecision, route_at_mention, route_selection_changed};
pub use ws::{AuthToken, Transport, TransportBuilder, TransportError, bind_random};
