//! Supplement-triggered Card Chain splits (ADR-0043): a user message that
//! lands below a session's live card while its Turn is in flight splits the
//! chain at that message — the previous card keeps everything before the
//! split, and the continuation card is the supplement's reply, carrying the
//! receipt line plus only the content that arrives after it (a delta handoff,
//! like a size split). The old text acknowledgement is gone; commands never
//! split the chain.

use std::sync::Arc;

use crate::backend::ToolStatus;
use crate::bridge::test_support::*;
use crate::feishu::card::CardState;

/// One question with a single option, for the two-question fixtures below.
fn question_fixture(text: &str, header: &str, label: &str) -> crate::opencode::types::QuestionInfo {
    use crate::opencode::types::{QuestionInfo, QuestionOption};
    QuestionInfo {
        question: text.into(),
        header: header.into(),
        options: vec![QuestionOption {
            label: label.into(),
            description: String::new(),
        }],
        multiple: None,
        custom: None,
    }
}

/// The question request the question flow replays and replies to.
fn question_request(request_id: &str, session_id: &str) -> crate::opencode::types::QuestionRequest {
    crate::opencode::types::QuestionRequest {
        id: request_id.into(),
        session_id: session_id.into(),
        questions: vec![
            question_fixture("选目录", "目录", "/a"),
            question_fixture("选分支", "分支", "main"),
        ],
    }
}

/// Seed the session's live card (content `text`, id `om_live`) and the in-flight
/// guard, so the next message takes the supplement path.
async fn seed_live_turn(app: &Arc<App>, session_id: &str, text: &str) {
    let cards = app.cards_handle();
    Turn::seed_card(&cards, session_id, Some("om_live")).await;
    Turn::set_card_state(&cards, session_id, CardState::Streaming).await;
    Turn::push_text(&cards, session_id, text).await;
    Turn::set_reply_target(&cards, session_id, "msg_1").await;
    app.inflight.lock().await.insert(session_id.to_string());
}

/// `count` segments of exactly one card's text budget, each stamped with a
/// unique marker, so one segment lands on one card and assertions can name the
/// slice they expect.
fn marked_slices(count: usize) -> String {
    let max = crate::feishu::card::MAX_CARD_TEXT_CHARS;
    (0..count)
        .map(|i| {
            let marker = format!("【S{i:02}】");
            format!("{marker}{}", "长".repeat(max - marker.chars().count()))
        })
        .collect()
}

/// The latest continuation card (`ReplyCard`) the platform recorded.
async fn continuation(platform: &RecordingPlatform) -> (String, serde_json::Value) {
    platform
        .calls
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { reply_to, card } => Some((reply_to.clone(), card.clone())),
            _ => None,
        })
        .expect("a continuation card must have been sent")
}

/// The latest in-place update of `message_id`, as a string.
async fn last_update_of(platform: &RecordingPlatform, message_id: &str) -> String {
    platform
        .calls
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage {
                message_id: mid,
                card,
            } if mid == message_id => Some(card.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no update for {message_id}"))
}

fn has(card: &serde_json::Value, needle: &str) -> bool {
    card_text(card).contains(needle)
}

/// Every continuation card that replied to a supplement message, in call order.
fn supplement_continuations(calls: &[PlatformCall]) -> Vec<serde_json::Value> {
    calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { reply_to, card } if reply_to.starts_with("msg_sup") => {
                Some(card.clone())
            }
            _ => None,
        })
        .collect()
}

