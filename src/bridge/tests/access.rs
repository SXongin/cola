use crate::bridge::test_support::*;

/// While unclaimed, every message is refused with a pointer at the startup
/// log, and nothing reaches the backend.
#[tokio::test]
async fn unclaimed_bot_refuses_messages() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config_unclaimed(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("尚未认领")),
        "expected the unclaimed refusal, got: {texts:?}"
    );
    assert!(
        app.sessions.lock().await.all_entries().is_empty(),
        "a refused message must not create a session"
    );
}

/// An identity-less `/claim` with the right code fails closed: no Principal,
/// no Host.
#[tokio::test]
async fn identity_less_claim_is_refused() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config_unclaimed(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let code = app.access.lock().await.claim_code().unwrap().to_string();
    app.handle_message(incoming_anonymous(
        "msg_1".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        format!("/claim {code}"),
    ))
    .await;

    let texts = platform.texts().await;
    assert!(texts.iter().any(|t| t.contains("尚未认领")), "got: {texts:?}");
    assert!(
        app.access.lock().await.claim_code().is_some(),
        "an identity-less claim must not bind a Host"
    );
}

/// The Claim: a p2p `/claim <code>` binds the sender as Host, and from then
/// on the Host's messages reach the backend.
#[tokio::test]
async fn claim_from_p2p_binds_the_host_and_lets_them_work() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config_unclaimed(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    let code = app
        .access
        .lock()
        .await
        .claim_code()
        .expect("unclaimed bot has a claim code")
        .to_string();
    app.handle_message(incoming(
        "msg_claim".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        format!("/claim {code}"),
        Some("ou_alice".into()),
    ))
    .await;

    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("认领成功")),
        "expected the claim confirmation, got: {texts:?}"
    );

    // The Host works normally now — and in groups too.
    app.handle_message(incoming(
        "msg_2".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        Some("ou_alice".into()),
    ))
    .await;
    assert_eq!(
        prompts.lock().await.len(),
        1,
        "the Host's prompt must reach the backend"
    );
}

/// Once claimed, a Principal who is not the Host is refused and nothing
/// reaches the backend.
#[tokio::test]
async fn non_host_is_refused_after_claim() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_p2p_2".into(),
        "p2p".into(),
        None,
        "hi".into(),
        Some("ou_mallory".into()),
    ))
    .await;

    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("仅限机主使用")),
        "expected the private-bot refusal, got: {texts:?}"
    );
    assert!(
        prompts.lock().await.is_empty(),
        "a stranger's prompt must not run"
    );
    assert!(app.sessions.lock().await.all_entries().is_empty());
}

/// A message with no resolvable Principal fails closed.
#[tokio::test]
async fn identity_less_message_is_refused() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(incoming_anonymous(
        "msg_1".into(),
        "oc_p2p_3".into(),
        "p2p".into(),
        None,
        "hi".into(),
    ))
    .await;

    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("仅限机主使用")),
        "expected the private-bot refusal, got: {texts:?}"
    );
    assert!(prompts.lock().await.is_empty());
}

/// The Claim is served from p2p only: a group `/claim` — even with the right
/// code — is refused and the bot stays unclaimed.
#[tokio::test]
async fn claim_in_a_group_is_refused() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config_unclaimed(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let code = app.access.lock().await.claim_code().unwrap().to_string();
    app.handle_message(incoming(
        "msg_1".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        format!("/claim {code}"),
        Some("ou_alice".into()),
    ))
    .await;

    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("尚未认领")),
        "expected the unclaimed refusal, got: {texts:?}"
    );
    assert!(
        app.access.lock().await.claim_code().is_some(),
        "a group claim must not bind a Host"
    );
}

/// A wrong code leaves the bot unclaimed.
#[tokio::test]
async fn wrong_claim_code_leaves_the_bot_unclaimed() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config_unclaimed(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "/claim WRONGCODE".into(),
        Some("ou_alice".into()),
    ))
    .await;

    let texts = platform.texts().await;
    assert!(texts.iter().any(|t| t.contains("尚未认领")), "got: {texts:?}");
    assert!(
        app.access.lock().await.claim_code().is_some(),
        "a wrong code must not bind a Host"
    );
}

