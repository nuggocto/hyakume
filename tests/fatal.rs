//! An owned task that panics stops the whole process.
//!
//! The test runs itself again as a child process with an environment
//! variable set. The child spawns a panicking task under the supervisor and
//! would print a marker if it survived.

// Test helpers may stop the test on unexpected failures.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::process::Command;

const CHILD: &str = "HYAKUME_FATAL_TEST_CHILD";
const SURVIVED: &str = "survived the panic";

#[test]
fn a_panicking_owned_task_stops_the_process() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_panicking_owned_task_stops_the_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env_remove("HYAKUME_LOG")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "the child exited normally:\n{stdout}"
    );
    assert!(!stdout.contains(SURVIVED), "{stdout}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(6),
            "expected an abort, got {:?}",
            output.status
        );
    }
}

fn child() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async { panic!("an invariant broke") });
        while let Some(done) = tasks.join_next().await {
            hyakume::tasks::observe(done);
        }
    });
    println!("{SURVIVED}");
}
