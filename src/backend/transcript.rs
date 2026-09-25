//! The Session Transcript: the neutral read of one Session (spec #332).
//!
//! The Bridge and the Platform consume only these views; backend protocol
//! field names stay inside the adapter's decoders (ADR-0053). A message's
//! identity and its server time travel together ([`TurnAnchor`]), so no caller
//! reassembles an anchor from backend fields.
//!
//! A message carries no error field: the wire read never surfaced one, and a
//! failed prompt's error is a prompt-response fact the adapters keep surfacing
//! where they always have — it is not transcript content.

use serde_json::Value;

/// One Session's normalized read: every message the backend reports, decoded
/// into typed views, in the order the backend returned them.
#[derive(Debug, Clone, Default)]
pub struct SessionTranscript {
    pub messages: Vec<TranscriptMessage>,
}

impl SessionTranscript {
    pub fn new(messages: Vec<TranscriptMessage>) -> Self {
        Self { messages }
    }

    /// The newest user message by server time, if any — the message a Turn's
    /// anchor comes from. A user message without a server time cannot be
    /// ordered and is ignored.
    pub fn newest_user(&self) -> Option<&TranscriptMessage> {
        self.messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .filter(|message| message.time.is_some())
            .max_by_key(|message| message.time.map(|time| time.created))
    }

    /// The Turn anchored on the user message `anchor` names: the assistant
    /// messages that belong to it, in transcript order, plus whether it has
    /// finished.
    ///
    /// Membership follows the streaming rule: a message still in flight (no
    /// completion stamp) always belongs — the previous run may still be
    /// streaming when this Turn's user message lands — while a completed
    /// message belongs only when it was created within the Turn or was still
    /// being produced as the Turn began. A message with no server time at all
    /// cannot be placed and stays out.
    ///
    /// The Turn is complete when an assistant message that started within it
    /// carries a terminal step-finish reason (every reason except a pause to
    /// run tools).
    pub fn turn_for_user(&self, anchor: &TurnAnchor) -> TurnView<'_> {
        let mut messages = Vec::new();
        let mut complete = false;
        for message in &self.messages {
            if message.role != MessageRole::Assistant {
                continue;
            }
            let Some(time) = &message.time else { continue };
            if !belongs_to_turn(time, anchor.created_ms) {
                continue;
            }
            if time.created >= anchor.created_ms && message.finishes_turn() {
                complete = true;
            }
            messages.push(message);
        }
        TurnView {
            anchor: anchor.clone(),
            messages,
            complete,
        }
    }

    /// The recent-conversation tail: the last (at most four) text-bearing
    /// user/assistant messages, newest last. A message is text-bearing when it
    /// carries at least one non-empty text part; reasoning/tool/step parts are
    /// inner monologue, not conversation, and are excluded. Messages without a
    /// created time cannot be ordered and are dropped.
    pub fn transcript_tail(&self) -> Vec<TailEntry> {
        const TAIL_LIMIT: usize = 4;
        let mut out: Vec<TailEntry> = self
            .messages
            .iter()
            .filter(|message| matches!(message.role, MessageRole::User | MessageRole::Assistant))
            .filter_map(|message| {
                let text = message.text();
                if text.trim().is_empty() {
                    return None;
                }
                Some(TailEntry {
                    role: message.role.clone(),
                    created_ms: message.time?.created,
                    text,
                })
            })
            .collect();
        // Oldest → newest, then keep only the newest `TAIL_LIMIT` so the tail
        // is the last four messages, newest last.
        out.sort_by_key(|entry| entry.created_ms);
        let start = out.len().saturating_sub(TAIL_LIMIT);
        out.drain(..start);
        out
    }
}

/// Whether an assistant message belongs to the Turn anchored at `anchor_ms`.
/// Still in flight (no completion stamp): always. Completed: only when it was
/// created within the Turn or was still being produced as the Turn began.
fn belongs_to_turn(time: &MessageTime, anchor_ms: i64) -> bool {
    match time.completed {
        None => true,
        Some(completed) => time.created >= anchor_ms || completed >= anchor_ms,
    }
}

