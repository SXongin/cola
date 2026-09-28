use crate::bridge::test_support::*;

/// `/model` (no args) sends the provider-picker card (step 1 of the
/// two-level flow): one button per provider, not per model — and the intro
/// names the CURRENT effective model (here the session's own override, with
/// its `/think` variant), so the user sees what a pick would replace.
#[tokio::test]
async fn model_no_arg_sends_picker_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode".into(),
        models: vec![
            model_option("deepseek-v4-flash", &[]),
            model_option("gpt-4o", &[]),
        ],
    }]);
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode/deepseek-v4-flash".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: Some("low".into()),
        },
    )
    .await;

    send_command_in(
        &app,
        "/model",
        key,
        "msg_model_card",
        crate::config::ConversationKind::P2p,
    )
    .await;

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
    assert!(
        text.contains("当前模型") && text.contains("opencode/deepseek-v4-flash@low"),
        "current model line: {text}"
    );
    let buttons = card_buttons(&card);
    assert!(
        buttons.iter().any(|b| b["value"]["value"] == "opencode"),
        "provider button: {text}"
    );
    assert!(
        !buttons
            .iter()
            .any(|b| b["value"]["value"] == "opencode/deepseek-v4-flash"),
        "step 1 must not show models as options: {text}"
    );
}

/// With no `/model` override, the current-model line falls back to cola's
/// configured default (`[opencode] model`).
#[tokio::test]
async fn model_card_falls_back_to_the_configured_default_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode".into(),
        models: vec![model_option("gpt-4o", &[])],
    }]);
    backend.with_default_model(crate::opencode::types::ModelInfo {
        id: "gpt-4o".into(),
        provider_id: "opencode".into(),
        variant: None,
    });
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

    send_command_in(
        &app,
        "/model",
        key,
        "msg_model_card",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let text = platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("a model card should be sent");
    assert!(
        text.contains("当前模型") && text.contains("`opencode/gpt-4o`"),
        "configured default shown: {text}"
    );
}

/// With neither an override nor a configured default, the current-model line
/// falls back to the model the server recorded on the session.
#[tokio::test]
async fn model_card_falls_back_to_the_server_recorded_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode".into(),
        models: vec![model_option("gpt-4o", &[])],
    }]);
    backend.with_session_model(crate::opencode::types::SessionModel {
        provider_id: "opencode".into(),
        id: "gpt-4o".into(),
    });
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

    send_command_in(
        &app,
        "/model",
        key,
        "msg_model_card",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let text = platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("a model card should be sent");
    assert!(
        text.contains("当前模型") && text.contains("`opencode/gpt-4o`"),
        "server-recorded model shown: {text}"
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
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode".into(),
        models: vec![
            model_option("deepseek-v4-flash", &[]),
            model_option("gpt-4o", &[]),
        ],
    }]);
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
    let step1 = app.host_action(provider).await.expect("provider card action");
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
    let step2 = app.host_action(model).await.expect("model card action");
    assert!(step2.card.is_none(), "selection is toast-only");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.model.as_deref(), Some("opencode/deepseek-v4-flash"));
}

