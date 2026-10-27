//! Hermetic card rendering through the real bridge (spec #860, ticket #880).
//!
//! The scripted app-server stands in for the runtime; the real [`CodexClient`],
//! [`App`] and `RecordingPlatform` drive the card, so these tests assert the
//! Feishu payload the reader sees rather than a hand-built transcript. A second
//! `CodexClient` is the user's own client: the same turn stream must reach both
//! connections, and only one of them renders.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::bridge::App;
use crate::bridge::test_support::{
    CardUpdates, HostCardAction, PlatformCall, RecordingPlatform, SETTLE_DEADLINE, card_buttons, card_header,
    card_text, incoming, seed_entry, send_command_in, test_config, wait_for_card_update, wait_for_handler,
    wait_for_recorded_card,
};
use crate::bridge::turn::Turn;
use crate::config::SessionEntry;

use super::client::{CodexClient, CodexOptions};
use super::schema::PinnedProbe;
use super::test_server::{FakeServer, ScriptedThread};
use super::transport::Endpoint;

const THREAD: &str = "thread-1";
const CHAT: &str = "chat-1";
const WORK: &str = "/work/cola";
/// The Feishu question card's session-scope note, mirrored as text so this
/// adapter test does not import the platform layer; the platform's own test
/// pins the real constant.
const SESSION_SCOPE_NOTE: &str = "🔁 始终允许：本会话内后续同类请求将自动批准";

async fn connect(url: &str) -> CodexClient {
    let endpoint = Endpoint::parse(url).expect("endpoint");
    CodexClient::connect(&CodexOptions::new(endpoint), &PinnedProbe)
        .await
        .expect("attach to the scripted app-server")
}

/// Build the bridge around a fresh Codex connection and map the thread to a
/// chat. `watermark` is the Sync Watermark the Session Sync pass starts from:
/// a recorded user message at or below it is already accounted for.
async fn build_bridge(
    url: &str,
    session_file: &Path,
    watermark: i64,
) -> (Arc<App>, Arc<RecordingPlatform>, CodexClient) {
    build_bridge_authored(
        url,
        session_file,
        watermark,
        super::authorship::Authorship::in_memory(),
    )
    .await
}

/// [`build_bridge`] with the turns-cola-started record the client answers
/// authorship from (ADR-0090). A restart test passes the same durable record
/// a second process would load.
async fn build_bridge_authored(
    url: &str,
    session_file: &Path,
    watermark: i64,
    authored: Arc<super::authorship::Authorship>,
) -> (Arc<App>, Arc<RecordingPlatform>, CodexClient) {
    let endpoint = Endpoint::parse(url).expect("endpoint");
    let client = CodexClient::connect(
        &CodexOptions::new(endpoint).with_authorship(authored),
        &PinnedProbe,
    )
    .await
    .expect("attach to the scripted app-server");
    let platform = Arc::new(RecordingPlatform::new());
    let backend: Arc<dyn Backend> = Arc::new(client.clone());
    let app =
        Arc::new(App::new(test_config(session_file), backend, platform.clone()).expect("the bridge app"));
    seed_entry(
        &app,
        SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: crate::config::ThreadKey::new(CHAT.into(), CHAT.into()),
            session_id: THREAD.into(),
            directory: WORK.into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert(THREAD.into(), watermark);
    app.external.poll_interval_ms.store(20, Ordering::Relaxed);
    app.external.render_poll_ms.store(5, Ordering::Relaxed);
    app.external.request_timeout_ms.store(500, Ordering::Relaxed);
    app.external
        .render_idle_timeout_ms
        .store(10_000, Ordering::Relaxed);
    app.completion_notice_delay_ms.store(0, Ordering::Relaxed);
    (app, platform, client)
}

/// Start Session Sync on its own task; the returned handle lets a restart test
/// stop the prior process's loop.
fn spawn_sync(app: &Arc<App>) -> tokio::task::JoinHandle<()> {
    let app = app.clone();
    tokio::spawn(async move {
        let _ = app.external.poll_loop(&app.flow_handles()).await;
    })
}

fn recording() -> Vec<Value> {
    serde_json::from_str(include_str!("fixtures/turn-recording.json")).expect("the recording is JSON")
}

/// Wait until no connection holds the thread: a dropped client deregisters its
/// server-side stream asynchronously.
async fn wait_for_all_unsubscribed(server: &FakeServer) {
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    while server.subscribers(THREAD) > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a dropped connection kept its subscription: {}",
            server.subscribers(THREAD)
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Wait until the App's own connection holds a live, seeded subscription.
/// A server-side subscriber count can include a connection whose local holder
/// has already been released (and whose events are therefore dropped), so the
/// client's own state is the fact a test must wait on.
async fn wait_for_seeded(client: &CodexClient) {
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    while !client.projections.is_seeded(THREAD) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the app's thread subscription never seeded"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// The first card the platform updated — the live card the notification armed.
async fn first_updated_card(platform: &RecordingPlatform) -> String {
    platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|call| match call {
            PlatformCall::UpdateMessage { message_id, .. } => Some(message_id.clone()),
            _ => None,
        })
        .expect("the card was updated")
}

/// Ticket #880 acceptance 1: a turn the user's own connection **starts** —
/// `turn/start` from the second connection, with the fake server emitting the
/// stream — reaches both subscribers, and the Feishu card renders it from the
/// first item to the settled end.
#[tokio::test]
async fn a_turn_started_on_another_connection_renders_on_the_feishu_card() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));

    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    // The user's own client: a second connection subscribed to the same thread.
    let tui = connect(&url).await;
    tui.transcript(THREAD).await.expect("the TUI's subscription");
    server.wait_for_subscribers(THREAD, 2).await;
    // The turn's origin is this connection's `turn/start`; the fake server —
    // not the test — emits the recorded stream to every subscriber.
    server.script_turn(THREAD, recording());
    tui.prompt(
        THREAD,
        "Fix the build",
        &[],
        &[],
        None,
        None,
        None,
        Some("msg_cola_from_tui"),
    )
    .await
    .expect("the second connection starts the turn");
    assert_eq!(
        server.turn_starts(),
        1,
        "the turn was started on the other connection"
    );

    wait_for_card_update(&platform, "the streamed answer", CardUpdates::Any, |card| {
        card_text(card).contains("The fix")
    })
    .await;
    wait_for_card_update(&platform, "the settled turn", CardUpdates::Latest, |card| {
        card_header(card).contains("完成")
    })
    .await;

    let card = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the notification card's settled update");
    let text = card_text(&card);
    assert!(
        text.contains("Fix the build"),
        "the user's prompt renders: {card}"
    );
    assert!(text.contains("The fix"), "the answer renders: {card}");
    // The TUI's message and the agent's answer must be DISTINGUISHABLE, not
    // merely both present: the user's text is its own quoted user block, and
    // the answer is a different element (live 2026-10-26: both read as one
    // paragraph).
    let elements = card["body"]["elements"].as_array().expect("elements");
    let user = elements
        .iter()
        .position(|e| e["content"].as_str().is_some_and(|c| c.contains("Fix the build")))
        .unwrap_or_else(|| panic!("the user block is missing: {card}"));
    let answer = elements
        .iter()
        .position(|e| e["content"].as_str().is_some_and(|c| c.contains("The fix")))
        .unwrap_or_else(|| panic!("the answer is missing: {card}"));
    assert_ne!(
        user, answer,
        "the message and the answer must be separate elements: {card}"
    );
    assert!(user < answer, "the message heads the card: {card}");
    let block = elements[user]["content"].as_str().unwrap();
    assert!(
        block.starts_with("> 👤") && block.lines().all(|line| line.starts_with('>')),
        "the TUI's message renders as its own quoted user block: {block}"
    );

    // The other face saw the same stream: the TUI's read is not a different
    // transcript. The subscriber runs on its own task, so await its read.
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    loop {
        let read = tui.transcript(THREAD).await.expect("the TUI's read");
        if read
            .messages
            .iter()
            .any(|message| message.id.as_str() == "item-agent" && message.text() == "The fix")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second connection never saw the turn stream"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    sync.abort();
}

