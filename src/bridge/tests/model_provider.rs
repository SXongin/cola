//! `/model`'s thread-level provider pick (issue #910): a new thread takes one at
//! creation, an existing thread only by a confirmed `thread/resume`, and the
//! default is the conservative "new thread only" — the click says so and sends
//! nothing. Provider switching never changes the model.

use crate::backend::types::{ChangeScope, ModelOption, ModelScope, ModelSurface, PickKind};
use crate::bridge::session::PendingEntry;
use crate::bridge::test_support::*;

const PROVIDER: &str = "deepseek";

fn key() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

/// A Codex-shaped surface: a grouped model scope (with a profile preset that
/// pairs a model with a provider) plus the thread-level provider scope.
fn surface(pending: bool) -> ModelSurface {
    let model = ModelScope {
        id: "model".into(),
        title: "模型（每轮可换）".into(),
        changeable: ChangeScope::ThisTurn,
        pick: PickKind::Model,
        options: vec![
            ModelOption {
                id: "gpt-5.1".into(),
                value: Some("codex/gpt-5.1".into()),
                group: Some("内置目录".into()),
                ..Default::default()
            },
            ModelOption {
                id: "deepseek-v4-pro".into(),
                label: Some("fast：deepseek-v4-pro（deepseek）".into()),
                value: Some("codex/deepseek-v4-pro".into()),
                group: Some("Profile 预设".into()),
                provider: Some(PROVIDER.into()),
                ..Default::default()
            },
        ],
        note: None,
        free_text: Some("/model codex/<id>".into()),
        intro: None,
    };
    let provider = ModelScope {
        id: "provider".into(),
        title: "Provider（线程级：新线程 / resume）".into(),
        changeable: if pending {
            ChangeScope::NewThread
        } else {
            ChangeScope::ResumeOnly
        },
        pick: PickKind::Provider,
        options: vec![ModelOption {
            id: PROVIDER.into(),
            label: Some(format!(
                "DeepSeek（{}）",
                if pending { "新线程生效" } else { "需确认" }
            )),
            value: Some(PROVIDER.into()),
            ..Default::default()
        }],
        note: None,
        free_text: None,
        intro: None,
    };
    ModelSurface {
        current: None,
        scopes: vec![model, provider],
    }
}

/// The callback a provider-scope button sends.
fn provider_pick() -> serde_json::Value {
    serde_json::json!({
        "action": "model",
        "level": "provider_switch",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": PROVIDER,
    })
}

/// The callback the confirmation card's 确认 button sends.
fn provider_confirm() -> serde_json::Value {
    serde_json::json!({
        "action": "model_provider_confirm",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": PROVIDER,
    })
}

/// Issue #910 acceptance 1 + 2: a Pending Session records the provider (and the
/// profile's paired provider), and the first message carries the pair into
/// `thread/start` — never into a turn.
#[tokio::test]
async fn a_pending_sessions_provider_and_model_ride_thread_creation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_model_surface(surface(true));
    let picks = backend.created_session_picks.clone();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    // A profile preset applies model *and* its provider.
    let step = app
        .host_action(serde_json::json!({
            "action": "model",
            "level": "model",
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "value": "codex/deepseek-v4-pro",
            "provider": PROVIDER,
        }))
        .await
        .expect("model pick");
    assert!(step.card.is_none(), "a pending pick is toast-only");
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pending survives");
    assert_eq!(pending.model.as_deref(), Some("codex/deepseek-v4-pro"));
    assert_eq!(pending.provider.as_deref(), Some(PROVIDER));

    // The first message materialises it: one `thread/start` carries both.
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
        &[(Some(PROVIDER.to_string()), Some("deepseek-v4-pro".to_string()))],
        "the creation carries the provider and the model it pairs with"
    );
    assert_eq!(
        prompt_models.lock().await.as_slice(),
        &[Some("codex/deepseek-v4-pro".to_string())],
        "the turn carries the model only — a provider is never a turn field"
    );
}

