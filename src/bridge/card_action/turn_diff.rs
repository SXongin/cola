//! The Turn card's 「🔍 查看完整变化」 click (spec #797, ticket #801).

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::App;

/// An action-less click: the payload carries `op` and no `action`, which is why
/// the registry selects it by `op` alone. The body stays in
/// [`App::handle_turn_diff_action`] until #1124 moves it; the dispatch's
/// [`Gate::Diffs`](crate::bridge::card_action::Gate::Diffs) already decides
/// whether the surface exists.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(app.handle_turn_diff_action(value))
}
