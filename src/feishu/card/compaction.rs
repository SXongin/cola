//! The **Compaction** row (ADR-0082, spec #719): how one compaction renders on
//! a card. A Session-scoped fact — it answers no prompt and is never a Turn
//! anchor — so its row is owned by the card timeline, which places it at the
//! compaction's own server time rather than at the card's end. The Session
//! Card's 最近对话 tail (ticket #723) renders the same row through
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
use crate::feishu::card::{MAX_CARD_TEXT_CHARS, budget, clip, fenced_overhead, format_tokens};
use serde_json::json;

/// The card elements one Compaction row is made of (ADR-0082), shared by every
/// surface that shows a row — the live card's timeline
/// ([`crate::feishu::card::shell::CardBuilder::with_compaction`]) and the Session
/// Card's 最近对话 tail (ticket #723) — so the two cannot disagree on the
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
            // [`budget::CARD_ELEMENT`], measured rather than reserved.
            let budget = budget::CARD_ELEMENT
                .max
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
/// explicit [`SUMMARY_TRUNCATED_MARKER`] element when it was cut. The chunks are
/// the markdown split's, so a construct crossing the budget is closed on one
/// element and re-opened on the next — this is a RAW-markdown caller, with no
/// card cleaner in front of it, so the chunker fences an oversized table itself
/// (issue #1098). `None` when there is nothing to fold — a running or failed
/// compaction, and a completed one the Backend never gave a text (an orphaned V1
/// part, say): the row is then its title alone, rather than a fold holding
/// nothing.
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
    let mut chunks = clip::clip(&clipped).split(budget::CARD_ELEMENT);
    if truncated {
        chunks.push(SUMMARY_TRUNCATED_MARKER.to_string());
    }
    Some(chunks)
}

/// The summary text one row's fold shows, and whether it was cut: at most
/// [`MAX_COMPACTION_SUMMARY_CHARS`] CHARACTERS and — the clip's marker included,
/// the rejection fallback's fence too, and the construct markers the element
/// chunker adds — at most [`MAX_COMPACTION_BODY_BYTES`] SERIALIZED bytes
/// ([`fold_body_bytes`]) — the character cap for an ordinary summary, the byte
/// cap for text the card encodes, fences or splits larger, so neither extreme can
/// outgrow one card on either attempt.
fn clipped_summary(summary: &str) -> (String, bool) {
    let chars = summary.chars().count();
    let at_char_cap = chars.min(MAX_COMPACTION_SUMMARY_CHARS);
    let head: String = summary.chars().take(at_char_cap).collect();
    if chars <= MAX_COMPACTION_SUMMARY_CHARS && fold_body_bytes(&head) <= MAX_COMPACTION_BODY_BYTES {
        return (head, false);
    }
    // The body IS cut, so the marker rides in the same budget: search the longest
    // prefix whose body — the prefix plus the marker the card will show, fence
    // and split markers included — fits. The search only ever keeps a prefix it
    // measured as fitting, so a cost that dips as the prefix grows (a fence that
    // closes) can only leave the clip shorter than it could be, never over the
    // cap; the character cap bounds the search.
    let (mut keep, mut too_long) = (0usize, at_char_cap);
    while keep < too_long {
        let mid = keep + (too_long - keep).div_ceil(2);
        let candidate: String = summary.chars().take(mid).collect();
        if fold_body_bytes(&candidate) + body_bytes(SUMMARY_TRUNCATED_MARKER) <= MAX_COMPACTION_BODY_BYTES {
            keep = mid;
        } else {
            too_long = mid - 1;
        }
    }
    (summary.chars().take(keep).collect(), true)
}

/// What one text costs the body budget: its exact serialized bytes
/// ([`serialized_len`]) plus the fence the rejection fallback wraps it in
/// ([`fenced_overhead`]), so a body at [`MAX_COMPACTION_BODY_BYTES`] still fits
/// one card when Feishu rejects the first attempt. This is a CAP quantity, not
/// the splitter's charge: the caps bound the row's actual body, while
/// [`charged_bytes`] bounds the splitter's decision.
fn body_bytes(text: &str) -> usize {
    serialized_len(text) + fenced_overhead(text)
}

