use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::Instrument;

use crate::backend::{MessageRole, Part, SessionTranscript, TurnAnchor};
use crate::bridge::handles::{CardsHandle, FlowHandles, NoticeRules};
use crate::bridge::turn::{ContinuationFacts, SettleTiming, Turn, WakeContinuation, YieldedUpdate};

/// The external-message flow: watches for user messages that were NOT sent by
/// cola (someone posted from OpenChamber or another client on the shared store)
/// and notifies the Feishu side with a small card. cola's own messages are
/// excluded by their self-identifying `msg_cola_` id (ADR-0026), never by a
/// timestamp baseline — authorship lives on the message, so a server crash
/// mid-turn cannot make cola's own round look external afterwards.
///
/// ADR-0059 grows this into **Session Sync**: the same per-thread pass over the
/// Active Session also renders **Wakes** (the Backend resuming the Session with
/// no user message) as Card Chain continuations, so work that happens after a
/// card was finalized reaches Feishu as a new message — its own notification.
pub struct ExternalFlow {
    /// session_id → Sync Watermark: the created time of the newest user message
    /// the poller has already accounted for (cola-authored or external). Owned
    /// by the poller alone — the prompt path no longer records it (ADR-0026).
    pub last_user_msg_epoch: Arc<Mutex<HashMap<String, i64>>>,
    /// External poll cadence (ms). Defaults to today's 8 s; tests store a small
    /// value so every loop branch runs without sleeping real seconds.
    pub poll_interval_ms: std::sync::atomic::AtomicU64,
    /// How long (ms) one backend call in the poll loop may take before it is
    /// abandoned and retried on the next tick. Bounds a request stuck on a
    /// half-open connection (e.g. across a server restart) so one hung call
    /// cannot freeze the poller forever. Defaults to 30 s; tests store a small
    /// value so the timeout branch runs without sleeping.
    pub request_timeout_ms: std::sync::atomic::AtomicU64,
    /// External-reply render poll cadence (ms).
    pub render_poll_ms: std::sync::atomic::AtomicU64,
    /// The external renderer's IDLE bound (ms): how long a run may go without
    /// rendering anything new before the loop gives up. The deadline is
    /// renewed on every poll whose render adds or updates content (#457), so a
    /// producing run is never cut; a message posted in OpenChamber that never
    /// actually sends still idles out, and the card then simply stays the
    /// "有新消息" notification. Injected small in tests to exercise the idle
    /// branch.
    pub render_idle_timeout_ms: std::sync::atomic::AtomicU64,
    /// The Wake continuation's settle grace (ms): the shared out-of-turn
    /// settle loop's lost-contact / stuck-panel bound. A different semantic
    /// from [`Self::render_idle_timeout_ms`] — "nobody can act" vs "nothing is
    /// produced" — so the two never share one value (#457).
    pub wake_settle_grace_ms: std::sync::atomic::AtomicU64,
    /// The Completion Notice's opt-in rules (ADR-0043), wired by the
    /// coordinator: this pass settles a yielded card's quiet true end (ADR-0060)
    /// and announces it with the same notice every other turn end sends.
    notice: NoticeRules,
}

impl ExternalFlow {
    pub fn new(notice: NoticeRules) -> Self {
        Self {
            last_user_msg_epoch: Arc::new(Mutex::new(HashMap::new())),
            poll_interval_ms: std::sync::atomic::AtomicU64::new(8_000),
            request_timeout_ms: std::sync::atomic::AtomicU64::new(30_000),
            render_poll_ms: std::sync::atomic::AtomicU64::new(1_500),
            render_idle_timeout_ms: std::sync::atomic::AtomicU64::new(600_000),
            wake_settle_grace_ms: std::sync::atomic::AtomicU64::new(600_000),
            notice,
        }
    }

    /// Independent poller: detects user messages not sent by cola and notifies
    /// the Feishu side. Started once at App startup.
    ///
    /// The Sync Watermark is advanced on every poll that reads the session:
    /// over cola-authored messages (which never notify) and over external
    /// messages (which notify once, at the moment they first exceed the
    /// watermark). Authorship is authoritative (ADR-0026): a user message whose
    /// id starts with `msg_cola_` was submitted by cola. cola chooses that id
    /// at send time and the server persists it, so even when a server dies
    /// mid-turn and cola never reads back the message's created time, the
    /// poller still recognises it as cola's own on the first poll after a heal
    /// — it can never be mistaken for an external message.
    ///
    /// The pass returns `Ok(())` by design (ADR-0048): a per-session failure (a
    /// message read, a notify, a card send) is logged where it happens, under
    /// that session's `external` span, so there is no tick-level failure for
    /// the loop's keyless latch to name. That latch is consumed only by a pass
    /// that fails as a whole; the server-reconcile loop is the one such pass
    /// today. A future pass returns `Err` only when the tick itself could not
    /// do its job, never to relay a per-item condition.
    pub(crate) async fn poll_loop(&self, handles: &FlowHandles) -> crate::error::Result<()> {
        let mut poll = crate::bridge::poll::PollLoop::new(&self.poll_interval_ms, "external message sync");
        // The seam owns the cadence ([`Self::poll_interval_ms`], injectable),
        // the sleep and the serverless guard (Lazy Start hasn't attached or
        // spawned a server yet — nothing to watch on the store, so the pass is
        // skipped quietly). The pass is one sync over the active sessions.
        poll.poll(
            || !handles.backend.base_url().is_empty(),
            move || async move {
                self.sync_sessions(handles).await;
                // Per-session failures were logged inside `sync_sessions`;
                // nothing tick-level to latch (see the doc above).
                Ok(())
            },
        )
        .await
    }

