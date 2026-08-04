use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::command::{self, Command};
use crate::bridge::session::SessionStore;
use crate::bridge::streaming::StreamAccumulator;
use crate::config::{Config, SessionEntry, ThreadKey};
use crate::feishu;
use crate::opencode;

pub struct App {
    pub sessions: Arc<Mutex<SessionStore>>,
    pub accumulators: Arc<Mutex<HashMap<String, StreamAccumulator>>>,
    pub card_message_ids: Arc<Mutex<HashMap<String, String>>>,
    pub permission_requests: Arc<Mutex<HashMap<String, String>>>,
    pub seen_event_ids: Arc<Mutex<HashSet<String>>>,
    /// Session ids with a prompt currently in flight (serializes prompts per
    /// session so concurrent messages don't clobber each other's accumulators).
    pub inflight: Arc<Mutex<HashSet<String>>>,
    pub opencode: opencode::Client,
    pub feishu: Arc<feishu::Client>,
}

impl App {
    pub fn new(cfg: Config, opencode: opencode::Client, feishu: feishu::Client) -> anyhow::Result<Self> {
        let session_store = SessionStore::new(cfg.bridge.session_file)?;
        Ok(Self {
            sessions: Arc::new(Mutex::new(session_store)),
            accumulators: Arc::new(Mutex::new(HashMap::new())),
            card_message_ids: Arc::new(Mutex::new(HashMap::new())),
            permission_requests: Arc::new(Mutex::new(HashMap::new())),
            seen_event_ids: Arc::new(Mutex::new(HashSet::new())),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            opencode,
            feishu: Arc::new(feishu),
        })
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let ws = Arc::clone(&self);
        let sse = Arc::clone(&self);
        let perm = Arc::clone(&self);
        let ws_task = tokio::spawn(async move { if let Err(e) = feishu::ws::event_loop(&ws).await { tracing::error!("WS: {}", e); } });
        let sse_task = tokio::spawn(async move { if let Err(e) = sse_stream_loop(&sse).await { tracing::error!("SSE: {}", e); } });
        // Permissions are not delivered on the global SSE (typed PubSub only),
        // and a prompt can be blocked on an unanswered permission forever, so the
        // poller must run independently of any single prompt lifecycle.
        let perm_task = tokio::spawn(async move { if let Err(e) = permission_poll_loop(&perm).await { tracing::error!("Permission poller: {}", e); } });
        tokio::try_join!(ws_task, sse_task, perm_task)?;
        Ok(())
    }

    pub async fn handle_message(self: &Arc<Self>, message_id: String, chat_id: String, root_id: String, text: String) {
        let thread_root = if root_id == message_id { chat_id.clone() } else { root_id.clone() };
        let thread_key = ThreadKey::new(chat_id.clone(), thread_root);
        if let Some(cmd) = command::parse_command(&text) {
            if let Err(e) = self.handle_command(cmd, thread_key, &message_id).await { tracing::error!("Cmd: {}", e); }
            return;
        }
        if let Err(e) = self.handle_prompt(thread_key, text, &message_id).await { tracing::error!("Prompt: {}", e); }
    }

    async fn handle_command(self: &Arc<Self>, cmd: Command, thread_key: ThreadKey, message_id: &str) -> crate::error::Result<()> {
        match cmd {
            Command::Dir(path) => {
                let session = self.opencode.create_session(&self.opencode.new_session_input(Some(&path))).await?;
                let entry = SessionEntry { thread_key: thread_key.clone(), session_id: session.id.clone(), name: path.clone(), directory: path.clone(), agent: None };
                let mut store = self.sessions.lock().await;
                store.set_active(entry);
                store.persist()?;
                self.feishu.reply_text(message_id, &format!("Session moved to {}", path)).await?;
            }
            Command::Switch(name) => {
                let mut store = self.sessions.lock().await;
                if store.switch(&thread_key, &name).is_some() { self.feishu.reply_text(message_id, &format!("Switched to \"{}\".", name)).await?; }
                else { self.feishu.reply_text(message_id, &format!("No session matching \"{}\".", name)).await?; }
            }
            Command::List => {
                let store = self.sessions.lock().await;
                let entries = store.list_thread(&thread_key);
                if entries.is_empty() { self.feishu.reply_text(message_id, "No sessions.").await?; return Ok(()); }
                let active_id = store.get_active(&thread_key).map(|e| &e.session_id);
                let mut list = String::from("**Sessions:**\n");
                for e in &entries {
                    let mark = if active_id.map_or(false, |id| id == &e.session_id) { " (active)" } else { "" };
                    list.push_str(&format!("- {} [{}]{mark}\n  dir: {}\n", e.name, &e.session_id[..e.session_id.len().min(12)], e.directory));
                }
                self.feishu.reply_text(message_id, &list).await?;
            }
            Command::New(name) => {
                let new_name = name.unwrap_or_else(|| format!("sess-{}", uuid::Uuid::new_v4()));
                let directory = std::env::current_dir().unwrap_or_default().to_string_lossy().to_string();
                let session = self.opencode.create_session(&self.opencode.new_session_input(Some(&directory))).await?;
                let entry = SessionEntry { thread_key: thread_key.clone(), session_id: session.id.clone(), name: new_name.clone(), directory, agent: None };
                let mut store = self.sessions.lock().await;
                store.set_active(entry);
                store.persist()?;
                self.feishu.reply_text(message_id, &format!("Created \"{}\".", new_name)).await?;
            }
            Command::Name(name) => {
                let mut store = self.sessions.lock().await;
                if let Some(entry) = store.get_active(&thread_key) { let mut e = entry.clone(); e.name = name.clone(); store.set_active(e); store.persist()?; }
                self.feishu.reply_text(message_id, &format!("Renamed to \"{}\".", name)).await?;
            }
            Command::Stop => {
                if let Some(id) = self.get_session_id(&thread_key).await { self.opencode.interrupt(&id).await?; self.feishu.reply_text(message_id, "Interrupted.").await?; }
            }
            Command::Compact => {
                if let Some(id) = self.get_session_id(&thread_key).await { self.opencode.compact(&id).await?; self.feishu.reply_text(message_id, "Compacting...").await?; }
            }
            Command::Agent(name) => {
                if let Some(id) = self.get_session_id(&thread_key).await { self.opencode.switch_agent(&id, &name).await?; self.feishu.reply_text(message_id, &format!("Agent: {}", name)).await?; }
            }
            Command::Model(name) => {
                if let Some(id) = self.get_session_id(&thread_key).await { self.opencode.switch_model(&id, &name).await?; self.feishu.reply_text(message_id, &format!("Model: {}", name)).await?; }
            }
            Command::Help => { self.feishu.reply_text(message_id, &command::help_text()).await?; }
            Command::Forward(text) => { self.handle_prompt(thread_key, text, message_id).await?; }
        }
        Ok(())
    }

