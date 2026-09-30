//! The durable Live Card reap (ADR-0063, #438): a fresh app over the sidecar
//! a previous cola life left behind reconciles each persisted record against
//! the Session's own reads — the transcript's real ending settles the card in
//! place (✅ / ❌ / ⏳ 等待后台任务), a never-promoted message ends Unreceived
//! (never ✅), a Wake continuation collects the old card as taken over, a
//! still-live Session keeps the record, and every terminal drops it.
//!
//! Every test drives the real Session Sync pass (`spawn_sync`) over a scripted
//! Backend and a recording Platform, so the assertions read the cards sent and
//! patched and the sidecar file — never private internals.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::drain::{assistant, ctx, script_transcript, scripted_app, spawn_sync, user};
use crate::backend::SessionTranscript;
use crate::bridge::live_cards::{LiveCard, LiveCards};
use crate::bridge::test_support::*;
use crate::bridge::turn::Turn;
use crate::feishu::card::CardState;
use crate::opencode::types::SessionStatus;

/// The sidecar path the app's own rule derives from the session file.
fn sidecar(session_file: &Path) -> PathBuf {
    session_file.with_file_name("live_cards.json")
}

/// Seed the record a previous cola life left behind — the card that was live
/// when the process died — directly into the sidecar, so the next app loads it
/// at construction exactly like a real restart.
fn seed_record(session_file: &Path, card_message_id: &str, message_id: &str, created_ms: Option<i64>) {
    LiveCards::load(sidecar(session_file))
        .replace("ses_test", LiveCard::new(card_message_id, message_id, created_ms));
}

/// The restarted process: a fresh app over `session_file`, with the transcript
/// and status the server would answer now. No card chain is in memory — that
/// is the restart.
async fn restarted_app(
    session_file: &Path,
    transcript: SessionTranscript,
    status: Option<SessionStatus>,
) -> (Arc<App>, Arc<RecordingPlatform>) {
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript("ses_test", vec![transcript]);
    if let Some(status) = status {
        backend.with_session_status("ses_test", Some(status));
    }
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(session_file), Arc::new(backend), platform.clone())
            .expect("the restarted app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    (app, platform)
}

/// The card the platform PATCHed onto `message_id` last, if any.
async fn last_update_of(platform: &RecordingPlatform, message_id: &str) -> Option<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|call| match call {
            PlatformCall::UpdateMessage {
                message_id: mid,
                card,
            } if mid == message_id => Some(card.clone()),
            _ => None,
        })
}

/// A restart reaps a persisted card by the transcript's real ending: the run
/// finished while cola was down, so the card takes ✅ — never an invented
/// interruption — and the record is spent.
#[tokio::test]
async fn a_restart_reaps_a_persisted_card_to_done() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, "回答"),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform) = restarted_app(&session_file, transcript, None).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the reaped card's ✅ ending",
        CardUpdates::Any,
        |card| card_header(card).contains("✅"),
    )
    .await;

    let card = last_update_of(&platform, "om_frozen")
        .await
        .expect("the persisted card is settled in place");
    assert_eq!(card_header(&card), "✅ 完成");
    assert!(
        app.cards_handle().live_cards.get("ses_test").is_none(),
        "a terminal removes the record"
    );
    assert!(
        !sidecar(&session_file).exists(),
        "the emptied record removes the sidecar"
    );
}

/// A run that failed while cola was down gets its real Error ending — the
/// transcript's own failure text, never an invented interruption.
#[tokio::test]
async fn a_restart_reaps_a_persisted_card_to_error() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let mut failed = assistant(2_000, "没成功。");
    failed.error = Some("503 request queue full".into());
    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题"), failed])
        .with_executions(vec![execution(2_500)]);
    let (app, platform) = restarted_app(&session_file, transcript, None).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the reaped card's ❌ ending",
        CardUpdates::Any,
        |card| card_header(card).contains("出错"),
    )
    .await;

    let card = last_update_of(&platform, "om_frozen")
        .await
        .expect("the persisted card is settled in place");
    assert!(
        card_text(&card).contains("503 request queue full"),
        "the failure's own message rides the card: {card}"
    );
    assert!(app.cards_handle().live_cards.get("ses_test").is_none());
}

