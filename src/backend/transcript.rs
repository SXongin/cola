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

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One Session's normalized read: every message the backend reports, decoded
/// into typed views, in the order the backend returned them, plus the current
/// generation's interaction facts.
#[derive(Debug, Clone, Default)]
pub struct SessionTranscript {
    pub messages: Vec<TranscriptMessage>,
    /// The Execution boundaries the backend recorded, in the read's order
    /// (V2 reads `order=asc`, so oldest first; the decoder never re-sorts).
    /// Empty on a generation that records none (V1) and when no boundary is
    /// present — a shutdown writes none, and absence is tolerated, never
    /// invented.
    pub executions: Vec<Execution>,
    /// The Wakes the backend recorded, in the read's order (V2 reads
    /// `order=asc`, so oldest first; the decoder never re-sorts). Empty on a
    /// generation without them (V1).
    pub wakes: Vec<Wake>,
    /// The Background Tasks still live at the end of the read: derived from the
    /// assistant tool parts that started them, with every matching Wake
    /// applied. Empty on a generation without them (V1).
    pub background_tasks: Vec<BackgroundTask>,
    /// The Background Tasks a **runtime reconciliation** read retired while no
    /// Wake retired them ([`SessionTranscript::apply_task_runtime`]): the
    /// completion record was lost, and the runtime either reported a terminal
    /// end or no longer knows the task. They have already left
    /// [`Self::background_tasks`], so the settle decision treats them as
    /// ended; the ledger renders each as a completion entry. Empty unless a
    /// reconciliation read ran.
    pub runtime_retired: Vec<TaskRetirement>,
    /// The call ids of live Background Tasks a reconciliation read could not
    /// confirm as running (a subagent child the runtime reports inactive) while
    /// no Wake retired them. The ledger renders those rows as 状态待确认; the
    /// settle rule is deliberately unchanged (only a Wake or a positive
    /// terminal verdict retires a task).
    pub unconfirmed_tasks: std::collections::HashSet<String>,
}

impl SessionTranscript {
    pub fn new(messages: Vec<TranscriptMessage>) -> Self {
        Self {
            messages,
            executions: Vec::new(),
            wakes: Vec::new(),
            background_tasks: Vec::new(),
            runtime_retired: Vec::new(),
            unconfirmed_tasks: std::collections::HashSet::new(),
        }
    }

    /// Attach the Execution boundaries the backend recorded. The V2 decoder
    /// builds a read through these, and a scripted bridge test sets the facts
    /// directly (the transcript is the one seam every flow consumes).
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

