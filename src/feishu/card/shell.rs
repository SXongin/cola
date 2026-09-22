use serde_json::json;

use super::MAX_ELEMENT_TEXT_CHARS;
use super::sanitize::CardMarkdown;
use super::tool_render::{ToolPanel, tool_panel_element};
use super::{
    CardActionButton, CardState, HeaderProgress, chunk_text, fenced_code, fmt_local_time, truncate_md,
};

/// Format a duration in seconds for the header's live timer (ADR-0014):
/// `42s`, `1m23s`, `2h5m`, `3d4h`. Whole seconds, so a header signature built
/// from it changes at most once per second — the natural flush throttle.
pub(crate) fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// A compact reasoning-length label for the thinking header: `832字`, `2.1k字`.
fn format_chars(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}k字", n as f64 / 1000.0)
    } else {
        format!("{n}字")
    }
}

/// Builds Feishu interactive card JSON (v2 schema with collapsible panels).
/// Body elements are appended in the order the builder methods are called, so
/// text and tool panels can be interleaved chronologically.
pub struct CardBuilder {
    state: CardState,
    /// Tool panels in call order (also drives the header's running-tool hint).
    tools: Vec<ToolPanel>,
    /// The Turn's globally running tool, used for the header when this card's
    /// slice carries none of its own (a split continuation's slice may not
    /// include the panel the Turn is still executing — the header describes
    /// the Turn's state, not just this card's contents). `None` keeps the
    /// slice-local selection.
    header_running_tool: Option<ToolPanel>,
    /// Body elements, in call order.
    body: Vec<serde_json::Value>,
    footer: Option<String>,
    subtitle: Option<String>,
    /// The card's date anchor (`MM-DD`, from the turn's server time), joined
    /// to the subtitle so a card spanning midnight stays readable (#183).
    date: Option<String>,
    /// JSON 2.0 buttons shown only on the Error card (e.g. a retry action).
    error_buttons: Vec<CardActionButton>,
    /// Progress/liveness inputs for the header (ADR-0014): the waiting flag,
    /// the phase timer, and the reasoning length. When absent (all defaults)
    /// the header renders the plain phase label — non-streaming builders
    /// (permission/switch cards) stay unchanged.
    progress: HeaderProgress,
    /// This card's markdown hygiene: Feishu-hostile model text is neutralized
    /// and the card's table budget tracked (see [`CardMarkdown`]).
    md: CardMarkdown,
}

/// Assemble a JSON 2.0 card from a ready-made header and body elements: the
/// one place the schema-2.0 skeleton is written. Callers that only need a
/// plain-text title plus a color template use [`card_shell`]; the streaming
/// [`CardBuilder`] passes its own header so it can add a subtitle.
fn card_with_header(header: serde_json::Value, elements: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": header,
        "body": { "elements": elements }
    })
}

/// Assemble a JSON 2.0 card whose header is a plain-text title and a color
/// template. Every non-streaming card goes through this shell, so schema churn
/// happens in exactly one place.
pub(crate) fn card_shell(title: &str, template: &str, elements: Vec<serde_json::Value>) -> serde_json::Value {
    card_with_header(
        json!({
            "title": { "tag": "plain_text", "content": title },
            "template": template
        }),
        elements,
    )
}

impl CardBuilder {
    pub fn new() -> Self {
        Self {
            state: CardState::Loading,
            tools: Vec::new(),
            header_running_tool: None,
            body: Vec::new(),
            footer: None,
            subtitle: None,
            date: None,
            error_buttons: Vec::new(),
            progress: HeaderProgress::default(),
            md: CardMarkdown::new(),
        }
    }

    /// Progress/liveness inputs for the header (ADR-0014): a waiting flag for
    /// pending permission/question interactions, the phase timer, and the
    /// reasoning length. When absent the header renders the plain phase label —
    /// builders that don't set progress (e.g. permission/switch cards) stay
    /// unchanged.
    pub fn with_progress(mut self, progress: HeaderProgress) -> Self {
        self.progress = progress;
        self
    }

