#![cfg(test)]

pub(crate) use std::sync::Arc;

pub(crate) use crate::bridge::handler::App;
pub(crate) use crate::feishu;
pub(crate) use crate::opencode;

/// A recorded `reply_question` call: (request_id, answers).
type QuestionReplyRecord = (String, Vec<Vec<String>>);

#[derive(Debug, Clone)]
#[allow(dead_code)] // the recording adapter captures full call details for assertions
pub enum PlatformCall {
    ReplyCard {
        reply_to: String,
        card: serde_json::Value,
    },
    SendCard {
        receive_id: String,
        card: serde_json::Value,
    },
    UpdateMessage {
        message_id: String,
        card: serde_json::Value,
    },
    ReplyText {
        message_id: String,
        text: String,
    },
    ReplyInThread {
        message_id: String,
        text: String,
        thread_id: Option<String>,
    },
    ReplyCardInThread {
        message_id: String,
        card: serde_json::Value,
        thread_id: Option<String>,
    },
    CompletionNotice {
        reply_to: String,
        open_id: String,
        name: Option<String>,
        text: String,
    },
}

/// Records every card cola would send, instead of posting to Feishu.
pub struct RecordingPlatform {
    pub calls: Arc<tokio::sync::Mutex<Vec<PlatformCall>>>,
    /// open_id → display name served by `user_name` (empty = lookup fails).
    pub user_names: std::collections::HashMap<String, String>,
    /// chat_id → display name served by `chat_name` (absent = None).
    pub chat_names: std::collections::HashMap<String, String>,
    /// When true, `send_card` fails (tests the topic-cover fallback path).
    pub fail_send_card: bool,
    /// When true, `reply_card` fails (tests the restart-announce fallback).
    pub fail_reply_card: bool,
    /// The thread_id `reply_in_thread` returns; `None` simulates a chat
    /// without topic support (the create-topic surfaces degrade with a
    /// message instead of mapping).
    pub reply_in_thread_thread_id: Option<String>,
    /// message_id → quoted-parent content served by `get_message` (absent =
    /// the default text parent). Lets tests script quote-injection cases.
    pub quoted_messages:
        std::sync::Mutex<std::collections::HashMap<String, crate::feishu::client::FeishuMessage>>,
}

impl RecordingPlatform {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            user_names: std::collections::HashMap::new(),
            chat_names: std::collections::HashMap::new(),
            fail_send_card: false,
            fail_reply_card: false,
            reply_in_thread_thread_id: Some("omt_created_topic".into()),
            quoted_messages: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Every card the app replied to a message with, in call order (C4 query
    /// helper — no fluent DSL, just the structured payload).
    pub(crate) async fn replied_cards(&self) -> Vec<serde_json::Value> {
        self.cards_of(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card),
            _ => None,
        })
        .await
    }

    /// Every card the app sent to a chat.
    pub(crate) async fn sent_cards(&self) -> Vec<serde_json::Value> {
        self.cards_of(|c| match c {
            PlatformCall::SendCard { card, .. } => Some(card),
            _ => None,
        })
        .await
    }

    /// Every in-place card update (the streaming card's flushes).
    pub(crate) async fn updated_cards(&self) -> Vec<serde_json::Value> {
        self.cards_of(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card),
            _ => None,
        })
        .await
    }

    /// Every plain text reply.
    pub(crate) async fn texts(&self) -> Vec<String> {
        self.calls
            .lock()
            .await
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every button `value` payload on every card the platform saw (replied,
    /// sent, in-thread or updated), in card walk order.
    pub(crate) async fn button_values(&self) -> Vec<serde_json::Value> {
        let calls = self.calls.lock().await;
        let mut out = Vec::new();
        for call in calls.iter() {
            match call {
                PlatformCall::ReplyCard { card, .. }
                | PlatformCall::SendCard { card, .. }
                | PlatformCall::UpdateMessage { card, .. }
                | PlatformCall::ReplyCardInThread { card, .. } => {
                    collect_button_values(card, &mut out);
                }
                _ => {}
            }
        }
        out
    }

    async fn cards_of(
        &self,
        pick: impl Fn(&PlatformCall) -> Option<&serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        self.calls.lock().await.iter().filter_map(pick).cloned().collect()
    }
}

/// Walk a card and collect every button's `value` payload (buttons nest in
/// column sets and action blocks, so the walk is recursive).
fn collect_button_values(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("tag").and_then(|t| t.as_str()) == Some("button")
                && let Some(value) = map.get("value")
            {
                out.push(value.clone());
            }
            for v in map.values() {
                collect_button_values(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_button_values(v, out);
            }
        }
        _ => {}
    }
}

#[async_trait::async_trait]
impl feishu::Platform for RecordingPlatform {
    async fn get_ws_endpoint(&self) -> crate::error::Result<String> {
        Ok("wss://example.test".into())
    }

