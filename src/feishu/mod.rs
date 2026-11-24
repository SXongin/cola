pub mod card;
pub mod client;
pub(crate) mod delivery;
pub mod event;
pub(crate) mod file;
pub(crate) mod image;
pub(crate) mod message;
pub(crate) mod pbbp2;
pub mod ws;

use crate::backend::FileContent;
use crate::error::Result;
use async_trait::async_trait;
use client::Client;
use serde_json::Value;

/// A one-shot callback the delivery layer runs when the Pending Card Update a
/// Completion Notice is gated on finally drains (spec #602, ticket #607). The
/// bridge builds the closure (it spawns the notice send, which needs the Turn's
/// own facts); the delivery layer only fires it, exactly once, when the write
/// it was armed against delivers.
pub(crate) type DeferredNotice = Box<dyn FnOnce() + Send + 'static>;

/// What a Completion Notice gate found (spec #602, ticket #607): whether the
/// card the terminal slice was written to still owes that write, so the notice
/// must wait for the retry's drain instead of racing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoticeGate {
    /// The card still owes a keyless Pending Card Update: the callback was
    /// stored and fires when that update delivers.
    Armed,
    /// Nothing is owed — the terminal write already landed (or the platform
    /// does not observe card writes): send the notice now.
    Delivered,
    /// The card's newest write settled without delivering (a permanent
    /// refusal): nothing will ever carry the terminal slice there, so the
    /// notice is suppressed.
    Never,
}

/// The Feishu platform, abstracted so the bridge core can be tested with a
/// recording adapter (captures every card cola would send) or, in the live
/// smoke test, the real [`Client`].
#[async_trait]
pub trait Platform: Send + Sync {
    async fn get_ws_endpoint(&self) -> Result<String>;

    async fn reply_card(&self, reply_to: &str, card: &Value) -> Result<String>;

    async fn send_card(&self, receive_id_type: &str, receive_id: &str, card: &Value) -> Result<String>;

    /// Send a message of an arbitrary `msg_type` into a chat (or user),
    /// mirroring [`Self::send_card`]'s `receive_id_type`/`receive_id` request
    /// shape. The chat-target counterpart of [`Self::reply_message`], used when
    /// a lane has a chat but no message to reply under — the permission
    /// companion's Rich Text and File Messages.
    async fn send_message(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: &Value,
    ) -> Result<String>;

    async fn update_message(&self, message_id: &str, card: &Value) -> Result<()>;