/// The supplement is sent to the Backend fire-and-forget, and the live card
/// splits at it: the previous card is finalized with everything before the
/// split, the continuation replies to the supplement with the receipt (and
/// only later content), and becomes the tracked live card — no separate
/// acknowledgement message.
#[tokio::test]
async fn supplement_splits_the_chain_and_the_continuation_takes_over() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_turn(&app, "ses_test", "第一段进度。").await;

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下，改用方案 B".into(),
        None,
    ))
    .await;

    // No acknowledgement message: only the previous card's finalization and
    // the continuation.
    assert!(
        platform.texts().await.is_empty(),
        "no reply_text acknowledgement may be sent: {:?}",
        platform.calls.lock().await
    );

    // The previous card ends with the standard split header and KEEPS the
    // content shown before the split (the handoff is a delta, not a copy).
    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        finalized.contains("部分完成，继续中"),
        "the previous card must wear the standard split header: {finalized}"
    );
    assert!(
        finalized.contains("第一段进度。"),
        "the previous card keeps everything before the split: {finalized}"
    );
    assert!(
        !finalized.contains("📨 已收到补充"),
        "the receipt records the supplement on the continuation only: {finalized}"
    );

    // The continuation replies to the supplement and carries ONLY the delta:
    // the receipt, never the content already finalized on the previous card.
    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(
        reply_to, "msg_sup",
        "the continuation must reply to the supplement message"
    );
    assert!(
        !has(&card, "第一段进度。"),
        "the continuation must not re-render the finalized content: {card}"
    );
    assert!(
        has(&card, "📨 已收到补充"),
        "the receipt must ride the continuation: {card}"
    );

    // The chain is re-anchored: the continuation is the tracked live card and
    // the split is consumed.
    {
        let cards = app.cards_handle();
        assert_eq!(
            Turn::card_message_id(&cards, "ses_test").await.as_deref(),
            Some("msg_reply")
        );
        assert_eq!(
            Turn::reply_target(&cards, "ses_test").await.as_deref(),
            Some("msg_sup")
        );
        assert!(
            !Turn::has_pending_split(&cards, "ses_test").await,
            "the split must be consumed"
        );
        assert!(
            Turn::card_is_live(&cards, "ses_test").await,
            "a continuation that fits is the new live card"
        );
    }

    // Later flushes target the continuation, not the finalized card.
    Turn::push_text(&app.cards_handle(), "ses_test", "后续进度。").await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
    let calls = platform.calls.lock().await.clone();
    let last_update = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
            _ => None,
        })
        .expect("a later flush");
    assert_eq!(
        last_update.0, "msg_reply",
        "later updates must target the continuation: {calls:?}"
    );
    assert!(has(&last_update.1, "后续进度。"));
}

/// A split while a tool is running: the panel is live tail content (ADR-0045),
/// so the finalized card finalizes WITHOUT it and the continuation — the new
/// live card — carries it, its header still naming the Turn's running tool
/// instead of falling back to "✍️ 回复中". A tool that already finished is not
/// running, so it does not leak into the continuation's header.
#[tokio::test]
async fn a_split_continuation_takes_the_running_tool_panel_over() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;

    let bash = |status: ToolStatus| {
        crate::feishu::card::tool_render::ToolPanel::from_parts(
            "bash",
            status,
            Some(serde_json::json!({"command": "sleep 30"})),
            None,
        )
    };
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
    Turn::push_text(&cards, "ses_test", "开始分析。").await;
    Turn::push_tool(&cards, "ses_test", "call_bash", bash(ToolStatus::Running)).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    app.inflight.lock().await.insert("ses_test".to_string());

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    // A running panel must never freeze on a finalized card...
    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        !finalized.contains("bash"),
        "a finalized card must not carry a running panel: {finalized}"
    );
    // ...it continues on the live continuation, whose header still says the
    // Turn is executing it — not the bare streaming label.
    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(reply_to, "msg_sup");
    assert!(
        has(&card, "bash"),
        "the continuation must carry the running panel: {card}"
    );
    let header = card["header"]["title"]["content"].as_str().unwrap();
    assert!(
        header.starts_with("⏳ bash"),
        "the continuation header must show the Turn's running tool: {card}"
    );
}

/// #243 / ADR-0045: the tool completes after the split. Its result renders on
/// the continuation — the live card — never on the finalized one, and never
/// nowhere (the bug this fixes).
#[tokio::test]
async fn a_tool_completing_after_the_split_renders_on_the_continuation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;

    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
    Turn::push_text(&cards, "ses_test", "开始分析。").await;
    Turn::push_tool(
        &cards,
        "ses_test",
        "call_bash",
        crate::feishu::card::tool_render::ToolPanel::from_parts(
            "bash",
            ToolStatus::Running,
            Some(serde_json::json!({"command": "sleep 30"})),
            None,
        ),
    )
    .await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    Turn::set_turn_anchor(&cards, "ses_test", &turn_anchor(0)).await;
    app.inflight.lock().await.insert("ses_test".to_string());

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    // The tool settles through the render path the poll uses: `sleep 30`
    // completes and its output lands.
    let msgs = vec![crate::opencode::types::SessionMessage {
        info: crate::opencode::types::MessageInfo {
            id: "a1".into(),
            role: Some("assistant".into()),
            parent_id: None,
            time: Some(crate::opencode::types::MessageTime {
                created: 1,
                completed: Some(1),
            }),
            model_id: None,
            provider_id: None,
            tokens: None,
        },
        parts: serde_json::json!([
            { "type": "tool", "tool": "bash", "callID": "call_bash",
              "state": { "status": "completed",
                         "input": { "command": "sleep 30" },
                         "output": "done" } },
        ]),
    }];
    crate::bridge::turn::Turn::render_and_flush(
        &app.cards_handle(),
        &app.sessions_handle(),
        &app.opencode,
        "ses_test",
        &crate::opencode::wire::decode(&msgs),
    )
    .await;

    // The completion renders on the continuation, in its live tail...
    let updated = last_update_of(&platform, "msg_reply").await;
    assert!(
        updated.contains("done") && updated.contains("✅ bash"),
        "the completion must render on the continuation: {updated}"
    );
    // ...and the finalized card stays frozen without it.
    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        !finalized.contains("done"),
        "the finalized card must keep its pre-split slice: {finalized}"
    );
}

