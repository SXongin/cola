#![cfg(test)]

pub(crate) use std::sync::Arc;

pub(crate) use crate::bridge::handler::App;
pub(crate) use crate::bridge::turn::Turn;
pub(crate) use crate::feishu;
pub(crate) use crate::opencode;

use crate::backend::{
    ContentBlock, FinishReason, MessageId, MessageRole, MessageTime, Part, ReasoningPart, SessionTranscript,
    StepFinish, StepStart, TextPart, ToolCall, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage,
    TurnAnchor,
};

/// One typed transcript message for view-shaped fixtures (spec #332): identity,
/// role, server time (completed at creation) and the given parts. `created:
/// None` models a message the payload carried no time for. Shared by the
/// snapshot, external and mock-backend test surfaces so their typed fixtures
/// cannot drift.
pub(crate) fn typed_message(
    id: &str,
    role: MessageRole,
    created: Option<i64>,
    parts: Vec<Part>,
) -> TranscriptMessage {
    TranscriptMessage {
        id: MessageId::new(id),
        role,
        time: created.map(|created| MessageTime {
            created,
            completed: Some(created),
        }),
        model: None,
        tokens: None,
        parts,
    }
}

/// One typed text part — the common content of the view-shaped fixtures.
pub(crate) fn text_part(text: &str) -> Part {
    Part::Text(TextPart {
        text: text.to_string(),
        started_at: None,
    })
}

/// One typed tool part — name, correlation id, status, raw input and a single
/// text output block: the shape most fixtures need. Fixtures that exercise
/// metadata, richer output blocks or server times build the call directly.
pub(crate) fn tool_part(
    name: &str,
    call_id: &str,
    status: ToolStatus,
    input: serde_json::Value,
    output: &str,
) -> Part {
    Part::Tool(ToolCall {
        identity: ToolIdentity {
            name: name.to_string(),
            call_id: call_id.to_string(),
        },
        status,
        started_at: None,
        input: Some(input),
        metadata: None,
        output: ToolOutput {
            raw: Some(serde_json::Value::String(output.to_string())),
            blocks: vec![ContentBlock::Text(output.to_string())],
            error: None,
        },
    })
}

/// A fixture Turn anchor: a message identity together with its server time,
/// one fact (spec #332).
pub(crate) fn turn_anchor(created_ms: i64) -> TurnAnchor {
    TurnAnchor {
        message_id: MessageId::new(format!("msg_anchor_{created_ms}")),
        created_ms,
    }
}

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
    /// An Instant Reminder (`time_sensitive`, ADR-0043) call: the pinned
    /// Chat/Topic, its kind, the targeted users and whether it was pinned
    /// (`true`) or cleared (`false`).
    InstantReminder {
        chat_id: String,
        is_group: bool,
        user_ids: Vec<String>,
        on: bool,
    },
    /// A waiting-message pin call (ADR-0043 amendment): the message put into
    /// (or removed from) its Chat/Topic's pinned-message list.
    PinMessage {
        message_id: String,
        on: bool,
    },
}

/// A one-shot gate on one platform call, so a test can freeze a card write
/// mid-send and interleave another writer (the concurrency races ADR-0038's
/// "one writer per card" invariant depends on).
pub struct CallGate {
    /// `"update"` (= `update_message`) or `"reply"` (= `reply_card`).
    pub method: &'static str,
    /// The message the gated call targets (`message_id` / `reply_to`).
    pub target: String,
    /// Signalled when the gated call has been entered (the caller is parked).
    pub entered: Arc<tokio::sync::Notify>,
    /// The parked call proceeds once this is signalled.
    pub release: Arc<tokio::sync::Notify>,
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
    /// The next N `reply_card` calls fail (a CONTINUATION send that must be
    /// retried without duplicating receipts, while the loading card's own
    /// reply — if any — succeeds).
    pub fail_reply_card_count: std::sync::atomic::AtomicUsize,
    /// The next N `update_message` calls fail with a card-content rejection
    /// (`230099`, what Feishu returns for a card whose markdown it refuses):
    /// the flush must degrade the card and retry instead of resending it.
    pub fail_update_card_content_count: std::sync::atomic::AtomicUsize,
    /// The next N `reply_card` calls fail with the same typed rejection, for
    /// the continuation-send recovery path.
    pub fail_reply_card_content_count: std::sync::atomic::AtomicUsize,
    /// When set, `set_instant_reminder` fails after recording the attempt
    /// (tests the best-effort pin path: failures log and never affect a turn).
    /// Atomic so a test can flip it mid-lifecycle and watch a recovery.
    pub fail_instant_reminder: std::sync::atomic::AtomicBool,
    /// When set, `pin_message`/`unpin_message` fail after recording the
    /// attempt (tests the best-effort waiting-card pin path: a failed pin is
    /// retried, a failed unpin stays tracked). Atomic so a test can flip it
    /// mid-lifecycle.
    pub fail_pin: std::sync::atomic::AtomicBool,
    /// The thread_id `reply_in_thread` returns; `None` simulates a chat
    /// without topic support (the create-topic surfaces degrade with a
    /// message instead of mapping).
    pub reply_in_thread_thread_id: Option<String>,
    /// message_id → quoted-parent content served by `get_message` (absent =
    /// the default text parent). Lets tests script quote-injection cases.
    pub quoted_messages:
        std::sync::Mutex<std::collections::HashMap<String, crate::feishu::client::FeishuMessage>>,
    /// One-shot mid-send pause installed by a concurrency test (absent in
    /// every other test). Taken by the first matching call.
    pub pause_call: std::sync::Mutex<Option<CallGate>>,
}

