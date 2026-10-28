//! `/model`'s provider+model pair (issue #910), in the **record-first** shape.
//!
//! A confirmed provider change is *recorded* in the session mirror; the next
//! prompt's pre-flight (`Turn::attempt`) applies it — release → resume with
//! `modelProvider` + `model` → `thread/read` read-back — before submitting, and
//! stops the prompt by name when it cannot. Nothing is applied from a card
//! callback.
//!
//! - a pick whose provider is the thread's own is an **ordinary model pick**: no
//!   `thread/resume`, no `thread/unsubscribe`;
//! - a different provider is recorded, then applied by the next prompt;
//! - a Pending Session takes the pair at `thread/start`.

use crate::backend::types::{ChangeScope, ModelPickRoute};
use crate::bridge::session::PendingEntry;
use crate::bridge::test_support::*;

const PROVIDER: &str = "deepseek";
const MODEL: &str = "deepseek-v4-pro";

fn key() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

fn entry() -> crate::config::SessionEntry {
    crate::config::SessionEntry::new(key(), "ses_test", "/work/cola")
}

/// The callback a paired row sends: the bare model id plus the provider it
/// pairs with (`ModelOption.provider` in the surface).
fn paired_pick(provider: &str, model: &str) -> serde_json::Value {
    serde_json::json!({
        "action": "model",
        "level": "model",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "provider": provider,
        "value": model,
    })
}

/// The value of the button tagged `action` on `card`.
fn button_value(card: &serde_json::Value, action: &str) -> serde_json::Value {
    card_buttons(card)
        .into_iter()
        .find(|button| button["value"]["action"] == action)
        .unwrap_or_else(|| panic!("no `{action}` button: {card}"))["value"]
        .clone()
}

/// Every card the platform has seen (a turn's error card is a fresh reply card
/// first, then patched), joined for a copy assertion.
async fn all_cards(platform: &RecordingPlatform) -> String {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyCard { card, .. }
            | PlatformCall::SendCard { card, .. }
            | PlatformCall::UpdateMessage { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn prompt(app: &std::sync::Arc<crate::bridge::handler::App>, message_id: &str, text: &str) {
    let app = app.clone();
    let message_id = message_id.to_string();
    let text = text.to_string();
    tokio::spawn(async move {
        app.handle_message(incoming(
            message_id,
            "chat_1".into(),
            "p2p".into(),
            None,
            text,
            None,
        ))
        .await;
    });
}

/// Issue #910, item 2 (and the kept creation path): a Pending Session records
/// the **pair**, and the first message carries both halves into one
/// `thread/start`.
#[tokio::test]
async fn a_pending_sessions_pair_rides_thread_creation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // A Pending Session's pair only exists on a client with a provider axis
    // (Codex's shape); the mock defaults to OpenCode's, which has none.
    backend.with_thread_provider(true);
    let picks = backend.created_session_picks.clone();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("paired pick");
    assert!(step.card.is_none(), "a pending pick is toast-only");
    assert!(
        step.toast.as_deref().unwrap_or_default().contains("新线程"),
        "the toast says where it applies: {step:?}",
        step = step.toast
    );
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pending survives");
    assert_eq!(pending.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
    assert_eq!(pending.provider.as_deref(), Some(PROVIDER));

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert_eq!(
        picks.lock().await.as_slice(),
        &[(Some(PROVIDER.to_string()), Some(MODEL.to_string()))],
        "one thread/start carries the pair"
    );
    assert_eq!(
        prompt_models.lock().await.as_slice(),
        &[Some("deepseek/deepseek-v4-pro".to_string())],
        "the turn carries the model only — a provider is never a turn field"
    );
}

/// Issue #910, item 3: the **same** provider with another model is an ordinary
/// pick even with the gate open — no thread-level request, no confirmation.
#[tokio::test]
async fn a_same_provider_pick_is_an_ordinary_model_pick() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::PerTurn)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let applies = backend.thread_model_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick("openai", "gpt-5.1"))
        .await
        .expect("same-provider pick");
    assert!(step.card.is_none(), "no confirmation for a model pick");
    assert!(
        step.toast.as_deref().unwrap_or_default().starts_with("Model:"),
        "the ordinary model toast: {step:?}",
        step = step.toast
    );
    assert!(
        applies.lock().await.is_empty(),
        "a same-provider pick sends nothing thread-level"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1")
    );
}

