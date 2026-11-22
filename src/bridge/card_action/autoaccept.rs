//! The `/autoaccept` toggle card's click.

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::{App, CardActionResult, thread_key_from_value};

/// Switch the session's auto-accept flag and refresh the toggle card.
///
/// Persist BEFORE approving (#892): a sweep landing in the approval window must
/// already read the flag as on, or it surfaces a card (and companion) for the
/// over-budget request this click is about to approve. A persist that did not
/// take effect approves nothing.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let core = &app.core;
        let on = value.get("value").and_then(|v| v.as_str()) == Some("on");
        let thread_key = thread_key_from_value(value);
        let Some((settings, pending_epoch)) = core
            .sessions_handle()
            .settings_and_pending_epoch(&thread_key)
            .await
        else {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!(
                    "{}还没有会话",
                    crate::bridge::display::feishu_side_label(&thread_key)
                )),
            });
        };
        // Only a real session has requests to approve; a Pending Session has
        // none yet (ADR-0041).
        let active = settings
            .session_id
            .clone()
            .map(|id| (id, settings.directory.clone()));
        // Field-only, with the pending's identity (issue #910): a snapshot
        // write could restore a stale model, and a `/new` during the sweep must
        // not take the flag meant for the displaced pending.
        match core
            .sessions_handle()
            .set_auto_accept(
                &thread_key,
                settings.session_id.as_deref(),
                pending_epoch.unwrap_or_default(),
                on,
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("会话状态已变化（可能刚切换/创建），这次操作没有生效。".to_string()),
                });
            }
            Err(err) => {
                tracing::warn!("autoaccept card: persist failed: {}", err);
                return Some(CardActionResult {
                    card: None,
                    toast: Some("自动授权状态保存失败，请重试。".to_string()),
                });
            }
        }
        let mut approved = Vec::new();
        if on && let Some((id, directory)) = &active {
            approved = core.approve_pending_for_session(id, directory).await;
        }
        // The same residue the permission card's toggle and `/autoaccept on`
        // leave: ONE mode receipt and the approved blocks dismissed. Without it
        // the sweep would resolve them as `⏱ 已由其他客户端处理` — a lie, cola
        // itself approved them.
        if let Some((id, _)) = &active
            && !approved.is_empty()
        {
            crate::bridge::request::delivery::resolve_blocks(
                &core.permission,
                &core.cards_handle(),
                &core.requests_handle(),
                &Some(id.clone()),
                id,
                crate::bridge::request::delivery::Origin::Command,
                &approved,
                crate::bridge::request::delivery::Residue::Single(
                    crate::bridge::request::kind::AUTOACCEPT_RECEIPT,
                ),
            )
            .await;
        }
        let current_on = core
            .session_settings(&thread_key)
            .await
            .map(|s| s.auto_accept)
            .unwrap_or(false);
        Some(CardActionResult {
            card: Some(crate::feishu::card::picker::build_autoaccept_card(
                &thread_key,
                current_on,
            )),
            toast: Some(if on {
                "已开启自动审批".to_string()
            } else {
                "已关闭自动审批".to_string()
            }),
        })
    })
}
