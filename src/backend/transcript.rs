//! The Session Transcript: the neutral read of one Session (spec #332).
//!
//! The Bridge and the Platform consume only these views; backend protocol
//! field names stay inside the adapter's decoders (ADR-0053). A message's
//! identity and its server time travel together ([`TurnAnchor`]), so no caller
//! reassembles an anchor from backend fields.
//!
//! A failed turn is transcript content: the server records the failure on its
//! assistant message, and the async-native Turn derives the turn's error from
//! that field ([`TurnView::error`]) instead of a blocking prompt response.
//!
//! The transcript also carries the current generation's own interaction facts
//! (ADR-0059): each **Execution**'s durable boundary and outcome, each
//! **Wake**, and the **Background Tasks** still live at the end of the read.
//! They are typed here and empty on a generation that records none (V1), so no
//! flow re-derives them from message kinds or timestamps.

use serde_json::Value;

/// One Session's normalized read: every message the backend reports, decoded
/// into typed views, in the order the backend returned them, plus the current
/// generation's interaction facts.
#[derive(Debug, Clone, Default)]
pub struct SessionTranscript {
    pub messages: Vec<TranscriptMessage>,
    /// The Execution boundaries the backend recorded, oldest first. Empty on a
    /// generation that records none (V1) and when no boundary is present — a
    /// shutdown writes none, and absence is tolerated, never invented.
    pub executions: Vec<Execution>,
    /// The Wakes the backend recorded, oldest first. Empty on a generation
    /// without them (V1).
    pub wakes: Vec<Wake>,
    /// The Background Tasks still live at the end of the read: derived from the
    /// assistant tool parts that started them, with every matching Wake
    /// applied. Empty on a generation without them (V1).
    pub background_tasks: Vec<BackgroundTask>,
}

impl SessionTranscript {
    pub fn new(messages: Vec<TranscriptMessage>) -> Self {
        Self {
            messages,
            executions: Vec::new(),
            wakes: Vec::new(),
            background_tasks: Vec::new(),
        }
    }

    /// Attach the Execution boundaries the backend recorded.
    pub fn with_executions(mut self, executions: Vec<Execution>) -> Self {
        self.executions = executions;
        self
    }

    /// Attach the Wakes the backend recorded.
    pub fn with_wakes(mut self, wakes: Vec<Wake>) -> Self {
        self.wakes = wakes;
        self
    }

