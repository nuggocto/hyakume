//! `--json` promises one JSON error object on stderr, including for argument
//! errors that clap catches before any command runs.

use std::process::Command;

fn hyakume(args: &[&str]) -> (i32, serde_json::Value) {
    let profile = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hyakume"))
        .args(args)
        .env("HYAKUME_DATA_DIR", profile.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let error = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {stderr}"));
    (output.status.code().unwrap(), error)
}

#[test]
fn argument_errors_are_json_under_json() {
    for args in [
        &["--json", "task", "create", "--project", "WEB"][..],
        &[
            "task",
            "edit",
            "WEB-1",
            "--expected-version",
            "abc",
            "--json",
        ],
    ] {
        let (code, error) = hyakume(args);
        assert_eq!(code, 2, "{args:?}");
        assert_eq!(error["error"]["code"], "invalid", "{args:?}");
    }
}

#[test]
fn command_errors_are_json_under_json() {
    let (code, error) = hyakume(&["--json", "task", "show", "not-a-ref"]);
    assert_eq!(code, 2);
    assert_eq!(error["error"]["code"], "invalid");
}
