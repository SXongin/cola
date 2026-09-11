use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::core::SharedCore;
use crate::bridge::render::{flush_card, render_and_flush};
use crate::bridge::streaming::StreamAccumulator;

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
    pub(crate) async fn poll_loop(&self, core: &Arc<SharedCore>) -> crate::error::Result<()> {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(
                self.poll_interval_ms.load(std::sync::atomic::Ordering::Relaxed),
            ))
            .await;
            // Serverless (Lazy Start hasn't attached/spawned yet): nothing to
            // watch on the store — skip quietly.
            if core.opencode.base_url().is_empty() {
                continue;
            }
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
                let store = core.sessions.lock().await;
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
                if !active.contains(&sid) {
                    // Historical (non-active) session: stop syncing it and drop
                    // its Sync Watermark so a later /switch back re-syncs it
                    // silently (first observation, no replay).
                    self.last_user_msg_epoch.lock().await.remove(&sid);
                    continue;
                }
                // While cola is answering this session, any new message is cola's own.
                if core.inflight.lock().await.contains(&sid) {
                    continue;
                }
                let Some(Ok(msgs)) = crate::bridge::bounded_call(
                    "external poll messages",
                    self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
                    core.opencode.messages(&sid),
                )
                .await
                else {
                    continue;
                };
                // The newest user message overall decides this poll. Its author
                // is authoritative (ADR-0026): a `msg_cola_` id means cola sent
                // it — advance the watermark, never notify. Only a message
                // written by another shared-store client and newer than the
                // watermark is an External Message.
                let newest = msgs
                    .iter()
                    .filter(|m| m.info.role.as_deref() == Some("user"))
                    .filter_map(|m| {
                        let id = m.info.id.as_str();
                        m.info.time.as_ref().map(|t| (t.created, id))
                    })
                    .max_by_key(|(created, _)| *created);
                let Some((latest, latest_id)) = newest else {
                    continue;
                };
                let cola_authored = crate::opencode::client::is_cola_message_id(latest_id);
                let mut map = self.last_user_msg_epoch.lock().await;
                let watermark = map.get(&sid).copied();
                if cola_authored {
                    // cola's own message: never notify; just make sure the
                    // watermark covers it so later external messages compare
                    // against it.
                    if watermark.is_none_or(|w| latest > w) {
                        map.insert(sid.clone(), latest);
                    }
                    continue;
                }
                // First observation: establish the watermark, don't notify.
                // External messages received before cola ever polled are marked
                // read, not replayed (ADR-0017).
                let Some(prev) = watermark else {
                    map.insert(sid.clone(), latest);
                    continue;
                };
                if latest > prev {
                    map.insert(sid.clone(), latest);
                    let preview = user_message_preview(&msgs, latest);
                    drop(map);
                    tracing::info!("External message on session {}: {}", sid, preview);
                    // The card title is the server's session title (ADR-0007)
                    // — fetched on demand, never a cola-side name. Bounded like
                    // the poll's other calls so a hung read cannot freeze the
                    // notify path either.
                    let title = crate::bridge::bounded_call(
                        "external poll session_info",
                        self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
                        core.opencode.clone().for_directory(&directory).session_info(&sid),
                    )
                    .await
                    .and_then(|r| r.ok())
                    .and_then(|i| i.title)
                    .unwrap_or_default();
                    let card = crate::feishu::card::build_external_message_card(&title, &preview);
                    // A topic session must be reached by replying to a
                    // message INSIDE the topic (the create API rejects
                    // `receive_id_type=thread_id`). Resolve an in-topic
                    // anchor — the persisted `/topic` confirmation card, or
                    // the newest bot message in the thread. Non-topic
                    // sessions fall back to the chat top level.
                    let anchor = crate::bridge::pollers::resolve_topic_anchor(core, &thread_key).await;
                    let sent = match anchor {
                        Some(anchor) => core.feishu.reply_card(&anchor, &card).await,
                        None => core.feishu.send_card("chat_id", &thread_key.chat_id, &card).await,
                    };
                    match sent {
                        Ok(card_id) => {
                            // Now render the model's reply INTO that card, so
                            // the Feishu side sees the answer, not just the
                            // notification.
                            self.start_reply_render(core, &sid, latest, &card_id, &preview)
                                .await;
                        }
                        Err(e) => tracing::warn!("external message notify: {}", e),
                    }
                }
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
        core: &Arc<SharedCore>,
        session_id: &str,
        epoch_ms: i64,
        card_id: &str,
        preview: &str,
    ) {
        // Guard: a renderer for THIS message is already armed (the accumulator
        // still carries its epoch). cola's own prompts get a fresh accumulator,
        // so a different epoch is NOT this message — a new renderer replaces the
        // old one (whose `submit_epoch_ms` no longer matches, so it exits).
        let already_rendering = {
            let cards = core.cards.lock().await;
            cards
                .get(session_id)
                .map(|c| c.acc.submit_epoch_ms == Some(epoch_ms))
                .unwrap_or(false)
        };
        if already_rendering {
            return;
        }
        let session_dir = {
            let store = core.sessions.lock().await;
            store
                .entry_for_session(session_id)
                .map(|e| e.directory.clone())
                .unwrap_or_default()
        };
        // Card subtitle: the server's live title (ADR-0007), or the id-tail.
        let title = crate::bridge::bounded_call(
            "external render session_info",
            self.request_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            core.opencode
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

        let mut acc = StreamAccumulator::new(&subtitle);
        acc.submit_epoch_ms = Some(epoch_ms);
        acc.session_id = Some(session_id.to_string());
        acc.reply_to_message_id = Some(card_id.to_string());
        acc.attach_work_context(&session_dir).await;
        // Footer model@variant: capture the session's `/think` variant when the
        // turn is ARMED, not when it finalizes — a `/think` issued mid-render
        // must not retro-tag this card (same rule as the work-context half,
        // ADR-0019).
        acc.variant = core
            .sessions
            .lock()
            .await
            .entry_for_session(session_id)
            .and_then(|e| e.variant.clone());
        // Keep the external message visible: the notification card is updated in
        // place, so its preview would otherwise vanish when the reply renders.
        if !preview.is_empty() {
            acc.push_text(&format!("👤 {}", preview));
        }
        {
            let mut cards = core.cards.lock().await;
            cards.insert(
                session_id.to_string(),
                crate::bridge::streaming::CardSession::new(acc, Some(card_id.to_string())),
            );
        }
        tracing::info!("external reply render armed for session {}", session_id);

        let core = Arc::clone(core);
        let sid = session_id.to_string();
        let poll_ms = self.render_poll_ms.load(std::sync::atomic::Ordering::Relaxed);
        let timeout_ms = self.render_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        tokio::spawn(async move {
            external_render_loop(&core, sid, epoch_ms, poll_ms, timeout_ms).await;
        });
    }

    /// ADR-0028 busy-adopt follow: the adopted session's in-flight EXTERNAL
    /// turn streams into the snapshot card instead of freezing a static
    /// "运行中" line. The snapshot card is the host (`CardSession` in
    /// `core.cards`, like `start_reply_render`): its message id is the live
    /// card, the static 已接管 prefix + tail ride as acc text (the same
    /// representation choice as the 👤 preview), and the adopt-time pending
    /// blocks are pre-seeded as inline sections so a permission approved from
    /// the snapshot resumes the run inside the SAME card. The renderer exits
    /// when a cola prompt or a newer external message replaces the accumulator
    /// (the existing `external_render_loop` guards).
    pub(crate) async fn start_snapshot_follow(
        &self,
        core: &Arc<SharedCore>,
        session_id: &str,
        card_id: &str,
        verb: &str,
        title: &str,
        data: &crate::bridge::snapshot::SnapshotData,
    ) -> bool {
        // Race (ADR-0028): busy at gather but idle by now → the turn already
        // finished; keep the static snapshot and never arm a renderer.
        let busy_now = matches!(
            core.opencode
                .session_status(session_id, Some(&data.directory))
                .await,
            Ok(Some(crate::opencode::SessionStatus::Busy))
        );
        if !busy_now {
            tracing::info!(
                "snapshot follow: session {} no longer busy; keeping static snapshot",
                session_id
            );
            return false;
        }
        let Some(epoch_ms) = data.newest_user_epoch else {
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
        // still carries its epoch). Re-point it at the new card — a re-adopt
        // sent a fresh snapshot mid-turn — so one renderer keeps one live
        // card, and never double-render.
        let already_rendering = {
            let cards = core.cards.lock().await;
            cards
                .get(session_id)
                .map(|c| c.acc.submit_epoch_ms == Some(epoch_ms))
                .unwrap_or(false)
        };
        if already_rendering {
            let mut cards = core.cards.lock().await;
            if let Some(card) = cards.get_mut(session_id) {
                card.repoint(card_id);
            }
            tracing::info!(
                "snapshot follow: re-pointing existing renderer at card {}",
                card_id
            );
            return true;
        }
        let session_dir = {
            let store = core.sessions.lock().await;
            store
                .entry_for_session(session_id)
                .map(|e| e.directory.clone())
                .unwrap_or_default()
        };
        let mut acc = StreamAccumulator::new("");
        acc.submit_epoch_ms = Some(epoch_ms);
        acc.session_id = Some(session_id.to_string());
        acc.reply_to_message_id = Some(card_id.to_string());
        acc.attach_work_context(&session_dir).await;
        acc.variant = core
            .sessions
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
                let role = match entry.role.as_str() {
                    "user" => "👤",
                    "assistant" => "🤖",
                    _ => "💬",
                };
                let text = if entry.text.trim().is_empty() {
                    "（空消息）".to_string()
                } else {
                    entry.text.clone()
                };
                static_text.push_str(&format!("\n{role} {text}"));
            }
        }
        acc.push_text(&static_text);
        // The adopt-time pending blocks ride as inline sections: the poll's
        // inline dedupe (push_inline sees them already present) prevents a
        // duplicate, and clicking one takes the normal inline path — resolved
        // sections are stripped and the run resumes into the same card.
        for req in &data.pending {
            match req {
                crate::bridge::request::PendingRequest::Permission(p) => {
                    acc.pending_permissions
                        .push(crate::bridge::streaming::PendingPermission {
                            session_id: session_id.to_string(),
                            request_id: p.request_id.clone(),
                            body: crate::bridge::request::describe_permission(p),
                            directory: data.directory.clone(),
                        });
                }
                crate::bridge::request::PendingRequest::Question(q) => {
                    acc.pending_questions
                        .push(crate::bridge::streaming::PendingQuestion {
                            request_id: q.id.clone(),
                            session_id: q.session_id.clone(),
                            questions: q.questions.clone(),
                            directory: data.directory.clone(),
                            answers: vec![None; q.questions.len()],
                            done: vec![false; q.questions.len()],
                        });
                    // Remember the full question request (like the static
                    // claim path): the poll loop never sees follow-hosted
                    // requests, so `prepare()` never runs for them and the
                    // block's buttons would resolve to nothing.
                    core.question
                        .question_requests
                        .lock()
                        .await
                        .insert(q.id.clone(), q.clone());
                    core.question
                        .question_dirs
                        .lock()
                        .await
                        .insert(q.id.clone(), data.directory.clone());
                }
            }
        }
        {
            let mut cards = core.cards.lock().await;
            cards.insert(
                session_id.to_string(),
                crate::bridge::streaming::CardSession::new(acc, Some(card_id.to_string())),
            );
        }
        tracing::info!("snapshot follow armed for session {}", session_id);

        let core = Arc::clone(core);
        let sid = session_id.to_string();
        let poll_ms = self.render_poll_ms.load(std::sync::atomic::Ordering::Relaxed);
        let timeout_ms = self.render_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        tokio::spawn(async move {
            external_render_loop(&core, sid, epoch_ms, poll_ms, timeout_ms).await;
        });
        true
    }
}

