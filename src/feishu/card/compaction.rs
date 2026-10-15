//! The **Compaction** row (ADR-0082, spec #719): how one compaction renders on
//! a card. A Session-scoped fact — it answers no prompt and is never a Turn
//! anchor — so its row is owned by the card timeline, which places it at the
//! compaction's own server time rather than at the card's end. The Session
//! Snapshot's 最近对话 tail (ticket #723) renders the same row through
//! [`compaction_elements`], so the two surfaces cannot disagree.
//!
//! Three shapes, one row: an in-flight compaction is a live line that updates in
//! place when the Backend writes the summary, a completed one is a folded panel
//! whose body is that summary, a failed one is a visible line carrying the
//! Backend's error. `recent` — the verbatim tail V2 keeps beside the summary —
//! is never rendered: it is history the reader already has.

use crate::backend::{Compaction, CompactionReason, CompactionStatus};
use crate::feishu::card::sanitize::{CardMarkdown, serialized_len};
use crate::feishu::card::shell::collapsible_panel_chunks;
use crate::feishu::card::{MAX_CARD_TEXT_CHARS, MAX_ELEMENT_TEXT_CHARS, chunk_text, format_tokens};
use serde_json::json;

/// The card elements one Compaction row is made of (ADR-0082), shared by every
/// surface that shows a row — the live card's timeline
/// ([`crate::feishu::card::shell::CardBuilder::with_compaction`]) and the Session
/// Snapshot's 最近对话 tail (ticket #723) — so the two cannot disagree on the
/// copy, the fold, or the session time they render.
///
/// A completed compaction is one folded panel: `element_id` its stable identity
/// (the reader's fold state survives an in-place update), the panel's title the
/// row's, the fold body [`compaction_fold_body`]'s — the summary capped to one
/// row's budget ([`MAX_COMPACTION_SUMMARY_CHARS`]) with its truncation marker,
/// then chunked to cola's per-element budget. With nothing to fold — an
/// in-flight compaction, a failure, or a completion the Backend gave no text (an
/// orphaned V1 part, say) — the row is its title alone, and a failure's error is
/// capped to that one element's budget inside the title ([`clipped_error`]).
/// `md` is the card's markdown state, so the summary draws from the card's one
/// table budget and fenced fallback.
pub(crate) fn compaction_elements(
    compaction: &Compaction,
    element_id: Option<&str>,
    md: &mut CardMarkdown,
) -> Vec<serde_json::Value> {
    let title = compaction_title(compaction);
    match compaction_fold_body(compaction) {
        Some(body) => {
            let chunks: Vec<String> = body.iter().map(|chunk| md.element(chunk)).collect();
            vec![collapsible_panel_chunks(&title, &chunks, element_id)]
        }
        None => vec![json!({ "tag": "markdown", "content": md.element(&title) })],
    }
}

/// Whether two compaction facts render the SAME row (ADR-0082): everything the
/// row shows — how it ended, why it ran, the summary, the failure's error and the
/// usage it reports, presence included. `recent` is deliberately not compared:
/// the card never renders it, so a refreshed `recent` alone is not a change on
/// the card.
///
/// This is the row's rendered revision, and POSITION is not part of it: what a
/// card holds is decided per transcript position (two distinct compactions with
/// identical fields are two rows), so the comparison belongs beside the copy it
/// must agree with rather than in a key of its own.
pub(crate) fn same_row(left: &Compaction, right: &Compaction) -> bool {
    left.status == right.status
        && left.reason == right.reason
        && left.summary == right.summary
        && left.error == right.error
        && left.usage.map(|usage| usage.total) == right.usage.map(|usage| usage.total)
}

