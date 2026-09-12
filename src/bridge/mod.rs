pub mod command;
pub mod core;
pub mod discovery;
pub mod external;
pub mod handler;
pub mod pollers;
pub mod render;
pub mod request;
pub mod session;
pub mod snapshot;
pub mod streaming;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
pub(crate) mod tests;

pub use handler::App;

use crate::feishu::client::ImageAttachment;
use async_trait::async_trait;

/// Bound one backend call by a timeout. `None` means the call timed out
/// (already logged): the caller moves on and retries on its next tick.
/// Background pollers use this so a single request stuck on a half-open
/// connection (e.g. across a server restart) cannot freeze the loop forever.
/// Total-timeout-free by design — a prompt POST legitimately runs for many
/// minutes, so only the calls that can afford to give up are bounded.
pub(crate) async fn bounded_call<T>(
    what: &str,
    timeout_ms: u64,
    fut: impl std::future::Future<Output = crate::error::Result<T>>,
) -> Option<crate::error::Result<T>> {
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), fut).await {
        Ok(result) => Some(result),
        Err(_) => {
            tracing::warn!("{} timed out after {}ms — retrying next tick", what, timeout_ms);
            None
        }
    }
}

/// A Feishu message as delivered into the bridge, already parsed by the
/// platform layer into its prompt-relevant parts. Carries the reply linkage
/// (`parent_id`) and any downloaded images so the bridge can build Quoted
/// Context and Image Attachments.
pub struct IncomingMessage {
    pub message_id: String,
    pub chat_id: String,
    pub chat_type: String,
    pub thread_id: Option<String>,
    /// The message this one replies to, if any (Feishu `parent_id`).
    pub parent_id: Option<String>,
    /// Extracted plain text (mention placeholders already replaced).
    pub text: String,
    /// Images embedded in this message, already downloaded by the platform.
    pub images: Vec<ImageAttachment>,
    /// The sender's open_id (used for group completion notices).
    pub requester_open_id: Option<String>,
}

/// The bridge-facing sink the Feishu platform delivers events into. Abstracted
/// so `feishu::ws` depends on this small interface rather than the concrete
/// `App` type — the mirror of how `Platform`/`Backend` abstract the other
/// direction. Implemented by [`App`].
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn handle_message(&self, msg: IncomingMessage);

    async fn handle_card_action(&self, value: serde_json::Value) -> Option<handler::CardActionResult>;
}
