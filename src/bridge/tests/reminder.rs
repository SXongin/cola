//! Instant Reminder pin/unpin lifecycle tests (ADR-0043): a pending
//! Permission/Question pins the Chat/Topic towards its turn's requester,
//! resolution clears it exactly once, and the `[bridge] instant_reminder`
//! opt-in is off unless explicitly enabled (#234). One deterministic owner per
//! Chat/Topic (#247) hands the pin over when a wait resolves; the pin set is
//! persisted and startup clears what a crash orphaned (#249).
//!
//! The long-task **Completion Notice** (ADR-0043 amendment 2026-09-21) is a
//! new message, not a pin: a p2p Turn that ran past the injected threshold
//! replies a notice to the prompt; a short p2p Turn is silent.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::bridge::test_support::*;
use crate::bridge::turn::{PromptContext, Turn};
use crate::config::ThreadKey;

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
/// turn's requester and generation. `is_group` picks the Chat/Topic kind.
async fn seed_turn(app: &Arc<App>, session_id: &str, is_group: bool, generation: u64) {
    seed_turn_for(app, session_id, is_group, generation, TEST_HOST).await
}

/// Like [`seed_turn`], with an explicit requester — the multi-pending tests
/// need distinct requesters in one Chat/Topic.
async fn seed_turn_for(app: &Arc<App>, session_id: &str, is_group: bool, generation: u64, requester: &str) {
    let cards = app.cards_handle();
    Turn::seed_card(&cards, session_id, Some("msg_card")).await;
    Turn::set_turn_identity(&cards, session_id, requester, is_group, generation).await;
}

/// A `[bridge] instant_reminder = true` app whose MockBackend serves one permission.
fn pinned_permission_app(
    cfg: &mut crate::config::Config,
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    cfg.bridge.instant_reminder = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![permission("per_1", "ses_1")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg.clone(), backend.clone(), platform.clone()).unwrap());
    (app, platform, backend)
}

/// A pending permission pins the Chat/Topic towards its turn's requester;
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
        "a pending permission pins the Chat/Topic towards its requester"
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
    cfg.bridge.instant_reminder = true;
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
        "a pending question pins the Chat/Topic"
    );

    backend.replied_questions.lock().await.insert("que_1".into());
    app.question.sweep(&app.core, &mut seen).await;
    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 2, "one pin, one clear: {calls:?}");
    assert!(calls[0].3 && !calls[1].3);
}

/// A group Chat/Topic's pin targets the group chat (`is_group = true`), the
/// branch that PATCHes `feed_cards/{chat_id}`.
#[tokio::test]
async fn a_group_pin_targets_its_chat() {
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

/// The opt-in default: with `[bridge] instant_reminder` unset/false, the same pending
/// lifecycle records no reminder call at all (an upgrade never changes
/// notification behavior).
#[tokio::test]
async fn pin_off_records_no_reminder_calls() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    assert!(!cfg.bridge.instant_reminder, "test_config is the off default");
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
        Turn::has_interaction_in(&app.cards_handle(), "ses_1", "per_1").await,
        "the permission still surfaces without pinning"
    );

    backend.replied_permissions.lock().await.insert("per_1".into());
    app.permission.sweep(&app.core, &mut seen).await;

    assert!(
        platform.reminders().await.is_empty(),
        "instant_reminder = false must make no Instant Reminder call"
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
    cfg.bridge.instant_reminder = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![permission("per_1", "ses_1")];
    let backend = Arc::new(backend);
    let platform = RecordingPlatform::new();
    platform.fail_instant_reminder.store(true, Ordering::SeqCst);
    let platform = Arc::new(platform);
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;
    seed_turn(&app, "ses_1", false, 1).await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;

    // The attempt was made and failed, but the card carries the live block.
    assert_eq!(platform.reminders().await.len(), 1);
    assert!(
        Turn::has_interaction_in(&app.cards_handle(), "ses_1", "per_1").await,
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
/// is never a wait, so the Chat/Topic must not pin.
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

/// A turn start registers the Chat/Topic's generation and self-heals a
/// possible startup orphan once — a reminder call on the first turn only.
#[tokio::test]
async fn a_turn_start_self_heals_a_possible_orphan_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
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

/// #247: one Chat/Topic has one deterministic reminder owner. The newest
/// pending wins across both flows, and when it resolves the next pending —
/// either kind — takes the pin in that same sweep.
#[tokio::test]
async fn the_newest_pending_owns_the_pin_and_hands_over_on_resolution() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.permissions = vec![permission("per_1", "ses_perm")];
    backend.questions = vec![question("que_1", "ses_ques")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_perm", "/work").await;
    seed_session(&app, "ses_ques", "/work").await;
    seed_turn_for(&app, "ses_perm", true, 5, "ou_perm").await;
    seed_turn_for(&app, "ses_ques", true, 7, "ou_ques").await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    assert_eq!(
        platform.reminders().await,
        vec![("chat_1".to_string(), true, vec!["ou_perm".to_string()], true)],
        "the only wait pins"
    );

    // The newer wait (generation 7) takes the pin: release the old requester,
    // then pin the new one.
    app.question.sweep(&app.core, &mut seen).await;
    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 3, "retarget: {calls:?}");
    assert!(!calls[1].3 && calls[1].2 == vec!["ou_perm".to_string()]);
    assert!(calls[2].3 && calls[2].2 == vec!["ou_ques".to_string()]);

    // The newer wait resolves: the older pending takes the pin back in the
    // same sweep — no pause, no gap where the chat is silently unpinned.
    backend.replied_questions.lock().await.insert("que_1".into());
    app.question.sweep(&app.core, &mut seen).await;
    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 5, "handover: {calls:?}");
    assert!(!calls[3].3 && calls[3].2 == vec!["ou_ques".to_string()]);
    assert!(calls[4].3 && calls[4].2 == vec!["ou_perm".to_string()]);

    // The last wait resolves: the reminder clears.
    backend.replied_permissions.lock().await.insert("per_1".into());
    app.permission.sweep(&app.core, &mut seen).await;
    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 6, "the resolution unpins: {calls:?}");
    assert!(!calls[5].3, "the last wait's resolution unpins");
}

