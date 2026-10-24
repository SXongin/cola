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
    MessageRole, MessageTime, OtherPart, Part, ReasoningPart, SessionTranscript, StepFinish, TextPart,
    TokenUsage, ToolCall, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage, TurnDiff, TurnDiffFile,
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
    /// Set when a subscription gap means the projection may be missing events;
    /// cleared only by a successful `thread/read` resync.
    incomplete: bool,
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

    /// Mark the projection as having missed events. The subscriber then
    /// resyncs from `thread/read`; until that succeeds a read must not look
    /// complete.
    pub(crate) fn mark_incomplete(&mut self) {
        self.incomplete = true;
    }

    /// Fold one notification into the projection. Returns whether the method
    /// was one this read model understands.
    ///
    /// A decode failure marks the projection incomplete instead of silently
    /// losing the event: a changed payload shape is visible on the read, never
    /// guessed around.
    pub(crate) fn apply(&mut self, method: &str, params: &Value) -> bool {
        if let Some(thread_id) = params.get("threadId").and_then(Value::as_str)
            && thread_id != self.thread_id
        {
            // The dispatcher routes by threadId; a direct feed (tests, a
            // future replay tool) must not pollute another thread's cache.
            return false;
        }
        match method {
            TURN_STARTED => self.decode(params, |this, n: TurnNotification| this.turn_started(n)),
            TURN_COMPLETED => self.decode(params, |this, n: TurnNotification| this.turn_completed(n)),
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
            ERROR_NOTIFICATION => self.decode(params, |this, n: ErrorNotification| this.turn_error(n)),
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
    pub(crate) fn resync(&mut self, turns: &[Value]) {
        self.items.clear();
        self.index.clear();
        self.turns.clear();
        self.turn_order.clear();
        self.live_turn = None;
        for turn in turns {
            self.seed_turn(turn);
        }
        self.incomplete = false;
    }

    /// Seed a fresh projection from `thread/resume`'s turn history. Unlike
    /// [`Self::resync`], this never clears existing items: it runs before the
    /// subscriber's first event on a fresh projection.
    pub(crate) fn seed(&mut self, turns: &[Value]) {
        for turn in turns {
            self.seed_turn(turn);
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

        let mut transcript = SessionTranscript::new(messages);
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

    pub(crate) fn model_context_window(&self) -> Option<i64> {
        self.context_window
    }

    fn message(&self, item: &Item) -> TranscriptMessage {
        let created = item
            .started_at_ms
            .or_else(|| self.turns.get(&item.turn_id).and_then(|turn| turn.started_ms))
            .unwrap_or(0);
        let (role, parts) = item.parts();
        TranscriptMessage {
            id: MessageId::new(item.id.clone()),
            role,
            time: Some(MessageTime {
                created,
                completed: item.completed_at_ms,
            }),
            model: None,
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
        });
        self.index.insert(id.to_string(), index);
        index
    }

    fn turn_started(&mut self, notification: TurnNotification) {
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

    fn turn_completed(&mut self, notification: TurnNotification) {
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

    fn turn_error(&mut self, notification: ErrorNotification) {
        if let Some(turn_id) = notification.turn_id {
            let message = notification.error.message;
            if !message.is_empty() {
                self.ensure_turn(&turn_id).error = Some(message);
            }
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
        });
        self.index.insert(id, index);
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
        });
        self.index.insert(id, index);
    }

    fn agent_delta(&mut self, delta: Delta) {
        let index = self.item_index(
            &delta.item_id,
            &delta.turn_id,
            ItemKind::AgentMessage { text: String::new() },
        );
        if let ItemKind::AgentMessage { text } = &mut self.items[index].kind {
            text.push_str(&delta.delta);
        }
    }

    fn reasoning_summary_delta(&mut self, delta: Delta) {
        let index = self.item_index(&delta.item_id, &delta.turn_id, ItemKind::reasoning());
        if let ItemKind::Reasoning { summary, .. } = &mut self.items[index].kind {
            summary.push_str(&delta.delta);
        }
    }

    fn reasoning_text_delta(&mut self, delta: Delta) {
        let index = self.item_index(&delta.item_id, &delta.turn_id, ItemKind::reasoning());
        if let ItemKind::Reasoning { text, .. } = &mut self.items[index].kind {
            text.push_str(&delta.delta);
        }
    }

    fn command_output_delta(&mut self, delta: Delta) {
        let index = self.item_index(&delta.item_id, &delta.turn_id, ItemKind::command());
        if let ItemKind::Command { output, .. } = &mut self.items[index].kind {
            output.push_str(&delta.delta);
        }
    }

    fn file_change_output_delta(&mut self, delta: Delta) {
        let index = self.item_index(&delta.item_id, &delta.turn_id, ItemKind::file_change());
        if let ItemKind::FileChange { output, .. } = &mut self.items[index].kind {
            output.push_str(&delta.delta);
        }
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
        if let ItemKind::Mcp { output, .. } = &mut self.items[index].kind {
            output.push_str(&delta.delta);
        }
    }

    fn plan_delta(&mut self, delta: Delta) {
        let index = self.item_index(
            &delta.item_id,
            &delta.turn_id,
            ItemKind::Plan { text: String::new() },
        );
        if let ItemKind::Plan { text } = &mut self.items[index].kind {
            text.push_str(&delta.delta);
        }
    }
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
}

impl Item {
    fn is_assistant(&self) -> bool {
        !matches!(self.kind, ItemKind::UserMessage { .. })
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
                            name: "command_execution".to_string(),
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
                            name: "file_change".to_string(),
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
                        name: "web_search".to_string(),
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
                        name: "image".to_string(),
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
                MessageRole::Assistant,
                vec![Part::Compaction(Compaction {
                    status: if self.completed {
                        CompactionStatus::Completed
                    } else {
                        CompactionStatus::Running
                    },
                    reason: CompactionReason::Auto,
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
        projection.resync(history.as_array().unwrap());
        assert!(!projection.transcript().truncated, "a resync clears the gap");

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
