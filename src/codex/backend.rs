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
    AgentInfo, CreateSessionInput, FormAnswer, Location, ModelInfo, ModelOption, ModelSurface,
    ModelSurfaceQuery, PermissionRequest, PromptFile, ProviderModels, QuestionRequest, Session, SessionInfo,
    SessionListInfo, SessionModel, SessionStatus,
};
use crate::backend::{
    Backend, BackendDirectory, ModelIdentity, PromptSkill, SessionTranscript, SkillInfo, TurnDiff,
};
use crate::bridge::tasks::TasksHandle;
use crate::error::{BridgeError, Result};

use super::authorship::SubmissionKind;
use super::client::CodexClient;
use super::error::{CodexError, Result as CodexResult};
use super::lifecycle::CodexLifecycle;
use super::read_model::{DiffSnapshot, ThreadProjection, ThreadStatus, decode_thread_status};
use super::threads::{Adoption, Thread, ThreadEvent, ThreadSubscription, Threads};
use super::transport::{Endpoint, Inbound, RpcConnection};

/// How long a truncated history waits before the next read retries paging. A
/// persistently failing server is polled at this cadence, never in a loop; a
/// healthy server clears the state on the first retry.
#[cfg(not(test))]
const PAGING_RETRY: Duration = Duration::from_secs(2);
#[cfg(test)]
const PAGING_RETRY: Duration = Duration::from_millis(20);

/// How long a first read waits for the subscriber to install a projection
/// before returning the current cache. A read must never block on adoption or
/// a history page; this bound only lets a fast local page finish.
#[cfg(not(test))]
const INITIAL_READ_BOUND: Duration = Duration::from_millis(150);
#[cfg(test)]
const INITIAL_READ_BOUND: Duration = Duration::from_millis(100);

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
    /// Monotone stamps for the thread-model facts (issue #904): taken **before**
    /// the read that will learn a thread's model, or at the accepted
    /// `turn/start` of a pick. The projection drops a fact older than the one it
    /// holds, so a read in flight across a newer fact cannot resurrect a stale
    /// name.
    model_stamps: std::sync::atomic::AtomicU64,
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
    /// Wakes a bounded first read when its thread leaves `Adopting` (and, for a
    /// failed adoption, when the next retry state is known).
    ready: Mutex<HashMap<String, std::sync::Weak<tokio::sync::Notify>>>,
    /// The subscriber tasks this cache owns. Aborted when the last client
    /// holding the cache is dropped, so a replaced attachment stops folding
    /// into a cache nothing reads.
    subscribers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// One release channel per live thread consumer. The consumer task is the
    /// **only** holder of the thread's [`ThreadSubscription`], so the release
    /// path hands it a command and it is that task which sends
    /// `thread/unsubscribe` — only the last local handle may unsubscribe.
    /// Taking the entry here makes a second release of the same thread a
    /// no-op instead of a second unsubscribe.
    releases: Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<ReleaseCommand>>>,
    /// Each thread's working directory, learned when it is created or adopted.
    /// The prompt axis carries no directory, so this is where a skill resolves
    /// against the session's own project first.
    cwds: Mutex<HashMap<String, String>>,
    /// Small Turn Diff snapshots for threads whose transcripts were evicted on
    /// release, so an in-flight diff card can still resolve without retaining
    /// the whole transcript.
    diff_snapshots: Mutex<HashMap<String, DiffSnapshot>>,
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

/// One request to the task that owns a thread's subscription: release it (the
/// task is the only holder, so only it can decide whether this was the last
/// local handle) and answer with the outcome. The answer lets the caller bound
/// its wait and makes a returned release a clean point: the next
/// `thread/resume` cannot race the teardown.
struct ReleaseCommand {
    ack: tokio::sync::oneshot::Sender<CodexResult<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionState {
    /// No subscriber task has finished `thread/resume` yet. Reads wait a
    /// bounded time and then answer from the (empty) cache.
    Adopting,
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

    fn ready_signal(&self, thread_id: &str) -> Arc<tokio::sync::Notify> {
        let mut signals = self.ready.lock().expect("codex ready signals");
        signals.retain(|_, signal| signal.strong_count() > 0);
        if let Some(signal) = signals.get(thread_id).and_then(std::sync::Weak::upgrade) {
            return signal;
        }
        let signal = Arc::new(tokio::sync::Notify::new());
        signals.insert(thread_id.to_string(), Arc::downgrade(&signal));
        signal
    }

    fn notify_ready(&self, thread_id: &str) {
        if let Some(signal) = self
            .ready
            .lock()
            .expect("codex ready signals")
            .get(thread_id)
            .and_then(std::sync::Weak::upgrade)
        {
            signal.notify_one();
        }
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
        let live = self
            .threads
            .lock()
            .expect("codex projections")
            .get(thread_id)
            .cloned()
            .and_then(|projection| projection.lock().expect("codex projection").diff_for(from));
        if live.as_ref().is_some_and(|diff| !diff.is_empty()) {
            return live;
        }
        if let Some(snapshot) = self
            .diff_snapshots
            .lock()
            .expect("codex diff snapshots")
            .get(thread_id)
            .and_then(|snapshot| snapshot.resolve(from))
        {
            // A live empty diff (a re-adopted completed turn) must not hide
            // the snapshot from the turn that actually changed files.
            if live.is_none() || (!snapshot.is_empty() && live.as_ref().is_some_and(TurnDiff::is_empty)) {
                return Some(snapshot);
            }
        }
        live
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
    ///
    /// Deliberately in memory: the label is cosmetic (the compaction item is
    /// the fact, and the card renders it either way). Persisting it would add a
    /// durable shape for a display hint, and a restart before the item projects
    /// simply titles that one row 自动.
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

    /// Remember a thread's working directory (from `thread/start` or
    /// `thread/resume`), so a skill resolves against its own project first.
    pub(crate) fn set_cwd(&self, thread_id: &str, cwd: &str) {
        if cwd.is_empty() {
            return;
        }
        self.cwds
            .lock()
            .expect("codex thread cwds")
            .insert(thread_id.to_string(), cwd.to_string());
    }

    /// Remember everything a thread read just told us about the thread: its
    /// directory (skill resolution) and the model it runs with (the card
    /// footer's only per-message fact, issue #904). One call so a new thread
    /// fact cannot be forgotten at one of the sites that read a thread.
    pub(crate) fn remember_thread_metadata(&self, thread: &Thread, observed: u64) {
        self.set_cwd(&thread.id, &thread.cwd);
        // The projection decides what silence means (a name the server itself
        // gave is dropped; a model cola sent survives an omitted field), and
        // which of two answers is the newer one.
        self.observe_thread_model(&thread.id, thread_model(thread), observed);
    }

    /// The stamp a read takes **before** it asks the server, so its answer can
    /// be ordered against a pick accepted while it was in flight (issue #904).
    pub(crate) fn next_model_stamp(&self) -> u64 {
        self.model_stamps
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Apply the model a thread read reported, including its silence: the
    /// projection owns what silence may clear (see
    /// [`ThreadProjection::observe_thread_model`]), and drops an answer an
    /// older read produced than the fact it already holds.
    pub(crate) fn observe_thread_model(&self, thread_id: &str, model: Option<ModelIdentity>, observed: u64) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .observe_thread_model(model, observed);
    }

    /// Record the model cola's own accepted `turn/start` sent: it is "for this
    /// turn and subsequent turns", so the thread now runs it — without this the
    /// footer would keep naming the model from before the pick.
    pub(crate) fn record_sent_model(&self, thread_id: &str, model: ModelIdentity, sent: u64) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .record_sent_model(model, sent);
    }

    /// The model the ordered store holds for a thread (issue #904), which is
    /// what both surfaces must report: a read whose answer an accepted pick
    /// superseded is not shown, even to the caller that made the read.
    pub(crate) fn model(&self, thread_id: &str) -> Option<ModelIdentity> {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .model()
    }

    pub(crate) fn cwd(&self, thread_id: &str) -> Option<String> {
        self.cwds
            .lock()
            .expect("codex thread cwds")
            .get(thread_id)
            .cloned()
    }

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

    /// Which thread and turn an item belongs to. The authorship question is
    /// asked with a transcript message id (a Codex item id), while the durable
    /// record is keyed by the turn cola started, so the projection — the only
    /// owner of the item→turn mapping — answers it.
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
        self.notify_ready(thread_id);
    }

    /// Leave no subscription state (a never-run thread): the next read retries
    /// adoption.
    fn clear_state(&self, thread_id: &str) {
        self.states
            .lock()
            .expect("codex subscription states")
            .remove(thread_id);
        self.notify_ready(thread_id);
    }