    /// Edit an existing non-card message (e.g. a Rich Text Message) with the
    /// platform's message-edit API. `Refused` means the platform definitively
    /// rejected the edit; transport/decoding failures remain `Err` because the
    /// edit may have landed ambiguously.
    async fn edit_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &Value,
    ) -> Result<MessageEditOutcome> {
        let _ = (message_id, msg_type, content);
        Err(crate::error::BridgeError::Feishu(
            "message editing is unsupported by this Platform".into(),
        ))
    }

    /// This platform's **delivery observer**: the one owner of ordered keyed
    /// delivery, the Pending Card Update drain and every delivery query
    /// (ADR-0067, ADR-0072). The behaviour is not a set of [`Platform`]
    /// defaults — it lives on [`delivery::DeliveryObserver`], implemented by
    /// the delivery decorator.
    ///
    /// The default is [`delivery::UNOBSERVED`], the ONE documented no-op: it
    /// holds no delivery state, so every query answers "nothing owed / not
    /// delivered", and its `submit_ordered` **fails visibly**
    /// ([`crate::error::BridgeError::Feishu`], with a WARN naming the card)
    /// rather than silently writing unordered. A platform that wants ordered
    /// delivery implements [`delivery::DeliveryObserver`] and returns `self`;
    /// [`delivery::CardDelivery`] is the production implementor.
    fn delivery(&self) -> &dyn delivery::DeliveryObserver {
        &delivery::UNOBSERVED
    }

    async fn reply_text(&self, message_id: &str, text: &str) -> Result<String>;

    /// Reply to a message in thread form (`reply_in_thread: true`), creating a
    /// topic. Returns the created reply's `message_id` (an anchor inside the
    /// topic) and the topic's `thread_id` (or `None` if the chat does not
    /// support topic replies).
    async fn reply_in_thread(&self, message_id: &str, text: &str) -> Result<(String, Option<String>)>;

    /// Reply to a message with a completion notice. When `name` is known the
    /// requester is @-mentioned (`<at user_id="...">name</at>`); otherwise a
    /// plain reply — Feishu still notifies the message's author either way.
    async fn reply_completion_notice(
        &self,
        message_id: &str,
        open_id: &str,
        name: Option<&str>,
        text: &str,
    ) -> Result<String>;

    /// The display name of a user (contact API), best-effort `Ok(None)` on failure.
    async fn user_name(&self, open_id: &str) -> Result<Option<String>>;

    /// The display name of a chat (im API), best-effort `Ok(None)` on failure.
    async fn chat_name(&self, chat_id: &str) -> Result<Option<String>>;

    /// The bot's own open_id, used to recognise @mentions of cola itself.
    async fn bot_open_id(&self) -> Result<String>;

    /// List messages in a container (chat or topic). `container_id_type` is
    /// `"chat"` or `"thread"`; used to find a message INSIDE a topic to reply
    /// into it (the create API cannot target a `thread_id`).
    async fn list_messages(
        &self,
        container_id_type: &str,
        container_id: &str,
    ) -> Result<Vec<client::ChatMessage>>;

    /// Fetch a message by id (`GET /im/v1/messages/{id}`), used to build the
    /// Quoted Context for a reply. Best-effort for the bridge: callers degrade
    /// to text-only on any error (e.g. the `im:message` permission missing).
    async fn get_message(&self, message_id: &str) -> Result<client::FeishuMessage>;

    /// Fetch a card message's currently rendered view
    /// (`GET /im/v1/messages/{id}?card_msg_content_type=user_card_content`):
    /// the schema-2.0 JSON a PATCH of the same message accepts back. The
    /// durable reap reads it to keep an orphaned card's existing body while it
    /// restamps the header (ADR-0063, #434 acceptance feedback). Best-effort:
    /// callers degrade to the bare ending on any error.
    async fn get_card_view(&self, message_id: &str) -> Result<Value>;

    /// Upload a File Content as a Feishu image and return the reusable
    /// `image_key` a card's `img` element references (`POST
    /// /open-apis/im/v1/images`, `image_type=message`, scope `im:resource`;
    /// ADR-0076). `Ok(None)` when `content` is not an embeddable image — a mime
    /// outside Feishu's set, or bytes/dimensions past its caps — so the caller
    /// leaves it to the File Message path (#649). Best-effort at the call site:
    /// any `Err` leaves the card intact and never fails the Turn.
    async fn upload_image(&self, content: &FileContent) -> Result<Option<String>>;

    /// Upload a File Content as a Feishu file and return the reusable
    /// `file_key` a `msg_type:"file"` message references (`POST
    /// /open-apis/im/v1/files`, scope `im:resource`; ADR-0076). `Ok(None)` when
    /// `content` is past Feishu's 30MB message cap — nothing is uploaded, so the
    /// caller marks it `未发送` without a request. Best-effort at the call site:
    /// any `Err` leaves the card intact and never fails the Turn.
    async fn upload_file(&self, content: &FileContent) -> Result<Option<String>>;

    /// Send a message of an arbitrary `msg_type` as a reply under `message_id`
    /// (ADR-0076): the send path for a File Message (`msg_type:"file"`,
    /// `content` `{"file_key": …}`). The body OMITS `reply_in_thread`, so Feishu
    /// keeps the reply in the target card's own container — the Session's topic
    /// when the card is already threaded, a plain reply that creates no topic
    /// otherwise (ADR-0076's amendment; a thread reply in a lobby would spawn a
    /// new topic and fork the conversation). Returns the created message id and
    /// the topic's `thread_id` (`None` when the reply created no topic, or when
    /// the chat does not support topic replies). Best-effort at the call site.
    async fn reply_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &Value,
    ) -> Result<(String, Option<String>)>;

    /// Download an image embedded in a message (`GET /im/v1/messages/{id}/resources/{key}?type=image`),
    /// used to attach images to a prompt. Requires the `im:message`
    /// permission (already held); callers degrade to a `[图片]` placeholder on
    /// error.
    async fn download_image(&self, message_id: &str, image_key: &str) -> Result<client::ImageAttachment>;

    /// Download a file attached to a message (`GET
    /// /im/v1/messages/{id}/resources/{key}?type=file`, ADR-0076's inbound
    /// amendment), used to attach a Prompt Attachment to a prompt. `name` is
    /// the display name from the message content (the resource response carries
    /// none). Requires the `im:message` permission (already held); a caller
    /// reports a failure instead of dropping the file silently.
    async fn download_file(
        &self,
        message_id: &str,
        file_key: &str,
        name: &str,
    ) -> Result<client::FileAttachment>;

    /// Set or clear a Chat/Topic's **Instant Reminder** (`time_sensitive`,
    /// ADR-0043): a group targets its own `chat_id`, a bot p2p chat targets
    /// the fixed bot feed card. `user_ids` are the open_ids whose message
    /// lists are pinned (the turn's requester). Best-effort for the bridge: a
    /// failure (e.g. the `im:datasync.feed_card.time_sensitive:write` scope
    /// is missing) is logged by the caller and never affects the turn.
    async fn set_instant_reminder(
        &self,
        chat_id: &str,
        is_group: bool,
        user_ids: &[String],
        on: bool,
    ) -> Result<()>;

    /// Pin a message into its Chat/Topic's pinned-message list (ADR-0043
    /// amendment): the in-chat locator for a waiting Permission/Question card.
    /// NOT idempotent at Feishu (pinning twice can fail) — the caller owns
    /// single-shot semantics. Best-effort for the bridge: failures log and the
    /// card lives on.
    async fn pin_message(&self, message_id: &str) -> Result<()>;

    /// Remove a message from its Chat/Topic's pinned-message list. Feishu
    /// reports success for a message that was not pinned (e.g. the user
    /// unpinned it by hand), so this is safe to call unconditionally.
    async fn unpin_message(&self, message_id: &str) -> Result<()>;
}