    async fn reply_card(&self, reply_to: &str, card: &serde_json::Value) -> crate::error::Result<String> {
        if self.fail_reply_card {
            return Err(crate::error::BridgeError::Feishu(
                "simulated reply_card failure".into(),
            ));
        }
        self.calls.lock().await.push(PlatformCall::ReplyCard {
            reply_to: reply_to.into(),
            card: card.clone(),
        });
        Ok("msg_reply".into())
    }

    async fn send_card(
        &self,
        _receive_id_type: &str,
        receive_id: &str,
        card: &serde_json::Value,
    ) -> crate::error::Result<String> {
        if self.fail_send_card {
            return Err(crate::error::BridgeError::Feishu(
                "simulated send_card failure".into(),
            ));
        }
        self.calls.lock().await.push(PlatformCall::SendCard {
            receive_id: receive_id.into(),
            card: card.clone(),
        });
        Ok("msg_sent".into())
    }

    async fn update_message(&self, message_id: &str, card: &serde_json::Value) -> crate::error::Result<()> {
        self.calls.lock().await.push(PlatformCall::UpdateMessage {
            message_id: message_id.into(),
            card: card.clone(),
        });
        Ok(())
    }

    async fn reply_text(&self, message_id: &str, text: &str) -> crate::error::Result<String> {
        self.calls.lock().await.push(PlatformCall::ReplyText {
            message_id: message_id.into(),
            text: text.into(),
        });
        Ok("msg_text".into())
    }

    async fn reply_in_thread(
        &self,
        message_id: &str,
        text: &str,
    ) -> crate::error::Result<(String, Option<String>)> {
        let thread_id = self.reply_in_thread_thread_id.clone();
        self.calls.lock().await.push(PlatformCall::ReplyInThread {
            message_id: message_id.into(),
            text: text.into(),
            thread_id: thread_id.clone(),
        });
        // The mock's created topic-reply message id becomes the anchor.
        Ok(("msg_topic_reply".into(), thread_id))
    }

    async fn reply_card_in_thread(
        &self,
        message_id: &str,
        card: &serde_json::Value,
    ) -> crate::error::Result<(String, Option<String>)> {
        let thread_id = self.reply_in_thread_thread_id.clone();
        self.calls.lock().await.push(PlatformCall::ReplyCardInThread {
            message_id: message_id.into(),
            card: card.clone(),
            thread_id: thread_id.clone(),
        });
        // The mock's created topic-reply message id becomes the anchor (same
        // id as the text reply_in_thread, so topic-adopt tests asserting the
        // anchor id pass for both seed kinds).
        Ok(("msg_topic_reply".into(), thread_id))
    }

    async fn reply_completion_notice(
        &self,
        message_id: &str,
        open_id: &str,
        name: Option<&str>,
        text: &str,
    ) -> crate::error::Result<String> {
        self.calls.lock().await.push(PlatformCall::CompletionNotice {
            reply_to: message_id.into(),
            open_id: open_id.into(),
            name: name.map(|s| s.to_string()),
            text: text.into(),
        });
        Ok("msg_notice".into())
    }

    async fn user_name(&self, open_id: &str) -> crate::error::Result<Option<String>> {
        Ok(self.user_names.get(open_id).cloned())
    }

    async fn chat_name(&self, chat_id: &str) -> crate::error::Result<Option<String>> {
        Ok(self.chat_names.get(chat_id).cloned())
    }

    async fn bot_open_id(&self) -> crate::error::Result<String> {
        Ok("ou_test_bot".into())
    }

    async fn list_messages(
        &self,
        _container_id_type: &str,
        _container_id: &str,
    ) -> crate::error::Result<Vec<crate::feishu::client::ChatMessage>> {
        Ok(vec![crate::feishu::client::ChatMessage {
            message_id: "msg_in_topic_anchor".into(),
            msg_type: "interactive".into(),
            create_time: "0".into(),
            chat_id: "chat_1".into(),
            sender: Some(crate::feishu::client::ChatMessageSender {
                id: Some("cli_bot".into()),
                sender_type: Some("app".into()),
            }),
            body: None,
        }])
    }

    async fn get_message(
        &self,
        message_id: &str,
    ) -> crate::error::Result<crate::feishu::client::FeishuMessage> {
        // The mock serves parents via `quoted_messages`; a missing entry is
        // a hard failure (like a real `im:message` permission error) so
        // degradation paths are testable.
        self.quoted_messages
            .lock()
            .unwrap()
            .get(message_id)
            .cloned()
            .ok_or_else(|| crate::error::BridgeError::Feishu(format!("mock: no quoted message {message_id}")))
    }

    async fn download_image(
        &self,
        _message_id: &str,
        _image_key: &str,
    ) -> crate::error::Result<crate::feishu::client::ImageAttachment> {
        Ok(crate::feishu::client::ImageAttachment {
            mime: "image/png".into(),
            data: vec![1, 2, 3, 4],
        })
    }
}