    /// The session/thread name, shown as the card's subtitle so the header can
    /// stay focused on state. The card is a reply to the user's message, so the
    /// question itself is already visible in the reply context — no need to echo
    /// it again. Empty subtitles are omitted.
    pub fn with_subtitle(mut self, subtitle: &str) -> Self {
        if !subtitle.is_empty() {
            self.subtitle = Some(subtitle.to_string());
        }
        self
    }

    /// The card's date anchor (`MM-DD`, from the turn's server time — the user
    /// message the server stored): appended to the subtitle so a card that
    /// lives across midnight stays readable (#183). The caller sources it from
    /// the server, never cola's clock, so the date can't disagree with the
    /// panels; every flush (and split continuation) keeps the same value.
    /// Empty dates are ignored.
    pub fn with_date(mut self, date: &str) -> Self {
        if !date.is_empty() {
            self.date = Some(date.to_string());
        }
        self
    }

    /// Buttons shown on the Error card (rendered only in that state).
    pub fn with_error_buttons(mut self, buttons: Vec<CardActionButton>) -> Self {
        self.error_buttons = buttons;
        self
    }

    pub fn with_state(mut self, state: CardState) -> Self {
        self.state = state;
        self
    }

    /// The card-content rejection fallback (`230099`): render every
    /// model-markdown element as a fenced code block. Set by the flush after
    /// Feishu refused a card it built, so the rebuilt slice can land instead
    /// of freezing the card on a rejection that repeats forever.
    pub fn with_fenced_markdown(mut self, fenced: bool) -> Self {
        if fenced {
            self.md = CardMarkdown::fenced();
        }
        self
    }

    pub fn with_text(mut self, text: &str) -> Self {
        // Long text is split across multiple elements, each within cola's
        // per-element budget (MAX_ELEMENT_TEXT_CHARS), while the card splitter
        // bounds how much text one card carries. The split is a size budget,
        // not a workaround for a Feishu truncation. The text is sanitized as
        // one blob (per-card table budget), then chunked.
        if self.md.is_fenced() {
            // Fallback mode: fence each chunk separately, so no element holds
            // an unclosed fence.
            for chunk in chunk_text(text, MAX_ELEMENT_TEXT_CHARS) {
                self.body
                    .push(json!({ "tag": "markdown", "content": fenced_code(&chunk, None) }));
            }
            return self;
        }
        let text = self.md.clean(text);
        for chunk in chunk_text(&text, MAX_ELEMENT_TEXT_CHARS) {
            self.body.push(json!({ "tag": "markdown", "content": chunk }));
        }
        self
    }

    /// A reasoning panel with its part's start time (`HH:MM`, local) in the
    /// panel header (#183), so the time is visible while collapsed. `at_ms` is
    /// the part's server `time.start`; `None` (a payload without one) renders
    /// no clock — the card only ever shows server times. `element_id` is the
    /// panel's stable identity on the card (see [`collapsible_panel_chunks`]).
    pub fn with_reasoning_at(
        mut self,
        reasoning: &str,
        at_ms: Option<i64>,
        element_id: Option<&str>,
    ) -> Self {
        if !reasoning.is_empty() {
            let body = truncate_md(reasoning, 800);
            let body = if self.md.is_fenced() {
                fenced_code(&body, None)
            } else {
                self.md.clean(&body)
            };
            self.body.push(collapsible_panel(
                &format!("💭 推理过程{}", panel_time_suffix(at_ms)),
                &body,
                element_id,
            ));
        }
        self
    }

    /// [`Self::with_reasoning_at`] for synthetic content with no part time —
    /// tests only, mirroring the accumulator's unkeyed push forms.
    #[cfg(test)]
    pub fn with_reasoning(self, reasoning: &str) -> Self {
        self.with_reasoning_at(reasoning, None, None)
    }

    /// A tool panel with the call's start time (`HH:MM`, local) in its header
    /// (#183). `at_ms` is the part's server `state.time.start`, so a
    /// running → completed update keeps the start time (the item is keyed once,
    /// on first appearance); `None` renders no clock. `element_id` is the
    /// panel's stable identity on the card (see [`collapsible_panel_chunks`]).
    pub fn with_tool_at(mut self, tool: ToolPanel, at_ms: Option<i64>, element_id: Option<&str>) -> Self {
        let panel = tool.clone();
        self.tools.push(tool);
        // All tools are shown; the streaming card splits into continuation
        // cards when the component estimate exceeds the Feishu limit.
        self.body
            .push(tool_panel_element(&panel, at_ms, element_id, &mut self.md));
        self
    }