/// The model picker's back button (level `provider`, `__providers__`)
/// returns to the full provider list, now carrying the session's current
/// model line.
#[tokio::test]
async fn model_picker_back_button_returns_to_providers() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![
        crate::opencode::types::ProviderModels {
            provider: "opencode".into(),
            models: vec![model_option("deepseek-v4-flash", &[])],
        },
        crate::opencode::types::ProviderModels {
            provider: "openrouter".into(),
            models: vec![model_option("gpt-4o", &[])],
        },
    ]);
    let (app, _platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("openrouter/gpt-4o".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    let value = serde_json::json!({
        "action": "model",
        "level": "provider",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "__providers__",
    });
    let result = app.host_action(value).await.expect("back action");
    let card = result.card.expect("back returns a card");
    let text = card.to_string();
    assert!(text.contains("opencode") && text.contains("openrouter"), "{text}");
    assert!(
        text.contains("当前模型") && text.contains("openrouter/gpt-4o"),
        "rebuilt provider list carries the current model: {text}"
    );
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
    backend.with_agents(vec![
        crate::opencode::types::AgentInfo {
            name: "sec-agent".into(),
            description: Some("a subagent".into()),
            mode: Some("subagent".into()),
            hidden: Some(false),
        },
        crate::opencode::types::AgentInfo {
            name: "build".into(),
            description: Some("build agent".into()),
            mode: Some("primary".into()),
            hidden: Some(false),
        },
        crate::opencode::types::AgentInfo {
            name: "default".into(),
            description: Some("an agent literally named default".into()),
            mode: Some("primary".into()),
            hidden: Some(false),
        },
    ]);
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

    send_command_in(
        &app,
        "/agent",
        key.clone(),
        "msg_agent_card",
        crate::config::ConversationKind::P2p,
    )
    .await;
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
    let buttons = card_buttons(&card);
    assert!(
        buttons.iter().any(|b| b["value"]["action"] == "agent_clear"),
        "clear action: {text}"
    );
    assert!(
        buttons.iter().any(|b| b["value"]["value"] == "default"),
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
        .host_action(literal_default)
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
    let result = app.host_action(value).await.expect("agent card action");
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
    app.host_action(clear).await.expect("agent clear action");
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

    send_command_in(
        &app,
        "/agent --reset",
        key.clone(),
        "msg_agent_reset",
        crate::config::ConversationKind::P2p,
    )
    .await;
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
    send_command_in(
        &app,
        "/agent build",
        key.clone(),
        "msg_agent_set",
        crate::config::ConversationKind::P2p,
    )
    .await;
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

    send_command_in(
        &app,
        "/autoaccept",
        key.clone(),
        "msg_aa",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .next()
        .expect("an autoaccept card should be sent");
    assert!(card_text(&card).contains("自动审批"), "toggle card: {card}");

    let value = serde_json::json!({
        "action": "autoaccept",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "on",
    });
    let result = app.host_action(value).await.expect("autoaccept card action");
    assert!(result.card.is_some(), "refreshed card returned");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert!(entry.auto_accept, "flag should flip on");

    // ... and back off. Clearing writes through the settings path (`update`),
    // never through activation, so the re-adoption carry-over rule (which
    // reads `false` as "unspecified") can never resurrect it.
    let value = serde_json::json!({
        "action": "autoaccept",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "off",
    });
    app.host_action(value).await.expect("autoaccept off card action");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert!(!entry.auto_accept, "flag should flip off");
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

    send_command(&app, "/name 新名字", "msg_name").await;

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
    let mut backend = MockBackend::new(realistic_parts());
    // The server title after the PATCH (the mock records the call but the
    // GET /session/{id} response comes from this map).
    backend.with_session_title("ses_test", "新名字");
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

    send_command_in(
        &app,
        "/name 新名字",
        crate::config::ThreadKey::new("chat_1".into(), "omt_t_1".into()),
        "msg_name",
        crate::config::ConversationKind::Topic,
    )
    .await;

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
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_title("ses_test", "New session - 2024-12-14T05:33:00.000Z");
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

    let settled = crate::bridge::topic::sync_topic_cover_title(
        &app.cards_handle(),
        &app.sessions_handle(),
        &app.opencode,
        "ses_test",
    )
    .await;

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
    crate::bridge::topic::spawn_cover_title_retry_at(
        &app.cards_handle(),
        &app.sessions_handle(),
        &app.opencode,
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

    send_command(&app, "/model opencode-go/deepseek-v4-flash", "msg_model").await;

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

    send_command_in(
        &app,
        "/think high",
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await;

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
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }]);
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

    send_command_in(
        &app,
        "/think medium",
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await;

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

    send_command_in(
        &app,
        "/think --reset",
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await;

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
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }]);
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

    send_command_in(
        &app,
        "/think default",
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await;
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
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "default"])],
    }]);
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

    send_command_in(
        &app,
        "/think default",
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await;
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
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }]);
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

    send_command_in(
        &app,
        "/think",
        key.clone(),
        "msg_think_card",
        crate::config::ConversationKind::P2p,
    )
    .await;

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
    let buttons = card_buttons(&card);
    assert!(
        buttons.iter().any(|b| b["value"]["value"] == "low"),
        "variant low: {text}"
    );
    assert!(
        buttons.iter().any(|b| b["value"]["value"] == "high"),
        "variant high: {text}"
    );
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
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }]);
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
    let result = app.host_action(value).await.expect("think card action");
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
    app.host_action(literal_default)
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
    app.host_action(clear).await.expect("think clear action");
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
    backend.with_models(vec![
        crate::opencode::types::ProviderModels {
            provider: "opencode-go".into(),
            models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
        },
        crate::opencode::types::ProviderModels {
            provider: "openrouter".into(),
            models: vec![model_option("other-model", &["low"])],
        },
    ]);
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
    send_command_in(
        &app,
        "/model opencode-go/deepseek-v4-flash",
        key.clone(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
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
    send_command_in(
        &app,
        "/model openrouter/other-model",
        key.clone(),
        "msg_model2",
        crate::config::ConversationKind::P2p,
    )
    .await;
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
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }]);
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

    send_command_in(
        &app,
        "/model openrouter/other-model",
        key.clone(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
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

    send_command(&app, "/model not-a-model", "msg_model").await;

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
/// submit path, not just a fresh Turn's prompt.
#[tokio::test]
async fn model_override_flows_through_supplement_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let async_models = backend.prompt_models.clone();
    let async_calls = backend.prompt_calls.clone();
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

    assert_eq!(async_calls.lock().await.as_slice(), &["补充内容".to_string()]);
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

    send_command(&app, "/agent primary", "msg_agent").await;

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
/// submit path, not just a fresh Turn's prompt.
#[tokio::test]
async fn agent_override_flows_through_supplement_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let async_agents = backend.prompt_agents.clone();
    let async_calls = backend.prompt_calls.clone();
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

    assert_eq!(async_calls.lock().await.as_slice(), &["补充内容".to_string()]);
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

    send_command(&app, "/model opencode-go/deepseek-v4-flash", "msg_model").await;

    // A freshly-loaded store (as after a restart) still has the override.
    let reloaded = crate::bridge::session::SessionStore::new(dir.path().join("sessions.json")).unwrap();
    let entry = reloaded
        .entry_for_session("ses_test")
        .expect("session mapping survives reload");
    assert_eq!(entry.model.as_deref(), Some("opencode-go/deepseek-v4-flash"));
}

/// A durable-generation `/model` pick (V2): the switch carries the whole model
/// ref, the surviving variant is judged from the SESSION's selection (not a
/// stale local mirror), and a model that does not declare it clears it in the
/// same switch (ADR-0020).
#[tokio::test]
async fn model_pick_switches_the_session_and_carries_the_servers_variant() {
    use crate::opencode::types::{ModelInfo, SessionSelection};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![
            model_option("alt-with-variants", &["low", "high"]),
            model_option("plain", &[]),
        ],
    }]);
    backend.with_session_selection(
        "ses_test",
        SessionSelection {
            model: Some(ModelInfo {
                id: "base".into(),
                provider_id: "opencode-go".into(),
                variant: Some("high".into()),
            }),
            agent: None,
        },
    );
    let switch_calls = backend.switch_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    // The local mirror disagrees with the server: the server has the variant,
    // the mirror does not. The pick must judge and carry the SERVER's variant.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/base".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    send_command_in(
        &app,
        "/model opencode-go/alt-with-variants",
        key.clone(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let calls = switch_calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "one durable switch");
    assert_eq!(calls[0].0, "ses_test");
    assert_eq!(
        calls[0].1,
        ModelInfo {
            id: "alt-with-variants".into(),
            provider_id: "opencode-go".into(),
            variant: Some("high".into()),
        },
        "the switch carries the server's surviving variant inside the ref"
    );
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.model.as_deref(), Some("opencode-go/alt-with-variants"));
    assert_eq!(entry.variant.as_deref(), Some("high"), "the mirror follows");

    // A model that declares no variant clears it, and the switch carries none.
    send_command_in(
        &app,
        "/model opencode-go/plain",
        key.clone(),
        "msg_model2",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let calls = switch_calls.lock().await.clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].0, "ses_test");
    assert_eq!(calls[1].1.id, "plain");
    assert_eq!(calls[1].1.variant, None, "the cleared variant is not sent");
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert!(entry.variant.is_none());
    let text = platform.texts().await.join("\n");
    assert!(text.contains("已清除思考等级 `high`"), "clear feedback: {text}");
}