/// The same split with the tool already finished: the continuation's header
/// falls back to the streaming label — the override is the live selection, so
/// a finished tool disappears naturally.
#[tokio::test]
async fn a_finished_tool_does_not_leak_into_the_continuation_header() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;

    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
    Turn::push_text(&cards, "ses_test", "开始分析。").await;
    Turn::push_tool(
        &cards,
        "ses_test",
        "call_bash",
        crate::feishu::card::tool_render::ToolPanel::from_parts(
            "bash",
            ToolStatus::Completed,
            Some(serde_json::json!({"command": "sleep 30"})),
            Some("done"),
        ),
    )
    .await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    app.inflight.lock().await.insert("ses_test".to_string());

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    let (_, card) = continuation(&platform).await;
    let header = card["header"]["title"]["content"].as_str().unwrap();
    assert!(
        header.starts_with("✍️ 回复中"),
        "a finished tool must not show in the continuation header: {card}"
    );
}

/// A supplement that lands while the loading card's reply is still in flight
/// (the turn-startup window) is not lost: the live card session already exists,
/// the split is requested on it, and the first flush after the card id lands
/// serves the continuation.
#[tokio::test]
async fn supplement_during_the_loading_card_round_trip_still_splits() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let async_calls = backend.prompt_async_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    // Park the loading card's reply: the turn holds the inflight guard and its
    // card session, but the card id has not landed yet.
    let (entered, release) = platform.pause("reply", "msg_1");
    let prompt = {
        let app = app.clone();
        tokio::spawn(async move {
            app.handle_message(incoming(
                "msg_1".into(),
                "chat_1".into(),
                "p2p".into(),
                None,
                "分析一下目录".into(),
                None,
            ))
            .await;
        })
    };
    entered.notified().await;

    // The supplement arrives in that window.
    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    // The split could not be served yet (no card id), but it is not dropped.
    {
        let cards = app.cards_handle();
        assert!(
            Turn::card_message_id(&cards, "ses_test").await.is_none(),
            "the loading reply is still parked"
        );
        assert!(
            Turn::has_pending_split(&cards, "ses_test").await,
            "the split waits for the card id instead of being lost"
        );
    }
    assert!(
        async_calls.lock().await.iter().any(|c| c.contains("补充一下")),
        "the supplement must still reach the backend"
    );

    release.notify_one();
    prompt.await.unwrap();

    // Once the id lands, the turn's flush serves the deferred split: the
    // previous card is finalized with the answer that had rendered before the
    // split, and the continuation carries only the receipt.
    let calls = platform.calls.lock().await.clone();
    let continuation = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { reply_to, card } if reply_to == "msg_sup" => Some(card.clone()),
            _ => None,
        })
        .expect("the deferred split must be served");
    assert!(
        has(&continuation, "📨 已收到补充"),
        "the receipt must ride the continuation: {continuation}"
    );
    assert!(
        !has(&continuation, "当前目录有 src/ 和 Cargo.toml。"),
        "content finalized before the split must not be repeated: {continuation}"
    );
    let finalized = last_update_of(&platform, "msg_reply").await;
    assert!(
        finalized.contains("当前目录有 src/ 和 Cargo.toml。"),
        "the answer stays on the finalized card: {finalized}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyText { text, .. } if text.contains("已收到补充"))),
        "no text acknowledgement may be sent: {calls:?}"
    );
    assert!(
        matches!(calls.last(), Some(PlatformCall::ReplyCard { .. })),
        "the continuation stays the newest message: {calls:?}"
    );
    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("msg_reply")
    );
    assert!(!Turn::has_pending_split(&cards, "ses_test").await);
    assert!(
        Turn::card_is_live(&cards, "ses_test").await,
        "the continuation is the live card"
    );
}

