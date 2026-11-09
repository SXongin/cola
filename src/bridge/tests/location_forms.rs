//! Location Forms (ADR-0103, ticket #1017): a directory-owned MCP
//! elicitation is delivered once as its own card into the directory's newest
//! mapped Chat/Topic, answered/cancelled through the directory route, or
//! deferred with 「稍后」 without declining. Session-backed elicitations keep
//! their ordinary Question path; unmarked session-less requests keep #489's
//! WARN-and-skip.
//!
//! These tests drive the real request flow through the mock backend and the
//! recording platform: they assert the card that was sent where, the route the
//! reply took, the pin transitions, and what the next sweep does — never a
//! private registry or cache shape.

use std::sync::Arc;

use crate::bridge::test_support::*;

use crate::backend::types::{
    FormFieldKind, QuestionInfo, QuestionOption, QuestionRequest, UncollectableReason,
};
use crate::config::{SessionEntry, ThreadKey};

/// A Location Form with one option-only field (a click answers it, no typed
/// input), naming an asking server and message.
fn location_form(request_id: &str) -> QuestionRequest {
    QuestionRequest::location(
        request_id,
        "Context7 登录".into(),
        vec![QuestionInfo {
            key: "choice".into(),
            question: "是否现在登录？".into(),
            header: "登录".into(),
            kind: FormFieldKind::String,
            options: vec![QuestionOption {
                value: "yes".into(),
                label: "现在登录".into(),
                description: String::new(),
            }],
            custom: Some(false),
            ..Default::default()
        }],
        Some("context7".into()),
        Some("请先登录 Context7 再继续".into()),
        None,
    )
}

/// A Location Form whose only field is a free-text input (the primary
/// elicitation shape: a typed answer submitted through the card's form).
fn typed_location_form(request_id: &str) -> QuestionRequest {
    QuestionRequest::location(
        request_id,
        "Context7 登录".into(),
        vec![QuestionInfo {
            key: "token".into(),
            question: "令牌".into(),
            header: "令牌".into(),
            kind: FormFieldKind::String,
            custom: Some(true),
            ..Default::default()
        }],
        Some("context7".into()),
        Some("请输入访问令牌".into()),
        None,
    )
}

