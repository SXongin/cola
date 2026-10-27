//! The adapter-internal transcript projection (spec #860, ticket #879).
//!
//! ADR-0090's consequence is the shape of this module: cola's `Backend` is
//! poll-shaped, `codex app-server` is push-shaped. The adapter owns the
//! subscription ([`super::backend`]) and folds every `turn/*`, `item/*` and
//! usage notification into one [`ThreadProjection`] per thread; a
//! [`Backend::transcript`](crate::backend::Backend::transcript) read is then a
//! pure, non-blocking projection of that cache into the neutral
//! [`SessionTranscript`]. The wire names (`item/agentMessage/delta`,
//! `turn/diff/updated`, …) never leave this layer (and its subscriber).
//!
//! Three rules make the projection safe to read while a turn is running:
//!
//! - **Item ids are the server's.** Every modelled item type carries an `id` on
//!   the wire; the projection keys on it and never synthesises one. An item the
//!   server did not id is not projected at all, rather than invented.
//! - **Replay is idempotent.** `item/started` rebuilds a live item from the
//!   server's start payload, deltas append, and `item/completed` replaces the
//!   accumulated fields with the authoritative final item. Applying the same
//!   recorded sequence twice therefore yields the same transcript — the second
//!   pass resets each item at its start before re-applying its deltas.
//! - **A gap is stated, never hidden.** [`ThreadProjection::mark_incomplete`]
//!   (the subscription's `Lagged` signal) sets [`SessionTranscript::truncated`]
//!   until a `thread/read` resync rebuilds the projection from persisted turns.
//!
//! The read model is deliberately limited to what the card/session tickets
//! (#880/#881) consume. Approval DTOs are decision 7 of ADR-0090 and belong to
//! #882.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::backend::{
    Compaction, CompactionReason, CompactionStatus, ContentBlock, FileContent, FinishReason, MessageId,
    MessageRole, MessageTime, ModelIdentity, OtherPart, Part, ReasoningPart, SessionTranscript, StepFinish,
    TextPart, TokenUsage, ToolCall, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage, TurnDiff,
    TurnDiffFile,
};

/// `item/started`: the server's opening payload for one item.
const ITEM_STARTED: &str = "item/started";
/// `item/completed`: the server's authoritative final payload for one item.
const ITEM_COMPLETED: &str = "item/completed";
/// Streaming deltas, in the order the adapter applies them.
const AGENT_DELTA: &str = "item/agentMessage/delta";
const REASONING_SUMMARY_DELTA: &str = "item/reasoning/summaryTextDelta";
const REASONING_TEXT_DELTA: &str = "item/reasoning/textDelta";
const COMMAND_OUTPUT_DELTA: &str = "item/commandExecution/outputDelta";
const FILE_CHANGE_PATCH_UPDATED: &str = "item/fileChange/patchUpdated";
const FILE_CHANGE_OUTPUT_DELTA: &str = "item/fileChange/outputDelta";
const MCP_PROGRESS: &str = "item/mcpToolCall/progress";
const PLAN_DELTA: &str = "item/plan/delta";
const TURN_STARTED: &str = "turn/started";
const TURN_COMPLETED: &str = "turn/completed";
const TURN_DIFF_UPDATED: &str = "turn/diff/updated";
const TOKEN_USAGE_UPDATED: &str = "thread/tokenUsage/updated";
const ERROR_NOTIFICATION: &str = "error";

/// The live-turn state a `thread/status/changed` broadcast reports. The
/// neutral [`crate::backend::types::SessionStatus`] is derived from it at the
/// `Backend` seam; this type keeps the wire's own vocabulary here, where it
/// belongs (ADR-0053).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadStatus {
    /// A turn is running on the thread (`{"type":"active", …}`).
    Active,
    /// No turn is running: `idle`, a thread the server unloaded
    /// (`notLoaded`), or one whose last run ended in a system error. All three
    /// mean "not live" to Session Sync; the run-state detail that matters to
    /// `cola doctor` is #883's.
    Idle,
}

/// Decode one `thread/status/changed` status value, tolerating both the tagged
/// object and a bare string spelling. An unknown type is no evidence — the
/// caller clears the cached status rather than guessing.
pub(crate) fn decode_thread_status(value: &Value) -> Option<ThreadStatus> {
    let kind = value
        .as_str()
        .or_else(|| value.get("type").and_then(Value::as_str))?;
    match kind {
        "active" => Some(ThreadStatus::Active),
        "idle" | "notLoaded" | "systemError" => Some(ThreadStatus::Idle),
        _ => None,
    }
}

/// The small Turn Diff facts a released thread keeps: the diff map and the
/// item→turn mapping a Turn anchor needs, without the transcript. Retaining
/// this is what lets a card's 「查看完整变化」 resolve after the projection is
/// evicted.
#[derive(Debug, Clone, Default)]
pub(crate) struct DiffSnapshot {
    pub(crate) diffs: HashMap<String, TurnDiff>,
    pub(crate) item_turn: HashMap<String, String>,
    pub(crate) completed_turns: std::collections::HashSet<String>,
    pub(crate) latest_turn: Option<String>,
}

impl DiffSnapshot {
    pub(crate) fn is_empty(&self) -> bool {
        self.diffs.is_empty() && self.completed_turns.is_empty() && self.latest_turn.is_none()
    }

    /// Resolve `from` exactly as [`ThreadProjection::diff_for`] does, over the
    /// snapshot's small maps.
    pub(crate) fn resolve(&self, from: Option<&str>) -> Option<TurnDiff> {
        let turn_id = from
            .and_then(|id| self.item_turn.get(id).cloned())
            .or_else(|| {
                from.filter(|id| self.completed_turns.contains(*id) || self.diffs.contains_key(*id))
                    .map(str::to_string)
            })
            .or_else(|| self.latest_turn.clone())?;
        if let Some(diff) = self.diffs.get(&turn_id) {
            return Some(diff.clone());
        }
        self.completed_turns.contains(&turn_id).then(TurnDiff::default)
    }
}

/// One thread's folded notifications. Construct with [`Self::new`]; apply
/// decoded events with [`Self::apply`] and read the neutral view with
/// [`Self::transcript`].
#[derive(Debug, Default)]
pub(crate) struct ThreadProjection {
    thread_id: String,
    /// Items in first-seen order. The server assigns every id; this vector is
    /// the transcript's message order.
    items: Vec<Item>,
    /// Item id → index in [`Self::items`]. The dedupe key the server owns.
    index: HashMap<String, usize>,
    /// Turns in first-seen order, keyed by the server's turn id.
    turns: HashMap<String, Turn>,
    turn_order: Vec<String>,
    /// The latest `turn/diff/updated` per turn.
    diffs: HashMap<String, TurnDiff>,
    /// The latest `thread/tokenUsage/updated` per turn, as the neutral shape.
    usage: HashMap<String, TokenUsage>,
    /// `tokenUsage.modelContextWindow`, once one arrived.
    context_window: Option<i64>,
    /// The turn between `turn/started` and `turn/completed`, if any.
    live_turn: Option<String>,
    /// The last explicit run state: a `thread/status/changed` broadcast (which
    /// arrives on this connection whether the thread is subscribed or not) or
    /// a turn boundary folded from the subscription. `None` means no status
    /// has been observed, so the read falls back to [`Self::is_live`].
    status: Option<ThreadStatus>,
    /// The receipt sequence of the event [`Self::status`] came from. The status
    /// watcher and the thread subscriber are different tasks, so one order
    /// (assigned in the connection's reader loop) decides which status is
    /// newer; a history page records its own boundary here so a status it
    /// supersedes can never write after it.
    status_seq: u64,
    /// The sequence a direct [`Self::apply`] feed (tests and replays) assigns to
    /// its own events when no connection receipt exists.
    #[cfg(test)]
    feed_seq: u64,
    /// Set when a subscription gap means the projection may be missing events;
    /// cleared only by a successful `thread/read` resync.
    incomplete: bool,
    /// A `/compact` this adapter requested, whose `contextCompaction` item has
    /// not arrived yet. The protocol's item carries no reason, so the request
    /// is the only evidence a compaction was manual (`/compact`) rather than
    /// automatic; the next compaction item consumes it.
    manual_compaction: bool,
    /// The model the **thread** runs with, learned from `thread/start`,
    /// `thread/resume`, `thread/read` or cola's own accepted `turn/start`
    /// (issue #904). Codex items carry no model of their own, so this is the
    /// only per-message model fact the adapter has; `None` until one was named.
    model: Option<ModelFact>,
}

/// Where the projection learned the thread's model (issue #904). The two
/// sources age differently: a server **observation** is superseded by a later
/// observation that names nothing, while cola's own accepted pick is not
/// contradicted by a server that simply omits the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSource {
    /// `thread/start`, `thread/resume` or `thread/read` reported it.
    Thread,
    /// cola's own `turn/start` sent it and the server accepted the turn.
    ColaSent,
}

/// The model a thread runs with, and where that fact came from.
#[derive(Debug, Clone)]
struct ModelFact {
    model: ModelIdentity,
    source: ModelSource,
}

