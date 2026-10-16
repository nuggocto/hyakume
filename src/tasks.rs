//! The fatal-invariant policy for owned tasks. Every spawned task has an
//! owner that observes its result. A task that panicked may have left shared
//! state half-changed, so its owner stops the process rather than go on with
//! it; ordinary failures are typed errors instead.

use std::panic::{AssertUnwindSafe, catch_unwind};

use tokio::task::{JoinError, JoinHandle};

/// Stops the process after a broken internal invariant.
pub fn fatal(what: &str) -> ! {
    tracing::error!(what, "stopping: an internal invariant was broken");
    std::process::abort()
}

/// Observes a finished task. A panic stops the process; a cancelled task
/// is an ordinary outcome.
pub fn observe<T>(result: Result<T, JoinError>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(e) if e.is_panic() => fatal("an owned task panicked"),
        Err(_) => None,
    }
}

/// Runs `job` on a blocking thread. A panic stops the process from that
/// thread, because the request awaiting the handle may be gone by then,
/// and a dropped handle would let the panic pass unseen.
pub fn spawn_blocking<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    tokio::task::spawn_blocking(move || match catch_unwind(AssertUnwindSafe(job)) {
        Ok(value) => value,
        Err(_) => fatal("a blocking job panicked"),
    })
}

/// Name of the variable that makes a debug build stop at a crash point.
pub const CRASH_VAR: &str = "HYAKUME_CRASH_AT";

/// Stops the process at once, as a crash would, when [`CRASH_VAR`] names
/// `point`. Only debug builds have crash points: tests use them to stop the
/// service between two steps of a commit and check what recovery makes of
/// the result. Release builds compile this to nothing.
pub fn crash_point(point: &str) {
    #[cfg(debug_assertions)]
    if std::env::var_os(CRASH_VAR).is_some_and(|v| v == point) {
        std::process::abort();
    }
    #[cfg(not(debug_assertions))]
    let _ = point;
}
