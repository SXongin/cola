pub(crate) mod help;
pub(crate) mod notify;
pub(crate) mod picker;
pub(crate) mod question;
pub(crate) mod session;
pub(crate) mod shell;
pub(crate) mod tool_render;

/// Card state for Feishu interactive message cards.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CardState {
    #[default]
    Loading,
    Reasoning,
    Streaming,
    /// A card that was finalized mid-turn because the content filled the card;
    /// the rest continues in the next card. Shown as "部分完成，继续中".
    Continued,
    Done,
    Error,
}

/// How much text ONE card carries before it is finalized and the rest continues
/// on the next card. Kept below Feishu's card limits so a card full of text
/// never overflows; long answers flow across continuation cards instead of a
/// separate plain-text message.
pub const MAX_CARD_TEXT_CHARS: usize = 6000;

/// Estimated component ceiling for a card body. Feishu rejects cards over ~200
/// components (ErrCode 11310); when a streaming card would cross this it is
/// finalized with a "to be continued" marker and a fresh continuation card is
/// sent instead.
pub const MAX_CARD_COMPONENTS: usize = 150;

/// Feishu's documented ceiling for a card's serialized body (the message API
/// rejects cards above it; ~30KB, returned as `230099` / "create universal
/// card fail" 200800). `MAX_CARD_JSON_CHARS` keeps cards under this by a
/// comfortable margin, since the split estimate trails the real serialized
/// size by the un-accounted tail (footer, inline buttons) plus per-element
/// overhead.
pub const FEISHU_CARD_LIMIT_BYTES: usize = 30_000;

/// Estimated JSON size ceiling for a card body. Feishu's documented card limit
/// is [`FEISHU_CARD_LIMIT_BYTES`], and the message API rejects cards far below
/// the old 100KB assumption — a real 44KB card fails with 230099 / "create
/// universal card fail" (200800). A streaming card splits when the estimate
/// crosses this, keeping every card comfortably under the 30KB cap (the
/// estimate trails the serialized size by a few hundred bytes per element, so
/// the margin absorbs it). The 5KB gap to the hard limit covers the tail
/// sections (footer, inline permission/question buttons) the estimate doesn't
/// count.
pub const MAX_CARD_JSON_CHARS: usize = FEISHU_CARD_LIMIT_BYTES - 5_000;

/// A single text element; bound it so a very long reply never pushes the card
/// over Feishu's total card size / element limits. Reasoning, tool input/output
/// and the question are already truncated per-element.
pub const MAX_ELEMENT_TEXT_CHARS: usize = 3000;

/// Split `text` into chunks of at most `max` chars (character-aware), keeping
/// the full content. Used to bound single card elements and timeline items.
pub(crate) fn chunk_text(text: &str, max: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let take: String = rest.chars().take(max).collect();
        chunks.push(take.clone());
        rest = &rest[take.len()..];
    }
    chunks
}

/// Progress/liveness signals for the card header (ADR-0014): whether the turn
/// is paused waiting for a permission/question, how long the current phase has
/// run, and the reasoning text length. Bundled so they travel through the
/// builder, the header renderer, and the accumulator as one unit instead of
/// three loose values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeaderProgress {
    /// A permission/question is pending: the header shows "等待你的授权/回答"
    /// instead of the phase label — the turn is paused, not stuck.
    pub waiting: bool,
    /// Seconds the current phase (thinking/reasoning/tool/streaming) has run.
    pub elapsed: Option<u64>,
    /// Reasoning text length, shown on the thinking header as real progress.
    pub reasoning_chars: usize,
}

/// A JSON 2.0 button (tag: `button`, sits directly in `body.elements`).
#[derive(Debug, Clone)]
pub struct CardActionButton {
    pub text: String,
    /// `primary` | `default` | `danger`.
    pub kind: &'static str,
    /// Callback payload delivered to cola on click.
    pub value: serde_json::Value,
}

/// The header title while a permission/question blocks the turn (ADR-0014).
/// Tests assert this constant so the copy lives in one place.
pub(crate) const AWAITING_ACTION_TITLE: &str = "⏳ 等待你的授权/回答";