    /// Apply one runtime reconciliation read (`TaskRuntime`, issue #454): a
    /// shell the runtime reports ended — or no longer knows at all — leaves the
    /// live list as a [`TaskRetirement`]; a subagent child the runtime reports
    /// inactive stays live but is marked unconfirmed; a task with no verdict is
    /// untouched. An empty read changes nothing, so V1 and a server that could
    /// not answer reconcile to the transcript exactly as read.
    ///
    /// The retirement is deliberately limited to positive evidence: an id the
    /// runtime did not mention (a failed per-shell read, an unrecognised active
    /// entry) never retires a task, and a subagent only ever gains the
    /// unconfirmed marker. A Wake-retired task is already absent here.
    pub fn apply_task_runtime(&mut self, runtime: &TaskRuntime) {
        if runtime.shells.is_empty() && runtime.children.is_empty() {
            return;
        }
        let mut kept = Vec::with_capacity(self.background_tasks.len());
        let mut retired = Vec::new();
        for task in std::mem::take(&mut self.background_tasks) {
            if let Some(shell_id) = task.shell_id.clone() {
                match runtime.shell(&shell_id) {
                    // No verdict: the read could not place this shell — leave
                    // the task exactly as the transcript read it.
                    None | Some(ShellRuntime::Running) => kept.push(task),
                    Some(ShellRuntime::Ended { end, completed_at }) => retired.push(TaskRetirement {
                        task,
                        ending: TaskRetirementEnding::Ended(end.clone()),
                        finished_at: *completed_at,
                    }),
                    Some(ShellRuntime::Missing) => retired.push(TaskRetirement {
                        task,
                        ending: TaskRetirementEnding::Lost,
                        finished_at: None,
                    }),
                }
                continue;
            }
            let Some(child_id) = task.child_id.clone() else {
                // A task with neither identity cannot be reconciled.
                kept.push(task);
                continue;
            };
            if runtime.child(&child_id) == Some(ChildRuntime::Inactive) {
                self.unconfirmed_tasks.insert(task.tool.call_id.clone());
            }
            kept.push(task);
        }
        self.background_tasks = kept;
        self.runtime_retired.extend(retired);
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

    /// The input of the tool call `call_id`, joined over the WHOLE read: a
    /// Background Task can outlive the Turn that started it, so its part may
    /// sit anywhere in the transcript (ADR-0060's label join). `None` when no
    /// part carries that call's input. The one join the live ledger rows and a
    /// reconciliation retirement entry both read, so they cannot drift.
    pub fn tool_input(&self, call_id: &str) -> Option<&serde_json::Value> {
        self.messages
            .iter()
            .flat_map(|message| &message.parts)
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == call_id => call.input.as_ref(),
                _ => None,
            })
    }

    /// The tool call `call_id`, joined over the WHOLE read — not the Turn
    /// window, because a caller that owns a call identity (a **Carried Tool
    /// Panel**, ADR-0068) must still resolve its current status and output
    /// once the message it came from has gone stale. `None` when the read
    /// carries no such part; call ids are unique, so the first match is the
    /// call. The one join the carry's per-read reconciliation reads.
    pub fn tool_call(&self, call_id: &str) -> Option<&ToolCall> {
        self.messages
            .iter()
            .flat_map(|message| &message.parts)
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == call_id => Some(call),
                _ => None,
            })
    }

    /// The `running`/`pending` tool calls of the Turn anchored at `anchor` —
    /// the orphaned Turn's projection, membership rule included: only calls
    /// whose message [`belongs_to_turn`] may be handed to a successor as
    /// **Carried Tool Panels** (ADR-0068), so an older card's stale `running`
    /// part is never resurrected. In transcript order.
    pub fn turn_running_tools(&self, anchor: &TurnAnchor) -> Vec<ToolCall> {
        self.turn_for_user(anchor)
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                Part::Tool(call) if call.status.is_live() => Some(call.clone()),
                _ => None,
            })
            .collect()
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

    /// The single settle decision every Turn ending uses (ADR-0059, ADR-0062):
    /// the drain, the out-of-turn follow, the unreceived watch and the Wake
    /// continuation all read a Turn's ending from here.
    ///
    /// `anchor` is the Turn's scope: the submitted user message's identity
    /// together with its server time, once a read has carried the message.
    /// `None` means the submitted message never reached the transcript — a
    /// steered admit no runner promoted. The caller owns the liveness read: it
    /// asks for a decision only once the Session reads non-busy, so an
    /// anchorless ask is the Unreceived ending ([`TurnSettle::Unreceived`]):
    /// nobody will ever answer the message. A read whose Wakes are not yet
    /// answered still holds every decision back (including Unreceived): a
    /// Wake's resumed run is exactly what can promote the queued message.
    ///
    /// The decision never reads a terminal step. A Turn spans every Execution
    /// its chain opened — including ones a Wake opened after the Backend went
    /// idle — so its ending is decidable only at an idle read whose newest
    /// Execution boundary answers every [placeable](Wake::created_ms) Wake:
    /// the Wake's own content (an assistant step with a terminal finish)
    /// landing inside a finalization window can no longer declare the Turn
    /// complete. [`TurnSettle::Running`] therefore means the read's own
    /// boundary rule is unsatisfied — never "the session is busy".
    ///
    /// At an answered read the ending is, in decision order:
    ///
    /// - no anchor — [`TurnSettle::Unreceived`]: the submitted message is not
    ///   in the transcript and the Session is idle, so the card ends
    ///   「⚠️ 这条消息未被接收」 — never ✅;
    /// - [`TurnSettle::Failed`] — the Turn's settled failure (its newest
    ///   assistant message's error). A failure dominates: a failed Turn never
    ///   yields waiting, even with live Background Tasks;
    /// - [`TurnSettle::Waiting`] — Background Tasks are live: the card yields
    ///   「⏳ 等待后台任务」, neither ✅ nor a terminal, and sends no Completion
    ///   Notice;
    /// - [`TurnSettle::Complete`] — the true end: idle with no live Background
    ///   Task.
    ///
    /// A read with no Executions, Wakes or Background Tasks (V1) decides
    /// exactly as before: idle with no live Background Task is complete.
    pub fn settle(&self, anchor: Option<&TurnAnchor>) -> TurnSettle {
        if !self.wakes_answered() {
            return TurnSettle::Running;
        }
        let Some(anchor) = anchor else {
            // No anchor: `anchor_of_user` never found the submitted message.
            // Background Tasks read here belong to an earlier Turn — a Turn
            // whose run started has an anchor — so they cannot make this one
            // wait.
            return TurnSettle::Unreceived;
        };
        if let Some(error) = self.turn_for_user(anchor).error {
            return TurnSettle::Failed(error);
        }
        if !self.background_tasks.is_empty() {
            return TurnSettle::Waiting;
        }
        TurnSettle::Complete
    }

    /// Whether the read is idle-bounded after its Wakes: every Wake the read
    /// can place is answered by an Execution boundary at/after it (a Wake
    /// opens an Execution, and the Execution's durable `idle` closes it). A
    /// read with no placeable Wake is bounded — V1 carries none and neither
    /// does the session's first read — and a Wake without a server time is
    /// durable evidence that never blocks an ending, exactly like
    /// [`Wake::retires`]' own tolerance: it cannot be ordered against the
    /// boundary.
    fn wakes_answered(&self) -> bool {
        let Some(newest_wake) = self.wakes.iter().filter_map(|wake| wake.created_ms).max() else {
            return true;
        };
        self.executions
            .iter()
            .filter_map(|execution| execution.ended_ms)
            .max()
            .is_some_and(|boundary| boundary >= newest_wake)
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
    /// nothing. `pub` for the tail read's boundary test (ADR-0068), which must
    /// read exactly the rule the membership check uses.
    pub fn newest_activity_ms(&self, created: i64) -> i64 {
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

/// A message's opaque server identity. Serializes as the bare id string (a
/// newtype), so a durable record that names a message stays readable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

impl TurnAnchor {
    /// The oldest server activity a message may carry and still belong to this
    /// Turn's projection (see [`belongs_to_turn`]): the anchor minus the
    /// in-flight window. A tail read can stop once it is past this line — no
    /// older message can be the Turn's. One derivation, so the read's bound
    /// and the membership rule cannot drift.
    pub fn in_flight_boundary_ms(&self) -> i64 {
        self.created_ms.saturating_sub(IN_FLIGHT_STALE_AFTER_MS)
    }
}

/// The newest end of one Session's transcript, as read by a bounded
/// newest-first tail scan (ADR-0068): the messages from the newest end back to
/// (and including) the page that crossed `boundary_ms`.
///
/// `complete` reports whether the scan actually reached `boundary_ms` (or the
/// session's start). `false` means a page cap stopped it early: the tail is a
/// prefix of the history, NOT a complete view of the window, so a caller that
/// needs the whole window — the restart carry — must treat it as carrying
/// nothing rather than guessing from a partial read.
#[derive(Debug, Clone, Default)]
pub struct TranscriptTail {
    pub transcript: SessionTranscript,
    pub complete: bool,
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

/// How a Turn ends, as [`SessionTranscript::settle`] decides it (ADR-0059,
/// ADR-0062). The one ending decision the drain, the out-of-turn follow, the
/// unreceived watch and the Wake continuation share; the sticky `/stop`
/// marker dominates it in the callers that know about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnSettle {
    /// The ending is not decided: a Wake's Execution has not reached its
    /// boundary yet, or the read carries no boundary to lean on. Keep
    /// observing; the caller's own liveness read already filtered "busy".
    Running,
    /// The true end with the Turn's settled failure — the newest assistant
    /// message's error. A failure dominates [`Self::Waiting`].
    Failed(String),
    /// The Execution ended but Background Tasks are still live: the card
    /// yields 「⏳ 等待后台任务」 — not ✅, not a terminal, no Completion
    /// Notice — and the next Wake continues the chain on a new card.
    Waiting,
    /// The Turn's submitted message never reached the transcript and the
    /// Session is not live (ADR-0062): a steered admit no runner promoted, so
    /// nobody will answer it. The card ends 「⚠️ 这条消息未被接收」 — never ✅ —
    /// with the 重新发起 action. The caller reaches this only from an
    /// idle-bounded read with no anchor: the message's own user message was
    /// never found (`anchor_of_user` absent).
    Unreceived,
    /// The true end: idle with no live Background Task.
    Complete,
}

/// One Execution boundary, as the backend records it: the durable marker that
/// a busy period ended, its server time when one was recorded, and the outcome
/// it recorded. A shutdown records none, so absence is a normal read — never
/// invented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    /// The marker message's identity.
    pub id: MessageId,
    /// When the Execution ended (the marker's server time). `None` when the
    /// payload carried no usable time: the outcome is still a fact, it just
    /// cannot be placed in time.
    pub ended_ms: Option<i64>,
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
    /// The message's server time. `None` when the payload carried no usable
    /// time: the Wake is still durable evidence that its task finished — it
    /// just cannot be ordered.
    pub created_ms: Option<i64>,
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
    /// The finished work's label as the Wake's OWN text named it: the shell
    /// command from `<shell … command="…">`, or the subagent's task
    /// description from `<subagent … description="…">`. `None` when the
    /// payload carried none — the label is never invented from the Wake's
    /// prose, and a receipt then claims only that something finished.
    pub label: Option<String>,
}

impl Wake {
    /// This Wake as a Turn anchor: the message's identity together with its
    /// server time, one fact. `None` when the payload carried no usable time —
    /// the Wake cannot be ordered, so no scope can be built from it.
    pub fn anchor(&self) -> Option<TurnAnchor> {
        Some(TurnAnchor {
            message_id: self.id.clone(),
            created_ms: self.created_ms?,
        })
    }

    /// Whether this Wake retires `task` — the backend's own completion
    /// correlation: a shell Wake names the task's shell id or the tool call
    /// that started it, and a subagent Wake names the task's child session. A
    /// Wake that names nothing retires nothing, and a Wake written before the
    /// task started cannot have completed it — but only when both times are
    /// known: an untimed record is durable evidence, never a reason to keep a
    /// phantom task live.
    pub fn retires(&self, task: &BackgroundTask) -> bool {
        if matches!(
            (self.created_ms, task.started_at),
            (Some(created), Some(started)) if created < started
        ) {
            return false;
        }
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

/// How a shell Background Task ended, as the runtime reports it (V2's
/// `/api/shell/{id}` terminal statuses). A status this build does not know
/// stays verbatim, like every other tolerant arm of the read model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEnd {
    Exited,
    Timeout,
    Killed,
    /// A status this build does not know, kept verbatim.
    Other(String),
}

/// One shell's runtime verdict from a reconciliation read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellRuntime {
    /// The runtime reports the shell running.
    Running,
    /// The runtime knows the shell and reports a terminal end.
    Ended {
        end: ShellEnd,
        completed_at: Option<i64>,
    },
    /// The runtime has no record of the shell: the record was removed, or the
    /// process that hosted it is gone. The task is not running under the
    /// attached server.
    Missing,
}

/// One subagent child session's runtime verdict: whether the server owns a live
/// drain for it. Only the live/inactive bit is read — the child's own transcript
/// is a separate, heavier read no reconciliation path needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildRuntime {
    /// The runtime reports the child session's drain active.
    Running,
    /// The runtime does not report the child active.
    Inactive,
}

