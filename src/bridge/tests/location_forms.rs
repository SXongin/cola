//! Location Forms (ADR-0103, ticket #1017): a directory-owned MCP
//! elicitation is delivered once as its own card into the directory's newest
//! mapped Chat/Topic, answered/cancelled through the directory route, or
//! deferred with 「稍后」 without declining. Session-backed elicitations keep
//! their ordinary Question path; unmarked session-less requests keep #489's
//! WARN-and-skip.
//!
//! These tests drive the real request flow through the mock backend and the
//! recording platform: they assert the card that was sent where, the route the
//! reply took, and what the next sweep does — never a private field.

use std::sync::Arc;

use crate::bridge::test_support::*;

use crate::backend::types::{
    FormFieldKind, QuestionInfo, QuestionOption, QuestionRequest, RequestOwner, UncollectableReason,
};
use crate::config::{SessionEntry, ThreadKey};

/// A Location Form with one option-only field (a click answers it, no typed
/// input), naming an asking server and message.
fn location_form(request_id: &str) -> QuestionRequest {
    QuestionRequest {
        id: request_id.into(),
        session_id: String::new(),
        title: "Context7 登录".into(),
        owner: RequestOwner::Location,
        server: Some("context7".into()),
        message: Some("请先登录 Context7 再继续".into()),
        questions: vec![QuestionInfo {
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
        ..Default::default()
    }
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

/// One sweep with a caller-owned `seen` (the poll loop's own set survives
/// sweeps; reusing it is what makes an in-process dismissal stable).
async fn sweep_with(app: &Arc<App>, seen: &mut std::collections::HashSet<String>) {
    app.question.sweep(&app.flow_handles(), seen).await;
}

/// One sweep with a fresh `seen` (the standalone push-once safety net is the
/// sent-card record, not the loop's set).
async fn sweep(app: &Arc<App>) {
    let mut seen = std::collections::HashSet::new();
    sweep_with(app, &mut seen).await;
}

/// Build the app while keeping the mock backend addressable, so a test can
/// inspect the recorded routes after the click.
fn build_with_shared_backend(
    cfg: crate::config::Config,
    backend: MockBackend,
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    (app, platform, backend)
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

/// The single card cola posted (send or reply), panicking if there is not
/// exactly one.
async fn posted_card(platform: &RecordingPlatform) -> serde_json::Value {
    let cards: Vec<_> = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::SendCard { card, .. } | PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect();
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
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_1"));
    let (app, platform, _backend) = build_with_shared_backend(cfg, backend);

    // Two mapped Sessions in the same directory; activation order is
    // newest-first, and another directory must not be chosen.
    seed_entry(&app, entry("chat_old", "ses_old", "/work")).await;
    seed_entry(&app, entry("chat_new", "ses_new", "/work")).await;
    seed_entry(&app, entry("chat_elsewhere", "ses_else", "/elsewhere")).await;

    sweep(&app).await;
    sweep(&app).await;

    assert_eq!(
        sent_to(&platform).await.as_deref(),
        Some("chat_new"),
        "the newest mapped Chat/Topic in the form's directory receives it"
    );
    assert_eq!(platform.sent_cards().await.len(), 1, "pushed exactly once");
    let card = posted_card(&platform).await;
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
        platform.reminders().await.is_empty(),
        "a Location Form takes no reminder"
    );
    assert!(!app.question.is_pending_for("ses_new").await);
    assert!(!app.question.is_pending_for("").await);
}

/// A Session mapped inside a Topic is reached through its topic anchor, not
/// the Chat lobby.
#[tokio::test]
async fn a_topic_location_form_replies_to_its_topic_anchor() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_topic"));
    let (app, platform, _backend) = build_with_shared_backend(cfg, backend);
    seed_entry(
        &app,
        topic_entry("omt_topic", "ses_topic", "/work", Some("msg_anchor")),
    )
    .await;

    sweep(&app).await;

    assert_eq!(replied_to(&platform).await.as_deref(), Some("msg_anchor"));
    assert_eq!(platform.sent_cards().await.len(), 0, "never posts into the lobby");
}

/// A topic whose anchor cannot be resolved defers the push: no card, no
/// suppression, and the next sweep retries once the anchor is reachable.
#[tokio::test]
async fn an_unresolved_topic_anchor_defers_the_push_without_falling_back() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_deferred"));
    let (app, platform, _backend) = build_with_shared_backend(cfg, backend);
    platform
        .no_topic_messages
        .store(true, std::sync::atomic::Ordering::SeqCst);
    seed_entry(&app, topic_entry("omt_topic", "ses_topic", "/work", None)).await;

    sweep(&app).await;
    assert!(
        platform.sent_cards().await.is_empty() && platform.replied_cards().await.is_empty(),
        "an unresolvable topic anchor sends nothing — not into the lobby"
    );

    platform
        .no_topic_messages
        .store(false, std::sync::atomic::Ordering::SeqCst);
    sweep(&app).await;
    assert_eq!(
        replied_to(&platform).await.as_deref(),
        Some("msg_in_topic_anchor"),
        "the deferred push retries and lands in the topic"
    );
}

/// A definitively failed send is not recorded, so the next complete sweep
/// retries it.
#[tokio::test]
async fn a_failed_location_form_push_retries_on_the_next_sweep() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_retry"));
    let (app, platform, _backend) = build_with_shared_backend(cfg, backend);
    seed_entry(
        &app,
        topic_entry("omt_topic", "ses_topic", "/work", Some("msg_anchor")),
    )
    .await;
    platform
        .fail_reply_card_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    sweep(&app).await;
    assert!(app.question.sent_cards.lock().await.is_empty());
    sweep(&app).await;
    assert_eq!(
        replied_to(&platform).await.as_deref(),
        Some("msg_anchor"),
        "the retry lands the card"
    );
}