/// Serves scripted parts/permissions instead of a live OpenCode server.
pub struct MockBackend {
    pub parts: serde_json::Value,
    pub permissions: Vec<opencode::client::PermissionRequest>,
    /// Number of initial `list_permissions` calls to hang forever — simulates
    /// an in-flight request stuck on a half-open connection while the server
    /// restarts. Each hung call decrements the counter; once it reaches zero
    /// the call serves normally, so a bounded poller recovers on a later poll.
    pub hang_list_permissions: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `list_questions`.
    pub hang_list_questions: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `messages` (the external poller's
    /// per-session read).
    pub hang_messages: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `session_info` (the session
    /// subtitle fetch — a wedged first request on a freshly spawned server).
    pub hang_session_info: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `list_sessions` (the Lazy Start
    /// readiness probe).
    pub hang_list_sessions: Arc<std::sync::atomic::AtomicUsize>,
    /// When set, `messages` returns this as a fresh user message (simulates
    /// a message posted from OpenChamber).
    pub external_user_message: Option<String>,
    /// Per-session fresh user messages (simulates an external message posted on
    /// a SPECIFIC session). Takes precedence over `external_user_message` when
    /// a session has an entry, so tests can script an external message on a
    /// historical (non-active) session while the active one has none.
    pub external_user_messages: std::collections::HashMap<String, String>,
    /// Per-session cola-authored user messages: simulates cola's OWN prompt
    /// persisting on the store with a `msg_cola_` id (ADR-0026) — e.g. after a
    /// server died mid-turn and healed, so the poller sees it as newer than the
    /// stale watermark. Returned with the newest user-message slot taken, so a
    /// test can assert cola's own round is never notified as external.
    pub cola_user_messages: std::collections::HashMap<String, String>,
    /// Created time of each cola-authored message, captured on first read so it
    /// stays stable across polls.
    pub cola_user_created: Arc<std::sync::Mutex<std::collections::HashMap<String, i64>>>,
    /// When set, `messages` returns this as the assistant reply to the
    /// external user message (simulates OpenCode answering it), replacing the
    /// default assistant turn. Returned only once `external_reply_ready`
    /// flips, so tests can script the reply arriving on a LATER poll.
    pub external_reply_parts: Option<serde_json::Value>,
    /// Gates whether the `external_reply_parts` assistant turn is returned.
    /// The test holds a clone of this `Arc` and flips it after the
    /// notification card is sent, to simulate the model answering later.
    pub external_reply_ready: Arc<std::sync::atomic::AtomicBool>,
    /// Created time of the external user message, captured on first read so
    /// it stays stable across polls (the poller's watermark logic must not
    /// see the same message as "new" every call). A test may bump it via
    /// the `Arc` handle to simulate a SECOND external message arriving.
    pub external_user_created: Arc<std::sync::Mutex<Option<i64>>>,
    /// Records every `reply_permission` call: (request_id, reply).
    pub reply_permission_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    /// Request ids cola answered via `reply_permission` — filtered out of
    /// `list_permissions` like the real server drops a replied request. A test
    /// simulating resolution by ANOTHER client inserts the id here directly.
    pub replied_permissions: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    /// When true, `reply_permission` reports the request as already gone (404):
    /// simulates a click replayed after the request was resolved elsewhere (or
    /// after a cola restart cleared the in-memory guard).
    pub reply_permission_not_found: bool,
    /// Number of initial `reply_permission` calls to fail with a genuine
    /// (non-404) error — tests the guard rollback that lets the user retry.
    pub fail_reply_permission_count: Arc<std::sync::atomic::AtomicUsize>,
    /// When true, `reply_question`/`reject_question` report the request as
    /// already gone (404), like a question resolved elsewhere.
    pub reply_question_not_found: bool,
    /// Requests a test adds AFTER the app is built (simulating a request that
    /// arrives after the snapshot was sent, ADR-0028).
    pub extra_permissions: Arc<tokio::sync::Mutex<Vec<opencode::client::PermissionRequest>>>,
    /// session_id → server title (simulates OpenChamber's session title).
    /// `std::sync::Mutex` for interior mutability: `update_session_title`
    /// writes it through `&self` (the trait requires `&self`).
    pub session_titles: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// Pending questions served by `list_questions`.
    pub questions: Vec<opencode::client::QuestionRequest>,
    /// Records `reply_question` calls: (request_id, answers).
    pub reply_question_calls: Arc<tokio::sync::Mutex<Vec<QuestionReplyRecord>>>,
    /// Question ids cola answered/submitted/rejected via `reply_question` —
    /// filtered out of `list_questions` like the real server drops a replied
    /// request. A test simulating resolution by ANOTHER client inserts the id
    /// here directly.
    pub replied_questions: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    /// When set, `prompt` fails with this message (simulates a provider 503).
    pub prompt_error: Option<String>,
    /// Number of initial `prompt` calls to fail (for testing retry-after-error).
    pub fail_prompt_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Records every `prompt` call's text (asserts retry re-submits).
    pub prompt_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Records the number of images attached to each `prompt` call.
    pub prompt_images: Arc<tokio::sync::Mutex<Vec<usize>>>,
    /// Records the model passed to each `prompt` call ("provider/model").
    pub prompt_models: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Records the variant passed to each `prompt` call (the `/think` override).
    pub prompt_variants: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Records the agent passed to each `prompt` call.
    pub prompt_agents: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Records the message_id passed to each `prompt` call (asserts every cola
    /// prompt carries a `msg_cola_` id and retries reuse it, ADR-0026).
    pub prompt_message_ids: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// A callback run inside `prompt` just before it succeeds — lets a test
    /// mutate the session's working tree mid-turn (e.g. switch branches or
    /// leave an uncommitted file) to exercise the Turn Footer's turn-end
    /// refresh (ADR-0019).
    pub on_prompt: Option<Box<dyn Fn() + Send + Sync>>,
    /// Records every `prompt_async` call's text (asserts supplement path).
    pub prompt_async_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Records the number of images attached to each `prompt_async` call.
    pub prompt_async_images: Arc<tokio::sync::Mutex<Vec<usize>>>,
    /// Records the model passed to each `prompt_async` call.
    pub prompt_async_models: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Records the variant passed to each `prompt_async` call.
    pub prompt_async_variants: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Records the agent passed to each `prompt_async` call.
    pub prompt_async_agents: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Records the message_id passed to each `prompt_async` call (asserts the
    /// supplement path carries a `msg_cola_` id, ADR-0026).
    pub prompt_async_message_ids: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// The session id `create_session` returns.
    pub session_id: String,
    /// When true, `prompt` 404s for any session id other than `session_id`
    /// (simulates a stale mapping to a session that no longer exists).
    pub stale_session_404: bool,
    /// child session_id → parent session_id, served by `session_info`
    /// (simulates sub-task sessions created by the `task` tool).
    pub session_parents: std::collections::HashMap<String, String>,
    /// The shared store served by `list_sessions` (for `/list`, `/attach`,
    /// `/switch` tests).
    pub session_list: Vec<opencode::client::SessionListInfo>,
    /// Records `update_session_title` calls: (session_id, title).
    pub update_title_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    /// Counts `list_sessions` invocations (asserts the 30 s cache).
    pub list_sessions_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Available agents served by `list_agents` (for the `/agent` card).
    pub agents: Vec<opencode::client::AgentInfo>,
    /// Available models grouped by provider, served by `list_models` (for the
    /// `/model` card).
    pub provider_models: Vec<opencode::client::ProviderModels>,
    /// The configured default model served by `configured_default_model` (the
    /// second rung of the `/think` effective-model resolution).
    pub default_model: Option<opencode::client::ModelInfo>,
    /// The server-recorded session model served by `session_info` (the third
    /// rung of the `/think` effective-model resolution).
    pub session_model: Option<opencode::client::SessionModel>,
    /// Per-session server status served by `session_status` (session_id →
    /// status). A missing key means idle (matches the server: a finished run is
    /// removed from the status map). `Some(None)` inside simulates a session
    /// present but with an unrecognised status type (unknown).
    pub session_statuses: std::collections::HashMap<String, Option<opencode::client::SessionStatus>>,
    /// When set, `session_status` fails with this message (simulates a read
    /// failure — the caller must not guess a status).
    pub session_status_error: Option<String>,
    /// Scripts the ADR-0028 busy→idle race: the first `session_status` read
    /// returns Busy (and clears the flag), later reads serve the map.
    pub status_busy_once: std::sync::atomic::AtomicBool,
}

impl MockBackend {
    pub fn new(parts: serde_json::Value) -> Self {
        Self {
            parts,
            permissions: Vec::new(),
            hang_list_permissions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_list_questions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_messages: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_session_info: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_list_sessions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            external_user_message: None,
            external_user_messages: std::collections::HashMap::new(),
            cola_user_messages: std::collections::HashMap::new(),
            cola_user_created: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            external_reply_parts: None,
            external_reply_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            external_user_created: Arc::new(std::sync::Mutex::new(None)),
            reply_permission_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            replied_permissions: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            reply_permission_not_found: false,
            fail_reply_permission_count: std::sync::atomic::AtomicUsize::new(0).into(),
            reply_question_not_found: false,
            extra_permissions: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            session_titles: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            questions: Vec::new(),
            reply_question_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            replied_questions: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            prompt_error: None,
            fail_prompt_count: std::sync::atomic::AtomicUsize::new(0).into(),
            prompt_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_images: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_models: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_variants: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_agents: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_message_ids: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            on_prompt: None,
            prompt_async_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_images: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_models: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_variants: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_agents: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_message_ids: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            session_id: "ses_test".into(),
            stale_session_404: false,
            session_parents: std::collections::HashMap::new(),
            session_list: Vec::new(),
            update_title_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            list_sessions_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            agents: Vec::new(),
            provider_models: Vec::new(),
            default_model: None,
            session_model: None,
            session_statuses: std::collections::HashMap::new(),
            session_status_error: None,
            status_busy_once: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Shared recording/error policy for the question reply endpoints: record
    /// the call, honor a scripted 404, then mark the request replied (like the
    /// real server drops a replied request from the pending list).
    async fn record_question_reply(
        &self,
        request_id: &str,
        answers: Vec<Vec<String>>,
    ) -> crate::error::Result<()> {
        self.reply_question_calls
            .lock()
            .await
            .push((request_id.to_string(), answers));
        if self.reply_question_not_found {
            return Err(crate::error::BridgeError::NotFound(format!(
                "question {request_id}"
            )));
        }
        self.replied_questions.lock().await.insert(request_id.to_string());
        Ok(())
    }
}

/// Test scaffolding: while `counter` is positive, hang this call forever.
/// Decrements once per hung call, so a later poll (after a caller-side timeout
/// cancelled the first) serves normally. A future awaiting forever is exactly
/// the half-open-connection failure mode: only a caller-side timeout can end it.
async fn hang_if_scripted(counter: &std::sync::atomic::AtomicUsize) {
    if counter.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        std::future::pending::<()>().await;
    }
}

#[async_trait::async_trait]
impl opencode::Backend for MockBackend {
    fn new_session_input(&self, directory: Option<&str>) -> opencode::client::CreateSessionInput {
        opencode::client::CreateSessionInput {
            id: None,
            agent: None,
            model: Some(opencode::client::ModelInfo {
                id: "m".into(),
                provider_id: "p".into(),
                variant: None,
            }),
            location: directory.map(|d| opencode::client::Location {
                directory: d.to_string(),
            }),
        }
    }

    async fn create_session(
        &self,
        _i: &opencode::client::CreateSessionInput,
    ) -> crate::error::Result<opencode::client::Session> {
        Ok(opencode::client::Session {
            id: self.session_id.clone(),
            project_id: None,
            agent: None,
            title: None,
            location: None,
            cost: None,
            time: None,
        })
    }

    async fn list_sessions(&self) -> crate::error::Result<Vec<opencode::client::SessionListInfo>> {
        hang_if_scripted(&self.hang_list_sessions).await;
        self.list_sessions_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.session_list.clone())
    }

    async fn update_session_title(&self, session_id: &str, title: &str) -> crate::error::Result<()> {
        self.update_title_calls
            .lock()
            .await
            .push((session_id.to_string(), title.to_string()));
        self.session_titles
            .lock()
            .unwrap()
            .insert(session_id.to_string(), title.to_string());
        Ok(())
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        images: &[opencode::client::ImageInput],
        _model: Option<&opencode::client::ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> crate::error::Result<opencode::client::PromptResponse> {
        self.prompt_calls.lock().await.push(text.to_string());
        self.prompt_images.lock().await.push(images.len());
        self.prompt_message_ids
            .lock()
            .await
            .push(message_id.map(|s| s.to_string()));
        self.prompt_models
            .lock()
            .await
            .push(_model.map(|m| format!("{}/{}", m.provider_id, m.id)));
        self.prompt_variants
            .lock()
            .await
            .push(variant.map(|v| v.to_string()));
        self.prompt_agents.lock().await.push(agent.map(|s| s.to_string()));
        if self.stale_session_404 && session_id != self.session_id {
            return Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()));
        }
        if self.fail_prompt_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.fail_prompt_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                "Simulated provider failure".into(),
            ));
        }
        if let Some(err) = &self.prompt_error {
            return Err(crate::error::BridgeError::OpenCode(err.clone()));
        }
        if let Some(hook) = &self.on_prompt {
            hook();
        }
        Ok(opencode::client::PromptResponse {
            id: "msg_assist".into(),
            session_id: Some(session_id.to_string()),
            admitted_seq: None,
            parent_id: Some("msg_user".into()),
            error: None,
            parts: self.parts.clone(),
        })
    }

    async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        images: &[opencode::client::ImageInput],
        _model: Option<&opencode::client::ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> crate::error::Result<()> {
        self.prompt_async_calls
            .lock()
            .await
            .push(format!("{}:{}", session_id, text));
        self.prompt_async_images.lock().await.push(images.len());
        self.prompt_async_message_ids
            .lock()
            .await
            .push(message_id.map(|s| s.to_string()));
        self.prompt_async_models
            .lock()
            .await
            .push(_model.map(|m| format!("{}/{}", m.provider_id, m.id)));
        self.prompt_async_variants
            .lock()
            .await
            .push(variant.map(|v| v.to_string()));
        self.prompt_async_agents
            .lock()
            .await
            .push(agent.map(|s| s.to_string()));
        Ok(())
    }

    async fn messages(
        &self,
        _session_id: &str,
    ) -> crate::error::Result<Vec<opencode::client::SessionMessage>> {
        hang_if_scripted(&self.hang_messages).await;
        let now = chrono::Utc::now().timestamp_millis();
        let mut msgs: Vec<opencode::client::SessionMessage> = Vec::new();
        // cola's OWN user message persisting on the store (ADR-0026): id starts
        // with `msg_cola_`, created time stable across polls. Simulates a prompt
        // cola sent that the poller must recognise as cola-authored even when it
        // surfaces AFTER a stale watermark (server died mid-turn then healed).
        if let Some(cola_text) = self.cola_user_messages.get(_session_id) {
            let created = {
                let mut map = self.cola_user_created.lock().unwrap();
                *map.entry(_session_id.to_string())
                    .or_insert_with(|| chrono::Utc::now().timestamp_millis())
            };
            msgs.push(opencode::client::SessionMessage {
                info: opencode::client::MessageInfo {
                    id: "msg_cola_mock_user".into(),
                    role: Some("user".into()),
                    parent_id: None,
                    time: Some(opencode::client::MessageTime { created }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{ "type": "text", "text": cola_text }]),
            });
        }
        // When set, simulate a user message posted by ANOTHER client (e.g.
        // OpenChamber), for the external-message poller tests. If an AI
        // reply is also set (and `external_reply_ready` has flipped), return
        // it as the assistant turn — simulates OpenCode answering the
        // shared-store message.
        let text = self
            .external_user_messages
            .get(_session_id)
            .cloned()
            .or_else(|| self.external_user_message.clone());
        if let Some(text) = text {
            // Stable created time: captured once, so the same message is not
            // seen as "new" on every poll.
            let created = {
                let mut slot = self.external_user_created.lock().unwrap();
                *slot.get_or_insert_with(|| chrono::Utc::now().timestamp_millis())
            };
            // If cola's own message is also present, guarantee the external one
            // is NEWEST — it was posted after cola's (the heal scenario).
            let created = {
                let map = self.cola_user_created.lock().unwrap();
                map.get(_session_id)
                    .map(|c| created.max(c + 1000))
                    .unwrap_or(created)
            };
            msgs.push(opencode::client::SessionMessage {
                info: opencode::client::MessageInfo {
                    id: "msg_ext_user".into(),
                    role: Some("user".into()),
                    parent_id: None,
                    time: Some(opencode::client::MessageTime { created }),
                    model_id: None,
                    provider_id: None,
                    tokens: None,
                },
                parts: serde_json::json!([{ "type": "text", "text": text }]),
            });
            if self
                .external_reply_ready
                .load(std::sync::atomic::Ordering::SeqCst)
                && let Some(parts) = &self.external_reply_parts
            {
                msgs.push(opencode::client::SessionMessage {
                    info: opencode::client::MessageInfo {
                        id: "msg_ext_assist".into(),
                        role: Some("assistant".into()),
                        parent_id: Some("msg_ext_user".into()),
                        time: Some(opencode::client::MessageTime {
                            created: created + 1000,
                        }),
                        model_id: None,
                        provider_id: None,
                        tokens: None,
                    },
                    parts: parts.clone(),
                });
            }
            return Ok(msgs);
        }
        // No cola-authored or external user message modeled: return only the
        // assistant side of cola's own turn (the default rendering path).
        if !msgs.is_empty() {
            return Ok(msgs);
        }
        Ok(vec![opencode::client::SessionMessage {
            info: opencode::client::MessageInfo {
                id: "msg_assist".into(),
                role: Some("assistant".into()),
                parent_id: Some("msg_user".into()),
                time: Some(opencode::client::MessageTime { created: now + 1000 }),
                model_id: None,
                provider_id: None,
                tokens: None,
            },
            parts: self.parts.clone(),
        }])
    }

    async fn list_permissions(
        &self,
        _d: Option<&str>,
    ) -> crate::error::Result<Vec<opencode::client::PermissionRequest>> {
        hang_if_scripted(&self.hang_list_permissions).await;
        let replied = self.replied_permissions.lock().await;
        let mut out: Vec<_> = self
            .permissions
            .iter()
            .filter(|p| !replied.contains(&p.request_id))
            .cloned()
            .collect();
        // Post-build scripting: a test may add requests AFTER the app is built
        // (simulating a request arriving after the snapshot was sent).
        out.extend(self.extra_permissions.lock().await.iter().cloned());
        Ok(out)
    }

    async fn list_questions(
        &self,
        _d: Option<&str>,
    ) -> crate::error::Result<Vec<opencode::client::QuestionRequest>> {
        hang_if_scripted(&self.hang_list_questions).await;
        let replied = self.replied_questions.lock().await;
        Ok(self
            .questions
            .iter()
            .filter(|q| !replied.contains(&q.id))
            .cloned()
            .collect())
    }

    async fn model_context_window(&self, _provider: &str, _model: &str) -> crate::error::Result<Option<i64>> {
        Ok(Some(100_000))
    }

    fn configured_default_model(&self) -> Option<opencode::client::ModelInfo> {
        self.default_model.clone()
    }

    async fn list_agents(&self) -> Vec<opencode::client::AgentInfo> {
        self.agents.clone()
    }

    async fn list_models(&self) -> Vec<opencode::client::ProviderModels> {
        self.provider_models.clone()
    }

    async fn reply_question(
        &self,
        request_id: &str,
        answers: &[Vec<String>],
        _d: Option<&str>,
    ) -> crate::error::Result<()> {
        self.record_question_reply(request_id, answers.to_vec()).await
    }

    async fn reject_question(&self, request_id: &str, _d: Option<&str>) -> crate::error::Result<()> {
        self.record_question_reply(request_id, vec![vec!["__reject__".to_string()]])
            .await
    }

    async fn reply_permission(&self, r: &str, reply: &str, _d: Option<&str>) -> crate::error::Result<()> {
        self.reply_permission_calls
            .lock()
            .await
            .push((r.to_string(), reply.to_string()));
        if self.reply_permission_not_found {
            return Err(crate::error::BridgeError::NotFound(format!("permission {r}")));
        }
        if self
            .fail_reply_permission_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_reply_permission_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                "simulated permission reply failure".into(),
            ));
        }
        // Like the real server, a replied request leaves the pending list.
        self.replied_permissions.lock().await.insert(r.to_string());
        Ok(())
    }

    async fn session_info(
        &self,
        session_id: &str,
        _d: Option<&str>,
    ) -> crate::error::Result<opencode::client::SessionInfo> {
        hang_if_scripted(&self.hang_session_info).await;
        Ok(opencode::client::SessionInfo {
            id: session_id.to_string(),
            parent_id: self.session_parents.get(session_id).cloned(),
            title: self.session_titles.lock().unwrap().get(session_id).cloned(),
            model: self.session_model.clone(),
        })
    }

    async fn session_status(
        &self,
        session_id: &str,
        _d: Option<&str>,
    ) -> crate::error::Result<Option<opencode::client::SessionStatus>> {
        if let Some(err) = &self.session_status_error {
            return Err(crate::error::BridgeError::OpenCode(err.clone()));
        }
        // Scripts the ADR-0028 busy→idle race: the FIRST read reports Busy
        // (the snapshot gather sees a mid-flight turn), later reads serve the
        // map (the follow's arm-time check sees the turn already finished).
        if self
            .status_busy_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(Some(opencode::client::SessionStatus::Busy));
        }
        // A scripted entry is served verbatim (`Some(None)` → unknown); a
        // missing key means idle (the server removes finished runs).
        Ok(match self.session_statuses.get(session_id) {
            Some(status) => *status,
            None => Some(opencode::client::SessionStatus::Idle),
        })
    }

    async fn interrupt(&self, _s: &str) -> crate::error::Result<()> {
        Ok(())
    }
    async fn compact(&self, _s: &str) -> crate::error::Result<()> {
        Ok(())
    }
    async fn reconnect(&self, _url: &str, _password: &str) -> crate::error::Result<()> {
        Ok(())
    }
    fn base_url(&self) -> String {
        "http://mock".into()
    }

    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn opencode::DirectoryBackend> {
        Arc::new(opencode::BackendDirectory::new(self, directory.to_string()))
    }
}