/// Ticket #880 acceptance 5: a terminal `error` notification mid-turn settles
/// the card with the error text even though no `turn/completed` follows.
#[tokio::test]
async fn a_terminal_error_notification_settles_the_card_with_its_text() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));

    let directory = tempfile::tempdir().expect("tempdir");
    let now = chrono::Utc::now().timestamp_millis() / 1_000 * 1_000;
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), now - 60_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    let started = now / 1_000;
    server.push_to_thread(
        THREAD,
        "turn/started",
        json!({
            "threadId": THREAD,
            "turn": { "id": "turn-live", "status": "inProgress", "startedAt": started }
        }),
    );
    server.push_to_thread(
        THREAD,
        "item/started",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "startedAtMs": now - 1_000,
            "item": {
                "type": "userMessage",
                "id": "item-user",
                "content": [{ "type": "text", "text": "do the thing" }]
            }
        }),
    );
    server.push_to_thread(
        THREAD,
        "item/started",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "startedAtMs": now,
            "item": { "type": "agentMessage", "id": "item-agent", "text": "" }
        }),
    );
    server.push_to_thread(
        THREAD,
        "item/agentMessage/delta",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "itemId": "item-agent",
            "delta": "working"
        }),
    );

    wait_for_card_update(&platform, "the live reply", CardUpdates::Any, |card| {
        card_text(card).contains("working")
    })
    .await;

    // No `turn/completed`: the terminal error is the ending.
    server.push_to_thread(
        THREAD,
        "error",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "error": { "message": "401 unauthorized" },
            "willRetry": false
        }),
    );

    wait_for_card_update(&platform, "the failure line", CardUpdates::Any, |card| {
        card_text(card).contains("401 unauthorized")
    })
    .await;
    wait_for_card_update(&platform, "the settled failure", CardUpdates::Latest, |card| {
        card_header(card).contains("出错")
    })
    .await;

    let card = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the settled card");
    assert!(
        !card_header(&card).contains("思考中") && !card_header(&card).contains("回复中"),
        "the card is not left hanging on a spinner: {}",
        card_header(&card)
    );

    sync.abort();
}

/// ADR-0090's credential error is `willRetry: true`: it is not the turn's
/// failure. The card must keep streaming (and never show the retry text as a
/// ❌ ending) until the run really ends.
#[tokio::test]
async fn a_retrying_error_keeps_the_card_live_and_off_the_error_ending() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));

    let directory = tempfile::tempdir().expect("tempdir");
    let now = chrono::Utc::now().timestamp_millis() / 1_000 * 1_000;
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), now - 60_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    server.push_to_thread(
        THREAD,
        "turn/started",
        json!({
            "threadId": THREAD,
            "turn": { "id": "turn-live", "status": "inProgress", "startedAt": now / 1_000 }
        }),
    );
    server.push_to_thread(
        THREAD,
        "item/started",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "startedAtMs": now - 1_000,
            "item": {
                "type": "userMessage",
                "id": "item-user",
                "content": [{ "type": "text", "text": "do the thing" }]
            }
        }),
    );
    server.push_to_thread(
        THREAD,
        "item/started",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "startedAtMs": now,
            "item": { "type": "agentMessage", "id": "item-agent", "text": "" }
        }),
    );
    server.push_to_thread(
        THREAD,
        "item/agentMessage/delta",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "itemId": "item-agent",
            "delta": "working"
        }),
    );
    wait_for_card_update(&platform, "the live reply", CardUpdates::Any, |card| {
        card_text(card).contains("working")
    })
    .await;

    server.push_to_thread(
        THREAD,
        "error",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "error": { "message": "stream disconnected" },
            "willRetry": true
        }),
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let cards = platform.updated_cards().await;
    assert!(
        cards
            .iter()
            .all(|card| !card_text(card).contains("stream disconnected")),
        "the retrying text is not the turn's failure: {cards:?}"
    );
    assert!(
        cards.iter().all(|card| !card_header(card).contains("出错")),
        "the card does not settle ❌ on a retry: {cards:?}"
    );

    // The retry succeeds and the turn ends normally.
    server.push_to_thread(
        THREAD,
        "item/completed",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "completedAtMs": now + 1_000,
            "item": { "type": "agentMessage", "id": "item-agent", "text": "recovered" }
        }),
    );
    server.push_to_thread(
        THREAD,
        "turn/completed",
        json!({
            "threadId": THREAD,
            "turn": {
                "id": "turn-live",
                "status": "completed",
                "startedAt": now / 1_000,
                "completedAt": now / 1_000 + 2
            }
        }),
    );
    wait_for_card_update(&platform, "the recovered turn", CardUpdates::Latest, |card| {
        card_header(card).contains("完成") && card_text(card).contains("recovered")
    })
    .await;

    sync.abort();
}

/// Ticket #880 review finding: the error can arrive before any assistant item.
/// The card must still settle with its text — the read model exposes the failed
/// turn through a stable synthetic message instead of losing it.
#[tokio::test]
async fn a_terminal_error_before_any_assistant_item_still_settles_with_its_text() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));

    let directory = tempfile::tempdir().expect("tempdir");
    let now = chrono::Utc::now().timestamp_millis() / 1_000 * 1_000;
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), now - 60_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    let started = now / 1_000;
    server.push_to_thread(
        THREAD,
        "turn/started",
        json!({
            "threadId": THREAD,
            "turn": { "id": "turn-live", "status": "inProgress", "startedAt": started }
        }),
    );
    server.push_to_thread(
        THREAD,
        "item/started",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "startedAtMs": now - 1_000,
            "item": {
                "type": "userMessage",
                "id": "item-user",
                "content": [{ "type": "text", "text": "do the thing" }]
            }
        }),
    );

    wait_for_card_update(&platform, "the user's message", CardUpdates::Any, |card| {
        card_text(card).contains("do the thing")
    })
    .await;

    // The credential failure ADR-0090 records is terminal here, and no model
    // item ever landed.
    server.push_to_thread(
        THREAD,
        "error",
        json!({
            "threadId": THREAD,
            "turnId": "turn-live",
            "error": { "message": "401 unauthorized" },
            "willRetry": false
        }),
    );

    wait_for_card_update(&platform, "the failure line", CardUpdates::Any, |card| {
        card_text(card).contains("401 unauthorized")
    })
    .await;
    wait_for_card_update(&platform, "the settled failure", CardUpdates::Latest, |card| {
        card_header(card).contains("出错")
    })
    .await;

    sync.abort();
}