/// Issue #910, item 3 (gate closed): a provider pick without the opt-in only
/// says where it *can* happen, and records nothing.
#[tokio::test]
async fn a_provider_pick_with_the_gate_off_only_toasts() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_model_pick_route(ModelPickRoute::ThreadModel);
    let applies = backend.thread_model_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    assert!(step.card.is_none(), "no confirmation when the door is shut");
    assert_eq!(step.toast.as_deref(), Some("provider 只能在新线程切换"));
    assert!(applies.lock().await.is_empty());
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the mirror is untouched"
    );
}

/// The record-first shape: the confirmation **records** the pair and answers
/// synchronously; the next prompt's pre-flight applies it before submitting.
#[tokio::test]
async fn a_confirmed_switch_is_recorded_then_applied_by_the_next_prompt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let applies = backend.thread_model_calls.clone();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    assert!(card.to_string().contains("deepseek/deepseek-v4-pro"));
    assert_eq!(applies.lock().await.len(), 0, "the pick applied nothing");

    let step = app
        .host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    let ack = step.card.expect("the recorded card");
    assert!(
        ack.to_string().contains("已记录") && ack.to_string().contains("下一条消息"),
        "the click records the pair for the next message: {ack}"
    );
    assert!(
        applies.lock().await.is_empty(),
        "a confirmation never touches the thread"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the pair is the recorded mirror"
    );

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert_eq!(
        applies.lock().await.as_slice(),
        &[("ses_test".to_string(), "deepseek/deepseek-v4-pro".to_string())],
        "the prompt's pre-flight applied the recorded pair once"
    );
    assert_eq!(
        prompt_models.lock().await.last().cloned().flatten().as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "and the prompt was submitted with it"
    );
}

/// Item 3 of the owner's bullets: a pick made **while a turn is running** needs
/// no special case — it is recorded, and the next prompt applies it.
#[tokio::test]
async fn a_pick_during_a_turn_is_applied_by_the_next_prompt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let applies = backend.thread_model_calls.clone();
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_models = backend.prompt_models.clone();
    let gate = backend.hold_prompts();
    let (app, _platform) = build_app(cfg, backend).await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    seed_entry(&app, entry()).await;

    prompt(&app, "msg_prompt_1", "first");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_calls.lock().await.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "turn 1 never started");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick during a turn");
    let card = step.card.expect("a confirmation card");
    let step = app
        .host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    assert!(
        step.card
            .as_ref()
            .is_some_and(|card| card.to_string().contains("已记录")),
        "a mid-turn confirmation records, it does not switch"
    );
    assert!(
        applies.lock().await.is_empty(),
        "the running turn is not disturbed"
    );

    // Turn 1 ends; the recorded pair is applied by the next prompt.
    gate.add_permits(1);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.inflight.lock().await.contains("ses_test") {
        assert!(tokio::time::Instant::now() < deadline, "turn 1 never finished");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    prompt(&app, "msg_prompt_2", "second");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_calls.lock().await.len() < 2 {
        assert!(tokio::time::Instant::now() < deadline, "turn 2 never submitted");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        applies.lock().await.as_slice(),
        &[("ses_test".to_string(), "deepseek/deepseek-v4-pro".to_string())],
        "the recorded pair was applied by the next prompt"
    );
    assert_eq!(
        prompt_models.lock().await.last().cloned().flatten().as_deref(),
        Some("deepseek/deepseek-v4-pro"),
    );
}