impl ThreadProjection {
    pub(crate) fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            ..Self::default()
        }
    }

    /// Whether a turn is currently between `turn/started` and `turn/completed`.
    /// The neutral transcript has no live flag of its own (a message's missing
    /// completion stamp and the session status carry that fact); this
    /// accessor is the adapter's own, for `session_status` and the tests.
    pub(crate) fn is_live(&self) -> bool {
        self.live_turn.is_some()
    }

    /// The live turn a cancelled `/stop` should interrupt. `None` when the
    /// session is idle: there is nothing to interrupt and an id from a
    /// finished turn would be a stale write.
    pub(crate) fn live_turn(&self) -> Option<&str> {
        self.live_turn.as_deref()
    }

    /// Apply what a thread read reported (issue #904): `thread/start`,
    /// `thread/resume` and `thread/read` carry the thread's model, and no item
    /// does. The transcript stamps it onto assistant messages, so the card
    /// footer and the `/model` current-model line can name what actually ran.
    ///
    /// A **reported** model always wins. Silence (`None` — an absent or null
    /// field, indistinguishable on the wire) is not evidence that the model
    /// changed, so it drops a name the server itself gave earlier — an old name
    /// would claim the wrong model answered — but never a model cola sent: a
    /// server that omits the field must not erase what cola just picked.
    pub(crate) fn observe_thread_model(&mut self, model: Option<ModelIdentity>) {
        self.model = match (model, self.model.as_ref().map(|fact| fact.source)) {
            (Some(model), _) => Some(ModelFact {
                model,
                source: ModelSource::Thread,
            }),
            (None, Some(ModelSource::Thread)) => None,
            (None, _) => self.model.take(),
        };
    }

    /// Record the model cola itself asked for on an accepted `turn/start`: the
    /// protocol's model is "for this turn and subsequent turns", so the thread
    /// now runs it (issue #904). A later read that names another model still
    /// wins — a peer's switch is a newer fact.
    pub(crate) fn record_sent_model(&mut self, model: ModelIdentity) {
        self.model = Some(ModelFact {
            model,
            source: ModelSource::ColaSent,
        });
    }

    /// The turn an item belongs to, for the authorship record: Codex assigns
    /// item ids itself, so the turn cola started is the finest authorship
    /// signal the protocol offers.
    pub(crate) fn turn_of_item(&self, item_id: &str) -> Option<&str> {
        self.index
            .get(item_id)
            .map(|index| self.items[*index].turn_id.as_str())
    }

    /// Whether `submission` was the id cola chose for an item. The chosen id
    /// rides the server's user item as `clientId`, so it is the precise
    /// authorship signal before the turn is bound (ADR-0092).
    pub(crate) fn submission_of_item(&self, item_id: &str) -> Option<&str> {
        self.index
            .get(item_id)
            .and_then(|index| self.items[*index].client_id.as_deref())
    }

    /// The last explicit run state, if one was observed.
    pub(crate) fn observed_status(&self) -> Option<ThreadStatus> {
        self.status
    }

    /// Record a `thread/status/changed` broadcast for this thread from a
    /// direct feed (tests): it takes the next local sequence.
    #[cfg(test)]
    pub(crate) fn set_observed_status(&mut self, status: Option<ThreadStatus>) {
        self.feed_seq += 1;
        let seq = self.feed_seq;
        self.set_observed_status_at(status, seq);
    }

    /// Record a `thread/status/changed` broadcast received at `seq`. A status
    /// older than the one already recorded is stale (the watcher and the
    /// subscriber process the same boundary out of order) and is dropped.
    pub(crate) fn set_observed_status_at(&mut self, status: Option<ThreadStatus>, seq: u64) {
        if seq > self.status_seq {
            self.status = status;
            self.status_seq = seq;
        }
    }

    /// Forget the cached status at `seq` — a closed or lagged connection can no
    /// longer vouch for it. A status queued at or below the boundary is stale
    /// too, hence the inclusive comparison.
    pub(crate) fn clear_observed_status_at(&mut self, seq: u64) {
        if seq >= self.status_seq {
            self.status = None;
            self.status_seq = seq;
        }
    }

    /// Record a status derived from a received event, provided it is newer than
    /// the one already recorded.
    fn set_status(&mut self, status: ThreadStatus, seq: u64) {
        if seq > self.status_seq {
            self.status = Some(status);
            self.status_seq = seq;
        }
    }

    /// Mark the projection as having missed events. The subscriber then
    /// resyncs from `thread/read`; until that succeeds a read must not look
    /// complete.
    pub(crate) fn mark_incomplete(&mut self) {
        self.incomplete = true;
    }

    /// Clear a truncation that no longer applies (a thread resolved as
    /// never-run: an empty transcript is complete, not a prefix).
    pub(crate) fn clear_incomplete(&mut self) {
        self.incomplete = false;
    }

    /// Mark a bounded read truncated only while no page has seeded a turn. If
    /// adoption completed between the wait's timeout and this mark, the
    /// projection already has content and must not be called a prefix.
    pub(crate) fn mark_incomplete_if_unseeded(&mut self) {
        if self.turn_order.is_empty() {
            self.incomplete = true;
        }
    }

    /// Fold one notification into the projection. Returns whether the method
    /// was one this read model understands. A direct feed (tests, a replay)
    /// assigns its own monotonically increasing sequence; the live path uses
    /// [`Self::apply_at`] with the connection's receipt order.
    #[cfg(test)]
    pub(crate) fn apply(&mut self, method: &str, params: &Value) -> bool {
        self.feed_seq += 1;
        let seq = self.feed_seq;
        self.apply_at(seq, method, params)
    }

    /// Fold one notification received at `seq` into the projection. Returns
    /// whether the method was one this read model understands.
    ///
    /// A decode failure marks the projection incomplete instead of silently
    /// losing the event: a changed payload shape is visible on the read, never
    /// guessed around.
    pub(crate) fn apply_at(&mut self, seq: u64, method: &str, params: &Value) -> bool {
        if let Some(thread_id) = params.get("threadId").and_then(Value::as_str)
            && thread_id != self.thread_id
        {
            // The dispatcher routes by threadId; a direct feed (tests, a
            // future replay tool) must not pollute another thread's cache.
            return false;
        }
        match method {
            TURN_STARTED => self.decode(params, |this, n: TurnNotification| this.turn_started(n, seq)),
            TURN_COMPLETED => self.decode(params, |this, n: TurnNotification| this.turn_completed(n, seq)),
            ITEM_STARTED => self.decode(params, |this, n: ItemStarted| this.item_started(n)),
            ITEM_COMPLETED => self.decode(params, |this, n: ItemCompleted| this.item_completed(n)),
            AGENT_DELTA => self.decode(params, |this, n: Delta| this.agent_delta(n)),
            REASONING_SUMMARY_DELTA => self.decode(params, |this, n: Delta| this.reasoning_summary_delta(n)),
            REASONING_TEXT_DELTA => self.decode(params, |this, n: Delta| this.reasoning_text_delta(n)),
            COMMAND_OUTPUT_DELTA => self.decode(params, |this, n: Delta| this.command_output_delta(n)),
            FILE_CHANGE_PATCH_UPDATED => {
                self.decode(params, |this, n: PatchUpdated| this.file_change_updated(n))
            }
            FILE_CHANGE_OUTPUT_DELTA => {
                self.decode(params, |this, n: Delta| this.file_change_output_delta(n))
            }
            MCP_PROGRESS => self.decode(params, |this, n: Delta| this.mcp_progress(n)),
            PLAN_DELTA => self.decode(params, |this, n: Delta| this.plan_delta(n)),
            TURN_DIFF_UPDATED => self.decode(params, |this, n: TurnDiffUpdated| this.turn_diff(n)),
            TOKEN_USAGE_UPDATED => self.decode(params, |this, n: TokenUsageUpdated| this.token_usage(n)),
            ERROR_NOTIFICATION => self.decode(params, |this, n: ErrorNotification| this.turn_error(n, seq)),
            _ => false,
        }
    }

    fn decode<T: for<'de> Deserialize<'de>>(
        &mut self,
        params: &Value,
        apply: impl FnOnce(&mut Self, T),
    ) -> bool {
        match serde_json::from_value::<T>(params.clone()) {
            Ok(decoded) => {
                apply(self, decoded);
                true
            }
            Err(error) => {
                tracing::warn!(
                    thread = %self.thread_id,
                    "a codex notification did not decode into the read model: {error}"
                );
                self.incomplete = true;
                false
            }
        }
    }

    /// Rebuild the item and turn projection from persisted `thread/read` turns
    /// (the `Lagged` recovery path). Diff and token-usage facts survive: they
    /// are per-turn notifications that history does not carry.
    pub(crate) fn resync(&mut self, turns: &[Value], boundary_seq: u64) {
        let previous: HashMap<String, Item> =
            self.items.drain(..).map(|item| (item.id.clone(), item)).collect();
        self.index.clear();
        self.turns.clear();
        self.turn_order.clear();
        self.live_turn = None;
        for turn in turns {
            self.seed_turn(turn);
        }
        for item in &mut self.items {
            item.set_pending_from(previous.get(&item.id));
        }
        // The replayed history is the authority on the turns it carries, at the
        // boundary of what this connection had received when it asked for the
        // page: a live turn is active, and no live turn proves a cached active
        // was the ending the gap dropped. A status received after the boundary
        // has a higher sequence and still applies.
        if boundary_seq >= self.status_seq {
            self.status_seq = boundary_seq;
            if self.live_turn.is_some() {
                self.status = Some(ThreadStatus::Active);
            } else if self.status == Some(ThreadStatus::Active) {
                self.status = Some(ThreadStatus::Idle);
            }
        }
        self.incomplete = false;
    }

    /// Seed a fresh projection from `thread/resume`'s turn history. Unlike
    /// [`Self::resync`], this never clears existing items: it runs before the
    /// subscriber's first event on a fresh projection.
    pub(crate) fn seed(&mut self, turns: &[Value], boundary_seq: u64) {
        let previous: std::collections::HashSet<String> =
            self.items.iter().map(|item| item.id.clone()).collect();
        for turn in turns {
            self.seed_turn(turn);
        }
        // The bounded fallback page is a snapshot too: queued deltas it already
        // contains must be consumed rather than appended.
        for item in &mut self.items {
            if !previous.contains(&item.id) {
                item.set_pending_from(None);
            }
        }
        // The page proves a live turn it carries; it cannot prove absence (the
        // resume page is bounded), so no live turn here clears nothing. The
        // boundary still advances, so a status received before this page can
        // never overwrite it once its watcher resumes.
        if boundary_seq >= self.status_seq {
            self.status_seq = boundary_seq;
            if self.live_turn.is_some() {
                self.status = Some(ThreadStatus::Active);
            }
        }
    }

    fn seed_turn(&mut self, turn: &Value) {
        let Some(id) = turn.get("id").and_then(Value::as_str) else {
            return;
        };
        let status = turn
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("inProgress")
            .to_string();
        let started = turn
            .get("startedAt")
            .and_then(Value::as_i64)
            .map(|seconds| seconds.saturating_mul(1000));
        let completed = turn
            .get("completedAt")
            .and_then(Value::as_i64)
            .map(|seconds| seconds.saturating_mul(1000));
        {
            let recorded = self.ensure_turn(id);
            recorded.started_ms = started.or(recorded.started_ms);
            recorded.completed_ms = completed.or(recorded.completed_ms);
            recorded.status = status.clone();
            recorded.error = turn_error(turn);
            recorded.live = status == "inProgress";
        }
        if status == "inProgress" {
            self.live_turn = Some(id.to_string());
        } else if self.live_turn.as_deref() == Some(id) {
            self.live_turn = None;
        }
        if let Some(items) = turn.get("items").and_then(Value::as_array) {
            for item in items {
                if status == "inProgress" {
                    // Only a completed turn's items are authoritative. An
                    // in-progress turn's persisted item is the state at the
                    // resume point; its queued item/started, deltas and
                    // item/completed still refine it. Seeding it as completed
                    // would freeze a statusless tool as finished and make the
                    // later start a no-op.
                    self.seed_live_item(id, item, started);
                } else {
                    self.item_completed(ItemCompleted {
                        thread_id: self.thread_id.clone(),
                        turn_id: id.to_string(),
                        item: item.clone(),
                        completed_at_ms: completed,
                    });
                }
            }
        }
    }

    /// Seed one item from an in-progress turn. Its content is the persisted
    /// snapshot (so the first read is useful), but it is not completed and its
    /// start has not been observed, so the queued lifecycle refines it in place.
    fn seed_live_item(&mut self, turn_id: &str, item: &Value, started_ms: Option<i64>) {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        if self.index.contains_key(id) {
            return;
        }
        let id = id.to_string();
        let index = self.items.len();
        self.items.push(Item {
            id: id.clone(),
            turn_id: turn_id.to_string(),
            kind: ItemKind::from_wire(item),
            started_at_ms: started_ms,
            completed_at_ms: None,
            completed: false,
            started_seen: false,
            pending: None,
            client_id: item_client_id(item),
            manual_compaction: false,
        });
        self.index.insert(id, index);
    }

    /// The neutral read. A pure fold over the cached items: no lock is held
    /// across an await, so a mid-turn read cannot block on the subscription.
    pub(crate) fn transcript(&self) -> SessionTranscript {
        let mut messages: Vec<TranscriptMessage> = self.items.iter().map(|item| self.message(item)).collect();

        // Turn-level facts attach to the turn's NEWEST assistant message: the
        // card footer reads token usage there, and `TurnView` reads a turn's
        // failure and completion from the same place.
        for (index, item) in self.items.iter().enumerate() {
            let Some(turn) = self.turns.get(&item.turn_id) else {
                continue;
            };
            if messages[index].role != MessageRole::Assistant {
                continue;
            }
            let is_newest_in_turn = !self.items[index + 1..]
                .iter()
                .any(|later| later.turn_id == item.turn_id && later.is_assistant());
            if !is_newest_in_turn {
                continue;
            }
            if let Some(usage) = self.usage.get(&item.turn_id) {
                messages[index].tokens = Some(*usage);
            }
            messages[index].error = turn.error.clone();
            if !turn.live {
                let reason = match turn.status.as_str() {
                    "failed" => FinishReason::Error,
                    "interrupted" => FinishReason::Other("interrupted".to_string()),
                    _ => FinishReason::Stop,
                };
                if !messages[index]
                    .parts
                    .iter()
                    .any(|part| matches!(part, Part::StepFinish(_)))
                {
                    messages[index]
                        .parts
                        .push(Part::StepFinish(StepFinish { reason }));
                }
            }
        }

        // A turn that failed before producing any assistant item still owes its
        // failure to the neutral read: the card reads a turn's error from its
        // newest assistant message, so without one the error would vanish and
        // the card would settle with no text. The message is synthetic but
        // stable (the server's turn id, never a fabricated item id), and it is
        // regenerated identically on every read, so the Rendered Cursor cannot
        // see a duplicate. It carries no display content — only the failure and
        // the terminal step finish that ends the turn.
        let assistant_turns: std::collections::HashSet<&str> = self
            .items
            .iter()
            .filter(|item| item.is_assistant())
            .map(|item| item.turn_id.as_str())
            .collect();
        for turn_id in &self.turn_order {
            let Some(turn) = self.turns.get(turn_id) else {
                continue;
            };
            let Some(error) = turn.error.as_ref() else {
                continue;
            };
            if assistant_turns.contains(turn_id.as_str()) {
                continue;
            }
            // Place the failure at the turn's own start (the user message's
            // server time when it has one), so it belongs to the turn's window
            // even when `turn/started` carried no clock.
            let created = self
                .items
                .iter()
                .find(|item| item.turn_id == *turn_id && matches!(item.kind, ItemKind::UserMessage { .. }))
                .and_then(|item| item.started_at_ms)
                .or(turn.started_ms)
                .or(turn.completed_ms)
                .unwrap_or(0);
            messages.push(TranscriptMessage {
                id: MessageId::new(format!("cola:turn-error:{turn_id}")),
                role: MessageRole::Assistant,
                time: Some(MessageTime {
                    created,
                    completed: turn.completed_ms.or(Some(created)),
                }),
                model: None,
                tokens: None,
                error: Some(error.clone()),
                parts: vec![Part::StepFinish(StepFinish {
                    reason: FinishReason::Error,
                })],
            });
        }

        let mut transcript = SessionTranscript::new(messages);
        // The id cola submitted for a user message is not the item id Codex
        // assigned; the server echoes it as `userMessage.clientId`. Hand the
        // pair to the neutral read so the Turn's anchor lookup (by the id cola
        // chose) still finds the message.
        let submissions: HashMap<String, MessageId> = self
            .items
            .iter()
            .filter(|item| matches!(item.kind, ItemKind::UserMessage { .. }))
            .filter_map(|item| {
                item.client_id
                    .as_ref()
                    .map(|client_id| (client_id.clone(), MessageId::new(item.id.clone())))
            })
            .collect();
        transcript = transcript.with_submissions(submissions);
        transcript.truncated = self.incomplete;
        transcript
    }

    /// The cached Turn Diff for the turn `from` names. `from` is the neutral
    /// Turn anchor's user-message id (a Codex item id) or a turn id; `None`
    /// selects the newest turn. A completed turn with no diff notification is
    /// an empty diff — the endpoint exists, it simply had nothing to report —
    /// while an unknown or still-running turn is `None`.
    pub(crate) fn diff_for(&self, from: Option<&str>) -> Option<TurnDiff> {
        let turn_id = from
            .and_then(|id| self.index.get(id).map(|index| self.items[*index].turn_id.clone()))
            .or_else(|| from.filter(|id| self.turns.contains_key(*id)).map(str::to_string))
            .or_else(|| self.turn_order.last().cloned())?;
        if let Some(diff) = self.diffs.get(&turn_id) {
            return Some(diff.clone());
        }
        self.turns
            .get(&turn_id)
            .filter(|turn| !turn.live)
            .map(|_| TurnDiff::default())
    }

    /// The small Turn Diff state retained when the transcript is evicted.
    pub(crate) fn diff_snapshot(&self) -> DiffSnapshot {
        DiffSnapshot {
            diffs: self.diffs.clone(),
            item_turn: self
                .items
                .iter()
                .map(|item| (item.id.clone(), item.turn_id.clone()))
                .collect(),
            completed_turns: self
                .turns
                .iter()
                .filter(|(_, turn)| !turn.live)
                .map(|(id, _)| id.clone())
                .collect(),
            latest_turn: self.turn_order.last().cloned(),
        }
    }

    pub(crate) fn model_context_window(&self) -> Option<i64> {
        self.context_window
    }

    fn message(&self, item: &Item) -> TranscriptMessage {
        let created = item
            .started_at_ms
            .or_else(|| self.turns.get(&item.turn_id).and_then(|turn| turn.started_ms))
            .unwrap_or(0);
        let (role, parts) = item.parts();
        // Codex's items carry no model — the thread does (issue #904), and only
        // an assistant message can honestly name the model that produced it.
        let model = matches!(role, MessageRole::Assistant)
            .then(|| self.model.as_ref().map(|fact| fact.model.clone()))
            .flatten();
        TranscriptMessage {
            id: MessageId::new(item.id.clone()),
            role,
            time: Some(MessageTime {
                created,
                completed: item.completed_at_ms,
            }),
            model,
            tokens: None,
            error: None,
            parts,
        }
    }

    fn ensure_turn(&mut self, id: &str) -> &mut Turn {
        if !self.turns.contains_key(id) {
            self.turn_order.push(id.to_string());
            self.turns.insert(id.to_string(), Turn::default());
        }
        self.turns.get_mut(id).expect("turn inserted above")
    }

    fn item_index(&mut self, id: &str, turn_id: &str, kind: ItemKind) -> usize {
        if let Some(index) = self.index.get(id) {
            return *index;
        }
        let index = self.items.len();
        self.items.push(Item {
            id: id.to_string(),
            turn_id: turn_id.to_string(),
            kind,
            started_at_ms: None,
            completed_at_ms: None,
            completed: false,
            started_seen: false,
            pending: None,
            client_id: None,
            manual_compaction: false,
        });
        self.index.insert(id.to_string(), index);
        index
    }

    fn turn_started(&mut self, notification: TurnNotification, seq: u64) {
        let id = notification.turn.id.clone();
        let started = notification
            .turn
            .started_at
            .map(|seconds| seconds.saturating_mul(1000));
        let status = notification
            .turn
            .status
            .clone()
            .unwrap_or_else(|| "inProgress".to_string());
        let completed = notification
            .turn
            .completed_at
            .map(|seconds| seconds.saturating_mul(1000));
        self.set_status(
            if status == "inProgress" {
                ThreadStatus::Active
            } else {
                ThreadStatus::Idle
            },
            seq,
        );
        {
            let turn = self.ensure_turn(&id);
            turn.started_ms = started.or(turn.started_ms);
            turn.completed_ms = completed.or(turn.completed_ms);
            turn.status = status.clone();
            turn.error = turn_error_value(notification.turn.error.as_ref());
            turn.live = status == "inProgress";
            if turn.live {
                self.live_turn = Some(id.clone());
            }
        }
        if status != "inProgress" {
            if self.live_turn.as_deref() == Some(&id) {
                self.live_turn = None;
            }
            for item in notification.turn.items {
                self.item_completed(ItemCompleted {
                    thread_id: notification.thread_id.clone(),
                    turn_id: id.clone(),
                    item,
                    completed_at_ms: completed,
                });
            }
        }
    }

    fn turn_completed(&mut self, notification: TurnNotification, seq: u64) {
        let id = notification.turn.id.clone();
        let completed = notification
            .turn
            .completed_at
            .map(|seconds| seconds.saturating_mul(1000));
        let status = notification
            .turn
            .status
            .clone()
            .unwrap_or_else(|| "completed".to_string());
        self.set_status(ThreadStatus::Idle, seq);
        {
            let turn = self.ensure_turn(&id);
            turn.completed_ms = completed.or(turn.completed_ms);
            turn.status = status.clone();
            turn.error = turn_error_value(notification.turn.error.as_ref());
            turn.live = false;
        }
        if self.live_turn.as_deref() == Some(&id) {
            self.live_turn = None;
        }
        for item in notification.turn.items {
            self.item_completed(ItemCompleted {
                thread_id: notification.thread_id.clone(),
                turn_id: id.clone(),
                item,
                completed_at_ms: completed,
            });
        }
    }

    fn turn_error(&mut self, notification: ErrorNotification, seq: u64) {
        // A retrying error leaves the turn live (the server schedules another
        // attempt). Its text must NOT become the turn's failure while retries
        // continue — ADR-0090's credential error is exactly this shape — so
        // only a terminal error is recorded. A terminal one is the turn's
        // ending even when no `turn/completed` follows: settle it here so the
        // card can take its error text instead of hanging on a run that has
        // already failed.
        //
        // Checked before the turn is resolved: a retry settles nothing, and
        // that is equally true of the thread-scoped shape below.
        if notification.will_retry {
            return;
        }
        // A terminal error the server did not scope to a turn belongs to the
        // turn this projection has live — that is the run it abandoned, and
        // settling it is what stops the card hanging on it. With no live turn
        // there is no run to end: a thread-level error settles no turn, and a
        // phantom one must never be invented for it.
        let turn_id = match notification.turn_id {
            Some(turn_id) => turn_id,
            None => match self.live_turn.clone() {
                Some(turn_id) => turn_id,
                None => return,
            },
        };
        let message = notification.error.message;
        if !message.is_empty() {
            self.ensure_turn(&turn_id).error = Some(message);
        }
        {
            let turn = self.ensure_turn(&turn_id);
            turn.live = false;
            turn.status = "failed".to_string();
        }
        if self.live_turn.as_deref() == Some(turn_id.as_str()) {
            self.live_turn = None;
            self.set_status(ThreadStatus::Idle, seq);
        } else if self.live_turn.is_none() {
            // The error names a turn this projection never saw start: the
            // thread is still idle, unless another turn is live (then its own
            // status stands).
            self.set_status(ThreadStatus::Idle, seq);
        }
    }

    fn turn_diff(&mut self, notification: TurnDiffUpdated) {
        let diff = parse_unified_diff(&notification.diff);
        self.diffs.insert(notification.turn_id, diff);
    }

    fn token_usage(&mut self, notification: TokenUsageUpdated) {
        let usage = notification.token_usage;
        self.context_window = usage.model_context_window.or(self.context_window);
        let last = usage.last.neutral();
        let fallback = usage.total.neutral();
        let usage = if last == TokenUsage::default() {
            fallback
        } else {
            last
        };
        self.usage.insert(notification.turn_id, usage);
    }

    fn item_started(&mut self, notification: ItemStarted) {
        let Some(id) = notification.item_id() else {
            // The server did not id this item. Never invent one: the item is
            // not part of the neutral transcript.
            return;
        };
        let id = id.to_string();
        let kind = ItemKind::from_wire(&notification.item);
        let started = notification.started_at_ms;
        if let Some(index) = self.index.get(&id) {
            let index = *index;
            // A completed item is never downgraded by a late start.
            if self.items[index].completed {
                return;
            }
            if self.items[index].started_seen {
                // A replay of the sequence starts here again: rebuild from the
                // start payload so the deltas that follow re-apply without
                // doubling the first pass's text.
                let turn_id = self.items[index].turn_id.clone();
                self.items[index] = Item {
                    id,
                    turn_id,
                    kind,
                    started_at_ms: started,
                    completed_at_ms: None,
                    completed: false,
                    started_seen: true,
                    pending: None,
                    client_id: item_client_id(&notification.item),
                    manual_compaction: false,
                };
            } else {
                // Deltas created this item before its start. The start is
                // replay metadata now: fill fields the deltas did not (command,
                // cwd, status), but never overwrite accumulated text or output,
                // even when the start payload carries a non-empty value.
                self.items[index].kind.merge_start(&notification.item);
                self.items[index].started_at_ms = started.or(self.items[index].started_at_ms);
                self.items[index].started_seen = true;
            }
            self.consume_manual_compaction(index);
            return;
        }
        let index = self.items.len();
        self.items.push(Item {
            id: id.clone(),
            turn_id: notification.turn_id,
            kind,
            started_at_ms: started,
            completed_at_ms: None,
            completed: false,
            started_seen: true,
            pending: None,
            client_id: item_client_id(&notification.item),
            manual_compaction: false,
        });
        self.index.insert(id, index);
        self.consume_manual_compaction(index);
    }

    /// Mark a compaction item as the `/compact` this adapter asked for, and
    /// consume the request: the protocol's item names no reason, so the request
    /// is the only evidence (ADR-0092).
    fn consume_manual_compaction(&mut self, index: usize) {
        if self.manual_compaction && matches!(self.items[index].kind, ItemKind::Compaction) {
            self.items[index].manual_compaction = true;
            self.manual_compaction = false;
        }
    }

    /// Record that cola requested a manual compaction for this thread.
    pub(crate) fn mark_manual_compaction(&mut self) {
        self.manual_compaction = true;
    }

    /// Forget a manual-compaction request that never reached the server.
    pub(crate) fn clear_manual_compaction(&mut self) {
        self.manual_compaction = false;
    }

    /// Whether a `/compact` request is still waiting for its item (test-only:
    /// proves an ambiguous failure keeps the marker).
    #[cfg(test)]
    pub(crate) fn manual_compaction_pending(&self) -> bool {
        self.manual_compaction
    }

    fn item_completed(&mut self, notification: ItemCompleted) {
        let Some(id) = notification.item_id() else {
            return;
        };
        let id = id.to_string();
        let completed = notification.completed_at_ms;
        if let Some(index) = self.index.get(&id) {
            let index = *index;
            self.items[index].complete_with(&notification.item, completed);
            if let Some(client_id) = item_client_id(&notification.item) {
                self.items[index].client_id = Some(client_id);
            }
            self.consume_manual_compaction(index);
            return;
        }
        let index = self.items.len();
        let turn_started = self
            .turns
            .get(&notification.turn_id)
            .and_then(|turn| turn.started_ms);
        self.items.push(Item {
            id: id.clone(),
            turn_id: notification.turn_id,
            kind: ItemKind::from_completed(&notification.item),
            started_at_ms: turn_started,
            completed_at_ms: completed,
            completed: true,
            started_seen: true,
            pending: None,
            client_id: item_client_id(&notification.item),
            manual_compaction: false,
        });
        self.index.insert(id, index);
        self.consume_manual_compaction(index);
    }

    fn agent_delta(&mut self, delta: Delta) {
        let index = self.item_index(
            &delta.item_id,
            &delta.turn_id,
            ItemKind::AgentMessage { text: String::new() },
        );
        self.items[index].push_agent_delta(&delta.delta);
    }

    fn reasoning_summary_delta(&mut self, delta: Delta) {
        let index = self.item_index(&delta.item_id, &delta.turn_id, ItemKind::reasoning());
        self.items[index].push_reasoning_summary_delta(&delta.delta);
    }

    fn reasoning_text_delta(&mut self, delta: Delta) {
        let index = self.item_index(&delta.item_id, &delta.turn_id, ItemKind::reasoning());
        self.items[index].push_reasoning_text_delta(&delta.delta);
    }

    fn command_output_delta(&mut self, delta: Delta) {
        let index = self.item_index(&delta.item_id, &delta.turn_id, ItemKind::command());
        self.items[index].push_command_output_delta(&delta.delta);
    }

    fn file_change_output_delta(&mut self, delta: Delta) {
        let index = self.item_index(&delta.item_id, &delta.turn_id, ItemKind::file_change());
        self.items[index].push_file_output_delta(&delta.delta);
    }

    fn file_change_updated(&mut self, notification: PatchUpdated) {
        let index = self.item_index(
            &notification.item_id,
            &notification.turn_id,
            ItemKind::file_change(),
        );
        if let ItemKind::FileChange { changes, .. } = &mut self.items[index].kind {
            *changes = decode_changes(&Value::Array(notification.changes));
        }
    }

    fn mcp_progress(&mut self, delta: Delta) {
        let index = self.item_index(&delta.item_id, &delta.turn_id, ItemKind::mcp());
        self.items[index].push_mcp_output_delta(&delta.delta);
    }

    fn plan_delta(&mut self, delta: Delta) {
        let index = self.item_index(
            &delta.item_id,
            &delta.turn_id,
            ItemKind::Plan { text: String::new() },
        );
        self.items[index].push_plan_delta(&delta.delta);
    }
}