/// A durable-generation `/think` pick (V2) rewrites the session's model ref
/// with the variant inside it; `--reset` rewrites it without one.
#[tokio::test]
async fn think_pick_rewrites_the_model_ref_with_the_variant() {
    use crate::opencode::types::{ModelInfo, SessionSelection};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }]);
    backend.with_session_selection(
        "ses_test",
        SessionSelection {
            model: Some(ModelInfo {
                id: "deepseek-v4-flash".into(),
                provider_id: "opencode-go".into(),
                variant: None,
            }),
            agent: None,
        },
    );
    let switch_calls = backend.switch_model_calls.clone();
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

    send_command_in(
        &app,
        "/think high",
        key.clone(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let calls = switch_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].1,
        ModelInfo {
            id: "deepseek-v4-flash".into(),
            provider_id: "opencode-go".into(),
            variant: Some("high".into()),
        },
        "the variant rides the model ref"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .and_then(|e| e.variant.clone())
            .as_deref(),
        Some("high")
    );

    send_command_in(
        &app,
        "/think --reset",
        key.clone(),
        "msg_think_reset",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let calls = switch_calls.lock().await.clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].1.id, "deepseek-v4-flash");
    assert_eq!(calls[1].1.variant, None, "reset rewrites the ref without it");
}