/// A recorded pair that cannot apply (another client, or another cola instance
/// sharing `CODEX_HOME`, still holds the thread) **stops** the prompt and is
/// explained by name; the record stays for a retry.
#[tokio::test]
async fn a_recorded_switch_that_cannot_apply_stops_the_prompt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .fail_provider_switch(
            "thread/resume 之后这条线程仍是 `openai/gpt-5.1-codex`：可能有另一个客户端（Codex TUI / \
             VS Code）或另一个共用同一个 `CODEX_HOME` 的 cola 实例仍持有它；请关闭它，或改用 \
             `thread/fork` 带上 provider/model",
        );
    let applies = backend.thread_model_calls.clone();
    let prompt_calls = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");

    // The turn ends on its error card: the prompt is never submitted.
    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert!(
        !applies.lock().await.is_empty(),
        "the pre-flight attempted the switch"
    );
    let text = all_cards(&platform).await;
    assert!(text.contains("TUI"), "the copy names the other client: {text}");
    assert!(text.contains("CODEX_HOME"), "and the second cola: {text}");
    assert!(text.contains("thread/fork"), "and the escape hatch: {text}");
    assert!(
        prompt_calls.lock().await.is_empty(),
        "the prompt is never sent on the old provider"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the recorded intent stays for a retry"
    );
}

/// The gate is enforced again at pre-flight: a pair recorded while the gate was
/// open must not switch after the config was turned off.
#[tokio::test]
async fn a_gate_turned_off_after_recording_stops_the_prompt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let gate = backend.provider_scope_handle();
    let applies = backend.thread_model_calls.clone();
    let prompt_calls = backend.prompt_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    // The operator turns the switch off before the next message.
    *gate.lock().expect("provider change scope") = ChangeScope::NewThread;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while applies.lock().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the switch was never attempted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        prompt_calls.lock().await.is_empty(),
        "the prompt is not sent on the old provider"
    );
}

/// Issue #910, item 9 / record-first: a record that cannot be written is a
/// failure, never 「已记录」.
#[tokio::test]
async fn a_failed_record_does_not_report_success() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    // The sidecar path becomes a directory: the record write fails.
    std::fs::remove_file(&sessions).expect("the seeded store file");
    std::fs::create_dir(&sessions).expect("replace the store with a directory");

    let step = app
        .host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    let text = format!("{:?}{:?}", step.card, step.toast);
    assert!(
        !text.contains("已记录：下一条消息"),
        "a failed record must not claim it was recorded: {text}"
    );
    assert!(text.contains("保存") || text.contains("失败"), "{text}");
}

/// A replayed confirmation is idempotent: the same pair, written twice, leaves
/// one record and never touches the thread.
#[tokio::test]
async fn a_replayed_confirmation_is_idempotent() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let applies = backend.thread_model_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    let confirm = button_value(&card, "model_provider_confirm");
    app.host_action_on_message(confirm.clone(), "om_confirm")
        .await
        .expect("first confirm");
    app.host_action_on_message(confirm, "om_confirm")
        .await
        .expect("replayed confirm");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("deepseek/deepseek-v4-pro")
    );
    assert!(
        applies.lock().await.is_empty(),
        "recording never touches the thread"
    );
}

/// Issue #910, item 7: the confirmation re-reads the gate — a card that outlived
/// the config change records nothing.
#[tokio::test]
async fn the_gate_is_rechecked_at_confirmation_time() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let gate = backend.provider_scope_handle();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    *gate.lock().expect("provider change scope") = ChangeScope::NewThread;

    let step = app
        .host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    assert_eq!(step.toast.as_deref(), Some("provider 只能在新线程切换"));
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "nothing was recorded"
    );
}

/// Issue #910, item 7 (stale card): the target is resolved at confirmation time;
/// a card that names another thread records nothing.
#[tokio::test]
async fn a_stale_provider_confirmation_does_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let step = app
        .host_action(serde_json::json!({
            "action": "model_provider_confirm",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "session_id": "ses_old",
            "provider": PROVIDER,
            "value": MODEL,
        }))
        .await
        .expect("stale confirm");
    assert!(step.card.is_none(), "a stale card gets no new card");
    assert!(
        step.toast.as_deref().unwrap_or_default().contains("过期"),
        "the click says the card is stale: {step:?}",
        step = step.toast
    );
    assert!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .is_none(),
        "nothing was recorded"
    );
}

/// Issue #910, item 7 (cancel): 取消 is a no-op.
#[tokio::test]
async fn cancelling_the_provider_confirmation_changes_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let step = app
        .host_action(serde_json::json!({
            "action": "model_provider_cancel",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "",
        }))
        .await
        .expect("cancel ack");
    assert!(step.card.is_none(), "the confirm card is left as it was");
    assert!(step.toast.is_none(), "not even a toast: nothing happened");
}