/// Ticket #880 acceptance 3: a Codex `contextCompaction` item renders once on
/// the card that owns it, and a cola restart — a fresh process whose adapter
/// rebuilds the projection from persisted history — does not render it again.
/// The "once" comes from the durable Rendered Cursor/delivered position, not
/// from the projection: the restarted read still carries the compaction.
#[tokio::test]
async fn a_compaction_renders_once_and_survives_a_restart() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    let mut thread = ScriptedThread::adopted(THREAD, "session-root", WORK, "a real thread");
    server.seed_thread(thread.clone());

    let directory = tempfile::tempdir().expect("tempdir");
    let session_file = directory.path().join("sessions.json");
    let now = chrono::Utc::now().timestamp_millis() / 1_000 * 1_000;
    let turn_one = now - 10_000;

    // The first cola process.
    let (first, first_platform, first_client) = build_bridge(&url, &session_file, turn_one - 60_000).await;
    let first_sync = spawn_sync(&first);
    server.wait_for_subscribers(THREAD, 1).await;
    push_turn_with_compaction(&server, "turn-1", "item-user-1", turn_one, "first", true);
    wait_for_card_update(&first_platform, "the first answer", CardUpdates::Any, |card| {
        card_text(card).contains("first answer")
    })
    .await;
    wait_for_card_update(
        &first_platform,
        "the first compaction",
        CardUpdates::Any,
        |card| card_text(card).contains("已压缩上下文"),
    )
    .await;
    wait_for_card_update(
        &first_platform,
        "the first settled turn",
        CardUpdates::Latest,
        |card| card_header(card).contains("完成"),
    )
    .await;
    let first_card = first_platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the first card");
    assert_eq!(
        card_text(&first_card).matches("已压缩上下文").count(),
        1,
        "the compaction renders exactly once: {first_card}"
    );
    let original_card = first_updated_card(&first_platform).await;

    first_sync.abort();
    drop(first);
    drop(first_client);
    drop(first_platform);
    wait_for_all_unsubscribed(&server).await;

    // The restart: the adapter rebuilds the same turn from persisted history.
    thread.turns = vec![persisted_turn("turn-1", "item-user-1", turn_one, "first")];
    server.set_turns(THREAD, thread.turns.clone());

    let (second, second_platform, second_client) = build_bridge(&url, &session_file, turn_one + 1_000).await;
    // The original card, re-rendered by the restarted process. The rebuilt read
    // still carries the compaction; only the durable delivered position keeps
    // it off the card.
    let rebuilt_client = connect(&url).await;
    let rebuilt = rebuilt_client.transcript(THREAD).await.expect("the rebuilt read");
    assert!(
        rebuilt
            .messages
            .iter()
            .any(|message| message.id.as_str() == "comp-turn-1"),
        "the compaction is in the restarted projection: {:?}",
        rebuilt
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>()
    );
    let anchor = rebuilt
        .anchor_of_user("item-user-1")
        .expect("the first turn's anchor");
    let flow = second.flow_handles();
    Turn::arm_external_render(
        &flow.cards,
        THREAD,
        &original_card,
        &anchor,
        "sub",
        WORK,
        None,
        None,
    )
    .await;
    Turn::render_and_flush(
        &flow.cards,
        &flow.sessions,
        &flow.backend,
        &flow.requests,
        THREAD,
        &rebuilt,
    )
    .await
    .expect("the original card re-renders");
    let rereleased = second_platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::UpdateMessage { message_id, card }
                if message_id.as_str() == original_card.as_str() =>
            {
                Some(card.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let rerendered = rereleased
        .last()
        .expect("the restarted process re-renders the original card");
    assert!(
        card_text(rerendered).contains("first answer"),
        "the original card is re-rendered, not left stale: {rerendered}"
    );
    assert!(
        !card_text(rerendered).contains("已压缩上下文"),
        "the delivered compaction is not replayed onto the original card: {rerendered}"
    );

    // A later turn on the restarted process also renders no compaction.
    let second_sync = spawn_sync(&second);
    wait_for_seeded(&second_client).await;
    let turn_two = now;
    push_turn_with_compaction(&server, "turn-2", "item-user-2", turn_two, "second", false);
    wait_for_card_update(&second_platform, "the second answer", CardUpdates::Any, |card| {
        card_text(card).contains("second answer")
    })
    .await;
    let second_cards = second_platform.updated_cards().await;
    assert!(
        second_cards
            .iter()
            .all(|card| !card_text(card).contains("已压缩上下文")),
        "the delivered compaction is not replayed after a restart: {second_cards:?}"
    );

    second_sync.abort();
}

/// Push one scripted turn whose items are `user → [contextCompaction] → agent`.
fn push_turn_with_compaction(
    server: &FakeServer,
    turn_id: &str,
    user_item: &str,
    started_ms: i64,
    answer: &str,
    compaction: bool,
) {
    server.push_to_thread(
        THREAD,
        "turn/started",
        json!({
            "threadId": THREAD,
            "turn": { "id": turn_id, "status": "inProgress", "startedAt": started_ms / 1_000 }
        }),
    );
    server.push_to_thread(
        THREAD,
        "item/started",
        json!({
            "threadId": THREAD,
            "turnId": turn_id,
            "startedAtMs": started_ms,
            "item": {
                "type": "userMessage",
                "id": user_item,
                "content": [{ "type": "text", "text": format!("message {answer}") }]
            }
        }),
    );
    let mut items = vec![json!({
        "type": "agentMessage",
        "id": format!("agent-{turn_id}"),
        "text": format!("{answer} answer")
    })];
    if compaction {
        items.insert(
            0,
            json!({ "type": "contextCompaction", "id": format!("comp-{turn_id}") }),
        );
    }
    for item in items {
        server.push_to_thread(
            THREAD,
            "item/completed",
            json!({
                "threadId": THREAD,
                "turnId": turn_id,
                "completedAtMs": started_ms + 1,
                "item": item,
            }),
        );
    }
    server.push_to_thread(
        THREAD,
        "turn/completed",
        json!({
            "threadId": THREAD,
            "turn": {
                "id": turn_id,
                "status": "completed",
                "startedAt": started_ms / 1_000,
                "completedAt": started_ms / 1_000 + 1
            }
        }),
    );
}

/// The persisted history the restarted adapter pages back.
fn persisted_turn(turn_id: &str, user_item: &str, started_ms: i64, answer: &str) -> Value {
    json!({
        "id": turn_id,
        "status": "completed",
        "startedAt": started_ms / 1_000,
        "completedAt": started_ms / 1_000 + 1,
        "error": null,
        "items": [
            {
                "type": "userMessage",
                "id": user_item,
                "content": [{ "type": "text", "text": format!("message {answer}") }]
            },
            { "type": "contextCompaction", "id": format!("comp-{turn_id}") },
            {
                "type": "agentMessage",
                "id": format!("agent-{turn_id}"),
                "text": format!("{answer} answer")
            }
        ]
    })
}

/// One plain Feishu message from the test Host, addressed to the mapped chat.
fn prompt_message(text: &str, message_id: &str) -> crate::bridge::IncomingMessage {
    incoming(
        message_id.to_string(),
        CHAT.to_string(),
        "p2p".to_string(),
        None,
        text.to_string(),
        None,
    )
}

/// Wait until the server holds a live turn for the thread (the bridge's Turn
/// took its guard before submitting, so a supplement or `/stop` sent after this
/// is racing nothing).
async fn wait_for_live_turn(server: &FakeServer) -> String {
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    loop {
        if let Some(turn) = server.live_turn(THREAD) {
            return turn;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the bridge never started its turn"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Wait until `turn/start` has arrived for the thread (the bridge submits on
/// its own task, so a synchronous assertion would race it).
async fn wait_for_turn_start(server: &FakeServer) -> Value {
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    loop {
        if let Some(params) = server.turn_params(THREAD).last().cloned() {
            return params;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the bridge never submitted its prompt"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Ticket #881 acceptance 1: a Feishu prompt reaches the thread as the server's
/// own user message (the request's content, persisted as a turn) and the turn
/// completes on the card.
#[tokio::test]
async fn a_feishu_prompt_becomes_a_user_message_and_completes() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    server.echo_prompts(THREAD);
    let now = chrono::Utc::now().timestamp();
    server.script_turn(
        THREAD,
        vec![
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": THREAD,
                    "turnId": "turn-1",
                    "completedAtMs": now * 1000,
                    "item": { "type": "agentMessage", "id": "agent-1", "text": "the codex answer" },
                },
            }),
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": THREAD,
                    "turn": { "id": "turn-1", "status": "completed", "startedAt": now, "completedAt": now + 1 },
                },
            }),
        ],
    );

    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, client) = build_bridge(&url, &directory.path().join("sessions.json"), 0).await;

    let handler = tokio::spawn({
        let app = app.clone();
        async move {
            app.handle_message(prompt_message("please help", "om_prompt"))
                .await
        }
    });
    wait_for_card_update(&platform, "the answer", CardUpdates::Any, |card| {
        card_text(card).contains("the codex answer")
    })
    .await;
    wait_for_card_update(&platform, "the settled turn", CardUpdates::Any, |card| {
        card_header(card).contains("完成")
    })
    .await;
    wait_for_handler(handler, "the prompt", SETTLE_DEADLINE).await;

    let card = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the settled card");
    assert!(
        card_header(&card).contains("完成"),
        "the turn completes: {}",
        card_header(&card)
    );
    assert!(
        card_text(&card).contains("the codex answer"),
        "the answer renders on the card: {card}"
    );

    // The prompt is the thread's own user message (the Feishu message the user
    // sent is the card's other face, not re-rendered on it), exactly once.
    let transcript = client.transcript(THREAD).await.expect("the thread's read");
    let user: Vec<_> = transcript
        .messages
        .iter()
        .filter(|message| message.role == crate::backend::MessageRole::User)
        .collect();
    assert_eq!(user.len(), 1, "one user message for one prompt");
    assert_eq!(user[0].text(), "please help");
}

