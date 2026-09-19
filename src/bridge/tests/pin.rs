//! Instant Reminder pin/unpin lifecycle tests (ADR-0043, ticket #234): a
//! pending Permission/Question pins the conversation towards its turn's
//! requester, resolution clears it exactly once, and the `[bridge] pin`
//! opt-in is off unless explicitly enabled.

use crate::bridge::test_support::*;

fn permission(request_id: &str, session_id: &str) -> opencode::types::PermissionRequest {
    opencode::types::PermissionRequest {
        request_id: request_id.into(),
        session_id: Some(session_id.into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
    }
}

fn question(id: &str, session_id: &str) -> opencode::types::QuestionRequest {
    opencode::types::QuestionRequest {
        id: id.into(),
        session_id: session_id.into(),
        questions: vec![opencode::types::QuestionInfo {
            question: "继续吗？".into(),
            header: "下一步".into(),
            options: vec![opencode::types::QuestionOption {
                label: "继续".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: None,
        }],
    }
}

/// Seed the turn state a pin needs: a live card whose accumulator carries the
/// turn's requester and generation. `is_group` picks the conversation kind.
async fn seed_turn(app: &Arc<App>, session_id: &str, is_group: bool, generation: u64) {
    let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
    acc.requester_open_id = Some(TEST_HOST.into());
    acc.is_group = is_group;
    acc.turn_generation = Some(generation);
    app.cards.lock().await.insert(
        session_id.to_string(),
        crate::bridge::streaming::CardSession::new(acc, Some("msg_card".into())),
    );
}

/// A `[bridge] pin = true` app whose MockBackend serves one permission.
fn pinned_permission_app(
    cfg: &mut crate::config::Config,
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    cfg.bridge.pin = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![permission("per_1", "ses_1")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg.clone(), backend.clone(), platform.clone()).unwrap());
    (app, platform, backend)
}

/// A pending permission pins the conversation towards its turn's requester;
/// resolution (another client resolves it) clears it — exactly once each.
#[tokio::test]
async fn a_pending_permission_pins_and_resolution_unpins_without_duplicates() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform, backend) = pinned_permission_app(&mut cfg);
    seed_session(&app, "ses_1", "/work").await;
    seed_turn(&app, "ses_1", false, 1).await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    assert_eq!(
        platform.reminders().await,
        vec![("chat_1".to_string(), false, vec![TEST_HOST.to_string()], true)],
        "a pending permission pins the conversation towards its requester"
    );

    // Seeing the same pending request again must not re-pin it.
    app.permission.sweep(&app.core, &mut seen).await;
    assert_eq!(
        platform.reminders().await.len(),
        1,
        "the pin lifecycle is idempotent: no duplicate reminder calls"
    );

    // Resolved elsewhere: the next complete sweep clears the pin.
    backend.replied_permissions.lock().await.insert("per_1".into());
    app.permission.sweep(&app.core, &mut seen).await;
    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 2, "one pin, one clear: {calls:?}");
    assert!(calls[0].3, "the first call pins");
    assert!(!calls[1].3, "the second call clears");
    assert_eq!(
        calls[1].2,
        vec![TEST_HOST.to_string()],
        "the clear targets the same requester"
    );

    // Nothing tracked any more: a later sweep makes no call.
    app.permission.sweep(&app.core, &mut seen).await;
    assert_eq!(platform.reminders().await.len(), 2);
}

/// The question flow drives the same lifecycle: a pending question pins, and
/// its resolution unpins.
#[tokio::test]
async fn a_pending_question_pins_and_resolution_unpins() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.pin = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.questions = vec![question("que_1", "ses_1")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_turn(&app, "ses_1", false, 1).await;

    let mut seen = std::collections::HashSet::new();
    app.question.sweep(&app.core, &mut seen).await;
    assert_eq!(
        platform.reminders().await,
        vec![("chat_1".to_string(), false, vec![TEST_HOST.to_string()], true)],
        "a pending question pins the conversation"
    );

    backend.replied_questions.lock().await.insert("que_1".into());
    app.question.sweep(&app.core, &mut seen).await;
    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 2, "one pin, one clear: {calls:?}");
    assert!(calls[0].3 && !calls[1].3);
}

