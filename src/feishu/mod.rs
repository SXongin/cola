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

    /// The bot's own open_id, used to recognise @mentions of cola itself.
    async fn bot_open_id(&self) -> Result<String>;
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

    async fn bot_open_id(&self) -> Result<String> {
        Client::bot_open_id(self).await
    }
}
