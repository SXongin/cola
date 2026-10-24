//! The Codex adapter's `Backend` implementation (spec #860, ticket #879).
//!
//! This is the seam ADR-0090 describes: cola's poll-shaped `Backend` on one
//! side, app-server's push-shaped notifications on the other. The subscription
//! is the adapter's, the cache is the adapter's, and every read call here is a
//! projection over that cache — the bridge learns nothing about item streams.
//!
//! Two types implement the contract:
//!
//! - [`CodexClient`] is one attached connection. It owns the [`Threads`]
//!   subscription surface and the read-model cache, so a test (and an eventual
//!   per-connection caller) can drive the whole read path.
//! - [`CodexLifecycle`] is the active client the composition root stores in
//!   `SharedCore.client`; it owns discovery/attach and delegates the `Backend`
//!   surface to whatever client the last attach pass installed. A read before
//!   an attach is a named error, never a silently empty transcript.
//!
//! Mid-turn reads never wait on the subscription: [`Projections`] is guarded by
//! a plain mutex held only for the projection fold, while the subscriber task is
//! the only thing that awaits the push stream. A dropped-event `Lagged` signal
//! first marks the projection incomplete (so the read states the gap) and then
//! resyncs from `thread/read` with persisted turns.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::backend::lifecycle::{AttachOutcome, EnsureAttached, ServerLifecycle};
use crate::backend::types::{
    AgentInfo, CreateSessionInput, FormAnswer, Location, ModelInfo, ModelOption, PermissionRequest,
    PromptFile, ProviderModels, QuestionRequest, Session, SessionInfo, SessionListInfo, SessionStatus,
};
use crate::backend::{Backend, BackendDirectory, PromptSkill, SessionTranscript, SkillInfo, TurnDiff};
use crate::bridge::tasks::TasksHandle;
use crate::error::{BridgeError, Result};

use super::client::CodexClient;
use super::error::{CodexError, Result as CodexResult};
use super::lifecycle::CodexLifecycle;
use super::read_model::{ThreadProjection, ThreadStatus, decode_thread_status};
use super::threads::{Adoption, ThreadEvent, ThreadSubscription, Threads};
use super::transport::{Endpoint, Inbound, RpcConnection};

/// How long a truncated history waits before the next read retries paging. A
/// persistently failing server is polled at this cadence, never in a loop; a
/// healthy server clears the state on the first retry.
#[cfg(not(test))]
const PAGING_RETRY: Duration = Duration::from_secs(2);
#[cfg(test)]
const PAGING_RETRY: Duration = Duration::from_millis(20);

/// Bound the adoption backlog drain. The subscription channel holds 1024
/// events; draining at most that many means a first read either reflects every
/// queued event or is explicitly marked incomplete.
const ADOPTION_DRAIN_LIMIT: usize = 1024;

/// The broadcast notification that carries a thread's run state (ADR-0090's
/// amendment). Unlike `turn/*` and `item/*` it reaches every connection, so it
/// feeds [`Projections`] without a subscription.
const THREAD_STATUS_CHANGED: &str = "thread/status/changed";

/// One thread's projection and the state of its subscription.
#[derive(Default)]
pub(crate) struct Projections {
    threads: Mutex<HashMap<String, Arc<Mutex<ThreadProjection>>>>,
    states: Mutex<HashMap<String, SubscriptionState>>,
    /// One adoption lock per thread. Two concurrent reads of the same new
    /// thread must not both send `thread/resume`, but an unrelated thread (a
    /// repeatedly polled never-run thread included) must never block another's
    /// first adoption. Entries are weak, so a thread that is never adopted
    /// again costs no retained lock.
    adopting: Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    /// One wake-up per thread: a read that finds a due paging retry signals
    /// the subscriber, which is the only task allowed to page or apply. Weak
    /// entries: the subscriber holds the strong handle, and a finished thread
    /// keeps no signal.
    resync: Mutex<HashMap<String, std::sync::Weak<tokio::sync::Notify>>>,
    /// The subscriber tasks this cache owns. Aborted when the last client
    /// holding the cache is dropped, so a replaced attachment stops folding
    /// into a cache nothing reads.
    subscribers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The adoption backlog drain limit. Zero means the production default;
    /// tests lower it to exercise the truncation-and-retry path.
    drain_limit: Mutex<usize>,
}

impl Drop for Projections {
    fn drop(&mut self) {
        for task in self
            .subscribers
            .get_mut()
            .expect("codex subscriber tasks")
            .drain(..)
        {
            task.abort();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionState {
    /// `thread/resume` succeeded and the full history was paged; a subscriber
    /// task is folding events.
    Live,
    /// The subscription is live but the first history page failed. The bounded
    /// resume page is shown as truncated, and a later read retries paging after
    /// `retry_at`.
    PagingIncomplete { retry_at: Instant },
}

impl Projections {
    fn adoption_lock(&self, thread_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        shared_lock(&self.adopting, thread_id)
    }

    fn resync_signal(&self, thread_id: &str) -> Arc<tokio::sync::Notify> {
        let mut signals = self.resync.lock().expect("codex resync signals");
        signals.retain(|_, signal| signal.strong_count() > 0);
        if let Some(signal) = signals.get(thread_id).and_then(std::sync::Weak::upgrade) {
            return signal;
        }
        let signal = Arc::new(tokio::sync::Notify::new());
        signals.insert(thread_id.to_string(), Arc::downgrade(&signal));
        signal
    }

    /// Ask the subscriber to re-page a truncated history. Returns whether a
    /// request was actually made: only a due `PagingIncomplete` is signalled,
    /// and the retry deadline is pushed forward so repeated reads coalesce.
    fn request_resync(&self, thread_id: &str) -> bool {
        let mut states = self.states.lock().expect("codex subscription states");
        let due = matches!(
            states.get(thread_id),
            Some(SubscriptionState::PagingIncomplete { retry_at }) if Instant::now() >= *retry_at
        );
        if !due {
            return false;
        }
        states.insert(
            thread_id.to_string(),
            SubscriptionState::PagingIncomplete {
                retry_at: Instant::now() + PAGING_RETRY,
            },
        );
        drop(states);
        self.resync_signal(thread_id).notify_one();
        true
    }

    /// Whether this thread's run state has a live subscription AND a paged
    /// history. Retained history alone is NOT enough: a dead subscription
    /// leaves stale state, so status/transcript must re-adopt before answering.
    pub(crate) fn is_seeded(&self, thread_id: &str) -> bool {
        self.state(thread_id) == Some(SubscriptionState::Live)
    }

    fn projection(&self, thread_id: &str) -> Arc<Mutex<ThreadProjection>> {
        let mut threads = self.threads.lock().expect("codex projections");
        threads
            .entry(thread_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(ThreadProjection::new(thread_id))))
            .clone()
    }

    pub(crate) fn apply_at(&self, thread_id: &str, seq: u64, method: &str, params: &Value) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .apply_at(seq, method, params);
    }

    pub(crate) fn seed(&self, thread_id: &str, turns: &[Value], boundary_seq: u64) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .seed(turns, boundary_seq);
    }

    pub(crate) fn resync(&self, thread_id: &str, turns: &[Value], boundary_seq: u64) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .resync(turns, boundary_seq);
    }

    pub(crate) fn mark_incomplete(&self, thread_id: &str) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .mark_incomplete();
    }

    pub(crate) fn transcript(&self, thread_id: &str) -> SessionTranscript {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .transcript()
    }

    pub(crate) fn diff(&self, thread_id: &str, from: Option<&str>) -> Option<TurnDiff> {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .diff_for(from)
    }

    /// The context window of whichever projection last recorded one. The
    /// `Backend` call names provider/model, but Codex's usage notification is
    /// per thread; an attached thread's figure is the only one there is.
    pub(crate) fn any_context_window(&self) -> Option<i64> {
        self.threads
            .lock()
            .expect("codex projections")
            .values()
            .filter_map(|projection| {
                projection
                    .lock()
                    .expect("codex projection")
                    .model_context_window()
            })
            .next()
    }

    pub(crate) fn status(&self, thread_id: &str) -> Option<SessionStatus> {
        let projection = self.projection(thread_id);
        let projection = projection.lock().expect("codex projection");
        Some(match projection.observed_status() {
            // The broadcast is the server's own run state for this thread,
            // whether or not this connection subscribed: it wins over the
            // subscription-derived live flag.
            Some(ThreadStatus::Active) => SessionStatus::Busy,
            Some(ThreadStatus::Idle) => SessionStatus::Idle,
            None => {
                if projection.is_live() {
                    SessionStatus::Busy
                } else {
                    SessionStatus::Idle
                }
            }
        })
    }

    /// The last `thread/status/changed` state observed for this thread, if
    /// any. `None` means the broadcast has not reported one, so the caller may
    /// have to adopt the thread to learn its run state.
    pub(crate) fn observed_status(&self, thread_id: &str) -> Option<ThreadStatus> {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .observed_status()
    }

    /// Fold a `thread/status/changed` broadcast into the cache. This path is
    /// deliberately independent of a subscription: the server broadcasts the
    /// notification to every connection, so Session Sync learns a thread's
    /// busy/idle state without `thread/resume`.
    fn ingest_status(&self, params: &Value, seq: u64) {
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            return;
        };
        let decoded = params.get("status").and_then(decode_thread_status);
        if decoded.is_none() {
            tracing::warn!(
                thread = %thread_id,
                "a codex thread status did not decode; clearing the cached state"
            );
        }
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .set_observed_status_at(decoded, seq);
    }

    /// Forget every explicit status after the connection's push stream lagged:
    /// a status notification that was dropped could otherwise leave a session
    /// busy forever. With no cached state the next read adopts the thread (or
    /// answers from its subscription-derived live flag).
    fn clear_statuses(&self, seq: u64) {
        let projections: Vec<_> = self
            .threads
            .lock()
            .expect("codex projections")
            .values()
            .cloned()
            .collect();
        for projection in projections {
            projection
                .lock()
                .expect("codex projection")
                .clear_observed_status_at(seq);
        }
    }

    /// Start the status consumer over `connection`'s push stream. The status
    /// family is server-broadcast (ADR-0090's amendment), so this is a second,
    /// subscription-free reader of the same broadcast the thread dispatcher
    /// consumes; it owns no thread and sends no request.
    pub(crate) fn watch_thread_status(self: &Arc<Self>, connection: &RpcConnection) {
        let mut inbound = connection.inbound();
        let connection = connection.clone();
        let projections = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            loop {
                match inbound.recv().await {
                    Ok(Inbound::Notification { seq, method, params }) if method == THREAD_STATUS_CHANGED => {
                        let Some(projections) = projections.upgrade() else {
                            return;
                        };
                        projections.ingest_status(&params, seq);
                    }
                    Ok(Inbound::Closed) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // The socket is gone: a cached run state can no longer
                        // be trusted, so the next status read adopts (or fails)
                        // instead of presenting a stale busy.
                        if let Some(projections) = projections.upgrade() {
                            projections.clear_statuses(connection.inbound_seq());
                        }
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped,
                            "the codex status stream fell behind; clearing the cached statuses"
                        );
                        if let Some(projections) = projections.upgrade() {
                            projections.clear_statuses(connection.inbound_seq());
                        }
                    }
                    _ => {}
                }
            }
        });
        self.track(task);
    }

    /// Record that `/compact` asked for a manual compaction on this thread: the
    /// protocol's `contextCompaction` item carries no reason, so the request is
    /// the only evidence the existing card path can use to title the row 手动
    /// rather than 自动 (ADR-0082, ADR-0092).
    pub(crate) fn mark_manual_compaction(&self, thread_id: &str) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .mark_manual_compaction();
    }

    /// Whether a `/compact` request is still waiting for its item (test-only).
    #[cfg(test)]
    pub(crate) fn manual_compaction_pending(&self, thread_id: &str) -> bool {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .manual_compaction_pending()
    }

    /// Forget a manual-compaction request whose `thread/compact/start` failed.
    pub(crate) fn clear_manual_compaction(&self, thread_id: &str) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .clear_manual_compaction();
    }

    pub(crate) fn live_turn(&self, thread_id: &str) -> Option<String> {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .live_turn()
            .map(str::to_string)
    }

    /// Which thread and turn an item belongs to. The authorship question is
    /// asked with a transcript message id (a Codex item id), while the durable
    /// record is keyed by the turn cola started, so the projection — the only
    /// owner of the item→turn mapping — answers it.
    pub(crate) fn submission_of_item(&self, thread_id: &str, item_id: &str) -> Option<String> {
        let projection = self
            .threads
            .lock()
            .expect("codex projections")
            .get(thread_id)
            .cloned()?;
        projection
            .lock()
            .expect("codex projection")
            .submission_of_item(item_id)
            .map(str::to_string)
    }

    pub(crate) fn turn_of_item(&self, item_id: &str) -> Option<(String, String)> {
        let projections: Vec<(String, Arc<Mutex<ThreadProjection>>)> = self
            .threads
            .lock()
            .expect("codex projections")
            .iter()
            .map(|(thread, projection)| (thread.clone(), projection.clone()))
            .collect();
        for (thread, projection) in projections {
            let turn = projection
                .lock()
                .expect("codex projection")
                .turn_of_item(item_id)
                .map(str::to_string);
            if let Some(turn) = turn {
                return Some((thread, turn));
            }
        }
        None
    }

    /// How many adoption-lock slots are retained (test-only; proves a finished
    /// adoption's slot is pruned rather than leaking).
    #[cfg(test)]
    fn adoption_lock_count(&self) -> usize {
        self.adopting.lock().expect("codex adoption locks").len()
    }

    fn drain_limit(&self) -> usize {
        let configured = *self.drain_limit.lock().expect("codex drain limit");
        if configured == 0 {
            ADOPTION_DRAIN_LIMIT
        } else {
            configured
        }
    }

    #[cfg(test)]
    fn set_drain_limit(&self, limit: usize) {
        *self.drain_limit.lock().expect("codex drain limit") = limit;
    }

    fn state(&self, thread_id: &str) -> Option<SubscriptionState> {
        self.states
            .lock()
            .expect("codex subscription states")
            .get(thread_id)
            .copied()
    }

    fn track(&self, task: tokio::task::JoinHandle<()>) {
        self.subscribers
            .lock()
            .expect("codex subscriber tasks")
            .push(task);
    }

    fn set_state(&self, thread_id: &str, state: SubscriptionState) {
        self.states
            .lock()
            .expect("codex subscription states")
            .insert(thread_id.to_string(), state);
    }

    /// Forget a dead subscription so the next read adopts again, and state the
    /// gap on the retained history: a later read must not present stale state
    /// as current.
    pub(crate) fn forget(&self, thread_id: &str) {
        self.states
            .lock()
            .expect("codex subscription states")
            .remove(thread_id);
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .mark_incomplete();
    }
}