/// Several supplements queued while the card id is still absent (the startup
/// window) are served by exactly ONE continuation: it replies to the NEWEST
/// queued supplement and carries one receipt per supplement, in arrival order.
/// No supplement is coalesced away and no busy notice is sent.
#[tokio::test]
async fn supplements_queued_in_the_startup_window_share_one_continuation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let async_calls = backend.prompt_async_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    // Park the loading card's reply: the turn holds the inflight guard and its
    // card session, but the card id has not landed yet.
    let (entered, release) = platform.pause("reply", "msg_1");
    let prompt = {
        let app = app.clone();
        tokio::spawn(async move {
            app.handle_message(incoming(
                "msg_1".into(),
                "chat_1".into(),
                "p2p".into(),
                None,
                "分析一下目录".into(),
                None,
            ))
            .await;
        })
    };
    entered.notified().await;

    // Two supplements arrive in that window and queue in arrival order.
    app.handle_message(incoming(
        "msg_sup_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一".into(),
        None,
    ))
    .await;
    app.handle_message(incoming(
        "msg_sup_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充二".into(),
        None,
    ))
    .await;
    {
        let cards = app.cards_handle();
        assert!(
            Turn::card_message_id(&cards, "ses_test").await.is_none(),
            "the loading reply is still parked"
        );
        let queued: Vec<String> = Turn::pending_splits(&cards, "ses_test")
            .await
            .into_iter()
            .map(|(reply_to, _)| reply_to)
            .collect();
        assert_eq!(
            queued,
            vec!["msg_sup_1", "msg_sup_2"],
            "both supplements queue, in arrival order"
        );
    }

    release.notify_one();
    prompt.await.unwrap();

    // ONE continuation serves the batch, anchored at the newest supplement,
    // with one receipt per queued supplement.
    let calls = platform.calls.lock().await.clone();
    let continuations: Vec<(String, serde_json::Value)> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { reply_to, card } if reply_to.starts_with("msg_sup") => {
                Some((reply_to.clone(), card.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        continuations.len(),
        1,
        "exactly one continuation serves the batch: {calls:?}"
    );
    let (reply_to, card) = &continuations[0];
    assert_eq!(
        reply_to, "msg_sup_2",
        "the continuation replies to the newest supplement"
    );
    assert_eq!(
        card.to_string().matches("📨 已收到补充").count(),
        2,
        "one receipt per queued supplement: {card}"
    );
    assert!(
        !has(card, "当前目录有 src/ 和 Cargo.toml。"),
        "the continuation carries the delta only, not the finalized answer: {card}"
    );
    let finalized = last_update_of(&platform, "msg_reply").await;
    assert!(
        finalized.contains("当前目录有 src/ 和 Cargo.toml。"),
        "the answer stays on the card finalized before the split: {finalized}"
    );

    // Both supplements reached the backend, and no busy notice was sent.
    let sent = async_calls.lock().await.clone();
    assert!(sent.iter().any(|c| c.contains("补充一")), "missing: {sent:?}");
    assert!(sent.iter().any(|c| c.contains("补充二")), "missing: {sent:?}");
    assert!(
        !platform
            .texts()
            .await
            .iter()
            .any(|t| t.contains("上一条消息还在处理中")),
        "a supplement is never answered busy: {:?}",
        platform.calls.lock().await
    );

    // The chain re-anchors at the continuation: later flushes target it.
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
    let calls = platform.calls.lock().await.clone();
    let (message_id, later) = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
            _ => None,
        })
        .expect("a later flush");
    assert_eq!(message_id, "msg_reply", "later updates target the continuation");
    assert!(has(&later, "📨 已收到补充"), "the receipts stay on it: {later}");
    {
        let cards = app.cards_handle();
        assert!(
            !Turn::has_pending_split(&cards, "ses_test").await,
            "the batch is served"
        );
        assert!(Turn::card_is_live(&cards, "ses_test").await);
        assert_eq!(
            Turn::reply_target(&cards, "ses_test").await.as_deref(),
            Some("msg_sup_2")
        );
    }
}