/// A plain session-backed form (the ordinary Question path).
fn session_form(request_id: &str, session_id: &str) -> QuestionRequest {
    QuestionRequest {
        id: request_id.into(),
        session_id: session_id.into(),
        title: "Questions".into(),
        questions: vec![QuestionInfo {
            key: "q0".into(),
            question: "继续吗？".into(),
            header: "继续".into(),
            kind: FormFieldKind::String,
            options: vec![QuestionOption {
                value: "yes".into(),
                label: "继续".into(),
                description: String::new(),
            }],
            custom: Some(false),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn entry(thread: &str, session_id: &str, directory: &str) -> SessionEntry {
    SessionEntry::new(
        ThreadKey::new(thread.into(), thread.into()),
        session_id,
        directory,
    )
}

fn topic_entry(thread_id: &str, session_id: &str, directory: &str, anchor: Option<&str>) -> SessionEntry {
    let mut entry = SessionEntry::new(
        ThreadKey::new("chat_1".into(), thread_id.into()),
        session_id,
        directory,
    );
    entry.topic_anchor = anchor.map(str::to_string);
    entry
}

/// The shared rig: a temp cwd and state dir (both kept alive for the test's
/// duration), the configured app, and its recording platform/backend.
struct Fixture {
    _cwd: tempfile::TempDir,
    _state: tempfile::TempDir,
    app: Arc<App>,
    platform: Arc<RecordingPlatform>,
    backend: Arc<MockBackend>,
}

async fn fixture(
    instant_reminder: bool,
    forms: &[(&str, QuestionRequest)],
    entries: &[SessionEntry],
    configure: impl FnOnce(&mut MockBackend),
) -> Fixture {
    let cwd = test_work_dir();
    let state = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&state.path().join("sessions.json"));
    cfg.bridge.instant_reminder = instant_reminder;
    let mut backend = MockBackend::new(realistic_parts());
    for (directory, form) in forms {
        backend.ask_question_in(directory, form.clone());
    }
    configure(&mut backend);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    for entry in entries {
        seed_entry(&app, entry.clone()).await;
    }
    Fixture {
        _cwd: cwd,
        _state: state,
        app,
        platform,
        backend,
    }
}

/// One sweep with a caller-owned `seen` (the poll loop's own set survives
/// sweeps; reusing it is what makes an in-process dismissal stable).
async fn sweep_with(fixture: &Fixture, seen: &mut std::collections::HashSet<String>) {
    fixture
        .app
        .question
        .sweep(&fixture.app.flow_handles(), seen)
        .await;
}

/// One sweep with a fresh `seen` (the standalone push-once safety net is the
/// persisted delivery record, not the loop's set).
async fn sweep(fixture: &Fixture) {
    let mut seen = std::collections::HashSet::new();
    sweep_with(fixture, &mut seen).await;
}

/// Every button value on a card whose `reply` matches.
fn button_with_reply(card: &serde_json::Value, reply: &str) -> Option<serde_json::Value> {
    fn walk(value: &serde_json::Value, reply: &str, found: &mut Option<serde_json::Value>) {
        match value {
            serde_json::Value::Object(map) => {
                if map
                    .get("value")
                    .and_then(|v| v.get("reply"))
                    .and_then(|v| v.as_str())
                    == Some(reply)
                {
                    *found = Some(map.get("value").cloned().unwrap());
                }
                for child in map.values() {
                    walk(child, reply, found);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    walk(child, reply, found);
                }
            }
            _ => {}
        }
    }
    let mut found = None;
    walk(card, reply, &mut found);
    found
}

/// Every card posted as a NEW message (send or reply), in call order.
async fn posted_cards(platform: &RecordingPlatform) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect()
}

/// The single card cola posted, panicking if there is not exactly one.
async fn posted_card(platform: &RecordingPlatform) -> serde_json::Value {
    let cards = posted_cards(platform).await;
    assert_eq!(cards.len(), 1, "expected exactly one posted card: {cards:?}");
    cards.into_iter().next().unwrap()
}

/// The chat a `SendCard` went to, when that is how the card was delivered.
async fn sent_to(platform: &RecordingPlatform) -> Option<String> {
    platform.calls.lock().await.iter().find_map(|call| match call {
        PlatformCall::SendCard { receive_id, .. } => Some(receive_id.clone()),
        _ => None,
    })
}

/// The message a `ReplyCard` replied to.
async fn replied_to(platform: &RecordingPlatform) -> Option<String> {
    platform.calls.lock().await.iter().find_map(|call| match call {
        PlatformCall::ReplyCard { reply_to, .. } => Some(reply_to.clone()),
        _ => None,
    })
}

/// The directory's newest mapped Chat/Topic receives the card, and a second
/// sweep (fresh `seen`) does not push it again.
#[tokio::test]
async fn a_location_form_is_pushed_once_to_the_newest_mapped_chat() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_1"))],
        &[
            entry("chat_old", "ses_old", "/work"),
            entry("chat_new", "ses_new", "/work"),
            entry("chat_elsewhere", "ses_else", "/elsewhere"),
        ],
        |_| {},
    )
    .await;

    sweep(&fixture).await;
    sweep(&fixture).await;

    assert_eq!(
        sent_to(&fixture.platform).await.as_deref(),
        Some("chat_new"),
        "the newest mapped Chat/Topic in the form's directory receives it"
    );
    assert_eq!(
        posted_cards(&fixture.platform).await.len(),
        1,
        "pushed exactly once"
    );
    let card = posted_card(&fixture.platform).await;
    let text = card_text(&card);
    for needle in [
        "context7",
        "请先登录 Context7 再继续",
        "/work",
        "整个目录",
        "其他目录",
    ] {
        assert!(text.contains(needle), "the card must name {needle:?}: {text}");
    }
    // Never the Instant Reminder, never a per-session wait.
    assert!(
        fixture.platform.reminders().await.is_empty(),
        "a Location Form takes no reminder"
    );
    assert!(!fixture.app.question.is_pending_for("ses_new").await);
    assert!(!fixture.app.question.is_pending_for("").await);
}

/// A Session mapped inside a Topic is reached through its topic anchor, not
/// the Chat lobby.
#[tokio::test]
async fn a_topic_location_form_replies_to_its_topic_anchor() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_topic"))],
        &[topic_entry("omt_topic", "ses_topic", "/work", Some("msg_anchor"))],
        |_| {},
    )
    .await;

    sweep(&fixture).await;

    assert_eq!(replied_to(&fixture.platform).await.as_deref(), Some("msg_anchor"));
    assert_eq!(
        fixture.platform.sent_cards().await.len(),
        0,
        "never posts into the lobby"
    );
}