/// What one text costs the body budget once the element chunker has split it:
/// every element's own [`body_bytes`] summed, so the construct markers the split
/// adds — a re-opened fence opener, a closer, the fence the markdown split wraps
/// an oversized table in, AND each element's own fallback fence (a marker line is
/// itself a backtick run) all count. The cap therefore bounds what the card
/// actually writes, not just the text before the chunker touches it.
fn fold_body_bytes(text: &str) -> usize {
    clip::clip(text)
        .split(budget::CARD_ELEMENT)
        .iter()
        .map(|chunk| body_bytes(chunk))
        .sum()
}

/// The safety factor every rendered element's charge is multiplied by. A row's
/// per-element charge cannot be exact, because the card layer computes some
/// expansions from state the splitter does not have; this factor turns the
/// measurable part into a CEILING over all of them.
///
/// The four expansions a rendered compaction element can meet, and where each is
/// covered:
///
/// - the sanitizer's markup escape — `<` becomes the five-byte `&#60;` — is
///   [`serialized_len`]'s, exactly;
/// - the JSON encoder's control-character escapes — a NUL becomes the six-byte
///   `\u0000`, and quotes and backslashes double — are [`serialized_len`]'s too;
/// - the rejection fallback's backtick fence, sized from the text's longest run,
///   is [`fenced_overhead`]'s;
/// - the table sanitizer's fence — a table past the card's five-table allowance
///   (or its 49-row cap) gains eight bytes of fence around syntax that needs at
///   least four — is this factor's. Its adder never exceeds ~2x the text it
///   wraps, so three covers it with room to spare.
///
/// Deliberately conservative: ordinary prose is charged three times what it
/// serializes, and a `<` or a backtick inside code (where neither expansion
/// applies) is charged as though it expanded. Over-charging is the safe
/// direction for a splitter — the worst it does is give a row a card of its own —
/// while under-charging is a card Feishu rejects, taking the row and the turn's
/// update with it.
pub(crate) const COMPACTION_CHARGE_FACTOR: usize = 3;