/// One lazily-created lock per key, held weakly so a finished generation does
/// not accumulate. Callers upgrade the same lock while any holder is alive.
fn shared_lock(
    locks: &Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    key: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = locks.lock().expect("codex shared locks");
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key.to_string(), Arc::downgrade(&lock));
    lock
}

impl CodexClient {
    /// The endpoint this connection dialled, for the neutral base-URL read.
    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The live turn a `/stop` should interrupt.
    ///
    /// The cache can miss a turn this process started: the subscription may
    /// begin *after* the server emitted `turn/started`, or — the race that
    /// matters — `/stop` can arrive after `turn/start` was sent but before the
    /// server persisted the turn. The thread's own persisted record is then the
    /// authority (`thread/read`'s bounded page carries its newest turns, and an
    /// `inProgress` one is the run to interrupt), and the lookup is **polled**
    /// for a short bound so a stop right after a prompt interrupts the run the
    /// moment it appears. Only a thread that still has none after the bound is
    /// genuinely idle.
    async fn live_turn_for_stop(&self, session_id: &str) -> Result<Option<String>> {
        if let Some(turn) = self.projections.live_turn(session_id) {
            return Ok(Some(turn));
        }
        for attempt in 0..STOP_LOOKUP_ATTEMPTS {
            let thread = self
                .threads()
                .read(session_id, true)
                .await
                .map_err(client_error)?;
            if let Some(turn) = thread
                .turns
                .iter()
                .rev()
                .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
                .and_then(|turn| turn.get("id").and_then(Value::as_str))
            {
                return Ok(Some(turn.to_string()));
            }
            if attempt + 1 < STOP_LOOKUP_ATTEMPTS {
                tokio::time::sleep(STOP_LOOKUP_INTERVAL).await;
            }
        }
        Ok(None)
    }

    /// Bind every pending submission this read can place: the user item whose
    /// `clientId` is a pending chosen id tells us which turn cola's accepted
    /// (but unanswered, or pre-restart) prompt landed in. This is the recovery
    /// path for a lost `turn/start` response (ADR-0092 decision 2), and it runs
    /// on the read the bridge is about to make, before it asks authorship.
    fn reconcile_submissions(&self, thread_id: &str, transcript: &SessionTranscript) {
        for (submission, message) in &transcript.submissions {
            if !self.authored.is_pending_submission(thread_id, submission) {
                continue;
            }
            let Some((owner, turn)) = self.projections.turn_of_item(message.as_str()) else {
                continue;
            };
            if owner != thread_id {
                continue;
            }
            self.authored.bind_submission(thread_id, submission, &turn);
        }
    }

    /// The `turn/start` params for one prompt. Model and effort ride the turn
    /// (and the thread's subsequent turns, per the protocol's own field
    /// documentation) rather than a durable session switch: `turn/start` is
    /// where Codex accepts them, so a `/model` or `/think` pick reaches the
    /// next turn through cola's per-session override — the same place V1 puts
    /// it, and the reason `switch_session_model` stays a no-op on this client.
    fn turn_params(
        &self,
        session_id: &str,
        text: &str,
        skills: &[PromptSkill],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<Value> {
        let mut params = json!({
            "threadId": session_id,
            "input": self.input_items(text, skills)?,
        });
        if let Some(message_id) = message_id {
            params["clientUserMessageId"] = json!(message_id);
        }
        if let Some(model) = model {
            params["model"] = json!(model.id);
        }
        if let Some(effort) = variant.or_else(|| model.and_then(|model| model.variant.as_deref())) {
            params["effort"] = json!(effort);
        }
        Ok(params)
    }

    /// The protocol's `input` array: the prompt text, then one `skill` part per
    /// attached skill. Codex's skill input needs the skill's path, so an id the
    /// last `skills/list` did not name is a named error rather than a silently
    /// dropped request (the model would otherwise answer without the skill).
    fn input_items(&self, text: &str, skills: &[PromptSkill]) -> Result<Value> {
        // `text_elements` is the protocol's spelling (`UserInput::Text` keeps
        // it snake_case on 0.163.0, unlike the surrounding camelCase fields).
        let mut input = vec![json!({ "type": "text", "text": text, "text_elements": [] })];
        for skill in skills {
            let Some(path) = self.skills.path_of(&skill.id) else {
                return Err(BridgeError::Client {
                    client: "codex",
                    detail: format!(
                        "skill `{}` is not in the app-server's skill list; send `/skill` to \
                         reload it",
                        skill.id
                    ),
                });
            };
            input.push(json!({ "type": "skill", "name": skill.name, "path": path }));
        }
        Ok(Value::Array(input))
    }

    /// Subscribe the thread if this connection has not already: `thread/resume`
    /// **is** the subscription (ADR-0090's amendment, question 2). A never-run
    /// thread is a named state, not an error; its read is an empty transcript.
    pub(crate) async fn ensure_subscription(&self, thread_id: &str) -> CodexResult<()> {
        match self.projections.state(thread_id) {
            Some(SubscriptionState::Live) => return Ok(()),
            Some(SubscriptionState::PagingIncomplete { .. }) => {
                // A read never pages. It signals the subscriber, which is the
                // only consumer, so no event can be held across a page.
                self.projections.request_resync(thread_id);
                return Ok(());
            }
            None => {}
        }
        let adoption = self.projections.adoption_lock(thread_id);
        let _adopting = adoption.lock().await;
        match self.projections.state(thread_id) {
            Some(SubscriptionState::Live) => return Ok(()),
            Some(SubscriptionState::PagingIncomplete { .. }) => {
                self.projections.request_resync(thread_id);
                return Ok(());
            }
            None => {}
        }
        // Every status this connection had already received when the history
        // request starts is superseded by the page; a status received after
        // this boundary still applies (it is newer than the page).
        let boundary = self.threads().inbound_seq();
        match self.threads().adopt(thread_id).await? {
            Adoption::Subscribed(subscription) => {
                // `thread/resume` carries only a bounded history page. A first
                // read must not present that prefix as the whole thread, so
                // page every turn before the subscriber starts folding. The
                // receiver was registered before `thread/resume`, so events
                // that arrive during this paging wait in its queue.
                match self.threads().all_turns(thread_id).await {
                    Ok(turns) => {
                        self.projections.resync(thread_id, &turns, boundary);
                        self.projections.set_state(thread_id, SubscriptionState::Live);
                    }
                    Err(error) => {
                        tracing::warn!(
                            thread = %thread_id,
                            "could not page the codex thread history on adoption; \
                             the bounded resume page is explicitly incomplete: {error}"
                        );
                        self.projections
                            .seed(thread_id, &subscription.thread().turns, boundary);
                        self.projections.mark_incomplete(thread_id);
                        self.projections.set_state(
                            thread_id,
                            SubscriptionState::PagingIncomplete {
                                retry_at: Instant::now() + PAGING_RETRY,
                            },
                        );
                    }
                }
                // Drain the events that queued while `thread/resume` and the
                // history page were in flight, so the first read reflects them
                // and cannot present a transcript as complete when the backlog
                // already overflowed the broadcast buffer.
                let mut subscription = *subscription;
                let mut drained = 0;
                let mut backlog_truncated = false;
                let mut subscription_dead = false;
                loop {
                    if drained >= self.projections.drain_limit() {
                        backlog_truncated = true;
                        break;
                    }
                    match subscription.try_next_event() {
                        Ok(Some(ThreadEvent::Notification { seq, method, params })) => {
                            self.projections.apply_at(thread_id, seq, &method, &params);
                            drained += 1;
                        }
                        Ok(Some(ThreadEvent::Lagged { skipped })) => {
                            tracing::warn!(
                                thread = %thread_id,
                                skipped,
                                "the adoption backlog overflowed the event buffer; marking the \
                                 projection incomplete"
                            );
                            backlog_truncated = true;
                            break;
                        }
                        Ok(None) => break,
                        Err(error) => {
                            tracing::warn!(
                                thread = %thread_id,
                                "the adoption backlog ended before it drained: {error}"
                            );
                            backlog_truncated = true;
                            subscription_dead = true;
                            break;
                        }
                    }
                }
                if subscription_dead {
                    // No live subscription is left to page through: forget the
                    // state so the next read re-adopts rather than sitting in a
                    // paging retry against a dead connection.
                    self.projections.forget(thread_id);
                } else if backlog_truncated {
                    // A limit hit (or a lag) leaves the projection incomplete
                    // with no later lag to schedule a resync. Enter the
                    // paging-retry state so a later read recovers instead of
                    // staying truncated forever.
                    self.projections.mark_incomplete(thread_id);
                    self.projections.set_state(
                        thread_id,
                        SubscriptionState::PagingIncomplete {
                            retry_at: Instant::now() + PAGING_RETRY,
                        },
                    );
                }
                let task = tokio::spawn(consume(
                    subscription,
                    self.threads().clone(),
                    Arc::downgrade(&self.projections),
                    self.projections.resync_signal(thread_id),
                ));
                self.projections.track(task);
                Ok(())
            }
            Adoption::NoHistoryYet => {
                // A thread created by `thread/start` cannot be resumed until
                // it has run a turn. That turn may be started by the user's
                // own client, so the state is NOT remembered as terminal: the
                // next read retries, and the subscription starts the moment
                // the rollout exists (the peer-attachment promise).
                Ok(())
            }
        }
    }
}

/// Fold one subscription's events into the cache. Notifications are applied
/// inline; a lag or a lost connection is explicitly surfaced.
async fn consume(
    mut subscription: ThreadSubscription,
    threads: Threads,
    projections: std::sync::Weak<Projections>,
    signal: Arc<tokio::sync::Notify>,
) {
    let thread_id = subscription.thread().id.clone();
    loop {
        // The subscriber is the only consumer, so an event is either dequeued
        // and applied immediately or left queued while this task pages. It can
        // never be in hand across a page and applied twice.
        tokio::select! {
            biased;
            _ = signal.notified() => {
                let due = projections.upgrade().is_some_and(|projections| {
                    matches!(
                        projections.state(&thread_id),
                        Some(SubscriptionState::PagingIncomplete { .. })
                    )
                });
                if due {
                    resync(&threads, &projections, &thread_id).await;
                }
            }
            event = subscription.next_event() => match event {
                Ok(ThreadEvent::Notification { seq, method, params }) => {
                    let Some(projections) = projections.upgrade() else {
                        return;
                    };
                    projections.apply_at(&thread_id, seq, &method, &params);
                }
                Ok(ThreadEvent::Lagged { skipped }) => {
                    tracing::warn!(
                        thread = %thread_id,
                        skipped,
                        "the codex push stream dropped events; resyncing the projection from paged turns"
                    );
                    resync(&threads, &projections, &thread_id).await;
                }
                Err(CodexError::SubscriptionLagged { skipped, .. }) => {
                    tracing::warn!(
                        thread = %thread_id,
                        skipped,
                        "the codex subscription fell behind; resyncing the projection from paged turns"
                    );
                    resync(&threads, &projections, &thread_id).await;
                }
                Err(error) => {
                    tracing::warn!(
                        thread = %thread_id,
                        "the codex thread subscription ended: {error}"
                    );
                    if let Some(projections) = projections.upgrade() {
                        projections.forget(&thread_id);
                    }
                    return;
                }
            }
        }
    }
}

/// Rebuild a projection from persisted turns. A failed read leaves the
/// projection marked incomplete — the gap stays visible rather than vanishing.
///
/// `thread/read` carries only a bounded page, so the resync pages
/// `thread/turns/list` to exhaustion. The cache handle is re-upgraded around
/// each step rather than held across the network await: a replaced attachment
/// can still drop the cache (and abort this task) while a request is in flight.
async fn resync(threads: &Threads, projections: &std::sync::Weak<Projections>, thread_id: &str) {
    if let Some(projections) = projections.upgrade() {
        projections.mark_incomplete(thread_id);
    }
    let boundary = threads.inbound_seq();
    match threads.all_turns(thread_id).await {
        Ok(turns) => {
            if let Some(projections) = projections.upgrade() {
                projections.resync(thread_id, &turns, boundary);
                // A lag resync recovers the history a failed adoption page
                // could not; a later read need not retry paging.
                projections.set_state(thread_id, SubscriptionState::Live);
            }
        }
        Err(error) => {
            tracing::warn!(
                thread = %thread_id,
                "could not resync the codex projection from thread/turns/list: {error}"
            );
            if let Some(projections) = projections.upgrade() {
                projections.mark_incomplete(thread_id);
                projections.set_state(
                    thread_id,
                    SubscriptionState::PagingIncomplete {
                        retry_at: Instant::now() + PAGING_RETRY,
                    },
                );
            }
        }
    }
}

/// Bound the `/stop` live-turn lookup: a `turn/start` the server accepts is
/// persisted within tens of milliseconds, so a stop that lands just before the
/// turn exists polls this long before it declares the session idle. The
/// interval is shorter in tests so the suite does not pay the wall clock.
const STOP_LOOKUP_ATTEMPTS: u32 = 10;

#[cfg(not(test))]
const STOP_LOOKUP_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(test)]
const STOP_LOOKUP_INTERVAL: Duration = Duration::from_millis(10);

