//! A per-condition failure latch for best-effort retries (ADR-0048).
//!
//! Instant Reminder and the waiting-card Message Pin are best-effort calls
//! retried on every sweep: without a latch, a condition that keeps failing — a
//! missing Feishu scope, a chat the bot cannot pin in — WARNs on every tick for
//! as long as it lasts. The latch keys its state by condition (the Chat/Topic
//! for the reminder, the message for the pin) and applies the level policy: the
//! first failure for a key, and any changed error, logs WARN once with the
//! cause and the actionable scope; an identical repeat logs DEBUG; a success
//! after a failure logs INFO. Retry cadence and the best-effort decision stay
//! with the caller — the latch only decides the line.

use std::collections::HashMap;

/// What each condition's last warned failure looked like, so a repeat can be
/// told from a changed error. An entry is dropped when the condition recovers.
#[derive(Default)]
pub(crate) struct FailureLatch {
    /// key → the error text the WARN already reported.
    warned: HashMap<String, String>,
}

/// The line prefix for a condition: `what key` when the condition has a finer
/// key (a message, a Chat/Topic), `what` alone when the operation IS the
/// condition (a background poll loop passes an empty key).
fn condition_label(what: &str, key: &str) -> String {
    if key.is_empty() {
        what.to_string()
    } else {
        format!("{what} {key}")
    }
}

impl FailureLatch {
    /// Record a failed attempt for `key`: WARN — with the cause and `scope`,
    /// the Feishu scope that makes the failure actionable — when this error has
    /// not been warned for the key yet, DEBUG for an identical repeat. `what`
    /// names the operation; the key is appended to it in the line. An empty key
    /// means the operation is its own condition (one background loop), and an
    /// empty scope means no Feishu scope makes it actionable: both segments are
    /// left out of the line rather than rendered empty.
    pub(crate) fn failed(&mut self, key: &str, what: &str, scope: &str, error: impl std::fmt::Display) {
        let error = error.to_string();
        let condition = condition_label(what, key);
        if self.warned.get(key).is_some_and(|warned| warned == &error) {
            tracing::debug!("{condition} still failing: {error}");
            return;
        }
        if scope.is_empty() {
            tracing::warn!("{condition} failed (best-effort; repeats log at DEBUG): {error}");
        } else {
            tracing::warn!("{condition} failed (best-effort; scope {scope}; repeats log at DEBUG): {error}");
        }
        self.warned.insert(key.to_string(), error);
    }

    /// Record a successful attempt for `key`: INFO once when it clears a
    /// latched failure, silence when nothing was latched.
    pub(crate) fn succeeded(&mut self, key: &str, what: &str) {
        if self.warned.remove(key).is_some() {
            tracing::info!("{} recovered", condition_label(what, key));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{capture_logs, level_count};

    const SCOPE: &str = "im:message.pins:write_only";

    /// The first failure warns with the cause and the scope; an identical
    /// repeat is DEBUG, never a second WARN.
    #[tokio::test]
    async fn a_repeated_failure_logs_debug_after_one_warn() {
        let mut latch = FailureLatch::default();
        let (_, logs) = capture_logs(async {
            latch.failed("msg_card", "waiting card pin", SCOPE, "boom");
            latch.failed("msg_card", "waiting card pin", SCOPE, "boom");
        })
        .await;

        assert_eq!(
            level_count(&logs, "failed", "WARN"),
            1,
            "one WARN, not one per attempt:\n{logs}"
        );
        assert_eq!(
            level_count(&logs, "still failing", "DEBUG"),
            1,
            "the repeat is DEBUG:\n{logs}"
        );
        assert!(
            logs.contains(SCOPE),
            "the WARN names the actionable scope:\n{logs}"
        );
        assert!(logs.contains("boom"), "the WARN carries the cause:\n{logs}");
    }

    /// A different error for the same condition is not the same failure: it
    /// warns again.
    #[tokio::test]
    async fn a_changed_error_warns_again() {
        let mut latch = FailureLatch::default();
        let (_, logs) = capture_logs(async {
            latch.failed("msg_card", "waiting card pin", SCOPE, "boom");
            latch.failed("msg_card", "waiting card pin", SCOPE, "boom");
            latch.failed("msg_card", "waiting card pin", SCOPE, "different");
        })
        .await;

        assert_eq!(
            level_count(&logs, "failed", "WARN"),
            2,
            "the changed error warns again:\n{logs}"
        );
        assert_eq!(
            level_count(&logs, "still failing", "DEBUG"),
            1,
            "only the identical repeat is DEBUG:\n{logs}"
        );
    }

    /// A success after a failure logs INFO exactly once; a success with nothing
    /// latched is silent.
    #[tokio::test]
    async fn a_success_logs_the_recovery_once() {
        let mut latch = FailureLatch::default();
        let (_, logs) = capture_logs(async {
            latch.succeeded("msg_card", "waiting card pin");
            latch.failed("msg_card", "waiting card pin", SCOPE, "boom");
            latch.succeeded("msg_card", "waiting card pin");
            latch.succeeded("msg_card", "waiting card pin");
        })
        .await;

        assert_eq!(
            level_count(&logs, "recovered", "INFO"),
            1,
            "recovery logs once, a later success is silent:\n{logs}"
        );
    }

    /// A loop-level condition has no finer key and no Feishu scope: the
    /// operation name labels the line and the scope segment is left out (the
    /// shape `PollLoop` uses for the background flows).
    #[tokio::test]
    async fn a_keyless_condition_renders_its_name_alone() {
        let mut latch = FailureLatch::default();
        let (_, logs) = capture_logs(async {
            latch.failed("", "server reconcile", "", "boom");
            latch.failed("", "server reconcile", "", "boom");
            latch.succeeded("", "server reconcile");
        })
        .await;

        assert_eq!(
            level_count(&logs, "server reconcile failed", "WARN"),
            1,
            "one WARN for the loop's condition:\n{logs}"
        );
        assert_eq!(
            level_count(&logs, "server reconcile still failing", "DEBUG"),
            1,
            "the repeat is DEBUG:\n{logs}"
        );
        assert_eq!(
            level_count(&logs, "server reconcile recovered", "INFO"),
            1,
            "a success clears the latch at INFO:\n{logs}"
        );
        assert!(
            !logs.contains("scope ;"),
            "no empty scope segment is rendered:\n{logs}"
        );
    }

    /// The latch is per condition: one key's failure never suppresses another's
    /// WARN, and one key's recovery never clears another's.
    #[tokio::test]
    async fn keys_are_independent() {
        let mut latch = FailureLatch::default();
        let (_, logs) = capture_logs(async {
            latch.failed("msg_a", "waiting card pin", SCOPE, "boom");
            latch.failed("msg_b", "waiting card pin", SCOPE, "boom");
            latch.succeeded("msg_a", "waiting card pin");
            latch.failed("msg_b", "waiting card pin", SCOPE, "boom");
        })
        .await;

        assert_eq!(
            level_count(&logs, "failed", "WARN"),
            2,
            "each key warns on its own first failure:\n{logs}"
        );
        assert_eq!(level_count(&logs, "recovered", "INFO"), 1, "{logs}");
    }
}
