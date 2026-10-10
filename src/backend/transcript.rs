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
    /// The Background Tasks the read retired while no Wake retired them: a
    /// **runtime reconciliation** read that reported a terminal end or no
    /// longer knows the task ([`SessionTranscript::apply_task_runtime`]), a
    /// child-evidence read that concluded the subagent's own transcript
    /// ([`SessionTranscript::apply_child_evidence`]), or the user's own
    /// cleanup click ([`SessionTranscript::apply_cleanup`]). They have already
    /// left [`Self::background_tasks`], so the settle decision treats them as
    /// ended; the ledger renders each as a completion entry. Empty unless a
    /// reconciliation read, an evidence read or a cleanup ran.
    pub task_retirements: Vec<TaskRetirement>,
    /// The call ids of live Background Tasks a reconciliation read could not
    /// confirm as running (a subagent child the runtime reports inactive) while
    /// no Wake retired them. The ledger renders those rows as 状态待确认; the
    /// settle rule is deliberately unchanged (only a Wake or a positive
    /// terminal verdict retires a task). The adapter's overlay re-applies the
    /// previous reconciles' markers to every read, so a read the shared
    /// throttle did not spend a verdict on keeps them; only a verdict resolves
    /// one — `Running` clears it, and a task that leaves the live list drops
    /// it with its row.
    pub unconfirmed_tasks: std::collections::HashSet<String>,
    /// Whether the backend's read stopped at its own page cap with more
    /// content behind it (spec #561, review #569): the V2 projected-message
    /// read pages to `MAX_MESSAGE_PAGES` and returns what it has, so the
    /// neutral view must say the read is a PREFIX — a cursor inside it cannot
    /// prove the tail beyond the cap absent, and an ended projection must not
    /// settle on it. V1 reads are unbounded and always `false`.
    pub truncated: bool,
    /// The live shells' output windows a shared reconcile read established
    /// (spec #588, ticket #592), keyed by shell id: the last bounded lines
    /// under each running shell's ledger row. A shell the read spent nothing
    /// on has no key — its row keeps rendering the window a previous cycle
    /// established (the accumulator's own cache) — while [`ShellOutputRead`]
    /// carries the read's own outcome for the shells it did spend on. Empty on
    /// V1 and on every path that does not reconcile.
    pub shell_outputs: std::collections::HashMap<String, ShellOutputRead>,
}

impl SessionTranscript {
    pub fn new(messages: Vec<TranscriptMessage>) -> Self {
        Self {
            messages,
            executions: Vec::new(),
            wakes: Vec::new(),
            background_tasks: Vec::new(),
            task_retirements: Vec::new(),
            unconfirmed_tasks: std::collections::HashSet::new(),
            truncated: false,
            shell_outputs: std::collections::HashMap::new(),
        }
    }

    /// Attach the Execution boundaries the backend recorded. The V2 decoder
    /// builds a read through these, and a scripted bridge test sets the facts
    /// directly (the transcript is the one seam every flow consumes).
    pub fn with_executions(mut self, executions: Vec<Execution>) -> Self {
        self.executions = executions;
        self
    }