/// The recorded variant clear is what the next prompt reads (issue #910): a
/// `/think` the new model does not declare is cleared by the **pre-flight** —
/// the confirmation records from a store write only (it must ack inside the
/// card budget) — and the prompt submits no variant.
#[tokio::test]
async fn a_recorded_switch_clears_an_unsupported_variant() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    backend.with_models(vec![crate::backend::types::ProviderModels {
        provider: PROVIDER.into(),
        models: vec![crate::backend::types::ModelOption {
            id: MODEL.into(),
            variants: vec!["low".into()],
            ..Default::default()
        }],
    }]);
    let prompt_variants = backend.prompt_variants.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .variant,
        Some("high".to_string()),
        "the confirmation records only: the clear waits for the pre-flight"
    );

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert_eq!(
        prompt_variants.lock().await.last().cloned().flatten(),
        None,
        "the prompt submits no cleared variant"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .variant,
        None,
        "the pre-flight recorded the clear before switching"
    );
}

/// Issue #910, item 6: a Pending Session's pair is one store write.
#[tokio::test]
async fn a_pending_pair_is_saved_in_one_write() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_thread_provider(true);
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    app.host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("paired pick");

    let reloaded =
        crate::bridge::session::SessionStore::for_client(sessions.clone(), crate::config::DEFAULT_CLIENT_ID)
            .expect("the store reloads");
    let pending = reloaded.pending_for(&key()).expect("the pending is on disk");
    assert_eq!(pending.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
    assert_eq!(pending.provider.as_deref(), Some(PROVIDER));
}

/// Finding (record-first): a session that materialises while the pending's
/// catalog lookup runs is a failure, never a silent claim.
#[tokio::test]
async fn a_pending_that_materialises_is_not_reported_saved() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_model_surface();
    let entered = backend.model_surface_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut live = entry();
    live.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, live).await;
    let mut pending = PendingEntry::new(key(), "/work/cola");
    pending.variant = Some("high".into());
    seed_pending(&app, pending).await;

    let app_for_pick = app.clone();
    let pick = tokio::spawn(async move { app_for_pick.host_action(paired_pick(PROVIDER, MODEL)).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(tokio::time::Instant::now() < deadline, "the lookup never started");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    app.sessions
        .lock()
        .await
        .clear_pending(&key())
        .expect("the pending materialises");
    gate.add_permits(1);

    let step = pick.await.expect("the pick joins").expect("a pick result");
    let toast = step.toast.unwrap_or_default();
    assert!(
        toast.contains("保存失败"),
        "a vanished pending is an honest failure: {toast}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the pair never falls through to the live entry"
    );
}

/// Finding: the creation claim makes a pair write during materialisation an
/// honest failure, never a pair the creation cannot apply.
#[tokio::test]
async fn a_pair_write_during_materialisation_fails_honestly() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_create_session();
    let entered = backend.create_session_gate_entered.clone();
    let picks = backend.created_session_picks.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut live = entry();
    live.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, live).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the creation never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("paired pick during creation");
    let toast = step.toast.unwrap_or_default();
    assert!(
        toast.contains("正在创建") || toast.contains("状态已变化"),
        "the click reports the creation honestly: {toast}"
    );
    assert!(
        !toast.contains("新线程创建时生效"),
        "it must not claim the pair will apply at creation: {toast}"
    );

    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        picks.lock().await.as_slice(),
        &[(None, Some("m".to_string()))],
        "the thread was created without the provider the click could not save"
    );
}

/// Final round, item 4: a replacement pending declared while a creation is in
/// flight survives that creation's activation.
#[tokio::test]
async fn a_replacement_pending_survives_the_older_creation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_create_session();
    let entered = backend.create_session_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/first")).await;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the creation never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let mut replacement = PendingEntry::new(key(), "/work/second");
    replacement.model = Some("deepseek/deepseek-v4-pro".into());
    replacement.provider = Some(PROVIDER.into());
    app.sessions
        .lock()
        .await
        .set_pending(replacement)
        .expect("the replacement is declared");

    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the older creation must not resolve the replacement");
    assert_eq!(pending.directory, "/work/second");
    assert_eq!(pending.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
}