/// Ticket #881 acceptance 3 through the bridge: a second message while cola owns
/// the live run is a Supplement → `turn/steer`, never a second `turn/start`, and
/// its text renders once on the split continuation.
#[tokio::test]
async fn a_supplement_steers_the_live_turn_and_renders_once() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    server.echo_prompts(THREAD);
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) = build_bridge(&url, &directory.path().join("sessions.json"), 0).await;

    let first = tokio::spawn({
        let app = app.clone();
        async move {
            app.handle_message(prompt_message("first message", "om_first"))
                .await
        }
    });
    let live = wait_for_live_turn(&server).await;
    // `thread/resume` is the subscription: pushing the turn's events before it
    // registers would send them to nobody, and the turn would never finish.
    server.wait_for_subscribers(THREAD, 1).await;

    // The supplement: cola holds the live execution, so it steers.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.handle_message(prompt_message("extra context", "om_supplement")),
    )
    .await
    .expect("the supplement returns promptly");

    assert_eq!(
        server.turn_starts(),
        1,
        "a Supplement must not start a second turn"
    );
    let steers = server.steer_params();
    assert_eq!(steers.len(), 1, "the supplement used turn/steer");
    assert_eq!(steers[0]["expectedTurnId"].as_str(), Some(live.as_str()));
    assert_eq!(steers[0]["input"][0]["text"], "extra context");

    // The turn ends after the steer: the answer and the completion.
    let now = chrono::Utc::now().timestamp();
    server.push_to_thread(
        THREAD,
        "item/completed",
        json!({
            "threadId": THREAD,
            "turnId": live,
            "completedAtMs": now * 1000,
            "item": { "type": "agentMessage", "id": "agent-after-steer", "text": "the steered answer" },
        }),
    );
    server.push_to_thread(
        THREAD,
        "turn/completed",
        json!({
            "threadId": THREAD,
            "turn": { "id": live, "status": "completed", "startedAt": now, "completedAt": now + 1 },
        }),
    );
    // Wait on the observable outcomes, not on the handler: the supplement's
    // continuation (a reply) and the steered run's answer (on a card or its
    // continuation) are what "the turn finished" means, and both are polled
    // until they appear. Only then is the handler's join a formality.
    wait_for_recorded_card(
        &platform,
        "the supplement continuation",
        SETTLE_DEADLINE,
        |card| card_text(card).contains("📨 已收到补充"),
    )
    .await;
    wait_for_recorded_card(&platform, "the steered answer", SETTLE_DEADLINE, |card| {
        card_text(card).contains("the steered answer")
    })
    .await;
    wait_for_handler(first, "the steered turn", SETTLE_DEADLINE).await;

    // The message settled as a Supplement: its own continuation carries the
    // receipt, and the steered run's answer renders on a card.
    let continuation = crate::bridge::test_support::supplement_continuation(&platform, "om_supplement").await;
    assert!(
        card_text(&continuation).contains("📨 已收到补充"),
        "the supplement's continuation carries the receipt"
    );
    let cards: Vec<Value> = platform
        .updated_cards()
        .await
        .into_iter()
        .chain(platform.replied_cards().await)
        .collect();
    assert!(
        cards
            .iter()
            .any(|card| card_text(card).contains("the steered answer")),
        "the steered run's answer renders: {cards:?}"
    );
    // The text itself appears once in the transcript (the card does not repeat
    // the user's own message).
    let transcript = _client.transcript(THREAD).await.expect("the thread's read");
    assert_eq!(
        transcript
            .messages
            .iter()
            .filter(|message| message.text().contains("extra context"))
            .count(),
        1,
        "the steered text lands exactly once"
    );
}

/// Ticket #881 acceptance 2: `/stop` interrupts the running turn and the card
/// settles as stopped — not an error, not a hang.
#[tokio::test]
async fn stop_interrupts_the_turn_and_settles_the_card_as_stopped() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    server.echo_prompts(THREAD);
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) = build_bridge(&url, &directory.path().join("sessions.json"), 0).await;

    let turn = tokio::spawn({
        let app = app.clone();
        async move { app.handle_message(prompt_message("a long run", "om_stop")).await }
    });
    let live = wait_for_live_turn(&server).await;

    send_command_in(
        &app,
        "/stop",
        crate::config::ThreadKey::new(CHAT.into(), CHAT.into()),
        "om_stop_command",
        crate::config::ConversationKind::P2p,
    )
    .await;

    wait_for_card_update(&platform, "the stopped card", CardUpdates::Any, |card| {
        card_header(card).contains("已停止")
    })
    .await;
    wait_for_handler(turn, "the stopped turn", SETTLE_DEADLINE).await;

    let interrupts = server.interrupt_params();
    assert_eq!(interrupts.len(), 1, "one turn/interrupt");
    assert_eq!(
        interrupts[0]["turnId"].as_str(),
        Some(live.as_str()),
        "the interrupt names the running turn"
    );
    let card = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the settled card");
    assert!(
        card_header(&card).contains("已停止"),
        "a stopped turn settles as stopped, not error or spinner: {}",
        card_header(&card)
    );
}

/// Ticket #881 acceptance 5: `/model` lists the server's catalog, and the pick
/// (with a `/think` effort) reaches the next `turn/start`.
#[tokio::test]
async fn a_model_pick_reaches_the_next_turn() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    server.set_models(vec![json!({
        "id": "gpt-5.1-codex",
        "displayName": "GPT-5.1 Codex",
        "hidden": false,
        "supportedReasoningEfforts": [
            { "reasoningEffort": "low", "description": "fast" },
            { "reasoningEffort": "high", "description": "careful" }
        ],
        "defaultReasoningEffort": "high"
    })]);
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, _platform, _client) = build_bridge(&url, &directory.path().join("sessions.json"), 0).await;
    let key = crate::config::ThreadKey::new(CHAT.into(), CHAT.into());

    // `/model` → the provider card → the model card.
    send_command_in(
        &app,
        "/model",
        key.clone(),
        "om_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let step = app
        .host_action(json!({
            "action": "model",
            "level": "provider",
            "chat_id": CHAT,
            "thread_id": CHAT,
            "value": "codex",
        }))
        .await
        .expect("the provider pick");
    let picker = step.card.expect("the model picker");
    assert!(
        card_text(&picker).contains("gpt-5.1-codex"),
        "the picker lists the server's model: {picker}"
    );
    app.host_action(json!({
        "action": "model",
        "level": "model",
        "chat_id": CHAT,
        "thread_id": CHAT,
        "value": "codex/gpt-5.1-codex",
    }))
    .await
    .expect("the model pick");

    // The effort rides the model entry: `/think high` is accepted for it.
    send_command_in(
        &app,
        "/think high",
        key.clone(),
        "om_think",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let handler = tokio::spawn({
        let app = app.clone();
        async move {
            app.handle_message(prompt_message("use the new model", "om_after_pick"))
                .await
        }
    });
    let params = wait_for_turn_start(&server).await;
    assert_eq!(params["model"], "gpt-5.1-codex");
    assert_eq!(params["effort"], "high");
    // The completion push below reaches subscribers only; wait for the resume.
    server.wait_for_subscribers(THREAD, 1).await;

    // End the turn so the spawned turn stops cleanly.
    let live = server.live_turn(THREAD).expect("the turn is running");
    let now = chrono::Utc::now().timestamp();
    server.push_to_thread(
        THREAD,
        "turn/completed",
        json!({
            "threadId": THREAD,
            "turn": { "id": live, "status": "completed", "startedAt": now, "completedAt": now + 1 },
        }),
    );
    // This turn renders no agent message, so no card outcome exists to poll;
    // the handler's completion is the signal. Poll it rather than racing a
    // fixed timeout: the first poll after the work lands sees it.
    wait_for_handler(handler, "the model turn", SETTLE_DEADLINE).await;
}

/// Ticket #881 acceptance 5 (skills half) through the bridge: `/skill` resolves
/// against `skills/list` and the pick reaches `turn/start` as the protocol's
/// `skill` input.
#[tokio::test]
async fn a_skill_pick_reaches_the_next_turn() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    server.echo_prompts(THREAD);
    server.set_skills(vec![json!({
        "name": "caveman",
        "description": "Ultra-compressed communication mode.",
        "path": "/home/u/.codex/skills/caveman/SKILL.md",
        "scope": "user",
        "enabled": true,
        "pluginId": null,
    })]);
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, _platform, _client) = build_bridge(&url, &directory.path().join("sessions.json"), 0).await;

    let handler = tokio::spawn({
        let app = app.clone();
        async move {
            app.handle_message(prompt_message("/skill caveman explain this", "om_skill"))
                .await
        }
    });
    let params = wait_for_turn_start(&server).await;
    assert_eq!(
        params["input"][1],
        json!({
            "type": "skill",
            "name": "caveman",
            "path": "/home/u/.codex/skills/caveman/SKILL.md",
        }),
        "the skill rides the prompt with the path the server advertised"
    );

    let live = server.live_turn(THREAD).expect("the turn is running");
    // The completion push below reaches subscribers only; wait for the resume.
    server.wait_for_subscribers(THREAD, 1).await;
    let now = chrono::Utc::now().timestamp();
    server.push_to_thread(
        THREAD,
        "turn/completed",
        json!({
            "threadId": THREAD,
            "turn": { "id": live, "status": "completed", "startedAt": now, "completedAt": now + 1 },
        }),
    );
    // No agent message renders on the card, so the handler's completion is the
    // only signal: poll it under the shared generous deadline.
    wait_for_handler(handler, "the skill turn", SETTLE_DEADLINE).await;
}

