use serde_json::json;

use super::MAX_ELEMENT_TEXT_CHARS;
use super::tool_render::{ToolPanel, tool_panel_element};
use super::{AWAITING_ACTION_TITLE, CardActionButton, CardState, HeaderProgress, chunk_text, truncate_md};

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
    /// Body elements, in call order.
    body: Vec<serde_json::Value>,
    footer: Option<String>,
    subtitle: Option<String>,
    /// JSON 2.0 buttons shown only on the Error card (e.g. a retry action).
    error_buttons: Vec<CardActionButton>,
    /// Progress/liveness inputs for the header (ADR-0014): the waiting flag,
    /// the phase timer, and the reasoning length. When absent (all defaults)
    /// the header renders the plain phase label — non-streaming builders
    /// (permission/switch cards) stay unchanged.
    progress: HeaderProgress,
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
            body: Vec::new(),
            footer: None,
            subtitle: None,
            error_buttons: Vec::new(),
            progress: HeaderProgress::default(),
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

    /// Buttons shown on the Error card (rendered only in that state).
    pub fn with_error_buttons(mut self, buttons: Vec<CardActionButton>) -> Self {
        self.error_buttons = buttons;
        self
    }

    pub fn with_state(mut self, state: CardState) -> Self {
        self.state = state;
        self
    }

    pub fn with_text(mut self, text: &str) -> Self {
        // Long text is split across multiple elements (each within the
        // per-element limit) so nothing is truncated; the card splitter bounds
        // how much text one card carries.
        for chunk in chunk_text(text, MAX_ELEMENT_TEXT_CHARS) {
            self.body.push(json!({ "tag": "markdown", "content": chunk }));
        }
        self
    }

    pub fn with_reasoning(mut self, reasoning: &str) -> Self {
        if !reasoning.is_empty() {
            self.body
                .push(collapsible_panel("💭 推理过程", &truncate_md(reasoning, 800)));
        }
        self
    }

    pub fn with_tool(mut self, tool: ToolPanel) -> Self {
        let panel = tool.clone();
        self.tools.push(tool);
        // All tools are shown; the streaming card splits into continuation
        // cards when the component estimate exceeds the Feishu limit.
        self.body.push(tool_panel_element(&panel));
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

        let running = self.tools.iter().find(|t| t.status == "running");
        let (header_title, template) = header_title_and_template(&self.state, running, &self.progress);
        let mut header = serde_json::json!({
            "title": { "tag": "plain_text", "content": header_title },
            "template": template
        });
        if let Some(ref subtitle) = self.subtitle {
            header["subtitle"] = serde_json::json!({
                "tag": "plain_text",
                "content": subtitle
            });
        }
        card_with_header(header, elements)
    }
}

/// Header title + template for a card. `running_tool` is the tool currently
/// running (from the builder's tools), driving the "⏳ tool" streaming header.
/// Active states append the progress signals passed in from the accumulator;
/// `progress.waiting` (a pending permission/question) overrides the phase label
/// entirely.
pub(crate) fn header_title_and_template(
    state: &CardState,
    running_tool: Option<&ToolPanel>,
    progress: &HeaderProgress,
) -> (String, &'static str) {
    if progress.waiting {
        return (AWAITING_ACTION_TITLE.to_string(), "orange");
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

/// Build a collapsible panel (v2), folded by default.
pub(super) fn collapsible_panel(title: &str, content: &str) -> serde_json::Value {
    collapsible_panel_chunks(title, &[content.to_string()])
}

/// Build a collapsible panel (v2) holding several markdown chunks, folded by
/// default. Used when one logical section (e.g. a snapshot tail entry's full
/// text) needs splitting across multiple markdown elements to stay within the
/// per-element character cap — a single `content` string would silently truncate.
pub(crate) fn collapsible_panel_chunks(title: &str, chunks: &[String]) -> serde_json::Value {
    json!({
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn waiting_header_overrides_phase() {
        // A pending permission/question pauses the turn: the header says the
        // truth — it's waiting for the user, not stuck.
        let card = CardBuilder::new()
            .with_state(CardState::Reasoning)
            .with_progress(HeaderProgress {
                waiting: true,
                elapsed: Some(83),
                ..Default::default()
            })
            .build();
        assert_eq!(
            card["header"]["title"]["content"].as_str().unwrap(),
            AWAITING_ACTION_TITLE
        );
        assert_eq!(card["header"]["template"].as_str().unwrap(), "orange");
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
                    waiting: true,
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
        // No element exceeds the per-element cap, and NOTHING is truncated —
        // the full text is preserved across the elements.
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