    /// One sync pass: snapshot the sessions from ONE store view, then run each
    /// active one through [`Self::poll_session`].
    async fn sync_sessions(&self, handles: &FlowHandles) {
        // Follow any Session that moved itself (into a git worktree, #433)
        // BEFORE the snapshot below, so this pass's reads and the sweeps'
        // directory set already use the new location. The returned map is the
        // PRE-follow directory of every session this pass moved — the move
        // verdict's baseline for a record that carries no directory of its own
        // (#439).
        let moved_from = self.follow_locations(handles).await;
        // Only each thread's ACTIVE session is synced (ADR-0017): a lobby
        // (p2p/group) can stack several sessions via /new and /switch, and
        // notifying for a historical one would interleave its cards with the
        // current conversation's. Historical sessions are skipped AND their
        // Sync Watermark cleared, so switching back to one re-syncs its
        // watermark silently (external messages received while it was
        // inactive are marked read, not replayed).
        //
        // `active` and `sessions` are derived from ONE store snapshot so a
        // session activated mid-poll can't slip through as active-but-unchecked.
        let (active, sessions): (
            std::collections::HashSet<String>,
            Vec<(String, crate::config::ThreadKey, String)>,
        ) = {
            let store = handles.sessions.store.lock().await;
            let mut active = std::collections::HashSet::new();
            let mut sessions = Vec::new();
            for e in store.all_entries() {
                if store
                    .get_active(&e.thread_key)
                    .map(|a| a.session_id == e.session_id)
                    .unwrap_or(false)
                {
                    active.insert(e.session_id.clone());
                }
                sessions.push((e.session_id.clone(), e.thread_key.clone(), e.directory.clone()));
            }
            (active, sessions)
        };
        // The same snapshot as a per-session lookup: the reap's records are
        // keyed by session id, not by the thread loop.
        let mapped: HashMap<String, (crate::config::ThreadKey, String)> = sessions
            .iter()
            .map(|(sid, thread_key, directory)| (sid.clone(), (thread_key.clone(), directory.clone())))
            .collect();
        for (sid, thread_key, directory) in sessions {
            // One `external` span per Session (ADR-0048): the observation,
            // the notification and the renderer it arms are all retrievable
            // by `rg 'session=ses_x'`.
            let span = crate::bridge::span::external(&sid, Some(&thread_key));
            self.poll_session(handles, &active, &sid, &thread_key, &directory)
                .instrument(span)
                .await;
        }
        // The durable Live Card reap (ADR-0063): every record is reconciled
        // against its Session's own reads on EVERY pass, after the sessions'
        // own steps — so a Wake continuation this pass posted is already
        // visible and the card it took the chain from is collected, never left
        // looking live. A record whose session is no longer mapped is still
        // reconciled (no directory to route its status read), so the sidecar
        // cannot keep a record nothing will ever settle.
        let read_timeout_ms = self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        for (sid, record) in handles.cards.chains.entries() {
            let mapping = mapped.get(&sid);
            let span = crate::bridge::span::external(&sid, mapping.map(|(thread_key, _)| thread_key));
            // The route is the followed directory; the move verdict's baseline
            // is where the card was tracked — the pre-follow directory when
            // this pass followed the session (#433), else the reap falls back
            // to the route itself.
            let baseline = moved_from.get(&sid).map(String::as_str);
            crate::bridge::reap::reconcile(
                handles,
                &sid,
                mapping.map(|(_, directory)| directory.as_str()),
                baseline,
                &record,
                read_timeout_ms,
            )
            .instrument(span)
            .await;
        }
        // Pending Card Updates (ADR-0067): every pass retries the card writes
        // Feishu refused since the last one — the reap's own endings included.
        // The decorator owns the per-entry backoff; the WS loop forces an
        // immediate drain on reconnect, since REST and WS reachability are
        // independent.
        handles.platform.drain_pending_card_updates(false).await;
    }

    /// Reconcile the transcript's live Background Tasks against the server's
    /// runtime registries (issue #454), on the Session Sync read the yielded
    /// ledger and the Wake step already share. A shell the runtime reports
    /// ended — or no longer knows — leaves the live list as a retirement (its
    /// completion entry renders where a Wake's would); a subagent the runtime
    /// reports inactive stays live but reads as unconfirmed; a task with no
    /// verdict is untouched. One read, made only while the transcript lists
    /// live tasks, sharing the pass's request bound; any failure leaves the
    /// transcript exactly as read, so a flaky runtime read can never end a
    /// wait on its own.
    async fn reconcile_task_runtime(
        &self,
        handles: &FlowHandles,
        sid: &str,
        directory: &str,
        transcript: &mut SessionTranscript,
    ) {
        // Observe only while a card can still receive the ledger
        // ([`CardSession::accepts_ledger_refresh`]): the entry renders on the
        // chain that observes the retirement, so observing with a settled chain
        // — or no chain at all — would record the task and swallow its entry
        // (found on a real restart, 2026-10-01). The next Waiting card
        // reconciles instead; nothing is lost, the transcript stays as read.
        let accepts = handles
            .cards
            .cards
            .lock()
            .await
            .get(sid)
            .is_some_and(|card| card.accepts_ledger_refresh());
        if !accepts {
            return;
        }
        let shells: Vec<String> = transcript
            .background_tasks
            .iter()
            .filter_map(|task| task.shell_id.clone())
            .collect();
        let children: Vec<String> = transcript
            .background_tasks
            .iter()
            .filter_map(|task| task.child_id.clone())
            .collect();
        if shells.is_empty() && children.is_empty() {
            return;
        }
        let read_timeout_ms = self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        match crate::bridge::bounded_call(
            "task runtime read",
            read_timeout_ms,
            handles
                .backend
                .task_runtime(sid, Some(directory), &shells, &children),
        )
        .await
        {
            Some(Ok(runtime)) => {
                transcript.apply_task_runtime(&runtime);
                // Record the retirements so every later transcript read — the
                // live render, the drain's settle, the reap — sees them gone:
                // the launch record never flips (issue #454), so without the
                // overlay the next read would resurrect the task.
                if !transcript.runtime_retired.is_empty() {
                    let call_ids: Vec<String> = transcript
                        .runtime_retired
                        .iter()
                        .map(|retirement| retirement.task.tool.call_id.clone())
                        .collect();
                    tracing::info!(
                        "session {sid}: runtime reconciliation retired {} background task(s)",
                        call_ids.len()
                    );
                    handles.backend.retire_background_tasks(sid, &call_ids);
                }
            }
            Some(Err(error)) => {
                tracing::debug!("session {sid} task runtime read failed: {error}; waiting for the next read");
            }
            None => {}
        }
    }

    /// Follow every mapped Session's server-reported location (#433): a
    /// Session can move itself into another directory mid-life (an agent
    /// creating and entering a git worktree), and the directory-routed reads
    /// — the permission/form sweeps, the next turn's work context, the reap's
    /// status read — must target the new directory. One list read per pass,
    /// through the shared session-list cache (at most one fetch per cache TTL
    /// touches the wire); a session absent from the list, an empty directory
    /// or an unchanged one all claim nothing. A V1 server has no move, so the
    /// same read simply never observes a change there.
    ///
    /// Returns the PRE-follow directory of every session this pass moved
    /// (session id → directory) — the reap's move-verdict baseline for a
    /// record that carries no directory of its own (#439).
    async fn follow_locations(&self, handles: &FlowHandles) -> HashMap<String, String> {
        let read_timeout_ms = self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        let Some(Ok(sessions)) = crate::bridge::bounded_call(
            "session sync location read",
            read_timeout_ms,
            handles.sessions.cached_session_list(&handles.backend),
        )
        .await
        else {
            return HashMap::new();
        };
        let locations: Vec<(String, String)> = sessions
            .into_iter()
            .filter(|listed| !listed.directory.is_empty())
            .map(|listed| (listed.id, listed.directory))
            .collect();
        match handles.sessions.follow_locations(&locations).await {
            Ok(moved) => {
                let listed: HashMap<&str, &str> = locations
                    .iter()
                    .map(|(session_id, directory)| (session_id.as_str(), directory.as_str()))
                    .collect();
                for (session_id, previous) in &moved {
                    if let Some(directory) = listed.get(session_id.as_str()) {
                        tracing::info!(
                            "session {session_id} followed its move from {previous} to {directory}"
                        );
                    }
                }
                moved.into_iter().collect()
            }
            Err(e) => {
                tracing::warn!("session sync: could not follow the session locations: {e}");
                HashMap::new()
            }
        }
    }