pub fn test_config(session_file: &std::path::Path) -> crate::config::Config {
    crate::config::Config {
        opencode: crate::config::OpenCodeConfig {
            url: Some("http://localhost:1".into()),
            model: Some("test/model".into()),
            start_server: crate::config::ServerStartPolicy::Auto,
        },
        feishu: crate::config::FeishuConfig {
            app_id: "app".into(),
            app_secret: "secret".into(),
        },
        bridge: crate::config::BridgeConfig {
            session_file: session_file.to_path_buf(),
            work_dir: None,
            group_completion_notice: true,
            log_days: 14,
        },
    }
}

/// The parts a real assistant turn produces: reasoning → tool → text.
pub fn realistic_parts() -> serde_json::Value {
    serde_json::json!([
        { "id": "prt_s1", "type": "step-start", "snapshot": "x" },
        { "id": "prt_r1", "type": "reasoning", "text": "用户想让我分析目录。" },
        { "id": "prt_t1", "type": "tool", "tool": "bash", "callID": "call_1",
          "state": { "status": "completed", "input": { "command": "ls -la" }, "output": "src/\nCargo.toml\n" } },
        { "id": "prt_f1", "type": "step-finish", "reason": "tool-calls" },
        { "id": "prt_s2", "type": "step-start", "snapshot": "x" },
        { "id": "prt_txt", "type": "text", "text": "当前目录有 src/ 和 Cargo.toml。" },
        { "id": "prt_f2", "type": "step-finish", "reason": "stop" },
    ])
}

