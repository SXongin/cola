//! The `/sub` child-session picker's clicks (spec #344; ticket #1122).
//!
//! `action: "sub"` carries two read-only ops, parsed by the body: `search`
//! rebuilds the card with the typed keyword at page 1 (dropping the
//! session-list cache so a just-created child shows up) and `page` flips to the
//! payload's page. Both carry the active keyword/page, so the filtered window
//! round-trips (ADR-0052). One registry entry, as the dispatch had one arm.

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::core::SharedCore;
use crate::bridge::handler::{App, CardActionResult, thread_key_from_value};
use crate::config::ThreadKey;

/// The `/sub` child-session card's callback. Read-only: an op the card does
/// not render stays unanswered.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { app.handle_sub_card_action(&app.core, value).await })
}

impl App {
    /// Handle a `/sub` child-session card button (spec #344): `op: "search"`
    /// rebuilds the card with the typed keyword at page 1 (and drops the
    /// session-list cache so a just-created child shows up); `op: "page"` flips
    /// to the payload's page. Both carry the active keyword/page, so the
    /// filtered window round-trips (ADR-0052). Read-only: these are the only
    /// two ops the card has.
    async fn handle_sub_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // The card's stateless filter (ADR-0052): a missing/garbage page reads
        // as 1; the builder clamps an out-of-range page to the last page.
        let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        match op {
            "search" => {
                core.invalidate_session_list_cache().await;
                Some(CardActionResult {
                    card: Some(self.build_child_card_for(core, &thread_key, &keyword, 1).await),
                    toast: None,
                })
            }
            "page" => Some(CardActionResult {
                card: Some(self.build_child_card_for(core, &thread_key, &keyword, page).await),
                toast: None,
            }),
            _ => None,
        }
    }

    /// Rebuild the `/sub` child-session card for a thread, narrowed by
    /// `keyword` and paged to `page` (ADR-0052). One status read per rendered
    /// row, gathered by [`crate::feishu::card::command::child_card_data`].
    async fn build_child_card_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        keyword: &str,
        page: usize,
    ) -> serde_json::Value {
        let (children, statuses) =
            crate::feishu::card::command::child_card_data(&core.command_handles(), thread_key, keyword, page)
                .await;
        crate::feishu::card::session::build_child_card(
            thread_key,
            &children,
            &statuses,
            keyword,
            page,
            chrono::Utc::now().timestamp_millis(),
        )
    }
}
