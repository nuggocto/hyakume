//! A panicked task or blocking job stops the process, even once nobody
//! awaits it; whether every owner observes its tasks is not checked here.
//! Each test reruns itself as a child, since an abort would end the test
//! runner, and the child prints a marker only if it survives.

// Test helpers may stop the test on unexpected failures.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::process::{Command, Stdio};
use std::time::Duration;

const CHILD: &str = "HYAKUME_FATAL_TEST_CHILD";
const SURVIVED: &str = "survived the panic";
/// A child that hangs instead of aborting is killed after this long.
const DEADLINE: Duration = Duration::from_secs(60);

/// Runs `test` again in a child process and checks that it aborted.
fn assert_child_aborts(test: &str) {
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env_remove("HYAKUME_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Its output is read while it runs, so a full pipe cannot stall it.
    let pid = rustix::process::Pid::from_child(&child);
    let (done, finished) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    let Ok(output) = finished.recv_timeout(DEADLINE) else {
        // Not reaped yet, so the PID still names the child.
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        waiter.join().unwrap();
        panic!("the child neither aborted nor ended in time");
    };
    waiter.join().unwrap();
    let output = output.unwrap();
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

#[test]
fn observing_a_panicked_task_stops_the_process() {
    if std::env::var_os(CHILD).is_none() {
        return assert_child_aborts("observing_a_panicked_task_stops_the_process");
    }
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

#[test]
fn a_blocking_job_that_panics_after_its_waiter_left_stops_the_process() {
    if std::env::var_os(CHILD).is_none() {
        return assert_child_aborts(
            "a_blocking_job_that_panics_after_its_waiter_left_stops_the_process",
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    runtime.block_on(async {
        let job = hyakume::tasks::spawn_blocking(move || {
            released.recv().unwrap();
            panic!("an invariant broke");
        });
        // The request awaiting the job ends first, as when its client
        // disconnects, and takes the job's handle with it.
        let waiter = tokio::spawn(job);
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
    });
    release.send(()).unwrap();
    // Dropping the runtime waits for the blocking job to finish.
    drop(runtime);
    println!("{SURVIVED}");
}