/// Wrap `text` in a fenced code block so long lines render without wrapping.
/// The fence is sized longer than any backtick run in the content, so an
/// embedded ``` can't break out of the block.
pub(crate) fn fenced_code(text: &str, lang: Option<&str>) -> String {
    let max_run = text
        .chars()
        .fold((0usize, 0usize), |(best, run), c| {
            if c == '`' {
                let run = run + 1;
                (best.max(run), run)
            } else {
                (best, 0)
            }
        })
        .0;
    let fence = "`".repeat((max_run + 1).max(3));
    let head = match lang {
        Some(l) if !l.is_empty() => format!("{fence}{l}"),
        _ => fence.clone(),
    };
    format!("{head}\n{text}\n{fence}")
}

/// A display label for a session title: strips raw Feishu mention tokens
/// (`@_user_N`) and drops meaningless default titles — the `/new`-generated
/// `sess-<uuid>` and the server's `New session - <iso>` / `Child session - <iso>`
/// placeholders (the caller then shows the session ID instead). Used for card
/// subtitles and notification cards.
pub fn clean_session_label(name: &str) -> String {
    let cleaned = crate::feishu::message::strip_mention_tokens(name);
    if (cleaned.starts_with("sess-") && cleaned.len() == 41)
        || cleaned.starts_with("New session - ")
        || cleaned.starts_with("Child session - ")
    {
        String::new()
    } else {
        cleaned
    }
}

/// `HH:MM` in the machine's local time for an epoch-millisecond instant — the
/// panel header suffix (#183) and the permission receipt's clock (ADR-0038
/// rule 4) read the same format. `None` for a value outside chrono's
/// representable range (never a real part key), so callers can skip the suffix.
pub(crate) fn fmt_local_time(epoch_ms: i64) -> Option<String> {
    format_local(epoch_ms, "%H:%M")
}

/// `MM-DD` in the machine's local time — the card header's date anchor
/// (#183), taken from the turn's submit epoch so it is stable across flushes.
pub(crate) fn fmt_local_date(epoch_ms: i64) -> Option<String> {
    format_local(epoch_ms, "%m-%d")
}

fn format_local(epoch_ms: i64, fmt: &str) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|at| at.with_timezone(&chrono::Local).format(fmt).to_string())
}

/// Build an epoch-millisecond instant from a local wall time, so tests can
/// assert `HH:MM` / `MM-DD` strings that hold in any machine timezone.
#[cfg(test)]
pub(crate) fn test_local_ms(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
    use chrono::TimeZone;

    chrono::Local
        .with_ymd_and_hms(y, m, d, h, min, 0)
        .single()
        .expect("unambiguous local time")
        .timestamp_millis()
}

/// Clip `text` to at most `max_len` characters, appending a "…" marker when it
/// was cut. Character-counted so CJK content (3 bytes/char) is truncated at the
/// same visual length as ASCII instead of at a byte budget.
pub(crate) fn truncate_md(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max_len).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #183: both panel headers and the card header date read the same local
    /// clock through these helpers. The expected strings are built from a local
    /// wall time, so they hold on any test-machine timezone.
    #[test]
    fn local_time_helpers_format_the_machine_zone() {
        let at = test_local_ms(2026, 9, 16, 14, 3);
        assert_eq!(fmt_local_time(at).as_deref(), Some("14:03"));
        assert_eq!(fmt_local_date(at).as_deref(), Some("09-16"));
        // Unrepresentable epochs (never a real server key) format to nothing
        // instead of panicking.
        assert_eq!(fmt_local_time(i64::MAX), None);
        assert_eq!(fmt_local_date(i64::MIN), None);
    }

    #[test]
    fn clean_session_label_handles_uuid_and_mentions() {
        // `/new`-generated `sess-<uuid>` names are meaningless → empty label, so
        // the caller shows the session ID instead (never "新会话").
        assert_eq!(
            clean_session_label("sess-7a025fa5-74a1-44e0-b5c5-80b9a21f71bc"),
            ""
        );
        assert_eq!(clean_session_label("@_user_1 你好"), "你好");
        assert_eq!(clean_session_label("frontend-refactor"), "frontend-refactor");
        // A notification card shows the cleaned label, not the raw name.
        let card = notify::build_external_message_card("sess-7a025fa5-74a1-44e0-b5c5-80b9a21f71bc", "hi");
        let text = card.to_string();
        assert!(!text.contains("sess-"), "raw sess-uuid must not leak: {}", text);
    }
}