/// A group conversation's pin targets the group chat (`is_group = true`), the
/// branch that PATCHes `feed_cards/{chat_id}`.
#[tokio::test]
async fn a_group_conversation_pins_its_chat() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform, _backend) = pinned_permission_app(&mut cfg);
    seed_session(&app, "ses_1", "/work").await;
    seed_turn(&app, "ses_1", true, 1).await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;

    assert_eq!(
        platform.reminders().await,
        vec![("chat_1".to_string(), true, vec![TEST_HOST.to_string()], true)]
    );
}

/// The opt-in default: with `[bridge] pin` unset/false, the same pending
/// lifecycle records no reminder call at all (an upgrade never changes
/// notification behavior).
#[tokio::test]
async fn pin_off_records_no_reminder_calls() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    assert!(!cfg.bridge.pin, "test_config is the off default");
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![permission("per_1", "ses_1")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_turn(&app, "ses_1", false, 1).await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    // The request still surfaced: only the pin is off.
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_1")
            .unwrap()
            .acc
            .interaction("per_1")
            .is_some(),
        "the permission still surfaces without pinning"
    );

    backend.replied_permissions.lock().await.insert("per_1".into());
    app.permission.sweep(&app.core, &mut seen).await;

    assert!(
        platform.reminders().await.is_empty(),
        "pin = false must make no Instant Reminder call"
    );
}

/// A failed pin (e.g. the `im:datasync.feed_card.time_sensitive:write` scope
/// missing) logs and never affects the turn or the card; a later sweep with
/// the request gone does not fabricate a clear for a pin that never landed.
#[tokio::test]
async fn a_failed_pin_never_affects_the_turn_and_does_not_clear() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.pin = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![permission("per_1", "ses_1")];
    let backend = Arc::new(backend);
    let mut platform = RecordingPlatform::new();
    platform.fail_instant_reminder = true;
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_turn(&app, "ses_1", false, 1).await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;

    // The attempt was made and failed, but the card carries the live block.
    assert_eq!(platform.reminders().await.len(), 1);
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_1")
            .unwrap()
            .acc
            .interaction("per_1")
            .is_some(),
        "the turn/card is unaffected by the pin failure"
    );

    backend.replied_permissions.lock().await.insert("per_1".into());
    app.permission.sweep(&app.core, &mut seen).await;
    assert_eq!(
        platform.reminders().await.len(),
        1,
        "a pin that never landed records nothing to clear"
    );
}

/// `/autoaccept` sessions answer a permission inside the sweep itself: there
/// is never a wait, so the conversation must not pin.
#[tokio::test]
async fn an_auto_accepted_permission_never_pins() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform, backend) = pinned_permission_app(&mut cfg);
    let mut entry = crate::config::SessionEntry::new(
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "ses_1",
        "/work",
    );
    entry.auto_accept = true;
    seed_entry(&app, entry).await;
    seed_turn(&app, "ses_1", false, 1).await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;

    assert_eq!(
        backend.reply_permission_calls.lock().await.as_slice(),
        &[("per_1".to_string(), "once".to_string())],
        "the auto-accepted permission was answered"
    );
    assert!(
        platform.reminders().await.is_empty(),
        "an auto-accepted request was never a wait: it must not pin"
    );
}

/// A turn start registers the conversation's generation and self-heals a
/// possible startup orphan once — a reminder call on the first turn only.
#[tokio::test]
async fn a_turn_start_self_heals_a_possible_orphan_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.pin = true;
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    app.handle_message(incoming(
        "msg_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "again".into(),
        None,
    ))
    .await;

    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 1, "the orphan self-heal runs once: {calls:?}");
    assert!(!calls[0].3, "it clears");
    assert_eq!(calls[0].0, "chat_1");
}