/// Extra finding: a `/agent` pick updates only its own field — it must not
/// restore a stale model/variant snapshot over a recorded switch.
#[tokio::test]
async fn an_agent_pick_keeps_the_recorded_pair() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("deepseek/deepseek-v4-pro".into());
    entry.variant = Some("low".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/agent build",
        key(),
        "msg_agent",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(stored.agent.as_deref(), Some("build"));
    assert_eq!(
        stored.model.as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the recorded pair is untouched"
    );
    assert_eq!(stored.variant.as_deref(), Some("low"));
}

/// Final round, item 1: a thread forgotten while its session is being created
/// must not be re-mapped by the finished creation.
#[tokio::test]
async fn a_forget_during_a_parked_creation_is_not_undone() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_create_session();
    let entered = backend.create_session_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the creation never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The user forgets the thread while `thread/start` is in flight.
    send_command(&app, "/switch forget", "msg_forget").await;
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "the forget removed the pending"
    );

    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let store = app.sessions.lock().await;
    assert!(
        store.get_active(&key()).is_none(),
        "the finished creation must not re-map a forgotten thread"
    );
    assert!(
        store.entry_for_session("ses_test").is_none(),
        "and the created session stays unmapped"
    );
}

/// Final round, item 1: a newer selection during a parked creation wins; the
/// stale creation does not overwrite it.
#[tokio::test]
async fn a_newer_selection_during_a_parked_creation_wins() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_create_session();
    let entered = backend.create_session_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the creation never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The user selects another session while `thread/start` is in flight.
    app.activate_session(crate::config::SessionEntry::new(
        key(),
        "ses_new",
        "/work/elsewhere",
    ))
    .await
    .expect("the newer selection activates");
    assert_eq!(
        app.sessions.lock().await.get_active(&key()).unwrap().session_id,
        "ses_new"
    );

    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let store = app.sessions.lock().await;
    assert_eq!(
        store.get_active(&key()).unwrap().session_id,
        "ses_new",
        "the stale creation must not overwrite the newer selection"
    );
    assert!(store.entry_for_session("ses_test").is_none());
}

/// Final round, item 3: the `/model <provider>/<model>` text form obeys the
/// card's rule — with the gate off it refuses **without recording**.
#[tokio::test]
async fn the_model_text_form_refuses_a_provider_change_with_the_gate_off() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_model_pick_route(ModelPickRoute::ThreadModel);
    let applies = backend.thread_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model deepseek/deepseek-v4-pro",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let text = platform.texts().await.join("\n");
    assert!(text.contains("只能在新线程切换"), "{text}");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the text form must not record the provider change"
    );
    assert!(
        !text.contains("Model:"),
        "no success reply for a refused provider change: {text}"
    );
    assert!(applies.lock().await.is_empty());
}

/// Final round, item 3: with the gate open the text form replies the same
/// confirmation card, and only the confirmation records.
#[tokio::test]
async fn the_model_text_form_confirms_a_provider_change() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model deepseek/deepseek-v4-pro",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|call| match call {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .expect("the confirmation card is the reply");
    assert!(
        card_buttons(&card)
            .iter()
            .any(|button| button["value"]["action"] == "model_provider_confirm"),
        "{card}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the text form itself records nothing"
    );

    app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the confirmation records the pair"
    );
}

