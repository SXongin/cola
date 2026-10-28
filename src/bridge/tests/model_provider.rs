//! `/model`'s provider+model pair (issue #910), measured against the real
//! app-server:
//!
//! - a pick whose provider is the thread's own is an **ordinary model pick** —
//!   no `thread/resume`, no `thread/unsubscribe`;
//! - a pick that changes the provider is a **thread-level switch**: release,
//!   `thread/resume` with the pair, `thread/read` read-back. It needs
//!   confirmation on an idle thread, is deferred while a turn runs, and never
//!   reports its own RPC's ok as success;
//! - the switch runs off the card-ack path, and a refused/failed save is never
//!   shown as "Applied".

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

/// Issue #910, item 2 (and the kept creation path): a Pending Session records
/// the **pair**, and the first message carries both halves into one
/// `thread/start`.
#[tokio::test]
async fn a_pending_sessions_pair_rides_thread_creation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
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
    assert_eq!(
        pending.model.as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the mirror records the pair"
    );
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
    let routes = backend.model_pick_route_calls.clone();
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
    assert_eq!(
        routes.lock().await.as_slice(),
        &[("ses_test".to_string(), "openai/gpt-5.1".to_string())],
        "the route was asked about the pair"
    );
    assert!(
        applies.lock().await.is_empty(),
        "a same-provider pick sends nothing thread-level"
    );
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(stored.model.as_deref(), Some("openai/gpt-5.1"));
}

/// Issue #910, item 3 (gate closed): a provider change without the opt-in only
/// says where it *can* happen, and changes nothing.
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
    assert!(
        applies.lock().await.is_empty(),
        "no request: the click changed nothing"
    );
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(
        stored.model.as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the mirror is untouched too"
    );
}

/// Issue #910, items 1/4/8: an idle thread asks first, and the confirmed
/// switch runs **off the ack** — the click is answered with 「正在…」, the RPC
/// happens after, and the outcome replaces the card.
#[tokio::test]
async fn a_provider_pick_confirms_then_switches_off_the_ack() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let applies = backend.thread_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    let text = card.to_string();
    assert!(text.contains("deepseek/deepseek-v4-pro"), "{text}");
    assert!(
        text.contains("CODEX_HOME"),
        "the peer copy names the second cola: {text}"
    );
    assert!(
        applies.lock().await.is_empty(),
        "the first click only asks: nothing was switched yet"
    );

    let confirm = button_value(&card, "model_provider_confirm");
    assert_eq!(confirm["provider"], PROVIDER);
    assert_eq!(confirm["value"], MODEL);
    assert_eq!(confirm["session_id"], "ses_test");
    let step = app
        .host_action_on_message(confirm, "om_confirm")
        .await
        .expect("confirm");
    let ack = step.card.expect("the in-progress card");
    assert!(
        ack.to_string().contains("正在切换"),
        "the ack is the in-progress card, not the outcome: {ack}"
    );

    wait_for_card_update(&platform, "the switch outcome", CardUpdates::Any, |card| {
        card_header(card).contains("已切换")
    })
    .await;
    assert_eq!(
        applies.lock().await.as_slice(),
        &[("ses_test".to_string(), "deepseek/deepseek-v4-pro".to_string())],
        "the confirmed click switches the thread with the pair"
    );
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(
        stored.model.as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the mirror names the pair the thread now runs"
    );
}

/// Issue #910, item 7 (stale card): the target session is resolved at
/// confirmation time; a card that names another thread does nothing.
#[tokio::test]
async fn a_stale_provider_confirmation_does_nothing() {
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

    // The card was rendered for `ses_old`; the thread has moved since.
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
        applies.lock().await.is_empty(),
        "a stale card must not switch anything"
    );
}

/// Issue #910, item 7 (gate recheck): a confirmation card that outlived the
/// config change cannot switch the thread.
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
    let applies = backend.thread_model_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    // The operator turns the switch off while the card sits in the chat.
    *gate.lock().expect("provider change scope") = ChangeScope::NewThread;

    let step = app
        .host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    assert_eq!(
        step.toast.as_deref(),
        Some("provider 只能在新线程切换"),
        "the confirmation re-reads the gate"
    );
    assert!(applies.lock().await.is_empty(), "nothing was switched");
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
    let applies = backend.thread_model_calls.clone();
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
    assert!(applies.lock().await.is_empty());
}