    async fn handle_prompt(self: &Arc<Self>, thread_key: ThreadKey, text: String, message_id: &str) -> crate::error::Result<()> {
        let session_id = self.get_or_create_session(&thread_key, &text).await?;

        // Serialize prompts per session: if one is already in flight, don't let
        // a second message overwrite its accumulator (the two would race on the
        // same card). Reply with a notice instead.
        {
            let mut inflight = self.inflight.lock().await;
            if inflight.contains(&session_id) {
                drop(inflight);
                let _ = self.feishu.reply_text(message_id, "⏳ 上一条消息还在处理中，请稍等它完成后重发。").await;
                return Ok(());
            }
            inflight.insert(session_id.clone());
        }

        let loading = crate::feishu::card::CardBuilder::new(&text.chars().take(50).collect::<String>()).with_state(crate::feishu::card::CardState::Loading).build();
        let card_msg_id = self.feishu.reply_card(message_id, &loading).await?;
        {
            let mut ids = self.card_message_ids.lock().await;
            ids.insert(session_id.clone(), card_msg_id);
        }
        let epoch_ms = chrono::Utc::now().timestamp_millis();
        {
            // Fresh accumulator per prompt: reuse leaks stale text/tools from the
            // previous turn into the next card.
            let mut acc = StreamAccumulator::new(&text.chars().take(50).collect::<String>());
            acc.question = text.chars().take(120).collect::<String>();
            acc.reply_to_message_id = Some(message_id.to_string());
            acc.submit_epoch_ms = Some(epoch_ms);
            let mut accs = self.accumulators.lock().await;
            accs.insert(session_id.clone(), acc);
        }

        // Incremental renderer: shows reasoning / tool calls / text as parts
        // complete while the synchronous prompt is still running.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let render_app = Arc::clone(self);
        let render_sid = session_id.clone();
        let render_done = std::sync::Arc::clone(&done);
        let render_task = tokio::spawn(async move {
            render_poll_loop(&render_app, render_sid, epoch_ms, render_done).await;
        });

        let prompt_resp = self.opencode.prompt(&session_id, &text).await;

        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = render_task.await;

        let prompt_err = match &prompt_resp {
            Ok(r) => r.error.clone(),
            Err(e) => Some(e.to_string()),
        };

        // Reconcile: render any parts the incremental poll missed, then mark the
        // card Done (or Error). Fall back to the response parts if the fetch fails.
        let final_msgs = self.opencode.messages(&session_id).await.ok();
        {
            let mut accs = self.accumulators.lock().await;
            if let Some(acc) = accs.get_mut(&session_id) {
                if let Ok(resp) = &prompt_resp {
                    let mut rendered = false;
                    if let Some(msgs) = &final_msgs {
                        rendered = render_new_turn_parts(acc, msgs, epoch_ms);
                    }
                    if !rendered {
                        render_parts(acc, &resp.parts);
                    }
                }
                if let Some(err) = &prompt_err {
                    acc.error = Some(err.clone());
                    acc.card_state = crate::feishu::card::CardState::Error;
                } else {
                    acc.card_state = crate::feishu::card::CardState::Done;
                }
                tracing::info!(
                    "final render: fetched_msgs={} text={} reasoning={} tools={} rendered_parts={} error={}",
                    final_msgs.as_ref().map(|m| m.len()).unwrap_or(0),
                    acc.text.len(),
                    acc.reasoning.len(),
                    acc.tools.len(),
                    acc.rendered_parts.len(),
                    acc.error.as_deref().unwrap_or("none"),
                );
            }
        }
        self.flush_card(&session_id).await;

        {
            let mut inflight = self.inflight.lock().await;
            inflight.remove(&session_id);
        }

        let after_seq = prompt_resp.as_ref().ok().and_then(|r| r.admitted_seq);
        // NOTE: with the synchronous canonical API, the full response is
        // already rendered from parts above, so no per-session SSE replay is
        // needed. Keep the spawn only when after_seq is known (async path).
        if let Some(seq) = after_seq {
            let sse_client = self.opencode.clone();
            let sid = session_id.clone();
            let accs = Arc::clone(&self.accumulators);
            let card_ids = Arc::clone(&self.card_message_ids);
            let feishu_client = Arc::clone(&self.feishu);
            tokio::spawn(async move {
                if let Err(e) = listen_session_sse(&sse_client, &sid, Some(seq), &feishu_client, &accs, &card_ids).await { tracing::warn!("SSE session: {}", e); }
            });
        }

        // Permissions are handled by the independent poller spawned in App::run,
        // so a prompt blocked on a permission still gets its card shown.
        Ok(())
    }