/// The provider bucket the `/model` picker shows for Codex. `model/list` names
/// no provider: a Codex model is whatever the attached app-server resolves
/// through its own `[model_providers.*]`, and the picker's `provider/model`
/// ref is cola's neutral mirror, so a client-scoped bucket is the honest label
/// — never a claim that the model is served by OpenAI.
const MODEL_PROVIDER: &str = "codex";

/// Bound the `/model` catalog read. A server that keeps minting fresh cursors
/// would otherwise hold the picker forever; past the bound the catalog is what
/// was read so far, and the truncation is logged.
const MAX_MODEL_PAGES: usize = 64;

/// The `skills/list` reads, scoped by the directory the read was made for.
/// Codex's `skill` prompt part carries a **path** as well as a name, and the
/// neutral [`crate::backend::PromptSkill`] only carries `{ id, name }`, so the
/// path is remembered here from the same read the `/skill` command made
/// immediately before the prompt.
///
/// Each read **replaces its own scope**: a skill deleted between two lists must
/// stop resolving (the prompt then fails by name, never sending a stale path),
/// while a read for another project cannot erase this one's entries. A name
/// present in two scopes resolves to the most recently read scope — the one the
/// `/skill` command's read just refreshed.
#[derive(Default)]
pub(crate) struct SkillCatalog {
    /// Directory key (`""` for the server default) → (skill name → path).
    scopes: Mutex<HashMap<String, ScopeSkills>>,
    /// Read order, so a name present in several scopes picks the newest one.
    next_read: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct ScopeSkills {
    read_seq: u64,
    paths: HashMap<String, String>,
}

impl SkillCatalog {
    /// Replace one directory's scope with what its latest read advertised.
    /// `skills` empty is a real "this directory has none" answer: the scope is
    /// emptied, so a vanished skill is refused rather than resolved stale.
    fn replace_scope(&self, directory: Option<&str>, skills: impl IntoIterator<Item = (String, String)>) {
        let key = directory.unwrap_or_default().to_string();
        let seq = self.next_read.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let paths: HashMap<String, String> = skills.into_iter().collect();
        let mut scopes = self.scopes.lock().expect("codex skill catalog");
        if paths.is_empty() {
            scopes.remove(&key);
        } else {
            scopes.insert(key, ScopeSkills { read_seq: seq, paths });
        }
    }

    fn path_of(&self, name: &str) -> Option<String> {
        let scopes = self.scopes.lock().expect("codex skill catalog");
        scopes
            .values()
            .filter_map(|scope| scope.paths.get(name).map(|path| (scope.read_seq, path.clone())))
            .max_by_key(|(seq, _)| *seq)
            .map(|(_, path)| path)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnStartResponse {
    turn: TurnRef,
}

#[derive(Debug, Deserialize)]
struct TurnRef {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelListResponse {
    #[serde(default)]
    data: Vec<ModelEntry>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelEntry {
    id: String,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    supported_reasoning_efforts: Vec<EffortOption>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffortOption {
    reasoning_effort: String,
}

#[derive(Debug, Deserialize)]
struct SkillsListResponse {
    #[serde(default)]
    data: Vec<SkillsEntry>,
}

#[derive(Debug, Deserialize)]
struct SkillsEntry {
    #[serde(default)]
    skills: Vec<SkillMetadata>,
    #[serde(default)]
    errors: Vec<SkillError>,
}

#[derive(Debug, Deserialize)]
struct SkillMetadata {
    name: String,
    #[serde(default)]
    description: String,
    path: String,
}

#[derive(Debug, Deserialize)]
struct SkillError {
    #[serde(default)]
    path: String,
    #[serde(default)]
    message: String,
}

/// Decode one method's result into its typed shape; a changed payload is a
/// named protocol failure, never a silently empty answer.
fn decode<T: serde::de::DeserializeOwned>(method: &str, value: Value) -> CodexResult<T> {
    serde_json::from_value(value).map_err(|error| CodexError::Protocol {
        method: method.to_string(),
        detail: error.to_string(),
    })
}

/// Whether the server **definitely refused** a request: it answered with a
/// JSON-RPC error, so the work did not start. Every other failure — a closed
/// connection, a timeout, an undecodable result — is ambiguous (the request may
/// have been accepted and only its answer lost), and an authorship marker for
/// an ambiguous submission must stay reconcilable (ADR-0092).
fn definitely_refused(error: &CodexError) -> bool {
    matches!(error, CodexError::Rpc { .. })
}

fn client_error(error: CodexError) -> BridgeError {
    BridgeError::Client {
        client: "codex",
        detail: error.to_string(),
    }
}

fn not_attached() -> BridgeError {
    BridgeError::Client {
        client: "codex",
        detail: "not attached to a codex app-server daemon".to_string(),
    }
}

fn neutral_session_input(directory: Option<&str>) -> CreateSessionInput {
    CreateSessionInput {
        id: None,
        agent: None,
        model: None,
        location: directory.map(|directory| Location {
            directory: directory.to_string(),
        }),
    }
}

#[async_trait]
impl ServerLifecycle for CodexClient {
    /// One attached connection is already attached: there is no discovery to
    /// run here. The active client's lifecycle is [`CodexLifecycle`].
    async fn ensure_attached(&self, _request: EnsureAttached<'_>) -> Result<AttachOutcome> {
        Ok(AttachOutcome::Attached)
    }
}

#[async_trait]
impl Backend for CodexClient {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
        neutral_session_input(directory)
    }

    async fn create_session(&self, input: &CreateSessionInput) -> Result<Session> {
        if input.id.is_some() {
            // Codex assigns the thread id itself; a caller-supplied one cannot
            // be honoured without inventing an identity the server did not
            // issue.
            return Err(BridgeError::Client {
                client: "codex",
                detail: "thread/start assigns the thread id; a caller-supplied session id is not \
                         supported by this adapter"
                    .to_string(),
            });
        }
        let cwd = input
            .location
            .as_ref()
            .map(|location| location.directory.as_str())
            .filter(|directory| !directory.is_empty());
        let thread = self.threads().start(cwd).await.map_err(client_error)?;
        Ok(Session {
            id: thread.id.clone(),
            project_id: None,
            agent: None,
            title: (!thread.title().is_empty()).then(|| thread.title().to_string()),
            location: Some(json!({ "directory": thread.cwd })),
            cost: None,
            time: Some(crate::backend::types::SessionTime {
                created: thread.created_at,
                updated: thread.updated_at,
                archived: None,
            }),
        })
    }

    async fn list_sessions(&self) -> Result<Vec<SessionListInfo>> {
        self.threads().list_all_any().await.map_err(client_error)
    }

    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.threads()
            .rename(session_id, title)
            .await
            .map_err(client_error)
    }

    async fn delete_session(&self, session_id: &str) -> Result<()> {
        self.connection()
            .request("thread/delete", json!({ "threadId": session_id }))
            .await
            .map_err(client_error)?;
        self.projections.forget(session_id);
        // The thread is gone; its authorship turns cannot be asked about again,
        // and a recreated id must not inherit them.
        self.authored.forget(session_id);
        Ok(())
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        files: &[PromptFile],
        skills: &[PromptSkill],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        _agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()> {
        if !files.is_empty() {
            return Err(BridgeError::Client {
                client: "codex",
                detail: "prompt attachments are not mapped onto this adapter yet".to_string(),
            });
        }
        // `turn/start` is the write half of this ticket's read model; a failed
        // submit is its own error, while the Turn's outcome is observed from
        // the transcript and session status (ADR-0011).
        //
        // The params are built BEFORE the pending marker: a pre-send validation
        // failure (an unknown skill) must not leave a marker behind, or a later
        // external live turn on this thread could be classified cola's.
        let params = self.turn_params(session_id, text, skills, model, variant, message_id)?;
        // The turn id is Codex's, so the authorship record is bound from the
        // response. The chosen submission id is recorded durably BEFORE the
        // request: the server may emit the turn's user item before this task
        // processes the response, and an accepted request whose response is
        // lost must still be recognisable — by that id — after a restart.
        self.authored.begin(session_id, message_id);
        let result = match self.connection().request("turn/start", params).await {
            Ok(result) => result,
            Err(error) => {
                if definitely_refused(&error) || message_id.is_none() {
                    // A definite refusal did not start the turn. A submission
                    // with no chosen id cannot be reconciled by a later read
                    // either, so its in-memory marker is always cleared when
                    // the call returns — otherwise it would vouch for a later
                    // external live turn forever.
                    self.authored.abandon(session_id, message_id);
                }
                // An ambiguous failure (a closed connection, no response) keeps
                // a chosen id: the server may have accepted the turn, and the
                // next history read binds it by that id.
                return Err(client_error(error));
            }
        };
        let turn = match decode::<TurnStartResponse>("turn/start", result) {
            Ok(response) => response.turn,
            Err(error) => {
                // The server answered, but the payload did not name the turn:
                // it may have started one, so a chosen id's pending marker
                // stays for the next read. An anonymous one cannot be
                // reconciled, so it does not linger.
                if message_id.is_none() {
                    self.authored.abandon(session_id, message_id);
                }
                return Err(client_error(error));
            }
        };
        // A prompt whose response names an already-active turn (the spike's
        // fold) binds that turn: it is the turn cola's message landed in, and
        // the card must settle on it. Never a second turn.
        self.authored.bind(session_id, &turn.id, message_id);
        Ok(())
    }

    /// `turn/steer` merges the message into the active turn. Its protocol
    /// precondition is the active turn's id; when this adapter has not observed
    /// one (the `turn/started` notification has not been folded yet, or the
    /// run already ended), a `turn/start` is the correct fallback — the server
    /// folds it into the active turn or starts a fresh one, never a second
    /// competing turn.
    #[allow(clippy::too_many_arguments)]
    async fn steer(
        &self,
        session_id: &str,
        text: &str,
        files: &[PromptFile],
        skills: &[PromptSkill],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()> {
        let Some(turn_id) = self.projections.live_turn(session_id) else {
            return self
                .prompt(session_id, text, files, skills, model, variant, agent, message_id)
                .await;
        };
        if !files.is_empty() {
            return Err(BridgeError::Client {
                client: "codex",
                detail: "prompt attachments are not mapped onto this adapter yet".to_string(),
            });
        }
        let input = self.input_items(text, skills)?;
        let mut params = json!({
            "threadId": session_id,
            "expectedTurnId": turn_id,
            "input": input,
        });
        if let Some(message_id) = message_id {
            params["clientUserMessageId"] = json!(message_id);
        }
        // No `model`/`effort` here: a steer adds input to a turn that already
        // captured its settings. The turn is cola's — the bridge only steers
        // while it owns the live execution — so no new provenance is recorded.
        self.connection()
            .request("turn/steer", params)
            .await
            .map_err(client_error)?;
        Ok(())
    }

    async fn is_cola_authored(&self, message_id: &str) -> bool {
        let Some((thread, turn)) = self.projections.turn_of_item(message_id) else {
            // Not in the cache: nothing cola can vouch for, unless the record
            // itself is unusable — then everything is cola's (fail closed).
            return !self.authored.is_trusted();
        };
        if self.authored.is_authored(&thread, Some(&turn)) {
            return true;
        }
        // The provenance answer before the turn is bound. An item carrying the
        // chosen submission id is cola's only if that id is still pending — no
        // other client's message can match it. An item with no client id falls
        // back to the in-flight marker, which is only consulted for the
        // thread's live turn (ADR-0092 decision 2).
        match self.projections.submission_of_item(&thread, message_id) {
            Some(submission) => self.authored.is_pending_submission(&thread, &submission),
            None => {
                self.authored.is_pending(&thread)
                    && self.projections.live_turn(&thread).as_deref() == Some(turn.as_str())
            }
        }
    }

    async fn reply_permission(
        &self,
        _session_id: &str,
        _request_id: &str,
        _reply: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        // The approval surface (and its neutral DTOs, decision 7) is #882.
        // Nothing lists a request on this adapter yet, so there is nothing to
        // reply to.
        Ok(())
    }

    async fn list_permissions(&self, _directory: Option<&str>) -> Result<Vec<PermissionRequest>> {
        Ok(Vec::new())
    }

    async fn list_questions(&self, _directory: Option<&str>) -> Result<Vec<QuestionRequest>> {
        Ok(Vec::new())
    }

    async fn reply_question(
        &self,
        _session_id: &str,
        _request_id: &str,
        _answers: &[FormAnswer],
        _directory: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }

    async fn reject_question(
        &self,
        _session_id: &str,
        _request_id: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }

    async fn transcript(&self, session_id: &str) -> Result<SessionTranscript> {
        self.ensure_subscription(session_id).await.map_err(client_error)?;
        let transcript = self.projections.transcript(session_id);
        // The read is where a lost `turn/start` response is reconciled: a
        // pending chosen id binds to the turn whose user item carries it, so
        // the authorship answer is exact even before the next prompt.
        self.reconcile_submissions(session_id, &transcript);
        Ok(transcript)
    }

    async fn session_diff(
        &self,
        session_id: &str,
        from: Option<&str>,
        _to: Option<&str>,
        _context: Option<u32>,
    ) -> Result<Option<TurnDiff>> {
        Ok(self.projections.diff(session_id, from))
    }

    async fn session_status(
        &self,
        session_id: &str,
        _directory: Option<&str>,
    ) -> Result<Option<SessionStatus>> {
        // The cache is not the server's status service: an unseeded projection
        // answering `Idle` would present "never observed" as an authoritative
        // idle run. A `thread/status/changed` broadcast is already the server's
        // own run state for the thread — subscribed or not — so a status it
        // reported is answered without adopting (Session Sync must not resume a
        // thread just to read its state). Otherwise adopt the thread first
        // (which seeds its live turn from the resume history); a never-run
        // thread then genuinely has no live run.
        if self.projections.observed_status(session_id).is_none() && !self.projections.is_seeded(session_id) {
            self.ensure_subscription(session_id).await.map_err(client_error)?;
        }
        Ok(self.projections.status(session_id))
    }

    fn tasks(&self) -> &TasksHandle {
        &self.tasks
    }

    /// Codex's app-server publishes no Background Task surface: its items are
    /// turn-scoped tool calls, with no shell records, child-session registry or
    /// runtime verdict endpoint. The Bridge therefore renders no ledger for a
    /// Codex session and spends no runtime reconcile read on it.
    fn background_tasks_supported(&self) -> bool {
        false
    }

    async fn model_context_window(&self, _provider: &str, _model: &str) -> Result<Option<i64>> {
        // The Codex usage notification states the window for the running
        // model; a read with no attached turn has no figure to report.
        Ok(self.projections.any_context_window())
    }

    fn configured_default_model(&self) -> Option<ModelInfo> {
        // The Codex model picker is a later ticket; no config model exists to
        // report, and a guess would be worse than the server default.
        None
    }

    async fn list_agents(&self) -> Vec<AgentInfo> {
        Vec::new()
    }

    async fn list_skills(&self, directory: Option<&str>) -> Vec<SkillInfo> {
        // A project's skills live under its own cwd, so the read is scoped to
        // the directory the session runs in; `None` keeps the server default.
        let mut params = json!({});
        if let Some(directory) = directory.filter(|directory| !directory.is_empty()) {
            params["cwds"] = json!([directory]);
        }
        let result = match self.connection().request("skills/list", params).await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!("codex skills/list failed; the picker degrades to no skills: {error}");
                return Vec::new();
            }
        };
        let response: SkillsListResponse = match decode("skills/list", result) {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!("codex skills/list did not decode: {error}");
                return Vec::new();
            }
        };
        let mut skills = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut catalog = Vec::new();
        for entry in response.data {
            for error in entry.errors {
                tracing::warn!(
                    path = %error.path,
                    "codex reported a skill it could not load: {}",
                    error.message
                );
            }
            for skill in entry.skills {
                catalog.push((skill.name.clone(), skill.path.clone()));
                if !seen.insert(skill.name.clone()) {
                    continue;
                }
                // Codex has no separate skill id: the name is what a prompt's
                // `skill` input names, so it IS the neutral id (V1's rule).
                skills.push(SkillInfo {
                    id: skill.name.clone(),
                    name: skill.name,
                    description: (!skill.description.trim().is_empty()).then_some(skill.description),
                    // The list route carries no body; the loaded-skill card's
                    // content fold therefore has none to show.
                    content: None,
                });
            }
        }
        self.skills.replace_scope(directory, catalog);
        skills
    }

    async fn list_models(&self) -> Vec<ProviderModels> {
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        let mut models = Vec::new();
        for page in 0..MAX_MODEL_PAGES {
            let mut params = json!({});
            if let Some(cursor) = cursor.as_deref() {
                params["cursor"] = json!(cursor);
            }
            let result = match self.connection().request("model/list", params).await {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!("codex model/list failed; the picker degrades: {error}");
                    break;
                }
            };
            let response: ModelListResponse = match decode("model/list", result) {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!("codex model/list did not decode: {error}");
                    break;
                }
            };
            for model in response.data {
                if model.hidden {
                    continue;
                }
                models.push(ModelOption {
                    id: model.id,
                    // The server's own reasoning-effort set rides the model
                    // entry (ADR-0020: each model declares its own scale).
                    variants: model
                        .supported_reasoning_efforts
                        .into_iter()
                        .map(|effort| effort.reasoning_effort)
                        .collect(),
                });
            }
            match response.next_cursor {
                None => break,
                Some(next) => {
                    if !seen.insert(next.clone()) {
                        tracing::warn!(
                            "codex model/list repeated pagination cursor `{next}`; \
                             showing the catalog read so far"
                        );
                        break;
                    }
                    cursor = Some(next);
                }
            }
            if page + 1 == MAX_MODEL_PAGES {
                tracing::warn!("codex model/list did not finish in {MAX_MODEL_PAGES} pages");
            }
        }
        if models.is_empty() {
            return Vec::new();
        }
        vec![ProviderModels {
            provider: MODEL_PROVIDER.to_string(),
            models,
        }]
    }