/// A topic whose anchor cannot be resolved defers the push: no card, no
/// suppression, and the next sweep retries once the anchor is reachable.
#[tokio::test]
async fn an_unresolved_topic_anchor_defers_the_push_without_falling_back() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_deferred"))],
        &[topic_entry("omt_topic", "ses_topic", "/work", None)],
        |_| {},
    )
    .await;
    fixture
        .platform
        .no_topic_messages
        .store(true, std::sync::atomic::Ordering::SeqCst);

    sweep(&fixture).await;
    assert!(
        posted_cards(&fixture.platform).await.is_empty(),
        "an unresolvable topic anchor sends nothing — not into the lobby"
    );

    fixture
        .platform
        .no_topic_messages
        .store(false, std::sync::atomic::Ordering::SeqCst);
    sweep(&fixture).await;
    assert_eq!(
        replied_to(&fixture.platform).await.as_deref(),
        Some("msg_in_topic_anchor"),
        "the deferred push retries and lands in the topic"
    );
}

/// A hung topic-anchor read is bounded like every other sweep read: the push
/// defers instead of wedging the sweep, and the next sweep retries.
#[tokio::test]
async fn a_hung_topic_anchor_read_defers_the_push_without_wedging_the_sweep() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_hung"))],
        &[topic_entry("omt_topic", "ses_topic", "/work", None)],
        |_| {},
    )
    .await;
    fixture
        .app
        .question
        .list_timeout_ms
        .store(25, std::sync::atomic::Ordering::Relaxed);
    fixture
        .platform
        .hang_topic_messages
        .store(1, std::sync::atomic::Ordering::SeqCst);

    sweep(&fixture).await;
    assert!(
        posted_cards(&fixture.platform).await.is_empty(),
        "a timed-out target walk sends nothing"
    );

    sweep(&fixture).await;
    assert_eq!(
        replied_to(&fixture.platform).await.as_deref(),
        Some("msg_in_topic_anchor"),
        "the next sweep retries and lands"
    );
}

/// A form whose directory has no mapped Chat/Topic has no target: nothing is
/// sent and nothing suppressed. The sweep only lists mapped directories, so
/// this branch is otherwise reachable only as a listing→delivery race; the
/// test drives the real surface path directly, then proves a later mapping
/// still delivers (no suppression was recorded).
#[tokio::test]
async fn a_directory_without_a_mapped_chat_sends_nothing_and_suppresses_nothing() {
    use crate::bridge::request::kind::PendingRequest;

    let fixture = fixture(
        false,
        &[("/work", location_form("frm_unmapped"))],
        &[entry("chat_1", "ses_1", "/elsewhere")],
        |_| {},
    )
    .await;
    let handles = fixture.app.flow_handles();
    let req = PendingRequest::Question(location_form("frm_unmapped"));
    let mut seen = std::collections::HashSet::new();
    fixture
        .app
        .question
        .surface_location_form_for_test(&handles, &req, "/work", &mut seen)
        .await;

    assert!(
        posted_cards(&fixture.platform).await.is_empty(),
        "an unmapped directory must not guess a chat"
    );

    // The mapping appears: the next sweep must still deliver, proving the
    // first attempt recorded nothing and suppressed nothing.
    seed_entry(&fixture.app, entry("chat_1", "ses_1", "/work")).await;
    sweep(&fixture).await;
    assert_eq!(
        sent_to(&fixture.platform).await.as_deref(),
        Some("chat_1"),
        "a later mapping still delivers the form"
    );
}

/// A definitively failed send is not recorded, so the next complete sweep
/// retries it.
#[tokio::test]
async fn a_failed_location_form_push_retries_on_the_next_sweep() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_retry"))],
        &[topic_entry("omt_topic", "ses_topic", "/work", Some("msg_anchor"))],
        |_| {},
    )
    .await;
    fixture
        .platform
        .fail_reply_card_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    sweep(&fixture).await;
    assert!(posted_cards(&fixture.platform).await.is_empty());
    sweep(&fixture).await;
    assert_eq!(
        replied_to(&fixture.platform).await.as_deref(),
        Some("msg_anchor"),
        "the retry lands the card"
    );
}