impl RecordingPlatform {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            user_names: std::collections::HashMap::new(),
            chat_names: std::collections::HashMap::new(),
            fail_send_card: false,
            fail_reply_card: false,
            fail_reply_card_count: std::sync::atomic::AtomicUsize::new(0),
            fail_update_card_content_count: std::sync::atomic::AtomicUsize::new(0),
            fail_reply_card_content_count: std::sync::atomic::AtomicUsize::new(0),
            fail_instant_reminder: std::sync::atomic::AtomicBool::new(false),
            fail_pin: std::sync::atomic::AtomicBool::new(false),
            reply_in_thread_thread_id: Some("omt_created_topic".into()),
            quoted_messages: std::sync::Mutex::new(std::collections::HashMap::new()),
            pause_call: std::sync::Mutex::new(None),
        }
    }

    /// Park the first `method` call targeting `target` until `release`, after
    /// signalling `entered`. Returns `(entered, release)` for the test to
    /// await and trigger.
    pub fn pause(
        &self,
        method: &'static str,
        target: &str,
    ) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *self.pause_call.lock().unwrap() = Some(CallGate {
            method,
            target: target.to_string(),
            entered: entered.clone(),
            release: release.clone(),
        });
        (entered, release)
    }

    /// Take the installed gate when this call matches it.
    fn take_gate(&self, method: &str, target: &str) -> Option<CallGate> {
        let mut slot = self.pause_call.lock().unwrap();
        match slot.as_ref() {
            Some(gate) if gate.method == method && gate.target == target => slot.take(),
            _ => None,
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

    /// Every Instant Reminder call in order: `(chat_id, is_group, user_ids,
    /// on)` — `on = true` is the pin, `false` the clear (ADR-0043).
    pub(crate) async fn reminders(&self) -> Vec<(String, bool, Vec<String>, bool)> {
        self.calls
            .lock()
            .await
            .iter()
            .filter_map(|c| match c {
                PlatformCall::InstantReminder {
                    chat_id,
                    is_group,
                    user_ids,
                    on,
                } => Some((chat_id.clone(), *is_group, user_ids.clone(), *on)),
                _ => None,
            })
            .collect()
    }

    /// Every Completion Notice call in order: `(reply_to, open_id, name, text)`
    /// — the reply that notifies the requester a Turn ended (ADR-0043
    /// amendment 2026-09-21).
    pub(crate) async fn completion_notices(&self) -> Vec<(String, String, Option<String>, String)> {
        self.calls
            .lock()
            .await
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
            .collect()
    }

    /// Every waiting-message pin call in order: `(message_id, on)` — `on`
    /// `true` puts the message into its Chat/Topic's pinned-message list,
    /// `false` removes it (ADR-0043 amendment).
    pub(crate) async fn message_pins(&self) -> Vec<(String, bool)> {
        self.calls
            .lock()
            .await
            .iter()
            .filter_map(|c| match c {
                PlatformCall::PinMessage { message_id, on } => Some((message_id.clone(), *on)),
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

/// Park on `gate` until the test releases it.
async fn wait_gate(gate: CallGate) {
    gate.entered.notify_one();
    gate.release.notified().await;
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
        if self
            .fail_reply_card_content_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_reply_card_content_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.calls.lock().await.push(PlatformCall::ReplyCard {
                reply_to: reply_to.into(),
                card: card.clone(),
            });
            return Err(crate::error::BridgeError::CardContentRejected {
                code: 230099,
                detail: "simulated card content rejection".into(),
            });
        }
        if self.fail_reply_card {
            return Err(crate::error::BridgeError::Feishu(
                "simulated reply_card failure".into(),
            ));
        }
        if self
            .fail_reply_card_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_reply_card_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::Feishu(
                "simulated reply_card failure".into(),
            ));
        }
        if let Some(gate) = self.take_gate("reply", reply_to) {
            wait_gate(gate).await;
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
        let rejected = self
            .fail_update_card_content_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0;
        if rejected {
            self.fail_update_card_content_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(gate) = self.take_gate("update", message_id) {
            wait_gate(gate).await;
        }
        self.calls.lock().await.push(PlatformCall::UpdateMessage {
            message_id: message_id.into(),
            card: card.clone(),
        });
        if rejected {
            return Err(crate::error::BridgeError::CardContentRejected {
                code: 230099,
                detail: "simulated card content rejection".into(),
            });
        }
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

    async fn set_instant_reminder(
        &self,
        chat_id: &str,
        is_group: bool,
        user_ids: &[String],
        on: bool,
    ) -> crate::error::Result<()> {
        // Recorded even when failing, so a test can assert the attempt was
        // made and the turn still proceeded.
        self.calls.lock().await.push(PlatformCall::InstantReminder {
            chat_id: chat_id.into(),
            is_group,
            user_ids: user_ids.to_vec(),
            on,
        });
        if self
            .fail_instant_reminder
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(crate::error::BridgeError::Feishu(
                "simulated set_instant_reminder failure".into(),
            ));
        }
        Ok(())
    }

    async fn pin_message(&self, message_id: &str) -> crate::error::Result<()> {
        self.calls.lock().await.push(PlatformCall::PinMessage {
            message_id: message_id.into(),
            on: true,
        });
        if self.fail_pin.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::error::BridgeError::Feishu(
                "simulated pin_message failure".into(),
            ));
        }
        Ok(())
    }

    async fn unpin_message(&self, message_id: &str) -> crate::error::Result<()> {
        self.calls.lock().await.push(PlatformCall::PinMessage {
            message_id: message_id.into(),
            on: false,
        });
        if self.fail_pin.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::error::BridgeError::Feishu(
                "simulated unpin_message failure".into(),
            ));
        }
        Ok(())
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
    /// The parts the default assistant turn carries: reasoning → tool → text.
    pub parts: Vec<Part>,
    pub permissions: Vec<opencode::types::PermissionRequest>,
    /// Number of initial `list_permissions` calls to hang forever — simulates
    /// an in-flight request stuck on a half-open connection while the server
    /// restarts. Each hung call decrements the counter; once it reaches zero
    /// the call serves normally, so a bounded poller recovers on a later poll.
    pub hang_list_permissions: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `list_questions`.
    pub hang_list_questions: Arc<std::sync::atomic::AtomicUsize>,
    /// Same as `hang_list_permissions`, for `transcript` (the external
    /// poller's per-session read).
    pub hang_transcript: Arc<std::sync::atomic::AtomicUsize>,
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
    /// When set, `transcript` returns this as the assistant reply to the
    /// external user message (simulates OpenCode answering it), replacing the
    /// default assistant turn. Returned only once `external_reply_ready`
    /// flips, so tests can script the reply arriving on a LATER poll.
    pub external_reply_parts: Option<Vec<Part>>,
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
    /// Records every `interrupt` call's session id (the `/stop` path).
    pub interrupt_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Records every `compact` call's session id (the `/compact` path).
    pub compact_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
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
    pub extra_permissions: Arc<tokio::sync::Mutex<Vec<opencode::types::PermissionRequest>>>,
    /// session_id → server title (simulates OpenChamber's session title).
    /// `std::sync::Mutex` for interior mutability: `update_session_title`
    /// writes it through `&self` (the trait requires `&self`).
    pub session_titles: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// Session id → scripted Session Transcripts served by `transcript`,
    /// consumed one snapshot per call (the last repeating). Lets a test script
    /// the Backend's history call by call — e.g. the post-prompt drain's exit
    /// check seeing no supplement and the racing finish re-check seeing one —
    /// and mutate it mid-turn (a supplement that missed the run, then its
    /// assistant reply). Sessions absent from the map keep the default shape.
    pub transcript_scripts:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, Vec<SessionTranscript>>>>,
    /// Records every `transcript` call's session id — the neutral read the
    /// render poll, the drain and the follow poll on.
    pub transcript_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Pending questions served by `list_questions`.
    pub questions: Vec<opencode::types::QuestionRequest>,
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
    /// When set, `prompt` waits for a permit here before returning, so a test
    /// can hold a turn in flight (inline a pending request, then abort) and
    /// only afterwards let it finish — the mid-turn ordering the turn-end
    /// leftover rejection (#187) acts on.
    pub prompt_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// Records every `prompt_async` call's text (asserts supplement path).
    pub prompt_async_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// When set, `prompt_async` fails with this message (a supplement whose
    /// send to the Backend failed — the failure notice must still be followed
    /// by the Card Chain split).
    pub prompt_async_error: Option<String>,
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
    /// Every `create_session` call's requested directory (ADR-0041: asserts
    /// lazy creation — nothing on `/new`, one creation on the first message,
    /// in the pending's directory).
    pub created_session_dirs: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    /// Number of initial `create_session` calls to fail — materialisation must
    /// keep the pending and retry on the next message.
    pub fail_create_session_count: Arc<std::sync::atomic::AtomicUsize>,
    /// When true, `prompt` 404s for any session id other than `session_id`
    /// (simulates a stale mapping to a session that no longer exists).
    pub stale_session_404: bool,
    /// child session_id → parent session_id, served by `session_info`
    /// (simulates sub-task sessions created by the `task` tool).
    pub session_parents: std::collections::HashMap<String, String>,
    /// The shared store served by `list_sessions` (for `/attach` and
    /// `/switch` tests).
    pub session_list: Vec<opencode::types::SessionListInfo>,
    /// Records `update_session_title` calls: (session_id, title).
    pub update_title_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    /// When true, `update_session_title` fails — materialisation must keep the
    /// created session (never orphan it) and warn.
    pub fail_title_patch: bool,
    /// Counts `list_sessions` invocations (asserts the 30 s cache).
    pub list_sessions_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Available agents served by `list_agents` (for the `/agent` card).
    pub agents: Vec<opencode::types::AgentInfo>,
    /// Available models grouped by provider, served by `list_models` (for the
    /// `/model` card).
    pub provider_models: Vec<opencode::types::ProviderModels>,
    /// The configured default model served by `configured_default_model` (the
    /// second rung of the `/think` effective-model resolution).
    pub default_model: Option<opencode::types::ModelInfo>,
    /// The context-window size `model_context_window` reports; `None`
    /// simulates a server that reports none (ADR-0044's used-only fallback).
    pub context_window: Option<i64>,
    /// Counts `model_context_window` calls, so a test can prove the per-turn
    /// memo fetches at most once per (provider, model).
    pub context_window_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// The server-recorded session model served by `session_info` (the third
    /// rung of the `/think` effective-model resolution).
    pub session_model: Option<opencode::types::SessionModel>,
    /// Per-session server status served by `session_status` (session_id →
    /// status). A missing key means idle (matches the server: a finished run is
    /// removed from the status map). `Some(None)` inside simulates a session
    /// present but with an unrecognised status type (unknown). Behind a lock so
    /// a test can flip a session Busy → Idle while a loop (e.g. the #284 drain
    /// follow) is watching it.
    pub session_statuses:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, Option<opencode::types::SessionStatus>>>>,
    /// When set, `session_status` fails with this message (simulates a read
    /// failure — the caller must not guess a status).
    pub session_status_error: Option<String>,
    /// Scripts the ADR-0028 busy→idle race: the first `session_status` read
    /// returns Busy (and clears the flag), later reads serve the map.
    pub status_busy_once: std::sync::atomic::AtomicBool,
    /// Scenario state for [`MockBackend::given_prompt`]: `(needle, parts)`
    /// pairs matched against the prompt text, first match wins.
    prompt_scripts: Vec<(String, Vec<Part>)>,
    /// The parts the last matched prompt script streamed. `transcript` serves
    /// them instead of `parts` once set, so the scripted prompt's own turn
    /// renders consistently through both read paths.
    last_prompt_parts: std::sync::Mutex<Option<Vec<Part>>>,
    /// The message counted `prompt` failures report (see
    /// [`MockBackend::fail_prompts`]); `None` keeps the generic one.
    fail_prompt_message: Option<String>,
}

