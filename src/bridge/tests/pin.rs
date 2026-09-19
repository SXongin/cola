//! Instant Reminder pin/unpin lifecycle tests (ADR-0043): a pending
//! Permission/Question pins the Chat/Topic towards its turn's requester,
//! resolution clears it exactly once, and the `[bridge] instant_reminder` opt-in is off
//! unless explicitly enabled (#234). A Turn whose Chat/Topic stays silent past
//! the injected long-turn threshold pins too, and its completion keeps the pin
//! for the injected TTL before clearing it (#236); any inbound user activity
//! restarts the silence clock and releases a live long-turn hold (ADR-0043
//! amendment).

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::bridge::pin::{PinReason, PinTarget};
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
    let mut acc = crate::bridge::streaming::StreamAccumulator::new("test");
    acc.requester_open_id = Some(TEST_HOST.into());
    acc.is_group = is_group;
    acc.turn_generation = Some(generation);
    app.cards.lock().await.insert(
        session_id.to_string(),
        crate::bridge::streaming::CardSession::new(acc, Some("msg_card".into())),
    );
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

/// The turn a long-turn test drives: p2p chat 1, addressed to `TEST_HOST` so
/// the pin has a requester (and the first turn self-heals a possible orphan).
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

/// Await `predicate` over the recorded reminder calls, or panic after 5 s.
async fn wait_for_reminders<F>(platform: &RecordingPlatform, what: &str, predicate: F)
where
    F: Fn(&[(String, bool, Vec<String>, bool)]) -> bool,
{
    let wait = async {
        loop {
            if predicate(&platform.reminders().await) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .unwrap_or_else(|_| panic!("reminders never satisfied: {what}"));
}

/// A Turn held past the injected long-turn threshold pins the Chat/Topic;
/// its completion keeps the pin until the injected TTL elapses, then the TTL
/// clear lands — exactly one pin and one completion clear.
#[tokio::test]
async fn a_long_turn_pins_and_completion_keeps_it_for_the_ttl() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    // The gate holds the prompt in flight so the turn runs past the threshold.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_gate = Some(gate.clone());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.pins.long_turn_ms.store(20, Ordering::Relaxed);
    app.pins.long_turn_tick_ms.store(5, Ordering::Relaxed);
    app.pins.ttl_ms.store(800, Ordering::Relaxed);

    let turn = {
        let app = Arc::clone(&app);
        tokio::spawn(async move { Turn::run(&app, turn_ctx("ses_test")).await })
    };

    // The gated prompt keeps the turn running past the threshold: it pins.
    wait_for_reminders(&platform, "the long-turn pin", |calls| calls.iter().any(|c| c.3)).await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the pin fires while the turn is still running"
    );

    // Completion keeps the pin: the TTL clear must not have run yet.
    gate.add_permits(1);
    turn.await.unwrap().unwrap();
    let calls = platform.reminders().await;
    assert_eq!(
        calls,
        vec![
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], false),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], true),
        ],
        "the orphan self-heal, then the pin; the completion TTL keeps it up: {calls:?}"
    );

    // The TTL elapses: the pin clears exactly once.
    wait_for_reminders(&platform, "the TTL clear", |calls| {
        calls.iter().filter(|c| !c.3).count() >= 2
    })
    .await;
    let calls = platform.reminders().await;
    assert_eq!(calls.iter().filter(|c| c.3).count(), 1, "one pin only: {calls:?}");
    assert_eq!(
        calls.iter().filter(|c| !c.3).count(),
        2,
        "the orphan clear and the TTL clear: {calls:?}"
    );
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the turn's guard is released"
    );
}