/// Answering a Location Form routes through the directory path (never the
/// session route), carrying the directory whose list produced the form.
#[tokio::test]
async fn answering_routes_the_location_form_with_its_directory() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_answer"))],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;

    sweep(&fixture).await;
    let card = posted_card(&fixture.platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");

    let result = fixture.app.host_action(answer).await.expect("a click result");
    assert!(
        result
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已回答")),
        "the click settles the card"
    );

    let calls = fixture.backend.location_reply_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "frm_answer");
    assert!(calls[0].1.is_some(), "an answer carries the keyed values");
    assert_eq!(
        calls[0].2.as_deref(),
        Some("/work"),
        "the reply carries the listing directory"
    );
    assert!(
        fixture.backend.reply_question_routes.lock().await.is_empty(),
        "a Location Form must never take the session route"
    );
}

/// A typed (form-submitted) Location Form answer survives the name-only
/// callback shape: Feishu omits `action.value`, `ws.rs` rebuilds the routing
/// from `submit|<req>||<i>` (no directory, no owner marker), and the flow must
/// still recover the remembered owner and directory.
#[tokio::test]
async fn a_typed_location_answer_keeps_its_directory_route() {
    let fixture = fixture(
        false,
        &[("/work", typed_location_form("frm_typed"))],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;
    sweep(&fixture).await;

    let payload = serde_json::json!({
        "event": {
            "action": {
                "name": "submit|frm_typed||0",
                "form_value": {"custom_0": "hello"},
            }
        }
    });
    let value = crate::feishu::ws::extract_card_action_value(payload.to_string().as_bytes()).expect("value");
    assert!(
        value.get("directory").is_none(),
        "the name-only rebuild carries no directory: {value}"
    );
    assert!(
        value.get("location_form").is_none(),
        "the name-only rebuild carries no owner marker: {value}"
    );

    let result = fixture
        .app
        .host_action(value)
        .await
        .expect("a typed submit result");
    assert!(
        result
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已回答")),
        "the typed answer settles the card"
    );

    let calls = fixture.backend.location_reply_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].2.as_deref(),
        Some("/work"),
        "the remembered owner recovers the listing directory"
    );
    let answers = calls[0].1.as_ref().expect("keyed answers");
    assert_eq!(
        answers[0].value,
        Some(crate::backend::types::FormValue::Text("hello".into())),
        "the typed text reaches the form reply"
    );
}

/// Cancelling uses the same Location route and remains an explicit decline.
#[tokio::test]
async fn cancelling_declines_through_the_location_route() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_cancel"))],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;

    sweep(&fixture).await;
    let card = posted_card(&fixture.platform).await;
    let reject = button_with_reply(&card, "reject").expect("a cancel control");
    fixture.app.host_action(reject).await.expect("a click result");

    let calls = fixture.backend.location_reply_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].1.is_none(), "a cancel carries no answers");
    assert_eq!(calls[0].2.as_deref(), Some("/work"));
    assert!(fixture.backend.reply_question_routes.lock().await.is_empty());
}