/// Ticket #881 acceptance 4 through the bridge: `/compact` runs, and the
/// compaction it caused renders through the existing path — once — on the next
/// turn's card.
#[tokio::test]
async fn compact_renders_through_the_existing_compaction_path_once() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    server.echo_prompts(THREAD);
    let now = chrono::Utc::now().timestamp();
    server.script_turn(
        THREAD,
        vec![
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": THREAD,
                    "turnId": "turn-2",
                    "completedAtMs": now * 1000,
                    "item": { "type": "agentMessage", "id": "agent-after-compact", "text": "after the compaction" },
                },
            }),
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": THREAD,
                    "turn": { "id": "turn-2", "status": "completed", "startedAt": now, "completedAt": now + 1 },
                },
            }),
        ],
    );
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) = build_bridge(&url, &directory.path().join("sessions.json"), 0).await;

    send_command_in(
        &app,
        "/compact",
        crate::config::ThreadKey::new(CHAT.into(), CHAT.into()),
        "om_compact",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(
        server.compact_params(),
        vec![json!({ "threadId": THREAD })],
        "thread/compact/start names the thread"
    );

    let handler = tokio::spawn({
        let app = app.clone();
        async move {
            app.handle_message(prompt_message("after compact", "om_post_compact"))
                .await
        }
    });
    wait_for_card_update(&platform, "the compaction row", CardUpdates::Any, |card| {
        card_text(card).contains("已压缩上下文（手动）")
    })
    .await;
    wait_for_card_update(&platform, "the compaction answer", CardUpdates::Any, |card| {
        card_text(card).contains("after the compaction")
    })
    .await;
    wait_for_handler(handler, "the compact turn", SETTLE_DEADLINE).await;

    let card = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("the settled card");
    assert_eq!(
        card_text(&card).matches("已压缩上下文（手动）").count(),
        1,
        "the manual compaction renders exactly once: {card}"
    );
    assert!(
        card_text(&card).contains("after the compaction"),
        "and the turn's own answer rides the same card: {card}"
    );
}

/// Wait until the client's transcript carries `needle`, returning that read.
async fn wait_for_transcript_text(client: &CodexClient, needle: &str) -> crate::backend::SessionTranscript {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let transcript = client.transcript(THREAD).await.expect("the transcript read");
        if transcript
            .messages
            .iter()
            .any(|message| message.text().contains(needle))
        {
            return transcript;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for `{needle}`"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Wait until the app's client has folded the thread's live turn, so a steer
/// targets it rather than falling back to `turn/start`.
async fn wait_for_client_live_turn(client: &CodexClient) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if client.projections.live_turn(THREAD).is_some() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the client never saw the TUI's live turn"
        );
        let _ = client.transcript(THREAD).await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The gate's case: the user's TUI starts the turn, and a Feishu Supplement is
/// steered into it. The supplement is cola's, so Session Sync must not announce
/// it (the notification path is also the external-turn boundary), while the
/// TUI's own id-less message in the same turn stays external.
#[tokio::test]
async fn a_steered_supplement_is_never_an_external_message() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    server.echo_prompts(THREAD);
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, client) = build_bridge(
        &url,
        &directory.path().join("sessions.json"),
        chrono::Utc::now().timestamp_millis() - 60_000,
    )
    .await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    // The user's own client starts the turn; its message carries no client id.
    let tui = connect(&url).await;
    tui.transcript(THREAD).await.expect("the TUI's subscription");
    tui.prompt(THREAD, "from the TUI", &[], &[], None, None, None, None)
        .await
        .expect("the TUI's prompt");
    wait_for_client_live_turn(&client).await;

    // The Feishu Supplement steers the running turn.
    client
        .steer(
            THREAD,
            "feishu supplement",
            &[],
            &[],
            None,
            None,
            None,
            Some("msg_cola_supp"),
        )
        .await
        .expect("the supplement steers");
    let transcript = wait_for_transcript_text(&client, "feishu supplement").await;
    let supplement = transcript
        .messages
        .iter()
        .find(|message| message.text() == "feishu supplement")
        .expect("the supplement's user message");
    assert!(
        client.is_cola_authored(supplement.id.as_str()).await,
        "the steered supplement is cola's"
    );
    let tui_message = transcript
        .messages
        .iter()
        .find(|message| message.text() == "from the TUI")
        .expect("the TUI's message");
    assert!(
        !client.is_cola_authored(tui_message.id.as_str()).await,
        "the TUI's id-less message in the same turn is external"
    );

    // Give Session Sync several passes: the supplement must never be announced.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let announced: Vec<String> = platform
        .sent_cards()
        .await
        .iter()
        .filter(|card| card_text(card).contains("feishu supplement"))
        .map(card_text)
        .collect();
    assert!(
        announced.is_empty(),
        "a steered supplement must not be announced as external: {announced:?}"
    );

    sync.abort();
}

/// Ticket #881 acceptance 6 through the bridge: a turn cola started is never an
/// External Message — including after a restart, which reads the durable record
/// rather than an in-memory one. A genuinely external turn still notifies.
#[tokio::test]
async fn a_cola_turn_is_never_an_external_message_after_a_restart() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    server.echo_prompts(THREAD);
    let now = chrono::Utc::now().timestamp();
    server.script_turn(
        THREAD,
        vec![
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": THREAD,
                    "turnId": "turn-1",
                    "completedAtMs": now * 1000,
                    "item": { "type": "agentMessage", "id": "agent-1", "text": "cola's answer" },
                },
            }),
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": THREAD,
                    "turn": { "id": "turn-1", "status": "completed", "startedAt": now, "completedAt": now + 1 },
                },
            }),
        ],
    );
    let directory = tempfile::tempdir().expect("tempdir");
    let session_file = directory.path().join("sessions.json");
    let record_path = directory.path().join("authored_turns.json");

    // The first cola process prompts; its turn is recorded durably.
    let (first, first_platform, first_client) = build_bridge_authored(
        &url,
        &session_file,
        now * 1000 - 60_000,
        super::authorship::Authorship::open(record_path.clone(), crate::config::DEFAULT_CLIENT_ID)
            .expect("the record opens"),
    )
    .await;
    let handler = tokio::spawn({
        let first = first.clone();
        async move {
            first
                .handle_message(prompt_message("cola's own turn", "om_own"))
                .await
        }
    });
    wait_for_all_unsubscribed(&server).await;
    wait_for_handler(handler, "the first turn", SETTLE_DEADLINE).await;
    drop(first);
    drop(first_client);
    drop(first_platform);
    wait_for_all_unsubscribed(&server).await;
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).expect("the durable record"))
            .expect("json");
    assert_eq!(
        record["turns"][crate::config::DEFAULT_CLIENT_ID][THREAD],
        json!(["turn-1"]),
        "the turn cola started is on disk: {record}"
    );

    // The restart: a new process reads the record and starts Session Sync.
    let (second, second_platform, second_client) = build_bridge_authored(
        &url,
        &session_file,
        now * 1000 - 60_000,
        super::authorship::Authorship::open(record_path, crate::config::DEFAULT_CLIENT_ID)
            .expect("the record reloads"),
    )
    .await;
    let sync = spawn_sync(&second);
    server.wait_for_subscribers(THREAD, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(
        !second_platform
            .sent_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains("有新消息")),
        "a restart must not report cola's own turn as an external message"
    );

    // The non-vacuous half: a turn the user's own client starts does notify.
    let tui = connect(&url).await;
    tui.transcript(THREAD).await.expect("the TUI's subscription");
    tui.prompt(THREAD, "from OpenChamber", &[], &[], None, None, None, None)
        .await
        .expect("the TUI's prompt");
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    loop {
        if second_platform
            .sent_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains("有新消息") && card_text(card).contains("from OpenChamber"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a genuinely external turn must still notify after the restart"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    sync.abort();
    drop(second_client);
}

// ---------------------------------------------------------------------------
// Ticket #882: approvals and questions in both directions
// ---------------------------------------------------------------------------

/// Start the permission poller on its own task; the sweep is the path that
/// turns a pending server request into a Feishu card.
fn spawn_permission_poll(app: &Arc<App>) -> tokio::task::JoinHandle<()> {
    app.permission.poll_interval_ms.store(20, Ordering::Relaxed);
    app.permission.list_timeout_ms.store(500, Ordering::Relaxed);
    let app = app.clone();
    tokio::spawn(async move {
        let _ = app.permission.poll_loop(&app.flow_handles()).await;
    })
}

fn spawn_question_poll(app: &Arc<App>) -> tokio::task::JoinHandle<()> {
    app.question.poll_interval_ms.store(20, Ordering::Relaxed);
    app.question.list_timeout_ms.store(500, Ordering::Relaxed);
    let app = app.clone();
    tokio::spawn(async move {
        let _ = app.question.poll_loop(&app.flow_handles()).await;
    })
}

/// Wait until the app sent a card matching `check`; a standalone permission or
/// question card is a `SendCard`, not an in-place update.
async fn wait_for_sent_card(
    platform: &RecordingPlatform,
    label: &str,
    check: impl Fn(&Value) -> bool,
) -> Value {
    let wait = async {
        loop {
            if let Some(card) = platform.sent_cards().await.into_iter().find(|card| check(card)) {
                return card;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(SETTLE_DEADLINE, wait)
        .await
        .unwrap_or_else(|_| panic!("the card never reached {label}"))
}

/// Every sent card paired with the chat it went to.
async fn sent_cards_with_targets(platform: &RecordingPlatform) -> Vec<(String, Value)> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::SendCard { receive_id, card } => Some((receive_id.clone(), card.clone())),
            _ => None,
        })
        .collect()
}

/// Whether a card JSON tree carries an element with this `tag` (a button, an
/// input, a form, …), wherever it nests.
fn contains_element_tag(value: &Value, tag: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.get("tag").and_then(Value::as_str) == Some(tag)
                || map.values().any(|value| contains_element_tag(value, tag))
        }
        Value::Array(items) => items.iter().any(|value| contains_element_tag(value, tag)),
        _ => false,
    }
}