/// Final round, item 4: a `/switch` between the confirmation's check and the
/// record makes the record a no-op instead of landing on the new session. The
/// record is a store write only, so the test parks it on the session's write
/// gate — its classification point — rather than on a catalog read.
#[tokio::test]
async fn a_remap_before_the_record_lands_writes_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    // Hold the session's serialization point so the confirm's write is parked
    // after its classification and before its compare-and-set.
    let gate = app.session_gates.gate("ses_test");
    let held = gate.enter().await;
    let confirm = tokio::spawn({
        let app = app.clone();
        async move {
            app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
                .await
        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while gate.waiting() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "the record never parked");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The chat switches while the record is parked.
    app.command_handles()
        .flow
        .unmap("ses_test")
        .await
        .expect("the old mapping is dropped");
    let mut other = crate::config::SessionEntry::new(key(), "ses_new", "/work/cola");
    other.model = Some("openai/gpt-5.1".into());
    app.activate_session(other)
        .await
        .expect("the new mapping activates");
    drop(held);

    let step = confirm.await.expect("the confirm joins").expect("a result");
    let text = format!("{:?}{:?}", step.card, step.toast);
    assert!(
        text.contains("状态已变化") || text.contains("过期"),
        "the record is refused honestly: {text}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1"),
        "the new session's mirror is untouched"
    );
}

/// Issue #910, item 2: the `Record` decision carries the session it classified,
/// so a `/switch` during the pick's own catalog read cannot record the model on
/// the new session (whose next prompt would then switch provider unconfirmed).
#[tokio::test]
async fn a_record_decision_is_bound_to_the_session_it_classified() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_models(vec![crate::backend::types::ProviderModels {
            provider: "openai".into(),
            models: vec![model_option("gpt-5.1", &["low"])],
        }])
        .with_model_pick_route(ModelPickRoute::PerTurn)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let gate = backend.hold_model_surface();
    let entered = backend.model_surface_gate_entered.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    // Same provider as the thread: a `Record`, whose variant clear reads the
    // catalog — the parking point between the classification and the write.
    let command = tokio::spawn({
        let app = app.clone();
        async move {
            send_command_in(
                &app,
                "/model openai/gpt-5.1",
                key(),
                "msg_model",
                crate::config::ConversationKind::P2p,
            )
            .await;
        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the catalog lookup never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The chat switches while the Record's catalog read is parked.
    app.command_handles()
        .flow
        .unmap("ses_test")
        .await
        .expect("the old mapping is dropped");
    let mut other = crate::config::SessionEntry::new(key(), "ses_new", "/work/cola");
    other.model = Some("openai/gpt-5.1-mini".into());
    app.activate_session(other)
        .await
        .expect("the new mapping activates");
    gate.add_permits(1);
    command.await.expect("the command joins");

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reply.contains("状态已变化") || reply.contains("过期"),
        "the Record is refused honestly: {reply}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-mini"),
        "the new session's mirror is untouched"
    );
}