/// The title one Compaction row carries (ADR-0082): the reason it ran, and the
/// usage of the compaction's OWN request as a trailing segment when the server
/// reports one. A completion carrying `providerContext` — V2's provider-native
/// checkpoint — is labelled no differently: that is the mechanism, not the
/// domain fact. A failure's error is clipped like the fold body is
/// ([`clipped_error`]): the title is ONE card element and the row is one
/// unsplittable timeline item, so an oversized error would make the card's
/// minimum one-row slice exceed the whole-card budget — and the failure row, the
/// case the reader most needs, would vanish with it.
pub(crate) fn compaction_title(compaction: &Compaction) -> String {
    let reason = match compaction.reason {
        CompactionReason::Auto => "自动",
        CompactionReason::Manual => "手动",
    };
    // Presence, not magnitude: a request the server reports with a zero usage is
    // still a reported usage, and the row says so.
    let suffix = match compaction.usage.map(|usage| usage.total) {
        Some(total) => format!(" · {} tokens", format_tokens(total)),
        None => String::new(),
    };
    match compaction.status {
        CompactionStatus::Running => format!("⏳ 压缩中（{reason}）{suffix}"),
        CompactionStatus::Completed => format!("已压缩上下文（{reason}）{suffix}"),
        CompactionStatus::Failed => {
            // One element, one budget: the prefix, the error — the clip's marker
            // included — and the usage suffix all share
            // [`MAX_ELEMENT_TEXT_CHARS`], measured rather than reserved.
            let budget = MAX_ELEMENT_TEXT_CHARS
                .saturating_sub(FAILED_TITLE_PREFIX.chars().count() + suffix.chars().count());
            let error = compaction
                .error
                .as_deref()
                .filter(|error| !error.is_empty())
                .unwrap_or("未知错误");
            format!("{FAILED_TITLE_PREFIX}{}{suffix}", clipped_error(error, budget))
        }
    }
}

/// The error text a failure title shows: the whole diagnostic when it fits
/// `budget` characters, else its longest prefix such that the prefix, the clip's
/// line break and [`ERROR_TRUNCATED_MARKER`] fit that budget — the marker is
/// INSIDE the budget, never on top of it. The stored `Compaction.error` keeps the
/// Backend's whole text: this caps what the card SHOWS, exactly as the fold body's
/// cap does for the summary.
fn clipped_error(error: &str, budget: usize) -> String {
    if error.chars().count() <= budget {
        return error.to_string();
    }
    let marker = format!("\n{ERROR_TRUNCATED_MARKER}");
    let keep = budget.saturating_sub(marker.chars().count());
    format!("{}{marker}", error.chars().take(keep).collect::<String>())
}

/// How much of a compaction's summary one row's fold shows, in CHARACTERS.
///
/// A Compaction row is a single timeline entry, and the card splitter can only
/// break BETWEEN entries — never inside one — so this row's own body has to fit
/// one card by itself. [`MAX_CARD_TEXT_CHARS`] is exactly the text budget the
/// splitter already gives a whole card of streamed text: capping the fold there
/// means one row costs at most one card's worth of text and nothing more. A
/// worst-case CJK summary at this cap serializes to ~18 KB (three bytes per
/// character; [`serde_json`] encodes non-ASCII as UTF-8, never as `\u` escapes),
/// which sits inside [`MAX_CARD_JSON_CHARS`](crate::feishu::card::MAX_CARD_JSON_CHARS)
/// (25 KB) once the panel, title, header and footer are added — and well inside
/// Feishu's 30 KB hard limit. The stored [`Part::Compaction`](crate::backend::Part)
/// keeps its summary whole: this caps what the card SHOWS.
pub(crate) const MAX_COMPACTION_SUMMARY_CHARS: usize = MAX_CARD_TEXT_CHARS;

/// The bytes one row's fold body may SERIALIZE to: the escaped cost a worst-case
/// CJK body at [`MAX_COMPACTION_SUMMARY_CHARS`] already carries (three bytes per
/// character, which is what the character cap was sized for). The body is bounded
/// by both — the character cap keeps an ordinary summary whole, and this byte cap
/// keeps the serialized size inside one card when the sanitizer EXPANDS the text:
/// a `<` becomes the five-byte `&#60;`, so a summary of markup at the character
/// cap alone would reach ~30 KB and blow the card, which is the very failure the
/// per-row cap exists to prevent. Only the tail of such a body is lost; prose is
/// untouched.
pub(crate) const MAX_COMPACTION_BODY_BYTES: usize = MAX_COMPACTION_SUMMARY_CHARS * 3;

/// The marker a clipped fold body ends with, as an element of its own so no
/// element outgrows the per-element budget.
pub(crate) const SUMMARY_TRUNCATED_MARKER: &str = "…（摘要已截断）";