/// Every supplement splits — there is no coalescing window: a second one
/// anchors its own continuation to itself and re-points the chain again.
#[tokio::test]
async fn every_supplement_splits_the_chain() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_turn(&app, "ses_test", "第一段进度。").await;

    app.handle_message(incoming(
        "msg_sup_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一".into(),
        None,
    ))
    .await;
    app.handle_message(incoming(
        "msg_sup_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充二".into(),
        None,
    ))
    .await;

    let continuations: Vec<(String, serde_json::Value)> = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { reply_to, card } if reply_to.starts_with("msg_sup") => {
                Some((reply_to.clone(), card.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        continuations
            .iter()
            .map(|(reply_to, _)| reply_to.as_str())
            .collect::<Vec<_>>(),
        vec!["msg_sup_1", "msg_sup_2"],
        "each supplement must anchor its own continuation"
    );
    // Delta: each continuation carries exactly its own supplement's receipt —
    // if a later one accumulated, it would carry both — and none re-renders
    // the content finalized before the split.
    for (i, (_, card)) in continuations.iter().enumerate() {
        assert_eq!(
            card.to_string().matches("📨 已收到补充").count(),
            1,
            "continuation {i} carries exactly one receipt: {card}"
        );
        assert!(
            !has(card, "第一段进度。"),
            "continuation {i} must not re-render finalized content: {card}"
        );
    }
    let cards = app.cards_handle();
    assert_eq!(
        Turn::reply_target(&cards, "ses_test").await.as_deref(),
        Some("msg_sup_2"),
        "the chain re-anchors at the newest supplement"
    );
    assert!(!Turn::has_pending_split(&cards, "ses_test").await);
}

/// A failed supplement send still splits: the failure notice is sent FIRST and
/// the continuation afterwards, so the live card remains the newest message.
#[tokio::test]
async fn failed_supplement_send_splits_after_the_failure_notice() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.fail_supplement("provider 503");
    let (app, platform) = build_app(cfg, backend).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_turn(&app, "ses_test", "第一段进度。").await;

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let notice = calls
        .iter()
        .position(|c| matches!(c, PlatformCall::ReplyText { text, .. } if text.contains("补充消息发送失败")))
        .expect("the failure notice must be sent");
    let continuation_at = calls
        .iter()
        .position(|c| matches!(c, PlatformCall::ReplyCard { .. }))
        .expect("the split must still happen");
    assert!(
        notice < continuation_at,
        "the failure notice goes first, the split still happens: {calls:?}"
    );
    assert!(
        matches!(calls.last(), Some(PlatformCall::ReplyCard { .. })),
        "the live card must remain the newest message: {calls:?}"
    );
    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(reply_to, "msg_sup");
    assert!(has(&card, "📨 已收到补充"), "the receipt still rides it: {card}");
    assert!(app.inflight.lock().await.contains("ses_test"));
}

/// A continuation send that FAILS keeps the split queued: the next flush
/// retries it (a later supplement arriving first queues behind it), and the
/// receipts are written exactly once per supplement — never duplicated, never
/// lost, anchored at the newest queued supplement.
#[tokio::test]
async fn a_failed_continuation_send_retries_without_duplicating_receipts() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(
            cfg,
            Arc::new(MockBackend::new(realistic_parts())),
            platform.clone(),
        )
        .unwrap(),
    );
    seed_session(&app, "ses_test", "/work").await;
    seed_live_turn(&app, "ses_test", "第一段进度。").await;

    // The first continuation send fails: its receipt was written and the split
    // stays queued for a retry.
    platform
        .fail_reply_card_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    app.handle_message(incoming(
        "msg_sup_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一".into(),
        None,
    ))
    .await;
    assert!(
        platform.replied_cards().await.is_empty(),
        "the failed send must not record a card"
    );
    {
        let cards = app.cards_handle();
        let queued = Turn::pending_splits(&cards, "ses_test").await;
        assert_eq!(queued.len(), 1, "the failed split stays queued");
        assert!(queued[0].1, "its receipt was written exactly once");
        assert!(
            !Turn::card_is_live(&cards, "ses_test").await,
            "the card is finalized, owing its continuation"
        );
    }

    // A second supplement arrives before the retry.
    app.handle_message(incoming(
        "msg_sup_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充二".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let continuations = supplement_continuations(&calls);
    assert_eq!(
        continuations.len(),
        1,
        "one retried continuation serves both supplements: {calls:?}"
    );
    let card = &continuations[0];
    assert_eq!(
        card.to_string().matches("📨 已收到补充").count(),
        2,
        "exactly one receipt per supplement, no duplicates: {card}"
    );
    assert!(
        !has(card, "第一段进度。"),
        "the retry must not re-render the content finalized on the previous card: {card}"
    );
    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        finalized.contains("第一段进度。"),
        "the content stays on the card finalized before the failed send: {finalized}"
    );
    assert!(
        matches!(
            calls.last(),
            Some(PlatformCall::ReplyCard { reply_to, .. }) if reply_to == "msg_sup_2"
        ),
        "it replies to the newest queued supplement: {calls:?}"
    );
    {
        let cards = app.cards_handle();
        assert!(
            !Turn::has_pending_split(&cards, "ses_test").await,
            "the queue is served"
        );
        assert!(
            Turn::card_is_live(&cards, "ses_test").await,
            "the continuation is live"
        );
        assert_eq!(
            Turn::reply_target(&cards, "ses_test").await.as_deref(),
            Some("msg_sup_2")
        );
    }

    // Later updates target the retried continuation.
    Turn::push_text(&app.cards_handle(), "ses_test", "后续进度。").await;
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
    let calls = platform.calls.lock().await.clone();
    let (message_id, later) = calls
        .iter()
        .rev()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
            _ => None,
        })
        .expect("a later flush");
    assert_eq!(message_id, "msg_reply");
    assert!(has(&later, "后续进度。"));
}