    /// One Session's pass through the poll loop, run inside that Session's
    /// `external` span (ADR-0048): skip historical and in-flight sessions, read
    /// the newest user message, advance the Sync Watermark and — for a new
    /// External Message — notify Feishu and arm the reply renderer. Split out
    /// of [`Self::poll_loop`] so the whole pass is instrumented in one place.
    async fn poll_session(
        &self,
        handles: &FlowHandles,
        active: &std::collections::HashSet<String>,
        sid: &str,
        thread_key: &crate::config::ThreadKey,
        directory: &str,
    ) {
        if !active.contains(sid) {
            // Historical (non-active) session: stop syncing it and drop
            // its Sync Watermark so a later /switch back re-syncs it
            // silently (first observation, no replay).
            self.last_user_msg_epoch.lock().await.remove(sid);
            return;
        }
        // While cola is answering this session, any new message is cola's own.
        if handles.waits.inflight.lock().await.contains(sid) {
            return;
        }
        // The newest user message comes from the Session Transcript's shared
        // `newest_user` projection, so selection and ordering live once
        // (ADR-0053). Its author is authoritative (ADR-0026): a `msg_cola_` id
        // means cola sent it — advance the watermark, never notify. Only a
        // message written by another shared-store client and newer than the
        // watermark is an External Message.
        let Some(Ok(mut transcript)) = crate::bridge::bounded_call(
            "external poll transcript",
            self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            handles.backend.transcript(sid),
        )
        .await
        else {
            return;
        };
        // The Background Task runtime reconciliation (issue #454), on the same
        // read the ledger paths below consume: a shell/subagent whose
        // completion record was lost is retired here (or marked unconfirmed) so
        // a Waiting card cannot be stranded on a task that is not running. One
        // extra read only while the transcript still lists live tasks, and a
        // failed one leaves the read exactly as it was (never guessed).
        self.reconcile_task_runtime(handles, sid, directory, &mut transcript)
            .await;
        let Some(newest) = transcript.newest_user() else {
            return;
        };
        let Some(turn_anchor) = newest.anchor() else {
            return;
        };
        let cola_authored = crate::opencode::parsing::is_cola_message_id(newest.id.as_str());
        // The Session Sync pass's clock (ADR-0060): the yielded refresh
        // compares the ledger's rendered elapsed at second granularity, and
        // the Wake decision and the yielded refresh of this same read must
        // stamp one moment, not two.
        let now_ms = chrono::Utc::now().timestamp_millis();
        if cola_authored {
            // The Session Sync Wake step (ADR-0059): the newest user message is
            // cola's own, so any work in this Session that the card chain has
            // not rendered was resumed by the Backend (a Wake) or landed after
            // its card was finalized — render it as a continuation. Runs before
            // the watermark's early returns on purpose: a Wake's visibility
            // must never depend on the Sync Watermark, which accounts user
            // messages only and is never moved by a Wake (ADR-0026).
            self.render_wake_continuation(
                handles,
                &transcript,
                sid,
                thread_key,
                directory,
                &turn_anchor,
                now_ms,
            )
            .await;
            // The same read also keeps a yielded (Waiting) card's ledger fresh
            // in place — and settles the card when that read is its true end
            // (ADR-0060): the freeze's carve-out. Ordered after the Wake step on
            // purpose: a Wake that owes a continuation hands the ledger over
            // through its split and leaves no Waiting card behind, so only the
            // quiet retirement (no continuation to render) lands here, and a
            // read that changed nothing PATCHes nothing.
            let stopped = handles.waits.is_stopped(sid).await;
            match Turn::refresh_yielded_ledger(
                &handles.cards,
                &handles.backend,
                &handles.requests,
                sid,
                &transcript,
                now_ms,
                stopped,
                self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            )
            .await
            {
                YieldedUpdate::Unchanged => {}
                YieldedUpdate::Refreshed => {
                    tracing::info!("yielded ledger refreshed: session {sid}");
                }
                YieldedUpdate::Settled { notice_at } => {
                    tracing::info!("yielded ledger settled the true end: session {sid}");
                    // The Completion Notice's existing rules (ADR-0043): groups
                    // per the opt-in, p2p per the long-task threshold. A Wake
                    // continuation carries the clock as `None` — its own card
                    // send was the notification (ADR-0059).
                    if let Some(started_at) = notice_at {
                        crate::bridge::turn::send_completion_notice(
                            &handles.cards,
                            &handles.platform,
                            &self.notice,
                            sid,
                            started_at,
                        )
                        .await;
                    }
                }
            }
        }
        let mut map = self.last_user_msg_epoch.lock().await;
        let watermark = map.get(sid).copied();
        if cola_authored {
            // cola's own message: never notify; just make sure the
            // watermark covers it so later external messages compare
            // against it.
            if watermark.is_none_or(|w| turn_anchor.created_ms > w) {
                map.insert(sid.to_string(), turn_anchor.created_ms);
            }
            return;
        }
        // First observation: establish the watermark, don't notify.
        // External messages received before cola ever polled are marked
        // read, not replayed (ADR-0017).
        let Some(prev) = watermark else {
            map.insert(sid.to_string(), turn_anchor.created_ms);
            return;
        };
        if turn_anchor.created_ms > prev {
            map.insert(sid.to_string(), turn_anchor.created_ms);
            let preview = message_preview(&transcript, &turn_anchor);
            drop(map);
            tracing::info!("External message on session {}: {}", sid, preview);
            // The card title is the server's session title (ADR-0007)
            // — fetched on demand, never a cola-side name. Bounded like
            // the poll's other calls so a hung read cannot freeze the
            // notify path either.
            let title = crate::bridge::bounded_call(
                "external poll session_info",
                self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
                handles.backend.clone().for_directory(directory).session_info(sid),
            )
            .await
            .and_then(|r| r.ok())
            .and_then(|i| i.title)
            .unwrap_or_default();
            let card = crate::feishu::card::notify::build_external_message_card(&title, &preview);
            // A topic session must be reached by replying to a
            // message INSIDE the topic (the create API rejects
            // `receive_id_type=thread_id`). Resolve an in-topic
            // anchor — the persisted `/topic` confirmation card, or
            // the newest bot message in the thread. Non-topic
            // sessions fall back to the chat top level.
            let anchor = crate::bridge::pollers::resolve_topic_anchor(
                &handles.sessions,
                &handles.platform,
                thread_key,
            )
            .await;
            let sent = match anchor {
                Some(anchor) => handles.platform.reply_card(&anchor, &card).await,
                None => {
                    handles
                        .platform
                        .send_card("chat_id", &thread_key.chat_id, &card)
                        .await
                }
            };
            match sent {
                Ok(card_id) => {
                    // Now render the model's reply INTO that card, so
                    // the Feishu side sees the answer, not just the
                    // notification.
                    self.start_reply_render(handles, sid, &turn_anchor, &card_id, &preview)
                        .await;
                }
                Err(e) => tracing::warn!("external message notify: {}", e),
            }
        }
    }