/// Answering a Location Form routes through the directory path (never the
/// session route), carrying the directory whose list produced the form.
#[tokio::test]
async fn answering_routes_the_location_form_with_its_directory() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_answer"));
    let (app, platform, backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_1", "ses_1", "/work")).await;

    sweep(&app).await;
    let card = posted_card(&platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");

    let result = app.host_action(answer).await.expect("a click result");
    assert!(
        result
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已回答")),
        "the click settles the card"
    );

    let calls = backend.location_reply_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "frm_answer");
    assert!(calls[0].1.is_some(), "an answer carries the keyed values");
    assert_eq!(
        calls[0].2.as_deref(),
        Some("/work"),
        "the reply carries the listing directory"
    );
    assert!(
        backend.reply_question_routes.lock().await.is_empty(),
        "a Location Form must never take the session route"
    );
    assert!(!app.question.has_question("frm_answer").await);
    assert!(app.question.sent_cards.lock().await.is_empty());
}

/// Cancelling uses the same Location route and remains an explicit decline.
#[tokio::test]
async fn cancelling_declines_through_the_location_route() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_cancel"));
    let (app, platform, backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_1", "ses_1", "/work")).await;

    sweep(&app).await;
    let card = posted_card(&platform).await;
    let reject = button_with_reply(&card, "reject").expect("a cancel control");
    app.host_action(reject).await.expect("a click result");

    let calls = backend.location_reply_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].1.is_none(), "a cancel carries no answers");
    assert_eq!(calls[0].2.as_deref(), Some("/work"));
    assert!(backend.reply_question_routes.lock().await.is_empty());
}

/// 「稍后」 withdraws the card and releases its Message Pin without declining:
/// no cancel call, the form stays pending, and no sweep auto-declines it.
#[tokio::test]
async fn defer_withdraws_the_card_and_releases_the_pin_without_declining() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_defer"));
    let (app, platform, backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_1", "ses_1", "/work")).await;

    let mut seen = std::collections::HashSet::new();
    sweep_with(&app, &mut seen).await;
    assert_eq!(
        platform.message_pins().await,
        vec![("msg_sent".to_string(), true)],
        "the waiting card is pinned"
    );
    assert!(
        platform.reminders().await.is_empty(),
        "a Location Form takes a message pin but never an Instant Reminder"
    );
    let card = posted_card(&platform).await;
    let defer = button_with_reply(&card, "defer").expect("a 稍后 control");
    let result = app.host_action(defer).await.expect("a click result");
    assert!(
        result
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已稍后处理")),
        "the clicked card is withdrawn"
    );

    assert!(
        backend.location_reply_calls.lock().await.is_empty(),
        "稍后 must not decline or answer the form"
    );
    assert!(
        backend.reply_question_routes.lock().await.is_empty(),
        "稍后 must not reach the session route"
    );
    assert!(
        app.question.has_question("frm_defer").await,
        "the form stays pending after 稍后"
    );
    assert!(
        app.question.sent_cards.lock().await.is_empty(),
        "the sent-card record is withdrawn"
    );
    assert_eq!(
        platform.message_pins().await,
        vec![("msg_sent".to_string(), true), ("msg_sent".to_string(), false)],
        "稍后 releases the pin immediately"
    );

    // No timer, no auto-decline: later sweeps leave the pending form alone.
    sweep_with(&app, &mut seen).await;
    sweep_with(&app, &mut seen).await;
    assert!(
        backend.location_reply_calls.lock().await.is_empty(),
        "nothing may decline the deferred form on its own"
    );
    assert_eq!(platform.sent_cards().await.len(), 1, "no second push");
}