/// 「稍后」 withdraws the card and releases its Message Pin without declining:
/// no cancel call, the form stays pending, and no sweep auto-declines it.
#[tokio::test]
async fn defer_withdraws_the_card_and_releases_the_pin_without_declining() {
    let fixture = fixture(
        true,
        &[("/work", location_form("frm_defer"))],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;

    let mut seen = std::collections::HashSet::new();
    sweep_with(&fixture, &mut seen).await;
    assert_eq!(
        fixture.platform.message_pins().await,
        vec![("msg_sent".to_string(), true)],
        "the waiting card is pinned"
    );
    assert!(
        fixture.platform.reminders().await.is_empty(),
        "a Location Form takes a message pin but never an Instant Reminder"
    );
    let card = posted_card(&fixture.platform).await;
    let defer = button_with_reply(&card, "defer").expect("a 稍后 control");
    let result = fixture.app.host_action(defer).await.expect("a click result");
    assert_eq!(result.toast.as_deref(), Some("已稍后处理"), "the defer landed");
    assert!(
        result.card.is_none(),
        "the explicit PATCH owns the card update; the ack must not double-write it"
    );
    assert!(
        fixture
            .platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains("已稍后处理")),
        "the clicked/stored card is PATCHed before its record is cleared"
    );

    assert!(
        fixture.backend.location_reply_calls.lock().await.is_empty(),
        "稍后 must not decline or answer the form"
    );
    assert!(
        fixture.backend.reply_question_routes.lock().await.is_empty(),
        "稍后 must not reach the session route"
    );
    assert_eq!(
        fixture.platform.message_pins().await,
        vec![("msg_sent".to_string(), true), ("msg_sent".to_string(), false)],
        "稍后 releases the pin immediately"
    );

    // No timer, no auto-decline: later sweeps leave the pending form alone and
    // do not push a second card.
    sweep_with(&fixture, &mut seen).await;
    sweep_with(&fixture, &mut seen).await;
    assert!(
        fixture.backend.location_reply_calls.lock().await.is_empty(),
        "nothing may decline the deferred form on its own"
    );
    assert_eq!(posted_cards(&fixture.platform).await.len(), 1, "no second push");

    // The form is still pending: a click on the withdrawn card still answers.
    let answer = button_with_reply(&card, "answer").expect("an answer control");
    let result = fixture.app.host_action(answer).await.expect("a click result");
    assert!(
        result
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已回答")),
        "the form stayed pending and answerable"
    );
    assert_eq!(
        fixture.backend.location_reply_calls.lock().await.len(),
        1,
        "the later click reaches the backend"
    );
}

/// A failed defer keeps the card, its record and its pin: the withdrawal is
/// PATCHed before the records go, so a failed card write cannot leave a
/// clickable card with nothing tracking it.
#[tokio::test]
async fn a_failed_defer_keeps_the_card_and_pin() {
    let fixture = fixture(
        true,
        &[("/work", location_form("frm_defer_fail"))],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;
    let mut seen = std::collections::HashSet::new();
    sweep_with(&fixture, &mut seen).await;
    assert_eq!(
        fixture.platform.message_pins().await,
        vec![("msg_sent".to_string(), true)]
    );

    fixture
        .platform
        .fail_update_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let card = posted_card(&fixture.platform).await;
    let defer = button_with_reply(&card, "defer").expect("a 稍后 control");
    let result = fixture.app.host_action(defer).await.expect("a click result");
    assert!(
        result
            .toast
            .as_deref()
            .is_some_and(|toast| toast.contains("未完成")),
        "a failed withdrawal says it did not fully land: {:?}",
        result.toast
    );
    assert!(
        result.card.is_none(),
        "a failed withdrawal must not let the ack replace the still-live card"
    );
    assert_eq!(
        fixture.platform.message_pins().await,
        vec![("msg_sent".to_string(), true)],
        "the pin is kept while the card is still the tracked surface"
    );
    // The form is still answerable: a later click settles it.
    let answer = button_with_reply(&card, "answer").expect("an answer control");
    fixture.app.host_action(answer).await.expect("a click result");
    assert_eq!(
        fixture.backend.location_reply_calls.lock().await.len(),
        1,
        "the retained card still answers"
    );
}

/// A retryable 404 (wrong location, or a cancelled pooled loser) keeps the
/// card and its record; a retry then succeeds, and only a complete sweep that
/// no longer lists the form withdraws it.
#[tokio::test]
async fn a_retryable_404_keeps_the_location_card_and_allows_a_retry() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_404"))],
        &[entry("chat_1", "ses_1", "/work")],
        |backend| {
            backend.location_form_reply_404_retryable();
        },
    )
    .await;

    sweep(&fixture).await;
    let card = posted_card(&fixture.platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");
    let result = fixture
        .app
        .host_action(answer.clone())
        .await
        .expect("a click result");

    assert!(
        result.card.is_none(),
        "a retryable refusal must not replace the still-live card"
    );
    assert!(
        result
            .toast
            .as_deref()
            .is_some_and(|toast| toast.contains("未送达")),
        "the user is told the answer did not land: {:?}",
        result.toast
    );
    // The form is still listed: a sweep must neither duplicate nor stale it.
    sweep(&fixture).await;
    assert_eq!(
        posted_cards(&fixture.platform).await.len(),
        1,
        "the pushed record survives, so no second card is posted"
    );
    assert!(
        fixture
            .platform
            .updated_cards()
            .await
            .iter()
            .all(|card| !card_text(card).contains("已处理")),
        "a still-listed form must not be marked handled"
    );

    // The failure clears: the same card's click is retryable.
    fixture
        .backend
        .location_form_retryable_404
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let retry = fixture.app.host_action(answer).await.expect("a retry result");
    assert!(
        retry
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已回答")),
        "the retry lands"
    );
    let calls = fixture.backend.location_reply_calls.lock().await.clone();
    assert_eq!(calls.len(), 2, "both attempts reached the backend");
    assert_eq!(calls[1].2.as_deref(), Some("/work"));
}