/// The threshold measures user silence: a message sent mid-turn (here a
/// supplement through the real handler) restarts the clock and releases the
/// live pin, and only a fresh threshold of silence pins again. Every step is
/// awaited (`wait_for_reminders`), never raced against a real sleep, so a
/// loaded machine cannot make an "assert no pin yet" window flake.
#[tokio::test]
async fn an_interaction_defers_the_long_turn_pin() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_gate = Some(gate.clone());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.pins.long_turn_ms.store(400, Ordering::Relaxed);
    app.pins.long_turn_tick_ms.store(5, Ordering::Relaxed);
    app.pins.ttl_ms.store(800, Ordering::Relaxed);

    let turn = {
        let app = Arc::clone(&app);
        tokio::spawn(async move { Turn::run(&app, turn_ctx("ses_test")).await })
    };
    // The gated turn stays silent: the threshold pins it.
    wait_for_reminders(&platform, "the long-turn pin", |calls| calls.iter().any(|c| c.3)).await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the turn is still running while pinned"
    );

    // An inbound message mid-turn is a supplement: it restarts the silence
    // clock and releases the live pin right away (the deferral is observable
    // as this release; the fresh clock is covered by the unit tests).
    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充".into(),
        None,
    ))
    .await;
    assert!(
        backend
            .prompt_async_calls
            .lock()
            .await
            .iter()
            .any(|c| c == "ses_test:补充"),
        "the message landed as a supplement"
    );
    wait_for_reminders(&platform, "the interaction release", |calls| {
        calls.len() >= 3 && !calls.last().is_some_and(|c| c.3)
    })
    .await;

    // Renewed silence pins the still-running turn again.
    wait_for_reminders(&platform, "the re-pin after renewed silence", |calls| {
        calls.iter().filter(|c| c.3).count() >= 2
    })
    .await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the turn is still running while re-pinned"
    );

    // Completion keeps the pin for the TTL, then the TTL clear lands.
    gate.add_permits(1);
    turn.await.unwrap().unwrap();
    wait_for_reminders(&platform, "the completion TTL clear", |calls| {
        calls.iter().filter(|c| !c.3).count() >= 3
    })
    .await;
    let calls = platform.reminders().await;
    assert_eq!(
        calls,
        vec![
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], false),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], true),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], false),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], true),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], false),
        ],
        "orphan self-heal, pin, the supplement's release, the renewed-silence pin, the TTL clear: {calls:?}"
    );
}

/// A card action is user activity too: the real card-action handler releases
/// a live long-turn hold (a pending wait's hold would survive).
#[tokio::test]
async fn a_card_action_releases_a_live_long_turn_hold() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    // A live long-turn pin, as the checker would have left it.
    app.pins
        .ensure(
            &app.feishu,
            &PinTarget {
                chat_id: "chat_1".into(),
                is_group: false,
                user_ids: vec![TEST_HOST.into()],
                generation: 1,
            },
            PinReason::LongTurn,
        )
        .await;
    assert_eq!(platform.reminders().await.len(), 1, "the pin is up");

    app.handle_card_action(serde_json::json!({
        "action": "noop",
        "chat_id": "chat_1",
        "operator_open_id": TEST_HOST,
    }))
    .await;

    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 2, "the card action released the pin: {calls:?}");
    assert!(!calls[1].3, "the release is an unpin");
}

/// Normalize a real card-click `card.action.trigger` callback around `value`.
/// The button's value carries no `chat_id` — the real permission/question
/// payloads never do — so the Chat/Topic can only come from the callback
/// context (`open_chat_id`), exactly as Feishu delivers it.
fn normalized_card_click(value: serde_json::Value, chat_id: &str) -> serde_json::Value {
    let payload = serde_json::json!({
        "header": { "event_type": "card.action.trigger", "event_id": "e_card" },
        "event": {
            "action": { "tag": "button", "value": value },
            "context": { "open_message_id": "om_card", "open_chat_id": chat_id },
            "operator": { "open_id": TEST_HOST },
        },
    });
    crate::feishu::ws::extract_card_action_value(payload.to_string().as_bytes())
        .expect("the card action normalizes")
}

/// A real permission card's button value (`src/feishu/card/permission.rs`):
/// `action`/`reply`/`request_id`/`session_id`, no routing `chat_id`.
fn normalized_permission_click(request_id: &str, session_id: &str, chat_id: &str) -> serde_json::Value {
    normalized_card_click(
        serde_json::json!({
            "action": "perm",
            "reply": "once",
            "request_id": request_id,
            "session_id": session_id,
        }),
        chat_id,
    )
}

/// A real question option button's value (`src/feishu/card/question.rs`):
/// `action`/`reply`/`request_id`/`session_id` plus the chosen option, and no
/// routing `chat_id`.
fn normalized_question_click(request_id: &str, session_id: &str, chat_id: &str) -> serde_json::Value {
    normalized_card_click(
        serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": request_id,
            "session_id": session_id,
            "question_index": 0,
            "answer": "/a",
        }),
        chat_id,
    )
}

/// A permission click's payload carries no `chat_id`; the callback context's
/// `open_chat_id` must still count the click as user activity and release a
/// live long-turn hold (ADR-0043 amendment).
#[tokio::test]
async fn a_permission_click_releases_a_live_long_turn_hold() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.pins
        .ensure(
            &app.feishu,
            &PinTarget {
                chat_id: "chat_1".into(),
                is_group: false,
                user_ids: vec![TEST_HOST.into()],
                generation: 1,
            },
            PinReason::LongTurn,
        )
        .await;
    assert_eq!(platform.reminders().await.len(), 1, "the pin is up");

    let value = normalized_permission_click("per_1", "ses_1", "chat_1");
    assert!(
        value.get("chat_id").is_none(),
        "precondition: the permission payload has no chat_id"
    );
    app.handle_card_action(value).await;

    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 2, "the click released the pin: {calls:?}");
    assert!(!calls[1].3, "the release is an unpin");
    assert_eq!(calls[1].0, "chat_1");
}