    /// Attach the Background Tasks still live at the end of the read.
    pub fn with_background_tasks(mut self, background_tasks: Vec<BackgroundTask>) -> Self {
        self.background_tasks = background_tasks;
        self
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

    /// The Turn anchor the user message `message_id` carries: the message's
    /// identity together with its server time, one fact (ADR-0026's
    /// `msg_cola_` id is how the Turn knows which message is its own). `None`
    /// when the read has no such user message or it carries no server time.
    /// The one derivation the renderer's anchor capture and the Turn's failure
    /// read both use, so they cannot drift.
    pub fn anchor_of_user(&self, message_id: &str) -> Option<TurnAnchor> {
        self.messages
            .iter()
            .find(|message| message.role == MessageRole::User && message.id.as_str() == message_id)
            .and_then(TranscriptMessage::anchor)
    }

    /// The Turn anchored on the user message `anchor` names: the assistant
    /// messages that belong to it, in transcript order, plus whether it has
    /// finished.
    ///
    /// Membership follows the streaming rule: a message created within the
    /// Turn always belongs; a message still in flight (no completion stamp)
    /// that predates the anchor belongs while it is recent — the previous run
    /// may still be streaming as this Turn begins — but an orphaned one (no
    /// server activity for [`IN_FLIGHT_STALE_AFTER_MS`]) stops belonging, so a
    /// run the server was killed in cannot replay its parts into every later
    /// Turn. A completed message belongs only when it was created within the
    /// Turn or was still being produced as the Turn began. A message with no
    /// server time at all cannot be placed and stays out.
    ///
    /// The Turn is complete when an assistant message that started within it
    /// carries a terminal step-finish reason (every reason except a pause to
    /// run tools). `error` carries the failure recorded on the Turn's newest
    /// assistant message, if any — a recovered earlier step is not a failure.
    pub fn turn_for_user(&self, anchor: &TurnAnchor) -> TurnView<'_> {
        let mut messages = Vec::new();
        let mut complete = false;
        for message in &self.messages {
            if message.role != MessageRole::Assistant {
                continue;
            }
            let Some(time) = &message.time else { continue };
            if !belongs_to_turn(message, anchor.created_ms) {
                continue;
            }
            if time.created >= anchor.created_ms && message.finishes_turn() {
                complete = true;
            }
            messages.push(message);
        }
        // The failure is the NEWEST assistant message's: a recovered step (an
        // earlier error the turn went on from) must not fail a turn whose final
        // state is clean, and a turn that failed leaves its error on the
        // message it settled on.
        let error = messages.last().and_then(|message| message.error.clone());
        TurnView {
            messages,
            complete,
            error,
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
/// A message still in flight (no completion stamp) belongs when it was created
/// within the Turn, or while its newest server activity is recent enough — the
/// previous run may still be streaming when this Turn's user message lands —
/// while an orphaned one (no activity for [`IN_FLIGHT_STALE_AFTER_MS`]) stops
/// belonging, so a run the server was killed in cannot replay its parts into
/// every later Turn. A completed message belongs only when it was created
/// within the Turn or was still being produced as the Turn began.
fn belongs_to_turn(message: &TranscriptMessage, anchor_ms: i64) -> bool {
    let Some(time) = message.time else { return false };
    match time.completed {
        None => {
            time.created >= anchor_ms
                || anchor_ms.saturating_sub(message.newest_activity_ms(time.created))
                    <= IN_FLIGHT_STALE_AFTER_MS
        }
        Some(completed) => time.created >= anchor_ms || completed >= anchor_ms,
    }
}

/// How long a message created before a Turn's anchor may go without a
/// completion stamp and still belong to the Turn. An in-flight message's
/// newest server activity is its creation time or its latest part start; once
/// the anchor is this far past it, the message cannot be producing anymore —
/// the server was killed mid-run and no later read settles the orphan
/// (verified: a restart leaves the message and its `running` tool part as they
/// were), so rendering it would replay its parts on every later Turn. Ten
/// minutes matches the bridge's default turn drain/follow bound; retune the
/// two together.
const IN_FLIGHT_STALE_AFTER_MS: i64 = 10 * 60 * 1000;

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
    /// The failure the server recorded on this message, when it recorded one
    /// (a model error, an abort, a context overflow). The async-native Turn
    /// reads a turn's failure from here instead of a blocking prompt response.
    pub error: Option<String>,
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

    /// The newest server activity this message reports: its creation time and
    /// the latest part start it carries. The neutral read carries no part end
    /// time, so a part contributes its start only; boundary kinds contribute
    /// nothing.
    fn newest_activity_ms(&self, created: i64) -> i64 {
        self.parts
            .iter()
            .filter_map(Part::started_at)
            .fold(created, i64::max)
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

/// One Turn as read from a transcript: the assistant messages that belong to
/// it (in-flight ones included), whether it has finished, and the failure its
/// newest assistant message recorded.
#[derive(Debug)]
pub struct TurnView<'a> {
    pub messages: Vec<&'a TranscriptMessage>,
    pub complete: bool,
    /// The failure recorded on the Turn's NEWEST assistant message, if any — a
    /// recovered earlier step is not a failure.
    pub error: Option<String>,
}

/// One Execution boundary, as the backend records it: the durable marker that
/// a busy period ended, its server time, and the outcome it recorded. A
/// shutdown records none, so absence is a normal read — never invented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    /// The marker message's identity.
    pub id: MessageId,
    /// When the Execution ended (the marker's server time).
    pub ended_ms: i64,
    pub outcome: ExecutionOutcome,
}

/// How an Execution ended, as the backend recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Succeeded,
    Failed,
    Interrupted,
    /// An outcome this build does not know, kept verbatim.
    Other(String),
    /// The payload reported no outcome at all.
    Unknown,
}

/// One Wake: a backend-generated message that resumed a Session with no user
/// message — a Background Task finishing, a subagent completing, the server
/// continuing after an interruption or restart (ADR-0059). The correlation
/// keys are what retire the Background Task it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wake {
    /// The message's identity.
    pub id: MessageId,
    /// The message's server time.
    pub created_ms: i64,
    /// What woke the Session.
    pub source: WakeSource,
    /// The shell a shell Wake completed, when it named one.
    pub shell_id: Option<String>,
    /// The background job a shell Wake completed, when it named one.
    pub job_id: Option<String>,
    /// The child session a subagent Wake completed, when it named one.
    pub child_id: Option<String>,
    /// The finished task's state as the Wake reported it (`completed`,
    /// `cancelled`, `error`), when it reported one.
    pub state: Option<String>,
}