/// A continuation that is itself over the budget advances `render_from` while
/// it is built; if its send FAILS, the boundary must be restored so the retry
/// re-renders that same slice. Without the restore the failed slice reaches no
/// card and the chain silently skips it.
#[tokio::test]
async fn a_failed_full_continuation_send_retries_the_same_slice() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(
            cfg,
            Arc::new(MockBackend::new(realistic_parts())),
            platform.clone(),
        )
        .unwrap(),
    );
    seed_session(&app, "ses_test", "/work").await;

    // Three uniquely marked slices: the supplement flush finalizes S00, builds
    // S01 as a FULL continuation (advancing `render_from` to S02) and fails
    // that send.
    let long = marked_slices(3);
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
    Turn::push_text(&cards, "ses_test", &long).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    app.inflight.lock().await.insert("ses_test".to_string());
    platform
        .fail_reply_card_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;
    assert!(
        platform.replied_cards().await.is_empty(),
        "the first (full) continuation send failed and recorded nothing"
    );
    {
        let cards = app.cards_handle();
        assert!(
            !Turn::card_is_live(&cards, "ses_test").await,
            "the chain owes its continuation"
        );
        assert_eq!(
            Turn::render_from(&cards, "ses_test").await,
            Some(1),
            "the failed slice must stay at the boundary"
        );
    }

    // The retry re-renders S01, then continues with S02 + the receipt: nothing
    // is skipped or duplicated.
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
    let calls = platform.calls.lock().await.clone();
    let cards = supplement_continuations(&calls);
    assert_eq!(cards.len(), 2, "the failed slice then the remainder: {calls:?}");
    assert!(
        !has(&cards[0], "【S00】") && has(&cards[0], "【S01】"),
        "the retry re-renders exactly the failed slice: {}",
        cards[0]
    );
    assert!(
        has(&cards[1], "【S02】") && has(&cards[1], "📨 已收到补充"),
        "the remainder and the receipt follow: {}",
        cards[1]
    );

    let finalized = last_update_of(&platform, "om_live").await;
    let mut delivered = finalized;
    for card in &cards {
        delivered.push_str(&card.to_string());
    }
    for i in 0..3 {
        let marker = format!("【S{i:02}】");
        assert_eq!(
            delivered.matches(&marker).count(),
            1,
            "{marker} must appear exactly once across the chain"
        );
    }
}

/// A command run mid-turn does not split the chain: its reply is the user's
/// current interaction and stays the newest message.
#[tokio::test]
async fn command_mid_turn_does_not_split_the_chain() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;
    seed_live_turn(&app, "ses_test", "第一段进度。").await;

    app.handle_message(incoming(
        "msg_cmd".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/help".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    // The command's own reply (here `/help`'s card) is the newest message —
    // replied to the command, not to the supplement, and never a chain
    // continuation of the live card.
    assert!(
        matches!(
            calls.last(),
            Some(PlatformCall::ReplyCard { reply_to, .. }) if reply_to == "msg_cmd"
        ),
        "the command reply stays the newest message: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, card }
                if message_id == "om_live" && card_text(card).contains("部分完成，继续中")
        )),
        "a command must never split the chain: {calls:?}"
    );
    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("om_live"),
        "the command must not re-point the live card"
    );
    assert!(!Turn::has_pending_split(&cards, "ses_test").await);
}

/// A pending permission inlined on the live card migrates to the continuation:
/// the previous card's controls are settled (it renders no tail), the block
/// stays live on the continuation, and it is still answerable there.
#[tokio::test]
async fn supplement_split_migrates_a_pending_permission() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_live", "ses_test", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    // The live card carries the inline permission (the poller path), so the
    // card handle records the block before the split.
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
    Turn::push_text(&cards, "ses_test", "回合的内容。").await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    app.inflight.lock().await.insert("ses_test".to_string());
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    assert_eq!(
        app.card_handles.lock().await.message_of("per_live"),
        Some("om_live"),
        "precondition: the live card shows the block"
    );

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    // The old card keeps its content but loses the controls (the header is the
    // accumulator's awaiting-action override, exactly as a size split does
    // while the block is still live).
    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        !finalized.contains("🔐 **权限请求**") && !finalized.contains("允许一次"),
        "the previous card's controls must be settled, not left dead: {finalized}"
    );
    assert!(
        finalized.contains("回合的内容。"),
        "the finalized card keeps its streamed content: {finalized}"
    );

    // The continuation carries the live block and its handle points at it.
    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(reply_to, "msg_sup");
    assert!(
        has(&card, "🔐 **权限请求**") && has(&card, "允许一次"),
        "the live block must ride the continuation: {card}"
    );
    assert!(has(&card, "📨 已收到补充"));
    assert!(
        !has(&card, "回合的内容。"),
        "the continuation carries the delta, not the finalized content: {card}"
    );
    assert_eq!(
        app.card_handles.lock().await.message_of("per_live"),
        Some("msg_reply"),
        "the handle follows the continuation"
    );

    // And it is answerable there: the click resolves on the continuation.
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_test",
            "directory": "/work",
            "request_id": "per_live",
            "open_message_id": "msg_reply",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await
        .expect("a card-action result");
    let ack = result
        .card
        .as_ref()
        .expect("the ack carries the card")
        .to_string();
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "the migrated block must resolve: {ack}"
    );
    assert!(
        !ack.contains("🔐 **权限请求**"),
        "the resolved block's controls must be gone: {ack}"
    );
    assert_eq!(backend.reply_permission_calls.lock().await.len(), 1);
}