/// #249: the pin set is persisted beside the session file, a confirmed clear
/// drops the record, and a fresh instance's startup sweep clears what a
/// crashed process left behind.
#[tokio::test]
async fn the_pin_set_is_persisted_beside_the_session_file() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let mut cfg = test_config(&session_file);
    let (app, _platform, backend) = pinned_permission_app(&mut cfg);
    seed_session(&app, "ses_1", "/work").await;
    seed_turn(&app, "ses_1", false, 1).await;

    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.core, &mut seen).await;
    let pins_file = dir.path().join("pinned_chats.json");
    let raw = std::fs::read_to_string(&pins_file).expect("the pin landed on disk");
    assert!(
        raw.contains("chat_1") && raw.contains(TEST_HOST),
        "the file records the (chat, user) key: {raw}"
    );

    // Resolution drops the record.
    backend.replied_permissions.lock().await.insert("per_1".into());
    app.permission.sweep(&app.core, &mut seen).await;
    assert!(!pins_file.exists(), "the confirmed clear removes the record");

    // A crash left one behind: a fresh instance's startup sweep clears it.
    std::fs::write(
        &pins_file,
        r#"{"pins":[{"chat_id":"oc_crashed","is_group":true,"user_ids":["ou_host"]}]}"#,
    )
    .unwrap();
    let restarted_platform = Arc::new(RecordingPlatform::new());
    let restarted = Arc::new(
        App::new(
            cfg,
            Arc::new(MockBackend::new(realistic_parts())),
            restarted_platform.clone(),
        )
        .unwrap(),
    );
    restarted.core.reminder.clear_orphans(&restarted.feishu).await;
    let calls = restarted_platform.reminders().await;
    assert_eq!(calls.len(), 1, "the startup sweep clears the orphan: {calls:?}");
    assert!(!calls[0].3 && calls[0].0 == "oc_crashed");
    assert!(!pins_file.exists(), "the cleared record is dropped");
}

/// The turn a Completion Notice test drives: p2p chat 1, addressed to
/// `TEST_HOST` so the notice has a requester (and the first turn self-heals a
/// possible orphan).
fn turn_ctx(session_id: &str) -> PromptContext {
    PromptContext {
        session_id: session_id.into(),
        thread_key: ThreadKey::new("chat_1".into(), "chat_1".into()),
        text: "hi".into(),
        message_id: "msg_1".into(),
        subtitle: "p2p".into(),
        existing_card_id: None,
        requester_open_id: Some(TEST_HOST.into()),
        is_group: false,
        cola_message_id: None,
        images: Vec::new(),
    }
}

/// ADR-0043 amendment 2026-09-21: a p2p Turn that ran past the long-task
/// threshold replies a Completion Notice — a new message, because the card
/// patch pushes no notification and does not bump the conversation. p2p needs
/// no @ mention: the reply itself notifies.
#[tokio::test]
async fn a_long_p2p_turn_sends_a_completion_notice() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.long_task_notice = true;
    // The gate holds the prompt in flight past the injected threshold.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_gate = Some(gate.clone());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.long_task_notice_ms.store(5, Ordering::Relaxed);

    let turn = {
        let app = Arc::clone(&app);
        tokio::spawn(async move { Turn::run(&app.turn_handles(), turn_ctx("ses_test")).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    gate.add_permits(1);
    turn.await.unwrap().unwrap();

    let notices = platform.completion_notices().await;
    assert_eq!(notices.len(), 1, "one notice: {notices:?}");
    assert_eq!(notices[0].0, "msg_1", "it replies to the prompt");
    assert_eq!(notices[0].1, TEST_HOST);
    assert_eq!(notices[0].2, None, "p2p needs no @ mention");
    assert!(
        notices[0].3.contains("已完成"),
        "unexpected text: {}",
        notices[0].3
    );
}

/// A short p2p Turn is silent: the notice only marks a long task, so an
/// ordinary exchange never gets an extra message.
#[tokio::test]
async fn a_short_p2p_turn_sends_no_completion_notice() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.long_task_notice = true;
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.long_task_notice_ms.store(60_000, Ordering::Relaxed);

    Turn::run(&app.turn_handles(), turn_ctx("ses_test"))
        .await
        .unwrap();

    assert!(
        platform.completion_notices().await.is_empty(),
        "a short p2p turn must not send a notice"
    );
}

/// The opt-in default: with `[bridge] long_task_notice` unset/false, even a
/// long p2p Turn stays silent.
#[tokio::test]
async fn a_long_p2p_turn_without_the_opt_in_sends_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    assert!(!cfg.bridge.long_task_notice, "test_config is the off default");
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_gate = Some(gate.clone());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.long_task_notice_ms.store(5, Ordering::Relaxed);

    let turn = {
        let app = Arc::clone(&app);
        tokio::spawn(async move { Turn::run(&app.turn_handles(), turn_ctx("ses_test")).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    gate.add_permits(1);
    turn.await.unwrap().unwrap();

    assert!(
        platform.completion_notices().await.is_empty(),
        "long_task_notice = false must make no notice call"
    );
}