/// A late click on a withdrawn Location Form card takes the benign
/// already-handled path: it never reaches the backend and never claims a
/// decision.
#[tokio::test]
async fn a_late_click_on_a_withdrawn_location_card_is_handled() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_late"))],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;

    sweep(&fixture).await;
    let card = posted_card(&fixture.platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");

    // Another client settles it; the complete sweep withdraws the card.
    fixture.backend.question_resolved_by_another("frm_late").await;
    sweep(&fixture).await;

    let result = fixture
        .app
        .host_action(answer)
        .await
        .expect("a late click still answers");
    assert!(
        result
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已处理")),
        "a late click gets the neutral handled receipt"
    );
    assert!(
        fixture.backend.location_reply_calls.lock().await.is_empty(),
        "a late click never reaches the backend"
    );
}

/// A 409 (already settled) takes the benign handled path: the card settles and
/// nothing survives.
#[tokio::test]
async fn a_settled_409_takes_the_handled_path() {
    let fixture = fixture(
        false,
        &[("/work", location_form("frm_409"))],
        &[entry("chat_1", "ses_1", "/work")],
        |backend| {
            backend.location_form_already_settled();
        },
    )
    .await;

    sweep(&fixture).await;
    let card = posted_card(&fixture.platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");
    let result = fixture.app.host_action(answer).await.expect("a click result");

    assert!(
        result
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已处理")),
        "409 is a neutral handled receipt: {:?}",
        result.card
    );
}

/// A URL-mode elicitation renders its link and an acknowledgement; it is not a
/// text input.
#[tokio::test]
async fn a_url_mode_elicitation_renders_its_link_and_acknowledgement() {
    let mut form = location_form("frm_url");
    form.questions = vec![QuestionInfo {
        key: "signin".into(),
        question: "打开登录页".into(),
        header: "登录".into(),
        kind: FormFieldKind::External,
        url: Some("https://example.com/oauth".into()),
        ..Default::default()
    }];
    let fixture = fixture(
        false,
        &[("/work", form)],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;

    sweep(&fixture).await;
    let card = posted_card(&fixture.platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("https://example.com/oauth"),
        "link rendered: {text}"
    );
    assert!(text.contains("我已了解"), "acknowledgement rendered: {text}");
    assert!(
        button_with_reply(&card, "answer").is_some(),
        "the URL acknowledgement is an answer control"
    );
}

/// A secret-format Location Form is uncollectable: the existing secret-input
/// reason is shown and it can only be declined — never rendered as an input.
#[tokio::test]
async fn a_secret_location_form_is_uncollectable_and_only_declinable() {
    let mut form = location_form("frm_secret");
    form.questions = vec![QuestionInfo {
        key: "token".into(),
        question: "API token".into(),
        header: "Token".into(),
        kind: FormFieldKind::String,
        secret: true,
        ..Default::default()
    }];
    form.uncollectable = Some(UncollectableReason::SecretInput);
    let fixture = fixture(
        false,
        &[("/work", form)],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;

    sweep(&fixture).await;
    let card = posted_card(&fixture.platform).await;
    let text = card_text(&card);
    assert!(text.contains("私密输入"), "the secret reason is shown: {text}");
    assert!(
        !card.to_string().contains("\"tag\":\"input\""),
        "never an input control: {card}"
    );
    assert!(
        button_with_reply(&card, "reject").is_some(),
        "the only decision is an explicit decline"
    );
    assert!(
        button_with_reply(&card, "defer").is_some(),
        "稍后 may still withdraw it without declining"
    );
}

/// A session-backed elicitation keeps the ordinary Question path: it is
/// answered on the session route, never the Location route.
#[tokio::test]
async fn a_session_backed_form_keeps_the_session_route() {
    let fixture = fixture(false, &[], &[entry("chat_1", "ses_1", "/work")], |backend| {
        backend.ask_questions(vec![session_form("q_session", "ses_1")]);
    })
    .await;

    sweep(&fixture).await;
    let card = posted_card(&fixture.platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");
    fixture.app.host_action(answer).await.expect("a click result");

    let routes = fixture.backend.reply_question_routes.lock().await.clone();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].1, "ses_1", "the session route keeps its session id");
    assert_eq!(routes[0].2.as_deref(), Some("/work"));
    assert!(
        fixture.backend.location_reply_calls.lock().await.is_empty(),
        "a session-backed form never takes the Location route"
    );
}

/// A pooled no-caller form can list in several directories with its own form
/// id per holder: each listing gets its own card (no cross-location dedupe).
#[tokio::test]
async fn per_holder_listings_each_get_a_card() {
    let mut first = location_form("frm_a");
    first.server = Some("shared".into());
    let mut second = location_form("frm_b");
    second.server = Some("shared".into());
    let fixture = fixture(
        false,
        &[("/work/a", first), ("/work/b", second)],
        &[
            entry("chat_a", "ses_a", "/work/a"),
            entry("chat_b", "ses_b", "/work/b"),
        ],
        |_| {},
    )
    .await;

    sweep(&fixture).await;

    assert_eq!(
        posted_cards(&fixture.platform).await.len(),
        2,
        "one card per holder"
    );
}

/// Answering releases a Location Form's Message Pin on the next sweep.
#[tokio::test]
async fn answering_releases_the_location_form_pin() {
    let fixture = fixture(
        true,
        &[("/work", location_form("frm_pin_answer"))],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;
    sweep(&fixture).await;
    assert_eq!(
        fixture.platform.message_pins().await,
        vec![("msg_sent".to_string(), true)]
    );

    let card = posted_card(&fixture.platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");
    fixture.app.host_action(answer).await.expect("a click result");
    sweep(&fixture).await;

    assert_eq!(
        fixture.platform.message_pins().await,
        vec![("msg_sent".to_string(), true), ("msg_sent".to_string(), false)],
        "a settled card releases its pin"
    );
}

/// Cancelling releases a Location Form's Message Pin on the next sweep.
#[tokio::test]
async fn cancelling_releases_the_location_form_pin() {
    let fixture = fixture(
        true,
        &[("/work", location_form("frm_pin_cancel"))],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;
    sweep(&fixture).await;

    let card = posted_card(&fixture.platform).await;
    let reject = button_with_reply(&card, "reject").expect("a cancel control");
    fixture.app.host_action(reject).await.expect("a click result");
    sweep(&fixture).await;

    assert_eq!(
        fixture.platform.message_pins().await,
        vec![("msg_sent".to_string(), true), ("msg_sent".to_string(), false)],
        "a cancelled card releases its pin"
    );
}

/// A Location Form that leaves the pending list releases its pin while the
/// card goes stale.
#[tokio::test]
async fn staling_releases_the_location_form_pin() {
    let fixture = fixture(
        true,
        &[("/work", location_form("frm_pin_stale"))],
        &[entry("chat_1", "ses_1", "/work")],
        |_| {},
    )
    .await;
    sweep(&fixture).await;

    fixture
        .backend
        .question_resolved_by_another("frm_pin_stale")
        .await;
    sweep(&fixture).await;

    assert_eq!(
        fixture.platform.message_pins().await,
        vec![("msg_sent".to_string(), true), ("msg_sent".to_string(), false)],
        "a stale card releases its pin"
    );
    assert!(
        fixture
            .platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains("已处理")),
        "the card goes stale through the resolved-elsewhere path"
    );
}

/// A card action whose session id is empty (a Location Form has none) never
/// probes `session_info("")`: the empty id names no Session and has no parent
/// chain to walk.
#[tokio::test]
async fn an_empty_session_id_card_action_never_probes_session_info() {
    let fixture = fixture(false, &[], &[entry("chat_1", "ses_1", "/work")], |_| {}).await;

    let result = fixture
        .app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "frm_unknown",
            "session_id": "",
            "directory": "/work",
            "question_index": 0,
            "answer": "yes",
        }))
        .await
        .expect("a late click result");

    assert!(
        result.card.is_some(),
        "the click is classified, not a silent no-op"
    );
    assert!(
        !fixture
            .backend
            .session_info_calls
            .lock()
            .await
            .iter()
            .any(String::is_empty),
        "an empty session id must never reach session_info"
    );
}