    /// [`Self::with_tool_at`] for panels with no part time — tests only.
    #[cfg(test)]
    pub fn with_tool(self, tool: ToolPanel) -> Self {
        self.with_tool_at(tool, None, None)
    }

    /// The Turn's globally running tool for the header, preferred over this
    /// card's slice-local panels: a split continuation's slice may not carry
    /// the panel the Turn is still executing (`sleep 30` at split time), yet
    /// its header must keep saying "⏳ tool", not fall back to "回复中".
    pub fn with_header_running_tool(mut self, tool: Option<ToolPanel>) -> Self {
        // Self-defending: only a LIVE panel may drive the header. A stale
        // override (finished/failed) would both lie and suppress the
        // slice-local fallback, so it is treated as absent.
        self.header_running_tool = tool.filter(|t| t.status == "running");
        self
    }

    pub fn with_footer(mut self, footer: &str) -> Self {
        self.footer = Some(footer.to_string());
        self
    }

    /// Push an already-built body element (used for inline permission/question
    /// sections on the streaming card).
    pub fn with_element(mut self, element: serde_json::Value) -> Self {
        self.body.push(element);
        self
    }

    /// How many body elements have been pushed so far — the streaming card
    /// records each interaction block's element range with it (ADR-0038, rule
    /// 2), so a cached card can be edited in place later.
    pub(crate) fn body_len(&self) -> usize {
        self.body.len()
    }

    /// Build the Feishu card JSON payload (v2 schema).
    pub fn build(&self) -> serde_json::Value {
        let mut elements = self.body.clone();

        if let Some(ref footer) = self.footer {
            elements.push(json!({"tag": "hr"}));
            elements.push(json!({ "tag": "markdown", "content": footer }));
        }

        // Error card actions: a retry button so the user can re-submit without
        // retyping. Only rendered in the Error state.
        if self.state == CardState::Error {
            for btn in &self.error_buttons {
                elements.push(json!({
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": btn.text },
                    "type": btn.kind,
                    "value": btn.value,
                }));
            }
        }

        // The caller's global running tool first (a continuation's slice may
        // lack the panel), else the slice-local one — unchanged for callers
        // that never set the override. Both are first-match in insertion
        // order (`Vec`/`IndexMap`), so multiple running tools pick
        // deterministically: the one started first.
        let running = self
            .header_running_tool
            .as_ref()
            .or_else(|| self.tools.iter().find(|t| t.status == "running"));
        let (header_title, template) = header_title_and_template(&self.state, running, &self.progress);
        let mut header = serde_json::json!({
            "title": { "tag": "plain_text", "content": header_title },
            "template": template
        });
        if let Some(subtitle) = match (self.subtitle.as_deref(), self.date.as_deref()) {
            (Some(title), Some(date)) => Some(format!("{title} · {date}")),
            (Some(title), None) => Some(title.to_string()),
            (None, Some(date)) => Some(date.to_string()),
            (None, None) => None,
        } {
            header["subtitle"] = serde_json::json!({
                "tag": "plain_text",
                "content": subtitle
            });
        }
        card_with_header(header, elements)
    }
}