    /// Mark the read as a prefix the backend stopped at its page cap (spec
    /// #561, review #569): the ended projection's truncation rule reads this.
    /// The V2 adapter sets the field directly; this builder is for fixtures.
    #[cfg(test)]
    pub fn with_truncated(mut self) -> Self {
        self.truncated = true;
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
    /// entry) never retires a task. The unconfirmed marker is resolved only on
    /// evidence too: the read carries the markers the overlay re-applied (the
    /// previous reconciles' state), a `Running` verdict clears that child's
    /// marker, an `Inactive` one (re)sets it, and a verdict that does not answer
    /// for the child keeps exactly what the read carried. A task the verdict
    /// retires drops its marker with it. A Wake-retired task is already absent
    /// here.
    pub fn apply_task_runtime(&mut self, runtime: &TaskRuntime) {
        if runtime.shells.is_empty() && runtime.children.is_empty() {
            return;
        }
        let mut kept = Vec::with_capacity(self.background_tasks.len());
        let mut retired = Vec::new();
        for task in std::mem::take(&mut self.background_tasks) {
            let call_id = task.tool.call_id.clone();
            if let Some(shell_id) = task.shell_id.clone() {
                match runtime.shell(&shell_id) {
                    // No verdict: the read could not place this shell — leave
                    // the task exactly as the transcript read it.
                    None | Some(ShellRuntime::Running) => kept.push(task),
                    Some(ShellRuntime::Ended { end, completed_at }) => {
                        self.unconfirmed_tasks.remove(&call_id);
                        retired.push(TaskRetirement {
                            task,
                            ending: TaskRetirementEnding::Ended(end.clone()),
                            finished_at: *completed_at,
                        });
                    }
                    Some(ShellRuntime::Missing) => {
                        self.unconfirmed_tasks.remove(&call_id);
                        retired.push(TaskRetirement {
                            task,
                            ending: TaskRetirementEnding::Lost,
                            finished_at: None,
                        });
                    }
                }
                continue;
            }
            let Some(child_id) = task.child_id.clone() else {
                // A task with neither identity cannot be reconciled.
                kept.push(task);
                continue;
            };
            match runtime.child(&child_id) {
                // Positive evidence the run is live: the marker resolves.
                Some(ChildRuntime::Running) => {
                    self.unconfirmed_tasks.remove(&call_id);
                }
                Some(ChildRuntime::Inactive) => {
                    self.unconfirmed_tasks.insert(call_id);
                }
                // No answer (a read that could not place the child): keep the
                // marker exactly as the read carried it.
                None => {}
            }
            kept.push(task);
        }
        self.background_tasks = kept;
        self.task_retirements.extend(retired);
    }

    /// Apply the user's manual cleanup (spec #588, ticket #590): every named
    /// **live** Background Task leaves [`Self::background_tasks`] as a
    /// [`TaskRetirement`] with the [`TaskRetirementEnding::Cleaned`] ending and
    /// the click's own clock, exactly as a runtime reconciliation retires what
    /// the runtime confirmed — so the settle rule follows (the last task gone is
    /// the true end) and the ledger renders one 🧹 entry per cleared row. The
    /// ids come from the caller's read of this same transcript, so an id with
    /// no live task is ignored: a cleanup never invents a retirement for a task
    /// the read no longer lists. The cleared task also leaves
    /// [`Self::unconfirmed_tasks`]; every other id stays as the read left it.
    pub fn apply_cleanup(&mut self, call_ids: &[String], now_ms: i64) {
        let clearing: std::collections::HashSet<&str> = call_ids.iter().map(String::as_str).collect();
        if clearing.is_empty() {
            return;
        }
        let mut kept = Vec::with_capacity(self.background_tasks.len());
        for task in std::mem::take(&mut self.background_tasks) {
            if clearing.contains(task.tool.call_id.as_str()) {
                self.task_retirements.push(TaskRetirement {
                    task,
                    ending: TaskRetirementEnding::Cleaned,
                    finished_at: Some(now_ms),
                });
            } else {
                kept.push(task);
            }
        }
        self.background_tasks = kept;
        self.unconfirmed_tasks
            .retain(|id| !clearing.contains(id.as_str()));
    }

    /// Apply one child-evidence read (#591, issue #464) to the suspect task
    /// `call_id` — a live subagent the runtime reported inactive and whose own
    /// newest assistant message was just read. A terminal [`ChildEvidence`]
    /// retires the task with the message's completion clock (the
    /// [`TaskRetirementEnding::ChildEnded`] ending), [`ChildEvidence::Gone`]
    /// retires it as [`TaskRetirementEnding::Lost`] identity-only, and
    /// [`ChildEvidence::Unfinished`] changes nothing — the row stays exactly as
    /// the runtime marked it. An id with no live task invents no retirement.
    pub fn apply_child_evidence(&mut self, call_id: &str, evidence: ChildEvidence) {
        if evidence == ChildEvidence::Unfinished {
            return;
        }
        let Some(index) = self
            .background_tasks
            .iter()
            .position(|task| task.tool.call_id == call_id)
        else {
            return;
        };
        let task = self.background_tasks.remove(index);
        self.unconfirmed_tasks.remove(call_id);
        let (ending, finished_at) = match evidence {
            ChildEvidence::Terminal { completed_at } => {
                (TaskRetirementEnding::ChildEnded, Some(completed_at))
            }
            ChildEvidence::Gone => (TaskRetirementEnding::Lost, None),
            // Returned above; kept for the match's exhaustiveness.
            ChildEvidence::Unfinished => return,
        };
        self.task_retirements.push(TaskRetirement {
            task,
            ending,
            finished_at,
        });
    }

    /// The live subagent tasks a reconciliation read could not confirm as
    /// running — one `(call_id, child_id)` per suspect, in the read's order.
    /// The shared reconcile step spends at most one child-evidence read per
    /// suspect per cycle (#591, issue #464); a child the runtime reports
    /// running is never read, and neither is a task without a child session.
    pub fn unconfirmed_children(&self) -> Vec<(String, String)> {
        self.background_tasks
            .iter()
            .filter(|task| self.unconfirmed_tasks.contains(task.tool.call_id.as_str()))
            .filter_map(|task| {
                task.child_id
                    .as_ref()
                    .map(|child_id| (task.tool.call_id.clone(), child_id.clone()))
            })
            .collect()
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
    /// window, because a caller that owns a call identity (the seed's live
    /// set, spec #561) must still resolve its current status and output once
    /// the message it came from has gone stale. `None` when the read carries
    /// no such part; call ids are unique, so the first match is the call. The
    /// one join the live set's per-read reconciliation reads.
    pub fn tool_call(&self, call_id: &str) -> Option<&ToolCall> {
        self.messages
            .iter()
            .flat_map(|message| &message.parts)
            .find_map(|part| match part {
                Part::Tool(call) if call.identity.call_id == call_id => Some(call),
                _ => None,
            })
    }

    /// The typed position of the part carrying the tool call `call_id` — its
    /// message identity and the part's ordinal in it — joined over the WHOLE
    /// read, like [`Self::tool_call`]. The seed's live-set entries carry it as
    /// their Rendered Cursor source (spec #561, review #569), so a call that
    /// settles after the restart can become the frontier instead of being
    /// re-rendered by a later restart. `None` when the read carries no such
    /// part.
    pub fn tool_call_position(&self, call_id: &str) -> Option<(MessageId, usize)> {
        self.messages.iter().find_map(|message| {
            message
                .parts
                .iter()
                .position(|part| matches!(part, Part::Tool(call) if call.identity.call_id == call_id))
                .map(|index| (message.id.clone(), index))
        })
    }

    /// The newest assistant text/reasoning part strictly before
    /// `(message_pos, part_index)` in this read — its `(message index, part
    /// ordinal)` and its character count. The part a tool-kind Rendered Cursor
    /// frontier's delivered extent belongs to (spec #561, review #569). User
    /// messages carry no renderable content and are skipped.
    pub fn newest_text_before(
        &self,
        message_pos: usize,
        part_index: usize,
    ) -> Option<((usize, usize), usize)> {
        for pos in (0..=message_pos).rev() {
            let message = &self.messages[pos];
            if message.role != MessageRole::Assistant {
                continue;
            }
            let limit = if pos == message_pos {
                part_index
            } else {
                message.parts.len()
            };
            for index in (0..limit).rev() {
                match &message.parts[index] {
                    Part::Text(text) => return Some(((pos, index), text.text.chars().count())),
                    Part::Reasoning(reasoning) => {
                        return Some(((pos, index), reasoning.text.chars().count()));
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// The `running`/`pending` tool calls of the Turn anchored at `anchor` —
    /// the orphaned Turn's projection, membership rule included: only calls
    /// whose message [`belongs_to_turn`] may enter a successor's seed as its
    /// live-set fallback (spec #561; ADR-0068's carry retires into it for a
    /// cursorless record), so an older card's stale `running` part is never
    /// resurrected. In transcript order.
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
            if !belongs_to_turn(message, anchor) {
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

    /// The skills the Turn `anchor` scopes attached, gathered from EVERY user
    /// message that belongs to it (spec #652, ticket #655): the anchor's own
    /// message AND any **Supplement** merged into the running turn. Membership
    /// is the same [`belongs_to_turn`] rule the render and ledger use, so a
    /// user message outside the Turn's span contributes nothing; the order is
    /// transcript order (the anchor first, then its Supplements). Empty for a
    /// generation whose user messages record no skill attachment (V1).
    pub fn turn_user_skills(&self, anchor: &TurnAnchor) -> Vec<MessageSkill> {
        self.messages
            .iter()
            .filter(|message| message.role == MessageRole::User && belongs_to_turn(message, anchor))
            .flat_map(|message| message.skills.iter().cloned())
            .collect()
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
                    skills: message.skills.clone(),
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

/// Whether an assistant message belongs to the Turn `anchor` scopes: the
/// membership rule proper. A message still in flight (no completion stamp)
/// belongs when it was created within the Turn, or while its newest server
/// activity is recent enough — the previous run may still be streaming when
/// this Turn's user message lands — while an orphaned one (no activity for
/// [`IN_FLIGHT_STALE_AFTER_MS`]) stops belonging, so a run the server was
/// killed in cannot replay its parts into every later Turn. A completed
/// message belongs only when it was created within the Turn or was still
/// being produced as the Turn began.
///
/// The arms are [`TurnAnchor::may_still_belong`]'s; the wrapper adds the ONE
/// input the two readings take differently: a message with no server time
/// cannot be placed, so it never BELONGS to a projection — while it is no
/// evidence about older messages either, so `may_still_belong` answers `true`
/// for it. The `time.is_some()` guard is therefore the membership rule's
/// alone, and it lives here, once.
fn belongs_to_turn(message: &TranscriptMessage, anchor: &TurnAnchor) -> bool {
    message.time.is_some() && anchor.may_still_belong(message)
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
    /// The skills this message attached (spec #652, ticket #655): the neutral
    /// `{ id, name, instructions }` facts a user message's skill payload
    /// carries, so the loaded-skill fold can render them. Empty for every
    /// assistant message and on a generation that records none (V1).
    pub skills: Vec<MessageSkill>,
    pub parts: Vec<Part>,
}

/// One skill a message attached (spec #652, ticket #655): the neutral
/// `{ id, name, instructions }` facts decoded from the generation's own skill
/// payload. `instructions` is the skill's prepared body as the server injected
/// it (V2's `<skill_content>` envelope text); `None` when the payload attached
/// the skill by identity alone, and empty for V1, whose message envelope
/// carries no skill attachment at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSkill {
    pub id: String,
    pub name: String,
    pub instructions: Option<String>,
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
    /// nothing. `pub` for the membership tests, which must read exactly the
    /// rule [`TurnAnchor::may_still_belong`] applies.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAnchor {
    pub message_id: MessageId,
    pub created_ms: i64,
}

impl TurnAnchor {
    /// The oldest server activity an UNFINISHED message (no completion stamp)
    /// may carry and still belong to this Turn's projection (see
    /// [`TurnAnchor::may_still_belong`]): the anchor minus the in-flight
    /// window. A completed message is measured against `created_ms` alone.
    pub fn in_flight_boundary_ms(&self) -> i64 {
        self.created_ms.saturating_sub(IN_FLIGHT_STALE_AFTER_MS)
    }

    /// Whether `message` may STILL belong to this Turn's projection: the
    /// membership rule's arms and nothing else — a message created at/after
    /// the anchor may always belong; a completed one may when it completed
    /// at/after the anchor; an unfinished one may while its newest activity is
    /// inside the in-flight window ([`Self::in_flight_boundary_ms`]).
    ///
    /// Deliberately DISTINCT from [`belongs_to_turn`], which the projection
    /// uses, on the one input they read differently: a message with no server
    /// time cannot be placed, so it never belongs there — but it is no
    /// evidence about older messages either, so this answers `true` for it.
    /// [`belongs_to_turn`] owns the `time.is_some()` guard.
    pub fn may_still_belong(&self, message: &TranscriptMessage) -> bool {
        let Some(time) = message.time else { return true };
        if time.created >= self.created_ms {
            return true;
        }
        match time.completed {
            Some(completed) => completed >= self.created_ms,
            None => message.newest_activity_ms(time.created) >= self.in_flight_boundary_ms(),
        }
    }
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

impl WakeSource {
    /// Whether this Wake is a genuine resumption (spec #602): the Backend
    /// resumed work the Turn had left — a finished background shell or
    /// subagent, a server restart, an interrupted response. Only these may
    /// open a Wake continuation Card; a source this build cannot classify (an
    /// `Other(_)` marker, or [`Self::Unknown`] on a payload that named none)
    /// never does, so a Wake-less content diff and a mislabelled event both
    /// stay off the 「已恢复执行」 path.
    pub fn is_genuine_resumption(&self) -> bool {
        matches!(
            self,
            Self::Shell | Self::Subagent | Self::Restart | Self::Interrupt
        )
    }
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

/// The bytes one shell output window reads (spec #588, ticket #592): the
/// server's own tail idiom asks for the record's last slice, so the window is
/// bounded whatever the command printed. Four KiB matches the completion
/// entry's own tail bound (#593).
pub const SHELL_OUTPUT_WINDOW_BYTES: usize = 4 * 1024;

/// The lines one shell output window shows (spec #588, ticket #592): the last
/// ones of the byte window, so "it is actually alive" reads at a glance. The
/// card's copy names the actual count when the window clipped.
pub const SHELL_OUTPUT_WINDOW_LINES: usize = 15;

/// One live shell's captured output window as an output read established it
/// (spec #588, ticket #592): the record's last bounded bytes, decoded and
/// clipped to the last bounded lines. The Bridge renders it under the shell's
/// ledger row; the completion entry reuses the same read for its fold body
/// (#593). Display-only: it never prompts, retires or settles anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutputWindow {
    /// The window's text: the last [`SHELL_OUTPUT_WINDOW_LINES`] lines of the
    /// record's last [`SHELL_OUTPUT_WINDOW_BYTES`] bytes, decoded UTF-8, with
    /// no trailing newline. Empty when the record exists but captured nothing
    /// (a successful read with nothing to show): a readable-empty window that
    /// the row and the entry omit, never an error and never 「输出已不可用」.
    pub text: String,
    /// Whether the record held more output than the window shows — the byte
    /// slice clipped it, or whole lines were dropped at its head. The card
    /// appends 「仅最后 N 行 · 已截断」; `false` means the window is the whole
    /// capture.
    pub clipped: bool,
    /// When the window was captured (cola's clock, ms): the 「截至于 HH:MM」
    /// label's own clock, stamped at the read that established it.
    pub captured_ms: i64,
}

/// One shell's output as a shared reconcile read established it (spec #588,
/// #592), keyed by shell id on the read: the window the row renders, or the
/// read spent with nothing to show. An absent key means no output read was
/// spent this cycle (the shared throttle), so a previously rendered window
/// stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellOutputRead {
    /// The record answered: output to show, or a readable-empty capture (an
    /// empty window) — both are successes, and [`Self::window`] tells apart
    /// what renders.
    Window(ShellOutputWindow),
    /// The read was spent and answered no record at all — a failed read or a
    /// vanished record. The window is omitted entirely, never rendered as an
    /// empty panel; the completion entry's own read says 「输出已不可用」 for
    /// this outcome (and only for it).
    Unavailable,
}

impl ShellOutputRead {
    /// The window this read renders, when it established one: `None` for an
    /// empty capture and for an unavailable record — the row omits both —
    /// while the completion entry's own read keeps the two apart: an empty
    /// capture leaves the entry identity-only and only a vanished or
    /// unreadable record says 「输出已不可用」 (spec #588, review).
    pub fn window(&self) -> Option<&ShellOutputWindow> {
        match self {
            Self::Window(window) if !window.text.is_empty() => Some(window),
            _ => None,
        }
    }
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

/// The evidence one child session's newest assistant message carries about its
/// Background Task's run (#591, issue #464): the shared reconcile step's
/// second, lighter read — one page, newest assistant first — spent on a
/// suspect the runtime no longer reports active. Positive evidence only: a
/// terminal step finish with the message's completion stamp, or a child
/// session the server no longer knows; anything else is no evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildEvidence {
    /// The newest assistant message finished its run with a terminal step
    /// finish and the server's completion stamp: the retirement's clock.
    Terminal { completed_at: i64 },
    /// The child session does not exist at the attached server (a 404 read):
    /// the run cannot still be hosted there. The retirement is identity-only,
    /// like a shell the runtime has no record of.
    Gone,
    /// No evidence: the newest assistant message is not terminal (or no
    /// assistant message was read at all). The task stays exactly as the
    /// runtime marked it — never guessed dead.
    Unfinished,
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

/// What took a task out of the live list, as its [`TaskRetirement`] records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRetirementEnding {
    /// The runtime reported a terminal end for the shell.
    Ended(ShellEnd),
    /// The runtime has no record of the task.
    Lost,
    /// The child session's own newest assistant message reported a terminal
    /// run (#591, issue #464): the runtime no longer lists the child active,
    /// and the child transcript's terminal step finish (with its completion
    /// stamp) ends the task without a Wake. Only a subagent carries it; the
    /// ledger renders it as the runtime's own 结束 ending.
    ChildEnded,
    /// The user's cleanup click retired the task (spec #588, ticket #590):
    /// never produced by a read, only by
    /// [`SessionTranscript::apply_cleanup`], and recorded in the process-local
    /// overlay like every other retirement.
    Cleaned,
}

/// The process-local overlay of the Background Task facts a **Runtime
/// Reconciliation** established this cola life (issue #454): the tasks the
/// runtime confirmed ended or the user cleared (spec #588, #590), and the live
/// tasks the runtime could not confirm as running (the `⚠️ 状态待确认`
/// markers). The transcript's launch record never flips, so without the first
/// every re-read would resurrect a task the runtime already retired — a live
/// turn's ledger would show it again, and the next Turn would yield waiting on
/// it, forever. And the marker state is derived per read
/// ([`SessionTranscript::apply_task_runtime`]) from a verdict the shared
/// throttle rations, so without the second any read the runtime was not asked
/// about — a Session Sync refresh, a drain tick, a follow — would drop the
/// marker (and the cleanup button it gates) with no evidence the child is
/// running (review, spec #588).
///
/// The Bridge records every retirement ([`Self::record`]) and replaces the
/// unconfirmed set ([`Self::set_unconfirmed`]) only after the flush that
/// carried a reconciliation read's entries was accepted — every caller's pass
/// commits then (review, PR #595), so a refused carrier records nothing and
/// the task stays live; the adapter applies the overlay to every transcript it
/// returns, so no read path can disagree. A marker is re-applied only to a task still live in
/// the read, so a retired task's marker never surfaces as live state. The
/// overlay is deliberately in-memory: a cola restart loses it, the next read
/// re-derives the same retirements and the next verdict the same markers, and
/// the runtime explains the task again. A manual cleanup is no different — it
/// is a cola-life dismissal, re-derived at restart.
#[derive(Default)]
pub struct BackgroundTaskOverlay {
    state: std::sync::Mutex<OverlayState>,
}

#[derive(Default)]
struct OverlayState {
    /// session_id → the call ids the runtime (or a cleanup) retired.
    retired: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// session_id → the live call ids the runtime could not confirm as
    /// running.
    unconfirmed: std::collections::HashMap<String, std::collections::HashSet<String>>,
}

impl BackgroundTaskOverlay {
    /// Record the call ids the runtime retired for one session. Idempotent.
    pub fn record(&self, session_id: &str, call_ids: &[String]) {
        let mut state = self
            .state
            .lock()
            .expect("the task-overlay lock is never poisoned");
        let session = state.retired.entry(session_id.to_string()).or_default();
        for call_id in call_ids {
            session.insert(call_id.clone());
        }
    }

    /// Replace one session's unconfirmed-marker set with exactly `call_ids` —
    /// the reconcile's post-verdict set of live tasks it could not confirm as
    /// running. Replace semantics let a verdict clear a marker: a set without
    /// the id stops riding later reads, while a failed read never calls this
    /// and so clears nothing.
    pub fn set_unconfirmed(&self, session_id: &str, call_ids: &[String]) {
        let mut state = self
            .state
            .lock()
            .expect("the task-overlay lock is never poisoned");
        if call_ids.is_empty() {
            state.unconfirmed.remove(session_id);
            return;
        }
        state
            .unconfirmed
            .insert(session_id.to_string(), call_ids.iter().cloned().collect());
    }

    /// The call ids one session's overlay has recorded as retired, sorted —
    /// the test seam for "a refused refresh records nothing" (spec #588,
    /// review PR #595). Production reads the overlay only through
    /// [`Self::apply`].
    #[cfg(test)]
    pub fn retired_call_ids(&self, session_id: &str) -> Vec<String> {
        Self::sorted_ids(&self.state, |state| state.retired.get(session_id))
    }

    /// The call ids one session's overlay carries as unconfirmed, sorted —
    /// [`Self::retired_call_ids`]'s marker-side twin.
    #[cfg(test)]
    pub fn unconfirmed_call_ids(&self, session_id: &str) -> Vec<String> {
        Self::sorted_ids(&self.state, |state| state.unconfirmed.get(session_id))
    }

    /// A session's recorded id set, cloned and sorted for an order-free test
    /// assertion.
    #[cfg(test)]
    fn sorted_ids(
        state: &std::sync::Mutex<OverlayState>,
        pick: impl Fn(&OverlayState) -> Option<&std::collections::HashSet<String>>,
    ) -> Vec<String> {
        let state = state.lock().expect("the task-overlay lock is never poisoned");
        let mut ids: Vec<String> = pick(&state)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// Apply the overlay to one read: every recorded retirement leaves
    /// `transcript`'s live list, and every recorded unconfirmed marker is
    /// re-inserted for a task still live in it. A session with nothing
    /// recorded is untouched (the common case does no set lookup per task).
    pub fn apply(&self, session_id: &str, transcript: &mut SessionTranscript) {
        let state = self
            .state
            .lock()
            .expect("the task-overlay lock is never poisoned");
        if let Some(ids) = state.retired.get(session_id) {
            transcript
                .background_tasks
                .retain(|task| !ids.contains(task.tool.call_id.as_str()));
        }
        let Some(ids) = state.unconfirmed.get(session_id) else {
            return;
        };
        for task in &transcript.background_tasks {
            if ids.contains(task.tool.call_id.as_str()) {
                transcript.unconfirmed_tasks.insert(task.tool.call_id.clone());
            }
        }
    }
}

/// One recent-conversation tail entry: a text-bearing user/assistant message's
/// role, created time and verbatim text.
#[derive(Debug, Clone, PartialEq)]
pub struct TailEntry {
    pub role: MessageRole,
    pub created_ms: i64,
    pub text: String,
    /// The skills the message attached (spec #652, ticket #655), rendered as
    /// the `🧩 已加载技能` folds beside the text. Empty for assistant messages
    /// and on a generation that records none (V1).
    pub skills: Vec<MessageSkill>,
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
            skills: Vec::new(),
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
            transcript.task_retirements,
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
            lost.task_retirements[0].ending,
            TaskRetirementEnding::Lost,
            "a runtime without the record is the Lost ending"
        );
        assert_eq!(lost.task_retirements[0].finished_at, None);
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
        assert!(untouched.task_retirements.is_empty());
        assert_eq!(untouched.settle(Some(&anchor)), TurnSettle::Waiting);
    }

    /// Spec #588 / #590: the manual cleanup ends exactly the named live tasks —
    /// each moves to the retirement list with the Cleaned ending and the click's
    /// own clock, an unnamed live task stays live, and the settle rule follows
    /// (the last task's cleanup is the true end). An id that is not live invents
    /// no retirement.
    #[test]
    fn a_manual_cleanup_retires_only_the_named_live_tasks() {
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
        let mut transcript = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![
                shell_task("call_clear", "sh_clear"),
                shell_task("call_keep", "sh_keep"),
            ]);
        transcript.unconfirmed_tasks.insert("call_clear".into());
        transcript.unconfirmed_tasks.insert("call_keep".into());

        transcript.apply_cleanup(&["call_clear".to_string()], 3_000);

        assert_eq!(
            transcript
                .background_tasks
                .iter()
                .map(|task| task.tool.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["call_keep"],
            "only the named task left the live list"
        );
        assert_eq!(
            transcript.task_retirements,
            vec![TaskRetirement {
                task: shell_task("call_clear", "sh_clear"),
                ending: TaskRetirementEnding::Cleaned,
                finished_at: Some(3_000),
            }],
            "the cleaned task keeps its identity, the click's clock, and the manual ending"
        );
        assert!(
            !transcript.unconfirmed_tasks.contains("call_clear"),
            "the cleared task leaves the unconfirmed set too"
        );
        assert!(
            transcript.unconfirmed_tasks.contains("call_keep"),
            "an unnamed id stays exactly as the read left it"
        );
        assert_eq!(
            transcript.settle(Some(&anchor)),
            TurnSettle::Waiting,
            "a live task still holds the wait"
        );

        // An id with no live task invents no retirement.
        transcript.apply_cleanup(&["call_absent".to_string()], 4_000);
        assert_eq!(transcript.task_retirements.len(), 1);

        // The last live task's cleanup is the true end.
        transcript.apply_cleanup(&["call_keep".to_string()], 4_000);
        assert!(transcript.background_tasks.is_empty());
        assert_eq!(transcript.task_retirements.len(), 2);
        assert_eq!(transcript.settle(Some(&anchor)), TurnSettle::Complete);
    }

    /// Spec #588 / #591: a subagent the runtime reports inactive is retirable
    /// on its own child session's evidence — a terminal finish retires it with
    /// the child's own completion clock (the ChildEnded ending), a gone child
    /// session retires it as Lost with no clock, and any other evidence leaves
    /// the row exactly as the runtime marked it. Positive evidence only: the
    /// settle follows (the last task's evidence is the true end), and an id
    /// with no live task invents no retirement.
    #[test]
    fn child_evidence_retires_a_suspect_on_terminal_or_gone_and_keeps_the_rest() {
        let (_, anchor) = anchored("msg_u1", 1_000);
        let child_task = |call_id: &str, child_id: &str| BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: call_id.into(),
            },
            shell_id: None,
            child_id: Some(child_id.into()),
            started_at: Some(1_100),
        };

        // The runtime marked the child inactive; its own terminal message ends
        // it with the message's completion clock.
        let mut terminal = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![child_task("call_sub", "ses_child")]);
        terminal.unconfirmed_tasks.insert("call_sub".into());
        terminal.apply_child_evidence("call_sub", ChildEvidence::Terminal { completed_at: 2_000 });
        assert!(
            terminal.background_tasks.is_empty(),
            "a terminal child leaves the live list"
        );
        assert!(
            !terminal.unconfirmed_tasks.contains("call_sub"),
            "a retired suspect leaves the unconfirmed set"
        );
        assert_eq!(
            terminal.task_retirements,
            vec![TaskRetirement {
                task: child_task("call_sub", "ses_child"),
                ending: TaskRetirementEnding::ChildEnded,
                finished_at: Some(2_000),
            }],
            "the child's own ending carries its completion clock"
        );
        assert_eq!(
            terminal.settle(Some(&anchor)),
            TurnSettle::Complete,
            "the last task's evidence is the true end"
        );

        // A child session that is gone retires as Lost, identity-only.
        let mut gone = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![child_task("call_sub", "ses_child")]);
        gone.unconfirmed_tasks.insert("call_sub".into());
        gone.apply_child_evidence("call_sub", ChildEvidence::Gone);
        assert_eq!(
            gone.task_retirements,
            vec![TaskRetirement {
                task: child_task("call_sub", "ses_child"),
                ending: TaskRetirementEnding::Lost,
                finished_at: None,
            }],
            "a gone child is the Lost ending with no invented clock"
        );
        assert_eq!(gone.settle(Some(&anchor)), TurnSettle::Complete);

        // No evidence (a non-terminal read) leaves the task exactly as the
        // runtime marked it.
        let mut unfinished = SessionTranscript::new(vec![assistant_step("msg_a1", 1_100, None)])
            .with_executions(vec![boundary(1_200)])
            .with_background_tasks(vec![child_task("call_sub", "ses_child")]);
        unfinished.unconfirmed_tasks.insert("call_sub".into());
        unfinished.apply_child_evidence("call_sub", ChildEvidence::Unfinished);
        assert_eq!(unfinished.background_tasks.len(), 1);
        assert!(unfinished.unconfirmed_tasks.contains("call_sub"));
        assert!(
            unfinished.task_retirements.is_empty(),
            "nothing settles on no evidence"
        );
        assert_eq!(unfinished.settle(Some(&anchor)), TurnSettle::Waiting);

        // An id with no live task invents no retirement.
        unfinished.apply_child_evidence("call_absent", ChildEvidence::Terminal { completed_at: 3_000 });
        assert_eq!(unfinished.background_tasks.len(), 1);
        assert!(unfinished.task_retirements.is_empty());
    }

    /// Spec #588 / #591: the suspects one reconcile cycle may read — only the
    /// live children the runtime marked unconfirmed; a running child, a shell,
    /// and an unmarked child are never suspects.
    #[test]
    fn unconfirmed_children_names_only_the_marked_live_children() {
        let child = |call_id: &str, child_id: &str| BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: call_id.into(),
            },
            shell_id: None,
            child_id: Some(child_id.into()),
            started_at: Some(1_100),
        };
        let task = child("call_sub", "ses_child");
        let mut transcript = SessionTranscript::new(vec![]).with_background_tasks(vec![
            child("call_run", "ses_run"),
            task.clone(),
            background_shell(1_100),
        ]);
        assert!(
            transcript.unconfirmed_children().is_empty(),
            "no marker means no suspect"
        );

        transcript.unconfirmed_tasks.insert("call_sub".into());
        assert_eq!(
            transcript.unconfirmed_children(),
            vec![("call_sub".to_string(), "ses_child".to_string())],
            "only the marked live child is a suspect"
        );
    }

    /// The process-local Background Task overlay's retirement half (issue #454
    /// review): a recorded call id leaves every later read of that session's
    /// live list, other sessions and other call ids stay untouched, and the
    /// live list's own facts (the retirement entries) are not what it filters.
    #[test]
    fn recorded_retirements_leave_later_reads_of_the_live_list() {
        let overlay = BackgroundTaskOverlay::default();
        let live = || SessionTranscript::new(vec![]).with_background_tasks(vec![background_shell(1_100)]);
        let mut transcript = live();
        overlay.apply("ses_a", &mut transcript);
        assert_eq!(
            transcript.background_tasks.len(),
            1,
            "nothing recorded yet leaves the read as decoded"
        );

        overlay.record("ses_a", &["call_bg".to_string()]);
        let mut filtered = live();
        overlay.apply("ses_a", &mut filtered);
        assert!(
            filtered.background_tasks.is_empty(),
            "the recorded task leaves the live list"
        );

        let mut other_session = live();
        overlay.apply("ses_b", &mut other_session);
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
        overlay.apply("ses_a", &mut other_id);
        assert_eq!(other_id.background_tasks.len(), 1, "another call id is untouched");
    }

    /// Review (spec #588, #590): the overlay carries the unconfirmed markers
    /// too — a recorded marker is re-applied to every later read's live task,
    /// replacing the set (the reconcile's post-verdict write) is how a verdict
    /// stops it riding reads, and a task the retirement half removed never gets
    /// its marker back: a marker only ever surfaces for live state.
    #[test]
    fn recorded_unconfirmed_markers_ride_later_reads_of_live_tasks_only() {
        let child_task = || BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_sub".into(),
            },
            shell_id: None,
            child_id: Some("ses_child".into()),
            started_at: Some(1_100),
        };
        let overlay = BackgroundTaskOverlay::default();
        let live = || SessionTranscript::new(vec![]).with_background_tasks(vec![child_task()]);

        let mut untouched = live();
        overlay.apply("ses_a", &mut untouched);
        assert!(
            untouched.unconfirmed_tasks.is_empty(),
            "nothing recorded yet marks nothing"
        );

        overlay.set_unconfirmed("ses_a", &["call_sub".to_string()]);
        let mut marked = live();
        overlay.apply("ses_a", &mut marked);
        assert!(
            marked.unconfirmed_tasks.contains("call_sub"),
            "the recorded marker rides every later read"
        );

        let mut other_session = live();
        overlay.apply("ses_b", &mut other_session);
        assert!(
            other_session.unconfirmed_tasks.is_empty(),
            "another session is untouched"
        );

        // The reconcile's replace write is how a verdict resolves a marker.
        overlay.set_unconfirmed("ses_a", &[]);
        let mut resolved = live();
        overlay.apply("ses_a", &mut resolved);
        assert!(
            resolved.unconfirmed_tasks.is_empty(),
            "a resolved verdict's replacement stops the marker riding reads"
        );

        // A retired task is not live state: its marker must not come back.
        overlay.set_unconfirmed("ses_a", &["call_sub".to_string()]);
        overlay.record("ses_a", &["call_sub".to_string()]);
        let mut retired = live();
        overlay.apply("ses_a", &mut retired);
        assert!(
            retired.background_tasks.is_empty(),
            "the retirement removes the task"
        );
        assert!(
            retired.unconfirmed_tasks.is_empty(),
            "a retired task's marker does not survive as live state"
        );
    }

