//! The `hs` binary entry point. All logic lives in the `hs_cli` library (`src/lib.rs` and its
//! modules) so it can be exercised in-process by `tests/e2e.rs`; this file only parses arguments,
//! dispatches, and translates the result into a process exit code.

use clap::Parser;
use hs_cli::Cli;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code = hs_cli::cli::dispatch(cli).await;
    std::process::ExitCode::from(code as u8)
}
