pub mod command;
pub mod core;
pub mod discovery;
pub mod external;
pub mod handler;
pub mod permission;
pub mod pollers;
pub mod question;
pub mod render;
pub mod session;
pub mod streaming;

pub use handler::App;

use async_trait::async_trait;

/// The bridge-facing sink the Feishu platform delivers events into. Abstracted
/// so `feishu::ws` depends on this small interface rather than the concrete
/// `App` type — the mirror of how `Platform`/`Backend` abstract the other
/// direction. Implemented by [`App`].
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn handle_message(
        &self,
        message_id: String,
        chat_id: String,
        chat_type: String,
        thread_id: Option<String>,
        text: String,
        requester_open_id: Option<String>,
    );

    async fn handle_card_action(&self, value: serde_json::Value) -> Option<handler::CardActionResult>;
}