    /// Review (spec #588): a runtime verdict resolves the carried unconfirmed
    /// marker only on evidence — the child RUNNING clears it, INACTIVE
    /// (re)sets it, and a verdict that does not answer for the child keeps
    /// exactly what the read carried.
    #[test]
    fn a_runtime_read_resolves_the_carried_unconfirmed_marker_only_on_evidence() {
        let child_task = || BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_sub".into(),
            },
            shell_id: None,
            child_id: Some("ses_child".into()),
            started_at: Some(1_100),
        };

        // A Running verdict resolves the carried marker; the task stays live.
        let mut running = SessionTranscript::new(vec![]).with_background_tasks(vec![child_task()]);
        running.unconfirmed_tasks.insert("call_sub".into());
        running.apply_task_runtime(&TaskRuntime {
            shells: vec![],
            children: vec![("ses_child".into(), ChildRuntime::Running)],
        });
        assert!(
            running.unconfirmed_tasks.is_empty(),
            "positive evidence resolves the marker"
        );
        assert_eq!(running.background_tasks.len(), 1, "a running child stays live");

        // An Inactive verdict (re)sets it, carried or not.
        let mut inactive = SessionTranscript::new(vec![]).with_background_tasks(vec![child_task()]);
        inactive.apply_task_runtime(&TaskRuntime {
            shells: vec![],
            children: vec![("ses_child".into(), ChildRuntime::Inactive)],
        });
        assert!(inactive.unconfirmed_tasks.contains("call_sub"));
        let mut re_marked = SessionTranscript::new(vec![]).with_background_tasks(vec![child_task()]);
        re_marked.unconfirmed_tasks.insert("call_sub".into());
        re_marked.apply_task_runtime(&TaskRuntime {
            shells: vec![],
            children: vec![("ses_child".into(), ChildRuntime::Inactive)],
        });
        assert!(
            re_marked.unconfirmed_tasks.contains("call_sub"),
            "an inactive child keeps its marker"
        );

        // A verdict that does not answer for the child keeps the carried
        // marker; a shell verdict elsewhere does not touch it either.
        let mut unanswered = SessionTranscript::new(vec![]).with_background_tasks(vec![child_task()]);
        unanswered.unconfirmed_tasks.insert("call_sub".into());
        unanswered.apply_task_runtime(&TaskRuntime {
            shells: vec![("sh_other".into(), ShellRuntime::Running)],
            children: vec![],
        });
        assert!(
            unanswered.unconfirmed_tasks.contains("call_sub"),
            "a verdict with no answer for the child changes nothing"
        );
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

    /// The tail copies each message's attached skills into its entry (spec #652,
    /// ticket #655), so the snapshot's loaded-skill fold reads them through this
    /// public projection — not a hand-built entry. An assistant message carries
    /// none.
    #[test]
    fn tail_copies_each_messages_attached_skills() {
        let mut with_skills = message(
            "u1",
            MessageRole::User,
            Some(MessageTime {
                created: 1_000,
                completed: Some(1_000),
            }),
            vec![text_part("看一下")],
        );
        with_skills.skills = vec![MessageSkill {
            id: "implement-spec".into(),
            name: "implement-spec".into(),
            instructions: Some("body".into()),
        }];
        let assistant = message(
            "a1",
            MessageRole::Assistant,
            Some(MessageTime {
                created: 2_000,
                completed: Some(2_000),
            }),
            vec![text_part("回答")],
        );

        let tail = SessionTranscript::new(vec![with_skills, assistant]).transcript_tail();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].skills.len(), 1, "{:?}", tail[0].skills);
        assert_eq!(tail[0].skills[0].id, "implement-spec");
        assert_eq!(tail[0].skills[0].name, "implement-spec");
        assert_eq!(tail[0].skills[0].instructions.as_deref(), Some("body"));
        assert!(tail[1].skills.is_empty(), "an assistant message carries none");
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