/// The runtime evidence one reconciliation read carries, in the order asked.
/// An id with no verdict is no evidence at all — the caller must leave its task
/// exactly as the transcript read it, never guess.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskRuntime {
    pub shells: Vec<(String, ShellRuntime)>,
    pub children: Vec<(String, ChildRuntime)>,
}

impl TaskRuntime {
    /// The verdict for one shell id, when the read produced one.
    pub fn shell(&self, shell_id: &str) -> Option<&ShellRuntime> {
        self.shells
            .iter()
            .find(|(id, _)| id == shell_id)
            .map(|(_, verdict)| verdict)
    }

    /// The verdict for one child session id, when the read produced one.
    pub fn child(&self, child_id: &str) -> Option<ChildRuntime> {
        self.children
            .iter()
            .find(|(id, _)| id == child_id)
            .map(|(_, verdict)| *verdict)
    }
}

/// A Background Task a runtime reconciliation retired while no Wake retired it
/// (issue #454): the completion record was lost, and the runtime either
/// reported a terminal end or no longer knows the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRetirement {
    pub task: BackgroundTask,
    pub ending: TaskRetirementEnding,
    /// When the run ended, when the runtime reported a completion time; the
    /// `Lost` ending carries none (the runtime had nothing to report).
    pub finished_at: Option<i64>,
}