    async fn get_session_id(&self, thread_key: &ThreadKey) -> Option<String> {
        self.sessions.lock().await.get_active(thread_key).map(|e| e.session_id.clone())
    }

    async fn get_or_create_session(&self, thread_key: &ThreadKey, text: &str) -> crate::error::Result<String> {
        if let Some(id) = self.get_session_id(thread_key).await { return Ok(id); }
        let directory = std::env::current_dir().unwrap_or_default().to_string_lossy().to_string();
        let session = self.opencode.create_session(&self.opencode.new_session_input(Some(&directory))).await?;
        let entry = SessionEntry { thread_key: thread_key.clone(), session_id: session.id.clone(), name: text.chars().take(50).collect(), directory, agent: None };
        let mut store = self.sessions.lock().await;
        store.set_active(entry);
        store.persist()?;
        Ok(session.id)
    }

    pub async fn process_event(&self, event: opencode::client::OpenCodeEvent) {
        let session_id = match Self::extract_session_id(&event) { Some(id) => id, None => return };
        let is_tool_boundary = matches!(&event, opencode::client::OpenCodeEvent::ToolCalled { .. } | opencode::client::OpenCodeEvent::ToolSuccess { .. } | opencode::client::OpenCodeEvent::ToolFailed { .. } | opencode::client::OpenCodeEvent::ShellStarted { .. } | opencode::client::OpenCodeEvent::ShellEnded { .. } | opencode::client::OpenCodeEvent::StepEnded { .. });
        let should_flush = {
            let mut accs = self.accumulators.lock().await;
            let Some(acc) = accs.get_mut(&session_id) else { return };
            if !acc.apply(&event) { return; }
            let now = std::time::Instant::now();
            if !is_tool_boundary && let Some(last) = acc.last_flush_at && now.duration_since(last) < std::time::Duration::from_millis(200) { false }
            else { acc.last_flush_at = Some(now); true }
        };
        if should_flush { self.flush_card(&session_id).await; }

        if let opencode::client::OpenCodeEvent::PermissionAsked { data, .. } = &event {
            if let Some(ref sid) = data.session_id {
                if let Some(ref req_id) = data.id { let mut reqs = self.permission_requests.lock().await; reqs.insert(sid.clone(), req_id.clone()); }
                let action = data.action.as_deref().unwrap_or("unknown");
                let resources = data.resources.as_deref().map(|r| r.join(", ")).unwrap_or_default();
                self.send_permission_card(sid, action, &resources).await;
            }
        }
    }

    async fn flush_card(&self, session_id: &str) {
        let accs = self.accumulators.lock().await;
        let Some(acc) = accs.get(session_id) else { return };
        let card = acc.build_card();
        drop(accs);
        let card_id = { let ids = self.card_message_ids.lock().await; ids.get(session_id).cloned() };
        if let Some(msg_id) = card_id && let Err(e) = self.feishu.update_message(&msg_id, &card).await { tracing::warn!("Card update failed: {}", e); }
    }

