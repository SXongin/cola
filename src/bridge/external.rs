use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::Instrument;

use crate::backend::{TranscriptMessage, TurnAnchor};
use crate::bridge::handles::{CardsHandle, FlowHandles};
use crate::bridge::turn::Turn;

/// The external-message flow: watches for user messages that were NOT sent by
/// cola (someone posted from OpenChamber or another client on the shared store)
/// and notifies the Feishu side with a small card. cola's own messages are
/// excluded by their self-identifying `msg_cola_` id (ADR-0026), never by a
/// timestamp baseline — authorship lives on the message, so a server crash
/// mid-turn cannot make cola's own round look external afterwards.
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
}

impl ExternalFlow {
    pub fn new() -> Self {
        Self {
            last_user_msg_epoch: Arc::new(Mutex::new(HashMap::new())),
            poll_interval_ms: std::sync::atomic::AtomicU64::new(8_000),
            request_timeout_ms: std::sync::atomic::AtomicU64::new(30_000),
            render_poll_ms: std::sync::atomic::AtomicU64::new(1_500),
            render_timeout_ms: std::sync::atomic::AtomicU64::new(600_000),
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
        for (sid, thread_key, directory) in sessions {
            // One `external` span per Session (ADR-0048): the observation,
            // the notification and the renderer it arms are all retrievable
            // by `rg 'session=ses_x'`.
            let span = crate::bridge::span::external(&sid, Some(&thread_key));
            self.poll_session(handles, &active, &sid, &thread_key, &directory)
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
            let preview = message_preview(newest);
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
        // still carries the message's server time as its turn anchor). cola's
        // own prompts get a fresh accumulator, so a different anchor is NOT
        // this message — a new renderer replaces the old one (whose
        // `turn_started_ms` no longer matches, so it exits).
        if armed_turn_anchor(&handles.cards, session_id).await == Some(anchor.created_ms) {
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
        let title = crate::bridge::bounded_call(
            "external render session_info",
            self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            handles
                .backend
                .clone()
                .for_directory(session_dir.as_str())
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
        let subtitle = if clean.is_empty() {
            id_tail
        } else {
            format!("{} · {}", clean, id_tail)
        };

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
        Turn::arm_external_render(
            &handles.cards,
            session_id,
            card_id,
            anchor.created_ms,
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
        let turn_anchor_ms = anchor.created_ms;
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
        // still carries its server-time anchor). Re-point it at the new card —
        // a re-adopt sent a fresh snapshot mid-turn — so one renderer keeps
        // one live card, and never double-render.
        if armed_turn_anchor(&handles.cards, session_id).await == Some(turn_anchor_ms) {
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
        Turn::arm_external_render(
            &handles.cards,
            session_id,
            card_id,
            turn_anchor_ms,
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
}

/// The turn anchor of the session's armed renderer, if one is armed: the
/// renderer identity both external arming paths compare their own turn's
/// server time against, so a duplicate arm is a no-op and a different turn
/// replaces it.
async fn armed_turn_anchor(cards: &CardsHandle, session_id: &str) -> Option<i64> {
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
/// Done when the model finishes. Exits when the turn completes, the accumulator
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
        // Completion and the newer-turn boundary come from the Session
        // Transcript's shared projections (ADR-0053); the raw read still feeds
        // the streaming renderer while the Turn module migrates (#336).
        let transcript = match handles.backend.transcript(&session_id).await {
            Ok(transcript) => transcript,
            Err(e) => {
                tracing::warn!("external render poll transcript: {}", e);
                continue;
            }
        };
        let msgs = match handles.backend.messages(&session_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("external render poll messages: {}", e);
                continue;
            }
        };
        // The accumulator was replaced (cola's own `run_prompt` inserted a fresh
        // one, or a newer external message's renderer took over): exit so this
        // turn isn't double-rendered into two cards.
        let replaced = Turn::armed_turn_anchor(&handles.cards, &session_id).await != Some(anchor.created_ms);
        if replaced {
            break;
        }
        // Stream the reply's reasoning/tools/text into the notification card.
        let Some((new_parts, _, _)) = Turn::render_and_flush(
            &handles.cards,
            &handles.sessions,
            &handles.backend,
            &session_id,
            &msgs,
        )
        .await
        else {
            break;
        };
        if new_parts > 0 {
            tracing::info!("external render: session {} gained parts", session_id);
        }
        // The model finished answering this turn: the transcript's turn
        // projection says so — finalize the card, then stop.
        if transcript.turn_for_user(&anchor).complete {
            finalize_done(&handles.cards, &session_id).await;
            tracing::info!("external reply rendered: session {} done", session_id);
            break;
        }
        // A NEWER user message is a turn boundary — the poller notifies and arms
        // a fresh renderer for it.
        let newer_turn = transcript
            .newest_user()
            .and_then(|message| message.time.map(|time| time.created))
            .is_some_and(|created| created > anchor.created_ms);
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

/// Preview of a user message for the external-message notification card: the
/// message's conversational text (the Session Transcript's text projection),
/// capped at 80 characters.
fn message_preview(message: &TranscriptMessage) -> String {
    message.text().chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MessageId, MessageRole, MessageTime, Part, TextPart};

    fn typed_message(id: &str, created: i64, texts: &[&str]) -> TranscriptMessage {
        TranscriptMessage {
            id: MessageId::new(id),
            role: MessageRole::User,
            time: Some(MessageTime {
                created,
                completed: Some(created),
            }),
            model: None,
            tokens: None,
            parts: texts
                .iter()
                .map(|text| {
                    Part::Text(TextPart {
                        text: text.to_string(),
                        started_at: None,
                    })
                })
                .collect(),
        }
    }

    #[test]
    fn preview_is_the_message_text_capped_at_80_chars() {
        let message = typed_message("msg_u1", 1000, &["第一段", "第二段"]);
        assert_eq!(message_preview(&message), "第一段\n第二段");

        let long = "很长的内容".repeat(30);
        let message = typed_message("msg_u2", 2000, &[&long]);
        let preview = message_preview(&message);
        assert_eq!(preview.chars().count(), 80, "preview must cap at 80 chars");
        assert_eq!(preview, long.chars().take(80).collect::<String>());
    }
}