/// On a durable generation (V2) the session DTO names a model only after an
/// explicit switch; with no selection and no configured default, `/think`
/// falls back to the model the session last actually ran with — what the
/// server's own next-turn resolution keeps using — instead of claiming no
/// current model on a working session.
#[tokio::test]
async fn think_card_falls_back_to_the_last_run_model_on_v2() {
    use crate::opencode::types::{ModelInfo, SessionSelection};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }]);
    backend.with_session_selection("ses_test", SessionSelection::default());
    backend.with_session_last_run_model(
        "ses_test",
        ModelInfo {
            id: "deepseek-v4-flash".into(),
            provider_id: "opencode-go".into(),
            variant: Some("high".into()),
        },
    );
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

    send_command_in(
        &app,
        "/think",
        key.clone(),
        "msg_think_last_run",
        crate::config::ConversationKind::P2p,
    )
    .await;

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
    assert!(
        text.contains("opencode-go/deepseek-v4-flash"),
        "the last-run model must be shown: {text}"
    );
    assert!(
        text.contains("当前思考等级") && text.contains("`high`"),
        "the variant the session ran with must be current: {text}"
    );
    let buttons = card_buttons(&card);
    assert!(
        buttons.iter().any(|b| b["value"]["value"] == "low"),
        "the model's variants must be offered: {text}"
    );
}