/// The permission click's release is scoped to the long-turn hold: a pending
/// Permission's hold survives the click, and only its resolution unpins —
/// proving the click released the `LongTurn` reason.
#[tokio::test]
async fn a_permission_click_keeps_a_pending_hold() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let target = PinTarget {
        chat_id: "chat_1".into(),
        is_group: false,
        user_ids: vec![TEST_HOST.into()],
        generation: 1,
    };
    app.pins.ensure(&app.feishu, &target, PinReason::LongTurn).await;
    app.pins.ensure(&app.feishu, &target, PinReason::Pending).await;
    assert_eq!(platform.reminders().await.len(), 1, "the pin is up");

    app.handle_card_action(normalized_permission_click("per_1", "ses_1", "chat_1"))
        .await;
    assert_eq!(
        platform.reminders().await.len(),
        1,
        "Pending holds the pin: the click makes no unpin call"
    );

    // Resolving the wait releases the last hold — which only unpins because
    // the click already released the `LongTurn` reason.
    app.pins.clear(&app.feishu, "chat_1", 1, PinReason::Pending).await;
    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 2, "the pending resolution unpins: {calls:?}");
    assert!(!calls[1].3);
}

/// A question answer's payload carries no `chat_id` either; the callback
/// context's `open_chat_id` must still count the click as user activity and
/// release a live long-turn hold (ADR-0043 amendment).
#[tokio::test]
async fn a_question_click_releases_a_live_long_turn_hold() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.pins
        .ensure(
            &app.feishu,
            &PinTarget {
                chat_id: "chat_1".into(),
                is_group: false,
                user_ids: vec![TEST_HOST.into()],
                generation: 1,
            },
            PinReason::LongTurn,
        )
        .await;
    assert_eq!(platform.reminders().await.len(), 1, "the pin is up");

    let value = normalized_question_click("que_1", "ses_1", "chat_1");
    assert!(
        value.get("chat_id").is_none(),
        "precondition: the question payload has no chat_id"
    );
    app.handle_card_action(value).await;

    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 2, "the click released the pin: {calls:?}");
    assert!(!calls[1].3, "the release is an unpin");
    assert_eq!(calls[1].0, "chat_1");
}

/// The question click's release is scoped to the long-turn hold: a pending
/// Question's hold survives the click, and only its resolution unpins —
/// proving the click released the `LongTurn` reason.
#[tokio::test]
async fn a_question_click_keeps_a_pending_hold() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let target = PinTarget {
        chat_id: "chat_1".into(),
        is_group: false,
        user_ids: vec![TEST_HOST.into()],
        generation: 1,
    };
    app.pins.ensure(&app.feishu, &target, PinReason::LongTurn).await;
    app.pins.ensure(&app.feishu, &target, PinReason::Pending).await;
    assert_eq!(platform.reminders().await.len(), 1, "the pin is up");

    app.handle_card_action(normalized_question_click("que_1", "ses_1", "chat_1"))
        .await;
    assert_eq!(
        platform.reminders().await.len(),
        1,
        "Pending holds the pin: the click makes no unpin call"
    );

    app.pins.clear(&app.feishu, "chat_1", 1, PinReason::Pending).await;
    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 2, "the pending resolution unpins: {calls:?}");
    assert!(!calls[1].3);
}