/// A run that idled with live Background Tasks while cola was down gets the
/// waiting ending — not ✅ — and KEEPS its record, so the later true end is
/// still reaped. The ending is PATCHed once per life, not every tick.
#[tokio::test]
async fn a_restart_reaps_a_persisted_card_to_waiting_and_keeps_its_record() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform) = restarted_app(&session_file, transcript, None).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the reaped card's waiting ending",
        CardUpdates::Any,
        |card| card_header(card).contains("等待后台任务"),
    )
    .await;

    let record = app
        .cards_handle()
        .live_cards
        .get("ses_test")
        .expect("a waiting card keeps its record");
    assert!(record.waiting_reaped, "the waiting ending is marked as stamped");
    assert!(
        !platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card).contains("✅")),
        "a waiting turn must never read ✅"
    );

    // Later passes leave the stamped card alone: one waiting PATCH, not one
    // per Session Sync tick.
    let stamped = platform
        .updated_cards()
        .await
        .iter()
        .filter(|card| card_header(card).contains("等待后台任务"))
        .count();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        platform
            .updated_cards()
            .await
            .iter()
            .filter(|card| card_header(card).contains("等待后台任务"))
            .count(),
        stamped,
        "the waiting ending is stamped once per life: {:?}",
        platform.calls.lock().await
    );
}

/// The #428 variant: a submit whose message never reached the transcript ends
/// 「⚠️ 这条消息未被接收」 — never ✅ — and drops its record.
#[tokio::test]
async fn a_never_promoted_card_ends_unreceived_after_a_restart() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    // The record was written at submit time, before any read carried the
    // message: no anchor was ever captured.
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", None);

    // The session has earlier traffic, but the submitted message never landed.
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_prev", 500, "上一条"),
        assistant(600, "好的。"),
    ])
    .with_executions(vec![execution(700)]);
    let (app, platform) = restarted_app(&session_file, transcript, None).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the reaped card's Unreceived ending",
        CardUpdates::Any,
        |card| card_header(card).contains("未被接收"),
    )
    .await;

    let card = last_update_of(&platform, "om_frozen")
        .await
        .expect("the persisted card is settled in place");
    assert_eq!(card_header(&card), "⚠️ 这条消息未被接收");
    assert!(
        !platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card).contains("✅")),
        "a message nobody received must never read ✅: {:?}",
        platform.calls.lock().await
    );
    assert!(app.cards_handle().live_cards.get("ses_test").is_none());
}

/// The restart-before-the-anchor variant: the message landed (and finished)
/// after the record's last write, so the reap must find it in the transcript
/// rather than read the missing anchor as "never received".
#[tokio::test]
async fn a_landed_message_settles_done_even_when_the_anchor_never_persisted() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", None);

    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, "回答"),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform) = restarted_app(&session_file, transcript, None).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the reaped card's ✅ ending",
        CardUpdates::Any,
        |card| card_header(card).contains("✅"),
    )
    .await;
    assert!(
        !platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card).contains("未被接收")),
        "a landed message is never read as unreceived: {:?}",
        platform.calls.lock().await
    );
}

/// A still-live Session keeps the card and the record: the run may still be
/// answering it, and a restart must not invent an ending for it.
#[tokio::test]
async fn a_still_live_session_keeps_its_persisted_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform) = restarted_app(&session_file, transcript, Some(SessionStatus::Busy)).await;

    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        last_update_of(&platform, "om_frozen").await.is_none(),
        "a live Session's card is never touched: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().live_cards.get("ses_test").is_some(),
        "a still-live Session keeps its record"
    );
}

/// A read the reap cannot make claims nothing: a wedged transcript keeps the
/// record (and the card) for the next tick instead of inventing an ending.
#[tokio::test]
async fn an_unreadable_transcript_keeps_the_record() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![user(
            "msg_cola_anchor",
            1_000,
            "问题",
        )])],
    );
    backend.hang_transcript_reads(1_000);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(test_config(&session_file), Arc::new(backend), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.external
        .request_timeout_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);

    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        last_update_of(&platform, "om_frozen").await.is_none(),
        "an unreadable transcript claims nothing: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().live_cards.get("ses_test").is_some(),
        "the record stays for the next tick"
    );
}

