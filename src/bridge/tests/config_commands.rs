use crate::bridge::command::*;
use crate::bridge::test_support::*;

/// `/model` (no args) sends the provider-picker card (step 1 of the
/// two-level flow): one button per provider, not per model.
#[tokio::test]
async fn model_no_arg_sends_picker_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![crate::opencode::client::ProviderModels {
        provider: "opencode".into(),
        models: vec![
            model_option("deepseek-v4-flash", &[]),
            model_option("gpt-4o", &[]),
        ],
    }];
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::ModelCard,
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_model_card",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .next()
        .expect("a model card should be sent");
    let text = card.to_string();
    assert!(text.contains("选择模型"), "header: {text}");
    assert!(text.contains("选择 provider"), "intro: {text}");
    assert!(text.contains("\"value\":\"opencode\""), "provider button: {text}");
    assert!(
        !text.contains("deepseek-v4-flash"),
        "step 1 must not show models: {text}"
    );
}

/// A `/model` provider-picker button (level `provider`) opens that
/// provider's model picker; a model button (level `model`) records the
/// per-session override.
#[tokio::test]
async fn model_card_button_records_override() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![crate::opencode::client::ProviderModels {
        provider: "opencode".into(),
        models: vec![
            model_option("deepseek-v4-flash", &[]),
            model_option("gpt-4o", &[]),
        ],
    }];
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    // Step 1: pick a provider → the ack card becomes that provider's model
    // picker.
    let provider = serde_json::json!({
        "action": "model",
        "level": "provider",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "opencode",
    });
    let step1 = app
        .handle_card_action(provider)
        .await
        .expect("provider card action");
    let step1_card = step1.card.expect("provider click swaps in the model picker");
    let text1 = step1_card.to_string();
    assert!(
        text1.contains("opencode/deepseek-v4-flash"),
        "model button: {text1}"
    );
    assert!(text1.contains("返回全部 provider"), "back button: {text1}");

    // Step 2: pick a model → the override is recorded (toast only, no card).
    let model = serde_json::json!({
        "action": "model",
        "level": "model",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "opencode/deepseek-v4-flash",
    });
    let step2 = app.handle_card_action(model).await.expect("model card action");
    assert!(step2.card.is_none(), "selection is toast-only");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.model.as_deref(), Some("opencode/deepseek-v4-flash"));
}

/// The model picker's back button (level `provider`, `__providers__`)
/// returns to the full provider list.
#[tokio::test]
async fn model_picker_back_button_returns_to_providers() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![
        crate::opencode::client::ProviderModels {
            provider: "opencode".into(),
            models: vec![model_option("deepseek-v4-flash", &[])],
        },
        crate::opencode::client::ProviderModels {
            provider: "openrouter".into(),
            models: vec![model_option("gpt-4o", &[])],
        },
    ];
    let (app, _platform) = build_app(cfg, backend).await;
    let value = serde_json::json!({
        "action": "model",
        "level": "provider",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "__providers__",
    });
    let result = app.handle_card_action(value).await.expect("back action");
    let card = result.card.expect("back returns a card");
    let text = card.to_string();
    assert!(text.contains("opencode") && text.contains("openrouter"), "{text}");
    assert!(
        !text.contains("deepseek-v4-flash"),
        "provider list, not models: {text}"
    );
}