/// Header title + template for a card. `running_tool` is the tool currently
/// running (the builder's slice-local panel, or the caller's global selection
/// via [`CardBuilder::with_header_running_tool`]), driving the "⏳ tool"
/// streaming header.
/// Active states append the progress signals passed in from the accumulator;
/// `progress.awaiting` (a pending permission and/or question) overrides the
/// phase label entirely, naming whichever kind is pending.
pub(crate) fn header_title_and_template(
    state: &CardState,
    running_tool: Option<&ToolPanel>,
    progress: &HeaderProgress,
) -> (String, &'static str) {
    if let Some(title) = progress.awaiting.title() {
        return (title.to_string(), "orange");
    }
    let (label, template) = match state {
        CardState::Loading => ("⏳ 思考中".to_string(), "blue"),
        CardState::Reasoning => ("💭 推理中".to_string(), "blue"),
        CardState::Streaming => {
            if let Some(tool) = running_tool {
                // One icon only: `status_icon` already marks running/pending
                // with ⏳, so an extra hardcoded 🔧 would show TWO icons
                // (e.g. "🔧 ⏳ bench") on long-running tools.
                (format!("{} {}", tool.status_icon(), tool.name), "orange")
            } else {
                ("✍️ 回复中".to_string(), "blue")
            }
        }
        CardState::Continued => ("⏳ 部分完成，继续中…".to_string(), "blue"),
        CardState::Done => ("✅ 完成".to_string(), "green"),
        CardState::Error => ("❌ 出错".to_string(), "red"),
    };
    let mut title = label;
    match state {
        CardState::Loading | CardState::Reasoning | CardState::Streaming => {
            if let Some(e) = progress.elapsed {
                title.push_str(&format!(" {}", fmt_elapsed(e)));
            }
            // Reasoning is streamed incrementally by OpenCode, so its length is
            // real progress during the thinking phase.
            if *state == CardState::Reasoning && progress.reasoning_chars > 0 {
                title.push_str(&format!(" · {}", format_chars(progress.reasoning_chars)));
            }
        }
        _ => {}
    }
    (title, template)
}

/// Build a collapsible panel (v2), folded by default. `element_id` is the
/// panel's stable identity on the card — see [`collapsible_panel_chunks`].
pub(super) fn collapsible_panel(title: &str, content: &str, element_id: Option<&str>) -> serde_json::Value {
    collapsible_panel_chunks(title, &[content.to_string()], element_id)
}

/// The `· HH:MM` suffix a panel header shows for its part's start time
/// (#183); empty when the part carries no server time — a fallback key is an
/// ordering device, not a clock, and rendering cola's moment in the same
/// costume would mix two clocks on one card.
pub(super) fn panel_time_suffix(at_ms: Option<i64>) -> String {
    at_ms
        .and_then(fmt_local_time)
        .map(|t| format!(" · {t}"))
        .unwrap_or_default()
}