impl MockBackend {
    pub fn new(parts: Vec<Part>) -> Self {
        Self {
            parts,
            permissions: Vec::new(),
            hang_list_permissions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_list_questions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hang_transcript: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
            interrupt_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            compact_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            replied_permissions: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
            reply_permission_not_found: false,
            fail_reply_permission_count: std::sync::atomic::AtomicUsize::new(0).into(),
            reply_question_not_found: false,
            extra_permissions: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            session_titles: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            transcript_scripts: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            transcript_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
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
            prompt_gate: None,
            prompt_async_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_error: None,
            prompt_async_images: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_models: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_variants: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_agents: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            prompt_async_message_ids: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            session_id: "ses_test".into(),
            created_session_dirs: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_create_session_count: std::sync::atomic::AtomicUsize::new(0).into(),
            stale_session_404: false,
            session_parents: std::collections::HashMap::new(),
            session_list: Vec::new(),
            update_title_calls: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail_title_patch: false,
            list_sessions_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            agents: Vec::new(),
            provider_models: Vec::new(),
            default_model: None,
            context_window: Some(100_000),
            context_window_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            session_model: None,
            session_statuses: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            session_status_error: None,
            status_busy_once: std::sync::atomic::AtomicBool::new(false),
            prompt_scripts: Vec::new(),
            last_prompt_parts: std::sync::Mutex::new(None),
            fail_prompt_message: None,
        }
    }

    // ===== Scenario vocabulary =====
    //
    // The methods below script the common flows without the test reaching into
    // the fields above: "given a prompt, stream these parts / ask these
    // permissions / fail this call". Fields stay public for the exotic axes;
    // these are the front door for the flows tests actually build.

    /// Scenario: the shared store holds exactly `sessions` (drives `/switch`,
    /// `/dir` and the external poller's discovery).
    pub(crate) fn given_sessions(&mut self, sessions: Vec<opencode::types::SessionListInfo>) -> &mut Self {
        self.session_list = sessions;
        self
    }

    /// Scenario: `request` is pending on the server (appends to the pending
    /// list `list_permissions` serves).
    pub(crate) fn ask_permission(&mut self, request: opencode::types::PermissionRequest) -> &mut Self {
        self.permissions.push(request);
        self
    }

    /// Scenario: exactly these permissions are pending.
    pub(crate) fn ask_permissions(&mut self, requests: Vec<opencode::types::PermissionRequest>) -> &mut Self {
        self.permissions = requests;
        self
    }

    /// Scenario: `request` is pending on the server (appends to the pending
    /// list `list_questions` serves).
    pub(crate) fn ask_question(&mut self, request: opencode::types::QuestionRequest) -> &mut Self {
        self.questions.push(request);
        self
    }

    /// Scenario: exactly these questions are pending.
    pub(crate) fn ask_questions(&mut self, requests: Vec<opencode::types::QuestionRequest>) -> &mut Self {
        self.questions = requests;
        self
    }

    /// Scenario: every `prompt` fails with `message`.
    pub(crate) fn fail_prompt(&mut self, message: &str) -> &mut Self {
        self.prompt_error = Some(message.to_string());
        self
    }

    /// Scenario: the next `count` `prompt` calls fail with `message`; the
    /// scripted failure is consumed, so later prompts succeed.
    pub(crate) fn fail_prompts(&mut self, count: usize, message: &str) -> &mut Self {
        self.fail_prompt_count
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self.fail_prompt_message = Some(message.to_string());
        self
    }

    /// Scenario: `prompt` streams `parts` when the prompt text contains
    /// `needle` (first match wins; `""` matches any prompt).
    pub(crate) fn given_prompt(&mut self, needle: &str, parts: Vec<Part>) -> &mut Self {
        self.prompt_scripts.push((needle.to_string(), parts));
        self
    }

    /// Scenario: `list_models` serves `models`.
    pub(crate) fn with_models(&mut self, models: Vec<opencode::types::ProviderModels>) -> &mut Self {
        self.provider_models = models;
        self
    }

    /// Scenario: `list_agents` serves `agents`.
    pub(crate) fn with_agents(&mut self, agents: Vec<opencode::types::AgentInfo>) -> &mut Self {
        self.agents = agents;
        self
    }

    /// Scenario: `configured_default_model` serves `model`.
    pub(crate) fn with_default_model(&mut self, model: opencode::types::ModelInfo) -> &mut Self {
        self.default_model = Some(model);
        self
    }

    /// Scenario: the server records `model` for the session (`session_info`).
    pub(crate) fn with_session_model(&mut self, model: opencode::types::SessionModel) -> &mut Self {
        self.session_model = Some(model);
        self
    }

    /// Scenario: `create_session` returns `id`.
    pub(crate) fn with_session_id(&mut self, id: &str) -> &mut Self {
        self.session_id = id.to_string();
        self
    }

    /// Scenario: `transcript` serves `snapshots` for `session_id`, one per
    /// call (the last repeating) — the neutral view of that session's history.
    /// Sessions without a script keep the default shape.
    pub(crate) fn given_transcript(
        &mut self,
        session_id: &str,
        snapshots: Vec<SessionTranscript>,
    ) -> &mut Self {
        self.transcript_scripts
            .try_lock()
            .expect("given_transcript before the app is built")
            .insert(session_id.to_string(), snapshots);
        self
    }

