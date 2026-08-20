use crate::feishu::card::{CardBuilder, CardState, ToolPanel};
use crate::opencode::client::OpenCodeEvent;
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

    /// Apply a single v1 streaming event, returning true if the card should be
    /// flushed.
    ///
    /// NOTE: the global `/event` stream only delivers v2 durable events, so this
    /// v1 state machine is not wired to production (rendering goes through
    /// `render_new_turn_parts`). Kept — and tested — as the canonical streaming
    /// model, reusable if cola moves to prompt_async + per-session SSE.
    #[allow(dead_code)]
    pub fn apply(&mut self, event: &OpenCodeEvent) -> bool {
        match event {
            OpenCodeEvent::StepStarted { data, .. } => {
                self.card_state = CardState::Streaming;
                if let Some(agent) = &data.agent {
                    self.title = agent.clone();
                }
                true
            }
            OpenCodeEvent::StepEnded { data, .. } => {
                self.card_state = CardState::Done;
                if let Some(cost) = data.cost {
                    self.token_cost = Some((cost as i64, 0));
                }
                if let Some(tokens) = &data.tokens {
                    let input = tokens.input.unwrap_or(0);
                    let output = tokens.output.unwrap_or(0);
                    self.token_cost = Some((input, output));
                }
                true
            }
            OpenCodeEvent::StepFailed { data, .. } => {
                self.card_state = CardState::Done;
                if let Some(err) = &data.error {
                    self.error = Some(err.message.clone().unwrap_or_else(|| "unknown".into()));
                }
                true
            }
            OpenCodeEvent::TextDelta { data, .. } => {
                if let Some(delta) = &data.delta {
                    self.push_text(delta);
                }
                false // don't flush on every delta, throttle handles it
            }
            OpenCodeEvent::TextEnded { data, .. } => {
                if let Some(t) = &data.text {
                    self.push_text(t);
                }
                true
            }
            OpenCodeEvent::ReasoningStarted { .. } => {
                self.card_state = CardState::Reasoning;
                true
            }
            OpenCodeEvent::ReasoningDelta { data, .. } => {
                if let Some(delta) = &data.delta {
                    self.push_reasoning(delta);
                }
                false
            }
            OpenCodeEvent::ReasoningEnded { data, .. } => {
                // ReasoningDelta already accumulates; only append if the ended
                // text wasn't already delivered as deltas.
                if let Some(t) = &data.text
                    && !self.reasoning.ends_with(t)
                {
                    self.push_reasoning(t);
                }
                self.card_state = CardState::Reasoning;
                true
            }
            OpenCodeEvent::ToolCalled { data, .. } => {
                if let Some(call_id) = &data.call_id {
                    let panel = ToolPanel {
                        name: data.tool.as_deref().unwrap_or("unknown").to_string(),
                        status: "running".to_string(),
                        input: data.input.clone(),
                        output: None,
                    };
                    self.push_tool(call_id, panel);
                }
                true
            }
            OpenCodeEvent::ToolSuccess { data, .. } => {
                if let Some(call_id) = &data.call_id
                    && let Some(panel) = self.tools.get_mut(call_id)
                {
                    panel.status = "completed".to_string();
                }
                true
            }
            OpenCodeEvent::ToolFailed { data, .. } => {
                if let Some(call_id) = &data.call_id
                    && let Some(panel) = self.tools.get_mut(call_id)
                {
                    panel.status = "failed".to_string();
                }
                true
            }
            OpenCodeEvent::ToolProgress { data, .. } => {
                if let Some(call_id) = &data.call_id
                    && let Some(panel) = self.tools.get_mut(call_id)
                {
                    panel.status = "running".to_string();
                }
                true
            }
            OpenCodeEvent::ShellStarted { data, .. } => {
                if let Some(call_id) = &data.call_id {
                    let panel = ToolPanel {
                        name: format!("shell: {}", data.command.as_deref().unwrap_or("")),
                        status: "running".to_string(),
                        input: None,
                        output: None,
                    };
                    self.push_tool(call_id, panel);
                }
                true
            }
            OpenCodeEvent::ShellEnded { data, .. } => {
                if let Some(call_id) = &data.call_id
                    && let Some(panel) = self.tools.get_mut(call_id)
                {
                    panel.status = "completed".to_string();
                    if let Some(output) = &data.output {
                        panel.output = Some(output.clone());
                    }
                }
                true
            }
            _ => false,
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
    fn estimate_split_index(&self, start: usize) -> usize {
        let mut comps = 0usize;
        let mut size = 0usize;
        let mut card_text = 0usize;
        for (i, item) in self.timeline.iter().enumerate().skip(start) {
            let (c, s, t) = match item {
                TimelineItem::Reasoning(r) => (4, 200 + r.chars().count().min(800), 0),
                TimelineItem::Tool(call_id) => {
                    let panel_size = self.tools.get(call_id).map(|p| {
                        let input = p
                            .input
                            .as_ref()
                            .map(|x| x.to_string().chars().count().min(400))
                            .unwrap_or(0);
                        let output = p
                            .output
                            .as_deref()
                            .map(|x| x.chars().count().min(800))
                            .unwrap_or(0);
                        200 + input + output
                    });
                    (4, panel_size.unwrap_or(200), 0)
                }
                // Text is chunked to ≤ MAX_CARD_TEXT_CHARS per item, so a
                // single item never exceeds the per-card budget; the budget
                // accumulates across items and splits at the boundary. The
                // component estimate mirrors `with_text`, which splits each
                // element at MAX_ELEMENT_TEXT_CHARS.
                TimelineItem::Text(t) => {
                    let chars = t.chars().count();
                    let elements = chars.div_ceil(crate::feishu::card::MAX_ELEMENT_TEXT_CHARS);
                    (elements, chars, chars)
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
    use crate::opencode::client::{
        ErrorMessage, OpenCodeEvent, ReasoningEndedData, ReasoningStartedData, StepEndedData, StepFailedData,
        StepStartedData, TextDeltaData, TextEndedData, TokenCount, ToolCalledData, ToolFailedData,
        ToolSuccessData,
    };

    fn make_step_started(session_id: &str, agent: &str) -> OpenCodeEvent {
        OpenCodeEvent::StepStarted {
            id: "evt_1".into(),
            data: StepStartedData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                agent: Some(agent.into()),
                model: None,
                snapshot: None,
                timestamp: Some(1000),
            },
        }
    }

    fn make_text_delta(session_id: &str, delta: &str) -> OpenCodeEvent {
        OpenCodeEvent::TextDelta {
            id: "evt_2".into(),
            data: TextDeltaData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                text_id: Some("txt_1".into()),
                delta: Some(delta.into()),
                timestamp: Some(2000),
            },
        }
    }

    fn make_text_ended(session_id: &str, text: &str) -> OpenCodeEvent {
        OpenCodeEvent::TextEnded {
            id: "evt_3".into(),
            data: TextEndedData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                text_id: Some("txt_1".into()),
                text: Some(text.into()),
                timestamp: Some(3000),
            },
        }
    }

    fn make_reasoning_started(session_id: &str) -> OpenCodeEvent {
        OpenCodeEvent::ReasoningStarted {
            id: "evt_r1".into(),
            data: ReasoningStartedData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                reasoning_id: Some("rsn_1".into()),
                timestamp: Some(1500),
            },
        }
    }

    fn make_reasoning_ended(session_id: &str, text: &str) -> OpenCodeEvent {
        OpenCodeEvent::ReasoningEnded {
            id: "evt_r2".into(),
            data: ReasoningEndedData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                reasoning_id: Some("rsn_1".into()),
                text: Some(text.into()),
                timestamp: Some(2500),
            },
        }
    }

    fn make_step_ended(session_id: &str, input: i64, output: i64) -> OpenCodeEvent {
        OpenCodeEvent::StepEnded {
            id: "evt_4".into(),
            data: StepEndedData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                finish: Some("stop".into()),
                cost: Some(0.01),
                tokens: Some(TokenCount {
                    input: Some(input),
                    output: Some(output),
                    reasoning: None,
                    cache: None,
                }),
                timestamp: Some(4000),
            },
        }
    }

    fn make_step_failed(session_id: &str, msg: &str) -> OpenCodeEvent {
        OpenCodeEvent::StepFailed {
            id: "evt_5".into(),
            data: StepFailedData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                error: Some(ErrorMessage {
                    error_type: Some("APIError".into()),
                    message: Some(msg.into()),
                }),
                timestamp: Some(5000),
            },
        }
    }

    fn make_tool_called(session_id: &str, call_id: &str, name: &str) -> OpenCodeEvent {
        OpenCodeEvent::ToolCalled {
            id: "evt_t1".into(),
            data: ToolCalledData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                call_id: Some(call_id.into()),
                tool: Some(name.into()),
                input: Some(serde_json::json!({"cmd": "ls"})),
                timestamp: Some(3000),
            },
        }
    }

    fn make_tool_success(session_id: &str, call_id: &str) -> OpenCodeEvent {
        OpenCodeEvent::ToolSuccess {
            id: "evt_t2".into(),
            data: ToolSuccessData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                call_id: Some(call_id.into()),
                content: None,
                result: Some(serde_json::json!({"status": "ok"})),
                timestamp: Some(3500),
            },
        }
    }

    fn make_tool_failed(session_id: &str, call_id: &str) -> OpenCodeEvent {
        OpenCodeEvent::ToolFailed {
            id: "evt_t3".into(),
            data: ToolFailedData {
                session_id: Some(session_id.into()),
                assistant_message_id: Some("msg_1".into()),
                call_id: Some(call_id.into()),
                error: Some(ErrorMessage {
                    error_type: Some("ExitError".into()),
                    message: Some("command failed".into()),
                }),
                timestamp: Some(3500),
            },
        }
    }

    #[test]
    fn flow_step_with_text() {
        let mut acc = StreamAccumulator::new("Test prompt");
        assert!(acc.apply(&make_step_started("ses_1", "primary")));
        assert_eq!(acc.card_state, CardState::Streaming);

        assert!(!acc.apply(&make_text_delta("ses_1", "Hello")));
        assert!(!acc.apply(&make_text_delta("ses_1", " world")));
        assert!(acc.apply(&make_text_ended("ses_1", "Hello world")));
        assert_eq!(acc.text, "Hello worldHello world");

        assert!(acc.apply(&make_step_ended("ses_1", 100, 50)));
        assert_eq!(acc.card_state, CardState::Done);
        assert_eq!(acc.token_cost, Some((100, 50)));

        let card = acc.build_card();
        assert!(card.to_string().contains("Hello"));
        assert!(card.to_string().contains("100"));
    }

    #[test]
    fn flow_with_reasoning() {
        let mut acc = StreamAccumulator::new("Test prompt");
        assert!(acc.apply(&make_reasoning_started("ses_1")));
        assert_eq!(acc.card_state, CardState::Reasoning);

        assert!(acc.apply(&make_reasoning_ended("ses_1", "Let me think...")));
        assert_eq!(acc.reasoning, "Let me think...");

        let card = acc.build_card();
        assert!(card.to_string().contains("Let me think"));
    }

    #[test]
    fn flow_step_failed_puts_error_inline() {
        let mut acc = StreamAccumulator::new("Test prompt");
        acc.apply(&make_step_started("ses_1", "primary"));
        acc.apply(&make_text_ended("ses_1", "Partial output"));

        assert!(acc.apply(&make_step_failed("ses_1", "API rate limit")));
        assert_eq!(acc.card_state, CardState::Done);
        assert!(acc.error.as_deref().unwrap().contains("API rate limit"));

        let card = acc.build_card();
        let card_str = card.to_string();
        assert!(card_str.contains("Partial output"));
        assert!(card_str.contains("API rate limit"));
    }

    #[test]
    fn tool_lifecycle_tracks_status() {
        let mut acc = StreamAccumulator::new("Test prompt");
        acc.apply(&make_step_started("ses_1", "primary"));

        assert!(acc.apply(&make_tool_called("ses_1", "call_1", "bash")));
        let tool = &acc.tools["call_1"];
        assert_eq!(tool.status, "running");
        assert_eq!(tool.name, "bash");

        assert!(acc.apply(&make_tool_success("ses_1", "call_1")));
        assert_eq!(acc.tools["call_1"].status, "completed");

        let card = acc.build_card();
        assert!(card.to_string().contains("bash"));
    }

    #[test]
    fn tool_failed_status() {
        let mut acc = StreamAccumulator::new("Test prompt");
        acc.apply(&make_step_started("ses_1", "primary"));
        acc.apply(&make_tool_called("ses_1", "call_2", "write"));
        acc.apply(&make_tool_failed("ses_1", "call_2"));

        assert_eq!(acc.tools["call_2"].status, "failed");

        let card = acc.build_card();
        assert!(card.to_string().contains("write"));
    }

    #[test]
    fn tools_render_in_call_order() {
        let mut acc = StreamAccumulator::new("Test prompt");
        acc.apply(&make_step_started("ses_1", "primary"));
        acc.apply(&make_tool_called("ses_1", "call_1", "read"));
        acc.apply(&make_tool_success("ses_1", "call_1"));
        acc.apply(&make_tool_called("ses_1", "call_2", "bash"));
        acc.apply(&make_tool_success("ses_1", "call_2"));
        acc.apply(&make_tool_called("ses_1", "call_3", "question"));
        acc.apply(&make_tool_success("ses_1", "call_3"));

        // Tools must appear in the card in the order they were called, not in
        // hash order.
        let card = acc.build_card().to_string();
        let read_pos = card.find("read").unwrap();
        let bash_pos = card.find("bash").unwrap();
        let question_pos = card.find("question").unwrap();
        assert!(read_pos < bash_pos, "read should render before bash");
        assert!(bash_pos < question_pos, "bash should render before question");
    }

    #[test]
    fn ignores_events_without_session_id() {
        let mut acc = StreamAccumulator::new("Test prompt");
        let event = OpenCodeEvent::StepStarted {
            id: "evt_1".into(),
            data: StepStartedData {
                session_id: None,
                assistant_message_id: None,
                agent: None,
                model: None,
                snapshot: None,
                timestamp: None,
            },
        };
        let _flushed = acc.apply(&event);
        assert_eq!(acc.card_state, CardState::Streaming);
    }

    /// Reasoning must also interleave chronologically with text and tools —
    /// not a single panel forced to the top.
    #[test]
    fn reasoning_interleaves_with_text_and_tools() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.push_reasoning("先想想。");
        acc.push_text("开始动手。");
        acc.push_tool(
            "call_1",
            ToolPanel {
                name: "bash".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("ls")),
                output: Some("src".into()),
            },
        );
        acc.push_reasoning("好像不太对，换个思路。");
        acc.push_text("结论是...");

        let card = acc.build_card();
        let elements = card["body"]["elements"].as_array().unwrap();
        let kinds: Vec<&str> = elements
            .iter()
            .map(|e| {
                if e["tag"] == "collapsible_panel" {
                    if e["header"]["title"]["content"]
                        .as_str()
                        .unwrap_or("")
                        .contains("推理")
                    {
                        "reasoning"
                    } else {
                        "tool"
                    }
                } else {
                    "text"
                }
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["reasoning", "text", "tool", "reasoning", "text"],
            "reasoning must interleave: {:?}",
            elements
        );
        // Both reasoning chunks are folded panels.
        assert!(
            elements[3]["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("推理")
        );
    }

    /// The completed card's footer shows directory · model · context usage.
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