/// The `⚠️ 压缩失败：` copy a failure title opens with — measured against the
/// element budget it shares with the error it introduces.
const FAILED_TITLE_PREFIX: &str = "⚠️ 压缩失败：";

/// The marker a clipped error ends with, on a line of its own so a long single
/// line of diagnostic cannot bury it.
pub(crate) const ERROR_TRUNCATED_MARKER: &str = "…（错误已截断）";

/// The fold body one Compaction row renders, chunked at the card's per-element
/// budget: the summary clipped to [`MAX_COMPACTION_SUMMARY_CHARS`] with an
/// explicit [`SUMMARY_TRUNCATED_MARKER`] element when it was cut. `None` when
/// there is nothing to fold — a running or failed compaction, and a completed one
/// the Backend never gave a text (an orphaned V1 part, say): the row is then its
/// title alone, rather than a fold holding nothing.
///
/// The ONE source the render, [`compaction_components`] and
/// [`compaction_estimate`] read, so the three cannot drift: the caps, the chunking
/// and the marker are measured exactly as they are built.
pub(crate) fn compaction_fold_body(compaction: &Compaction) -> Option<Vec<String>> {
    let summary = compaction.summary.trim();
    if summary.is_empty() {
        return None;
    }
    let (clipped, truncated) = clipped_summary(summary);
    let mut chunks = chunk_text(&clipped, MAX_ELEMENT_TEXT_CHARS);
    if truncated {
        chunks.push(SUMMARY_TRUNCATED_MARKER.to_string());
    }
    Some(chunks)
}

/// The summary text one row's fold shows, and whether it was cut: at most
/// [`MAX_COMPACTION_SUMMARY_CHARS`] CHARACTERS and — the clip's marker included —
/// at most [`MAX_COMPACTION_BODY_BYTES`] SERIALIZED bytes ([`serialized_len`]) —
/// the character cap for an ordinary summary, the byte cap for text the card
/// encodes larger, so neither extreme can outgrow one card.
fn clipped_summary(summary: &str) -> (String, bool) {
    let chars = summary.chars().count();
    let at_char_cap = chars.min(MAX_COMPACTION_SUMMARY_CHARS);
    let head: String = summary.chars().take(at_char_cap).collect();
    if chars <= MAX_COMPACTION_SUMMARY_CHARS && serialized_len(&head) <= MAX_COMPACTION_BODY_BYTES {
        return (head, false);
    }
    // The body IS cut, so the marker rides in the same budget: search the longest
    // prefix whose body — the prefix plus the marker the card will show — fits.
    // The cost is monotone in the prefix's length, and the character cap bounds
    // the search.
    let (mut keep, mut too_long) = (0usize, at_char_cap);
    while keep < too_long {
        let mid = keep + (too_long - keep).div_ceil(2);
        let candidate: String = summary.chars().take(mid).collect();
        if serialized_len(&candidate) + serialized_len(SUMMARY_TRUNCATED_MARKER) <= MAX_COMPACTION_BODY_BYTES
        {
            keep = mid;
        } else {
            too_long = mid - 1;
        }
    }
    (summary.chars().take(keep).collect(), true)
}

/// How many body elements one Compaction row adds: a plain line is one element,
/// a folded row is the panel (as a reasoning panel charges it) plus one markdown
/// element per fold-body chunk — the truncation marker included, since it is one.
pub(crate) fn compaction_components(compaction: &Compaction) -> usize {
    match compaction_fold_body(compaction) {
        Some(chunks) => 3 + chunks.len(),
        None => 1,
    }
}

