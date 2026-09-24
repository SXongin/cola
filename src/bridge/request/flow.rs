//! The request-flow engine (spec #298, ticket C): the poll sweep that lists
//! pending requests and surfaces them as cards (poll → prepare → render →
//! resolve), the per-kind in-flight state, and the double-click and claim
//! guards. Everything kind-specific is delegated through the
//! [`super::kind::RequestKind`] seam; the card-delivery helpers live in
//! [`super::delivery`].

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::Instrument;

use crate::bridge::handler::CardActionResult;
use crate::bridge::handles::{CardsHandle, FlowHandles, RequestsHandle, SessionsHandle};
use crate::bridge::pollers::{CardTarget, inline_host_session, mark_stale_cards, resolve_card_target};
use crate::bridge::question::{QuestionState, stale_question_card};
use crate::bridge::surfaces::{StandaloneSurface, Surfaces};
use crate::bridge::turn::Turn;
use crate::opencode;

use super::delivery::{
    Origin, Residue, already_handled_result, denied_receipt, handled_elsewhere_receipt, resolve_blocks,
};
use super::kind::{PendingRequest, RequestKind};

/// A request card cola sent: the live Message id, the summary shown when the
/// card is marked stale, and the owning directory. The directory lets the
/// sweep skip cleanups for a directory whose list call failed — "absent from
/// the pending list" only speaks for directories that listed successfully
/// (#130, #144).
#[derive(Clone)]
pub struct SentCard {
    pub message_id: String,
    pub summary: String,
    pub directory: String,
}

/// The fused permission/question flow. The poll sweep, the in-flight state and
/// the double-click/claim guards live here once; the kind supplies the deltas
/// and the delivery module settles their cards.
pub struct RequestFlow {
    kind: Box<dyn RequestKind>,
    /// How often (milliseconds) the poll loop lists pending requests. Defaults
    /// to today's 3 s cadence; tests store a small value so every branch is
    /// exercisable without sleeping real seconds.
    pub poll_interval_ms: std::sync::atomic::AtomicU64,
    /// How long (milliseconds) one pending-request list call may take before
    /// the poll loop gives up on it and retries next tick. Bounds a request
    /// stuck on a half-open connection (e.g. across a server restart) so one
    /// hung call cannot freeze the poller forever. Defaults to 30 s; tests
    /// store a small value so the timeout branch runs without sleeping.
    pub list_timeout_ms: std::sync::atomic::AtomicU64,
    /// request_id → the card cola sent (used to mark a card stale when the
    /// request is resolved by ANOTHER client).
    pub sent_cards: Arc<Mutex<HashMap<String, SentCard>>>,
    /// Directories whose list call has SUCCEEDED at least once in this process
    /// (#130). A question card only exists after its state was recorded, so
    /// for one of these directories a missing state entry proves the request
    /// left the pending list — the late-click classifier uses this instead of
    /// remembering every pruned request id.
    listed_dirs: Arc<Mutex<std::collections::HashSet<String>>>,
    /// request_id → the question request's whole in-flight state: the request,
    /// its owning directory, the finalized answers and the live multi-select
    /// toggles (question kind only; the permission flow's map stays empty). One
    /// entry per request, so no transition can update one field without the
    /// others.
    question_state: Arc<Mutex<HashMap<String, QuestionState>>>,
    /// request_id → the WINNING click's result. A losing click re-serves this
    /// instead of rendering its own decision — otherwise a rapid second click
    /// on a different button (允许一次 vs 总是允许) would flip the card to a
    /// decision the backend never received. In-memory like the claim set on
    /// `handles.requests.answered_requests`, and one card per answered request.
    answered_results: Arc<Mutex<HashMap<String, CardActionResult>>>,
    /// The persisted surface mirror (ADR-0038 restart re-adoption): every
    /// standalone card this flow sends is written through, and its persisted
    /// records seed [`Self::recovered`] at startup.
    pub(crate) surfaces: Arc<Surfaces>,
    /// request_id → owning directory of the surfaces a PREVIOUS process
    /// persisted (ADR-0038 restart re-adoption). The first sweep that lists
    /// such a request re-adopts its card instead of posting a second one; an
    /// entry whose request left the pending list is reconciled away by the
    /// stale/inline cleanups, like every other in-memory surface.
    recovered: Arc<Mutex<HashMap<String, String>>>,
}

impl RequestFlow {
    pub fn new(kind: Box<dyn RequestKind>, surfaces: Arc<Surfaces>) -> Self {
        // Hydrate this kind's half of the persisted record: the standalone
        // cards it sent and the requests whose surfaces the next sweep must
        // re-adopt (both an inline block and a standalone card count).
        let state = surfaces.snapshot();
        let claim_kind = kind.claim_kind();
        let sent_cards: HashMap<String, SentCard> = state
            .standalone
            .iter()
            .filter(|(_, s)| s.kind == claim_kind)
            .map(|(id, s)| {
                (
                    id.clone(),
                    SentCard {
                        message_id: s.message_id.clone(),
                        summary: s.summary.clone(),
                        directory: s.directory.clone(),
                    },
                )
            })
            .collect();
        let recovered: HashMap<String, String> = state
            .inline
            .iter()
            .filter(|(_, s)| s.kind == claim_kind)
            .map(|(id, s)| (id.clone(), s.directory.clone()))
            .chain(
                state
                    .standalone
                    .iter()
                    .filter(|(_, s)| s.kind == claim_kind)
                    .map(|(id, s)| (id.clone(), s.directory.clone())),
            )
            .collect();
        Self {
            kind,
            poll_interval_ms: std::sync::atomic::AtomicU64::new(3000),
            list_timeout_ms: std::sync::atomic::AtomicU64::new(30_000),
            sent_cards: Arc::new(Mutex::new(sent_cards)),
            listed_dirs: Arc::new(Mutex::new(std::collections::HashSet::new())),
            question_state: Arc::new(Mutex::new(HashMap::new())),
            answered_results: Arc::new(Mutex::new(HashMap::new())),
            surfaces,
            recovered: Arc::new(Mutex::new(recovered)),
        }
    }