/// Issue #910, item 4: a pick while a turn is running is **recorded**, not
/// failed — and the next prompt applies it before sending.
#[tokio::test]
async fn a_running_turn_defers_the_switch_to_the_next_prompt() {
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
    // No initial mirror: the first prompt has no pair to pre-flight, so the only
    // thread-level call in this test is the deferred switch's.
    seed_entry(&app, entry()).await;

    let first = {
        let app = app.clone();
        tokio::spawn(async move {
            app.handle_message(incoming(
                "msg_prompt_1".into(),
                "chat_1".into(),
                "p2p".into(),
                None,
                "first".into(),
                None,
            ))
            .await;
        })
    };
    // The prompt is at the gate: the turn is live.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_calls.lock().await.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "the turn never started");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick during a turn");
    assert!(step.card.is_none(), "a deferred switch is not a failure");
    assert!(
        step.toast.as_deref().unwrap_or_default().contains("已记录"),
        "the toast says it was recorded: {step:?}",
        step = step.toast
    );
    assert!(
        applies.lock().await.is_empty(),
        "the switch waits for the turn to end"
    );
    let mirror = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(
        mirror.model.as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the mirror is the pending record"
    );

    gate.add_permits(1);
    first.await.expect("the first turn ends");

    // The next prompt applies the switch BEFORE it is sent.
    app.handle_message(incoming(
        "msg_prompt_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "second".into(),
        None,
    ))
    .await;
    assert_eq!(
        applies.lock().await.as_slice(),
        &[("ses_test".to_string(), "deepseek/deepseek-v4-pro".to_string())],
        "the deferred pair was applied"
    );
    assert_eq!(
        prompt_calls.lock().await.len(),
        2,
        "both turns were sent (the deferred one did not block pick time)"
    );
    assert_eq!(
        prompt_models.lock().await.last().cloned().flatten().as_deref(),
        Some("deepseek/deepseek-v4-pro")
    );
}

/// Issue #910, item 4: when the deferred switch still cannot apply at prompt
/// time, the prompt is **not sent** on the old provider.
#[tokio::test]
async fn a_deferred_switch_that_cannot_apply_stops_the_prompt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .fail_provider_switch("另一个客户端或另一个 cola 实例持有这条线程");
    let applies = backend.thread_model_calls.clone();
    let prompt_calls = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    // A pair recorded for the next turn (e.g. by a previous, interrupted pick).
    app.sessions
        .lock()
        .await
        .set_settings(
            &key(),
            crate::bridge::session::SessionSettings {
                directory: "/work/cola".into(),
                session_id: Some("ses_test".into()),
                model: Some("deepseek/deepseek-v4-pro".into()),
                ..Default::default()
            },
        )
        .expect("seed the pending pair");

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
        prompt_calls.lock().await.is_empty(),
        "the turn must not be sent on the old provider"
    );
    assert!(
        !applies.lock().await.is_empty(),
        "the prompt path tried to apply the pair first"
    );
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(
        stored.model.as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the recorded pair stays for a retry once the holder is gone"
    );
    // The user is told why, before any turn ran.
    let text = platform
        .updated_cards()
        .await
        .iter()
        .map(|card| card.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("持有这条线程"),
        "the failure is explained on the card: {text}"
    );
}

/// Issue #910, item 8: a slow three-step switch does not hold the ack — the
/// click is answered while the RPC is still running, and the outcome lands as a
/// follow-up card.
#[tokio::test]
async fn a_slow_switch_still_answers_the_click() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .with_thread_model_delay(std::time::Duration::from_millis(600));
    let applies = backend.thread_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");

    let started = std::time::Instant::now();
    let step = app
        .host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    let acked = started.elapsed();
    assert!(
        acked < std::time::Duration::from_millis(600),
        "the ack must not wait for the switch: {acked:?}"
    );
    assert!(
        applies.lock().await.is_empty(),
        "the switch is still running when the click is answered"
    );
    assert!(
        step.card
            .as_ref()
            .is_some_and(|card| card.to_string().contains("正在切换")),
        "the ack is the in-progress card"
    );

    wait_for_card_update(&platform, "the slow switch's outcome", CardUpdates::Any, |card| {
        card_header(card).contains("已切换")
    })
    .await;
    assert_eq!(
        applies.lock().await.len(),
        1,
        "the switch ran exactly once, after the ack"
    );
}

/// Issue #910, item 5 (failure at the bridge): a refused switch is a named
/// error card, and the mirror keeps the previous pair.
#[tokio::test]
async fn a_refused_switch_is_named_and_keeps_the_mirror() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .fail_provider_switch(
            "thread/resume 之后这条线程仍是 `openai/gpt-5.1-codex`：可能有另一个客户端（Codex TUI / \
             VS Code）或另一个共用同一个 `CODEX_HOME` 的 cola 实例仍持有它",
        );
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

    wait_for_card_update(&platform, "the refusal", CardUpdates::Any, |card| {
        card_header(card).contains("切换失败")
    })
    .await;
    let updated = platform.updated_cards().await;
    let text = updated.last().unwrap().to_string();
    assert!(text.contains("CODEX_HOME"), "the holder kinds are named: {text}");
    assert!(text.contains("TUI"), "{text}");
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(
        stored.model.as_deref(),
        Some("openai/gpt-5.1-codex"),
        "a refused switch does not move the mirror"
    );
}

