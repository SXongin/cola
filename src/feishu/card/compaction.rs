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
use crate::feishu::card::sanitize::{CardMarkdown, escaped_len};
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
/// capped to one element's budget in the title ([`MAX_COMPACTION_ERROR_CHARS`]).
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
/// ([`compaction_error`]): the title is ONE card element and the row is one
/// unsplittable timeline item, so an oversized error would make the card's
/// minimum one-row slice exceed the whole-card budget — and the failure row, the
/// case the reader most needs, would vanish with it.
pub(crate) fn compaction_title(compaction: &Compaction) -> String {
    let reason = match compaction.reason {
        CompactionReason::Auto => "自动",
        CompactionReason::Manual => "手动",
    };
    let title = match compaction.status {
        CompactionStatus::Running => format!("⏳ 压缩中（{reason}）"),
        CompactionStatus::Completed => format!("已压缩上下文（{reason}）"),
        CompactionStatus::Failed => format!("⚠️ 压缩失败：{}", compaction_error(compaction)),
    };
    // Presence, not magnitude: a request the server reports with a zero usage is
    // still a reported usage, and the row says so.
    match compaction.usage.map(|usage| usage.total) {
        Some(total) => format!("{title} · {} tokens", format_tokens(total)),
        None => title,
    }
}

/// The error a failed row's title splices in, clipped to
/// [`MAX_COMPACTION_ERROR_CHARS`] with [`ERROR_TRUNCATED_MARKER`] on a line of its
/// own when it was cut. The stored `Compaction.error` keeps the Backend's whole
/// text: this caps what the card SHOWS, exactly as the fold body's cap does for
/// the summary.
fn compaction_error(compaction: &Compaction) -> String {
    let error = compaction
        .error
        .as_deref()
        .filter(|error| !error.is_empty())
        .unwrap_or("未知错误");
    let clipped: String = error.chars().take(MAX_COMPACTION_ERROR_CHARS).collect();
    if clipped.chars().count() < error.chars().count() {
        format!("{clipped}\n{ERROR_TRUNCATED_MARKER}")
    } else {
        clipped
    }
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

/// The copy a failure title always carries around the error it splices in: the
/// `⚠️ 压缩失败：` prefix, the widest ` · 12k tokens` usage suffix, and the clip's
/// own marker and its newline. Reserved against the card's per-element budget so
/// a clipped title is still ONE element inside it (a test pins the widest
/// combination against [`MAX_ELEMENT_TEXT_CHARS`]).
const TITLE_RESERVED_CHARS: usize = 32;

/// How much of a failed compaction's error the row's title shows, in CHARACTERS.
///
/// The title is one card element — the row is a plain line, never a fold — so
/// [`MAX_ELEMENT_TEXT_CHARS`], cola's per-element budget, is its natural bound,
/// less the copy the title reserves ([`TITLE_RESERVED_CHARS`]). Worst case, an
/// error at this cap is ~9 KB of card JSON (CJK, three bytes per character:
/// [`serde_json`] emits UTF-8, never `\u` escapes) — comfortable inside
/// [`MAX_CARD_JSON_CHARS`](crate::feishu::card::MAX_CARD_JSON_CHARS) (25 KB) and
/// Feishu's 30 KB hard limit, which is what an unbounded error (a stack trace)
/// would otherwise blow for the whole card, failure row included.
pub(crate) const MAX_COMPACTION_ERROR_CHARS: usize = MAX_ELEMENT_TEXT_CHARS - TITLE_RESERVED_CHARS;

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
/// [`MAX_COMPACTION_SUMMARY_CHARS`] CHARACTERS and at most
/// [`MAX_COMPACTION_BODY_BYTES`] SERIALIZED bytes ([`escaped_len`]) — the
/// character cap for an ordinary summary, the byte cap for text the sanitizer
/// expands, so neither extreme can outgrow one card.
fn clipped_summary(summary: &str) -> (String, bool) {
    let mut clipped = String::new();
    let mut bytes = 0;
    for (chars, ch) in summary.chars().enumerate() {
        let cost = crate::feishu::card::sanitize::escaped_char_len(ch);
        if chars >= MAX_COMPACTION_SUMMARY_CHARS || bytes + cost > MAX_COMPACTION_BODY_BYTES {
            return (clipped, true);
        }
        clipped.push(ch);
        bytes += cost;
    }
    (clipped, false)
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
/// drift. Every charge is [`escaped_len`]'s: what the sanitizer will SERIALIZE,
/// not the raw text, so a markup-heavy row (every `<` becomes the five-byte
/// `&#60;`) cannot be charged less than it costs. The body is charged in JSON
/// bytes rather than as card text, like a ledger entry's output: a fold body is
/// one element per chunk and cannot be split the way streamed text can. Because
/// both are capped, a row's cost is bounded whatever the stored summary holds.
pub(crate) fn compaction_estimate(compaction: &Compaction) -> usize {
    let title = escaped_len(&compaction_title(compaction));
    match compaction_fold_body(compaction) {
        None => title + 80,
        Some(chunks) => 300 + title + chunks.iter().map(|chunk| escaped_len(chunk) + 40).sum::<usize>(),
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

    /// A failed compaction carrying `error` — and, when asked, a usage of its own
    /// and a summary (a malformed payload can carry both).
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

    /// One row cannot be split by the card splitter, so its fold body is capped
    /// (ADR-0082): a summary at the cap, one far past it, and one whose content
    /// is CJK — 3 bytes per character, the worst a summary's own encoding can
    /// cost, since the JSON writer emits UTF-8 rather than `\u` escapes — each
    /// render a card inside the whole-card JSON budget. Without the cap a long
    /// summary blows the budget and Feishu rejects the card, taking the row and
    /// the turn's update with it.
    #[test]
    fn a_summary_past_the_cap_stays_inside_the_card_budget() {
        for (label, summary) in [
            ("at the cap", "摘".repeat(MAX_COMPACTION_SUMMARY_CHARS)),
            ("far past the cap", "摘".repeat(MAX_COMPACTION_SUMMARY_CHARS * 20)),
            (
                "ASCII far past the cap",
                "a".repeat(MAX_COMPACTION_SUMMARY_CHARS * 20),
            ),
        ] {
            let json = card_json(&summary);
            assert!(
                json.len() < MAX_CARD_JSON_CHARS,
                "{label}: the card must stay inside the whole-card JSON budget, got {} bytes",
                json.len()
            );
        }
    }

    /// The ONE amplification the cap does not bound is the markdown sanitizer's
    /// own escaping (`<` → `&#60;`, five characters each) — the exposure every
    /// model-markdown element carries, and the reason the fenced fallback exists.
    /// A summary made of markup therefore costs one rejected write, and the
    /// fenced rebuild — what a rejected card is retried as — lands inside the
    /// budget: the row is never lost, and the cap still bounds the body it holds.
    /// (The byte cap below keeps the FIRST attempt inside the budget too.)
    #[test]
    fn a_markup_laden_summary_lands_inside_the_budget_through_the_fenced_fallback() {
        let summary = "<".repeat(MAX_COMPACTION_SUMMARY_CHARS * 20);
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

    /// Markup the sanitizer EXPANDS is bounded by serialized bytes, not characters
    /// alone: a summary of pure `<` at the character cap would serialize to ~30 KB
    /// (every `<` becomes the five-byte `&#60;`) and Feishu would reject the card,
    /// taking the row with it. The fold body is clipped so its escaped cost fits
    /// [`MAX_COMPACTION_BODY_BYTES`] — the allowance a worst-case CJK body at the
    /// character cap already costs — and the row's card stays inside the budget.
    #[test]
    fn a_markup_laden_fold_body_stays_inside_the_card_budget() {
        for (label, summary) in [
            ("at the character cap", "<".repeat(MAX_COMPACTION_SUMMARY_CHARS)),
            (
                "far past every cap",
                "<".repeat(MAX_COMPACTION_SUMMARY_CHARS * 20),
            ),
            (
                "markup inside a code fence",
                format!("```\n{}\n```", "<".repeat(MAX_COMPACTION_SUMMARY_CHARS)),
            ),
        ] {
            let json = card_json(&summary);
            assert!(
                json.len() < MAX_CARD_JSON_CHARS,
                "{label}: the card must stay inside the whole-card JSON budget, got {} bytes",
                json.len()
            );
        }

        // The clip keeps the head, names itself, and stops at the BYTE cap: a
        // pure-`<` body shows as many characters as the escaped budget allows.
        let body = compaction_fold_body(&compaction(&"<".repeat(MAX_COMPACTION_SUMMARY_CHARS)))
            .expect("a folded body");
        assert_eq!(body.last().map(String::as_str), Some(SUMMARY_TRUNCATED_MARKER));
        let visible: String = body[..body.len() - 1].concat();
        let cap = MAX_COMPACTION_BODY_BYTES / 5;
        assert_eq!(visible, "<".repeat(cap), "the head is kept up to the byte cap");
        assert!(crate::feishu::card::sanitize::escaped_len(&visible) <= MAX_COMPACTION_BODY_BYTES);
    }

    /// A failure's error can carry markup too, and the title is one element: the
    /// escaped clip still leaves the whole card inside the budget while keeping
    /// the head of the error.
    #[test]
    fn a_markup_heavy_error_stays_inside_the_card_budget() {
        for (label, error) in [
            ("at the cap", "<".repeat(MAX_COMPACTION_ERROR_CHARS)),
            ("far past the cap", "<".repeat(MAX_COMPACTION_ERROR_CHARS * 20)),
        ] {
            let json = card_of(&failed(&error));
            assert!(
                json.len() < MAX_CARD_JSON_CHARS,
                "{label}: the card must stay inside the whole-card JSON budget, got {} bytes",
                json.len()
            );
        }
        let title = compaction_title(&failed(&"<".repeat(MAX_COMPACTION_ERROR_CHARS * 2)));
        assert!(title.contains(ERROR_TRUNCATED_MARKER));
        assert!(title.contains(&"<".repeat(MAX_COMPACTION_ERROR_CHARS)));
        assert!(!title.contains(&"<".repeat(MAX_COMPACTION_ERROR_CHARS + 1)));
    }

    /// The charge is never an UNDER-charge: for a markup-heavy row the estimate
    /// covers what the row's elements actually serialize to — measured from the
    /// very builder the render uses — so the splitter cannot be told a card fits
    /// when it does not.
    #[test]
    fn the_estimate_never_undercharges_a_markup_heavy_row() {
        for (label, fact) in [
            (
                "a markup-heavy summary",
                compaction(&"<".repeat(MAX_COMPACTION_SUMMARY_CHARS * 5)),
            ),
            ("a prose summary", compaction("保留了结论。")),
            (
                "a markup-heavy error",
                failed(&"<".repeat(MAX_COMPACTION_ERROR_CHARS * 5)),
            ),
            ("a short error", failed("context length exceeded")),
        ] {
            let mut md = CardMarkdown::new();
            let serialized: usize =
                crate::feishu::card::compaction::compaction_elements(&fact, Some("compaction_1"), &mut md)
                    .iter()
                    .map(|element| element.to_string().len())
                    .sum();
            assert!(
                compaction_estimate(&fact) >= serialized,
                "{label}: the estimate ({}) must cover the row's serialized {} bytes",
                compaction_estimate(&fact),
                serialized
            );
        }
    }

    /// The capped body keeps the head of the summary and says it was cut, with
    /// the marker in an element of its own so no element outgrows the per-element
    /// budget. A summary within the cap renders whole, with no marker.
    #[test]
    fn a_clipped_fold_body_keeps_the_head_and_names_the_truncation() {
        let summary = "摘".repeat(MAX_COMPACTION_SUMMARY_CHARS * 2);
        let body = compaction_fold_body(&compaction(&summary)).expect("a folded body");
        assert_eq!(
            body.last().map(String::as_str),
            Some(SUMMARY_TRUNCATED_MARKER),
            "the clip is named, never silent: {body:?}"
        );
        assert!(
            body.iter()
                .all(|chunk| chunk.chars().count() <= MAX_ELEMENT_TEXT_CHARS),
            "the marker rides its own element: {body:?}"
        );
        let visible: String = body[..body.len() - 1].concat();
        assert_eq!(
            visible,
            "摘".repeat(MAX_COMPACTION_SUMMARY_CHARS),
            "the head of the summary is kept"
        );

        let short = "保留了结论。";
        assert_eq!(
            compaction_fold_body(&compaction(short)).unwrap().concat(),
            short,
            "a summary within the cap is rendered whole"
        );
    }

    /// The component count and the size estimate measure the SAME capped body the
    /// render builds: past the cap a summary costs no more than one at the cap
    /// plus the marker's own element, so the splitter charges a bounded row.
    #[test]
    fn the_row_cost_cannot_outgrow_the_capped_body() {
        let at_cap = compaction(&"摘".repeat(MAX_COMPACTION_SUMMARY_CHARS));
        let past_cap = compaction(&"摘".repeat(MAX_COMPACTION_SUMMARY_CHARS * 50));
        let further = compaction(&"摘".repeat(MAX_COMPACTION_SUMMARY_CHARS * 500));
        let marker_cost = SUMMARY_TRUNCATED_MARKER.len() + 40;
        assert_eq!(
            compaction_estimate(&past_cap),
            compaction_estimate(&at_cap) + marker_cost,
            "the estimate is the capped body's (plus its marker), not the stored summary's"
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
            compaction_estimate(&further) + compaction_title(&further).len() < MAX_CARD_JSON_CHARS,
            "one row leaves room for a card's own chrome"
        );

        // A failure row is a plain line, and its cost is the CLIPPED title's: an
        // error far past the cap costs no more than one at the cap.
        let failed_at_cap = failed(&"字".repeat(MAX_COMPACTION_ERROR_CHARS));
        let failed_past_cap = failed(&"字".repeat(MAX_COMPACTION_ERROR_CHARS * 100));
        assert_eq!(
            compaction_estimate(&failed_past_cap),
            compaction_estimate(&failed_at_cap) + ERROR_TRUNCATED_MARKER.len() + 1, /* the marker's own line break */
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

    /// A failure row's card stays inside the whole-card JSON budget whatever the
    /// Backend's error holds: the title is one element and the row one
    /// unsplittable timeline item, so an unbounded error (a stack trace) would
    /// make the card's minimum one-row slice too large and Feishu would reject it
    /// — the failure row vanishing, which is the case the reader most needs.
    #[test]
    fn a_failure_error_past_the_cap_stays_inside_the_card_budget() {
        for (label, error) in [
            ("at the cap", "字".repeat(MAX_COMPACTION_ERROR_CHARS)),
            ("far past the cap", "字".repeat(MAX_COMPACTION_ERROR_CHARS * 100)),
            (
                "a stack trace far past the cap",
                "Error: boom\n    at src/main.rs:1\n".repeat(MAX_COMPACTION_ERROR_CHARS),
            ),
        ] {
            let json = card_of(&failed(&error));
            assert!(
                json.len() < MAX_CARD_JSON_CHARS,
                "{label}: the card must stay inside the whole-card JSON budget, got {} bytes",
                json.len()
            );
        }
    }

    /// The clipped failure title keeps the head of the error and names the
    /// truncation, stays ONE element inside the per-element budget — with the
    /// widest usage suffix a completion can add — and leaves a short error (the
    /// ordinary case) exactly as the Backend wrote it.
    #[test]
    fn a_clipped_error_keeps_the_head_and_names_the_truncation() {
        let error = "字".repeat(MAX_COMPACTION_ERROR_CHARS * 3);
        let mut fact = failed(&error);
        fact.usage = Some(crate::backend::TokenUsage {
            total: 20_000_000,
            ..Default::default()
        });
        let title = compaction_title(&fact);
        assert!(
            title.contains(ERROR_TRUNCATED_MARKER),
            "the clip is named, never silent: {title}"
        );
        assert!(
            title.contains(&"字".repeat(MAX_COMPACTION_ERROR_CHARS)),
            "the head of the error is kept"
        );
        assert!(
            !title.contains(&"字".repeat(MAX_COMPACTION_ERROR_CHARS + 1)),
            "nothing past the cap renders"
        );
        assert!(
            title.ends_with(" · 20M tokens"),
            "the usage suffix still closes the row: {title}"
        );
        assert!(
            title.chars().count() <= MAX_ELEMENT_TEXT_CHARS,
            "the widest title must stay one budgeted element, got {} chars",
            title.chars().count()
        );

        assert_eq!(
            compaction_title(&failed("context length exceeded")),
            "⚠️ 压缩失败：context length exceeded",
            "a short error renders exactly as the Backend wrote it"
        );
    }
}