/// A Wake continuation posted by the restart's Session Sync takes the chain
/// over: the old card is collected as 「已由新卡片接管」 and the record follows
/// the successor, so two cards never both look live.
#[tokio::test]
async fn a_restart_continuation_collects_the_old_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (app, platform) = restarted_app(&session_file, transcript, None).await;

    spawn_sync(&app);
    // The continuation posts, streams the resumed work and ends ✅.
    wait_for_card_update(
        &platform,
        "the restart continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;
    // The orphan is collected in place, never left looking live.
    wait_for_card_update(
        &platform,
        "the orphan's taken-over note",
        CardUpdates::Any,
        |card| card_header(card).contains("已由新卡片接管"),
    )
    .await;
    let collected = last_update_of(&platform, "om_frozen")
        .await
        .expect("the orphaned card is collected in place");
    assert_eq!(card_header(&collected), "⏳ 已由新卡片接管 · 已停止更新");

    // The record followed the successor: after the continuation's own settle
    // (it ended ✅ in the same read) the record is spent, but the send that
    // took over re-pointed it first — proven by the successor's own send
    // having tracked a card id that is not the orphan's.
    let record = app.cards_handle().live_cards.get("ses_test");
    assert!(
        record
            .as_ref()
            .is_none_or(|record| record.card_message_id != "om_frozen"),
        "the record never keeps naming the collected orphan: {record:?}"
    );
}

/// A record left naming an older card while a live in-memory card owns the
/// session (the record write raced the handover) is re-pointed at the owner:
/// the sidecar never keeps naming a card nothing will reap — and nothing is
/// collected, because a real handover collects at its own takeover.
#[tokio::test]
async fn a_record_that_lagged_a_handover_is_repointed_at_the_live_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_old", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, "回答"),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform) = restarted_app(&session_file, transcript, None).await;
    // The process's live card: a handover the record missed.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_live")).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;

    spawn_sync(&app);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let record = app
        .cards_handle()
        .live_cards
        .get("ses_test")
        .expect("the live card keeps a record");
    assert_eq!(
        record.card_message_id, "om_live",
        "the record follows the live card"
    );
    assert!(
        platform.updated_cards().await.is_empty(),
        "a re-point collects nothing: {:?}",
        platform.calls.lock().await
    );
}

/// A Turn tracks its card the moment it becomes live — card id, message id and
/// the captured anchor — and a terminal drops the record; a Waiting yield
/// keeps it for the later true end.
#[tokio::test]
async fn a_live_turn_tracks_its_card_and_every_terminal_drops_the_record() {
    let _wd = test_work_dir();
    // The same session, twice: first a Waiting yield (record kept), then the
    // quiet true end (record dropped).
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (dir, app, backend, _platform) = scripted_app(vec![waiting], Some(SessionStatus::Idle)).await;
    let session_file = dir.path().join("sessions.json");

    Turn::run(&app.turn_handles(), ctx("ses_test", "跑一下 CI"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting),
        "the fixture yields"
    );
    let record = app
        .cards_handle()
        .live_cards
        .get("ses_test")
        .expect("a live Turn tracks its card");
    assert_eq!(record.card_message_id, "msg_reply", "the sent card is tracked");
    assert_eq!(record.message_id, "msg_cola_anchor");
    assert_eq!(
        record.created_ms,
        Some(1_000),
        "the record carries the captured anchor's server time"
    );

    // The background work ends while cola is up: the yielded card's quiet true
    // end settles it and the record is spent.
    let ended = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)]);
    script_transcript(&backend, vec![ended]).await;
    spawn_sync(&app);
    wait_for_card_update(
        &_platform,
        "the yielded card's quiet true end",
        CardUpdates::Any,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;
    assert!(
        app.cards_handle().live_cards.get("ses_test").is_none(),
        "the quiet true end is a terminal: its record is spent"
    );
    assert!(
        !sidecar(&session_file).exists(),
        "the emptied record removes the sidecar"
    );
}