    /// The double-click guard's read-only check: whether `req_id` was already
    /// answered. Used to re-serve a result to a late click; the atomic
    /// [`Self::try_mark_answered`] is what decides who may reply.
    pub(crate) async fn is_answered(&self, requests: &RequestsHandle, req_id: &str) -> bool {
        requests.answered_requests.lock().await.contains(req_id)
    }

    /// Atomically claim `req_id` for this click: `true` when it was not yet
    /// answered (this click may reply), `false` when another click already
    /// claimed it (re-serve the first result, never reply again). The
    /// check-and-insert happens under ONE lock, so two near-simultaneous clicks
    /// cannot both win the way the old `is_answered` + `mark_answered` pair
    /// could.
    pub(crate) async fn try_mark_answered(&self, requests: &RequestsHandle, req_id: &str) -> bool {
        requests.answered_requests.lock().await.insert(req_id.to_string())
    }

    /// Roll back a claim made by [`Self::try_mark_answered`] after a GENUINE
    /// reply failure, so the user can retry. A benign 404 ("already resolved")
    /// keeps the claim — the request really is gone.
    pub(crate) async fn unmark_answered(&self, requests: &RequestsHandle, req_id: &str) {
        requests.answered_requests.lock().await.remove(req_id);
    }

    /// Record the winning click's result so a click that loses the guard race
    /// re-serves the SAME result instead of rendering its own decision (which
    /// could disagree with what actually reached the backend).
    pub(crate) async fn remember_answered_result(&self, req_id: &str, result: &CardActionResult) {
        self.answered_results
            .lock()
            .await
            .insert(req_id.to_string(), result.clone());
    }

    /// The winning click's result, if it has settled. `None` while the winner
    /// is still in flight — the losing click then renders a generic replay.
    pub(crate) async fn answered_result(&self, req_id: &str) -> Option<CardActionResult> {
        self.answered_results.lock().await.get(req_id).cloned()
    }