/// One message of a [`SessionTranscript`]: identity, role, server time, model
/// identity and token usage are typed; the body is typed parts.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptMessage {
    pub id: MessageId,
    pub role: MessageRole,
    /// Server time the message was created and, for an assistant message,
    /// finished producing. `None` when the payload carried no time at all.
    pub time: Option<MessageTime>,
    /// The model that produced an assistant message, when the payload names
    /// one.
    pub model: Option<ModelIdentity>,
    /// Token usage the message reports, when it reports any.
    pub tokens: Option<TokenUsage>,
    pub parts: Vec<Part>,
}

impl TranscriptMessage {
    /// This message as a Turn anchor: its identity together with its server
    /// time, one fact. `None` when the payload carried no server time.
    pub fn anchor(&self) -> Option<TurnAnchor> {
        self.time.map(|time| TurnAnchor {
            message_id: self.id.clone(),
            created_ms: time.created,
        })
    }

    /// The message's conversational text: its text parts verbatim,
    /// newline-joined. Empty when it carries no text part.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether one of this message's parts finished its Turn (a terminal
    /// `step-finish`).
    fn finishes_turn(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, Part::StepFinish(finish) if finish.reason.is_terminal()))
    }
}

/// A message's opaque server identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(String);

impl MessageId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for MessageId {
    fn from(id: &str) -> Self {
        Self(id.to_string())
    }
}

impl From<String> for MessageId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// What a message is: the two conversation roles cola renders, plus tolerant
/// arms for anything else a newer backend may report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    /// A role this build does not know, kept verbatim.
    Other(String),
    /// The payload carried no role.
    Unknown,
}

/// A message's server time (epoch ms). `completed` is absent while the
/// message is still being produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageTime {
    pub created: i64,
    pub completed: Option<i64>,
}

/// The model that produced a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelIdentity {
    pub provider_id: String,
    pub model_id: String,
    pub variant: Option<String>,
}

/// Token usage a message reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: i64,
    pub output: i64,
    pub total: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl TokenUsage {
    /// The context the model actually consumed: `total` when the server
    /// reports it, else the cached prefix plus the fresh input. (`input` alone
    /// is only the per-message delta — mostly cache reads — so it understates
    /// context a lot.)
    pub fn context_used(&self) -> i64 {
        let fallback = self.input + self.cache_read;
        if self.total > 0 { self.total } else { fallback }
    }
}

/// A Turn's anchor: the identity of the user message it answers together with
/// that message's server time. The two travel as one fact, so no caller can
/// reassemble the anchor from backend fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnAnchor {
    pub message_id: MessageId,
    pub created_ms: i64,
}

/// One Turn as read from a transcript: its anchor, the assistant messages
/// that belong to it (in-flight ones included), and whether it has finished.
#[derive(Debug)]
pub struct TurnView<'a> {
    /// The anchor the view was read for, echoed so the view is self-describing;
    /// production callers already hold it.
    #[allow(dead_code)] // exercised by the projection tests only
    pub anchor: TurnAnchor,
    pub messages: Vec<&'a TranscriptMessage>,
    pub complete: bool,
}

/// One recent-conversation tail entry: a text-bearing user/assistant message's
/// role, created time and verbatim text.
#[derive(Debug, Clone, PartialEq)]
pub struct TailEntry {
    pub role: MessageRole,
    pub created_ms: i64,
    pub text: String,
}

/// A message part, typed. Unknown kinds decode into [`Part::Other`] with their
/// raw payload, so a newer backend never breaks the read.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(TextPart),
    Reasoning(ReasoningPart),
    Tool(ToolCall),
    StepStart(StepStart),
    StepFinish(StepFinish),
    Patch(Patch),
    /// A part kind this build does not model, kept raw.
    Other(OtherPart),
}

/// An assistant's (or user's) text part.
#[derive(Debug, Clone, PartialEq)]
pub struct TextPart {
    pub text: String,
    /// Server time the part started producing, when reported.
    pub started_at: Option<i64>,
}

