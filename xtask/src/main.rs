//! Hyakume's project tasks, run as `cargo xtask package` and `cargo xtask
//! qualify`. They are Rust so the same steps can serve Windows later. They
//! run cargo, rustc, git, and docker only where those are the source of
//! truth for what the tasks report.

mod notices;
mod package;
mod qualify;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(about = "Hyakume's project tasks")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Build the Linux package in dist/
    Package,
    /// Run a Linux package through the clean-host journey in a container
    /// (needs docker)
    Qualify {
        /// The package; by default, this version's package for this host
        /// in dist/
        package: Option<PathBuf>,
        /// The container image. Its C library must be at least as new as
        /// the build host's.
        #[arg(long, default_value = "debian:13")]
        image: String,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().task {
        Task::Package => package::package().map(|path| println!("{}", path.display())),
        Task::Qualify { package, image } => qualify::qualify(package, &image),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits inside the repository")
}

/// The cargo that runs this task, in the repository root.
fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root());
    cmd
}

/// Runs `cmd` to its end and returns its standard output. Its errors go to
/// this task's standard error.
fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .current_dir(root())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("{cmd:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd:?} failed: {}", out.status).into());
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// Runs `cmd` to its end, showing its output.
fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .current_dir(root())
        .status()
        .map_err(|e| format!("{cmd:?}: {e}"))?;
    if !status.success() {
        return Err(format!("{cmd:?} failed: {status}").into());
    }
    Ok(())
}

/// The target this toolchain builds for by default.
fn host_target() -> Result<String> {
    let info = output(Command::new("rustc").arg("-vV"))?;
    info.lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| "rustc -vV names no host".into())
}

/// The package file name for `version` and `target`, without extension.
fn package_name(version: &str, target: &str) -> String {
    format!("hyakume-{version}-{target}")
}

/// The SHA-256 of `data` in lowercase hex.
fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(data))
}
