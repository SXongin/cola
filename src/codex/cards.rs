//! Hermetic card rendering through the real bridge (spec #860, ticket #880).
//!
//! The scripted app-server stands in for the runtime; the real [`CodexClient`],
//! [`App`] and `RecordingPlatform` drive the card, so these tests assert the
//! Feishu payload the reader sees rather than a hand-built transcript. A second
//! `CodexClient` is the user's own client: the same turn stream must reach both
//! connections, and only one of them renders.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::bridge::App;
use crate::bridge::test_support::{
    CardUpdates, RecordingPlatform, card_header, card_text, seed_entry, test_config, wait_for_card_update,
};
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

async fn seed_thread_entry(app: &Arc<App>) {
    seed_entry(
        app,
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
}

/// Shorten Session Sync's cadences and return the App + the platform that
/// records every card write.
fn build_app(cfg: crate::config::Config, client: CodexClient) -> (Arc<App>, Arc<RecordingPlatform>) {
    let platform = Arc::new(RecordingPlatform::new());
    let backend: Arc<dyn crate::backend::Backend> = Arc::new(client);
    let app = Arc::new(App::new(cfg, backend, platform.clone()).expect("the bridge app"));
    app.external.poll_interval_ms.store(20, Ordering::Relaxed);
    app.external.render_poll_ms.store(5, Ordering::Relaxed);
    app.external.request_timeout_ms.store(500, Ordering::Relaxed);
    app.external
        .render_idle_timeout_ms
        .store(10_000, Ordering::Relaxed);
    app.completion_notice_delay_ms.store(0, Ordering::Relaxed);
    (app, platform)
}

/// Start Session Sync on its own task, returning the handle so a restart test
/// can stop the prior process's loop.
fn spawn_sync(app: &Arc<App>) -> tokio::task::JoinHandle<()> {
    let app = app.clone();
    tokio::spawn(async move {
        let _ = app.external.poll_loop(&app.flow_handles()).await;
    })
}

fn recording() -> Vec<Value> {
    serde_json::from_str(include_str!("fixtures/turn-recording.json")).expect("the recording is JSON")
}

/// Ticket #880 acceptance 1: a turn started through the user's own connection
/// reaches both subscribers, and the Feishu card renders it from the first
/// item to the settled end. The turn is "started" by a second, independent
/// connection; the card is the notification Session Sync sends for the
/// external message.
#[tokio::test]
async fn a_turn_from_another_connection_renders_on_the_feishu_card() {
    let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
    server.seed_thread(ScriptedThread::adopted(
        THREAD,
        "session-root",
        WORK,
        "a real thread",
    ));

    let cola = connect(&url).await;
    let directory = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(&directory.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, cola);
    seed_thread_entry(&app).await;
    // The recorded turn predates the watermark, so the fixture's user message
    // is the "new" external message this pass notifies for.
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert(THREAD.into(), 1_600_000_000_000);
    let sync = spawn_sync(&app);
    server.wait_for_subscribers(THREAD, 1).await;

    // The user's own client, a second connection subscribed to the same thread.
    let tui = connect(&url).await;
    tui.transcript(THREAD).await.expect("the TUI's subscription");
    server.wait_for_subscribers(THREAD, 2).await;

    // The spike's finding: the full turn/item stream reaches every subscriber.
    for event in recording() {
        let method = event["method"].as_str().expect("recorded method");
        assert_eq!(
            server.push_to_thread(THREAD, method, event["params"].clone()),
            2,
            "both the TUI and cola receive `{method}`"
        );
    }

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

    let cola = connect(&url).await;
    let directory = tempfile::tempdir().expect("tempdir");
    let cfg = test_config(&directory.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, cola);
    seed_thread_entry(&app).await;
    let now = chrono::Utc::now().timestamp_millis() / 1_000 * 1_000;
    app.external
        .last_user_msg_epoch
        .lock()
        .await
        .insert(THREAD.into(), now - 60_000);
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

/// Ticket #880 acceptance 3: a Codex `contextCompaction` item renders once on
/// the card that owns it, and a cola restart — a fresh process whose adapter
/// rebuilds the projection from persisted history — does not replay it. The
/// "once" comes from the durable Rendered Cursor, not from the projection.
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
    let first_client = connect(&url).await;
    let cfg = test_config(&session_file);
    let (first, first_platform) = build_app(cfg, first_client);
    seed_thread_entry(&first).await;
    first
        .external
        .last_user_msg_epoch
        .lock()
        .await
        .insert(THREAD.into(), turn_one - 60_000);
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

    first_sync.abort();
    drop(first);
    drop(first_platform);
    server.wait_for_subscribers(THREAD, 0).await;

    // The restart: the adapter rebuilds the same turn from persisted history.
    thread.turns = vec![persisted_turn("turn-1", "item-user-1", turn_one, "first")];
    server.set_turns(THREAD, thread.turns.clone());

    let second_client = connect(&url).await;
    let second_read = second_client.clone();
    let cfg = test_config(&session_file);
    let (second, second_platform) = build_app(cfg, second_client);
    seed_thread_entry(&second).await;
    // The new process starts with the first turn already accounted for; only
    // the second turn is new.
    second
        .external
        .last_user_msg_epoch
        .lock()
        .await
        .insert(THREAD.into(), turn_one + 1_000);
    let second_sync = spawn_sync(&second);
    server.wait_for_subscribers(THREAD, 1).await;

    // The restarted adapter rebuilds turn 1 — compaction included — from the
    // paged history; only the cursor keeps it off the new card.
    let rebuilt = second_read.transcript(THREAD).await.expect("the rebuilt read");
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

    let turn_two = now;
    push_turn_with_compaction(&server, "turn-2", "item-user-2", turn_two, "second", false);
    wait_for_card_update(&second_platform, "the second answer", CardUpdates::Any, |card| {
        card_text(card).contains("second answer")
    })
    .await;
    wait_for_card_update(
        &second_platform,
        "the second settled turn",
        CardUpdates::Latest,
        |card| card_header(card).contains("完成"),
    )
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