    /// Arm an incremental renderer that streams the model's reply to the
    /// external message INTO the notification card (update in place — no
    /// second card). The loop exits when the turn finishes, cola's own prompt
    /// (or a newer external message) replaces the accumulator, a newer EXTERNAL
    /// message starts a new turn, or the idle bound elapses with nothing new
    /// rendered (#457) — a cola-authored Supplement merges into the run it is
    /// already streaming (#451).
    pub(crate) async fn start_reply_render(
        &self,
        handles: &FlowHandles,
        session_id: &str,
        anchor: &TurnAnchor,
        card_id: &str,
        preview: &str,
    ) {
        // Guard: a renderer for THIS message is already armed (the accumulator
        // still carries the message's full anchor — id + server time). cola's
        // own prompts get a fresh accumulator, so a different anchor is NOT
        // this message — a new renderer replaces the old one (whose anchor no
        // longer matches, so it exits).
        if armed_turn_anchor(&handles.cards, session_id).await.as_ref() == Some(anchor) {
            return;
        }
        let (session_dir, session_thread_key) = {
            let store = handles.sessions.store.lock().await;
            match store.entry_for_session(session_id) {
                Some(e) => (e.directory.clone(), Some(e.thread_key.clone())),
                None => (String::new(), None),
            }
        };
        // Card subtitle: the server's live title (ADR-0007), or the id-tail.
        let subtitle = self.session_subtitle(handles, session_id, &session_dir).await;

        // Footer model@variant: capture the session's `/think` variant when the
        // turn is ARMED, not when it finalizes — a `/think` issued mid-render
        // must not retro-tag this card (same rule as the work-context half,
        // ADR-0019).
        let variant = handles
            .sessions
            .store
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.variant.clone());
        // Keep the external message visible: the notification card is updated in
        // place, so its preview would otherwise vanish when the reply renders.
        // Keyed just before the turn's anchor so the reply's parts — whose
        // server times are at or after it — always insert BELOW the preview.
        let anchor_text = (!preview.is_empty()).then(|| format!("👤 {}", preview));
        // This render starts a NEW external run: a `/stop` from before it is
        // not this run's ending — the same rule a fresh Turn applies to the
        // sticky marker (ADR-0043). A stop landing after this point ends the
        // render promptly as ⏹ 已停止 (the loop's own check).
        handles.waits.stopped_sessions.lock().await.remove(session_id);
        Turn::arm_external_render(
            &handles.cards,
            session_id,
            card_id,
            anchor,
            &subtitle,
            &session_dir,
            variant,
            anchor_text.as_deref(),
        )
        .await;
        tracing::info!("external reply render armed for session {}", session_id);

