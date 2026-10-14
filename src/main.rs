use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = hyakume::cli::Cli::parse();
    hyakume::diagnostics::init_from_env();
    hyakume::cli::main(cli)
}