/// Issue #910, item 4/6-adjacent: the thread already ran the pair, so the
/// confirmed switch reports "already" rather than a change.
#[tokio::test]
async fn an_already_switched_thread_says_so() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .with_thread_model_unchanged();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let updated = platform.updated_cards().await;
        if updated
            .iter()
            .any(|card| card_header(card).contains("已经是这个值"))
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no 'already' card");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Issue #910, item 9: the pair is saved **before** the RPC (so a prompt racing
/// the switch cannot send the old model on the new provider). A save that fails
/// stops the switch and says so — no request, never a clean success.
#[tokio::test]
async fn a_failed_save_stops_the_switch_before_any_request() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let applies = backend.thread_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    let confirm = button_value(&card, "model_provider_confirm");

    // The sidecar path becomes a directory: the pending-pair save fails.
    std::fs::remove_file(&sessions).expect("the seeded store file");
    std::fs::create_dir(&sessions).expect("replace the store with a directory");

    app.host_action_on_message(confirm, "om_confirm")
        .await
        .expect("confirm");
    wait_for_card_update(&platform, "the blocked switch", CardUpdates::Any, |card| {
        card_header(card).contains("没有开始")
    })
    .await;
    let text = platform.updated_cards().await.last().unwrap().to_string();
    assert!(text.contains("没有发出任何切换请求"), "{text}");
    assert!(
        applies.lock().await.is_empty(),
        "a save that did not land must not switch the thread"
    );
}

/// Issue #910, item 9: recording the pair can fail; the toast must say so, never
/// "Applied".
#[tokio::test]
async fn a_failed_paired_save_does_not_report_success() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    // Make the sidecar unwritable: its path is a directory, so the store's
    // blocking write fails while the pick is being saved.
    std::fs::remove_file(&sessions).expect("the seeded store file");
    std::fs::create_dir(&sessions).expect("replace the store with a directory");

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("paired pick");
    let toast = step.toast.unwrap_or_default();
    assert!(
        toast.contains("保存失败"),
        "a save that did not land is reported: {toast}"
    );
    assert!(
        !toast.contains("新线程创建时生效"),
        "a failed save must not claim the pick applies: {toast}"
    );
}

/// Review round, finding 1: a failed switch must put the mirror back exactly —
/// including "there was no model override" — instead of leaving the attempted
/// pair to be retried forever.
#[tokio::test]
async fn a_failed_switch_restores_a_mirror_that_had_no_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .fail_provider_switch("另一个客户端仍持有这条线程");
    let (app, platform) = build_app(cfg, backend).await;
    // No model override at all: the mirror starts empty.
    seed_entry(&app, entry()).await;
    assert!(
        app.sessions
            .lock()
            .await
            .settings(&key())
            .unwrap()
            .model
            .is_none()
    );

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    wait_for_card_update(&platform, "the refusal", CardUpdates::Any, |card| {
        card_header(card).contains("切换失败")
    })
    .await;

    assert_eq!(
        app.sessions.lock().await.settings(&key()).unwrap().model,
        None,
        "the failed attempt is not left in the mirror"
    );
}

/// Review round, finding 1 (variant half): writing the pair can clear a
/// `/think` variant the new model does not declare; a failed switch restores it
/// together with the model.
#[tokio::test]
async fn a_failed_switch_restores_a_cleared_variant() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .fail_provider_switch("另一个客户端仍持有这条线程");
    // The attempted model declares only `low`, so the `high` variant is cleared
    // by the pending write (ADR-0020).
    backend.with_models(vec![crate::backend::types::ProviderModels {
        provider: PROVIDER.into(),
        models: vec![crate::backend::types::ModelOption {
            id: MODEL.into(),
            variants: vec!["low".into()],
            ..Default::default()
        }],
    }]);
    let (app, platform) = build_app(cfg, backend).await;
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
    wait_for_card_update(&platform, "the refusal", CardUpdates::Any, |card| {
        card_header(card).contains("切换失败")
    })
    .await;

    let settings = app.sessions.lock().await.settings(&key()).unwrap();
    assert_eq!(
        settings.model.as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the model is restored"
    );
    assert_eq!(
        settings.variant.as_deref(),
        Some("high"),
        "the variant the attempt cleared is restored too"
    );
}