impl Wake {
    /// Whether this Wake retires `task` — the backend's own completion
    /// correlation: a shell Wake names the task's shell id or the tool call
    /// that started it, and a subagent Wake names the task's child session. A
    /// Wake that names nothing retires nothing.
    pub fn retires(&self, task: &BackgroundTask) -> bool {
        let names_shell_or_call = |id: &str| task.shell_id.as_deref() == Some(id) || task.tool.call_id == id;
        self.shell_id.as_deref().is_some_and(names_shell_or_call)
            || self.job_id.as_deref().is_some_and(names_shell_or_call)
            || self
                .child_id
                .as_deref()
                .is_some_and(|child| task.child_id.as_deref() == Some(child))
    }
}

/// What kind of event woke the Session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeSource {
    /// A backgrounded shell command finished.
    Shell,
    /// A subagent finished.
    Subagent,
    /// The server restarted while the Session was working.
    Restart,
    /// The backend resumed an interrupted response.
    Interrupt,
    /// A source this build does not know, kept verbatim.
    Other(String),
    /// The payload named no source at all (OpenCode 2.0.x's interruption
    /// continuation carries no marker).
    Unknown,
}

/// Work the agent left running in the background: derived from the assistant
/// tool part that started it, live until its Wake retires it (ADR-0059). While
/// one is live its Turn is not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    /// The tool call that started the task — the part's own identity.
    pub tool: ToolIdentity,
    /// The shell the task runs in, when the part reported one.
    pub shell_id: Option<String>,
    /// The child session a subagent runs in, when the part reported one.
    pub child_id: Option<String>,
    /// When the tool call started the task, when the payload reported a time.
    pub started_at: Option<i64>,
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

impl Part {
    /// The server time the part started producing, when it reports one: text,
    /// reasoning and tool parts carry a start; boundary kinds carry none. The
    /// one place the part-kind-to-time mapping lives, so liveness reads cannot
    /// drift.
    pub fn started_at(&self) -> Option<i64> {
        match self {
            Part::Text(text) => text.started_at,
            Part::Reasoning(reasoning) => reasoning.started_at,
            Part::Tool(call) => call.started_at,
            Part::StepStart(_) | Part::StepFinish(_) | Part::Patch(_) | Part::Other(_) => None,
        }
    }
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
            error: None,
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