    async fn session_info(&self, session_id: &str, _directory: Option<&str>) -> Result<SessionInfo> {
        let thread = self
            .threads()
            .read(session_id, false)
            .await
            .map_err(client_error)?;
        let title = (!thread.title().is_empty()).then(|| thread.title().to_string());
        Ok(SessionInfo {
            id: thread.id,
            parent_id: thread.parent_thread_id,
            title,
            model: None,
        })
    }

    async fn interrupt(&self, session_id: &str) -> Result<()> {
        let Some(turn_id) = self.live_turn_for_stop(session_id).await? else {
            // No turn has been observed even after adopting the thread:
            // nothing to interrupt. `/stop` on an idle session is a no-op,
            // exactly as the bridge expects.
            return Ok(());
        };
        self.connection()
            .request(
                "turn/interrupt",
                json!({ "threadId": session_id, "turnId": turn_id }),
            )
            .await
            .map_err(client_error)?;
        Ok(())
    }

    async fn compact(&self, session_id: &str) -> Result<()> {
        // Mark the request before it goes out: the compaction's item can reach
        // the subscription before this task processes the response.
        self.projections.mark_manual_compaction(session_id);
        if let Err(error) = self
            .connection()
            .request("thread/compact/start", json!({ "threadId": session_id }))
            .await
        {
            // Only a definite refusal proves no compaction started, so only it
            // clears the marker: an ambiguous failure (a lost response after
            // acceptance) must still title the item 手动. An item that never
            // arrives leaves the marker for the thread's next compaction; the
            // next `/compact` overwrites it, so it cannot linger across a
            // request that failed at the transport.
            if definitely_refused(&error) {
                self.projections.clear_manual_compaction(session_id);
            }
            return Err(client_error(error));
        }
        Ok(())
    }

    fn base_url(&self) -> String {
        self.endpoint().display_url()
    }

    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn crate::backend::DirectoryBackend> {
        Arc::new(BackendDirectory::new(self, directory.to_string()))
    }
}

/// The active client the composition root installs in `SharedCore.client`:
/// the lifecycle owns discovery/attach, and the `Backend` surface delegates to
/// the client the last attach pass installed.
#[async_trait]
impl Backend for CodexLifecycle {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
        neutral_session_input(directory)
    }

    async fn create_session(&self, input: &CreateSessionInput) -> Result<Session> {
        Backend::create_session(&self.current().await?, input).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionListInfo>> {
        Backend::list_sessions(&self.current().await?).await
    }

    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        Backend::update_session_title(&self.current().await?, session_id, title).await
    }

    async fn delete_session(&self, session_id: &str) -> Result<()> {
        Backend::delete_session(&self.current().await?, session_id).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        files: &[PromptFile],
        skills: &[PromptSkill],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()> {
        Backend::prompt(
            &self.current().await?,
            session_id,
            text,
            files,
            skills,
            model,
            variant,
            agent,
            message_id,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn steer(
        &self,
        session_id: &str,
        text: &str,
        files: &[PromptFile],
        skills: &[PromptSkill],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()> {
        Backend::steer(
            &self.current().await?,
            session_id,
            text,
            files,
            skills,
            model,
            variant,
            agent,
            message_id,
        )
        .await
    }

    async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> Result<()> {
        Backend::reply_permission(&self.current().await?, session_id, request_id, reply, directory).await
    }

    async fn list_permissions(&self, directory: Option<&str>) -> Result<Vec<PermissionRequest>> {
        Backend::list_permissions(&self.current().await?, directory).await
    }

    async fn list_questions(&self, directory: Option<&str>) -> Result<Vec<QuestionRequest>> {
        Backend::list_questions(&self.current().await?, directory).await
    }

    async fn reply_question(
        &self,
        session_id: &str,
        request_id: &str,
        answers: &[FormAnswer],
        directory: Option<&str>,
    ) -> Result<()> {
        Backend::reply_question(&self.current().await?, session_id, request_id, answers, directory).await
    }

    async fn reject_question(
        &self,
        session_id: &str,
        request_id: &str,
        directory: Option<&str>,
    ) -> Result<()> {
        Backend::reject_question(&self.current().await?, session_id, request_id, directory).await
    }

    async fn transcript(&self, session_id: &str) -> Result<SessionTranscript> {
        Backend::transcript(&self.current().await?, session_id).await
    }

    async fn session_diff(
        &self,
        session_id: &str,
        from: Option<&str>,
        to: Option<&str>,
        context: Option<u32>,
    ) -> Result<Option<TurnDiff>> {
        Backend::session_diff(&self.current().await?, session_id, from, to, context).await
    }

    async fn session_status(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionStatus>> {
        Backend::session_status(&self.current().await?, session_id, directory).await
    }

    fn tasks(&self) -> &TasksHandle {
        &self.tasks
    }

    /// Every Codex client this lifecycle can attach has the same surface: the
    /// app-server publishes no Background Tasks, so this is false before and
    /// after an attach.
    fn background_tasks_supported(&self) -> bool {
        false
    }

    async fn model_context_window(&self, provider: &str, model: &str) -> Result<Option<i64>> {
        Backend::model_context_window(&self.current().await?, provider, model).await
    }

    fn configured_default_model(&self) -> Option<ModelInfo> {
        None
    }

    async fn list_agents(&self) -> Vec<AgentInfo> {
        Vec::new()
    }

    async fn list_skills(&self, directory: Option<&str>) -> Vec<SkillInfo> {
        match self.attached_client().await {
            Some(client) => Backend::list_skills(&client, directory).await,
            None => Vec::new(),
        }
    }

    async fn list_models(&self) -> Vec<ProviderModels> {
        match self.attached_client().await {
            Some(client) => Backend::list_models(&client).await,
            None => Vec::new(),
        }
    }

    async fn is_cola_authored(&self, message_id: &str) -> bool {
        // The answer needs the item→turn mapping, which only a live
        // projection has; a call with no attachment cannot name a message this
        // process was handed, so there is nothing to vouch for.
        match self.attached_client().await {
            Some(client) => Backend::is_cola_authored(&client, message_id).await,
            None => false,
        }
    }

    async fn session_info(&self, session_id: &str, directory: Option<&str>) -> Result<SessionInfo> {
        Backend::session_info(&self.current().await?, session_id, directory).await
    }

    async fn interrupt(&self, session_id: &str) -> Result<()> {
        Backend::interrupt(&self.current().await?, session_id).await
    }

    async fn compact(&self, session_id: &str) -> Result<()> {
        Backend::compact(&self.current().await?, session_id).await
    }

    fn base_url(&self) -> String {
        // The control socket is the stable target even while serverless: a
        // replacement daemon keeps the same path, and the lifecycle's liveness
        // pass (not this string) decides whether it is the same server.
        format!("unix://{}", self.environment_socket().display())
    }

    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn crate::backend::DirectoryBackend> {
        Arc::new(BackendDirectory::new(self, directory.to_string()))
    }
}

impl CodexLifecycle {
    async fn current(&self) -> Result<CodexClient> {
        self.attached_client().await.ok_or_else(not_attached)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::codex::client::CodexOptions;
    use crate::codex::schema::PinnedProbe;
    use crate::codex::test_server::{FakeServer, ScriptedThread};
    use crate::codex::transport::Endpoint;

    const RECORDING: &str = include_str!("fixtures/turn-recording.json");

    async fn connect(url: &str) -> CodexClient {
        let endpoint = Endpoint::parse(url).expect("endpoint");
        CodexClient::connect(&CodexOptions::new(endpoint), &PinnedProbe)
            .await
            .expect("attach to the scripted app-server")
    }

    async fn connect_with(url: &str, authored: Arc<crate::codex::Authorship>) -> CodexClient {
        let endpoint = Endpoint::parse(url).expect("endpoint");
        CodexClient::connect(
            &CodexOptions::new(endpoint).with_authorship(authored),
            &PinnedProbe,
        )
        .await
        .expect("attach to the scripted app-server")
    }

    /// Wait until the projection has folded the live turn a prompt started.
    async fn wait_for_live_turn(client: &CodexClient, thread_id: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while client.projections.live_turn(thread_id).is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the prompt's live turn never reached the projection"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn recording() -> Vec<Value> {
        serde_json::from_str(RECORDING).expect("the recording is a JSON array")
    }

    /// One persisted turn in the fake server's history: one agent-message
    /// item, with the turn status the projection reads.
    fn scripted_turn(id: &str, status: &str, started_at: i64, item_id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "status": status,
            "startedAt": started_at,
            "completedAt": (status != "inProgress").then_some(started_at + 1),
            "error": null,
            "items": [{ "type": "agentMessage", "id": item_id, "text": text }],
        })
    }

    fn tokens_total(transcript: &SessionTranscript) -> Option<i64> {
        transcript
            .messages
            .iter()
            .filter_map(|message| message.tokens.map(|tokens| tokens.total))
            .next()
    }

    /// Wait for one token-usage total. The fixture's usage notification is its
    /// last data event, so a matching total is the replay's terminal sentinel:
    /// nothing earlier in that pass remains in flight.
    async fn wait_for_tokens(client: &CodexClient, thread_id: &str, total: i64) -> SessionTranscript {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let transcript = client.transcript(thread_id).await.expect("transcript");
            if tokens_total(&transcript) == Some(total) {
                return transcript;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for token usage {total}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_text(client: &CodexClient, thread_id: &str, needle: &str) -> SessionTranscript {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let transcript = client.transcript(thread_id).await.expect("transcript");
            if transcript
                .messages
                .iter()
                .any(|message| message.text().contains(needle))
            {
                return transcript;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for `{needle}`"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait until the broadcast status for `thread_id` is `expected` — the
    /// status consumer is a task of its own, so an assertion on its next tick
    /// would race it.
    async fn wait_for_observed_status(client: &CodexClient, thread_id: &str, expected: Option<ThreadStatus>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if client.projections.observed_status(thread_id) == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for status {expected:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Ticket #879: the recorded notification fixture replays through the real
    /// connection, and two reads deliver the same transcript with no duplicate
    /// items.
    #[tokio::test]
    async fn a_recorded_fixture_replays_over_the_subscription_without_duplicates() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;

        // The first read subscribes the thread; no history yet, so it is empty.
        assert!(
            client
                .transcript("thread-1")
                .await
                .expect("the empty read")
                .messages
                .is_empty()
        );
        server.wait_for_subscribers("thread-1", 1).await;

        for event in recording() {
            let method = event["method"].as_str().expect("recorded method");
            let pushed = server.push_to_thread("thread-1", method, event["params"].clone());
            assert_eq!(pushed, 1, "the subscriber receives `{method}`");
        }

        // Synchronise on the fixture's terminal usage marker, then on the
        // completed turn: agent text alone arrives several events too early and
        // would compare a mid-replay snapshot.
        let first = wait_for_tokens(&client, "thread-1", 12_345).await;
        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("session status"),
            Some(SessionStatus::Idle)
        );
        assert!(!first.truncated, "a complete recording is not a prefix");

        // Replay the whole recording over the live subscription, then push a
        // sentinel usage update: WebSocket order means seeing the sentinel
        // proves the second pass was fully folded, not merely that a stale
        // read happened to match.
        for event in recording() {
            let method = event["method"].as_str().unwrap();
            server.push_to_thread("thread-1", method, event["params"].clone());
        }
        server.push_to_thread(
            "thread-1",
            "thread/tokenUsage/updated",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "tokenUsage": {
                    "total": {
                        "totalTokens": 24690,
                        "inputTokens": 16000,
                        "cachedInputTokens": 8000,
                        "cacheWriteInputTokens": 0,
                        "outputTokens": 690,
                        "reasoningOutputTokens": 100
                    },
                    "last": {
                        "totalTokens": 99999,
                        "inputTokens": 1,
                        "cachedInputTokens": 0,
                        "cacheWriteInputTokens": 0,
                        "outputTokens": 1,
                        "reasoningOutputTokens": 0
                    },
                    "modelContextWindow": 128000
                }
            }),
        );
        let second = wait_for_tokens(&client, "thread-1", 99_999).await;
        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("session status"),
            Some(SessionStatus::Idle)
        );

        // The only expected difference is the sentinel usage; the second
        // replay must not add a message or double any streamed text.
        let mut first_messages = first.messages.clone();
        let mut second_messages = second.messages.clone();
        for message in first_messages.iter_mut().chain(second_messages.iter_mut()) {
            message.tokens = None;
        }
        assert_eq!(
            first_messages, second_messages,
            "a replayed recording produces the same transcript"
        );
        let ids: std::collections::HashSet<&str> = second
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(
            second.messages.len(),
            ids.len(),
            "a replayed read must not duplicate an item"
        );
        assert_eq!(
            second
                .messages
                .iter()
                .find(|message| message.id.as_str() == "item-agent")
                .unwrap()
                .text(),
            "The fix",
            "the replay must not double the streamed text"
        );

        // The turn boundary reached the status read: completed is idle.
        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("session status"),
            Some(SessionStatus::Idle)
        );
    }

    /// Contract finding: `session_status` is the server's live run state, so
    /// the first call on a busy thread must adopt and seed before answering —
    /// never present an unseeded cache as `Idle`.
    #[tokio::test]
    async fn a_status_first_read_on_a_busy_thread_is_busy() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a real thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "inProgress",
            1_700_000_000,
            "item-live",
            "working",
        )];
        server.seed_thread(thread);
        let client = connect(&url).await;

        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("status read"),
            Some(SessionStatus::Busy),
            "the status read seeded the live turn instead of guessing idle"
        );
        assert_eq!(server.resume_calls(), 1);
        server.wait_for_subscribers("thread-1", 1).await;
    }