fn button_value(card: &Value, reply: &str) -> Option<Value> {
    card_buttons(card)
        .into_iter()
        .find(|button| button["value"]["reply"] == reply)
        .map(|button| button["value"].clone())
}

/// Ticket #882 acceptance 1: a command approval raised by the user's own
/// (TUI) turn reaches the Feishu card, and the Feishu answer is the JSON-RPC
/// response that lets the turn continue.
#[tokio::test]
async fn a_tui_command_approval_is_answered_from_the_feishu_card() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));

    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    // The user's own client: a second connection subscribed to the thread.
    let tui = connect(&url).await;
    tui.transcript(THREAD).await.expect("the TUI's subscription");
    server.wait_for_subscribers(THREAD, 2).await;

    // The turn was started on the TUI; the server raises the approval to
    // every subscriber (the spike's cross-connection shape).
    let request_id = server.request_to_subscribers(
        THREAD,
        "item/commandExecution/requestApproval",
        json!({
            "threadId": THREAD,
            "turnId": "turn-tui",
            "itemId": "item-tui",
            "command": "rm -rf build",
            "cwd": WORK,
            "reason": "The build directory is stale",
            "startedAtMs": 1_700_000_000_000_i64,
        }),
    );
    let poll = spawn_permission_poll(&app);

    let card = wait_for_sent_card(&platform, "the Feishu permission card", |card| {
        card_text(card).contains("rm -rf build")
    })
    .await;
    let text = card_text(&card);
    assert!(
        text.contains("权限请求"),
        "the approval is a permission card: {card}"
    );
    assert!(
        text.contains("The build directory is stale"),
        "the reason renders: {card}"
    );
    // The session-scoped button is offered, and the card says what it means.
    assert!(
        text.contains("始终允许"),
        "the advertised decision is offered: {card}"
    );
    assert!(
        text.contains(SESSION_SCOPE_NOTE),
        "the card explains the session scope: {card}"
    );

    let once = button_value(&card, "once").expect("the allow-once button");
    // The neutral id is the wire id plus this connection's namespace tag, so a
    // request from another connection can never alias it (issue #896).
    let neutral = once["request_id"].as_str().expect("the card names a request");
    assert!(
        neutral.starts_with(&format!("n:{}@", request_id.as_u64().unwrap())),
        "the card carries this connection's request id, got {neutral}"
    );

    let result = app.host_action(once).await.expect("the answer card");
    assert!(
        result
            .card
            .as_ref()
            .map(card_text)
            .unwrap_or_default()
            .contains("已允许一次"),
        "the click reports its decision"
    );

    server.wait_for_server_request_replies(1).await;
    let replies = server.server_request_replies();
    assert_eq!(replies.len(), 1, "one answer, one response frame");
    assert_eq!(replies[0].connection, 1, "Feishu answered on cola's connection");
    assert_eq!(replies[0].result, Some(json!({ "decision": "accept" })));
    assert_eq!(replies[0].error, None);

    // The server's resolution reaches the TUI too, so the turn it started has
    // no pending approval left — the answer unblocked it.
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    while !tui
        .list_permissions(Some(WORK))
        .await
        .expect("the TUI's pending list")
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the TUI's approval was not resolved by the Feishu answer"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    poll.abort();
    sync.abort();
}

/// Whole-branch review #883: server requests are **lossless**. A request
/// followed by a burst larger than the notification broadcast's capacity still
/// becomes a card and is still answerable — the approval record no longer reads
/// the lossy fan-out, where a lagged consumer loses the oldest frames (a server
/// then waits forever with no card).
#[tokio::test]
async fn an_approval_survives_a_notification_burst_larger_than_the_broadcast() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));

    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    // The request arrives first; the burst then outruns the broadcast's
    // capacity before the poller has read anything.
    server.request_to_subscribers(
        THREAD,
        "item/commandExecution/requestApproval",
        json!({
            "threadId": THREAD,
            "turnId": "turn-burst",
            "itemId": "item-burst",
            "command": "cargo test --workspace --locked",
            "cwd": WORK,
        }),
    );
    // More frames than the connection's 1024-entry broadcast holds, so a
    // consumer on that path is lagged and its oldest entries are gone.
    for _ in 0..1100 {
        server.push_notification("remoteControl/status/changed", json!({ "status": "disabled" }));
    }

    let poll = spawn_permission_poll(&app);
    let card = wait_for_sent_card(&platform, "the Feishu permission card", |card| {
        card_text(card).contains("cargo test --workspace --locked")
    })
    .await;
    let once = button_value(&card, "once").expect("the allow-once button");
    app.host_action(once).await.expect("the answer card");

    server.wait_for_server_request_replies(1).await;
    assert_eq!(
        server.server_request_replies()[0].result,
        Some(json!({ "decision": "accept" })),
        "the request the burst could have dropped is answered"
    );

    poll.abort();
    sync.abort();
}