/// A prompt whose answer is far longer than one card's text budget, so it
/// must flow across continuation cards (no plain-text fallback anymore).
pub fn long_answer_parts() -> serde_json::Value {
    // 1200 × 6 chars = 7200 chars, above MAX_CARD_TEXT_CHARS (6000).
    let long_text = "很长的回答。".repeat(1200);
    serde_json::json!([
        { "id": "prt_s1", "type": "step-start", "snapshot": "x" },
        { "id": "prt_txt", "type": "text", "text": long_text },
        { "id": "prt_f1", "type": "step-finish", "reason": "stop" },
    ])
}

/// Build a `ModelOption` with the given id and declared variants.
pub(crate) fn model_option(id: &str, variants: &[&str]) -> crate::opencode::client::ModelOption {
    crate::opencode::client::ModelOption {
        id: id.to_string(),
        variants: variants.iter().map(|s| s.to_string()).collect(),
    }
}

pub(crate) async fn build_app(
    cfg: crate::config::Config,
    backend: MockBackend,
) -> (Arc<App>, Arc<RecordingPlatform>) {
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
    (app, platform)
}

/// Build a plain text `IncomingMessage` for tests (no parent, no images).
pub(crate) fn incoming(
    message_id: String,
    chat_id: String,
    chat_type: String,
    thread_id: Option<String>,
    text: String,
    requester: Option<String>,
) -> crate::bridge::IncomingMessage {
    crate::bridge::IncomingMessage {
        message_id,
        chat_id,
        chat_type,
        thread_id,
        parent_id: None,
        text,
        images: vec![],
        requester_open_id: requester,
    }
}

