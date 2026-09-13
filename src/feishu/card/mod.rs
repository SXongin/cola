mod help;
mod notify;
mod picker;
mod question;
mod session;
mod shell;
mod tool_render;

pub use help::build_help_card;
pub use notify::{build_external_message_card, build_resolved_elsewhere_card};
pub(crate) use picker::{PICKER_BACK_TO_PROVIDERS, PickerLevel};
pub use picker::{
    build_agent_card, build_autoaccept_card, build_model_picker_cards, build_model_provider_cards,
    build_think_card,
};
pub use question::{
    build_permission_card, build_question_card, permission_buttons, question_elements, question_summary,
};
pub use session::{build_dir_card, build_force_confirm_card, build_switch_card};
pub use shell::CardBuilder;
pub(crate) use shell::{card_shell, collapsible_panel_chunks, header_title_and_template};
pub(crate) use tool_render::parse_edit_diff;
pub use tool_render::{TOOL_OUTPUT_MAX_CHARS, ToolPanel};

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
        let card = build_external_message_card("sess-7a025fa5-74a1-44e0-b5c5-80b9a21f71bc", "hi");
        let text = card.to_string();
        assert!(!text.contains("sess-"), "raw sess-uuid must not leak: {}", text);
    }
}
