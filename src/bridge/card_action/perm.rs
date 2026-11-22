//! The `perm` click's adapter: the permission flow owns the body.

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::App;

/// A permission card click. The body — the reply, the double-click guard, the
/// card patch — stays in [`crate::bridge::request::flow`] (ADR-0102): the seam
/// only carries the click to it, so the dispatch and the flow cannot drift. The
/// flow owns its own deadline handling; the seam does not re-wrap it.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        app.permission
            .handle_card_action(&app.flow_handles(), value)
            .await
    })
}
