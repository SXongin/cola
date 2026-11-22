//! The `/mcp` card's `op: "reopen"` click (ADR-0103).
//!
//! An adapter only: the body — the capability check, the bounded listing read
//! and the toasts — stays in [`crate::bridge::mcp`], which the dispatch used to
//! call directly. The click's deadline is the seam's, so the list read spends
//! the same budget it always did.

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::App;

/// Forward the click to [`crate::bridge::mcp::handle_action`].
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { crate::bridge::mcp::handle_action(&app.command_handles(), value, deadline).await })
}