/// A `/claim` on an already-claimed bot is acknowledged and changes nothing.
#[tokio::test]
async fn claim_when_already_claimed_changes_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let access_file = cfg.bridge.access_file.clone();
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "/claim SOMETHING".into(),
        Some("ou_mallory".into()),
    ))
    .await;

    let texts = platform.texts().await;
    assert!(texts.iter().any(|t| t.contains("已被认领")), "got: {texts:?}");
    assert_eq!(
        crate::bridge::access::AccessList::load(&access_file).host(),
        Some(TEST_HOST),
        "the original Host must be untouched"
    );
}

/// The Claim survives a restart: a fresh app built on the same config loads
/// the Host from disk and mints no new code.
#[tokio::test]
async fn claim_survives_a_restart() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config_unclaimed(&dir.path().join("sessions.json"));

    let (app1, _p1) = build_app(cfg.clone(), MockBackend::new(realistic_parts())).await;
    let code = app1.access.lock().await.claim_code().unwrap().to_string();
    app1.handle_message(incoming(
        "msg_claim".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        format!("/claim {code}"),
        Some("ou_alice".into()),
    ))
    .await;
    drop(app1);

    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app2, _p2) = build_app(cfg, backend).await;
    assert!(
        app2.access.lock().await.claim_code().is_none(),
        "a claimed bot mints no code on restart"
    );
    app2.handle_message(incoming(
        "msg_2".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        Some("ou_alice".into()),
    ))
    .await;
    assert_eq!(
        prompts.lock().await.len(),
        1,
        "the Host from the persisted list must be admitted"
    );
}

/// The permission click every card-action gate test sends: the highest-stakes
/// button in cola (it runs tools on the machine), so "no action" is observable
/// on the backend's `reply_permission` record.
fn permission_click() -> serde_json::Value {
    serde_json::json!({
        "action": "perm",
        "reply": "once",
        "session_id": "ses_1",
        "request_id": "per_1",
        "perm_label": "✅ 已允许一次",
        "perm_color": "green",
        "perm_body": "bash",
    })
}

/// Once claimed, a click from anyone but the Host is refused: the ack carries
/// a refusal Toast and nothing reaches the backend.
#[tokio::test]
async fn non_host_card_action_is_refused() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let replies = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;

    let mut value = permission_click();
    value["operator_open_id"] = serde_json::json!("ou_mallory");
    let result = app.handle_card_action(value).await.expect("a refusal result");

    assert!(result.card.is_none(), "a refusal must not update the card");
    assert!(
        result.toast.clone().unwrap_or_default().contains("仅限机主使用"),
        "expected the private-bot refusal, got: {:?}",
        result.toast
    );
    assert!(
        replies.lock().await.is_empty(),
        "a stranger's click must not reach the backend"
    );
}

/// A click with no resolvable operator identity fails closed.
#[tokio::test]
async fn identity_less_card_action_is_refused() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let replies = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;

    let result = app
        .handle_card_action(permission_click())
        .await
        .expect("a refusal result");

    assert!(result.card.is_none(), "a refusal must not update the card");
    assert!(
        result.toast.clone().unwrap_or_default().contains("仅限机主使用"),
        "expected the private-bot refusal, got: {:?}",
        result.toast
    );
    assert!(
        replies.lock().await.is_empty(),
        "an identity-less click must not reach the backend"
    );
}

/// While unclaimed, every card click is refused the same way — even one that
/// would be the Host's after a Claim.
#[tokio::test]
async fn unclaimed_bot_refuses_card_actions() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config_unclaimed(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let replies = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;

    let mut value = permission_click();
    value["operator_open_id"] = serde_json::json!(TEST_HOST);
    let result = app.handle_card_action(value).await.expect("a refusal result");

    assert!(result.card.is_none(), "a refusal must not update the card");
    assert!(
        result.toast.clone().unwrap_or_default().contains("尚未认领"),
        "expected the unclaimed refusal, got: {:?}",
        result.toast
    );
    assert!(
        replies.lock().await.is_empty(),
        "an unclaimed cola must not act on a click"
    );
}

/// The Host's click is unchanged: it reaches the backend and returns the
/// flow's result.
#[tokio::test]
async fn host_card_action_proceeds() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let replies = backend.reply_permission_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_1", "/work").await;

    let result = app.host_action(permission_click()).await.expect("a result card");

    assert_eq!(result.toast.as_deref(), Some("已允许本次执行"));
    assert_eq!(
        replies.lock().await.clone(),
        vec![("per_1".to_string(), "once".to_string())],
        "the Host's click must reach the backend"
    );
}