    /// Handle a card action (permission Allow/Deny). Returns an updated card
    /// showing the decision, so the caller can send it back in the ack.
    pub async fn handle_card_action(&self, value: serde_json::Value) -> Option<serde_json::Value> {
        let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let session_id = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        // Carried from the permission card for the result display
        let perm_label = value.get("perm_label").and_then(|v| v.as_str()).unwrap_or("");
        let perm_color = value.get("perm_color").and_then(|v| v.as_str()).unwrap_or("green");
        let perm_body = value.get("perm_body").and_then(|v| v.as_str()).unwrap_or("");

        let mut result_card = None;
        match action {
            "perm" => {
                let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
                let request_id = value.get("request_id").and_then(|v| v.as_str());
                if let Some(req_id) = request_id {
                    // Route the reply to the instance owning the session.
                    let directory = {
                        let store = self.sessions.lock().await;
                        store.directory_for_session(&session_id)
                    };
                    if let Err(e) = self.opencode.reply_permission(req_id, reply, directory.as_deref()).await {
                        tracing::error!("perm reply failed: {}", e);
                    } else {
                        tracing::info!("Permission reply sent: {} session={}", reply, session_id);
                        // Build the result card: shows the decision, no buttons
                        let label = if !perm_label.is_empty() { perm_label } else { reply };
                        result_card = Some(serde_json::json!({
                            "config": { "wide_screen_mode": true },
                            "header": { "title": { "tag": "plain_text", "content": label }, "template": perm_color },
                            "elements": [
                                { "tag": "markdown", "content": if perm_body.is_empty() { format!("Permission: {}", reply) } else { perm_body.to_string() } }
                            ]
                        }));
                    }
                }
            }
            _ => {}
        }
        result_card
    }

    async fn send_permission_card(&self, session_id: &str, action: &str, resources: &str) {
        let reply_to = { let accs = self.accumulators.lock().await; accs.get(session_id).and_then(|a| a.reply_to_message_id.clone()) };
        let Some(msg_id) = reply_to else { return };
        let card = serde_json::json!({
            "config": { "wide_screen_mode": true },
            "header": { "title": { "tag": "plain_text", "content": "Permission Required" }, "template": "red" },
            "elements": [
                { "tag": "markdown", "content": format!("**{}**\n{}", action, resources) },
                { "tag": "action", "actions": [
                    { "tag": "button", "text": { "tag": "plain_text", "content": "Allow Once" }, "type": "primary", "value": { "action": "perm", "reply": "once", "session_id": session_id } },
                    { "tag": "button", "text": { "tag": "plain_text", "content": "Allow Always" }, "type": "primary", "value": { "action": "perm", "reply": "always", "session_id": session_id } },
                    { "tag": "button", "text": { "tag": "plain_text", "content": "Deny" }, "type": "danger", "value": { "action": "perm", "reply": "reject", "session_id": session_id } }
                ]}
            ]
        });
        let _ = self.feishu.reply_card(&msg_id, &card).await;
    }