        let handles = handles.clone();
        let sid = session_id.to_string();
        let anchor = anchor.clone();
        let poll_ms = self.render_poll_ms.load(std::sync::atomic::Ordering::Relaxed);
        let read_timeout_ms = self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        let idle_timeout_ms = self
            .render_idle_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        // A spawn inherits no span, so the render loop is instrumented
        // explicitly with this Session's `external` span (ADR-0048) — rooted, so
        // its lines do not repeat the ambient chain of whoever armed it.
        let span = crate::bridge::span::external(session_id, session_thread_key.as_ref());
        tokio::spawn(
            async move {
                external_render_loop(&handles, sid, anchor, poll_ms, read_timeout_ms, idle_timeout_ms).await;
            }
            .instrument(span),
        );
    }

    /// ADR-0028 busy-adopt follow: the adopted session's in-flight EXTERNAL
    /// turn streams into the snapshot card instead of freezing a static
    /// "运行中" line. The snapshot card is the host (the session's
    /// `CardSession` accumulator, like `start_reply_render`): its message id
    /// is the live card, the static 已接管 prefix + tail ride as acc text (the
    /// same representation choice as the 👤 preview), and the adopt-time
    /// pending blocks are pre-seeded as inline sections so a permission
    /// approved from the snapshot resumes the run inside the SAME card. The
    /// renderer exits when a cola prompt or a newer external message replaces
    /// the accumulator (the existing `external_render_loop` guards).
    pub(crate) async fn start_snapshot_follow(
        &self,
        handles: &FlowHandles,
        session_id: &str,
        card_id: &str,
        verb: &str,
        title: &str,
        data: &crate::bridge::snapshot::SnapshotData,
    ) -> bool {
        // Race (ADR-0028): busy at gather but idle by now → the turn already
        // finished; keep the static snapshot and never arm a renderer.
        let busy_now = matches!(
            handles
                .backend
                .session_status(session_id, Some(&data.directory))
                .await,
            Ok(Some(crate::opencode::types::SessionStatus::Busy))
        );
        if !busy_now {
            tracing::info!(
                "snapshot follow: session {} no longer busy; keeping static snapshot",
                session_id
            );
            return false;
        }
        let Some(anchor) = data.newest_user_anchor.clone() else {
            tracing::warn!(
                "snapshot follow: session {} busy but has no user message to follow",
                session_id
            );
            return false;
        };
        // The follow is scoped to EXTERNAL turns (ADR-0028): the busy run
        // answers the newest user message, so a cola-authored newest means the
        // in-flight turn is cola's OWN (this thread, another thread, or a
        // re-/switch mid-answer) — keep the static snapshot and never touch
        // cola's live accumulator.
        if data.newest_user_is_cola_authored {
            tracing::info!(
                "snapshot follow: session {} busy on a cola-authored turn; keeping static snapshot",
                session_id
            );
            return false;
        }
        // Guard: a renderer for THIS turn is already armed (the accumulator
        // still carries its full anchor — id + server time; two messages can
        // share a millisecond). Re-point it at the new card — a re-adopt sent
        // a fresh snapshot mid-turn — so one renderer keeps one live card,
        // and never double-render.
        if armed_turn_anchor(&handles.cards, session_id).await.as_ref() == Some(&anchor) {
            Turn::repoint_card(&handles.cards, session_id, card_id).await;
            tracing::info!(
                "snapshot follow: re-pointing existing renderer at card {}",
                card_id
            );
            return true;
        }
        let (session_dir, session_thread_key) = {
            let store = handles.sessions.store.lock().await;
            match store.entry_for_session(session_id) {
                Some(e) => (e.directory.clone(), Some(e.thread_key.clone())),
                None => (String::new(), None),
            }
        };
        let variant = handles
            .sessions
            .store
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.variant.clone());
        // The snapshot's identity rides as static text (same choice as the
        // 👤 preview in `start_reply_render`): the header verb 已接管 stays
        // visible while the turn streams, and the 最近对话 tail keeps the
        // verbatim context the static layout showed (spec: tail verbatim).
        // `push_text` chunks long text, and the card splitter breaks overflow
        // into continuation cards, so no truncation happens here.
        let mut static_text = format!(
            "已{verb} {}（正在继续该会话的回合，有新进展会自动更新）",
            crate::feishu::snapshot_card::display_title(title, session_id)
        );
        if !data.tail.is_empty() {
            static_text.push_str("\n\n**最近对话**");
            for entry in &data.tail {
                let role = crate::feishu::snapshot_card::role_marker(&entry.role);
                let text = if entry.text.trim().is_empty() {
                    "（空消息）".to_string()
                } else {
                    entry.text.clone()
                };
                static_text.push_str(&format!("\n{role} {text}"));
            }
        }
        // A NEW external run (the adoption follows it): a `/stop` from before
        // it is not this run's ending, same as the reply renderer's arm. A
        // stop landing after this point ends the follow promptly as
        // ⏹ 已停止 (the loop's own check).
        handles.waits.stopped_sessions.lock().await.remove(session_id);
        Turn::arm_external_render(
            &handles.cards,
            session_id,
            card_id,
            &anchor,
            "",
            &session_dir,
            variant,
            Some(&static_text),
        )
        .await;
        // The adopt-time pending blocks ride as inline sections: the poll's
        // inline dedupe (the block is already present on the accumulator)
        // prevents a duplicate, and clicking one takes the normal inline path —
        // resolved sections are stripped and the run resumes into the same card.
        for req in &data.pending {
            let flow = handles.requests.flow_for(req.claim_kind());
            flow.add_initial_inline(&handles.cards, session_id, req, &data.directory)
                .await;
            // Remember the full request (like the static claim path): the poll
            // loop never sees follow-hosted requests, so `prepare()` never runs
            // for them and the block's buttons would resolve to nothing.
            flow.remember_surfaced(req, &data.directory).await;
        }
        tracing::info!("snapshot follow armed for session {}", session_id);

        let handles = handles.clone();
        let sid = session_id.to_string();
        let poll_ms = self.render_poll_ms.load(std::sync::atomic::Ordering::Relaxed);
        let read_timeout_ms = self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        let idle_timeout_ms = self
            .render_idle_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        // The follow's render loop is its own task, so it is instrumented
        // explicitly with the adopted Session's `external` span (ADR-0048),
        // rooted like the plain reply renderer's.
        let span = crate::bridge::span::external(session_id, session_thread_key.as_ref());
        tokio::spawn(
            async move {
                external_render_loop(&handles, sid, anchor, poll_ms, read_timeout_ms, idle_timeout_ms).await;
            }
            .instrument(span),
        );
        true
    }

    /// The card subtitle a continuation carries: the server's live session
    /// title (ADR-0007) plus the id tail, or the bare id tail when the server
    /// has no real title yet. Bounded like every other poll read, so a hung
    /// server degrades the subtitle instead of the send.
    async fn session_subtitle(&self, handles: &FlowHandles, session_id: &str, session_dir: &str) -> String {
        let title = crate::bridge::bounded_call(
            "external render session_info",
            self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            handles
                .backend
                .clone()
                .for_directory(session_dir)
                .session_info(session_id),
        )
        .await
        .and_then(|r| r.ok())
        .and_then(|i| i.title)
        .unwrap_or_default();
        let clean = crate::feishu::card::clean_session_label(&title);
        let id_tail: String = session_id
            .strip_prefix("ses_")
            .unwrap_or(session_id)
            .chars()
            .take(7)
            .collect();
        if clean.is_empty() {
            id_tail
        } else {
            format!("{} · {}", clean, id_tail)
        }
    }

    /// Render a Wake's resumed work as a Card Chain continuation (ADR-0059) —
    /// the Session Sync observation with no user message behind it. The caller
    /// has already scoped it to the thread's Active Session whose newest user
    /// message is a Cola-Authored Message; this owns "is anything owed", "where
    /// does the card go" and "what does it reply to", then arms the
    /// continuation and spawns its settle loop.
    ///
    /// A chain that exists is continued by a SPLIT of that chain (the same
    /// handoff ADR-0043 defined for supplements): the previous card is
    /// finalized, the flush sends the new continuation card — the
    /// notification — replied to the Turn's own reply target, and only content
    /// the chain had not rendered lands on it. With no chain (a cola restart
    /// happened while the Wake was pending) a fresh card is armed, scoped at
    /// the newest Wake, and sent to the thread's reachable anchor — the Turn is
    /// never replayed onto it. Both paths are independent of any in-memory
    /// accumulator from before: the card-exists case diffs the chain's own
    /// rendered state, the restart case scopes by the Wake's server time.
    ///
    /// `now_ms` is this pass's clock, carried to the split's ledger handover so
    /// one read stamps one moment (ADR-0060).
    #[allow(clippy::too_many_arguments)] // the pass's read: handles + transcript + session/thread/directory + anchor + clock
    async fn render_wake_continuation(
        &self,
        handles: &FlowHandles,
        transcript: &SessionTranscript,
        sid: &str,
        thread_key: &crate::config::ThreadKey,
        directory: &str,
        turn_anchor: &TurnAnchor,
        now_ms: i64,
    ) {
        // A live Turn/follow/renderer owns the session: it renders (or will
        // render) whatever arrives — never double-render into a second card.
        if Turn::chain_ownership(&handles.cards, &handles.waits, sid)
            .await
            .is_some()
        {
            return;
        }
        let Some(continuation) = Turn::wake_continuation(&handles.cards, sid, transcript, turn_anchor).await
        else {
            return;
        };
        // The Wake starts new work: a `/stop` from before it is not this run's
        // ending — the same rule a fresh Turn applies to the sticky marker
        // (ADR-0043). A stop landing after this point ends the continuation
        // promptly as ⏹ 已停止.
        handles.waits.stopped_sessions.lock().await.remove(sid);

        match continuation {
            WakeContinuation::ResumeInPlace { wake_id } => {
                // ADR-0066, the one-card-per-request handoff: the yielded card
                // resumes in place. Nothing is sent and nothing is replied to,
                // so this path needs no Feishu reply target — which is exactly
                // what closes the lobby/restart gap where a split had nowhere
                // to go and the work never rendered.
                if !Turn::resume_yielded_card(&handles.cards, sid, &wake_id, transcript, now_ms).await {
                    return;
                }
                let (Some(anchor), Some(chain)) = (
                    Turn::armed_turn_anchor(&handles.cards, sid).await,
                    Turn::chain_id(&handles.cards, sid).await,
                ) else {
                    return;
                };
                tracing::info!(
                    "wake continuation: session {} resumes its yielded card in place",
                    sid
                );
                // The resumed run ends on the REQUEST's own card, which never
                // sent a notification: its true end notifies under the
                // ordinary rules (ADR-0066), unlike a split continuation —
                // whose card send was its own notification — so this arm
                // carries the notice rules the loop hands back to.
                self.spawn_wake_render(
                    handles,
                    sid,
                    thread_key,
                    anchor,
                    directory,
                    chain,
                    Some(self.notice.clone()),
                );
            }
            WakeContinuation::ContinueChain { line } => {
                // The Feishu reply target: the Turn's own when the chain still
                // knows it (the user's message the exchange continues from),
                // else an in-topic anchor — the external path's fallback order.
                // `None` means only a top-level send can reach the thread,
                // which a split cannot do.
                let reply_target = self.wake_reply_target(handles, sid, thread_key).await;
                // The split carries the continuation: its flush re-stamps the
                // previous card, sends the new card and tracks it. The loop's
                // guard facts are read AFTER the split, so they describe the
                // chain the continuation actually lives on.
                let Some(reply_to) = reply_target else {
                    tracing::warn!(
                        "wake continuation: session {} has a chain but no reachable reply target",
                        sid
                    );
                    return;
                };
                // The ledger handover (ADR-0060) rides the split: writing it,
                // queueing the split and flushing are one write-lock-held
                // sequence inside `split_chain_for_wake`, so the outgoing
                // card's finalize PATCH carries the read's remaining list and
                // the retiring Wakes' entries, and the continuation — whose
                // slice starts after both — opens with only its 承接 line and
                // the remaining list.
                if !Turn::split_chain_for_wake(&handles.cards, sid, &reply_to, line, transcript, now_ms).await
                {
                    return;
                }
                let (Some(anchor), Some(chain)) = (
                    Turn::armed_turn_anchor(&handles.cards, sid).await,
                    Turn::chain_id(&handles.cards, sid).await,
                ) else {
                    return;
                };
                tracing::info!("wake continuation: session {} continues its card chain", sid);
                self.spawn_wake_render(handles, sid, thread_key, anchor, directory, chain, None);
            }
            WakeContinuation::Fresh { anchor } => {
                // The Feishu reply target, the split path's own fallback order:
                // a restart leaves no reply target behind, and only a top-level
                // send can reach the thread then (`send_card` below).
                let reply_target = self.wake_reply_target(handles, sid, thread_key).await;
                // No chain (a cola restart): arm a fresh card scoped at the
                // newest Wake, so the lost card's content is never replayed.
                //
                // #424: the read that decided this predates any message that
                // arrived since, and a received message is invisible to the
                // transcript until its Turn admits it — so the in-process
                // claim is the check that closes the observed race, and the
                // re-read only corroborates it for messages another
                // shared-store client wrote.
                if handles.waits.inbound_pending(sid).await
                    || handles.waits.inflight.lock().await.contains(sid)
                {
                    return;
                }
                let Some(Ok(fresh)) = crate::bridge::bounded_call(
                    "wake continuation recheck",
                    self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
                    handles.backend.transcript(sid),
                )
                .await
                else {
                    return;
                };
                if fresh.newest_user().map(|message| message.id.as_str())
                    != Some(turn_anchor.message_id.as_str())
                {
                    return;
                }
                let subtitle = self.session_subtitle(handles, sid, directory).await;
                let variant = handles
                    .sessions
                    .store
                    .lock()
                    .await
                    .entry_for_session(sid)
                    .and_then(|e| e.variant.clone());
                let Some(card) = Turn::arm_wake_continuation(
                    &handles.cards,
                    sid,
                    &anchor,
                    ContinuationFacts {
                        reply_to: reply_target.as_deref(),
                        chat_id: &thread_key.chat_id,
                        subtitle: &subtitle,
                        directory,
                        variant,
                    },
                )
                .await
                else {
                    return;
                };
                let sent = match &reply_target {
                    Some(target) => handles.platform.reply_card(target, &card).await,
                    None => {
                        handles
                            .platform
                            .send_card("chat_id", &thread_key.chat_id, &card)
                            .await
                    }
                };
                match sent {
                    Ok(card_id) => {
                        // The continuation takes the chain over: attach its
                        // identity first, then collect whatever card a previous
                        // life left recorded and re-point the durable record at
                        // the continuation (ADR-0063) — a chain handover never
                        // leaves the old card looking live.
                        Turn::take_over_card(&handles.cards, sid, &card_id, Some(directory)).await;
                        // The card carried the 承接 line: the Wake is now
                        // user-visible, so the durable Wake Watermark advances
                        // (ADR-0061).
                        handles
                            .cards
                            .chains
                            .advance(sid, anchor.message_id.as_str(), anchor.created_ms);
                        if handles.waits.inbound_pending(sid).await
                            || handles.waits.inflight.lock().await.contains(sid)
                        {
                            // The accepted residual window: the claim landed
                            // between the pre-send check and the send. Logged,
                            // never recalled — a destructive API for a
                            // sub-second window is worse than the rare stray
                            // card.
                            tracing::warn!(
                                "wake continuation: session {sid} posted while an inbound message was being admitted"
                            );
                        }
                        let Some(chain) = Turn::chain_id(&handles.cards, sid).await else {
                            return;
                        };
                        tracing::info!("wake continuation: session {} continues after a restart", sid);
                        self.spawn_wake_render(handles, sid, thread_key, anchor, directory, chain, None);
                    }
                    Err(e) => {
                        tracing::warn!("wake continuation send: {}", e);
                        Turn::drop_armed_card(&handles.cards, sid, &anchor).await;
                    }
                }
            }
        }
    }

    /// The Feishu message a Wake continuation card replies to: the Turn's own
    /// when the chain still knows it (the user's message the exchange continues
    /// from), else an in-topic anchor — the external path's fallback order.
    /// `None` means only a top-level send can reach the thread, which a split
    /// cannot do: the Fresh path sends top-level instead, and the in-place
    /// resume (ADR-0066) PATCHes the card the chain already has, so it asks for
    /// no target at all.
    async fn wake_reply_target(
        &self,
        handles: &FlowHandles,
        sid: &str,
        thread_key: &crate::config::ThreadKey,
    ) -> Option<String> {
        match Turn::reply_target(&handles.cards, sid).await {
            Some(target) => Some(target),
            None => {
                crate::bridge::pollers::resolve_topic_anchor(&handles.sessions, &handles.platform, thread_key)
                    .await
            }
        }
    }

    /// Spawn the Wake continuation's out-of-turn settle loop with this flow's
    /// injectable cadences: the render poll, the per-read bound, and the
    /// lost-contact grace (the external render timeout's second reading).
    /// `notice` carries the Completion Notice's rules for a continuation that
    /// must announce its own ending — the in-place resume, whose card never
    /// sent (ADR-0066) — and `None` for a split continuation, whose card send
    /// was the notification.
    #[allow(clippy::too_many_arguments)] // the loop's fixture: the pass's facts + the chain it watches + the notice rules
    fn spawn_wake_render(
        &self,
        handles: &FlowHandles,
        sid: &str,
        thread_key: &crate::config::ThreadKey,
        anchor: TurnAnchor,
        directory: &str,
        chain: u64,
        notice: Option<NoticeRules>,
    ) {
        let flow = handles.clone();
        let session_id = sid.to_string();
        let directory = directory.to_string();
        let timing = SettleTiming {
            poll_ms: self.render_poll_ms.load(std::sync::atomic::Ordering::Relaxed),
            read_timeout_ms: self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            grace_ms: self
                .wake_settle_grace_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        };
        // A spawn inherits no span: instrument the loop with this Session's
        // `external` span (ADR-0048), rooted like the other render loops'.
        let span = crate::bridge::span::external(sid, Some(thread_key));
        tokio::spawn(
            async move {
                let ended =
                    Turn::wake_settle_loop(&flow, &session_id, &directory, &anchor, chain, timing).await;
                let Some(rules) = notice else { return };
                if !ended {
                    // The loop stopped owning the card (a new Turn, another
                    // arm) or its accumulator vanished: nothing ended here, so
                    // nothing is announced.
                    return;
                }
                // The in-place resume's ending is the request's true end: it
                // notifies under the ordinary rules, with the clock the quiet
                // true end reads (ADR-0066). `send_completion_notice` itself
                // declines a card that is not at an ending, so a yield back to
                // 「⏳」 stays silent and the quiet true end that follows owns
                // the one notice.
                let Some(started_at) = Turn::turn_started_at(&flow.cards, &session_id).await else {
                    return;
                };
                crate::bridge::turn::send_completion_notice(
                    &flow.cards,
                    &flow.platform,
                    &rules,
                    &session_id,
                    started_at,
                )
                .await;
            }
            .instrument(span),
        );
    }
}