/// `/agent` (no args) sends the agent-picker card: it shows the CURRENT
/// agent (the server's default when no override is set — derived as the
/// first primary non-hidden agent, skipping subagents), records an override
/// from a button, treats an agent literally named `default` as a normal
/// pick, and clears via the dedicated `agent_clear` action.
#[tokio::test]
async fn agent_card_picker_and_button() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.agents = vec![
        crate::opencode::client::AgentInfo {
            name: "sec-agent".into(),
            description: Some("a subagent".into()),
            mode: Some("subagent".into()),
            hidden: Some(false),
        },
        crate::opencode::client::AgentInfo {
            name: "build".into(),
            description: Some("build agent".into()),
            mode: Some("primary".into()),
            hidden: Some(false),
        },
        crate::opencode::client::AgentInfo {
            name: "default".into(),
            description: Some("an agent literally named default".into()),
            mode: Some("primary".into()),
            hidden: Some(false),
        },
    ];
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::AgentCard,
        key.clone(),
        "msg_agent_card",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .next()
        .expect("an agent card should be sent");
    let text = card.to_string();
    assert!(text.contains("当前 Agent"), "current label: {text}");
    // The subagent sorts first in the fixture but is skipped: `build` is
    // the derived server default.
    assert!(text.contains("`build`（默认）"), "default agent: {text}");
    assert!(
        text.contains("\"action\":\"agent_clear\""),
        "clear action: {text}"
    );
    assert!(
        text.contains("\"value\":\"default\""),
        "an agent named `default` is a selectable value: {text}"
    );

    // An agent literally named `default` is a normal pick, never a clear.
    let literal_default = serde_json::json!({
        "action": "agent",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "default",
    });
    let result = app
        .handle_card_action(literal_default)
        .await
        .expect("agent literal-default action");
    assert!(result.card.is_some(), "refreshed card returned");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.agent.as_deref(), Some("default"));

    // A real pick records the override.
    let value = serde_json::json!({
        "action": "agent",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "build",
    });
    let result = app.handle_card_action(value).await.expect("agent card action");
    assert!(result.card.is_some(), "refreshed card returned");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.agent.as_deref(), Some("build"));

    // The dedicated `agent_clear` action clears it (mechanism, not value).
    let clear = serde_json::json!({
        "action": "agent_clear",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "",
    });
    app.handle_card_action(clear).await.expect("agent clear action");
    assert!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .and_then(|e| e.agent.clone())
            .is_none(),
        "agent_clear must clear the override"
    );
}

/// `/agent --reset` (text) clears the per-session override; a bare agent
/// name never clears — it is always a pick.
#[tokio::test]
async fn agent_text_reset_flag_clears_override() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: Some("build".into()),
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Agent("--reset".into()),
        key.clone(),
        "msg_agent_reset",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .and_then(|e| e.agent.clone())
            .is_none(),
        "--reset must clear the agent override"
    );
    let text = platform.texts().await.join("\n");
    assert!(text.contains("已清除 Agent"), "clear reply: {text}");

    // A bare name never clears — it records the override.
    crate::bridge::command::handle_command(
        &app.core,
        Command::Agent("build".into()),
        key.clone(),
        "msg_agent_set",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert_eq!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .and_then(|e| e.agent.clone())
            .as_deref(),
        Some("build"),
        "a bare agent name must be recorded, not clear"
    );
}

/// `/autoaccept` (no args) sends the toggle card; a button flips the flag.
#[tokio::test]
async fn autoaccept_card_toggles_flag() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::AutoAccept(crate::bridge::command::AutoAcceptAction::Status),
        key.clone(),
        "msg_aa",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .next()
        .expect("an autoaccept card should be sent");
    assert!(card.to_string().contains("自动审批"), "toggle card: {card}");

    let value = serde_json::json!({
        "action": "autoaccept",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "on",
    });
    let result = app
        .handle_card_action(value)
        .await
        .expect("autoaccept card action");
    assert!(result.card.is_some(), "refreshed card returned");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert!(entry.auto_accept, "flag should flip on");
}

#[tokio::test]
async fn name_patches_server_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let title_calls = backend.update_title_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Name("新名字".into()),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_name",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    assert_eq!(
        title_calls.lock().await.as_slice(),
        &[("ses_test".to_string(), "新名字".to_string())]
    );
}

/// ADR-0023: `/name` inside a cover-rooted topic patches the cover card
/// IMMEDIATELY (no waiting for the next completed turn) — the chat-list
/// topic entry is the card's content.
#[tokio::test]
async fn name_patches_cover_card_in_cover_rooted_topic() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    // The server title after the PATCH (the mock records the call but the
    // GET /session/{id} response comes from this map).
    backend
        .session_titles
        .lock()
        .unwrap()
        .insert("ses_test".into(), "新名字".into());
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_cover".into()),
            variant: None,
        },
    )
    .await;
    seed_cover_title(&app, "ses_test", "旧标题").await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Name("新名字".into()),
        crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
        "msg_name",
        crate::config::ConversationKind::Topic,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let patched = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_cover" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !patched.is_empty(),
        "the cover card must be patched right away, got {calls:?}"
    );
    assert!(
        patched.last().unwrap().contains("新名字"),
        "cover card must show the new title: {:?}",
        patched.last()
    );
    assert_eq!(
        app.core
            .cover_titles
            .lock()
            .await
            .get("ses_test")
            .map(|c| c.title.clone()),
        Some("新名字".to_string())
    );
}

