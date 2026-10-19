//! The Turn card's change index (spec #797, ticket #477).
//!
//! A Turn that edited files gets one markdown line — `📝 本次改动 · N files ·
//! +A −D` — and one button — `🔍 查看完整改动` — in its live card's tail, after
//! the tool panels and before the footer. The line is only an index: the
//! complete red/green diff is the HTML artifact (spec decision 1), so no patch
//! body ever rides the card.
//!
//! The counts are the [`TurnDiff::stats`](crate::backend::TurnDiff::stats) fold
//! over the #800 aggregation — the same deduped entries the artifact renders —
//! so the index and what the reader opens cannot disagree. The button's payload
//! names the Turn by its [`TurnAnchor`](crate::backend::TurnAnchor) user message
//! (`from`), which is exactly the `session.diff` range the click asks for; a
//! card with no anchor renders neither element, because an entry that cannot
//! name its Turn would open another Turn's diff.
//!
//! [`index_elements`] is the one renderer and [`index_estimate`] measures its
//! own output (`serde_json` bytes), so the card-size estimate charges exactly
//! what the card carries — the split decision cannot regress by forgetting the
//! two new elements.

use crate::backend::TurnDiffStats;

/// The button's copy. Named once so the tests and the estimate cannot read a
/// different string from the render.
pub(crate) const TURN_DIFF_BUTTON_TEXT: &str = "🔍 查看完整改动";

/// The callback op that names this button's click (`op: "turn_diff"`).
pub(crate) const TURN_DIFF_OP: &str = "turn_diff";

/// How many body elements [`index_elements`] pushes: the markdown line and the
/// button. The split estimator charges them together, never one alone.
pub(crate) const INDEX_COMPONENTS: usize = 2;

/// The index line, from the deduped stats the artifact also uses.
pub(crate) fn index_line(stats: TurnDiffStats) -> String {
    format!(
        "📝 本次改动 · {} files · +{} −{}",
        stats.files, stats.additions, stats.deletions
    )
}

/// The card-tail elements for one Turn's change index: the line, then the
/// complete-diff button. `session_id`, `from` (the Turn anchor's user message
/// id) and `directory` are the click's whole routing payload — the handler
/// needs no card lookup (spec decision 2).
pub(crate) fn index_elements(
    stats: TurnDiffStats,
    session_id: &str,
    from: &str,
    directory: &str,
) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "tag": "markdown",
            "content": index_line(stats),
        }),
        serde_json::json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": TURN_DIFF_BUTTON_TEXT },
            "type": "default",
            "value": {
                "op": TURN_DIFF_OP,
                "session_id": session_id,
                "from": from,
                "directory": directory,
            },
        }),
    ]
}

/// The serialized JSON bytes [`index_elements`] adds to a card — measured from
/// those same elements, so the estimate and the render cannot drift.
pub(crate) fn index_estimate(stats: TurnDiffStats, session_id: &str, from: &str, directory: &str) -> usize {
    index_elements(stats, session_id, from, directory)
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
        let elements = index_elements(stats(), "ses_123", "msg_user_9", "/repo/cola");
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

    /// The estimate is measured from the rendered elements, so a split decision
    /// charges the two new body elements (and a quote-heavy directory's JSON
    /// escaping) exactly.
    #[test]
    fn the_estimate_charges_exactly_the_elements_it_renders() {
        let elements = index_elements(stats(), "ses_123", "msg_user_9", "/repo/\"quoted\"");
        let rendered: usize = elements
            .iter()
            .map(|element| serde_json::to_string(element).unwrap().len())
            .sum();

        assert_eq!(
            index_estimate(stats(), "ses_123", "msg_user_9", "/repo/\"quoted\""),
            rendered
        );
        assert!(rendered > 0);
        assert_eq!(INDEX_COMPONENTS, elements.len());
    }
}