/// The same handoff for an inline question: the continuation carries the
/// controls, the previous card's are settled, and a partial answer still
/// refreshes the continuation.
#[tokio::test]
async fn supplement_split_migrates_a_pending_question() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
    Turn::push_text(&cards, "ses_test", "回合的内容。").await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    Turn::add_question(
        &cards,
        "ses_test",
        &question_request("que_live", "ses_test"),
        "/work",
        &[None, None],
        &[false; 2],
    )
    .await;
    app.inflight.lock().await.insert("ses_test".to_string());
    Turn::flush_card(&app.cards_handle(), "ses_test").await;
    app.question
        .remember_question(&question_request("que_live", "ses_test"), "/work")
        .await;

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    let finalized = last_update_of(&platform, "om_live").await;
    assert!(
        !finalized.contains("选目录") && !finalized.contains("选分支"),
        "the previous card's question controls must be settled: {finalized}"
    );
    assert!(
        finalized.contains("回合的内容。"),
        "the previous card keeps the content finalized before the split: {finalized}"
    );
    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(reply_to, "msg_sup");
    assert!(
        has(&card, "选目录") && has(&card, "选分支"),
        "the question must ride the continuation: {card}"
    );
    assert!(
        !has(&card, "回合的内容。"),
        "the continuation carries the delta, not the finalized content: {card}"
    );

    // A partial answer on the continuation still lands there (已选 marker in
    // the ack), proving the moved block is live, not a dead copy.
    let result = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_live",
            "session_id": "ses_test",
            "directory": "/work",
            "question_index": 0,
            "answer": "/a",
            "open_message_id": "msg_reply",
        }))
        .await
        .expect("a card-action result");
    let ack = result
        .card
        .as_ref()
        .expect("the ack carries the card")
        .to_string();
    assert!(ack.contains("已选：/a"), "marker missing: {ack}");
    assert!(
        ack.contains("📨 已收到补充") && !ack.contains("回合的内容。"),
        "the ack is the continuation's delta: {ack}"
    );
}

/// When a size split and a supplement split coincide, exactly one continuation
/// is sent — replied to the supplement, with the receipt on it — and no content
/// is lost: the finalized card keeps its slice and the continuation carries the
/// remainder.
#[tokio::test]
async fn size_and_supplement_split_collide_with_one_continuation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;

    // A card far over the component budget (the card_handles split fixture):
    // a plain flush would already split it, and the supplement lands at the
    // same moment.
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_filled")).await;
    Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
    for i in 0..50 {
        Turn::push_tool(
            &cards,
            "ses_test",
            &format!("call_{i}"),
            crate::feishu::card::tool_render::ToolPanel::from_parts(
                &format!("tool{i}"),
                ToolStatus::Completed,
                None,
                None,
            ),
        )
        .await;
    }
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    app.inflight.lock().await.insert("ses_test".to_string());

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    let (reply_to, card) = continuation(&platform).await;
    assert_eq!(reply_to, "msg_sup");
    assert!(has(&card, "📨 已收到补充"), "the receipt rides it: {card}");
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "exactly one continuation: {:?}",
        platform.calls.lock().await
    );

    // No content is lost and none is re-rendered: the finalized card keeps the
    // first slice, the continuation carries the remainder only (a delta split
    // either way), and every tool appears exactly once across the chain.
    let finalized = last_update_of(&platform, "om_filled").await;
    assert!(finalized.contains("tool0"), "the first slice stays: {finalized}");
    assert!(
        !has(&card, "tool0"),
        "the continuation must not re-render the finalized slice: {card}"
    );
    let mut all = finalized;
    all.push_str(&card.to_string());
    for i in 0..50 {
        assert!(all.contains(&format!("tool{i}")), "tool{i} vanished");
    }
}