/// ADR-0023: the server's default title (`New session - <ts>`) must never
/// be patched onto the cover card — it would replace the meaningful
/// creation title (the directory name) before the auto-title exists.
#[tokio::test]
async fn default_server_title_does_not_patch_cover_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    backend
        .session_titles
        .lock()
        .unwrap()
        .insert("ses_test".into(), "New session - 2024-12-14T05:33:00.000Z".into());
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_cover".into()),
            variant: None,
        },
    )
    .await;
    seed_cover_title(&app, "ses_test", "cola").await;

    let settled = crate::bridge::command::sync_topic_cover_title(&app.core, "ses_test").await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, PlatformCall::UpdateMessage { message_id, .. } if message_id == "om_cover")),
        "the default title must not be patched onto the cover card: {calls:?}"
    );
    assert_eq!(
        app.core
            .cover_titles
            .lock()
            .await
            .get("ses_test")
            .map(|c| c.title.clone()),
        Some("cola".to_string())
    );
    assert!(
        !settled,
        "a default title must signal 'retry later', not 'settled'"
    );
}

/// ADR-0023: when the auto-title lands shortly AFTER a short turn ends,
/// the retry ladder (backoff) patches the cover card anyway — the title
/// agent races the turn, so the cover must not depend on the user sending
/// another message.
#[tokio::test]
async fn cover_title_retry_ladder_catches_late_auto_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let titles = backend.session_titles.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: Some("om_seed".into()),
            topic_root: Some("om_cover".into()),
            variant: None,
        },
    )
    .await;
    seed_cover_title(&app, "ses_test", "cola").await;

    // The title is NOT on the server when the turn ends; the ladder starts
    // and the title lands a moment later.
    crate::bridge::command::spawn_cover_title_retry_at(
        &app.core,
        "ses_test",
        &[
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(200),
        ],
    );
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    titles
        .lock()
        .unwrap()
        .insert("ses_test".into(), "迟到标题".into());

    // Wait past the ladder window; the 100ms attempt must catch it.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let calls = platform.calls.lock().await.clone();
    let patched = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "om_cover" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !patched.is_empty(),
        "the retry ladder must patch the cover card once the title lands: {calls:?}"
    );
    assert!(
        patched.last().unwrap().contains("迟到标题"),
        "cover card must show the late title: {:?}",
        patched.last()
    );
}

/// `/model <provider/model>` records a per-session override (the OpenCode
/// server has no model-switch endpoint) and the NEXT prompt carries it.
#[tokio::test]
async fn model_command_records_override_used_on_next_prompt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Model("opencode-go/deepseek-v4-flash".into()),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The override is recorded on the session's persisted entry.
    let stored = {
        let store = app.sessions.lock().await;
        store.entry_for_session("ses_test").and_then(|e| e.model.clone())
    };
    assert_eq!(stored.as_deref(), Some("opencode-go/deepseek-v4-flash"));
    // The override is NOT applied to an unrelated session.
    assert!(app.sessions.lock().await.entry_for_session("ses_other").is_none());

    // The next message prompts with the override as the model.
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
        prompt_models.lock().await.as_slice(),
        &[Some("opencode-go/deepseek-v4-flash".to_string())]
    );
}

/// `/think <variant>` records a per-session override (the OpenCode server
/// has no thinking-level endpoint) and the NEXT prompt carries it as the
/// per-prompt `variant`.
#[tokio::test]
async fn think_command_records_variant_used_on_next_prompt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_variants = backend.prompt_variants.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/deepseek-v4-flash".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Think("high".into()),
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let stored = {
        let store = app.sessions.lock().await;
        store
            .entry_for_session("ses_test")
            .and_then(|e| e.variant.clone())
    };
    assert_eq!(stored.as_deref(), Some("high"));

    // The next message prompts with the variant.
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
        prompt_variants.lock().await.as_slice(),
        &[Some("high".to_string())]
    );
}

/// `/think` validates the variant against the effective model's declared
/// set: an undeclared value is rejected with feedback and nothing is stored.
#[tokio::test]
async fn think_command_rejects_undeclared_variant() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![crate::opencode::client::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }];
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/deepseek-v4-flash".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Think("medium".into()),
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let text = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .next()
        .expect("a rejection reply should be sent");
    assert!(text.contains("不支持思考等级"), "rejection: {text}");
    assert!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .and_then(|e| e.variant.clone())
            .is_none(),
        "an undeclared variant must not be stored"
    );
}

/// `/think --reset` clears the override; the bare words `default`/`off`/`reset`
/// are ordinary variant picks now (never clear words — ADR-0020).
#[tokio::test]
async fn think_reset_flag_clears_variant() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: Some("high".into()),
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Think("--reset".into()),
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    assert!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .and_then(|e| e.variant.clone())
            .is_none(),
        "--reset must clear the variant"
    );
}

