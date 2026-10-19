//! The Turn card's change index (spec #797, ticket #477).
//!
//! A Turn that edited files gets one markdown line — `📝 本次改动 · N files ·
//! +A −D` — and one button — `🔍 查看完整改动` — in its live card's tail, after
//! the tool panels and before the footer. The line is only an index: the
//! complete red/green diff is the HTML artifact (spec decision 1), so no patch
//! body ever rides the card.
//!
//! The counts are a **transcript-side estimate**, not the artifact's read: the
//! [`TurnDiff::stats`](crate::backend::TurnDiff::stats) fold over #800's
//! aggregation of the Turn's recorded tool patches, deduped by path with the
//! last recorded patch per path winning. #477 forbids a network read at render
//! time, so on V2 this is deliberately not the same source the click uses:
//! opening the button fetches the authoritative `session.diff` for the Turn
//! (turn start → now), and that artifact can report larger counts when a file
//! was edited more than once or changed outside a file-editing tool (a
//! formatter or `git checkout` in `bash`). V1's artifact falls back to this
//! same aggregation, so only V2 has the gap. The artifact's numbers are the
//! ones that count; the index pays this fidelity for a render that never waits
//! on the server.
//!
//! The button's payload names the Turn by its
//! [`TurnAnchor`](crate::backend::TurnAnchor) user message (`from`), which is
//! exactly the `session.diff` range the click asks for; a card with no anchor
//! renders neither element, because an entry that cannot name its Turn would
//! open another Turn's diff.
//!
//! The counts line is live, but the **button waits for the card's terminal
//! state**. The artifact is generated once per `(session, from)` and the first
//! successful send is final, so offering the button mid-turn would freeze a
//! partial snapshot that later edits cannot update. The line may keep counting
//! while the Turn runs; only the terminal card offers the one-shot entry.
//!
//! [`index_elements`] is the one renderer and [`index_estimate`] measures its
//! own output (`serde_json` bytes), so the card-size estimate charges exactly
//! what the card carries — the split decision cannot regress by forgetting the
//! line or the button.

use crate::backend::TurnDiffStats;

/// The button's copy. Named once so the tests and the estimate cannot read a
/// different string from the render.
pub(crate) const TURN_DIFF_BUTTON_TEXT: &str = "🔍 查看完整改动";

/// The callback op that names this button's click (`op: "turn_diff"`).
pub(crate) const TURN_DIFF_OP: &str = "turn_diff";

/// How many body elements [`index_elements`] pushes for a settled (or still
/// running) card: the markdown line always, the button only once the Turn's
/// card is terminal. The split estimator charges exactly this count.
pub(crate) fn index_components(show_button: bool) -> usize {
    1 + usize::from(show_button)
}

/// The index line, from the transcript-side estimate (#800 aggregation, last
/// recorded patch per path) — the artifact's own read may be larger on V2.
pub(crate) fn index_line(stats: TurnDiffStats) -> String {
    format!(
        "📝 本次改动 · {} files · +{} −{}",
        stats.files, stats.additions, stats.deletions
    )
}

/// The card-tail elements for one Turn's change index: the line, then — only
/// once the card is terminal — the complete-diff button. `session_id`, `from`
/// (the Turn anchor's user message id) and `directory` are the click's whole
/// routing payload — the handler needs no card lookup (spec decision 2).
pub(crate) fn index_elements(
    stats: TurnDiffStats,
    session_id: &str,
    from: &str,
    directory: &str,
    show_button: bool,
) -> Vec<serde_json::Value> {
    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": index_line(stats),
    })];
    if show_button {
        elements.push(serde_json::json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": TURN_DIFF_BUTTON_TEXT },
            "type": "default",
            "value": {
                "op": TURN_DIFF_OP,
                "session_id": session_id,
                "from": from,
                "directory": directory,
            },
        }));
    }
    elements
}

/// The serialized JSON bytes [`index_elements`] adds to a card — measured from
/// those same elements, so the estimate and the render cannot drift.
pub(crate) fn index_estimate(
    stats: TurnDiffStats,
    session_id: &str,
    from: &str,
    directory: &str,
    show_button: bool,
) -> usize {
    index_elements(stats, session_id, from, directory, show_button)
        .iter()
        .filter_map(|element| serde_json::to_string(element).ok())
        .map(|serialized| serialized.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> TurnDiffStats {
        TurnDiffStats {
            files: 2,
            additions: 3,
            deletions: 1,
        }
    }

    /// The acceptance copy, byte for byte: files, additions and deletions are
    /// the #800 stats, and the minus is U+2212 so the line reads like the
    /// artifact's counts.
    #[test]
    fn the_index_line_names_the_files_and_line_counts() {
        assert_eq!(index_line(stats()), "📝 本次改动 · 2 files · +3 −1");
        assert_eq!(
            index_line(TurnDiffStats::default()),
            "📝 本次改动 · 0 files · +0 −0"
        );
    }

    /// The click payload is the ticket's exact shape — `from` is the Turn
    /// anchor's user message id, and no other key rides along.
    #[test]
    fn the_button_carries_the_exact_turn_diff_payload() {
        let elements = index_elements(stats(), "ses_123", "msg_user_9", "/repo/cola", true);
        let button = &elements[1];

        assert_eq!(button["tag"], "button");
        assert_eq!(button["text"]["content"], TURN_DIFF_BUTTON_TEXT);
        assert_eq!(
            button["value"],
            serde_json::json!({
                "op": "turn_diff",
                "session_id": "ses_123",
                "from": "msg_user_9",
                "directory": "/repo/cola",
            })
        );
        assert_eq!(
            button["value"].as_object().map(|value| value.len()),
            Some(4),
            "the payload carries exactly op/session_id/from/directory"
        );
    }

    /// A running card keeps the live counts line but offers no entry: the
    /// one-shot artifact must not freeze a mid-turn snapshot.
    #[test]
    fn a_running_card_renders_the_line_without_the_button() {
        let elements = index_elements(stats(), "ses_123", "msg_user_9", "/repo/cola", false);

        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0]["tag"], "markdown");
        assert_eq!(elements[0]["content"], index_line(stats()));
        assert!(
            !elements
                .iter()
                .any(|element| element["value"]["op"] == TURN_DIFF_OP),
            "no button before the Turn's card is terminal"
        );
    }

    /// The estimate is measured from the rendered elements, so a split decision
    /// charges exactly the elements present — both on a terminal card, only the
    /// line while the Turn is still running.
    #[test]
    fn the_estimate_charges_exactly_the_elements_it_renders() {
        for show_button in [false, true] {
            let elements = index_elements(stats(), "ses_123", "msg_user_9", "/repo/\"quoted\"", show_button);
            let rendered: usize = elements
                .iter()
                .map(|element| serde_json::to_string(element).unwrap().len())
                .sum();

            assert_eq!(
                index_estimate(stats(), "ses_123", "msg_user_9", "/repo/\"quoted\"", show_button),
                rendered
            );
            assert!(rendered > 0);
            assert_eq!(index_components(show_button), elements.len());
            assert_eq!(elements.len(), 1 + usize::from(show_button));
        }
    }
}