/// The bytes one rendered text is CHARGED for, as a ceiling: its serialized
/// content ([`serialized_len`]) plus the fallback's fence
/// ([`fenced_overhead`]), multiplied by [`COMPACTION_CHARGE_FACTOR`]. The charge
/// is what the card splitter's budget sees; what the card actually writes is
/// bounded by the row's caps ([`MAX_COMPACTION_SUMMARY_CHARS`],
/// [`MAX_COMPACTION_BODY_BYTES`]) instead.
fn charged_bytes(text: &str) -> usize {
    (serialized_len(text) + fenced_overhead(text)) * COMPACTION_CHARGE_FACTOR
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

/// The bytes one Compaction row is CHARGED in the card splitter's budget
/// (ADR-0082): its title, plus — for a folded row — the panel's own element
/// overhead and the body's markdown chunks, measured from the same chunks the
/// render builds so the two cannot drift. Every text is charged
/// [`charged_bytes`]'s ceiling, so the expansions the card layer applies but the
/// splitter cannot see cannot make a row cost more than it was charged; the fixed
/// per-element constants cover the element skeletons and the minimum fence every
/// fenced element carries. The body is charged in bytes rather than as card text,
/// like a ledger entry's output: a fold body is one element per chunk and cannot
/// be split the way streamed text can.
///
/// A consequence, and the point: a large body's ceiling now exceeds a card's
/// budget, so the row travels on a card of its own. That is the correct placement
/// — the row cannot be split, and the caps bound what it really writes — where
/// before it shared a card and risked the whole write.
pub(crate) fn compaction_estimate(compaction: &Compaction) -> usize {
    let title = compaction_title(compaction);
    let title = charged_bytes(&title);
    match compaction_fold_body(compaction) {
        None => title + 80,
        Some(chunks) => {
            300 + title
                + chunks
                    .iter()
                    .map(|chunk| charged_bytes(chunk) + 40)
                    .sum::<usize>()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::card::MAX_CARD_JSON_CHARS;
    use crate::feishu::card::sanitize::{MAX_CARD_TABLES, neutralize_tail};
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
    /// markup, CJK, control characters, JSON metacharacters, the rejection
    /// fallback's fence, and a mix.
    fn atoms() -> [(&'static str, &'static str); 6] {
        [
            ("NUL", "\0"),
            ("markup", "<"),
            ("CJK", "摘"),
            ("quotes and backslashes", "\"\\"),
            ("backticks", "`"),
            ("mixed", "a<\0\\\"摘`"),
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
    /// uses, so the charge can be held against the real payload. `fenced` builds
    /// the rejection fallback's rebuild instead of the first attempt.
    fn row_serialized_len(fact: &Compaction, fenced: bool) -> usize {
        let md = if fenced {
            CardMarkdown::fenced()
        } else {
            CardMarkdown::new()
        };
        row_serialized_len_with(fact, md)
    }

    /// [`row_serialized_len`] with a caller-built markdown state — the way to
    /// render a row whose card has already spent its table budget, so every table
    /// in the text takes the sanitizer's own fence.
    fn row_serialized_len_with(fact: &Compaction, mut md: CardMarkdown) -> usize {
        compaction_elements(fact, Some("compaction_1"), &mut md)
            .iter()
            .map(|element| element.to_string().len())
            .sum()
    }

    /// The multi-table body the adversarial pass described: hundreds of minimal
    /// markdown tables in one element, every one of them past the card's
    /// five-table allowance and so wrapped in the sanitizer's fence.
    fn many_short_tables() -> String {
        "a|\n|-\n\n".repeat(333)
    }

    /// The same body, long enough that the fixed wrapper constants cannot mask the
    /// adder (measured: 1,000 minimal tables serialize to 10,000 bytes and render
    /// to 17,458 — the fence is ~2x the syntax it wraps).
    fn a_thousand_short_tables() -> String {
        "a|\n|-\n\n".repeat(1_000)
    }

    /// The card the row builds, `fenced` selecting the rejection fallback's
    /// rebuild.
    fn card_with(fact: &Compaction, fenced: bool) -> String {
        CardBuilder::new()
            .with_state(crate::feishu::card::CardState::Done)
            .with_fenced_markdown(fenced)
            .with_compaction(fact, Some("compaction_1"))
            .build()
            .to_string()
    }

    /// What one fold body's TEXT serializes to, the truncation marker included.
    fn body_serialized_len(chunks: &[String]) -> usize {
        chunks.iter().map(|chunk| serialized_len(chunk)).sum()
    }

    /// What one fold body costs its budget: every element's serialized bytes PLUS
    /// the fence the rejection fallback would wrap it in — the quantity
    /// [`MAX_COMPACTION_BODY_BYTES`] bounds (`body_bytes`, summed).
    fn body_bytes_len(chunks: &[String]) -> usize {
        chunks
            .iter()
            .map(|chunk| serialized_len(chunk) + fenced_overhead(chunk))
            .sum()
    }

    /// The measure is never below what the card actually carries: it applies the
    /// sanitizer's expansion AND the JSON encoding, so control characters, quotes
    /// and backslashes cost what the writer will spend on them — no per-character
    /// table to fall behind a new expansion vector.
    #[test]
    fn the_measure_covers_what_the_card_carries_for_any_content() {
        for (label, atom) in atoms() {
            for length in [1, 10, budget::CARD_ELEMENT.max, budget::CARD_ELEMENT.max * 5] {
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
                "字".repeat(budget::CARD_ELEMENT.max),
            ),
            (
                "prose far past the cap".to_string(),
                "字".repeat(budget::CARD_ELEMENT.max * 100),
            ),
            (
                "a stack trace far past the cap".to_string(),
                "Error: boom\n    at src/main.rs:1\n".repeat(budget::CARD_ELEMENT.max),
            ),
        ];
        for (label, atom) in atoms() {
            for (scale, length) in [
                ("at the cap", budget::CARD_ELEMENT.max),
                ("far past the cap", budget::CARD_ELEMENT.max * 100),
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
                title.chars().count() <= budget::CARD_ELEMENT.max,
                "{label}: the title must stay one budgeted element, got {} chars",
                title.chars().count()
            );
        }
    }

    /// The charge is never an UNDER-charge: for every content kind — control
    /// characters, markup, the fallback's backtick fence and the sanitizer's
    /// table fence included — at and past each cap, the estimate covers what the
    /// row's elements serialize to in the ordinary render, in the fenced rebuild
    /// Feishu forces after a content rejection, and in a render whose tables all
    /// take the sanitizer's fence. Measured from the very builder the render uses,
    /// so the splitter can never be told a card fits when it does not.
    #[test]
    fn the_estimate_never_undercharges_a_row() {
        let mut cases: Vec<(String, Compaction)> = vec![
            ("a prose summary".to_string(), compaction("保留了结论。")),
            ("a short error".to_string(), failed("context length exceeded")),
            (
                "the many-short-tables body".to_string(),
                compaction(&many_short_tables()),
            ),
            (
                "a thousand-short-tables body".to_string(),
                compaction(&a_thousand_short_tables()),
            ),
            (
                "a many-short-tables error".to_string(),
                failed(&many_short_tables()),
            ),
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
                ("at the cap", budget::CARD_ELEMENT.max),
                ("past the cap", budget::CARD_ELEMENT.max * 5),
            ] {
                cases.push((format!("{label} error {scale}"), failed(&atom.repeat(length))));
            }
        }
        for (label, fact) in cases {
            let mut spent_tables = CardMarkdown::new();
            // Spend the card's five-table allowance first: every table the row
            // carries is then fenced, the worst case for this adder.
            spent_tables.clean(&"a|\n|-\n\n".repeat(MAX_CARD_TABLES));
            for (render, serialized) in [
                ("the first attempt", row_serialized_len(&fact, false)),
                ("the fenced rebuild", row_serialized_len(&fact, true)),
                (
                    "a render whose tables are all fenced",
                    row_serialized_len_with(&fact, spent_tables),
                ),
            ] {
                assert!(
                    compaction_estimate(&fact) >= serialized,
                    "{label} on {render}: the estimate ({}) must cover the row's serialized \
                     {serialized} bytes",
                    compaction_estimate(&fact)
                );
            }
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
                .all(|chunk| chunk.chars().count() <= budget::CARD_ELEMENT.max),
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

    /// Issue #1098: the fold body is a raw-markdown caller, so the chunker must
    /// keep a fence self-contained across the element split. A fenced block
    /// crossing `budget::CARD_ELEMENT.max` renders as code in every element, and
    /// the prose after the original closer is rendered as prose — never
    /// swallowed by a fence left open.
    #[test]
    fn a_fence_crossing_the_element_budget_renders_self_contained_elements() {
        let code: String = (0..200).map(|i| format!("print(<{i}>)  # 一行代码\n")).collect();
        let summary = format!("说明\n```python\n{code}```\n后的 <文字>");
        let fact = compaction(&summary);
        let body = compaction_fold_body(&fact).expect("a folded body");
        assert!(
            body.len() > 1,
            "the fixture must cross the element budget: {} chunks",
            body.len()
        );
        for chunk in &body {
            assert!(
                chunk.chars().count() <= budget::CARD_ELEMENT.max,
                "chunk over the element budget: {} chars",
                chunk.chars().count()
            );
            assert_eq!(
                neutralize_tail(chunk, "尾"),
                None,
                "a chunk ends inside a fence: {chunk:?}"
            );
        }
        let mut md = CardMarkdown::new();
        let elements = compaction_elements(&fact, Some("compaction_1"), &mut md);
        let rendered = elements[0]["elements"].to_string();
        assert!(
            rendered.contains("print(<199>)"),
            "a code line rendered as prose (its `<` escaped): {rendered}"
        );
        assert!(
            rendered.contains("&#60;文字>"),
            "prose swallowed into a code block: {rendered}"
        );
    }

    /// Issue #1098: a long table in a summary is one fenced block, not a header
    /// repeated on every element — each repeat counts as another native table
    /// against the card's five-table allowance, which fences the tables after
    /// it.
    #[test]
    fn a_long_table_in_a_summary_is_fenced_once_and_costs_no_table_slot() {
        let row = format!("| {} | 值 |\n", "长".repeat(100));
        let long = format!("| 长表 | 值 |\n|---|---|\n{}", row.repeat(30));
        let small = "| 短表 | 值 |\n|---|---|\n| 一 | 1 |";
        let summary = format!("前面的话。\n\n{long}\n\n后面的话。\n\n{small}\n\n结尾。");
        let fact = compaction(&summary);
        let body = compaction_fold_body(&fact).expect("a folded body");
        assert!(
            body.len() > 1,
            "the fixture must cross the element budget: {} chunks",
            body.len()
        );
        assert_eq!(
            body.iter()
                .filter(|chunk| chunk.contains("| 长表 | 值 |"))
                .count(),
            1,
            "the long table's header must appear once: {body:?}"
        );
        assert!(
            body.iter().any(|chunk| chunk.contains("```\n| 长表 | 值 |")),
            "the long table is one code block: {body:?}"
        );
        let cost = body_bytes_len(&body);
        assert!(
            cost <= MAX_COMPACTION_BODY_BYTES,
            "the body must fit its own byte budget, got {cost} bytes"
        );

        // Four of the card's five tables are already spent: the fenced long
        // table costs no slot, so the short one still renders natively. A
        // repeated header would have fenced it.
        let mut md = CardMarkdown::new();
        for _ in 0..MAX_CARD_TABLES - 1 {
            md.clean("| a | b |\n|---|---|\n| 1 | 2 |");
        }
        let elements = compaction_elements(&fact, Some("compaction_1"), &mut md);
        let rendered = elements[0]["elements"].to_string();
        let at = rendered.find("| 短表 | 值 |").expect("the short table renders");
        assert!(
            !rendered[..at].trim_end().ends_with("```"),
            "the short table was fenced after the long table took a slot: {rendered}"
        );
    }

    /// Issue #1098: the clip's byte budget covers the construct markers the
    /// chunker adds — a re-opened opener and a closer per split. A body at the
    /// character cap that a fence forces to split would grow past
    /// `MAX_COMPACTION_BODY_BYTES` without them counted, so the clip stops
    /// earlier instead.
    #[test]
    fn the_clip_reserves_the_chunkers_construct_markers() {
        // Exactly at the character cap, with a fence left open at the end: no
        // markers are needed until the body is split, and the split adds them.
        let summary = format!(
            "```\n{}",
            "摘".repeat(MAX_COMPACTION_SUMMARY_CHARS - "```\n".chars().count())
        );
        assert_eq!(summary.chars().count(), MAX_COMPACTION_SUMMARY_CHARS);
        let fact = compaction(&summary);
        let body = compaction_fold_body(&fact).expect("a folded body");
        assert_eq!(
            body.last().map(String::as_str),
            Some(SUMMARY_TRUNCATED_MARKER),
            "the markers come out of the byte budget, so the body is cut: {} chunks",
            body.len()
        );
        let cost = body_bytes_len(&body);
        assert!(
            cost <= MAX_COMPACTION_BODY_BYTES,
            "the body — markers included — must fit, got {cost} bytes"
        );
        for chunk in &body {
            assert!(
                chunk.chars().count() <= budget::CARD_ELEMENT.max,
                "chunk over the element budget: {} chars",
                chunk.chars().count()
            );
            assert_eq!(
                neutralize_tail(chunk, "尾"),
                None,
                "a chunk ends inside a fence: {chunk:?}"
            );
        }
        assert!(
            card_json(&summary).len() < MAX_CARD_JSON_CHARS,
            "the card must stay inside the whole-card JSON budget"
        );
    }

    /// The clipped failure title keeps the head of the error and names the
    /// truncation — the marker INSIDE the element budget, never on top of it —
    /// leaves a short error (the ordinary case) exactly as the Backend wrote it,
    /// and keeps the usage suffix on the row.
    #[test]
    fn a_clipped_error_keeps_the_head_and_names_the_truncation() {
        let error = "字".repeat(budget::CARD_ELEMENT.max * 3);
        let mut fact = failed(&error);
        fact.usage = Some(crate::backend::TokenUsage {
            total: 20_000_000,
            ..Default::default()
        });
        let title = compaction_title(&fact);
        assert!(
            title.chars().count() <= budget::CARD_ELEMENT.max,
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

    /// A summary of long backtick runs is charged for the fence the rejection
    /// fallback wraps it in: the fenced rebuild — which Feishu forces after a
    /// content rejection — stays inside the whole-card budget, the charge covers
    /// what that rebuild serializes, and the body's budget admits the fence.
    /// Without the term, ~6 KB of fence per chunk rode free.
    #[test]
    fn a_backtick_heavy_fold_body_stays_inside_the_budget_once_fenced() {
        for (label, summary) in [
            ("at the cap", "`".repeat(MAX_COMPACTION_SUMMARY_CHARS)),
            ("far past the cap", "`".repeat(MAX_COMPACTION_SUMMARY_CHARS * 20)),
            (
                "a long run inside prose",
                format!("{}{}{}", "a".repeat(1_000), "`".repeat(3_000), "b".repeat(1_000)),
            ),
            (
                // The fence, not the text, is what overflows here: the run's
                // fence costs twice its length, and only a body that counts it
                // stops inside the budget.
                "a run plus control characters",
                format!("{}{}", "`".repeat(3_000), "\0".repeat(3_000)),
            ),
        ] {
            let fact = compaction(&summary);
            let body = compaction_fold_body(&fact).expect("a folded body");
            // The first chunk carries no lead (only a continuation gets one), so
            // it still opens with the head of the summary; the split's markers
            // are closers and later leads, never content.
            let head: String = summary.trim().chars().take(100).collect();
            assert!(
                body[0].starts_with(&head),
                "{label}: the body keeps the head of the summary: {:?}",
                body[0].chars().take(100).collect::<String>()
            );
            assert!(
                body_bytes_len(&body) <= MAX_COMPACTION_BODY_BYTES,
                "{label}: the body — fence included — must fit its own byte budget, got {} bytes",
                body_bytes_len(&body)
            );

            let fenced = card_with(&fact, true);
            assert!(
                fenced.len() < MAX_CARD_JSON_CHARS,
                "{label}: the fenced rebuild must stay inside the whole-card budget, got {} bytes",
                fenced.len()
            );
            let serialized = row_serialized_len(&fact, true);
            assert!(
                compaction_estimate(&fact) >= serialized,
                "{label}: the estimate ({}) must cover the fenced row's {serialized} bytes",
                compaction_estimate(&fact)
            );
        }
    }

    /// The same for a failure's error, whose fence the fallback adds around the
    /// whole title element.
    #[test]
    fn a_backtick_heavy_error_stays_inside_the_budget_once_fenced() {
        for (label, error) in [
            ("at the cap", "`".repeat(budget::CARD_ELEMENT.max)),
            ("far past the cap", "`".repeat(budget::CARD_ELEMENT.max * 20)),
        ] {
            let fact = failed(&error);
            let fenced = card_with(&fact, true);
            assert!(
                fenced.len() < MAX_CARD_JSON_CHARS,
                "{label}: the fenced rebuild must stay inside the whole-card budget, got {} bytes",
                fenced.len()
            );
            let serialized = row_serialized_len(&fact, true);
            assert!(
                compaction_estimate(&fact) >= serialized,
                "{label}: the estimate ({}) must cover the fenced row's {serialized} bytes",
                compaction_estimate(&fact)
            );
        }
    }

    /// The charge is a CEILING over the measurable cost: for any content it is at
    /// least the serialized bytes plus the fallback's fence, and at most the
    /// safety factor times them — never below what the card can spend, and never
    /// wildly above it.
    #[test]
    fn the_charge_is_a_ceiling_over_the_measurable_cost() {
        for (label, atom) in atoms() {
            for length in [1, 10, budget::CARD_ELEMENT.max, budget::CARD_ELEMENT.max * 5] {
                let text = atom.repeat(length);
                let measured = serialized_len(&text) + fenced_overhead(&text);
                assert!(
                    charged_bytes(&text) >= measured,
                    "{label} ×{length}: the charge ({}) must dominate the measured {measured} bytes",
                    charged_bytes(&text)
                );
                assert!(
                    charged_bytes(&text) <= measured * COMPACTION_CHARGE_FACTOR,
                    "{label} ×{length}: the charge must stay within the factor of the measurement"
                );
                assert!(
                    charged_bytes(&text) >= text.len(),
                    "{label} ×{length}: the charge must dominate the raw bytes too"
                );
            }
        }
    }

    /// Ordinary prose RENDERS exactly as it did before the charge became a
    /// ceiling: no backtick run means no fence, nothing is clipped, and the card
    /// carries the text verbatim. Its charge is the ceiling — deliberately more
    /// than it serializes — which is the safe direction for a splitter.
    #[test]
    fn the_ordinary_path_renders_unchanged_under_a_ceiling_charge() {
        for (label, fact) in [
            ("a prose summary", compaction("保留了结论。")),
            ("an ASCII summary", compaction("kept the conclusions.")),
            ("a prose error", failed("context length exceeded")),
        ] {
            let title = compaction_title(&fact);
            assert_eq!(fenced_overhead(&title), 0, "{label}: no extra fence to charge");
            let exact = match compaction_fold_body(&fact) {
                None => serialized_len(&title) + 80,
                Some(chunks) => {
                    assert!(
                        chunks.iter().all(|chunk| fenced_overhead(chunk) == 0),
                        "{label}: no chunk carries a fence charge"
                    );
                    assert_eq!(
                        chunks.concat(),
                        fact.summary.trim(),
                        "{label}: the body is the text, verbatim"
                    );
                    assert_eq!(chunks.len(), 1, "{label}: ordinary prose fits one element");
                    300 + serialized_len(&title)
                        + chunks
                            .iter()
                            .map(|chunk| serialized_len(chunk) + 40)
                            .sum::<usize>()
                }
            };
            assert!(
                compaction_estimate(&fact) >= exact,
                "{label}: the ceiling is never below the exact cost"
            );
            assert!(
                compaction_estimate(&fact) <= exact * COMPACTION_CHARGE_FACTOR,
                "{label}: and never more than the factor above it"
            );
            assert!(
                !card_of(&fact).contains("已截断"),
                "{label}: nothing is clipped on the ordinary path"
            );
        }
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
            3 + MAX_COMPACTION_SUMMARY_CHARS.div_ceil(budget::CARD_ELEMENT.max),
            "the panel plus one element per chunk"
        );
        assert!(
            compaction_estimate(&further) >= row_serialized_len(&further, false),
            "the ceiling covers what the row writes"
        );

        // A failure row is a plain line, and its charge is the CLIPPED title's: an
        // error far past the caps costs the marker more than one at the cap, never
        // more.
        let failed_at_cap = failed(&"字".repeat(budget::CARD_ELEMENT.max));
        let failed_past_cap = failed(&"字".repeat(budget::CARD_ELEMENT.max * 100));
        assert_eq!(
            compaction_components(&failed_past_cap),
            1,
            "a plain row is one element"
        );
        assert!(
            compaction_estimate(&failed_past_cap)
                <= compaction_estimate(&failed_at_cap)
                    + charged_bytes(&format!("\n{ERROR_TRUNCATED_MARKER}")),
            "the cut adds the marker's own charge, nothing else"
        );

        // The CEILING may exceed a whole card — that is the splitter being told the
        // truth about a row it cannot split — while what the row actually WRITES
        // stays inside one card, which is what the caps bound.
        for (render, fact) in [
            ("the capped body", &further),
            ("a capped error", &failed_past_cap),
        ] {
            for (attempt, serialized) in [
                ("the first attempt", row_serialized_len(fact, false)),
                ("the fenced rebuild", row_serialized_len(fact, true)),
            ] {
                assert!(
                    serialized < MAX_CARD_JSON_CHARS,
                    "{render} on {attempt}: the row's real payload must stay inside one card, \
                     got {serialized} bytes"
                );
                assert!(
                    compaction_estimate(fact) >= serialized,
                    "{render} on {attempt}: the ceiling must cover the payload"
                );
            }
        }
    }
}