/// What the runtime said about a retired task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRetirementEnding {
    /// The runtime reported a terminal end for the shell.
    Ended(ShellEnd),
    /// The runtime has no record of the task.
    Lost,
}

/// The process-local overlay of Background Tasks the runtime confirmed ended
/// (issue #454): the transcript's launch record never flips, so without this
/// every re-read of the transcript would resurrect a task the runtime already
/// retired — a live turn's ledger would show it again, and the next Turn would
/// yield waiting on it, forever.
///
/// The Bridge records each retirement here right after a reconciliation read
/// ([`SessionTranscript::apply_task_runtime`]); the adapter applies the overlay
/// to every transcript it returns ([`SessionTranscript`]'s
/// `background_tasks` minus the recorded call ids), so no read path can
/// disagree. It is deliberately in-memory: a cola restart loses it, the next
/// read re-derives the same retirement and re-renders one entry on the newest
/// chain, and the runtime explains the task again.
#[derive(Default)]
pub struct TaskRetirements {
    retired: std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>,
}

impl TaskRetirements {
    /// Record the call ids the runtime retired for one session. Idempotent.
    pub fn record(&self, session_id: &str, call_ids: &[String]) {
        let mut retired = self
            .retired
            .lock()
            .expect("the task-retirement lock is never poisoned");
        let session = retired.entry(session_id.to_string()).or_default();
        for call_id in call_ids {
            session.insert(call_id.clone());
        }
    }