/// Ticket #882 acceptance 2 + 3: a cola-started turn's approval is answerable
/// from the user's own client, and the Feishu card is withdrawn on the
/// server's `serverRequest/resolved` instead of staying clickable.
#[tokio::test]
async fn an_approval_for_a_cola_turn_is_answered_by_the_other_client_and_withdrawn() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));

    let directory = tempfile::tempdir().expect("tempdir");
    let (app, _platform, client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    let tui = connect(&url).await;
    tui.transcript(THREAD).await.expect("the TUI's subscription");
    server.wait_for_subscribers(THREAD, 2).await;

    // Cola starts the turn from Feishu.
    let handler = tokio::spawn({
        let app = app.clone();
        async move {
            app.handle_message(prompt_message("fix the build", "om_task"))
                .await
        }
    });
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    while server.turn_starts() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cola turn never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    server.request_to_subscribers(
        THREAD,
        "item/commandExecution/requestApproval",
        json!({
            "threadId": THREAD,
            "turnId": "turn-cola",
            "itemId": "item-cola",
            "command": "make test",
            "cwd": WORK,
        }),
    );
    let poll = spawn_permission_poll(&app);

    // Both faces see the same pending approval: cola's poll-shaped read and
    // the TUI's own request record.
    let request_id = {
        let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
        loop {
            if let Some(pending) = tui
                .list_permissions(Some(WORK))
                .await
                .expect("the TUI's pending list")
                .first()
                .cloned()
            {
                break pending.request_id;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the TUI never received the approval"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    while client
        .list_permissions(Some(WORK))
        .await
        .expect("cola's pending list")
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "cola never received the approval the TUI saw"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The live cola turn carries the inline block.
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    while Turn::live_permissions(&app.cards_handle(), THREAD)
        .await
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the permission never became an inline block"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The user's client answers first.
    tui.reply_permission(THREAD, &request_id, "once", Some(WORK))
        .await
        .expect("the TUI's answer");
    server.wait_for_server_request_replies(1).await;
    let replies = server.server_request_replies();
    assert_eq!(replies[0].connection, 2, "the TUI answered, not cola");
    assert_eq!(replies[0].result, Some(json!({ "decision": "accept" })));

    // `serverRequest/resolved` withdraws cola's copy.
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    while !client
        .list_permissions(Some(WORK))
        .await
        .expect("cola's pending list")
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "resolved approval stayed pending on cola"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    let receipt = async {
        loop {
            let Some(card) = Turn::rendered_card(&app.cards_handle(), THREAD).await else {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                continue;
            };
            if card_text(&card).contains("已由其他客户端处理") {
                break card;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    let card = tokio::time::timeout(SETTLE_DEADLINE, receipt)
        .await
        .expect("the withdrawn block leaves a receipt");
    assert!(
        !card_text(&card).contains("始终允许"),
        "the withdrawn card must not stay clickable: {card}"
    );
    assert!(
        !card_text(&card).contains("权限请求"),
        "the live block is gone: {card}"
    );

    poll.abort();
    sync.abort();
    handler.abort();
}

/// Ticket #882 acceptance 4: a permission-grant request does not advertise a
/// session decision, so the card offers neither the button nor its scope copy;
/// the grant echoes exactly the requested profile and a denial grants nothing.
#[tokio::test]
async fn a_permission_grant_offers_only_what_cola_can_express() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));

    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    server.request_to_subscribers(
        THREAD,
        "item/permissions/requestApproval",
        json!({
            "threadId": THREAD,
            "turnId": "turn-1",
            "itemId": "item-1",
            "cwd": WORK,
            "reason": "needs to write the build directory",
            "permissions": {
                "fileSystem": { "write": [WORK] },
                "network": { "enabled": false }
            },
            "startedAtMs": 1_700_000_000_000_i64,
        }),
    );
    let poll = spawn_permission_poll(&app);

    let card = wait_for_sent_card(&platform, "the permission-grant card", |card| {
        card_text(card).contains("申请额外权限")
    })
    .await;
    let text = card_text(&card);
    assert!(text.contains(WORK), "the requested path renders: {card}");
    assert!(
        !text.contains("始终允许"),
        "the session decision was not advertised, so it is not offered: {card}"
    );
    assert!(
        !text.contains(SESSION_SCOPE_NOTE),
        "no scope copy for a decision the card does not offer: {card}"
    );

    let once = button_value(&card, "once").expect("the allow-once button");
    app.host_action(once).await.expect("the answer card");
    server.wait_for_server_request_replies(1).await;
    assert_eq!(
        server.server_request_replies()[0].result,
        Some(json!({
            "permissions": {
                "fileSystem": { "write": [WORK] },
                "network": { "enabled": false }
            },
            "scope": "turn"
        })),
        "a grant echoes exactly the requested profile, never a wider one"
    );

    poll.abort();
    sync.abort();
}

/// Ticket #882 acceptance 5: a child-session (subagent) approval resolves its
/// card target through the parent chain (pitfall `child-session-permissions`).
#[tokio::test]
async fn a_child_session_approval_reaches_the_parents_chat() {
    const CHILD: &str = "thread-child";
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "the parent thread",
    ));
    let mut child = ScriptedThread::adopted(CHILD, "session-child", WORK, "a subagent");
    child.parent_thread_id = Some(THREAD.to_string());
    server.seed_thread(child);

    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    // The server delivers a subagent's approval to the parent turn's
    // subscribers (the child has no subscription of its own in cola); the
    // request itself still names the child thread.
    server.request_to_subscribers(
        THREAD,
        "item/commandExecution/requestApproval",
        json!({
            "threadId": CHILD,
            "turnId": "turn-child",
            "itemId": "item-child",
            "command": "cargo test",
            "cwd": WORK,
        }),
    );
    let poll = spawn_permission_poll(&app);

    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    loop {
        if let Some((receive_id, card)) = sent_cards_with_targets(&platform)
            .await
            .into_iter()
            .find(|(_, card)| card_text(card).contains("cargo test"))
        {
            assert_eq!(
                receive_id, CHAT,
                "the child's card is delivered through the parent's chat mapping"
            );
            let button = button_value(&card, "once").expect("the child's button");
            assert_eq!(
                button["session_id"], CHILD,
                "the answer routes to the requesting child session"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child approval never reached the parent's chat"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    poll.abort();
    sync.abort();
}

/// Ticket #882 acceptance 6: two fast clicks produce one JSON-RPC response.
#[tokio::test]
async fn two_fast_clicks_send_one_decision() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    server.request_to_subscribers(
        THREAD,
        "item/commandExecution/requestApproval",
        json!({
            "threadId": THREAD,
            "turnId": "turn-1",
            "itemId": "item-1",
            "command": "ls",
            "cwd": WORK,
        }),
    );
    let poll = spawn_permission_poll(&app);
    let card = wait_for_sent_card(&platform, "the permission card", |card| {
        card_text(card).contains("权限请求")
    })
    .await;
    let once = button_value(&card, "once").expect("the allow-once button");

    // Hold the claim guard so both clicks queue on it, then release: the
    // check-and-claim pair must still let exactly one click reply.
    let guard = app.answered_requests.lock().await;
    let a = {
        let app = app.clone();
        let value = once.clone();
        tokio::spawn(async move { app.host_action(value).await })
    };
    let b = {
        let app = app.clone();
        tokio::spawn(async move { app.host_action(once).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    drop(guard);
    let (a, b) = tokio::join!(a, b);
    assert!(a.is_ok() && b.is_ok(), "both clicks are handled");

    server.wait_for_server_request_replies(1).await;
    // Give a hypothetical second response a chance to arrive before counting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        server.server_request_replies().len(),
        1,
        "two clicks must send one decision: {:?}",
        server.server_request_replies()
    );

    poll.abort();
    sync.abort();
}

/// Ticket #882: a tool's user-input request is a Question card, and the
/// Feishu answer maps onto the protocol's keyed `answers` payload.
#[tokio::test]
async fn a_user_input_question_is_answered_from_the_feishu_card() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    server.request_to_subscribers(
        THREAD,
        "item/tool/requestUserInput",
        json!({
            "threadId": THREAD,
            "turnId": "turn-1",
            "itemId": "item-1",
            "isBlocking": true,
            "questions": [{
                "id": "q1",
                "header": "确认",
                "question": "继续执行吗？",
                "options": [
                    { "label": "是", "description": "继续" },
                    { "label": "否", "description": "停止" }
                ]
            }]
        }),
    );
    let poll = spawn_question_poll(&app);
    let card = wait_for_sent_card(&platform, "the question card", |card| {
        card_text(card).contains("继续执行吗")
    })
    .await;
    let answer = card_buttons(&card)
        .into_iter()
        .find(|button| button["value"]["reply"] == "answer" && button["value"]["answer"] == "是")
        .map(|button| button["value"].clone())
        .expect("the 是 option button");

    app.host_action(answer).await.expect("the answer card");
    server.wait_for_server_request_replies(1).await;
    assert_eq!(
        server.server_request_replies()[0].result,
        Some(json!({ "answers": { "q1": { "answers": ["是"] } } }))
    );

    poll.abort();
    sync.abort();
}

/// Ticket #882: an MCP form elicitation is a typed Question card, and its
/// answer is the protocol's `content` object.
#[tokio::test]
async fn a_form_elicitation_is_answered_with_typed_content() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    server.request_to_subscribers(
        THREAD,
        "mcpServer/elicitation/request",
        json!({
            "serverName": "files",
            "threadId": THREAD,
            "message": "把文件写入工作区？",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "confirm": { "type": "boolean", "title": "确认写入" }
                },
                "required": ["confirm"]
            }
        }),
    );
    let poll = spawn_question_poll(&app);
    let card = wait_for_sent_card(&platform, "the elicitation card", |card| {
        card_text(card).contains("确认写入")
    })
    .await;
    assert!(
        card_text(&card).contains("把文件写入工作区？"),
        "the elicitation's message explains the request even with sparse fields: {card}"
    );
    let yes = card_buttons(&card)
        .into_iter()
        .find(|button| button["value"]["reply"] == "answer" && button["value"]["answer"] == "true")
        .map(|button| button["value"].clone())
        .expect("the 是 button");

    app.host_action(yes).await.expect("the answer card");
    server.wait_for_server_request_replies(1).await;
    assert_eq!(
        server.server_request_replies()[0].result,
        Some(json!({ "action": "accept", "content": { "confirm": true } }))
    );

    poll.abort();
    sync.abort();
}

/// Ticket #882 residuals: an elicitation's message and a secret refusal are
/// rendered on the **inline** block too, not only on a standalone card.
#[tokio::test]
async fn question_titles_and_refusals_render_inline() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, _platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    // A live cola turn owns the streaming card the question blocks join.
    let handler = tokio::spawn({
        let app = app.clone();
        async move {
            app.handle_message(prompt_message("keep this turn open", "om_task"))
                .await
        }
    });
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    while server.turn_starts() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cola turn never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let poll = spawn_question_poll(&app);

    server.request_to_subscribers(
        THREAD,
        "mcpServer/elicitation/request",
        json!({
            "serverName": "files",
            "threadId": THREAD,
            "message": "把文件写入工作区？",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": { "confirm": { "type": "boolean", "title": "确认写入" } },
                "required": ["confirm"]
            }
        }),
    );
    let message = "把文件写入工作区？";
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    loop {
        if let Some(card) = Turn::rendered_card(&app.cards_handle(), THREAD).await
            && card_text(&card).contains(message)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "an elicitation's message never reached the inline card"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    server.request_to_subscribers(
        THREAD,
        "item/tool/requestUserInput",
        json!({
            "threadId": THREAD,
            "turnId": "turn-1",
            "itemId": "item-1",
            "isBlocking": true,
            "questions": [{
                "id": "password",
                "header": "登录",
                "question": "请输入密码",
                "isSecret": true
            }]
        }),
    );
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    loop {
        if let Some(card) = Turn::rendered_card(&app.cards_handle(), THREAD).await
            && card_text(&card).contains("私密输入")
        {
            assert!(
                !contains_element_tag(&card, "input"),
                "the inline refusal has no text input: {card}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the secret refusal never reached the inline card"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    poll.abort();
    sync.abort();
    handler.abort();
}

/// Ticket #882: a URL-mode elicitation has no card cola can render, so it is
/// declined explicitly (never silently ignored, never approximated as a form).
#[tokio::test]
async fn a_url_elicitation_is_declined_explicitly() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    server.request_to_subscribers(
        THREAD,
        "mcpServer/elicitation/request",
        json!({
            "serverName": "files",
            "threadId": THREAD,
            "message": "打开授权页面",
            "mode": "url",
            "url": "https://example.test/consent",
            "elicitationId": "e-1"
        }),
    );
    let poll = spawn_question_poll(&app);
    server.wait_for_server_request_replies(1).await;
    assert_eq!(
        server.server_request_replies()[0].result,
        Some(json!({ "action": "decline" }))
    );
    assert!(
        platform.sent_cards().await.is_empty(),
        "a URL elicitation is refused, not shown as a half-form"
    );

    poll.abort();
    sync.abort();
}

/// Ticket #882: a server request cola does not implement is refused by name,
/// so the server never waits on a client that will not answer.
#[tokio::test]
async fn an_unanswered_request_kind_is_refused_by_name() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    let directory = tempfile::tempdir().expect("tempdir");
    let (_app, _platform, _client) = build_bridge(&url, &directory.path().join("sessions.json"), 0).await;

    server.request_to_all("account/chatgptAuthTokens/refresh", json!({}));
    server.wait_for_server_request_replies(1).await;
    let reply = &server.server_request_replies()[0];
    assert_eq!(
        reply
            .error
            .as_ref()
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64),
        Some(-32601),
        "the refusal names the unsupported method: {reply:?}"
    );
    assert!(
        reply
            .error
            .as_ref()
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("account/chatgptAuthTokens/refresh"),
        "{reply:?}"
    );
}