/// A `/think <variant>` pick on a durable generation with no session
/// selection resolves the last-run model and rewrites its ref with the
/// variant — setting the thinking level on the model in use instead of the
/// "pick a model" refusal.
#[tokio::test]
async fn think_pick_uses_the_last_run_model_on_v2() {
    use crate::opencode::types::{ModelInfo, SessionSelection};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "opencode-go".into(),
        models: vec![model_option("deepseek-v4-flash", &["low", "high"])],
    }]);
    backend.with_session_selection("ses_test", SessionSelection::default());
    backend.with_session_last_run_model(
        "ses_test",
        ModelInfo {
            id: "deepseek-v4-flash".into(),
            provider_id: "opencode-go".into(),
            variant: None,
        },
    );
    let switch_calls = backend.switch_model_calls.clone();
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

    send_command_in(
        &app,
        "/think high",
        key.clone(),
        "msg_think_last_run",
        crate::config::ConversationKind::P2p,
    )
    .await;

    assert_eq!(
        switch_calls.lock().await.as_slice(),
        &[(
            "ses_test".to_string(),
            ModelInfo {
                id: "deepseek-v4-flash".into(),
                provider_id: "opencode-go".into(),
                variant: Some("high".into()),
            },
        )],
        "the pick must rewrite the last-run model's ref with the variant"
    );
    let text = platform.texts().await.join("\n");
    assert!(text.contains("Thinking: high"), "pick feedback: {text}");
}

/// A failed last-run read degrades the ladder to "no current model" — a text
/// explanation, never a hang or a guessed model.
#[tokio::test]
async fn think_card_degrades_when_the_last_run_read_fails() {
    use crate::opencode::types::SessionSelection;

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection("ses_test", SessionSelection::default());
    backend.fail_session_last_run_model("last-run read is down");
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

    send_command_in(
        &app,
        "/think",
        key,
        "msg_think_last_run",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains("无法确定当前模型"),
        "a failed read must degrade to the pick-a-model prompt: {texts}"
    );
}

