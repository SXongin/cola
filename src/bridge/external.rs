use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::core::SharedCore;
use crate::bridge::render::{flush_card, render_and_flush};
use crate::bridge::streaming::StreamAccumulator;

/// The external-message flow: watches for user messages that were NOT sent by
/// cola (someone posted from OpenChamber or another client on the shared store)
/// and notifies the Feishu side with a small card. cola's own prompts are
/// excluded via the per-session baseline set at the end of each prompt.
pub struct ExternalFlow {
    /// session_id → created time of the last user message cola knows about.
    pub last_user_msg_epoch: Arc<Mutex<HashMap<String, i64>>>,
    /// External poll cadence (ms). Defaults to today's 8 s; tests store a small
    /// value so every loop branch runs without sleeping real seconds.
    pub poll_interval_ms: std::sync::atomic::AtomicU64,
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
            render_poll_ms: std::sync::atomic::AtomicU64::new(1_500),
            render_timeout_ms: std::sync::atomic::AtomicU64::new(600_000),
        }
    }

    /// Record the user-message baseline after cola's own prompt completes, so
    /// the external poller treats anything newer as posted by another client.
    /// Owns `last_user_msg_epoch` — the prompt path calls this method instead
    /// of poking the map directly.
    pub(crate) async fn record_prompt_baseline(&self, session_id: &str, baseline: i64) {
        self.last_user_msg_epoch
            .lock()
            .await
            .insert(session_id.to_string(), baseline);
    }

    /// Independent poller: detects user messages not sent by cola and notifies
    /// the Feishu side. Started once at App startup.
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
            let sessions: Vec<(String, crate::config::ThreadKey, String)> = core
                .sessions
                .lock()
                .await
                .all_entries()
                .into_iter()
                .map(|e| (e.session_id.clone(), e.thread_key.clone(), e.directory.clone()))
                .collect();
            for (sid, thread_key, directory) in sessions {
                // While cola is answering this session, any new message is cola's own.
                if core.inflight.lock().await.contains(&sid) {
                    continue;
                }
                let Ok(msgs) = core.opencode.messages(&sid).await else {
                    continue;
                };
                let latest_user = msgs
                    .iter()
                    .filter(|m| m.info.role.as_deref() == Some("user"))
                    .filter_map(|m| m.info.time.as_ref().map(|t| t.created))
                    .max();
                let Some(latest) = latest_user else {
                    continue;
                };
                let mut map = self.last_user_msg_epoch.lock().await;
                match map.get(&sid).copied() {
                    // First observation: just establish the baseline, don't notify.
                    None => {
                        map.insert(sid.clone(), latest);
                    }
                    Some(prev) if latest > prev => {
                        map.insert(sid.clone(), latest);
                        let preview = user_message_preview(&msgs, latest);
                        drop(map);
                        tracing::info!("External message on session {}: {}", sid, preview);
                        // The card title is the server's session title (ADR-0007)
                        // — fetched on demand, never a cola-side name.
                        let title = core
                            .opencode
                            .clone()
                            .for_directory(&directory)
                            .session_info(&sid)
                            .await
                            .ok()
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
                    _ => {}
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
        let title = core
            .opencode
            .clone()
            .for_directory(session_dir.as_str())
            .session_info(session_id)
            .await
            .ok()
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
        acc.directory = if session_dir.is_empty() {
            None
        } else {
            Some(session_dir)
        };
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
/// reply that finished (or timed out with content rendered).
async fn finalize_done(core: &Arc<SharedCore>, session_id: &str) {
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
