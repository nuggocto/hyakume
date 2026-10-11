//! Checks of the built `hyakume` binary.

// Test helpers may stop the test on unexpected I/O failure.
#![allow(clippy::unwrap_used)]

use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_hyakume");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env_remove("HYAKUME_LOG")
        .output()
        .unwrap()
}

#[test]
fn version_prints_the_package_version_and_succeeds() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("hyakume {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn help_describes_the_command_and_succeeds() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Usage: hyakume"), "{text}");
    assert!(text.contains("--version"), "{text}");
}

#[test]
fn no_arguments_shows_help_and_fails() {
    let out = run(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Usage: hyakume"));
}

#[test]
fn unknown_arguments_are_refused() {
    let out = run(&["--definitely-not-an-option"]);
    assert_eq!(out.status.code(), Some(2));
}