/// A secret question becomes a named refusal card — never a free-text field.
/// Its only control is the ordinary reject button, and that click is the
/// protocol's decline to the server.
#[tokio::test]
async fn a_secret_question_is_refused_at_the_card_not_collected() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    server.request_to_subscribers(
        THREAD,
        "item/tool/requestUserInput",
        json!({
            "threadId": THREAD,
            "turnId": "turn-1",
            "itemId": "item-1",
            "isBlocking": true,
            "questions": [{
                "id": "password",
                "header": "登录",
                "question": "请输入密码",
                "isSecret": true
            }]
        }),
    );
    let poll = spawn_question_poll(&app);
    let card = wait_for_sent_card(&platform, "the secret-refusal card", |card| {
        card_text(card).contains("私密输入")
    })
    .await;
    assert!(
        !card_text(&card).contains("请输入密码"),
        "the secret field is never rendered: {card}"
    );
    assert!(
        card_text(&card).contains("重新发起"),
        "the copy says to re-initiate on the original client: {card}"
    );
    assert!(
        !contains_element_tag(&card, "input") && !contains_element_tag(&card, "form"),
        "the refusal card has no text input: {card}"
    );

    let reject = button_value(&card, "reject").expect("the refusal's reject control");
    app.host_action(reject).await.expect("the refusal card");
    server.wait_for_server_request_replies(1).await;
    assert_eq!(
        server.server_request_replies()[0].result,
        Some(json!({ "answers": {} })),
        "rejecting the refusal form declines the request"
    );
    assert_eq!(server.server_request_replies()[0].error, None);

    poll.abort();
    sync.abort();
}

/// A permission profile with a field the card cannot render is refused: the
/// reply grants nothing rather than echoing a hidden grant.
#[tokio::test]
async fn a_profile_with_a_hidden_grant_is_declined() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));
    let directory = tempfile::tempdir().expect("tempdir");
    let (app, platform, _client) =
        build_bridge(&url, &directory.path().join("sessions.json"), 1_600_000_000_000).await;
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    server.request_to_subscribers(
        THREAD,
        "item/permissions/requestApproval",
        json!({
            "threadId": THREAD,
            "turnId": "turn-1",
            "itemId": "item-1",
            "cwd": WORK,
            "permissions": {
                "fileSystem": {
                    "write": [WORK],
                    "globScanMaxDepth": 8
                }
            }
        }),
    );
    let poll = spawn_permission_poll(&app);
    server.wait_for_server_request_replies(1).await;
    assert_eq!(
        server.server_request_replies()[0].result,
        Some(json!({ "permissions": {} })),
        "a profile the card cannot show is never granted"
    );
    // A path longer than the card's pattern budget is the same class: the
    // suffix would be clipped, so it is declined, not partially shown.
    server.request_to_subscribers(
        THREAD,
        "item/permissions/requestApproval",
        json!({
            "threadId": THREAD,
            "turnId": "turn-2",
            "itemId": "item-2",
            "cwd": WORK,
            "permissions": {
                "fileSystem": { "write": ["a".repeat(145)] }
            }
        }),
    );
    server.wait_for_server_request_replies(2).await;
    assert_eq!(
        server.server_request_replies()[1].result,
        Some(json!({ "permissions": {} })),
        "an overlong path is never granted"
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !platform
            .sent_cards()
            .await
            .iter()
            .any(|card| card_text(card).contains("申请额外权限")),
        "no approval card for a grant the user cannot see in full"
    );

    poll.abort();
    sync.abort();
}

/// A silent `model/list` must not wedge a model pick: the full-catalog
/// validation is finite, and a timeout is **unknown**, so the stored `/think`
/// variant is preserved rather than cleared as if the model declared none.
#[tokio::test]
async fn a_silent_catalog_does_not_wedge_a_model_pick() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(THREAD, "session-root", WORK, "a thread"));
    server.silence_next_model_list();
    let dir = tempfile::tempdir().unwrap();
    let (app, _platform, _client) = build_bridge(&url, &dir.path().join("sessions.json"), 0).await;
    let handles = app.command_handles();
    let key = crate::config::ThreadKey::new(CHAT.into(), CHAT.into());
    let mut settings = handles
        .flow
        .sessions
        .session_settings(&key)
        .await
        .expect("the seeded settings");
    settings.variant = Some("high".into());
    assert!(
        handles
            .flow
            .sessions
            .set_session_settings(&key, settings)
            .await
            .expect("persist the variant")
    );

    let started = std::time::Instant::now();
    let outcome = handles
        .flow
        .sessions
        .pick_model(&handles.flow.backend, &key, "codex/new-model")
        .await;
    let elapsed = started.elapsed();

    assert!(
        matches!(
            outcome,
            Ok(crate::bridge::handles::PickOutcome::Applied {
                cleared_variant: None
            })
        ),
        "the pick proceeded with unknown variants: {outcome:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "a silent catalog held the pick for {elapsed:?}"
    );
    let stored = handles
        .flow
        .sessions
        .session_settings(&key)
        .await
        .expect("the stored settings");
    assert_eq!(stored.model.as_deref(), Some("codex/new-model"));
    assert_eq!(
        stored.variant.as_deref(),
        Some("high"),
        "a timeout is unknown, not 'no variants'"
    );
}
