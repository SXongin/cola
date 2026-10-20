//! The Turn Diff index: one read, at the Turn's settle (spec #797, ticket
//! #477; ADR-0087).
//!
//! The card's `📝 本轮期间工作区变化 · N files · +A −D` line and its
//! complete-diff button read the SAME source as the click's HTML artifact: V2's
//! `session.diff` scoped by the Turn's user-message anchor.
//! [`settle_turn_diff_stats`] is that one read. It runs at a terminal ending,
//! outside every card lock, bounded by the caller; a non-empty answer is the
//! index and an empty answer, a failure or a timeout is no index at all. The
//! card therefore never claims a change the artifact cannot show (issue #830).
//!
//! The read diffs two **worktree** snapshots, so the numbers are the worktree's
//! change during the Turn's window, not an authorship claim: a concurrent
//! session, an editor or a git operation in the same directory lands in the
//! same diff, and the API has no write provenance to tell them apart (#844).
//! The copy names that measurement, and the artifact's subtitle repeats it.
//!
//! **V2-only.** V1 has no `session.diff` endpoint, so it has no Turn Diff at
//! all: no index, no button, no artifact fallback. #800's tool-metadata
//! aggregation and its artifact fallback were deleted with this decision — a
//! second source is exactly what produced the mismatch. The Tool Panel's own
//! per-call diff is a different surface and is unaffected.

use crate::backend::{Backend, TurnAnchor, TurnDiffStats};

/// The card's change index, resolved once at a terminal ending.
///
/// `None` means the card renders neither the line nor the button: there is no
/// anchor to scope the Turn, the generation has no `session.diff` (`Ok(None)`),
/// the server answered `data: []` (a net-zero Turn), or the read failed or timed
/// out. An empty answer is never replaced by a tool-metadata estimate.
///
/// `context = 0` keeps the read to the counts' cost: this is a settle step on
/// every terminal V2 card, not only on a click. The artifact's own click reads
/// again with `context = 3` for its hunks.
pub(crate) async fn settle_turn_diff_stats(
    backend: &dyn Backend,
    session_id: &str,
    anchor: Option<&TurnAnchor>,
    timeout_ms: u64,
) -> Option<TurnDiffStats> {
    let anchor = anchor?;
    let from = anchor.message_id.as_str();
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        backend.session_diff(session_id, Some(from), None, Some(0)),
    )
    .await;
    match read {
        Ok(Ok(Some(diff))) if !diff.is_empty() => Some(diff.stats()),
        // A successful empty answer is final: a Turn whose edits cancelled out
        // has no entry (issue #830). V1's `Ok(None)` is the same no-entry arm.
        Ok(Ok(_)) => None,
        Ok(Err(error)) => {
            tracing::warn!("turn diff index: session diff read failed: {error}");
            None
        }
        Err(_) => {
            tracing::warn!(
                "turn diff index: session diff read timed out after {timeout_ms}ms; the terminal card carries no index"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MessageId, TurnDiff, TurnDiffFile};
    use crate::bridge::test_support::MockBackend;

    fn anchor() -> TurnAnchor {
        TurnAnchor {
            message_id: MessageId::new("msg_user"),
            created_ms: 1_000,
        }
    }

    fn diff_file(path: &str, additions: usize, deletions: usize) -> TurnDiffFile {
        TurnDiffFile {
            path: path.to_string(),
            patch: String::new(),
            additions,
            deletions,
            status: "modified".to_string(),
        }
    }

    /// The settle read asks for the Turn's own snapshot, with no `to` and
    /// `context = 0`, and the index is exactly the server's answer.
    #[tokio::test]
    async fn the_index_is_the_turns_session_diff_read_at_zero_context() {
        let mut backend = MockBackend::new(Vec::new());
        backend.given_session_diff(TurnDiff(vec![
            diff_file("src/a.rs", 2, 1),
            diff_file("src/b.rs", 0, 3),
        ]));

        let stats = settle_turn_diff_stats(&backend, "ses_1", Some(&anchor()), 1_000)
            .await
            .expect("a non-empty answer is the index");

        assert_eq!(
            stats,
            TurnDiffStats {
                files: 2,
                additions: 2,
                deletions: 4,
            }
        );
        assert_eq!(
            backend.session_diff_calls.lock().await.as_slice(),
            &[("ses_1".to_string(), Some("msg_user".to_string()), None, Some(0))]
        );
    }

    /// Issue #830: the authority's empty answer is final. It must not be
    /// replaced by anything derived from the transcript (which is now gone from
    /// this path entirely).
    #[tokio::test]
    async fn an_empty_answer_yields_no_index() {
        let mut backend = MockBackend::new(Vec::new());
        backend.given_session_diff(TurnDiff::default());

        assert!(
            settle_turn_diff_stats(&backend, "ses_1", Some(&anchor()), 1_000)
                .await
                .is_none()
        );
    }

    /// V1 has no endpoint: the mock's `Ok(None)` arm is a no-entry, and the read
    /// still asked the backend (the mock records it, the real client answers
    /// without a request).
    #[tokio::test]
    async fn a_generation_without_the_endpoint_yields_no_index() {
        let backend = MockBackend::new(Vec::new());

        assert!(
            settle_turn_diff_stats(&backend, "ses_1", Some(&anchor()), 1_000)
                .await
                .is_none()
        );
    }

    /// An anchorless card cannot scope a Turn, so it spends no read and gets no
    /// index.
    #[tokio::test]
    async fn an_anchorless_card_spends_no_read() {
        let backend = MockBackend::new(Vec::new());

        assert!(
            settle_turn_diff_stats(&backend, "ses_1", None, 1_000)
                .await
                .is_none()
        );
        assert!(backend.session_diff_calls.lock().await.is_empty());
    }

    /// A hung read is abandoned at the caller's bound and leaves no index; the
    /// terminal card must not wait on an unhealthy server.
    #[tokio::test]
    async fn a_hung_read_times_out_to_no_index() {
        let mut backend = MockBackend::new(Vec::new());
        backend.given_session_diff(TurnDiff(vec![diff_file("src/a.rs", 1, 0)]));
        backend
            .hang_session_diff
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let started = std::time::Instant::now();
        assert!(
            settle_turn_diff_stats(&backend, "ses_1", Some(&anchor()), 20)
                .await
                .is_none()
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(10),
            "the read was actually awaited"
        );
        assert_eq!(
            backend.session_diff_calls.lock().await.len(),
            1,
            "the attempt is recorded before the timeout"
        );
    }
}