/// Whether an in-place edit was accepted by the messaging platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageEditOutcome {
    Updated,
    /// Feishu refused because this message has exhausted its edit budget.
    EditLimitReached,
    /// A business refusal for any reason other than the exhausted edit budget.
    Refused,
}

#[async_trait]
impl Platform for Client {
    async fn get_ws_endpoint(&self) -> Result<String> {
        Client::get_ws_endpoint(self).await
    }

    async fn reply_card(&self, reply_to: &str, card: &Value) -> Result<String> {
        Client::reply_card(self, reply_to, card).await
    }

    async fn send_card(&self, receive_id_type: &str, receive_id: &str, card: &Value) -> Result<String> {
        Client::send_card(self, receive_id_type, receive_id, card).await
    }

    async fn send_message(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: &Value,
    ) -> Result<String> {
        Client::send_message(self, receive_id_type, receive_id, msg_type, content).await
    }

    async fn update_message(&self, message_id: &str, card: &Value) -> Result<()> {
        Client::update_message(self, message_id, card).await
    }

    async fn edit_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &Value,
    ) -> Result<MessageEditOutcome> {
        Client::edit_message(self, message_id, msg_type, content).await
    }

    async fn reply_text(&self, message_id: &str, text: &str) -> Result<String> {
        Client::reply_text(self, message_id, text).await
    }

    async fn reply_in_thread(&self, message_id: &str, text: &str) -> Result<(String, Option<String>)> {
        Client::reply_in_thread(self, message_id, text).await
    }

    async fn reply_completion_notice(
        &self,
        message_id: &str,
        open_id: &str,
        name: Option<&str>,
        text: &str,
    ) -> Result<String> {
        Client::reply_completion_notice(self, message_id, open_id, name, text).await
    }

    async fn user_name(&self, open_id: &str) -> Result<Option<String>> {
        Client::user_name(self, open_id).await
    }

    async fn chat_name(&self, chat_id: &str) -> Result<Option<String>> {
        Client::chat_name(self, chat_id).await
    }

    async fn bot_open_id(&self) -> Result<String> {
        Client::bot_open_id(self).await
    }

    async fn list_messages(
        &self,
        container_id_type: &str,
        container_id: &str,
    ) -> Result<Vec<client::ChatMessage>> {
        Client::list_messages(self, container_id_type, container_id).await
    }

    async fn get_message(&self, message_id: &str) -> Result<client::FeishuMessage> {
        Client::get_message(self, message_id).await
    }

    async fn get_card_view(&self, message_id: &str) -> Result<Value> {
        Client::get_card_view(self, message_id).await
    }

    async fn upload_image(&self, content: &FileContent) -> Result<Option<String>> {
        if !image::embeddable_image(content) {
            return Ok(None);
        }
        let Some(bytes) = content.bytes() else {
            return Ok(None);
        };
        Client::upload_image(self, &bytes, &content.mime).await.map(Some)
    }

    async fn upload_file(&self, content: &FileContent) -> Result<Option<String>> {
        if !file::deliverable_file(content) {
            return Ok(None);
        }
        let Some(bytes) = content.bytes() else {
            return Ok(None);
        };
        let file_type = file::file_type_for(&content.mime, &content.name);
        Client::upload_file(self, &bytes, file_type, &content.name)
            .await
            .map(Some)
    }

    async fn reply_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &Value,
    ) -> Result<(String, Option<String>)> {
        Client::reply_message(self, message_id, msg_type, content).await
    }

    async fn download_image(&self, message_id: &str, image_key: &str) -> Result<client::ImageAttachment> {
        Client::download_image(self, message_id, image_key).await
    }

    async fn download_file(
        &self,
        message_id: &str,
        file_key: &str,
        name: &str,
    ) -> Result<client::FileAttachment> {
        Client::download_file(self, message_id, file_key, name).await
    }

    async fn set_instant_reminder(
        &self,
        chat_id: &str,
        is_group: bool,
        user_ids: &[String],
        on: bool,
    ) -> Result<()> {
        Client::set_instant_reminder(self, chat_id, is_group, user_ids, on).await
    }

    async fn pin_message(&self, message_id: &str) -> Result<()> {
        Client::pin_message(self, message_id).await
    }

    async fn unpin_message(&self, message_id: &str) -> Result<()> {
        Client::unpin_message(self, message_id).await
    }
}