/// A retryable 404 (wrong location, or a cancelled pooled loser) keeps the
/// card, its surface and the record; only a complete sweep that no longer
/// lists the form withdraws it.
#[tokio::test]
async fn a_retryable_404_keeps_the_location_card_and_surface() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_404"));
    backend.location_form_reply_404_retryable();
    let (app, platform, backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_1", "ses_1", "/work")).await;

    sweep(&app).await;
    let card = posted_card(&platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");
    let result = app.host_action(answer).await.expect("a click result");

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
    assert!(
        app.question.sent_cards.lock().await.contains_key("frm_404"),
        "the pushed record survives"
    );
    assert!(
        app.question.has_question("frm_404").await,
        "the surface and its controls survive for a retry"
    );

    // The form genuinely leaves the list: the next complete sweep withdraws it.
    backend.question_resolved_by_another("frm_404").await;
    sweep(&app).await;
    assert!(!app.question.sent_cards.lock().await.contains_key("frm_404"));
    assert!(
        platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains("已处理")),
        "the card goes stale through the resolved-elsewhere path"
    );
}

/// A late click on a withdrawn Location Form card takes the benign
/// already-handled path: it never reaches the backend and never claims a
/// decision.
#[tokio::test]
async fn a_late_click_on_a_withdrawn_location_card_is_handled() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_late"));
    let (app, platform, backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_1", "ses_1", "/work")).await;

    sweep(&app).await;
    let card = posted_card(&platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");

    // Another client settles it; the complete sweep withdraws the card.
    backend.question_resolved_by_another("frm_late").await;
    sweep(&app).await;

    let result = app.host_action(answer).await.expect("a late click still answers");
    assert!(
        result
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已处理")),
        "a late click gets the neutral handled receipt"
    );
    assert!(
        backend.location_reply_calls.lock().await.is_empty(),
        "a late click never reaches the backend"
    );
}

/// A 409 (already settled) takes the benign handled path: the card settles and
/// nothing survives.
#[tokio::test]
async fn a_settled_409_takes_the_handled_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question_in("/work", location_form("frm_409"));
    backend.location_form_already_settled();
    let (app, platform, _backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_1", "ses_1", "/work")).await;

    sweep(&app).await;
    let card = posted_card(&platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");
    let result = app.host_action(answer).await.expect("a click result");

    assert!(
        result
            .card
            .as_ref()
            .is_some_and(|card| card_text(card).contains("已处理")),
        "409 is a neutral handled receipt: {:?}",
        result.card
    );
    assert!(app.question.sent_cards.lock().await.is_empty());
    assert!(!app.question.has_question("frm_409").await);
}

/// A URL-mode elicitation renders its link and an acknowledgement; it is not a
/// text input.
#[tokio::test]
async fn a_url_mode_elicitation_renders_its_link_and_acknowledgement() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let mut form = location_form("frm_url");
    form.questions = vec![QuestionInfo {
        key: "signin".into(),
        question: "打开登录页".into(),
        header: "登录".into(),
        kind: FormFieldKind::External,
        url: Some("https://example.com/oauth".into()),
        ..Default::default()
    }];
    backend.ask_question_in("/work", form);
    let (app, platform, _backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_1", "ses_1", "/work")).await;

    sweep(&app).await;
    let card = posted_card(&platform).await;
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

/// A secret-format form is uncollectable: the existing secret-input reason is
/// shown and it can only be declined — never rendered as an input.
#[tokio::test]
async fn a_secret_location_form_is_uncollectable_and_only_declinable() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
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
    backend.ask_question_in("/work", form);
    let (app, platform, _backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_1", "ses_1", "/work")).await;

    sweep(&app).await;
    let card = posted_card(&platform).await;
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
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_questions(vec![session_form("q_session", "ses_1")]);
    let (app, platform, backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_1", "ses_1", "/work")).await;

    sweep(&app).await;
    let card = posted_card(&platform).await;
    let answer = button_with_reply(&card, "answer").expect("an answer control");
    app.host_action(answer).await.expect("a click result");

    let routes = backend.reply_question_routes.lock().await.clone();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].1, "ses_1", "the session route keeps its session id");
    assert_eq!(routes[0].2.as_deref(), Some("/work"));
    assert!(
        backend.location_reply_calls.lock().await.is_empty(),
        "a session-backed form never takes the Location route"
    );
}

/// A pooled no-caller form can list in several directories with its own form
/// id per holder: each listing gets its own card (no cross-location dedupe).
#[tokio::test]
async fn per_holder_listings_each_get_a_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let mut first = location_form("frm_a");
    first.server = Some("shared".into());
    let mut second = location_form("frm_b");
    second.server = Some("shared".into());
    backend.ask_question_in("/work/a", first);
    backend.ask_question_in("/work/b", second);
    let (app, platform, _backend) = build_with_shared_backend(cfg, backend);
    seed_entry(&app, entry("chat_a", "ses_a", "/work/a")).await;
    seed_entry(&app, entry("chat_b", "ses_b", "/work/b")).await;

    sweep(&app).await;

    assert_eq!(platform.sent_cards().await.len(), 2, "one card per holder");
    assert!(app.question.sent_cards.lock().await.contains_key("frm_a"));
    assert!(app.question.sent_cards.lock().await.contains_key("frm_b"));
}
