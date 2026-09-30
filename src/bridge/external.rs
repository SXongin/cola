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
    /// How long the external-reply renderer waits for a reply before giving up
    /// (ms). A message posted in OpenChamber may never actually be sent, so the
    /// loop idles and times out; the card then simply stays the "有新消息"
    /// notification. Injected small in tests to exercise the timeout branch.
    pub render_timeout_ms: std::sync::atomic::AtomicU64,
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
            render_timeout_ms: std::sync::atomic::AtomicU64::new(600_000),
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
        for (sid, record) in handles.cards.live_cards.entries() {
            let mapping = mapped.get(&sid);
            let span = crate::bridge::span::external(&sid, mapping.map(|(thread_key, _)| thread_key));
            crate::bridge::reap::reconcile(
                handles,
                &sid,
                mapping.map(|(_, directory)| directory.as_str()),
                &record,
                read_timeout_ms,
            )
            .instrument(span)
            .await;
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
        let Some(Ok(transcript)) = crate::bridge::bounded_call(
            "external poll transcript",
            self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            handles.backend.transcript(sid),
        )
        .await
        else {
            return;
        };
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
            match Turn::refresh_yielded_ledger(&handles.cards, sid, &transcript, now_ms, stopped).await {
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
    /// (or a newer external message) replaces the accumulator, a newer user
    /// message starts a new turn, or a hard timeout elapses.
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
        let timeout_ms = self.render_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        // A spawn inherits no span, so the render loop is instrumented
        // explicitly with this Session's `external` span (ADR-0048) — rooted, so
        // its lines do not repeat the ambient chain of whoever armed it.
        let span = crate::bridge::span::external(session_id, session_thread_key.as_ref());
        tokio::spawn(
            async move {
                external_render_loop(&handles, sid, anchor, poll_ms, timeout_ms).await;
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
        let timeout_ms = self.render_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        // The follow's render loop is its own task, so it is instrumented
        // explicitly with the adopted Session's `external` span (ADR-0048),
        // rooted like the plain reply renderer's.
        let span = crate::bridge::span::external(session_id, session_thread_key.as_ref());
        tokio::spawn(
            async move {
                external_render_loop(&handles, sid, anchor, poll_ms, timeout_ms).await;
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
        // The Feishu reply target: the Turn's own when the chain still knows
        // it (the user's message the exchange continues from), else an in-topic
        // anchor — the external path's fallback order. `None` means only a
        // top-level send can reach the thread, which a split cannot do.
        let reply_target = match Turn::reply_target(&handles.cards, sid).await {
            Some(target) => Some(target),
            None => {
                crate::bridge::pollers::resolve_topic_anchor(&handles.sessions, &handles.platform, thread_key)
                    .await
            }
        };
        // The Wake starts new work: a `/stop` from before it is not this run's
        // ending — the same rule a fresh Turn applies to the sticky marker
        // (ADR-0043). A stop landing after this point ends the continuation
        // promptly as ⏹ 已停止.
        handles.waits.stopped_sessions.lock().await.remove(sid);

        match continuation {
            WakeContinuation::ContinueChain { line } => {
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
                self.spawn_wake_render(handles, sid, thread_key, anchor, directory, chain);
            }
            WakeContinuation::Fresh { anchor } => {
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
                        // The continuation takes the chain over: whatever card
                        // a previous life left recorded is collected as taken
                        // over and the durable record re-points at the
                        // continuation (ADR-0063) — a chain handover never
                        // leaves the old card looking live. This runs BEFORE
                        // the id attach, whose own re-point collects nothing.
                        Turn::track_live_card(&handles.cards, sid, &card_id, true, Some(directory)).await;
                        Turn::set_card_message_id(&handles.cards, sid, &card_id).await;
                        // The card carried the 承接 line: the Wake is now
                        // user-visible, so the durable Wake Watermark advances
                        // (ADR-0061).
                        handles.cards.wake_watermarks.advance(
                            sid,
                            anchor.message_id.as_str(),
                            anchor.created_ms,
                        );
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
                        self.spawn_wake_render(handles, sid, thread_key, anchor, directory, chain);
                    }
                    Err(e) => {
                        tracing::warn!("wake continuation send: {}", e);
                        Turn::drop_armed_card(&handles.cards, sid, &anchor).await;
                    }
                }
            }
        }
    }

    /// Spawn the Wake continuation's out-of-turn settle loop with this flow's
    /// injectable cadences: the render poll, the per-read bound, and the
    /// lost-contact grace (the external render timeout's second reading).
    fn spawn_wake_render(
        &self,
        handles: &FlowHandles,
        sid: &str,
        thread_key: &crate::config::ThreadKey,
        anchor: TurnAnchor,
        directory: &str,
        chain: u64,
    ) {
        let flow = handles.clone();
        let session_id = sid.to_string();
        let directory = directory.to_string();
        let timing = SettleTiming {
            poll_ms: self.render_poll_ms.load(std::sync::atomic::Ordering::Relaxed),
            read_timeout_ms: self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            grace_ms: self.render_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
        };
        // A spawn inherits no span: instrument the loop with this Session's
        // `external` span (ADR-0048), rooted like the other render loops'.
        let span = crate::bridge::span::external(sid, Some(thread_key));
        tokio::spawn(
            async move {
                Turn::wake_settle_loop(&flow, &session_id, &directory, &anchor, chain, timing).await;
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
/// was replaced (cola's own prompt or a newer external message), a newer user
/// message starts a new turn, or the hard timeout elapses.
async fn external_render_loop(
    handles: &FlowHandles,
    session_id: String,
    anchor: TurnAnchor,
    poll_ms: u64,
    timeout_ms: u64,
) {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(poll_ms)).await;
        // Completion, the newer-turn boundary and the streaming render all
        // come from the one Session Transcript read (ADR-0053).
        let transcript = match handles.backend.transcript(&session_id).await {
            Ok(transcript) => transcript,
            Err(e) => {
                tracing::warn!("external render poll transcript: {}", e);
                continue;
            }
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
        let Some((new_parts, _, _)) = Turn::render_and_flush(
            &handles.cards,
            &handles.sessions,
            &handles.backend,
            &handles.requests,
            &session_id,
            &transcript,
        )
        .await
        else {
            break;
        };
        if new_parts > 0 {
            tracing::info!("external render: session {} gained parts", session_id);
        }
        if stopped {
            Turn::finalize_stopped(&handles.cards, &session_id).await;
            tracing::info!("external reply render stopped: session {}", session_id);
            break;
        }
        // The model finished answering this turn: the transcript's turn
        // projection says so — finalize the card, then stop.
        if transcript.turn_for_user(&anchor).complete {
            finalize_done(&handles.cards, &session_id).await;
            tracing::info!("external reply rendered: session {} done", session_id);
            break;
        }
        // A NEWER user message is a turn boundary — the poller notifies and
        // arms a fresh renderer for it. The boundary stays time-based (the
        // pre-migration rule: strictly greater server time wins) because a
        // same-millisecond message cannot be ordered by server time; the
        // armed-renderer guards above use full-anchor identity instead.
        let newer_turn = transcript
            .newest_user()
            .and_then(|message| message.anchor())
            .is_some_and(|newest| newest.created_ms > anchor.created_ms);
        if newer_turn {
            break;
        }
        // Safety net for messages that never trigger a run. If a partial reply
        // was rendered, finalize it so the card never sits on an eternal
        // spinner; otherwise leave the "有新消息" notification as-is.
        if tokio::time::Instant::now() >= deadline {
            let has_content = Turn::has_rendered_content(&handles.cards, &session_id).await;
            if has_content {
                finalize_done(&handles.cards, &session_id).await;
                tracing::info!(
                    "external reply render timed out; finalized session {}",
                    session_id
                );
            }
            break;
        }
    }
}

/// Mark the accumulator's card Done and flush it — the terminal state for a
/// reply that finished (or timed out with content rendered). The work context
/// is refreshed first (ADR-0019), so the final card shows where the turn
/// landed (branch/dirty) rather than only where it started.
async fn finalize_done(cards: &CardsHandle, session_id: &str) {
    Turn::finalize_done(cards, session_id).await;
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