/// A durable-generation `/agent` pick (V2) switches the session's agent; the
/// reset switches to the server's default agent id (V2 has no unset arm).
#[tokio::test]
async fn agent_pick_switches_the_session_and_reset_switches_the_default() {
    use crate::opencode::types::{AgentInfo, SessionSelection};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_agents(vec![
        AgentInfo {
            name: "build".into(),
            description: None,
            mode: Some("primary".into()),
            hidden: Some(false),
        },
        AgentInfo {
            name: "explore".into(),
            description: None,
            mode: Some("subagent".into()),
            hidden: Some(false),
        },
    ]);
    backend.with_session_selection("ses_test", SessionSelection::default());
    let switch_calls = backend.switch_agent_calls.clone();
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

    send_command_in(
        &app,
        "/agent explore",
        key.clone(),
        "msg_agent",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(
        switch_calls.lock().await.as_slice(),
        &[("ses_test".to_string(), "explore".to_string())]
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .and_then(|e| e.agent.clone())
            .as_deref(),
        Some("explore")
    );

    send_command_in(
        &app,
        "/agent --reset",
        key.clone(),
        "msg_agent_reset",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let calls = switch_calls.lock().await.clone();
    assert_eq!(
        calls,
        vec![
            ("ses_test".to_string(), "explore".to_string()),
            ("ses_test".to_string(), "build".to_string()),
        ],
        "reset switches to the server's default agent id"
    );
    assert!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .and_then(|e| e.agent.clone())
            .is_none(),
        "the mirror clears"
    );
}

/// A durable-generation `/agent --reset` with no server default to resolve
/// writes nothing and reports it, rather than claiming a clear that did not
/// land on the session.
#[tokio::test]
async fn agent_reset_refuses_when_no_default_agent_resolves() {
    use crate::opencode::types::SessionSelection;

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // No agents listed: the default cannot be resolved.
    backend.with_session_selection("ses_test", SessionSelection::default());
    let (app, platform) = build_app(cfg, backend).await;
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

    send_command_in(
        &app,
        "/agent --reset",
        key.clone(),
        "msg_agent_reset",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("无法确定服务器默认 Agent"),
        "the refusal is visible: {text}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .and_then(|e| e.agent.clone())
            .as_deref(),
        Some("build"),
        "a refused reset must not clear the mirror"
    );
}

/// A failed session switch never mirrors the pick: the local settings stay as
/// they were and the failure is reported.
#[tokio::test]
async fn a_failed_session_switch_leaves_the_mirror_untouched() {
    use crate::opencode::types::SessionSelection;

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection("ses_test", SessionSelection::default());
    backend.fail_session_switch("switch boom");
    let switch_calls = backend.switch_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("opencode-go/old-model".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    send_command_in(
        &app,
        "/model opencode-go/new-model",
        key.clone(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let text = platform.texts().await.join("\n");
    assert!(text.contains("切换模型失败"), "failure feedback: {text}");
    assert!(
        switch_calls.lock().await.is_empty(),
        "the mock fails before recording"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .and_then(|e| e.model.clone())
            .as_deref(),
        Some("opencode-go/old-model"),
        "a failed switch must not change the mirror"
    );
}

/// The effective-model ladder prefers the SESSION's durable selection over the
/// local mirror: the `/model` card names it, `/think` validates against its
/// variants, and `/think` rewrites that same ref.
#[tokio::test]
async fn the_ladder_reads_the_servers_selection_over_the_local_mirror() {
    use crate::opencode::types::{ModelInfo, SessionSelection};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "p".into(),
        models: vec![
            model_option("server-model", &["high"]),
            model_option("local-model", &["low"]),
        ],
    }]);
    backend.with_session_selection(
        "ses_test",
        SessionSelection {
            model: Some(ModelInfo {
                id: "server-model".into(),
                provider_id: "p".into(),
                variant: Some("high".into()),
            }),
            agent: None,
        },
    );
    let switch_calls = backend.switch_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("p/local-model".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    send_command_in(
        &app,
        "/model",
        key.clone(),
        "msg_model_card",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("a model card should be sent");
    assert!(
        card.contains("p/server-model@high"),
        "the card names the server's selection: {card}"
    );

    // `/think low` is rejected: the SERVER's model does not declare it.
    send_command_in(
        &app,
        "/think low",
        key.clone(),
        "msg_think_low",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("不支持思考等级 `low`"),
        "validation reads the server's model: {text}"
    );

    // `/think high` rewrites the server model's ref with the variant.
    send_command_in(
        &app,
        "/think high",
        key.clone(),
        "msg_think_high",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let calls = switch_calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "only the accepted pick switches");
    assert_eq!(calls[0].1.id, "server-model");
    assert_eq!(calls[0].1.variant.as_deref(), Some("high"));
}

/// On a durable generation a failed selection read is NEVER a licence to
/// confirm a mirror-only write: `/think` reports the unknown state, keeps the
/// mirror, and touches no session.
#[tokio::test]
async fn a_failed_durable_read_never_confirms_a_think_pick() {
    use crate::opencode::types::{ModelInfo, SessionSelection};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "p".into(),
        models: vec![model_option("server-model", &["high"])],
    }]);
    backend.with_session_selection(
        "ses_test",
        SessionSelection {
            model: Some(ModelInfo {
                id: "server-model".into(),
                provider_id: "p".into(),
                variant: None,
            }),
            agent: None,
        },
    );
    backend.fail_session_selection("selection boom");
    let switch_calls = backend.switch_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("p/server-model".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let (_, logs) = capture_logs(async {
        send_command_in(
            &app,
            "/think high",
            key.clone(),
            "msg_think",
            crate::config::ConversationKind::P2p,
        )
        .await;
    })
    .await;

    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("无法确定当前模型"),
        "the unknown state is reported: {text}"
    );
    assert!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .and_then(|e| e.variant.clone())
            .is_none(),
        "the mirror must stay untouched"
    );
    assert!(
        switch_calls.lock().await.is_empty(),
        "nothing may be switched on an unreadable selection"
    );
    assert_line_level(&logs, "durable selection read failed", "WARN");
}