/// The full anchor of the session's armed renderer, if one is armed: the
/// renderer identity both external arming paths compare against, so a
/// duplicate arm is a no-op and a different turn replaces it. The identity is
/// the message id together with its server time — a time-only comparison
/// would confuse two messages that share a millisecond.
async fn armed_turn_anchor(cards: &CardsHandle, session_id: &str) -> Option<TurnAnchor> {
    Turn::armed_turn_anchor(cards, session_id).await
}

/// ADR-0028: settle a snapshot card right after it was sent — arm the
/// busy-adopt follow when the adopted session is mid-turn (the in-flight
/// external run streams into the snapshot), otherwise keep the static
/// snapshot and claim its embedded pendings. A follow that declines to arm
/// (the busy→idle race: the turn already finished) falls back to the static
/// claim path, so the embedded pendings are never left unclaimed. Shared by
/// every adoption surface so the busy/static decision cannot drift between
/// them. The settle runs inside the adopted Session's `snapshot` span
/// (ADR-0048), so its follow/claim lines are retrievable by it; the follow's
/// own render task is instrumented at its spawn with the `external` span.
/// `back` is the `/switch`-list state the card was adopted from, when it came
/// from the list card (ADR-0052): the claim path keeps its 返回列表 button
/// across rebuilds.
pub(crate) async fn settle_snapshot_after_send(
    external: &ExternalFlow,
    handles: &FlowHandles,
    snapshot_message_id: &str,
    verb: &str,
    title: &str,
    data: &crate::bridge::snapshot::SnapshotData,
    back: Option<&crate::feishu::card::session::BackToList>,
) {
    let thread_key = crate::bridge::span::thread_key_of(&handles.sessions, &data.session_id).await;
    let span = crate::bridge::span::snapshot(&data.session_id, thread_key.as_ref());
    async {
        let followed = data.status == Some(crate::opencode::types::SessionStatus::Busy)
            && external
                .start_snapshot_follow(handles, &data.session_id, snapshot_message_id, verb, title, data)
                .await;
        if !followed {
            crate::bridge::snapshot_claims::claim_snapshot_pendings(
                &handles.requests,
                snapshot_message_id,
                verb,
                title,
                data,
                back,
            )
            .await;
        }
    }
    .instrument(span)
    .await;
}