/// The `MAX_CARD_CHAIN` size bound never refuses a supplement split: content
/// needing more slices than the cap still ends with the continuation sent (the
/// live card is the newest message), and any remaining slice is reconciled on
/// the next flush.
#[tokio::test]
async fn the_chain_bound_never_refuses_a_supplement_split() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;

    // Ten slices of text (60000 chars, each card holds 6000): more
    // continuations than the per-flush cap of 8.
    let long = "很长的回答。".repeat(10_000);
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
    Turn::push_text(&cards, "ses_test", &long).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    app.inflight.lock().await.insert("ses_test".to_string());

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    // The supplement split was not refused: a continuation replied to it, and
    // the live card is the newest message.
    let (reply_to, _) = continuation(&platform).await;
    assert_eq!(reply_to, "msg_sup", "the supplement continuation must be sent");
    assert!(
        matches!(
            platform.calls.lock().await.last(),
            Some(PlatformCall::ReplyCard { .. })
        ),
        "the live card must be the newest message: {:?}",
        platform.calls.lock().await
    );
    {
        let cards = app.cards_handle();
        assert_eq!(
            Turn::card_message_id(&cards, "ses_test").await.as_deref(),
            Some("msg_reply")
        );
        assert!(
            !Turn::has_pending_split(&cards, "ses_test").await,
            "the split is consumed"
        );
    }

    // The remaining slice is reconciled on the next flush: the full text
    // eventually renders across the chain.
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
    let delivered: String = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } | PlatformCall::UpdateMessage { card, .. } => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect();
    let chars = delivered.chars().filter(|c| *c != '"').count();
    assert!(
        chars >= long.chars().count(),
        "all text must be delivered across the chain: {chars} < {}",
        long.chars().count()
    );
}

/// A chain that exhausts the bound must resume on the FOLLOW-UP flush by
/// sending the remaining slice as a NEW continuation — never by patching the
/// finalized slice the bound left tracked (the mock reuses one reply id, so an
/// in-place overwrite shows up as an `UpdateMessage` on the tracked card).
#[tokio::test]
async fn bound_exhaustion_does_not_overwrite_the_finalized_continuation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    seed_session(&app, "ses_test", "/work").await;

    // Ten uniquely marked slices; each fills exactly one card.
    let long = marked_slices(10);
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_test", Some("om_live")).await;
    Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
    Turn::push_text(&cards, "ses_test", &long).await;
    Turn::set_reply_target(&cards, "ses_test", "msg_1").await;
    app.inflight.lock().await.insert("ses_test".to_string());

    app.handle_message(incoming(
        "msg_sup".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "补充一下".into(),
        None,
    ))
    .await;

    // The supplement flush stops at the bound (`MAX_CARD_CHAIN` continuations),
    // leaving the last slice (S09) unsent and the tracked card a finalized
    // slice carrying S08.
    let calls = platform.calls.lock().await.clone();
    let before = supplement_continuations(&calls);
    assert_eq!(
        before.len(),
        crate::bridge::turn::MAX_CARD_CHAIN,
        "the supplement flush ends at the chain bound: {:?}",
        platform.calls.lock().await
    );
    assert!(
        has(before.last().unwrap(), "【S08】"),
        "the last finalized continuation carries its own slice: {:?}",
        before.last()
    );
    assert!(
        !before.iter().any(|c| has(c, "【S09】")),
        "the remaining slice must not have been sent yet"
    );

    // The follow-up flush reconciles S09 on a NEW continuation.
    crate::bridge::turn::Turn::flush_card(&app.cards_handle(), "ses_test").await;
    let calls = platform.calls.lock().await.clone();
    let after = supplement_continuations(&calls);
    assert_eq!(
        after.len(),
        crate::bridge::turn::MAX_CARD_CHAIN + 1,
        "the remainder must land on a new continuation: {calls:?}"
    );
    let last = after.last().unwrap();
    assert!(
        has(last, "【S09】"),
        "the new continuation carries the remaining slice: {last}"
    );
    assert!(
        has(last, "📨 已收到补充"),
        "the supplement receipt rides the live card: {last}"
    );
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { message_id, .. } if message_id == "msg_reply"
        )),
        "a finalized continuation must never be patched in place: {calls:?}"
    );
    // Every slice is on the chain, and the chain ends live.
    let delivered: String = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } | PlatformCall::UpdateMessage { card, .. } => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect();
    for i in 0..10 {
        let marker = format!("【S{i:02}】");
        assert!(delivered.contains(&marker), "{marker} vanished");
    }
    let cards = app.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_test").await.as_deref(),
        Some("msg_reply")
    );
    assert!(
        Turn::card_is_live(&cards, "ses_test").await,
        "the new continuation is the live card"
    );
}