/// A variant literally named `default` is a normal pick, never a clear
/// word: when the effective model doesn't declare it, `/think default` is
/// rejected (ADR-0020 decoupling).
#[tokio::test]
async fn think_bare_default_undeclared_is_rejected() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![crate::opencode::client::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }];
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/deepseek-v4-flash".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: Some("high".into()),
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Think("default".into()),
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .and_then(|e| e.variant.clone())
            .is_some(),
        "an undeclared bare word must not clear the variant"
    );
}

/// A variant literally named `default` that the model DOES declare is
/// stored as the override — the value namespace is never overloaded by the
/// clear mechanism (ADR-0020).
#[tokio::test]
async fn think_bare_default_declared_is_stored() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![crate::opencode::client::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "default"])],
    }];
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/deepseek-v4-flash".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Think("default".into()),
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert_eq!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .and_then(|e| e.variant.clone())
            .as_deref(),
        Some("default"),
        "a declared `default` variant must be stored"
    );
}

/// `/think` (no args) sends the variant-picker card listing the effective
/// model's declared variants.
#[tokio::test]
async fn think_no_arg_sends_variant_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![crate::opencode::client::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }];
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/deepseek-v4-flash".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: Some("high".into()),
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::ThinkCard,
        key.clone(),
        "msg_think_card",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .next()
        .expect("a think card should be sent");
    let text = card.to_string();
    assert!(text.contains("思考等级"), "header: {text}");
    assert!(text.contains("opencode-go/deepseek-v4-flash"), "model: {text}");
    assert!(text.contains("默认（清除）"), "default option: {text}");
    assert!(text.contains("\"value\":\"low\""), "variant low: {text}");
    assert!(text.contains("\"value\":\"high\""), "variant high: {text}");
    assert!(text.contains("当前思考等级"), "current label: {text}");
}

/// A `/think` card button records the chosen variant and refreshes the
/// card; the dedicated `think_clear` action clears it, and a card button
/// whose value is literally `default` is rejected (not a clear — the value
/// namespace is never overloaded, ADR-0020).
#[tokio::test]
async fn think_card_button_records_variant() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![crate::opencode::client::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }];
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/deepseek-v4-flash".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let value = serde_json::json!({
        "action": "think",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "high",
    });
    let result = app.handle_card_action(value).await.expect("think card action");
    assert!(result.card.is_some(), "refreshed card returned");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.variant.as_deref(), Some("high"));

    // A `think` action whose value is literally `default` is NOT a clear
    // and NOT a pick (the declared set lacks it): the current `high`
    // override must survive untouched.
    let literal_default = serde_json::json!({
        "action": "think",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "default",
    });
    app.handle_card_action(literal_default)
        .await
        .expect("think literal-default action");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .and_then(|e| e.variant.clone())
            .as_deref(),
        Some("high"),
        "a literal `default` value must neither clear nor replace the variant"
    );

    // The dedicated `think_clear` action clears it (mechanism, not value).
    let clear = serde_json::json!({
        "action": "think_clear",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "",
    });
    app.handle_card_action(clear).await.expect("think clear action");
    assert!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .and_then(|e| e.variant.clone())
            .is_none(),
        "think_clear must clear the variant"
    );
}

/// Switching `/model` to a model that doesn't declare the current variant
/// auto-clears it (ADR-0020) instead of leaving every prompt to fail with a
/// server VariantUnavailableError.
#[tokio::test]
async fn model_switch_clears_undeclared_variant() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![
        crate::opencode::client::ProviderModels {
            provider: "opencode-go".into(),
            models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
        },
        crate::opencode::client::ProviderModels {
            provider: "openrouter".into(),
            models: vec![model_option("other-model", &["low"])],
        },
    ];
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/deepseek-v4-flash".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: Some("high".into()),
        },
    )
    .await;

    // Switching to the SAME model (declares high) keeps the variant.
    crate::bridge::command::handle_command(
        &app.core,
        Command::Model("opencode-go/deepseek-v4-flash".into()),
        key.clone(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert_eq!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .and_then(|e| e.variant.clone())
            .as_deref(),
        Some("high"),
        "a still-declared variant must survive a model switch"
    );

    // A model that positively lacks the variant clears it.
    crate::bridge::command::handle_command(
        &app.core,
        Command::Model("openrouter/other-model".into()),
        key.clone(),
        "msg_model2",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .and_then(|e| e.variant.clone())
            .is_none(),
        "a model that lacks the variant must clear it"
    );
}