/// Incremental renderer for an external message's reply: poll the session,
/// stream reasoning/tool/text into the notification card, then finalize it as
/// Done when the model finishes — or ⏹ 已停止 when a deliberate `/stop` lands
/// on the run it renders (#394). Exits when the turn completes, the accumulator
/// was replaced (cola's own prompt or a newer external message), a newer
/// EXTERNAL user message starts a new turn, or the idle bound elapses with no
/// observable progress (#457) — a cola-authored Supplement merges into the
/// run this loop is already streaming (#451).
async fn external_render_loop(
    handles: &FlowHandles,
    session_id: String,
    anchor: TurnAnchor,
    poll_ms: u64,
    read_timeout_ms: u64,
    idle_timeout_ms: u64,
) {
    // The IDLE bound (#457): every poll whose render made observable progress
    // resets the clock, so a running model that keeps producing is never cut
    // at 10 minutes; only a run whose card shows nothing new for the whole
    // window gives up. A failed or timed-out read is no progress either, so a
    // wedged Backend idles out too; the header's per-second tick and other
    // footer churn are not progress — reading them as such would make the
    // bound never fire.
    let idle_bound = tokio::time::Duration::from_millis(idle_timeout_ms);
    let mut last_progress = tokio::time::Instant::now();
    let mut last_mark = Turn::progress_mark(&handles.cards, &session_id)
        .await
        .unwrap_or(0);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(poll_ms)).await;
        // Completion, the newer-turn boundary and the streaming render all
        // come from the one Session Transcript read (ADR-0053). The read is
        // BOUNDED: a half-open connection must not park the loop past its
        // idle bound — a timed-out read is just another failed one.
        let read = crate::bridge::bounded_call(
            "external render transcript",
            read_timeout_ms,
            handles.backend.transcript(&session_id),
        )
        .await;
        let transcript = match read {
            Some(Ok(transcript)) => Some(transcript),
            Some(Err(e)) => {
                tracing::warn!("external render poll transcript: {}", e);
                None
            }
            // `bounded_call` already logged the timeout.
            None => None,
        };
        let Some(transcript) = transcript else {
            // A failed or timed-out read is the "nothing observable is
            // happening" state: the stalled-poll rule owns what happens next
            // (a replacement ends this renderer silently; otherwise the idle
            // bound may end the card).
            if !stalled_poll(handles, &session_id, &anchor, last_progress, idle_bound).await {
                break;
            }
            continue;
        };
        // The accumulator was replaced (cola's own `run_prompt` inserted a fresh
        // one, or a newer external message's renderer took over): exit so this
        // turn isn't double-rendered into two cards. The full anchor is the
        // identity — a same-millisecond message from another turn must not
        // pass as this one.
        let replaced = Turn::armed_turn_anchor(&handles.cards, &session_id)
            .await
            .as_ref()
            != Some(&anchor);
        if replaced {
            break;
        }
        // A deliberate `/stop` on this run owns the ending (#394): the render
        // below first reconciles the abort's settled tool states, then the
        // card stamps ⏹ 已停止 — never the ✅ at the bottom of the tick. The
        // arm cleared any earlier marker, so only a stop landing on THIS run
        // lands here.
        let stopped = handles.waits.is_stopped(&session_id).await;
        // Stream the reply's reasoning/tools/text into the notification card.
        // The WHOLE pass is bounded (#457): the liveness gather reads child
        // transcripts and the footer refresh reads the server, so a wedged
        // read anywhere inside must not park the loop past its idle bound —
        // an abandoned pass is just another no-progress poll.
        let rendered = tokio::time::timeout(
            std::time::Duration::from_millis(read_timeout_ms),
            Turn::render_and_flush(
                &handles.cards,
                &handles.sessions,
                &handles.backend,
                &handles.requests,
                &session_id,
                &transcript,
            ),
        )
        .await;
        let stats = match rendered {
            Ok(Some(stats)) => Some(stats),
            // The accumulator vanished: nothing left to render.
            Ok(None) => break,
            Err(_) => {
                tracing::warn!("external render: pass timed out after {} ms", read_timeout_ms);
                None
            }
        };
        // Renew on the accumulator's progress mark (#457): it advances the
        // moment a stage renders, so content a pass had already rendered
        // before its timeout abandoned it still counts — a cancelled pass can
        // never return its stats.
        if let Some(mark) = Turn::progress_mark(&handles.cards, &session_id).await
            && mark != last_mark
        {
            last_mark = mark;
            last_progress = tokio::time::Instant::now();
        }
        let Some(stats) = stats else {
            if !stalled_poll(handles, &session_id, &anchor, last_progress, idle_bound).await {
                break;
            }
            continue;
        };
        if stats.new_parts > 0 {
            tracing::info!("external render: session {} gained parts", session_id);
        }
        if stopped {
            if Turn::finalize_stopped_if_anchor(&handles.cards, &session_id, &anchor).await {
                tracing::info!("external reply render stopped: session {}", session_id);
            }
            break;
        }
        // The model finished answering this turn: the transcript's turn
        // projection says so — finalize the card, then stop. The guarded
        // finalize re-checks the anchor under the stamp's own lock, so a
        // successor that replaced the accumulator during the render keeps its
        // own card (#457).
        if transcript.turn_for_user(&anchor).complete {
            if Turn::finalize_done_if_anchor(&handles.cards, &session_id, &anchor).await {
                tracing::info!("external reply rendered: session {} done", session_id);
            }
            break;
        }
        // A NEWER EXTERNAL user message is a turn boundary — the poller
        // notifies and arms a fresh renderer for it (ADR-0028). A message cola
        // authored itself is NOT: a Supplement merges into THIS run and keeps
        // streaming into this card (#451), while a genuine new Turn replaces
        // the accumulator and is already caught by the `replaced` guard above.
        // Both this boundary and the poller key on the session's newest user
        // message, so the two agree on which message is the boundary. The
        // boundary stays time-based (the pre-migration rule: strictly greater
        // server time wins) because a same-millisecond message cannot be
        // ordered by server time; the armed-renderer guards above use
        // full-anchor identity instead.
        let newer_turn = transcript
            .newest_user()
            .filter(|message| !crate::opencode::parsing::is_cola_message_id(message.id.as_str()))
            .and_then(|message| message.anchor())
            .is_some_and(|newest| newest.created_ms > anchor.created_ms);
        if newer_turn {
            break;
        }
        // Safety net for messages that never trigger a run, and for a run that
        // goes quiet: if a partial reply was rendered, finalize it so the card
        // never sits on an eternal spinner; otherwise leave the "有新消息"
        // notification as-is.
        if last_progress.elapsed() >= idle_bound {
            idle_bound_reached(handles, &session_id, &anchor).await;
            break;
        }
    }
}