/// Issue #910 acceptance 3: with the switch off (the default), a provider pick
/// on an existing thread only says where it *can* happen and sends nothing.
#[tokio::test]
async fn a_live_provider_pick_with_the_switch_off_sends_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_model_surface(surface(false));
    let switches = backend.provider_switch_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_test", "/work/cola"),
    )
    .await;

    let step = app.host_action(provider_pick()).await.expect("provider pick");
    assert!(step.card.is_none(), "no confirmation when the door is shut");
    assert_eq!(
        step.toast.as_deref(),
        Some("provider 只能在新线程切换"),
        "the copy names the only place it works"
    );
    assert!(
        switches.lock().await.is_empty(),
        "no resume, no request: the click changed nothing"
    );
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(stored.model, None, "the model is untouched too");
}

/// Issue #910 acceptance 4 + 7: with the switch open, the pick asks first; the
/// confirmation names the peer effect and only 确认 resumes the thread.
#[tokio::test]
async fn a_live_provider_pick_confirms_before_the_resume() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_model_surface(surface(false))
        .with_provider_change_scope(ChangeScope::ResumeOnly);
    let switches = backend.provider_switch_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = crate::config::SessionEntry::new(key(), "ses_test", "/work/cola");
    entry.model = Some("codex/gpt-5.1".into());
    seed_entry(&app, entry).await;

    let step = app.host_action(provider_pick()).await.expect("provider pick");
    let card = step.card.expect("a confirmation card");
    let text = card.to_string();
    assert!(text.contains("其他客户端"), "the peer effect is stated: {text}");
    assert!(text.contains("model_provider_confirm"), "{text}");
    assert!(
        switches.lock().await.is_empty(),
        "the first click only asks: nothing was resumed yet"
    );

    let step = app.host_action(provider_confirm()).await.expect("confirm");
    assert_eq!(
        switches.lock().await.as_slice(),
        &[("ses_test".to_string(), PROVIDER.to_string())],
        "the confirmed click resumes the thread with the provider"
    );
    let done = step.card.expect("a result card");
    assert!(done.to_string().contains("已切换"), "{done}");
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(
        stored.model.as_deref(),
        Some("codex/gpt-5.1"),
        "a provider switch never changes the model"
    );
}

/// Issue #910 acceptance 4: 取消 does nothing at all — no request, no store
/// write, and the ack carries neither a card nor a toast.
#[tokio::test]
async fn cancelling_the_provider_confirmation_changes_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_provider_change_scope(ChangeScope::ResumeOnly);
    let switches = backend.provider_switch_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_test", "/work/cola"),
    )
    .await;

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
    assert!(switches.lock().await.is_empty());
}

/// Issue #910 acceptance 6: a refused resume names itself in the card and the
/// mirror keeps the previous provider — no silent fallback.
#[tokio::test]
async fn a_refused_provider_switch_names_the_error_and_keeps_the_mirror() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_provider_change_scope(ChangeScope::ResumeOnly)
        .fail_provider_switch("unknown provider: deepseek");
    let switches = backend.provider_switch_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = crate::config::SessionEntry::new(key(), "ses_test", "/work/cola");
    entry.model = Some("codex/gpt-5.1".into());
    seed_entry(&app, entry).await;

    let step = app.host_action(provider_confirm()).await.expect("confirm");
    let card = step.card.expect("an error card");
    let text = card.to_string();
    assert!(text.contains("切换失败"), "the failure is named: {text}");
    assert!(text.contains("unknown provider"), "{text}");
    assert!(
        text.contains("没有改动") || text.contains("保持原来的 provider"),
        "the copy promises the thread did not move: {text}"
    );
    assert!(step.toast.is_some(), "the click answers with a toast too");
    assert!(
        switches.lock().await.is_empty(),
        "a refused switch records nothing"
    );
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(
        stored.model.as_deref(),
        Some("codex/gpt-5.1"),
        "the mirror still names the thread's model"
    );
}
