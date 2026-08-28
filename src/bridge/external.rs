use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::handler::App;

/// The external-message flow: watches for user messages that were NOT sent by
/// cola (someone posted from OpenChamber or another client on the shared store)
/// and notifies the Feishu side with a small card. cola's own prompts are
/// excluded via the per-session baseline set at the end of each prompt.
pub struct ExternalFlow {
    /// session_id → created time of the last user message cola knows about.
    pub last_user_msg_epoch: Arc<Mutex<HashMap<String, i64>>>,
}

impl ExternalFlow {
    pub fn new() -> Self {
        Self {
            last_user_msg_epoch: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Independent poller: detects user messages not sent by cola and notifies
    /// the Feishu side. Started once at App startup.
    pub(crate) async fn poll_loop(&self, app: &Arc<App>) -> crate::error::Result<()> {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(8)).await;
            let sessions: Vec<(String, crate::config::ThreadKey, String)> = app
                .sessions
                .lock()
                .await
                .all_entries()
                .into_iter()
                .map(|e| (e.session_id.clone(), e.thread_key.clone(), e.name.clone()))
                .collect();
            for (sid, thread_key, name) in sessions {
                // While cola is answering this session, any new message is cola's own.
                if app.inflight.lock().await.contains(&sid) {
                    continue;
                }
                let Ok(msgs) = app.opencode.messages(&sid).await else {
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
                        let card = crate::feishu::card::build_external_message_card(&name, &preview);
                        // A topic session must be reached by replying to a
                        // message INSIDE the topic (the create API rejects
                        // `receive_id_type=thread_id`). Resolve an in-topic
                        // anchor — the persisted `/topic` confirmation card, or
                        // the newest bot message in the thread. Non-topic
                        // sessions fall back to the chat top level.
                        let anchor = crate::bridge::pollers::resolve_topic_anchor(app, &thread_key).await;
                        let sent = match anchor {
                            Some(anchor) => app.feishu.reply_card(&anchor, &card).await,
                            None => app.feishu.send_card("chat_id", &thread_key.chat_id, &card).await,
                        };
                        if let Err(e) = sent {
                            tracing::warn!("external message notify: {}", e);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
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
