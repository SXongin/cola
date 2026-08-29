use crate::feishu::card::{CardBuilder, CardState, ToolPanel};
use indexmap::IndexMap;

/// One entry on a turn's chronological timeline. The card renders text,
/// reasoning and tool panels in this order, matching how OpenChamber shows them
/// interleaved.
#[derive(Debug, Clone)]
pub enum TimelineItem {
    Text(String),
    Reasoning(String),
    Tool(String),
}

/// A permission request surfaced inline on the streaming card (instead of a
/// separate card), so the whole turn lives on ONE card.
#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub session_id: String,
    pub request_id: String,
    pub body: String,
    pub directory: String,
}

/// A `question` tool request surfaced inline on the streaming card. `answers[i]`
/// tracks which questions are already answered (None = open), kept in sync with
/// `App.question_partial`.
#[derive(Debug, Clone)]
pub struct PendingQuestion {
    pub request_id: String,
    pub session_id: String,
    pub questions: Vec<crate::opencode::client::QuestionInfo>,
    pub directory: String,
    pub answers: Vec<Option<Vec<String>>>,
}

/// One live card per session: the streaming accumulator plus the card identity
/// chain — the current live card's message id, updated in place by
/// `flush_card` (including continuation cards). Replaces the two per-session
/// maps kept in lockstep; owned by [`SharedCore::cards`].
pub struct CardSession {
    pub acc: StreamAccumulator,
    pub card_message_id: Option<String>,
}

/// Accumulates streaming state for one session.
#[derive(Default)]
pub struct StreamAccumulator {
    pub card_state: CardState,
    pub text: String,
    pub reasoning: String,
    /// Tool panels keyed by call ID (current state; `timeline` keeps order).
    pub tools: IndexMap<String, ToolPanel>,
    /// Text chunks and tool markers in the order they happened — the card is
    /// built from this, so message ↔ tool interleaving is preserved.
    pub timeline: Vec<TimelineItem>,
    /// Permission requests surfaced inline on this turn's card.
    pub pending_permissions: Vec<PendingPermission>,
    /// Question requests surfaced inline on this turn's card.
    pub pending_questions: Vec<PendingQuestion>,
    /// Timeline index the CURRENT card starts rendering from. When a card fills
    /// up (Feishu component limit) it is finalized with a "to be continued"
    /// marker and `render_from` advances — a fresh continuation card renders the
    /// remaining timeline from there.
    pub render_from: usize,
    /// Provider ID of the model answering this turn (e.g. "opencode-go").
    pub provider_id: Option<String>,
    /// Model ID of the model answering this turn (e.g. "deepseek-v4-flash").
    pub model_id: Option<String>,
    /// Context tokens the model consumed this turn (includes cached prefix), for
    /// the context-usage ratio in the card footer.
    pub context_tokens: i64,
    /// Estimated context-window usage (0..1); set when the turn completes.
    pub context_ratio: Option<f64>,
    /// Working directory of the session, shown in the card footer.
    pub directory: Option<String>,
    /// The session/thread name; shown as the card subtitle so the header can
    /// stay focused on state (the question is already in the reply context).
    pub title: String,
    pub token_cost: Option<(i64, i64)>,
    pub error: Option<String>,
    pub reply_to_message_id: Option<String>,
    /// This turn's session id, carried on the error-card retry button so the
    /// card callback can find the accumulator + card to reuse.
    pub session_id: Option<String>,
    /// The full original prompt text of this turn, kept so the error-card
    /// "retry" button can re-submit it without the user retyping.
    pub prompt: Option<String>,
    /// Who sent the prompt (Feishu open_id), so the group completion notice can
    /// be replied to them / @-mention them.
    pub requester_open_id: Option<String>,
    /// Whether the prompt came from a group chat (completion notice is group-only).
    pub is_group: bool,
    /// Part ids already rendered into this card — dedupes incremental polling.
    /// Reasoning/text parts are written empty first and updated with full text,
    /// so they are only tracked once they have content. Tool parts are tracked
    /// separately in `rendered_tool_states` because they get re-rendered on
    /// status changes (running → completed).
    pub rendered_parts: std::collections::HashSet<String>,
    /// callID → state signature for tool panels (status + output length); a tool
    /// is re-rendered when its signature changes.
    pub rendered_tool_states: std::collections::HashMap<String, String>,
    /// Epoch (ms) when this turn's prompt was submitted. Parts written at or
    /// after this time belong to this turn.
    pub submit_epoch_ms: Option<i64>,
}