/// Build a collapsible panel (v2) holding several markdown chunks, folded by
/// default. Used when one logical section (e.g. a snapshot tail entry's full
/// text) needs splitting across multiple markdown elements to stay within
/// cola's per-element budget ([`MAX_ELEMENT_TEXT_CHARS`]) — Feishu does not
/// truncate a long element itself; the budget keeps the card's total size and
/// element count bounded.
///
/// `element_id` names the panel for the duration of the card: the streaming
/// card re-renders its whole JSON on every flush, and the client holds each
/// panel's open/closed state locally. A panel whose id is derived from the
/// timeline item it renders (`tool_{seq}` / `reason_{seq}`) keeps that id as
/// the timeline grows or reorders, so the fold state follows the panel instead
/// of whichever panel happens to sit at its old position.
pub(crate) fn collapsible_panel_chunks(
    title: &str,
    chunks: &[String],
    element_id: Option<&str>,
) -> serde_json::Value {
    let mut panel = json!({
        "tag": "collapsible_panel",
        "expanded": false,
        "header": {
            "title": { "tag": "plain_text", "content": title },
            "icon": { "tag": "standard_icon", "token": "down-small-ccm_outlined" },
            "icon_position": "right"
        },
        "elements": chunks
            .iter()
            .map(|c| json!({ "tag": "markdown", "content": c }))
            .collect::<Vec<_>>(),
    });
    if let Some(id) = element_id {
        panel["element_id"] = json!(id);
    }
    panel
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::card::{
        AWAITING_BOTH_TITLE, AWAITING_PERMISSION_TITLE, AWAITING_QUESTION_TITLE, AwaitingAction,
    };

    /// Feishu's card parser recognizes its own tag names inside markdown and
    /// rejects a card whose tag is malformed (`<number_tag>` without a 1-99
    /// body is `230099/11311 markdown content parse error`). Text reaches the
    /// card from the model, so every `<` outside a fenced code block is
    /// neutralized with Feishu's own escape — visually identical, never a tag.
    /// Inline code is not exempt: measured 2026-09-22 it does not protect a
    /// tag once an earlier unclosed tag fragment changes the parser's context,
    /// so only fences are trusted.
    #[test]
    fn text_escapes_tag_openers_outside_fences() {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_text("a <number_tag> b\n\n```\n<number_tag>\n```\n\n`<at id=x></at>`")
            .build();
        let content = card["body"]["elements"][0]["content"].as_str().unwrap();
        assert!(content.contains("a &#60;number_tag> b"), "{content}");
        assert!(
            content.contains("```\n<number_tag>\n```"),
            "fenced code untouched: {content}"
        );
        assert!(
            content.contains("`&#60;at id=x>&#60;/at>`"),
            "inline code escaped: {content}"
        );
    }

    /// Feishu validates image keys: any `![alt](dest)` whose dest is not an
    /// image this app uploaded fails the whole card with `230099/200570 card
    /// contains invalid image keys`. Model text can never hold a valid key, so
    /// images become plain links (same target, no validation); fenced code
    /// stays literal.
    #[test]
    fn images_become_links_outside_fences() {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_text("shot ![x](./x.png) here\n\n```\n![y](./y.png)\n```")
            .build();
        let content = card["body"]["elements"][0]["content"].as_str().unwrap();
        assert!(content.contains("shot [x](./x.png) here"), "{content}");
        assert!(!content.contains("![x]"), "no image syntax left: {content}");
        assert!(
            content.contains("```\n![y](./y.png)\n```"),
            "fenced code untouched: {content}"
        );
    }

    /// Feishu counts markdown tables across the whole card (elements and
    /// panels): a 6th table fails with `230099/11310 card table number over
    /// limit`. Cola budgets 5 per card build; the overflow renders as code.
    #[test]
    fn tables_beyond_the_card_budget_become_code() {
        let table = |n: usize| format!("| t{n} | b |\n|---|---|\n| 1 | 2 |");
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_text(&format!("{}\n\n{}\n\n{}\n\n", table(1), table(2), table(3)))
            .with_text(&format!("{}\n\n{}\n\n{}\n\n", table(4), table(5), table(6)))
            .build();
        let content: String = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["content"].as_str().unwrap_or_default())
            .collect();
        assert!(
            content.contains("| t5 | b |\n|---|---|"),
            "the fifth table still renders natively: {content}"
        );
        assert!(
            content.contains("```\n| t6 | b |\n|---|---|\n| 1 | 2 |\n```"),
            "the sixth table is fenced: {content}"
        );
    }

    /// Feishu rejects a markdown table with 50 body rows (`230099/11310
    /// element exceeds the limit`); 49 render. The whole table becomes code
    /// instead of losing rows.
    #[test]
    fn over_long_tables_become_code() {
        let table = |rows: usize| {
            let mut t = String::from("| a | b |\n|---|---|\n");
            for i in 0..rows {
                t.push_str(&format!("| {i} | 2 |\n"));
            }
            t
        };
        let content_of = |rows: usize| {
            let card = CardBuilder::new()
                .with_state(CardState::Done)
                .with_text(&table(rows))
                .build();
            card["body"]["elements"][0]["content"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert!(
            content_of(49).contains("| a | b |\n|---|---|"),
            "49 rows render natively"
        );
        assert!(
            content_of(50).contains("```\n| a | b |\n|---|---|"),
            "50 rows are fenced: {}",
            content_of(50)
        );
    }

    #[test]
    fn card_shell_builds_the_json_2_0_skeleton() {
        let card = card_shell(
            "标题",
            "blue",
            vec![json!({ "tag": "markdown", "content": "正文" })],
        );
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
        assert_eq!(card["config"]["wide_screen_mode"], true);
        assert_eq!(card["header"]["title"]["content"], "标题");
        assert_eq!(card["header"]["template"], "blue");
        assert_eq!(card["body"]["elements"][0]["content"], "正文");
    }

    #[test]
    fn card_with_header_carries_the_subtitle() {
        let header = json!({
            "title": { "tag": "plain_text", "content": "标题" },
            "template": "blue",
            "subtitle": { "tag": "plain_text", "content": "会话" }
        });
        let card = card_with_header(header, vec![]);
        assert_eq!(card["header"]["subtitle"]["content"], "会话");
        assert_eq!(card["body"]["elements"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn loading_card_header() {
        let card = CardBuilder::new().with_state(CardState::Loading).build();
        let header = &card["header"]["title"]["content"];
        assert!(header.as_str().unwrap().contains("思考中"));
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
    }

    #[test]
    fn reasoning_is_collapsible_panel() {
        let card = CardBuilder::new()
            .with_state(CardState::Reasoning)
            .with_reasoning("Let me analyze this code...")
            .build();
        let header = &card["header"]["title"]["content"];
        assert!(header.as_str().unwrap().contains("推理中"));
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[0]["tag"].as_str().unwrap(), "collapsible_panel");
        assert!(!elements[0]["expanded"].as_bool().unwrap());
        assert!(elements[0].to_string().contains("analyze"));
    }

    #[test]
    fn streaming_card_shows_text() {
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_text("pub fn main() {")
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements.last().unwrap()["content"].as_str().unwrap(),
            "pub fn main() {"
        );
    }

    #[test]
    fn running_tool_shows_in_header() {
        let tool = ToolPanel {
            name: "bash".into(),
            status: "running".into(),
            input: Some(json!({"command": "cargo test"})),
            output: None,
        };
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_tool(tool)
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert!(header.contains("bash"));
        // The running tool's header must show exactly ONE icon (the status icon
        // ⏳), not a duplicated "🔧 ⏳" pair.
        assert_eq!(
            header, "⏳ bash",
            "running-tool header must be a single icon: {}",
            header
        );
        assert_eq!(card["header"]["template"].as_str().unwrap(), "orange");
    }

    /// A card whose slice has NO tool panel still shows the Turn's running tool
    /// when the caller passes the global override (a split continuation), and
    /// falls back to today's slice-local selection when it does not.
    #[test]
    fn header_running_tool_override_wins_over_the_slice() {
        let tool = ToolPanel {
            name: "bash".into(),
            status: "running".into(),
            input: Some(json!({"command": "sleep 30"})),
            output: None,
        };
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_text("补充后的内容。")
            .with_header_running_tool(Some(tool))
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert_eq!(
            header, "⏳ bash",
            "the global running tool must drive the header: {header}"
        );
        assert_eq!(card["header"]["template"].as_str().unwrap(), "orange");

        // No override: the slice-local fallback (no panel here → phase label).
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_text("补充后的内容。")
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert_eq!(
            header, "✍️ 回复中",
            "without the override the slice-local fallback stays: {header}"
        );
    }

    /// A stale override (a finished tool) is treated as absent: it neither
    /// lies in the header nor suppresses the slice-local running panel.
    #[test]
    fn a_non_running_header_tool_override_is_ignored() {
        let finished = |name: &str| ToolPanel {
            name: name.into(),
            status: "completed".into(),
            input: None,
            output: None,
        };

        // The slice still has a running panel: the stale override must not
        // hide it.
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_tool(ToolPanel {
                name: "bash".into(),
                status: "running".into(),
                input: Some(json!({"command": "sleep 30"})),
                output: None,
            })
            .with_header_running_tool(Some(finished("stale")))
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert!(
            header.starts_with("⏳ bash"),
            "the slice-local running panel must win over a stale override: {header}"
        );
        assert!(
            !header.contains("stale"),
            "the stale panel must not show: {header}"
        );

        // No running panel anywhere: the stale override falls back to the
        // plain streaming label, not to itself.
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_text("补充后的内容。")
            .with_header_running_tool(Some(finished("stale")))
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert_eq!(
            header, "✍️ 回复中",
            "a non-running override must fall back to the phase label: {header}"
        );
    }

    #[test]
    fn done_card_green() {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_text("Done!")
            .build();
        assert_eq!(card["header"]["template"].as_str().unwrap(), "green");
        assert!(
            card["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("✅")
        );
    }

    #[test]
    fn error_card_shows_red_header() {
        let card = CardBuilder::new()
            .with_state(CardState::Error)
            .with_text("**错误**: 503 request queue full")
            .build();
        assert_eq!(card["header"]["template"].as_str().unwrap(), "red");
        assert!(
            card["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("出错")
        );
        assert!(
            card["body"]["elements"][0]["content"]
                .as_str()
                .unwrap()
                .contains("503")
        );
    }

    #[test]
    fn fmt_elapsed_formats_durations() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(42), "42s");
        assert_eq!(fmt_elapsed(83), "1m23s");
        assert_eq!(fmt_elapsed(3700), "1h1m");
        assert_eq!(fmt_elapsed(90_000), "1d1h");
    }

    #[test]
    fn loading_header_with_progress_shows_timer() {
        // Without progress inputs the header is the plain phase label (existing
        // builders/tests unchanged).
        let plain = CardBuilder::new().with_state(CardState::Loading).build();
        assert_eq!(plain["header"]["title"]["content"].as_str().unwrap(), "⏳ 思考中");
        // With a phase timer the active header counts up.
        let card = CardBuilder::new()
            .with_state(CardState::Loading)
            .with_progress(HeaderProgress {
                elapsed: Some(83),
                ..Default::default()
            })
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert!(header.contains("思考中"), "{}", header);
        assert!(header.contains("1m23s"), "timer missing: {}", header);
    }

    #[test]
    fn reasoning_header_shows_timer_and_length() {
        let card = CardBuilder::new()
            .with_state(CardState::Reasoning)
            .with_progress(HeaderProgress {
                elapsed: Some(83),
                reasoning_chars: 2100,
                ..Default::default()
            })
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert!(header.contains("推理中"), "{}", header);
        assert!(header.contains("1m23s"), "timer missing: {}", header);
        assert!(header.contains("2.1k字"), "reasoning length missing: {}", header);
        assert_eq!(card["header"]["template"].as_str().unwrap(), "blue");
    }

    #[test]
    fn waiting_header_names_the_pending_request_kind() {
        // A pending permission/question pauses the turn: the header says the
        // truth — waiting for the authorization, for the answer, or both —
        // not stuck.
        let header_of = |awaiting| {
            CardBuilder::new()
                .with_state(CardState::Reasoning)
                .with_progress(HeaderProgress {
                    awaiting,
                    elapsed: Some(83),
                    ..Default::default()
                })
                .build()
        };
        for (awaiting, title) in [
            (AwaitingAction::Permission, AWAITING_PERMISSION_TITLE),
            (AwaitingAction::Question, AWAITING_QUESTION_TITLE),
            (AwaitingAction::Both, AWAITING_BOTH_TITLE),
        ] {
            let card = header_of(awaiting);
            assert_eq!(
                card["header"]["title"]["content"].as_str().unwrap(),
                title,
                "awaiting state {awaiting:?} must name itself"
            );
            assert_eq!(card["header"]["template"].as_str().unwrap(), "orange");
        }
        // Nothing pending: the phase label stays, with its own template.
        let card = header_of(AwaitingAction::None);
        assert!(
            card["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("推理中"),
            "no awaiting -> phase label: {card}"
        );
        assert_eq!(card["header"]["template"].as_str().unwrap(), "blue");
    }

    #[test]
    fn header_changes_with_progress_and_state() {
        // The render poll flushes when the header changes: the timer tick and a
        // state change must each yield distinct headers.
        let header_of = |state: CardState, progress: HeaderProgress| {
            CardBuilder::new()
                .with_state(state)
                .with_progress(progress)
                .build()["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let tick = |elapsed: Option<u64>| HeaderProgress {
            elapsed,
            ..Default::default()
        };
        assert_ne!(
            header_of(CardState::Streaming, tick(Some(0))),
            header_of(CardState::Streaming, tick(Some(1))),
            "timer tick must change the header"
        );
        assert_ne!(
            header_of(CardState::Streaming, tick(Some(1))),
            header_of(
                CardState::Streaming,
                HeaderProgress {
                    awaiting: AwaitingAction::Permission,
                    ..tick(Some(1))
                }
            ),
            "waiting must change the header"
        );
    }

    #[test]
    fn subtitle_shows_session_name_in_header() {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_subtitle("proj-lib")
            .with_text("有 3 个文件。")
            .build();
        assert_eq!(
            card["header"]["subtitle"]["content"].as_str().unwrap(),
            "proj-lib"
        );
        // The header itself is state-only — the question is not echoed.
        assert_eq!(card["header"]["title"]["content"].as_str().unwrap(), "✅ 完成");
        // No "问:" body element (the reply context already shows the question).
        let text = card.to_string();
        assert!(!text.contains("**问**"), "question echoed: {}", text);
    }

    /// #183: the panel header carries its part's start time (`HH:MM`, local),
    /// so the time stays visible while the panel is collapsed. A part with no
    /// server time (a fallback key) shows no clock — never cola's.
    #[test]
    fn reasoning_panel_header_carries_the_start_time() {
        let at = crate::feishu::card::test_local_ms(2026, 9, 16, 14, 3);
        let card = CardBuilder::new()
            .with_state(CardState::Reasoning)
            .with_reasoning_at("Let me analyze this code...", Some(at), None)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[0]["header"]["title"]["content"].as_str().unwrap(),
            "💭 推理过程 · 14:03"
        );

        let untimed = CardBuilder::new()
            .with_state(CardState::Reasoning)
            .with_reasoning_at("synthetic", None, None)
            .build();
        let elements = untimed["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[0]["header"]["title"]["content"].as_str().unwrap(),
            "💭 推理过程"
        );
    }

    /// #183: the header's date anchor is the turn's SERVER time, not "now",
    /// so it stays stable across flushes.
    #[test]
    fn header_date_joins_the_subtitle() {
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_subtitle("proj-lib")
            .with_date("09-16")
            .build();
        assert_eq!(
            card["header"]["subtitle"]["content"].as_str().unwrap(),
            "proj-lib · 09-16"
        );
        // A card with no subtitle still carries the date.
        let bare = CardBuilder::new()
            .with_state(CardState::Done)
            .with_date("09-16")
            .build();
        assert_eq!(bare["header"]["subtitle"]["content"].as_str().unwrap(), "09-16");
    }

    #[test]
    fn failed_tool_does_not_fail_whole_card() {
        // A failed tool call is a normal part of an agent run — the model
        // retries or works around it. The card stays "✅ 完成"; the failure is
        // shown only on the tool's own panel (❌ + reason).
        let tool = ToolPanel {
            name: "edit".into(),
            status: "error".into(),
            input: Some(json!({"filePath": "src/main.rs"})),
            output: Some("❌ Could not find oldString...".into()),
        };
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        assert_eq!(card["header"]["template"].as_str().unwrap(), "green");
        assert!(
            card["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("完成")
        );
    }

    #[test]
    fn tool_panels_are_all_rendered() {
        // All tool panels are shown; the streaming card splits into
        // continuation cards when the component estimate crosses the limit.
        let mut builder = CardBuilder::new().with_state(CardState::Done);
        for i in 0..25 {
            builder = builder.with_tool(ToolPanel {
                name: format!("tool {}", i),
                status: "completed".into(),
                input: Some(json!("in")),
                output: Some("out".into()),
            });
        }
        let card = builder.build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let panels = elements
            .iter()
            .filter(|e| e["tag"].as_str() == Some("collapsible_panel"))
            .count();
        assert_eq!(panels, 25, "every tool panel must be rendered");
        assert!(
            !elements.iter().any(|e| e.to_string().contains("工具未显示")),
            "no hidden-tools note should appear"
        );
    }

    #[test]
    fn long_text_splits_across_multiple_elements() {
        let long = "x".repeat(MAX_ELEMENT_TEXT_CHARS * 2 + 100);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_text(&long)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let text_els: Vec<_> = elements.iter().filter(|e| e["tag"] == "markdown").collect();
        assert!(
            text_els.len() >= 3,
            "long text must split into multiple elements, got {}: {}",
            text_els.len(),
            card
        );
        // No element exceeds cola's per-element budget, and NOTHING is
        // truncated — the full text is preserved across the elements.
        for el in &text_els {
            let content = el["content"].as_str().unwrap();
            assert!(
                content.chars().count() <= MAX_ELEMENT_TEXT_CHARS,
                "element over cap: {} chars",
                content.chars().count()
            );
        }
        let joined: String = text_els.iter().map(|e| e["content"].as_str().unwrap()).collect();
        assert_eq!(joined, long, "all text must be preserved, none truncated");
    }

    #[test]
    fn short_text_is_not_truncated() {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_text("short reply")
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let text_el = elements.iter().find(|e| e["tag"] == "markdown").unwrap();
        assert_eq!(text_el["content"].as_str().unwrap(), "short reply");
    }
}
