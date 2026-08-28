pub mod card;
pub mod client;
pub mod event;
pub mod ws;

pub use client::Client;

use crate::error::Result;
use async_trait::async_trait;
use serde_json::Value;

/// The Feishu platform, abstracted so the bridge core can be tested with a
/// recording adapter (captures every card cola would send) or, in the live
/// smoke test, the real [`Client`].
#[async_trait]
pub trait Platform: Send + Sync {
    async fn get_ws_endpoint(&self) -> Result<String>;

    async fn reply_card(&self, reply_to: &str, card: &Value) -> Result<String>;

    async fn send_card(&self, receive_id_type: &str, receive_id: &str, card: &Value) -> Result<String>;

    async fn update_message(&self, message_id: &str, card: &Value) -> Result<()>;

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

    async fn update_message(&self, message_id: &str, card: &Value) -> Result<()> {
        Client::update_message(self, message_id, card).await
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
}
