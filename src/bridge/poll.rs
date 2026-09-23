//! The shared skeleton of the background poll loops (spec #298, ticket E).
//!
//! The request sweep, the external-message sync and the server-reconcile
//! loop each grew their own copy of
//!
//! ```text
//! loop {
//!     sleep(cadence);
//!     if serverless { continue; }
//!     pass;                 // may use `bounded_call` internally
//!     handle the failure;   // (or warn on every tick)
//! }
//! ```
//!
//! so a change to the sleep/guard/failure handling had to be made in every
//! copy. [`PollLoop`] owns that skeleton once: the cadence, the serverless
//! guard and the existing [`FailureLatch`] (ADR-0048). A flow supplies only
//! its pass as a closure returning the tick's future; the future is awaited
//! before the next tick starts, so a pass can own whatever state it needs.
//!
//! The cadence stays with the caller: the flows hold their interval as an
//! injectable `AtomicU64` field (tests store a small value), and the loop reads
//! it fresh on every tick, so every branch runs without sleeping real seconds.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::bridge::failure_latch::FailureLatch;

/// One background poll loop: wait the cadence, skip while serverless, run the
/// pass, latch its outcome.
///
/// A pass reports a failed tick as `Err`; the loop latches it under its own
/// name — the first failure (and any changed error) WARNs once with the cause,
/// identical repeats are DEBUG, and a success after a failure logs the
/// recovery (ADR-0048). Per-item failures inside a pass (one directory, one
/// session) stay with the pass, which knows their finer conditions; the latch
/// is for "this tick could not do its job".
pub(crate) struct PollLoop<'a> {
    /// Tick cadence in milliseconds, read fresh every tick.
    cadence_ms: &'a AtomicU64,
    /// Names this loop's one condition in the latch's lines ("server
    /// reconcile").
    name: &'static str,
    /// WARN once per distinct failure, DEBUG on identical repeats, INFO a
    /// recovery.
    latch: FailureLatch,
}

impl<'a> PollLoop<'a> {
    pub(crate) fn new(cadence_ms: &'a AtomicU64, name: &'static str) -> Self {
        Self {
            cadence_ms,
            name,
            latch: FailureLatch::default(),
        }
    }

    /// One tick: wait the cadence, then run `pass` unless `ready` reports
    /// nothing to watch (the serverless guard — Lazy Start has not attached or
    /// spawned a server yet). Returns whether the pass ran. Exposed to the
    /// crate so a loop whose pass cannot be driven in a test (the server
    /// reconcile scans the real process table) can pin its failure policy at
    /// the seam.
    pub(crate) async fn tick<F, Fut>(&mut self, ready: impl Fn() -> bool, pass: &mut F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), String>>,
    {
        tokio::time::sleep(Duration::from_millis(self.cadence_ms.load(Ordering::Relaxed))).await;
        if !ready() {
            return false;
        }
        match pass().await {
            Ok(()) => self.latch.succeeded("", self.name),
            Err(e) => self.latch.failed("", self.name, "", e),
        }
        true
    }

    /// The shared skeleton, until the task is dropped: tick forever.
    pub(crate) async fn poll<F, Fut>(&mut self, ready: impl Fn() -> bool, mut pass: F) -> !
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), String>>,
    {
        loop {
            self.tick(&ready, &mut pass).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{assert_line_level, capture_logs, level_count};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    /// The cadence is read from the injected field: with 1 ms stored, ticks
    /// complete without waiting on the production seconds.
    #[tokio::test]
    async fn ticks_run_on_the_injected_cadence() {
        let cadence = AtomicU64::new(1);
        let mut poll = PollLoop::new(&cadence, "test poll");
        let passes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&passes);
        let mut pass = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };

        for _ in 0..3 {
            assert!(poll.tick(|| true, &mut pass).await);
        }
        assert_eq!(passes.load(Ordering::SeqCst), 3);
    }

    /// The serverless guard: a tick with nothing to watch waits the cadence
    /// and skips the pass entirely; the next tick with a server runs it.
    #[tokio::test]
    async fn a_tick_without_a_server_skips_the_pass() {
        let cadence = AtomicU64::new(1);
        let mut poll = PollLoop::new(&cadence, "test poll");
        let passes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&passes);
        let mut pass = move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };

        assert!(!poll.tick(|| false, &mut pass).await, "the pass is skipped");
        assert_eq!(passes.load(Ordering::SeqCst), 0);
        assert!(poll.tick(|| true, &mut pass).await);
        assert_eq!(passes.load(Ordering::SeqCst), 1);
    }

    /// ADR-0048 through the seam: a failing pass WARNs once with the cause,
    /// the identical repeat is DEBUG, and a success after the failure logs the
    /// recovery at INFO.
    #[tokio::test]
    async fn a_repeated_pass_failure_warns_once_and_recovers() {
        let cadence = AtomicU64::new(1);
        let mut poll = PollLoop::new(&cadence, "test poll");
        let failing = Arc::new(AtomicBool::new(true));

        let (_, logs) = capture_logs(async {
            let signal = Arc::clone(&failing);
            let mut pass = move || {
                let signal = Arc::clone(&signal);
                async move {
                    if signal.load(Ordering::SeqCst) {
                        Err("boom".to_string())
                    } else {
                        Ok(())
                    }
                }
            };
            poll.tick(|| true, &mut pass).await;
            poll.tick(|| true, &mut pass).await;
            failing.store(false, Ordering::SeqCst);
            poll.tick(|| true, &mut pass).await;
        })
        .await;

        assert_eq!(
            level_count(&logs, "test poll failed", "WARN"),
            1,
            "one warning, not one per tick:\n{logs}"
        );
        assert!(logs.contains("boom"), "the warning carries the cause:\n{logs}");
        assert_eq!(
            level_count(&logs, "test poll still failing", "DEBUG"),
            1,
            "the identical repeat is DEBUG:\n{logs}"
        );
        assert_line_level(&logs, "test poll recovered", "INFO");
    }

    /// A changed error is not the same failure: it warns again.
    #[tokio::test]
    async fn a_changed_error_warns_again() {
        let cadence = AtomicU64::new(1);
        let mut poll = PollLoop::new(&cadence, "test poll");
        let attempt = Arc::new(AtomicUsize::new(0));

        let (_, logs) = capture_logs(async {
            let attempt = Arc::clone(&attempt);
            let mut pass = move || {
                let attempt = Arc::clone(&attempt);
                async move {
                    if attempt.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err("boom".to_string())
                    } else {
                        Err("different".to_string())
                    }
                }
            };
            poll.tick(|| true, &mut pass).await;
            poll.tick(|| true, &mut pass).await;
        })
        .await;

        assert_eq!(
            level_count(&logs, "test poll failed", "WARN"),
            2,
            "the changed error warns again:\n{logs}"
        );
        assert_eq!(
            level_count(&logs, "test poll still failing", "DEBUG"),
            0,
            "a changed error is not an identical repeat:\n{logs}"
        );
    }
}