/// Create a temp work dir, set it as the process cwd (sessions are created
/// in cwd, and tests must never operate in the cola repo) and return it.
/// The returned TempDir must stay alive for the test's duration.
pub(crate) fn test_work_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    dir
}

/// Run a git command in `dir`, panicking on failure (test setup helper).
pub(crate) fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A temp git repo on branch `main` with one commit — a real work context
/// for the Turn Footer tests (ADR-0019). The returned TempDir must stay
/// alive for the test's duration.
pub(crate) fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git_in(dir.path(), &["init", "-b", "main"]);
    git_in(dir.path(), &["config", "user.email", "test@example.com"]);
    git_in(dir.path(), &["config", "user.name", "test"]);
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    git_in(dir.path(), &["add", "a.txt"]);
    git_in(dir.path(), &["commit", "-m", "init"]);
    dir
}

/// The last card the app flushed in place — the finalized card.
pub(crate) async fn final_card(platform: &RecordingPlatform) -> serde_json::Value {
    let calls = platform.calls.lock().await;
    calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .expect("the final card must be updated in place")
}

/// Map `entry` as its thread's active session and persist the store — the
/// setup tests need instead of reaching into `app.sessions` directly.
pub(crate) async fn seed_entry(app: &Arc<App>, entry: crate::config::SessionEntry) {
    let mut store = app.sessions.lock().await;
    store.set_active(entry);
    store
        .persist()
        .expect("seed_entry: persisting the seeded store failed");
}