/// An assistant's reasoning part.
#[derive(Debug, Clone, PartialEq)]
pub struct ReasoningPart {
    pub text: String,
    /// Server time the part started producing, when reported.
    pub started_at: Option<i64>,
}

/// A step boundary that carries no renderable content.
#[derive(Debug, Clone, PartialEq)]
pub struct StepStart;

/// A step's finish. Its reason declares the Turn's completion state.
#[derive(Debug, Clone, PartialEq)]
pub struct StepFinish {
    pub reason: FinishReason,
}

/// Why a step finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// The model paused to run tools; the Turn is NOT complete.
    ToolCalls,
    Stop,
    Length,
    ContentFilter,
    Error,
    /// A reason this build does not know — terminal, like every reason except
    /// a pause to run tools.
    Other(String),
    /// No reason was reported at all: NOT terminal. Completion is only ever
    /// declared by a reason the server spelled out.
    Unknown,
}

impl FinishReason {
    /// Whether this reason ends the Turn: every reason except a pause to run
    /// tools and a missing one.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::ToolCalls | Self::Unknown)
    }
}

/// A snapshot patch part: the files a step changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub hash: Option<String>,
    pub files: Vec<String>,
}

/// A part kind this build does not model, kept raw.
#[derive(Debug, Clone, PartialEq)]
pub struct OtherPart {
    /// The backend's kind name, empty when the payload named none.
    pub kind: String,
    pub raw: Value,
}

/// One tool call: who it is, how it is doing, and its raw payloads.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub identity: ToolIdentity,
    pub status: ToolStatus,
    /// Server time the call started, when reported.
    pub started_at: Option<i64>,
    /// The raw structured input, verbatim.
    pub input: Option<Value>,
    /// The raw metadata, verbatim — an edit's unified diff lives here.
    pub metadata: Option<Value>,
    pub output: ToolOutput,
}

/// A tool call's identity: the tool's name plus the call's opaque correlation
/// id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolIdentity {
    pub name: String,
    pub call_id: String,
}

/// A tool call's lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Error,
    /// A status this build does not know, kept verbatim.
    Other(String),
    /// The payload reported no status at all.
    Unknown,
}

impl ToolStatus {
    /// Whether the call is still live: pending or running, i.e. not settled.
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// A tool call's output: the raw payload, its content blocks, and the
/// failure message when it errored. There is deliberately no `String` output
/// field — presentation assembles the panel from these typed pieces
/// (ADR-0042).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutput {
    /// The tool's output payload as the server reported it, verbatim.
    /// `None` when the state carried no output.
    pub raw: Option<Value>,
    /// The output's content blocks: one text block with the output text the
    /// Bridge has always rendered, followed by any non-text blocks kept raw.
    /// The decoder owns the source precedence, so a payload that carries more
    /// than one text source never renders twice.
    pub blocks: Vec<ContentBlock>,
    /// The failure message when the call errored. It is output-side data: the
    /// decoder normalizes the server's error shapes, while how a failure
    /// renders stays in the Platform.
    pub error: Option<String>,
}