    /// Remove every Background Task the runtime already retired for
    /// `session_id` from `transcript`'s live list. A session with no recorded
    /// retirement is untouched (the common case does no set lookup per task).
    pub fn apply(&self, session_id: &str, transcript: &mut SessionTranscript) {
        let retired = self
            .retired
            .lock()
            .expect("the task-retirement lock is never poisoned");
        let Some(ids) = retired.get(session_id) else {
            return;
        };
        transcript
            .background_tasks
            .retain(|task| !ids.contains(task.tool.call_id.as_str()));
    }
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

/// A run a settled tool call moved to the background: which tool launched it
/// and the identity that tool's metadata carries for it. The call's own
/// durable record — the Background Task's live membership is separate
/// ([`BackgroundTask`], which a retiring Wake drops). The generation decoder
/// that owns the launch marker produces it (`ToolCall::background_launch`,
/// implemented beside the V2 decoders).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundLaunch {
    /// A backgrounded shell command: the shell it runs in, when the metadata
    /// named one.
    Shell { shell_id: Option<String> },
    /// A backgrounded subagent: the child session it runs in, when the
    /// metadata named one.
    Subagent { child_id: Option<String> },
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

    /// One Execution boundary with the given end time. The read-model tests
    /// keep their own fixtures on purpose: this layer must not depend on the
    /// Bridge's test support (`src/bridge/test_support.rs`), which mirrors
    /// these in its scripting fixtures for bridge tests.
    fn boundary(ended_ms: i64) -> Execution {
        Execution {
            id: MessageId::new(format!("msg_idle_{ended_ms}")),
            ended_ms: Some(ended_ms),
            outcome: ExecutionOutcome::Succeeded,
        }
    }