    fn clear_incomplete(&self, thread_id: &str) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .clear_incomplete();
    }

    /// Whether the thread is still waiting for its first `thread/resume`.
    pub(crate) fn is_adopting(&self, thread_id: &str) -> bool {
        self.state(thread_id) == Some(SubscriptionState::Adopting)
    }

    /// Mark a timed-out first read truncated, but only if it is still the
    /// adoption that is pending. Held under the state lock so a concurrent
    /// `Live` transition (and its completed page) cannot be marked after the
    /// fact.
    fn mark_incomplete_if_adopting(&self, thread_id: &str) {
        let states = self.states.lock().expect("codex subscription states");
        if !matches!(states.get(thread_id), Some(SubscriptionState::Adopting)) {
            return;
        }
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .mark_incomplete_if_unseeded();
    }

    /// Register the consumer that owns `thread_id`'s subscription. A re-adopt
    /// replaces any previous sender (which by then belongs to a task that has
    /// already ended or is about to).
    fn watch_release(&self, thread_id: &str, sender: tokio::sync::mpsc::UnboundedSender<ReleaseCommand>) {
        self.releases
            .lock()
            .expect("codex release channels")
            .insert(thread_id.to_string(), sender);
    }

    /// Claim `thread_id`'s release channel. `None` means no consumer is live:
    /// the thread was never adopted on this connection, or it was already
    /// released — either way there is nothing to unsubscribe.
    fn take_release(&self, thread_id: &str) -> Option<tokio::sync::mpsc::UnboundedSender<ReleaseCommand>> {
        self.releases
            .lock()
            .expect("codex release channels")
            .remove(thread_id)
    }

    /// Drop a released thread's live state and evict its transcript. Only the
    /// small Turn Diff snapshot survives, so an in-flight diff card can still
    /// resolve; re-adoption pages the history again.
    fn released(&self, thread_id: &str) {
        self.evict(thread_id, true);
    }

    /// Remove a thread's live state and projection. `keep_diffs` retains the
    /// small Turn Diff snapshot (release); a deleted thread keeps nothing.
    fn evict(&self, thread_id: &str, keep_diffs: bool) {
        self.states
            .lock()
            .expect("codex subscription states")
            .remove(thread_id);
        self.notify_ready(thread_id);
        let projection = self.threads.lock().expect("codex projections").remove(thread_id);
        if let Some(projection) = projection
            && keep_diffs
        {
            let snapshot = projection.lock().expect("codex projection").diff_snapshot();
            if !snapshot.is_empty() {
                self.diff_snapshots
                    .lock()
                    .expect("codex diff snapshots")
                    .insert(thread_id.to_string(), snapshot);
            }
        } else if !keep_diffs {
            self.diff_snapshots
                .lock()
                .expect("codex diff snapshots")
                .remove(thread_id);
        }
        self.cwds.lock().expect("codex thread cwds").remove(thread_id);
        self.releases
            .lock()
            .expect("codex release channels")
            .remove(thread_id);
    }

    #[cfg(test)]
    fn projection_count(&self) -> usize {
        self.threads.lock().expect("codex projections").len()
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
        self.notify_ready(thread_id);
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

    /// Stop mirroring `thread_id` on this connection: the subscriber task sends
    /// `thread/unsubscribe` when it was the last local handle, then the thread's
    /// live state is dropped (the retained history stays, explicitly
    /// incomplete). The server session is untouched — it stays listable,
    /// resumable and readable — and a thread this connection never adopted (or
    /// already released) is a `Ok(())` no-op.
    ///
    /// Serialised with [`Self::ensure_subscription`] by the same per-thread
    /// adoption lock: a release cannot kill a subscription a concurrent read
    /// just made, and a release that has returned is not raced by the next
    /// `thread/resume`.
    pub(crate) async fn release_thread(&self, thread_id: &str) -> CodexResult<()> {
        let adoption = self.projections.adoption_lock(thread_id);
        let _adopting = adoption.lock().await;
        let Some(release) = self.projections.take_release(thread_id) else {
            return Ok(());
        };
        // Drop the live state before the unsubscribe can return, so a read
        // arriving while it is in flight re-adopts instead of reading a stream
        // that is ending.
        self.projections.forget(thread_id);
        let (ack, answer) = tokio::sync::oneshot::channel();
        if release.send(ReleaseCommand { ack }).is_err() {
            // The consumer already ended (its connection closed), taking the
            // local handle with it; there is nothing left to unsubscribe.
            return Ok(());
        }
        match answer.await {
            Ok(result) => result,
            // The consumer ended without answering: its task was aborted with
            // the cache, so this connection is gone.
            Err(_) => Err(CodexError::NoResponse(format!("thread/unsubscribe {thread_id}"))),
        }
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
        let deadline = std::time::Instant::now() + STOP_LOOKUP_BOUND;
        loop {
            // Each read is bounded: a wedged RPC must not hold `/stop` past its
            // own look-up window.
            match tokio::time::timeout(STOP_LOOKUP_READ_TIMEOUT, self.threads().read(session_id, true)).await
            {
                Ok(Ok(thread)) => {
                    if let Some(turn) = thread
                        .turns
                        .iter()
                        .rev()
                        .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
                        .and_then(|turn| turn.get("id").and_then(Value::as_str))
                    {
                        return Ok(Some(turn.to_string()));
                    }
                }
                Ok(Err(error)) => return Err(client_error(error)),
                Err(_) => {
                    tracing::warn!(
                        thread = %session_id,
                        "the codex thread read did not answer within {STOP_LOOKUP_READ_TIMEOUT:?} \
                         during a stop; retrying until the lookup bound"
                    );
                }
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(STOP_LOOKUP_INTERVAL.min(deadline - now)).await;
        }
    }

    /// Bind every pending submission this read can place: the user item whose
    /// `clientId` is a pending chosen id tells us which turn cola's accepted
    /// (but unanswered, or pre-restart) prompt landed in. This is the recovery
    /// path for a lost `turn/start` response (ADR-0092 decision 2), and it runs
    /// on the read the bridge is about to make, before it asks authorship.
    fn reconcile_submissions(&self, thread_id: &str, transcript: &SessionTranscript) {
        // Prune first: the safety valve must be able to stop an aged id from
        // vouching on the very read that would otherwise bind it.
        self.authored.expire(thread_id);
        for (submission, message) in &transcript.submissions {
            // Only an id this record actually saw may vouch. A peer chooses its
            // own `clientUserMessageId`, so the id's shape proves nothing: an
            // unfamiliar id — however it is spelled — is somebody else's.
            let Some(kind) = self.authored.pending_kind(thread_id, submission) else {
                continue;
            };
            let Some((owner, turn)) = self.projections.turn_of_item(message.as_str()) else {
                continue;
            };
            if owner != thread_id {
                continue;
            }
            match kind {
                // A prompt claims the turn it started.
                SubmissionKind::Prompt => self.authored.bind_submission(thread_id, submission, &turn),
                // A steer claims only its own item; the turn belongs to whoever
                // started it.
                SubmissionKind::Steer => self.authored.resolve_steer(thread_id, submission),
            }
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
            "input": self.input_items(session_id, text, skills)?,
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
    /// The session's own directory is tried first, so a same-named skill in
    /// another project can never be substituted for it.
    fn input_items(&self, session_id: &str, text: &str, skills: &[PromptSkill]) -> Result<Value> {
        // `text_elements` is the protocol's spelling (`UserInput::Text` keeps
        // it snake_case on 0.163.0, unlike the surrounding camelCase fields).
        let mut input = vec![json!({ "type": "text", "text": text, "text_elements": [] })];
        let directory = self.projections.cwd(session_id);
        for skill in skills {
            let Some(path) = self.skills.path_of(&skill.id, directory.as_deref()) else {
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
    ///
    /// Adoption and paging run in the subscriber task, never on the read path.
    /// A read waits at most [`INITIAL_READ_BOUND`] for the first projection and
    /// otherwise returns what is cached, explicitly truncated; the task keeps
    /// paging and the next read sees the complete history.
    pub(crate) async fn ensure_subscription(&self, thread_id: &str) -> CodexResult<()> {
        match self.projections.state(thread_id) {
            Some(SubscriptionState::Live) => return Ok(()),
            Some(SubscriptionState::PagingIncomplete { .. }) => {
                // A read never pages. It signals the subscriber, which is the
                // only consumer, so no event can be held across a page.
                self.projections.request_resync(thread_id);
                return Ok(());
            }
            Some(SubscriptionState::Adopting) => {
                self.wait_for_initial(thread_id).await;
                return Ok(());
            }
            None => {}
        }
        let adoption = self.projections.adoption_lock(thread_id);
        let adopting = adoption.lock().await;
        match self.projections.state(thread_id) {
            Some(SubscriptionState::Live) => return Ok(()),
            Some(SubscriptionState::PagingIncomplete { .. }) => {
                self.projections.request_resync(thread_id);
                return Ok(());
            }
            Some(SubscriptionState::Adopting) => {
                drop(adopting);
                self.wait_for_initial(thread_id).await;
                return Ok(());
            }
            None => {
                // Register the release channel before spawning: a release that
                // arrives while `thread/resume` is in flight is queued for the
                // task instead of being lost.
                let (release, releases) = tokio::sync::mpsc::unbounded_channel();
                self.projections.watch_release(thread_id, release);
                self.projections.set_state(thread_id, SubscriptionState::Adopting);
                let task = tokio::spawn(run_thread(
                    self.threads().clone(),
                    Arc::downgrade(&self.projections),
                    thread_id.to_string(),
                    self.projections.resync_signal(thread_id),
                    releases,
                ));
                self.projections.track(task);
            }
        }
        drop(adopting);
        self.wait_for_initial(thread_id).await;
        Ok(())
    }

    /// Wait a bounded time for the subscriber to leave `Adopting`. On expiry
    /// the current projection is marked truncated (empty is acceptable) and
    /// the task keeps paging in the background.
    async fn wait_for_initial(&self, thread_id: &str) {
        if !self.projections.is_adopting(thread_id) {
            return;
        }
        let ready = self.projections.ready_signal(thread_id);
        let _ = tokio::time::timeout(INITIAL_READ_BOUND, async {
            loop {
                if !self.projections.is_adopting(thread_id) {
                    break;
                }
                ready.notified().await;
            }
        })
        .await;
        self.projections.mark_incomplete_if_adopting(thread_id);
    }
}

/// Adopt, page, drain and then consume one thread. This runs as the thread's
/// subscriber task: the read path only signals it, so a slow `thread/resume`
/// or `thread/turns/list` can never hold a read.
async fn run_thread(
    threads: Threads,
    projections: std::sync::Weak<Projections>,
    thread_id: String,
    signal: Arc<tokio::sync::Notify>,
    releases: tokio::sync::mpsc::UnboundedReceiver<ReleaseCommand>,
) {
    // The model the resume reports describes the thread as it was when the
    // read started; stamp it BEFORE asking so an accepted pick cannot be
    // overwritten by an answer that was already in flight (issue #904).
    let observed = projections
        .upgrade()
        .map(|cache| cache.next_model_stamp())
        .unwrap_or_default();
    let subscription = match threads.adopt(&thread_id).await {
        Ok(Adoption::Subscribed(subscription)) => *subscription,
        Ok(Adoption::NoHistoryYet) => {
            // A thread created by `thread/start` cannot be resumed until it
            // has run a turn. That turn may be started by the user's own
            // client, so the state is NOT remembered as terminal: the next
            // read retries, and the subscription starts the moment the
            // rollout exists (the peer-attachment promise).
            if let Some(cache) = projections.upgrade() {
                // A thread with no rollout has an empty, complete history; an
                // earlier bounded read must not leave it looking truncated.
                // Clear the state first so a timed-out waiter stops marking;
                // then clear the flag its earlier mark may have set.
                cache.clear_state(&thread_id);
                cache.clear_incomplete(&thread_id);
            }
            return;
        }
        Err(error) => {
            tracing::warn!(thread = %thread_id, "could not adopt the codex thread: {error}");
            if let Some(cache) = projections.upgrade() {
                cache.forget(&thread_id);
            }
            return;
        }
    };
    {
        let Some(cache) = projections.upgrade() else {
            return;
        };
        cache.remember_thread_metadata(subscription.thread(), observed);
    }
    // Every status this connection had already received when the history
    // request starts is superseded by the page; a status received after this
    // boundary still applies (it is newer than the page).
    let boundary = threads.inbound_seq();
    // `thread/resume` carries only a bounded history page. Page every turn
    // before the subscriber starts folding; the receiver was registered
    // before `thread/resume`, so events that arrive during this paging wait
    // in its queue. Hold no cache reference across the await: a replaced
    // attachment must still be able to drop the cache and abort this task.
    let paged = threads.all_turns(&thread_id).await;
    let Some(cache) = projections.upgrade() else {
        return;
    };
    match paged {
        Ok(turns) => {
            cache.resync(&thread_id, &turns, boundary);
            cache.set_state(&thread_id, SubscriptionState::Live);
        }
        Err(error) => {
            tracing::warn!(
                thread = %thread_id,
                "could not page the codex thread history on adoption; the bounded resume \
                 page is explicitly incomplete: {error}"
            );
            cache.seed(&thread_id, &subscription.thread().turns, boundary);
            cache.mark_incomplete(&thread_id);
            cache.set_state(
                &thread_id,
                SubscriptionState::PagingIncomplete {
                    retry_at: Instant::now() + PAGING_RETRY,
                },
            );
        }
    }
    // Drain the events that queued while `thread/resume` and the history page
    // were in flight, so the first read reflects them and cannot present a
    // transcript as complete when the backlog already overflowed the broadcast
    // buffer.
    let mut subscription = subscription;
    let mut drained = 0;
    let mut backlog_truncated = false;
    let mut subscription_dead = false;
    loop {
        if drained >= cache.drain_limit() {
            backlog_truncated = true;
            break;
        }
        match subscription.try_next_event() {
            Ok(Some(ThreadEvent::Notification { seq, method, params })) => {
                cache.apply_at(&thread_id, seq, &method, &params);
                drained += 1;
            }
            Ok(Some(ThreadEvent::Lagged { skipped })) => {
                tracing::warn!(
                    thread = %thread_id,
                    skipped,
                    "the adoption backlog overflowed the event buffer; marking the projection \
                     incomplete"
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
        // No live subscription is left to page through: forget the state so
        // the next read re-adopts rather than sitting in a paging retry
        // against a dead connection.
        cache.forget(&thread_id);
    } else if backlog_truncated {
        // A limit hit (or a lag) leaves the projection incomplete with no
        // later lag to schedule a resync. Enter the paging-retry state so a
        // later read recovers instead of staying truncated forever.
        cache.mark_incomplete(&thread_id);
        cache.set_state(
            &thread_id,
            SubscriptionState::PagingIncomplete {
                retry_at: Instant::now() + PAGING_RETRY,
            },
        );
    }
    drop(cache);
    consume(subscription, threads, projections, signal, releases).await;
}

/// Fold one subscription's events into the cache. Notifications are applied
/// inline; a lag or a lost connection is explicitly surfaced. A release command
/// ends the task: it is the only holder of the subscription, so it sends the
/// acknowledged `thread/unsubscribe` (unless another local handle still holds
/// the thread) and forgets the thread's live state.
async fn consume(
    mut subscription: ThreadSubscription,
    threads: Threads,
    projections: std::sync::Weak<Projections>,
    signal: Arc<tokio::sync::Notify>,
    mut releases: tokio::sync::mpsc::UnboundedReceiver<ReleaseCommand>,
) {
    let thread_id = subscription.thread().id.clone();
    let command = loop {
        // The subscriber is the only consumer, so an event is either dequeued
        // and applied immediately or left queued while this task pages. It can
        // never be in hand across a page and applied twice.
        tokio::select! {
            biased;
            command = releases.recv() => break command,
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
    };
    // A closed release channel means the cache went away with the client; the
    // subscription's drop releases our local handle just the same.
    let Some(command) = command else {
        return;
    };
    // This task holds the subscription, so only it can ask whether this was the
    // last local handle. `release` answers `Ok(None)` when another handle still
    // holds the thread — the server must keep streaming to it, and no
    // `thread/unsubscribe` is sent.
    let result = subscription.release().await.map(|_| ());
    if let Some(projections) = projections.upgrade() {
        projections.released(&thread_id);
    }
    // The caller may have bounded its wait and gone (a slow server, a
    // cancelled command): the release still happened, so a dropped ack is not
    // a failure.
    let _ = command.ack.send(result);
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

/// How long `/stop` polls for a turn the server has not persisted yet. A
/// `turn/start` the server accepts is persisted within tens of milliseconds, so
/// a stop that lands just before the turn exists retries this long before it
/// declares the session idle.
#[cfg(not(test))]
const STOP_LOOKUP_BOUND: Duration = Duration::from_millis(600);
#[cfg(test)]
const STOP_LOOKUP_BOUND: Duration = Duration::from_millis(200);

/// One poll's own deadline: a stalled `thread/read` must not hold `/stop`, so a
/// slow read is abandoned and the loop either retries or reaches its bound.
#[cfg(not(test))]
const STOP_LOOKUP_READ_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(test)]
const STOP_LOOKUP_READ_TIMEOUT: Duration = Duration::from_millis(60);

#[cfg(not(test))]
const STOP_LOOKUP_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(test)]
const STOP_LOOKUP_INTERVAL: Duration = Duration::from_millis(10);

/// The bound on the thread metadata read a skill prompt takes when this client
/// has not learned the session's directory yet. A half-open connection must not
/// hang the prompt before submission; on expiry the prompt fails by name, since
/// a skill cannot be resolved without its directory.
#[cfg(not(test))]
const SKILL_CWD_READ_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const SKILL_CWD_READ_TIMEOUT: Duration = Duration::from_millis(60);

/// The provider bucket the `/model` picker shows for Codex. `model/list` names
/// no provider: a Codex model is whatever the attached app-server resolves
/// through its own `[model_providers.*]`, and the picker's `provider/model`
/// ref is cola's neutral mirror, so a client-scoped bucket is the honest label
/// — never a claim that the model is served by OpenAI.
const MODEL_PROVIDER: &str = "codex";

/// The `(provider, model)` a thread runs with, as the server reports them
/// (issue #904). `model/list` names no provider — its catalog is flat — so an
/// absent `modelProvider` falls back to that same neutral bucket rather than
/// claiming OpenAI. `None` when the server named no model: a fact cola does not
/// have is never filled in from config (that would be `config/read`, a
/// different read).
fn thread_model(thread: &Thread) -> Option<ModelIdentity> {
    let model_id = thread.model.as_deref().filter(|model| !model.trim().is_empty())?;
    let provider_id = thread
        .model_provider
        .as_deref()
        .filter(|provider| !provider.trim().is_empty())
        .unwrap_or(MODEL_PROVIDER);
    Some(ModelIdentity {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        variant: None,
    })
}

/// Bound the `/model` catalog read. A server that keeps minting fresh cursors
/// would otherwise hold the picker forever; past the bound the catalog is what
/// was read so far, and the truncation is logged.
const MAX_MODEL_PAGES: usize = 64;

/// A catalog read with **no ack budget** (a command's page, or the variant
/// validation a model pick awaits) still gets a finite total bound: a silently
/// stalled `model/list` must not hold the pick forever. Alive-but-slow servers
/// still page through it; the partial catalog is "unknown", which preserves a
/// stored variant rather than deciding it has none.
#[cfg(not(test))]
const CATALOG_READ_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const CATALOG_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// One `model/list` round-trip's own bound inside that read. A request that
/// does not answer in this time stops the page loop; whatever was read so far
/// is the catalog.
#[cfg(not(test))]
const CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_millis(200);

/// Map Codex's flat `model/list` catalog onto the client-declared surface when
/// there is no config read to enrich it (an unattached lifecycle, or the cheap
/// catalog-only read the `/think` variant lookup asks for). The real surface is
/// [`super::model_surface::build_surface`], which adds the config default,
/// profile presets, probed provider models and the provider scope.
fn model_surface_from_catalog(providers: &[ProviderModels], query: &ModelSurfaceQuery<'_>) -> ModelSurface {
    super::model_surface::build_surface(
        providers,
        &super::model_surface::CodexModelConfig::default(),
        &std::collections::HashMap::new(),
        None,
        query,
    )
}

/// The `skills/list` reads, scoped by the directory the read was made for.
/// Codex's `skill` prompt part carries a **path** as well as a name, and the
/// neutral [`crate::backend::PromptSkill`] only carries `{ id, name }`, so the
/// path is remembered here from the same read the `/skill` command made
/// immediately before the prompt.
///
/// Each read **replaces its own scope**: a skill deleted between two lists must
/// stop resolving (the prompt then fails by name, never sending a stale path),
/// while a read for another project cannot erase this one's entries. A prompt
/// resolves **only** in the session's own directory: a same-named skill from
/// another project is a different file and must never be substituted.
#[derive(Default)]
pub(crate) struct SkillCatalog {
    /// Directory key (`""` for the server default) → (skill name → path).
    scopes: Mutex<HashMap<String, HashMap<String, String>>>,
}

impl SkillCatalog {
    /// Replace one directory's scope with what its latest read advertised.
    /// `skills` empty is a real "this directory has none" answer: the scope is
    /// emptied, so a vanished skill is refused rather than resolved stale.
    fn replace_scope(&self, directory: Option<&str>, skills: impl IntoIterator<Item = (String, String)>) {
        let key = directory.unwrap_or_default().to_string();
        let paths: HashMap<String, String> = skills.into_iter().collect();
        let mut scopes = self.scopes.lock().expect("codex skill catalog");
        if paths.is_empty() {
            scopes.remove(&key);
        } else {
            scopes.insert(key, paths);
        }
    }

    /// Resolve `name` for the session's own directory, and only there. A skill
    /// advertised for another project must never be substituted: the paths and
    /// the skill itself may differ, and the prompt would silently use the wrong
    /// one. A thread whose `cwd` is unknown resolves nothing — the adapter
    /// reads the thread before a skill prompt rather than guessing.
    fn path_of(&self, name: &str, directory: Option<&str>) -> Option<String> {
        let directory = directory?;
        self.scopes
            .lock()
            .expect("codex skill catalog")
            .get(&scope_key(Some(directory)))
            .and_then(|scope| scope.get(name))
            .cloned()
    }
}

/// The catalog's key for a directory: trailing separators removed, so a
/// configured `/work/cola` and a server `cwd` of `/work/cola/` meet.
fn scope_key(directory: Option<&str>) -> String {
    directory
        .unwrap_or_default()
        .trim_end_matches(['/', '\\'])
        .to_string()
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

/// Whether an id can name a Codex thread. Thread ids are UUIDs, not OpenCode
/// `ses…` handles; only an empty id (a request that named no thread) is
/// unaddressable. Shared by the client and the lifecycle's static answer.
fn codable_thread_id(id: &str) -> bool {
    !id.trim().is_empty()
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

impl CodexClient {
    /// `model/list` paged to exhaustion, or until `deadline` (a card
    /// callback's slice) leaves no time for the next page.
    ///
    /// A deadline hit keeps the pages already read: a partial catalog is a
    /// usable degraded surface (the rest of the card still renders), whereas
    /// waiting on a stalled server would miss the callback ack. **`None` is
    /// still finite**: a command's page or a validation read gets
    /// [`CATALOG_READ_TIMEOUT`] in total, and each request is additionally
    /// bounded by [`CATALOG_REQUEST_TIMEOUT`], so a permanently silent server
    /// cannot hold a model pick forever. `MAX_MODEL_PAGES` caps the loop.
    pub(crate) async fn list_models_within(
        &self,
        deadline: Option<tokio::time::Instant>,
    ) -> Vec<ProviderModels> {
        let deadline = deadline.unwrap_or_else(|| tokio::time::Instant::now() + CATALOG_READ_TIMEOUT);
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        let mut models = Vec::new();
        for page in 0..MAX_MODEL_PAGES {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("codex model/list reached its read deadline; showing the catalog read so far");
                break;
            }
            let mut params = json!({});
            if let Some(cursor) = cursor.as_deref() {
                params["cursor"] = json!(cursor);
            }
            let request = self.connection().request("model/list", params);
            // A callback's slice may be shorter than one request's own bound;
            // the earlier of the two wins.
            let request_deadline = deadline.min(tokio::time::Instant::now() + CATALOG_REQUEST_TIMEOUT);
            let result = match tokio::time::timeout_at(request_deadline, request).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(
                        "codex model/list did not answer before its read deadline; \
                         showing the catalog read so far"
                    );
                    break;
                }
            };
            let result = match result {
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
                    ..ModelOption::default()
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
        let observed_stamp = self.projections.next_model_stamp();
        let thread = self.threads().start(cwd).await.map_err(client_error)?;
        self.projections.remember_thread_metadata(&thread, observed_stamp);
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
        // Stop mirroring before the thread disappears, so the subscriber task is
        // not left folding a stream whose thread is gone. A failed release is
        // not the delete's failure: the server is the authority on the delete.
        if let Err(error) = self.release_thread(session_id).await {
            tracing::debug!(
                thread = %session_id,
                "could not release the codex thread subscription before deleting the thread: {error}"
            );
        }
        self.connection()
            .request("thread/delete", json!({ "threadId": session_id }))
            .await
            .map_err(client_error)?;
        self.projections.evict(session_id, false);
        // The thread is gone; its authorship turns cannot be asked about again,
        // and a recreated id must not inherit them.
        self.authored.forget(session_id);
        Ok(())
    }

    async fn release_session(&self, session_id: &str) -> Result<()> {
        self.release_thread(session_id).await.map_err(client_error)
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
        // A skill resolves in the session's own directory only, and the prompt
        // axis carries no directory: learn the thread's `cwd` from the server
        // when this client has not seen it yet (a fresh restart can prompt
        // before its first read). A failed read leaves the directory unknown,
        // and a skill is then refused rather than taken from another project.
        if !skills.is_empty() && self.projections.cwd(session_id).is_none() {
            let observed_stamp = self.projections.next_model_stamp();
            match tokio::time::timeout(SKILL_CWD_READ_TIMEOUT, self.threads().read(session_id, false)).await {
                Ok(Ok(thread)) => self.projections.remember_thread_metadata(&thread, observed_stamp),
                Ok(Err(error)) => {
                    return Err(BridgeError::Client {
                        client: "codex",
                        detail: format!(
                            "could not read the thread's directory before a skill prompt: {error}"
                        ),
                    });
                }
                Err(_) => {
                    return Err(BridgeError::Client {
                        client: "codex",
                        detail: format!(
                            "the codex thread metadata read did not answer within \
                             {SKILL_CWD_READ_TIMEOUT:?}; refusing to submit a skill prompt \
                             without the session's directory"
                        ),
                    });
                }
            }
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
        if let Err(detail) = self.authored.begin(session_id, message_id) {
            // The marker is a precondition: a prompt the record did not learn
            // about is exactly the turn a restart would report as external.
            return Err(BridgeError::Client {
                client: "codex",
                detail,
            });
        }
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
        // A pick rides `turn/start` "for this turn and subsequent turns", so
        // the thread now runs this model: record it here or the footer would
        // keep naming the model from before the pick (issue #904). The
        // response is the acceptance this waits for — a refused turn/start
        // changed nothing.
        if let Some(model) = model {
            // The pick's provider is the neutral `codex` bucket when it came from
            // the picker, and a model pick does not change the thread's provider
            // (`model_provider` is configuration, not part of `turn/start`). A
            // provider the thread already named is the honest label to show —
            // the footer must not claim `codex/<model>` for a custom provider.
            let provider_id = self
                .projections
                .model(session_id)
                .map(|held| held.provider_id)
                .unwrap_or_else(|| model.provider_id.clone());
            self.projections.record_sent_model(
                session_id,
                ModelIdentity {
                    provider_id,
                    model_id: model.id.clone(),
                    variant: None,
                },
                self.projections.next_model_stamp(),
            );
        }
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
        let input = self.input_items(session_id, text, skills)?;
        let mut params = json!({
            "threadId": session_id,
            "expectedTurnId": turn_id,
            "input": input,
        });
        if let Some(message_id) = message_id {
            params["clientUserMessageId"] = json!(message_id);
        }
        // Record the id before the request, as a **steer**: only the item the
        // server writes for it is cola's. The turn may have been started by the
        // user's TUI, so neither the turn nor its other items are claimed — an
        // id-less item in it must stay external. Recording before the send also
        // keeps an ambiguous (lost) response reconcilable by id.
        self.authored
            .begin_steer(session_id, message_id)
            .map_err(|detail| BridgeError::Client {
                client: "codex",
                detail,
            })?;
        // No `model`/`effort` here: a steer adds input to a turn that already
        // captured its settings.
        let result = match self.connection().request("turn/steer", params).await {
            Ok(result) => result,
            Err(error) => {
                if definitely_refused(&error) || message_id.is_none() {
                    self.authored.abandon(session_id, message_id);
                }
                // An ambiguous failure keeps the marker: the steer may have
                // landed, and the next history read resolves it by its id.
                return Err(client_error(error));
            }
        };
        let _ = result;
        // The server accepted the steer, so the item exists and the id is now a
        // resolved precise match (it stays recorded for later reads, including
        // after a restart, but no longer counts as unresolved).
        if let Some(message_id) = message_id {
            self.authored.resolve_steer(session_id, message_id);
        }
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
        request_id: &str,
        reply: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        // The server request lives on this connection, so the directory is not
        // a routing axis: a Codex approval is answered where it arrived.
        self.approvals
            .reply_permission(self.connection(), request_id, reply)
            .await
    }

    async fn list_permissions(&self, directory: Option<&str>) -> Result<Vec<PermissionRequest>> {
        Ok(self.approvals.permissions(directory))
    }

    async fn list_questions(&self, directory: Option<&str>) -> Result<Vec<QuestionRequest>> {
        Ok(self.approvals.questions(directory))
    }

    async fn reply_question(
        &self,
        _session_id: &str,
        request_id: &str,
        answers: &[FormAnswer],
        _directory: Option<&str>,
    ) -> Result<()> {
        self.approvals
            .reply_question(self.connection(), request_id, answers)
            .await
    }

    async fn reject_question(
        &self,
        _session_id: &str,
        request_id: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        self.approvals
            .reject_question(self.connection(), request_id)
            .await
    }

    fn is_addressable_session_id(&self, id: &str) -> bool {
        codable_thread_id(id)
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
        if self.projections.is_adopting(session_id) {
            // The bounded first-read wait expired before adoption finished; an
            // empty cache must not answer `Idle` as if it were observed.
            return Ok(None);
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
        self.list_models_within(None).await
    }

    /// The `/model` surface: the built-in catalog (global, its own group), the
    /// config default and `[profiles.*]` presets (one-click model picks), every
    /// successfully probed provider's models (per-provider groups), and the
    /// configured providers as a thread-level scope. Every model pick keeps the
    /// `codex/<id>` callback value the text form records; the provider scope is
    /// listing only — switching is a later ticket, so a provider click falls
    /// back to this same page (with the bridge's back button) rather than
    /// pretending to change the thread's channel.
    ///
    /// # Read budgets
    ///
    /// A root `/model` page is a text command, so `query.ack_budget` is `None`:
    /// it keeps full fidelity, with a
    /// [`super::model_surface::CONFIG_READ_TIMEOUT`] (2 s) config read and a
    /// catalog read bounded by [`CATALOG_READ_TIMEOUT`] in total (2 s per
    /// request), with fresh probes.
    ///
    /// A **drilled** page is a card callback and arrives with `ack_budget =
    /// Some(budget)`. It cannot chain those two waits, so **both** its config
    /// read and its catalog read get `budget - 1.4 s` (600 ms out of a 2 s
    /// budget) — they run concurrently, so the surface returns by about that
    /// slice — probes are served **cache-only** (zero network), and the rest of
    /// the budget is left for the card build and the ack response. A stalled
    /// `model/list` yields the pages read so far (or an empty catalog) plus the
    /// normal note and hand-write hint, never a hung card. A cold drilled click
    /// therefore shows the providers without their probed models; the next root
    /// open fills them.
    async fn model_surface(&self, query: ModelSurfaceQuery<'_>) -> ModelSurface {
        // `/think`'s variant lookup is the one caller that asks for the bare
        // catalog (`catalog_only`): it runs on a card callback's short ack
        // budget and needs only the advertised variants. Every card page —
        // a root page or a Pending Session's drilled provider read — keeps the
        // full config/provider shape.
        let fast = query.catalog_only;
        // A callback-originated read carries its budget; a command's page does
        // not. `catalog_only` never reaches the config/probe path.
        let callback = query.ack_budget.is_some();
        // One slice, used by both concurrent reads. The catalog's own page loop
        // is bounded by this deadline too, so a paginated-but-alive server
        // cannot spend the whole budget.
        let catalog_deadline = super::model_surface::callback_read_slice(query.ack_budget)
            .map(|slice| tokio::time::Instant::now() + slice);
        // With a deadline the catalog is read through the page loop's bounded
        // form; without one it stays the trait's read (the declared catalog
        // source). Both produce the same `ProviderModels`.
        let catalog = async {
            match catalog_deadline {
                Some(deadline) => self.list_models_within(Some(deadline)).await,
                None => Backend::list_models(self).await,
            }
        };
        let (catalog, config) = if fast {
            (catalog.await, super::model_surface::CodexModelConfig::default())
        } else {
            let (catalog, config) = tokio::join!(
                catalog,
                super::model_surface::read_config(
                    self.connection(),
                    query.directory,
                    super::model_surface::config_read_timeout(query.ack_budget),
                )
            );
            let config = config.unwrap_or_else(|error| {
                tracing::debug!(
                    "codex config/read failed; the model surface shows the catalog only: {error}"
                );
                super::model_surface::CodexModelConfig {
                    read_failed: true,
                    ..Default::default()
                }
            });
            (catalog, config)
        };
        let probes = if fast {
            Default::default()
        } else if callback {
            // A callback must not start a probe: 2 s of network on top of the
            // config read would miss the ack budget. The previous root open
            // already warmed this cache; a cold click simply omits the groups.
            self.model_probes.cached(&config.providers)
        } else {
            self.model_probes.probe_all(&config.providers).await
        };
        // A picker pick records the neutral `codex` bucket, so the thread's own
        // ordered model fact (when this connection has one) is the honest
        // provider marker.
        let thread_provider = query
            .session_id
            .and_then(|session_id| self.projections.model(session_id))
            .map(|model| model.provider_id);
        super::model_surface::build_surface(&catalog, &config, &probes, thread_provider.as_deref(), &query)
    }

    async fn session_info(&self, session_id: &str, _directory: Option<&str>) -> Result<SessionInfo> {
        let observed_stamp = self.projections.next_model_stamp();
        let thread = self
            .threads()
            .read(session_id, false)
            .await
            .map_err(client_error)?;
        // The read is the freshest model fact cola has (a peer client may have
        // switched it), so the footer's projection learns it too — the same
        // read serves both surfaces (issue #904). Its stamp was taken before the
        // request, so a pick accepted while it was in flight still wins; the
        // answer therefore comes from the ordered store, never from this
        // response alone (the picker must not show a name the picker's own pick
        // superseded).
        self.projections.remember_thread_metadata(&thread, observed_stamp);
        // Keyed by the id the response names, the same id the metadata was
        // remembered under: a response that named another thread must not report
        // this session's cached model for it.
        let model = self.projections.model(&thread.id).map(|model| SessionModel {
            provider_id: model.provider_id,
            id: model.model_id,
        });
        let title = (!thread.title().is_empty()).then(|| thread.title().to_string());
        Ok(SessionInfo {
            id: thread.id,
            parent_id: thread.parent_thread_id,
            title,
            model,
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

    async fn release_session(&self, session_id: &str) -> Result<()> {
        Backend::release_session(&self.current().await?, session_id).await
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

    fn is_addressable_session_id(&self, id: &str) -> bool {
        codable_thread_id(id)
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

    async fn model_surface(&self, query: ModelSurfaceQuery<'_>) -> ModelSurface {
        match self.attached_client().await {
            Some(client) => Backend::model_surface(&client, query).await,
            None => model_surface_from_catalog(&[], &query),
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
    use crate::backend::MessageRole;
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

    /// Issue #904: the thread is where Codex keeps the model fact (`model` +
    /// `modelProvider`), and it is the only place — items carry none. Both the
    /// card footer (through the transcript) and the `/model` current-model line
    /// (through `session_info`) read it from there, so a session cola never
    /// picked a model for still names the model it is actually running.
    #[tokio::test]
    async fn the_threads_model_reaches_the_transcript_and_the_session_info() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        thread.model = Some("gpt-6-luna".into());
        thread.model_provider = Some("opencode-go".into());
        server.seed_thread(thread);
        let client = connect(&url).await;

        let transcript = client.transcript("thread-1").await.expect("the transcript read");
        let answering = transcript
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("the turn's assistant message");
        let model = answering.model.as_ref().expect("the model the thread runs with");
        assert_eq!(model.model_id, "gpt-6-luna");
        assert_eq!(model.provider_id, "opencode-go");

        let info = client
            .session_info("thread-1", Some("/work/cola"))
            .await
            .expect("the session read");
        let model = info.model.expect("the session names its model");
        assert_eq!(model.id, "gpt-6-luna");
        assert_eq!(model.provider_id, "opencode-go");
    }

    /// The wire's `modelProvider` is nullable, and cola must not turn "the
    /// server named no provider" into a false one: the neutral `codex` bucket
    /// is the documented fallback (`model/list` names no provider either), and
    /// a thread the server reports no model for stays modelless.
    #[tokio::test]
    async fn a_thread_without_a_reported_provider_or_model_shows_no_invented_one() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        thread.model = Some("gpt-6-luna".into());
        thread.model_provider = None;
        server.seed_thread(thread);
        let client = connect(&url).await;

        let info = client
            .session_info("thread-1", Some("/work/cola"))
            .await
            .expect("the session read");
        assert_eq!(
            info.model.map(|model| model.provider_id),
            Some("codex".to_string()),
            "an absent provider falls back to the neutral bucket"
        );

        let (bare_server, bare_url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut bare = ScriptedThread::adopted("thread-2", "session-two", "/work/cola", "bare");
        bare.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        bare_server.seed_thread(bare);
        let bare_client = connect(&bare_url).await;
        let info = bare_client
            .session_info("thread-2", Some("/work/cola"))
            .await
            .expect("the session read");
        assert!(info.model.is_none(), "no model on the wire, none invented");
        let transcript = bare_client.transcript("thread-2").await.expect("the read");
        assert!(
            transcript.messages.iter().all(|message| message.model.is_none()),
            "the transcript invents no model either"
        );
    }

    /// A thread read is authoritative **including silence** (review round 1):
    /// when the server stops naming a model for a thread, the projection clears
    /// the one it remembered, so the footer drops the name instead of keeping a
    /// stale one. A stale name would claim the wrong model answered.
    #[tokio::test]
    async fn a_read_that_names_no_model_clears_the_footers_model() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.model = Some("gpt-6-luna".into());
        thread.model_provider = Some("opencode-go".into());
        server.seed_thread(thread);
        let client = connect(&url).await;

        client.transcript("thread-1").await.expect("adopt");
        server.wait_for_subscribers("thread-1", 1).await;
        server.push_item("thread-1", "item-1", "one");
        let transcript = wait_for_text(&client, "thread-1", "one").await;
        assert!(
            transcript.messages.iter().any(|message| message.model.is_some()),
            "the thread already names its model"
        );

        server.set_thread_model("thread-1", None, None);
        let info = client
            .session_info("thread-1", Some("/work/cola"))
            .await
            .expect("the session read");
        assert!(info.model.is_none(), "the read named no model");
        let transcript = client.transcript("thread-1").await.expect("the read");
        assert!(
            transcript.messages.iter().all(|message| message.model.is_none()),
            "the silent read must clear the remembered model, not keep a stale one"
        );
    }

    /// The `/model` read is also how the footer learns a **peer's** switch (a
    /// TUI changing the thread's model), so `session_info` must feed the same
    /// projection the footer reads — not just answer the picker.
    #[tokio::test]
    async fn a_peer_model_switch_reaches_the_footer_through_the_session_read() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.model = Some("gpt-6-luna".into());
        thread.model_provider = Some("opencode-go".into());
        server.seed_thread(thread);
        let client = connect(&url).await;

        client.transcript("thread-1").await.expect("adopt");
        server.wait_for_subscribers("thread-1", 1).await;
        server.push_item("thread-1", "item-1", "one");
        wait_for_text(&client, "thread-1", "one").await;

        server.set_thread_model("thread-1", Some("gpt-5.5"), Some("opencode-go"));
        let info = client
            .session_info("thread-1", Some("/work/cola"))
            .await
            .expect("the session read");
        assert_eq!(
            info.model.map(|model| model.id),
            Some("gpt-5.5".to_string()),
            "the picker read names the model the peer switched to"
        );
        let transcript = client.transcript("thread-1").await.expect("the read");
        let answering = transcript
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("the assistant item");
        assert_eq!(
            answering.model.as_ref().map(|model| model.model_id.as_str()),
            Some("gpt-5.5"),
            "the footer's projection is refreshed by the same read"
        );
    }

    /// A refused `turn/start` changed nothing on the thread, so it must not
    /// move the recorded model: only the accepted response is evidence.
    #[tokio::test]
    async fn a_refused_pick_does_not_move_the_footers_model() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.model = Some("gpt-6-luna".into());
        thread.model_provider = Some("opencode-go".into());
        server.seed_thread(thread);
        let client = connect(&url).await;

        client.transcript("thread-1").await.expect("adopt");
        server.wait_for_subscribers("thread-1", 1).await;
        server.push_item("thread-1", "item-1", "one");
        wait_for_text(&client, "thread-1", "one").await;

        server.reject_next_turn_start("model is not available here");
        let pick = ModelInfo {
            id: "gpt-5.5".into(),
            provider_id: "opencode-go".into(),
            variant: None,
        };
        client
            .prompt("thread-1", "run it", &[], &[], Some(&pick), None, None, None)
            .await
            .expect_err("the refusal is an error");

        let transcript = client.transcript("thread-1").await.expect("the read");
        let answering = transcript
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("the assistant item");
        assert_eq!(
            answering.model.as_ref().map(|model| model.model_id.as_str()),
            Some("gpt-6-luna"),
            "a refused turn/start changed nothing on the thread"
        );
    }

    /// A server that omits the model field must not erase what cola just
    /// picked (review round 2): silence is not evidence against cola's own
    /// accepted `turn/start`, while a name the server gave IS dropped on
    /// silence (the test above). Same rule, seen from the two sources.
    #[tokio::test]
    async fn a_silent_read_keeps_the_models_cola_picked() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.model = Some("gpt-6-luna".into());
        thread.model_provider = Some("opencode-go".into());
        server.seed_thread(thread);
        let client = connect(&url).await;

        client.transcript("thread-1").await.expect("adopt");
        server.wait_for_subscribers("thread-1", 1).await;
        server.push_item("thread-1", "item-1", "one");
        wait_for_text(&client, "thread-1", "one").await;

        let pick = ModelInfo {
            id: "gpt-5.5".into(),
            provider_id: "opencode-go".into(),
            variant: None,
        };
        client
            .prompt("thread-1", "run it", &[], &[], Some(&pick), None, None, None)
            .await
            .expect("the prompt");

        // The server stops naming a model on the wire.
        server.set_thread_model("thread-1", None, None);
        let info = client
            .session_info("thread-1", Some("/work/cola"))
            .await
            .expect("the session read");
        assert_eq!(
            info.model.map(|model| model.id),
            Some("gpt-5.5".to_string()),
            "the picker line reads the same ordered store: cola's pick stands \
             while the wire names nothing"
        );

        let transcript = client.transcript("thread-1").await.expect("the read");
        let answering = transcript
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("the assistant item");
        assert_eq!(
            answering.model.as_ref().map(|model| model.model_id.as_str()),
            Some("gpt-5.5"),
            "an omitted field must not erase the model cola sent"
        );
    }

    /// A picker pick carries the neutral `codex` bucket as its provider (that
    /// is the reference cola sends back), but a model pick does not move the
    /// thread's `model_provider`. The footer must keep naming the real provider
    /// the thread reported, not claim `codex/<model>` for a custom one.
    #[tokio::test]
    async fn a_neutral_picker_value_keeps_the_threads_real_provider() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.model = Some("gpt-6-luna".into());
        thread.model_provider = Some("opencode-go".into());
        server.seed_thread(thread);
        let client = connect(&url).await;

        client.transcript("thread-1").await.expect("adopt");
        server.wait_for_subscribers("thread-1", 1).await;
        server.push_item("thread-1", "item-1", "one");
        wait_for_text(&client, "thread-1", "one").await;

        // Exactly what the picker's button value parses to: the neutral bucket.
        let pick = ModelInfo {
            id: "gpt-5.5".into(),
            provider_id: "codex".into(),
            variant: None,
        };
        client
            .prompt("thread-1", "run it", &[], &[], Some(&pick), None, None, None)
            .await
            .expect("the prompt");

        // The projection, not a fresh thread read: a read answers with what the
        // server currently reports, which the scripted server does not apply.
        let transcript = client.transcript("thread-1").await.expect("the read");
        let answering = transcript
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("the assistant item");
        let model = answering.model.as_ref().expect("the picked model");
        assert_eq!(model.model_id, "gpt-5.5");
        assert_eq!(
            model.provider_id, "opencode-go",
            "the footer keeps the thread's real provider"
        );

        // Once the server reflects the pick (it does: the override is "for this
        // turn and subsequent turns"), `/model`'s line names it too.
        server.set_thread_model("thread-1", Some("gpt-5.5"), Some("opencode-go"));
        let info = client
            .session_info("thread-1", Some("/work/cola"))
            .await
            .expect("the session read");
        let model = info.model.expect("the session's model");
        assert_eq!(model.id, "gpt-5.5");
        assert_eq!(model.provider_id, "opencode-go");
    }

    /// A `thread/read` that answers with a **different** thread than the one
    /// asked for must not hand this session the other thread's cached model:
    /// the read's id keys both the metadata update and the model lookup.
    #[tokio::test]
    async fn a_response_naming_another_thread_is_not_answered_with_this_ones_model() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut asked = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "asked");
        asked.model = Some("gpt-6-luna".into());
        asked.model_provider = Some("opencode-go".into());
        server.seed_thread(asked);
        let mut other = ScriptedThread::adopted("thread-2", "session-other", "/work/other", "other");
        other.model = Some("gpt-5.5".into());
        other.model_provider = Some("opencode-go".into());
        server.seed_thread(other);
        let client = connect(&url).await;

        assert_eq!(
            client
                .session_info("thread-1", Some("/work/cola"))
                .await
                .expect("the first read")
                .model
                .map(|model| model.id),
            Some("gpt-6-luna".to_string()),
            "thread-1's own model"
        );

        server.answer_next_thread_read_with("thread-2");
        let info = client
            .session_info("thread-1", Some("/work/cola"))
            .await
            .expect("the substituted read");
        assert_eq!(info.id, "thread-2", "the response names thread-2");
        assert_eq!(
            info.model.map(|model| model.id),
            Some("gpt-5.5".to_string()),
            "the model comes from the thread the response named, never thread-1's cache"
        );
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

    /// Spec #879: a mid-turn (here, first) read never blocks on a history
    /// page. A stalled page returns the cache as truncated, and the subscriber
    /// finishes the page in the background.
    #[tokio::test]
    async fn a_stalled_first_history_page_returns_promptly_then_completes() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        thread.turns_page_delay_ms = 400;
        server.seed_thread(thread);
        let client = connect(&url).await;

        let started = std::time::Instant::now();
        let first = client.transcript("thread-1").await.expect("bounded first read");
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "the first read waited for the stalled page: {:?}",
            started.elapsed()
        );
        assert!(
            first.truncated,
            "a read that outran the page must say it is truncated"
        );
        assert!(
            first.messages.is_empty(),
            "the stalled page has not delivered a projection yet"
        );

        let complete = wait_for_text(&client, "thread-1", "one").await;
        assert!(!complete.truncated, "the background page completed the read");
    }

    /// Released threads evict their transcripts; only the small Turn Diff
    /// snapshot survives, and re-adoption pages the history again.
    #[tokio::test]
    async fn released_threads_evict_transcripts_and_keep_their_diff() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        for index in 0..3 {
            let id = format!("thread-{index}");
            let mut thread = ScriptedThread::adopted(&id, "session-root", "/work/cola", &id);
            thread.turns = vec![scripted_turn(
                "turn-1",
                "completed",
                1_700_000_000,
                &format!("item-{index}"),
                &format!("text {index}"),
            )];
            server.seed_thread(thread);
        }
        let client = connect(&url).await;

        for index in 0..3 {
            let id = format!("thread-{index}");
            wait_for_text(&client, &id, &format!("text {index}")).await;
        }
        assert_eq!(client.projections.projection_count(), 3);

        server.push_to_thread(
            "thread-0",
            "turn/diff/updated",
            json!({
                "threadId": "thread-0",
                "turnId": "turn-1",
                "diff": "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n"
            }),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let diff = client
                .session_diff("thread-0", Some("item-0"), None, None)
                .await
                .expect("diff read");
            if diff.as_ref().is_some_and(|diff| !diff.is_empty()) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the diff notification never applied"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        for index in 0..3 {
            let id = format!("thread-{index}");
            client.release_thread(&id).await.expect("release");
        }
        assert_eq!(
            client.projections.projection_count(),
            0,
            "released threads must not retain their transcripts"
        );

        let diff = client
            .session_diff("thread-0", Some("item-0"), None, None)
            .await
            .expect("diff read")
            .expect("the retained diff snapshot");
        assert_eq!(diff.0.len(), 1);
        assert_eq!(diff.0[0].path, "src/lib.rs");

        let restored = wait_for_text(&client, "thread-0", "text 0").await;
        assert!(
            !restored.truncated,
            "re-adoption pages the evicted transcript again"
        );
        assert_eq!(client.projections.projection_count(), 1);
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

    /// The `/model` surface has two scopes: the per-turn `model` scope (the
    /// built-in catalog in its own group plus whatever config/probes add) and
    /// the thread-level `provider` scope. The callback value stays `codex/<id>`
    /// (what the text form records), and the hand-write hint names that shape.
    #[test]
    fn model_surface_has_a_model_scope_and_a_provider_scope() {
        use crate::backend::types::{ChangeScope, ModelSelection, ModelSurfaceQuery};

        let providers = vec![ProviderModels {
            provider: MODEL_PROVIDER.to_string(),
            models: vec![ModelOption {
                id: "gpt-5.1-codex".into(),
                variants: vec!["low".into(), "high".into()],
                ..Default::default()
            }],
        }];
        let query = ModelSurfaceQuery {
            session_id: Some("thread-1"),
            directory: Some("/work/cola"),
            current: Some(ModelSelection {
                label: "opencode-go/gpt-6-luna@high".into(),
                reference: "opencode-go/gpt-6-luna".into(),
            }),
            ..Default::default()
        };
        let mut config = crate::codex::model_surface::CodexModelConfig::default();
        config
            .providers
            .push(crate::codex::model_surface::ConfiguredProvider {
                id: "opencode-go".into(),
                name: "OpenCode Go".into(),
                base_url: None,
                token: None,
                env_key: None,
            });

        let surface = crate::codex::model_surface::build_surface(
            &providers,
            &config,
            &std::collections::HashMap::new(),
            Some("opencode-go"),
            &query,
        );

        assert_eq!(surface.scopes.len(), 2, "model + provider, no pseudo level");
        let model = &surface.scopes[0];
        assert_eq!(model.id, "model");
        assert_eq!(model.title, "模型（每轮可换）");
        assert_eq!(model.changeable, ChangeScope::ThisTurn);
        assert_eq!(model.options[0].id, "gpt-5.1-codex");
        assert_eq!(model.options[0].callback_value(), "codex/gpt-5.1-codex");
        assert_eq!(model.options[0].variants, ["low", "high"]);
        assert_eq!(
            model.options[0].group.as_deref(),
            Some(crate::codex::model_surface::CATALOG_GROUP),
            "the global catalog is its own group, never a provider's"
        );
        assert!(
            model.free_text.as_deref().unwrap().contains("codex/<id>"),
            "hand-write hint names the provider prefix"
        );
        assert_eq!(
            surface.current.as_ref().map(|c| c.label.as_str()),
            Some("opencode-go/gpt-6-luna@high")
        );

        let provider = &surface.scopes[1];
        assert_eq!(provider.id, "provider");
        assert_eq!(provider.changeable, ChangeScope::NewThread);
        assert_eq!(provider.options[0].id, "opencode-go");
        assert_eq!(provider.options[0].label.as_deref(), Some("OpenCode Go（当前）"));
        assert_eq!(provider.options[0].callback_value(), "opencode-go");
    }

    /// An empty catalog still answers both scopes with a note and the
    /// hand-write hint — never a blank card.
    #[test]
    fn model_surface_degrades_to_a_note_and_hint() {
        use crate::backend::types::ModelSurfaceQuery;

        let surface = model_surface_from_catalog(&[], &ModelSurfaceQuery::default());
        let scope = &surface.scopes[0];
        assert!(scope.options.is_empty());
        assert_eq!(scope.note.as_deref(), Some("_(没有可用模型)_"));
        assert!(scope.free_text.is_some());
        let provider = &surface.scopes[1];
        assert!(provider.options.is_empty());
        assert_eq!(provider.note.as_deref(), Some("_(config 里没有 provider)_"));
    }

    /// The wired surface: `config/read` supplies providers, the config default
    /// and profiles; a provider with a token is probed against its own
    /// `/models`; a second open inside the TTL reuses that answer.
    #[tokio::test]
    async fn model_surface_wires_config_read_and_per_provider_probes() {
        use crate::backend::types::{ChangeScope, ModelSelection, ModelSurfaceQuery};

        let probed = crate::test_http::TestHttpServer::start().await;
        probed.route(
            "GET",
            "/v1/models",
            200,
            r#"{"data":[{"id":"deepseek-flash"},{"id":"deepseek-v4-pro"}]}"#,
        );
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.set_models(vec![json!({
            "id": "gpt-5.1-codex",
            "hidden": false,
            "supportedReasoningEfforts": [],
        })]);
        server.set_config_read(json!({
            "config": {},
            "layers": [{
                "name": { "type": "user" },
                "version": "v1",
                "config": {
                    "model": "gpt-5.1-codex",
                    "model_provider": "opencode-go",
                    "model_providers": {
                        "deepseek": {
                            "name": "DeepSeek",
                            "base_url": format!("{}/v1", probed.base_url()),
                            "experimental_bearer_token": "sentinel-wired-token"
                        },
                        "opencode-go": {
                            "name": "OpenCode Go",
                            "base_url": "http://127.0.0.1:1/v1",
                            "env_key": "OPENCODE_KEY"
                        }
                    },
                    "profiles": {
                        "fast": { "model": "deepseek-v4-pro", "model_provider": "deepseek" }
                    }
                }
            }]
        }));
        let client = connect(&url).await;

        let query = || ModelSurfaceQuery {
            session_id: Some("thread-1"),
            directory: Some("/work/cola"),
            current: Some(ModelSelection {
                label: "opencode-go/gpt-6-luna".into(),
                reference: "opencode-go/gpt-6-luna".into(),
            }),
            ..Default::default()
        };
        let surface = Backend::model_surface(&client, query()).await;

        assert_eq!(server.config_read_calls(), 1);
        let params = server.config_read_params();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0]["includeLayers"], true, "layers are required");
        assert_eq!(params[0]["cwd"], "/work/cola", "the project scope is resolved");
        assert_eq!(probed.request_count(), 1);
        assert_eq!(surface.scopes.len(), 2);
        let model = &surface.scopes[0];
        assert_eq!(
            model
                .options
                .iter()
                .filter(|option| option.group.as_deref() == Some(crate::codex::model_surface::CATALOG_GROUP))
                .count(),
            1,
            "the catalog keeps its own group"
        );
        assert!(
            model
                .options
                .iter()
                .any(|option| option.callback_value() == "codex/deepseek-flash"),
            "a probed model is a model-only pick"
        );
        assert!(
            model
                .options
                .iter()
                .any(|option| option.callback_value() == "codex/deepseek-v4-pro"
                    && option.group.as_deref() == Some(crate::codex::model_surface::PROFILE_GROUP)),
            "the profile preset is offered with its model"
        );
        assert!(
            model.note.as_deref().unwrap_or_default().contains("env_key"),
            "the env_key provider's reason is on the card"
        );
        let provider_scope = &surface.scopes[1];
        assert_eq!(provider_scope.changeable, ChangeScope::NewThread);
        assert!(
            provider_scope
                .options
                .iter()
                .any(|option| option.id == "opencode-go" && option.display_label().contains("当前"))
        );

        // A second open within the cache TTL: another `config/read` and
        // catalog read, but no second `/models` request.
        let again = Backend::model_surface(&client, query()).await;
        assert_eq!(again.scopes.len(), 2);
        assert_eq!(probed.request_count(), 1, "the probe answer was cached");
    }

    /// The context-free read (`/think`'s variant lookup: no session, no
    /// directory) is answered from the catalog alone, so a pick callback never
    /// waits on config or a provider.
    #[tokio::test]
    async fn model_surface_variant_lookup_skips_config_and_probes() {
        let probed = crate::test_http::TestHttpServer::start().await;
        probed.route("GET", "/v1/models", 200, r#"{"data":[{"id":"one"}]}"#);
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.set_models(vec![
            json!({ "id": "gpt-5.1-codex", "supportedReasoningEfforts": [] }),
        ]);
        server.set_config_read(json!({
            "layers": [{
                "name": { "type": "user" },
                "version": "v1",
                "config": {
                    "model_providers": {
                        "deepseek": {
                            "name": "DeepSeek",
                            "base_url": format!("{}/v1", probed.base_url()),
                            "experimental_bearer_token": "sentinel-fast-token"
                        }
                    }
                }
            }]
        }));
        let client = connect(&url).await;

        let surface = Backend::model_surface(
            &client,
            crate::backend::types::ModelSurfaceQuery {
                scope: Some("provider"),
                selected: Some("codex"),
                catalog_only: true,
                ..Default::default()
            },
        )
        .await;

        assert!(
            surface.scopes[0]
                .options
                .iter()
                .any(|option| option.id == "gpt-5.1-codex")
        );
        assert_eq!(server.config_read_calls(), 0, "the cheap read never asks config");
        assert_eq!(probed.request_count(), 0, "and never probes a provider");
    }

    /// A Pending Session's drilled provider read has a directory but no session
    /// id. Only `/think`'s explicit `catalog_only` read skips config — this page
    /// is a card page, so the provider options survive the click. It is a card
    /// callback, so a cold cache must not start a probe: the page is bounded to
    /// config + catalog, and the provider list still renders.
    #[tokio::test]
    async fn a_pending_sessions_drilled_provider_read_keeps_its_options() {
        let probed = crate::test_http::TestHttpServer::start().await;
        probed.route("GET", "/v1/models", 200, r#"{"data":[{"id":"one"}]}"#);
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.set_models(vec![
            json!({ "id": "gpt-5.1-codex", "supportedReasoningEfforts": [] }),
        ]);
        server.set_config_read(json!({
            "config": {},
            "layers": [{
                "name": { "type": "user" },
                "version": "v1",
                "config": {
                    "model_providers": {
                        "deepseek": {
                            "name": "DeepSeek",
                            "base_url": format!("{}/v1", probed.base_url()),
                            "experimental_bearer_token": "sentinel-drilled-token"
                        },
                        "opencode-go": { "name": "OpenCode Go" }
                    }
                }
            }]
        }));
        let client = connect(&url).await;

        let surface = Backend::model_surface(
            &client,
            crate::backend::types::ModelSurfaceQuery {
                scope: Some("provider"),
                selected: Some("deepseek"),
                directory: Some("/work/cola"),
                // A provider click is a card callback; 2 s mirrors the
                // bridge's ack budget without naming the Feishu layer here.
                ack_budget: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await;

        assert_eq!(
            server.config_read_calls(),
            1,
            "a card page reads config even without a session id"
        );
        assert_eq!(
            probed.request_count(),
            0,
            "a drilled callback never starts a probe on a cold cache"
        );
        let providers: Vec<&str> = surface.scopes[1]
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect();
        assert_eq!(providers, ["deepseek", "opencode-go"], "the options survive");
        assert!(
            !surface.scopes[0]
                .options
                .iter()
                .any(|option| option.callback_value() == "codex/one"),
            "a cold drilled read omits the probed models rather than probing"
        );
        let params = server.config_read_params();
        assert_eq!(params[0]["includeLayers"], true);
        assert_eq!(params[0]["cwd"], "/work/cola");
    }

    /// A drilled page is a card callback: a stalled config read is abandoned at
    /// the drilled slice of `CARD_ACK_BUDGET` (600 ms), not at the root page's
    /// 2 s, and probes are not started. The provider scope then says config was
    /// unreadable instead of hanging the ack.
    #[tokio::test]
    async fn a_drilled_config_read_is_bounded_by_the_ack_budget() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.set_config_read_delay(Duration::from_secs(5));
        let client = connect(&url).await;

        let started = Instant::now();
        let surface = Backend::model_surface(
            &client,
            crate::backend::types::ModelSurfaceQuery {
                scope: Some("provider"),
                selected: Some("deepseek"),
                directory: Some("/work/cola"),
                // A provider click is a card callback; 2 s mirrors the
                // bridge's ack budget without naming the Feishu layer here.
                ack_budget: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(server.config_read_calls(), 1, "the read was sent");
        assert!(
            elapsed < Duration::from_millis(600) + Duration::from_secs(3),
            "a callback read took {elapsed:?}; the 600ms slice plus test-runner \
             scheduling exceeded it"
        );
        assert!(
            elapsed >= Duration::from_millis(600),
            "the callback read should wait for its 600ms config slice, took {elapsed:?}"
        );
        let note = surface.scopes[1].note.as_deref().unwrap_or_default();
        assert!(note.contains("读不到 config"), "note: {note}");
    }

    /// A stalled `model/list` is bounded by the same callback slice, and the
    /// page still renders: provider options, the degraded model note and the
    /// hand-write hint, never an empty card or a call past the ack budget.
    #[tokio::test]
    async fn a_stalled_model_list_is_bounded_by_the_callback_budget() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.silence_next_model_list();
        server.set_config_read(json!({
            "config": {},
            "layers": [{
                "name": { "type": "user" },
                "version": "v1",
                "config": {
                    "model_providers": { "deepseek": { "name": "DeepSeek" } }
                }
            }]
        }));
        let client = connect(&url).await;

        let started = Instant::now();
        let surface = Backend::model_surface(
            &client,
            crate::backend::types::ModelSurfaceQuery {
                scope: Some("provider"),
                selected: Some("deepseek"),
                directory: Some("/work/cola"),
                ack_budget: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            server.model_list_calls(),
            1,
            "the stalled page was attempted once, then the deadline stopped the loop"
        );
        assert!(
            elapsed < Duration::from_millis(600) + Duration::from_secs(3),
            "a stalled catalog took {elapsed:?}; the bounded read plus test-runner \
             scheduling exceeded it"
        );
        assert!(
            elapsed >= Duration::from_millis(150),
            "the stalled page should wait for the request bound, took {elapsed:?}"
        );
        assert!(
            surface.scopes[1]
                .options
                .iter()
                .any(|option| option.id == "deepseek"),
            "the provider options still render"
        );
        assert!(
            surface.scopes[0].note.is_some(),
            "the degraded model scope carries a note"
        );
        assert!(
            surface.scopes[0].free_text.is_some(),
            "the hand-write hint survives"
        );
    }

    /// A paginated-but-alive catalog is capped by the deadline as well: the
    /// page loop keeps what it read and does not spend 64 pages of the budget.
    #[tokio::test]
    async fn a_slow_paginated_catalog_is_capped_by_the_callback_budget() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.set_models(
            (0..100)
                .map(|index| json!({ "id": format!("model-{index}"), "supportedReasoningEfforts": [] }))
                .collect(),
        );
        server.set_model_page_size(1);
        server.set_model_list_delay(Duration::from_millis(150));
        let client = connect(&url).await;

        let started = Instant::now();
        let surface = Backend::model_surface(
            &client,
            crate::backend::types::ModelSurfaceQuery {
                scope: Some("provider"),
                selected: Some("codex"),
                directory: Some("/work/cola"),
                ack_budget: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await;
        let elapsed = started.elapsed();

        let calls = server.model_list_calls();
        assert!(calls >= 1, "the first page was read");
        assert!(
            calls < 100,
            "the deadline stopped the 100-page catalog after {calls} pages"
        );
        assert!(
            elapsed < Duration::from_millis(600) + Duration::from_secs(3),
            "a paginated catalog took {elapsed:?}; the 600ms slice plus \
             test-runner scheduling exceeded it"
        );
        assert!(
            surface
                .scopes
                .iter()
                .flat_map(|scope| scope.options.iter())
                .any(|option| option.id.starts_with("model-")),
            "the partial catalog is shown, not discarded"
        );
        assert!(
            surface.scopes[0].free_text.is_some(),
            "the page still offers the hand-write hint"
        );
    }

    /// A model on a later catalog page still resolves its variants when the
    /// earlier pages are slow — the validation read has no ack slice, so it is
    /// never truncated before the target's page.
    #[tokio::test]
    async fn a_later_catalog_page_resolves_variants_without_an_ack_budget() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let models: Vec<Value> = (0..6)
            .map(|index| {
                let efforts = if index == 4 {
                    json!([{ "reasoningEffort": "high" }])
                } else {
                    json!([])
                };
                json!({ "id": format!("model-{index}"), "supportedReasoningEfforts": efforts })
            })
            .collect();
        server.set_models(models);
        server.set_model_page_size(1);
        // Slow enough to span many requests, fast enough that test-runner
        // scheduling cannot trip the per-request bound.
        server.set_model_list_delay(Duration::from_millis(50));
        let client = connect(&url).await;

        let surface = Backend::model_surface(
            &client,
            crate::backend::types::ModelSurfaceQuery {
                scope: Some("provider"),
                selected: Some("codex"),
                catalog_only: true,
                // The validation read: no budget, so every page is read.
                ..Default::default()
            },
        )
        .await;

        let model = surface.scopes[0]
            .options
            .iter()
            .find(|option| option.id == "model-4")
            .expect("the later page's model is in the catalog");
        assert_eq!(model.variants, ["high"]);
    }

    /// A validation read has no ack slice, but it is still finite: a silent
    /// `model/list` stops at the request bound and the catalog is unknown
    /// (empty plus the normal note), not a hung pick.
    #[tokio::test]
    async fn a_silent_catalog_validation_read_is_bounded() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.silence_next_model_list();
        let client = connect(&url).await;

        let started = Instant::now();
        let surface = Backend::model_surface(
            &client,
            crate::backend::types::ModelSurfaceQuery {
                scope: Some("provider"),
                selected: Some("codex"),
                catalog_only: true,
                // The validation read: no ack budget, still finite.
                ..Default::default()
            },
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(server.model_list_calls(), 1);
        assert!(
            elapsed < Duration::from_millis(200) + Duration::from_secs(3),
            "a silent catalog held the validation read for {elapsed:?}"
        );
        assert!(surface.scopes[0].options.is_empty());
        assert!(
            surface.scopes[0].note.is_some(),
            "the degraded catalog still renders its note"
        );
    }

    /// The `/think` variant lookup's `catalog_only` shape is bounded too: it
    /// sends a callback budget, so a stalled catalog cannot hold the pick.
    #[tokio::test]
    async fn a_stalled_catalog_only_variant_lookup_is_bounded() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.silence_next_model_list();
        let client = connect(&url).await;

        let started = Instant::now();
        let surface = Backend::model_surface(
            &client,
            crate::backend::types::ModelSurfaceQuery {
                scope: Some("provider"),
                selected: Some("codex"),
                catalog_only: true,
                ack_budget: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(server.model_list_calls(), 1);
        assert!(
            elapsed < Duration::from_millis(600) + Duration::from_secs(3),
            "the variant lookup took {elapsed:?}; the bounded read plus \
             test-runner scheduling exceeded it"
        );
        assert!(
            elapsed >= Duration::from_millis(150),
            "the lookup should wait for the request bound, took {elapsed:?}"
        );
        // Degrades to the empty-scope note + hint, never a failed read.
        assert!(surface.scopes[0].options.is_empty());
        assert!(surface.scopes[0].note.is_some());
        assert!(surface.scopes[0].free_text.is_some());
    }

    /// A profile preset's callback value reaches the next `turn/start` as its
    /// bare model id — the profile's provider is a declaration, not a
    /// per-turn protocol field.
    #[tokio::test]
    async fn a_profile_preset_pick_rides_the_next_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        // `codex/deepseek-v4-pro` is the value a profile preset's button sends;
        // the bridge parses it into this ModelInfo.
        let model = ModelInfo {
            id: "deepseek-v4-pro".into(),
            provider_id: "codex".into(),
            variant: None,
        };
        client
            .prompt("thread-1", "run it", &[], &[], Some(&model), None, None, None)
            .await
            .expect("the prompt");
        let params = server.turn_params("thread-1");
        assert_eq!(params[0]["model"], "deepseek-v4-pro");
        assert_eq!(
            params[0].get("modelProvider"),
            None,
            "turn/start has no provider field; the thread's provider stays put"
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
    /// two directories is ambiguous until the session's own directory answers.
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

        server.set_skills(vec![json!({
            "name": "caveman",
            "description": "Brief mode",
            "path": "/work/other/.codex/skills/caveman/SKILL.md",
            "scope": "repo",
            "enabled": true,
            "pluginId": null,
        })]);
        assert_eq!(client.list_skills(Some("/work/other")).await.len(), 1);
        assert_eq!(
            client.skills.path_of("caveman", Some("/work/cola")).as_deref(),
            Some("/work/cola/.codex/skills/caveman/SKILL.md"),
            "the session's own directory answers first"
        );
        assert_eq!(
            client.skills.path_of("caveman", Some("/work/other")).as_deref(),
            Some("/work/other/.codex/skills/caveman/SKILL.md")
        );
        assert!(
            client.skills.path_of("caveman", None).is_none(),
            "without a directory no skill resolves, however many scopes agree"
        );

        // The session's own skill vanishes: the other project's skill is NOT a
        // substitute, even as the only remaining scope.
        server.set_skills(Vec::new());
        assert!(
            client.list_skills(Some("/work/cola")).await.is_empty(),
            "the vanished scope is replaced"
        );
        assert!(
            client.skills.path_of("caveman", Some("/work/cola")).is_none(),
            "another directory's skill is never substituted"
        );
        assert_eq!(
            client.skills.path_of("caveman", Some("/work/other")).as_deref(),
            Some("/work/other/.codex/skills/caveman/SKILL.md"),
            "the other directory still resolves its own"
        );
        assert!(client.list_skills(Some("/work/other")).await.is_empty());
        assert!(
            client.skills.path_of("caveman", Some("/work/cola")).is_none(),
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

    /// Finding 5: the prompt resolves a skill in the session's own directory,
    /// so a same-named skill advertised for another project is never sent.
    #[tokio::test]
    async fn a_skill_resolves_in_the_sessions_own_directory() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/other",
            "a real thread",
        ));
        let skill = |path: &str| {
            json!({
                "name": "caveman",
                "description": "Brief mode",
                "path": path,
                "scope": "repo",
                "enabled": true,
                "pluginId": null,
            })
        };
        let client = connect(&url).await;
        server.set_skills(vec![skill("/work/cola/.codex/skills/caveman/SKILL.md")]);
        client.list_skills(Some("/work/cola")).await;
        server.set_skills(vec![skill("/work/other/.codex/skills/caveman/SKILL.md")]);
        client.list_skills(Some("/work/other")).await;
        // Adopting the thread teaches the adapter its cwd.
        client.transcript("thread-1").await.expect("subscribe");

        client
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
            .expect("the prompt submits");
        let params = server.turn_params("thread-1");
        assert_eq!(
            params[0]["input"][1]["path"], "/work/other/.codex/skills/caveman/SKILL.md",
            "the session's own project wins over the other read"
        );
    }

    /// Finding 3 (suppression): a chosen id whose request never reached the
    /// server must stop vouching for the thread-wide fallback after its window,
    /// so a later foreign live turn with no client id is reported external.
    #[tokio::test]
    async fn a_never_delivered_marker_does_not_vouch_for_a_later_tui_turn() {
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
        client
            .authored
            .begin("thread-1", Some("msg_cola_never"))
            .expect("the marker is recorded");
        tokio::time::sleep(Duration::from_millis(
            (crate::codex::authorship::PENDING_FALLBACK_TTL_MS as u64) + 30,
        ))
        .await;
        assert!(!client.authored.is_pending("thread-1"), "the window expired");

        // A live turn the user's own client starts, with no client id.
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
            "an unresolved, never-delivered submission must not suppress a foreign turn"
        );
    }

    /// Finding 1 (adapter): a marker that cannot be persisted refuses the
    /// prompt before anything is sent, and leaves the existing record trusted.
    #[tokio::test]
    async fn a_prompt_is_refused_when_its_marker_cannot_be_persisted() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("authored_turns.json");
        let record = crate::codex::Authorship::open(path.clone(), "codex").expect("the record opens");
        // A directory at the writer's temp path makes the atomic replace fail.
        let blocked = path.with_extension("tmp");
        std::fs::create_dir(&blocked).expect("block the temp path");
        let client = connect_with(&url, record.clone()).await;

        let error = client
            .prompt(
                "thread-1",
                "cannot be recorded",
                &[],
                &[],
                None,
                None,
                None,
                Some("msg_cola_blocked"),
            )
            .await
            .expect_err("the marker cannot be persisted");
        assert!(error.to_string().contains("could not be persisted"), "{error}");
        assert_eq!(
            server.turn_starts(),
            0,
            "nothing may be sent without a durable provenance record"
        );
        assert!(record.is_trusted(), "the valid record is still trusted");
        assert!(!record.is_pending("thread-1"));

        // Once the record can be written again, the next prompt goes out.
        std::fs::remove_dir(&blocked).expect("unblock");
        client
            .prompt(
                "thread-1",
                "recordable now",
                &[],
                &[],
                None,
                None,
                None,
                Some("msg_cola_ok"),
            )
            .await
            .expect("the retry submits");
        assert_eq!(server.turn_starts(), 1);
    }

    /// Finding 2 (adapter): an unresolved marker is never evicted to make room;
    /// the prompt that would exceed the cap is refused and not sent.
    #[tokio::test]
    async fn a_full_authorship_record_refuses_the_next_prompt() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        for index in 0..32 {
            client
                .authored
                .begin("thread-1", Some(&format!("msg_cola_{index}")))
                .expect("below the cap");
        }
        let error = client
            .prompt(
                "thread-1",
                "over capacity",
                &[],
                &[],
                None,
                None,
                None,
                Some("msg_cola_over"),
            )
            .await
            .expect_err("at capacity");
        assert!(error.to_string().contains("unresolved submissions"), "{error}");
        assert_eq!(server.turn_starts(), 0, "nothing was sent");
        assert!(
            client.authored.is_pending_submission("thread-1", "msg_cola_0"),
            "the oldest unresolved marker is still owed"
        );
    }

    /// Finding 4: a stalled `thread/read` cannot hold `/stop`.
    #[tokio::test]
    async fn a_stalled_stop_read_is_bounded() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        server.silence_next_thread_read();
        tokio::time::timeout(Duration::from_secs(1), client.interrupt("thread-1"))
            .await
            .expect("a stalled read must not hold the stop")
            .expect("the stop completes");
        assert!(
            server.interrupt_params().is_empty(),
            "the idle thread had nothing to interrupt"
        );
    }

    /// A steer into a turn the user's TUI started claims only the item the
    /// supplement created: the turn and its other (id-less) items stay external.
    #[tokio::test]
    async fn a_steer_into_a_tui_turn_marks_only_its_own_item() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        let tui = connect(&url).await;
        tui.transcript("thread-1").await.expect("the TUI's subscription");
        tui.prompt("thread-1", "from the TUI", &[], &[], None, None, None, None)
            .await
            .expect("the TUI's prompt");
        let live = server.live_turn("thread-1").expect("the turn is running");

        let cola = connect(&url).await;
        cola.transcript("thread-1").await.expect("cola's subscription");
        wait_for_live_turn(&cola, "thread-1").await;

        cola.steer(
            "thread-1",
            "feishu supplement",
            &[],
            &[],
            None,
            None,
            None,
            Some("msg_cola_supp"),
        )
        .await
        .expect("the supplement steers");

        let steers = server.steer_params();
        assert_eq!(steers.len(), 1);
        assert_eq!(steers[0]["expectedTurnId"].as_str(), Some(live.as_str()));
        let transcript = wait_for_text(&cola, "thread-1", "feishu supplement").await;
        let supplement = transcript
            .messages
            .iter()
            .find(|message| message.text() == "feishu supplement")
            .expect("the supplement's user item");
        let tui_message = transcript
            .messages
            .iter()
            .find(|message| message.text() == "from the TUI")
            .expect("the TUI's own message");
        assert!(
            cola.is_cola_authored(supplement.id.as_str()).await,
            "the supplement item is cola's, even though the turn is not"
        );
        assert!(
            !cola.is_cola_authored(tui_message.id.as_str()).await,
            "the TUI's id-less item in the same turn stays external"
        );
        assert!(
            !cola.authored.is_authored("thread-1", Some(live.as_str())),
            "a steer never claims the turn it joined"
        );
        assert!(
            transcript.submissions.contains_key("msg_cola_supp"),
            "the anchor map carries the steer id: {:?}",
            transcript.submissions
        );
    }

    /// The steer id is durable: a rebuilt client still classifies the item.
    #[tokio::test]
    async fn a_steer_id_survives_a_restart() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        let tui = connect(&url).await;
        tui.transcript("thread-1").await.expect("the TUI's subscription");
        tui.prompt("thread-1", "from the TUI", &[], &[], None, None, None, None)
            .await
            .expect("the TUI's prompt");
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("authored_turns.json");
        let first = connect_with(
            &url,
            crate::codex::Authorship::open(path.clone(), "codex").expect("the record opens"),
        )
        .await;
        first.transcript("thread-1").await.expect("subscribe");
        wait_for_live_turn(&first, "thread-1").await;
        first
            .steer(
                "thread-1",
                "feishu supplement",
                &[],
                &[],
                None,
                None,
                None,
                Some("msg_cola_supp"),
            )
            .await
            .expect("the supplement steers");
        let transcript = wait_for_text(&first, "thread-1", "feishu supplement").await;
        let item_id = transcript
            .messages
            .iter()
            .find(|message| message.text() == "feishu supplement")
            .map(|message| message.id.as_str().to_string())
            .expect("the supplement item");
        drop(first);

        let second = connect_with(
            &url,
            crate::codex::Authorship::open(path, "codex").expect("the record reloads"),
        )
        .await;
        let rebuilt = second.transcript("thread-1").await.expect("the rebuilt read");
        assert!(
            second.is_cola_authored(&item_id).await,
            "the durable steer id still classifies its item after a restart"
        );
        let tui_message = rebuilt
            .messages
            .iter()
            .find(|message| message.text() == "from the TUI")
            .expect("the TUI's message");
        assert!(!second.is_cola_authored(tui_message.id.as_str()).await);
    }

    /// An aged, never-delivered steer marker neither vouches for its id nor
    /// drives the thread-wide fallback.
    #[tokio::test]
    async fn an_aged_steer_marker_vouches_for_nothing() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("authored_turns.json");
        let aged = chrono::Utc::now().timestamp_millis() - crate::codex::authorship::PENDING_ID_TTL_MS - 1;
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"turns":{{}},"pending":{{"codex":{{"thread-1":[{{"id":"msg_cola_steer","at_ms":{aged},"kind":"steer"}}]}}}}}}"#
            ),
        )
        .expect("write an aged steer record");
        let record = crate::codex::Authorship::open(path.clone(), "codex").expect("the record opens");
        let cola = connect_with(&url, record.clone()).await;
        cola.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;

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
        wait_for_text(&cola, "thread-1", "from the TUI").await;
        assert!(
            !cola.is_cola_authored("item-tui").await,
            "an aged steer marker must not vouch for an id-less TUI item"
        );
        assert!(
            !cola.authored.is_pending_submission("thread-1", "msg_cola_steer"),
            "the aged steer entry is out of its recorded life"
        );
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
        assert!(
            file["pending"].get("codex").is_none(),
            "the read pruned the aged steer entry: {file}"
        );
    }

    /// Finding 1 (aged id): the precise match is bounded by the recorded life,
    /// and the read prunes before it binds. After the thread sat unread past the
    /// safety valve, its very first history read sees a foreign item reusing the
    /// aged id — and must not bind it. The foreign item is in the persisted
    /// history, so this is the first-read case the finding names.
    #[tokio::test]
    async fn an_aged_recorded_id_cannot_bind_a_foreign_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        server.echo_prompts("thread-1");
        // A peer starts a turn whose chosen id reuses an aged recorded one.
        let tui = connect(&url).await;
        tui.prompt(
            "thread-1",
            "reusing an aged id",
            &[],
            &[],
            None,
            None,
            None,
            Some("msg_cola_aged"),
        )
        .await
        .expect("the peer's prompt");

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("authored_turns.json");
        let aged = chrono::Utc::now().timestamp_millis() - crate::codex::authorship::PENDING_ID_TTL_MS - 1;
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"turns":{{}},"pending":{{"codex":{{"thread-1":[{{"id":"msg_cola_aged","at_ms":{aged}}}]}}}}}}"#
            ),
        )
        .expect("write an aged record");
        let record = crate::codex::Authorship::open(path.clone(), "codex").expect("the record opens");
        let cola = connect_with(&url, record.clone()).await;

        // cola's FIRST read of the thread pages the peer's persisted turn.
        let transcript = cola.transcript("thread-1").await.expect("the first read");
        let mut transcript = transcript;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !transcript
            .messages
            .iter()
            .any(|message| message.text() == "reusing an aged id")
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the peer's persisted prompt never reached the read"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            transcript = cola.transcript("thread-1").await.expect("the retried read");
        }
        let item = transcript
            .messages
            .iter()
            .find(|message| message.text() == "reusing an aged id")
            .expect("the peer's user message");
        assert!(
            !cola.is_cola_authored(item.id.as_str()).await,
            "an aged recorded id must not vouch for a foreign item that reuses it"
        );
        assert!(
            !cola.authored.is_authored("thread-1", Some("turn-1")),
            "the read must not have bound the foreign turn"
        );
        assert!(
            !record.is_pending_submission("thread-1", "msg_cola_aged"),
            "the aged entry is out of its recorded life"
        );
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
        assert!(
            file["pending"].get("codex").is_none(),
            "the read pruned the aged entry before reconciling: {file}"
        );
    }

    /// Finding 2: a stalled metadata read on the skill path fails the prompt by
    /// name instead of hanging before submission.
    #[tokio::test]
    async fn a_stalled_cwd_read_fails_the_skill_prompt_by_name() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        // No adoption: the client has not learned the thread's cwd.
        let client = connect(&url).await;
        server.silence_next_thread_read();
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            client.prompt(
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
                Some("msg_cola_skill"),
            ),
        )
        .await
        .expect("a stalled metadata read must not hang the prompt")
        .expect_err("the read timed out");
        assert!(error.to_string().contains("did not answer within"), "{error}");
        assert_eq!(server.turn_starts(), 0, "nothing was sent");
        assert!(!client.authored.is_pending("thread-1"));
    }

    /// Finding 1: a peer chooses its own `clientUserMessageId`, so the id's
    /// spelling proves nothing. Only an id this record saw may vouch — an
    /// unfamiliar id claiming cola's prefix is somebody else's turn.
    #[tokio::test]
    async fn a_foreign_client_id_claiming_colas_prefix_is_external() {
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

        for (method, params) in [
            (
                "turn/started",
                json!({ "threadId": "thread-1", "turn": { "id": "turn-forged", "status": "inProgress", "startedAt": 5 } }),
            ),
            (
                "item/started",
                json!({
                    "threadId": "thread-1",
                    "turnId": "turn-forged",
                    "startedAtMs": 5_000_i64,
                    "item": {
                        "type": "userMessage",
                        "id": "item-forged",
                        "clientId": "msg_cola_forged",
                        "content": [{ "type": "text", "text": "a peer claiming cola's id", "textElements": [] }],
                    },
                }),
            ),
            (
                "turn/completed",
                json!({ "threadId": "thread-1", "turn": { "id": "turn-forged", "status": "completed", "startedAt": 5, "completedAt": 6 } }),
            ),
        ] {
            server.push_to_thread("thread-1", method, params);
        }
        let transcript = wait_for_text(&client, "thread-1", "a peer claiming cola's id").await;
        assert!(
            transcript
                .messages
                .iter()
                .any(|message| message.id.as_str() == "item-forged"),
            "the read sees the peer's turn"
        );
        assert!(
            !client.is_cola_authored("item-forged").await,
            "an id the record never saw is never inferred from its spelling"
        );
        assert!(
            !client.authored.is_authored("thread-1", Some("turn-forged")),
            "the read must not bind a foreign turn as cola's"
        );
    }

    /// Finding 3: a skill that exists only in another project is refused, not
    /// silently taken from that directory.
    #[tokio::test]
    async fn a_skill_only_in_another_directory_is_refused() {
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
            "path": "/work/other/.codex/skills/caveman/SKILL.md",
            "scope": "repo",
            "enabled": true,
            "pluginId": null,
        })]);
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        // Only the other project's directory was read.
        client.list_skills(Some("/work/other")).await;

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
            .expect_err("another project's skill is not substituted");
        assert!(error.to_string().contains("caveman"), "{error}");
        assert_eq!(
            server.turn_starts(),
            0,
            "the refused skill never reached the server"
        );
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

    /// The pick changes what the thread runs from then on ("for this turn and
    /// subsequent turns"), so the footer must name the PICKED model after the
    /// prompt, not the one the thread was adopted with (issue #904).
    #[tokio::test]
    async fn a_pick_moves_the_transcript_model_with_it() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.model = Some("gpt-6-luna".into());
        thread.model_provider = Some("opencode-go".into());
        server.seed_thread(thread);
        let client = connect(&url).await;

        // The first read is the adoption, so the item pushed afterwards is
        // delivered to a subscriber.
        client.transcript("thread-1").await.expect("adopt");
        server.wait_for_subscribers("thread-1", 1).await;
        server.push_item("thread-1", "item-1", "one");
        let transcript = wait_for_text(&client, "thread-1", "one").await;
        let answering = transcript
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("the assistant item");
        assert_eq!(
            answering.model.as_ref().map(|model| model.model_id.as_str()),
            Some("gpt-6-luna"),
            "adoption names the model the thread already runs"
        );

        let pick = ModelInfo {
            id: "gpt-5.5".into(),
            provider_id: "opencode-go".into(),
            variant: None,
        };
        client
            .prompt("thread-1", "run it", &[], &[], Some(&pick), None, None, None)
            .await
            .expect("the prompt");

        let transcript = client.transcript("thread-1").await.expect("the read");
        let answering = transcript
            .messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::Assistant)
            .expect("the assistant item");
        let model = answering.model.as_ref().expect("the picked model");
        assert_eq!(model.model_id, "gpt-5.5");
        assert_eq!(model.provider_id, "opencode-go");
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
        client
            .authored
            .begin("thread-1", None)
            .expect("the marker is recorded");
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

        client
            .authored
            .begin("thread-1", Some("msg_cola_early"))
            .expect("the marker is recorded");
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

    /// A release frees this connection's subscription: `thread/unsubscribe`
    /// goes out once, the released thread's items stop reaching its projection,
    /// and another connection's subscription to the SAME thread keeps
    /// streaming (the server must not unload a thread another subscriber
    /// holds).
    #[tokio::test]
    async fn releasing_a_thread_unsubscribes_this_connection_only() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let first = connect(&url).await;
        let second = connect(&url).await;
        first.transcript("thread-1").await.expect("adopt on the first");
        second.transcript("thread-1").await.expect("adopt on the second");
        server.wait_for_subscribers("thread-1", 2).await;

        first.release_session("thread-1").await.expect("release");
        assert_eq!(server.unsubscribe_calls(), 1, "exactly one unsubscribe");
        server.wait_for_subscribers("thread-1", 1).await;

        // The peer's stream survives, so the server had another subscriber.
        assert_eq!(
            server.push_item("thread-1", "item-1", "after the release"),
            1,
            "only the peer is still subscribed"
        );
        wait_for_text(&second, "thread-1", "after the release").await;
        assert!(
            first
                .projections
                .transcript("thread-1")
                .messages
                .iter()
                .all(|message| !message.text().contains("after the release")),
            "a released thread folds no further items"
        );
    }

    /// Releasing a session no read ever adopted is a `Ok(())` no-op: no
    /// `thread/unsubscribe` for a subscription this connection never made.
    #[tokio::test]
    async fn releasing_a_session_that_was_never_adopted_is_a_no_op() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;

        client
            .release_session("thread-1")
            .await
            .expect("an unadopted release is a no-op");
        assert_eq!(server.unsubscribe_calls(), 0);
        assert_eq!(server.resume_calls(), 0, "a release never adopts");
    }

    /// A released thread is adoptable again: the next read sends a fresh
    /// `thread/resume` and its item stream works.
    #[tokio::test]
    async fn re_adopting_a_released_thread_resumes_again() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("adopt");
        server.wait_for_subscribers("thread-1", 1).await;

        client.release_session("thread-1").await.expect("release");
        server.wait_for_subscribers("thread-1", 0).await;

        client.transcript("thread-1").await.expect("re-adopt");
        assert_eq!(server.resume_calls(), 2, "the re-adoption resumes again");
        server.wait_for_subscribers("thread-1", 1).await;
        assert_eq!(server.push_item("thread-1", "item-2", "after re-adoption"), 1);
        wait_for_text(&client, "thread-1", "after re-adoption").await;
    }

    /// A server that refuses `thread/unsubscribe` reports the failure to the
    /// caller, but the local teardown already happened: the next read adopts
    /// again rather than sitting on a phantom subscription.
    #[tokio::test]
    async fn a_refused_unsubscribe_is_reported_and_the_thread_re_adopts() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("adopt");
        server.wait_for_subscribers("thread-1", 1).await;
        server.reject_unsubscribe(1);

        let error = client
            .release_session("thread-1")
            .await
            .expect_err("the refusal is reported");
        assert!(error.to_string().contains("thread unsubscribe failed"), "{error}");

        client.transcript("thread-1").await.expect("re-adopt");
        assert_eq!(server.resume_calls(), 2);
        server.wait_for_subscribers("thread-1", 1).await;
    }
}
