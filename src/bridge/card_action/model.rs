//! The `/model` picker card's navigation and pick clicks (spec #930).

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::core::SharedCore;
use crate::bridge::handler::{App, CardActionResult, thread_key_from_value};
use crate::bridge::handles::{ModelPickDecision, PickReply, PickTarget, classify_pick};
use crate::config::ThreadKey;

/// Handle a `/model` picker-card button. Two levels (the provider → model
/// flow): a `level: "provider"` button either opens a provider's model list or
/// (`value == PICKER_BACK_TO_PROVIDERS`) returns to the full provider list; a
/// `level: "model"` button records the per-session override.
///
/// The navigation read is bounded by the ACK budget anchored at the callback's
/// **true start**, not at this verb: the Host gate, span setup and the
/// dispatch's pre-dispatch reads already spent part of the budget. The
/// dispatch's `deadline` is the store budget the mirror writes settle on
/// (ADR-0089), so the verb starts no second clock.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    deadline: Instant,
    callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move {
        let core = &app.core;
        let surface_deadline = callback_started + crate::feishu::ws::CARD_ACK_BUDGET;
        let thread_key = thread_key_from_value(value);
        let picked = value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // A scope states what its pick does (`PickKind`): navigation asks the
        // client for the next page; every other scope is a concrete pick. The
        // bridge never infers the action from a scope id (OpenCode's and
        // Codex's provider scopes are both named `provider` but do opposite
        // things — #910).
        let level = value.get("level").and_then(|v| v.as_str()).unwrap_or("");
        if level == crate::feishu::card::picker::PICKER_LEVEL_PROVIDER {
            // A Codex callback from a pre-#931 provider card is stale: its
            // one-page surface has no provider navigation and must not render a
            // back button that loops to the same model page. OpenCode continues
            // to expose its real provider→model navigation tree.
            let back = core.client.has_provider_navigation()
                && picked != crate::feishu::card::picker::PICKER_BACK_TO_PROVIDERS;
            let (scope, selected) = if back {
                (Some("provider"), Some(picked.as_str()))
            } else {
                (None, None)
            };
            let handles = core.command_handles();
            let surface = crate::feishu::card::command::model_surface(
                &handles,
                &thread_key,
                scope,
                selected,
                Some(surface_deadline),
            )
            .await;
            let cards = crate::feishu::card::picker::build_model_surface_cards(&thread_key, &surface, back);
            return Some(picker_pages_result(core, &thread_key, cards).await);
        }

        Some(pick_model_ref(core, &thread_key, &picked, deadline).await)
    })
}

/// A concrete **model-only** pick: record the override (and on a durable
/// generation switch the session's model), carrying or clearing the `/think`
/// variant (ADR-0020), same as the `/model` text form. The named provider must
/// name the current channel (or the neutral bucket, or be unknown); a different
/// one is refused before any request (spec #930).
async fn pick_model_ref(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    picked: &str,
    deadline: Instant,
) -> CardActionResult {
    let Some(parsed) = crate::backend::parsing::parse_model(picked) else {
        return CardActionResult {
            card: None,
            toast: Some("模型格式应为 <provider>/<model>".to_string()),
        };
    };
    let (target, pending_target, provider_fact) = match core
        .sessions_handle()
        .model_pick_decision(&core.client, thread_key, &parsed.provider_id)
        .await
    {
        ModelPickDecision::Refuse(text) => {
            return CardActionResult {
                card: None,
                toast: Some(text),
            };
        }
        ModelPickDecision::Record {
            target,
            provider_fact,
        } => {
            let pending = matches!(target, PickTarget::Pending(_));
            (target, pending, provider_fact)
        }
    };
    // The toast must say where a pick applies: a Pending Session's model rides
    // `thread/start`, while a live thread's is the next new turn (spec #922).
    let outcome = match target {
        PickTarget::Live(session_id) => {
            core.pick_model_expecting(
                thread_key,
                Some(&session_id),
                provider_fact,
                picked,
                Some(deadline),
            )
            .await
        }
        PickTarget::Pending(epoch) => {
            core.sessions_handle()
                .pick_pending_model(
                    &core.client,
                    thread_key,
                    epoch,
                    provider_fact,
                    picked,
                    Some(deadline),
                )
                .await
        }
    };
    let cleared_variant = match classify_pick(
        outcome,
        "模型",
        crate::bridge::display::feishu_side_label(thread_key),
    ) {
        PickReply::Applied { cleared_variant } => cleared_variant,
        PickReply::Failed(text) => {
            return CardActionResult {
                card: None,
                toast: Some(text),
            };
        }
    };
    let extra = cleared_variant
        .map(|v| format!("（已清除思考等级 `{v}`：新模型不支持）"))
        .unwrap_or_default();
    // A client with a thread-level provider axis (Codex) records a one-shot
    // next-turn pending, not an applied switch (spec #922).
    let toast = if pending_target {
        format!("Model: {picked}（将在会话创建时生效）{extra}")
    } else if core.client.owns_next_turn_pending() {
        format!("Model: {picked}（已记录：将在下一条新消息生效）{extra}")
    } else {
        format!("Model: {picked}（下一条消息开始生效）{extra}")
    };
    CardActionResult {
        card: None,
        toast: Some(toast),
    }
}

/// Turn a page of picker cards into a card-action result: the FIRST card
/// replaces the clicked one in place (via the ack); the remaining pages are
/// posted into the chat so the full picker stays visible.
async fn picker_pages_result(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    cards: Vec<serde_json::Value>,
) -> CardActionResult {
    let (first, rest) = cards.split_first().map_or((None, &[][..]), |(f, r)| (Some(f), r));
    for card in rest {
        let _ = core.feishu.send_card("chat_id", &thread_key.chat_id, card).await;
    }
    CardActionResult {
        card: first.cloned(),
        toast: None,
    }
}