/// Delta text a page snapshot already contains beyond what the client had
/// applied before the resync. Each field is consumed in order by the queued
/// delta events, so an event the page already reflects is not applied twice.
#[derive(Debug, Clone, Default)]
struct PendingDeltas {
    agent: String,
    reasoning_summary: String,
    reasoning_text: String,
    command_output: String,
    file_output: String,
    mcp_output: String,
    plan: String,
}

impl PendingDeltas {
    fn is_empty(&self) -> bool {
        self.agent.is_empty()
            && self.reasoning_summary.is_empty()
            && self.reasoning_text.is_empty()
            && self.command_output.is_empty()
            && self.file_output.is_empty()
            && self.mcp_output.is_empty()
            && self.plan.is_empty()
    }

    fn agent_slot(&mut self) -> &mut String {
        &mut self.agent
    }
    fn reasoning_summary_slot(&mut self) -> &mut String {
        &mut self.reasoning_summary
    }
    fn reasoning_text_slot(&mut self) -> &mut String {
        &mut self.reasoning_text
    }
    fn command_output_slot(&mut self) -> &mut String {
        &mut self.command_output
    }
    fn file_output_slot(&mut self) -> &mut String {
        &mut self.file_output
    }
    fn mcp_output_slot(&mut self) -> &mut String {
        &mut self.mcp_output
    }
    fn plan_slot(&mut self) -> &mut String {
        &mut self.plan
    }
}