    /// One tool part with the given server start time and lifecycle status.
    fn tool(name: &str, started_at: Option<i64>, status: ToolStatus) -> Part {
        Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: name.into(),
                call_id: name.into(),
            },
            status,
            started_at,
            input: None,
            metadata: None,
            output: ToolOutput::default(),
        })
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
            // In flight but created within the Turn: always this Turn's.
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
        let ids: Vec<_> = turn.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["msg_straddle", "msg_inflight", "msg_new"]);
        assert!(!turn.complete, "an in-flight tail is not a completed Turn");
    }

    /// #378: the server can be killed mid-run, leaving an assistant message
    /// without a completion stamp that no later read settles (verified on
    /// 2.0.18: a restart leaves the message and its `running` tool part as
    /// they were). Once its newest server activity is older than the
    /// staleness window it cannot be producing anymore, so it stops belonging
    /// — otherwise its parts (e.g. a stuck or errored tool panel) re-render on
    /// every later Turn.
    #[test]
    fn an_orphaned_in_flight_message_stops_belonging() {
        let (_, anchor) = anchored("msg_u1", 10_000_000);
        let mut zombie = message(
            "msg_zombie",
            MessageRole::Assistant,
            Some(MessageTime {
                created: anchor.created_ms - 1_500_000,
                completed: None,
            }),
            vec![tool(
                "bash",
                Some(anchor.created_ms - 1_490_000),
                ToolStatus::Running,
            )],
        );
        zombie.error = Some("被杀的运行".into());

        let transcript = SessionTranscript::new(vec![zombie]);
        let turn = transcript.turn_for_user(&anchor);
        assert!(
            turn.messages.is_empty(),
            "an orphan with 24 minutes of silence must not belong: {:?}",
            turn.messages
        );
        assert!(
            turn.error.is_none(),
            "an orphan's recorded error must not leak into the Turn"
        );
    }

    /// A pre-anchor in-flight message whose newest activity is inside the
    /// window is the legitimate straddle (#310: the previous run was still
    /// streaming as this Turn began) and keeps belonging.
    #[test]
    fn a_recent_pre_anchor_in_flight_message_belongs() {
        let (_, anchor) = anchored("msg_u1", 10_000_000);
        // Created just under 9 minutes before the anchor, last part a minute
        // later: inside the 10-minute window.
        let transcript = SessionTranscript::new(vec![message(
            "msg_straddle",
            MessageRole::Assistant,
            Some(MessageTime {
                created: anchor.created_ms - 540_000,
                completed: None,
            }),
            vec![tool(
                "task",
                Some(anchor.created_ms - 480_000),
                ToolStatus::Running,
            )],
        )]);

        let turn = transcript.turn_for_user(&anchor);
        let ids: Vec<_> = turn.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["msg_straddle"]);
    }

    /// A part that started at/after the anchor is activity within this Turn,
    /// so the message keeps belonging even when its creation is old.
    #[test]
    fn a_pre_anchor_in_flight_message_with_activity_after_the_anchor_belongs() {
        let (_, anchor) = anchored("msg_u1", 10_000_000);
        let transcript = SessionTranscript::new(vec![message(
            "msg_straddle",
            MessageRole::Assistant,
            Some(MessageTime {
                created: anchor.created_ms - 1_500_000,
                completed: None,
            }),
            vec![tool("task", Some(anchor.created_ms + 500), ToolStatus::Running)],
        )]);

        let turn = transcript.turn_for_user(&anchor);
        let ids: Vec<_> = turn.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["msg_straddle"]);
    }

    /// A message created within the Turn always belongs — a live Turn sitting
    /// on a settled errored tool while the model continues must keep
    /// rendering.
    #[test]
    fn an_in_turn_in_flight_message_belongs_however_settled() {
        let (_, anchor) = anchored("msg_u1", 10_000_000);
        let transcript = SessionTranscript::new(vec![message(
            "msg_live",
            MessageRole::Assistant,
            Some(MessageTime {
                created: anchor.created_ms + 100,
                completed: None,
            }),
            vec![tool("bash", Some(anchor.created_ms + 200), ToolStatus::Error)],
        )]);

        let turn = transcript.turn_for_user(&anchor);
        let ids: Vec<_> = turn.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["msg_live"]);
    }

    /// The completed-message rule is untouched by the staleness window: a
    /// message created long before the anchor but completed at/after it was
    /// still being produced as this Turn began and belongs (#190/#310).
    #[test]
    fn a_completed_straddler_belongs_however_old() {
        let (_, anchor) = anchored("msg_u1", 10_000_000);
        let transcript = SessionTranscript::new(vec![message(
            "msg_straddle",
            MessageRole::Assistant,
            Some(MessageTime {
                created: anchor.created_ms - 3_600_000,
                completed: Some(anchor.created_ms + 100),
            }),
            vec![text_part("接着上一个回合")],
        )]);

        let turn = transcript.turn_for_user(&anchor);
        let ids: Vec<_> = turn.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["msg_straddle"]);
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

    /// A Turn's failure is its newest assistant message's: an error outside the
    /// Turn (completed before the anchor) never leaks in, a recovered earlier
    /// step does not fail a turn whose final message is clean, and a newer
    /// failure supersedes an older one.
    #[test]
    fn turn_for_user_takes_the_newest_messages_error_only() {
        let (_, anchor) = anchored("msg_u1", 1_000);
        let assistant = |id: &str, created: i64, completed: i64, error: Option<&str>| {
            let mut message = message(
                id,
                MessageRole::Assistant,
                Some(MessageTime {
                    created,
                    completed: Some(completed),
                }),
                vec![text_part("回答")],
            );
            message.error = error.map(str::to_string);
            message
        };

        // An error from the previous Turn (completed before the anchor) is out;
        // a failed first step and a failed final step: the newest wins.
        let previous = assistant("msg_old", 500, 800, Some("上一个回合的错误"));
        let failed_step = assistant("msg_a1", 1_100, 1_200, Some("第一步失败但恢复了"));
        let fatal = assistant("msg_a2", 1_300, 1_400, Some("最后失败"));
        let transcript = SessionTranscript::new(vec![previous.clone(), failed_step.clone(), fatal]);
        assert_eq!(
            transcript.turn_for_user(&anchor).error.as_deref(),
            Some("最后失败")
        );

        // A recovered earlier error with a clean final message is NOT a failure
        // (the error-card retry's shape: the retried step succeeds).
        let clean = assistant("msg_a2", 1_300, 1_400, None);
        let transcript = SessionTranscript::new(vec![previous, failed_step, clean]);
        assert!(transcript.turn_for_user(&anchor).error.is_none());
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