/// ADR-0028: settle a snapshot card right after it was sent — arm the
/// busy-adopt follow when the adopted session is mid-turn (the in-flight
/// external run streams into the snapshot), otherwise keep the static
/// snapshot and claim its embedded pendings. A follow that declines to arm
/// (the busy→idle race: the turn already finished) falls back to the static
/// claim path, so the embedded pendings are never left unclaimed. Shared by
/// every adoption surface so the busy/static decision cannot drift between
/// them.
pub(crate) async fn settle_snapshot_after_send(
    core: &Arc<SharedCore>,
    snapshot_message_id: &str,
    verb: &str,
    title: &str,
    data: &crate::bridge::snapshot::SnapshotData,
) {
    let followed = data.status == Some(crate::opencode::SessionStatus::Busy)
        && core
            .external
            .start_snapshot_follow(core, &data.session_id, snapshot_message_id, verb, title, data)
            .await;
    if !followed {
        crate::bridge::request::claim_snapshot_pendings(core, snapshot_message_id, verb, title, data).await;
    }
}

/// Incremental renderer for an external message's reply: poll the session,
/// stream reasoning/tool/text into the notification card, then finalize it as
/// Done when the model finishes. Exits when the turn completes, the accumulator
/// was replaced (cola's own prompt or a newer external message), a newer user
/// message starts a new turn, or the hard timeout elapses.
async fn external_render_loop(
    core: &Arc<SharedCore>,
    session_id: String,
    epoch_ms: i64,
    poll_ms: u64,
    timeout_ms: u64,
) {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(poll_ms)).await;
        let msgs = match core.opencode.messages(&session_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("external render poll messages: {}", e);
                continue;
            }
        };
        // The accumulator was replaced (cola's own `run_prompt` inserted a fresh
        // one, or a newer external message's renderer took over): exit so this
        // turn isn't double-rendered into two cards.
        let replaced = {
            let cards = core.cards.lock().await;
            cards
                .get(&session_id)
                .map(|c| c.acc.submit_epoch_ms != Some(epoch_ms))
                .unwrap_or(true)
        };
        if replaced {
            break;
        }
        // Stream the reply's reasoning/tools/text into the notification card.
        let Some((new_parts, _, _)) = render_and_flush(core, &session_id, epoch_ms, &msgs).await else {
            break;
        };
        if new_parts > 0 {
            tracing::info!("external render: session {} gained parts", session_id);
        }
        // The model finished answering: finalize the card, then stop.
        if external_turn_completed(&msgs, epoch_ms) {
            finalize_done(core, &session_id).await;
            tracing::info!("external reply rendered: session {} done", session_id);
            break;
        }
        // A NEWER user message is a turn boundary — the poller notifies and arms
        // a fresh renderer for it.
        let newer_turn = msgs
            .iter()
            .filter(|m| m.info.role.as_deref() == Some("user"))
            .filter_map(|m| m.info.time.as_ref().map(|t| t.created))
            .any(|created| created > epoch_ms);
        if newer_turn {
            break;
        }
        // Safety net for messages that never trigger a run. If a partial reply
        // was rendered, finalize it so the card never sits on an eternal
        // spinner; otherwise leave the "有新消息" notification as-is.
        if tokio::time::Instant::now() >= deadline {
            let has_content = {
                let cards = core.cards.lock().await;
                cards
                    .get(&session_id)
                    .map(|c| !c.acc.rendered_parts.is_empty() || !c.acc.rendered_tool_states.is_empty())
                    .unwrap_or(false)
            };
            if has_content {
                finalize_done(core, &session_id).await;
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
async fn finalize_done(core: &Arc<SharedCore>, session_id: &str) {
    crate::bridge::streaming::refresh_work_context(core, session_id).await;
    {
        let mut cards = core.cards.lock().await;
        if let Some(card) = cards.get_mut(session_id) {
            card.acc.card_state = crate::feishu::card::CardState::Done;
        }
    }
    flush_card(core, session_id).await;
}

/// Whether the model has finished answering the external message: an assistant
/// message in this turn carries a `step-finish` part whose reason is NOT the
/// pause to run tools. OpenCode's terminal finish reasons are "stop", "length",
/// "content-filter", "error" and "unknown"; "tool-calls" only means the step
/// ended to execute tools and the model will continue.
fn external_turn_completed(msgs: &[crate::opencode::client::SessionMessage], epoch_ms: i64) -> bool {
    msgs.iter()
        .filter(|m| m.info.role.as_deref() == Some("assistant"))
        .filter(|m| {
            m.info
                .time
                .as_ref()
                .map(|t| t.created >= epoch_ms)
                .unwrap_or(false)
        })
        .flat_map(|m| m.parts.as_array().into_iter().flatten())
        .any(|part| {
            part.get("type").and_then(|t| t.as_str()) == Some("step-finish")
                && part
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .is_some_and(|r| r != "tool-calls")
        })
}

/// Preview of a user message, for the external-message notification card.
fn user_message_preview(msgs: &[crate::opencode::client::SessionMessage], created: i64) -> String {
    let mut out = String::new();
    for m in msgs {
        if m.info.role.as_deref() != Some("user") {
            continue;
        }
        if m.info.time.as_ref().map(|t| t.created) != Some(created) {
            continue;
        }
        if let Some(parts) = m.parts.as_array() {
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                }
            }
        }
    }
    out.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

    fn msg(role: &str, created: i64, parts: serde_json::Value) -> SessionMessage {
        SessionMessage {
            info: MessageInfo {
                id: format!("msg_{role}_{created}"),
                role: Some(role.into()),
                parent_id: None,
                time: Some(MessageTime { created }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts,
        }
    }

    fn finish(reason: &str) -> serde_json::Value {
        serde_json::json!([{ "type": "step-finish", "reason": reason }])
    }

    #[test]
    fn turn_completed_on_terminal_finish() {
        // tool-calls pauses to execute tools — NOT complete.
        let msgs = vec![
            msg(
                "user",
                1000,
                serde_json::json!([{ "type": "text", "text": "hi" }]),
            ),
            msg("assistant", 2000, finish("tool-calls")),
        ];
        assert!(!external_turn_completed(&msgs, 1000));

        // A later step finishes with "stop" — complete.
        let msgs = vec![
            msg(
                "user",
                1000,
                serde_json::json!([{ "type": "text", "text": "hi" }]),
            ),
            msg("assistant", 2000, finish("tool-calls")),
            msg("assistant", 3000, finish("stop")),
        ];
        assert!(external_turn_completed(&msgs, 1000));

        // Other terminal reasons count too.
        assert!(external_turn_completed(
            &[msg("assistant", 2000, finish("length"))],
            1000
        ));
        assert!(external_turn_completed(
            &[msg("assistant", 2000, finish("error"))],
            1000
        ));
    }

    #[test]
    fn turn_completion_ignores_other_turns() {
        // A step-finish BEFORE the external epoch belongs to an earlier turn.
        let msgs = vec![msg("assistant", 500, finish("stop"))];
        assert!(!external_turn_completed(&msgs, 1000));
    }

    #[test]
    fn preview_is_content_of_the_latest_user_message() {
        let msgs = vec![
            msg(
                "user",
                1000,
                serde_json::json!([{ "type": "text", "text": "第一条" }]),
            ),
            msg(
                "user",
                2000,
                serde_json::json!([{ "type": "text", "text": "第二条，很长很长的内容" }]),
            ),
        ];
        assert_eq!(user_message_preview(&msgs, 2000), "第二条，很长很长的内容");
        assert_eq!(user_message_preview(&msgs, 1000), "第一条");
    }
}
