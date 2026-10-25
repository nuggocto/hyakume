use std::path::PathBuf;
use std::process::ExitCode;

use hyakume_server::config::Config;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: hyakume-server --config PATH";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let config_path = match (args.next().as_deref(), args.next(), args.next()) {
        (Some("--config"), Some(path), None) => PathBuf::from(path),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match Config::load(&config_path) {
        Ok(config) => hyakume_server::run(config).await,
        Err(err) => Err(err),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}