/// A new Turn means the user is active again: the message that starts it
/// releases the previous long turn's completion-TTL hold right away. The TTL
/// is far beyond the test, so the unpin can only be the new turn's release,
/// never the old timer.
#[tokio::test]
async fn a_new_turn_releases_the_previous_turns_ttl_pin() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_gate = Some(gate.clone());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.pins.long_turn_ms.store(20, Ordering::Relaxed);
    app.pins.long_turn_tick_ms.store(5, Ordering::Relaxed);
    // The TTL is minutes away: the release below cannot be the timer.
    app.pins.ttl_ms.store(60_000, Ordering::Relaxed);

    // Turn 1 runs long and completes: its pin stays for the TTL.
    let first = {
        let app = Arc::clone(&app);
        tokio::spawn(async move { Turn::run(&app, turn_ctx("ses_test")).await })
    };
    wait_for_reminders(&platform, "turn 1's pin", |calls| calls.iter().any(|c| c.3)).await;
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    let calls = platform.reminders().await;
    assert_eq!(calls.len(), 2, "the orphan self-heal, then the pin: {calls:?}");

    // The next message starts a new short turn: it releases the TTL hold
    // immediately. The huge threshold keeps that turn from pinning again.
    app.pins.long_turn_ms.store(60_000, Ordering::Relaxed);
    Turn::run(&app, turn_ctx("ses_test")).await.unwrap();

    let calls = platform.reminders().await;
    assert_eq!(
        calls,
        vec![
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], false),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], true),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], false),
        ],
        "orphan self-heal, pin, then the new turn's release: {calls:?}"
    );
}

/// A short turn never pins: completion wins the race with the still-sleeping
/// threshold checker, which must exit as a no-op. The threshold is far above
/// any plausible mock-turn duration and the assertion sits well past it, so a
/// loaded machine cannot turn this into a race.
#[tokio::test]
async fn a_short_turn_never_pins() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.pins.long_turn_ms.store(600, Ordering::Relaxed);
    app.pins.long_turn_tick_ms.store(5, Ordering::Relaxed);
    app.pins.ttl_ms.store(50, Ordering::Relaxed);

    Turn::run(&app, turn_ctx("ses_test")).await.unwrap();

    // Clearly past the threshold: a checker that ignored completion would pin
    // here; the completed turn's checker has exited instead.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let calls = platform.reminders().await;
    assert!(
        calls.iter().all(|c| !c.3),
        "a short turn must never pin: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| !c.3),
        "the first turn still self-heals a possible startup orphan: {calls:?}"
    );
}

/// A completed long turn's TTL is generation-scoped: a newer turn releases
/// the old TTL hold at its start, pins on its own threshold, and the older
/// turn's still-sleeping timer must not unpin it.
#[tokio::test]
async fn a_stale_completion_ttl_never_clears_a_newer_turns_pin() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = MockBackend::new(realistic_parts());
    backend.prompt_gate = Some(gate.clone());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.pins.long_turn_ms.store(20, Ordering::Relaxed);
    app.pins.long_turn_tick_ms.store(5, Ordering::Relaxed);
    app.pins.ttl_ms.store(400, Ordering::Relaxed);

    // Turn 1 runs long and completes: its pin stays for the TTL.
    let first = {
        let app = Arc::clone(&app);
        tokio::spawn(async move { Turn::run(&app, turn_ctx("ses_test")).await })
    };
    wait_for_reminders(&platform, "turn 1's pin", |calls| calls.iter().any(|c| c.3)).await;
    gate.add_permits(1);
    first.await.unwrap().unwrap();
    // The prompt returns the gate permit on the way out; hold it so turn 2 is
    // held too (the test releases it explicitly).
    let held = gate
        .try_acquire()
        .expect("turn 1's gate permit is back after its prompt returned");

    // Turn 2 starts (a new generation): its begin releases turn 1's TTL hold,
    // then its own threshold pins again before turn 1's TTL fires.
    let second = {
        let app = Arc::clone(&app);
        tokio::spawn(async move { Turn::run(&app, turn_ctx("ses_test")).await })
    };
    // Turn 2's release and re-pin are awaited, not slept for; then past turn
    // 1's TTL (2× the TTL, so the stale clear has certainly fired): it must
    // leave turn 2's pin alone (the newest event is still turn 2's pin).
    wait_for_reminders(&platform, "turn 2's release and re-pin", |calls| calls.len() >= 4).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let calls = platform.reminders().await;
    assert_eq!(
        calls,
        vec![
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], false),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], true),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], false),
            ("chat_1".to_string(), false, vec![TEST_HOST.to_string()], true),
        ],
        "orphan clear, turn 1's pin, turn 2's release, turn 2's pin: {calls:?}"
    );
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "turn 2 is still running while pinned"
    );

    // Turn 2 completes: its own TTL clear releases the pin.
    drop(held);
    second.await.unwrap().unwrap();
    wait_for_reminders(&platform, "turn 2's TTL clear", |calls| {
        calls.iter().filter(|c| !c.3).count() >= 3
    })
    .await;
    let calls = platform.reminders().await;
    assert_eq!(
        calls.iter().filter(|c| c.3).count(),
        2,
        "turn 1's and turn 2's pins: {calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|c| !c.3).count(),
        3,
        "orphan clear, turn 2's release, turn 2's TTL clear: {calls:?}"
    );
}