    /// One shell Wake carrying the live task's correlation key.
    fn shell_wake(created_ms: i64) -> Wake {
        Wake {
            id: MessageId::new(format!("msg_wake_{created_ms}")),
            created_ms: Some(created_ms),
            source: WakeSource::Shell,
            shell_id: Some("sh_bg".into()),
            job_id: Some("sh_bg".into()),
            child_id: None,
            state: Some("completed".into()),
            label: Some("gh run watch".into()),
        }
    }

    /// The one live Background Task [`shell_wake`] retires.
    fn background_shell(started_at: i64) -> BackgroundTask {
        BackgroundTask {
            tool: ToolIdentity {
                name: "shell".into(),
                call_id: "call_bg".into(),
            },
            shell_id: Some("sh_bg".into()),
            child_id: None,
            started_at: Some(started_at),
        }
    }

    /// A terminal assistant step at `created`, optionally failed.
    fn assistant_step(id: &str, created: i64, error: Option<&str>) -> TranscriptMessage {
        let mut message = message(
            id,
            MessageRole::Assistant,
            Some(MessageTime {
                created,
                completed: Some(created + 100),
            }),
            vec![finish(FinishReason::Stop)],
        );
        message.error = error.map(str::to_string);
        message
    }

    /// Idle with no live Background Task is the true end — on a V2 read with a
    /// boundary and on a V1 read that carries no interaction facts at all.
    #[test]
    fn settle_completes_an_idle_read_with_no_live_background_task() {
        let (_, anchor) = anchored("msg_u1", 1_000);
        let v2 = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_wakes(vec![shell_wake(1_050)]);
        assert_eq!(v2.settle(Some(&anchor)), TurnSettle::Complete);

        let v1 = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)]);
        assert_eq!(v1.settle(Some(&anchor)), TurnSettle::Complete);
    }

    /// A live Background Task turns the idle ending into Waiting; a settled
    /// failure dominates it — a failed Turn never yields waiting.
    #[test]
    fn settle_yields_waiting_and_a_settled_failure_dominates_it() {
        let (_, anchor) = anchored("msg_u1", 1_000);
        let waiting = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![background_shell(1_100)]);
        assert_eq!(waiting.settle(Some(&anchor)), TurnSettle::Waiting);

        let failed = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, Some("provider 503"))])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![background_shell(1_100)]);
        assert_eq!(
            failed.settle(Some(&anchor)),
            TurnSettle::Failed("provider 503".into())
        );
    }

    /// The runtime reconciliation (issue #454): a shell the runtime reports
    /// ended — or no longer knows — leaves the live list as a retirement (with
    /// the runtime's completion time when it has one), so an idle read whose
    /// last task is gone settles complete; a running shell and a child the
    /// runtime reports inactive stay live (the child marked unconfirmed); a
    /// task with no verdict and an empty read are untouched.
    #[test]
    fn a_runtime_read_retires_ended_shells_and_marks_inactive_children() {
        let (_, anchor) = anchored("msg_u1", 1_000);
        let shell_task = |call_id: &str, shell_id: &str| BackgroundTask {
            tool: ToolIdentity {
                name: "shell".into(),
                call_id: call_id.into(),
            },
            shell_id: Some(shell_id.into()),
            child_id: None,
            started_at: Some(1_100),
        };
        let child_task = BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_sub".into(),
            },
            shell_id: None,
            child_id: Some("ses_child".into()),
            started_at: Some(1_100),
        };

        // An ended shell retires with the runtime's own completion time; a
        // running shell and an inactive child stay live (the child marked).
        let mut transcript = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![
                shell_task("call_done", "sh_done"),
                shell_task("call_run", "sh_run"),
                child_task.clone(),
            ]);
        transcript.apply_task_runtime(&TaskRuntime {
            shells: vec![
                (
                    "sh_done".into(),
                    ShellRuntime::Ended {
                        end: ShellEnd::Exited,
                        completed_at: Some(2_000),
                    },
                ),
                ("sh_run".into(), ShellRuntime::Running),
            ],
            children: vec![("ses_child".into(), ChildRuntime::Inactive)],
        });
        assert_eq!(
            transcript
                .background_tasks
                .iter()
                .map(|task| task.tool.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["call_run", "call_sub"],
            "only the ended shell left the live list"
        );
        assert!(transcript.unconfirmed_tasks.contains("call_sub"));
        assert_eq!(
            transcript.runtime_retired,
            vec![TaskRetirement {
                task: shell_task("call_done", "sh_done"),
                ending: TaskRetirementEnding::Ended(ShellEnd::Exited),
                finished_at: Some(2_000),
            }]
        );
        assert_eq!(
            transcript.settle(Some(&anchor)),
            TurnSettle::Waiting,
            "an unconfirmed child still holds the wait"
        );

        // A missing record retires as Lost with no completion time, so the
        // last task's retirement is the true end.
        let mut lost = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![shell_task("call_lost", "sh_lost")]);
        lost.apply_task_runtime(&TaskRuntime {
            shells: vec![("sh_lost".into(), ShellRuntime::Missing)],
            children: vec![],
        });
        assert_eq!(
            lost.runtime_retired[0].ending,
            TaskRetirementEnding::Lost,
            "a runtime without the record is the Lost ending"
        );
        assert_eq!(lost.runtime_retired[0].finished_at, None);
        assert!(lost.background_tasks.is_empty());
        assert_eq!(
            lost.settle(Some(&anchor)),
            TurnSettle::Complete,
            "a lost completion record no longer holds the wait"
        );

        // No verdict for an id (a failed per-shell read) and an empty read
        // leave the transcript exactly as read.
        let mut untouched = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![shell_task("call_bg", "sh_bg")]);
        untouched.apply_task_runtime(&TaskRuntime {
            shells: vec![("sh_other".into(), ShellRuntime::Missing)],
            children: vec![],
        });
        untouched.apply_task_runtime(&TaskRuntime::default());
        assert_eq!(untouched.background_tasks.len(), 1);
        assert!(untouched.runtime_retired.is_empty());
        assert_eq!(untouched.settle(Some(&anchor)), TurnSettle::Waiting);
    }

    /// The process-local retirement overlay (issue #454 review): a recorded
    /// call id leaves every later read of that session's live list, other
    /// sessions and other call ids stay untouched, and the live list's own
    /// facts (the retirement entries) are not what it filters.
    #[test]
    fn recorded_retirements_leave_later_reads_of_the_live_list() {
        let retirements = TaskRetirements::default();
        let live = || SessionTranscript::new(vec![]).with_background_tasks(vec![background_shell(1_100)]);
        let mut transcript = live();
        retirements.apply("ses_a", &mut transcript);
        assert_eq!(
            transcript.background_tasks.len(),
            1,
            "nothing recorded yet leaves the read as decoded"
        );

        retirements.record("ses_a", &["call_bg".to_string()]);
        let mut filtered = live();
        retirements.apply("ses_a", &mut filtered);
        assert!(
            filtered.background_tasks.is_empty(),
            "the recorded task leaves the live list"
        );

        let mut other_session = live();
        retirements.apply("ses_b", &mut other_session);
        assert_eq!(
            other_session.background_tasks.len(),
            1,
            "another session is untouched"
        );

        let mut other_id = SessionTranscript::new(vec![]).with_background_tasks(vec![BackgroundTask {
            tool: ToolIdentity {
                name: "shell".into(),
                call_id: "call_other".into(),
            },
            shell_id: Some("sh_other".into()),
            child_id: None,
            started_at: Some(1_100),
        }]);
        retirements.apply("ses_a", &mut other_id);
        assert_eq!(other_id.background_tasks.len(), 1, "another call id is untouched");
    }

    /// A Wake opens an Execution, so its content landing before that
    /// Execution's boundary cannot declare the Turn complete (or waiting): the
    /// decision stays Running until a boundary answers the Wake — and a read
    /// with no boundary at all cannot decide either. Retiring the Background
    /// Task alone does not end the Turn.
    #[test]
    fn an_unanswered_wake_keeps_the_turn_running() {
        let (_, anchor) = anchored("msg_u1", 1_000);
        let no_boundary = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_wakes(vec![shell_wake(1_300)]);
        assert_eq!(no_boundary.settle(Some(&anchor)), TurnSettle::Running);

        let woke = SessionTranscript::new(vec![
            assistant_step("msg_a1", 1_100, None),
            assistant_step("msg_wake_reply", 1_400, None),
        ])
        .with_executions(vec![boundary(1_200)])
        .with_wakes(vec![shell_wake(1_300)]);
        assert_eq!(woke.settle(Some(&anchor)), TurnSettle::Running);

        // The Wake's Execution ends: the boundary after the Wake answers it.
        let answered = SessionTranscript::new(vec![
            assistant_step("msg_a1", 1_100, None),
            assistant_step("msg_wake_reply", 1_400, None),
        ])
        .with_executions(vec![boundary(1_200), boundary(1_500)])
        .with_wakes(vec![shell_wake(1_300)]);
        assert_eq!(answered.settle(Some(&anchor)), TurnSettle::Complete);
    }

    /// A Wake without a server time cannot be ordered against a boundary, so
    /// — like its retirement rule — it never blocks an ending.
    #[test]
    fn an_untimed_wake_never_blocks_an_ending() {
        let (_, anchor) = anchored("msg_u1", 1_000);
        let mut untimed = shell_wake(1_300);
        untimed.created_ms = None;
        let transcript = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_wakes(vec![untimed]);

        assert_eq!(transcript.settle(Some(&anchor)), TurnSettle::Complete);
    }

    /// No anchor: the submitted message never reached the transcript. At an
    /// idle read (the caller owns the liveness read) nobody will answer it —
    /// the Unreceived ending (ADR-0062), even when the read carries other
    /// content and live Background Tasks (which belong to an earlier Turn: a
    /// Turn whose run started has an anchor). An unanswered Wake still holds
    /// the decision back — its resumed run is exactly what can promote the
    /// queued message.
    #[test]
    fn settle_ends_unreceived_when_the_submitted_message_never_landed() {
        let never_landed = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![background_shell(1_100)]);
        assert_eq!(never_landed.settle(None), TurnSettle::Unreceived);

        let unanswered_wake = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_wakes(vec![shell_wake(1_300)]);
        assert_eq!(unanswered_wake.settle(None), TurnSettle::Running);
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