/// The same failed read must not present the local mirror as the session's
/// selection: the `/model` card omits its current line, and a pick drops the
/// unknown variant (the switch replaces the ref wholesale) instead of reviving
/// the mirror's.
#[tokio::test]
async fn a_failed_durable_read_hides_the_mirror_and_drops_an_unknown_variant() {
    use crate::opencode::types::{ModelInfo, SessionSelection};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "p".into(),
        models: vec![
            model_option("new-model", &["high"]),
            model_option("local-model", &["high"]),
        ],
    }]);
    backend.with_session_selection(
        "ses_test",
        SessionSelection {
            model: Some(ModelInfo {
                id: "server-model".into(),
                provider_id: "p".into(),
                variant: Some("high".into()),
            }),
            agent: None,
        },
    );
    backend.fail_session_selection("selection boom");
    let switch_calls = backend.switch_model_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/aa".into(),
            agent: None,
            model: Some("p/local-model".into()),
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: Some("high".into()),
        },
    )
    .await;

    send_command_in(
        &app,
        "/model",
        key.clone(),
        "msg_model_card",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("a model card should be sent");
    assert!(
        !card.contains("p/local-model"),
        "the mirror must not be shown as the session's selection: {card}"
    );
    assert!(!card.contains("当前模型"), "no current-model line: {card}");

    // A pick still switches (the server accepts a new ref); the unknown
    // variant is dropped rather than taken from the mirror.
    send_command_in(
        &app,
        "/model p/new-model",
        key.clone(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let calls = switch_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.id, "new-model");
    assert_eq!(
        calls[0].1.variant, None,
        "an unreadable selection means the variant is unknown, not the mirror's"
    );
    let entry = app.sessions.lock().await.get_active(&key).cloned().unwrap();
    assert_eq!(entry.model.as_deref(), Some("p/new-model"));
    assert!(entry.variant.is_none());
}

/// The `/model` picker card's model button switches the session on a durable
/// generation, carrying the session's surviving variant (here the selection's
/// `high`), and the `/agent` picker's button switches the agent.
#[tokio::test]
async fn picker_buttons_switch_the_session_on_a_durable_generation() {
    use crate::opencode::types::{AgentInfo, ModelInfo, SessionSelection};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_models(vec![crate::opencode::types::ProviderModels {
        provider: "p".into(),
        models: vec![model_option("alt-model", &["low", "high"])],
    }]);
    backend.with_agents(vec![AgentInfo {
        name: "build".into(),
        description: None,
        mode: Some("primary".into()),
        hidden: Some(false),
    }]);
    backend.with_session_selection(
        "ses_test",
        SessionSelection {
            model: Some(ModelInfo {
                id: "server-model".into(),
                provider_id: "p".into(),
                variant: Some("high".into()),
            }),
            agent: None,
        },
    );
    let model_calls = backend.switch_model_calls.clone();
    let agent_calls = backend.switch_agent_calls.clone();
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

    let model = serde_json::json!({
        "action": "model",
        "level": "model",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "p/alt-model",
    });
    app.host_action(model).await.expect("model card action");
    let calls = model_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ses_test");
    assert_eq!(
        calls[0].1,
        ModelInfo {
            id: "alt-model".into(),
            provider_id: "p".into(),
            variant: Some("high".into()),
        },
        "the button pick carries the session's surviving variant"
    );

    let agent = serde_json::json!({
        "action": "agent",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": "build",
    });
    app.host_action(agent).await.expect("agent card action");
    assert_eq!(
        agent_calls.lock().await.as_slice(),
        &[("ses_test".to_string(), "build".to_string())]
    );
}