    /// Scenario: hold every `prompt` until the returned semaphore is released
    /// — the test can keep a turn in flight and interleave state through the
    /// normal seams.
    pub(crate) fn hold_prompts(&mut self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        self.prompt_gate = Some(Arc::clone(&gate));
        gate
    }

    /// Scenario: the addressed permission was resolved elsewhere — a reply
    /// reports it gone (404) instead of succeeding.
    pub(crate) fn permission_resolved_elsewhere(&mut self) -> &mut Self {
        self.reply_permission_not_found = true;
        self
    }

    /// Scenario: the addressed question was resolved elsewhere — a reply
    /// reports it gone (404) instead of succeeding.
    pub(crate) fn question_resolved_elsewhere(&mut self) -> &mut Self {
        self.reply_question_not_found = true;
        self
    }

    /// Scenario: another client already resolved `request_id` — the server
    /// filters it out of the pending list, like a replied request. Usable
    /// mid-lifecycle (through the `Arc` the app holds).
    pub(crate) async fn permission_resolved_by_another(&self, request_id: &str) {
        self.replied_permissions
            .lock()
            .await
            .insert(request_id.to_string());
    }

    /// Scenario: another client already resolved `question_id` — the server
    /// filters it out of the pending list.
    pub(crate) async fn question_resolved_by_another(&self, question_id: &str) {
        self.replied_questions
            .lock()
            .await
            .insert(question_id.to_string());
    }

    /// Scenario: the external message's server timestamp (captured on first
    /// read otherwise) — lets a test place it in the past.
    pub(crate) fn external_message_created_at(&self, ms: i64) -> &Self {
        *self.external_user_created.lock().unwrap() = Some(ms);
        self
    }

    /// Scenario: another client (e.g. OpenChamber) posts `text` into the
    /// session; the external poller sees it as a new user message.
    pub(crate) fn external_message(&mut self, text: &str) -> &mut Self {
        self.external_user_message = Some(text.to_string());
        self
    }

    /// Scenario: another client posts `text` into a SPECIFIC session (takes
    /// precedence over [`external_message`] for that session).
    pub(crate) fn external_message_for(&mut self, session_id: &str, text: &str) -> &mut Self {
        self.external_user_messages
            .insert(session_id.to_string(), text.to_string());
        self
    }

    /// Scenario: cola's OWN prompt persisted on the store with a `msg_cola_`
    /// id (ADR-0026) — e.g. after a server died mid-turn and healed. The
    /// poller must recognise it as cola-authored, never as external traffic.
    pub(crate) fn cola_message(&mut self, session_id: &str, text: &str) -> &mut Self {
        self.cola_user_messages
            .insert(session_id.to_string(), text.to_string());
        self
    }

