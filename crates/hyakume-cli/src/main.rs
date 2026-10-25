//! `hyakume`: the CLI, and the launcher for the desktop app when run without
//! arguments. This binary never links GPUI, so headless machines can install
//! it alone.

mod cli;
mod commands;
mod output;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use hyakume_client::ClientError;
use serde_json::json;

use crate::output::{error_kind, exit_code, terminal_safe};

const DESKTOP_BINARY: &str = "hyakume-desktop";
/// Shell convention for "command not found".
const DESKTOP_MISSING: u8 = 127;

fn main() -> ExitCode {
    if std::env::args_os().len() == 1 {
        return launch_desktop();
    }
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return usage_error(&err),
    };
    let json = cli.json;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: cannot start: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(commands::run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            report(&err, json);
            ExitCode::from(exit_code(&err))
        }
    }
}

/// Help and version go to stdout as usual. Real parse errors follow the
/// output contract: a JSON object on stderr under `--json`, exit code 2.
fn usage_error(err: &clap::Error) -> ExitCode {
    use clap::error::ErrorKind;

    let wants_json = std::env::args_os().any(|a| a == "--json");
    if !wants_json
        || matches!(
            err.kind(),
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
        )
    {
        err.exit();
    }
    let rendered = err.render().to_string();
    // Keep the error lines (clap lists missing arguments on their own lines)
    // and drop the usage block and help hint that follow.
    let message = rendered
        .lines()
        .take_while(|line| !line.is_empty() && !line.starts_with("Usage:"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    let invalid = ClientError::Invalid(message.trim_start_matches("error: ").to_owned());
    report(&invalid, true);
    ExitCode::from(exit_code(&invalid))
}

fn report(err: &ClientError, json: bool) {
    if json {
        let mut body = json!({ "error": { "code": error_kind(err), "message": err.to_string() } });
        if let ClientError::Conflict { latest, .. } = err {
            body["error"]["latest"] = json!(latest);
        }
        eprintln!("{body}");
        return;
    }
    eprintln!("error: {}", terminal_safe(&err.to_string()));
    if let ClientError::Conflict { latest, .. } = err {
        eprintln!(
            "latest version {}: \"{}\". Review it with `hyakume task show {}` and retry.",
            latest.version,
            terminal_safe(&latest.fields.title),
            terminal_safe(&latest.reference)
        );
    }
}

/// Replaces this process with the desktop app, found next to this binary
/// first and on PATH second.
fn launch_desktop() -> ExitCode {
    use std::os::unix::process::CommandExt;

    let Some(path) = find_desktop() else {
        eprintln!(
            "error: the desktop app ({DESKTOP_BINARY}) is not installed. \
             Install it, or run `hyakume --help` for CLI commands."
        );
        return ExitCode::from(DESKTOP_MISSING);
    };
    let err = std::process::Command::new(&path).exec();
    eprintln!("error: cannot start {}: {err}", path.display());
    ExitCode::FAILURE
}

fn find_desktop() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(DESKTOP_BINARY)));
    let on_path = std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(DESKTOP_BINARY))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    beside.into_iter().chain(on_path).find(|p| p.is_file())
}
