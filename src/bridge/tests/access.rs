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