/// Issue #910, item 1: a client without a thread-level provider axis (OpenCode)
/// records **no** provider on a Pending Session — the pair is the model ref
/// alone — so the creation body it sends is unchanged.
#[tokio::test]
async fn a_pending_pick_records_no_provider_without_the_axis() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // OpenCode's shape: the provider rides each prompt, not the thread.
    backend.with_thread_provider(false);
    let picks = backend.created_session_picks.clone();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    send_command_in(
        &app,
        "/model deepseek/deepseek-v4-pro",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pending survives");
    assert_eq!(pending.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
    assert_eq!(
        pending.provider, None,
        "a client without the axis records no thread-level provider"
    );

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    let picks = picks.lock().await.clone();
    assert_eq!(picks.len(), 1, "one creation: {picks:?}");
    assert_eq!(
        picks[0].0, None,
        "no thread-level provider reaches the creation input: {picks:?}"
    );
    // The model never rode the creation body on this client (main's shape):
    // it is applied to the created session and the first turn carries it.
    assert_eq!(
        prompt_models.lock().await.as_slice(),
        &[Some("deepseek/deepseek-v4-pro".to_string())],
        "the pick still reaches the first turn"
    );
}

/// Issue #910, item 3: a durable generation's session switch is a backend RPC,
/// so it runs **outside** the session gate — a stalled server must not wedge
/// this session's next prompt. The pick is parked inside `switch_session_model`
/// here, and the prompt still goes through.
#[tokio::test]
async fn a_model_pick_switches_the_session_outside_the_gate() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_session_selection(
            "ses_test",
            crate::backend::types::SessionSelection {
                model: Some(crate::backend::types::ModelInfo {
                    id: "gpt-5.1-codex".into(),
                    provider_id: "openai".into(),
                    variant: Some("high".into()),
                }),
                agent: None,
            },
        )
        .with_model_pick_route(ModelPickRoute::PerTurn);
    let gate = backend.hold_session_switch();
    let entered = backend.switch_model_gate_entered.clone();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    let pick = tokio::spawn({
        let app = app.clone();
        async move {
            send_command_in(
                &app,
                "/model openai/gpt-5.1",
                key(),
                "msg_model",
                crate::config::ConversationKind::P2p,
            )
            .await;
        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(tokio::time::Instant::now() < deadline, "the switch never started");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The pick is parked in its RPC; the session gate must still be free, so
    // the next prompt's pre-flight reaches `prompt` without waiting for it.
    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_models.lock().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the prompt was blocked by the pick's RPC"
        );
        assert!(
            !pick.is_finished(),
            "the pick was not parked when the prompt went through"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    gate.add_permits(1);
    pick.await.expect("the pick joins");
}

/// Issue #910, item 4: a confirmation acks from a store write only — it never
/// waits on a catalog read, whose slowest case would blow the card budget.
#[tokio::test]
async fn a_confirmation_acks_without_a_catalog_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let gate = backend.hold_model_surface();
    let entered = backend.model_surface_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    let step = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm"),
    )
    .await
    .expect("the confirmation acks inside the card budget")
    .expect("a result");
    assert_eq!(
        entered.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the confirmation read no catalog"
    );
    assert!(
        step.card
            .as_ref()
            .is_some_and(|card| card.to_string().contains("已记录")),
        "the recorded card is the ack"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the pair is the store write"
    );
    gate.add_permits(1);
}

/// Issue #910, item 5 (the allowed half): with the gate off but the cache
/// knowing the thread's provider, a **same-provider** pick is still an ordinary
/// model pick — the cache-only hedge refuses only what it cannot prove.
#[tokio::test]
async fn a_same_provider_pick_is_recorded_with_the_gate_off_when_the_cache_knows_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_thread_provider(true)
        .with_known_thread_provider(Some("openai"))
        .with_model_pick_route(ModelPickRoute::PerTurn)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model openai/gpt-5.1",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(reply.contains("Model: openai/gpt-5.1"), "{reply}");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1"),
        "the same-provider pick is recorded"
    );
}

/// Issue #910, item 2: a target classified as **live** is re-verified at the
/// write. If the chat became a Pending Session in between, the record refuses —
/// it must never become a model-only write into the new pending (which would
/// drop the provider the pick was classified with). And a decision taken on a
/// pending carries `Pending`, whose write records both halves.
#[tokio::test]
async fn a_live_session_that_became_pending_before_the_record_refuses() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_thread_provider(true)
        .with_known_thread_provider(Some(PROVIDER));
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let spec = format!("{PROVIDER}/{MODEL}");
    let decision = app
        .sessions_handle()
        .model_pick_decision(&app.client, &key(), PROVIDER, MODEL)
        .await;
    assert!(
        matches!(
            &decision,
            crate::bridge::handles::ModelPickDecision::Record {
                target: crate::bridge::handles::PickTarget::Live(id)
            } if id == "ses_test"
        ),
        "the live session is the classified target"
    );

    // The chat becomes a Pending Session while the record is on its way.
    app.command_handles()
        .flow
        .unmap("ses_test")
        .await
        .expect("the old mapping is dropped");
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    let outcome = app
        .sessions_handle()
        .pick_model_expecting(&app.client, &key(), Some("ses_test"), &spec)
        .await
        .expect("the write runs");
    assert_eq!(
        outcome,
        crate::bridge::handles::PickOutcome::StateChanged,
        "a live target that became pending is an honest refusal"
    );
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pending is there");
    assert_eq!(pending.model, None, "nothing was written");
    assert_eq!(pending.provider, None, "no half pair either");

    // A decision that sees the pending carries it, and the write the caller
    // performs for it records the pair.
    let decision = app
        .sessions_handle()
        .model_pick_decision(&app.client, &key(), PROVIDER, MODEL)
        .await;
    assert!(
        matches!(
            &decision,
            crate::bridge::handles::ModelPickDecision::Record {
                target: crate::bridge::handles::PickTarget::Pending
            }
        ),
        "the pending target is carried"
    );
    let outcome = app
        .sessions_handle()
        .pick_pending_pair(&app.client, &key(), PROVIDER, &spec)
        .await
        .expect("the pending write runs");
    assert_eq!(
        outcome,
        crate::bridge::handles::PickOutcome::Applied {
            cleared_variant: None
        }
    );
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pending is still there");
    assert_eq!(pending.model.as_deref(), Some(spec.as_str()));
    assert_eq!(
        pending.provider.as_deref(),
        Some(PROVIDER),
        "the pair is recorded, not a model-only write"
    );
}