    fn extract_session_id(event: &opencode::client::OpenCodeEvent) -> Option<String> {
        match event {
            opencode::client::OpenCodeEvent::StepStarted { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::StepEnded { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::StepFailed { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::TextStarted { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::TextDelta { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::TextEnded { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::ReasoningStarted { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::ReasoningDelta { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::ReasoningEnded { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::ToolCalled { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::ToolSuccess { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::ToolFailed { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::ToolProgress { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::ShellStarted { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::ShellEnded { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::PermissionAsked { data, .. } => data.session_id.clone(),
            opencode::client::OpenCodeEvent::QuestionAsked { data, .. } => data.session_id.clone(),
            _ => None,
        }
    }
}

// ===== Standalone functions =====

/// Render canonical message parts (from `POST /session/{id}/message` response)
/// into the accumulator so the card shows the assistant's final result.
fn render_part(acc: &mut StreamAccumulator, part: &serde_json::Value) {
    match part.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                acc.text.push_str(t);
            }
            acc.card_state = crate::feishu::card::CardState::Streaming;
        }
        Some("reasoning") => {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                acc.reasoning.push_str(t);
            }
            acc.card_state = crate::feishu::card::CardState::Reasoning;
        }
        Some("tool") => {
            let name = part.get("tool").and_then(|v| v.as_str()).unwrap_or("tool");
            let call_id = part
                .get("callID")
                .and_then(|v| v.as_str())
                .unwrap_or(name)
                .to_string();
            let status = part
                .get("state")
                .and_then(|s| s.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("completed");
            let input = part
                .get("state")
                .and_then(|s| s.get("input"))
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let output = part
                .get("state")
                .and_then(|s| s.get("output"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    part.get("state")
                        .and_then(|s| s.get("metadata"))
                        .and_then(|m| m.get("output"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });
            acc.tools.insert(
                call_id,
                crate::feishu::card::ToolPanel {
                    name: name.to_string(),
                    status: status.to_string(),
                    input,
                    output,
                },
            );
            if status == "running" {
                acc.card_state = crate::feishu::card::CardState::Streaming;
            }
        }
        Some("step-start") | Some("step-finish") | Some("patch") => {
            // No visible content for these
        }
        _ => {}
    }
}

fn render_parts(acc: &mut StreamAccumulator, parts: &serde_json::Value) {
    let Some(arr) = parts.as_array() else { return };
    for part in arr {
        render_part(acc, part);
    }
}

/// Render the parts of this turn's assistant messages that haven't been
/// rendered yet. Returns true if anything new was rendered.
fn render_new_turn_parts(
    acc: &mut StreamAccumulator,
    msgs: &[opencode::client::SessionMessage],
    epoch_ms: i64,
) -> bool {
    let mut rendered_any = false;
    for m in msgs {
        let is_assistant = m.info.role.as_deref() == Some("assistant");
        let in_turn = m
            .info
            .time
            .as_ref()
            .map(|t| t.created >= epoch_ms)
            .unwrap_or(false);
        if !is_assistant || !in_turn {
            continue;
        }
        let Some(parts) = m.parts.as_array() else { continue };
        for part in parts {
            let ptype = part.get("type").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            // Reasoning/text parts are written with empty text first, then
            // updated with the full content. Only render once they have content,
            // otherwise we'd freeze the placeholder version.
            if ptype == "reasoning" || ptype == "text" {
                let has_text = part
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if !has_text {
                    continue;
                }
                let part_id = part.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let Some(id) = &part_id {
                    if acc.rendered_parts.contains(id) {
                        continue;
                    }
                    acc.rendered_parts.insert(id.clone());
                }
                render_part(acc, part);
                rendered_any = true;
                continue;
            }
            // Tool parts get updated in place (running → completed); re-render
            // whenever the state signature changes so panels don't stay stuck
            // on "running".
            if ptype == "tool" {
                let call_id = part.get("callID").and_then(|v| v.as_str()).unwrap_or_default();
                let status = part
                    .pointer("/state/status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let output_len = part
                    .pointer("/state/output")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                let sig = format!("{}|{}", status, output_len);
                if let Some(prev) = acc.rendered_tool_states.get(call_id) {
                    if prev == &sig {
                        continue;
                    }
                }
                acc.rendered_tool_states.insert(call_id.to_string(), sig);
                render_part(acc, part);
                rendered_any = true;
                continue;
            }
            // Everything else (step-start/step-finish/patch): render once.
            let part_id = part.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
            if let Some(id) = &part_id {
                if acc.rendered_parts.contains(id) {
                    continue;
                }
                acc.rendered_parts.insert(id.clone());
            }
            render_part(acc, part);
            rendered_any = true;
        }
    }
    rendered_any
}

/// Incremental renderer: while the synchronous prompt is in flight, poll the
/// session's messages and flush the card as parts complete (reasoning, tools,
/// text). `done` stops the loop once the prompt returns.
async fn render_poll_loop(
    app: &Arc<App>,
    session_id: String,
    epoch_ms: i64,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
        if done.load(Ordering::SeqCst) {
            return;
        }
        let msgs = match app.opencode.messages(&session_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("render poll messages: {}", e);
                continue;
            }
        };
        let (changed, parts_rendered, text_len, reasoning_len) = {
            let mut accs = app.accumulators.lock().await;
            let Some(acc) = accs.get_mut(&session_id) else { continue; };
            let before = acc.rendered_parts.len();
            let changed = render_new_turn_parts(acc, &msgs, epoch_ms);
            (changed, acc.rendered_parts.len() - before, acc.text.len(), acc.reasoning.len())
        };
        if changed {
            tracing::info!("render poll: {} new parts, text={} reasoning={}", parts_rendered, text_len, reasoning_len);
            app.flush_card(&session_id).await;
        }
    }
}

async fn listen_session_sse(
    client: &opencode::Client, session_id: &str, after_seq: Option<i64>,
    feishu: &Arc<feishu::Client>, accs_lock: &Arc<Mutex<HashMap<String, StreamAccumulator>>>,
    card_ids_lock: &Arc<Mutex<HashMap<String, String>>>,
) -> crate::error::Result<()> {
    let mut url = client.url(&format!("/api/session/{}/event", session_id));
    if let Some(seq) = after_seq { url.push_str(&format!("?after={}", seq)); }
    let response = client.http.get(&url).send().await?.error_for_status()?;
    tracing::info!("Session SSE connected: {}", session_id);
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(boundary) = buffer.find("\n\n") {
            let block = buffer[..boundary].to_string();
            buffer = buffer[boundary + 2..].to_string();
            if block.trim().is_empty() { continue; }
            let data: String = block.lines().filter(|l| l.starts_with("data:")).map(|l| l.strip_prefix("data:").unwrap().trim()).collect::<Vec<_>>().join("\n");
            if data.is_empty() { continue; }
            let event = match serde_json::from_str::<opencode::client::OpenCodeEvent>(&data) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("Session SSE parse fail: {} raw={}", e, &data[..data.len().min(200)]);
                    continue;
                }
            };
            if let opencode::client::OpenCodeEvent::Unknown = &event {
                tracing::debug!("Session SSE unknown: {}", &data[..data.len().min(120)]);
                continue;
            }
            tracing::info!("Session SSE evt: {}", event_type_name(&event));
            let is_tool_boundary = matches!(&event, opencode::client::OpenCodeEvent::ToolCalled { .. } | opencode::client::OpenCodeEvent::ToolSuccess { .. } | opencode::client::OpenCodeEvent::ToolFailed { .. } | opencode::client::OpenCodeEvent::StepEnded { .. });
            let should_flush = {
                let mut accs = accs_lock.lock().await;
                let Some(acc) = accs.get_mut(session_id) else { continue; };
                if !acc.apply(&event) { continue; }
                let now = std::time::Instant::now();
                if !is_tool_boundary && let Some(last) = acc.last_flush_at && now.duration_since(last) < std::time::Duration::from_millis(200) { false }
                else { acc.last_flush_at = Some(now); true }
            };
            if should_flush {
                let accs = accs_lock.lock().await;
                let Some(acc) = accs.get(session_id) else { continue; };
                let card = acc.build_card();
                drop(accs);
                let card_id = { let ids = card_ids_lock.lock().await; ids.get(session_id).cloned() };
                if let Some(msg_id) = card_id && let Err(e) = feishu.update_message(&msg_id, &card).await { tracing::warn!("Card update: {}", e); }
            }
        }
    }
    Ok(())
}

/// Independent permission poller: runs forever, surfaces pending permission
/// requests as cards as soon as they appear. Started once at App startup so a
/// prompt blocked on an unanswered permission still gets its card shown.
async fn permission_poll_loop(app: &Arc<App>) -> crate::error::Result<()> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        // Pending permissions live in the server instance for the session's
        // directory; `GET /permission` must be scoped with `?directory=` or it
        // only sees the server cwd instance. Check every known session directory.
        let directories = { app.sessions.lock().await.directories() };
        for dir in &directories {
            match app.opencode.list_permissions(Some(dir)).await {
                Ok(perms) => {
                    for p in &perms {
                        let sid = p.session_id.clone().unwrap_or_default();
                        if seen.contains(&p.request_id) { continue; }
                        seen.insert(p.request_id.clone());
                        tracing::info!("Permission ({}): {} {:?}", dir, p.permission.as_deref().unwrap_or("?"), p.patterns);
                        {
                            let mut reqs = app.permission_requests.lock().await;
                            reqs.insert(sid.clone(), p.request_id.clone());
                        }
                        let body = describe_permission(p);
                        // Result card shown after clicking a button (buttons carry these)
                        let btn_value = |reply: &str, label: &str, color: &str| serde_json::json!({
                            "action": "perm",
                            "reply": reply,
                            "session_id": &sid,
                            "request_id": &p.request_id,
                            "perm_label": label,
                            "perm_color": color,
                            "perm_body": &body,
                        });
                        let card = serde_json::json!({
                            "config": { "wide_screen_mode": true },
                            "header": { "title": { "tag": "plain_text", "content": "🔐 Permission Required" }, "template": "orange" },
                            "elements": [
                                { "tag": "markdown", "content": body },
                                { "tag": "action", "actions": [
                                    { "tag": "button", "text": { "tag": "plain_text", "content": "✅ Allow Once" }, "type": "primary", "value": btn_value("once", "✅ Allowed once", "green") },
                                    { "tag": "button", "text": { "tag": "plain_text", "content": "🔁 Allow Always" }, "type": "default", "value": btn_value("always", "✅ Allowed always", "green") },
                                    { "tag": "button", "text": { "tag": "plain_text", "content": "🚫 Deny" }, "type": "danger", "value": btn_value("reject", "🚫 Denied", "red") }
                                ]}
                            ]
                        });
                        // Reply to the message that triggered the prompt for this
                        // session; fall back to sending into the chat when the
                        // accumulator is gone (e.g. after a cola restart).
                        let reply_to = {
                            let accs = app.accumulators.lock().await;
                            accs.get(&sid).and_then(|a| a.reply_to_message_id.clone())
                        };
                        let sent = if let Some(msg_id) = reply_to {
                            app.feishu.reply_card(&msg_id, &card).await.is_ok()
                        } else {
                            let chat = { app.sessions.lock().await.chat_for_session(&sid) };
                            if let Some(chat_id) = chat {
                                app.feishu.send_card("chat_id", &chat_id, &card).await.is_ok()
                            } else {
                                tracing::warn!("No reply target or chat for permission on session {}", sid);
                                false
                            }
                        };
                        if !sent {
                            tracing::warn!("Permission card send failed on session {}", sid);
                        }
                    }
                }
                Err(e) => tracing::warn!("poll perm ({}): {}", dir, e),
            }
        }
    }
}

fn describe_permission(p: &opencode::client::PermissionRequest) -> String {
    let action = p.permission.as_deref().unwrap_or("?");
    let (emoji, label) = describe_action(action);
    let mut s = format!("{} **{}**\n", emoji, label);

    if !p.patterns.is_empty() {
        s.push_str("**对象**:\n");
        for r in &p.patterns {
            s.push_str(&format!("- `{}`\n", truncate(r, 150)));
        }
    }

    // Metadata often carries richer context (e.g. bash command, tool input)
    if let Some(meta) = &p.metadata {
        let mut shown = 0;
        for (key, label) in [("command", "命令"), ("cwd", "目录"), ("description", "说明"), ("input", "输入"), ("path", "路径")] {
            if let Some(v) = meta.get(key) {
                let val = v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string());
                s.push_str(&format!("**{}**: `{}`\n", label, truncate(&val, 200)));
                shown += 1;
                if shown >= 3 { break; }
            }
        }
        if shown == 0 {
            let compact = serde_json::to_string(meta).unwrap_or_default();
            if compact.len() > 300 {
                s.push_str(&format!("**详情**: `{}`", truncate(&compact, 300)));
            } else if !compact.is_empty() && compact != "{}" {
                s.push_str(&format!("**详情**: `{}`", compact));
            }
        }
    }

    s.push_str("\nAI 想要执行这个操作，是否允许？");
    s
}

/// Map an OpenCode permission action to a friendly emoji + Chinese description.
fn describe_action(action: &str) -> (&'static str, &'static str) {
    match action {
        "bash" => ("⚡", "执行 Shell 命令"),
        "read" => ("📖", "读取文件"),
        "write" => ("✏️", "修改文件"),
        "edit" => ("✏️", "编辑文件"),
        "patch" => ("✏️", "应用补丁"),
        "webfetch" => ("🌐", "访问网页"),
        "fetch" => ("🌐", "获取网络资源"),
        "external_directory" => ("📁", "访问外部目录"),
        "kill" => ("🛑", "终止进程"),
        "ls" => ("📂", "列出目录"),
        "list" => ("📂", "列出内容"),
        "rename" => ("🔤", "重命名"),
        "remove" | "delete" | "rm" => ("🗑️", "删除文件"),
        "mkdir" => ("📁", "创建目录"),
        "move" | "mv" => ("📦", "移动文件"),
        "copy" | "cp" => ("📋", "复制文件"),
        "link" | "ln" => ("🔗", "创建链接"),
        "install" => ("📥", "安装依赖"),
        "test" => ("🧪", "运行测试"),
        "build" => ("🏗️", "构建项目"),
        "git" => ("🌿", "执行 Git 操作"),
        _ => ("🔐", "执行操作"),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}...", s.chars().take(max).collect::<String>()) }
}

/// Short name of an OpenCode event for logging.
fn event_type_name(e: &opencode::client::OpenCodeEvent) -> &'static str {
    match e {
        opencode::client::OpenCodeEvent::ServerConnected => "server.connected",
        opencode::client::OpenCodeEvent::SessionPrompted { .. } => "prompted",
        opencode::client::OpenCodeEvent::SessionPromptAdmitted { .. } => "prompt.admitted",
        opencode::client::OpenCodeEvent::StepStarted { .. } => "step.started",
        opencode::client::OpenCodeEvent::StepEnded { .. } => "step.ended",
        opencode::client::OpenCodeEvent::StepFailed { .. } => "step.failed",
        opencode::client::OpenCodeEvent::TextStarted { .. } => "text.started",
        opencode::client::OpenCodeEvent::TextDelta { .. } => "text.delta",
        opencode::client::OpenCodeEvent::TextEnded { .. } => "text.ended",
        opencode::client::OpenCodeEvent::ReasoningStarted { .. } => "reasoning.started",
        opencode::client::OpenCodeEvent::ReasoningDelta { .. } => "reasoning.delta",
        opencode::client::OpenCodeEvent::ReasoningEnded { .. } => "reasoning.ended",
        opencode::client::OpenCodeEvent::ToolCalled { .. } => "tool.called",
        opencode::client::OpenCodeEvent::ToolSuccess { .. } => "tool.success",
        opencode::client::OpenCodeEvent::ToolFailed { .. } => "tool.failed",
        opencode::client::OpenCodeEvent::ToolProgress { .. } => "tool.progress",
        opencode::client::OpenCodeEvent::ShellStarted { .. } => "shell.started",
        opencode::client::OpenCodeEvent::ShellEnded { .. } => "shell.ended",
        opencode::client::OpenCodeEvent::PermissionAsked { .. } => "permission.asked",
        opencode::client::OpenCodeEvent::PermissionReplied { .. } => "permission.replied",
        opencode::client::OpenCodeEvent::QuestionAsked { .. } => "question.asked",
        opencode::client::OpenCodeEvent::QuestionReplied { .. } => "question.replied",
        opencode::client::OpenCodeEvent::QuestionRejected { .. } => "question.rejected",
        opencode::client::OpenCodeEvent::CompactionStarted { .. } => "compaction.started",
        opencode::client::OpenCodeEvent::CompactionEnded { .. } => "compaction.ended",
        opencode::client::OpenCodeEvent::Unknown => "unknown",
    }
}

async fn sse_stream_loop(app: &Arc<App>) -> crate::error::Result<()> {
    loop {
        match sse_stream_once(app).await {
            Ok(()) => tracing::warn!("OpenCode SSE stream ended, reconnecting..."),
            Err(e) => tracing::warn!("OpenCode SSE error: {}, reconnecting...", e),
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}

async fn sse_stream_once(app: &Arc<App>) -> crate::error::Result<()> {
    let mut stream = opencode::sse::SseStream::connect(&app.opencode).await?;
    tracing::info!("Connected to OpenCode SSE");
    while let Some(result) = stream.next_event().await {
        match result {
            Ok(event) => app.process_event(event).await,
            Err(e) => tracing::warn!("SSE: {}", e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::streaming::StreamAccumulator;
    use crate::feishu::card::CardState;

    #[test]
    fn render_parts_shows_reasoning_and_tool_output() {
        // Shapes copied from a real turn in the message store: reasoning parts
        // carry "text", tool parts carry "state.output" (NOT state.metadata.output).
        let parts = serde_json::json!([
            {"type": "step-start", "snapshot": "abc"},
            {"type": "reasoning", "text": "The user is asking in Chinese."},
            {"type": "tool", "tool": "bash", "callID": "call_1",
             "state": {"status": "completed", "input": {"command": "pwd && ls -la"},
                       "output": "/root/workspace/dev/cola\n..."}},
            {"type": "step-finish", "reason": "tool-calls"},
            {"type": "step-start", "snapshot": "abc"},
            {"type": "text", "text": "我是 opencode。"},
            {"type": "step-finish", "reason": "stop"},
        ]);

        let mut acc = StreamAccumulator::new("test");
        render_parts(&mut acc, &parts);
        acc.card_state = CardState::Done;

        assert!(acc.reasoning.contains("The user is asking in Chinese."));
        assert_eq!(acc.tools.len(), 1);
        let tool = &acc.tools["call_1"];
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.status, "completed");
        assert!(tool.output.as_deref().unwrap().contains("/root/workspace/dev/cola"));
        assert!(tool.input.as_deref().unwrap().contains("pwd"));

        let card = acc.build_card().to_string();
        assert!(card.contains("推理过程"));
        assert!(card.contains("bash"));
    }

    #[test]
    fn render_parts_falls_back_to_metadata_output() {
        let parts = serde_json::json!([
            {"type": "tool", "tool": "read", "callID": "call_2",
             "state": {"status": "completed", "input": {"path": "src/main.rs"},
                       "metadata": {"output": "fn main() {}"}}},
        ]);
        let mut acc = StreamAccumulator::new("test");
        render_parts(&mut acc, &parts);
        assert_eq!(acc.tools["call_2"].output.as_deref(), Some("fn main() {}"));
    }

    #[test]
    fn render_new_turn_parts_filters_turn_and_dedups() {
        use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 1000;
        let mut acc = StreamAccumulator::new("test");
        acc.submit_epoch_ms = Some(epoch);

        let msgs = vec![
            // Old turn assistant message (before epoch) — skipped.
            SessionMessage {
                info: MessageInfo { id: "old".into(), role: Some("assistant".into()), parent_id: None, time: Some(MessageTime { created: 100 }) },
                parts: serde_json::json!([{ "id": "prt_old", "type": "reasoning", "text": "old reasoning" }]),
            },
            // User message — skipped (not assistant).
            SessionMessage {
                info: MessageInfo { id: "user".into(), role: Some("user".into()), parent_id: None, time: Some(MessageTime { created: 2000 }) },
                parts: serde_json::json!([{ "id": "prt_user", "type": "text", "text": "question" }]),
            },
            // Current turn assistant message.
            SessionMessage {
                info: MessageInfo { id: "a1".into(), role: Some("assistant".into()), parent_id: None, time: Some(MessageTime { created: 3000 }) },
                parts: serde_json::json!([
                    { "id": "prt_rsn", "type": "reasoning", "text": "Let me think" },
                    { "id": "prt_tool", "type": "tool", "tool": "bash", "callID": "call_1", "state": { "status": "completed", "input": { "command": "ls" }, "output": "src" } },
                ]),
            },
        ];

        assert!(render_new_turn_parts(&mut acc, &msgs, epoch));
        assert!(acc.reasoning.contains("Let me think"));
        assert_eq!(acc.tools.len(), 1);
        assert_eq!(acc.rendered_parts.len(), 1);
        assert_eq!(acc.rendered_tool_states.len(), 1);
        assert!(!acc.text.contains("question"));
        assert!(!acc.reasoning.contains("old reasoning"));

        assert!(!render_new_turn_parts(&mut acc, &msgs, epoch));
    }

    #[test]
    fn tool_part_update_re_renders_panel() {
        use crate::opencode::client::{MessageInfo, MessageTime, SessionMessage};

        let epoch = 0;
        let mut acc = StreamAccumulator::new("test");
        acc.submit_epoch_ms = Some(epoch);

        let msgs = |status: &str, output: &str| vec![SessionMessage {
            info: MessageInfo {
                id: "a1".into(),
                role: Some("assistant".into()),
                parent_id: None,
                time: Some(MessageTime { created: 100 }),
            },
            parts: serde_json::json!([{
                "id": "prt_tool",
                "type": "tool",
                "tool": "bash",
                "callID": "call_1",
                "state": { "status": status, "input": { "command": "ls" }, "output": output },
            }]),
        }];

        // First render: tool running.
        assert!(render_new_turn_parts(&mut acc, &msgs("running", ""), epoch));
        assert_eq!(acc.tools["call_1"].status, "running");

        // Same part id, updated to completed — must re-render (upsert).
        assert!(render_new_turn_parts(&mut acc, &msgs("completed", "src\n"), epoch));
        assert_eq!(acc.tools["call_1"].status, "completed");

        // No change → nothing new.
        assert!(!render_new_turn_parts(&mut acc, &msgs("completed", "src\n"), epoch));
    }
}
