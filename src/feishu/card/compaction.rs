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
use crate::feishu::card::sanitize::CardMarkdown;
use crate::feishu::card::shell::collapsible_panel_chunks;
use crate::feishu::card::{MAX_ELEMENT_TEXT_CHARS, chunk_text, format_tokens};
use serde_json::json;

/// The card elements one Compaction row is made of (ADR-0082), shared by every
/// surface that shows a row — the live card's timeline
/// ([`crate::feishu::card::shell::CardBuilder::with_compaction`]) and the Session
/// Snapshot's 最近对话 tail (ticket #723) — so the two cannot disagree on the
/// copy, the fold, or the session time they render.
///
/// A completed compaction is one folded panel: `element_id` its stable identity
/// (the reader's fold state survives an in-place update), the panel's title the
/// row's, the fold body the summary chunked to cola's per-element budget. With
/// nothing to fold — an in-flight compaction, a failure, or a completion the
/// Backend gave no text (an orphaned V1 part, say) — the row is its title alone.
/// `md` is the card's markdown state, so the summary draws from the card's one
/// table budget and fenced fallback.
pub(crate) fn compaction_elements(
    compaction: &Compaction,
    element_id: Option<&str>,
    md: &mut CardMarkdown,
) -> Vec<serde_json::Value> {
    let title = compaction_title(compaction);
    match compaction_summary(compaction) {
        Some(summary) => {
            let chunks: Vec<String> = chunk_text(&summary, MAX_ELEMENT_TEXT_CHARS)
                .iter()
                .map(|chunk| md.element(chunk))
                .collect();
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
/// domain fact.
pub(crate) fn compaction_title(compaction: &Compaction) -> String {
    let reason = match compaction.reason {
        CompactionReason::Auto => "自动",
        CompactionReason::Manual => "手动",
    };
    let title = match compaction.status {
        CompactionStatus::Running => format!("⏳ 压缩中（{reason}）"),
        CompactionStatus::Completed => format!("已压缩上下文（{reason}）"),
        CompactionStatus::Failed => format!(
            "⚠️ 压缩失败：{}",
            compaction
                .error
                .as_deref()
                .filter(|error| !error.is_empty())
                .unwrap_or("未知错误")
        ),
    };
    // Presence, not magnitude: a request the server reports with a zero usage is
    // still a reported usage, and the row says so.
    match compaction.usage.map(|usage| usage.total) {
        Some(total) => format!("{title} · {} tokens", format_tokens(total)),
        None => title,
    }
}

/// The summary a folded row shows, with whitespace around it trimmed. `None`
/// when there is none to fold — a running or failed compaction, and a completed
/// one the Backend never gave a text (an orphaned V1 part, say): the row is then
/// its title alone, rather than a fold holding nothing.
pub(crate) fn compaction_summary(compaction: &Compaction) -> Option<String> {
    let summary = compaction.summary.trim();
    (!summary.is_empty()).then(|| summary.to_string())
}

/// How many body elements one Compaction row adds: a plain line is one element,
/// a folded row is the panel (as a reasoning panel charges it) plus one markdown
/// element per summary chunk.
pub(crate) fn compaction_components(compaction: &Compaction) -> usize {
    match compaction_summary(compaction) {
        Some(summary) => 3 + chunk_text(&summary, MAX_ELEMENT_TEXT_CHARS).len(),
        None => 1,
    }
}

/// The bytes one Compaction row adds to the body (ADR-0082): its title, plus —
/// for a folded row — the panel's own element overhead and the summary's
/// markdown chunks, measured from the same chunks the render builds so the two
/// cannot drift. The summary is charged in JSON bytes rather than as card text,
/// like a ledger entry's output: a fold body is one element per chunk and cannot
/// be split the way streamed text can.
pub(crate) fn compaction_estimate(compaction: &Compaction) -> usize {
    let title = compaction_title(compaction).len();
    match compaction_summary(compaction) {
        None => title + 80,
        Some(summary) => {
            let chunks = chunk_text(&summary, MAX_ELEMENT_TEXT_CHARS);
            300 + title + chunks.iter().map(|chunk| chunk.len() + 40).sum::<usize>()
        }
    }
}
