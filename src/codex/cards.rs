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
    CardUpdates, PlatformCall, RecordingPlatform, card_header, card_text, seed_entry, test_config,
    wait_for_card_update,
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
    let client = connect(url).await;
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
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
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
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
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

    // The other face saw the same stream: the TUI's read is not a different
    // transcript. The subscriber runs on its own task, so await its read.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
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