    /// Remember a pending question request so its card/block buttons can
    /// resolve: the full request (card rebuilds) and its owning directory
    /// (routing a reply whose callback no longer carries one). Refreshing an
    /// already-remembered request keeps its recorded answers/toggles.
    pub(crate) async fn remember_question(&self, req: &opencode::types::QuestionRequest, dir: &str) {
        let mut states = self.question_state.lock().await;
        match states.entry(req.id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().refresh(req, dir);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(QuestionState::new(req.clone(), dir.to_string()));
            }
        }
    }

    /// Run `f` against one remembered question's state under the state lock.
    /// `None` when the request is no longer remembered (a sweep raced the
    /// click, #130) — the caller must classify that, never reply from an
    /// empty snapshot.
    pub(crate) async fn with_question_state<R>(
        &self,
        req_id: &str,
        f: impl FnOnce(&mut QuestionState) -> R,
    ) -> Option<R> {
        self.question_state.lock().await.get_mut(req_id).map(f)
    }

    /// Read the whole question-state map under ONE lock — the question kind's
    /// snapshot contribution needs a single consistent moment (ADR-0028), the
    /// way the old inline loop held the lock across every pending block.
    pub(crate) async fn with_question_states<R>(
        &self,
        f: impl FnOnce(&HashMap<String, QuestionState>) -> R,
    ) -> R {
        let states = self.question_state.lock().await;
        f(&states)
    }

    /// The remembered question request, cloned for a card rebuild.
    pub(crate) async fn question_request(&self, req_id: &str) -> Option<opencode::types::QuestionRequest> {
        self.question_state
            .lock()
            .await
            .get(req_id)
            .map(|s| s.request().clone())
    }

    /// Whether `req_id` is a remembered (pending) question request. Test-only
    /// today: the production poll loop tracks self-surfaced ids in its own
    /// `seen` set.
    #[cfg(test)]
    pub(crate) async fn has_question(&self, req_id: &str) -> bool {
        self.question_state.lock().await.contains_key(req_id)
    }

    /// The remembered owning directory of a question request, used as the
    /// fallback when a card callback carries none.
    pub(crate) async fn question_dir(&self, req_id: &str) -> Option<String> {
        self.question_state
            .lock()
            .await
            .get(req_id)
            .map(|s| s.dir().to_string())
    }

    /// Drop a question request's in-flight state once it is finalized: the
    /// remembered request, its owning directory, the final answers and any live
    /// multi-select toggles. `sent_cards` is cleared only where the request was
    /// actually delivered as its own card.
    pub(crate) async fn remove_question(&self, req_id: &str) {
        self.question_state.lock().await.remove(req_id);
    }

    /// The remembered questions and their current answers, captured for the
    /// reply payload and the completion card. The answers are CLONED, not
    /// consumed: a genuine reply failure rolls the guard claim back and the
    /// state must stay intact so the click can be retried. `None` when the
    /// state vanished (a sweep raced the click) — the caller must classify
    /// that (#130), never reply from an empty snapshot.
    pub(crate) async fn question_snapshot(
        &self,
        req_id: &str,
    ) -> Option<(Vec<opencode::types::QuestionInfo>, Vec<Vec<String>>)> {
        let states = self.question_state.lock().await;
        let state = states.get(req_id)?;
        let answers = state
            .answers()
            .iter()
            .map(|a| a.clone().unwrap_or_default())
            .collect();
        Some((state.request().questions.clone(), answers))
    }

    /// The live state the kind contributes to a Session Snapshot re-render
    /// after an interaction (ADR-0028) — the 已选/✅ markers the standalone
    /// question cards show. The contribution is the kind's; a kind with no
    /// snapshot state contributes nothing.
    pub(crate) async fn live_snapshot_state(
        &self,
        pending: &[PendingRequest],
    ) -> crate::feishu::snapshot_card::SnapshotQuestionState {
        self.kind.snapshot_state(self, pending).await
    }

    /// Let the kind remember a request surfaced without the poll loop (a
    /// Session Snapshot claim or a busy-follow host), so its block's buttons
    /// still resolve. A kind with no in-flight state has nothing to remember.
    pub(crate) async fn remember_surfaced(&self, req: &PendingRequest, dir: &str) {
        self.kind.remember_surfaced(self, req, dir).await;
    }

    /// Seed this kind's inline block on a new host card with its initial
    /// state — the busy-follow host path (`start_snapshot_follow`).
    pub(crate) async fn add_initial_inline(
        &self,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        self.kind.add_initial_inline(self, cards, host, req, dir).await
    }

    /// The result for a click whose question state is gone (#130). It never
    /// reaches the backend and never records anything: a directory this
    /// process has listed successfully means the request provably left the
    /// pending list → neutral "已处理"; otherwise cola cannot know (fresh
    /// process before the first sweep, failing lists, unknown directory) and
    /// must not claim the request was handled.
    pub(crate) async fn missing_question_result(
        &self,
        directory: Option<&str>,
        inline: bool,
    ) -> CardActionResult {
        let classified = match directory {
            Some(dir) => self.listed_dirs.lock().await.contains(dir),
            None => false,
        };
        if classified {
            already_handled_result(self.kind.label(), inline, "该问题已处理")
        } else {
            stale_question_card(inline)
        }
    }

    /// Whether `req_id` still has live question state.
    pub(crate) async fn question_state_live(&self, req_id: &str) -> bool {
        self.question_state.lock().await.contains_key(req_id)
    }

    /// The #130 gate in front of every question click: `Some(result)` when the
    /// state is gone — the click must not reply, so it gets the classified
    /// result; `None` when the state is live and the caller may proceed.
    pub(crate) async fn question_gate(
        &self,
        req_id: &str,
        directory: Option<&str>,
        inline: bool,
    ) -> Option<CardActionResult> {
        if self.question_state_live(req_id).await {
            return None;
        }
        Some(self.missing_question_result(directory, inline).await)
    }

    /// The classified result for a click whose state vanished AFTER it claimed
    /// the request: roll the claim back so the reply never happens and the
    /// fresh card can still answer.
    pub(crate) async fn missing_after_claim(
        &self,
        requests: &RequestsHandle,
        req_id: &str,
        directory: Option<&str>,
        inline: bool,
    ) -> CardActionResult {
        self.unmark_answered(requests, req_id).await;
        self.missing_question_result(directory, inline).await
    }

    /// #187: reject every request of this kind that is still pending for
    /// `session_id` or one of its sub-task descendants, when the turn that
    /// would have consumed it ended without completing (`/stop`, interrupt,
    /// prompt error). The tool fiber behind such a request is dead, so
    /// approving it is meaningless; leaving it alive only lets the next turn
    /// re-host a ghost block (ADR-0038, rule 1). Mirrors
    /// [`approve_pending_for_session`]'s session/descendant filter.
    ///
    /// Only a KNOWN state is resolved: a failed/timed-out list, an empty
    /// session id, another session, and a snapshot claim all stay untouched —
    /// unknown is never read as resolved (#130/#144). Returns the ids the
    /// server actually rejected, in list order, so the caller turns their
    /// blocks into `🚫 已拒绝` receipts through `resolve_blocks`.
    pub(crate) async fn reject_pending_for_session(
        &self,
        requests: &RequestsHandle,
        sessions: &SessionsHandle,
        backend: &Arc<dyn opencode::Backend>,
        session_id: &str,
        directory: &str,
    ) -> Vec<String> {
        let dir_backend = backend.clone().for_directory(directory);
        // Bounded like the sweep's list: a half-open connection must not stall
        // the turn's finish behind a request that will never answer.
        let listed = match crate::bridge::bounded_call(
            &format!("turn end {} ({}) list", self.kind.label(), directory),
            self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            self.kind.list(&dir_backend),
        )
        .await
        {
            Some(Ok(listed)) => listed,
            Some(Err(e)) => {
                tracing::warn!("turn end {} ({}): {}", self.kind.label(), directory, e);
                return Vec::new();
            }
            None => return Vec::new(),
        };
        let mut rejected = Vec::new();
        for req in &listed {
            let sid = req.session_id();
            if !session_belongs_to(sessions, backend, sid, session_id, directory).await {
                continue;
            }
            // An answered request was already resolved (or is being resolved)
            // by its click: never reply a second time.
            if self.is_answered(requests, req.id()).await {
                continue;
            }
            // A claimed request's block lives on a snapshot card, whose
            // lifecycle is its own (ADR-0038, rule 6).
            if requests.snapshot_claims.lock().await.contains(req.id()) {
                continue;
            }
            let result = self.kind.reject(self, &dir_backend, req).await;
            match result {
                Ok(()) => {
                    tracing::info!(
                        "Rejected leftover {} {} on session {} (its turn ended without completing)",
                        self.kind.label(),
                        req.id(),
                        sid
                    );
                    rejected.push(req.id().to_string());
                }
                // Resolved elsewhere in the meantime: the sweep's next pass
                // leaves the neutral receipt for it — cola did not decide.
                Err(e) if e.is_not_found() => {
                    tracing::info!(
                        "leftover {} {} was already resolved: {}",
                        self.kind.label(),
                        req.id(),
                        e
                    )
                }
                // Genuine failure (network, routing): the request may still be
                // pending, so leave its block live — the Host can still answer
                // it, and the sweep keeps polling.
                Err(e) => tracing::warn!("reject leftover {} {}: {}", self.kind.label(), req.id(), e),
            }
        }
        rejected
    }

    /// Independent poller: surfaces pending requests as cards (inline on a
    /// streaming card when possible, else a separate card), auto-resolves where
    /// the kind says so, and marks stale cards when another client resolves a
    /// request. Spawned once per kind at App startup. The [`PollLoop`] seam
    /// owns the cadence ([`Self::poll_interval_ms`], injectable), the
    /// serverless guard and the failure latch; the pass is one sweep.
    ///
    /// The pass returns `Ok(())` by design (ADR-0048): every per-item condition
    /// of a sweep — one directory's list call, one request's surfacing, one
    /// card repaint — is reported where it happens with its own finer
    /// key/scope, so there is no tick-level failure for the loop's keyless
    /// latch to name. That latch is consumed only by a pass that fails as a
    /// whole; the server-reconcile loop is the one such pass today. A future
    /// pass returns `Err` only when the tick itself could not do its job, never
    /// to relay a per-item condition.
    ///
    /// [`PollLoop`]: crate::bridge::poll::PollLoop
    pub(crate) async fn poll_loop(&self, handles: &FlowHandles) -> crate::error::Result<()> {
        // The pass returns its tick's future, so the loop's cross-tick memory
        // (the requests it has already surfaced) travels through an Arc rather
        // than a borrowed local; this flow is its only owner.
        let seen = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let mut poll = crate::bridge::poll::PollLoop::new(&self.poll_interval_ms, "request poll");
        poll.poll(
            || !handles.backend.base_url().is_empty(),
            move || {
                let seen = Arc::clone(&seen);
                async move {
                    let mut seen = seen.lock().await;
                    self.sweep(handles, &mut seen).await;
                    // Per-item failures were logged inside `sweep`; nothing
                    // tick-level to latch (see the doc above).
                    Ok(())
                }
            },
        )
        .await
    }

    /// One poll iteration: list pending requests per known session directory,
    /// surface the unseen ones, then reconcile everything that left the list
    /// (stale standalone cards, inline sections, snapshot claims, and the
    /// remembered question state). Extracted from the loop so tests can drive
    /// one deterministic sweep; the loop's serverless guard (Lazy Start hasn't
    /// attached or spawned yet) lives in [`Self::poll_loop`], so a direct
    /// caller on a serverless app must skip it itself.
    pub(crate) async fn sweep(&self, handles: &FlowHandles, seen: &mut std::collections::HashSet<String>) {
        // Pending requests live in the server instance for the session's
        // directory; `GET /permission` / `GET /question` must be scoped with
        // `?directory=` or they only see the server cwd instance. Check every
        // known session directory.
        let directories = { handles.sessions.store.lock().await.directories() };
        let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Feishu's Instant Reminder (ADR-0043): this kind's pin candidates,
        // collected here and resolved after the sweep. Collected (not resolved
        // inline) so a request auto-resolved in the same sweep (e.g.
        // `/autoaccept`) never pins — it was never waiting on the user.
        let mut pin_candidates: Vec<(PendingRequest, String)> = Vec::new();
        let mut auto_resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Directories whose list call failed (error or timeout) this sweep.
        // `pending` only speaks for directories that listed SUCCESSFULLY, so
        // their state must NOT be read as resolved (#130).
        let mut failed_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Directories that DID list successfully this sweep, remembered on the
        // flow so a later late click can classify a missing state entry.
        let mut listed_now: std::collections::HashSet<String> = std::collections::HashSet::new();
        for dir in &directories {
            let backend = handles.backend.clone().for_directory(dir);
            // Bound the list call: a server restart can leave an in-flight
            // request on a half-open connection, and without a timeout the
            // poller would sit on it forever (no cards, no auto-accept).
            // On timeout the call is cancelled and the next tick retries.
            let listed = match crate::bridge::bounded_call(
                &format!("poll {} ({}) list", self.kind.label(), dir),
                self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
                self.kind.list(&backend),
            )
            .await
            {
                Some(listed) => listed,
                None => {
                    failed_dirs.insert(dir.clone());
                    continue;
                }
            };
            match listed {
                Ok(requests) => {
                    listed_now.insert(dir.clone());
                    for req in &requests {
                        pending.insert(req.id().to_string());
                        // Both pin surfaces need the listed candidates: the
                        // reminder's chat-level target and the waiting-card
                        // registry's host message.
                        if handles.waits.reminder.enabled() || handles.waits.message_pins.enabled() {
                            pin_candidates.push((req.clone(), dir.clone()));
                        }
                        // One span per request (ADR-0048): its `prepare`, its
                        // re-host and its card delivery are all retrievable by
                        // the session it belongs to. `chat`/`topic` ride along
                        // when the store maps that session — a sub-task child
                        // is not mapped and carries `session` alone.
                        let thread_key = {
                            let store = handles.sessions.store.lock().await;
                            store
                                .entry_for_session(req.session_id())
                                .map(|e| e.thread_key.clone())
                        };
                        let span = crate::bridge::span::request(req.session_id(), thread_key.as_ref());
                        if self.surface(handles, req, dir, seen).instrument(span).await {
                            auto_resolved.insert(req.id().to_string());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("poll {} ({}): {}", self.kind.label(), dir, e);
                    failed_dirs.insert(dir.clone());
                }
            }
        }
        // Instant Reminder (ADR-0043): reconcile this kind's pending requests
        // with the pin state. A sweep where any directory failed must not
        // move it — that directory said nothing, and unknown is never read as
        // resolved (#130) — so the pin survives until a complete sweep. A
        // request auto-resolved in this sweep (prepare said handled) is not a
        // wait, so it never pins.
        if handles.waits.reminder.enabled() && failed_dirs.is_empty() {
            let mut pin_targets: Vec<crate::bridge::reminder::ReminderTarget> = Vec::new();
            for (req, dir) in &pin_candidates {
                if auto_resolved.contains(req.id()) {
                    continue;
                }
                // `None` (an external turn, or a request pending across a
                // restart) cannot be pinned — there is no requester to target.
                if let Some(target) = crate::bridge::reminder::reminder_target(
                    &handles.sessions,
                    &handles.cards,
                    &handles.backend,
                    req.session_id(),
                    dir,
                )
                .await
                {
                    pin_targets.push(target);
                }
            }
            handles
                .waits
                .reminder
                .sync(&handles.platform, self.kind.claim_kind(), &pin_targets)
                .await;
        }
        // Mark stale: a card cola sent whose request is no longer pending
        // (resolved by another client) and was NOT answered by cola. A card
        // owned by a directory whose list failed stays live (#144).
        mark_stale_cards(
            &handles.requests,
            &handles.platform,
            &pending,
            &self.sent_cards,
            &self.surfaces,
            &failed_dirs,
            self.kind.label(),
        )
        .await;
        // Requests cola itself is answering or has answered (a click's claim,
        // an auto-accept approval): their block belongs to the settlement
        // writing the true receipt, so the sweep must not read their
        // disappearance from the pending list as another client's work.
        // Snapshot once — every pass below must judge the same moment.
        let cola_claimed = handles.requests.claimed_requests().await;
        // Card handles (ADR-0038, rule 2): a live block on a card whose
        // accumulator is gone (a replaced/aborted turn) is repainted from the
        // cached JSON — the accumulator pass below only reaches the card its
        // own flush would repaint. `flush_owned` maps each accumulator-owned
        // block to that card, so a block whose handle already names it is left
        // to the flush (never resolved twice), while one whose handle names an
        // older card still gets that stale card repainted.
        let flush_owned = Turn::flush_owned_blocks(&handles.cards).await;
        let dropped = handles.cards.card_handles.lock().await.drop_vanished(
            self.kind.claim_kind(),
            &pending,
            &failed_dirs,
            &cola_claimed,
            &flush_owned,
            handled_elsewhere_receipt,
        );
        for (message_id, card) in dropped {
            if let Err(e) = handles.platform.update_message(&message_id, &card).await {
                tracing::warn!("vanished block repaint failed on {}: {}", message_id, e);
            } else {
                tracing::info!(
                    "{} block repainted from its card handle: {}",
                    self.kind.label(),
                    message_id
                );
            }
        }
        // Resolve inline blocks whose request vanished (resolved by another
        // client) into their Interaction Receipts and repaint the cards that
        // host them — the receipt lands within this sweep, with no reliance on
        // the render tick (which stops when a turn ends). Blocks owned by a
        // failed directory stay (#144).
        let repaint = self
            .kind
            .resolve_vanished_inline(&handles.cards, &pending, &failed_dirs, &cola_claimed)
            .await;
        for session_id in &repaint {
            Turn::flush_card(&handles.cards, session_id).await;
        }
        // ADR-0028: a claimed request that left the pending list was
        // resolved — by the snapshot's own buttons (the click handler
        // re-renders synchronously; this just cleans the registry) or by
        // another client (the block must drop from the snapshot). The
        // snapshot card itself is never marked stale — that targets
        // standalone cards. A claim hosted from a directory whose list failed
        // stays: that directory said nothing (#144). The registry returns the
        // rebuilt cards; the flow patches them (never holding the registry
        // lock across the Feishu call).
        let dropped = handles.requests.snapshot_claims.lock().await.drop_vanished(
            self.kind.claim_kind(),
            &pending,
            &failed_dirs,
        );
        for (message_id, card) in dropped {
            if let Err(e) = handles.platform.update_message(&message_id, &card).await {
                tracing::warn!("snapshot claim drop: card update failed: {}", e);
            } else {
                tracing::info!("snapshot {} re-rendered without resolved claims", message_id);
            }
        }
        // Waiting-card pins (ADR-0043 amendment): keep every card still
        // waiting on the user in its chat's pinned-message list, so the
        // Chat/Topic's Instant Reminder nudge leads to the exact message once
        // the user opens the chat. The host is the card rendering the live
        // block (the streaming card, possibly a re-hosted newer one), the
        // standalone card the request was sent as, or — for a request claimed
        // by a Session Snapshot (ADR-0028) — the snapshot card itself, whose
        // lifecycle owns the claimed block. A request cola is already
        // answering or one auto-resolved in this sweep is not a wait and never
        // pins. Best-effort, like the reminder; a failed directory's tracked
        // pin stays (#130).
        if handles.waits.message_pins.enabled() {
            let claimed_hosts: std::collections::HashMap<String, String> = {
                let claims = handles.requests.snapshot_claims.lock().await;
                pin_candidates
                    .iter()
                    .filter_map(|(req, _)| {
                        claims
                            .claim_of(req.id())
                            .map(|(message_id, _)| (req.id().to_string(), message_id.to_string()))
                    })
                    .collect()
            };
            // Snapshot the standalone cards first: never hold `sent_cards` and
            // `card_handles` at once (one lock order, no deadlock surface).
            let sent_ids: std::collections::HashMap<String, String> = self
                .sent_cards
                .lock()
                .await
                .iter()
                .map(|(id, card)| (id.clone(), card.message_id.clone()))
                .collect();
            let waiting: Vec<crate::bridge::message_pins::WaitingCard> = {
                let handles = handles.cards.card_handles.lock().await;
                pin_candidates
                    .iter()
                    .filter(|(req, _)| !auto_resolved.contains(req.id()) && !cola_claimed.contains(req.id()))
                    .filter_map(|(req, dir)| {
                        let message_id = claimed_hosts
                            .get(req.id())
                            .cloned()
                            .or_else(|| handles.message_of(req.id()).map(str::to_string))
                            .or_else(|| sent_ids.get(req.id()).cloned())?;
                        Some(crate::bridge::message_pins::WaitingCard {
                            request_id: req.id().to_string(),
                            directory: dir.clone(),
                            message_id,
                        })
                    })
                    .collect()
            };
            handles
                .waits
                .message_pins
                .sync(&handles.platform, self.kind.claim_kind(), &waiting, &failed_dirs)
                .await;
        }
        // #130: remember which directories listed successfully — with that
        // knowledge a missing state entry proves the request left pending.
        // A directory that left the session store is forgotten: pending can no
        // longer speak for it, so its pruned state must NOT be classified as
        // resolved later. (Scoped so the lock never overlaps question_state.)
        {
            let mut known = self.listed_dirs.lock().await;
            known.retain(|dir| directories.contains(dir));
            known.extend(listed_now);
        }
        // #130: forget question state whose request left the pending list —
        // the flip side of the stale-card / inline / claim cleanups above.
        // A directory that failed to list is retained: we do not know it
        // resolved, and unknown must never be read as resolved.
        self.question_state
            .lock()
            .await
            .retain(|id, state| pending.contains(id) || failed_dirs.contains(state.dir()));
        // The recovered-surface memory is one-shot: an entry the sweep did not
        // list (the request was resolved while cola was down) has no card left
        // to re-adopt — its surfaces were reconciled by the stale/inline
        // cleanups above. A failed directory's entries stay: it said nothing
        // (#130).
        self.recovered
            .lock()
            .await
            .retain(|id, dir| pending.contains(id) || failed_dirs.contains(dir));
    }

    /// Everything one sweep does with ONE listed request, wrapped by the
    /// caller in that request's session span (ADR-0048): the snapshot-claim
    /// skip, the re-host for a request already surfaced, the kind's `prepare`
    /// (auto-accept / remember), and the card delivery — inline on the
    /// session's live card, else a standalone card. Returns whether `prepare`
    /// handled the request (an auto-accept), which the sweep must exclude from
    /// its reminder/pin reconciliation.
    ///
    /// `seen` records the requests this process has surfaced: a re-host needs
    /// the record (`seen.contains`), a first surfacing inserts it — so the
    /// poller never surfaces one request twice.
    async fn surface(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
        seen: &mut std::collections::HashSet<String>,
    ) -> bool {
        // ADR-0028: a claimed request is hosted by a snapshot card — already
        // surfaced, never a standalone card, never re-inlined.
        if handles.requests.snapshot_claims.lock().await.contains(req.id()) {
            return false;
        }
        if seen.contains(req.id()) {
            // Already surfaced: if it outlived the turn that hosted it,
            // re-host it onto the session's current card so the live controls
            // follow the newest card (ADR-0038, rule 1).
            self.rehost_block(handles, req, dir).await;
            return false;
        }
        seen.insert(req.id().to_string());
        tracing::info!(
            "{} ({}): {} on session {}",
            self.kind.label(),
            dir,
            req.id(),
            req.session_id()
        );
        // ADR-0038 restart re-adoption: a request whose card a PREVIOUS
        // process persisted is already surfaced. Re-adopt that card — remember
        // what its buttons need to resolve, repaint it — instead of posting a
        // second one. The entry is one-shot: it exists exactly for the first
        // sweep that meets the request after the restart. A recovered card
        // that cannot be repainted falls through to the normal path, which
        // surfaces the request as a standalone card.
        if self.recovered.lock().await.remove(req.id()).is_some()
            && self.readopt_surface(handles, req, dir).await
        {
            return false;
        }
        // Kind-specific pre-card handling (auto-accept / remember). true →
        // handled, no card needed.
        if self.kind.prepare(self, handles, req, dir).await {
            return true;
        }
        // One-card-per-turn: surface the request INLINE on the streaming card
        // of the session that owns it — the session itself, or (sub-task
        // children) its nearest ancestor with a live card. Only a separate card
        // when there is no active card (e.g. external turns or restarts).
        if let Some(host) =
            inline_host_session(&handles.cards, &handles.backend, req.session_id(), Some(dir)).await
        {
            let pushed = self.kind.add_inline(self, &handles.cards, &host, req, dir).await;
            if pushed {
                tracing::info!(
                    "{} {} inlined on session {} card",
                    self.kind.label(),
                    req.id(),
                    host
                );
                // Flush so the inline section appears NOW — the render loop
                // only flushes on new parts, and a blocked prompt produces
                // none.
                Turn::flush_card(&handles.cards, &host).await;
            }
            return false;
        }
        let card = self.kind.build_card(req, dir);
        // Reply to the message that triggered the prompt for this session; fall
        // back to sending into the chat when the accumulator is gone (e.g.
        // after a cola restart). Sub-task sessions resolve up the parent chain.
        let sent_id = match resolve_card_target(
            &handles.sessions,
            &handles.cards,
            &handles.backend,
            &handles.platform,
            req.session_id(),
            dir,
        )
        .await
        {
            Some(CardTarget::ReplyTo(msg_id)) => handles.platform.reply_card(&msg_id, &card).await.ok(),
            Some(CardTarget::Chat(chat_id)) => {
                handles.platform.send_card("chat_id", &chat_id, &card).await.ok()
            }
            None => {
                tracing::warn!(
                    "No reply target or chat for {} on session {}",
                    self.kind.label(),
                    req.session_id()
                );
                None
            }
        };
        if let Some(mid) = sent_id {
            let summary = self.kind.summary(req);
            self.sent_cards.lock().await.insert(
                req.id().to_string(),
                SentCard {
                    message_id: mid.clone(),
                    summary: summary.clone(),
                    directory: dir.to_string(),
                },
            );
            // Persist the standalone surface so a restart re-adopts it instead
            // of posting a second card (ADR-0038 restart re-adoption).
            self.surfaces.set_standalone(
                req.id(),
                StandaloneSurface {
                    kind: self.kind.claim_kind(),
                    message_id: mid,
                    summary,
                    directory: dir.to_string(),
                },
            );
        } else {
            tracing::warn!(
                "{} card send failed on session {}",
                self.kind.label(),
                req.session_id()
            );
        }
        false
    }

    /// Re-host a still-pending inline block onto the session's current card
    /// (ADR-0038, rule 1): a request that outlived the turn which surfaced it
    /// follows the newest card, and the card that used to show it is repainted
    /// without the controls — so a live block lives on exactly one card. Called
    /// from the sweep for requests already in `seen` (the poller would
    /// otherwise never touch them again).
    async fn rehost_block(&self, handles: &FlowHandles, req: &PendingRequest, dir: &str) {
        let id = req.id();
        // Only a block the flush recorded (an inline card) follows the card:
        // a standalone card and a snapshot claim keep their lifecycles
        // (ADR-0038, rule 6).
        let Some(old_message) = handles
            .cards
            .card_handles
            .lock()
            .await
            .message_of(id)
            .map(str::to_string)
        else {
            return;
        };
        let Some(host) =
            inline_host_session(&handles.cards, &handles.backend, req.session_id(), Some(dir)).await
        else {
            return;
        };
        if !Turn::has_card(&handles.cards, &host).await {
            return;
        }
        // The accumulator still carries the block (its own flush owns the
        // current card), or the handle already names that card: nothing to
        // move.
        if Turn::has_interaction_in(&handles.cards, &host, id).await
            || Turn::card_message_id(&handles.cards, &host).await.as_deref() == Some(old_message.as_str())
        {
            return;
        }
        let pushed = self.kind.add_inline(self, &handles.cards, &host, req, dir).await;
        if !pushed {
            return;
        }
        // Flush first: the current card renders the block and the handle moves
        // with it.
        Turn::flush_card(&handles.cards, &host).await;
        tracing::info!(
            "{} {} re-hosted on session {} card (old {})",
            self.kind.label(),
            id,
            host,
            old_message
        );
        // Then strip the old card's copy through its handle. The block moved,
        // not resolved: no receipt — the controls simply leave that card.
        if let Some(card) = handles
            .cards
            .card_handles
            .lock()
            .await
            .remove_on(&old_message, id)
            && let Err(e) = handles.platform.update_message(&old_message, &card).await
        {
            tracing::warn!("re-host old-card repaint failed on {}: {}", old_message, e);
        }
    }

    /// Re-adopt the card a previous process surfaced this request on (ADR-0038
    /// restart re-adoption). The kind's in-flight state is remembered first, so
    /// a click on the card resolves against this process; then the persisted
    /// card is repainted — refreshed from that state for a question, whose
    /// partial answers did not survive the restart. Returns whether the request
    /// is settled on its recovered card; `false` means the card is gone (or its
    /// persisted JSON unusable), so the caller must surface the request
    /// normally.
    async fn readopt_surface(&self, handles: &FlowHandles, req: &PendingRequest, dir: &str) -> bool {
        let id = req.id();
        // Remembering the kind's state is what makes the card's buttons resolve
        // — the poller never ran `prepare` for a request it considered already
        // surfaced.
        self.kind.remember_surfaced(self, req, dir).await;
        // A recovered standalone card needs no repaint: it is already the live
        // surface and its buttons carry everything a click needs (ADR-0038,
        // rule 6).
        let Some(message_id) = handles
            .cards
            .card_handles
            .lock()
            .await
            .message_of(id)
            .map(str::to_string)
        else {
            return true;
        };
        // Inline: repaint the persisted card — refreshed from the state this
        // process holds for a question, so partial answers that did not
        // survive the restart are not shown as selected.
        let repainted = match self
            .kind
            .refresh_cached_block(self, &handles.cards, &message_id, req, dir)
            .await
        {
            Some(card) => match handles.platform.update_message(&message_id, &card).await {
                Ok(()) => {
                    tracing::info!(
                        "{} {} re-adopted on card {} after restart",
                        self.kind.label(),
                        id,
                        message_id
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        "{} {} re-adoption repaint failed on {}: {} — surfacing it anew",
                        self.kind.label(),
                        id,
                        message_id,
                        e
                    );
                    false
                }
            },
            // The persisted JSON cannot render the block: the handle is dead.
            None => false,
        };
        if !repainted {
            // The card is gone (or unusable): forget the stale surface, in
            // memory and in the record, and let the caller surface the request
            // normally.
            let mut handles = handles.cards.card_handles.lock().await;
            handles.remove_on(&message_id, id);
            handles.forget(id);
        }
        repainted
    }

    /// The owning directory of a request this flow knows, when a callback
    /// carries none and the session store cannot resolve it: the question's
    /// remembered request, the standalone card's record, or the persisted
    /// block's handle (a re-adopted card after a restart).
    async fn remembered_directory(&self, cards: &CardsHandle, req_id: &str) -> Option<String> {
        if let Some(dir) = self.question_dir(req_id).await {
            return Some(dir);
        }
        if let Some(card) = self.sent_cards.lock().await.get(req_id) {
            return Some(card.directory.clone());
        }
        cards
            .card_handles
            .lock()
            .await
            .directory_of(req_id)
            .map(str::to_string)
    }

    /// Handle a card action on this kind's card: answer / submit / reject. The
    /// shared skeleton resolves the delivery context and the double-click guard;
    /// the kind applies its own semantics.
    pub(crate) async fn handle_card_action(
        &self,
        handles: &FlowHandles,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let session_id = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        let request_id = value.get("request_id").and_then(|v| v.as_str());
        let req_id = request_id?;
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let directory = match directory {
            Some(d) => Some(d),
            None => {
                let from_store = handles
                    .sessions
                    .store
                    .lock()
                    .await
                    .directory_for_session(session_id);
                if from_store.is_some() {
                    from_store
                } else {
                    self.remembered_directory(&handles.cards, req_id).await
                }
            }
        };
        let directory = directory.as_deref();

        let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
        // The card the click landed on, so its handle can update THAT card
        // atomically in the ack (ADR-0038, rule 3) — the callback context
        // carries it (ws.rs injects `open_message_id`).
        let clicked = value.get("open_message_id").and_then(|v| v.as_str());
        // ADR-0028: a claimed request's block lives on a static snapshot card,
        // NOT a streaming card — its click is not "inline", and the ack must
        // patch the snapshot (block updated or dropped), never a standalone
        // card.
        let claimed_message = {
            let registry = handles.requests.snapshot_claims.lock().await;
            registry.claim_of(req_id).map(|(m, _)| m.to_string())
        };
        // Inline interaction: the session (or its sub-task parent chain) has a
        // live streaming card, so the result is NOT returned as a replacement
        // card — the streaming card re-renders itself on the next poll. The
        // clicked card rendering the block through its handle counts too: a
        // re-adopted card after a restart has no accumulator, but its cached
        // JSON still carries the block and the ack can edit it in place.
        let host = if claimed_message.is_some() {
            None
        } else if Turn::has_card(&handles.cards, session_id).await {
            Some(session_id.to_string())
        } else {
            inline_host_session(&handles.cards, &handles.backend, session_id, directory).await
        };
        let inline = host.is_some()
            || match clicked {
                Some(message_id) => handles
                    .cards
                    .card_handles
                    .lock()
                    .await
                    .renders(message_id, req_id),
                None => false,
            };

        let mut r = self
            .kind
            .handle_action(
                self, handles, session_id, req_id, reply, value, directory, inline, &host, clicked,
            )
            .await;

        // A late second click on a block already resolved from this snapshot
        // (the double-click race: the first click's re-render is still in
        // flight): keep patching the snapshot — never replace the whole card
        // with a standalone result card.
        if claimed_message.is_none()
            && handles
                .requests
                .snapshot_claims
                .lock()
                .await
                .is_tombstoned(req_id)
            && let Some(result) = &mut r
        {
            result.card = None;
        }
        if let Some(result) = &mut r
            && let Some(message_id) = claimed_message
        {
            // Did THIS click resolve the request? The double-click guard
            // cannot tell a successful reply from a failed one, so check the
            // server: the block drops only once the request is no longer
            // pending. A failed reply (already resolved elsewhere) also leaves
            // it pending → the block stays and the poll sweep cleans it.
            let still_pending = match directory {
                Some(dir) => self
                    .kind
                    .list(&handles.backend.clone().for_directory(dir))
                    .await
                    .map(|reqs| reqs.iter().any(|r| r.id() == req_id))
                    .unwrap_or(true),
                None => true,
            };
            if still_pending {
                // The click recorded state (question) or was a no-op
                // (double-click): keep the block and show the live answer
                // state, so the snapshot mirrors the standalone card's
                // 已选/✅ markers.
                let pending = handles
                    .requests
                    .snapshot_claims
                    .lock()
                    .await
                    .host_pending(&message_id)
                    .unwrap_or_default();
                let state = handles.requests.question.live_snapshot_state(&pending).await;
                result.card = handles
                    .requests
                    .snapshot_claims
                    .lock()
                    .await
                    .rebuild(&message_id, &state);
            } else {
                // Resolved: drop the block from the snapshot and patch it in
                // place — no new message. `resolve` does the whole transition
                // (unclaim → tombstone → rebuild → prune) under one lock.
                let state = crate::feishu::snapshot_card::SnapshotQuestionState::new();
                result.card =
                    handles
                        .requests
                        .snapshot_claims
                        .lock()
                        .await
                        .resolve(req_id, &message_id, &state);
            }
        }
        r
    }
}

/// Whether session `candidate` belongs to `session_id` — the session itself or
/// one of its sub-task descendants (parent-chain walk). The ownership filter
/// `/autoaccept`'s pending approval and the turn-end leftover rejection (#187)
/// share; an empty id matches nothing.
pub(crate) async fn session_belongs_to(
    sessions: &SessionsHandle,
    backend: &Arc<dyn opencode::Backend>,
    candidate: &str,
    session_id: &str,
    directory: &str,
) -> bool {
    !candidate.is_empty()
        && (candidate == session_id
            || sessions
                .descends_from(backend, candidate, session_id, directory)
                .await)
}

/// #187: settle what a turn that ended without completing left behind. Reject
/// the still-pending Permission/Question requests of the turn's session and
/// its sub-task descendants on the server, then resolve their blocks through
/// the one seam into `🚫 已拒绝` receipts — nobody else handled them, so the
/// neutral `⏱ 已由其他客户端处理` line would be a lie. A no-op without a mapped
/// directory and on a failed list (unknown is never resolved). Returns how
/// many requests were rejected.
pub(crate) async fn reject_leftovers_for_turn(
    requests: &RequestsHandle,
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn opencode::Backend>,
    session_id: &str,
) -> usize {
    let directory = sessions.directory_for_session(session_id).await;
    let Some(directory) = directory.filter(|d| !d.is_empty()) else {
        return 0;
    };
    let mut rejected = 0;
    for flow in [&requests.permission, &requests.question] {
        let ids = flow
            .reject_pending_for_session(requests, sessions, backend, session_id, &directory)
            .await;
        if ids.is_empty() {
            continue;
        }
        rejected += ids.len();
        resolve_blocks(
            flow,
            cards,
            requests,
            &Some(session_id.to_string()),
            session_id,
            Origin::Command,
            &ids,
            Residue::PerBlock(&denied_receipt),
        )
        .await;
    }
    if rejected > 0 {
        tracing::info!(
            "turn {} ended without completing: rejected {} leftover request(s)",
            session_id,
            rejected
        );
    }
    rejected
}
