//! Waiting-card message pin tests (ADR-0043 amendment): a pending
//! Permission/Question pins the exact card carrying it into the Chat/Topic's
//! pinned-message list — the in-chat locator under the Instant Reminder's
//! list-level nudge. Off by default with `[bridge] instant_reminder`,
//! best-effort on failure, and never re-asserted while an unchanged wait
//! leaves a user's manual unpin alone.

use std::sync::atomic::Ordering;

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

/// Seed the live card a request would be inlined onto: the accumulator
/// carries the turn's requester and generation.
async fn seed_turn(app: &Arc<App>, session_id: &str, generation: u64) {
    let cards = app.cards_handle();
    Turn::seed_card(&cards, session_id, Some("msg_card")).await;
    Turn::set_turn_identity(&cards, session_id, TEST_HOST, false, generation).await;
}

/// An `[bridge] instant_reminder = true` app whose MockBackend serves one
/// permission.
fn pinned_permission_app(
    cfg: &mut crate::config::Config,
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    cfg.bridge.instant_reminder = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(permission("per_1", "ses_1"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg.clone(), backend.clone(), platform.clone()).unwrap());
    (app, platform, backend)
}

/// A standalone waiting card (no live card to inline onto) is pinned when it
/// is sent, and unpinned once the request leaves the pending list.
#[tokio::test]
async fn a_standalone_waiting_card_pins_and_resolution_unpins() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform, backend) = pinned_permission_app(&mut cfg);
    seed_session(&app, "ses_1", "/work").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        platform.message_pins().await,
        vec![("msg_sent".to_string(), true)],
        "the standalone permission card is pinned"
    );

    // Seeing the same pending request again must not re-pin it — a card cola
    // already pinned is never touched twice, so a manual unpin stays.
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(platform.message_pins().await.len(), 1);

    // Resolved elsewhere: the next complete sweep unpins exactly once.
    backend.permission_resolved_by_another("per_1").await;
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    let calls = platform.message_pins().await;
    assert_eq!(
        calls,
        vec![("msg_sent".to_string(), true), ("msg_sent".to_string(), false)],
        "one pin, one unpin: {calls:?}"
    );

    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(platform.message_pins().await.len(), 2, "nothing tracked any more");
}

/// A request inlined on a live streaming card pins THAT card (not a standalone
/// one), and the question flow drives the same lifecycle.
#[tokio::test]
async fn an_inlined_waiting_card_pins_the_live_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question(question("que_1", "ses_1"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_turn(&app, "ses_1", 1).await;

    let mut seen = std::collections::HashSet::new();
    app.question.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        platform.message_pins().await,
        vec![("msg_card".to_string(), true)],
        "the live card carrying the question block is pinned"
    );

    backend.question_resolved_by_another("que_1").await;
    app.question.sweep(&app.flow_handles(), &mut seen).await;
    let calls = platform.message_pins().await;
    assert_eq!(calls.len(), 2, "one pin, one unpin: {calls:?}");
    assert!(calls[0].1 && !calls[1].1);
    assert_eq!(calls[1].0, "msg_card");
}

/// A pending request claimed by a Session Snapshot (ADR-0028) pins the
/// snapshot card: whoever hosts the controls is the locator, whatever that
/// card's lifecycle. Claiming moves the pin off the standalone card.
#[tokio::test]
async fn a_snapshot_claimed_wait_pins_the_snapshot_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform, backend) = pinned_permission_app(&mut cfg);
    seed_session(&app, "ses_1", "/work").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        platform.message_pins().await,
        vec![("msg_sent".to_string(), true)],
        "the standalone card starts pinned"
    );

    app.core.snapshot_claims.lock().await.claim(
        "mid_snap",
        "已接管",
        "接管",
        &crate::bridge::snapshot::SnapshotData {
            session_id: "ses_1".into(),
            directory: "/work".into(),
            status: None,
            pending: vec![crate::bridge::request::kind::PendingRequest::Permission(
                permission("per_1", "ses_1"),
            )],
            pending_elsewhere: None,
            tail: Vec::new(),
            newest_user_anchor: None,
            newest_user_is_cola_authored: false,
        },
        None,
    );

    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    let calls = platform.message_pins().await;
    assert_eq!(
        calls,
        vec![
            ("msg_sent".to_string(), true),
            ("msg_sent".to_string(), false),
            ("mid_snap".to_string(), true),
        ],
        "the claim moves the pin to the snapshot card: {calls:?}"
    );

    backend.permission_resolved_by_another("per_1").await;
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    let calls = platform.message_pins().await;
    assert_eq!(
        calls.last(),
        Some(&("mid_snap".to_string(), false)),
        "resolution unpins the snapshot card: {calls:?}"
    );
}

/// `/autoaccept` sessions answer a permission inside the sweep itself: there
/// is never a wait, so no card is pinned.
#[tokio::test]
async fn an_auto_accepted_permission_never_pins() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform, _backend) = pinned_permission_app(&mut cfg);
    let mut entry = crate::config::SessionEntry::new(
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "ses_1",
        "/work",
    );
    entry.auto_accept = true;
    seed_entry(&app, entry).await;
    seed_turn(&app, "ses_1", 1).await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        platform.message_pins().await.is_empty(),
        "an auto-accepted request was never a wait: it must not pin"
    );
}

/// The opt-in default: with `[bridge] instant_reminder` off, the same pending
/// lifecycle records no message pin call at all.
#[tokio::test]
async fn pin_off_records_no_message_pins() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    assert!(!cfg.bridge.instant_reminder, "test_config is the off default");
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(permission("per_1", "ses_1"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        platform.message_pins().await.is_empty(),
        "instant_reminder = false must make no message pin call"
    );
}

/// A failed pin is best-effort: the card/turn is unaffected, the attempt is
/// logged, and a later resolution fabricates no unpin for a pin that never
/// landed.
#[tokio::test]
async fn a_failed_pin_does_not_fabricate_an_unpin() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(permission("per_1", "ses_1"));
    let backend = Arc::new(backend);
    let platform = RecordingPlatform::new();
    platform.fail_pin.store(true, Ordering::SeqCst);
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        platform.message_pins().await,
        vec![("msg_sent".to_string(), true)],
        "the attempt was made and failed"
    );

    backend.permission_resolved_by_another("per_1").await;
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    let calls = platform.message_pins().await;
    assert_eq!(
        calls.len(),
        1,
        "a pin that never landed records nothing to clear: {calls:?}"
    );
    assert!(calls[0].1, "no unpin call is fabricated");
}