/// Switching `/model` to a model NOT in the advertised catalog leaves the
/// variant in place (best-effort: an unknown model can't be judged, so it is
/// not destroyed; the server's VariantUnavailableError is the fallback).
#[tokio::test]
async fn model_switch_keeps_variant_when_new_model_unknown() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.provider_models = vec![crate::opencode::client::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }];
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/deepseek-v4-flash".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: Some("high".into()),
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Model("openrouter/other-model".into()),
        key.clone(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    assert_eq!(
        app.sessions
            .lock()
            .await
            .entry_for_session("ses_test")
            .and_then(|e| e.variant.clone())
            .as_deref(),
        Some("high"),
        "an unknown model must not clear the variant"
    );
}

/// `/model` with a malformed value gets immediate feedback instead of a
/// silent no-op.
#[tokio::test]
async fn model_command_rejects_malformed_value() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Model("not-a-model".into()),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("⚠️") && text.contains("<provider>/<model>"),
        "malformed /model must reply with format guidance: {text}"
    );
    // Nothing was recorded.
    let stored = {
        let store = app.sessions.lock().await;
        store.entry_for_session("ses_test").and_then(|e| e.model.clone())
    };
    assert!(stored.is_none(), "malformed /model must not record an override");
}

/// The `/model` override also flows through the supplement (in-flight)
/// prompt_async path, not just the synchronous prompt.
#[tokio::test]
async fn model_override_flows_through_supplement_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let async_models = backend.prompt_async_models.clone();
    let async_calls = backend.prompt_async_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // Record the override on the persisted entry, then mark the session
    // busy so the next message takes the supplement path.
    {
        let mut store = app.sessions.lock().await;
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let mut entry = store.get_active(&key).unwrap().clone();
        entry.model = Some("opencode-go/deepseek-v4-flash".into());
        store.set_active(entry);
    }
    app.inflight.lock().await.insert("ses_test".into());

    app.handle_message(incoming(
        "msg_supp".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充内容".into(),
        None,
    ))
    .await;

    assert_eq!(
        async_calls.lock().await.as_slice(),
        &["ses_test:补充内容".to_string()]
    );
    assert_eq!(
        async_models.lock().await.as_slice(),
        &[Some("opencode-go/deepseek-v4-flash".to_string())]
    );
}

/// `/agent <name>` records a per-session override (the OpenCode server has
/// no agent-switch endpoint) and the NEXT prompt carries it.
#[tokio::test]
async fn agent_command_records_override_used_on_next_prompt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_agents = backend.prompt_agents.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Agent("primary".into()),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_agent",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The override is recorded on the session's persisted entry.
    let stored = {
        let store = app.sessions.lock().await;
        store.entry_for_session("ses_test").and_then(|e| e.agent.clone())
    };
    assert_eq!(stored.as_deref(), Some("primary"));
    // The override is NOT applied to an unrelated session.
    assert!(app.sessions.lock().await.entry_for_session("ses_other").is_none());

    // The next message prompts with the override as the agent.
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
        prompt_agents.lock().await.as_slice(),
        &[Some("primary".to_string())]
    );
}

/// The `/agent` override also flows through the supplement (in-flight)
/// prompt_async path, not just the synchronous prompt.
#[tokio::test]
async fn agent_override_flows_through_supplement_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let async_agents = backend.prompt_async_agents.clone();
    let async_calls = backend.prompt_async_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: Some("primary".into()),
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    app.inflight.lock().await.insert("ses_test".into());

    app.handle_message(incoming(
        "msg_supp".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充内容".into(),
        None,
    ))
    .await;

    assert_eq!(
        async_calls.lock().await.as_slice(),
        &["ses_test:补充内容".to_string()]
    );
    assert_eq!(
        async_agents.lock().await.as_slice(),
        &[Some("primary".to_string())]
    );
}

/// `/model` and `/agent` overrides are persisted in sessions.json, so a cola
/// restart (which reloads the store) keeps them.
#[tokio::test]
async fn model_override_persists_across_store_reload() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Model("opencode-go/deepseek-v4-flash".into()),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // A freshly-loaded store (as after a restart) still has the override.
    let reloaded = crate::bridge::session::SessionStore::new(dir.path().join("sessions.json")).unwrap();
    let entry = reloaded
        .entry_for_session("ses_test")
        .expect("session mapping survives reload");
    assert_eq!(entry.model.as_deref(), Some("opencode-go/deepseek-v4-flash"));
}
