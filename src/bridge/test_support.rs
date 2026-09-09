#![cfg(test)]

use std::sync::Arc;

use crate::bridge::handler::App;
use crate::feishu;
use crate::opencode;

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
            reply_in_thread_thread_id: Some("omt_created_topic".into()),
            quoted_messages: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl feishu::Platform for RecordingPlatform {
    async fn get_ws_endpoint(&self) -> crate::error::Result<String> {
        Ok("wss://example.test".into())
    }

    async fn reply_card(&self, reply_to: &str, card: &serde_json::Value) -> crate::error::Result<String> {
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
    /// session_id → server title (simulates OpenChamber's session title).
    /// `std::sync::Mutex` for interior mutability: `update_session_title`
    /// writes it through `&self` (the trait requires `&self`).
    pub session_titles: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// Pending questions served by `list_questions`.
    pub questions: Vec<opencode::client::QuestionRequest>,
    /// Records `reply_question` calls: (request_id, answers).
    pub reply_question_calls: Arc<tokio::sync::Mutex<Vec<QuestionReplyRecord>>>,
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
}

impl MockBackend {
    pub fn new(parts: serde_json::Value) -> Self {
        Self {
            parts,
            permissions: Vec::new(),
            external_user_message: None,
            external_user_messages: std::collections::HashMap::new(),
            cola_user_messages: std::collections::HashMap::new(),
            cola_user_created: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            external_reply_parts: None,
            external_reply_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            external_user_created: Arc::new(std::sync::Mutex::new(None)),
            reply_permission_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            session_titles: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            questions: Vec::new(),
            reply_question_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_error: None,
            fail_prompt_count: std::sync::atomic::AtomicUsize::new(0).into(),
            prompt_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_images: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_models: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_variants: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_agents: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_message_ids: Arc::new(tokio::sync::Mutex::new(Vec::new())),
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
        }
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
        Ok(self.permissions.clone())
    }

    async fn list_questions(
        &self,
        _d: Option<&str>,
    ) -> crate::error::Result<Vec<opencode::client::QuestionRequest>> {
        Ok(self.questions.clone())
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
        self.reply_question_calls
            .lock()
            .await
            .push((request_id.to_string(), answers.to_vec()));
        Ok(())
    }

    async fn reject_question(&self, request_id: &str, _d: Option<&str>) -> crate::error::Result<()> {
        self.reply_question_calls
            .lock()
            .await
            .push((request_id.to_string(), vec![vec!["__reject__".to_string()]]));
        Ok(())
    }

    async fn reply_permission(&self, r: &str, reply: &str, _d: Option<&str>) -> crate::error::Result<()> {
        self.reply_permission_calls
            .lock()
            .await
            .push((r.to_string(), reply.to_string()));
        Ok(())
    }

    async fn session_info(
        &self,
        session_id: &str,
        _d: Option<&str>,
    ) -> crate::error::Result<opencode::client::SessionInfo> {
        Ok(opencode::client::SessionInfo {
            id: session_id.to_string(),
            parent_id: self.session_parents.get(session_id).cloned(),
            title: self.session_titles.lock().unwrap().get(session_id).cloned(),
            model: self.session_model.clone(),
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

pub(crate) mod integration_tests {
    use super::*;
    use crate::bridge::command::{Command, SwitchAction};

    /// Build a `ModelOption` with the given id and declared variants.
    fn model_option(id: &str, variants: &[&str]) -> crate::opencode::client::ModelOption {
        crate::opencode::client::ModelOption {
            id: id.to_string(),
            variants: variants.iter().map(|s| s.to_string()).collect(),
        }
    }

    async fn build_app(
        cfg: crate::config::Config,
        backend: MockBackend,
    ) -> (Arc<App>, Arc<RecordingPlatform>) {
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
        (app, platform)
    }

    /// Build a plain text `IncomingMessage` for tests (no parent, no images).
    fn incoming(
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
    fn test_work_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        dir
    }

    #[tokio::test]
    async fn handle_prompt_renders_reasoning_tools_and_text() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "分析一下目录".into(),
            None,
        ))
        .await;

        let calls = platform.calls.lock().await.clone();
        // First call must be the Loading reply card.
        assert!(matches!(calls.first(), Some(PlatformCall::ReplyCard { .. })));
        // At least one card update (flush) must follow.
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect();
        assert!(!updates.is_empty(), "expected card updates, got: {:?}", calls);

        let final_card = updates.last().unwrap().clone();
        let text = final_card.to_string();
        assert!(text.contains("✅"), "final header should be Done: {}", text);
        assert!(text.contains("推理过程"), "reasoning panel missing: {}", text);
        assert!(text.contains("bash"), "tool panel missing: {}", text);
        assert!(
            text.contains("当前目录有 src/ 和 Cargo.toml。"),
            "text missing: {}",
            text
        );
        assert!(text.contains("ls -la"), "tool input missing: {}", text);
    }

    #[tokio::test]
    async fn prompt_error_renders_error_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.prompt_error = Some("Streaming response failed: [503] The request queue is full.".into());
        let (app, platform) = build_app(cfg, backend).await;

        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        let calls = platform.calls.lock().await.clone();
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !updates.is_empty(),
            "expected an error card update, got: {:?}",
            calls
        );
        let card = updates.last().unwrap().to_string();
        assert!(card.contains("❌"), "error card header missing: {}", card);
        assert!(card.contains("503"), "error text missing: {}", card);
    }

    #[tokio::test]
    async fn error_card_retry_reuses_card_and_reruns_prompt() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        // First prompt fails (provider hiccup); the retry must succeed.
        let mock = MockBackend::new(realistic_parts());
        mock.fail_prompt_count
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let prompt_calls = mock.prompt_calls.clone();
        let prompt_ids = mock.prompt_message_ids.clone();
        let backend = Arc::new(mock);
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());

        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        // The error card must carry a retry button.
        let calls = platform.calls.lock().await.clone();
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect();
        let error_card = updates.last().unwrap().clone();
        let err_text = error_card.to_string();
        assert!(err_text.contains("❌"), "error card missing: {}", err_text);
        assert!(err_text.contains("重试"), "retry button missing: {}", err_text);

        // The card the retry will reuse: the loading reply card id.
        let card_id = match calls.first().unwrap() {
            PlatformCall::ReplyCard { card, .. } if card.to_string().contains("思考中") => "msg_reply",
            _ => panic!("expected a loading reply card first: {:?}", calls),
        };
        assert_eq!(card_id, "msg_reply");

        // User clicks the retry button.
        let retry = app
            .handle_card_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
            .await;
        assert!(retry.is_some(), "retry ack card expected");
        assert_eq!(retry.unwrap().toast.as_deref(), Some("正在重试..."));

        // The spawned retry re-runs the prompt on the SAME card, not a new reply.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

        let calls = platform.calls.lock().await.clone();
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            updates.last().unwrap().0,
            "msg_reply",
            "retry must update the original card, not send a new one: {:?}",
            calls
        );
        let final_card = updates.last().unwrap().1.to_string();
        assert!(
            final_card.contains("✅"),
            "retry should finish Done: {}",
            final_card
        );
        assert!(
            final_card.contains("当前目录有 src/ 和 Cargo.toml。"),
            "retried answer missing: {}",
            final_card
        );

        let backend_calls = prompt_calls.lock().await.clone();
        assert_eq!(backend_calls, vec!["hi".to_string(), "hi".to_string()]);
        // Both attempts are the SAME logical user message: a fresh `msg_cola_`
        // id on the first send, REUSED by the retry (ADR-0026) so the server
        // deduplicates instead of appending a second user message.
        let message_ids = prompt_ids.lock().await.clone();
        assert_eq!(message_ids.len(), 2, "two prompt attempts expected");
        let first = message_ids[0].clone().expect("prompt must carry a message id");
        assert!(
            first.starts_with("msg_cola_"),
            "cola prompt must self-identify: {}",
            first
        );
        assert_eq!(
            message_ids[1].as_deref(),
            Some(first.as_str()),
            "retry must reuse the failed attempt's message id"
        );
    }

    #[tokio::test]
    async fn group_completion_sends_notice_to_requester() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(incoming(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            Some("ou_requester".into()),
        ))
        .await;

        let calls = platform.calls.lock().await.clone();
        let notices: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::CompletionNotice {
                    reply_to,
                    open_id,
                    text,
                    ..
                } => Some((reply_to.clone(), open_id.clone(), text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            vec![(
                "msg_1".to_string(),
                "ou_requester".to_string(),
                "✅ 已完成。".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn group_completion_at_mentions_requester_when_name_resolvable() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut rp = RecordingPlatform::new();
        rp.user_names
            .insert("ou_requester".to_string(), "李明".to_string());
        let platform = Arc::new(rp);
        let app = Arc::new(
            App::new(
                cfg,
                Arc::new(MockBackend::new(realistic_parts())),
                platform.clone(),
            )
            .unwrap(),
        );

        app.handle_message(incoming(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            Some("ou_requester".into()),
        ))
        .await;

        let calls = platform.calls.lock().await.clone();
        let notices: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::CompletionNotice {
                    reply_to,
                    open_id,
                    name,
                    text,
                } => Some((reply_to.clone(), open_id.clone(), name.clone(), text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            vec![(
                "msg_1".to_string(),
                "ou_requester".to_string(),
                Some("李明".to_string()),
                "✅ 已完成。".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn p2p_prompt_sends_no_completion_notice() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(incoming(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            Some("ou_user".into()),
        ))
        .await;

        let calls = platform.calls.lock().await.clone();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, PlatformCall::CompletionNotice { .. })),
            "p2p must not send a completion notice: {:?}",
            calls
        );
    }

    #[tokio::test]
    async fn subtitle_falls_back_to_id_tail_without_server_title() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_01ba0ed03ffeRvYNWua6mg8d9c".into(),
                directory: "/tmp/x".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        // No server title → the id-tail alone identifies the session (no cola
        // side name to fall back on; the current prompt is never echoed).
        assert_eq!(
            crate::bridge::render::session_subtitle(&app.core, &key, "另一个问题").await,
            "01ba0ed"
        );
        assert_eq!(
            crate::bridge::render::session_subtitle(&app.core, &key, "你好").await,
            "01ba0ed"
        );
    }

    /// A server default title (`New session - ...`) is treated as absent — the
    /// id-tail is shown until the server generates a real title.
    #[tokio::test]
    async fn subtitle_ignores_server_default_title() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mock = MockBackend::new(realistic_parts());
        mock.session_titles.lock().unwrap().insert(
            "ses_00ea4e77cffez1fo4wrNuJyHF0".into(),
            "New session - 2026-08-28".into(),
        );
        let (app, _) = build_app(cfg, mock).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_00ea4e77cffez1fo4wrNuJyHF0".into(),
                directory: "/tmp/y".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }
        assert_eq!(
            crate::bridge::render::session_subtitle(&app.core, &key, "另一个问题").await,
            "00ea4e7"
        );
    }

    /// The card subtitle prefers the OpenCode server's own session title (what
    /// OpenChamber shows), not cola's `/new`-generated names.
    #[tokio::test]
    async fn subtitle_prefers_server_title() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mock = MockBackend::new(realistic_parts());
        mock.session_titles
            .lock()
            .unwrap()
            .insert("ses_test".into(), "OpenChamber 显示的标题".into());
        let (app, _) = build_app(cfg, mock).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/x".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }
        assert_eq!(
            crate::bridge::render::session_subtitle(&app.core, &key, "问题").await,
            "OpenChamber 显示的标题 · test"
        );
    }

    /// The card subtitle follows the server's live session title during a turn:
    /// OpenCode auto-renames a session after streaming, and `refresh_session_title`
    /// (called from the render poll loop) must update the card instead of leaving
    /// it on the "new session" default until restart.
    #[tokio::test]
    async fn refresh_session_title_updates_live_card_on_server_rename() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mock = MockBackend::new(realistic_parts());
        // The server already has a final auto-generated title.
        mock.session_titles
            .lock()
            .unwrap()
            .insert("ses_test".into(), "修复登录鉴权问题".into());
        let backend = Arc::new(mock);
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/x".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        // Simulate an in-flight turn whose card was captured with the OLD
        // default subtitle before the server auto-titled the session.
        let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
        acc.reply_to_message_id = Some("msg_1".into());
        acc.session_id = Some("ses_test".into());
        {
            let mut cards = app.cards.lock().await;
            cards.insert(
                "ses_test".into(),
                crate::bridge::streaming::CardSession::new(acc, None),
            );
        }

        // The server's live title differs → refresh must update the card subtitle.
        let refreshed = crate::bridge::render::refresh_session_title(&app.core, "ses_test").await;
        assert!(refreshed, "a server rename must refresh the card title");
        let title = app.cards.lock().await.get("ses_test").unwrap().acc.title.clone();
        assert_eq!(title, "修复登录鉴权问题 · test");
        // A second refresh with no further change must be a no-op (no churn).
        assert!(
            !crate::bridge::render::refresh_session_title(&app.core, "ses_test").await,
            "no change when the title already matches"
        );
        // No accumulator (a finished turn) → refresh is a no-op.
        app.cards.lock().await.remove("ses_test");
        assert!(!crate::bridge::render::refresh_session_title(&app.core, "ses_test").await);
    }

    /// ADR-0023: when the server auto-titles a session MID-TURN, the render
    /// tick that refreshes the live card's title must ALSO patch the topic
    /// cover card — the chat-list entry updates as early as the title agent
    /// finishes, not only when the turn completes.
    #[tokio::test]
    async fn refresh_session_title_syncs_cover_card_mid_turn() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mock = MockBackend::new(realistic_parts());
        mock.session_titles
            .lock()
            .unwrap()
            .insert("ses_test".into(), "修复登录鉴权问题".into());
        let (app, platform) = build_app(cfg, mock).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/x".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some("om_seed".into()),
                topic_root: Some("om_cover".into()),
                variant: None,
            });
        }
        app.core.cover_titles.lock().await.insert(
            "ses_test".into(),
            crate::bridge::core::CoverTitle {
                title: "cola".into(),
                model: None,
            },
        );
        // An in-flight turn whose card was captured with the OLD subtitle
        // (before the server auto-titled the session).
        let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
        acc.reply_to_message_id = Some("msg_1".into());
        acc.session_id = Some("ses_test".into());
        {
            let mut cards = app.cards.lock().await;
            cards.insert(
                "ses_test".into(),
                crate::bridge::streaming::CardSession::new(acc, None),
            );
        }

        let refreshed = crate::bridge::render::refresh_session_title(&app.core, "ses_test").await;
        assert!(refreshed, "the mid-turn title change must refresh the card");

        let calls = platform.calls.lock().await.clone();
        let patched = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { message_id, card } if message_id == "om_cover" => {
                    Some(card.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !patched.is_empty(),
            "the cover card must be patched on the same tick, got {calls:?}"
        );
        assert!(
            patched.last().unwrap().contains("修复登录鉴权问题"),
            "cover card must show the auto-title: {:?}",
            patched.last()
        );
        assert_eq!(
            app.core
                .cover_titles
                .lock()
                .await
                .get("ses_test")
                .map(|c| c.title.clone()),
            Some("修复登录鉴权问题".to_string())
        );
    }

    #[tokio::test]
    async fn long_answer_splits_across_cards_no_plain_text() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(long_answer_parts())).await;

        app.handle_message(incoming(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        let calls = platform.calls.lock().await.clone();
        // Every message cola sent must be a CARD (ReplyCard / UpdateMessage) —
        // no separate plain-text message for long answers.
        assert!(
            calls.iter().all(|c| {
                matches!(
                    c,
                    PlatformCall::ReplyCard { .. } | PlatformCall::UpdateMessage { .. }
                )
            }),
            "long answer must stay on cards, got: {:?}",
            calls
        );
        // The FULL text must be present across the cards (not truncated).
        let all_cards: String = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
                PlatformCall::UpdateMessage { card, .. } => Some(card.to_string()),
                _ => None,
            })
            .collect();
        assert!(
            all_cards.contains("很长的回答。"),
            "full answer must appear on a card: {}",
            all_cards
        );
        let expected = "很长的回答。".repeat(1200);
        assert!(
            all_cards.chars().filter(|c| *c != '"').count() >= expected.chars().count(),
            "all text must be delivered (preview-only would lose the tail)"
        );
    }

    #[tokio::test]
    async fn short_answer_stays_in_card_no_extra_message() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(incoming(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        let calls = platform.calls.lock().await.clone();
        assert!(
            calls.iter().all(|c| matches!(
                c,
                PlatformCall::ReplyCard { .. } | PlatformCall::UpdateMessage { .. }
            )),
            "answer must stay on cards: {:?}",
            calls
        );
    }

    #[tokio::test]
    async fn permission_poller_sends_card_and_card_action_replies() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_1".into(),
            session_id: Some("ses_test".into()),
            permission: Some("bash".into()),
            patterns: vec!["ls -la".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let (app, _platform) = build_app(cfg, backend).await;

        // Seed a session + accumulator so the poller has a reply target.
        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        // Run the permission poller briefly.
        tokio::spawn({
            let app = app.clone();
            async move {
                app.permission
                    .poll_interval_ms
                    .store(50, std::sync::atomic::Ordering::Relaxed);
                let _ = app.permission.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // The session has an active streaming card, so the permission is surfaced
        // INLINE on it (one-card-per-turn) — not as a separate card.
        let perm_inline = app
            .cards
            .lock()
            .await
            .get("ses_test")
            .expect("accumulator exists")
            .acc
            .pending_permissions
            .clone();
        assert_eq!(perm_inline.len(), 1, "permission should be inlined");
        assert_eq!(perm_inline[0].request_id, "per_1");
        // The streaming card itself renders the inline permission section.
        let card = app.cards.lock().await.get("ses_test").unwrap().acc.build_card();
        let card_text = card.to_string();
        assert!(
            card_text.contains("权限请求"),
            "inline section missing: {}",
            card_text
        );
        assert!(
            card_text.contains("ls -la"),
            "permission body missing: {}",
            card_text
        );
        assert!(
            card_text.contains("允许一次"),
            "allow button missing: {}",
            card_text
        );

        // Simulate the user clicking "允许一次" — answered inline, so the ack
        // carries a toast but must NOT replace the streaming card.
        let value = serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_test",
            "request_id": "per_1",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        });
        let result = app.handle_card_action(value).await;
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(
            result.card.is_none(),
            "inline answer must not replace the streaming card"
        );
        // A toast gives the client instant feedback on the button press.
        assert_eq!(result.toast.as_deref(), Some("已允许本次执行"));
        // The inline section is removed from the accumulator.
        assert!(
            app.cards
                .lock()
                .await
                .get("ses_test")
                .unwrap()
                .acc
                .pending_permissions
                .is_empty()
        );
    }

    /// Clicking "开启自动授权" on a permission card turns on the session's
    /// Auto-Accept (cola-side `/autoaccept`) AND approves the current pending
    /// permission — all without posting a new message. The backend sees the
    /// normal "once" reply, never "autoaccept".
    #[tokio::test]
    async fn autoaccept_toggle_on_permission_card_flips_flag_and_approves() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.permissions = vec![
            opencode::client::PermissionRequest {
                request_id: "per_aa_toggle".into(),
                session_id: Some("ses_test".into()),
                permission: Some("bash".into()),
                patterns: vec!["ls -la".into()],
                metadata: None,
                always: Vec::new(),
            },
            // A second, already-pending request for the same session — the
            // toggle approves it too and must drop its inline section in the
            // same interaction, not leave it lingering until the next poll.
            opencode::client::PermissionRequest {
                request_id: "per_aa_other".into(),
                session_id: Some("ses_test".into()),
                permission: Some("edit".into()),
                patterns: vec!["src/main.rs".into()],
                metadata: None,
                always: Vec::new(),
            },
        ];
        let perm_calls = backend.reply_permission_calls.clone();
        let (app, platform) = build_app(cfg, backend).await;

        // Seed a session (auto_accept defaults to false) + a live streaming card
        // so the poller surfaces the permission INLINE.
        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        tokio::spawn({
            let app = app.clone();
            async move {
                app.permission
                    .poll_interval_ms
                    .store(50, std::sync::atomic::Ordering::Relaxed);
                let _ = app.permission.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Permission inlined on the streaming card, auto_accept still off.
        assert_eq!(
            app.cards
                .lock()
                .await
                .get("ses_test")
                .unwrap()
                .acc
                .pending_permissions
                .len(),
            2
        );
        assert!(
            !app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .unwrap()
                .auto_accept
        );

        // Click the toggle.
        let value = serde_json::json!({
            "action": "perm",
            "reply": "autoaccept",
            "session_id": "ses_test",
            "request_id": "per_aa_toggle",
            "perm_label": "✅ 已开启自动授权",
            "perm_color": "blue",
            "perm_body": "bash",
        });
        let result = app.handle_card_action(value).await.expect("toggle result");
        assert!(
            result.card.is_none(),
            "inline toggle must not replace the streaming card"
        );
        assert_eq!(result.toast.as_deref(), Some("已开启自动授权"));

        // Both pending permissions approved with "once"; flag flipped.
        let mut calls = perm_calls.lock().await.clone();
        calls.sort();
        assert_eq!(
            calls,
            vec![
                ("per_aa_other".to_string(), "once".to_string()),
                ("per_aa_toggle".to_string(), "once".to_string()),
            ],
            "the toggle should approve the current AND other pending permissions"
        );
        assert!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .unwrap()
                .auto_accept,
            "auto_accept flag should flip on"
        );
        assert!(
            app.cards
                .lock()
                .await
                .get("ses_test")
                .unwrap()
                .acc
                .pending_permissions
                .is_empty(),
            "ALL inline sections removed after the toggle, not just the clicked one"
        );
        // No NEW message: only the loading card + final card, no text reply.
        let sent = platform.calls.lock().await.clone();
        assert!(
            !sent.iter().any(|c| matches!(c, PlatformCall::ReplyText { .. })),
            "toggle must not send a new message"
        );
    }

    #[tokio::test]
    async fn auto_accept_session_answers_permission_without_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_aa".into(),
            session_id: Some("ses_test".into()),
            permission: Some("bash".into()),
            patterns: vec!["ls".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let perm_calls = mock.reply_permission_calls.clone();
        let (app, platform) = build_app(cfg, mock).await;

        // Enable `/autoaccept` on the session.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: true,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }

        tokio::spawn({
            let app = app.clone();
            async move {
                app.permission
                    .poll_interval_ms
                    .store(50, std::sync::atomic::Ordering::Relaxed);
                let _ = app.permission.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Auto-accepted: reply_permission called with "once", no card sent.
        let calls = perm_calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![("per_aa".to_string(), "once".to_string())],
            "auto-accept should reply once"
        );
        let sent = platform.calls.lock().await.clone();
        assert!(
            !sent.iter().any(|c| {
                if let PlatformCall::ReplyCard { card, .. } = c {
                    card.to_string().contains("权限请求")
                } else {
                    false
                }
            }),
            "no permission card should be sent for an auto-accept session: {:?}",
            sent
        );
    }

    /// `/autoaccept on` must also approve requests that were ALREADY pending
    /// (seen before the flag existed), not just future ones. Otherwise the
    /// poller's `seen` set leaves old permission cards hanging forever.
    #[tokio::test]
    async fn autoaccept_on_approves_already_pending_permission() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_pending".into(),
            session_id: Some("ses_test".into()),
            permission: Some("bash".into()),
            patterns: vec!["ls -la".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let perm_calls = mock.reply_permission_calls.clone();
        let (app, _platform) = build_app(cfg, mock).await;

        // Session already mapped but autoaccept OFF — the permission would have
        // been surfaced as a card before the user enabled it.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }

        // Now the user turns autoaccept on via the command.
        crate::bridge::command::handle_command(
            &app.core,
            Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Set(true)),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_cmd",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The already-pending request was approved with "once" immediately.
        let calls = perm_calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![("per_pending".to_string(), "once".to_string())],
            "turning autoaccept on should approve already-pending requests"
        );
        // The flag is persisted for future requests.
        let entry = {
            let store = app.sessions.lock().await;
            store
                .get_active(&crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()))
                .cloned()
        };
        assert!(entry.unwrap().auto_accept, "autoaccept flag should persist");
    }

    /// Sub-task child sessions carry their own sessionID; `/autoaccept on` on
    /// the parent must reach through the parent chain and approve their
    /// pending permissions too.
    #[tokio::test]
    async fn autoaccept_on_approves_child_session_permission() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_child".into(),
            session_id: Some("ses_child".into()),
            permission: Some("bash".into()),
            patterns: vec!["rm -rf x".into()],
            metadata: None,
            always: Vec::new(),
        }];
        mock.session_parents.insert("ses_child".into(), "ses_test".into());
        let perm_calls = mock.reply_permission_calls.clone();
        let (app, _platform) = build_app(cfg, mock).await;

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Set(true)),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_cmd",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = perm_calls.lock().await.clone();
        assert_eq!(
            calls,
            vec![("per_child".to_string(), "once".to_string())],
            "child-session permission should be approved via the parent chain"
        );
    }

    #[tokio::test]
    async fn stale_permission_card_marked_handled_when_resolved_elsewhere() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        // No pending permissions on the server — the card cola sent is stale.
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        app.permission
            .sent_cards
            .lock()
            .await
            .insert("per_stale".into(), ("om_sent_card".into(), "bash ls -la".into()));

        tokio::spawn({
            let app = app.clone();
            async move {
                app.permission
                    .poll_interval_ms
                    .store(50, std::sync::atomic::Ordering::Relaxed);
                let _ = app.permission.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let calls = platform.calls.lock().await.clone();
        let stale = calls.iter().find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_sent_card" => {
                Some(card.clone())
            }
            _ => None,
        });
        let stale = stale.expect("stale permission card should be marked");
        assert!(
            stale.to_string().contains("已处理"),
            "stale card should show as handled: {}",
            stale
        );
        assert!(
            stale.to_string().contains("bash ls -la"),
            "stale card should keep the original request text: {}",
            stale
        );
    }

    #[tokio::test]
    async fn external_message_from_shared_store_notifies_feishu() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.external_user_message = Some("OpenChamber 里发的消息".to_string());
        let (app, platform) = build_app(cfg, mock).await;

        // A known session whose chat the notification goes to.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
                session_id: "ses_ext".into(),
                directory: "/tmp/ext".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        // Baseline: a minute ago, so the fresh user message is "new".
        let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_ext".into(), watermark);

        app.external
            .poll_interval_ms
            .store(50, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let calls = platform.calls.lock().await.clone();
        let notify = calls.iter().find_map(|c| match c {
            PlatformCall::SendCard { card, .. } if card.to_string().contains("有新消息") => {
                Some(card.clone())
            }
            _ => None,
        });
        let notify = notify.expect("external message should produce a notification card");
        assert!(
            notify.to_string().contains("OpenChamber 里发的消息"),
            "notification should preview the message: {}",
            notify
        );
    }

    /// ADR-0026 regression (observed 2026-09-09): when a server dies mid-turn and
    /// cola heals by starting its own server on the same store, cola's OWN
    /// persisted user message (`msg_cola_` id) surfaces as newer than the stale
    /// Sync Watermark. It must NOT be re-notified into Feishu as an external
    /// message — authorship is carried by the id prefix, not by the watermark.
    #[tokio::test]
    async fn cola_own_message_after_heal_is_never_notified_external() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        // cola's own prompt persisted on the store before the crash (a
        // `msg_cola_` id, exactly what the real server echoes back).
        mock.cola_user_messages.insert(
            "ses_ext".into(),
            "可以把我本地的 openchamber serve 杀掉吗？".to_string(),
        );
        let (app, platform) = build_app(cfg, mock).await;

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
                session_id: "ses_ext".into(),
                directory: "/tmp/ext".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        // Stale watermark: the old run_prompt-era epoch predates the message the
        // dying server actually persisted — the exact state that caused the bug.
        let stale = chrono::Utc::now().timestamp_millis() - 60_000;
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_ext".into(), stale);

        app.external
            .poll_interval_ms
            .store(50, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // No external-message card: cola's own round is recognised, not echoed.
        let calls = platform.calls.lock().await.clone();
        assert!(
            !calls.iter().any(|c| match c {
                PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                    card.to_string().contains("有新消息")
                }
                _ => false,
            }),
            "cola's own message must never be notified as external: {calls:?}"
        );
        // The watermark advanced PAST cola's message (so a later genuine
        // external message is still detected).
        let watermark = app
            .external
            .last_user_msg_epoch
            .lock()
            .await
            .get("ses_ext")
            .copied();
        assert!(
            watermark.is_some_and(|w| w > stale),
            "watermark should advance over cola's own message: {watermark:?}"
        );
    }

    /// ADR-0026: a GENUINE external message posted AFTER cola's own (the heal
    /// scenario) is still notified — recognising cola's authorship must not
    /// swallow real OpenChamber traffic.
    #[tokio::test]
    async fn newer_external_message_after_cola_own_still_notifies() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.cola_user_messages
            .insert("ses_ext".into(), "cola 自己的一轮".to_string());
        mock.external_user_message = Some("OpenChamber 后来发的消息".to_string());
        let (app, platform) = build_app(cfg, mock).await;

        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
                session_id: "ses_ext".into(),
                directory: "/tmp/ext".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        let stale = chrono::Utc::now().timestamp_millis() - 60_000;
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_ext".into(), stale);

        app.external
            .poll_interval_ms
            .store(50, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let calls = platform.calls.lock().await.clone();
        let notify = calls.iter().find_map(|c| match c {
            PlatformCall::SendCard { card, .. } if card.to_string().contains("有新消息") => {
                Some(card.clone())
            }
            _ => None,
        });
        let notify = notify.expect("the genuine external message should be notified");
        assert!(
            notify.to_string().contains("OpenChamber 后来发的消息"),
            "notification should preview the external message: {}",
            notify
        );
    }

    /// ADR-0017: an external message on a HISTORICAL (non-active) lobby session
    /// must NOT be notified into the chat — only the thread's active session is
    /// synced. Its watermark is also cleared so a later /switch back re-syncs.
    #[tokio::test]
    async fn external_message_to_historical_session_is_not_notified() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        // External message ONLY on the historical session; the active session
        // has none (otherwise BOTH would get an external message and the test
        // couldn't isolate the historical one being suppressed).
        mock.external_user_messages
            .insert("ses_historical".into(), "历史会话的外部消息".to_string());
        let (app, platform) = build_app(cfg, mock).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        // Active session A; historical session B mapped to the SAME lobby.
        {
            let mut store = app.sessions.lock().await;
            // set_active pushes to the front, so the LAST call is the active one.
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_historical".into(),
                directory: "/tmp/hist".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_active".into(),
                directory: "/tmp/active".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        // The ACTIVE session is `ses_active` (last set_active pushes to front).
        assert_eq!(
            app.sessions.lock().await.get_active(&key).unwrap().session_id,
            "ses_active"
        );
        // Both sessions have a watermark from before the external message.
        let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_active".into(), watermark);
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_historical".into(), watermark);

        app.external
            .poll_interval_ms
            .store(50, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // No notification card at all: the only external message is on the
        // historical session, which must be suppressed.
        let calls = platform.calls.lock().await.clone();
        assert!(
            !calls.iter().any(|c| match c {
                PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                    card.to_string().contains("有新消息")
                }
                _ => false,
            }),
            "historical session external message must NOT be notified: {calls:?}"
        );
        // The historical session's watermark was cleared (ready to re-sync
        // silently when it becomes active again).
        assert!(
            !app.external
                .last_user_msg_epoch
                .lock()
                .await
                .contains_key("ses_historical"),
            "historical session watermark should be cleared"
        );
    }

    /// ADR-0017: switching back to a historical session makes it the active one
    /// and re-syncs SILENTLY — external messages received while it was
    /// inactive are marked read, not replayed as a stale notification.
    #[tokio::test]
    async fn reactivated_session_resyncs_silently() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        // The external message is on the session that is being REACTIVATED.
        mock.external_user_messages
            .insert("ses_old".into(), "离开期间的外部消息".to_string());
        let (app, platform) = build_app(cfg, mock).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        {
            let mut store = app.sessions.lock().await;
            // set_active pushes to the front, so the LAST call is the active one
            // (ses_old, the session being reactivated by /switch).
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_new".into(),
                directory: "/tmp/new".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_old".into(),
                directory: "/tmp/old".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        // ses_old is the active session after the /switch back.
        assert_eq!(
            app.sessions.lock().await.get_active(&key).unwrap().session_id,
            "ses_old"
        );
        // While ses_old was historical, the poller cleared its watermark (see the
        // test above). So on the first poll after reactivation the map has NO
        // entry for it → first-observation path → silent re-sync, no notify.
        assert!(
            !app.external
                .last_user_msg_epoch
                .lock()
                .await
                .contains_key("ses_old"),
            "precondition: watermark was cleared while inactive"
        );

        app.external
            .poll_interval_ms
            .store(50, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // No notification: the external message was marked read on first
        // observation (silent re-sync), not replayed as stale.
        let calls = platform.calls.lock().await.clone();
        assert!(
            !calls.iter().any(|c| match c {
                PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                    card.to_string().contains("有新消息")
                }
                _ => false,
            }),
            "reactivated session must re-sync silently, no notification: {calls:?}"
        );
        // The watermark is now recorded for the reactivated session.
        assert!(
            app.external
                .last_user_msg_epoch
                .lock()
                .await
                .contains_key("ses_old"),
            "reactivated session should have a recorded watermark"
        );
    }

    /// External messages to a TOPIC-backed session are notified by replying to
    /// a message INSIDE the topic, not sent to the chat top level. Covers the
    /// no-persisted-anchor case (session created before `/topic` stored the
    /// anchor): `resolve_topic_anchor` queries the thread for the newest bot
    /// message and replies to it.
    #[tokio::test]
    async fn external_message_to_topic_session_notifies_into_thread() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.external_user_message = Some("话题里的外部消息".to_string());
        let (app, platform) = build_app(cfg, mock).await;

        // A TOPIC-backed session (thread_id != chat_id) with NO persisted
        // anchor (like the old /topic sessions) — the anchor must be resolved
        // by querying the thread.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_topic_ext".into()),
                session_id: "ses_ext".into(),
                directory: "/tmp/ext".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_ext".into(), watermark);
        app.external
            .poll_interval_ms
            .store(50, std::sync::atomic::Ordering::Relaxed);

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let calls = platform.calls.lock().await.clone();
        assert!(
            calls.iter().any(|c| matches!(
                c,
                PlatformCall::ReplyCard { reply_to, card } if reply_to == "msg_in_topic_anchor" && card.to_string().contains("有新消息")
            )),
            "topic external notification should reply into the topic (resolved anchor): {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "chat_1")),
            "topic external notification must NOT go to chat top level: {calls:?}"
        );
    }

    /// The model's reply to an external (OpenChamber-posted) message must render
    /// INTO the notification card — update in place, so the Feishu side sees the
    /// answer without a second card. The reply is scripted to arrive on a LATER
    /// poll: the renderer must idle (no card update) while the reply is absent,
    /// then stream reasoning/tools/text in and finalize Done when the turn
    /// completes.
    #[tokio::test]
    async fn external_message_reply_renders_into_notification_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.external_user_message = Some("OpenChamber 里发的消息".to_string());
        // OpenCode's reply to that message: reasoning → tool → text → stop.
        mock.external_reply_parts = Some(serde_json::json!([
            { "type": "step-start", "snapshot": "x" },
            { "type": "reasoning", "text": "我来看看目录。" },
            { "type": "tool", "tool": "bash", "callID": "call_1",
              "state": { "status": "completed", "input": { "command": "ls" }, "output": "src" } },
            { "type": "text", "text": "目录里有 src。" },
            { "type": "step-finish", "reason": "stop" },
        ]));
        let reply_ready = mock.external_reply_ready.clone();
        let (app, platform) = build_app(cfg, mock).await;

        // A known session whose chat the notification goes to.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
                session_id: "ses_ext".into(),
                directory: "/tmp/ext".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
        app.external
            .poll_interval_ms
            .store(50, std::sync::atomic::Ordering::Relaxed);
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_ext".into(), watermark);

        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app.core).await;
            }
        });
        // First poll tick (8s) sends the notification and arms the renderer;
        // a few render-loop ticks then run while the reply is still absent.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // The renderer must NOT have touched the card yet — the reply hasn't
        // arrived, so the notification card stays as sent.
        let calls_before = platform.calls.lock().await.clone();
        assert!(
            calls_before
                .iter()
                .all(|c| !matches!(c, PlatformCall::UpdateMessage { .. })),
            "no card update while the reply is absent: {calls_before:?}"
        );
        assert!(
            calls_before.iter().any(
                |c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "oc_group_1")
            ),
            "notification card should be sent: {calls_before:?}"
        );

        // Now the model replies; the renderer picks it up on the next poll.
        reply_ready.store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

        let calls = platform.calls.lock().await.clone();
        let sent_card = calls.iter().find_map(|c| match c {
            PlatformCall::SendCard { receive_id, card } if receive_id == "oc_group_1" => Some(card.clone()),
            _ => None,
        });
        let sent_card = sent_card.expect("notification card should be sent");
        // The reply rendered IN PLACE on the SAME card (msg_sent): it shows the
        // external user's message and the AI's reasoning/tool/text, finalized Done.
        let final_card = calls.iter().find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_sent" => {
                Some(card.clone())
            }
            _ => None,
        });
        let final_card = final_card.expect("the notification card must be updated in place");
        let text = final_card.to_string();
        assert!(text.contains("✅"), "reply card should be Done: {}", text);
        assert!(
            text.contains("OpenChamber 里发的消息"),
            "the external user message must stay visible: {}",
            text
        );
        assert!(text.contains("我来看看目录"), "reasoning missing: {}", text);
        assert!(text.contains("bash"), "tool panel missing: {}", text);
        assert!(text.contains("目录里有 src。"), "reply text missing: {}", text);

        // No SECOND card was sent — the reply lives entirely on the notification.
        let second_cards = calls
            .iter()
            .filter(|c| matches!(c, PlatformCall::ReplyCard { .. } | PlatformCall::SendCard { .. }))
            .count();
        assert_eq!(
            second_cards, 1,
            "only the notification card should be sent, got: {calls:?}"
        );
        // Sanity: the notification card was NOT replaced by a different one.
        assert!(sent_card.to_string().contains("有新消息"));
    }

    /// The renderer guard: arming a renderer for the SAME external message must
    /// be a no-op (no clobbering of the armed card), while a NEWER message must
    /// replace the armed renderer so the old one exits on the next poll.
    #[tokio::test]
    async fn external_reply_render_guard_replaces_only_newer_messages() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        // No reply parts: the armed loops just idle/exit; we assert state only.
        mock.external_user_message = Some("外部消息".to_string());
        let (app, _platform) = build_app(cfg, mock).await;

        // Arm once for the first external message.
        app.external
            .start_reply_render(&app.core, "ses_ext", 1000, "n1", "第一条")
            .await;
        {
            let cards = app.cards.lock().await;
            let acc = &cards.get("ses_ext").expect("renderer accumulator").acc;
            assert_eq!(acc.submit_epoch_ms, Some(1000));
            assert_eq!(acc.reply_to_message_id.as_deref(), Some("n1"));
        }
        assert_eq!(
            app.cards
                .lock()
                .await
                .get("ses_ext")
                .and_then(|c| c.card_message_id.as_deref()),
            Some("n1")
        );

        // Re-arming for the SAME message (e.g. a duplicate poll) is a no-op:
        // the armed card id and epoch must not be clobbered.
        app.external
            .start_reply_render(&app.core, "ses_ext", 1000, "n1b", "第一条")
            .await;
        {
            let cards = app.cards.lock().await;
            let acc = &cards.get("ses_ext").expect("renderer accumulator").acc;
            assert_eq!(acc.submit_epoch_ms, Some(1000));
            assert_eq!(acc.reply_to_message_id.as_deref(), Some("n1"));
        }
        assert_eq!(
            app.cards
                .lock()
                .await
                .get("ses_ext")
                .and_then(|c| c.card_message_id.as_deref()),
            Some("n1")
        );

        // A NEWER external message replaces the armed renderer (its card id and
        // epoch move to the new notification).
        app.external
            .start_reply_render(&app.core, "ses_ext", 2000, "n2", "第二条")
            .await;
        {
            let cards = app.cards.lock().await;
            let acc = &cards.get("ses_ext").expect("renderer accumulator").acc;
            assert_eq!(acc.submit_epoch_ms, Some(2000));
            assert_eq!(acc.reply_to_message_id.as_deref(), Some("n2"));
        }
        assert_eq!(
            app.cards
                .lock()
                .await
                .get("ses_ext")
                .and_then(|c| c.card_message_id.as_deref()),
            Some("n2")
        );
    }

    /// The external-reply renderer's hard-timeout branch: a partial reply is
    /// rendered but the model never finishes, so the loop finalizes the card as
    /// Done when the (injected, tiny) timeout elapses — the card never sits on
    /// an eternal spinner. Exercises the timeout with millisecond fields instead
    /// of the production 10-minute default.
    #[tokio::test]
    async fn external_reply_render_times_out_and_finalizes_partial_content() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.external_user_message = Some("OpenChamber 里发的消息".to_string());
        // A partial reply: reasoning + text, but NO step-finish — the turn never
        // completes, so the loop must be stopped by the timeout.
        mock.external_reply_parts = Some(serde_json::json!([
            { "type": "step-start", "snapshot": "x" },
            { "type": "reasoning", "text": "我在想。" },
            { "type": "text", "text": "部分回答。" },
        ]));
        mock.external_reply_ready
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (app, platform) = build_app(cfg, mock).await;

        // A known session whose chat the notification goes to.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
                session_id: "ses_ext".into(),
                directory: "/tmp/ext".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        // Tiny cadences + timeout so the whole branch runs in milliseconds.
        app.external
            .poll_interval_ms
            .store(50, std::sync::atomic::Ordering::Relaxed);
        app.external
            .render_poll_ms
            .store(5, std::sync::atomic::Ordering::Relaxed);
        app.external
            .render_timeout_ms
            .store(20, std::sync::atomic::Ordering::Relaxed);
        let watermark = chrono::Utc::now().timestamp_millis() - 60_000;
        app.external
            .last_user_msg_epoch
            .lock()
            .await
            .insert("ses_ext".into(), watermark);

        // The poll loop detects the external message, sends the notification,
        // arms the renderer with the REAL message epoch, and the renderer
        // renders the partial reply then times out and finalizes it Done.
        tokio::spawn({
            let app = app.clone();
            async move {
                let _ = app.external.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // The partial content was rendered AND the card finalized as Done (the
        // final update carries the terminal state).
        let calls = platform.calls.lock().await.clone();
        let updates: Vec<serde_json::Value> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect();
        let last = updates.last().expect("card was updated at least once");
        assert!(
            last.to_string().contains("部分回答"),
            "partial text must render: {}",
            last
        );
        let done_header = last["header"]["title"]["content"].as_str().unwrap_or("");
        assert!(
            done_header.contains("完成") || done_header.contains("✓"),
            "timeout must finalize the card as Done, header: {}",
            done_header
        );
    }

    #[tokio::test]
    async fn subtask_permission_routes_to_mapped_parent_and_reply_carries_directory() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        // A sub-task session cola never created; its permission must be routed
        // up to the parent session's chat.
        let child = "ses_child_task";
        backend
            .session_parents
            .insert(child.into(), backend.session_id.clone());
        backend.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_child".into(),
            session_id: Some(child.into()),
            permission: Some("bash".into()),
            patterns: vec!["git status".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let parent_id = backend.session_id.clone();
        let (app, _platform) = build_app(cfg, backend).await;

        // Seed the parent session so it maps child → parent → chat.
        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        tokio::spawn({
            let app = app.clone();
            async move {
                app.permission
                    .poll_interval_ms
                    .store(50, std::sync::atomic::Ordering::Relaxed);
                let _ = app.permission.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // The parent session has a live streaming card, so the child's
        // permission is INLINED on it (one-card-per-turn), not sent as a
        // separate card — the child itself has no accumulator, so it must be
        // hosted on the parent's card found by walking the parent chain.
        let perm_inline = app
            .cards
            .lock()
            .await
            .get(&parent_id)
            .expect("parent accumulator exists")
            .acc
            .pending_permissions
            .clone();
        assert_eq!(
            perm_inline.len(),
            1,
            "subtask permission should be inlined on the parent card"
        );
        assert_eq!(perm_inline[0].request_id, "per_child");
        assert_eq!(perm_inline[0].session_id, child);

        // The streaming card renders the inline section with the child's buttons.
        let card = app.cards.lock().await.get(&parent_id).unwrap().acc.build_card();
        let card_text = card.to_string();
        assert!(card_text.contains("权限请求"), "inline section missing");
        assert!(card_text.contains("git status"), "permission body missing");
        // The button carries the CHILD session id (the request's owner) plus the
        // owning directory so the reply routes to the right instance even though
        // the child session isn't in the store.
        let first_button = card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .expect("permission buttons present");
        let value = first_button["value"].clone();
        assert_eq!(value["session_id"], child);
        assert!(
            value["directory"]
                .as_str()
                .map(|d| !d.is_empty())
                .unwrap_or(false),
            "permission card must carry a directory, got: {}",
            value
        );

        // Clicking Allow routes the reply with that directory and drops the
        // inline section (no replacement card — the streaming card re-renders).
        let mut value = value;
        value["reply"] = serde_json::json!("once");
        value["perm_label"] = serde_json::json!("✅ 已允许一次");
        value["perm_color"] = serde_json::json!("green");
        let result = app.handle_card_action(value).await;
        assert!(result.is_some(), "reply should succeed for subtask session");
        assert!(
            result.unwrap().card.is_none(),
            "inline answer must not replace the streaming card"
        );
        assert!(
            app.cards
                .lock()
                .await
                .get(&parent_id)
                .unwrap()
                .acc
                .pending_permissions
                .is_empty(),
            "inline permission section should be removed after answering"
        );
    }

    /// Without a live streaming card (e.g. the parent turn finished or cola
    /// restarted), a sub-task child's permission falls back to a separate card
    /// sent into the parent's chat — still routed up the parent chain, never
    /// dropped.
    #[tokio::test]
    async fn subtask_permission_without_streaming_card_sends_card_to_parent_chat() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        let child = "ses_child_task";
        backend
            .session_parents
            .insert(child.into(), backend.session_id.clone());
        backend.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_child".into(),
            session_id: Some(child.into()),
            permission: Some("bash".into()),
            patterns: vec!["git status".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let (app, platform) = build_app(cfg, backend).await;

        // Map the parent session to a chat WITHOUT an active accumulator (no
        // handle_message call — the turn is finished).
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }

        tokio::spawn({
            let app = app.clone();
            async move {
                app.permission
                    .poll_interval_ms
                    .store(50, std::sync::atomic::Ordering::Relaxed);
                let _ = app.permission.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // No inline host: the permission becomes a separate card delivered into
        // the parent's chat (the child has no card of its own to host it).
        let calls = platform.calls.lock().await.clone();
        let perm_card = calls.iter().find_map(|c| match c {
            PlatformCall::SendCard { receive_id, card }
                if receive_id == "chat_1" && card.to_string().contains("git status") =>
            {
                Some(card.clone())
            }
            _ => None,
        });
        let perm_card =
            perm_card.expect("subtask permission should fall back to a separate card in the parent chat");
        // The card still carries the child's session id + owning directory.
        let first_button = perm_card["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .expect("permission card has buttons");
        let value = first_button["value"].clone();
        assert_eq!(value["session_id"], child);
        assert!(
            value["directory"]
                .as_str()
                .map(|d| !d.is_empty())
                .unwrap_or(false),
            "separate card must carry a directory, got: {}",
            value
        );
    }

    /// A topic-backed session with no streaming card falls back to a separate
    /// permission card. The card replies to the session's topic anchor (a
    /// message inside the topic), which keeps it inside the topic — the create
    /// API rejects `thread_id` as a receive target, so replying to the anchor is
    /// the reliable way to reach the topic.
    #[tokio::test]
    async fn separate_permission_card_sent_into_topic_for_topic_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.permissions = vec![opencode::client::PermissionRequest {
            request_id: "per_topic".into(),
            session_id: Some("ses_topic".into()),
            permission: Some("bash".into()),
            patterns: vec!["cargo build".into()],
            metadata: None,
            always: Vec::new(),
        }];
        let (app, platform) = build_app(cfg, backend).await;

        // Map the session to a TOPIC (thread_id != chat_id) with an anchor
        // message inside the topic, no accumulator.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_topic_1".into()),
                session_id: "ses_topic".into(),
                directory: "/tmp/topic".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some("msg_in_topic_anchor".into()),
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }

        tokio::spawn({
            let app = app.clone();
            async move {
                app.permission
                    .poll_interval_ms
                    .store(50, std::sync::atomic::Ordering::Relaxed);
                let _ = app.permission.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // The separate card must be reply'd to the topic anchor (not sent to
        // the chat top level).
        let calls = platform.calls.lock().await.clone();
        let perm_card = calls.iter().find_map(|c| match c {
            PlatformCall::ReplyCard { reply_to, card }
                if reply_to == "msg_in_topic_anchor" && card.to_string().contains("cargo build") =>
            {
                Some(card.clone())
            }
            _ => None,
        });
        assert!(
            perm_card.is_some(),
            "topic permission card should reply to the topic anchor, got: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "chat_1")),
            "permission card must NOT go to the chat top level: {calls:?}"
        );
    }

    /// A live E2E run: real cola bot (Platform) + a MOCK backend + the test bot
    /// reading the group.
    struct LiveHarness {
        app: Arc<App>,
        backend: Arc<MockBackend>,
        test_bot: feishu::Client,
        group_chat_id: String,
        _dir: tempfile::TempDir,
    }

    impl LiveHarness {
        /// Post a message into the group via the test bot and have cola process
        /// it; returns the sent message id (cola replies to it).
        async fn send_and_process(&self, prompt: &str) -> String {
            let sent_msg_id = self
                .test_bot
                .send_text("chat_id", &self.group_chat_id, prompt)
                .await
                .expect("send prompt to group");
            self.app
                .handle_message(incoming(
                    sent_msg_id.clone(),
                    self.group_chat_id.clone(),
                    "group".into(),
                    None,
                    prompt.to_string(),
                    None,
                ))
                .await;
            sent_msg_id
        }

        /// Poll the group until an interactive card whose content contains
        /// `needle` appears; returns the content or "" on timeout.
        async fn wait_for_card(&self, needle: &str, timeout_secs: i64) -> String {
            let deadline = chrono::Utc::now() + chrono::Duration::seconds(timeout_secs);
            let mut found = String::new();
            loop {
                let msgs = self
                    .test_bot
                    .list_messages("chat", &self.group_chat_id)
                    .await
                    .expect("list group messages");
                for m in &msgs {
                    if m.msg_type == "interactive" {
                        let content = m
                            .body
                            .as_ref()
                            .and_then(|b| b.get("content"))
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        if content.contains(needle) {
                            found = content.to_string();
                        }
                    }
                }
                if !found.is_empty() || chrono::Utc::now() > deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
            }
            found
        }
    }

    /// Shared live E2E setup. Returns None when the test-bot credentials aren't
    /// configured (the test then skips).
    async fn live_setup(backend: MockBackend) -> Option<LiveHarness> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("cola=debug")
            .with_writer(std::io::stderr)
            .try_init();

        #[derive(serde::Deserialize)]
        struct LiveTestCfg {
            #[serde(rename = "app_id")]
            app_id: String,
            #[serde(rename = "app_secret")]
            app_secret: String,
            #[serde(rename = "group_chat_id")]
            group_chat_id: String,
            #[serde(rename = "work_dir", default)]
            work_dir: Option<String>,
        }

        let test_cfg = std::fs::read_to_string("cola-test.toml")
            .ok()
            .and_then(|s| toml::from_str::<LiveTestCfg>(&s).ok());
        let test_app_id = test_cfg
            .as_ref()
            .map(|c| c.app_id.clone())
            .or_else(|| std::env::var("COLA_TEST_BOT_APP_ID").ok())
            .unwrap_or_default();
        let test_app_secret = test_cfg
            .as_ref()
            .map(|c| c.app_secret.clone())
            .or_else(|| std::env::var("COLA_TEST_BOT_APP_SECRET").ok())
            .unwrap_or_default();
        let group_chat_id = test_cfg
            .as_ref()
            .map(|c| c.group_chat_id.clone())
            .or_else(|| std::env::var("COLA_TEST_GROUP_CHAT_ID").ok())
            .unwrap_or_default();
        let work_dir = test_cfg
            .as_ref()
            .and_then(|c| c.work_dir.clone())
            .or_else(|| std::env::var("COLA_TEST_WORK_DIR").ok())
            .unwrap_or_default();
        if test_app_id.is_empty() || test_app_secret.is_empty() || group_chat_id.is_empty() {
            tracing::warn!("skipping live E2E: configure cola-test.toml or set the COLA_TEST_BOT_* env vars");
            return None;
        }

        // Load the cola bot config from the repo; point new sessions at the
        // configured work dir instead of chdir'ing the whole process.
        let mut cfg = crate::config::load(std::path::Path::new("cola.toml")).expect("load cola.toml");
        if !work_dir.is_empty() {
            cfg.bridge.work_dir = Some(work_dir.into());
        }
        let dir = tempfile::tempdir().unwrap();
        cfg.bridge.session_file = dir.path().join("sessions.json");

        let cola_platform = feishu::Client::new(cfg.feishu.clone());
        let test_bot = feishu::Client::new(crate::config::FeishuConfig {
            app_id: test_app_id,
            app_secret: test_app_secret,
        });
        let backend = Arc::new(backend);
        let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(cola_platform)).unwrap());
        Some(LiveHarness {
            app,
            backend,
            test_bot,
            group_chat_id,
            _dir: dir,
        })
    }

    /// Live end-to-end wire check with a real Feishu bot.
    ///
    /// cola renders cards from a MOCK backend (deterministic, no real OpenCode
    /// server), the real cola bot posts them into a test group, and a second
    /// Feishu bot reads back what was actually delivered and asserts it matches
    /// expectation. This verifies the wire format Feishu accepts, not just the
    /// JSON cola builds in-process.
    ///
    /// Credentials come from `cola-test.toml` (gitignored, see the .example
    /// template) or the env vars COLA_TEST_BOT_APP_ID / COLA_TEST_BOT_APP_SECRET /
    /// COLA_TEST_GROUP_CHAT_ID. Run:
    ///   `cargo test --bin cola live_e2e_real_bot -- --ignored`
    #[tokio::test]
    #[ignore = "requires a second Feishu bot + a test group; see test docs"]
    async fn live_e2e_real_bot_renders_expected_cards() {
        let Some(harness) = live_setup(MockBackend::new(realistic_parts())).await else {
            return;
        };

        // The test bot sends a real message so cola has a real reply target.
        let prompt = "自动测试：请分析一下目录，然后汇报。";
        let _sent_msg_id = harness.send_and_process(prompt).await;

        // Read back the group until the cola bot's final Done card appears.
        // Feishu's start_time/end_time window returns empty for recent messages
        // on this API, so query without a window and filter client-side. Note:
        // the API only returns the v2 card's *fallback* (title + "upgrade your
        // client" placeholder), so this asserts real delivery + terminal header
        // state; the full reasoning/tool/text body is asserted in-process by
        // the RecordingPlatform tests. The needle is this test's own prompt so
        // it can't match a sibling live test sharing the group.
        let final_text = harness.wait_for_card("自动测试：请分析一下目录", 30).await;

        assert!(
            final_text.contains("✅"),
            "cola bot never posted a Done card to the group"
        );
        assert!(
            final_text.contains("自动测试：请分析一下目录"),
            "card fallback title should carry the question, got: {}",
            final_text
        );
    }

    /// Live E2E for the interactive `question` tool: the mock backend surfaces a
    /// pending question request, the question poller turns it into a real card
    /// the cola bot posts to the group, and the test bot reads it back.
    #[tokio::test]
    #[ignore = "requires a second Feishu bot + a test group; see live_e2e docs"]
    async fn live_e2e_question_card_is_delivered() {
        // Unique per run so wait_for_card can't match a stale question card left
        // in the group by a previous run.
        let marker = format!("q-{}", uuid::Uuid::new_v4().to_string().get(..8).unwrap_or("x"));
        let question_text = format!("你想继续吗？（{}）", marker);
        let mut backend = MockBackend::new(realistic_parts());
        backend.questions = vec![opencode::client::QuestionRequest {
            id: "que_live".into(),
            session_id: "ses_test".into(),
            questions: vec![opencode::client::QuestionInfo {
                question: question_text.clone(),
                header: "下一步".into(),
                options: vec![
                    opencode::client::QuestionOption {
                        label: "继续".into(),
                        description: String::new(),
                    },
                    opencode::client::QuestionOption {
                        label: "停止".into(),
                        description: String::new(),
                    },
                ],
                multiple: None,
                custom: None,
            }],
        }];
        let Some(harness) = live_setup(backend).await else {
            return;
        };

        // Give cola a session + reply target to work against.
        harness.send_and_process("自动测试：请回答我的问题。").await;

        // Run the question poller (it surfaces pending questions as cards).
        tokio::spawn({
            let app = harness.app.clone();
            async move {
                let _ = app.question.poll_loop(&app.core).await;
            }
        });

        let content = harness.wait_for_card(&marker, 30).await;
        assert!(
            content.contains("❓ AI 想问你"),
            "question card header missing: {}",
            content
        );
        assert!(
            content.contains(&question_text),
            "question card body missing: {}",
            content
        );
        assert!(
            content.contains("继续"),
            "question option button missing: {}",
            content
        );

        // Simulate the user clicking an option. Feishu's messages API strips
        // button `value` payloads from the returned card, so the click is driven
        // with the known payload (the payload shape is pinned in-process by the
        // Seam C card test); the wire test above already confirmed the card was
        // delivered with the question text and option labels.
        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_live",
            "session_id": "ses_test",
            "question_index": 0,
            "answer": "继续",
        });
        let result = harness.app.handle_card_action(value).await;
        assert!(
            result.is_some(),
            "clicking an option should produce a result card"
        );

        let calls = harness.backend.reply_question_calls.lock().await.clone();
        assert!(
            calls
                .iter()
                .any(|(req, answers)| { req == "que_live" && answers == &vec![vec!["继续".to_string()]] }),
            "the chosen answer was not posted to the backend: {:?}",
            calls
        );
    }

    #[tokio::test]
    async fn new_session_uses_configured_work_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(&dir.path().join("sessions.json"));
        let work = dir.path().join("work");
        cfg.bridge.work_dir = Some(work.clone());

        // No chdir: the session directory must come from [bridge] work_dir, not
        // the process cwd.
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        let thread = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&thread)
            .cloned()
            .expect("a session should have been created");
        assert_eq!(entry.directory, work.to_string_lossy().to_string());
    }

    /// `/new` in a conversation whose active session lives in a project must
    /// inherit that project's directory (ADR-0012) — NOT the configured
    /// work_dir. Only a conversation with no session falls back to work_dir.
    #[tokio::test]
    async fn new_command_inherits_active_sessions_directory() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(&dir.path().join("sessions.json"));
        let work = dir.path().join("work");
        cfg.bridge.work_dir = Some(work.clone());
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        let thread_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let proj = tempfile::tempdir().unwrap();
        let proj_dir = proj.path().to_string_lossy().to_string();

        // First `/dir <proj>` roots a session in the project.
        crate::bridge::command::handle_command(
            &app.core,
            crate::bridge::command::Command::Dir(proj_dir.clone()),
            thread_key.clone(),
            "msg_dir",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Then `/new` must stay in the project, not jump back to work_dir.
        crate::bridge::command::handle_command(
            &app.core,
            crate::bridge::command::Command::New(None),
            thread_key.clone(),
            "msg_new",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&thread_key)
            .cloned()
            .expect("a session should be active after /new");
        // normalize_directory canonicalizes the project path (resolving
        // /private/var on macOS and \\?\ / 8.3 short names on Windows), so
        // compare against the canonicalized form — not the raw tempdir path.
        let canonical = std::fs::canonicalize(proj.path()).unwrap();
        assert_eq!(
            entry.directory,
            canonical.to_string_lossy(),
            "/new must inherit the active session's directory, not work_dir"
        );
        assert_ne!(
            entry.directory,
            work.to_string_lossy().to_string(),
            "/new must NOT fall back to work_dir when a session is active"
        );
    }

    /// `/new` in a conversation with NO active session still falls back to the
    /// configured work_dir (the fresh-machine / fresh-topic case).
    #[tokio::test]
    async fn new_command_falls_back_to_work_dir_without_active_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(&dir.path().join("sessions.json"));
        let work = dir.path().join("work");
        cfg.bridge.work_dir = Some(work.clone());
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        let thread_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        crate::bridge::command::handle_command(
            &app.core,
            crate::bridge::command::Command::New(None),
            thread_key.clone(),
            "msg_new",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&thread_key)
            .cloned()
            .expect("a session should be active after /new");
        assert_eq!(entry.directory, work.to_string_lossy().to_string());
    }
    /// by a new session rooted at <dir>, maps the returned thread_id to that
    /// session, and leaves the lobby conversation untouched.
    #[tokio::test]
    async fn topic_command_creates_topic_mapped_to_new_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_id = "ses_topic".into();
        let title_calls = backend.update_title_calls.clone();
        let (app, platform) = build_app(cfg, backend).await;
        let proj = tempfile::tempdir().unwrap();
        let proj_dir = proj.path().to_string_lossy().to_string();

        crate::bridge::command::handle_command(
            &app.core,
            Command::Topic {
                directory: Some(proj_dir.clone()),
                name: Some("api-refactor".into()),
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The topic is created via a cover card sent to the chat's top level,
        // then reply_in_thread on THAT card: the cover becomes the thread root,
        // so the chat-list topic entry shows the session brief permanently
        // (ADR-0023). On the mock the cover send returns "msg_sent".
        let calls = platform.calls.lock().await.clone();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, PlatformCall::SendCard { receive_id, .. } if receive_id == "chat_1")),
            "expected a cover card sent to the chat, got {calls:?}"
        );
        assert!(
            calls.iter().any(
                |c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_sent")
            ),
            "expected reply_in_thread on the cover card, got {calls:?}"
        );
        // The cover card carries the session brief; the in-topic seed is only a
        // short hint (the brief lives on the root card at the top of the thread).
        let cover = calls
            .iter()
            .find_map(|c| match c {
                PlatformCall::SendCard { card, .. } => Some(card.to_string()),
                _ => None,
            })
            .expect("cover card JSON");
        assert!(
            cover.contains("💬 `api-refactor`") && cover.contains("会话 `topic`"),
            "cover card should lead with the title and session, got: {cover}"
        );
        let seed = calls
            .iter()
            .find_map(|c| match c {
                PlatformCall::ReplyInThread { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("reply_in_thread seed text");
        assert_eq!(seed, "请在本话题内回复，即可和这个会话对话。");

        // The created topic's thread_id is mapped to the new session.
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&topic_key)
            .cloned()
            .expect("topic thread_id should map to the new session");
        assert_eq!(entry.session_id, "ses_topic");
        // normalize_directory canonicalizes the project path (resolving
        // /private/var on macOS and \\?\ / 8.3 short names on Windows), so
        // compare against the canonicalized form — not the raw tempdir path.
        assert_eq!(
            entry.directory,
            std::fs::canonicalize(proj.path()).unwrap().to_string_lossy()
        );
        // The named `/topic` PATCHed the server title (ADR-0007).
        assert_eq!(
            title_calls.lock().await.as_slice(),
            &[("ses_topic".to_string(), "api-refactor".to_string())]
        );
        // The topic anchor is the confirmation message INSIDE the topic; future
        // sent cards reply to it so they stay in the topic.
        assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
        // The thread root is the cover card (ADR-0023): the injection guard
        // excludes it from Quoted Context, and the post-turn hook patches it
        // when the server auto-generates a title.
        assert_eq!(entry.topic_root.as_deref(), Some("msg_sent"));
        // The cover title is recorded so the post-turn hook can sync it.
        assert_eq!(
            app.core.cover_titles.lock().await.get("ses_topic").cloned(),
            Some(crate::bridge::core::CoverTitle {
                title: "api-refactor".to_string(),
                model: None
            })
        );

        // The lobby conversation still maps to nothing new (no session was
        // created for the lobby itself).
        let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&lobby_key).is_none());
    }

    /// ADR-0023: when the cover card cannot be sent, the thread anchors on the
    /// user's command message instead — the old behavior — and no cover title
    /// is recorded (so the post-turn hook never tries to patch the user's
    /// message, which Feishu would reject).
    #[tokio::test]
    async fn topic_cover_send_failure_falls_back_to_command_root() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let mut platform = Arc::new(RecordingPlatform::new());
        Arc::get_mut(&mut platform).unwrap().fail_send_card = true;
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
        let proj = tempfile::tempdir().unwrap();
        let proj_dir = proj.path().to_string_lossy().to_string();

        // A stale cover-title entry may already exist (e.g. the session was
        // previously adopted into a cover-rooted topic); the fallback must drop
        // it so the post-turn hook never patches the user's command message.
        app.core.cover_titles.lock().await.insert(
            "ses_test".into(),
            crate::bridge::core::CoverTitle {
                title: "旧封面".into(),
                model: None,
            },
        );

        crate::bridge::command::handle_command(
            &app.core,
            Command::Topic {
                directory: Some(proj_dir.clone()),
                name: None,
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        assert!(
            calls.iter().any(
                |c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_topic")
            ),
            "expected reply_in_thread on the command message after cover failure, got {calls:?}"
        );

        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&topic_key)
            .cloned()
            .expect("topic thread_id should map to the new session");
        assert_eq!(entry.topic_root.as_deref(), Some("msg_topic"));
        assert!(
            app.core.cover_titles.lock().await.is_empty(),
            "no cover title may survive without a cover card"
        );
    }

    /// ADR-0023: after a completed turn, when the server holds a different
    /// title for the session (auto-generated after the first exchange), cola
    /// patches the cover card in place — the thread root — so the chat-list
    /// topic entry shows the real title.
    #[tokio::test]
    async fn topic_cover_card_updated_with_auto_title_after_turn() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        backend
            .session_titles
            .lock()
            .unwrap()
            .insert("ses_t1".into(), "修复登录 bug".into());
        let (app, platform) = build_app(cfg, backend).await;

        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: topic_key.clone(),
                session_id: "ses_t1".into(),
                directory: "/work/t".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some("om_seed".into()),
                topic_root: Some("om_cover".into()),
                variant: None,
            });
        }
        app.core.cover_titles.lock().await.insert(
            "ses_t1".into(),
            crate::bridge::core::CoverTitle {
                title: "旧标题".into(),
                model: None,
            },
        );

        app.handle_message(crate::bridge::IncomingMessage {
            message_id: "msg_1".into(),
            chat_id: "chat_1".into(),
            chat_type: "group".into(),
            thread_id: Some("omt_t_1".into()),
            parent_id: None,
            text: "继续".into(),
            images: vec![],
            requester_open_id: None,
        })
        .await;

        let calls = platform.calls.lock().await.clone();
        let patched = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { message_id, card } if message_id == "om_cover" => {
                    Some(card.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !patched.is_empty(),
            "the cover card must be patched in place, got {calls:?}"
        );
        assert!(
            patched.last().unwrap().contains("修复登录 bug"),
            "cover card must show the server title: {:?}",
            patched.last()
        );
        assert_eq!(
            app.core.cover_titles.lock().await.get("ses_t1").cloned(),
            Some(crate::bridge::core::CoverTitle {
                title: "修复登录 bug".to_string(),
                model: None
            })
        );
    }

    /// Bare `/topic` (no args) creates the topic session in the conversation's
    /// CURRENT PROJECT — the active session's directory — instead of demanding
    /// an explicit `<dir>`, exactly like `/new` (ADR-0012 project model).
    #[tokio::test]
    async fn topic_command_bare_inherits_current_project_directory() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_id = "ses_topic".into();
        let (app, platform) = build_app(cfg, backend).await;
        let proj = tempfile::tempdir().unwrap();
        let proj_dir = proj.path().to_string_lossy().to_string();

        let thread_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        // Root a session in the project with `/dir`; it becomes the active
        // session whose directory bare `/topic` must inherit.
        crate::bridge::command::handle_command(
            &app.core,
            crate::bridge::command::Command::Dir(proj_dir.clone()),
            thread_key.clone(),
            "msg_dir",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Bare `/topic` — no directory given.
        crate::bridge::command::handle_command(
            &app.core,
            Command::Topic {
                directory: None,
                name: None,
            },
            thread_key.clone(),
            "msg_topic",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The topic is created on the cover card sent to the chat, like the
        // explicit form (ADR-0023); the mock cover send returns "msg_sent".
        let calls = platform.calls.lock().await.clone();
        assert!(
            calls.iter().any(
                |c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_sent")
            ),
            "expected a reply_in_thread on the cover card, got {calls:?}"
        );

        // The topic session lives in the inherited project directory.
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&topic_key)
            .cloned()
            .expect("bare /topic should map the created topic to its session");
        assert_eq!(entry.session_id, "ses_topic");
        assert_eq!(
            entry.directory,
            std::fs::canonicalize(proj.path()).unwrap().to_string_lossy(),
            "bare /topic must inherit the active session's directory"
        );
        assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
    }

    /// A message sent INSIDE the created topic routes to the topic's session,
    /// not to a fresh lobby session.
    #[tokio::test]
    async fn topic_command_created_topic_routes_messages_to_its_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_id = "ses_topic".into();
        let prompt_calls = backend.prompt_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;

        // Create the topic first.
        let proj = tempfile::tempdir().unwrap();
        let proj_dir = proj.path().to_string_lossy().to_string();
        crate::bridge::command::handle_command(
            &app.core,
            Command::Topic {
                directory: Some(proj_dir),
                name: None,
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Now a message arrives inside that topic (thread_id = the mapped one).
        app.handle_message(incoming(
            "msg_in_topic".into(),
            "chat_1".into(),
            "p2p".into(),
            Some("omt_created_topic".into()),
            "帮我看看这个目录".into(),
            None,
        ))
        .await;

        // It must reuse the topic session (ses_topic), not create a new one.
        let calls = prompt_calls.lock().await.clone();
        assert_eq!(calls, vec!["帮我看看这个目录".to_string()]);
        let store = app.sessions.lock().await;
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        assert_eq!(
            store.get_active(&topic_key).map(|e| e.session_id.as_str()),
            Some("ses_topic")
        );
        // The lobby got NO session of its own.
        let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(store.get_active(&lobby_key).is_none());
    }

    /// `/topic` invoked from inside a topic is rejected with a note rather than
    /// nesting another topic.
    #[tokio::test]
    async fn topic_command_rejected_inside_existing_topic() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::Topic {
                directory: Some("/root/proj/lib".into()),
                name: None,
            },
            crate::config::ThreadKey::new("chat_1".into(), "omt_existing".into()),
            "msg_topic",
            crate::config::ConversationKind::Topic,
        )
        .await
        .unwrap();

        // No session created, no topic created — just a plain text note.
        let calls = platform.calls.lock().await.clone();
        assert!(
            calls
                .iter()
                .all(|c| !matches!(c, PlatformCall::ReplyInThread { .. })),
            "must not create a topic from inside a topic: {calls:?}"
        );
        assert!(calls.iter().any(|c| matches!(c, PlatformCall::ReplyText { .. })));
        let store = app.sessions.lock().await;
        assert!(store.all_entries().is_empty());
    }

    // ===== /topic --adopt (ADR-0016) =====

    /// `/topic --adopt <kw>` resolves an existing session and opens a NEW topic
    /// around it, mapping the adopted session to the new topic's ThreadKey.
    #[tokio::test]
    async fn topic_adopt_opens_topic_around_existing_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_foreign123abc",
            "重写登录模块",
            "/work/auth",
            100,
        )];
        let (app, platform) = build_app(cfg, backend).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::TopicAdopt {
                keyword: "重写登录".into(),
                force: false,
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic_adopt",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The topic is created via a cover card sent to the chat, then
        // reply_in_thread on that card (ADR-0023); the mock cover returns "msg_sent".
        let calls = platform.calls.lock().await.clone();
        assert!(
            calls.iter().any(
                |c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_sent")
            ),
            "expected reply_in_thread on the cover card, got {calls:?}"
        );

        // The new topic's thread_id maps to the ADOPTED session (not a new one),
        // with the in-topic confirmation as the fallback-card anchor.
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&topic_key)
            .cloned()
            .expect("topic thread_id should map to the adopted session");
        assert_eq!(entry.session_id, "ses_foreign123abc");
        assert_eq!(entry.directory, "/work/auth");
        assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
        // The lobby is untouched (no session created for it).
        let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&lobby_key).is_none());
    }

    /// `/topic --adopt` rejects a child (sub-task) session.
    #[tokio::test]
    async fn topic_adopt_rejects_child_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![opencode::client::SessionListInfo {
            parent_id: Some("ses_parent".into()),
            ..list_session("ses_child09", "Child session - x", "/work/auth", 100)
        }];
        let (app, platform) = build_app(cfg, backend).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::TopicAdopt {
                keyword: "ses_child09".into(),
                force: false,
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic_adopt",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("子任务"), "rejection mentions sub-task: {text}");
        assert!(
            calls
                .iter()
                .all(|c| !matches!(c, PlatformCall::ReplyInThread { .. })),
            "no topic created for a child session: {calls:?}"
        );
        assert!(app.sessions.lock().await.all_entries().is_empty());
    }

    /// `/topic --adopt` of a session mapped to another thread rejects with an
    /// actionable note pointing at the text `--force`, unless `--force` is given.
    #[tokio::test]
    async fn topic_adopt_rejects_owned_session_without_force() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_owned", "被占用的会话", "/work/auth", 100)];
        let mut platform = RecordingPlatform::new();
        platform
            .chat_names
            .insert("oc_group_other".into(), "隔壁群".into());
        let platform = Arc::new(platform);
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
                session_id: "ses_owned".into(),
                directory: "/work/auth".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::TopicAdopt {
                keyword: "ses_owned".into(),
                force: false,
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic_adopt",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("隔壁群"), "rejection names the owning chat: {text}");
        assert!(
            text.contains("--force"),
            "rejection points at /topic --adopt ... --force: {text}"
        );
        assert!(
            calls
                .iter()
                .all(|c| !matches!(c, PlatformCall::ReplyInThread { .. })),
            "no topic created for an owned session: {calls:?}"
        );
    }

    /// `/topic --adopt ... --force` steals a session mapped to another thread:
    /// the other thread becomes sessionless, the new topic owns it.
    #[tokio::test]
    async fn topic_adopt_force_steals_mapping() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_owned", "被占用的会话", "/work/auth", 100)];
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
                session_id: "ses_owned".into(),
                directory: "/work/auth".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::TopicAdopt {
                keyword: "ses_owned".into(),
                force: true,
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic_adopt",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        assert_eq!(
            app.sessions
                .lock()
                .await
                .get_active(&topic_key)
                .map(|e| e.session_id.as_str()),
            Some("ses_owned"),
            "new topic owns the stolen session"
        );
        let other_key = crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into());
        assert!(
            app.sessions.lock().await.get_active(&other_key).is_none(),
            "the old owner thread becomes sessionless"
        );
    }

    /// `/topic --adopt` (no arg) pops the session-picker card (the `/switch`
    /// card), whose rows carry the "建话题接管" button.
    #[tokio::test]
    async fn topic_adopt_no_arg_sends_switch_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
        let (app, platform) = build_app(cfg, backend).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::TopicAdoptCard,
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_card",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let card = calls
            .iter()
            .find_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .expect("a session card is sent");
        let card_str = card.to_string();
        assert!(
            card_str.contains("建话题接管"),
            "card rows offer 建话题接管: {card_str}"
        );
        assert!(
            card_str.contains("ses_alpha01"),
            "card lists the session: {card_str}"
        );
    }

    /// The switch card's "建话题接管" op opens a topic anchored on the card's own
    /// message (`open_message_id`) and maps the session to the new topic key.
    #[tokio::test]
    async fn switch_card_topic_adopt_action_creates_topic() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
        let (app, _platform) = build_app(cfg, backend).await;

        let value = serde_json::json!({
            "action": "switch",
            "op": "topic_adopt",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "session_id": "ses_alpha01",
            "open_message_id": "om_switch_card",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("topic_adopt should return a result");
        assert!(result.card.is_some(), "topic_adopt refreshes the card");
        assert!(
            result.toast.clone().unwrap_or_default().contains("话题"),
            "topic_adopt toasts: {:?}",
            result.toast
        );

        // The new topic (thread_id from the mock reply_in_thread) owns the session.
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&topic_key)
            .cloned()
            .expect("topic_adopt maps the session to the new topic");
        assert_eq!(entry.session_id, "ses_alpha01");
        assert_eq!(entry.directory, "/work/auth");
        assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
        // The lobby thread itself got no session.
        let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&lobby_key).is_none());
    }

    /// The switch card's "建话题接管" op rejects a session mapped to another
    /// thread with a Toast (no --force from the card).
    #[tokio::test]
    async fn switch_card_topic_adopt_rejects_owned_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_owned", "被占用的会话", "/work/auth", 100)];
        let mut platform = RecordingPlatform::new();
        platform
            .chat_names
            .insert("oc_group_other".into(), "隔壁群".into());
        let platform = Arc::new(platform);
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
                session_id: "ses_owned".into(),
                directory: "/work/auth".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        let value = serde_json::json!({
            "action": "switch",
            "op": "topic_adopt",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "session_id": "ses_owned",
            "open_message_id": "om_switch_card",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("topic_adopt should return a result");
        assert!(
            result.toast.clone().unwrap_or_default().contains("占用"),
            "owned session rejected with a Toast: {:?}",
            result.toast
        );
        // The lobby thread got no session and no new topic was created for it.
        let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&lobby_key).is_none());
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        assert!(
            app.sessions.lock().await.get_active(&topic_key).is_none(),
            "no new topic mapping for an owned session"
        );
    }

    /// The switch card's "建话题接管" op fails gracefully when the card action
    /// carries no `open_message_id` (the anchor needed to create the topic).
    #[tokio::test]
    async fn switch_card_topic_adopt_missing_open_message_id() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
        let (app, _platform) = build_app(cfg, backend).await;

        let value = serde_json::json!({
            "action": "switch",
            "op": "topic_adopt",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "session_id": "ses_alpha01",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("topic_adopt should return a result");
        assert_eq!(result.card, None, "no card refresh on failure");
        assert!(
            result
                .toast
                .clone()
                .unwrap_or_default()
                .contains("缺少卡片消息引用"),
            "missing open_message_id surfaces a hint: {:?}",
            result.toast
        );
        assert!(app.sessions.lock().await.all_entries().is_empty());
    }

    /// `dir_card_data` derives Recent Directories from the shared store:
    /// children and archived sessions excluded, deduped by directory (keeping
    /// the latest activity), sorted most-recent-first.
    #[tokio::test]
    async fn dir_card_data_dedupes_sorts_and_filters() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        let mut child = list_session("ses_child", "子任务", "/work/a", 999);
        child.parent_id = Some("ses_root".into());
        let mut archived = list_session("ses_arch", "归档", "/work/arch", 888);
        archived.time = Some(opencode::client::SessionTime {
            created: 1,
            updated: 888,
            archived: Some(1),
        });
        backend.session_list = vec![
            list_session("ses_a1", "A1", "/work/a", 100),
            list_session("ses_b", "B", "/work/b", 200),
            list_session("ses_a2", "A2", "/work/a", 300),
            child,
            archived,
        ];
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

        let (dirs, current) = crate::bridge::command::dir_card_data(&app.core, &key).await;
        assert_eq!(dirs, vec!["/work/a".to_string(), "/work/b".to_string()]);
        assert_eq!(current, None);
    }

    /// The `/dir` Recent Directories card's `pick` op re-roots the thread into
    /// the picked directory: it creates a NEW session there (matching the text
    /// `/dir <path>` form), maps it active, and refreshes the card in place.
    #[tokio::test]
    async fn dir_card_pick_creates_session_and_refreshes_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_a", "项目A", "/work/a", 100),
            list_session("ses_b", "项目B", "/work/b", 200),
        ];
        let (app, _platform) = build_app(cfg, backend).await;

        let value = serde_json::json!({
            "action": "dir",
            "op": "pick",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "directory": "/work/b",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("dir pick should return a result");
        assert!(result.card.is_some(), "dir pick refreshes the card");
        assert!(
            result.toast.clone().unwrap_or_default().contains("已切换目录"),
            "dir pick toasts: {:?}",
            result.toast
        );
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&key)
            .cloned()
            .expect("dir pick maps the new session active");
        assert_eq!(entry.directory, "/work/b");
        // The refreshed card marks the new directory as current.
        let card_str = result.card.unwrap().to_string();
        assert!(
            card_str.contains("当前"),
            "refreshed card marks current: {card_str}"
        );
    }

    /// Picking the directory the thread is ALREADY in is a no-op: a Toast, no
    /// new session (mirrors the switch card's "已在当前会话").
    #[tokio::test]
    async fn dir_card_pick_current_directory_toasts_only() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_a", "项目A", "/work/a", 100)];
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_a".into(),
                directory: "/work/a".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        let value = serde_json::json!({
            "action": "dir",
            "op": "pick",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "directory": "/work/a",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("dir pick should return a result");
        assert!(result.card.is_some(), "card still refreshes");
        assert_eq!(
            result.toast.as_deref(),
            Some("已在当前目录"),
            "current dir pick toasts: {:?}",
            result.toast
        );
        // No new session was created: the active entry is unchanged.
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_eq!(entry.session_id, "ses_a");
        assert_eq!(entry.directory, "/work/a");
    }

    /// A bare `/dir` in a bound topic is rejected like the other selection
    /// commands — the Recent Directories card is still a Dir command.
    #[tokio::test]
    async fn dir_card_is_rejected_in_bound_topic() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: topic_key.clone(),
                session_id: "ses_topic".into(),
                directory: "/work/topic".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }
        platform.calls.lock().await.clear();
        crate::bridge::command::handle_command(
            &app.core,
            Command::DirCard,
            topic_key.clone(),
            "msg_topic",
            crate::config::ConversationKind::Topic,
        )
        .await
        .unwrap();
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("回主对话操作"),
            "DirCard must be rejected in a bound topic: {text}"
        );
    }

    /// The `/dir` Recent Directories card's "建话题" op (ADR-0025) wraps a NEW
    /// session in a brand-new topic — the card equivalent of `/topic <dir>`:
    /// cover card at the chat's top level, thread anchored on it, the new
    /// session mapped to the new topic key, the lobby untouched.
    #[tokio::test]
    async fn dir_card_topic_creates_topic_with_new_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_id = "ses_dir".into();
        backend.session_list = vec![list_session("ses_b", "项目B", "/work/b", 200)];
        let (app, platform) = build_app(cfg, backend).await;

        let value = serde_json::json!({
            "action": "dir",
            "op": "topic",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "directory": "/work/b",
            "open_message_id": "om_dir_card",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("dir topic should return a result");
        assert!(result.card.is_some(), "dir topic refreshes the card");
        let toast = result.toast.clone().unwrap_or_default();
        assert!(
            toast.contains("已建话题") && toast.contains("b"),
            "dir topic toasts the created topic: {toast:?}"
        );
        assert!(
            result.card.unwrap().to_string().contains("建话题"),
            "refreshed card keeps the 建话题 rows"
        );

        // The topic pipeline is /topic's (ADR-0023): a cover card leading with
        // the directory basename as the display title goes to the chat's top
        // level, then reply_in_thread on THAT card seeds the topic.
        let calls = platform.calls.lock().await.clone();
        let cover = calls
            .iter()
            .find_map(|c| match c {
                PlatformCall::SendCard { receive_id, card } if receive_id == "chat_1" => {
                    Some(card.to_string())
                }
                _ => None,
            })
            .expect("cover card sent to the chat");
        assert!(
            cover.contains("💬 `b`") && cover.contains("`/work/b`"),
            "cover leads with the directory basename, got: {cover}"
        );
        assert!(
            calls.iter().any(
                |c| matches!(c, PlatformCall::ReplyInThread { message_id, .. } if message_id == "msg_sent")
            ),
            "reply_in_thread anchors on the cover card, got {calls:?}"
        );

        // The new topic owns the NEW session; the lobby stays untouched.
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&topic_key)
            .cloned()
            .expect("dir topic maps the new session to the new topic");
        assert_eq!(entry.session_id, "ses_dir");
        assert_eq!(entry.directory, "/work/b");
        assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
        assert_eq!(entry.topic_root.as_deref(), Some("msg_sent"));
        assert_eq!(
            app.core.cover_titles.lock().await.get("ses_dir").cloned(),
            Some(crate::bridge::core::CoverTitle {
                title: "b".into(),
                model: None
            })
        );
        let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(
            app.sessions.lock().await.get_active(&lobby_key).is_none(),
            "lobby must not gain a session from 建话题"
        );
    }

    /// 建话题 on the CURRENT directory's row is allowed — it equals bare
    /// `/topic` in the current project (mirroring the switch card, whose ✅ 当前
    /// row still carries 建话题接管). Only `pick` is a no-op on the current row.
    #[tokio::test]
    async fn dir_card_topic_on_current_directory_opens_topic() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let (app, _platform) = build_app(cfg, backend).await;
        let lobby_key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: lobby_key.clone(),
                session_id: "ses_a".into(),
                directory: "/work/a".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        let value = serde_json::json!({
            "action": "dir",
            "op": "topic",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "directory": "/work/a",
            "open_message_id": "om_dir_card",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("dir topic should return a result");
        assert!(
            result.toast.clone().unwrap_or_default().contains("已建话题"),
            "current-dir 建话题 opens the topic: {:?}",
            result.toast
        );
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        assert!(
            app.sessions.lock().await.get_active(&topic_key).is_some(),
            "current-dir 建话题 must map the fresh topic"
        );
        // The lobby's own session is unchanged (no re-rooting happened).
        assert_eq!(
            app.sessions
                .lock()
                .await
                .get_active(&lobby_key)
                .map(|e| e.session_id.clone()),
            Some("ses_a".into())
        );
    }

    /// A topic cannot be created from inside a topic (ADR-0006, ADR-0025): a
    /// `/dir` card opened in a never-bound topic — which may legally bind its
    /// session via `pick` — must not nest another topic via 建话题.
    #[tokio::test]
    async fn dir_card_topic_rejects_inside_topic() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let (app, _platform) = build_app(cfg, backend).await;

        let value = serde_json::json!({
            "action": "dir",
            "op": "topic",
            "chat_id": "chat_1",
            "thread_id": "omt_t_1",
            "directory": "/work/b",
            "open_message_id": "om_dir_card",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("dir topic should return a result");
        assert_eq!(result.card, None, "no card refresh on rejection");
        assert!(
            result.toast.clone().unwrap_or_default().contains("回主对话操作"),
            "nested topic creation is rejected with a Toast: {:?}",
            result.toast
        );
        assert!(app.sessions.lock().await.all_entries().is_empty());
    }

    /// The dir card's 建话题 op fails gracefully when the card action carries
    /// no `open_message_id` (the anchor needed to create the topic).
    #[tokio::test]
    async fn dir_card_topic_missing_open_message_id() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let (app, _platform) = build_app(cfg, backend).await;

        let value = serde_json::json!({
            "action": "dir",
            "op": "topic",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "directory": "/work/b",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("dir topic should return a result");
        assert_eq!(result.card, None);
        assert!(
            result
                .toast
                .clone()
                .unwrap_or_default()
                .contains("缺少卡片消息引用"),
            "missing open_message_id surfaces a hint: {:?}",
            result.toast
        );
        assert!(app.sessions.lock().await.all_entries().is_empty());
    }

    /// A chat without topic support returns no thread_id: 建话题 degrades with
    /// a toast pointing at `/dir` or a manual topic — nothing is mapped.
    #[tokio::test]
    async fn dir_card_topic_no_thread_id_degrades_with_guidance() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let mut platform = RecordingPlatform::new();
        platform.reply_in_thread_thread_id = None;
        let platform = Arc::new(platform);
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());

        let value = serde_json::json!({
            "action": "dir",
            "op": "topic",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "directory": "/work/b",
            "open_message_id": "om_dir_card",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("dir topic should return a result");
        assert_eq!(result.card, None);
        assert!(
            result
                .toast
                .clone()
                .unwrap_or_default()
                .contains("不支持创建话题"),
            "no-thread_id chat surfaces guidance: {:?}",
            result.toast
        );
        assert!(app.sessions.lock().await.all_entries().is_empty());
    }

    /// The location-based guard also covers a topic that bound its session via
    /// the row's left button on THIS same card: after `pick` binds the
    /// never-bound topic, clicking 建话题 on the refreshed card must still
    /// reject — the topic now has a session, so it cannot open another topic.
    #[tokio::test]
    async fn dir_card_topic_rejects_after_topic_bound_via_pick() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let (app, _platform) = build_app(cfg, backend).await;
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());

        // Step 1: the never-bound topic binds its single session via `pick`.
        let pick_value = serde_json::json!({
            "action": "dir",
            "op": "pick",
            "chat_id": "chat_1",
            "thread_id": "omt_t_1",
            "directory": "/work/a",
        });
        app.handle_card_action(pick_value)
            .await
            .expect("pick should bind the topic");
        assert!(
            app.sessions.lock().await.get_active(&topic_key).is_some(),
            "pick binds the never-bound topic's session"
        );

        // Step 2: 建话题 on the same thread is rejected — the guard is
        // location-based (thread_id != chat_id), independent of session state.
        let topic_value = serde_json::json!({
            "action": "dir",
            "op": "topic",
            "chat_id": "chat_1",
            "thread_id": "omt_t_1",
            "directory": "/work/b",
            "open_message_id": "om_dir_card",
        });
        let result = app
            .handle_card_action(topic_value)
            .await
            .expect("dir topic should return a result");
        assert_eq!(result.card, None, "no card refresh on rejection");
        assert!(
            result.toast.clone().unwrap_or_default().contains("回主对话操作"),
            "bound topic still rejects nesting: {:?}",
            result.toast
        );
        // Only the original binding remains — no second topic was created.
        assert_eq!(app.sessions.lock().await.all_entries().len(), 1);
    }

    /// The switch card's "建话题接管" op is rejected inside a topic thread too
    /// (ADR-0025 retrofit): the session card may legally open in a never-bound
    /// topic, but its topic op must not nest — same rule as the text forms.
    #[tokio::test]
    async fn switch_card_topic_adopt_rejects_inside_topic() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
        let (app, _platform) = build_app(cfg, backend).await;

        let value = serde_json::json!({
            "action": "switch",
            "op": "topic_adopt",
            "chat_id": "chat_1",
            "thread_id": "omt_t_1",
            "session_id": "ses_alpha01",
            "open_message_id": "om_switch_card",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("topic_adopt should return a result");
        assert_eq!(result.card, None, "no card refresh on rejection");
        assert!(
            result.toast.clone().unwrap_or_default().contains("回主对话操作"),
            "nested topic_adopt is rejected with a Toast: {:?}",
            result.toast
        );
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
        assert!(
            app.sessions.lock().await.get_active(&topic_key).is_none(),
            "no new topic mapping inside a topic"
        );
    }

    #[tokio::test]
    async fn group_root_message_creates_lobby_session_and_shows_guidance() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        // A top-level group message (no thread_id) is the group "lobby".
        app.handle_message(incoming(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        // Guidance text is replied once.
        let calls = platform.calls.lock().await.clone();
        let guidance: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            guidance.iter().any(|t| t.contains("已创建群会话")),
            "expected lobby guidance, got: {:?}",
            guidance
        );

        // Session lives under the lobby key (chat_id == thread_id).
        let lobby_key = crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into());
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&lobby_key)
            .cloned()
            .expect("lobby session created");
        assert_eq!(entry.session_id, "ses_test");
    }

    #[tokio::test]
    async fn group_root_guidance_shown_once_per_lobby() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(incoming(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;
        app.handle_message(incoming(
            "msg_2".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "again".into(),
            None,
        ))
        .await;

        let calls = platform.calls.lock().await.clone();
        let guidance: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            guidance.len(),
            1,
            "guidance must be one-time, got: {:?}",
            guidance
        );
    }

    #[tokio::test]
    async fn p2p_top_level_message_gets_no_guidance() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        app.handle_message(incoming(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        let calls = platform.calls.lock().await.clone();
        let guidance: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            guidance.is_empty(),
            "p2p must not show lobby guidance, got: {:?}",
            guidance
        );
    }

    #[tokio::test]
    async fn topic_message_isolates_session_from_lobby() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        // Seed distinct sessions for the lobby key and the topic key.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "oc_group_1".into()),
                session_id: "ses_lobby".into(),
                directory: "/tmp/lobby".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_1".into(), "omt_topic_1".into()),
                session_id: "ses_topic".into(),
                directory: "/tmp/topic".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }

        // Lobby message routes to the lobby session; topic message routes to
        // the topic session — never creating or switching across.
        app.handle_message(incoming(
            "msg_1".into(),
            "oc_group_1".into(),
            "group".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;
        app.handle_message(incoming(
            "msg_2".into(),
            "oc_group_1".into(),
            "group".into(),
            Some("omt_topic_1".into()),
            "refactor".into(),
            None,
        ))
        .await;

        let store = app.sessions.lock().await;
        let lobby = store
            .get_active(&crate::config::ThreadKey::new(
                "oc_group_1".into(),
                "oc_group_1".into(),
            ))
            .cloned()
            .unwrap();
        let topic = store
            .get_active(&crate::config::ThreadKey::new(
                "oc_group_1".into(),
                "omt_topic_1".into(),
            ))
            .cloned()
            .unwrap();
        assert_eq!(lobby.session_id, "ses_lobby");
        assert_eq!(topic.session_id, "ses_topic");
        assert_ne!(lobby.thread_key, topic.thread_key);
        drop(store);

        // No guidance: the lobby session already existed.
        let calls = platform.calls.lock().await.clone();
        let guidance: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            guidance.is_empty(),
            "no guidance when lobby exists, got: {:?}",
            guidance
        );
    }

    #[tokio::test]
    async fn p2p_topic_isolated_from_p2p_top_level() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        // Seed a p2p top-level session and a p2p topic session.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_p2p_1".into(), "oc_p2p_1".into()),
                session_id: "ses_top".into(),
                directory: "/tmp/top".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_p2p_1".into(), "omt_p2p_1".into()),
                session_id: "ses_p2p_topic".into(),
                directory: "/tmp/ptopic".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }

        app.handle_message(incoming(
            "msg_1".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;
        app.handle_message(incoming(
            "msg_2".into(),
            "oc_p2p_1".into(),
            "p2p".into(),
            Some("omt_p2p_1".into()),
            "topic hi".into(),
            None,
        ))
        .await;

        let store = app.sessions.lock().await;
        let top = store
            .get_active(&crate::config::ThreadKey::new(
                "oc_p2p_1".into(),
                "oc_p2p_1".into(),
            ))
            .cloned()
            .unwrap();
        let topic = store
            .get_active(&crate::config::ThreadKey::new(
                "oc_p2p_1".into(),
                "omt_p2p_1".into(),
            ))
            .cloned()
            .unwrap();
        assert_eq!(top.session_id, "ses_top");
        assert_eq!(topic.session_id, "ses_p2p_topic");
        assert_ne!(top.thread_key, topic.thread_key);
    }

    #[tokio::test]
    async fn stale_session_mapping_is_recreated_on_404() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_id = "ses_new".into();
        backend.stale_session_404 = true;
        let (app, platform) = build_app(cfg, backend).await;

        // Seed stale mappings: thread -> ses_old (active), then ses_old2. When
        // ses_old 404s, cola must create a FRESH session, not fall through to
        // the next stale mapping.
        let thread = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: thread.clone(),
                session_id: "ses_old2".into(),
                directory: "/tmp/old2".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: thread.clone(),
                session_id: "ses_old".into(),
                directory: "/tmp/old".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }

        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;

        // The prompt on the stale session 404s; cola must recreate the session
        // and retry, landing on a Done card instead of an error.
        let calls = platform.calls.lock().await.clone();
        let updates: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
                _ => None,
            })
            .collect();
        assert!(!updates.is_empty(), "expected a card update, got: {:?}", calls);
        let card = updates.last().unwrap().to_string();
        assert!(card.contains("✅"), "expected a Done card, got: {}", card);

        // The store must now map the thread to the recreated session.
        let sid = app
            .sessions
            .lock()
            .await
            .get_active(&thread)
            .map(|e| e.session_id.clone());
        assert_eq!(sid.as_deref(), Some("ses_new"));
    }

    /// ADR-0023: when a cola-created topic's session 404s and is recreated, the
    /// topic's creation messages (`topic_anchor`/`topic_root`) survive the
    /// recreate — they are Feishu message ids, not session state — so the
    /// quote-injection guard keeps working for later replies in the topic.
    #[tokio::test]
    async fn stale_topic_recreate_preserves_creation_messages() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_id = "ses_new".into();
        backend.stale_session_404 = true;
        let prompt_calls = backend.prompt_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;

        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: topic_key.clone(),
                session_id: "ses_old".into(),
                directory: "/work/topic".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some("om_seed".into()),
                topic_root: Some("om_root_cmd".into()),
                variant: None,
            });
        }
        // The cover-title sync state belongs to the topic, not the session —
        // it must move to the recreated session so the hook keeps patching.
        app.core.cover_titles.lock().await.insert(
            "ses_old".into(),
            crate::bridge::core::CoverTitle {
                title: "旧标题".into(),
                model: None,
            },
        );

        // The first message 404s on the stale session; cola recreates and retries.
        app.handle_message(crate::bridge::IncomingMessage {
            message_id: "msg_1".into(),
            chat_id: "chat_1".into(),
            chat_type: "group".into(),
            thread_id: Some("omt_t_1".into()),
            parent_id: Some("om_root_cmd".into()),
            text: "第一条".into(),
            images: vec![],
            requester_open_id: None,
        })
        .await;

        // The recreated entry keeps the topic's creation messages (ADR-0023).
        let entry = app
            .sessions
            .lock()
            .await
            .get_active(&topic_key)
            .cloned()
            .expect("recreated session mapped to the topic");
        assert_eq!(entry.session_id, "ses_new");
        assert_eq!(entry.topic_anchor.as_deref(), Some("om_seed"));
        assert_eq!(entry.topic_root.as_deref(), Some("om_root_cmd"));
        // The cover-title sync state moved to the recreated session id.
        let covers = app.core.cover_titles.lock().await.clone();
        assert!(!covers.contains_key("ses_old"), "stale cover title must move");
        assert_eq!(covers.get("ses_new").map(|c| c.title.as_str()), Some("旧标题"));

        // A later plain reply pointing at the root must still skip injection.
        prompt_calls.lock().await.clear();
        app.handle_message(crate::bridge::IncomingMessage {
            message_id: "msg_2".into(),
            chat_id: "chat_1".into(),
            chat_type: "group".into(),
            thread_id: Some("omt_t_1".into()),
            parent_id: Some("om_root_cmd".into()),
            text: "第二条".into(),
            images: vec![],
            requester_open_id: None,
        })
        .await;
        assert_eq!(
            *prompt_calls.lock().await,
            vec!["第二条".to_string()],
            "the recreated entry must still exclude the topic root from Quoted Context"
        );
    }

    #[tokio::test]
    async fn question_card_action_posts_answer_back() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        // The poll loop has surfaced a pending question request.
        app.question.question_requests.lock().await.insert(
            "que_1".into(),
            opencode::client::QuestionRequest {
                id: "que_1".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::client::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![
                        opencode::client::QuestionOption {
                            label: "/a".into(),
                            description: String::new(),
                        },
                        opencode::client::QuestionOption {
                            label: "/b".into(),
                            description: String::new(),
                        },
                    ],
                    multiple: None,
                    custom: None,
                }],
            },
        );
        // Seed the session → directory mapping so the reply routes correctly.
        {
            let thread = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: thread,
                session_id: "ses_1".into(),
                directory: "/work".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        // User clicks the "/a" option button.
        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "question_index": 0,
            "answer": "/a",
        });
        let result = app.handle_card_action(value).await;
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .card
                .as_ref()
                .unwrap()
                .to_string()
                .contains("已回答")
        );

        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1, "expected one reply_question call");
        assert_eq!(calls[0].0, "que_1");
        assert_eq!(calls[0].1, vec![vec!["/a".to_string()]]);
    }

    #[tokio::test]
    async fn question_card_action_rejects() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        let value = serde_json::json!({
            "action": "question",
            "reply": "reject",
            "request_id": "que_1",
            "session_id": "ses_1",
            "directory": "/work",
        });
        let result = app.handle_card_action(value).await;
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .card
                .as_ref()
                .unwrap()
                .to_string()
                .contains("拒绝")
        );

        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "que_1");
        assert!(calls[0].1[0][0].contains("reject"));
    }

    #[tokio::test]
    async fn double_click_on_same_request_replies_once() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        // A pending single-question request (the same one the card was built for).
        app.question.question_requests.lock().await.insert(
            "que_1".into(),
            opencode::client::QuestionRequest {
                id: "que_1".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::client::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                }],
            },
        );

        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": "/a",
        });

        // First click replies; second (a fast re-click before the result card
        // replaces the buttons) must NOT re-reply — same request, one answer.
        let first = app.handle_card_action(value.clone()).await;
        assert!(first.is_some());
        let second = app.handle_card_action(value).await;
        assert!(second.is_some(), "second click still gets the result card");

        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1, "double click must not double-reply: {:?}", calls);
    }

    #[tokio::test]
    async fn question_with_multiple_parts_waits_for_all_answers() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        let mk_questions = || {
            vec![
                opencode::client::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
                opencode::client::QuestionInfo {
                    question: "选择分支".into(),
                    header: "分支".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "main".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
            ]
        };
        app.question.question_requests.lock().await.insert(
            "que_2".into(),
            opencode::client::QuestionRequest {
                id: "que_2".into(),
                session_id: "ses_1".into(),
                questions: mk_questions(),
            },
        );

        let value = |index: u64, answer: &str| {
            serde_json::json!({
                "action": "question",
                "reply": "answer",
                "request_id": "que_2",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": index,
                "answer": answer,
            })
        };

        // Answer the FIRST question only: must NOT submit (the second is open).
        let first = app.handle_card_action(value(0, "/a")).await;
        assert!(first.is_some());
        let first = first.unwrap();
        assert_eq!(first.toast.as_deref(), Some("已记录答案，还有 1 题未答"));
        // The returned card is still a question card (not a result card).
        let first_card = first.card.as_ref().unwrap().to_string();
        assert!(first_card.contains("❓ AI 想问你"));
        assert!(first_card.contains("已选：/a"));
        assert!(
            !first_card.contains("已选：main"),
            "question 2 not answered yet: {}",
            first_card
        );
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // Answer the SECOND question: now everything is answered → submits.
        let second = app.handle_card_action(value(1, "main")).await;
        assert!(second.is_some());
        assert_eq!(second.unwrap().toast.as_deref(), Some("已回答"));
        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1, "one reply_question call total");
        assert_eq!(calls[0].0, "que_2");
        assert_eq!(calls[0].1, vec![vec!["/a".to_string()], vec!["main".to_string()]]);
    }

    /// A multi-select question (`multiple: true`) never finalizes on an option
    /// click: each click toggles the label in the answer set, the returned card
    /// shows the running selection (已选) and a per-question 确定该题 button, and
    /// only that confirm commits — once it does and every question is answered,
    /// the request auto-submits with the accumulated set.
    #[tokio::test]
    async fn multi_select_question_toggles_until_submit() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());

        app.question.question_requests.lock().await.insert(
            "que_multi".into(),
            opencode::client::QuestionRequest {
                id: "que_multi".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::client::QuestionInfo {
                    question: "选择水果".into(),
                    header: "水果".into(),
                    options: vec![
                        opencode::client::QuestionOption {
                            label: "苹果".into(),
                            description: String::new(),
                        },
                        opencode::client::QuestionOption {
                            label: "香蕉".into(),
                            description: String::new(),
                        },
                        opencode::client::QuestionOption {
                            label: "橙子".into(),
                            description: String::new(),
                        },
                    ],
                    multiple: Some(true),
                    custom: None,
                }],
            },
        );

        let value = |answer: &str| {
            serde_json::json!({
                "action": "question",
                "reply": "answer",
                "request_id": "que_multi",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": 0,
                "answer": answer,
            })
        };

        // Click 苹果 → NOT submitted (multi-select toggles, never auto-submits).
        let r1 = app.handle_card_action(value("苹果")).await.expect("result");
        assert_eq!(r1.toast.as_deref(), Some("已记录选项"));
        let c1 = r1.card.as_ref().expect("re-rendered card").to_string();
        assert!(c1.contains("已选：苹果"), "marker missing: {}", c1);
        assert!(c1.contains("可多选"), "multi hint missing: {}", c1);
        assert!(c1.contains("✅ 确定该题"), "confirm button missing: {}", c1);
        // The selected button shows its ✅/checked state in the card JSON.
        assert!(
            c1.contains("\"content\":\"✅ 苹果\""),
            "selected button state missing: {}",
            c1
        );
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // Click 香蕉 → accumulates a second label.
        let r2 = app.handle_card_action(value("香蕉")).await.expect("result");
        let c2 = r2.card.as_ref().expect("re-rendered card").to_string();
        assert!(c2.contains("已选：苹果、香蕉"), "accumulate failed: {}", c2);
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // Click 苹果 again → toggles it OFF, only 香蕉 remains.
        let r3 = app.handle_card_action(value("苹果")).await.expect("result");
        let c3 = r3.card.as_ref().expect("re-rendered card").to_string();
        assert!(c3.contains("已选：香蕉"), "toggle off failed: {}", c3);
        assert!(!c3.contains("已选：苹果、香蕉"), "toggle off kept 苹果: {}", c3);
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // 确定该题 → commits the toggled set; the single question is answered so
        // the request auto-submits with the accumulated set.
        let confirm = app
            .handle_card_action(serde_json::json!({
                "action": "question",
                "reply": "confirm",
                "request_id": "que_multi",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": 0,
            }))
            .await
            .expect("result");
        assert_eq!(confirm.toast.as_deref(), Some("已回答"));
        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec![vec!["香蕉".to_string()]]);
    }

    /// A multi-select question can be confirmed with an EMPTY selection ("不选"):
    /// the per-question 确定该题 button is always present, and confirming with
    /// nothing toggled replies with an empty answer set.
    #[tokio::test]
    async fn multi_select_can_submit_empty_selection() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());

        app.question.question_requests.lock().await.insert(
            "que_empty".into(),
            opencode::client::QuestionRequest {
                id: "que_empty".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::client::QuestionInfo {
                    question: "选择水果".into(),
                    header: "水果".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "苹果".into(),
                        description: String::new(),
                    }],
                    multiple: Some(true),
                    custom: None,
                }],
            },
        );

        // Toggle 苹果 on, then off → back to an open question with NO selection.
        app.handle_card_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_empty",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": "苹果",
        }))
        .await;
        let r2 = app
            .handle_card_action(serde_json::json!({
                "action": "question",
                "reply": "answer",
                "request_id": "que_empty",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": 0,
                "answer": "苹果",
            }))
            .await
            .expect("result");
        // The re-rendered card has NO selection but STILL shows the 确定该题
        // button, so "不选" is expressible.
        let c2 = r2.card.as_ref().expect("re-rendered card").to_string();
        assert!(!c2.contains("已选"), "selection must be cleared: {}", c2);
        assert!(c2.contains("✅ 确定该题"), "confirm must stay visible: {}", c2);
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // Confirming with nothing selected replies with an empty set.
        let confirm = app
            .handle_card_action(serde_json::json!({
                "action": "question",
                "reply": "confirm",
                "request_id": "que_empty",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": 0,
            }))
            .await
            .expect("result");
        assert_eq!(confirm.toast.as_deref(), Some("已回答"));
        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec![Vec::<String>::new()]);
    }

    /// A stale card may re-confirm an already-confirmed multi-select question
    /// (the re-render hasn't removed the button yet, while other questions stay
    /// open). That second confirm must be a no-op — it must NOT wipe the locked
    /// answer by overwriting it with the now-empty toggles slot.
    #[tokio::test]
    async fn stale_confirm_on_done_multi_select_is_a_no_op() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());

        app.question.question_requests.lock().await.insert(
            "que_stale".into(),
            opencode::client::QuestionRequest {
                id: "que_stale".into(),
                session_id: "ses_1".into(),
                questions: vec![
                    opencode::client::QuestionInfo {
                        question: "选择目录".into(),
                        header: "目录".into(),
                        options: vec![opencode::client::QuestionOption {
                            label: "/a".into(),
                            description: String::new(),
                        }],
                        multiple: None,
                        custom: None,
                    },
                    opencode::client::QuestionInfo {
                        question: "选择水果".into(),
                        header: "水果".into(),
                        options: vec![opencode::client::QuestionOption {
                            label: "苹果".into(),
                            description: String::new(),
                        }],
                        multiple: Some(true),
                        custom: None,
                    },
                ],
            },
        );

        let confirm = |index: u64| {
            serde_json::json!({
                "action": "question",
                "reply": "confirm",
                "request_id": "que_stale",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": index,
            })
        };

        // Toggle 苹果 on Q1 then confirm it → Q1 locked, Q0 still open.
        app.handle_card_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_stale",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 1,
            "answer": "苹果",
        }))
        .await;
        let r1 = app.handle_card_action(confirm(1)).await.expect("result");
        assert_eq!(r1.toast.as_deref(), Some("已确定该题，还有 1 题未答"));
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // A stale second confirm on Q1 must NOT wipe 苹果 with an empty set.
        let r2 = app.handle_card_action(confirm(1)).await.expect("result");
        assert_eq!(r2.toast.as_deref(), Some("已确定该题，还有 1 题未答"));
        let c2 = r2.card.as_ref().expect("re-rendered card").to_string();
        assert!(
            c2.contains("已选：苹果"),
            "stale confirm wiped the answer: {}",
            c2
        );
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // Answer Q0 → both done → submits with 苹果 intact.
        let ans = app
            .handle_card_action(serde_json::json!({
                "action": "question",
                "reply": "answer",
                "request_id": "que_stale",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": 0,
                "answer": "/a",
            }))
            .await
            .expect("result");
        assert_eq!(ans.toast.as_deref(), Some("已回答"));
        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec![vec!["/a".to_string()], vec!["苹果".to_string()]]);
    }

    /// A mixed request (one single-select + one multi-select) only submits once
    /// EVERY question is finalized: toggling multi-select options or typing a
    /// custom answer must never auto-submit, and confirming the multi-select
    /// while the single-select is still open stays in-progress. Only when both
    /// are done does the request reply.
    #[tokio::test]
    async fn mixed_single_and_multi_question_waits_for_all_confirmed() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = Arc::new(MockBackend::new(realistic_parts()));
        let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());

        app.question.question_requests.lock().await.insert(
            "que_mix".into(),
            opencode::client::QuestionRequest {
                id: "que_mix".into(),
                session_id: "ses_1".into(),
                questions: vec![
                    opencode::client::QuestionInfo {
                        question: "选择目录".into(),
                        header: "目录".into(),
                        options: vec![opencode::client::QuestionOption {
                            label: "/a".into(),
                            description: String::new(),
                        }],
                        multiple: None,
                        custom: None,
                    },
                    opencode::client::QuestionInfo {
                        question: "选择水果".into(),
                        header: "水果".into(),
                        options: vec![opencode::client::QuestionOption {
                            label: "苹果".into(),
                            description: String::new(),
                        }],
                        multiple: Some(true),
                        custom: None,
                    },
                ],
            },
        );

        let answer = |index: u64, a: &str| {
            serde_json::json!({
                "action": "question",
                "reply": "answer",
                "request_id": "que_mix",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": index,
                "answer": a,
            })
        };

        // Answer the single-select (Q0) → Q1 still open, NO auto-submit yet.
        let r0 = app.handle_card_action(answer(0, "/a")).await.expect("result");
        assert_eq!(r0.toast.as_deref(), Some("已记录答案，还有 1 题未答"));
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // Toggle a multi-select option (Q1) → still no submit (not confirmed).
        let r1 = app.handle_card_action(answer(1, "苹果")).await.expect("result");
        assert_eq!(r1.toast.as_deref(), Some("已记录选项"));
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // Type a custom answer into the multi-select (reply "custom") → the
        // label is appended to the toggles, still no submit.
        let r2 = app
            .handle_card_action(serde_json::json!({
                "action": "question",
                "reply": "custom",
                "request_id": "que_mix",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": 1,
                "answer": "自定义水果",
            }))
            .await
            .expect("result");
        assert_eq!(r2.toast.as_deref(), Some("已记录选项"));
        // The displayed selection now holds the option + the custom label.
        let c2 = r2.card.as_ref().expect("re-rendered card").to_string();
        assert!(c2.contains("已选：苹果、自定义水果"), "append failed: {}", c2);
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

        // Confirm Q1 → both questions done → auto-submits with both answers.
        let confirm = app
            .handle_card_action(serde_json::json!({
                "action": "question",
                "reply": "confirm",
                "request_id": "que_mix",
                "session_id": "ses_1",
                "directory": "/work",
                "question_index": 1,
            }))
            .await
            .expect("result");
        assert_eq!(confirm.toast.as_deref(), Some("已回答"));
        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1,
            vec![
                vec!["/a".to_string()],
                vec!["苹果".to_string(), "自定义水果".to_string()]
            ]
        );
    }

    /// A question raised during an active turn is surfaced INLINE on the
    /// streaming card; answering one of several only toasts and returns the
    /// re-rendered streaming card (markers/已选 in the callback response — the
    /// mechanism Feishu actually refreshes the clicked card with), while the
    /// final answer finalizes the request and drops the inline section.
    #[tokio::test]
    async fn inline_question_answered_on_streaming_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut mock = MockBackend::new(realistic_parts());
        mock.questions = vec![opencode::client::QuestionRequest {
            id: "que_inline".into(),
            session_id: "ses_test".into(),
            questions: vec![
                opencode::client::QuestionInfo {
                    question: "选目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
                opencode::client::QuestionInfo {
                    question: "选分支".into(),
                    header: "分支".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "main".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
            ],
        }];
        let backend = Arc::new(mock);
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

        // Seed a session + active accumulator (an in-flight turn).
        app.handle_message(incoming(
            "msg_1".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;
        assert!(
            app.cards.lock().await.contains_key("ses_test"),
            "accumulator expected"
        );

        // Run the question poller → the question is inlined on the accumulator.
        tokio::spawn({
            let app = app.clone();
            async move {
                app.question
                    .poll_interval_ms
                    .store(50, std::sync::atomic::Ordering::Relaxed);
                let _ = app.question.poll_loop(&app.core).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let pending = app
            .cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .pending_questions
            .clone();
        assert_eq!(pending.len(), 1, "question should be inlined");
        assert_eq!(pending[0].request_id, "que_inline");

        // Answer the first question → toast only, no card replacement.
        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_inline",
            "session_id": "ses_test",
            "question_index": 0,
            "answer": "/a",
        });
        let r1 = app.handle_card_action(value).await.expect("result");
        assert_eq!(r1.toast.as_deref(), Some("已记录答案，还有 1 题未答"));
        // The returned card is the RE-RENDERED streaming card (markers in the
        // callback response — the reliable card-update mechanism). Feishu's
        // PATCH alone leaves the clicked card on its pre-answer state.
        let r1_card = r1
            .card
            .as_ref()
            .expect("inline answer must return the rebuilt card");
        let r1_text = r1_card.to_string();
        assert!(r1_text.contains("已选：/a"), "marker missing: {}", r1_text);
        assert!(!r1_text.contains("已选：main"), "q2 must stay open: {}", r1_text);
        assert_eq!(backend.reply_question_calls.lock().await.len(), 0);
        // The accumulator's inline question reflects the partial answer.
        let pending = app
            .cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .pending_questions
            .clone();
        assert_eq!(pending[0].answers[0], Some(vec!["/a".to_string()]));
        assert_eq!(pending[0].answers[1], None);

        // Answer the second → finalized, reply called, inline section removed.
        let value = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_inline",
            "session_id": "ses_test",
            "question_index": 1,
            "answer": "main",
        });
        let r2 = app.handle_card_action(value).await.expect("result");
        assert_eq!(r2.toast.as_deref(), Some("已回答"));
        assert!(r2.card.is_none(), "inline final answer must not replace the card");
        let calls = backend.reply_question_calls.lock().await.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec![vec!["/a".to_string()], vec!["main".to_string()]]);
        assert!(
            app.cards
                .lock()
                .await
                .get("ses_test")
                .unwrap()
                .acc
                .pending_questions
                .is_empty()
        );
    }

    /// When a turn is already in flight, a new message must NOT start a
    /// competing run_prompt (which would overwrite the running accumulator and
    /// race on the same card). It goes through the supplement path: the message
    /// is sent fire-and-forget via prompt_async (OpenCode merges it into the
    /// current turn) and the user gets a notice — no Loading card, no second
    /// accumulator.
    #[tokio::test]
    async fn message_during_inflight_goes_to_supplement_path() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let sup_calls = backend.prompt_async_calls.clone();
        let sup_ids = backend.prompt_async_message_ids.clone();
        let (app, platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        app.inflight.lock().await.insert("ses_test".to_string());

        app.handle_message(incoming(
            "msg_sup".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "补充一下，改用方案 B".into(),
            None,
        ))
        .await;

        // prompt_async was called with the supplement text.
        let calls = sup_calls.lock().await.clone();
        assert!(
            calls.iter().any(|c| c.contains("补充一下，改用方案 B")),
            "supplement text must be sent via prompt_async: {:?}",
            calls
        );
        // The supplement is a cola-authored message: fresh `msg_cola_` id
        // (ADR-0026).
        let ids = sup_ids.lock().await.clone();
        assert!(
            ids.iter()
                .all(|id| id.as_deref().is_some_and(|i| i.starts_with("msg_cola_"))),
            "every supplement must self-identify as cola-authored: {:?}",
            ids
        );

        // NO Loading card / run_prompt was started for the supplement message.
        let sent = platform.calls.lock().await.clone();
        assert!(
            sent.iter().all(|c| matches!(c, PlatformCall::ReplyText { .. })),
            "supplement must only reply text, not start a card: {:?}",
            sent
        );
        // The in-flight marker is preserved (still running).
        assert!(app.inflight.lock().await.contains("ses_test"));
    }

    // ===== Session discovery & adoption (ADR-0008) =====

    /// A helper: a session in the shared store with the given title/dir/id.
    fn list_session(
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

    #[tokio::test]
    async fn list_shows_global_sessions_marking_own() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "外部会话", "/tmp/ext", 100),
            list_session("ses_beta02", "本地会话", "/work/cola", 300),
        ];
        let (app, platform) = build_app(cfg, backend).await;
        // Our own lobby session, so /list marks it active (ADR-0022: only the
        // active session is marked; the 本会话 ownership marker is gone).
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_beta02".into(),
                directory: "/work/cola".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::List {
                keyword: None,
                all: false,
            }),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_list",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("外部会话"), "external session visible: {text}");
        assert!(text.contains("本地会话"), "own session visible: {text}");
        // The newest (updated 300) sorts first; own session marked active.
        let pos_ext = text.find("外部会话").unwrap();
        let pos_local = text.find("本地会话").unwrap();
        assert!(pos_local < pos_ext, "own (newer) session sorts first: {text}");
        assert!(text.contains("(active)"), "active session marked: {text}");
        assert!(!text.contains("本会话"), "ownership marker dropped: {text}");
    }

    #[tokio::test]
    async fn list_filters_by_keyword_and_hides_children() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "重写登录模块", "/work/auth", 100),
            list_session("ses_beta02", "修 bug", "/work/cola", 300),
            opencode::client::SessionListInfo {
                parent_id: Some("ses_alpha01".into()),
                ..list_session("ses_child09", "Child session - x", "/work/auth", 400)
            },
        ];
        let (app, platform) = build_app(cfg, backend).await;

        // Keyword filters by title.
        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::List {
                keyword: Some("登录".into()),
                all: false,
            }),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_list",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("重写登录模块"), "keyword match: {text}");
        assert!(!text.contains("修 bug"), "non-matching title filtered: {text}");

        // Without --all the child is hidden even though it is newest.
        platform.calls.lock().await.clear();
        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::List {
                keyword: None,
                all: false,
            }),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_list2",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("Child session"), "child hidden by default: {text}");

        // --all reveals the child.
        platform.calls.lock().await.clear();
        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::List {
                keyword: None,
                all: true,
            }),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_list3",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("child09"), "child shown with --all: {text}");
    }

    /// Repeated `/list` within the 30 s TTL must not re-hit the server; an
    /// external rename is only visible after invalidation/expiry.
    #[tokio::test]
    async fn list_is_cached_within_ttl_and_invalidated_on_rename() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_alpha01", "标题 A", "/work/a", 100)];
        let calls_counter = backend.list_sessions_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_alpha01".into(),
                directory: "/work/a".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        // Two /list in a row → one server fetch.
        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::List {
                keyword: None,
                all: false,
            }),
            key.clone(),
            "m1",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::List {
                keyword: None,
                all: false,
            }),
            key.clone(),
            "m2",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A rename invalidates the cache → next /list refetches.
        crate::bridge::command::handle_command(
            &app.core,
            Command::Name("新名字".into()),
            key.clone(),
            "m3",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::List {
                keyword: None,
                all: false,
            }),
            key,
            "m4",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert_eq!(calls_counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn attach_adopts_foreign_session_by_id() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_foreign123abc",
            "OpenChamber 里的任务",
            "/work/foreign",
            100,
        )];
        let (app, _platform) = build_app(cfg, backend).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Attach {
                query: "ses_foreign123abc".into(),
                force: false,
            }),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_attach",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The thread now maps to the foreign session with its directory.
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_eq!(entry.session_id, "ses_foreign123abc");
        assert_eq!(entry.directory, "/work/foreign");
    }

    #[tokio::test]
    async fn attach_rejects_session_owned_by_another_thread_without_force() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_foreign123abc",
            "OpenChamber 里的任务",
            "/work/foreign",
            100,
        )];
        let mut platform = RecordingPlatform::new();
        platform
            .chat_names
            .insert("oc_group_other".into(), "隔壁群".into());
        let platform = Arc::new(platform);
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
        // Another thread already owns the session.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
                session_id: "ses_foreign123abc".into(),
                directory: "/work/foreign".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Attach {
                query: "ses_foreign123abc".into(),
                force: false,
            }),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_attach",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Rejected: the current thread still has no session.
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&key).is_none());
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("隔壁群"), "rejection names the owning chat: {text}");
        assert!(text.contains("--force"), "rejection points at --force: {text}");
    }

    #[tokio::test]
    async fn attach_force_steals_mapping_from_other_thread() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session(
            "ses_foreign123abc",
            "OpenChamber 里的任务",
            "/work/foreign",
            100,
        )];
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into()),
                session_id: "ses_foreign123abc".into(),
                directory: "/work/foreign".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Attach {
                query: "ses_foreign123abc".into(),
                force: true,
            }),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_attach",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Stolen: current thread owns it, other thread is sessionless.
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert_eq!(
            app.sessions.lock().await.get_active(&key).unwrap().session_id,
            "ses_foreign123abc"
        );
        let other = crate::config::ThreadKey::new("oc_group_other".into(), "oc_group_other".into());
        assert!(app.sessions.lock().await.get_active(&other).is_none());
    }

    #[tokio::test]
    async fn forget_unmaps_thread_keeping_server_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Forget),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_forget",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&key).is_none());
    }

    #[tokio::test]
    async fn switch_adopts_unique_foreign_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
        let (app, _platform) = build_app(cfg, backend).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Match("唯一外部标题".into())),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_switch",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_eq!(entry.session_id, "ses_alpha01");
        assert_eq!(entry.directory, "/work/ext");
    }

    #[tokio::test]
    async fn switch_ambiguous_global_match_lists_candidates() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "任务 A", "/work/a", 100),
            list_session("ses_beta02", "任务 B", "/work/b", 200),
        ];
        let (app, platform) = build_app(cfg, backend).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Match("任务".into())),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_switch",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // Ambiguous → no adoption, candidates listed.
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert!(app.sessions.lock().await.get_active(&key).is_none());
        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("/switch"), "points at /switch: {text}");
    }

    #[tokio::test]
    async fn switch_prefers_threads_own_sessions() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_own1", "本项目会话", "/work/cola", 500),
            list_session("ses_foreign", "本项目会话", "/other/place", 100),
        ];
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_own1".into(),
                directory: "/work/cola".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_other_own".into(),
                directory: "/work/other".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Match("本项目".into())),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_switch",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The thread's own session wins (mapping unchanged, just active).
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert_eq!(
            app.sessions.lock().await.get_active(&key).unwrap().session_id,
            "ses_own1"
        );
    }

    /// `/switch` (no args) sends the interactive session card: a search form,
    /// one row per session with a switch/adopt button, and a "＋new" footer.
    #[tokio::test]
    async fn switch_no_arg_sends_session_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "重写登录", "/work/auth", 100),
            list_session("ses_beta02", "修 bug", "/work/cola", 300),
        ];
        let (app, platform) = build_app(cfg, backend).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Card),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_switch_card",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let card = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .next()
            .expect("a switch card should be sent");
        let text = card.to_string();
        assert!(text.contains("会话管理"), "header: {text}");
        assert!(text.contains("重写登录"), "session row: {text}");
        assert!(text.contains("修 bug"), "session row: {text}");
        assert!(text.contains("接管"), "adopt button: {text}");
        assert!(text.contains("＋ 新建会话"), "new button: {text}");
        assert!(text.contains("switch_search"), "search form: {text}");
    }

    /// `/switch` (no args) defaults to the current directory (ADR-0022): with
    /// an active session in /work/cola, the card shows only that directory's
    /// sessions, a header naming the directory, and a 全部 toggle to widen.
    #[tokio::test]
    async fn switch_card_defaults_to_current_directory_scope() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "重写登录", "/work/auth", 100),
            list_session("ses_beta02", "修 bug", "/work/cola", 300),
        ];
        let (app, platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_beta02".into(),
                directory: "/work/cola".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Card),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_switch_card",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let card = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .next()
            .expect("a switch card should be sent");
        let text = card.to_string();
        assert!(text.contains("cola 的会话"), "header names the directory: {text}");
        assert!(text.contains("修 bug"), "own-directory session shown: {text}");
        assert!(
            !text.contains("重写登录"),
            "other-directory session hidden: {text}"
        );
        assert!(text.contains("全部"), "scope-widen toggle present: {text}");
        assert!(
            text.contains("\"scope\":\"dir\""),
            "search carries dir scope: {text}"
        );
    }

    /// The 全部 toggle widens the switch card from the current directory to the
    /// whole store, and the card then offers 本目录 to scope back down.
    #[tokio::test]
    async fn switch_card_scope_toggle_shows_whole_store() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "重写登录", "/work/auth", 100),
            list_session("ses_beta02", "修 bug", "/work/cola", 300),
        ];
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_beta02".into(),
                directory: "/work/cola".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        let value = serde_json::json!({
            "action": "switch",
            "op": "scope",
            "scope": "all",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("scope toggle should return a card");
        let card = result.card.expect("toggle rebuilds the card");
        let text = card.to_string();
        assert!(text.contains("最近会话"), "all-scope header: {text}");
        assert!(text.contains("修 bug"), "own-directory session: {text}");
        assert!(
            text.contains("重写登录"),
            "other-directory session now visible: {text}"
        );
        assert!(text.contains("本目录"), "scope-back toggle present: {text}");
    }

    /// Without an active session the switch card has no directory to scope to,
    /// so it falls back to the whole store and omits the toggle.
    #[tokio::test]
    async fn switch_card_falls_back_to_global_without_active_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![
            list_session("ses_alpha01", "重写登录", "/work/auth", 100),
            list_session("ses_beta02", "修 bug", "/work/cola", 300),
        ];
        let (app, platform) = build_app(cfg, backend).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Card),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_switch_card",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let card = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .next()
            .expect("a switch card should be sent");
        let text = card.to_string();
        assert!(text.contains("最近会话"), "global fallback header: {text}");
        assert!(text.contains("重写登录"), "all sessions shown: {text}");
        assert!(text.contains("修 bug"), "all sessions shown: {text}");
        assert!(
            !text.contains("本目录"),
            "no scope-back toggle without a directory: {text}"
        );
    }

    /// A `/switch` card "adopt" action maps the session into the thread and
    /// returns a refreshed card + toast in the ack.
    #[tokio::test]
    async fn switch_card_adopt_action_maps_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_alpha01", "重写登录", "/work/auth", 100)];
        let (app, _platform) = build_app(cfg, backend).await;

        let value = serde_json::json!({
            "action": "switch",
            "op": "adopt",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "session_id": "ses_alpha01",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("switch adopt should return a result");
        assert!(
            result.card.is_some(),
            "adopt returns a refreshed card: {:?}",
            result.card
        );
        assert!(
            result.toast.clone().unwrap_or_default().contains("接管"),
            "adopt toasts: {:?}",
            result.toast
        );
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        assert_eq!(
            app.sessions.lock().await.get_active(&key).unwrap().session_id,
            "ses_alpha01"
        );
    }

    /// A `/switch` card "new" action creates a session in the current project
    /// (equivalent to `/new`) and maps it as active.
    #[tokio::test]
    async fn switch_card_new_action_creates_session_in_current_project() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        // Root an existing session in /work/proj so "current project" is set.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_old".into(),
                directory: "/work/proj".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        let value = serde_json::json!({
            "action": "switch",
            "op": "new",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("switch new should return a result");
        assert!(result.card.is_some(), "new returns a refreshed card");
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_ne!(entry.session_id, "ses_old", "a fresh session is created");
        assert_eq!(entry.directory, "/work/proj", "inherits the current project");
    }

    /// `/model` (no args) sends the provider-picker card (step 1 of the
    /// two-level flow): one button per provider, not per model.
    #[tokio::test]
    async fn model_no_arg_sends_picker_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![crate::opencode::client::ProviderModels {
            provider: "opencode".into(),
            models: vec![
                model_option("deepseek-v4-flash", &[]),
                model_option("gpt-4o", &[]),
            ],
        }];
        let (app, platform) = build_app(cfg, backend).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::ModelCard,
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_model_card",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let card = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .next()
            .expect("a model card should be sent");
        let text = card.to_string();
        assert!(text.contains("选择模型"), "header: {text}");
        assert!(text.contains("选择 provider"), "intro: {text}");
        assert!(text.contains("\"value\":\"opencode\""), "provider button: {text}");
        assert!(
            !text.contains("deepseek-v4-flash"),
            "step 1 must not show models: {text}"
        );
    }

    /// A `/model` provider-picker button (level `provider`) opens that
    /// provider's model picker; a model button (level `model`) records the
    /// per-session override.
    #[tokio::test]
    async fn model_card_button_records_override() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![crate::opencode::client::ProviderModels {
            provider: "opencode".into(),
            models: vec![
                model_option("deepseek-v4-flash", &[]),
                model_option("gpt-4o", &[]),
            ],
        }];
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        // Step 1: pick a provider → the ack card becomes that provider's model
        // picker.
        let provider = serde_json::json!({
            "action": "model",
            "level": "provider",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "opencode",
        });
        let step1 = app
            .handle_card_action(provider)
            .await
            .expect("provider card action");
        let step1_card = step1.card.expect("provider click swaps in the model picker");
        let text1 = step1_card.to_string();
        assert!(
            text1.contains("opencode/deepseek-v4-flash"),
            "model button: {text1}"
        );
        assert!(text1.contains("返回全部 provider"), "back button: {text1}");

        // Step 2: pick a model → the override is recorded (toast only, no card).
        let model = serde_json::json!({
            "action": "model",
            "level": "model",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "opencode/deepseek-v4-flash",
        });
        let step2 = app.handle_card_action(model).await.expect("model card action");
        assert!(step2.card.is_none(), "selection is toast-only");
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_eq!(entry.model.as_deref(), Some("opencode/deepseek-v4-flash"));
    }

    /// The model picker's back button (level `provider`, `__providers__`)
    /// returns to the full provider list.
    #[tokio::test]
    async fn model_picker_back_button_returns_to_providers() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![
            crate::opencode::client::ProviderModels {
                provider: "opencode".into(),
                models: vec![model_option("deepseek-v4-flash", &[])],
            },
            crate::opencode::client::ProviderModels {
                provider: "openrouter".into(),
                models: vec![model_option("gpt-4o", &[])],
            },
        ];
        let (app, _platform) = build_app(cfg, backend).await;
        let value = serde_json::json!({
            "action": "model",
            "level": "provider",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "__providers__",
        });
        let result = app.handle_card_action(value).await.expect("back action");
        let card = result.card.expect("back returns a card");
        let text = card.to_string();
        assert!(text.contains("opencode") && text.contains("openrouter"), "{text}");
        assert!(
            !text.contains("deepseek-v4-flash"),
            "provider list, not models: {text}"
        );
    }

    /// `/agent` (no args) sends the agent-picker card: it shows the CURRENT
    /// agent (the server's default when no override is set — derived as the
    /// first primary non-hidden agent, skipping subagents), records an override
    /// from a button, treats an agent literally named `default` as a normal
    /// pick, and clears via the dedicated `agent_clear` action.
    #[tokio::test]
    async fn agent_card_picker_and_button() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.agents = vec![
            crate::opencode::client::AgentInfo {
                name: "sec-agent".into(),
                description: Some("a subagent".into()),
                mode: Some("subagent".into()),
                hidden: Some(false),
            },
            crate::opencode::client::AgentInfo {
                name: "build".into(),
                description: Some("build agent".into()),
                mode: Some("primary".into()),
                hidden: Some(false),
            },
            crate::opencode::client::AgentInfo {
                name: "default".into(),
                description: Some("an agent literally named default".into()),
                mode: Some("primary".into()),
                hidden: Some(false),
            },
        ];
        let (app, platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::AgentCard,
            key.clone(),
            "msg_agent_card",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        let calls = platform.calls.lock().await.clone();
        let card = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .next()
            .expect("an agent card should be sent");
        let text = card.to_string();
        assert!(text.contains("当前 Agent"), "current label: {text}");
        // The subagent sorts first in the fixture but is skipped: `build` is
        // the derived server default.
        assert!(text.contains("`build`（默认）"), "default agent: {text}");
        assert!(
            text.contains("\"action\":\"agent_clear\""),
            "clear action: {text}"
        );
        assert!(
            text.contains("\"value\":\"default\""),
            "an agent named `default` is a selectable value: {text}"
        );

        // An agent literally named `default` is a normal pick, never a clear.
        let literal_default = serde_json::json!({
            "action": "agent",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "default",
        });
        let result = app
            .handle_card_action(literal_default)
            .await
            .expect("agent literal-default action");
        assert!(result.card.is_some(), "refreshed card returned");
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_eq!(entry.agent.as_deref(), Some("default"));

        // A real pick records the override.
        let value = serde_json::json!({
            "action": "agent",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "build",
        });
        let result = app.handle_card_action(value).await.expect("agent card action");
        assert!(result.card.is_some(), "refreshed card returned");
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_eq!(entry.agent.as_deref(), Some("build"));

        // The dedicated `agent_clear` action clears it (mechanism, not value).
        let clear = serde_json::json!({
            "action": "agent_clear",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "",
        });
        app.handle_card_action(clear).await.expect("agent clear action");
        assert!(
            app.sessions
                .lock()
                .await
                .get_active(&key)
                .and_then(|e| e.agent.clone())
                .is_none(),
            "agent_clear must clear the override"
        );
    }

    /// `/agent --reset` (text) clears the per-session override; a bare agent
    /// name never clears — it is always a pick.
    #[tokio::test]
    async fn agent_text_reset_flag_clears_override() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: Some("build".into()),
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Agent("--reset".into()),
            key.clone(),
            "msg_agent_reset",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .and_then(|e| e.agent.clone())
                .is_none(),
            "--reset must clear the agent override"
        );
        let text = platform
            .calls
            .lock()
            .await
            .clone()
            .into_iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("已清除 Agent"), "clear reply: {text}");

        // A bare name never clears — it records the override.
        crate::bridge::command::handle_command(
            &app.core,
            Command::Agent("build".into()),
            key.clone(),
            "msg_agent_set",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert_eq!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .and_then(|e| e.agent.clone())
                .as_deref(),
            Some("build"),
            "a bare agent name must be recorded, not clear"
        );
    }

    /// `/autoaccept` (no args) sends the toggle card; a button flips the flag.
    #[tokio::test]
    async fn autoaccept_card_toggles_flag() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Status),
            key.clone(),
            "msg_aa",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        let calls = platform.calls.lock().await.clone();
        let card = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .next()
            .expect("an autoaccept card should be sent");
        assert!(card.to_string().contains("自动审批"), "toggle card: {card}");

        let value = serde_json::json!({
            "action": "autoaccept",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "on",
        });
        let result = app
            .handle_card_action(value)
            .await
            .expect("autoaccept card action");
        assert!(result.card.is_some(), "refreshed card returned");
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert!(entry.auto_accept, "flag should flip on");
    }

    #[tokio::test]
    async fn name_patches_server_title() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let title_calls = backend.update_title_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Name("新名字".into()),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_name",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        assert_eq!(
            title_calls.lock().await.as_slice(),
            &[("ses_test".to_string(), "新名字".to_string())]
        );
    }

    /// ADR-0023: `/name` inside a cover-rooted topic patches the cover card
    /// IMMEDIATELY (no waiting for the next completed turn) — the chat-list
    /// topic entry is the card's content.
    #[tokio::test]
    async fn name_patches_cover_card_in_cover_rooted_topic() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        // The server title after the PATCH (the mock records the call but the
        // GET /session/{id} response comes from this map).
        backend
            .session_titles
            .lock()
            .unwrap()
            .insert("ses_test".into(), "新名字".into());
        let (app, platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some("om_seed".into()),
                topic_root: Some("om_cover".into()),
                variant: None,
            });
        }
        app.core.cover_titles.lock().await.insert(
            "ses_test".into(),
            crate::bridge::core::CoverTitle {
                title: "旧标题".into(),
                model: None,
            },
        );

        crate::bridge::command::handle_command(
            &app.core,
            Command::Name("新名字".into()),
            crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            "msg_name",
            crate::config::ConversationKind::Topic,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let patched = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { message_id, card } if message_id == "om_cover" => {
                    Some(card.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !patched.is_empty(),
            "the cover card must be patched right away, got {calls:?}"
        );
        assert!(
            patched.last().unwrap().contains("新名字"),
            "cover card must show the new title: {:?}",
            patched.last()
        );
        assert_eq!(
            app.core
                .cover_titles
                .lock()
                .await
                .get("ses_test")
                .map(|c| c.title.clone()),
            Some("新名字".to_string())
        );
    }

    /// ADR-0023: the server's default title (`New session - <ts>`) must never
    /// be patched onto the cover card — it would replace the meaningful
    /// creation title (the directory name) before the auto-title exists.
    #[tokio::test]
    async fn default_server_title_does_not_patch_cover_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        backend
            .session_titles
            .lock()
            .unwrap()
            .insert("ses_test".into(), "New session - 2024-12-14T05:33:00.000Z".into());
        let (app, platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some("om_seed".into()),
                topic_root: Some("om_cover".into()),
                variant: None,
            });
        }
        app.core.cover_titles.lock().await.insert(
            "ses_test".into(),
            crate::bridge::core::CoverTitle {
                title: "cola".into(),
                model: None,
            },
        );

        let settled = crate::bridge::command::sync_topic_cover_title(&app.core, "ses_test").await;

        let calls = platform.calls.lock().await.clone();
        assert!(
            !calls.iter().any(
                |c| matches!(c, PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_cover")
            ),
            "the default title must not be patched onto the cover card: {calls:?}"
        );
        assert_eq!(
            app.core
                .cover_titles
                .lock()
                .await
                .get("ses_test")
                .map(|c| c.title.clone()),
            Some("cola".to_string())
        );
        assert!(
            !settled,
            "a default title must signal 'retry later', not 'settled'"
        );
    }

    /// ADR-0023: when the auto-title lands shortly AFTER a short turn ends,
    /// the retry ladder (backoff) patches the cover card anyway — the title
    /// agent races the turn, so the cover must not depend on the user sending
    /// another message.
    #[tokio::test]
    async fn cover_title_retry_ladder_catches_late_auto_title() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let titles = backend.session_titles.clone();
        let (app, platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some("om_seed".into()),
                topic_root: Some("om_cover".into()),
                variant: None,
            });
        }
        app.core.cover_titles.lock().await.insert(
            "ses_test".into(),
            crate::bridge::core::CoverTitle {
                title: "cola".into(),
                model: None,
            },
        );

        // The title is NOT on the server when the turn ends; the ladder starts
        // and the title lands a moment later.
        crate::bridge::command::spawn_cover_title_retry_at(
            &app.core,
            "ses_test",
            &[
                std::time::Duration::from_millis(50),
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(200),
            ],
        );
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        titles
            .lock()
            .unwrap()
            .insert("ses_test".into(), "迟到标题".into());

        // Wait past the ladder window; the 100ms attempt must catch it.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        let calls = platform.calls.lock().await.clone();
        let patched = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::UpdateMessage { message_id, card } if message_id == "om_cover" => {
                    Some(card.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !patched.is_empty(),
            "the retry ladder must patch the cover card once the title lands: {calls:?}"
        );
        assert!(
            patched.last().unwrap().contains("迟到标题"),
            "cover card must show the late title: {:?}",
            patched.last()
        );
    }

    /// `/model <provider/model>` records a per-session override (the OpenCode
    /// server has no model-switch endpoint) and the NEXT prompt carries it.
    #[tokio::test]
    async fn model_command_records_override_used_on_next_prompt() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_models = backend.prompt_models.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Model("opencode-go/deepseek-v4-flash".into()),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_model",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The override is recorded on the session's persisted entry.
        let stored = {
            let store = app.sessions.lock().await;
            store.entry_for_session("ses_test").and_then(|e| e.model.clone())
        };
        assert_eq!(stored.as_deref(), Some("opencode-go/deepseek-v4-flash"));
        // The override is NOT applied to an unrelated session.
        assert!(app.sessions.lock().await.entry_for_session("ses_other").is_none());

        // The next message prompts with the override as the model.
        app.handle_message(incoming(
            "msg_prompt".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;
        assert_eq!(
            prompt_models.lock().await.as_slice(),
            &[Some("opencode-go/deepseek-v4-flash".to_string())]
        );
    }

    /// `/think <variant>` records a per-session override (the OpenCode server
    /// has no thinking-level endpoint) and the NEXT prompt carries it as the
    /// per-prompt `variant`.
    #[tokio::test]
    async fn think_command_records_variant_used_on_next_prompt() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_variants = backend.prompt_variants.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: Some("opencode-go/deepseek-v4-flash".into()),
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Think("high".into()),
            key.clone(),
            "msg_think",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let stored = {
            let store = app.sessions.lock().await;
            store
                .entry_for_session("ses_test")
                .and_then(|e| e.variant.clone())
        };
        assert_eq!(stored.as_deref(), Some("high"));

        // The next message prompts with the variant.
        app.handle_message(incoming(
            "msg_prompt".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;
        assert_eq!(
            prompt_variants.lock().await.as_slice(),
            &[Some("high".to_string())]
        );
    }

    /// `/think` validates the variant against the effective model's declared
    /// set: an undeclared value is rejected with feedback and nothing is stored.
    #[tokio::test]
    async fn think_command_rejects_undeclared_variant() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![crate::opencode::client::ProviderModels {
            provider: "opencode-go".into(),
            models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
        }];
        let (app, platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: Some("opencode-go/deepseek-v4-flash".into()),
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Think("medium".into()),
            key.clone(),
            "msg_think",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .next()
            .expect("a rejection reply should be sent");
        assert!(text.contains("不支持思考等级"), "rejection: {text}");
        assert!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .and_then(|e| e.variant.clone())
                .is_none(),
            "an undeclared variant must not be stored"
        );
    }

    /// `/think --reset` clears the override; the bare words `default`/`off`/`reset`
    /// are ordinary variant picks now (never clear words — ADR-0020).
    #[tokio::test]
    async fn think_reset_flag_clears_variant() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: Some("high".into()),
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Think("--reset".into()),
            key.clone(),
            "msg_think",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        assert!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .and_then(|e| e.variant.clone())
                .is_none(),
            "--reset must clear the variant"
        );
    }

    /// A variant literally named `default` is a normal pick, never a clear
    /// word: when the effective model doesn't declare it, `/think default` is
    /// rejected (ADR-0020 decoupling).
    #[tokio::test]
    async fn think_bare_default_undeclared_is_rejected() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![crate::opencode::client::ProviderModels {
            provider: "opencode-go".into(),
            models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
        }];
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: Some("opencode-go/deepseek-v4-flash".into()),
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: Some("high".into()),
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Think("default".into()),
            key.clone(),
            "msg_think",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .and_then(|e| e.variant.clone())
                .is_some(),
            "an undeclared bare word must not clear the variant"
        );
    }

    /// A variant literally named `default` that the model DOES declare is
    /// stored as the override — the value namespace is never overloaded by the
    /// clear mechanism (ADR-0020).
    #[tokio::test]
    async fn think_bare_default_declared_is_stored() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![crate::opencode::client::ProviderModels {
            provider: "opencode-go".into(),
            models: vec![model_option("deepseek-v4-flash", &["low", "default"])],
        }];
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: Some("opencode-go/deepseek-v4-flash".into()),
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Think("default".into()),
            key.clone(),
            "msg_think",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert_eq!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .and_then(|e| e.variant.clone())
                .as_deref(),
            Some("default"),
            "a declared `default` variant must be stored"
        );
    }

    /// `/think` (no args) sends the variant-picker card listing the effective
    /// model's declared variants.
    #[tokio::test]
    async fn think_no_arg_sends_variant_card() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![crate::opencode::client::ProviderModels {
            provider: "opencode-go".into(),
            models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
        }];
        let (app, platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: Some("opencode-go/deepseek-v4-flash".into()),
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: Some("high".into()),
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::ThinkCard,
            key.clone(),
            "msg_think_card",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let card = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
                _ => None,
            })
            .next()
            .expect("a think card should be sent");
        let text = card.to_string();
        assert!(text.contains("思考等级"), "header: {text}");
        assert!(text.contains("opencode-go/deepseek-v4-flash"), "model: {text}");
        assert!(text.contains("默认（清除）"), "default option: {text}");
        assert!(text.contains("\"value\":\"low\""), "variant low: {text}");
        assert!(text.contains("\"value\":\"high\""), "variant high: {text}");
        assert!(text.contains("当前思考等级"), "current label: {text}");
    }

    /// A `/think` card button records the chosen variant and refreshes the
    /// card; the dedicated `think_clear` action clears it, and a card button
    /// whose value is literally `default` is rejected (not a clear — the value
    /// namespace is never overloaded, ADR-0020).
    #[tokio::test]
    async fn think_card_button_records_variant() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![crate::opencode::client::ProviderModels {
            provider: "opencode-go".into(),
            models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
        }];
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: Some("opencode-go/deepseek-v4-flash".into()),
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        let value = serde_json::json!({
            "action": "think",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "high",
        });
        let result = app.handle_card_action(value).await.expect("think card action");
        assert!(result.card.is_some(), "refreshed card returned");
        let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
        assert_eq!(entry.variant.as_deref(), Some("high"));

        // A `think` action whose value is literally `default` is NOT a clear
        // and NOT a pick (the declared set lacks it): the current `high`
        // override must survive untouched.
        let literal_default = serde_json::json!({
            "action": "think",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "default",
        });
        app.handle_card_action(literal_default)
            .await
            .expect("think literal-default action");
        assert_eq!(
            app.sessions
                .lock()
                .await
                .get_active(&key)
                .and_then(|e| e.variant.clone())
                .as_deref(),
            Some("high"),
            "a literal `default` value must neither clear nor replace the variant"
        );

        // The dedicated `think_clear` action clears it (mechanism, not value).
        let clear = serde_json::json!({
            "action": "think_clear",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "",
        });
        app.handle_card_action(clear).await.expect("think clear action");
        assert!(
            app.sessions
                .lock()
                .await
                .get_active(&key)
                .and_then(|e| e.variant.clone())
                .is_none(),
            "think_clear must clear the variant"
        );
    }

    /// Switching `/model` to a model that doesn't declare the current variant
    /// auto-clears it (ADR-0020) instead of leaving every prompt to fail with a
    /// server VariantUnavailableError.
    #[tokio::test]
    async fn model_switch_clears_undeclared_variant() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![
            crate::opencode::client::ProviderModels {
                provider: "opencode-go".into(),
                models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
            },
            crate::opencode::client::ProviderModels {
                provider: "openrouter".into(),
                models: vec![model_option("other-model", &["low"])],
            },
        ];
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: Some("opencode-go/deepseek-v4-flash".into()),
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: Some("high".into()),
            });
        }

        // Switching to the SAME model (declares high) keeps the variant.
        crate::bridge::command::handle_command(
            &app.core,
            Command::Model("opencode-go/deepseek-v4-flash".into()),
            key.clone(),
            "msg_model",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert_eq!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .and_then(|e| e.variant.clone())
                .as_deref(),
            Some("high"),
            "a still-declared variant must survive a model switch"
        );

        // A model that positively lacks the variant clears it.
        crate::bridge::command::handle_command(
            &app.core,
            Command::Model("openrouter/other-model".into()),
            key.clone(),
            "msg_model2",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .and_then(|e| e.variant.clone())
                .is_none(),
            "a model that lacks the variant must clear it"
        );
    }

    /// Switching `/model` to a model NOT in the advertised catalog leaves the
    /// variant in place (best-effort: an unknown model can't be judged, so it is
    /// not destroyed; the server's VariantUnavailableError is the fallback).
    #[tokio::test]
    async fn model_switch_keeps_variant_when_new_model_unknown() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.provider_models = vec![crate::opencode::client::ProviderModels {
            provider: "opencode-go".into(),
            models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
        }];
        let (app, _platform) = build_app(cfg, backend).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: key.clone(),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: Some("opencode-go/deepseek-v4-flash".into()),
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: Some("high".into()),
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Model("openrouter/other-model".into()),
            key.clone(),
            "msg_model",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();
        assert_eq!(
            app.sessions
                .lock()
                .await
                .entry_for_session("ses_test")
                .and_then(|e| e.variant.clone())
                .as_deref(),
            Some("high"),
            "an unknown model must not clear the variant"
        );
    }

    /// `/model` with a malformed value gets immediate feedback instead of a
    /// silent no-op.
    #[tokio::test]
    async fn model_command_rejects_malformed_value() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Model("not-a-model".into()),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_model",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("⚠️") && text.contains("<provider>/<model>"),
            "malformed /model must reply with format guidance: {text}"
        );
        // Nothing was recorded.
        let stored = {
            let store = app.sessions.lock().await;
            store.entry_for_session("ses_test").and_then(|e| e.model.clone())
        };
        assert!(stored.is_none(), "malformed /model must not record an override");
    }

    /// The `/model` override also flows through the supplement (in-flight)
    /// prompt_async path, not just the synchronous prompt.
    #[tokio::test]
    async fn model_override_flows_through_supplement_path() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let async_models = backend.prompt_async_models.clone();
        let async_calls = backend.prompt_async_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        // Record the override on the persisted entry, then mark the session
        // busy so the next message takes the supplement path.
        {
            let mut store = app.sessions.lock().await;
            let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
            let mut entry = store.get_active(&key).unwrap().clone();
            entry.model = Some("opencode-go/deepseek-v4-flash".into());
            store.set_active(entry);
        }
        app.inflight.lock().await.insert("ses_test".into());

        app.handle_message(incoming(
            "msg_supp".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "补充内容".into(),
            None,
        ))
        .await;

        assert_eq!(
            async_calls.lock().await.as_slice(),
            &["ses_test:补充内容".to_string()]
        );
        assert_eq!(
            async_models.lock().await.as_slice(),
            &[Some("opencode-go/deepseek-v4-flash".to_string())]
        );
    }

    /// `/agent <name>` records a per-session override (the OpenCode server has
    /// no agent-switch endpoint) and the NEXT prompt carries it.
    #[tokio::test]
    async fn agent_command_records_override_used_on_next_prompt() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_agents = backend.prompt_agents.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Agent("primary".into()),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_agent",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // The override is recorded on the session's persisted entry.
        let stored = {
            let store = app.sessions.lock().await;
            store.entry_for_session("ses_test").and_then(|e| e.agent.clone())
        };
        assert_eq!(stored.as_deref(), Some("primary"));
        // The override is NOT applied to an unrelated session.
        assert!(app.sessions.lock().await.entry_for_session("ses_other").is_none());

        // The next message prompts with the override as the agent.
        app.handle_message(incoming(
            "msg_prompt".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "hi".into(),
            None,
        ))
        .await;
        assert_eq!(
            prompt_agents.lock().await.as_slice(),
            &[Some("primary".to_string())]
        );
    }

    /// The `/agent` override also flows through the supplement (in-flight)
    /// prompt_async path, not just the synchronous prompt.
    #[tokio::test]
    async fn agent_override_flows_through_supplement_path() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let async_agents = backend.prompt_async_agents.clone();
        let async_calls = backend.prompt_async_calls.clone();
        let (app, _platform) = build_app(cfg, backend).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: Some("primary".into()),
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
            store.persist().unwrap();
        }
        app.inflight.lock().await.insert("ses_test".into());

        app.handle_message(incoming(
            "msg_supp".into(),
            "chat_1".into(),
            "p2p".into(),
            None,
            "补充内容".into(),
            None,
        ))
        .await;

        assert_eq!(
            async_calls.lock().await.as_slice(),
            &["ses_test:补充内容".to_string()]
        );
        assert_eq!(
            async_agents.lock().await.as_slice(),
            &[Some("primary".to_string())]
        );
    }

    /// `/model` and `/agent` overrides are persisted in sessions.json, so a cola
    /// restart (which reloads the store) keeps them.
    #[tokio::test]
    async fn model_override_persists_across_store_reload() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
                session_id: "ses_test".into(),
                directory: "/tmp/aa".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: None,
                topic_root: None,
                variant: None,
            });
        }

        crate::bridge::command::handle_command(
            &app.core,
            Command::Model("opencode-go/deepseek-v4-flash".into()),
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_model",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        // A freshly-loaded store (as after a restart) still has the override.
        let reloaded = crate::bridge::session::SessionStore::new(dir.path().join("sessions.json")).unwrap();
        let entry = reloaded
            .entry_for_session("ses_test")
            .expect("session mapping survives reload");
        assert_eq!(entry.model.as_deref(), Some("opencode-go/deepseek-v4-flash"));
    }

    /// `/model`, `/agent`, `/stop` and `/compact` used to silently no-op on a
    /// thread with no mapped session; they now reply a hint instead.
    #[tokio::test]
    async fn session_commands_reply_hint_without_mapped_session() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let kind = crate::config::ConversationKind::P2p;

        for (cmd, msg) in [
            (Command::Model("opencode-go/deepseek-v4-flash".into()), "msg_m"),
            (Command::Agent("build".into()), "msg_a"),
            (Command::Stop, "msg_s"),
            (Command::Compact, "msg_c"),
        ] {
            crate::bridge::command::handle_command(&app.core, cmd, key.clone(), msg, kind)
                .await
                .unwrap();
        }

        let text = platform
            .calls
            .lock()
            .await
            .clone()
            .into_iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("还没有会话"), "every command must hint: {text}");
    }

    /// `/topic` with a nonexistent directory replies a clear error and creates
    /// neither a topic nor a session (spec: validate before creating).
    #[tokio::test]
    async fn topic_command_rejects_nonexistent_directory() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

        crate::bridge::command::handle_command(
            &app.core,
            Command::Topic {
                directory: Some("/nonexistent/dir/xyz".into()),
                name: None,
            },
            crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            "msg_topic",
            crate::config::ConversationKind::P2p,
        )
        .await
        .unwrap();

        let calls = platform.calls.lock().await.clone();
        let text = calls
            .iter()
            .filter_map(|c| match c {
                PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("目录不存在") && text.contains("/nonexistent/dir/xyz"),
            "/topic with a bad dir must reply a clear error: {text}"
        );
        // No topic was created and nothing was mapped.
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, PlatformCall::ReplyInThread { .. })),
            "a topic must not be created for a bad directory: {calls:?}"
        );
        assert!(
            app.sessions.lock().await.all_entries().is_empty(),
            "no session mapping should be created: {:?}",
            app.sessions.lock().await.all_entries()
        );
    }

    #[tokio::test]
    async fn topic_with_session_rejects_selection_commands() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_foreign123abc", "外部会话", "/work/ext", 100)];
        let (app, platform) = build_app(cfg, backend).await;
        // The topic already owns a session.
        {
            let mut store = app.sessions.lock().await;
            store.set_active(crate::config::SessionEntry {
                thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
                session_id: "ses_topic_owned".into(),
                directory: "/work/topic".into(),
                agent: None,
                model: None,
                auto_accept: false,
                topic_anchor: Some("msg_anchor".into()),
                topic_root: None,
                variant: None,
            });
        }
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into());

        for cmd in [
            Command::Switch(SwitchAction::List {
                keyword: None,
                all: false,
            }),
            Command::Switch(SwitchAction::Match("外部".into())),
            Command::Switch(SwitchAction::Attach {
                query: "ses_foreign123abc".into(),
                force: false,
            }),
            Command::New(None),
            Command::Dir("/work/x".into()),
            Command::DirCard,
        ] {
            platform.calls.lock().await.clear();
            crate::bridge::command::handle_command(
                &app.core,
                cmd.clone(),
                topic_key.clone(),
                "msg_topic",
                crate::config::ConversationKind::Topic,
            )
            .await
            .unwrap();
            let calls = platform.calls.lock().await.clone();
            let text = calls
                .iter()
                .filter_map(|c| match c {
                    PlatformCall::ReplyText { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains("回主对话操作"),
                "{cmd:?} must be rejected in a bound topic: {text}"
            );
        }
        // The topic's session mapping is untouched.
        assert_eq!(
            app.sessions
                .lock()
                .await
                .get_active(&topic_key)
                .unwrap()
                .session_id,
            "ses_topic_owned"
        );
    }

    #[tokio::test]
    async fn fresh_topic_attach_adopts_with_in_topic_anchor() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let mut backend = MockBackend::new(realistic_parts());
        backend.session_list = vec![list_session("ses_foreign123abc", "外部会话", "/work/ext", 100)];
        let (app, _platform) = build_app(cfg, backend).await;
        let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_fresh".into());

        crate::bridge::command::handle_command(
            &app.core,
            Command::Switch(SwitchAction::Attach {
                query: "ses_foreign123abc".into(),
                force: false,
            }),
            topic_key.clone(),
            "msg_topic_cmd",
            crate::config::ConversationKind::Topic,
        )
        .await
        .unwrap();

        // Adopted as the topic's single session, anchored to a reply inside it.
        let entry = app.sessions.lock().await.get_active(&topic_key).cloned().unwrap();
        assert_eq!(entry.session_id, "ses_foreign123abc");
        assert_eq!(entry.topic_anchor.as_deref(), Some("msg_topic_reply"));
    }

    /// A reply injects its parent's text as Quoted Context, prefixed ahead of
    /// the user's own message so the model sees what the reply answers.
    #[tokio::test]
    async fn reply_injects_quoted_parent_text() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_calls = backend.prompt_calls.clone();
        let prompt_images = backend.prompt_images.clone();
        let platform = RecordingPlatform::new();
        platform.quoted_messages.lock().unwrap().insert(
            "om_parent".into(),
            crate::feishu::client::FeishuMessage {
                msg_type: "text".into(),
                content: r#"{"text":"父消息：看看这个报错 @_user_1"}"#.into(),
                mentions: vec![crate::feishu::event::Mention {
                    key: Some("@_user_1".into()),
                    id: Some(crate::feishu::event::MentionId {
                        open_id: Some("ou_other".into()),
                    }),
                    name: Some("李明".into()),
                }],
            },
        );
        let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());

        app.handle_message(crate::bridge::IncomingMessage {
            message_id: "msg_reply".into(),
            chat_id: "chat_1".into(),
            chat_type: "p2p".into(),
            thread_id: None,
            parent_id: Some("om_parent".into()),
            text: "还是不行".into(),
            images: vec![],
            requester_open_id: None,
        })
        .await;

        let calls = prompt_calls.lock().await.clone();
        assert_eq!(
            calls,
            vec!["[引用消息]:\n父消息：看看这个报错 @李明\n\n还是不行".to_string()]
        );
        // The text parent carries no images.
        assert_eq!(*prompt_images.lock().await, vec![0]);
    }

    /// ADR-0023: a plain message in a cola-created topic carries `parent_id`
    /// pointing at the topic's own creation messages — the thread root (the
    /// user's `/topic` command) or the seed card (`topic_anchor`). Neither must
    /// be injected as Quoted Context: both are boilerplate, and injecting them
    /// pollutes every prompt in the topic.
    #[tokio::test]
    async fn topic_plain_reply_skips_own_root_and_seed_injection() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_calls = backend.prompt_calls.clone();
        let platform = RecordingPlatform::new();
        for (id, text) in [
            ("om_root_cmd", "📌 已创建会话 `api-refactor`"),
            ("om_seed", "📌 已创建会话 `api-refactor`\n请在本话题内回复"),
        ] {
            platform.quoted_messages.lock().unwrap().insert(
                id.into(),
                crate::feishu::client::FeishuMessage {
                    msg_type: "text".into(),
                    content: format!(r#"{{"text":"{text}"}}"#),
                    mentions: vec![],
                },
            );
        }
        let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());
        // The cola-created topic already owns a session; its creation messages
        // are the thread root (the `/topic` command) and the seed card.
        app.sessions.lock().await.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            session_id: "ses_topic".into(),
            directory: "/work/topic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_root_cmd".into()),
            variant: None,
        });

        for pid in ["om_root_cmd", "om_seed"] {
            prompt_calls.lock().await.clear();
            app.handle_message(crate::bridge::IncomingMessage {
                message_id: format!("msg_{pid}"),
                chat_id: "chat_1".into(),
                chat_type: "group".into(),
                thread_id: Some("omt_t_1".into()),
                parent_id: Some(pid.into()),
                text: "普通回复".into(),
                images: vec![],
                requester_open_id: None,
            })
            .await;
            assert_eq!(
                *prompt_calls.lock().await,
                vec!["普通回复".to_string()],
                "parent {pid} is the topic's own creation message — must not be injected"
            );
        }
    }

    /// ADR-0023: a genuine quote of a real message inside a topic still injects
    /// — only the topic's own creation messages are excluded.
    #[tokio::test]
    async fn topic_explicit_quote_of_real_message_still_injects() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_calls = backend.prompt_calls.clone();
        let platform = RecordingPlatform::new();
        platform.quoted_messages.lock().unwrap().insert(
            "om_real_quote".into(),
            crate::feishu::client::FeishuMessage {
                msg_type: "text".into(),
                content: r#"{"text":"真正的上下文"}"#.into(),
                mentions: vec![],
            },
        );
        let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());
        app.sessions.lock().await.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            session_id: "ses_topic".into(),
            directory: "/work/topic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_root_cmd".into()),
            variant: None,
        });

        app.handle_message(crate::bridge::IncomingMessage {
            message_id: "msg_q".into(),
            chat_id: "chat_1".into(),
            chat_type: "group".into(),
            thread_id: Some("omt_t_1".into()),
            parent_id: Some("om_real_quote".into()),
            text: "继续".into(),
            images: vec![],
            requester_open_id: None,
        })
        .await;

        assert_eq!(
            *prompt_calls.lock().await,
            vec!["[引用消息]:\n真正的上下文\n\n继续".to_string()]
        );
    }

    /// ADR-0023: a manually-created topic has neither `topic_root` nor
    /// `topic_anchor`, so the guard is silent and the user's own subject
    /// message (the topic's root) still injects — it is context, not
    /// boilerplate.
    #[tokio::test]
    async fn manual_topic_plain_reply_injects_user_root() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_calls = backend.prompt_calls.clone();
        let platform = RecordingPlatform::new();
        platform.quoted_messages.lock().unwrap().insert(
            "om_user_root".into(),
            crate::feishu::client::FeishuMessage {
                msg_type: "text".into(),
                content: r#"{"text":"用户的主题消息"}"#.into(),
                mentions: vec![],
            },
        );
        let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());
        app.sessions.lock().await.set_active(crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            session_id: "ses_topic".into(),
            directory: "/work/topic".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        });

        app.handle_message(crate::bridge::IncomingMessage {
            message_id: "msg_manual".into(),
            chat_id: "chat_1".into(),
            chat_type: "group".into(),
            thread_id: Some("omt_t_1".into()),
            parent_id: Some("om_user_root".into()),
            text: "继续".into(),
            images: vec![],
            requester_open_id: None,
        })
        .await;

        assert_eq!(
            *prompt_calls.lock().await,
            vec!["[引用消息]:\n用户的主题消息\n\n继续".to_string()]
        );
    }

    /// A reply to an IMAGE message downloads the quoted image and attaches it as
    /// an Image Attachment (prompt_images > 0).
    #[tokio::test]
    async fn reply_to_image_downloads_quoted_image() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_images = backend.prompt_images.clone();
        let platform = RecordingPlatform::new();
        platform.quoted_messages.lock().unwrap().insert(
            "om_img_parent".into(),
            crate::feishu::client::FeishuMessage {
                msg_type: "image".into(),
                content: r#"{"image_key":"img_q"}"#.into(),
                mentions: vec![],
            },
        );
        let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(platform)).unwrap());

        app.handle_message(crate::bridge::IncomingMessage {
            message_id: "msg_r".into(),
            chat_id: "chat_1".into(),
            chat_type: "p2p".into(),
            thread_id: None,
            parent_id: Some("om_img_parent".into()),
            text: "把这里放大看看".into(),
            images: vec![],
            requester_open_id: None,
        })
        .await;

        assert_eq!(*prompt_images.lock().await, vec![1]);
    }

    /// An incoming image message attaches its downloaded image; the text is the
    /// `[图片]` placeholder (never raw JSON).
    #[tokio::test]
    async fn image_message_attaches_image_and_placeholder() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_calls = backend.prompt_calls.clone();
        let prompt_images = backend.prompt_images.clone();
        let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(RecordingPlatform::new())).unwrap());

        app.handle_message(crate::bridge::IncomingMessage {
            message_id: "msg_img".into(),
            chat_id: "chat_1".into(),
            chat_type: "p2p".into(),
            thread_id: None,
            parent_id: None,
            text: "[图片]".into(),
            images: vec![crate::feishu::client::ImageAttachment {
                mime: "image/png".into(),
                data: vec![1, 2, 3],
            }],
            requester_open_id: None,
        })
        .await;

        assert_eq!(*prompt_images.lock().await, vec![1]);
        assert_eq!(*prompt_calls.lock().await, vec!["[图片]".to_string()]);
    }

    /// Quote-injection failures degrade to text-only (the pre-change behavior).
    #[tokio::test]
    async fn reply_degrades_when_quote_fetch_fails() {
        let _wd = test_work_dir();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let backend = MockBackend::new(realistic_parts());
        let prompt_calls = backend.prompt_calls.clone();
        let app = Arc::new(App::new(cfg, Arc::new(backend), Arc::new(RecordingPlatform::new())).unwrap());

        // `om_missing` is not in `quoted_messages` → get_message fails → the
        // reply reaches the model without any quote prefix.
        app.handle_message(crate::bridge::IncomingMessage {
            message_id: "msg_d".into(),
            chat_id: "chat_1".into(),
            chat_type: "p2p".into(),
            thread_id: None,
            parent_id: Some("om_missing".into()),
            text: "hi".into(),
            images: vec![],
            requester_open_id: None,
        })
        .await;

        let calls = prompt_calls.lock().await.clone();
        assert_eq!(calls, vec!["hi".to_string()]);
    }
}