/// The id cola submitted for an item, when the server echoed it
/// (`userMessage.clientId`). Any other item type carries none.
fn item_client_id(item: &Value) -> Option<String> {
    item.get("clientId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// One projected item: the server's identity, its turn, and the typed payload.
#[derive(Debug, Clone)]
struct Item {
    id: String,
    turn_id: String,
    kind: ItemKind,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
    /// Whether `item/completed` has been folded in. The event is authoritative
    /// even when its optional timestamp is missing, so tool state reads this
    /// rather than inferring completion from the clock.
    completed: bool,
    /// Whether `item/started` has already been seen for this item. A delta can
    /// create the entry first (the server emits the start before its deltas,
    /// but a resumed stream is not guaranteed to replay that start), so the
    /// first start must merge into the accumulated deltas; a *replayed* start
    /// resets the item so replaying the same sequence cannot double its text.
    started_seen: bool,
    /// Content the page snapshot already contains beyond what the client had
    /// applied before the resync. Queued delta events that match this suffix
    /// are consumed without appending, so an event the page already reflects
    /// is applied exactly once.
    pending: Option<Box<PendingDeltas>>,
    /// The id cola submitted for this item, when the server echoes it back
    /// (`userMessage.clientId`). It is the only link from cola's chosen id to
    /// the id the server assigned, so the Turn's anchor lookup can answer for
    /// the id cola submitted (ADR-0026, ADR-0092).
    client_id: Option<String>,
    /// Whether this compaction was the one cola requested with `/compact`
    /// (the protocol carries no reason on the item).
    manual_compaction: bool,
}

impl Item {
    /// Record the page's content beyond what the previous projection had
    /// applied. A delta whose text matches the start of one of these suffixes
    /// is already in the page and must be consumed instead of appended.
    fn set_pending_from(&mut self, old: Option<&Item>) {
        let old_agent = old.map_or("", old_agent_text);
        let old_summary = old.map_or("", old_reasoning_summary);
        let old_reasoning = old.map_or("", old_reasoning_text);
        let old_command = old.map_or("", old_command_output);
        let old_file = old.map_or("", old_file_output);
        let old_mcp = old.map_or("", old_mcp_output);
        let old_plan = old.map_or("", old_plan_text);
        let mut pending = PendingDeltas::default();
        match &mut self.kind {
            ItemKind::AgentMessage { text } => {
                pending.agent = text_suffix(text, old_agent);
            }
            ItemKind::Reasoning { summary, text } => {
                pending.reasoning_summary = text_suffix(summary, old_summary);
                pending.reasoning_text = text_suffix(text, old_reasoning);
            }
            ItemKind::Command { output, .. } => {
                pending.command_output = text_suffix(output, old_command);
            }
            ItemKind::FileChange { output, .. } => {
                pending.file_output = text_suffix(output, old_file);
            }
            ItemKind::Mcp { output, .. } => {
                pending.mcp_output = text_suffix(output, old_mcp);
            }
            ItemKind::Plan { text } => {
                pending.plan = text_suffix(text, old_plan);
            }
            _ => {}
        }
        if !pending.is_empty() {
            self.pending = Some(Box::new(pending));
        }
    }

    /// Consume `delta` from one pending suffix, returning whether the page
    /// already contained it. A mismatch means the queued delta is new content;
    /// the page suffix it did not match is dropped (it is already in the text).
    fn consume_pending(&mut self, field: fn(&mut PendingDeltas) -> &mut String, delta: &str) -> bool {
        let Some(pending) = self.pending.as_mut() else {
            return false;
        };
        let slot = field(pending);
        if slot.is_empty() {
            return false;
        }
        let matched = slot.starts_with(delta);
        if matched {
            slot.drain(..delta.len());
        } else {
            slot.clear();
        }
        let empty = pending.is_empty();
        if empty {
            self.pending = None;
        }
        matched
    }

    fn push_agent_delta(&mut self, delta: &str) {
        if self.consume_pending(PendingDeltas::agent_slot, delta) {
            return;
        }
        if let ItemKind::AgentMessage { text } = &mut self.kind {
            text.push_str(delta);
        }
    }

    fn push_reasoning_summary_delta(&mut self, delta: &str) {
        if self.consume_pending(PendingDeltas::reasoning_summary_slot, delta) {
            return;
        }
        if let ItemKind::Reasoning { summary, .. } = &mut self.kind {
            summary.push_str(delta);
        }
    }

    fn push_reasoning_text_delta(&mut self, delta: &str) {
        if self.consume_pending(PendingDeltas::reasoning_text_slot, delta) {
            return;
        }
        if let ItemKind::Reasoning { text, .. } = &mut self.kind {
            text.push_str(delta);
        }
    }

    fn push_command_output_delta(&mut self, delta: &str) {
        if self.consume_pending(PendingDeltas::command_output_slot, delta) {
            return;
        }
        if let ItemKind::Command { output, .. } = &mut self.kind {
            output.push_str(delta);
        }
    }

    fn push_file_output_delta(&mut self, delta: &str) {
        if self.consume_pending(PendingDeltas::file_output_slot, delta) {
            return;
        }
        if let ItemKind::FileChange { output, .. } = &mut self.kind {
            output.push_str(delta);
        }
    }

    fn push_mcp_output_delta(&mut self, delta: &str) {
        if self.consume_pending(PendingDeltas::mcp_output_slot, delta) {
            return;
        }
        if let ItemKind::Mcp { output, .. } = &mut self.kind {
            output.push_str(delta);
        }
    }

    fn push_plan_delta(&mut self, delta: &str) {
        if self.consume_pending(PendingDeltas::plan_slot, delta) {
            return;
        }
        if let ItemKind::Plan { text } = &mut self.kind {
            text.push_str(delta);
        }
    }

    fn is_assistant(&self) -> bool {
        // The neutral role's predicate: a user message and a compaction are
        // the two non-assistant items (a compaction is a session fact).
        !matches!(self.kind, ItemKind::UserMessage { .. } | ItemKind::Compaction)
    }

    /// The tool state of an item whose wire type carries no status field
    /// (`webSearch`, `image`, `contextCompaction`): live until its
    /// `item/completed` arrives, completed only then.
    fn tool_completion_status(&self) -> ToolStatus {
        if self.completed {
            ToolStatus::Completed
        } else {
            ToolStatus::Running
        }
    }

    /// The neutral role and parts of this item. One item becomes one message:
    /// Codex items are already atomic (a command, a message, a reasoning
    /// block), so no part assembly beyond the item's own content is needed.
    fn parts(&self) -> (MessageRole, Vec<Part>) {
        match &self.kind {
            ItemKind::UserMessage { parts } => (MessageRole::User, parts.clone()),
            ItemKind::AgentMessage { text } => (
                MessageRole::Assistant,
                vec![Part::Text(TextPart {
                    text: text.clone(),
                    started_at: self.started_at_ms,
                })],
            ),
            ItemKind::Reasoning { summary, text } => {
                let Some(text) = combine_reasoning(summary, text) else {
                    return (MessageRole::Assistant, Vec::new());
                };
                (
                    MessageRole::Assistant,
                    vec![Part::Reasoning(ReasoningPart {
                        text,
                        started_at: self.started_at_ms,
                    })],
                )
            }
            ItemKind::Command {
                command,
                cwd,
                status,
                output,
                exit_code,
            } => {
                let status = command_status(status, *exit_code);
                let output = tool_output(output, status.is_failure());
                (
                    MessageRole::Assistant,
                    vec![Part::Tool(ToolCall {
                        identity: ToolIdentity {
                            name: codex_tool_id("command_execution"),
                            call_id: self.id.clone(),
                        },
                        status: status.clone(),
                        started_at: self.started_at_ms,
                        input: Some(json!({ "command": command, "cwd": cwd })),
                        metadata: Some(json!({
                            "exitCode": exit_code,
                            "cwd": cwd,
                            "status": status_label(&status),
                        })),
                        output,
                    })],
                )
            }
            ItemKind::FileChange {
                changes,
                status,
                output,
            } => {
                let status = patch_status(status);
                let diff = if output.is_empty() {
                    changes
                        .iter()
                        .map(|change| change.diff.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    output.clone()
                };
                (
                    MessageRole::Assistant,
                    vec![Part::Tool(ToolCall {
                        identity: ToolIdentity {
                            name: codex_tool_id("file_change"),
                            call_id: self.id.clone(),
                        },
                        status: status.clone(),
                        started_at: self.started_at_ms,
                        input: Some(json!({
                            "changes": changes
                                .iter()
                                .map(|change| json!({
                                    "path": change.path,
                                    "kind": change.kind,
                                }))
                                .collect::<Vec<_>>(),
                        })),
                        metadata: Some(json!({
                            "changes": changes
                                .iter()
                                .map(|change| json!({
                                    "path": change.path,
                                    "kind": change.kind,
                                    "diff": change.diff,
                                }))
                                .collect::<Vec<_>>(),
                        })),
                        output: tool_output(&diff, status.is_failure()),
                    })],
                )
            }
            ItemKind::Mcp {
                server,
                tool,
                status,
                arguments,
                result,
                error,
                output,
            } => {
                let status = mcp_status(status);
                let text = mcp_text(result).or_else(|| {
                    if output.is_empty() {
                        None
                    } else {
                        Some(output.clone())
                    }
                });
                (
                    MessageRole::Assistant,
                    vec![Part::Tool(ToolCall {
                        identity: ToolIdentity {
                            name: format!("{server}/{tool}"),
                            call_id: self.id.clone(),
                        },
                        status: status.clone(),
                        started_at: self.started_at_ms,
                        input: Some(arguments.clone()),
                        metadata: Some(json!({ "server": server, "tool": tool })),
                        output: ToolOutput {
                            raw: result.clone(),
                            blocks: text
                                .map(|text| vec![ContentBlock::Text(text)])
                                .unwrap_or_default(),
                            error: error
                                .as_ref()
                                .and_then(|error| error.get("message"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        },
                    })],
                )
            }
            ItemKind::WebSearch {
                query,
                action,
                results,
            } => (
                MessageRole::Assistant,
                vec![Part::Tool(ToolCall {
                    identity: ToolIdentity {
                        name: codex_tool_id("web_search"),
                        call_id: self.id.clone(),
                    },
                    status: self.tool_completion_status(),
                    started_at: self.started_at_ms,
                    input: Some(json!({ "query": query, "action": action })),
                    metadata: Some(json!({ "results": results })),
                    output: ToolOutput {
                        raw: results.clone(),
                        blocks: Vec::new(),
                        error: None,
                    },
                })],
            ),
            ItemKind::Image { path, .. } => (
                MessageRole::Assistant,
                vec![Part::Tool(ToolCall {
                    identity: ToolIdentity {
                        name: codex_tool_id("image"),
                        call_id: self.id.clone(),
                    },
                    status: self.tool_completion_status(),
                    started_at: self.started_at_ms,
                    input: Some(json!({ "path": path })),
                    metadata: Some(json!({ "path": path })),
                    output: ToolOutput::default(),
                })],
            ),
            ItemKind::Plan { text } => (
                MessageRole::Assistant,
                vec![Part::Text(TextPart {
                    text: text.clone(),
                    started_at: self.started_at_ms,
                })],
            ),
            ItemKind::Compaction => (
                // A compaction is a Session-scoped fact, not an assistant
                // answer (ADR-0082): the neutral role says so, so it neither
                // answers a Turn nor takes its newest-assistant error/step
                // finish. The card's compaction walk reads the part itself.
                MessageRole::Other("compaction".into()),
                vec![Part::Compaction(Compaction {
                    status: if self.completed {
                        CompactionStatus::Completed
                    } else {
                        CompactionStatus::Running
                    },
                    reason: if self.manual_compaction {
                        CompactionReason::Manual
                    } else {
                        CompactionReason::Auto
                    },
                    summary: String::new(),
                    usage: None,
                    error: None,
                    recent: String::new(),
                })],
            ),
            ItemKind::Other { kind, raw } => (
                MessageRole::Assistant,
                vec![Part::Other(OtherPart {
                    kind: kind.clone(),
                    raw: raw.clone(),
                })],
            ),
        }
    }

    /// Apply an `item/completed` payload over the accumulated item. The final
    /// payload is authoritative; a delta-filled field it does not carry is
    /// preserved rather than dropped.
    fn complete_with(&mut self, item: &Value, completed_at_ms: Option<i64>) {
        self.completed_at_ms = completed_at_ms.or(self.completed_at_ms);
        self.completed = true;
        self.started_seen = true;
        self.pending = None;
        self.kind.apply_completed(item);
    }
}

impl ItemKind {
    /// Apply an item payload over the accumulated kind. The completed payload
    /// is authoritative, but a field it does not carry is preserved rather
    /// than dropped; `item/started` uses the same rule to merge over deltas
    /// that arrived before the start.
    fn merge_start(&mut self, item: &Value) {
        match self {
            ItemKind::AgentMessage { .. }
            | ItemKind::Reasoning { .. }
            | ItemKind::Plan { .. }
            | ItemKind::Compaction
            | ItemKind::Other { .. }
            | ItemKind::UserMessage { .. } => {}
            ItemKind::Command {
                command,
                cwd,
                status,
                exit_code,
                ..
            } => {
                if let Some(value) = item.get("command").and_then(Value::as_str) {
                    *command = value.to_string();
                }
                if let Some(value) = item.get("cwd").and_then(Value::as_str) {
                    *cwd = value.to_string();
                }
                if let Some(value) = item.get("status").and_then(Value::as_str) {
                    *status = Some(value.to_string());
                }
                if let Some(value) = item.get("exitCode").and_then(Value::as_i64) {
                    *exit_code = Some(value);
                }
            }
            ItemKind::FileChange { status, .. } => {
                if let Some(value) = item.get("status").and_then(Value::as_str) {
                    *status = Some(value.to_string());
                }
            }
            ItemKind::Mcp {
                server,
                tool,
                status,
                arguments,
                ..
            } => {
                if let Some(value) = item.get("server").and_then(Value::as_str) {
                    *server = value.to_string();
                }
                if let Some(value) = item.get("tool").and_then(Value::as_str) {
                    *tool = value.to_string();
                }
                if let Some(value) = item.get("status").and_then(Value::as_str) {
                    *status = Some(value.to_string());
                }
                if let Some(value) = item.get("arguments") {
                    *arguments = value.clone();
                }
            }
            ItemKind::WebSearch { query, action, .. } => {
                if let Some(value) = item.get("query").and_then(Value::as_str) {
                    *query = value.to_string();
                }
                if let Some(value) = item.get("action") {
                    *action = Some(value.clone());
                }
            }
            ItemKind::Image { path } => {
                if path.is_none()
                    && let Some(value) = item
                        .get("path")
                        .or_else(|| item.get("savedPath"))
                        .and_then(Value::as_str)
                {
                    *path = Some(value.to_string());
                }
            }
        }
    }

    fn apply_completed(&mut self, item: &Value) {
        match self {
            ItemKind::AgentMessage { text } => {
                if let Some(final_text) = item.get("text").and_then(Value::as_str)
                    && !final_text.is_empty()
                {
                    *text = final_text.to_string();
                }
            }
            ItemKind::Reasoning { summary, text } => {
                let final_summary = string_array(item.get("summary"));
                let final_text = string_array(item.get("content"));
                if !final_summary.is_empty() {
                    *summary = final_summary;
                }
                if !final_text.is_empty() {
                    *text = final_text;
                }
            }
            ItemKind::Command {
                command,
                cwd,
                status,
                output,
                exit_code,
            } => {
                if let Some(value) = item.get("command").and_then(Value::as_str) {
                    *command = value.to_string();
                }
                if let Some(value) = item.get("cwd").and_then(Value::as_str) {
                    *cwd = value.to_string();
                }
                if let Some(value) = item.get("status").and_then(Value::as_str) {
                    *status = Some(value.to_string());
                }
                if let Some(value) = item.get("aggregatedOutput").and_then(Value::as_str) {
                    *output = value.to_string();
                }
                if let Some(value) = item.get("exitCode").and_then(Value::as_i64) {
                    *exit_code = Some(value);
                }
            }
            ItemKind::FileChange {
                changes,
                status,
                output,
            } => {
                if let Some(value) = item.get("changes") {
                    *changes = decode_changes(value);
                }
                if let Some(value) = item.get("status").and_then(Value::as_str) {
                    *status = Some(value.to_string());
                }
                if let Some(value) = item.get("aggregatedOutput").and_then(Value::as_str) {
                    *output = value.to_string();
                }
            }
            ItemKind::Mcp {
                server,
                tool,
                status,
                arguments,
                result,
                error,
                ..
            } => {
                if let Some(value) = item.get("server").and_then(Value::as_str) {
                    *server = value.to_string();
                }
                if let Some(value) = item.get("tool").and_then(Value::as_str) {
                    *tool = value.to_string();
                }
                if let Some(value) = item.get("status").and_then(Value::as_str) {
                    *status = Some(value.to_string());
                }
                if let Some(value) = item.get("arguments") {
                    *arguments = value.clone();
                }
                if let Some(value) = item.get("result") {
                    *result = Some(value.clone());
                }
                if let Some(value) = item.get("error") {
                    *error = Some(value.clone());
                }
            }
            ItemKind::WebSearch {
                query,
                action,
                results,
            } => {
                if let Some(value) = item.get("query").and_then(Value::as_str) {
                    *query = value.to_string();
                }
                if let Some(value) = item.get("action") {
                    *action = Some(value.clone());
                }
                if let Some(value) = item.get("results") {
                    *results = Some(value.clone());
                }
            }
            ItemKind::Image { path } => {
                if let Some(value) = item
                    .get("path")
                    .or_else(|| item.get("savedPath"))
                    .and_then(Value::as_str)
                {
                    *path = Some(value.to_string());
                }
            }
            ItemKind::Plan { text } => {
                if let Some(value) = item.get("text").and_then(Value::as_str)
                    && !value.is_empty()
                {
                    *text = value.to_string();
                }
            }
            ItemKind::Compaction => {}
            ItemKind::Other { raw, .. } => *raw = item.clone(),
            // A completed user message replaces its parts; the wire carries
            // the full content both times.
            ItemKind::UserMessage { parts } => *parts = user_parts(item),
        }
    }
}

/// The typed payload of one item. Only the item kinds the ticket names are
/// modelled; anything else with a server id is preserved raw.
#[derive(Debug, Clone)]
enum ItemKind {
    UserMessage {
        parts: Vec<Part>,
    },
    AgentMessage {
        text: String,
    },
    Reasoning {
        summary: String,
        text: String,
    },
    Command {
        command: String,
        cwd: String,
        status: Option<String>,
        output: String,
        exit_code: Option<i64>,
    },
    FileChange {
        changes: Vec<FileUpdate>,
        status: Option<String>,
        output: String,
    },
    Mcp {
        server: String,
        tool: String,
        status: Option<String>,
        arguments: Value,
        result: Option<Value>,
        error: Option<Value>,
        output: String,
    },
    WebSearch {
        query: String,
        action: Option<Value>,
        results: Option<Value>,
    },
    Image {
        path: Option<String>,
    },
    Plan {
        text: String,
    },
    Compaction,
    Other {
        kind: String,
        raw: Value,
    },
}

impl ItemKind {
    fn from_wire(item: &Value) -> Self {
        match item.get("type").and_then(Value::as_str) {
            Some("userMessage") => Self::UserMessage {
                parts: user_parts(item),
            },
            Some("agentMessage") => Self::AgentMessage {
                text: item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            Some("reasoning") => Self::Reasoning {
                summary: string_array(item.get("summary")),
                text: string_array(item.get("content")),
            },
            Some("commandExecution") => Self::Command {
                command: item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                cwd: item
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                status: item.get("status").and_then(Value::as_str).map(str::to_string),
                output: item
                    .get("aggregatedOutput")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                exit_code: item.get("exitCode").and_then(Value::as_i64),
            },
            Some("fileChange") => Self::FileChange {
                changes: item.get("changes").map(decode_changes).unwrap_or_default(),
                status: item.get("status").and_then(Value::as_str).map(str::to_string),
                output: item
                    .get("aggregatedOutput")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            Some("mcpToolCall") => Self::Mcp {
                server: item
                    .get("server")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                tool: item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                status: item.get("status").and_then(Value::as_str).map(str::to_string),
                arguments: item.get("arguments").cloned().unwrap_or(Value::Null),
                result: item.get("result").cloned(),
                error: item.get("error").cloned(),
                output: String::new(),
            },
            Some("webSearch") => Self::WebSearch {
                query: item
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                action: item.get("action").cloned(),
                results: item.get("results").cloned(),
            },
            Some("imageView") => Self::Image {
                path: item.get("path").and_then(Value::as_str).map(str::to_string),
            },
            Some("imageGeneration") => Self::Image {
                path: item.get("savedPath").and_then(Value::as_str).map(str::to_string),
            },
            Some("plan") => Self::Plan {
                text: item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            Some("contextCompaction") => Self::Compaction,
            Some(kind) => Self::Other {
                kind: kind.to_string(),
                raw: item.clone(),
            },
            None => Self::Other {
                kind: String::new(),
                raw: item.clone(),
            },
        }
    }

    fn from_completed(item: &Value) -> Self {
        Self::from_wire(item)
    }

    fn reasoning() -> Self {
        Self::Reasoning {
            summary: String::new(),
            text: String::new(),
        }
    }

    fn command() -> Self {
        Self::Command {
            command: String::new(),
            cwd: String::new(),
            status: None,
            output: String::new(),
            exit_code: None,
        }
    }

    fn file_change() -> Self {
        Self::FileChange {
            changes: Vec::new(),
            status: None,
            output: String::new(),
        }
    }

    fn mcp() -> Self {
        Self::Mcp {
            server: String::new(),
            tool: String::new(),
            status: None,
            arguments: Value::Null,
            result: None,
            error: None,
            output: String::new(),
        }
    }
}

/// One file in a `fileChange` item / `patchUpdated` event.
#[derive(Debug, Clone)]
struct FileUpdate {
    path: String,
    kind: Value,
    diff: String,
}

fn decode_changes(value: &Value) -> Vec<FileUpdate> {
    value
        .as_array()
        .map(|changes| {
            changes
                .iter()
                .filter_map(|change| {
                    let path = change.get("path")?.as_str()?.to_string();
                    let diff = change
                        .get("diff")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some(FileUpdate {
                        path,
                        kind: change.get("kind").cloned().unwrap_or(Value::Null),
                        diff,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn text_suffix(new: &str, old: &str) -> String {
    if !old.is_empty() && new.starts_with(old) {
        new[old.len()..].to_string()
    } else {
        new.to_string()
    }
}

fn old_agent_text(item: &Item) -> &str {
    match &item.kind {
        ItemKind::AgentMessage { text } => text,
        _ => "",
    }
}

fn old_reasoning_summary(item: &Item) -> &str {
    match &item.kind {
        ItemKind::Reasoning { summary, .. } => summary,
        _ => "",
    }
}

fn old_reasoning_text(item: &Item) -> &str {
    match &item.kind {
        ItemKind::Reasoning { text, .. } => text,
        _ => "",
    }
}

fn old_command_output(item: &Item) -> &str {
    match &item.kind {
        ItemKind::Command { output, .. } => output,
        _ => "",
    }
}

fn old_file_output(item: &Item) -> &str {
    match &item.kind {
        ItemKind::FileChange { output, .. } => output,
        _ => "",
    }
}

fn old_mcp_output(item: &Item) -> &str {
    match &item.kind {
        ItemKind::Mcp { output, .. } => output,
        _ => "",
    }
}

fn old_plan_text(item: &Item) -> &str {
    match &item.kind {
        ItemKind::Plan { text } => text,
        _ => "",
    }
}

fn string_array(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn combine_reasoning(summary: &str, text: &str) -> Option<String> {
    match (summary.is_empty(), text.is_empty()) {
        (true, true) => None,
        (false, true) => Some(summary.to_string()),
        (true, false) => Some(text.to_string()),
        (false, false) => Some(format!("{summary}\n\n{text}")),
    }
}

fn user_parts(item: &Value) -> Vec<Part> {
    let mut parts = Vec::new();
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return parts;
    };
    for entry in content {
        match entry.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = entry.get("text").and_then(Value::as_str) {
                    parts.push(Part::Text(TextPart {
                        text: text.to_string(),
                        started_at: None,
                    }));
                } else {
                    parts.push(Part::Other(OtherPart {
                        kind: "text".to_string(),
                        raw: entry.clone(),
                    }));
                }
            }
            Some("image") => {
                let url = entry.get("url").and_then(Value::as_str);
                match url.and_then(|url| FileContent::decode(url, None, None)) {
                    Some(content) => parts.push(Part::File(content)),
                    None => parts.push(Part::Other(OtherPart {
                        kind: "image".to_string(),
                        raw: entry.clone(),
                    })),
                }
            }
            Some(kind) => parts.push(Part::Other(OtherPart {
                kind: kind.to_string(),
                raw: entry.clone(),
            })),
            None => parts.push(Part::Other(OtherPart {
                kind: String::new(),
                raw: entry.clone(),
            })),
        }
    }
    parts
}

/// The tool id a Codex item kind carries into the neutral transcript.
///
/// The `codex/` namespace keeps the adapter's own vocabulary (`command_execution`
/// / `file_change` / `web_search` / `image`) from colliding with a third-party
/// tool's id, the way an MCP call is `server/tool` (ADR-0042's amendment). The
/// card maps these ids to its own words; a tool that merely shares the raw item
/// kind is untouched.
fn codex_tool_id(kind: &str) -> String {
    format!("codex/{kind}")
}

fn command_status(status: &Option<String>, exit_code: Option<i64>) -> ToolStatus {
    match status.as_deref() {
        Some("inProgress") => ToolStatus::Running,
        Some("completed") if exit_code.is_some_and(|code| code != 0) => ToolStatus::Error,
        Some("completed") => ToolStatus::Completed,
        Some("failed") => ToolStatus::Error,
        Some("declined") => ToolStatus::Other("declined".to_string()),
        Some(other) => ToolStatus::Other(other.to_string()),
        None => ToolStatus::Unknown,
    }
}

fn patch_status(status: &Option<String>) -> ToolStatus {
    match status.as_deref() {
        Some("inProgress") => ToolStatus::Running,
        Some("completed") => ToolStatus::Completed,
        Some("failed") => ToolStatus::Error,
        Some("declined") => ToolStatus::Other("declined".to_string()),
        Some(other) => ToolStatus::Other(other.to_string()),
        None => ToolStatus::Unknown,
    }
}

fn mcp_status(status: &Option<String>) -> ToolStatus {
    match status.as_deref() {
        Some("inProgress") => ToolStatus::Running,
        Some("completed") => ToolStatus::Completed,
        Some("failed") => ToolStatus::Error,
        Some(other) => ToolStatus::Other(other.to_string()),
        None => ToolStatus::Unknown,
    }
}

fn status_label(status: &ToolStatus) -> String {
    match status {
        ToolStatus::Pending => "pending".to_string(),
        ToolStatus::Running => "inProgress".to_string(),
        ToolStatus::Completed => "completed".to_string(),
        ToolStatus::Error => "failed".to_string(),
        ToolStatus::Other(other) => other.clone(),
        ToolStatus::Unknown => String::new(),
    }
}

fn tool_output(text: &str, failed: bool) -> ToolOutput {
    ToolOutput {
        raw: if text.is_empty() {
            None
        } else {
            Some(Value::String(text.to_string()))
        },
        blocks: if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text(text.to_string())]
        },
        error: failed.then(|| {
            if text.is_empty() {
                "the command failed".to_string()
            } else {
                text.to_string()
            }
        }),
    }
}

fn mcp_text(result: &Option<Value>) -> Option<String> {
    let result = result.as_ref()?;
    let content = result.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter_map(|entry| entry.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn turn_error(turn: &Value) -> Option<String> {
    turn_error_value(turn.get("error"))
}

fn turn_error_value(error: Option<&Value>) -> Option<String> {
    let error = error?.as_object()?;
    error
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Wire payloads. All camelCase; a missing optional field is tolerated, a
// changed required one fails the decode above and marks the read incomplete.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnNotification {
    #[serde(default)]
    #[allow(dead_code)]
    thread_id: String,
    turn: RawTurn,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTurn {
    id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    started_at: Option<i64>,
    #[serde(default)]
    completed_at: Option<i64>,
    #[serde(default)]
    items: Vec<Value>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemStarted {
    #[serde(default)]
    #[allow(dead_code)]
    thread_id: String,
    #[serde(default)]
    turn_id: String,
    item: Value,
    #[serde(default)]
    started_at_ms: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    completed_at_ms: Option<i64>,
}

impl ItemStarted {
    fn item_id(&self) -> Option<&str> {
        self.item.get("id").and_then(Value::as_str)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemCompleted {
    #[serde(default)]
    #[allow(dead_code)]
    thread_id: String,
    #[serde(default)]
    turn_id: String,
    item: Value,
    #[serde(default)]
    completed_at_ms: Option<i64>,
}

impl ItemCompleted {
    fn item_id(&self) -> Option<&str> {
        self.item.get("id").and_then(Value::as_str)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delta {
    #[serde(default)]
    #[allow(dead_code)]
    thread_id: String,
    #[serde(default)]
    turn_id: String,
    item_id: String,
    delta: String,
    #[serde(default)]
    #[allow(dead_code)]
    summary_index: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    content_index: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchUpdated {
    #[serde(default)]
    #[allow(dead_code)]
    thread_id: String,
    #[serde(default)]
    turn_id: String,
    item_id: String,
    #[serde(default)]
    changes: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnDiffUpdated {
    #[serde(default)]
    #[allow(dead_code)]
    thread_id: String,
    turn_id: String,
    diff: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsageUpdated {
    #[serde(default)]
    #[allow(dead_code)]
    thread_id: String,
    turn_id: String,
    token_usage: ThreadTokenUsage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadTokenUsage {
    #[serde(default)]
    total: TokenBreakdown,
    #[serde(default)]
    last: TokenBreakdown,
    #[serde(default)]
    model_context_window: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenBreakdown {
    #[serde(default)]
    total_tokens: i64,
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
    #[serde(default)]
    cache_write_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
}

impl TokenBreakdown {
    fn neutral(&self) -> TokenUsage {
        TokenUsage {
            input: self.input_tokens,
            output: self.output_tokens,
            total: self.total_tokens,
            cache_read: self.cached_input_tokens,
            cache_write: self.cache_write_input_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorNotification {
    #[serde(default)]
    turn_id: Option<String>,
    error: WireError,
    /// Whether the server schedules another attempt. A missing field is read
    /// as terminal (the pinned surface carries it): a malformed error must
    /// never leave a card hanging on a run the server has abandoned.
    #[serde(default)]
    will_retry: bool,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(default)]
    message: String,
}

/// One turn's boundary state.
#[derive(Debug, Default)]
struct Turn {
    started_ms: Option<i64>,
    completed_ms: Option<i64>,
    status: String,
    error: Option<String>,
    live: bool,
}

/// Parse the aggregated unified diff from `turn/diff/updated` into the neutral
/// per-file view. Only the `diff --git` shape Codex emits is split here; a
/// body that is not a git diff is one unnamed file so its patch is never
/// dropped.
pub(crate) fn parse_unified_diff(diff: &str) -> TurnDiff {
    let normalized = diff.replace("\r\n", "\n");
    let mut files = Vec::new();
    let mut current: Option<DiffFile> = None;

    for line in normalized.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                files.push(file.finish());
            }
            current = Some(DiffFile::new(git_diff_path(rest)));
            continue;
        }
        let Some(file) = current.as_mut() else {
            // A bare unified patch (`--- a/x` / `+++ b/x` with no git header):
            // start one file at the first `---`.
            if let Some(path) = line.strip_prefix("--- ") {
                current = Some(DiffFile::new(bare_diff_path(path)));
            }
            continue;
        };
        file.observe(line);
    }
    if let Some(file) = current {
        files.push(file.finish());
    }
    if files.is_empty() && !normalized.trim().is_empty() {
        // Not a git-diff-shaped body: keep it as one entry rather than losing
        // the change report.
        let mut file = DiffFile::new(String::new());
        for line in normalized.lines() {
            file.observe(line);
        }
        files.push(file.finish());
    }
    TurnDiff(files)
}

fn git_diff_path(rest: &str) -> String {
    rest.rsplit_once(" b/")
        .map(|(_, path)| path)
        .unwrap_or(rest)
        .trim_matches('"')
        .to_string()
}

fn bare_diff_path(path: &str) -> String {
    path.trim_start_matches("a/").trim_matches('"').to_string()
}

struct DiffFile {
    path: String,
    patch: Vec<String>,
    additions: usize,
    deletions: usize,
    added: bool,
    deleted: bool,
}

impl DiffFile {
    fn new(path: String) -> Self {
        Self {
            path,
            patch: Vec::new(),
            additions: 0,
            deletions: 0,
            added: false,
            deleted: false,
        }
    }

    fn observe(&mut self, line: &str) {
        if line.starts_with("new file mode") {
            self.added = true;
        } else if line.starts_with("deleted file mode") {
            self.deleted = true;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            self.additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            self.deletions += 1;
        }
        self.patch.push(line.to_string());
    }

    fn finish(self) -> TurnDiffFile {
        let status = if self.added {
            "added"
        } else if self.deleted {
            "deleted"
        } else {
            "modified"
        };
        TurnDiffFile {
            path: self.path,
            patch: self.patch.join("\n").trim_end_matches('\n').to_string(),
            additions: self.additions,
            deletions: self.deletions,
            status: status.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Part, TurnDiffStats};

    /// The recorded fixture: one real-shaped turn covering every item kind the
    /// ticket names, both turn boundaries, a diff and token usage.
    const RECORDING: &str = include_str!("fixtures/turn-recording.json");

    fn recording() -> Vec<Value> {
        serde_json::from_str(RECORDING).expect("the recording is a JSON array")
    }

    fn replay(projection: &mut ThreadProjection, events: &[Value]) {
        for event in events {
            let method = event
                .get("method")
                .and_then(Value::as_str)
                .expect("a recorded method");
            let params = event.get("params").expect("recorded params");
            assert!(projection.apply(method, params), "{method} must apply");
        }
    }

    fn projected() -> ThreadProjection {
        let mut projection = ThreadProjection::new("thread-1");
        replay(&mut projection, &recording());
        projection
    }

    /// Issue #904: Codex items carry no model — the thread does — so the
    /// projection stamps the thread's model onto the assistant messages the
    /// card footer reads. With no thread model the read invents none, and a
    /// user message never names one (it produced nothing).
    #[test]
    fn the_threads_model_stamps_assistant_messages_only() {
        let mut projection = projected();
        assert!(
            projection
                .transcript()
                .messages
                .iter()
                .all(|message| message.model.is_none()),
            "a projection that was told no model must not invent one"
        );

        projection.observe_thread_model(Some(ModelIdentity {
            provider_id: "opencode-go".into(),
            model_id: "gpt-6-luna".into(),
            variant: None,
        }));
        let transcript = projection.transcript();
        let mut assistants = 0;
        for message in &transcript.messages {
            match message.role {
                crate::backend::MessageRole::Assistant => {
                    assistants += 1;
                    let model = message.model.as_ref().expect("the thread's model");
                    assert_eq!(model.model_id, "gpt-6-luna");
                    assert_eq!(model.provider_id, "opencode-go");
                }
                _ => assert!(
                    message.model.is_none(),
                    "only an assistant message names the model that produced it: {message:?}"
                ),
            }
        }
        assert!(assistants > 0, "the recording carries assistant messages");
    }

    /// Issue #904, review round 2: what silence means depends on where the fact
    /// came from. A name the **server** gave is dropped when a later read names
    /// none (a stale name would claim the wrong model answered); a model cola
    /// **sent** on an accepted `turn/start` survives an omitted field, because
    /// silence cannot contradict it. A reported model always wins, pick or not.
    #[test]
    fn silence_drops_a_reported_model_but_never_a_picked_one() {
        fn model_of(projection: &ThreadProjection) -> Option<String> {
            projection
                .transcript()
                .messages
                .iter()
                .rev()
                .find(|message| message.role == crate::backend::MessageRole::Assistant)
                .and_then(|message| message.model.as_ref())
                .map(|model| model.model_id.clone())
        }
        let identity = |model_id: &str| ModelIdentity {
            provider_id: "opencode-go".into(),
            model_id: model_id.into(),
            variant: None,
        };

        let mut projection = projected();
        projection.observe_thread_model(Some(identity("gpt-6-luna")));
        assert_eq!(model_of(&projection), Some("gpt-6-luna".into()));

        // A silent read drops the name the server itself gave.
        projection.observe_thread_model(None);
        assert_eq!(model_of(&projection), None);

        // A picked model survives a silent read ...
        projection.record_sent_model(identity("gpt-5.5"));
        projection.observe_thread_model(None);
        assert_eq!(model_of(&projection), Some("gpt-5.5".into()));

        // ... and a server that names another model still wins over it.
        projection.observe_thread_model(Some(identity("gpt-6-luna")));
        assert_eq!(model_of(&projection), Some("gpt-6-luna".into()));
        // That reported name is again droppable, i.e. the source moved back.
        projection.observe_thread_model(None);
        assert_eq!(model_of(&projection), None);
    }

    /// Ticket #879 acceptance 1: the same recording replayed twice yields the
    /// same transcript, with no duplicate item — keyed on the server's ids,
    /// never on a synthesised one.
    #[test]
    fn a_recorded_sequence_replays_into_the_same_duplicate_free_transcript() {
        let mut projection = ThreadProjection::new("thread-1");
        replay(&mut projection, &recording());
        let first = projection.transcript();

        replay(&mut projection, &recording());
        let second = projection.transcript();

        assert_eq!(first.messages, second.messages, "a replay must be idempotent");
        assert_eq!(first.truncated, second.truncated);

        let ids: Vec<&str> = second
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "duplicate message ids: {ids:?}");
        assert_eq!(
            ids,
            [
                "item-user",
                "item-reasoning",
                "item-agent",
                "item-command",
                "item-file",
                "item-mcp",
                "item-web",
                "item-image",
                "item-plan",
                "item-compaction",
            ],
            "one message per server-owned item, in first-seen order"
        );
    }

    /// The adapter's item kinds reach the card **namespaced** (ADR-0042's
    /// amendment), so the card's exact-id mapping cannot collide with a
    /// third-party tool. A bare id leaking out would silently lose the display
    /// mapping, and this is the only test that holds the adapter side of it.
    #[test]
    fn item_kinds_become_namespaced_tool_ids() {
        let transcript = projected().transcript();
        let names: Vec<&str> = transcript
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                Part::Tool(call) => Some(call.identity.name.as_str()),
                _ => None,
            })
            .collect();
        for expected in [
            "codex/command_execution",
            "codex/file_change",
            "codex/web_search",
            "codex/image",
        ] {
            assert!(names.contains(&expected), "{expected} missing from {names:?}");
        }
        for bare in ["command_execution", "file_change", "web_search", "image"] {
            assert!(!names.contains(&bare), "bare id leaked: {names:?}");
        }
    }

    /// Acceptance 2: a mid-turn read is the cache's current state — the applied
    /// delta is there, the turn is live, and a later read keeps it.
    #[test]
    fn a_mid_turn_read_keeps_the_deltas_already_applied_and_reports_live() {
        let mut projection = ThreadProjection::new("thread-1");
        let events = recording();
        for event in &events {
            let method = event.get("method").and_then(Value::as_str).unwrap();
            let params = event.get("params").unwrap();
            projection.apply(method, params);
            if method == AGENT_DELTA {
                break;
            }
        }

        assert!(projection.is_live(), "the turn has not completed");
        let mid = projection.transcript();
        let agent = mid
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-agent")
            .expect("the streamed item is present");
        assert_eq!(agent.text(), "The");
        assert!(agent.time.is_some_and(|time| time.completed.is_none()));

        // A later delta must not erase what the read already saw.
        let delta = events
            .iter()
            .filter(|event| event["method"] == AGENT_DELTA)
            .nth(1)
            .expect("a second agent delta");
        projection.apply("item/agentMessage/delta", &delta["params"]);
        let later = projection.transcript();
        let agent = later
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-agent")
            .unwrap();
        assert_eq!(agent.text(), "The fix");
    }

    /// Acceptance 3: a tool's output survives the neutral mapping, with the
    /// exit status on the part, and a partial stream stays running.
    #[test]
    fn a_tool_stream_is_running_until_completed_and_keeps_its_exit_status() {
        let events = recording();
        let mut partial = ThreadProjection::new("thread-1");
        for event in &events {
            let method = event["method"].as_str().unwrap();
            let params = &event["params"];
            partial.apply(method, params);
            if method == COMMAND_OUTPUT_DELTA {
                break;
            }
        }
        let mid = partial.transcript();
        let Part::Tool(call) = mid
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-command")
            .and_then(|message| message.parts.first())
            .expect("the command part")
        else {
            panic!("the command maps to a tool part");
        };
        assert_eq!(
            call.status,
            ToolStatus::Running,
            "a partial stream is not a finished tool"
        );
        assert!(
            matches!(call.output.blocks.first(), Some(ContentBlock::Text(text)) if text.contains("Compiling"))
        );

        let full = projected().transcript();
        let Part::Tool(call) = full
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-command")
            .and_then(|message| message.parts.first())
            .expect("the command part")
        else {
            panic!("the command maps to a tool part");
        };
        assert_eq!(call.status, ToolStatus::Error, "exitCode 1 is a failed command");
        assert_eq!(call.metadata.as_ref().unwrap()["exitCode"], 1);
        assert!(
            matches!(call.output.blocks.first(), Some(ContentBlock::Text(text)) if text.contains("error[E0425]"))
        );
        assert!(call.output.error.is_some(), "the failure text survives");
    }

    /// The kinds whose wire item has no status field are live until their
    /// completion arrives: a started web search, image or compaction must not
    /// render as a finished tool.
    #[test]
    fn statusless_items_are_running_until_their_completion_arrives() {
        let events = recording();
        let mut projection = ThreadProjection::new("thread-1");
        for event in &events {
            let method = event["method"].as_str().unwrap();
            let params = &event["params"];
            let Some(item) = params.get("item") else {
                continue;
            };
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            if !["item-web", "item-image", "item-compaction"].contains(&id) {
                continue;
            }
            if method == ITEM_STARTED {
                projection.apply(method, params);
            }
        }

        let started = projection.transcript();
        let find = |id: &str| {
            started
                .messages
                .iter()
                .find(|message| message.id.as_str() == id)
                .and_then(|message| message.parts.first())
                .unwrap_or_else(|| panic!("{id} is projected"))
        };
        for id in ["item-web", "item-image"] {
            let Part::Tool(call) = find(id) else {
                panic!("{id} maps to a tool part");
            };
            assert_eq!(call.status, ToolStatus::Running, "{id} is still streaming");
        }
        let Part::Compaction(fact) = find("item-compaction") else {
            panic!("item-compaction maps to a compaction part");
        };
        assert_eq!(fact.status, CompactionStatus::Running);

        // The completions settle them.
        for event in &events {
            if event["method"] == ITEM_COMPLETED {
                let id = event["params"]["item"]["id"].as_str().unwrap_or("");
                if ["item-web", "item-image", "item-compaction"].contains(&id) {
                    projection.apply(ITEM_COMPLETED, &event["params"]);
                }
            }
        }
        let completed = projection.transcript();
        for (id, expected) in [
            ("item-web", ToolStatus::Completed),
            ("item-image", ToolStatus::Completed),
        ] {
            let Part::Tool(call) = completed
                .messages
                .iter()
                .find(|message| message.id.as_str() == id)
                .and_then(|message| message.parts.first())
                .unwrap()
            else {
                panic!("{id} maps to a tool part");
            };
            assert_eq!(call.status, expected, "{id} settles when completed");
        }
        let Part::Compaction(fact) = completed
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-compaction")
            .and_then(|message| message.parts.first())
            .unwrap()
        else {
            panic!("item-compaction maps to a compaction part");
        };
        assert_eq!(fact.status, CompactionStatus::Completed);
    }

    /// A delta may arrive before its `item/started` (a resumed stream need not
    /// replay the start). The start folds over the accumulated text instead of
    /// erasing what the server already streamed.
    #[test]
    fn a_start_after_a_delta_preserves_the_delta() {
        let mut projection = ThreadProjection::new("thread-1");
        projection.apply(
            AGENT_DELTA,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-agent",
                "delta": "Hello",
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_000_000_i64,
                // A late start with non-empty text must still be replay
                // metadata only: the deltas already folded are authoritative.
                "item": { "type": "agentMessage", "id": "item-agent", "text": "FROM START" },
            }),
        );
        projection.apply(
            AGENT_DELTA,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-agent",
                "delta": " world",
            }),
        );
        let transcript = projection.transcript();
        assert_eq!(
            transcript
                .messages
                .iter()
                .find(|message| message.id.as_str() == "item-agent")
                .unwrap()
                .text(),
            "Hello world",
            "the start must not overwrite an already-applied delta"
        );

        // The same merge keeps command output that arrived before its start.
        projection.apply(
            COMMAND_OUTPUT_DELTA,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-command",
                "delta": "partial output\n",
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_000_100_i64,
                "item": {
                    "type": "commandExecution",
                    "id": "item-command",
                    "command": "cargo test",
                    "cwd": "/work/cola",
                    "status": "inProgress",
                    "aggregatedOutput": null,
                    "exitCode": null,
                },
            }),
        );
        let transcript = projection.transcript();
        let Part::Tool(call) = transcript
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-command")
            .and_then(|message| message.parts.first())
            .unwrap()
        else {
            panic!("the command maps to a tool part");
        };
        assert!(
            matches!(call.output.blocks.first(), Some(ContentBlock::Text(text)) if text == "partial output\n"),
            "the start preserved the output delta"
        );
    }

    /// Acceptance 4: a turn that changed files has its Turn Diff and its token
    /// usage on the newest assistant message.
    #[test]
    fn turn_diff_and_token_usage_are_populated_for_a_changed_file_turn() {
        let projection = projected();
        let diff = projection.diff_for(Some("item-user")).expect("the turn's diff");
        assert_eq!(
            diff.stats(),
            TurnDiffStats {
                files: 1,
                additions: 1,
                deletions: 1,
            }
        );
        assert_eq!(diff.0[0].path, "src/lib.rs");
        assert_eq!(diff.0[0].status, "modified");
        assert!(diff.0[0].patch.contains("@@ -1 +1 @@"));

        let transcript = projection.transcript();
        let usage = transcript
            .messages
            .iter()
            .find_map(|message| message.tokens)
            .expect("token usage rides the turn's newest assistant message");
        assert_eq!(usage.total, 12_345);
        assert_eq!(usage.cache_read, 8_000);
        assert_eq!(usage.output, 900);
        assert_eq!(
            projection.model_context_window(),
            Some(128_000),
            "the context window comes from the usage notification"
        );

        // The completed turn carries a terminal step finish, so the neutral
        // turn view can see it complete without a server-status read.
        let newest = transcript
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("an assistant message");
        assert!(
            newest
                .parts
                .iter()
                .any(|part| matches!(part, Part::StepFinish(finish) if finish.reason == FinishReason::Stop))
        );
    }

    /// A read after `turn/completed` is stable: the live flag clears and a
    /// second read is byte-identical.
    #[test]
    fn a_completed_projection_is_stable_and_not_live() {
        let projection = projected();
        assert!(!projection.is_live());
        assert_eq!(projection.transcript().messages, projection.transcript().messages);
        let transcript = projection.transcript();
        let agent = transcript
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-agent")
            .unwrap();
        assert!(agent.time.unwrap().completed.is_some());
    }

    /// A lagged subscription states the gap, and a successful `thread/read`
    /// resync clears it while dropping nothing: the rebuilt projection has one
    /// item per turn item, not the live items plus the history copies.
    #[test]
    fn a_lagged_projection_states_the_gap_and_a_resync_rebuilds_it() {
        let mut projection = ThreadProjection::new("thread-1");
        replay(&mut projection, &recording());
        projection.mark_incomplete();
        assert!(projection.transcript().truncated, "the gap is visible");

        let history = json!([{
            "id": "turn-1",
            "status": "completed",
            "startedAt": 1_700_000_000,
            "completedAt": 1_700_000_002,
            "error": null,
            "items": [
                {
                    "type": "userMessage",
                    "id": "item-user",
                    "content": [{ "type": "text", "text": "Fix the build", "textElements": [] }]
                },
                { "type": "agentMessage", "id": "item-agent", "text": "Done." }
            ]
        }]);
        // The gap may have dropped the ending while a stale `active` stayed
        // cached; the replayed history proves the turn is over.
        projection.set_observed_status(Some(ThreadStatus::Active));
        projection.resync(history.as_array().unwrap(), projection.feed_seq + 1);
        assert!(!projection.transcript().truncated, "a resync clears the gap");
        assert_eq!(
            projection.observed_status(),
            Some(ThreadStatus::Idle),
            "the rebuilt history settles a stale active status"
        );

        let transcript = projection.transcript();
        assert_eq!(
            transcript
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["item-user", "item-agent"],
            "history rebuilds the projection, it does not append to it"
        );
        assert_eq!(transcript.messages[1].text(), "Done.");
    }

    /// Ticket #880 review finding: the status watcher and the thread subscriber
    /// are separate tasks over one broadcast. A status the watcher dequeued
    /// before a history resync must not write after it — the page's receipt
    /// boundary is recorded even when the page writes no status of its own.
    #[test]
    fn a_status_older_than_the_replayed_page_cannot_resurrect_a_live_run() {
        let mut projection = ThreadProjection::new("thread-1");
        let history = json!([{
            "id": "turn-1",
            "status": "completed",
            "startedAt": 1_700_000_000,
            "completedAt": 1_700_000_002,
            "error": null,
            "items": []
        }]);
        // The connection received `active` as notification 5, but its watcher
        // has not run yet. The page read at that boundary proves the turn over.
        projection.resync(history.as_array().unwrap(), 5);
        projection.set_observed_status_at(Some(ThreadStatus::Active), 5);
        assert_ne!(
            projection.observed_status(),
            Some(ThreadStatus::Active),
            "a status at the page's own boundary is stale"
        );

        // A status received after the page is newer and still applies.
        projection.set_observed_status_at(Some(ThreadStatus::Active), 6);
        assert_eq!(projection.observed_status(), Some(ThreadStatus::Active));
    }

    /// A closed or lagged connection cannot vouch for its cached status, and a
    /// status that was already queued behind the boundary must not restore it.
    #[test]
    fn a_cleared_status_drops_queued_older_ones() {
        let mut projection = ThreadProjection::new("thread-1");
        projection.set_observed_status_at(Some(ThreadStatus::Idle), 5);
        projection.clear_observed_status_at(7);
        assert_eq!(projection.observed_status(), None);
        projection.set_observed_status_at(Some(ThreadStatus::Active), 6);
        assert_eq!(
            projection.observed_status(),
            None,
            "a queued status older than the clear is dropped"
        );
        projection.set_observed_status_at(Some(ThreadStatus::Active), 8);
        assert_eq!(projection.observed_status(), Some(ThreadStatus::Active));
    }

    /// The `item/*` wire vocabulary is this module's alone: no file outside the
    /// adapter may name a notification this read model consumes.
    #[test]
    fn the_item_wire_names_do_not_leak_outside_the_adapter() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for path in rust_sources(&root) {
            let relative = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if relative.starts_with("codex/") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read a source file");
            for method in [
                ITEM_STARTED,
                ITEM_COMPLETED,
                AGENT_DELTA,
                REASONING_SUMMARY_DELTA,
                REASONING_TEXT_DELTA,
                COMMAND_OUTPUT_DELTA,
                FILE_CHANGE_PATCH_UPDATED,
                FILE_CHANGE_OUTPUT_DELTA,
                MCP_PROGRESS,
                PLAN_DELTA,
                TURN_STARTED,
                TURN_COMPLETED,
                TURN_DIFF_UPDATED,
                TOKEN_USAGE_UPDATED,
            ] {
                assert!(
                    !source.contains(method),
                    "`{method}` leaked out of the adapter into src/{relative}"
                );
            }
        }
    }

    /// The id rule: an item the server did not id is not projected, and two
    /// completions of one server id replace rather than append.
    #[test]
    fn an_item_without_a_server_id_is_never_invented_and_a_repeat_replaces_it() {
        let mut projection = ThreadProjection::new("thread-1");
        assert!(projection.apply(
            ITEM_COMPLETED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "completedAtMs": 10,
                "item": { "type": "agentMessage", "text": "no id here" }
            })
        ));
        assert!(
            projection.transcript().messages.is_empty(),
            "an id-less item must not become a message"
        );

        for completed in [10, 20] {
            projection.apply(
                ITEM_COMPLETED,
                &json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "completedAtMs": completed,
                    "item": { "type": "agentMessage", "id": "item-a", "text": "one" }
                }),
            );
        }
        let transcript = projection.transcript();
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].id.as_str(), "item-a");
    }

    /// Ticket #880 acceptance 5: a terminal `error` notification is the turn's
    /// ending even when no `turn/completed` follows. The card reads the failure
    /// from the transcript's newest assistant message and settles instead of
    /// hanging on a run the server has abandoned.
    #[test]
    fn a_terminal_error_settles_the_live_turn_with_its_text() {
        let mut projection = ThreadProjection::new("thread-1");
        projection.apply(
            TURN_STARTED,
            &json!({
                "threadId": "thread-1",
                "turn": { "id": "turn-1", "status": "inProgress", "startedAt": 1_700_000_000_i64 }
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_000_000_i64,
                "item": {
                    "type": "userMessage",
                    "id": "item-user",
                    "content": [{ "type": "text", "text": "go" }]
                }
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_001_000_i64,
                "item": { "type": "agentMessage", "id": "item-agent", "text": "" }
            }),
        );
        projection.apply(
            AGENT_DELTA,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-agent",
                "delta": "partial"
            }),
        );
        assert!(projection.is_live());

        projection.apply(
            ERROR_NOTIFICATION,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "error": { "message": "401 unauthorized" },
                "willRetry": false
            }),
        );

        assert!(!projection.is_live(), "a terminal error ends the turn");
        assert_eq!(projection.observed_status(), Some(ThreadStatus::Idle));
        let transcript = projection.transcript();
        let anchor = transcript
            .newest_user()
            .and_then(|user| user.anchor())
            .expect("turn anchor");
        let turn = transcript.turn_for_user(&anchor);
        assert_eq!(turn.error.as_deref(), Some("401 unauthorized"));
        assert!(turn.complete, "the failure is a terminal ending");
        assert_eq!(
            transcript.settle(Some(&anchor)),
            crate::backend::TurnSettle::Failed("401 unauthorized".to_string())
        );
    }

    /// Ticket #880 acceptance 5, the harder shape: a terminal error that lands
    /// before any assistant item still reaches the neutral read. The card reads
    /// a turn's failure from its newest assistant message, so the read model
    /// gives the failed turn a stable synthetic one (keyed by the server's turn
    /// id) rather than dropping the text.
    #[test]
    fn a_terminal_error_before_an_assistant_item_keeps_its_text() {
        let mut projection = ThreadProjection::new("thread-1");
        projection.apply(
            TURN_STARTED,
            &json!({
                "threadId": "thread-1",
                "turn": { "id": "turn-1", "status": "inProgress", "startedAt": 1_700_000_000_i64 }
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_000_000_i64,
                "item": {
                    "type": "userMessage",
                    "id": "item-user",
                    "content": [{ "type": "text", "text": "go" }]
                }
            }),
        );
        projection.apply_at(
            7,
            ERROR_NOTIFICATION,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "error": { "message": "401 unauthorized" },
                "willRetry": false
            }),
        );

        let transcript = projection.transcript();
        let anchor = transcript
            .newest_user()
            .and_then(|user| user.anchor())
            .expect("the turn anchor");
        let turn = transcript.turn_for_user(&anchor);
        assert_eq!(turn.error.as_deref(), Some("401 unauthorized"));
        assert!(turn.complete, "the synthetic failure ends the turn");
        assert_eq!(
            transcript.settle(Some(&anchor)),
            crate::backend::TurnSettle::Failed("401 unauthorized".to_string())
        );

        // The synthetic message is a stable projection of the turn, never an
        // appended duplicate: two reads are identical.
        let synthetic = transcript
            .messages
            .iter()
            .find(|message| message.id.as_str() == "cola:turn-error:turn-1")
            .expect("the failure is exposed");
        assert!(synthetic.text().is_empty(), "it carries no invented content");
        let second = projection.transcript();
        assert_eq!(
            second
                .messages
                .iter()
                .map(|message| (message.id.clone(), message.error.clone()))
                .collect::<Vec<_>>(),
            transcript
                .messages
                .iter()
                .map(|message| (message.id.clone(), message.error.clone()))
                .collect::<Vec<_>>(),
            "a re-read reproduces the same synthetic message"
        );
    }

    /// The retrying shape of the same notification is NOT an ending: the
    /// server schedules another attempt, so the turn stays live and its card
    /// keeps streaming.
    #[test]
    fn a_retrying_error_keeps_the_turn_live() {
        let mut projection = ThreadProjection::new("thread-1");
        projection.apply(
            TURN_STARTED,
            &json!({
                "threadId": "thread-1",
                "turn": { "id": "turn-1", "status": "inProgress", "startedAt": 1_700_000_000_i64 }
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_000_000_i64,
                "item": {
                    "type": "userMessage",
                    "id": "item-user",
                    "content": [{ "type": "text", "text": "go" }]
                }
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_001_000_i64,
                "item": { "type": "agentMessage", "id": "item-agent", "text": "working" }
            }),
        );
        projection.apply(
            ERROR_NOTIFICATION,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "error": { "message": "stream disconnected" },
                "willRetry": true
            }),
        );
        assert!(projection.is_live(), "a retry keeps the run live");
        assert_eq!(projection.observed_status(), Some(ThreadStatus::Active));
        let transcript = projection.transcript();
        let anchor = transcript
            .newest_user()
            .and_then(|user| user.anchor())
            .expect("the turn anchor");
        assert_eq!(
            transcript.turn_for_user(&anchor).error,
            None,
            "a retrying error is not the turn's failure"
        );
        assert!(
            !matches!(
                transcript.settle(Some(&anchor)),
                crate::backend::TurnSettle::Failed(_)
            ),
            "the card must not take the retry's text as a terminal failure"
        );
    }

    /// Ticket #880 re-review: an `error` notification mid-turn settles the card
    /// even when the server scoped it to the thread rather than the turn. The
    /// live run is the one it abandoned, so its text lands on that turn and the
    /// projection stops reporting a live run instead of hanging on one.
    #[test]
    fn a_thread_scoped_terminal_error_settles_the_live_turn() {
        let mut projection = ThreadProjection::new("thread-1");
        projection.apply(
            TURN_STARTED,
            &json!({
                "threadId": "thread-1",
                "turn": { "id": "turn-1", "status": "inProgress", "startedAt": 1_700_000_000_i64 }
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_000_000_i64,
                "item": {
                    "type": "userMessage",
                    "id": "item-user",
                    "content": [{ "type": "text", "text": "go" }]
                }
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_001_000_i64,
                "item": { "type": "agentMessage", "id": "item-agent", "text": "working" }
            }),
        );
        assert!(projection.is_live());

        projection.apply(
            ERROR_NOTIFICATION,
            &json!({
                "threadId": "thread-1",
                "error": { "message": "401 unauthorized" },
                "willRetry": false
            }),
        );

        assert_eq!(
            projection.live_turn(),
            None,
            "the abandoned run is no longer live"
        );
        assert!(!projection.is_live());
        assert_eq!(projection.observed_status(), Some(ThreadStatus::Idle));
        let transcript = projection.transcript();
        let anchor = transcript
            .newest_user()
            .and_then(|user| user.anchor())
            .expect("turn anchor");
        let turn = transcript.turn_for_user(&anchor);
        assert_eq!(turn.error.as_deref(), Some("401 unauthorized"));
        assert!(turn.complete, "the failure is a terminal ending");
        assert_eq!(
            transcript.settle(Some(&anchor)),
            crate::backend::TurnSettle::Failed("401 unauthorized".to_string())
        );

        // A second read is the same settled turn, not a re-opened one.
        let second = projection.transcript();
        assert!(!projection.is_live());
        assert_eq!(
            second.turn_for_user(&anchor).error.as_deref(),
            Some("401 unauthorized")
        );
    }

    /// The retrying shape of a thread-scoped error is not an ending either:
    /// the check comes before the turn is resolved, so the live turn stands.
    #[test]
    fn a_thread_scoped_retrying_error_keeps_the_turn_live() {
        let mut projection = ThreadProjection::new("thread-1");
        projection.apply(
            TURN_STARTED,
            &json!({
                "threadId": "thread-1",
                "turn": { "id": "turn-1", "status": "inProgress", "startedAt": 1_700_000_000_i64 }
            }),
        );
        projection.apply(
            ITEM_STARTED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "startedAtMs": 1_700_000_000_000_i64,
                "item": {
                    "type": "userMessage",
                    "id": "item-user",
                    "content": [{ "type": "text", "text": "go" }]
                }
            }),
        );
        projection.apply(
            ERROR_NOTIFICATION,
            &json!({
                "threadId": "thread-1",
                "error": { "message": "stream disconnected" },
                "willRetry": true
            }),
        );

        assert_eq!(projection.live_turn(), Some("turn-1"));
        assert!(projection.is_live(), "a retry keeps the run live");
        assert_eq!(projection.observed_status(), Some(ThreadStatus::Active));
        let transcript = projection.transcript();
        let anchor = transcript
            .newest_user()
            .and_then(|user| user.anchor())
            .expect("the turn anchor");
        assert_eq!(
            transcript.turn_for_user(&anchor).error,
            None,
            "a retrying error is not the turn's failure"
        );
    }

    /// A terminal error with no turn at all is the thread's, not a run's: it
    /// settles no turn and invents no phantom one for the next read.
    #[test]
    fn a_thread_level_terminal_error_without_a_live_turn_invents_no_turn() {
        let mut projection = ThreadProjection::new("thread-1");
        projection.apply(
            ERROR_NOTIFICATION,
            &json!({
                "threadId": "thread-1",
                "error": { "message": "no credential" },
                "willRetry": false
            }),
        );

        assert!(!projection.is_live());
        assert_eq!(projection.live_turn(), None);
        let transcript = projection.transcript();
        assert!(
            transcript.messages.is_empty(),
            "a thread-level error is not a turn's failure"
        );
        assert!(
            projection.diff_for(None).is_none(),
            "no phantom turn was created for the thread-level error"
        );
    }

    /// The status family is not transcript content: a stray broadcast routed to
    /// a subscription is ignored rather than folded into the read.
    #[test]
    fn a_status_notification_does_not_pollute_the_transcript() {
        let mut projection = ThreadProjection::new("thread-1");
        assert!(!projection.apply(
            "thread/status/changed",
            &json!({
                "threadId": "thread-1",
                "status": { "type": "active", "activeFlags": [] }
            })
        ));
        assert!(projection.transcript().messages.is_empty());
    }

    /// Ticket #880 acceptance 3: the compaction's message identity and server
    /// time are what the Rendered Cursor remembers, so the projection must
    /// rebuild them identically from persisted history after a restart. The
    /// card then skips the position and the row renders once, on the card that
    /// owns it.
    #[test]
    fn a_compaction_keeps_its_position_across_a_restart_resync() {
        let mut projection = ThreadProjection::new("thread-1");
        projection.apply(
            TURN_STARTED,
            &json!({
                "threadId": "thread-1",
                "turn": { "id": "turn-1", "status": "inProgress", "startedAt": 1_700_000_000_i64 }
            }),
        );
        projection.apply(
            ITEM_COMPLETED,
            &json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "completedAtMs": 1_700_000_002_000_i64,
                "item": { "type": "contextCompaction", "id": "item-compaction" }
            }),
        );
        let before = projection.transcript();
        let carried = before
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-compaction")
            .expect("the live projection carries the compaction");
        assert_eq!(
            carried.role,
            MessageRole::Other("compaction".to_string()),
            "a compaction is a session fact, not an assistant answer"
        );
        assert!(matches!(
            carried.parts.first(),
            Some(Part::Compaction(fact)) if fact.status == CompactionStatus::Completed
        ));
        let created = carried.time.expect("the message's server time").created;

        // The restart path: rebuild from `thread/turns/list`'s persisted turns.
        projection.resync(
            &[json!({
                "id": "turn-1",
                "status": "completed",
                "startedAt": 1_700_000_000,
                "completedAt": 1_700_000_002,
                "error": null,
                "items": [{ "type": "contextCompaction", "id": "item-compaction" }]
            })],
            projection.feed_seq + 1,
        );

        let after = projection.transcript();
        let rebuilt = after
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-compaction")
            .expect("the resync rebuilds the same compaction identity");
        assert_eq!(
            rebuilt.time.expect("the rebuilt message's time").created,
            created,
            "the server time the Rendered Cursor keys on is stable"
        );
        assert!(matches!(
            rebuilt.parts.first(),
            Some(Part::Compaction(fact)) if fact.status == CompactionStatus::Completed
        ));

        // The compaction is not an answer: the turn's completion and error do
        // not ride it.
        let anchor = crate::backend::TurnAnchor {
            message_id: crate::backend::MessageId::new("item-compaction"),
            created_ms: created,
        };
        assert!(
            after.turn_for_user(&anchor).messages.is_empty(),
            "a compaction never joins the Turn's assistant window"
        );
    }

    fn rust_sources(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut sources = Vec::new();
        for entry in std::fs::read_dir(directory).expect("read src") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                sources.extend(rust_sources(&path));
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
        sources
    }

    #[test]
    fn a_parse_of_the_recorded_diff_splits_added_and_deleted_files() {
        let diff = "diff --git a/new.rs b/new.rs\nnew file mode 100644\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,2 @@\n+one\n+two\n\
                    diff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\n--- a/gone.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-bye\n";
        let parsed = parse_unified_diff(diff);
        assert_eq!(parsed.0.len(), 2);
        assert_eq!(parsed.0[0].path, "new.rs");
        assert_eq!(parsed.0[0].status, "added");
        assert_eq!(parsed.0[0].additions, 2);
        assert_eq!(parsed.0[1].status, "deleted");
        assert_eq!(parsed.0[1].deletions, 1);
    }
}