/// Seed the cover-title cache for `session_id` (the topic cover card's title
/// memory) — the `app.core` setup tests would otherwise do by hand.
pub(crate) async fn seed_cover_title(app: &Arc<App>, session_id: &str, title: &str) {
    app.core.cover_titles.lock().await.insert(
        session_id.into(),
        crate::bridge::core::CoverTitle {
            title: title.into(),
            model: None,
        },
    );
}

/// Seed the chat_1 lobby thread's session with the default entry shape.
pub(crate) async fn seed_session(app: &Arc<App>, session_id: &str, directory: &str) {
    seed_entry(
        app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: session_id.into(),
            directory: directory.into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
}

// ===== Session discovery & adoption (ADR-0008) =====

/// A helper: a session in the shared store with the given title/dir/id.
pub(crate) fn list_session(
    id: &str,
    title: &str,
    directory: &str,
    updated: i64,
) -> opencode::client::SessionListInfo {
    opencode::client::SessionListInfo {
        id: id.into(),
        title: title.into(),
        directory: directory.into(),
        parent_id: None,
        agent: None,
        model: None,
        time: Some(opencode::client::SessionTime {
            created: updated,
            updated,
            archived: None,
        }),
    }
}

/// ADR-0028 re-switch scenario: `ses_own1`（本项目会话, /work/cola）is both
/// the thread's mapped active session and the shared store's only session,
/// so `/switch 本项目` takes the mapped-hit branch of `handle_switch`. The
/// backend arrives pre-scripted with the cell under test (cola/external
/// newest message, status, pendings).
pub(crate) async fn build_reeswitch_app(
    backend: MockBackend,
) -> (Arc<App>, Arc<RecordingPlatform>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = backend;
    backend.session_list = vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)];
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_own1".into(),
            directory: "/work/cola".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    (app, platform, dir)
}

// ===== ADR-0028 adopt-time pending claim (ticket 05) =====

/// A permission request helper for the claim tests.
pub(crate) fn perm_request(
    request_id: &str,
    session_id: &str,
    pattern: &str,
) -> opencode::client::PermissionRequest {
    opencode::client::PermissionRequest {
        request_id: request_id.into(),
        session_id: Some(session_id.into()),
        permission: Some("bash".into()),
        patterns: vec![pattern.into()],
        metadata: None,
        always: vec![],
    }
}
