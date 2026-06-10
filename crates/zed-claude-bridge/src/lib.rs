//! Sidecar library for bridging Zed to Claude Code's `/ide` command.
//!
//! Layer order (lower depends on higher; never the reverse):
//! `protocol → lockfile → mcp → transport → ipc → app → main`.
//!
//! Modules are added by the implementer as tasks complete.

pub mod app;
pub mod ipc;
pub mod lockfile;
pub mod mcp;
pub mod protocol;
pub mod transport;
pub mod zed_watch;
