//! Entrypoint for the `zed-claude-bridge` sidecar binary.
//!
//! This file is intentionally tiny: it parses CLI args, hands them to
//! [`zed_claude_bridge::app::run`], and translates the returned `Result`
//! into a process exit code. All real work happens behind the library's
//! module boundary.

use std::process::ExitCode;

use clap::Parser;

use zed_claude_bridge::app::{Cli, run};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zed-claude-bridge: {e:#}");
            ExitCode::FAILURE
        }
    }
}