    /// Ticket #880 acceptance 2: Session Sync's status read is served by the
    /// `thread/status/changed` broadcast — which reaches every connection,
    /// subscribed or not — so a session whose thread cola never resumed still
    /// reports Busy/Idle. No `thread/resume` may be sent.
    #[tokio::test]
    async fn session_status_is_fed_by_the_status_broadcast_without_subscribing() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a thread",
        ));
        let client = connect(&url).await;

        server.push_notification(
            "thread/status/changed",
            json!({
                "threadId": "thread-1",
                "status": { "type": "active", "activeFlags": [] },
            }),
        );
        wait_for_observed_status(&client, "thread-1", Some(ThreadStatus::Active)).await;

        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("status read"),
            Some(SessionStatus::Busy),
            "the broadcast's active status reads as busy"
        );
        assert_eq!(
            server.resume_calls(),
            0,
            "Session Sync must not subscribe a thread just to read its status"
        );

        server.push_notification(
            "thread/status/changed",
            json!({ "threadId": "thread-1", "status": { "type": "idle" } }),
        );
        wait_for_observed_status(&client, "thread-1", Some(ThreadStatus::Idle)).await;

        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("status read"),
            Some(SessionStatus::Idle),
            "a discarded run reads as idle"
        );
        assert_eq!(server.resume_calls(), 0);
    }

    /// Ticket #880 review finding: a closed connection cannot vouch for a
    /// cached run state. The status watcher clears it, so a later read adopts
    /// (or fails loudly) instead of reporting a stale busy forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_closed_connection_clears_its_cached_status() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a thread",
        ));
        let client = connect(&url).await;
        server.push_notification(
            "thread/status/changed",
            json!({ "threadId": "thread-1", "status": { "type": "active", "activeFlags": [] } }),
        );
        wait_for_observed_status(&client, "thread-1", Some(ThreadStatus::Active)).await;

        server.die().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while client.projections.observed_status("thread-1").is_some() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a closed connection kept its cached status"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The lag path clears every thread's cached state through the same
    /// wrapper: a dropped status notification must not leave a session busy.
    #[test]
    fn clearing_the_status_cache_drops_every_thread() {
        let projections = Projections::default();
        for thread in ["thread-a", "thread-b"] {
            projections
                .projection(thread)
                .lock()
                .expect("projection")
                .set_observed_status_at(Some(ThreadStatus::Active), 5);
        }
        projections.clear_statuses(7);
        assert_eq!(projections.observed_status("thread-a"), None);
        assert_eq!(projections.observed_status("thread-b"), None);
        // A status queued behind the clear is stale too.
        projections
            .projection("thread-a")
            .lock()
            .expect("projection")
            .set_observed_status_at(Some(ThreadStatus::Active), 6);
        assert_eq!(projections.observed_status("thread-a"), None);
    }

    /// A bounded `thread/resume` page is never presented as the whole history:
    /// adoption pages `thread/turns/list` before the first transcript read.
    #[tokio::test]
    async fn adoption_pages_the_whole_history_without_a_lag() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a long thread");
        thread.turns = vec![
            scripted_turn("turn-1", "completed", 1_700_000_000, "item-1", "one"),
            scripted_turn("turn-2", "completed", 1_700_000_010, "item-2", "two"),
            scripted_turn("turn-3", "completed", 1_700_000_020, "item-3", "three"),
        ];
        // The bounded resume page carries only the first turn.
        thread.read_turn_limit = Some(1);
        server.seed_thread(thread);
        let client = connect(&url).await;

        let first = client.transcript("thread-1").await.expect("first read");
        assert_eq!(
            first.messages.len(),
            3,
            "the first read must not present the bounded resume page as complete"
        );
        assert!(!first.truncated);
        assert_eq!(
            first
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["item-1", "item-2", "item-3"]
        );
        assert!(
            server.turns_calls() >= 2,
            "three turns at two per page need paging"
        );
        let params = server.turns_params();
        assert_eq!(params[0]["itemsView"], "full");
        assert_eq!(params[0]["sortDirection"], "asc");
    }

    /// The `ThreadEvent::Lagged` path through the subscriber pages the history
    /// again, rather than resyncing from the bounded `thread/read` page.
    #[tokio::test]
    async fn a_lagged_subscription_resyncs_every_paged_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a long thread");
        thread.turns = vec![
            scripted_turn("turn-1", "completed", 1_700_000_000, "item-1", "one"),
            scripted_turn("turn-2", "completed", 1_700_000_010, "item-2", "two"),
            scripted_turn("turn-3", "completed", 1_700_000_020, "item-3", "three"),
        ];
        thread.read_turn_limit = Some(1);
        server.seed_thread(thread);
        let client = connect(&url).await;

        assert_eq!(
            client.transcript("thread-1").await.expect("adopt").messages.len(),
            3
        );
        server.wait_for_subscribers("thread-1", 1).await;
        let pages_before = server.turns_calls();
        client.threads().simulate_lag(3);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let resynced = loop {
            let transcript = client.transcript("thread-1").await.expect("resynced read");
            if transcript.messages.len() == 3 && server.turns_calls() > pages_before {
                break transcript;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the lag resync did not page the history again"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(!resynced.truncated, "a full paged resync is complete");
        assert_eq!(
            resynced
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["item-1", "item-2", "item-3"]
        );
    }

    /// A malformed `thread/turns/list` page is a decode error, not an empty
    /// successful history: the bounded resume page is kept and the projection
    /// stays explicitly truncated.
    #[tokio::test]
    async fn a_malformed_turns_page_leaves_the_first_read_truncated() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a long thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        thread.read_turn_limit = Some(1);
        thread.malformed_turns_page = true;
        server.seed_thread(thread);
        let client = connect(&url).await;

        let first = client.transcript("thread-1").await.expect("first read");
        assert_eq!(first.messages.len(), 1, "the bounded page is still shown");
        assert!(
            first.truncated,
            "a failed page must keep the projection incomplete"
        );
        assert_eq!(server.turns_calls(), 1);
    }

    /// A server that repeats its cursor is refused rather than looped, and the
    /// projection stays explicitly incomplete.
    #[tokio::test]
    async fn a_repeated_turns_cursor_leaves_the_first_read_truncated() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a long thread");
        thread.turns = vec![
            scripted_turn("turn-1", "completed", 1_700_000_000, "item-1", "one"),
            scripted_turn("turn-2", "completed", 1_700_000_010, "item-2", "two"),
            scripted_turn("turn-3", "completed", 1_700_000_020, "item-3", "three"),
        ];
        thread.read_turn_limit = Some(1);
        thread.repeat_turns_cursor = true;
        server.seed_thread(thread);
        let client = connect(&url).await;

        let first = client.transcript("thread-1").await.expect("first read");
        assert_eq!(first.messages.len(), 1, "the bounded page is still shown");
        assert!(
            first.truncated,
            "a repeated cursor must keep the projection incomplete"
        );
        assert!(
            server.turns_calls() >= 2,
            "the repeat is only detected after a second page"
        );
    }

    /// The discriminating case: an event the retry page ALSO contains must not
    /// be applied a second time. The client has "Hello"; the page snapshot is
    /// "Hello world" and the same " world" delta is delivered while the page is
    /// in flight, so the final text must be "Hello world", not doubled.
    #[tokio::test]
    async fn an_event_already_in_the_page_is_applied_once() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a live thread");
        thread.turns = vec![json!({
            "id": "turn-live",
            "status": "inProgress",
            "startedAt": 1_700_000_000,
            "completedAt": null,
            "error": null,
            "items": [{ "type": "agentMessage", "id": "item-live", "text": "Hello" }],
        })];
        thread.read_turn_limit = Some(1);
        thread.turns_page_delay_ms = 150;
        server.seed_thread(thread);
        let client = connect(&url).await;
        server.fail_turns_pages(1);

        let first = client.transcript("thread-1").await.expect("first read");
        assert!(first.truncated, "the first page failed");
        let bounded = first
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-live")
            .map(|message| message.text())
            .unwrap_or_default();
        assert_eq!(bounded, "Hello", "the fallback carries the pre-delta text");

        // The page snapshot now includes the delta's effect.
        server.set_turns(
            "thread-1",
            vec![json!({
                "id": "turn-live",
                "status": "inProgress",
                "startedAt": 1_700_000_000,
                "completedAt": null,
                "error": null,
                "items": [{ "type": "agentMessage", "id": "item-live", "text": "Hello world" }],
            })],
        );
        tokio::time::sleep(PAGING_RETRY + Duration::from_millis(20)).await;

        // Trigger the retry and inject the delta while its page reply is
        // delayed. Under the old ordering the in-hand delta is applied after
        // the page that already contains it.
        let retry = tokio::spawn({
            let client = client.clone();
            async move { client.transcript("thread-1").await }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while server.turns_calls() < 2 {
            assert!(tokio::time::Instant::now() < deadline, "no paging retry started");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        server.push_to_thread(
            "thread-1",
            "item/agentMessage/delta",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-live",
                "itemId": "item-live",
                "delta": " world"
            }),
        );
        retry.await.expect("retry joined").expect("the retry read");

        // A terminal sentinel after the delta proves the delta was folded
        // before this assertion even though the retry itself is complete.
        server.push_to_thread(
            "thread-1",
            "thread/tokenUsage/updated",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-live",
                "tokenUsage": {
                    "total": {
                        "totalTokens": 77777,
                        "inputTokens": 1,
                        "cachedInputTokens": 0,
                        "cacheWriteInputTokens": 0,
                        "outputTokens": 1,
                        "reasoningOutputTokens": 0
                    },
                    "last": {
                        "totalTokens": 77777,
                        "inputTokens": 1,
                        "cachedInputTokens": 0,
                        "cacheWriteInputTokens": 0,
                        "outputTokens": 1,
                        "reasoningOutputTokens": 0
                    },
                    "modelContextWindow": 128000
                }
            }),
        );
        let transcript = wait_for_tokens(&client, "thread-1", 77_777).await;
        let text = transcript
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-live")
            .map(|message| message.text())
            .unwrap_or_default();
        assert_eq!(
            text, "Hello world",
            "an event already contained in the page was applied twice"
        );
        assert!(!transcript.truncated, "the retry completed");
    }

    /// A paging retry must not erase an event the subscriber applied while the
    /// page was in flight: paging and application share one serialisation.
    #[tokio::test]
    async fn an_event_during_a_paging_retry_survives_the_resync() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        thread.turns_page_delay_ms = 150;
        server.seed_thread(thread);
        let client = connect(&url).await;
        server.fail_turns_pages(1);

        let first = client.transcript("thread-1").await.expect("first read");
        assert!(first.truncated, "the failed page is stated");

        tokio::time::sleep(PAGING_RETRY + Duration::from_millis(20)).await;
        let retry = tokio::spawn({
            let client = client.clone();
            async move { client.transcript("thread-1").await }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while server.turns_calls() < 2 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the paging retry never reached the server"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Lands after the retry's page was read but before its resync applies.
        server.push_to_thread(
            "thread-1",
            "item/completed",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "completedAtMs": 1_700_000_002_000_i64,
                "item": { "type": "agentMessage", "id": "item-late", "text": "late event" }
            }),
        );

        retry.await.expect("retry joined").expect("the retried read");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let transcript = client.transcript("thread-1").await.expect("settled read");
            if transcript
                .messages
                .iter()
                .any(|message| message.id.as_str() == "item-late" && message.text() == "late event")
            {
                assert!(!transcript.truncated, "the retry completed");
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "an event applied during the retry was erased by its resync"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A drain that reaches its limit must schedule a paging retry rather than
    /// leaving the transcript truncated forever with no lag marker.
    #[tokio::test]
    async fn a_limited_drain_recovers_on_the_next_read() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        thread.turns_page_delay_ms = 150;
        server.seed_thread(thread);
        let client = connect(&url).await;
        client.projections.set_drain_limit(1);

        let adoption = tokio::spawn({
            let client = client.clone();
            async move { client.transcript("thread-1").await }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while server.turns_calls() < 1 {
            assert!(tokio::time::Instant::now() < deadline, "adoption never paged");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        for id in ["item-flood-a", "item-flood-b"] {
            server.push_to_thread(
                "thread-1",
                "item/completed",
                json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "completedAtMs": 1_700_000_003_000_i64,
                    "item": { "type": "agentMessage", "id": id, "text": id }
                }),
            );
        }
        let first = adoption.await.expect("adoption joined").expect("first read");
        assert!(first.truncated, "the drain limit leaves the first read truncated");

        tokio::time::sleep(PAGING_RETRY + Duration::from_millis(20)).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let transcript = client.transcript("thread-1").await.expect("retried read");
            if !transcript.truncated {
                assert!(
                    transcript
                        .messages
                        .iter()
                        .any(|message| message.id.as_str() == "item-1"),
                    "the retry recovered the paged history"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "a limited drain must schedule a paging retry"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// An in-progress historical turn is not authoritative: a statusless tool
    /// seeded from it must be running, and the queued completion settles it.
    #[tokio::test]
    async fn an_in_progress_history_item_seeds_as_running() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a live thread");
        thread.turns = vec![json!({
            "id": "turn-live",
            "status": "inProgress",
            "startedAt": 1_700_000_000,
            "completedAt": null,
            "error": null,
            "items": [{
                "type": "webSearch",
                "id": "item-web",
                "query": "still running",
                "action": { "type": "search", "query": "still running", "queries": null },
                "results": null
            }],
        })];
        server.seed_thread(thread);
        let client = connect(&url).await;

        let first = client.transcript("thread-1").await.expect("first read");
        let crate::backend::Part::Tool(call) = first
            .messages
            .iter()
            .find(|message| message.id.as_str() == "item-web")
            .and_then(|message| message.parts.first())
            .expect("the web search item")
        else {
            panic!("web search maps to a tool part");
        };
        assert_eq!(
            call.status,
            crate::backend::ToolStatus::Running,
            "an in-progress turn's item is not finished"
        );

        server.push_to_thread(
            "thread-1",
            "item/completed",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-live",
                "completedAtMs": 1_700_000_002_000_i64,
                "item": {
                    "type": "webSearch",
                    "id": "item-web",
                    "query": "still running",
                    "action": { "type": "search", "query": "still running", "queries": null },
                    "results": [{ "title": "done", "url": "https://example.test" }]
                }
            }),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let transcript = client.transcript("thread-1").await.expect("settled read");
            let crate::backend::Part::Tool(call) = transcript
                .messages
                .iter()
                .find(|message| message.id.as_str() == "item-web")
                .and_then(|message| message.parts.first())
                .unwrap()
            else {
                panic!("web search maps to a tool part");
            };
            if call.status == crate::backend::ToolStatus::Completed {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the queued completion must settle the seeded item"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A failed history page is retried on a later read (bounded by backoff),
    /// not left truncated forever by the live subscription state.
    #[tokio::test]
    async fn a_failed_history_page_retries_on_the_next_read() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a long thread");
        thread.turns = vec![
            scripted_turn("turn-1", "completed", 1_700_000_000, "item-1", "one"),
            scripted_turn("turn-2", "completed", 1_700_000_010, "item-2", "two"),
            scripted_turn("turn-3", "completed", 1_700_000_020, "item-3", "three"),
        ];
        thread.read_turn_limit = Some(1);
        server.seed_thread(thread);
        let client = connect(&url).await;
        server.fail_turns_pages(1);

        let first = client.transcript("thread-1").await.expect("first read");
        assert_eq!(first.messages.len(), 1, "the bounded fallback is the only data");
        assert!(first.truncated, "the failed page is stated");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let complete = loop {
            let transcript = client.transcript("thread-1").await.expect("retried read");
            if transcript.messages.len() == 3 && !transcript.truncated {
                break transcript;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "a failed history page must retry on a later read"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(
            complete
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["item-1", "item-2", "item-3"]
        );
        assert_eq!(
            server.resume_calls(),
            1,
            "the retry pages history, it does not re-subscribe"
        );
        assert!(
            server.turns_calls() >= 3,
            "the failed page and both retry pages ran"
        );
    }

    /// A dead subscription must not leave retained history looking seeded: the
    /// next status read re-adopts rather than answering stale state.
    #[tokio::test]
    async fn a_dead_subscription_reopens_on_the_next_status_read() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        server.seed_thread(thread);
        let client = connect(&url).await;

        assert_eq!(
            client
                .transcript("thread-1")
                .await
                .expect("subscribe")
                .messages
                .len(),
            1
        );
        assert_eq!(server.resume_calls(), 1);

        client.threads().simulate_close();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = client
                .session_status("thread-1", None)
                .await
                .expect("status read");
            if server.resume_calls() >= 2 {
                assert_eq!(status, Some(SessionStatus::Idle));
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "a dead subscription must be re-adopted by the next status read"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn finished_adoption_locks_are_pruned() {
        let projections = Projections::default();
        {
            let _first = projections.adoption_lock("thread-a");
            let _second = projections.adoption_lock("thread-b");
            assert_eq!(projections.adoption_lock_count(), 2);
        }
        // Both locks have no strong owner now; the next call prunes them.
        let _next = projections.adoption_lock("thread-c");
        assert_eq!(
            projections.adoption_lock_count(),
            1,
            "finished threads must not retain adoption-lock slots"
        );
    }

    /// Contract finding: adoption locks are per thread. A hung `thread/resume`
    /// for one never-run thread must not block an unrelated thread's first
    /// read.
    #[tokio::test]
    async fn a_hung_adoption_does_not_block_an_unrelated_thread() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut other =
            ScriptedThread::adopted("thread-other", "session-other", "/work/cola", "another thread");
        other.turns = vec![scripted_turn(
            "turn-other",
            "completed",
            1_700_000_000,
            "item-other",
            "ready",
        )];
        server.seed_thread(other);
        server.seed_thread(ScriptedThread::never_run(
            "thread-new",
            "session-new",
            "/work/cola",
        ));
        let client = connect(&url).await;

        server.silence_next_resume();
        let hung = tokio::spawn({
            let client = client.clone();
            async move { client.transcript("thread-new").await }
        });
        server.wait_for_resume_calls(1).await;

        let other = tokio::time::timeout(Duration::from_secs(1), client.transcript("thread-other"))
            .await
            .expect("an unrelated adoption must not wait on the hung one")
            .expect("the unrelated thread reads");
        assert_eq!(other.messages.len(), 1);

        hung.abort();
    }

    /// A mid-turn read must not block on the push stream, and it keeps the
    /// deltas already folded into the cache. The read is bounded by a timeout
    /// that a subscription-awaiting implementation would miss.
    #[tokio::test]
    async fn a_mid_turn_read_is_prompt_and_keeps_applied_deltas() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;

        // Stop the recording at the first agent delta: the turn is live.
        let events = recording();
        for event in &events {
            let method = event["method"].as_str().unwrap();
            server.push_to_thread("thread-1", method, event["params"].clone());
            if method == "item/agentMessage/delta" {
                break;
            }
        }
        let mid = tokio::time::timeout(Duration::from_secs(1), wait_for_text(&client, "thread-1", "The"))
            .await
            .expect("a mid-turn read must not block");
        assert_eq!(
            mid.messages
                .iter()
                .find(|message| message.id.as_str() == "item-agent")
                .expect("the streamed message")
                .text(),
            "The"
        );
        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("session status"),
            Some(SessionStatus::Busy),
            "the neutral status says the projection is live"
        );

        // The rest of the turn settles the same cache; nothing is dropped.
        for event in events
            .iter()
            .skip_while(|event| event["method"] != "item/agentMessage/delta")
            .skip(1)
        {
            let method = event["method"].as_str().unwrap();
            server.push_to_thread("thread-1", method, event["params"].clone());
        }
        let final_read = wait_for_text(&client, "thread-1", "The fix").await;
        assert_eq!(
            final_read
                .messages
                .iter()
                .find(|message| message.id.as_str() == "item-agent")
                .unwrap()
                .text(),
            "The fix"
        );
    }

    /// A never-run thread's read is empty, and the next read retries the
    /// subscription. That is the peer story: the user's own client runs the
    /// first turn, and cola starts mirroring the moment the rollout exists.
    #[tokio::test]
    async fn a_never_run_thread_retries_the_subscription_after_its_first_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::never_run(
            "thread-new",
            "session-root",
            "/work/cola",
        ));
        let client = connect(&url).await;

        assert!(
            client
                .transcript("thread-new")
                .await
                .expect("the empty read")
                .messages
                .is_empty()
        );
        assert_eq!(server.resume_calls(), 1, "the first read tried to resume");

        server.mark_history("thread-new");
        assert!(
            client
                .transcript("thread-new")
                .await
                .expect("the retried read")
                .messages
                .is_empty()
        );
        assert_eq!(
            server.resume_calls(),
            2,
            "a read after the first turn retries `thread/resume`"
        );
        server.wait_for_subscribers("thread-new", 1).await;
    }

    /// Turn Diff and token usage reach the neutral seam for a turn that changed
    /// a file.
    #[tokio::test]
    async fn turn_diff_and_token_usage_are_served_for_a_changed_file_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;
        for event in recording() {
            let method = event["method"].as_str().unwrap();
            server.push_to_thread("thread-1", method, event["params"].clone());
        }
        let _transcript = wait_for_text(&client, "thread-1", "The fix").await;
        let transcript = {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                let read = client.transcript("thread-1").await.expect("transcript");
                if read.messages.iter().any(|message| message.tokens.is_some()) {
                    break read;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for token usage"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        let diff = client
            .session_diff("thread-1", Some("item-user"), None, None)
            .await
            .expect("diff read")
            .expect("the turn's diff");
        assert_eq!(diff.0.len(), 1);
        assert_eq!(diff.0[0].path, "src/lib.rs");
        assert_eq!(diff.0[0].additions, 1);
        assert_eq!(diff.0[0].deletions, 1);

        let usage = transcript
            .messages
            .iter()
            .find_map(|message| message.tokens)
            .expect("token usage on the turn's newest assistant message");
        assert_eq!(usage.total, 12_345);
        assert_eq!(
            client
                .model_context_window("openai", "gpt-5")
                .await
                .expect("context window"),
            Some(128_000)
        );
    }

    /// Ticket #881 acceptance 1: a bridge prompt reaches the thread as a user
    /// message the server writes, and the turn it started is recorded as cola's
    /// (ADR-0090's authorship consequence) — never an External Message.
    #[tokio::test]
    async fn a_prompt_becomes_the_threads_user_message_and_is_recorded_as_colas() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        let authored = crate::codex::Authorship::in_memory();
        let client = connect_with(&url, authored).await;

        client
            .prompt(
                "thread-1",
                "hello codex",
                &[],
                &[],
                None,
                None,
                None,
                Some("msg_cola_1"),
            )
            .await
            .expect("the prompt submits");

        let params = server.turn_params("thread-1");
        assert_eq!(params.len(), 1, "one turn/start");
        assert_eq!(
            params[0]["input"],
            json!([{ "type": "text", "text": "hello codex", "text_elements": [] }]),
            "the prompt is the turn's input, with the protocol's own field spelling"
        );
        assert_eq!(
            params[0]["clientUserMessageId"], "msg_cola_1",
            "cola's own id rides the submission"
        );

        // The server persisted the turn: an adoption after the stream still
        // carries the user message, and the item's turn is in cola's record.
        let transcript = wait_for_text(&client, "thread-1", "hello codex").await;
        let user = transcript
            .messages
            .iter()
            .find(|message| message.role == crate::backend::MessageRole::User)
            .expect("the prompt's user message");
        assert!(
            client.is_cola_authored(user.id.as_str()).await,
            "the turn cola started is cola's"
        );
        assert!(
            !client.is_cola_authored("item-not-ours").await,
            "a message outside cola's record is external"
        );
    }

    /// Ticket #881 acceptance 6: the record survives a restart. A second
    /// process builds a fresh connection and projection from the same state
    /// directory and still answers "cola's" for the earlier turn.
    #[tokio::test]
    async fn a_turn_cola_started_survives_a_restart() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("authored_turns.json");

        let first = connect_with(
            &url,
            crate::codex::Authorship::open(path.clone(), "codex").expect("the record opens"),
        )
        .await;
        first
            .prompt("thread-1", "before the restart", &[], &[], None, None, None, None)
            .await
            .expect("the prompt submits");
        let transcript = wait_for_text(&first, "thread-1", "before the restart").await;
        let user_id = transcript
            .messages
            .iter()
            .find(|message| message.role == crate::backend::MessageRole::User)
            .map(|message| message.id.as_str().to_string())
            .expect("the user message");
        drop(first);

        // The restarted process: a fresh connection, the record reloaded from
        // disk, and the projection rebuilt from the server's persisted turns.
        let second = connect_with(
            &url,
            crate::codex::Authorship::open(path, "codex").expect("the record reloads"),
        )
        .await;
        let rebuilt = second.transcript("thread-1").await.expect("the rebuilt read");
        assert!(
            rebuilt
                .messages
                .iter()
                .any(|message| message.id.as_str() == user_id),
            "the restarted projection still carries the turn"
        );
        assert!(
            second.is_cola_authored(&user_id).await,
            "a restart must not report cola's own turn as external"
        );
    }

    /// The record is the only evidence, so an unusable file fails closed: every
    /// message is treated as cola's and the adapter says so loudly.
    #[tokio::test]
    async fn a_corrupt_authorship_record_never_reports_a_message_as_external() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("authored_turns.json");
        std::fs::write(&path, "{not json").expect("write a corrupt record");

        let authored = crate::codex::Authorship::open(path, "codex").expect("a corrupt file loads");
        let client = connect_with(&url, authored).await;
        assert!(
            client.is_cola_authored("item-anything").await,
            "an unusable record must not point External Message detection at cola's own turns"
        );
    }

    /// Ticket #881: a prompt that returns an already-active turn is folded into
    /// it (the spike's finding) — never a second turn — and the bound turn is
    /// recorded for the card to settle on.
    #[tokio::test]
    async fn a_prompt_that_folds_into_an_active_turn_binds_that_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        let authored = crate::codex::Authorship::in_memory();
        let cola = connect_with(&url, authored).await;
        cola.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;

        // The user's own client starts the turn.
        let tui = connect(&url).await;
        tui.transcript("thread-1").await.expect("the TUI's subscription");
        tui.prompt("thread-1", "from the TUI", &[], &[], None, None, None, None)
            .await
            .expect("the TUI's prompt");
        wait_for_live_turn(&cola, "thread-1").await;
        let live = cola.projections.live_turn("thread-1").expect("a live turn");

        // cola's prompt while that turn runs folds into it.
        cola.prompt(
            "thread-1",
            "cola's addition",
            &[],
            &[],
            None,
            None,
            None,
            Some("msg_cola_fold"),
        )
        .await
        .expect("the prompt is accepted");

        assert_eq!(
            cola.projections.live_turn("thread-1").as_deref(),
            Some(live.as_str()),
            "a fold never creates a second turn"
        );
        assert_eq!(server.turn_params("thread-1").len(), 2, "both prompts ran");
        let transcript = wait_for_text(&cola, "thread-1", "cola's addition").await;
        let tui_message = transcript
            .messages
            .iter()
            .find(|message| message.text().contains("from the TUI"))
            .expect("the TUI's message is in the same turn");
        assert_eq!(
            client_turn(&cola, tui_message.id.as_str()).as_deref(),
            Some(live.as_str()),
            "both messages belong to the one active turn"
        );
        assert!(
            cola.is_cola_authored(tui_message.id.as_str()).await,
            "the turn cola joined is now cola's turn (the protocol has no per-message author)"
        );
    }

    /// The turn a message belongs to, from the adapter's own projection.
    fn client_turn(client: &CodexClient, item_id: &str) -> Option<String> {
        client.projections.turn_of_item(item_id).map(|(_, turn)| turn)
    }

    /// Ticket #881 acceptance 3: a Supplement steers the in-flight turn
    /// (`turn/steer`, with the active turn's id as its precondition) instead of
    /// starting a new one, and the text appears once in the transcript.
    #[tokio::test]
    async fn a_supplement_steers_the_in_flight_turn_once() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;

        client
            .prompt(
                "thread-1",
                "first message",
                &[],
                &[],
                None,
                None,
                None,
                Some("msg_cola_1"),
            )
            .await
            .expect("the first prompt");
        wait_for_live_turn(&client, "thread-1").await;
        let live = client.projections.live_turn("thread-1").expect("live");

        client
            .steer(
                "thread-1",
                "and also this",
                &[],
                &[],
                None,
                None,
                None,
                Some("msg_cola_2"),
            )
            .await
            .expect("the supplement steers");

        let steers = server.steer_params();
        assert_eq!(steers.len(), 1, "turn/steer, not a second turn/start");
        assert_eq!(steers[0]["expectedTurnId"].as_str(), Some(live.as_str()));
        assert_eq!(steers[0]["input"][0]["text"], "and also this");
        assert_eq!(server.turn_starts(), 1, "the supplement never starts a turn");

        let transcript = wait_for_text(&client, "thread-1", "and also this").await;
        assert_eq!(
            transcript
                .messages
                .iter()
                .filter(|message| message.text().contains("and also this"))
                .count(),
            1,
            "the steered text appears exactly once"
        );
        assert_eq!(
            client.projections.live_turn("thread-1").as_deref(),
            Some(live.as_str()),
            "the steer lands in the same turn"
        );
    }

    /// Ticket #881 acceptance 2: `/stop` interrupts the turn the projection
    /// observed, and the server's `interrupted` ending settles it — not an
    /// error, not a hang.
    #[tokio::test]
    async fn interrupting_sends_the_live_turn_and_settles_interrupted() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;
        client
            .prompt("thread-1", "a long run", &[], &[], None, None, None, None)
            .await
            .expect("the prompt");
        wait_for_live_turn(&client, "thread-1").await;
        let live = client.projections.live_turn("thread-1").expect("live");

        client.interrupt("thread-1").await.expect("the interrupt");

        let interrupts = server.interrupt_params();
        assert_eq!(interrupts.len(), 1);
        assert_eq!(interrupts[0]["threadId"], "thread-1");
        assert_eq!(
            interrupts[0]["turnId"].as_str(),
            Some(live.as_str()),
            "the interrupt names the observed live turn"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while client.projections.live_turn("thread-1").is_some() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "an interrupted turn must settle, not hang live"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            client.session_status("thread-1", None).await.expect("status"),
            Some(SessionStatus::Idle),
            "an interrupted run is idle, never an error or a hang"
        );
        assert_eq!(server.live_turn("thread-1"), None);
    }

    /// An idle `/stop` is a no-op: there is no turn id to interrupt, and no
    /// request is sent.
    #[tokio::test]
    async fn interrupting_an_idle_thread_sends_nothing() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.interrupt("thread-1").await.expect("idle stop is a no-op");
        assert!(server.interrupt_params().is_empty());
    }

    /// Ticket #881 acceptance 4: `/compact` runs `thread/compact/start`, and
    /// the server's `contextCompaction` item is projected as the neutral
    /// Compaction the existing card path already renders — exactly once.
    #[tokio::test]
    async fn compact_runs_and_projects_the_compaction_once() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;

        client.compact("thread-1").await.expect("the compaction starts");

        assert_eq!(
            server.compact_params(),
            vec![json!({ "threadId": "thread-1" })],
            "thread/compact/start names the thread"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let transcript = client.transcript("thread-1").await.expect("read");
            let compactions: Vec<_> = transcript
                .messages
                .iter()
                .filter(|message| {
                    message
                        .parts
                        .iter()
                        .any(|part| matches!(part, crate::backend::Part::Compaction(_)))
                })
                .collect();
            if !compactions.is_empty() {
                assert_eq!(compactions.len(), 1, "one compaction item, one row");
                let crate::backend::Part::Compaction(fact) = &compactions[0].parts[0] else {
                    unreachable!()
                };
                assert_eq!(fact.status, crate::backend::CompactionStatus::Completed);
                assert_eq!(
                    fact.reason,
                    crate::backend::CompactionReason::Manual,
                    "a /compact-requested row is manual, not auto"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the compaction never reached the transcript"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // A second read is not a second compaction (the projection dedupes on
        // the server's item id).
        let read = client.transcript("thread-1").await.expect("second read");
        assert_eq!(
            read.messages
                .iter()
                .filter(|message| {
                    message
                        .parts
                        .iter()
                        .any(|part| matches!(part, crate::backend::Part::Compaction(_)))
                })
                .count(),
            1
        );
    }

    /// Ticket #881 acceptance 5 (listing half): `/model` shows the server's
    /// catalog, paged, with each model's reasoning efforts as its variants.
    #[tokio::test]
    async fn list_models_pages_the_catalog_with_effort_variants() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.set_models(vec![
            json!({
                "id": "gpt-5.1-codex",
                "displayName": "GPT-5.1 Codex",
                "hidden": false,
                "supportedReasoningEfforts": [
                    { "reasoningEffort": "low", "description": "fast" },
                    { "reasoningEffort": "high", "description": "careful" }
                ],
                "defaultReasoningEffort": "high"
            }),
            json!({
                "id": "an-internal-model",
                "displayName": "Internal",
                "hidden": true,
                "supportedReasoningEfforts": [],
                "defaultReasoningEffort": "low"
            }),
        ]);
        server.set_model_page_size(1);
        let client = connect(&url).await;

        let providers = client.list_models().await;

        assert_eq!(providers.len(), 1, "one neutral provider bucket");
        assert_eq!(providers[0].provider, "codex");
        assert_eq!(
            providers[0]
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["gpt-5.1-codex"],
            "a hidden model is not offered"
        );
        assert_eq!(providers[0].models[0].variants, ["low", "high"]);
        assert_eq!(
            server.model_list_calls(),
            2,
            "the catalog is paged to the end, not read as its first page"
        );
    }

    /// Ticket #881 acceptance 5 (skills half): `/skill` lists what the server
    /// advertises, and a pick reaches the next turn as the protocol's `skill`
    /// input (which needs the path the neutral DTO does not carry).
    #[tokio::test]
    async fn a_skill_pick_reaches_the_next_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        server.set_skills(vec![json!({
            "name": "caveman",
            "description": "Ultra-compressed communication mode.",
            "path": "/home/u/.codex/skills/caveman/SKILL.md",
            "scope": "user",
            "enabled": true,
            "pluginId": null,
        })]);
        let client = connect(&url).await;

        let skills = client.list_skills(Some("/work/cola")).await;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "caveman", "Codex's identity is the name");
        assert_eq!(skills[0].name, "caveman");
        assert_eq!(
            skills[0].description.as_deref(),
            Some("Ultra-compressed communication mode.")
        );
        assert_eq!(skills[0].content, None, "the list route carries no body to fold");

        client
            .prompt(
                "thread-1",
                "explain this",
                &[],
                &[PromptSkill {
                    id: "caveman".into(),
                    name: "caveman".into(),
                }],
                None,
                None,
                None,
                None,
            )
            .await
            .expect("the prompt submits with the skill");

        let params = server.turn_params("thread-1");
        assert_eq!(
            params[0]["input"][1],
            json!({ "type": "skill", "name": "caveman", "path": "/home/u/.codex/skills/caveman/SKILL.md" })
        );
    }

    /// A skill the last list did not name is a named error, never a silently
    /// dropped request (the model would answer without it).
    #[tokio::test]
    async fn an_unknown_skill_fails_the_prompt_by_name_and_leaves_no_marker() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;
        let error = client
            .prompt(
                "thread-1",
                "x",
                &[],
                &[PromptSkill {
                    id: "never-listed".into(),
                    name: "never-listed".into(),
                }],
                None,
                None,
                None,
                Some("msg_cola_never"),
            )
            .await
            .expect_err("an unknown skill is refused");
        assert!(error.to_string().contains("never-listed"), "{error}");
        // Finding 2: a pre-send validation failure must not leave a marker
        // that would suppress a later external turn on the thread.
        assert!(!client.authored.is_pending("thread-1"));
        assert!(
            !client
                .authored
                .is_pending_submission("thread-1", "msg_cola_never")
        );

        // A turn the user's own client starts afterwards is external.
        for (method, params) in [
            (
                "turn/started",
                json!({ "threadId": "thread-1", "turn": { "id": "turn-tui", "status": "inProgress", "startedAt": 5 } }),
            ),
            (
                "item/started",
                json!({
                    "threadId": "thread-1",
                    "turnId": "turn-tui",
                    "startedAtMs": 5_000_i64,
                    "item": {
                        "type": "userMessage",
                        "id": "item-tui",
                        "clientId": null,
                        "content": [{ "type": "text", "text": "from the TUI", "textElements": [] }],
                    },
                }),
            ),
        ] {
            server.push_to_thread("thread-1", method, params);
        }
        wait_for_text(&client, "thread-1", "from the TUI").await;
        assert!(
            !client.is_cola_authored("item-tui").await,
            "a failed pre-send validation must not vouch for a later external turn"
        );
    }

    /// Finding 4: a skill deleted between two lists must stop resolving, so the
    /// prompt fails by name instead of sending a stale path. A name present in
    /// two directories resolves to the most recently read scope.
    #[tokio::test]
    async fn a_skill_that_disappears_between_lists_is_refused() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.set_skills(vec![json!({
            "name": "caveman",
            "description": "Brief mode",
            "path": "/work/cola/.codex/skills/caveman/SKILL.md",
            "scope": "repo",
            "enabled": true,
            "pluginId": null,
        })]);
        let client = connect(&url).await;
        assert_eq!(client.list_skills(Some("/work/cola")).await.len(), 1);

        // A second read for another project refreshes that scope and wins.
        server.set_skills(vec![json!({
            "name": "caveman",
            "description": "Brief mode",
            "path": "/work/other/.codex/skills/caveman/SKILL.md",
            "scope": "repo",
            "enabled": true,
            "pluginId": null,
        })]);
        assert_eq!(client.list_skills(Some("/work/other")).await.len(), 1);
        let path = client.skills.path_of("caveman").expect("the newest scope");
        assert_eq!(path, "/work/other/.codex/skills/caveman/SKILL.md");

        // The skill vanishes from its own directory: a new list replaces that
        // scope, and the name is gone unless another scope still carries it.
        server.set_skills(Vec::new());
        assert!(
            client.list_skills(Some("/work/cola")).await.is_empty(),
            "the vanished scope is replaced"
        );
        assert!(
            client.skills.path_of("caveman").is_some(),
            "the other directory's read still carries it"
        );
        assert!(client.list_skills(Some("/work/other")).await.is_empty());
        assert!(
            client.skills.path_of("caveman").is_none(),
            "a vanished skill is not resolved from a stale path"
        );
        let error = client
            .prompt(
                "thread-1",
                "be brief",
                &[],
                &[PromptSkill {
                    id: "caveman".into(),
                    name: "caveman".into(),
                }],
                None,
                None,
                None,
                None,
            )
            .await
            .expect_err("the deleted skill is refused");
        assert!(error.to_string().contains("caveman"), "{error}");
    }

    /// Finding 3: `/stop` can land after `turn/start` was accepted but before
    /// the server persisted the turn. The lookup polls the thread's record, so
    /// the interrupt fires the moment the turn appears.
    #[tokio::test]
    async fn a_stop_waits_for_the_turn_the_server_persists_a_moment_later() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        // The user's own client starts the turn; cola has no subscription, so
        // its cache cannot see it.
        let tui = connect(&url).await;
        tui.transcript("thread-1").await.expect("the TUI's subscription");
        tui.prompt("thread-1", "a long run", &[], &[], None, None, None, None)
            .await
            .expect("the TUI's prompt");
        let live = server.live_turn("thread-1").expect("the turn is running");

        let cola = connect(&url).await;
        // The first read still hides the in-progress turn: the stop must retry.
        server.hide_live_turn_for("thread-1", 1);
        cola.interrupt("thread-1")
            .await
            .expect("the stop waits for the turn");

        let interrupts = server.interrupt_params();
        assert_eq!(interrupts.len(), 1, "the retry interrupted the run");
        assert_eq!(interrupts[0]["turnId"].as_str(), Some(live.as_str()));
        assert_eq!(server.live_turn("thread-1"), None);
    }

    /// Finding 5: an ambiguous `thread/compact/start` failure (the request was
    /// accepted, its response lost) keeps the manual marker, so the item is not
    /// mislabelled automatic; a definite refusal clears it.
    #[tokio::test]
    async fn an_ambiguous_compaction_failure_keeps_the_manual_marker() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;
        server.drop_next_compact_response();
        client
            .compact("thread-1")
            .await
            .expect_err("the response was lost");
        assert!(
            client.projections.manual_compaction_pending("thread-1"),
            "an ambiguous failure must not relabel the compaction automatic"
        );
    }

    #[tokio::test]
    async fn a_refused_compaction_clears_the_manual_marker() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.reject_next_compact();
        let client = connect(&url).await;
        client.compact("thread-1").await.expect_err("the server refused");
        assert!(
            !client.projections.manual_compaction_pending("thread-1"),
            "a definite refusal did not start a compaction"
        );
    }

    /// A `/model` and `/think` pick ride the next `turn/start`: the model as
    /// `model`, the variant as `effort`. That is the per-turn shape Codex
    /// accepts (its durable-switch route does not exist), which is why
    /// `switch_session_model` stays the trait's no-op here.
    #[tokio::test]
    async fn a_model_and_effort_pick_ride_the_next_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        let model = ModelInfo {
            id: "gpt-5.1-codex".into(),
            provider_id: "codex".into(),
            variant: None,
        };
        client
            .prompt(
                "thread-1",
                "run it",
                &[],
                &[],
                Some(&model),
                Some("high"),
                None,
                None,
            )
            .await
            .expect("the prompt");
        let params = server.turn_params("thread-1");
        assert_eq!(params[0]["model"], "gpt-5.1-codex");
        assert_eq!(params[0]["effort"], "high");
        assert!(
            !client.keeps_session_selection(),
            "the pick is per-turn, not a durable session switch"
        );
    }

    /// The provenance window (ADR-0092 decision 2): an item can reach the
    /// subscription before the `turn/start` response names its turn. With a
    /// submission in flight, the thread's live turn is cola's — an older,
    /// completed turn of the same thread is not.
    #[tokio::test]
    async fn an_item_without_a_client_id_in_the_live_turn_is_colas_while_a_prompt_is_pending() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect_with(&url, crate::codex::Authorship::in_memory()).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;

        // An older, completed turn from another client.
        for (method, params) in [
            (
                "turn/started",
                json!({ "threadId": "thread-1", "turn": { "id": "turn-old", "status": "inProgress", "startedAt": 1 } }),
            ),
            (
                "item/started",
                json!({
                    "threadId": "thread-1",
                    "turnId": "turn-old",
                    "startedAtMs": 1_000_i64,
                    "item": {
                        "type": "userMessage",
                        "id": "item-old",
                        "clientId": null,
                        "content": [{ "type": "text", "text": "someone else's message", "textElements": [] }],
                    },
                }),
            ),
            (
                "turn/completed",
                json!({ "threadId": "thread-1", "turn": { "id": "turn-old", "status": "completed", "startedAt": 1, "completedAt": 2 } }),
            ),
        ] {
            server.push_to_thread("thread-1", method, params);
        }
        wait_for_text(&client, "thread-1", "someone else's message").await;
        assert!(
            !client.is_cola_authored("item-old").await,
            "a completed turn cola never touched is external"
        );

        // cola's prompt is in flight; its turn's stream arrives first, and the
        // item carries no client id (the server dropped it).
        client.authored.begin("thread-1", None);
        server.push_to_thread(
            "thread-1",
            "turn/started",
            json!({ "threadId": "thread-1", "turn": { "id": "turn-live", "status": "inProgress", "startedAt": 3 } }),
        );
        server.push_to_thread(
            "thread-1",
            "item/started",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-live",
                "startedAtMs": 3_000_i64,
                "item": {
                    "type": "userMessage",
                    "id": "item-live",
                    "clientId": null,
                    "content": [{ "type": "text", "text": "cola's live prompt", "textElements": [] }],
                },
            }),
        );
        wait_for_text(&client, "thread-1", "cola's live prompt").await;
        assert!(
            client.is_cola_authored("item-live").await,
            "the in-flight submission vouches for the live turn before the response names it"
        );
        assert!(
            !client.is_cola_authored("item-old").await,
            "the pending marker never reaches back into a finished turn"
        );

        // The response binds the turn: the record answers from here on.
        client.authored.bind("thread-1", "turn-live", None);
        assert!(client.is_cola_authored("item-live").await);
        assert!(!client.authored.is_pending("thread-1"));
    }

    /// A pending **durable** submission binds to the turn its user item landed
    /// in on the next history read: this is how a lost `turn/start` response is
    /// recovered (ADR-0092 decision 2).
    #[tokio::test]
    async fn an_item_carrying_the_pending_submission_binds_its_turn_on_the_read() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect_with(&url, crate::codex::Authorship::in_memory()).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;

        client.authored.begin("thread-1", Some("msg_cola_early"));
        for (method, params) in [
            (
                "turn/started",
                json!({ "threadId": "thread-1", "turn": { "id": "turn-early", "status": "inProgress", "startedAt": 3 } }),
            ),
            (
                "item/started",
                json!({
                    "threadId": "thread-1",
                    "turnId": "turn-early",
                    "startedAtMs": 3_000_i64,
                    "item": {
                        "type": "userMessage",
                        "id": "item-early",
                        "clientId": "msg_cola_early",
                        "content": [{ "type": "text", "text": "cola's early prompt", "textElements": [] }],
                    },
                }),
            ),
        ] {
            server.push_to_thread("thread-1", method, params);
        }
        // The read is what reconciles: the item's client id names the pending
        // submission, and its turn is bound.
        wait_for_text(&client, "thread-1", "cola's early prompt").await;
        assert!(
            client.authored.is_authored("thread-1", Some("turn-early")),
            "the read bound the pending submission's turn"
        );
        assert!(
            !client
                .authored
                .is_pending_submission("thread-1", "msg_cola_early"),
            "the pending marker is resolved by the binding"
        );
        assert!(client.is_cola_authored("item-early").await);
    }

    /// Finding 1: an accepted `turn/start` whose response is lost must not be
    /// forgotten — not even by a restart. The chosen id is durable before the
    /// request, and the restarted process reconciles it from persisted history.
    #[tokio::test]
    async fn an_accepted_prompt_with_a_lost_response_is_colas_after_a_restart() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("authored_turns.json");

        let first = connect_with(
            &url,
            crate::codex::Authorship::open(path.clone(), "codex").expect("the record opens"),
        )
        .await;
        first.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;
        server.drop_next_turn_start_response();
        let error = first
            .prompt(
                "thread-1",
                "accepted but unanswered",
                &[],
                &[],
                None,
                None,
                None,
                Some("msg_cola_lost"),
            )
            .await
            .expect_err("the response was lost");
        let _ = error;
        assert!(
            first.authored.is_pending_submission("thread-1", "msg_cola_lost"),
            "an ambiguous failure keeps the chosen id"
        );
        drop(first);

        // The restarted process: a fresh connection and the durable record. Its
        // history read binds the submission to the turn the server persisted.
        let second = connect_with(
            &url,
            crate::codex::Authorship::open(path, "codex").expect("the record reloads"),
        )
        .await;
        let mut transcript = second.transcript("thread-1").await.expect("the read");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !transcript
            .messages
            .iter()
            .any(|message| message.text() == "accepted but unanswered")
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the persisted prompt never reached the read"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            transcript = second.transcript("thread-1").await.expect("the retried read");
        }
        let user = transcript
            .messages
            .iter()
            .find(|message| message.text() == "accepted but unanswered")
            .expect("the prompt's user message");
        assert!(
            second.is_cola_authored(user.id.as_str()).await,
            "a restart must not report the accepted, unanswered prompt as external"
        );
        assert!(
            !second.authored.is_pending_submission("thread-1", "msg_cola_lost"),
            "the history read resolved the pending submission"
        );
    }

    /// Finding 1 (the other side): a **definite** refusal clears its marker, so
    /// a later external turn on the thread is not suppressed.
    #[tokio::test]
    async fn a_definitely_refused_prompt_clears_its_pending_marker() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect_with(&url, crate::codex::Authorship::in_memory()).await;
        server.reject_next_turn_start("model not found");
        let error = client
            .prompt(
                "thread-1",
                "never sent",
                &[],
                &[],
                None,
                None,
                None,
                Some("msg_cola_refused"),
            )
            .await
            .expect_err("the server refused");
        assert!(error.to_string().contains("model not found"), "{error}");
        assert!(!client.authored.is_pending("thread-1"));
        assert!(
            !client
                .authored
                .is_pending_submission("thread-1", "msg_cola_refused")
        );
    }

    /// The adapter is constructed by kind, and an unattached lifecycle is a
    /// named "not attached" read rather than an empty transcript that could be
    /// mistaken for a real one.
    #[tokio::test]
    async fn an_unattached_lifecycle_refuses_a_transcript_read_by_name() {
        let directory = tempfile::tempdir().expect("tempdir");
        let settings = crate::config::CodexConfig::default();
        let environment = crate::codex::discovery::Environment {
            home: directory.path().to_path_buf(),
            cli: directory.path().join("no-codex"),
            version_timeout: Duration::from_millis(50),
            start_timeout: Duration::from_millis(50),
            socket_ready_timeout: Duration::from_millis(50),
        };
        let lifecycle = CodexLifecycle::new(settings, environment, Box::new(crate::codex::ManagedProbe));
        let error = lifecycle
            .transcript("thread-1")
            .await
            .expect_err("an unattached client has no transcript");
        assert!(error.to_string().contains("not attached"), "{error}");
        let status_error = lifecycle
            .session_status("thread-1", None)
            .await
            .expect_err("no client to read");
        assert!(
            status_error.to_string().contains("not attached"),
            "{status_error}"
        );
    }
}
