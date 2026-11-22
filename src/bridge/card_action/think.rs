//! The `/think` picker's pick and clear clicks (ADR-0020).

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::handler::{App, CardActionResult, thread_key_from_value};
use crate::bridge::handles::{PickReply, classify_pick};

/// A `/think` option pick: record the chosen variant and refresh the card so
/// the current selection updates. Best-effort validation: a pick is rejected
/// only when the effective model is resolvable AND positively lacks the variant
/// (ADR-0020); unknown models fall through to the server's
/// `VariantUnavailableError`.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { pick(app, value, false).await })
}

/// The dedicated `think_clear` control. Clearing is carried by the ACTION tag,
/// never a value word — a variant literally named like a clear verb stays
/// selectable (ADR-0020), so `clear` is a dispatch flag, not a value predicate.
pub(crate) fn run_clear<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    _deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { pick(app, value, true).await })
}

async fn pick(app: &Arc<App>, value: &serde_json::Value, clear: bool) -> Option<CardActionResult> {
    let core = &app.core;
    let picked = value
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let thread_key = thread_key_from_value(value);
    let Some(settings) = core.session_settings(&thread_key).await else {
        return Some(CardActionResult {
            card: None,
            toast: Some(format!(
                "{}还没有会话",
                crate::bridge::display::feishu_side_label(&thread_key)
            )),
        });
    };
    if !clear
        && let Some((provider, model)) = core.effective_model(&settings).await
        && let Some(variants) = core.model_variants(&provider, &model).await
        && !variants.iter().any(|v| v == &picked)
    {
        return Some(CardActionResult {
            card: None,
            toast: Some(format!("当前模型 `{provider}/{model}` 不支持思考等级 `{picked}`")),
        });
    }
    match classify_pick(
        core.pick_think(&thread_key, if clear { None } else { Some(picked.clone()) })
            .await,
        "思考等级",
        crate::bridge::display::feishu_side_label(&thread_key),
    ) {
        PickReply::Applied { .. } => {}
        PickReply::Failed(text) => {
            return Some(CardActionResult {
                card: None,
                toast: Some(text),
            });
        }
    }
    let (card, _error) = crate::feishu::card::command::think_card(&core.command_handles(), &thread_key).await;
    Some(CardActionResult {
        card,
        toast: Some(if clear {
            "已清除思考等级（回到模型默认）".to_string()
        } else {
            format!("Thinking: {}（下一条消息开始生效）", picked)
        }),
    })
}