    /// Scenario: the next `count` `reply_permission` calls fail with a genuine
    /// (non-404) error — the in-flight guard must roll back so the user can
    /// retry.
    pub(crate) fn fail_permission_replies(&self, count: usize) -> &Self {
        self.fail_reply_permission_count
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: run `hook` inside `prompt` just before it succeeds — lets a
    /// test mutate the session's working tree mid-turn (e.g. switch branches
    /// or leave an uncommitted file) to exercise the Turn Footer's turn-end
    /// refresh (ADR-0019).
    pub(crate) fn on_prompt(&mut self, hook: impl Fn() + Send + Sync + 'static) -> &mut Self {
        self.on_prompt = Some(Box::new(hook));
        self
    }

    /// Scenario: the server's live status for `session_id` (`None` = an
    /// unrecognised status type; an absent entry means idle).
    pub(crate) fn with_session_status(
        &mut self,
        session_id: &str,
        status: Option<opencode::types::SessionStatus>,
    ) -> &mut Self {
        self.session_statuses
            .try_lock()
            .expect("with_session_status before the app is built")
            .insert(session_id.to_string(), status);
        self
    }

    /// Scenario: update the server's live status for `session_id` AFTER the
    /// app is built — e.g. a session that was Busy during the drain goes Idle
    /// while the out-of-turn follow watches it (#284).
    pub(crate) async fn set_session_status(
        &self,
        session_id: &str,
        status: Option<opencode::types::SessionStatus>,
    ) {
        self.session_statuses
            .lock()
            .await
            .insert(session_id.to_string(), status);
    }

    /// Scenario: the server's title for `session_id` (the render poll's
    /// subtitle source, `session_info`).
    pub(crate) fn with_session_title(&mut self, session_id: &str, title: &str) -> &mut Self {
        self.session_titles
            .lock()
            .unwrap()
            .insert(session_id.to_string(), title.to_string());
        self
    }

    /// Scenario: the next `count` `list_permissions` calls hang forever (a
    /// request stuck on a half-open connection); later polls serve normally.
    pub(crate) fn hang_permission_lists(&self, count: usize) -> &Self {
        self.hang_list_permissions
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` `list_questions` calls hang forever.
    pub(crate) fn hang_question_lists(&self, count: usize) -> &Self {
        self.hang_list_questions
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` per-session transcript reads hang forever
    /// (a wedged per-session read).
    pub(crate) fn hang_transcript_reads(&self, count: usize) -> &Self {
        self.hang_transcript
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` `session_info` calls hang forever (a wedged
    /// subtitle fetch on a freshly spawned server).
    pub(crate) fn hang_session_info_reads(&self, count: usize) -> &Self {
        self.hang_session_info
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the next `count` `list_sessions` calls hang forever (the Lazy
    /// Start readiness probe's wedged startup window).
    pub(crate) fn hang_session_lists(&self, count: usize) -> &Self {
        self.hang_list_sessions
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: the FIRST `session_status` read reports Busy, later reads
    /// serve the map (the ADR-0028 busy→idle race).
    pub(crate) fn busy_then_idle_once(&self) -> &Self {
        self.status_busy_once
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: `request` arrives AFTER the app was built (the snapshot was
    /// already sent, ADR-0028).
    pub(crate) fn ask_permission_later(&self, request: opencode::types::PermissionRequest) -> &Self {
        self.extra_permissions
            .try_lock()
            .expect("ask_permission_later before the app is built")
            .push(request);
        self
    }

    /// Scenario: the model answers the external message with `parts`. Returns
    /// the gate the test flips once the notification card has been sent.
    pub(crate) fn external_reply(&mut self, parts: Vec<Part>) -> Arc<std::sync::atomic::AtomicBool> {
        self.external_reply_parts = Some(parts);
        Arc::clone(&self.external_reply_ready)
    }

    /// Scenario: `child` is a sub-task session whose parent is `parent`
    /// (`session_info` serves the parent chain).
    pub(crate) fn with_session_parent(&mut self, child: &str, parent: &str) -> &mut Self {
        self.session_parents.insert(child.to_string(), parent.to_string());
        self
    }

    /// Scenario: the mapped session no longer exists on the server — `prompt`
    /// 404s for any session other than the one `create_session` returns.
    pub(crate) fn stale_session_mapping(&mut self) -> &mut Self {
        self.stale_session_404 = true;
        self
    }

    /// Scenario: the next `count` `create_session` calls fail (materialisation
    /// must keep the pending and retry on the next message).
    pub(crate) fn fail_create_sessions(&mut self, count: usize) -> &mut Self {
        self.fail_create_session_count
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// Scenario: `update_session_title` fails (materialisation must keep the
    /// created session and warn).
    pub(crate) fn title_patch_fails(&mut self) -> &mut Self {
        self.fail_title_patch = true;
        self
    }

    /// Scenario: `prompt_async` fails with `message` (a Supplement whose send
    /// failed).
    pub(crate) fn fail_supplement(&mut self, message: &str) -> &mut Self {
        self.prompt_async_error = Some(message.to_string());
        self
    }

    /// Scenario: `session_status` fails with `message` (the caller must not
    /// guess a status).
    pub(crate) fn status_read_fails(&mut self, message: &str) -> &mut Self {
        self.session_status_error = Some(message.to_string());
        self
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
    /// The default transcript shape `transcript` serves when the test scripts
    /// no transcript for the session: cola's own or an external user message
    /// when modeled, otherwise the assistant side of cola's own turn.
    fn default_transcript(&self, session_id: &str) -> SessionTranscript {
        let now = chrono::Utc::now().timestamp_millis();
        let mut messages: Vec<TranscriptMessage> = Vec::new();
        // cola's OWN user message persisting on the store (ADR-0026): id starts
        // with `msg_cola_`, created time stable across polls. Simulates a prompt
        // cola sent that the poller must recognise as cola-authored even when it
        // surfaces AFTER a stale watermark (server died mid-turn then healed).
        if let Some(cola_text) = self.cola_user_messages.get(session_id) {
            let created = {
                let mut map = self.cola_user_created.lock().unwrap();
                *map.entry(session_id.to_string())
                    .or_insert_with(|| chrono::Utc::now().timestamp_millis())
            };
            messages.push(typed_message(
                "msg_cola_mock_user",
                MessageRole::User,
                Some(created),
                vec![text_part(cola_text)],
            ));
        }
        // When set, simulate a user message posted by ANOTHER client (e.g.
        // OpenChamber), for the external-message poller tests. If an AI
        // reply is also set (and `external_reply_ready` has flipped), return
        // it as the assistant turn — simulates OpenCode answering the
        // shared-store message.
        let text = self
            .external_user_messages
            .get(session_id)
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
                map.get(session_id)
                    .map(|c| created.max(c + 1000))
                    .unwrap_or(created)
            };
            messages.push(typed_message(
                "msg_ext_user",
                MessageRole::User,
                Some(created),
                vec![text_part(&text)],
            ));
            if self
                .external_reply_ready
                .load(std::sync::atomic::Ordering::SeqCst)
                && let Some(parts) = &self.external_reply_parts
            {
                messages.push(typed_message(
                    "msg_ext_assist",
                    MessageRole::Assistant,
                    Some(created + 1000),
                    parts.clone(),
                ));
            }
            return SessionTranscript::new(messages);
        }
        // No cola-authored or external user message modeled: return only the
        // assistant side of cola's own turn (the default rendering path).
        if !messages.is_empty() {
            return SessionTranscript::new(messages);
        }
        // A matched prompt script's parts win over the construction-time
        // `parts`: the render poll must see the same turn the prompt streamed.
        let parts = self
            .last_prompt_parts
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.parts.clone());
        SessionTranscript::new(vec![typed_message(
            "msg_assist",
            MessageRole::Assistant,
            Some(now + 1000),
            parts,
        )])
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
impl crate::backend::Backend for MockBackend {
    fn new_session_input(&self, directory: Option<&str>) -> opencode::types::CreateSessionInput {
        opencode::types::CreateSessionInput {
            id: None,
            agent: None,
            model: Some(opencode::types::ModelInfo {
                id: "m".into(),
                provider_id: "p".into(),
                variant: None,
            }),
            location: directory.map(|d| opencode::types::Location {
                directory: d.to_string(),
            }),
        }
    }

    async fn create_session(
        &self,
        input: &opencode::types::CreateSessionInput,
    ) -> crate::error::Result<opencode::types::Session> {
        self.created_session_dirs
            .lock()
            .await
            .push(input.location.as_ref().map(|l| l.directory.clone()));
        if self
            .fail_create_session_count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.fail_create_session_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                "Simulated create failure".into(),
            ));
        }
        Ok(opencode::types::Session {
            id: self.session_id.clone(),
            project_id: None,
            agent: None,
            title: None,
            location: None,
            cost: None,
            time: None,
        })
    }

    async fn list_sessions(&self) -> crate::error::Result<Vec<opencode::types::SessionListInfo>> {
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
        if self.fail_title_patch {
            return Err(crate::error::BridgeError::OpenCode(
                "Simulated title failure".into(),
            ));
        }
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
        images: &[opencode::types::ImageInput],
        _model: Option<&opencode::types::ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> crate::error::Result<opencode::types::PromptResponse> {
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
        // Hold the turn in flight while a test injects state (a pending
        // request) through the normal seams, then release it into the scripted
        // outcome — abort or success.
        if let Some(gate) = &self.prompt_gate {
            let _permit = gate.acquire().await;
        }
        if self.stale_session_404 && session_id != self.session_id {
            return Err(crate::error::BridgeError::SessionNotFound(session_id.to_string()));
        }
        if self.fail_prompt_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.fail_prompt_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(crate::error::BridgeError::OpenCode(
                self.fail_prompt_message
                    .clone()
                    .unwrap_or_else(|| "Simulated provider failure".into()),
            ));
        }
        if let Some(err) = &self.prompt_error {
            return Err(crate::error::BridgeError::OpenCode(err.clone()));
        }
        if let Some(hook) = &self.on_prompt {
            hook();
        }
        // A prompt script wins over the construction-time `parts`: the prompt
        // that matches streams its own parts, and `transcript` serves them too.
        let parts = match self
            .prompt_scripts
            .iter()
            .find(|(needle, _)| needle.is_empty() || text.contains(needle.as_str()))
        {
            Some((_, parts)) => {
                *self.last_prompt_parts.lock().unwrap() = Some(parts.clone());
                parts.clone()
            }
            None => self.parts.clone(),
        };
        Ok(opencode::types::PromptResponse {
            id: "msg_assist".into(),
            session_id: Some(session_id.to_string()),
            admitted_seq: None,
            parent_id: Some("msg_user".into()),
            error: None,
            // The mock scripts typed parts: the prompt-response path carries
            // the same neutral views the polled read does (spec #332).
            parts,
        })
    }

    async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        images: &[opencode::types::ImageInput],
        _model: Option<&opencode::types::ModelInfo>,
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
        if let Some(err) = &self.prompt_async_error {
            return Err(crate::error::BridgeError::OpenCode(err.clone()));
        }
        Ok(())
    }

    /// Read one Session's neutral transcript (ADR-0053). A scripted transcript
    /// wins; otherwise the scenario's default typed shape is served (spec
    /// #332, #334–#339).
    async fn transcript(&self, session_id: &str) -> crate::error::Result<SessionTranscript> {
        hang_if_scripted(&self.hang_transcript).await;
        let scripted = {
            let mut scripts = self.transcript_scripts.lock().await;
            match scripts.get_mut(session_id) {
                Some(script) if !script.is_empty() => Some(if script.len() == 1 {
                    script[0].clone()
                } else {
                    script.remove(0)
                }),
                _ => None,
            }
        };
        let transcript = match scripted {
            Some(transcript) => transcript,
            None => self.default_transcript(session_id),
        };
        self.transcript_calls.lock().await.push(session_id.to_string());
        Ok(transcript)
    }

    async fn list_permissions(
        &self,
        _d: Option<&str>,
    ) -> crate::error::Result<Vec<opencode::types::PermissionRequest>> {
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
    ) -> crate::error::Result<Vec<opencode::types::QuestionRequest>> {
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
        self.context_window_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.context_window)
    }

    fn configured_default_model(&self) -> Option<opencode::types::ModelInfo> {
        self.default_model.clone()
    }

    async fn list_agents(&self) -> Vec<opencode::types::AgentInfo> {
        self.agents.clone()
    }

    async fn list_models(&self) -> Vec<opencode::types::ProviderModels> {
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
    ) -> crate::error::Result<opencode::types::SessionInfo> {
        hang_if_scripted(&self.hang_session_info).await;
        Ok(opencode::types::SessionInfo {
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
    ) -> crate::error::Result<Option<opencode::types::SessionStatus>> {
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
            return Ok(Some(opencode::types::SessionStatus::Busy));
        }
        // A scripted entry is served verbatim (`Some(None)` → unknown); a
        // missing key means idle (the server removes finished runs).
        Ok(match self.session_statuses.lock().await.get(session_id) {
            Some(status) => *status,
            None => Some(opencode::types::SessionStatus::Idle),
        })
    }

    async fn interrupt(&self, _s: &str) -> crate::error::Result<()> {
        self.interrupt_calls.lock().await.push(_s.to_string());
        Ok(())
    }
    async fn compact(&self, _s: &str) -> crate::error::Result<()> {
        self.compact_calls.lock().await.push(_s.to_string());
        Ok(())
    }
    async fn reconnect(&self, _url: &str, _password: &str) -> crate::error::Result<()> {
        Ok(())
    }
    fn base_url(&self) -> String {
        "http://mock".into()
    }

    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn crate::backend::DirectoryBackend> {
        Arc::new(crate::backend::BackendDirectory::new(self, directory.to_string()))
    }
}

/// The test Host every `test_config` app is claimed for (ADR-0035). Messages
/// built with [`incoming`] and no explicit requester act as this Principal.
pub(crate) const TEST_HOST: &str = "ou_test_host";

pub fn test_config(session_file: &std::path::Path) -> crate::config::Config {
    let cfg = crate::config::Config {
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
            access_file: session_file.with_file_name("access.json"),
            work_dir: None,
            group_completion_notice: true,
            long_task_notice: false,
            instant_reminder: false,
            log_days: 14,
        },
    };
    // A test config comes claimed for TEST_HOST: the suite's apps are ready to
    // use. The gate tests build an unclaimed config explicitly.
    let mut access = crate::bridge::access::AccessList::load(&cfg.bridge.access_file);
    access.claim(TEST_HOST).unwrap();
    cfg
}

/// A config whose Access List does not exist — the bot is unclaimed.
pub fn test_config_unclaimed(session_file: &std::path::Path) -> crate::config::Config {
    let mut cfg = test_config(session_file);
    cfg.bridge.access_file = session_file.with_file_name("access-unclaimed.json");
    cfg
}

/// The parts a real assistant turn produces: reasoning → tool → text.
pub fn realistic_parts() -> Vec<Part> {
    vec![
        Part::StepStart(StepStart),
        Part::Reasoning(ReasoningPart {
            text: "用户想让我分析目录。".into(),
            started_at: None,
        }),
        tool_part(
            "bash",
            "call_1",
            ToolStatus::Completed,
            serde_json::json!({ "command": "ls -la" }),
            "src/\nCargo.toml\n",
        ),
        Part::StepFinish(StepFinish {
            reason: FinishReason::ToolCalls,
        }),
        Part::StepStart(StepStart),
        Part::Text(TextPart {
            text: "当前目录有 src/ 和 Cargo.toml。".into(),
            started_at: None,
        }),
        Part::StepFinish(StepFinish {
            reason: FinishReason::Stop,
        }),
    ]
}

/// A prompt whose answer is far longer than one card's text budget, so it
/// must flow across continuation cards (no plain-text fallback anymore).
pub fn long_answer_parts() -> Vec<Part> {
    // 1200 × 6 chars = 7200 chars, above MAX_CARD_TEXT_CHARS (6000).
    let long_text = "很长的回答。".repeat(1200);
    vec![
        Part::StepStart(StepStart),
        Part::Text(TextPart {
            text: long_text,
            started_at: None,
        }),
        Part::StepFinish(StepFinish {
            reason: FinishReason::Stop,
        }),
    ]
}

/// Build a `ModelOption` with the given id and declared variants.
pub(crate) fn model_option(id: &str, variants: &[&str]) -> crate::opencode::types::ModelOption {
    crate::opencode::types::ModelOption {
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
/// `None` means "no explicit requester": the message acts as [`TEST_HOST`],
/// the Principal every `test_config` app admits. Use [`incoming_anonymous`]
/// for an identity-less payload.
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
        requester_open_id: Some(requester.unwrap_or_else(|| TEST_HOST.to_string())),
    }
}

/// A plain text message with NO sender identity — the gate must fail closed.
pub(crate) fn incoming_anonymous(
    message_id: String,
    chat_id: String,
    chat_type: String,
    thread_id: Option<String>,
    text: String,
) -> crate::bridge::IncomingMessage {
    crate::bridge::IncomingMessage {
        message_id,
        chat_id,
        chat_type,
        thread_id,
        parent_id: None,
        text,
        images: vec![],
        requester_open_id: None,
    }
}

/// Drive a slash command through the coordinator's message entry point — the
/// production parse → gate → route path — instead of calling the dispatcher
/// directly. The routing is what a test at this level is about; the
/// dispatcher's own unit concerns live in `command.rs`. `text` must round-trip
/// through [`crate::bridge::command::parse_command`] to the same command the
/// test means.
pub(crate) async fn send_command(app: &Arc<App>, text: &str, message_id: &str) {
    send_command_in(
        app,
        text,
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        message_id,
        crate::config::ConversationKind::P2p,
    )
    .await;
}

/// [`send_command`] for an explicit conversation: the helper rebuilds the
/// `IncomingMessage` from `thread_key` and `kind`, so the coordinator derives
/// exactly the routing the direct call used to pass by hand. A
/// [`Topic`](crate::config::ConversationKind::Topic) carries `thread_key`'s
/// thread id; the other kinds are the chat's top level.
pub(crate) async fn send_command_in(
    app: &Arc<App>,
    text: &str,
    thread_key: crate::config::ThreadKey,
    message_id: &str,
    kind: crate::config::ConversationKind,
) {
    let chat_type = match kind {
        crate::config::ConversationKind::GroupLobby => "group",
        _ => "p2p",
    };
    let thread_id = match kind {
        crate::config::ConversationKind::Topic => Some(thread_key.thread_id.clone()),
        _ => None,
    };
    app.handle_message(incoming(
        message_id.into(),
        thread_key.chat_id,
        chat_type.into(),
        thread_id,
        text.into(),
        None,
    ))
    .await;
}

/// Dispatch a card action as the Host (ADR-0035). A real click carries the
/// clicking user's `open_id` on the value (the platform threads it in) and the
/// bridge's gate refuses a click without one, so tests that mean "the Host
/// clicked this" go through this helper instead of `App::handle_card_action`.
#[async_trait::async_trait]
pub(crate) trait HostCardAction {
    async fn host_action(&self, value: serde_json::Value)
    -> Option<crate::bridge::handler::CardActionResult>;
}

#[async_trait::async_trait]
impl HostCardAction for Arc<App> {
    async fn host_action(
        &self,
        value: serde_json::Value,
    ) -> Option<crate::bridge::handler::CardActionResult> {
        let mut value = value;
        value["operator_open_id"] = serde_json::Value::String(TEST_HOST.to_string());
        self.handle_card_action(value).await
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

/// Every visible text string on a card, in walk order: `content` fields
/// (markdown / plain_text, including nested panels) and `text` fields. Button
/// payloads (`value`) are not visible text and stay out, so an assertion on
/// rendered copy reads the card's structure instead of its JSON dump.
pub(crate) fn card_texts(card: &serde_json::Value) -> Vec<String> {
    fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, v) in map {
                    match (key.as_str(), v.as_str()) {
                        ("content" | "text", Some(s)) => out.push(s.to_string()),
                        _ => walk(v, out),
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    walk(v, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(card, &mut out);
    out
}

/// A card's visible text joined by newlines — the structural counterpart of
/// `card.to_string()` for copy assertions.
pub(crate) fn card_text(card: &serde_json::Value) -> String {
    card_texts(card).join("\n")
}

/// Every button element on a card, in walk order (buttons nest in column sets
/// and action blocks) — for assertions on a button's `value` payload or label
/// without string-matching the card's JSON dump.
pub(crate) fn card_buttons(card: &serde_json::Value) -> Vec<&serde_json::Value> {
    fn walk<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("tag").and_then(|t| t.as_str()) == Some("button") {
                    out.push(value);
                }
                for v in map.values() {
                    walk(v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    walk(v, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(card, &mut out);
    out
}

/// A card's header title, for Done/Streaming/topic assertions.
pub(crate) fn card_header(card: &serde_json::Value) -> &str {
    card["header"]["title"]["content"].as_str().unwrap_or("")
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

/// The most recent card the platform saw, whether updated in place or sent as
/// a split's continuation card.
pub(crate) async fn latest_card(platform: &RecordingPlatform) -> serde_json::Value {
    let calls = platform.calls.lock().await;
    calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } | PlatformCall::ReplyCard { card, .. } => {
                Some(card.clone())
            }
            _ => None,
        })
        .expect("a card must have been sent")
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

/// Seed a mapped session carrying the per-session overrides the re-adoption
/// tests assert on: `provider/model-a`, variant `high`, Auto-Accept on.
pub(crate) async fn seed_overridden_entry(
    app: &Arc<App>,
    thread_key: crate::config::ThreadKey,
    session_id: &str,
    directory: &str,
) {
    let mut entry = crate::config::SessionEntry::new(thread_key, session_id, directory);
    entry.model = Some("provider/model-a".into());
    entry.variant = Some("high".into());
    entry.auto_accept = true;
    seed_entry(app, entry).await;
}

/// Seed a Pending Session for a thread (ADR-0041): the `set_pending` write,
/// with the same persist-failure panic policy as [`seed_entry`].
pub(crate) async fn seed_pending(app: &Arc<App>, pending: crate::bridge::session::PendingEntry) {
    app.sessions
        .lock()
        .await
        .set_pending(pending)
        .expect("seed_pending: persisting the seeded store failed");
}

/// Seed the cover-title cache for `session_id` (the topic cover card's title
/// memory) — the `app.core` setup tests would otherwise do by hand.
pub(crate) async fn seed_cover_title(app: &Arc<App>, session_id: &str, title: &str) {
    app.core.cover_titles.lock().await.insert(
        session_id.into(),
        crate::bridge::core::CoverTitle {
            title: title.into(),
            model: None,
            pending: false,
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

/// #144 rig: the three live surfaces a pending request may own. The caller
/// seeds the one session (`ses_1`, `/work`) before passing these to
/// [`assert_failed_dir_keeps_surfaces`].
pub(crate) struct FailedDirSurfaces {
    /// The standalone card's request id and Message id.
    pub card_id: &'static str,
    pub card_message_id: &'static str,
    /// The inline request id, already seeded onto `inline_acc`.
    pub inline_id: &'static str,
    /// The snapshot host's Message id; the claim rides `claim`.
    pub snapshot_message_id: &'static str,
    /// The request inlined on the streaming card, seeded onto it by the rig.
    pub inline_request: crate::bridge::request::kind::PendingRequest,
    /// The request embedded (and claimed) by the snapshot card.
    pub claim: crate::bridge::request::kind::PendingRequest,
}

/// Seed `request` as an inline block on `session_id`'s card, kind-agnostically —
/// the #144 rig's fixture builder.
async fn seed_inline_request(
    cards: &crate::bridge::handles::CardsHandle,
    session_id: &str,
    request: &crate::bridge::request::kind::PendingRequest,
    directory: &str,
) {
    use crate::bridge::request::kind::PendingRequest;
    match request {
        PendingRequest::Permission(p) => {
            Turn::add_permission(cards, session_id, p, directory).await;
        }
        PendingRequest::Question(q) => {
            Turn::add_question(
                cards,
                session_id,
                q,
                directory,
                &vec![None; q.questions.len()],
                &vec![false; q.questions.len()],
            )
            .await;
        }
    }
}

/// #144: drive one hanging-list sweep and one successful-list sweep over the
/// three surfaces: the first must keep them all ("unknown must never be read
/// as resolved"), the second must still fire every cleanup. `flow` and `hang`
/// pick the kind; `surfaces` carries its kind-specific fixtures.
pub(crate) async fn assert_failed_dir_keeps_surfaces(
    app: &Arc<App>,
    flow: &crate::bridge::request::flow::RequestFlow,
    hang: &std::sync::atomic::AtomicUsize,
    platform: &Arc<RecordingPlatform>,
    surfaces: FailedDirSurfaces,
) {
    flow.list_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);
    let claim_id = surfaces.claim.id().to_string();

    // One live surface per request, all owned by the failing directory /work.
    flow.sent_cards.lock().await.insert(
        surfaces.card_id.into(),
        crate::bridge::request::flow::SentCard {
            message_id: surfaces.card_message_id.into(),
            summary: "待处理的请求".into(),
            directory: "/work".into(),
        },
    );
    let cards = app.core.cards_handle();
    Turn::seed_card(&cards, "ses_1", None).await;
    seed_inline_request(&cards, "ses_1", &surfaces.inline_request, "/work").await;
    app.core.snapshot_claims.lock().await.claim(
        surfaces.snapshot_message_id,
        "已接管",
        "标题",
        &crate::bridge::snapshot::SnapshotData {
            session_id: "ses_1".into(),
            directory: "/work".into(),
            status: None,
            pending: vec![surfaces.claim],
            pending_elsewhere: None,
            tail: vec![],
            newest_user_anchor: None,
            newest_user_is_cola_authored: false,
        },
        None,
    );

    // The list call hangs: /work said nothing this sweep.
    hang.store(1, std::sync::atomic::Ordering::SeqCst);
    let mut seen = std::collections::HashSet::new();
    flow.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        flow.sent_cards.lock().await.contains_key(surfaces.card_id),
        "a failed list must not mark the standalone card stale"
    );
    assert!(
        inline_surface_live(app, surfaces.inline_id).await,
        "a failed list must not drop the inline section"
    );
    assert!(
        app.core.snapshot_claims.lock().await.contains(&claim_id),
        "a failed list must not drop the snapshot claim"
    );
    let first_calls = platform.calls.lock().await.clone();
    assert!(
        first_calls.iter().all(|c| !matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. }
                if message_id == surfaces.card_message_id
                    || message_id == surfaces.snapshot_message_id
        )),
        "a failed list must not re-render any surface: {first_calls:?}"
    );

    // The next sweep lists /work successfully with the requests gone: every
    // cleanup fires.
    flow.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        !flow.sent_cards.lock().await.contains_key(surfaces.card_id),
        "a successful list must still stale the gone card"
    );
    assert!(
        !inline_surface_live(app, surfaces.inline_id).await,
        "a successful list must still drop the gone inline section"
    );
    assert!(
        inline_handled_elsewhere_receipt_present(app).await,
        "a successful list must leave the gone inline block's receipt (#175)"
    );
    assert!(
        !app.core.snapshot_claims.lock().await.contains(&claim_id),
        "a successful list must still drop the gone claim"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, card }
                if message_id == surfaces.card_message_id && card.to_string().contains("已处理")
        )),
        "the standalone card is marked stale: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. }
                if message_id == surfaces.snapshot_message_id
        )),
        "the snapshot is re-rendered without the block: {calls:?}"
    );
}

/// Whether any card still carries a LIVE inline block for `id` (either kind) —
/// the #144 rig's surface probe. A resolved block left its receipt tombstone in
/// the section, which is not live (ADR-0038).
async fn inline_surface_live(app: &Arc<App>, id: &str) -> bool {
    Turn::has_live_interaction(&app.cards_handle(), id).await
}

/// #175: whether any card carries an Interaction Receipt left by a sweep over a
/// vanished inline block — the #144 rig proves it lands once a SUCCESSFUL list
/// resolves the block.
async fn inline_handled_elsewhere_receipt_present(app: &Arc<App>) -> bool {
    Turn::has_receipt_prefix(&app.cards_handle(), "⏱ 已由其他客户端处理").await
}

// ===== Session discovery & adoption (ADR-0008) =====

/// A helper: a session in the shared store with the given title/dir/id.
pub(crate) fn list_session(
    id: &str,
    title: &str,
    directory: &str,
    updated: i64,
) -> opencode::types::SessionListInfo {
    opencode::types::SessionListInfo {
        id: id.into(),
        title: title.into(),
        directory: directory.into(),
        parent_id: None,
        agent: None,
        model: None,
        time: Some(opencode::types::SessionTime {
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
    backend.given_sessions(vec![list_session("ses_own1", "本项目会话", "/work/cola", 500)]);
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
) -> opencode::types::PermissionRequest {
    opencode::types::PermissionRequest {
        request_id: request_id.into(),
        session_id: Some(session_id.into()),
        permission: Some("bash".into()),
        patterns: vec![pattern.into()],
        metadata: None,
        always: vec![],
    }
}

// ===== Session-scoped log capture (ADR-0048) =====

/// An in-memory [`tracing_subscriber::fmt::MakeWriter`]: every formatted event
/// appends to one shared byte buffer. Cloneable because `make_writer` hands a
/// fresh writer to each event.
#[derive(Clone, Default)]
struct CaptureBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureBuffer {
    type Writer = CaptureBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The level the fmt layer rendered on a captured line — the token after the
/// timestamp (`INFO`, `DEBUG`, `WARN`, …). Lets a test pin a line's level,
/// which the capture's TRACE-wide filter would otherwise hide.
pub(crate) fn line_level(line: &str) -> &str {
    line.split_whitespace().nth(1).unwrap_or("")
}

/// The first captured line containing `needle` — the line an assertion is
/// about — or a panic dumping every captured line.
pub(crate) fn line_with<'a>(logs: &'a str, needle: &str) -> &'a str {
    logs.lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no captured line contains {needle:?}:\n{logs}"))
}

/// Assert that the captured line containing `needle` was rendered at `level`
/// and — unless `level` is INFO itself — that no same-`needle` line leaks at
/// INFO: the ADR-0048 policy keeps whole payload dumps and ack timings off
/// INFO, so a demoted line that is still emitted there is a regression. The
/// line is returned so the caller can additionally assert on its body.
pub(crate) fn assert_line_level<'a>(logs: &'a str, needle: &str, level: &str) -> &'a str {
    let line = line_with(logs, needle);
    assert_eq!(
        line_level(line),
        level,
        "the {needle:?} line must be rendered at {level}: {line}"
    );
    if level != "INFO" {
        assert!(
            !logs
                .lines()
                .any(|other| line_level(other) == "INFO" && other.contains(needle)),
            "the {needle:?} line must never be emitted at INFO:\n{logs}"
        );
    }
    line
}

/// How many captured lines contain `needle` and were rendered at `level` — the
/// count [`assert_line_level`] cannot express (a WARN-once policy is about how
/// many lines there are, and which level each carries).
pub(crate) fn level_count(logs: &str, needle: &str, level: &str) -> usize {
    logs.lines()
        .filter(|line| line_level(line) == level && line.contains(needle))
        .count()
}

/// Run `body` under a captured subscriber and return its output plus every log
/// line it emitted.
///
/// The subscriber writes production's plain-text, ANSI-free shape into an
/// in-memory buffer ([`CaptureBuffer`]) and is installed with
/// `tracing::subscriber::set_default`, i.e. THREAD-LOCAL: the global subscriber
/// is never touched, so tests stay safe under the harness's parallel threads.
/// No level filter is applied (production's `EnvFilter` default is `cola=info`),
/// so a test can also pin a line's level. Run the body on a current-thread
/// runtime (`#[tokio::test]`) — spawned tasks then share the capture thread and
/// their instrumented spans still land in the buffer.
pub(crate) async fn capture_logs<F, T>(body: F) -> (T, String)
where
    F: std::future::Future<Output = T>,
{
    let buffer = CaptureBuffer::default();
    // A second, sink-writing dispatcher, kept alive for the capture's duration.
    // What defeats the callsite interest cache is REGISTRATION, not TLS
    // installation: `Dispatch::new` registers the dispatch in tracing's
    // dispatcher registry and rebuilds every cached `Interest` — `set_default`
    // below registers this capture the same way. With two registered
    // dispatchers a callsite first hit by a parallel test's thread no longer
    // takes the `JustOne` shortcut (which consults that thread's own, empty,
    // default) and caches `always` instead of `never`.
    let _interest_keepalive = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish(),
    );
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(buffer.clone())
        .finish();

    let guard = tracing::subscriber::set_default(subscriber);
    let output = body.await;
    drop(guard);
    let text = String::from_utf8(buffer.0.lock().unwrap().clone()).expect("the fmt layer writes valid UTF-8");
    (output, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::backend::{MessageRole, Part, SessionTranscript};

    /// A scripted transcript is served as-is, so a test can describe cola's
    /// domain instead of the backend's wire format (spec #332).
    #[tokio::test]
    async fn mock_serves_a_scripted_transcript_as_is() {
        let mut mock = MockBackend::new(Vec::new());
        mock.given_transcript(
            "ses_x",
            vec![SessionTranscript::new(vec![typed_message(
                "msg_u1",
                MessageRole::User,
                Some(1000),
                vec![text_part("脚本化的问题")],
            )])],
        );
        let transcript_calls = std::sync::Arc::clone(&mock.transcript_calls);
        let backend: std::sync::Arc<dyn Backend> = std::sync::Arc::new(mock);

        let transcript = backend.transcript("ses_x").await.unwrap();

        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].text(), "脚本化的问题");
        assert_eq!(
            transcript_calls.lock().await.as_slice(),
            &["ses_x".to_string()],
            "the transcript read must be recorded for polls that wait on it"
        );
    }

    /// Without a script, the mock serves the default typed shape built from the
    /// scenario's parts.
    #[tokio::test]
    async fn mock_serves_its_default_typed_shape_when_no_transcript_is_scripted() {
        let mock = MockBackend::new(realistic_parts());
        let transcript_calls = std::sync::Arc::clone(&mock.transcript_calls);
        let backend: std::sync::Arc<dyn Backend> = std::sync::Arc::new(mock);

        let transcript = backend.transcript("ses_test").await.unwrap();
        assert_eq!(transcript_calls.lock().await.len(), 1, "the read is recorded");

        assert_eq!(transcript.messages.len(), 1);
        let message = &transcript.messages[0];
        assert_eq!(message.role, MessageRole::Assistant);
        assert_eq!(message.text(), "当前目录有 src/ 和 Cargo.toml。");
        assert!(
            message
                .parts
                .iter()
                .any(|part| matches!(part, Part::Tool(tool) if tool.identity.name == "bash")),
            "the typed tool part must round-trip: {:?}",
            message.parts
        );
    }
}