/// One output content block. Text blocks carry their text; any other kind
/// stays raw.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text(String),
    /// A content block kind this build does not model, kept raw.
    Other(Value),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(
        id: &str,
        role: MessageRole,
        time: Option<MessageTime>,
        parts: Vec<Part>,
    ) -> TranscriptMessage {
        TranscriptMessage {
            id: MessageId::new(id),
            role,
            time,
            model: None,
            tokens: None,
            parts,
        }
    }

    fn text_part(text: &str) -> Part {
        Part::Text(TextPart {
            text: text.to_string(),
            started_at: None,
        })
    }

    fn finish(reason: FinishReason) -> Part {
        Part::StepFinish(StepFinish { reason })
    }

    fn anchored(id: &str, created_ms: i64) -> (TranscriptMessage, TurnAnchor) {
        let message = message(
            id,
            MessageRole::User,
            Some(MessageTime {
                created: created_ms,
                completed: Some(created_ms),
            }),
            vec![text_part("问题")],
        );
        let anchor = message.anchor().expect("timed message has an anchor");
        (message, anchor)
    }

    #[test]
    fn newest_user_takes_the_latest_server_time_and_ignores_missing_times() {
        let (first, _) = anchored("msg_u1", 1_000);
        let (second, _) = anchored("msg_u2", 2_000);
        let no_time = message("msg_u3", MessageRole::User, None, vec![text_part("无时间")]);
        let transcript = SessionTranscript::new(vec![
            first,
            message(
                "msg_a1",
                MessageRole::Assistant,
                Some(MessageTime {
                    created: 1_500,
                    completed: Some(1_500),
                }),
                vec![],
            ),
            second,
            no_time,
        ]);

        let newest = transcript.newest_user().expect("a timed user message exists");
        assert_eq!(newest.id.as_str(), "msg_u2");
        assert_eq!(newest.time.unwrap().created, 2_000);

        // No user message at all → no newest.
        let only_assistant = SessionTranscript::new(vec![message(
            "msg_a1",
            MessageRole::Assistant,
            Some(MessageTime {
                created: 1_000,
                completed: Some(1_000),
            }),
            vec![],
        )]);
        assert!(only_assistant.newest_user().is_none());
    }

    #[test]
    fn turn_for_user_includes_in_flight_and_ignores_pre_anchor_messages() {
        let (_, anchor) = anchored("msg_u1", 1_000);
        let transcript = SessionTranscript::new(vec![
            // The previous Turn's completed message: finished before the anchor.
            message(
                "msg_old",
                MessageRole::Assistant,
                Some(MessageTime {
                    created: 700,
                    completed: Some(800),
                }),
                vec![finish(FinishReason::Stop)],
            ),
            // Still being produced as this Turn began (created before the
            // anchor, completed after): this Turn's content.
            message(
                "msg_straddle",
                MessageRole::Assistant,
                Some(MessageTime {
                    created: 900,
                    completed: Some(1_100),
                }),
                vec![text_part("接着上一个回合")],
            ),
            // In flight: no completion stamp → always this Turn's.
            message(
                "msg_inflight",
                MessageRole::Assistant,
                Some(MessageTime {
                    created: 1_200,
                    completed: None,
                }),
                vec![text_part("正在写")],
            ),
            // Started within the Turn.
            message(
                "msg_new",
                MessageRole::Assistant,
                Some(MessageTime {
                    created: 2_000,
                    completed: Some(2_100),
                }),
                vec![text_part("答案")],
            ),
            // A message with no server time cannot be placed.
            message("msg_timeless", MessageRole::Assistant, None, vec![text_part("?")]),
            // Non-assistant roles never render into the Turn.
            message(
                "msg_user_late",
                MessageRole::User,
                Some(MessageTime {
                    created: 2_500,
                    completed: Some(2_500),
                }),
                vec![text_part("追问")],
            ),
        ]);

        let turn = transcript.turn_for_user(&anchor);
        assert_eq!(turn.anchor.message_id.as_str(), "msg_u1");
        assert_eq!(turn.anchor.created_ms, 1_000);
        let ids: Vec<_> = turn.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["msg_straddle", "msg_inflight", "msg_new"]);
        assert!(!turn.complete, "an in-flight tail is not a completed Turn");
    }

    #[test]
    fn turn_for_user_completes_on_terminal_finish_but_not_on_tool_calls() {
        let (_, anchor) = anchored("msg_u1", 1_000);
        let assistant = |id: &str, created: i64, reason: FinishReason| {
            message(
                id,
                MessageRole::Assistant,
                Some(MessageTime {
                    created,
                    completed: Some(created + 100),
                }),
                vec![finish(reason)],
            )
        };

        // A pause to run tools is not completion.
        let transcript = SessionTranscript::new(vec![assistant("msg_a1", 1_100, FinishReason::ToolCalls)]);
        assert!(!transcript.turn_for_user(&anchor).complete);

        // Every other spelled-out reason is terminal.
        for reason in [
            FinishReason::Stop,
            FinishReason::Length,
            FinishReason::ContentFilter,
            FinishReason::Error,
            FinishReason::Other("new-reason".to_string()),
        ] {
            let transcript = SessionTranscript::new(vec![assistant("msg_a1", 1_100, reason.clone())]);
            assert!(
                transcript.turn_for_user(&anchor).complete,
                "reason {reason:?} must be terminal"
            );
        }

        // A missing reason never declares completion.
        let transcript = SessionTranscript::new(vec![assistant("msg_a1", 1_100, FinishReason::Unknown)]);
        assert!(!transcript.turn_for_user(&anchor).complete);

        // A finish BEFORE the anchor belongs to the previous Turn.
        let transcript = SessionTranscript::new(vec![assistant("msg_old", 500, FinishReason::Stop)]);
        assert!(!transcript.turn_for_user(&anchor).complete);
    }

    #[test]
    fn projections_tolerate_unknown_parts_and_statuses() {
        let (user, anchor) = anchored("msg_u1", 1_000);
        let assistant = message(
            "msg_a1",
            MessageRole::Assistant,
            Some(MessageTime {
                created: 1_100,
                completed: Some(1_200),
            }),
            vec![
                // A part kind a newer backend invented.
                Part::Other(OtherPart {
                    kind: "mystery".into(),
                    raw: serde_json::json!({"type": "mystery", "payload": 42}),
                }),
                // A tool whose status this build does not know.
                Part::Tool(ToolCall {
                    identity: ToolIdentity {
                        name: "mystery".into(),
                        call_id: "call_1".into(),
                    },
                    status: ToolStatus::Other("weird".into()),
                    started_at: None,
                    input: None,
                    metadata: None,
                    output: ToolOutput::default(),
                }),
                // A tool whose payload reported no status at all.
                Part::Tool(ToolCall {
                    identity: ToolIdentity {
                        name: "unknown".into(),
                        call_id: "call_2".into(),
                    },
                    status: ToolStatus::Unknown,
                    started_at: None,
                    input: None,
                    metadata: None,
                    output: ToolOutput::default(),
                }),
            ],
        );
        let transcript = SessionTranscript::new(vec![user, assistant]);

        // Unknown arms are not completion and do not disturb the projections.
        let turn = transcript.turn_for_user(&anchor);
        assert_eq!(turn.messages.len(), 1);
        assert!(!turn.complete);
        assert_eq!(transcript.newest_user().unwrap().id.as_str(), "msg_u1");
        let tail = transcript.transcript_tail();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].text, "问题");
    }

    #[test]
    fn tail_is_newest_last_text_bearing_only_and_caps_at_four() {
        let user = |id: &str, created: i64, text: &str| {
            message(
                id,
                MessageRole::User,
                Some(MessageTime {
                    created,
                    completed: Some(created),
                }),
                vec![text_part(text)],
            )
        };
        let mut messages = vec![
            user("u1", 1_000, "问题一"),
            // Reasoning/tool-only assistant: excluded.
            message(
                "a1",
                MessageRole::Assistant,
                Some(MessageTime {
                    created: 2_000,
                    completed: Some(2_000),
                }),
                vec![Part::Reasoning(ReasoningPart {
                    text: "想想".into(),
                    started_at: None,
                })],
            ),
            // Image-only user message (no text part): excluded.
            message(
                "u2",
                MessageRole::User,
                Some(MessageTime {
                    created: 3_000,
                    completed: Some(3_000),
                }),
                vec![Part::Other(OtherPart {
                    kind: "file".into(),
                    raw: serde_json::json!({"type": "file"}),
                })],
            ),
            // A system message: excluded.
            message(
                "s1",
                MessageRole::System,
                Some(MessageTime {
                    created: 3_500,
                    completed: Some(3_500),
                }),
                vec![text_part("系统提示")],
            ),
        ];
        for i in 4..=8 {
            messages.push(user(&format!("u{i}"), i * 1_000, &format!("m{i}")));
        }
        // A message without a created time cannot be ordered: dropped.
        messages.push(message(
            "u_none",
            MessageRole::User,
            None,
            vec![text_part("无时间")],
        ));

        let tail = SessionTranscript::new(messages).transcript_tail();
        let texts: Vec<_> = tail.iter().map(|entry| entry.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["m5", "m6", "m7", "m8"],
            "tail must cap at 4, newest last"
        );
        assert_eq!(tail[0].role, MessageRole::User);
        assert_eq!(tail[3].created_ms, 8_000);
    }

    #[test]
    fn tail_joins_multiple_text_parts_and_keeps_roles() {
        let parts = vec![text_part("第一段"), text_part("第二段")];
        let transcript = SessionTranscript::new(vec![
            message(
                "u1",
                MessageRole::User,
                Some(MessageTime {
                    created: 1_000,
                    completed: Some(1_000),
                }),
                parts,
            ),
            message(
                "a1",
                MessageRole::Assistant,
                Some(MessageTime {
                    created: 2_000,
                    completed: Some(2_000),
                }),
                vec![text_part("回答")],
            ),
        ]);
        let tail = transcript.transcript_tail();
        let roles: Vec<_> = tail.iter().map(|entry| entry.role.clone()).collect();
        assert_eq!(roles, vec![MessageRole::User, MessageRole::Assistant]);
        assert_eq!(tail[0].text, "第一段\n第二段");
        assert_eq!(tail[1].text, "回答");
    }

    #[test]
    fn a_message_without_time_has_no_anchor() {
        let timeless = message("msg_x", MessageRole::User, None, vec![text_part("你好")]);
        assert!(timeless.anchor().is_none());
        assert_eq!(timeless.text(), "你好");

        let timed = message(
            "msg_y",
            MessageRole::User,
            Some(MessageTime {
                created: 42,
                completed: None,
            }),
            vec![],
        );
        let anchor = timed.anchor().expect("a timed message anchors");
        assert_eq!(anchor.message_id.as_str(), "msg_y");
        assert_eq!(anchor.created_ms, 42);
    }

    #[test]
    fn tool_view_keeps_payloads_raw_and_status_typed() {
        // The model carries raw tool payloads and a typed status; it must not
        // grow a `String` output field (ADR-0042's presentation stays in the
        // Platform).
        let call = ToolCall {
            identity: ToolIdentity {
                name: "edit".into(),
                call_id: "call_1".into(),
            },
            status: ToolStatus::Completed,
            started_at: Some(1_000),
            input: Some(serde_json::json!({"filePath": "src/a.rs"})),
            metadata: Some(serde_json::json!({"diff": "@@ -1 +1 @@"})),
            output: ToolOutput {
                raw: Some(serde_json::json!("Edit applied successfully.")),
                blocks: vec![ContentBlock::Text("Edit applied successfully.".into())],
                error: None,
            },
        };
        assert!(!call.status.is_live());
        assert_eq!(
            call.output.raw.as_ref().unwrap().as_str().unwrap(),
            "Edit applied successfully."
        );
        assert_eq!(call.metadata.as_ref().unwrap()["diff"], "@@ -1 +1 @@");
    }

    #[test]
    fn context_used_prefers_total_over_input_delta() {
        // Real shape: `input` is only the per-message delta; the cached prefix
        // is the bulk of the context. Using `input` alone understates usage.
        let tokens = TokenUsage {
            input: 263,
            output: 308,
            total: 612_920,
            cache_read: 612_096,
            cache_write: 0,
        };
        assert_eq!(tokens.context_used(), 612_920);

        // No `total` (older server): input + cache.read.
        let tokens = TokenUsage {
            input: 263,
            total: 0,
            cache_read: 612_096,
            ..Default::default()
        };
        assert_eq!(tokens.context_used(), 612_359);

        // Degenerate: neither present.
        assert_eq!(TokenUsage::default().context_used(), 0);
    }
}
