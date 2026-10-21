pub mod access;
pub(crate) mod attach;
pub mod card_handles;
pub(crate) mod chain;
pub mod command;
pub mod core;
pub mod discovery;
pub mod display;
pub mod external;
pub mod failure_latch;
pub mod handler;
pub(crate) mod handles;
pub mod message_pins;
pub mod poll;
pub mod pollers;
pub mod question;
pub mod reminder;
pub(crate) mod repo_diff;
pub mod request;
pub(crate) mod runtime;
pub mod session;
pub(crate) mod sidecar;
pub mod snapshot;
pub mod snapshot_claims;
pub mod span;
pub mod surfaces;
pub(crate) mod tasks;
pub(crate) mod terminal;
pub(crate) mod terminal_record;
pub mod topic;
pub mod turn;
pub(crate) mod turn_diff;
pub(crate) mod viewer;
pub(crate) mod worktree;

#[cfg(test)]
mod attach_live;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
pub(crate) mod tests;

/// The bridge module's deliberate entry point: the coordinator every caller
/// (main, tests) constructs and drives through [`EventSink`].
pub use handler::App;

use crate::feishu::client::{FileAttachment, ImageAttachment};
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

/// Why an inbound attachment was not attached to a prompt (ADR-0076's inbound
/// amendment): the body carries the reason, so nothing is dropped silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmitReason {
    /// The server would not read the media type as model content (an
    /// unsupported mime, a folder entry, a non-four image format).
    Unsupported,
    /// The payload is past the server's 20MB attachment cap (decoded).
    OverSize,
    /// The bytes could not be downloaded from Feishu.
    DownloadFailed,
}

impl OmitReason {
    /// The Chinese label the prompt body's note and the warning reply read.
    pub fn label(self) -> &'static str {
        match self {
            OmitReason::Unsupported => "类型不支持",
            OmitReason::OverSize => "超过 20MB",
            OmitReason::DownloadFailed => "下载失败",
        }
    }
}

/// An attachment reference the platform layer extracted but did not turn into a
/// payload: a folder entry (unsupported), or a download that failed. `name` is
/// `None` for an image (it has no name). The bridge reports it instead of
/// dropping it silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmittedAttachment {
    pub name: Option<String>,
    pub reason: OmitReason,
}

/// A Feishu message as delivered into the bridge, already parsed by the
/// platform layer into its prompt-relevant parts. Carries the reply linkage
/// (`parent_id`) and any downloaded attachments so the bridge can build Quoted
/// Context and Prompt Attachments.
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
    /// Files attached to this message (a `file` message, or a rich-text
    /// message's top-level `files[]`), already downloaded by the platform.
    pub files: Vec<FileAttachment>,
    /// Attachment references the platform did not turn into a payload (a folder
    /// entry, a failed download), reported as `未附带：…`.
    pub omitted: Vec<OmittedAttachment>,
    /// The sender's open_id (used for group completion notices).
    pub requester_open_id: Option<String>,
}

/// The bridge-facing sink the Feishu platform delivers events into. Abstracted
/// so `feishu::ws` depends on this small interface rather than the concrete
/// `App` type — the mirror of how `Platform`/`Backend` abstract the other
/// direction. Implemented by [`App`].
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Resolve the message's Principal (ADR-0035) **before its attachments are
    /// downloaded**, so a denied sender's file is never fetched and buffered.
    /// Returns `true` when the message is admitted — the platform then
    /// downloads its attachments and delivers it; `false` when the gate already
    /// answered it (a refusal, a Claim acknowledgement), so nothing further is
    /// delivered. The default admits everything: a sink with no access policy
    /// (the test sinks) never blocks a download, and [`crate::bridge::App`]
    /// overrides it with the real gate.
    async fn admit(&self, _msg: &IncomingMessage) -> bool {
        true
    }

    async fn handle_message(&self, msg: IncomingMessage);

    async fn handle_card_action(&self, value: serde_json::Value) -> Option<handler::CardActionResult>;
}
