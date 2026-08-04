use crate::feishu::card::{CardBuilder, CardState, ToolPanel};
use crate::opencode::client::OpenCodeEvent;
use std::collections::HashMap;

/// Accumulates streaming state for one session.
#[derive(Default)]
pub struct StreamAccumulator {
    pub card_state: CardState,
    pub text: String,
    pub reasoning: String,
    pub tools: HashMap<String, ToolPanel>,
    pub title: String,
    /// The user question this accumulator is answering (shown in card header).
    pub question: String,
    pub token_cost: Option<(i64, i64)>,
    pub error: Option<String>,
    pub reply_to_message_id: Option<String>,
    pub last_flush_at: Option<std::time::Instant>,
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
            question: String::new(),
            reply_to_message_id: None,
            last_flush_at: None,
            rendered_parts: std::collections::HashSet::new(),
            rendered_tool_states: std::collections::HashMap::new(),
            submit_epoch_ms: None,
            ..Default::default()
        }
    }

    /// Apply a single event, returning true if the card should be flushed.
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
                    self.text.push_str(delta);
                }
                false // don't flush on every delta, throttle handles it
            }
            OpenCodeEvent::TextEnded { data, .. } => {
                if let Some(t) = &data.text {
                    self.text.push_str(t);
                }
                true
            }
            OpenCodeEvent::ReasoningStarted { .. } => {
                self.card_state = CardState::Reasoning;
                true
            }
            OpenCodeEvent::ReasoningDelta { data, .. } => {
                if let Some(delta) = &data.delta {
                    self.reasoning.push_str(delta);
                }
                false
            }
            OpenCodeEvent::ReasoningEnded { data, .. } => {
                // ReasoningDelta already accumulates; only append if the ended
                // text wasn't already delivered as deltas.
                if let Some(t) = &data.text
                    && !self.reasoning.ends_with(t)
                {
                    self.reasoning.push_str(t);
                }
                self.card_state = CardState::Reasoning;
                true
            }
            OpenCodeEvent::ToolCalled { data, .. } => {
                if let Some(call_id) = &data.call_id {
                    let panel = ToolPanel {
                        name: data.tool.as_deref().unwrap_or("unknown").to_string(),
                        status: "running".to_string(),
                        input: data
                            .input
                            .as_ref()
                            .map(|i| serde_json::to_string_pretty(i).unwrap_or_default()),
                        output: None,
                    };
                    self.tools.insert(call_id.clone(), panel);
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
                    self.tools.insert(call_id.clone(), panel);
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

    pub fn build_card(&self) -> serde_json::Value {
        let mut builder = CardBuilder::new(&self.title).with_state(self.card_state.clone());

        // Show which question this reply is answering (helps with queued prompts)
        if !self.question.is_empty() {
            builder = builder.with_question(&self.question);
        }

        if !self.reasoning.is_empty() {
            builder = builder.with_reasoning(&self.reasoning);
        }

        for tool in self.tools.values() {
            builder = builder.with_tool(tool.clone());
        }

        let mut display_text = self.text.clone();
        if let Some(ref err) = self.error {
            if !display_text.is_empty() {
                display_text.push('\n');
            }
            display_text.push_str(&format!("\n**错误**: {}", err));
        }

        if !display_text.is_empty() {
            builder = builder.with_text(&display_text);
        }

        if let Some((input, output)) = self.token_cost {
            builder = builder.with_footer(&format!("Tokens: {}入 {}出", input, output));
        }

        builder.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
