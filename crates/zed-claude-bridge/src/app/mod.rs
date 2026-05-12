//! Process-level orchestration: CLI parsing, tracing setup, lifecycle.
//!
//! Layer position: this is the top of the library — it wires every other
//! layer together. It is the only place that should use `anyhow::Result`
//! (per `.harness/project.md`).

pub mod cli;
pub mod lifecycle;
pub mod picker;

pub use cli::Cli;
pub use lifecycle::run;
pub use picker::pick_candidate;