/// The bytes one Compaction row adds to the body (ADR-0082): its title, plus —
/// for a folded row — the panel's own element overhead and the body's markdown
/// chunks, measured from the same chunks the render builds so the two cannot
/// drift. The text of every charge is [`serialized_len`]'s: EXACTLY what the card
/// will carry for it (the sanitizer's `<` expansion and the JSON encoding
/// included), so no expansion vector — escaping, control characters, encoding —
/// can make a row cost more than it was charged. The fixed per-element constants
/// on top cover the element skeletons and the sanitizer's non-content wrappers (a
/// table's fence, the rejection fallback's fence), each bounded by the card text
/// it wraps. The body is charged in JSON bytes rather than as card text, like a
/// ledger entry's output: a fold body is one element per chunk and cannot be
/// split the way streamed text can. Because both are capped, a row's cost is
/// bounded whatever the stored summary holds.
pub(crate) fn compaction_estimate(compaction: &Compaction) -> usize {
    let title = serialized_len(&compaction_title(compaction));
    match compaction_fold_body(compaction) {
        None => title + 80,
        Some(chunks) => {
            300 + title
                + chunks
                    .iter()
                    .map(|chunk| serialized_len(chunk) + 40)
                    .sum::<usize>()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::card::MAX_CARD_JSON_CHARS;
    use crate::feishu::card::shell::CardBuilder;

    fn compaction(summary: &str) -> Compaction {
        Compaction {
            status: CompactionStatus::Completed,
            reason: CompactionReason::Auto,
            summary: summary.to_string(),
            usage: None,
            error: None,
            recent: "读者已经读过的最近对话".to_string(),
        }
    }

    /// A failed compaction carrying `error`.
    fn failed(error: &str) -> Compaction {
        Compaction {
            status: CompactionStatus::Failed,
            reason: CompactionReason::Auto,
            summary: String::new(),
            usage: None,
            error: Some(error.to_string()),
            recent: String::new(),
        }
    }

    /// One atom of each content kind whose card encoding differs from its raw
    /// length — the vectors that made hand-rolled size arithmetic wrong: escaped
    /// markup, CJK, control characters, JSON metacharacters, and a mix.
    fn atoms() -> [(&'static str, &'static str); 5] {
        [
            ("NUL", "\0"),
            ("markup", "<"),
            ("CJK", "摘"),
            ("quotes and backslashes", "\"\\"),
            ("mixed", "a<\0\\\"摘"),
        ]
    }

    /// The card one compaction row builds, serialized as Feishu receives it.
    fn card_json(summary: &str) -> String {
        card_of(&compaction(summary))
    }

    /// [`card_json`] for any fact.
    fn card_of(fact: &Compaction) -> String {
        CardBuilder::new()
            .with_state(crate::feishu::card::CardState::Done)
            .with_compaction(fact, Some("compaction_1"))
            .build()
            .to_string()
    }

    /// What the row's own elements serialize to — the very builder the render
    /// uses, so the charge can be held against the real payload.
    fn row_serialized_len(fact: &Compaction) -> usize {
        let mut md = CardMarkdown::new();
        compaction_elements(fact, Some("compaction_1"), &mut md)
            .iter()
            .map(|element| element.to_string().len())
            .sum()
    }

    /// What one fold body's TEXT serializes to, the truncation marker included —
    /// the quantity [`MAX_COMPACTION_BODY_BYTES`] bounds.
    fn body_serialized_len(chunks: &[String]) -> usize {
        chunks.iter().map(|chunk| serialized_len(chunk)).sum()
    }

    /// The measure is never below what the card actually carries: it applies the
    /// sanitizer's expansion AND the JSON encoding, so control characters, quotes
    /// and backslashes cost what the writer will spend on them — no per-character
    /// table to fall behind a new expansion vector.
    #[test]
    fn the_measure_covers_what_the_card_carries_for_any_content() {
        for (label, atom) in atoms() {
            for length in [1, 10, MAX_ELEMENT_TEXT_CHARS, MAX_ELEMENT_TEXT_CHARS * 5] {
                let text = atom.repeat(length);
                let mut md = CardMarkdown::new();
                let sanitized = md.element(&text);
                let carried = serde_json::to_string(&sanitized).unwrap().len() - 2;
                assert!(
                    serialized_len(&text) >= carried,
                    "{label} ×{length}: measured {} < carried {carried}",
                    serialized_len(&text)
                );
            }
        }
    }

    /// One row cannot be split by the card splitter, so its fold body is capped
    /// (ADR-0082): a summary at the cap and far past it — prose, CJK, or content
    /// the card encodes larger — each renders a card inside the whole-card JSON
    /// budget, with the body's own serialized text (the truncation marker
    /// included) inside [`MAX_COMPACTION_BODY_BYTES`]. Without the caps a long
    /// summary blows the budget and Feishu rejects the card, taking the row and
    /// the turn's update with it.
    #[test]
    fn a_summary_past_the_cap_stays_inside_both_budgets() {
        let mut cases: Vec<(String, String)> = vec![
            (
                "prose at the cap".to_string(),
                "a".repeat(MAX_COMPACTION_SUMMARY_CHARS),
            ),
            (
                "prose far past the cap".to_string(),
                "a".repeat(MAX_COMPACTION_SUMMARY_CHARS * 20),
            ),
        ];
        for (label, atom) in atoms() {
            for (scale, length) in [
                ("at the cap", MAX_COMPACTION_SUMMARY_CHARS),
                ("far past the cap", MAX_COMPACTION_SUMMARY_CHARS * 20),
            ] {
                cases.push((format!("{label} {scale}"), atom.repeat(length)));
            }
        }
        for (label, summary) in cases {
            let json = card_json(&summary);
            assert!(
                json.len() < MAX_CARD_JSON_CHARS,
                "{label}: the card must stay inside the whole-card JSON budget, got {} bytes",
                json.len()
            );
            let body = compaction_fold_body(&compaction(&summary)).expect("a folded body");
            assert!(
                body_serialized_len(&body) <= MAX_COMPACTION_BODY_BYTES,
                "{label}: the body — marker included — must fit its own byte budget, got {} bytes",
                body_serialized_len(&body)
            );
        }
    }

    /// A failure's error is capped the same way: at and past its cap, for every
    /// content kind, the card stays inside the whole-card budget and the title
    /// stays ONE element inside the per-element budget.
    #[test]
    fn a_failure_error_past_the_cap_stays_inside_the_card_budget() {
        let mut cases: Vec<(String, String)> = vec![
            (
                "prose at the cap".to_string(),
                "字".repeat(MAX_ELEMENT_TEXT_CHARS),
            ),
            (
                "prose far past the cap".to_string(),
                "字".repeat(MAX_ELEMENT_TEXT_CHARS * 100),
            ),
            (
                "a stack trace far past the cap".to_string(),
                "Error: boom\n    at src/main.rs:1\n".repeat(MAX_ELEMENT_TEXT_CHARS),
            ),
        ];
        for (label, atom) in atoms() {
            for (scale, length) in [
                ("at the cap", MAX_ELEMENT_TEXT_CHARS),
                ("far past the cap", MAX_ELEMENT_TEXT_CHARS * 100),
            ] {
                cases.push((format!("{label} {scale}"), atom.repeat(length)));
            }
        }
        for (label, error) in cases {
            let fact = failed(&error);
            let json = card_of(&fact);
            assert!(
                json.len() < MAX_CARD_JSON_CHARS,
                "{label}: the card must stay inside the whole-card JSON budget, got {} bytes",
                json.len()
            );
            let title = compaction_title(&fact);
            assert!(
                title.chars().count() <= MAX_ELEMENT_TEXT_CHARS,
                "{label}: the title must stay one budgeted element, got {} chars",
                title.chars().count()
            );
        }
    }

    /// The charge is never an UNDER-charge: for every content kind, at and past
    /// each cap, the estimate covers what the row's elements actually serialize
    /// to — measured from the very builder the render uses — so the splitter
    /// cannot be told a card fits when it does not.
    #[test]
    fn the_estimate_never_undercharges_a_row() {
        let mut cases: Vec<(String, Compaction)> = vec![
            ("a prose summary".to_string(), compaction("保留了结论。")),
            ("a short error".to_string(), failed("context length exceeded")),
        ];
        for (label, atom) in atoms() {
            for (scale, length) in [
                ("at the cap", MAX_COMPACTION_SUMMARY_CHARS),
                ("past the cap", MAX_COMPACTION_SUMMARY_CHARS * 5),
            ] {
                cases.push((
                    format!("{label} summary {scale}"),
                    compaction(&atom.repeat(length)),
                ));
            }
            for (scale, length) in [
                ("at the cap", MAX_ELEMENT_TEXT_CHARS),
                ("past the cap", MAX_ELEMENT_TEXT_CHARS * 5),
            ] {
                cases.push((format!("{label} error {scale}"), failed(&atom.repeat(length))));
            }
        }
        for (label, fact) in cases {
            let serialized = row_serialized_len(&fact);
            assert!(
                compaction_estimate(&fact) >= serialized,
                "{label}: the estimate ({}) must cover the row's serialized {serialized} bytes",
                compaction_estimate(&fact)
            );
        }
    }

    /// Markup inside a fence is expanded by the measure and not by the card —
    /// the conservative side — and the row still lands inside the budget both on
    /// the first attempt and on the fenced rebuild a rejected card is retried as.
    #[test]
    fn a_markup_laden_summary_lands_inside_the_budget_through_the_fenced_fallback() {
        let summary = format!("```\n{}\n```", "<".repeat(MAX_COMPACTION_SUMMARY_CHARS));
        let fenced = CardBuilder::new()
            .with_state(crate::feishu::card::CardState::Done)
            .with_fenced_markdown(true)
            .with_compaction(&compaction(&summary), Some("compaction_1"))
            .build()
            .to_string();
        assert!(
            fenced.len() < MAX_CARD_JSON_CHARS,
            "the fenced rebuild must fit the budget, got {} bytes",
            fenced.len()
        );
    }

    /// The capped body keeps the head of the summary and says it was cut, with
    /// the marker in an element of its own so no element outgrows the per-element
    /// budget. A summary within the caps renders whole, with no marker.
    #[test]
    fn a_clipped_fold_body_keeps_the_head_and_names_the_truncation() {
        let summary = "摘".repeat(MAX_COMPACTION_SUMMARY_CHARS * 2);
        let body = compaction_fold_body(&compaction(&summary)).expect("a folded body");
        assert_eq!(
            body.last().map(String::as_str),
            Some(SUMMARY_TRUNCATED_MARKER),
            "the clip is named, never silent: {} chunks",
            body.len()
        );
        assert!(
            body.iter()
                .all(|chunk| chunk.chars().count() <= MAX_ELEMENT_TEXT_CHARS),
            "the marker rides its own element: {} chunks",
            body.len()
        );
        let visible: String = body[..body.len() - 1].concat();
        assert!(
            summary.starts_with(&visible),
            "the head of the summary is kept, in order"
        );
        assert!(
            visible.chars().count() < MAX_COMPACTION_SUMMARY_CHARS,
            "the byte budget, not the character cap, cut this body"
        );
        let mut one_more = visible.clone();
        one_more.push('摘');
        assert!(
            serialized_len(&one_more) + serialized_len(SUMMARY_TRUNCATED_MARKER) > MAX_COMPACTION_BODY_BYTES,
            "the head is as long as the byte budget — the marker included — allows"
        );

        let short = "保留了结论。";
        assert_eq!(
            compaction_fold_body(&compaction(short)).unwrap().concat(),
            short,
            "a summary within the cap is rendered whole"
        );
    }

    /// The clip stops at the BYTE cap with the marker already counted: a body of
    /// pure `<` shows as many characters as the escaped budget allows less the
    /// marker's own cost, and a NUL-only body (six bytes per character) stops
    /// where its encoding, not its length, says.
    #[test]
    fn the_clip_counts_the_marker_inside_the_byte_budget() {
        for (label, atom, per_char) in [("markup", "<", 5usize), ("NUL", "\0", 6)] {
            let summary = atom.repeat(MAX_COMPACTION_SUMMARY_CHARS);
            let body = compaction_fold_body(&compaction(&summary)).expect("a folded body");
            let body_bytes = body_serialized_len(&body);
            assert!(
                body_bytes <= MAX_COMPACTION_BODY_BYTES,
                "{label}: the body — marker included — must fit, got {body_bytes} bytes"
            );
            assert_eq!(
                body.last().map(String::as_str),
                Some(SUMMARY_TRUNCATED_MARKER),
                "{label}: the byte cap cuts this body"
            );
            let visible: String = body[..body.len() - 1].concat();
            let marker = serialized_len(SUMMARY_TRUNCATED_MARKER);
            assert_eq!(
                serialized_len(&visible),
                MAX_COMPACTION_BODY_BYTES - marker - (MAX_COMPACTION_BODY_BYTES - marker) % per_char,
                "{label}: the head is kept up to the byte budget, less the marker"
            );
        }
    }

    /// The clipped failure title keeps the head of the error and names the
    /// truncation — the marker INSIDE the element budget, never on top of it —
    /// leaves a short error (the ordinary case) exactly as the Backend wrote it,
    /// and keeps the usage suffix on the row.
    #[test]
    fn a_clipped_error_keeps_the_head_and_names_the_truncation() {
        let error = "字".repeat(MAX_ELEMENT_TEXT_CHARS * 3);
        let mut fact = failed(&error);
        fact.usage = Some(crate::backend::TokenUsage {
            total: 20_000_000,
            ..Default::default()
        });
        let title = compaction_title(&fact);
        assert!(
            title.chars().count() <= MAX_ELEMENT_TEXT_CHARS,
            "the widest title must stay one budgeted element, got {} chars",
            title.chars().count()
        );
        assert!(
            title.contains(ERROR_TRUNCATED_MARKER),
            "the clip is named, never silent"
        );
        let head = title
            .split(ERROR_TRUNCATED_MARKER)
            .next()
            .expect("the text before the marker");
        assert!(head.contains(&"字".repeat(100)), "the head of the error is kept");
        assert!(
            !title.contains(&"字".repeat(title.chars().count())),
            "unreachable: the title is shorter than its own length"
        );
        assert!(
            title.ends_with(" · 20M tokens"),
            "the usage suffix still closes the row"
        );

        assert_eq!(
            compaction_title(&failed("context length exceeded")),
            "⚠️ 压缩失败：context length exceeded",
            "a short error renders exactly as the Backend wrote it"
        );
        assert_eq!(
            compaction_title(&failed("")),
            "⚠️ 压缩失败：未知错误",
            "a failure with no error text says so"
        );
    }

    /// The component count and the size estimate measure the SAME capped body the
    /// render builds: past the caps a summary costs no more than one at the caps
    /// plus the marker's own element, so the splitter charges a bounded row.
    #[test]
    fn the_row_cost_cannot_outgrow_the_capped_body() {
        let at_cap = compaction(&"摘".repeat(MAX_COMPACTION_SUMMARY_CHARS));
        let past_cap = compaction(&"摘".repeat(MAX_COMPACTION_SUMMARY_CHARS * 50));
        let further = compaction(&"摘".repeat(MAX_COMPACTION_SUMMARY_CHARS * 500));
        let marker_cost = serialized_len(SUMMARY_TRUNCATED_MARKER) + 40;
        assert!(
            compaction_estimate(&past_cap) <= compaction_estimate(&at_cap) + marker_cost,
            "the estimate is the capped body's (plus its marker element at most), not the \
             stored summary's"
        );
        assert_eq!(
            compaction_estimate(&further),
            compaction_estimate(&past_cap),
            "past the cap the row's cost cannot grow"
        );
        assert_eq!(
            compaction_components(&past_cap),
            compaction_components(&at_cap) + 1
        );
        assert_eq!(
            compaction_components(&at_cap),
            3 + MAX_COMPACTION_SUMMARY_CHARS.div_ceil(MAX_ELEMENT_TEXT_CHARS),
            "the panel plus one element per chunk"
        );
        assert!(
            compaction_estimate(&further) + serialized_len(&compaction_title(&further)) < MAX_CARD_JSON_CHARS,
            "one row leaves room for a card's own chrome"
        );

        // A failure row is a plain line, and its cost is the CLIPPED title's: an
        // error far past the caps costs no more than one at the cap.
        let failed_at_cap = failed(&"字".repeat(MAX_ELEMENT_TEXT_CHARS));
        let failed_past_cap = failed(&"字".repeat(MAX_ELEMENT_TEXT_CHARS * 100));
        assert_eq!(
            compaction_estimate(&failed_past_cap),
            compaction_estimate(&failed_at_cap),
            "the estimate is the clipped title's, not the stored error's"
        );
        assert_eq!(
            compaction_components(&failed_past_cap),
            1,
            "a plain row is one element"
        );
        assert!(
            compaction_estimate(&failed_past_cap) < MAX_CARD_JSON_CHARS,
            "one failure row leaves room for a card's own chrome"
        );
    }
}