impl StreamAccumulator {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            reply_to_message_id: None,
            session_id: None,
            prompt: None,
            requester_open_id: None,
            is_group: false,
            rendered_parts: std::collections::HashSet::new(),
            rendered_tool_states: std::collections::HashMap::new(),
            submit_epoch_ms: None,
            ..Default::default()
        }
    }

    /// Append a text chunk, keeping it in the chronological timeline (merging
    /// consecutive text chunks so the card doesn't produce one element each).
    /// Text is chunked so no single timeline item exceeds `MAX_CARD_TEXT_CHARS`
    /// — the card splitter can then break a long answer across cards at item
    /// boundaries instead of truncating it.
    pub fn push_text(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        let max = crate::feishu::card::MAX_CARD_TEXT_CHARS;
        let mut remaining = chunk;
        while !remaining.is_empty() {
            let space = match self.timeline.last_mut() {
                Some(TimelineItem::Text(last)) => max.saturating_sub(last.chars().count()),
                _ => 0,
            };
            if space == 0 {
                let take: String = remaining.chars().take(max).collect();
                self.timeline.push(TimelineItem::Text(take.clone()));
                remaining = &remaining[take.len()..];
                continue;
            }
            let take: String = remaining.chars().take(space).collect();
            match self.timeline.last_mut() {
                Some(TimelineItem::Text(last)) => last.push_str(&take),
                _ => self.timeline.push(TimelineItem::Text(take.clone())),
            }
            remaining = &remaining[take.len()..];
        }
    }

    /// Append a reasoning chunk, keeping it in the chronological timeline
    /// (consecutive chunks merge into one panel).
    pub fn push_reasoning(&mut self, chunk: &str) {
        self.reasoning.push_str(chunk);
        match self.timeline.last_mut() {
            Some(TimelineItem::Reasoning(last)) => last.push_str(chunk),
            _ => self.timeline.push(TimelineItem::Reasoning(chunk.to_string())),
        }
    }

    /// Insert/update a tool panel; the timeline gets a marker only on the FIRST
    /// appearance (state updates re-render in place).
    pub fn push_tool(&mut self, call_id: &str, panel: ToolPanel) {
        let is_new = !self.tools.contains_key(call_id);
        self.tools.insert(call_id.to_string(), panel);
        if is_new {
            self.timeline.push(TimelineItem::Tool(call_id.to_string()));
        }
    }

    /// Build the whole card (tests + simple callers). Assembles the full
    /// timeline with the tail sections (inline interactions, buttons, footer).
    #[cfg(test)]
    pub fn build_card(&self) -> serde_json::Value {
        self.build_card_inner(0, self.timeline.len(), true, None)
    }

    /// Build the card from `render_from` onward, splitting when the estimated
    /// component count would exceed Feishu's card limit. Returns `(card, full)`:
    /// when `full`, the card is finalized with the "部分完成，继续中…" header
    /// state and `render_from` has advanced past the split point — the caller
    /// then sends a fresh continuation card. When not full, the card includes
    /// the tail sections and is the turn's final visible card.
    pub fn build_card_with_split(&mut self) -> (serde_json::Value, bool) {
        let split = self.estimate_split_index(self.render_from);
        let full = split < self.timeline.len();
        let state = if full { Some(CardState::Continued) } else { None };
        let card = self.build_card_inner(self.render_from, split, !full, state);
        // Advance `render_from` ONLY on an actual split: while the card still
        // fits, subsequent flushes must re-render from the SAME start so the
        // content accumulates instead of only showing the latest delta.
        if full {
            self.render_from = split;
        }
        (card, full)
    }

    /// First timeline index whose items would push the estimated component
    /// count over `MAX_CARD_COMPONENTS`, the estimated JSON size over
    /// `MAX_CARD_JSON_CHARS`, or the accumulated text over
    /// `MAX_CARD_TEXT_CHARS` (Feishu caps total card size too, not just the
    /// element count). Returns `timeline.len()` when everything fits.
    ///
    /// The size estimate is in **UTF-8 bytes** — Feishu rejects on bytes, not
    /// characters, so CJK content (3 bytes/char) counts 3x what a char-count
    /// estimate would. It adds each element's serialized JSON overhead
    /// (measured ~260-340 bytes for a collapsible panel, ~35 for a markdown
    /// element) so the estimate trails the real card size by only a few
    /// hundred bytes — the `MAX_CARD_JSON_CHARS` margin absorbs the rest.
    fn estimate_split_index(&self, start: usize) -> usize {
        // Byte length of the first `n` chars (mirrors `truncate_md`, which caps
        // rendered content by characters).
        let first_n_bytes = |s: &str, n: usize| s.chars().take(n).map(|c| c.len_utf8()).sum::<usize>();
        let mut comps = 0usize;
        let mut size = 0usize;
        let mut card_text = 0usize;
        for (i, item) in self.timeline.iter().enumerate().skip(start) {
            let (c, s, t) = match item {
                TimelineItem::Reasoning(r) => (4, 300 + first_n_bytes(r, 800), 0),
                TimelineItem::Tool(call_id) => {
                    let panel_size = self.tools.get(call_id).map(|p| {
                        let input = p
                            .input
                            .as_ref()
                            .map(|x| x.to_string())
                            .map(|s| first_n_bytes(&s, 400))
                            .unwrap_or(0);
                        let output = p
                            .output
                            .as_deref()
                            .map(|s| first_n_bytes(s, crate::feishu::card::TOOL_OUTPUT_MAX_CHARS))
                            .unwrap_or(0);
                        400 + input + output
                    });
                    (4, panel_size.unwrap_or(400), 0)
                }
                // Text is chunked to ≤ MAX_CARD_TEXT_CHARS per item, so a
                // single item never exceeds the per-card budget; the budget
                // accumulates across items and splits at the boundary. The
                // component estimate mirrors `with_text`, which splits each
                // element at MAX_ELEMENT_TEXT_CHARS.
                TimelineItem::Text(t) => {
                    let chars = t.chars().count();
                    let elements = chars.div_ceil(crate::feishu::card::MAX_ELEMENT_TEXT_CHARS);
                    (elements, t.len() + 40, chars)
                }
            };
            comps += c;
            size += s;
            card_text += t;
            if comps > crate::feishu::card::MAX_CARD_COMPONENTS
                || size > crate::feishu::card::MAX_CARD_JSON_CHARS
                || card_text > crate::feishu::card::MAX_CARD_TEXT_CHARS
            {
                return i;
            }
        }
        self.timeline.len()
    }

    /// Assemble the card JSON for `timeline[start..end]`. `include_tail` adds
    /// the non-timeline sections (inline permission/question, error, retry
    /// button, token footer) — only the turn's final card should carry them.
    /// `state_override` forces the header state (e.g. "部分完成" on split cards).
    fn build_card_inner(
        &self,
        start: usize,
        end: usize,
        include_tail: bool,
        state_override: Option<CardState>,
    ) -> serde_json::Value {
        let state = state_override.unwrap_or_else(|| self.card_state.clone());
        let mut builder = CardBuilder::new().with_state(state);

        // The card is a reply to the user's message, so the session/thread name
        // goes in the subtitle and the question is NOT echoed again.
        builder = builder.with_subtitle(&self.title);

        // Render reasoning, text and tool panels in CHRONOLOGICAL order
        // (interleaved), like OpenChamber shows them. Text is chunked at push
        // time and bounded per card by `estimate_split_index`, so everything in
        // [start..end) renders in full — no preview truncation, no separate
        // plain-text message.
        let mut pending = String::new();
        let mut saw_content = false;
        for item in self.timeline.iter().take(end).skip(start) {
            match item {
                TimelineItem::Reasoning(r) => {
                    if !pending.is_empty() {
                        builder = builder.with_text(&pending);
                        pending.clear();
                    }
                    builder = builder.with_reasoning(r);
                    saw_content = true;
                }
                TimelineItem::Text(t) => {
                    pending.push_str(t);
                    saw_content = true;
                }
                TimelineItem::Tool(call_id) => {
                    if !pending.is_empty() {
                        builder = builder.with_text(&pending);
                        pending.clear();
                    }
                    if let Some(panel) = self.tools.get(call_id) {
                        builder = builder.with_tool(panel.clone());
                    }
                }
            }
        }
        if !pending.is_empty() {
            builder = builder.with_text(&pending);
        }

        if include_tail {
            // Inline permission requests: the whole turn lives on one card, so a
            // pending permission renders as a section with its buttons right here.
            for p in &self.pending_permissions {
                builder = builder.with_text(&format!("🔐 **权限请求**\n{}", p.body));
                for btn in crate::feishu::card::permission_buttons(
                    &p.session_id,
                    &p.request_id,
                    &p.body,
                    &p.directory,
                ) {
                    builder = builder.with_element(btn);
                }
            }
            // Inline question requests (with their current partial answers).
            for q in &self.pending_questions {
                for el in crate::feishu::card::question_elements(
                    &q.request_id,
                    &q.session_id,
                    &q.questions,
                    &q.directory,
                    &q.answers,
                ) {
                    builder = builder.with_element(el);
                }
            }

            if let Some(ref err) = self.error {
                builder = builder.with_text(&format!("\n**错误**: {}", err));
            }

            // Streaming/Loading with nothing rendered yet: keep the card alive.
            if !saw_content
                && self.error.is_none()
                && (self.card_state == CardState::Streaming || self.card_state == CardState::Loading)
            {
                builder = builder.with_text("⏳ ...");
            }

            // Error card: offer a retry that re-submits the original prompt on
            // the same card, so the user doesn't have to retype it.
            if self.card_state == CardState::Error
                && let Some(sid) = &self.session_id
            {
                builder = builder.with_error_buttons(vec![crate::feishu::card::CardActionButton {
                    text: "🔄 重试".to_string(),
                    kind: "primary",
                    value: serde_json::json!({ "action": "retry", "session_id": sid }),
                }]);
            }

            // Card footer: working directory · model · context usage. Gives the
            // user at-a-glance context for what this turn ran on.
            let mut footer_parts: Vec<String> = Vec::new();
            if let Some(dir) = &self.directory {
                footer_parts.push(format!("📁 {}", crate::feishu::ws::strip_mention_tokens(dir)));
            }
            if let Some(model) = &self.model_id {
                let label = match self.provider_id.as_deref() {
                    Some(p) => format!("{}/{}", p, model),
                    None => model.clone(),
                };
                footer_parts.push(format!("🤖 {}", label));
            }
            if let Some(ratio) = self.context_ratio {
                footer_parts.push(format!("📊 上下文 {:.0}%", ratio * 100.0));
            }
            if !footer_parts.is_empty() {
                builder = builder.with_footer(&footer_parts.join(" · "));
            } else if let Some((input, output)) = self.token_cost {
                builder = builder.with_footer(&format!("Tokens: {}入 {}出", input, output));
            }
        }

        builder.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::card::MAX_CARD_TEXT_CHARS;

    #[test]
    fn card_footer_shows_directory_model_and_context() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.directory = Some("/root/workspace/dev/cola".into());
        acc.provider_id = Some("opencode-go".into());
        acc.model_id = Some("deepseek-v4-flash".into());
        acc.context_ratio = Some(0.36);
        acc.push_text("结果");

        let card = acc.build_card();
        let text = card.to_string();
        assert!(
            text.contains("📁 /root/workspace/dev/cola"),
            "dir missing: {}",
            text
        );
        assert!(
            text.contains("🤖 opencode-go/deepseek-v4-flash"),
            "model missing: {}",
            text
        );
        assert!(text.contains("📊 上下文 36%"), "ratio missing: {}", text);
    }

    /// While the card fits the component budget, successive flushes must
    /// ACCUMULATE — each build re-renders from the same start, so earlier
    /// reasoning/tools/text stay on the card (not just the latest delta).
    #[test]
    fn successive_builds_accumulate_without_splitting() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;

        acc.push_reasoning("先想想。");
        acc.push_text("第一条文本。");
        let (card, full) = acc.build_card_with_split();
        assert!(!full);
        assert!(card.to_string().contains("第一条文本"));
        assert!(card.to_string().contains("推理"));

        // More content arrives on the next poll.
        acc.push_tool(
            "call_1",
            ToolPanel {
                name: "bash".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("ls")),
                output: Some("src".into()),
            },
        );
        acc.push_text("第二条文本。");
        let (card2, full2) = acc.build_card_with_split();
        assert!(!full2);
        let text2 = card2.to_string();
        assert!(
            text2.contains("第一条文本") && text2.contains("第二条文本"),
            "content must accumulate, not only show the delta: {}",
            text2
        );
        assert!(text2.contains("推理"), "reasoning must persist");
        assert!(text2.contains("bash"), "tool must persist");
    }

    /// When a turn has too many parts to fit one card (Feishu component limit),
    /// the card splits: the first is finalized with a "to be continued" marker
    /// and a continuation card renders the remaining timeline.
    #[test]
    fn card_splits_into_continuation_when_over_component_limit() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        for i in 0..50 {
            acc.push_tool(
                &format!("call_{}", i),
                ToolPanel {
                    name: format!("tool{}", i),
                    status: "completed".into(),
                    input: None,
                    output: None,
                },
            );
        }
        acc.push_text("最后的结论。");

        let (card, full) = acc.build_card_with_split();
        assert!(full, "50 tools must exceed the component budget");
        let card_text = card.to_string();
        assert!(
            card_text.contains("部分完成，继续中"),
            "split card must show a partial header: {}",
            card_text
        );
        assert!(acc.render_from > 0, "render_from must advance past the split");

        // The continuation holds the remaining content, nothing duplicated.
        let (rest, full2) = acc.build_card_with_split();
        assert!(!full2, "rest should fit: {:?}", rest);
        assert!(
            rest.to_string().contains("最后的结论。"),
            "conclusion must appear on a continuation"
        );
    }

    /// Feishu rejects cards whose serialized body approaches 30KB (documented
    /// limit; a real 44KB card fails with 230099 / "create universal card
    /// fail"). A turn heavy on tool panels must split on the JSON-size budget —
    /// not just the component count — so every card stays under the cap.
    #[test]
    fn card_splits_on_json_size_budget() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        // Each full-size tool panel serializes to ~3.4KB; 12 of them (~41KB of
        // body) exceed MAX_CARD_JSON_CHARS but not MAX_CARD_COMPONENTS, so only
        // the size budget can catch the overflow.
        let big_output = "o".repeat(crate::feishu::card::TOOL_OUTPUT_MAX_CHARS);
        for i in 0..12 {
            acc.push_tool(
                &format!("call_{}", i),
                ToolPanel {
                    name: format!("tool{}", i),
                    status: "completed".into(),
                    input: Some(serde_json::json!("in")),
                    output: Some(big_output.clone()),
                },
            );
        }
        acc.push_text("最后的结论。");

        let mut cards = Vec::new();
        let mut full = true;
        while full {
            let (card, f) = acc.build_card_with_split();
            full = f;
            cards.push(card);
        }
        assert!(cards.len() >= 2, "12 big panels must split: {}", cards.len());
        for (i, card) in cards.iter().enumerate() {
            let bytes = serde_json::to_string(card).unwrap().len();
            assert!(
                bytes < crate::feishu::card::FEISHU_CARD_LIMIT_BYTES,
                "card {} exceeds Feishu's 30KB card limit: {} bytes",
                i,
                bytes
            );
        }
        let last = cards.last().unwrap();
        assert!(
            last.to_string().contains("最后的结论。"),
            "conclusion must survive on a continuation: {}",
            last
        );
    }

    /// The size budget is measured in UTF-8 BYTES, not characters: a CJK panel
    /// is ~3 bytes per char, so a card that "fits" by char count can still blow
    /// past Feishu's byte limit. A card heavy on Chinese tool output must split
    /// on the byte budget.
    #[test]
    fn card_splits_on_byte_budget_for_multibyte_content() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        // 7 panels of Chinese output: the CHAR estimate is 7 * ~3.4K = ~23.8K
        // chars (fits under MAX_CARD_JSON_CHARS), but the BYTE estimate is
        // 7 * ~9.4K = ~66K bytes (way over) — a char-counted estimate would
        // wrongly ship one ~66KB card and Feishu would reject it.
        let cjk_output = "中".repeat(crate::feishu::card::TOOL_OUTPUT_MAX_CHARS);
        for i in 0..7 {
            acc.push_tool(
                &format!("call_{}", i),
                ToolPanel {
                    name: format!("tool{}", i),
                    status: "completed".into(),
                    input: Some(serde_json::json!("入参")),
                    output: Some(cjk_output.clone()),
                },
            );
        }
        acc.push_text("最后的结论。");

        let mut cards = Vec::new();
        let mut full = true;
        while full {
            let (card, f) = acc.build_card_with_split();
            full = f;
            cards.push(card);
        }
        assert!(
            cards.len() >= 2,
            "CJK panels must split on bytes: {}",
            cards.len()
        );
        for (i, card) in cards.iter().enumerate() {
            let bytes = serde_json::to_string(card).unwrap().len();
            assert!(
                bytes < crate::feishu::card::FEISHU_CARD_LIMIT_BYTES,
                "card {} exceeds Feishu's 30KB card limit: {} bytes",
                i,
                bytes
            );
        }
    }

    /// A long answer flows across continuation cards instead of being truncated
    /// to a preview (there is no plain-text fallback anymore). Text is chunked
    /// at push time so the split lands between chunks, and every chunk renders.
    #[test]
    fn long_text_splits_across_continuation_cards() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        let long = "很长的回答。".repeat(2000); // 12_000 chars > one card budget
        acc.push_text(&long);

        // First card: text-budget split marks it "to be continued".
        let (card, full) = acc.build_card_with_split();
        assert!(full, "long text must split past one card's budget");
        assert!(
            card.to_string().contains("部分完成，继续中"),
            "split card must show the partial header: {}",
            card
        );
        assert!(acc.render_from > 0, "render_from must advance");

        // The continuation carries the tail; between them all text is present.
        let (rest, full2) = acc.build_card_with_split();
        assert!(!full2, "tail should fit: {:?}", rest);
        let card_text = card.to_string();
        let rest_text = rest.to_string();
        let joined_len =
            card_text.chars().filter(|c| *c != '"').count() + rest_text.chars().filter(|c| *c != '"').count();
        assert!(
            joined_len >= long.chars().count(),
            "all text must survive the split (card={} rest={} long={})",
            card_text.len(),
            rest_text.len(),
            long.chars().count()
        );
        assert!(
            rest_text.contains("很长的回答。"),
            "tail text must appear on the continuation: {}",
            rest_text
        );
    }

    /// `push_text` chunks one large blob into bounded timeline items so a single
    /// item never exceeds the per-card text budget.
    #[test]
    fn push_text_chunks_long_blobs() {
        let mut acc = StreamAccumulator::new("test");
        let long = "字".repeat(MAX_CARD_TEXT_CHARS * 2 + 100);
        acc.push_text(&long);

        let text_items: Vec<&String> = acc
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(
            text_items.len() >= 3,
            "a long blob must be split into multiple text items, got {}",
            text_items.len()
        );
        for t in &text_items {
            assert!(
                t.chars().count() <= MAX_CARD_TEXT_CHARS,
                "text item over the per-card budget: {} chars",
                t.chars().count()
            );
        }
        let joined: String = text_items.iter().map(|t| t.as_str()).collect();
        assert_eq!(joined, long, "all text must be preserved across chunks");
    }

    /// Text and tool calls must render interleaved (chronological), not all
    /// tools on top and all text below — matching OpenChamber's presentation.
    #[test]
    fn card_interleaves_text_and_tools_in_timeline_order() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.push_text("先看一下目录。");
        acc.push_tool(
            "call_1",
            ToolPanel {
                name: "bash".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("ls")),
                output: Some("src".into()),
            },
        );
        acc.push_text("再看一下配置。");
        acc.push_tool(
            "call_2",
            ToolPanel {
                name: "read".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("cola.toml")),
                output: Some("[bridge]".into()),
            },
        );
        acc.push_text("结论是...");

        let card = acc.build_card();
        let elements = card["body"]["elements"].as_array().unwrap();
        // Element order: text, tool, text, tool, text.
        let kinds: Vec<&str> = elements
            .iter()
            .map(|e| {
                if e["tag"] == "collapsible_panel" {
                    "tool"
                } else {
                    "text"
                }
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["text", "tool", "text", "tool", "text"],
            "text/tools must interleave: {:?}",
            elements
        );
        assert!(elements[0]["content"].as_str().unwrap().contains("先看一下目录"));
        assert!(
            elements[1]["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("bash")
        );
        assert!(elements[2]["content"].as_str().unwrap().contains("再看一下配置"));
        assert!(
            elements[3]["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("read")
        );
        assert!(elements[4]["content"].as_str().unwrap().contains("结论是"));
    }

    /// Re-rendering the same tool (running → completed) must NOT duplicate its
    /// timeline marker.
    #[test]
    fn tool_state_update_does_not_duplicate_timeline_marker() {
        let mut acc = StreamAccumulator::new("test");
        acc.push_tool(
            "call_1",
            ToolPanel {
                name: "bash".into(),
                status: "running".into(),
                input: Some(serde_json::json!("ls")),
                output: None,
            },
        );
        acc.push_tool(
            "call_1",
            ToolPanel {
                name: "bash".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("ls")),
                output: Some("src".into()),
            },
        );
        assert_eq!(acc.tools.len(), 1);
        assert_eq!(acc.timeline.len(), 1, "one tool marker, no duplicates");
    }
}