/// Review round, finding 2: a confirmation that arrives while a turn is running
/// defers exactly like a pick — the thread is never changed under the turn.
#[tokio::test]
async fn a_confirmation_during_a_turn_is_deferred() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let applies = backend.thread_model_calls.clone();
    let prompt_calls = backend.prompt_calls.clone();
    let gate = backend.hold_prompts();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    // The card is rendered while idle...
    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");

    // ...then a prompt starts before the user confirms.
    let first = {
        let app = app.clone();
        tokio::spawn(async move {
            app.handle_message(incoming(
                "msg_prompt_1".into(),
                "chat_1".into(),
                "p2p".into(),
                None,
                "first".into(),
                None,
            ))
            .await;
        })
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_calls.lock().await.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "the turn never started");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let step = app
        .host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    let ack = step.card.expect("a deferred card");
    assert!(
        ack.to_string().contains("已记录"),
        "the confirmation says it was recorded, not switched: {ack}"
    );
    assert!(
        applies.lock().await.is_empty(),
        "the thread is not touched while the turn runs"
    );

    gate.add_permits(1);
    first.await.expect("the turn ends");
    // The next prompt applies the recorded pair before it is sent.
    app.handle_message(incoming(
        "msg_prompt_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "second".into(),
        None,
    ))
    .await;
    assert_eq!(
        applies.lock().await.as_slice(),
        &[("ses_test".to_string(), "deepseek/deepseek-v4-pro".to_string())],
        "the deferred pair is applied by the next prompt"
    );
}

/// Review round, finding 4: one switch per session at a time — a replay while
/// the first is running is answered, not started.
#[tokio::test]
async fn a_replayed_confirmation_starts_one_switch() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .with_thread_model_delay(std::time::Duration::from_millis(600));
    let applies = backend.thread_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
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
    let step = app
        .host_action_on_message(confirm, "om_confirm")
        .await
        .expect("replayed confirm");
    assert!(
        step.card
            .as_ref()
            .is_some_and(|card| card.to_string().contains("还在进行中")),
        "a replay while the switch runs is answered as in progress: {step:?}",
        step = step.card
    );

    wait_for_card_update(&platform, "the switch outcome", CardUpdates::Any, |card| {
        card_header(card).contains("已切换")
    })
    .await;
    assert_eq!(
        applies.lock().await.len(),
        1,
        "the replayed click started no second release/resume sequence"
    );
}

/// Review round, finding 5: a slow failed switch must not roll back over a
/// newer pick that landed while it ran.
#[tokio::test]
async fn a_stale_rollback_does_not_undo_a_newer_pick() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .with_thread_model_delay(std::time::Duration::from_millis(600))
        .fail_provider_switch("另一个客户端仍持有这条线程");
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

    // A newer pick lands while the slow switch is still running.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut settings = app.sessions.lock().await.settings(&key()).unwrap();
    settings.model = Some("openai/gpt-5.1".into());
    app.sessions
        .lock()
        .await
        .set_settings(&key(), settings)
        .expect("the newer pick saves");

    wait_for_card_update(&platform, "the refusal", CardUpdates::Any, |card| {
        card_header(card).contains("切换失败")
    })
    .await;
    assert_eq!(
        app.sessions
            .lock()
            .await
            .settings(&key())
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1"),
        "the stale rollback did not overwrite the newer pick"
    );
}

/// Review round, finding 6: a Pending Session's pair is one store write, so the
/// two halves cannot be split on disk.
#[tokio::test]
async fn a_pending_pair_is_saved_in_one_write() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    app.host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("paired pick");

    // Read the sidecar back: both halves are there, from one write.
    let reloaded =
        crate::bridge::session::SessionStore::for_client(sessions.clone(), crate::config::DEFAULT_CLIENT_ID)
            .expect("the store reloads");
    let pending = reloaded.pending_for(&key()).expect("the pending is on disk");
    assert_eq!(pending.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
    assert_eq!(pending.provider.as_deref(), Some(PROVIDER));
}

/// Review round, finding 3 (the surface half): a thread that already runs the
/// target provider with another model is not "already switched" — the model
/// lands on the next turn, and the card says so.
#[tokio::test]
async fn a_provider_match_with_another_model_is_not_already() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_pick_route(ModelPickRoute::ThreadModel)
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .with_thread_model_next_turn();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let step = app
        .host_action(paired_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    let card = step.card.expect("a confirmation card");
    app.host_action_on_message(button_value(&card, "model_provider_confirm"), "om_confirm")
        .await
        .expect("confirm");
    wait_for_card_update(&platform, "the model-pending outcome", CardUpdates::Any, |card| {
        card_header(card).contains("模型下一轮生效")
    })
    .await;
}
