use std::process::ExitCode;

use clap::Parser;

/// Pass files, text, and links between your own devices.
#[derive(Parser)]
#[command(
    name = "hyakume",
    version,
    about,
    arg_required_else_help = true,
    after_help = "Pairing and transfers are not available in this version yet."
)]
struct Cli {}

fn main() -> ExitCode {
    let Cli {} = Cli::parse();
    hyakume::diagnostics::init_from_env();
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "started");
    ExitCode::SUCCESS
}