/// Issue #910, item 3: two competing durable picks. The first is slow (parked
/// inside its `switch_session_model`), the second queues at the session's
/// switch point — so the newer one applies **after** the older one, and both
/// the server and the mirror end on it (never server = old, mirror = new).
#[tokio::test]
async fn two_competing_durable_picks_keep_the_server_and_mirror_in_step() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_session_selection(
            "ses_test",
            crate::backend::types::SessionSelection {
                model: Some(crate::backend::types::ModelInfo {
                    id: "gpt-5.1-codex".into(),
                    provider_id: "openai".into(),
                    variant: None,
                }),
                agent: None,
            },
        )
        .with_model_pick_route(ModelPickRoute::PerTurn);
    let gate = backend.hold_session_switch();
    let entered = backend.switch_model_gate_entered.clone();
    let selections = backend.session_selections.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let spawn_pick = |model: &'static str| {
        let app = app.clone();
        tokio::spawn(async move {
            send_command_in(
                &app,
                model,
                key(),
                "msg_model",
                crate::config::ConversationKind::P2p,
            )
            .await;
        })
    };
    let first = spawn_pick("/model openai/model-a");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first switch never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let second = spawn_pick("/model openai/model-b");
    let switch = app.session_gates.gate("ses_test");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while switch.switch_waiting() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second pick never queued at the switch point"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        entered.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second pick does not enter the server while the first is in flight"
    );

    gate.add_permits(1);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second switch never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    gate.add_permits(1);
    first.await.expect("the first joins");
    second.await.expect("the second joins");

    assert_eq!(
        selections
            .lock()
            .await
            .get("ses_test")
            .and_then(|selection| selection.model.as_ref())
            .map(|model| model.id.as_str()),
        Some("model-b"),
        "the newest pick is the server's selection"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/model-b"),
        "and the mirror agrees with the server"
    );
}

/// Issue #910, item 4: when the pre-flight's variant clear loses its
/// compare-and-set to a racing `/think`, the prompt must not send the value it
/// just identified as unsupported — it takes the fresh value instead (and only
/// stops by name when that one is unsupported too).
#[tokio::test]
async fn a_racing_think_replaces_the_cleared_variant_instead_of_sending_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .with_models(vec![crate::backend::types::ProviderModels {
            provider: PROVIDER.into(),
            models: vec![model_option(MODEL, &["low"])],
        }]);
    let gate = backend.hold_model_surface();
    let entered = backend.model_surface_gate_entered.clone();
    let prompt_variants = backend.prompt_variants.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pre-flight's catalog lookup never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The `/think` write lands while the clear is in flight, so its
    // compare-and-set (`high`) can no longer match. (The write is what races;
    // the `/think` command's own catalog validation would park on the same
    // gate the prompt holds, so it is driven directly here.)
    let think = app
        .sessions_handle()
        .pick_think(&app.client, &key(), Some("low".to_string()))
        .await
        .expect("the think write runs");
    assert_eq!(
        think,
        crate::bridge::handles::PickOutcome::Applied {
            cleared_variant: None
        },
        "the racing `/think` lands on its own snapshot"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .variant
            .as_deref(),
        Some("low"),
        "the `/think` owns the mirror now"
    );
    gate.add_permits(2);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_variants.lock().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the prompt was never submitted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        prompt_variants.lock().await.last().cloned().flatten(),
        Some("low".to_string()),
        "the fresh value is sent, never the one just identified as unsupported"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .variant
            .as_deref(),
        Some("low"),
        "and the mirror is left alone"
    );
}