/// A stalled poll's disposition (#457): `true` keeps the loop polling, `false`
/// stops it. A successor that replaced this renderer's accumulator ends it
/// silently — stamping by session id could hit the successor's live card. The
/// idle bound (checked second, so a replacement always wins) ends the card
/// through the anchor-guarded ending.
async fn stalled_poll(
    handles: &FlowHandles,
    session_id: &str,
    anchor: &TurnAnchor,
    last_progress: tokio::time::Instant,
    idle_bound: tokio::time::Duration,
) -> bool {
    if Turn::armed_turn_anchor(&handles.cards, session_id).await.as_ref() != Some(anchor) {
        return false;
    }
    if last_progress.elapsed() >= idle_bound {
        idle_bound_reached(handles, session_id, anchor).await;
        return false;
    }
    true
}

/// The idle bound's ending (#457): a card that rendered partial content is
/// finalized Done so it never sits on an eternal spinner; a card that never
/// rendered anything is left as the 有新消息 notification untouched. Shared by
/// the no-progress tick and the stalled-read arms; the Done stamp is
/// anchor-guarded, so a stale renderer can never finalize a successor.
async fn idle_bound_reached(handles: &FlowHandles, session_id: &str, anchor: &TurnAnchor) {
    let has_content = Turn::has_rendered_content(&handles.cards, session_id).await;
    if has_content {
        if Turn::finalize_done_if_anchor(&handles.cards, session_id, anchor).await {
            tracing::info!(
                "external reply render: idle bound reached; finalized session {}",
                session_id
            );
        }
    } else {
        tracing::info!(
            "external reply render: idle bound reached with no content; notification stays for session {}",
            session_id
        );
    }
}

/// Preview of the External Message for the notification card: every user
/// message the backend reported at the anchor's server time (typically one),
/// its text and reasoning parts concatenated verbatim with no separator, then
/// capped at 80 characters. This mirrors the pre-transcript preview, which
/// folded every `text` string of the newest-epoch user messages.
///
/// A part kind this build does not model ([`Part::Other`]) is not folded in:
/// that would mean indexing raw protocol fields, which the read model exists to
/// prevent. Such a part carrying top-level `text` does not occur in user
/// messages in practice.
fn message_preview(transcript: &SessionTranscript, anchor: &TurnAnchor) -> String {
    transcript
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .filter(|message| message.time.is_some_and(|time| time.created == anchor.created_ms))
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| match part {
            Part::Text(text) => Some(text.text.as_str()),
            Part::Reasoning(reasoning) => Some(reasoning.text.as_str()),
            _ => None,
        })
        .collect::<String>()
        .chars()
        .take(80)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MessageRole, Part, ReasoningPart, SessionTranscript, TranscriptMessage};
    use crate::bridge::test_support::{text_part, turn_anchor, typed_message};

    fn user(id: &str, created: i64, parts: Vec<Part>) -> TranscriptMessage {
        typed_message(id, MessageRole::User, Some(created), parts)
    }

    #[test]
    fn preview_joins_the_anchors_user_text_with_no_separator() {
        let anchor = turn_anchor(1_000);
        let transcript = SessionTranscript::new(vec![
            user("msg_u1", 1_000, vec![text_part("第一段"), text_part("第二段")]),
            // A same-epoch user message is folded in too (the old preview
            // gathered every user message created at the newest epoch).
            user("msg_u1b", 1_000, vec![text_part("第三段")]),
            // A different turn's message is not part of this preview.
            user("msg_other", 2_000, vec![text_part("别的回合")]),
        ]);
        assert_eq!(message_preview(&transcript, &anchor), "第一段第二段第三段");
    }

    #[test]
    fn preview_folds_text_and_reasoning_and_caps_at_80_chars() {
        let anchor = turn_anchor(1_000);
        let transcript = SessionTranscript::new(vec![user(
            "msg_u1",
            1_000,
            vec![
                text_part("问题"),
                Part::Reasoning(ReasoningPart {
                    text: "想想".into(),
                    started_at: None,
                }),
            ],
        )]);
        assert_eq!(message_preview(&transcript, &anchor), "问题想想");

        let long = "很长的内容".repeat(30);
        let transcript = SessionTranscript::new(vec![user("msg_u1", 1_000, vec![text_part(&long)])]);
        let preview = message_preview(&transcript, &anchor);
        assert_eq!(preview.chars().count(), 80, "preview must cap at 80 chars");
        assert_eq!(preview, long.chars().take(80).collect::<String>());
    }
}
