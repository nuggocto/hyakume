//! The fatal-invariant policy for owned tasks.
//!
//! Every spawned task has an owner that keeps its handle and observes its
//! result. A task that panicked may have left shared state half-changed, so
//! its owner stops the process instead of continuing with that state.
//! Operational failures are typed errors returned by the task; only broken
//! invariants panic.

use tokio::task::JoinError;

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
