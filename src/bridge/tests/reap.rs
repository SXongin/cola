//! The durable Chain Record reap (ADR-0063, ADR-0069, #438): a fresh app over
//! the sidecar a previous cola life left behind reconciles each persisted
//! record against the Session's own reads — the transcript's real ending
//! settles the card in place (✅ / ❌ / ⏳ 等待后台任务), a never-promoted
//! message ends Unreceived (never ✅), a Wake continuation collects the old
//! card as taken over, a still-live Session keeps the record and its orphaned
//! card gets the one-time restart stamp (#443), and every terminal drops it.
//!
//! Every test drives the real Session Sync pass (`spawn_sync`) over a scripted
//! Backend and a recording Platform, so the assertions read the cards sent and
//! patched and the sidecar file — never private internals.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::drain::{
    assistant, ctx, script_transcript, scripted_app, settle_tool, spawn_sync, spawn_sync_with_timeout,
    spawn_turn, tool_assistant, user, wait_for_card_header, wait_for_card_text,
};
use crate::backend::{
    ContentBlock, FinishReason, MessageId, MessageRole, MessageTime, Part, ReasoningPart, SessionTranscript,
    StepFinish, TextPart, ToolCall, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage,
};
use crate::bridge::card_handles::RenderedBlock;
use crate::bridge::chain::{
    ChainRecords, CursorFrontier, CursorPartKind, PendingGap, RenderedCursor, cursor_prefix_digest,
};
use crate::bridge::snapshot_claims::ClaimKind;
use crate::bridge::test_support::*;
use crate::bridge::turn::Turn;
use crate::config::{SessionEntry, ThreadKey};
use crate::feishu::card::CardState;
use crate::opencode::types::{SessionListInfo, SessionStatus};

/// The sidecar path the app's own rule derives from the session file.
fn sidecar(session_file: &Path) -> PathBuf {
    session_file.with_file_name("chain_records.json")
}

/// Seed the record a previous cola life left behind — the card that was live
/// when the process died — directly into the sidecar through the store's own
/// track entry, so the next app loads it at construction exactly like a real
/// restart.
fn seed_record(session_file: &Path, card_message_id: &str, message_id: &str, created_ms: Option<i64>) {
    seed_chain_record(session_file, card_message_id, message_id, created_ms, None);
}

/// [`seed_record`] carrying the Session's stored directory (the reap's route).
fn seed_chain_record(
    session_file: &Path,
    card_message_id: &str,
    message_id: &str,
    created_ms: Option<i64>,
    directory: Option<&str>,
) {
    ChainRecords::load(sidecar(session_file)).track(
        "ses_test",
        card_message_id,
        MessageId::new(message_id),
        created_ms,
        directory,
        None,
    );
}

/// Seed the durable Wake Watermark a previous cola life left behind — the
/// completion whose in-place PATCH announced it (ADR-0061) — directly into the
/// sidecar file, so the next app loads it at construction exactly like a real
/// restart.
fn seed_wake_mark(session_file: &Path, wake_id: &str, created_ms: i64) {
    ChainRecords::load(sidecar(session_file)).advance("ses_test", wake_id, created_ms);
}

/// The restarted process: a fresh app over `session_file`, with the transcript
/// and status the server would answer now. No card chain is in memory — that
/// is the restart.
async fn restarted_app(
    session_file: &Path,
    transcript: SessionTranscript,
    status: Option<SessionStatus>,
) -> (Arc<App>, Arc<RecordingPlatform>) {
    let (app, platform, _backend) = restarted_app_with_backend(session_file, transcript, status).await;
    (app, platform)
}

/// [`restarted_app`] with the scripted Backend handed back — for a test that
/// re-scripts the session's read mid-life (the tasks retiring while cola
/// watches) or waits on the reap's own reads.
async fn restarted_app_with_backend(
    session_file: &Path,
    transcript: SessionTranscript,
    status: Option<SessionStatus>,
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    let (app, platform, backend) = build_restarted(session_file, transcript).await;
    if let Some(status) = status {
        backend.set_session_status("ses_test", Some(status)).await;
    }
    seed_session(&app, "ses_test", "/work").await;
    (app, platform, backend)
}

/// The restarted process with NO session mapping at all: only the sidecar (and
/// the backend's own reads) says anything about the session — an orphan whose
/// mapping was forgotten while cola was down.
async fn restarted_app_unmapped(
    session_file: &Path,
    transcript: SessionTranscript,
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    build_restarted(session_file, transcript).await
}

/// Build the app over `session_file` with `ses_test`'s transcript scripted and
/// nothing else wired — the common half of the two restart helpers above.
async fn build_restarted(
    session_file: &Path,
    transcript: SessionTranscript,
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    build_restarted_with_sessions(session_file, transcript, Vec::new()).await
}

/// [`build_restarted`] with the store's own session list scripted too — the
/// read the move verdict compares against the record's directory (#439).
async fn build_restarted_with_sessions(
    session_file: &Path,
    transcript: SessionTranscript,
    sessions: Vec<SessionListInfo>,
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript("ses_test", vec![transcript]);
    backend.given_sessions(sessions);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(session_file), backend.clone(), platform.clone())
            .expect("the restarted app builds"),
    );
    (app, platform, backend)
}

/// A turn-capable app over an explicit session file — the two-life test needs
/// the SAME path for the process whose final PATCH failed and the fresh
/// process that reaps the record. Cadences are the tiny test ones.
async fn turn_capable_app(
    session_file: &Path,
    transcript: SessionTranscript,
) -> (Arc<App>, Arc<RecordingPlatform>, Arc<MockBackend>) {
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript("ses_test", vec![transcript]);
    backend.with_session_status("ses_test", Some(SessionStatus::Idle));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(session_file), backend.clone(), platform.clone()).expect("the turn app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.turn_drain_timeout_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);
    app.turn_follow_read_timeout_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);
    (app, platform, backend)
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

/// Wait until the reap read `session_id`'s status at least `n` times — proof
/// its passes ran and did not stop at a guard — or panic after 5 s. Lets a
/// negative assertion ("no card was touched") follow observed work instead of
/// racing a bare sleep.
async fn wait_for_status_reads(backend: &Arc<MockBackend>, session_id: &str, n: usize) {
    let probe = async {
        loop {
            let reads = backend
                .session_status_reads
                .lock()
                .await
                .iter()
                .filter(|sid| sid.as_str() == session_id)
                .count();
            if reads >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("the reap never read {session_id}'s status {n} times"));
}

/// Wait until the session's record names `card_message_id`, or panic after
/// 5 s.
async fn wait_for_record_card(app: &Arc<App>, session_id: &str, card_message_id: &str) {
    let probe = async {
        loop {
            if app
                .cards_handle()
                .chains
                .get(session_id)
                .is_some_and(|record| record.card_message_id == card_message_id)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("the record never named {card_message_id}"));
}

/// Wait until `message_id` has at least `n` recorded in-place PATCHes, or
/// panic after 5 s — the observation a takeover test needs before it asserts
/// on the writes a parked stamp let queue up behind it.
async fn wait_for_patches(platform: &RecordingPlatform, message_id: &str, n: usize) {
    let probe = async {
        loop {
            if patches_to(platform, message_id).await.len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("{message_id} never reached {n} PATCHes"));
}

/// Wait until the registry no longer caches `message_id`, or panic after 5 s.
/// The collect's cache release now runs in a **detached continuation** (spec
/// #571 review), so a test observes it instead of racing it.
async fn wait_for_cache_release(app: &Arc<App>, message_id: &str) {
    let probe = async {
        loop {
            if app
                .cards_handle()
                .card_handles
                .lock()
                .await
                .cached_card(message_id)
                .is_none()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("{message_id}'s cached presentation was never released"));
}

/// How many `get_card_view` reads the platform served for `message_id` — the
/// count the settled-key churn test pins (spec #571, ticket #575): a stamp that
/// already settled must never make the reap read the card again.
fn card_views_to(platform: &RecordingPlatform, message_id: &str) -> usize {
    platform
        .card_view_reads
        .lock()
        .unwrap()
        .iter()
        .filter(|mid| mid.as_str() == message_id)
        .count()
}

/// Wait until the platform served at least `n` card-view reads for
/// `message_id`, or panic after 5 s — the observation the stamp-ordering tests
/// need before they release a parked write, so the interleaving they create is
/// proven rather than raced (spec #571, ticket #575).
async fn wait_for_card_views(platform: &RecordingPlatform, message_id: &str, n: usize) {
    let probe = async {
        loop {
            if card_views_to(platform, message_id) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("{message_id} never reached {n} card-view reads"));
}

/// Wait until `message_id`'s stamp key is covered in the card-delivery queue —
/// settled (delivered, or permanently refused by Feishu, #522) — or panic
/// after 5 s. The observation a "#522 is given up" test needs before it asserts
/// on the ticks that must not touch the card again.
async fn wait_for_covered_stamp(app: &Arc<App>, session_id: &str, card_message_id: &str) {
    let probe = async {
        loop {
            let covered = app.cards_handle().chains.get(session_id).is_some_and(|record| {
                app.cards_handle().feishu.keyed_write_covered(
                    card_message_id,
                    record.generation,
                    crate::feishu::delivery::CardWriteIntent::Stamp,
                )
            });
            if covered {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("the stamp key for {card_message_id} was never covered"));
}

/// The transcript that has finished while cola was down: the submitted
/// message and a clean assistant answer.
fn completed(created_ms: i64) -> SessionTranscript {
    SessionTranscript::new(vec![
        user("msg_cola_anchor", created_ms, "问题"),
        assistant(created_ms + 1_000, "回答"),
    ])
    .with_executions(vec![execution(created_ms + 1_500)])
}

/// The card view a whole-card read answers for a live turn's card: markdown,
/// a collapsible panel with nested markdown, a `form` of controls, an action
/// block and an hr — the shape a reap must keep without its controls (#434
/// acceptance feedback).
fn realistic_card_view() -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "streaming_mode": true },
        "header": {
            "template": "blue",
            "title": { "tag": "plain_text", "content": "✍️ 回复中" }
        },
        "body": { "elements": [
            { "tag": "markdown", "content": "**正文** 第一段" },
            { "tag": "collapsible_panel", "expanded": false,
              "header": { "title": { "tag": "plain_text", "content": "🧰 bash" } },
              "elements": [
                  { "tag": "markdown", "content": "面板里的输出" },
                  { "tag": "button", "text": { "tag": "plain_text", "content": "重试" },
                    "value": { "action": "retry" } }
              ] },
            { "tag": "form", "name": "switch_search", "elements": [
                { "tag": "input", "name": "search" },
                { "tag": "button", "text": { "tag": "plain_text", "content": "搜索" },
                  "value": { "action": "submit" } }
            ] },
            { "tag": "action", "actions": [
                { "tag": "button", "text": { "tag": "plain_text", "content": "重新发起" },
                  "value": { "action": "resume" } }
            ] },
            { "tag": "hr" }
        ] }
    })
}

/// The preservation contract the kept-body tests share: the old body's text
/// survived, no interactive element or control container did (the whole-card
/// read loses a control's `value`, so a kept control could only be dead), and
/// streaming is off.
fn assert_preserved_body(card: &serde_json::Value) {
    assert!(
        card_text(card).contains("**正文** 第一段") && card_text(card).contains("面板里的输出"),
        "the card keeps what it already showed: {card}"
    );
    assert!(
        card_buttons(card).is_empty(),
        "no preserved button survives: {card}"
    );
    assert!(
        !card_has_tag(card, "action") && !card_has_tag(card, "form") && !card_has_tag(card, "input"),
        "no preserved control or control container survives: {card}"
    );
    assert_eq!(
        card["config"]["streaming_mode"], false,
        "a preserved card never keeps a live-streaming presentation: {card}"
    );
    assert_eq!(card["schema"], "2.0", "the PATCH stays schema 2.0: {card}");
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

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;

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
        app.cards_handle().chains.get("ses_test").is_none(),
        "a terminal removes the record"
    );
    assert!(
        sidecar(&session_file).exists(),
        "the Chain Record file is kept when emptied (ADR-0069)"
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
    assert!(app.cards_handle().chains.get("ses_test").is_none());
}

/// The #428/#439 shape: the Session moved itself (into a git worktree) while
/// the run was in flight, so the step was interrupted — and the card a restart
/// reaps carries the move line naming the new directory, explaining the
/// interruption instead of leaving it mysterious. The line carries no chat
/// content.
#[tokio::test]
async fn a_reap_of_a_moved_session_names_the_move() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    let mut interrupted = assistant(2_000, "被打断了。");
    interrupted.error = Some("Step interrupted".into());
    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题"), interrupted])
        .with_executions(vec![execution(2_500)]);
    // The store now reports the Session under its new directory, while the
    // record still names the old one.
    let (app, platform, _backend) = build_restarted_with_sessions(
        &session_file,
        transcript,
        vec![list_session(
            "ses_test",
            "题目",
            "/work/.worktrees/zh-user-guide",
            1_000,
        )],
    )
    .await;

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
    let text = card_text(&card);
    assert!(
        text.contains("Step interrupted"),
        "the failure's own message rides the card: {card}"
    );
    assert!(
        text.contains("会话已迁移") && text.contains("/work/.worktrees/zh-user-guide"),
        "the move line names the new directory: {card}"
    );
    assert!(
        !text.contains("问题") && !text.contains("被打断了。"),
        "the move line rebuilds no chat content: {card}"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the terminal drops the record"
    );
}

/// The never-promoted variant of a moved Session (the #428 follow-up message):
/// the reaped card ends Unreceived — never ✅ — and still names the move.
#[tokio::test]
async fn a_moved_never_promoted_card_names_the_move_too() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(&session_file, "om_frozen", "msg_cola_anchor", None, Some("/work"));

    // The submitted message never landed; the store reports the new directory.
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_prev", 500, "上一条"),
        assistant(600, "好的。"),
    ])
    .with_executions(vec![execution(700)]);
    let (app, platform, _backend) = build_restarted_with_sessions(
        &session_file,
        transcript,
        vec![list_session(
            "ses_test",
            "题目",
            "/work/.worktrees/zh-user-guide",
            500,
        )],
    )
    .await;

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
    let text = card_text(&card);
    assert!(
        text.contains("会话已迁移") && text.contains("/work/.worktrees/zh-user-guide"),
        "the Unreceived card still names the move: {card}"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the terminal drops the record"
    );
}

/// A run whose Session never moved reaps exactly as before: the settling card
/// carries no move line (#439 leaves the no-move cards unchanged).
#[tokio::test]
async fn a_reap_of_an_unmoved_session_carries_no_move_line() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    // The store reports the same directory the record was tracked under.
    let (app, platform, _backend) = build_restarted_with_sessions(
        &session_file,
        completed(1_000),
        vec![list_session("ses_test", "题目", "/work", 1_000)],
    )
    .await;

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
        !card_text(&card).contains("会话已迁移"),
        "a same-directory reap carries no move line: {card}"
    );
}

/// A Session the store's session list does not carry cannot have its move
/// claimed: the card settles exactly as before.
#[tokio::test]
async fn an_unknown_current_directory_claims_no_move() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    let (app, platform, _backend) =
        build_restarted_with_sessions(&session_file, completed(1_000), Vec::new()).await;

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
    assert!(
        !card_text(&card).contains("会话已迁移"),
        "no listed directory claims nothing: {card}"
    );
}

/// The move baseline is the routing directory: a record with no directory of
/// its own falls back to the mapping's, so a mapping that still names the
/// pre-move directory still produces the line (#439).
#[tokio::test]
async fn a_mapping_directory_serves_as_the_move_baseline() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    // The record carries no directory; the mapping (seeded below) is the only
    // baseline the reap has.
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, _backend) = build_restarted_with_sessions(
        &session_file,
        completed(1_000),
        vec![list_session("ses_test", "题目", "/work/moved", 1_000)],
    )
    .await;
    seed_session(&app, "ses_test", "/work").await;

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
    let text = card_text(&card);
    assert!(
        text.contains("会话已迁移") && text.contains("/work/moved"),
        "the mapping's directory is the baseline: {card}"
    );
}

/// A Waiting yield is not a settle: a moved Session's yielded card keeps
/// 「⏳ 等待后台任务」 without the move line — the line belongs to the true end
/// (#439).
#[tokio::test]
async fn a_waiting_yield_carries_no_move_line() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform, _backend) = build_restarted_with_sessions(
        &session_file,
        transcript,
        vec![list_session(
            "ses_test",
            "题目",
            "/work/.worktrees/zh-user-guide",
            1_000,
        )],
    )
    .await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the reaped card's waiting ending",
        CardUpdates::Any,
        |card| card_header(card).contains("等待后台任务"),
    )
    .await;

    let card = last_update_of(&platform, "om_frozen")
        .await
        .expect("the persisted card is yielded in place");
    assert_eq!(card_header(&card), "⏳ 等待后台任务");
    assert!(
        !card_text(&card).contains("会话已迁移"),
        "a yield is not a settle and carries no move line: {card}"
    );
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
    let (app, platform, backend) = restarted_app_with_backend(&session_file, transcript, None).await;

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
        .chains
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

    // Observed later passes leave the stamped card alone: one waiting PATCH,
    // not one per Session Sync tick.
    let stamped = platform
        .updated_cards()
        .await
        .iter()
        .filter(|card| card_header(card).contains("等待后台任务"))
        .count();
    let reads = backend.session_status_reads.lock().await.len();
    wait_for_status_reads(&backend, "ses_test", reads + 3).await;
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

/// A Waiting card orphaned by a restart is not left frozen: when the Session's
/// true end arrives (the last Background Task retires while cola watches), the
/// same truth rule settles it ✅ and spends the record — the waiting stamp
/// never becomes the card's permanent state.
#[tokio::test]
async fn a_waiting_orphan_settles_at_its_true_end() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform, backend) = restarted_app_with_backend(&session_file, waiting, None).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the reaped card's waiting ending",
        CardUpdates::Latest,
        |card| card_header(card).contains("等待后台任务"),
    )
    .await;

    // The background work retires: the same settle rule now reads the true end.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, "已经交给后台了。"),
                assistant(3_100, "CI 通过了。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)]),
        ],
    )
    .await;
    wait_for_card_update(
        &platform,
        "the waiting orphan's true end",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅"),
    )
    .await;
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the true end spends the record"
    );
}

/// A card persisted MID-RESUME (#426/#487): cola died while the resumed run
/// was in flight, so the sidecar still names the request's card and the
/// durable Wake Watermark holds the completion the previous life's in-place
/// PATCH already announced (ADR-0061, ADR-0066). The restart has nothing to
/// post for that Wake, and the reap reads the transcript's true ending — the
/// resumed run finished while cola was down — settling THAT card in place:
/// never frozen at the resuming header its last life showed.
#[tokio::test]
async fn a_card_persisted_mid_resume_is_reaped_to_its_true_end() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_resumed", "msg_cola_anchor", Some(1_000));
    // The previous life's in-place PATCH announced this Wake; the mark is on
    // disk BEFORE the restart builds, exactly as the killed process left it.
    seed_wake_mark(&session_file, "msg_wake_2900", 2_900);

    // The read the restart wakes to: the completion Wake (2_900) resumed the
    // run, and the resumed run's own Execution boundary (4_000) arrived while
    // cola was down — the true end, with every Wake answered.
    let resumed = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (app, platform) = restarted_app(&session_file, resumed, None).await;
    // The restarted process really loaded the previous life's mark from disk:
    // the no-repost assertion below is the durable path's, not a pre-set field.
    assert_eq!(
        app.cards_handle()
            .chains
            .announced("ses_test")
            .map(|mark| mark.created_ms),
        Some(2_900),
        "the restart loads the previous life's Wake Watermark"
    );

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the mid-resume card's true ending",
        CardUpdates::Any,
        |card| card_header(card).contains("✅"),
    )
    .await;

    let card = last_update_of(&platform, "om_resumed")
        .await
        .expect("the mid-resume card is settled in place");
    assert_eq!(
        card_header(&card),
        "✅ 完成",
        "the reap stamps the transcript's true end, not the resuming header"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the true end spends the record"
    );
    // No 承接 card: the mark LOADED from disk is what suppresses the Fresh
    // path — the absent-mark counterfactual (a continuation posted, the old
    // card collected as taken over) is `a_restart_continuation_collects_the_old_card`.
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "a Wake the previous life announced is never re-posted: {:?}",
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
    assert!(app.cards_handle().chains.get("ses_test").is_none());
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

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;

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

/// A still-live Session keeps the record and its orphaned card is stamped
/// once (#443): the run may still answer it, so a restart must not invent an
/// ending — but the card froze when the previous process died, and the stamp
/// tells the user why. The card keeps its own body under the new header.
///
/// The retired per-record mark is gone (spec #571, ticket #575), so the stamp
/// is re-decided on every Session Sync tick; the delivery queue's settled key
/// is what keeps the later ticks free — no further PATCH, and no further
/// card-view GET behind a key that already landed.
#[tokio::test]
async fn a_still_live_session_stamps_its_persisted_card_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    wait_for_card_update(&platform, "the restart stamp", CardUpdates::Any, |card| {
        card_header(card).contains("已重启，等待运行结束")
    })
    .await;
    // Several more observed passes, each reading the live status and
    // re-deciding the stamp: the settled key drops it without a Feishu call.
    wait_for_status_reads(&backend, "ses_test", 5).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "the restart stamp lands exactly once: {patches:?}"
    );
    assert_eq!(card_header(&patches[0]), "⏳ 已重启，等待运行结束");
    assert_preserved_body(&patches[0]);
    assert_eq!(
        card_views_to(&platform, "om_frozen"),
        1,
        "a settled key never re-reads the card: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        card_posts(&platform).await,
        0,
        "the cursorless fallback stamps in place and never arms a successor: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a still-live Session keeps its record"
    );
}

/// A takeover during the stamp's card-view read wins over the stamp (spec
/// #571, tickets #574/#575): the stamp was decided under the orphan record's
/// generation, while the takeover tracked its own card first and submitted its
/// collect under the NEW generation — so the stamp the parked read releases
/// afterwards is dropped by the queue's generation rule, and the collect is
/// the orphan's only write. The pre-check that used to catch this race, and
/// the test-only card-id drop that used to get past it, are both retired
/// (ticket #575): the ordering key is the whole mechanism now.
#[tokio::test]
async fn a_takeover_during_the_stamp_read_wins_over_the_stamp() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // A second live orphan the reap keeps reading: the pass clock proving
    // ticks ran after the parked stamp was released.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Park the stamp's view read: the decision is made, the submission is not.
    let (entered, release) = platform.pause("card_view", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // A successor arms over the orphan while the read is parked: attach first,
    // then collect — the order `take_over_card` owns — and the chain bumps to
    // the new card's generation.
    Turn::seed_card(&app.cards_handle(), "ses_test", None).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
    Turn::take_over_card(&app.cards_handle(), "ses_test", "om_new", Some("/work")).await;

    let before = backend
        .session_status_reads
        .lock()
        .await
        .iter()
        .filter(|sid| sid.as_str() == "ses_other")
        .count();
    release.notify_one();
    // Three more observed passes, the takeover already landed: the released
    // stamp had every chance to (wrongly) land before these.
    wait_for_status_reads(&backend, "ses_other", before + 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "the takeover collect is the orphan's only write, never a stale stamp: {patches:?}"
    );
    assert_eq!(card_header(&patches[0]), "⏳ 已由新卡片接管 · 已停止更新");
}

/// A collect reads the chain's **live generation at submission** (spec #571
/// review): the generation is read after the collect's card-view read, not
/// captured by its caller before it. A takeover that re-points the chain
/// during the read bumps the generation; the collect is then a write of the
/// NEW chain state and still lands, while a writer that decided under the
/// outrun generation is the one the queue drops.
#[tokio::test]
async fn a_collect_submits_the_live_generation_after_its_card_view_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Park the collect's view read: its composition, and so its submission,
    // are still outstanding.
    let (entered, release) = platform.pause("card_view", "om_frozen");

    let cards = app.cards_handle();
    let collect = {
        let cards = cards.clone();
        tokio::spawn(async move {
            crate::bridge::chain::collect_orphan_after_takeover(&cards, "ses_test", "om_frozen", &[]).await
        })
    };
    entered.notified().await;

    // A takeover re-points the chain onto its own card while the read hangs:
    // the chain's generation bumps past the one the parked collect saw.
    cards.chains.track(
        "ses_test",
        "om_new",
        MessageId::new("msg_cola_new"),
        Some(2_000),
        Some("/work"),
        None,
    );
    // The new chain state writes the orphan first, at the NEW generation: the
    // card's queue now holds that generation and has delivered its stamp.
    let stamp = crate::feishu::card::shell::CardBuilder::new()
        .with_state(CardState::Restarted)
        .build();
    let outcome = cards
        .feishu
        .submit_ordered(crate::feishu::delivery::KeyedSubmission {
            message_id: "om_frozen",
            generation: 1,
            intent: crate::feishu::delivery::CardWriteIntent::Stamp,
            card: &stamp,
            fallback: None,
        })
        .await
        .settled()
        .await;
    assert!(matches!(
        outcome,
        crate::feishu::delivery::WriteOutcome::Delivered
    ));

    release.notify_one();
    collect.await.unwrap();

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the new chain state's stamp and the collect, never a dropped collect: {patches:?}"
    );
    assert_eq!(card_header(&patches[0]), "⏳ 已重启，等待运行结束");
    assert_eq!(
        card_header(patches.last().expect("the collect")),
        "⏳ 已由新卡片接管 · 已停止更新",
        "the collect landed under the live generation: {patches:?}"
    );

    // The outrun generation is stale now: a writer that decided under the
    // orphan record's generation is dropped without a Feishu call.
    let stale = crate::feishu::card::shell::CardBuilder::new()
        .with_state(CardState::Restarted)
        .build();
    let outcome = cards
        .feishu
        .submit_ordered(crate::feishu::delivery::KeyedSubmission {
            message_id: "om_frozen",
            generation: 0,
            intent: crate::feishu::delivery::CardWriteIntent::Stamp,
            card: &stale,
            fallback: None,
        })
        .await
        .settled()
        .await;
    assert!(
        matches!(outcome, crate::feishu::delivery::WriteOutcome::Superseded),
        "a submission below the collect's generation is dropped"
    );
    assert_eq!(
        patches_to(&platform, "om_frozen").await.len(),
        2,
        "no write followed the collect"
    );
}

/// An in-flight stamp followed by a collect leaves the collect as the card's
/// last write (spec #571, tickets #574/#575): the takeover's collect is
/// submitted at the NEW chain generation while the stamp's PATCH is parked, so
/// the queue writes it after the stamp. The stamp's legacy post-PATCH repair
/// is retired with the rest of the guard cluster (ticket #575), so the two
/// writes are the whole story — no third PATCH repairs anything.
#[tokio::test]
async fn a_successor_owning_the_session_collects_after_the_stamp() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // A second live orphan: the pass clock.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    // The orphan's view carries a running `⏳` panel and the ledger too: the
    // plain takeover settles `Everything`, so the collect must keep the whole
    // preserved body (ADR-0068's strip is scoped to the fresh-Turn path).
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // Park the stamp's PATCH after its pre-PATCH ownership check passed.
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // The successor arms and re-points the record while the stamp is parked;
    // its collect takes the queue's waiting slot behind the in-flight stamp.
    let takeover = {
        let app = app.clone();
        tokio::spawn(async move {
            Turn::seed_card(&app.cards_handle(), "ses_test", None).await;
            Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
            Turn::take_over_card(&app.cards_handle(), "ses_test", "om_new", Some("/work")).await;
        })
    };
    wait_for_record_card(&app, "ses_test", "om_new").await;

    let before = backend
        .session_status_reads
        .lock()
        .await
        .iter()
        .filter(|sid| sid.as_str() == "ses_other")
        .count();
    release.notify_one();
    takeover.await.unwrap();
    // Three more observed passes: the takeover's collect landed with the
    // parked pass, and any repair PATCH would have followed it.
    wait_for_status_reads(&backend, "ses_other", before + 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the stamp and the takeover's collect, never a repair: {patches:?}"
    );
    assert_eq!(
        card_header(&patches[0]),
        "⏳ 已重启，等待运行结束",
        "the stamp landed in the window this test creates: {patches:?}"
    );
    assert_eq!(
        card_header(patches.last().unwrap()),
        "⏳ 已由新卡片接管 · 已停止更新",
        "the takeover has the card's last word: {patches:?}"
    );
    // No fresh-Turn keep rule was recorded (the plain arm: `take_over_card`),
    // so the collect keeps today's preserved body — the running panel and the
    // ledger both stay, exactly as every Wake/external collect always did.
    let text = card_text(patches.last().unwrap());
    assert!(
        text.contains("⏳ shell") && text.contains("⏳ 后台任务") && text.contains("**正文** 已经写完的部分"),
        "the plain takeover's collect preserves the whole body: {text}"
    );
}

/// The amendment's window (spec #571): a settle decided while the stamp's
/// card-view read is still outstanding closes its generation, so the stamp the
/// parked read releases afterwards is dropped — 「已重启，等待运行结束」 never
/// lands over the ✅. The record is released only after the keyed ending write
/// is confirmed.
#[tokio::test]
async fn a_settle_during_the_stamp_read_closes_the_generation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // A second live orphan the reap keeps reading: the pass clock proving
    // ticks ran after the parked stamp was released.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Busy)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Park the stamp's view read: its submission is not made yet.
    let (entered, release) = platform.pause("card_view", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // The run ends while the stamp's read hangs: the next pass decides the
    // settle, its own read (a later call, not gated) composes the ending, and
    // the keyed ✅ closes the generation.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_update(&platform, "the ✅ ending", CardUpdates::Any, |card| {
        card_header(card).contains("✅")
    })
    .await;
    // The ending is confirmed before the record goes (ADR-0063's amendment).
    wait_for_record_gone(&app, "ses_test").await;

    // Now the stamp's read returns: it composes and submits at the record's
    // generation, which the settle closed.
    release.notify_one();
    let before = backend
        .session_status_reads
        .lock()
        .await
        .iter()
        .filter(|sid| sid.as_str() == "ses_other")
        .count();
    wait_for_status_reads(&backend, "ses_other", before + 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "the ✅ is the orphan's only write, never a late stamp: {patches:?}"
    );
    assert_eq!(card_header(&patches[0]), "✅ 完成");
    assert!(
        !patches.iter().any(|card| card_header(card).contains("已重启")),
        "「已重启」 never appears: {patches:?}"
    );
}

/// The waiting yield shadows a late stamp (spec #571's amendment 2): park the
/// stamp's card-view read, let the run yield to its background tasks — the
/// keyed yield lands 「⏳ 等待后台任务」 — then release the read. The stamp it
/// releases is dropped by the ending shadow, so 「已重启」 never appears; a
/// later live read is covered by the shadow too and never re-reads the card.
#[tokio::test]
async fn a_waiting_yield_shadows_a_late_stamp() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, waiting, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Park the stamp's view read: its submission is not made yet.
    let (entered, release) = platform.pause("card_view", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // The run yields to its background tasks while the stamp's read hangs: the
    // keyed yield lands ⏳.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_update(&platform, "the ⏳ waiting yield", CardUpdates::Any, |card| {
        card_header(card).contains("等待后台任务")
    })
    .await;

    // The stamp's read returns: it composes and submits at the record's
    // generation, which the yield shadowed.
    release.notify_one();
    wait_for_status_reads(&backend, "ses_test", 5).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "the ⏳ is the orphan's only write, never a late stamp: {patches:?}"
    );
    assert_eq!(card_header(&patches[0]), "⏳ 等待后台任务");
    assert!(
        !patches.iter().any(|card| card_header(card).contains("已重启")),
        "「已重启」 never appears: {patches:?}"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a waiting orphan keeps its record"
    );

    // The run is live again: the shadow covers the stamp's re-decision, so the
    // card is never read for it again.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Busy))
        .await;
    wait_for_status_reads(&backend, "ses_test", 9).await;
    assert_eq!(
        card_views_to(&platform, "om_frozen"),
        2,
        "the shadow covers the stamp's re-decision: no further card-view read"
    );
    assert_eq!(
        patches_to(&platform, "om_frozen").await.len(),
        1,
        "and no further write"
    );
}

/// A typed refusal of the *yield's* preserved shape still ends the wait: the
/// bare ⏳ lands keyless — today's degradation — and the waiting record is kept
/// (spec #571's amendment 2 maps the yield's outcomes exactly like the
/// settle's, minus the generation close).
#[tokio::test]
async fn a_rejected_preserved_yield_retries_the_bare_one() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform, _backend) = restarted_app_with_backend(&session_file, waiting, None).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The preserved attempt is refused as card content (230099): the bare ⏳
    // must still land.
    platform
        .fail_update_card_content_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    wait_for_card_update(&platform, "the bare ⏳", CardUpdates::Latest, |card| {
        card_header(card).contains("等待后台任务")
            && card["body"]["elements"]
                .as_array()
                .is_some_and(|elements| elements.is_empty())
    })
    .await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the preserved attempt and its bare retry: {patches:?}"
    );
    assert!(
        card_text(&patches[0]).contains("**正文** 第一段"),
        "the first attempt kept the body: {}",
        patches[0]
    );
    assert!(
        patches[1]["body"]["elements"].as_array().unwrap().is_empty(),
        "the retry is the bare ending: {}",
        patches[1]
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a waiting yield keeps its record"
    );
}

/// The mirror window: a settle the takeover outran is dropped like any stale
/// submission (spec #571's amendment) — it never lands over the collect that
/// bumped the chain.
#[tokio::test]
async fn a_takeover_during_the_settle_read_wins_over_the_settle() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // A second live orphan the reap keeps reading: the pass clock.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Idle)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Park the settle's preserved-ending read: the pass decided, nothing was
    // submitted.
    let (entered, release) = platform.pause("card_view", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // A successor takes the orphan over while the settle's read is parked:
    // the chain bumps to the new card's generation and its collect lands.
    Turn::seed_card(&app.cards_handle(), "ses_test", None).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
    Turn::take_over_card(&app.cards_handle(), "ses_test", "om_new", Some("/work")).await;
    wait_for_patches(&platform, "om_frozen", 1).await;

    let before = backend
        .session_status_reads
        .lock()
        .await
        .iter()
        .filter(|sid| sid.as_str() == "ses_other")
        .count();
    release.notify_one();
    // Three more observed passes: the released settle had every chance to
    // (wrongly) land over the collect before these.
    wait_for_status_reads(&backend, "ses_other", before + 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "the takeover collect is the orphan's only write, never a stale settle: {patches:?}"
    );
    assert_eq!(card_header(&patches[0]), "⏳ 已由新卡片接管 · 已停止更新");
}

// --- The message takeover's seed (spec #561, ticket #565) -------------------
//
// A fresh Turn's takeover over an orphaned card resolves the orphan record's
// Rendered Cursor against the session's own read and seeds the successor: the
// orphaned Turn's window cut at the confirmed frontier (its undelivered tail),
// plus the live set resolved by identity. A record written by an older release
// carries no cursor; its still-running calls keep ADR-0068's carry exactly —
// the seed's live-set fallback, resolved by identity, replaying nothing. The
// seed reads the FULL transcript (the read the render polls use), so the
// cursor's positions stay valid as the run grows.

/// An assistant message with no completion stamp whose only content is one
/// `shell` call started at `started_at` — the orphaned Turn's still-running
/// call (ADR-0068).
fn in_flight_shell(
    id: &str,
    created: i64,
    call_id: &str,
    status: ToolStatus,
    started_at: i64,
    output: &str,
) -> TranscriptMessage {
    TranscriptMessage {
        id: MessageId::new(id),
        role: MessageRole::Assistant,
        time: Some(MessageTime {
            created,
            completed: None,
        }),
        model: None,
        tokens: None,
        error: None,
        parts: vec![Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: "shell".into(),
                call_id: call_id.into(),
            },
            status,
            started_at: Some(started_at),
            input: Some(serde_json::json!({"command": "sleep 3600"})),
            metadata: None,
            output: ToolOutput {
                raw: None,
                blocks: if output.is_empty() {
                    Vec::new()
                } else {
                    vec![ContentBlock::Text(output.to_string())]
                },
                error: None,
            },
        })],
    }
}

/// The whole-card view of an orphaned live card carrying the live tail the
/// takeover collect decides on: written content, a running tool panel (`⏳`),
/// a settled backgrounded launch (`🌙`) and the Background Task Ledger.
fn live_tail_orphan_view() -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "streaming_mode": true },
        "header": {
            "template": "blue",
            "title": { "tag": "plain_text", "content": "✍️ 回复中" }
        },
        "body": { "elements": [
            { "tag": "markdown", "content": "**正文** 已经写完的部分" },
            { "tag": "collapsible_panel", "expanded": false, "element_id": "tool_call_sleep",
              "header": { "title": { "tag": "plain_text", "content": "⏳ shell · 12:00" } },
              "elements": [ { "tag": "markdown", "content": "**Input**\n`sleep 3600`" } ] },
            { "tag": "collapsible_panel", "expanded": false, "element_id": "tool_2",
              "header": { "title": { "tag": "plain_text", "content": "🌙 shell · 12:01" } },
              "elements": [ { "tag": "markdown", "content": "moved to background" } ] },
            { "tag": "collapsible_panel", "expanded": false, "element_id": "task_ledger",
              "header": { "title": { "tag": "plain_text", "content": "⏳ 后台任务（1）" } },
              "elements": [ { "tag": "markdown",
                              "content": "· shell：**npm run build** · 12:00 · 3m12s" } ] }
        ] }
    })
}

/// The seed's ONE INFO decision line (spec #523, story 14) — `capture_logs`'s
/// output for the one test whose requirement is a log line. The behavioral
/// tests assert the cards, never the logs. Panics when the count is not one.
fn seed_info_line(logs: &str) -> &str {
    let matches: Vec<&str> = logs
        .lines()
        .filter(|line| line_level(line) == "INFO" && line.contains("restart seed"))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one INFO seed decision per takeover:\n{logs}"
    );
    matches[0]
}

/// A restarted app over `session_file` whose session serves `transcript` and
/// `status`, with the tiny turn cadences the seed tests need. The returned gate
/// holds every prompt until the test releases it — the live window in which the
/// takeover's seed is observed.
async fn seeded_app(
    session_file: &Path,
    transcript: SessionTranscript,
    status: SessionStatus,
) -> (
    Arc<App>,
    Arc<RecordingPlatform>,
    Arc<MockBackend>,
    Arc<tokio::sync::Semaphore>,
) {
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript("ses_test", vec![transcript]);
    backend.with_session_status("ses_test", Some(status));
    let gate = backend.hold_prompts();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(session_file), backend.clone(), platform.clone()).expect("the seed app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.turn_drain_timeout_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);
    app.turn_follow_read_timeout_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);
    (app, platform, backend, gate)
}

/// ADR-0068's carry, retired into the seed: a user message that starts a fresh
/// Turn over the orphaned card hands the orphan Turn's still-running call to
/// the successor. The panel rides the successor's live tail, its completion
/// renders exactly once after its message has gone stale, and the seed is
/// display-only (the durable record names the new Turn's message; the card
/// still settles ✅).
#[tokio::test]
async fn a_restart_takeover_seeds_the_orphans_running_tool() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    // The call started long before the new message: outside its in-flight
    // window, so only the seed can reach the successor.
    let started_at = 1_300_000;
    let orphan = |status: ToolStatus, output: &str| {
        in_flight_shell(
            "a_orphan",
            orphan_anchor + 500,
            "call_sleep",
            status,
            started_at,
            output,
        )
    };
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan(ToolStatus::Running, ""),
        user("msg_cola_new", new_anchor, "新问题"),
    ]);
    let (app, platform, backend, gate) = seeded_app(&session_file, live, SessionStatus::Busy).await;
    // The orphaned card's rendered view: the seeded running panel rides the
    // successor, so the collect must drop it there (ADR-0068's generalized
    // strip).
    platform.given_card_view("om_frozen", live_tail_orphan_view());

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);

    // The successor's live card carries the orphan's panel while the new
    // prompt is still in flight.
    wait_for_card_text(&platform, "⏳ shell").await;
    // The takeover itself is today's: the old card is collected in place, once.
    let orphan_patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        orphan_patches.len(),
        1,
        "the takeover collect is the orphan's only write: {orphan_patches:?}"
    );
    assert_eq!(card_header(&orphan_patches[0]), "⏳ 已由新卡片接管 · 已停止更新");
    // The seeded panel is live on the successor, so the orphan's preserved
    // body drops it — and the ledger always — while the written body and the
    // settled launch stay.
    let orphan_text = card_text(&orphan_patches[0]);
    assert!(
        !orphan_text.contains("⏳ shell"),
        "the seeded running panel leaves the collected card: {orphan_text}"
    );
    assert!(
        !orphan_text.contains("⏳ 后台任务"),
        "the ledger leaves every takeover collect: {orphan_text}"
    );
    assert!(
        orphan_text.contains("**正文** 已经写完的部分") && orphan_text.contains("🌙 shell"),
        "non-tail body content and the settled launch stay: {orphan_text}"
    );
    // Display-only: the durable record names the NEW Turn's message, never the
    // orphan's.
    wait_for_record_card(&app, "ses_test", "msg_reply").await;
    let record = app.cards_handle().chains.get("ses_test").unwrap();
    assert_eq!(
        record.message_id,
        MessageId::new("msg_cola_new"),
        "the seed moves no record: the successor owns the Turn"
    );

    // The call settles while its message stays stale (beyond the Turn window);
    // the new turn also ends. Every render read must resolve the seeded
    // identity past the window, exactly once.
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
                orphan(ToolStatus::Completed, "slept"),
                user("msg_cola_new", new_anchor, "新问题"),
                assistant(new_anchor + 1_000, "新回答"),
            ])],
        )
        .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;

    wait_for_card_text(&platform, "slept").await;
    let seen = platform.updated_cards().await;
    assert_eq!(
        card_text(seen.last().unwrap()).matches("slept").count(),
        1,
        "the settled result renders exactly once: {seen:?}"
    );
    // Further render reads of the same call must not duplicate it.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let after = platform.updated_cards().await;
    assert_eq!(
        card_text(after.last().unwrap()).matches("slept").count(),
        1,
        "a later transcript render does not duplicate the seeded call"
    );

    // Release the held prompt: the Turn settles ✅ as usual, with the seeded
    // panel settled into the successor's timeline.
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
    wait_for_card_header(&platform, "✅").await;
    let final_card = platform.updated_cards().await;
    let final_text = card_text(final_card.last().unwrap());
    assert_eq!(
        final_text.matches("slept").count(),
        1,
        "the settled seeded result stays exactly once: {final_text}"
    );
    assert!(
        final_text.contains("新回答"),
        "the new Turn's own reply renders as usual: {final_text}"
    );
}

/// The canonical restart window: the fresh Turn's message is queued behind the
/// still-running orphan run, so the server's transcript carries no anchor for
/// as long as that run lasts. A seeded call is resolved by call identity
/// against the whole read, so its completion still reconciles onto the
/// successor's card — while the anchor is unobserved — exactly once, and
/// nothing duplicates when the message finally lands.
#[tokio::test]
async fn a_seeded_call_reconciles_while_the_fresh_turn_is_still_queued() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    let started_at = 1_300_000;
    let orphan = |status: ToolStatus, output: &str| {
        in_flight_shell(
            "a_orphan",
            orphan_anchor + 500,
            "call_sleep",
            status,
            started_at,
            output,
        )
    };
    // The session's newest end while the orphan run still holds the queue: the
    // fresh Turn's message is NOT in the transcript yet — the canonical
    // restart window, with no anchor to observe.
    let queued = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan(ToolStatus::Running, ""),
    ]);
    let (app, platform, backend, gate) = seeded_app(&session_file, queued, SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);

    // The seed hands the successor's live tail the orphan's running call while
    // the submitted prompt is held in flight.
    wait_for_card_text(&platform, "⏳ shell").await;
    assert_eq!(
        Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await,
        None,
        "the fresh message is queued behind the orphan run, so no anchor is observable"
    );

    // The call completes while its message stays stale AND the fresh message is
    // still absent: the seeded identity must reconcile anyway, joining the
    // successor's timeline at its server start key.
    script_transcript(
        &backend,
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            orphan(ToolStatus::Completed, "slept"),
        ])],
    )
    .await;
    wait_for_card_text(&platform, "slept").await;
    assert_eq!(
        Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await,
        None,
        "the completion renders while the anchor is still unobserved"
    );

    // The message finally lands (the orphan run released the queue), the new
    // Turn completes, and the settled call renders exactly once, in place.
    script_transcript(
        &backend,
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            orphan(ToolStatus::Completed, "slept"),
            user("msg_cola_new", new_anchor, "新问题"),
            assistant(new_anchor + 1_000, "新回答"),
        ])],
    )
    .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
    wait_for_card_header(&platform, "✅").await;
    let final_card = platform.updated_cards().await;
    let final_text = card_text(final_card.last().unwrap());
    assert_eq!(
        final_text.matches("slept").count(),
        1,
        "the seeded completion stays exactly once: {final_text}"
    );
    assert!(
        final_text.contains("新回答"),
        "the new Turn's own reply renders as usual: {final_text}"
    );
}

/// The takeover's collect always drops the Background Task Ledger — a
/// ledger-only orphan (its launch settled 🌙 while the run is still live) is
/// collected without the stale live list, and the successor's own reads
/// rebuild it. Nothing is running to seed, so the settled launch and the
/// written body stay untouched.
#[tokio::test]
async fn a_restart_takeover_drops_a_ledger_only_orphans_live_list() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    // The orphan Turn's shell call settled into the background: nothing is
    // still running to seed; only the ledger lists its run.
    let launch = typed_message(
        "msg_launch",
        MessageRole::Assistant,
        Some(orphan_anchor + 500),
        vec![tool_part(
            "shell",
            "call_bg",
            ToolStatus::Completed,
            serde_json::json!({ "command": "npm run build" }),
            "moved to background",
        )],
    );
    // The Session's newest end: the background run is still live, so the
    // successor's own reads rebuild the ledger.
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个构建"),
        launch,
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ])
    .with_background_tasks(vec![background_shell(orphan_anchor + 600)]);
    let (app, platform, _backend, gate) = seeded_app(&session_file, live, SessionStatus::Idle).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);

    // While the prompt is held, the successor's own read rebuilds the live
    // list from the still-running task.
    wait_for_card_update(
        &platform,
        "the successor's rebuilt ledger",
        CardUpdates::Any,
        |card| card_text(card).contains("⏳ 后台任务（1）"),
    )
    .await;

    // The collect drops the orphan's stale live list; the settled 🌙 launch
    // and the written body stay (nothing was seeded).
    let orphan_patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        orphan_patches.len(),
        1,
        "the collect is the orphan's only write: {orphan_patches:?}"
    );
    assert_eq!(card_header(&orphan_patches[0]), "⏳ 已由新卡片接管 · 已停止更新");
    let orphan_text = card_text(&orphan_patches[0]);
    assert!(
        !orphan_text.contains("⏳ 后台任务"),
        "the stale live list leaves the collected card: {orphan_text}"
    );
    assert!(
        orphan_text.contains("🌙 shell") && orphan_text.contains("**正文** 已经写完的部分"),
        "the settled launch and the body stay: {orphan_text}"
    );

    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
}

/// The best-effort collect rule (ticket #526): a takeover collect whose PATCH
/// fails only warns — the successor keeps ownership of the chain and its seeded
/// panel — and never blocks the Turn. The ticket's acceptance names exactly one
/// warning for the failed collect, so that count is read directly; the
/// ownership behavior stands on the cards and the record, not the log.
#[tokio::test]
async fn a_failed_takeover_collect_warns_and_keeps_the_successor() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    let orphan = in_flight_shell(
        "a_orphan",
        orphan_anchor + 500,
        "call_sleep",
        ToolStatus::Running,
        1_300_000,
        "",
    );
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan,
        user("msg_cola_new", new_anchor, "新问题"),
    ]);
    let (app, platform, backend, gate) = seeded_app(&session_file, live, SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // Exactly the collect's PATCH fails: it is the first card write after the
    // new reply, and the failure must not touch anything else.
    platform
        .fail_update_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let ((), logs) = capture_logs(async {
        let turn = spawn_turn(&app, context);
        // The seeded panel is live on the successor: the collect (and its
        // failure) already ran, and the successor still owns the chain.
        wait_for_card_text(&platform, "⏳ shell").await;
        wait_for_record_card(&app, "ses_test", "msg_reply").await;
        // The call settles and the session idles: the Turn ends as usual.
        backend
            .given_transcript_after_build(
                "ses_test",
                vec![SessionTranscript::new(vec![
                    user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
                    in_flight_shell(
                        "a_orphan",
                        orphan_anchor + 500,
                        "call_sleep",
                        ToolStatus::Completed,
                        1_300_000,
                        "slept",
                    ),
                    user("msg_cola_new", new_anchor, "新问题"),
                    assistant(new_anchor + 1_000, "新回答"),
                ])],
            )
            .await;
        backend
            .set_session_status("ses_test", Some(SessionStatus::Idle))
            .await;
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), turn)
            .await
            .expect("a failed collect must not block the turn")
            .unwrap()
            .unwrap();
    })
    .await;

    let warnings = logs
        .lines()
        .filter(|line| line_level(line) == "WARN" && line.contains("could not collect card om_frozen"))
        .count();
    assert_eq!(warnings, 1, "a failed collect warns exactly once:\n{logs}");
}

/// The collect's cache release follows its own delivery (spec #571, ticket
/// #574): while the collect's write is parked at the platform the orphan's
/// cached JSON stays registered, and it is released once the write lands. The
/// release belongs to the delivered repaint that stripped the controls — a
/// submission a newer chain state superseded has nothing to release.
#[tokio::test]
async fn a_takeover_collect_releases_the_cache_only_when_its_write_lands() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    // The registry still holds the orphan's last-rendered JSON: a live block
    // keeps a card cached (a cache with no block is not a cache).
    app.cards_handle().card_handles.lock().await.record(
        "om_frozen",
        &realistic_card_view(),
        vec![RenderedBlock {
            request_id: "req_perm".into(),
            start: 0,
            end: 1,
            kind: ClaimKind::Permission,
            session_id: "ses_test".into(),
            directory: "/work".into(),
            target: "om_frozen".into(),
        }],
    );
    // Park the takeover collect's PATCH.
    let (entered, release) = platform.pause("update", "om_frozen");
    let takeover = {
        let app = app.clone();
        tokio::spawn(async move {
            Turn::seed_card(&app.cards_handle(), "ses_test", None).await;
            Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
            Turn::take_over_card(&app.cards_handle(), "ses_test", "om_new", Some("/work")).await;
        })
    };
    entered.notified().await;
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .cached_card("om_frozen")
            .is_some(),
        "the cache stays while the collect's write is in flight"
    );

    release.notify_one();
    takeover.await.unwrap();
    // The release follows the delivery in the detached continuation (spec #571
    // review).
    wait_for_cache_release(&app, "om_frozen").await;
}

/// A collect that fails recoverably and is later delivered by the drain still
/// releases the old card's cache (spec #571 review): the queue retries the
/// write without this caller ever seeing the delivery, so the release happens
/// conservatively at the failure — the same choice the bounded-await timeout
/// makes, and the release is idempotent — and a re-host can never repaint the
/// collected presentation from the stale cache.
#[tokio::test]
async fn a_recoverably_failed_collect_releases_the_cache() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    // The registry still holds the orphan's last-rendered JSON.
    app.cards_handle().card_handles.lock().await.record(
        "om_frozen",
        &realistic_card_view(),
        vec![RenderedBlock {
            request_id: "req_perm".into(),
            start: 0,
            end: 1,
            kind: ClaimKind::Permission,
            session_id: "ses_test".into(),
            directory: "/work".into(),
            target: "om_frozen".into(),
        }],
    );
    // The collect's first attempt fails recoverably: the queue keeps it owed
    // and retries it.
    platform
        .fail_update_transport_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    Turn::seed_card(&app.cards_handle(), "ses_test", None).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
    Turn::take_over_card(&app.cards_handle(), "ses_test", "om_new", Some("/work")).await;

    // The write is owed — its delivery comes later, without this caller — and
    // the detached continuation releases the cache at the failure.
    wait_for_cache_release(&app, "om_frozen").await;

    // The drain delivers the owed collect: the card is collected, and the
    // stale cache stays gone.
    app.core.feishu.drain_pending_card_updates(true).await;
    wait_for_patches(&platform, "om_frozen", 2).await;
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        card_header(patches.last().unwrap()),
        "⏳ 已由新卡片接管 · 已停止更新",
        "the owed collect landed on the drain: {patches:?}"
    );
    assert!(
        app.cards_handle()
            .card_handles
            .lock()
            .await
            .cached_card("om_frozen")
            .is_none(),
        "the delivered retry still leaves no stale cache"
    );
}

/// The seed's live set is scoped to the orphan Turn's projection: an older,
/// unrelated turn's stale `running` part is never resurrected on the
/// successor.
#[tokio::test]
async fn a_restart_takeover_never_seeds_an_older_turns_stale_call() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    let new_anchor = 2_000_000;
    // Only an unrelated older turn's stale running part: created before the
    // orphan anchor, long past the in-flight window.
    let ghost = in_flight_shell(
        "a_ghost",
        1_000 - 30 * 60_000,
        "call_ghost",
        ToolStatus::Running,
        1_000 - 29 * 60_000,
        "",
    );
    let live = SessionTranscript::new(vec![
        ghost,
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, _backend, gate) = seeded_app(&session_file, live, SessionStatus::Idle).await;
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    let cards = platform.updated_cards().await;
    let text = card_text(cards.last().unwrap());
    assert!(
        !text.contains("call_ghost") && !text.contains("⏳ shell"),
        "an older turn's stale running part is never resurrected: {text}"
    );
}

/// A failed seed read degrades to today's behavior: nothing is seeded, the
/// takeover still collects the orphan under today's keep rule (the running
/// marker stays), and the successor settles as usual.
#[tokio::test]
async fn a_failed_seed_read_seeds_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        in_flight_shell(
            "a_orphan",
            orphan_anchor + 500,
            "call_sleep",
            ToolStatus::Running,
            1_300_000,
            "",
        ),
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, backend, gate) = seeded_app(&session_file, live, SessionStatus::Idle).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // Exactly the seed's read fails; every later read serves normally.
    backend.fail_transcript_reads(1);
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    let cards = platform.updated_cards().await;
    let text = card_text(cards.last().unwrap());
    assert!(!text.contains("call_sleep"), "nothing was seeded: {text}");
    // A failed read seeds nothing, so the orphan's running marker stays in its
    // preserved body — while the ledger leaves it either way.
    let orphan_patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        orphan_patches.len(),
        1,
        "the takeover collect is the orphan's only write: {orphan_patches:?}"
    );
    assert_eq!(
        card_header(&orphan_patches[0]),
        "⏳ 已由新卡片接管 · 已停止更新",
        "the takeover collect is otherwise unchanged"
    );
    let orphan_text = card_text(&orphan_patches[0]);
    assert!(
        orphan_text.contains("⏳ shell"),
        "a failed seed read keeps the running marker: {orphan_text}"
    );
    assert!(
        !orphan_text.contains("⏳ 后台任务"),
        "the ledger leaves the collect even when nothing was seeded: {orphan_text}"
    );
}

/// A timed-out seed read seeds nothing and leaves the takeover as today — the
/// existing read timeout bounds the prompt's wait.
#[tokio::test]
async fn a_timed_out_seed_read_seeds_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        in_flight_shell(
            "a_orphan",
            orphan_anchor + 500,
            "call_sleep",
            ToolStatus::Running,
            1_300_000,
            "",
        ),
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, backend, gate) = seeded_app(&session_file, live, SessionStatus::Idle).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // The seed's read hangs; the 20 ms read timeout ends it.
    backend.hang_transcript_reads(1);
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    let cards = platform.updated_cards().await;
    let text = card_text(cards.last().unwrap());
    assert!(!text.contains("call_sleep"), "nothing was seeded: {text}");
    // A timed-out read seeds nothing, so the orphan's running marker stays in
    // its preserved body — the ledger leaves it either way.
    let orphan_patches = patches_to(&platform, "om_frozen").await;
    let orphan_text = card_text(&orphan_patches[0]);
    assert!(
        orphan_text.contains("⏳ shell"),
        "a timed-out seed read keeps the running marker: {orphan_text}"
    );
    assert!(
        !orphan_text.contains("⏳ 后台任务"),
        "the ledger leaves the collect: {orphan_text}"
    );
}

/// A fresh Turn takes the orphan over while the #443 stamp's PATCH is in
/// flight: the takeover's collect is submitted at the NEW chain generation, so
/// it takes the queue's waiting slot behind the stamp and lands after it — the
/// collect is the orphan's last write, and the retired repair (spec #571,
/// ticket #575) never adds a third. The collect composes the takeover's own
/// strip rule — not `Everything` — so the stamp's stale body never restores
/// the tail the takeover removed. Seeding one running call, that means no
/// running `⏳` panel and no ledger on the orphan.
#[tokio::test]
async fn a_stamp_over_a_seeding_takeover_collects_without_the_seeded_tail() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    let orphan = in_flight_shell(
        "a_orphan",
        orphan_anchor + 500,
        "call_sleep",
        ToolStatus::Running,
        1_300_000,
        "",
    );
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan,
        user("msg_cola_new", new_anchor, "新问题"),
    ]);
    let (app, platform, backend, gate) = seeded_app(&session_file, live, SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // Park the stamp PATCH after its pre-PATCH ownership check passed: it
    // holds the card's delivery lock, so the takeover's collect queues behind
    // it.
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    // The takeover's collect read the orphan's view (the stamp's own read was
    // the first): the observed proof it reached its composition — its PATCH
    // queues behind the parked stamp, which lands first.
    wait_for_card_views(&platform, "om_frozen", 2).await;
    release.notify_one();
    // The collect is the orphan's second write: the parked stamp, then the
    // takeover's queued collect. Waiting for it pins the interleaving this
    // test creates.
    wait_for_patches(&platform, "om_frozen", 2).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the stamp and the takeover's collect, never a repair: {patches:?}"
    );
    assert_eq!(
        card_header(&patches[0]),
        "⏳ 已重启，等待运行结束",
        "the parked stamp landed first: {patches:?}"
    );
    // The one write after the stamp is the takeover's collect: it must not
    // restore the live tail the takeover removed.
    let collect = patches.last().expect("the collect");
    assert_eq!(
        card_header(collect),
        "⏳ 已由新卡片接管 · 已停止更新",
        "the post-stamp write is the takeover's collect: {collect}"
    );
    let text = card_text(collect);
    assert!(
        !text.contains("⏳ shell"),
        "a seeded running marker never returns to the collected card: {text}"
    );
    assert!(
        !text.contains("⏳ 后台任务"),
        "the ledger leaves every takeover collect: {text}"
    );
    assert!(
        text.contains("**正文** 已经写完的部分") && text.contains("🌙 shell"),
        "non-tail body content and the settled launch stay: {text}"
    );

    // End the turn as usual: the seeded call settles and the session idles.
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
                in_flight_shell(
                    "a_orphan",
                    orphan_anchor + 500,
                    "call_sleep",
                    ToolStatus::Completed,
                    1_300_000,
                    "slept",
                ),
                user("msg_cola_new", new_anchor, "新问题"),
                assistant(new_anchor + 1_000, "新回答"),
            ])],
        )
        .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
}

/// The same stamp/takeover interleaving for a takeover that seeded nothing (no
/// live call in the orphan Turn's own window): the takeover's collect — the
/// orphan's second and last write, submitting at the new generation and
/// composing its own strip rule — keeps the running `⏳` marker — spec #523:
/// "When nothing was carried … the running marker stays" — while the ledger
/// still leaves the collected card.
#[tokio::test]
async fn a_stamp_over_an_empty_takeover_collects_without_the_ledger() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    // The orphan Turn's call is already settled in this read: the seed's live
    // set is empty, so nothing moves to the successor.
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        in_flight_shell(
            "a_orphan",
            orphan_anchor + 500,
            "call_sleep",
            ToolStatus::Completed,
            1_300_000,
            "slept",
        ),
        user("msg_cola_new", new_anchor, "新问题"),
    ]);
    let (app, platform, backend, gate) = seeded_app(&session_file, live, SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // The seed resolves no live call: nothing moves to the successor, so the
    // collect keeps the running panel and drops the ledger.
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    // The takeover's collect read the orphan's view (the stamp's own read was
    // the first): the observed proof it reached its composition.
    wait_for_card_views(&platform, "om_frozen", 2).await;
    release.notify_one();
    wait_for_patches(&platform, "om_frozen", 2).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the stamp and the takeover's collect, never a repair: {patches:?}"
    );
    assert_eq!(
        card_header(&patches[0]),
        "⏳ 已重启，等待运行结束",
        "the parked stamp landed first: {patches:?}"
    );
    let collect = patches.last().expect("the collect");
    assert_eq!(
        card_header(collect),
        "⏳ 已由新卡片接管 · 已停止更新",
        "the post-stamp write is the takeover's collect: {collect}"
    );
    let text = card_text(collect);
    assert!(
        text.contains("⏳ shell"),
        "an unseeded running marker stays: {text}"
    );
    assert!(
        !text.contains("⏳ 后台任务"),
        "the ledger leaves the collect: {text}"
    );
    assert!(
        text.contains("**正文** 已经写完的部分") && text.contains("🌙 shell"),
        "non-tail body content and the settled launch stay: {text}"
    );

    // End the turn as usual before the held prompt is released.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
}

/// An ordinary turn with no durable record never seeds: a takeover without an
/// orphan behaves exactly as today — one card, settled ✅.
#[tokio::test]
async fn a_takeover_with_no_orphan_record_seeds_nothing() {
    let _wd = test_work_dir();
    let new_anchor = 2_000_000;
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    assert_eq!(
        card_posts(&platform).await,
        1,
        "the turn's own card is the only send: {:?}",
        platform.calls.lock().await
    );
}

/// A record with no captured anchor seeds nothing — it cannot scope the
/// orphan Turn's projection — and the takeover still collects the orphan under
/// today's keep rule.
#[tokio::test]
async fn an_anchorless_record_seeds_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", None);
    let new_anchor = 2_000_000;
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, _backend, gate) = seeded_app(&session_file, transcript, SessionStatus::Idle).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    let orphan_patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        orphan_patches.len(),
        1,
        "an anchorless record still collects its orphan: {orphan_patches:?}"
    );
    let text = card_text(&orphan_patches[0]);
    assert!(
        text.contains("⏳ shell"),
        "an anchorless record cannot resolve the running marker away: {text}"
    );
}

/// The seed's operator-facing contract (spec #523, story 14): every takeover
/// logs exactly ONE INFO decision line — the session and a decision word,
/// never chat content. The spec's testing rule keeps log text out of the
/// behavioral tests; this is the one test that reads it, because the logging
/// requirement is itself a log line. The fixture is the simplest decision: an
/// orphan Turn with nothing running to seed.
#[tokio::test]
async fn a_seed_decision_logs_one_info_line() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    let new_anchor = 2_000_000;
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, _backend, gate) = seeded_app(&session_file, live, SessionStatus::Idle).await;
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let ((), logs) = capture_logs(async {
        Turn::run(&app.turn_handles(), context).await.unwrap();
    })
    .await;

    wait_for_card_header(&platform, "✅").await;
    let line = seed_info_line(&logs);
    assert!(
        line.contains("ses_test") && line.contains("resolved 0 live calls"),
        "the one INFO line names the session and its decision: {line}"
    );
}

/// A killed run's seeded call does not outlive the Turn: the transcript never
/// settles it, yet the follow still ends ✅ by the Turn's own settle decision
/// (the seeded panel is display-only), and the settled card omits the
/// still-running panel — no permanent `⏳` survives the Turn. A genuinely
/// running call keeps its panel while the successor is live; only the
/// end-of-turn card drops it, because no renderer will ever update it again.
#[tokio::test]
async fn a_killed_runs_seeded_call_does_not_outlive_the_turn() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    let orphan = in_flight_shell(
        "a_orphan",
        orphan_anchor + 500,
        "call_sleep",
        ToolStatus::Running,
        1_300_000,
        "",
    );
    // The call NEVER settles (a killed run leaves its part behind), while the
    // new turn's reply lands terminal.
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan,
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, backend, gate) = seeded_app(&session_file, live, SessionStatus::Busy).await;
    // The drain bound hands the card to the follow, where the live-panel guard
    // would otherwise keep waiting on the seeded call.
    app.turn_drain_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    // The seeded panel is live on the successor while the prompt is held.
    wait_for_card_text(&platform, "⏳ shell").await;

    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must hand off at the drain bound")
        .unwrap()
        .unwrap();
    // The follow observes an idle session; the seeded call is display-only, so
    // it neither extends the settle decision nor outlives the Turn.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_header(&platform, "✅").await;

    let cards = platform.updated_cards().await;
    let settled = cards
        .iter()
        .rev()
        .find(|card| card_header(card).contains("✅"))
        .expect("the follow settled the card ✅");
    let text = card_text(settled);
    assert!(
        !text.contains("⏳ shell"),
        "a killed run's seeded call must not leave a permanent running panel: {text}"
    );
}

/// A record naming a card this process still holds is that card's own
/// lifecycle, never a restart orphan: a run that did not restart is unchanged
/// — no view read, no stamp, no PATCH — even while the Session reads live.
#[tokio::test]
async fn a_card_this_process_still_holds_is_never_stamped() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_live",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Busy)).await;
    // The process's own live card, tracked under the record's id.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_live")).await;
    // A stamp attempt would succeed if one were made.
    platform.given_card_view("om_live", realistic_card_view());

    spawn_sync(&app);
    // The mapped session's transcript reads prove the passes ran.
    wait_for_transcript_reads(&backend, "ses_test", 3).await;
    assert!(
        patches_to(&platform, "om_live").await.is_empty(),
        "a card this process holds is never stamped: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "the live card keeps its record"
    );
}

/// A message still being routed claims the Session for the reap (#424): the
/// pending inbound claim suppresses the restart stamp exactly like a live
/// Turn's guard, so the persisted card is left for the message's own Turn to
/// continue — no stamp races the admission (ADR-0070).
#[tokio::test]
async fn a_pending_inbound_claim_is_never_stamped_over() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // A second live orphan the reap keeps reading: the pass clock proving
    // ticks ran while the claim held.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    // The claim an inbound message leaves while it is being routed (#424):
    // the reap must count it, or its stamp races the message's admission.
    app.waits_handle().note_inbound("ses_test").await;
    // A stamp attempt would succeed if one were made.
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    // The un-claimed orphan keeps reading: the claimed record's passes ran.
    wait_for_status_reads(&backend, "ses_other", 3).await;
    assert!(
        patches_to(&platform, "om_frozen").await.is_empty(),
        "a pending inbound claim is never stamped over: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "the claimed orphan keeps its record for the message's Turn"
    );
}

/// A stamp read the platform cannot serve claims nothing: the card is left
/// exactly as it was — no bare stamp ever lands, because the stamp's whole
/// value is the body it keeps — and the next pass retries until the view is
/// readable (#443).
#[tokio::test]
async fn a_failed_stamp_read_leaves_the_card_untouched_until_readable() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    // No `given_card_view` yet: the read fails, like a missing permission.

    spawn_sync(&app);
    wait_for_status_reads(&backend, "ses_test", 3).await;
    assert!(
        patches_to(&platform, "om_frozen").await.is_empty(),
        "a failed stamp read claims nothing: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "the un-stamped orphan keeps its record"
    );

    // The read recovers: the next pass stamps.
    platform.given_card_view("om_frozen", realistic_card_view());
    wait_for_card_update(&platform, "the retried restart stamp", CardUpdates::Any, |card| {
        card_header(card).contains("已重启，等待运行结束")
    })
    .await;
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(patches.len(), 1, "the retried stamp lands once: {patches:?}");
    assert_preserved_body(&patches[0]);
}

/// A stamp PATCH that fails recoverably is not given up: the queue keeps the
/// write owed with its key, and the next tick's re-decision (the key is not
/// settled, so the card is read again) replaces the owed payload and lands it.
/// Once a write lands the settled key holds — no further attempt (#443,
/// spec #571's ticket #575).
#[tokio::test]
async fn a_failed_stamp_patch_retries_until_it_lands() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The first stamp PATCH fails recoverably (transport class): the write may
    // not have landed, so the queue keeps it owed with its backoff. The reap's
    // per-tick re-decisions inherit that schedule (spec #571 review), so the
    // retry is the drain's — forced here, as the WS reconnect path does.
    platform
        .fail_update_transport_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    wait_for_update_attempts(&platform, "om_frozen", 1).await;
    app.core.feishu.drain_pending_card_updates(true).await;
    wait_for_card_update(&platform, "the retried restart stamp", CardUpdates::Any, |card| {
        card_header(card).contains("已重启，等待运行结束")
    })
    .await;
    // Further observed passes: the settled key holds — no third PATCH.
    wait_for_status_reads(&backend, "ses_test", 5).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the failed attempt and the landing retry, nothing more: {patches:?}"
    );
    assert!(
        patches
            .iter()
            .all(|patch| card_header(patch) == "⏳ 已重启，等待运行结束"),
        "both attempts carry the stamp: {patches:?}"
    );
    assert_preserved_body(&patches[1]);
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a still-live Session keeps its record"
    );
}

/// #522 folds into the queue's one permanent-refusal outcome (spec #571,
/// ticket #575): a stamp PATCH Feishu *permanently* refuses — a typed
/// `CardContentRejected`, deterministic by ADR-0067's own treatment — settles
/// its `(generation, Stamp)` key. Every later Session Sync tick then drops the
/// re-decided stamp without a Feishu call, and the settled-key query also
/// spares the card-view read. The record stays (the pre-#443 behavior for that
/// rare card), and the real ending still supersedes the missing stamp.
#[tokio::test]
async fn a_rejected_stamp_is_attempted_once_and_given_up() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The one stamp attempt is refused as card content (230099): every later
    // tick would send the identical payload, so none may be made.
    platform
        .fail_update_card_content_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    // The refusal settles the queue's key — the fact every later tick reads
    // before it does anything.
    wait_for_covered_stamp(&app, "ses_test", "om_frozen").await;
    // Several observed passes after the rejection: the key holds.
    wait_for_status_reads(&backend, "ses_test", 6).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "the refused stamp is attempted once, never retried: {patches:?}"
    );
    assert_eq!(
        card_views_to(&platform, "om_frozen"),
        1,
        "a refused key is settled too: no later tick re-reads the card"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a given-up stamp keeps its record — the card waits for its real ending"
    );

    // The run ends: transcript truth settles ✅, superseding the missing stamp.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_update(
        &platform,
        "the ✅ over the given-up stamp",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅"),
    )
    .await;
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(patches.len(), 2, "the refused stamp then its settle: {patches:?}");
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the ending spends the record"
    );
}

/// A stamp PATCH that hangs never wedges the pass: the view read and the
/// composition run off the pass's critical path, and the write is owned by the
/// delivery queue — the pass keeps reconciling other records while the write
/// waits. When Feishu finally answers, the stamp lands exactly once (#443).
#[tokio::test]
async fn a_hung_stamp_patch_never_wedges_the_pass() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // A second live orphan whose reads prove the pass kept ticking while the
    // first record's stamp hung.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Park the stamp PATCH forever; nobody (the pass included) awaits it.
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync(&app);
    entered.notified().await;
    // The pass keeps reconciling while the write hangs.
    let before = backend
        .session_status_reads
        .lock()
        .await
        .iter()
        .filter(|sid| sid.as_str() == "ses_other")
        .count();
    wait_for_status_reads(&backend, "ses_other", before + 3).await;
    assert!(
        patches_to(&platform, "om_frozen").await.is_empty(),
        "the hung attempt has not landed: {:?}",
        platform.calls.lock().await
    );

    // Feishu answers: the queue's own write lands, once, and the settled key
    // keeps every later tick from writing it again.
    release.notify_one();
    wait_for_card_update(&platform, "the stamp after the hang", CardUpdates::Any, |card| {
        card_header(card).contains("已重启，等待运行结束")
    })
    .await;
    wait_for_status_reads(&backend, "ses_other", before + 6).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "the hung write lands once, never a retry: {patches:?}"
    );
    assert_preserved_body(&patches[0]);
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a still-live Session keeps its record"
    );
}

/// A run that ends while the stamp's write is in flight is settled only after
/// that write lands (spec #571's amendment): the retired claim no longer gates
/// the decision — the next pass decides the ending freely, its keyed
/// submission queues behind the stamp's in-flight write and lands after it,
/// closing the generation — and the ✅ is the card's last word. No later tick
/// can resurrect the stamp over it.
#[tokio::test]
async fn a_terminal_waits_for_an_in_flight_stamp() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // The pass clock: this record's reads keep ticking after the settle.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Busy)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync(&app);
    entered.notified().await;
    // The run ends while the stamp write hangs: the next pass decides the
    // settle, and its preserved-ending read (the stamp's own was the first
    // card-view read) is the observed point that decision reached the card —
    // the keyed submission queues behind the in-flight stamp.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_views(&platform, "om_frozen", 2).await;
    assert!(
        patches_to(&platform, "om_frozen").await.is_empty(),
        "no settle lands while the stamp write is in flight: {:?}",
        platform.calls.lock().await
    );

    // The write lands, and the settled ✅ is the card's last word.
    release.notify_one();
    wait_for_card_update(&platform, "the ✅ over the stamp", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;
    // Further observed passes: nothing resurrects the stamp over the ✅.
    let before = backend
        .session_status_reads
        .lock()
        .await
        .iter()
        .filter(|sid| sid.as_str() == "ses_other")
        .count();
    wait_for_status_reads(&backend, "ses_other", before + 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(patches.len(), 2, "the stamp then its terminal: {patches:?}");
    assert_eq!(card_header(&patches[0]), "⏳ 已重启，等待运行结束");
    assert_eq!(card_header(&patches[1]), "✅ 完成");
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the terminal spends the record"
    );
}

/// The stamp is not an ending: when the run ends, the transcript-truth settle
/// reads the card back, replaces the header and spends the record as always
/// (#443).
#[tokio::test]
async fn a_later_ending_replaces_the_restart_stamp() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    wait_for_card_update(&platform, "the restart stamp", CardUpdates::Any, |card| {
        card_header(card).contains("已重启，等待运行结束")
    })
    .await;

    // The run ends while cola watches: the status leaves live and the reap
    // settles the card from transcript truth.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_card_update(
        &platform,
        "the settle over the stamp",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅"),
    )
    .await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(patches.len(), 2, "the stamp and its settle: {patches:?}");
    assert_eq!(
        card_header(&patches[0]),
        "⏳ 已重启，等待运行结束",
        "the stamp came first"
    );
    assert_eq!(
        card_header(&patches[1]),
        "✅ 完成",
        "the ending supersedes the stamp"
    );
    assert_preserved_body(&patches[1]);
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the terminal spends the record"
    );
}

/// An unrecognised status kind is unknown, never idle: the reap decides
/// nothing from it — not even with a transcript that reads complete.
#[tokio::test]
async fn an_unrecognised_status_never_decides_an_ending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    // `None` in the script is the backend's `Ok(None)`: a status kind this
    // build does not recognise.
    let (app, platform, backend) = restarted_app_with_backend(&session_file, completed(1_000), None).await;
    backend.set_session_status("ses_test", None).await;

    spawn_sync(&app);
    wait_for_status_reads(&backend, "ses_test", 3).await;
    assert!(
        last_update_of(&platform, "om_frozen").await.is_none(),
        "an unrecognised status claims nothing: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "the record stays for the next tick"
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

    let (app, platform, backend) = restarted_app_with_backend(&session_file, completed(1_000), None).await;
    backend.hang_transcript_reads(1_000);
    app.external
        .request_timeout_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);

    spawn_sync(&app);
    wait_for_status_reads(&backend, "ses_test", 3).await;
    assert!(
        last_update_of(&platform, "om_frozen").await.is_none(),
        "an unreadable transcript claims nothing: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "the record stays for the next tick"
    );
}

/// A record whose session lost its mapping is still reaped when the record
/// carries the directory its reads route under — no cwd-instance guess.
#[tokio::test]
async fn a_stored_directory_routes_the_reap_without_the_mapping() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/gone"),
    );

    // No mapping at all: only the record says where the session lives.
    let (app, platform, _backend) = restarted_app_unmapped(&session_file, completed(1_000)).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the routed card's ✅ ending",
        CardUpdates::Any,
        |card| card_header(card).contains("✅"),
    )
    .await;
    assert!(app.cards_handle().chains.get("ses_test").is_none());
}

/// A record with neither a stored directory nor a mapping decides nothing: on
/// a generation whose reads are per-directory (V1), a cwd-routed status could
/// belong to another instance's run, and stamping over a live run is worse
/// than keeping the record (growth is bounded by the live-card sessions).
#[tokio::test]
async fn a_record_without_a_directory_is_never_decided() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_orphan", "msg_cola_anchor", Some(1_000));

    // The pass needs work it CAN interpret: another mapped session, whose
    // transcript reads are the proof the reap had passes to run.
    let (app, platform, backend) = restarted_app_unmapped(&session_file, completed(1_000)).await;
    backend
        .given_transcript_after_build(
            "ses_other",
            vec![SessionTranscript::new(vec![user(
                "msg_cola_other",
                1_000,
                "别的会话",
            )])],
        )
        .await;
    seed_entry(
        &app,
        SessionEntry {
            thread_key: ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_other".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            variant: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
        },
    )
    .await;

    spawn_sync(&app);
    wait_for_transcript_reads(&backend, "ses_other", 3).await;
    assert!(
        last_update_of(&platform, "om_orphan").await.is_none(),
        "a directory-less record is never decided: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "the record stays for a later life"
    );
}

/// The Wake completion entries a card carries: ledger panels whose title
/// opens with the entry's 🔔 marker, counted for exactly-once assertions.
fn wake_entry_count(card: &serde_json::Value) -> usize {
    fn walk(value: &serde_json::Value, count: &mut usize) {
        match value {
            serde_json::Value::Object(map) => {
                let is_entry = map.get("tag").and_then(|tag| tag.as_str()) == Some("collapsible_panel")
                    && map
                        .get("header")
                        .and_then(|header| header.get("title"))
                        .and_then(|title| title.get("content"))
                        .and_then(|content| content.as_str())
                        .is_some_and(|title| title.starts_with('🔔'));
                if is_entry {
                    *count += 1;
                }
                for nested in map.values() {
                    walk(nested, count);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|nested| walk(nested, count)),
            _ => {}
        }
    }
    let mut count = 0;
    walk(card, &mut count);
    count
}

/// A Wake the durable Watermark already covers is never re-announced by a
/// projection (spec #561, review #569; ADR-0061): the restart's fresh
/// accumulator seeds the Wake floor from the mark, so the successor re-renders
/// the resumed work but inserts no duplicate completion entry.
#[tokio::test]
async fn a_projection_never_re_announces_a_wake_the_watermark_covers() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经交给后台了。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let mid = "停机期间续写的一段。";
    let resumed = "CI 通过了。";
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, delivered),
        assistant(2_500, mid),
        assistant(3_100, resumed),
    ])
    .with_executions(vec![execution(2_600), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    // An earlier card already announced this Wake: the durable mark covers it.
    crate::bridge::chain::ChainRecords::load(session_file.with_file_name("chain_records.json")).advance(
        "ses_test",
        "msg_wake_2900",
        2_900,
    );
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert!(
        successor_text.contains(resumed),
        "the resumed work still renders: {successor}"
    );
    assert_eq!(
        wake_entry_count(&successor),
        0,
        "a Wake the durable Watermark covers is never re-announced: {successor}"
    );
    assert_eq!(
        app.cards_handle()
            .chains
            .announced("ses_test")
            .map(|mark| mark.created_ms),
        Some(2_900),
        "no new announcement is staged for a covered Wake"
    );
}

/// A Wake strictly NEWER than the durable Watermark still announces (spec
/// #561, review #569; ADR-0061): the floor covers only what an earlier card
/// already showed, and the older Wake below it is suppressed.
#[tokio::test]
async fn a_projection_announces_a_wake_newer_than_the_watermark() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经交给后台了。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let mid = "停机期间续写的一段。";
    let resumed = "CI 通过了。";
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, delivered),
        assistant(2_500, mid),
        assistant(3_100, resumed),
    ])
    .with_executions(vec![execution(2_600), execution(3_400), execution(4_200)])
    .with_wakes(vec![shell_wake(2_900), shell_wake(3_600)]);
    // The durable mark already announced the older Wake.
    crate::bridge::chain::ChainRecords::load(session_file.with_file_name("chain_records.json")).advance(
        "ses_test",
        "msg_wake_2900",
        2_900,
    );
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert_eq!(
        wake_entry_count(&successor),
        1,
        "only the strictly newer Wake announces: {successor}"
    );
    assert!(
        successor_text.contains(resumed),
        "the resumed work lands: {successor}"
    );
    wait_for_announced(&app, "ses_test", 3_600).await;
}

/// A record-carrying chain's Wake goes through the projection (spec #561,
/// ticket #566): the durable record hands the Wake to the projection, which
/// renders everything after the confirmed cursor — the content produced while
/// cola was down, the resumed work included — and settles by transcript truth.
/// The Fresh Wake path never arms a card over a recorded chain, and the old
/// card is collected as taken over.
#[tokio::test]
async fn a_record_carrying_wake_goes_through_the_projection() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经交给后台了。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    // The run continued past the confirmed frontier while cola was down —
    // content produced BEFORE the Wake included — then the Wake's completion
    // resumed it and the true end arrived.
    let mid = "停机期间续写的一段。";
    let resumed = "CI 通过了。";
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, delivered),
        assistant(2_500, mid),
        assistant(3_100, resumed),
    ])
    .with_executions(vec![execution(2_600), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    // The projection's collect is not the fresh-Turn takeover: ADR-0068's
    // strip leaves this body untouched.
    platform.given_card_view("om_frozen", live_tail_orphan_view());

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert_eq!(
        card_header(&successor),
        "✅ 完成",
        "the projection settles the transcript's true end"
    );
    assert_eq!(
        successor_text.matches(mid).count(),
        1,
        "the pre-Wake tail lands exactly once: {successor}"
    );
    assert_eq!(
        successor_text.matches(resumed).count(),
        1,
        "the resumed work lands exactly once: {successor}"
    );
    assert!(
        !successor_text.contains(delivered),
        "the delivered prefix is never repeated: {successor}"
    );
    assert_eq!(
        card_posts(&platform).await,
        1,
        "one card, the projection's successor — never a Fresh Wake card: {:?}",
        platform.calls.lock().await
    );

    // The orphan is collected in place, never left looking live.
    let collected = last_update_of(&platform, "om_frozen")
        .await
        .expect("the orphaned card is collected in place");
    assert_eq!(card_header(&collected), "⏳ 已由新卡片接管 · 已停止更新");
    // The projection's collect keeps the running panel it did not resolve;
    // the ledger element always leaves a takeover collect (ADR-0060: the
    // successor's own reads rebuild the live list).
    let collected_text = card_text(&collected);
    assert!(
        collected_text.contains("⏳ shell"),
        "an unresolved running panel stays on the collected card: {collected_text}"
    );
    assert!(
        !collected_text.contains("⏳ 后台任务"),
        "the ledger leaves the projection's collect: {collected_text}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A Wake whose work a projection rendered is announced exactly once (spec
/// #561, ticket #566; ADR-0061, #424): the successor's confirmed create drains
/// the staged Wake Watermark like any delivering card write, so a SECOND
/// restart — recordless by then, the terminal successor having spent the
/// record — finds the Wake covered and the Fresh gate posts nothing.
#[tokio::test]
async fn a_projection_announces_its_wake_across_a_second_restart() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经交给后台了。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let resumed = "CI 通过了。";
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, delivered),
        assistant(2_500, "停机期间续写的一段。"),
        assistant(3_100, resumed),
    ])
    .with_executions(vec![execution(2_600), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript.clone(), Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());

    // Life 1: the projection renders the Wake's resumed work and settles the
    // successor — the confirmed create is the write that announces the Wake.
    spawn_sync(&app);
    let (_successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;
    assert!(successor_text.contains(resumed), "{successor_text}");
    wait_for_record_gone(&app, "ses_test").await;
    wait_for_announced(&app, "ses_test", 2_900).await;
    drop(app); // life 1 ends

    // Life 2: the record is spent, so the chain is recordless and only the
    // durable Watermark can stop a re-announcement. The Fresh gate must Keep.
    let (restarted, platform2, backend2) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    assert!(
        restarted.cards_handle().chains.get("ses_test").is_none(),
        "the terminal successor spent life 1's record"
    );
    spawn_sync(&restarted);
    // The pass ran (its per-session transcript read) before the negative
    // assertion.
    wait_for_transcript_reads(&backend2, "ses_test", 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        card_posts(&platform2).await,
        0,
        "an announced Wake is not re-posted after a second restart: {:?}",
        platform2.calls.lock().await
    );
}

/// The Fresh gate narrows to recordless posts (spec #561, ticket #566): a
/// durable record — cursorless here, so the projection has nothing to seed —
/// hands its Wake to the reap's fallback, which settles the recorded card in
/// place. The Fresh path never arms a card over a recorded chain, so the lost
/// chain's stale running call is never replayed onto a new card.
#[tokio::test]
async fn a_record_carrying_wake_is_never_re_posted_by_the_fresh_path() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000_000));

    // The lost chain: its running call sits beyond the Wake's own in-flight
    // window, so only a Fresh card could ever replay it.
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000_000, "跑一下 CI"),
        in_flight_shell(
            "a_bg",
            1_300_000,
            "call_sleep",
            ToolStatus::Running,
            1_300_000,
            "",
        ),
        assistant(2_100_000, "CI 通过了。"),
    ])
    .with_executions(vec![execution(1_400_000), execution(2_200_000)])
    .with_wakes(vec![shell_wake(2_000_000)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;

    spawn_sync(&app);
    // The fallback still runs: the recorded card settles in place by
    // transcript truth.
    wait_for_card_update(
        &platform,
        "the reap's in-place settle",
        CardUpdates::Any,
        |card| card_header(card).contains("✅"),
    )
    .await;

    assert_eq!(
        card_posts(&platform).await,
        0,
        "a record-carrying Wake never arms a Fresh continuation: {:?}",
        platform.calls.lock().await
    );
    let settled = last_update_of(&platform, "om_frozen")
        .await
        .expect("the recorded card settles in place");
    assert_eq!(card_header(&settled), "✅ 完成");
    assert!(
        !card_text(&settled).contains("call_sleep"),
        "the lost chain's stale running call is never replayed: {settled}"
    );
}

/// A record left naming an older card while a live in-memory card owns the
/// session (the record write raced the handover) collects the orphan in place
/// and re-points the record at the owner: no card is left looking live, and
/// the sidecar never keeps naming a card nothing will reap.
#[tokio::test]
async fn a_record_that_lagged_a_handover_collects_the_orphan_and_repoints() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_old",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    // The process's live card: a handover the record missed.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_live")).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;

    spawn_sync(&app);
    wait_for_record_card(&app, "ses_test", "om_live").await;
    let collected = last_update_of(&platform, "om_old")
        .await
        .expect("the recorded card is collected in place");
    assert_eq!(card_header(&collected), "⏳ 已由新卡片接管 · 已停止更新");
    assert!(
        last_update_of(&platform, "om_live").await.is_none(),
        "the live card is never touched: {:?}",
        platform.calls.lock().await
    );
    let record = app.cards_handle().chains.get("ses_test").unwrap();
    assert_eq!(
        record.directory.as_deref(),
        Some("/work"),
        "the re-point keeps the session's route"
    );
}

/// The lagged handover the reap **releases** — the successor is attached but
/// still anchorless, so the recorded orphan is collected and the record is
/// spent — must collect at the HANDOVER's generation, not at whatever a read
/// inside the detached task finds (spec #571 review). Read after the release
/// that is no record at all — generation zero — and the old chain's late
/// stamp, its pre-submission read outliving the handover, would be admitted at
/// the same generation and repaint 「已重启」 over the takeover notice. The
/// stamp's own key, submitted through the queue's public seam, must be dropped
/// below the collect's generation.
#[tokio::test]
async fn a_released_lagged_handover_collects_at_the_handover_generation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The process's live successor — attached, still anchorless: the reap
    // collects the recorded orphan and releases the record.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_live")).await;
    let lagging = app.cards_handle().chains.generation("ses_test");

    spawn_sync(&app);
    wait_for_patches(&platform, "om_frozen", 1).await;
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the anchorless successor cannot carry the record: it is released"
    );
    assert_eq!(
        card_header(&patches_to(&platform, "om_frozen").await[0]),
        "⏳ 已由新卡片接管 · 已停止更新"
    );

    // The old chain's stamp, decided under the lagging record's generation:
    // the handover's collect owns that generation's card now, so the stamp is
    // dropped without a Feishu call — never painted over the notice.
    let stale = crate::feishu::card::shell::CardBuilder::new()
        .with_state(CardState::Restarted)
        .build();
    let outcome = app
        .cards_handle()
        .feishu
        .submit_ordered(crate::feishu::delivery::KeyedSubmission {
            message_id: "om_frozen",
            generation: lagging,
            intent: crate::feishu::delivery::CardWriteIntent::Stamp,
            card: &stale,
            fallback: None,
        })
        .await
        .settled()
        .await;
    assert!(
        matches!(outcome, crate::feishu::delivery::WriteOutcome::Superseded),
        "a stamp at the lagging generation is dropped below the handover's collect"
    );
    assert_eq!(
        patches_to(&platform, "om_frozen").await.len(),
        1,
        "no write followed the collect"
    );
}

/// The lagged handover the reap **re-points** — a running successor with an
/// armed anchor — collects the orphan at the HANDOVER's generation too (spec
/// #571 review), so it outranks every write the old chain decided under the
/// lagging record's generation even while a prior stamp's read is still
/// outstanding. The record ends following the successor with the successor's
/// anchor, and the late stamp's key is dropped below the collect.
#[tokio::test]
async fn a_repointed_lagged_handover_collects_at_the_handover_generation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The process's live card: a handover the record missed, its anchor armed.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_new")).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
    let lagging = app.cards_handle().chains.generation("ses_test");

    spawn_sync(&app);
    wait_for_patches(&platform, "om_frozen", 1).await;
    // The record follows the successor with the successor's anchor.
    wait_for_record_card(&app, "ses_test", "om_new").await;
    let record = app.cards_handle().chains.get("ses_test").unwrap();
    assert_eq!(
        record.message_id,
        MessageId::new("msg_anchor_1000"),
        "the successor's anchor scopes the reaped chain"
    );

    // The old chain's stamp, decided under the lagging record's generation:
    // the handover's collect outranks it.
    let stale = crate::feishu::card::shell::CardBuilder::new()
        .with_state(CardState::Restarted)
        .build();
    let outcome = app
        .cards_handle()
        .feishu
        .submit_ordered(crate::feishu::delivery::KeyedSubmission {
            message_id: "om_frozen",
            generation: lagging,
            intent: crate::feishu::delivery::CardWriteIntent::Stamp,
            card: &stale,
            fallback: None,
        })
        .await
        .settled()
        .await;
    assert!(
        matches!(outcome, crate::feishu::delivery::WriteOutcome::Superseded),
        "a stamp at the lagging generation is dropped below the handover's collect"
    );
    assert_eq!(
        patches_to(&platform, "om_frozen").await.len(),
        1,
        "no write followed the collect"
    );
}

/// A successor can be attached before its Turn anchor is armed. The pass reads
/// the armed anchor itself (the card probe) and re-reads it right after the
/// collect is handed off, so an anchor that lands around the collect must still
/// re-point the record at the successor — never be read as absent and release
/// it, which would leave the live card unreaped after a later restart. (The
/// collect's own pipeline is detached since spec #571's review, so the anchor
/// is armed before the pass runs rather than while a collect is parked on the
/// platform.)
#[tokio::test]
async fn an_anchor_armed_around_the_collect_still_repoints_the_record() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_old",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_old", realistic_card_view());
    // The process's live card: a handover the record missed — attached, but
    // its Turn anchor is armed only now, after the record was written.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_live")).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;

    spawn_sync_with_timeout(&app, 5_000);

    // The record follows the successor with its freshly armed anchor — never
    // released as an anchorless one.
    wait_for_record_card(&app, "ses_test", "om_live").await;
}

/// The acceptance feedback fix (#434): a collected orphan keeps what it already
/// showed — the takeover restamps the header over the fetched body, with the
/// old card's controls stripped (a whole-card read returns no button `value`,
/// so a kept control could only be dead).
#[tokio::test]
async fn a_collected_orphan_keeps_its_body_without_the_controls() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_old",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_old", realistic_card_view());
    // The process's live card: a handover the record missed.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_live")).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the orphan's taken-over note",
        CardUpdates::Any,
        |card| card_header(card).contains("已由新卡片接管"),
    )
    .await;

    let collected = last_update_of(&platform, "om_old")
        .await
        .expect("the recorded card is collected in place");
    assert_eq!(card_header(&collected), "⏳ 已由新卡片接管 · 已停止更新");
    assert_preserved_body(&collected);
    assert_eq!(
        collected["body"]["elements"][0]["content"], "**正文** 第一段",
        "the collected ending carries no line of its own, so the kept body leads: {collected}"
    );
}

/// A reaped (restart) card settled ✅ keeps its body under the new header
/// (#434 acceptance feedback) — the user sees the card they had, ended,
/// instead of a blank one.
#[tokio::test]
async fn a_reaped_done_card_keeps_its_body() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_frozen", realistic_card_view());

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
    assert_preserved_body(&card);
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "a terminal removes the record"
    );
}

/// A reaped Error card keeps its body too, with the ending's own lines — the
/// failure's message and the move line (#439) — prepended before it, in order
/// (#434 acceptance feedback).
#[tokio::test]
async fn a_reaped_error_card_keeps_its_body_after_the_detail_and_move_line() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );

    let mut failed = assistant(2_000, "没成功。");
    failed.error = Some("503 request queue full".into());
    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题"), failed])
        .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) = build_restarted_with_sessions(
        &session_file,
        transcript,
        vec![list_session(
            "ses_test",
            "题目",
            "/work/.worktrees/zh-user-guide",
            1_000,
        )],
    )
    .await;
    platform.given_card_view("om_frozen", realistic_card_view());

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
    assert_eq!(card_header(&card), "❌ 出错");
    let elements = card["body"]["elements"].as_array().unwrap();
    assert!(
        elements[0]["content"]
            .as_str()
            .is_some_and(|line| line.contains("503 request queue full")),
        "the failure's own message leads: {card}"
    );
    assert!(
        elements[1]["content"].as_str().is_some_and(
            |line| line.contains("会话已迁移") && line.contains("/work/.worktrees/zh-user-guide")
        ),
        "the move line follows the detail: {card}"
    );
    assert_eq!(
        elements[2]["content"], "**正文** 第一段",
        "the kept body follows both lines: {card}"
    );
    assert_preserved_body(&card);
}

/// The Unreceived ending keeps the card's body as well — the copy (and the
/// absence of the 重新发起 action) is the ending's, the content is the user's
/// (#434 acceptance feedback, ADR-0062's reaped-card rule).
#[tokio::test]
async fn a_reaped_unreceived_card_keeps_its_body() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", None);

    // The session has earlier traffic, but the submitted message never landed.
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_prev", 500, "上一条"),
        assistant(600, "好的。"),
    ])
    .with_executions(vec![execution(700)]);
    let (app, platform) = restarted_app(&session_file, transcript, None).await;
    platform.given_card_view("om_frozen", realistic_card_view());

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
    assert_preserved_body(&card);
    assert!(app.cards_handle().chains.get("ses_test").is_none());
}

/// A Waiting yield keeps the body too, and the record stays for the later true
/// end (#434 acceptance feedback).
#[tokio::test]
async fn a_reaped_waiting_card_keeps_its_body() {
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
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the reaped card's waiting ending",
        CardUpdates::Any,
        |card| card_header(card).contains("等待后台任务"),
    )
    .await;

    let card = last_update_of(&platform, "om_frozen")
        .await
        .expect("the persisted card is yielded in place");
    assert_eq!(card_header(&card), "⏳ 等待后台任务");
    assert_preserved_body(&card);
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a waiting yield keeps its record"
    );
}

/// An unreadable card view settles the bare ending exactly as before the fix:
/// the reap must never depend on the read (#434 acceptance feedback).
#[tokio::test]
async fn a_reap_without_a_readable_card_view_falls_back_to_the_bare_ending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    // No `given_card_view`: the read fails, like a missing permission.
    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;

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
        card["body"]["elements"].as_array().unwrap().is_empty(),
        "an unreadable view settles the bare ending: {card}"
    );
}

/// When the platform refuses the preserved shape (e.g. `230099`), the ending
/// still lands: the PATCH retries once with the bare ending, so the card never
/// stays looking live behind a rejected nicety (#434 acceptance feedback).
#[tokio::test]
async fn a_rejected_preserved_ending_retries_the_bare_one() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    platform
        .fail_update_card_content_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    wait_for_card_update(&platform, "the bare retry", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
            && card["body"]["elements"]
                .as_array()
                .is_some_and(|elements| elements.is_empty())
    })
    .await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the preserved attempt and its bare retry: {patches:?}"
    );
    assert!(
        card_text(&patches[0]).contains("**正文** 第一段"),
        "the first attempt kept the body: {}",
        patches[0]
    );
    assert!(
        patches[1]["body"]["elements"].as_array().unwrap().is_empty(),
        "the retry is the bare ending: {}",
        patches[1]
    );
    // The fallback landed THROUGH the keyed queue (spec #571 review): its
    // delivery settled the ending's key DELIVERED, so the queue now answers
    // the key's re-decisions with `Delivered` and writes nothing again. A bare
    // retry sent outside the queue would have left the key permanently
    // REFUSED by the preserved attempt's rejection, answering `Superseded`.
    let again = crate::feishu::card::shell::CardBuilder::new()
        .with_state(CardState::Done)
        .build();
    let ticket = app
        .cards_handle()
        .feishu
        .submit_ordered(crate::feishu::delivery::KeyedSubmission {
            message_id: "om_frozen",
            generation: 0,
            intent: crate::feishu::delivery::CardWriteIntent::Settle,
            card: &again,
            fallback: None,
        })
        .await;
    assert!(
        matches!(
            ticket.settled().await,
            crate::feishu::delivery::WriteOutcome::Delivered
        ),
        "the keyed fallback settled the key delivered, not refused"
    );
    assert_eq!(
        patches_to(&platform, "om_frozen").await.len(),
        2,
        "the settled key never writes again"
    );
}

/// A permanently refused ending — the preserved shape AND its bare fallback —
/// is a confirmed non-delivery: the terminal record is spent (ADR-0067:
/// delivery *or* permanent refusal releases) instead of surviving every pass,
/// and no card-view GET repeats (spec #571 review).
#[tokio::test]
async fn a_permanently_refused_ending_releases_the_record() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // A second live orphan the reap keeps reading: the pass clock.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Idle)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Both the preserved ending and its bare fallback are refused as card
    // content (230099): nothing can ever land for this key.
    platform
        .fail_update_card_content_count
        .store(2, std::sync::atomic::Ordering::SeqCst);

    spawn_sync_with_timeout(&app, 5_000);
    // The refusal spends the record: no later pass may bring the ending back.
    wait_for_record_gone(&app, "ses_test").await;
    // Several more observed passes: the settled key is never re-read.
    wait_for_status_reads(&backend, "ses_other", 3).await;

    assert_eq!(
        card_views_to(&platform, "om_frozen"),
        1,
        "a refused ending is never re-read: {:?}",
        platform.calls.lock().await
    );
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the preserved attempt and its bare fallback, nothing more: {patches:?}"
    );
}

/// A refused waiting yield keeps its record (only the true end spends it), but
/// its settled key short-circuits the re-read: the reap must not GET the card
/// again every tick (spec #571 review).
#[tokio::test]
async fn a_refused_waiting_yield_is_never_re_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, waiting, Some(SessionStatus::Idle)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    platform
        .fail_update_card_content_count
        .store(2, std::sync::atomic::Ordering::SeqCst);

    spawn_sync_with_timeout(&app, 5_000);
    wait_for_status_reads(&backend, "ses_other", 4).await;

    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a waiting orphan keeps its record"
    );
    assert_eq!(
        card_views_to(&platform, "om_frozen"),
        1,
        "the refused yield is never re-read: {:?}",
        platform.calls.lock().await
    );
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the refused attempt set, nothing more: {patches:?}"
    );
}

/// The collect path's degradation goes through the queue's fallback too (spec
/// #571 review): a preserved collect the platform refuses as card content still
/// lands its bare taken-over marker, under the collect's own
/// `(generation, Collect)` key — never a keyless write the generation order
/// cannot see.
#[tokio::test]
async fn a_rejected_preserved_collect_lands_the_bare_marker() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The preserved collect is refused as card content (230099): its bare
    // marker must still land.
    platform
        .fail_update_card_content_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    // A successor takes the orphan over: the collect PATCHes the orphan card.
    Turn::seed_card(&app.cards_handle(), "ses_test", None).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
    Turn::take_over_card(&app.cards_handle(), "ses_test", "om_new", Some("/work")).await;

    // The collect's submission is detached (spec #571 review): the refused
    // attempt and its delivered fallback both land through the queue.
    wait_for_patches(&platform, "om_frozen", 2).await;
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the preserved collect and its bare fallback: {patches:?}"
    );
    assert!(
        card_text(&patches[0]).contains("**正文** 第一段"),
        "the first attempt kept the body: {}",
        patches[0]
    );
    assert_eq!(card_header(&patches[1]), "⏳ 已由新卡片接管 · 已停止更新");
    assert!(
        patches[1]["body"]["elements"].as_array().unwrap().is_empty(),
        "the fallback is the bare taken-over marker: {}",
        patches[1]
    );
}

/// Different collect variants are different logical writes (spec #571 review):
/// the ordinary collect (the reap's successor collect, keeping the whole body)
/// and the fresh-Turn takeover's strip collect share a generation but carry
/// different payload rules, so the strip collect must not collapse into the
/// ordinary one as a duplicate — it lands after it, and the stripped live tail
/// (the ledger) is the card's last word.
#[tokio::test]
async fn a_takeover_collect_is_not_dropped_behind_an_ordinary_collect() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform) = restarted_app(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // Park the ordinary collect's PATCH.
    let (entered, release) = platform.pause("update", "om_frozen");

    let cards = app.cards_handle();
    let ordinary = {
        let cards = cards.clone();
        tokio::spawn(
            async move { crate::bridge::chain::collect_orphan(&cards, "ses_test", "om_frozen").await },
        )
    };
    entered.notified().await;

    // The fresh-Turn takeover's strip collect queues at the SAME generation
    // behind the in-flight ordinary collect.
    let strip = {
        let cards = cards.clone();
        tokio::spawn(async move {
            crate::bridge::chain::collect_orphan_after_takeover(&cards, "ses_test", "om_frozen", &[]).await
        })
    };
    // Both collects read the card (the ordinary one's read was the first).
    wait_for_card_views(&platform, "om_frozen", 2).await;

    release.notify_one();
    ordinary.await.unwrap();
    strip.await.unwrap();
    // The submission is detached (spec #571 review): the strip collect lands
    // through the queue after its submitter returned.
    wait_for_patches(&platform, "om_frozen", 2).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the ordinary collect and the takeover's strip collect, never a collapsed duplicate: {patches:?}"
    );
    let text = card_text(patches.last().expect("the strip collect"));
    assert!(
        !text.contains("⏳ 后台任务"),
        "the strip collect owns the card's last word: {text}"
    );
    assert!(
        text.contains("**正文** 已经写完的部分"),
        "the strip collect keeps the rest of the body: {text}"
    );
}

/// The bare-ending fallback stays inside the queue's ordering (spec #571
/// review): a preserved ending the platform refuses as card content
/// degrades to the bare ending, but under the SAME key and the same held card
/// lock — a newer-generation collect queued behind the ending owns the card's
/// last word. The pre-review keyless retry could acquire the card lock after
/// the collect and repaint a stale ✅ over the taken-over card.
#[tokio::test]
async fn a_refused_preserved_ending_never_lands_over_a_newer_collect() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The preserved ✅ ending is refused as card content (230099): its bare
    // fallback must still land — ordered under the ending's own key.
    platform
        .fail_update_card_content_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    // Park the preserved ending's PATCH: the refusal and the fallback are both
    // still outstanding when the takeover queues behind them.
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // A successor takes the orphan over while the ending is parked: its collect
    // queues at the new chain generation and must be the card's last write.
    let takeover = {
        let app = app.clone();
        tokio::spawn(async move {
            Turn::seed_card(&app.cards_handle(), "ses_test", None).await;
            Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
            Turn::take_over_card(&app.cards_handle(), "ses_test", "om_new", Some("/work")).await;
        })
    };
    wait_for_record_card(&app, "ses_test", "om_new").await;

    release.notify_one();
    takeover.await.unwrap();
    // The preserved attempt, the bare fallback and the collect — nothing more.
    wait_for_patches(&platform, "om_frozen", 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        card_header(patches.last().expect("the writes")),
        "⏳ 已由新卡片接管 · 已停止更新",
        "the newer collect owns the card's last word, never the stale ending: {patches:?}"
    );
}

/// A transport failure on the keyed ending is never retried bare, and a later
/// tick does not re-PATCH it either: accepting the ending closed its
/// generation, so the reap's re-decisions are dropped and the queue's own
/// retry (the drain) owns the write. The record stays — the release gate sees
/// the owed ending (spec #571's amendment).
#[tokio::test]
async fn a_transport_failed_settle_stays_owed_for_the_drain() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, backend) = restarted_app_with_backend(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // A transport failure on every attempt, so each observed tick records what
    // it sent before failing.
    platform
        .fail_update_transport_count
        .store(100, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    // Three observed status reads: at least two later passes re-decided the
    // ending and dropped it on the closed generation.
    wait_for_status_reads(&backend, "ses_test", 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "one keyed attempt; the closed generation drops every re-decision: {patches:?}"
    );
    assert!(
        card_text(&patches[0]).contains("**正文** 第一段"),
        "no transport failure falls back to the bare ending: {patches:?}"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a failed settle keeps the record for the queue's retry"
    );
    assert!(
        app.cards_handle().feishu.has_pending_card_update("om_frozen"),
        "the ending is still owed"
    );
}

/// The queue's own retry converges an owed ending: once its backoff is due the
/// pass drain delivers the keyed write, and the reap — whose re-decision now
/// reads the settled key — confirms the ending and releases the record. No
/// restart and no forced drain; the paused clock elapses the backoff virtually
/// (spec #571's amendment).
#[tokio::test(start_paused = true)]
async fn an_owed_settle_converges_through_the_pass_drain() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The first keyed attempt fails at the transport; the queue keeps the
    // ending owed with its backoff.
    platform
        .fail_update_transport_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    // The pass drain retries once the 5s backoff elapses; the reap then
    // confirms the ending and releases the record — both observed here, or the
    // test times out.
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if app.cards_handle().chains.get("ses_test").is_none()
                && !app.cards_handle().feishu.has_pending_card_update("om_frozen")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the pass drain converges the ending and the reap releases the record");

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        2,
        "the failed attempt and the drain's retry: {patches:?}"
    );
    assert_eq!(card_header(patches.last().unwrap()), "✅ 完成");
}

/// A hung keyed write never holds Session Sync (spec #571 review): the platform
/// never answers the settle's keyed ✅, so the write stays in flight — never
/// cancelled — while the writer's bounded ticket await gives up and the pass
/// keeps ticking. The queue still owns the write, so the ending stays owed (the
/// record is not confirmed) and the card is never re-PATCHed behind it.
#[tokio::test]
async fn a_hung_keyed_write_does_not_block_the_pass() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The writer's ticket await is tiny: the pass must never wait on Feishu —
    // the write itself is never cancelled and stays in flight.
    platform.given_keyed_ticket_await(Duration::from_millis(50));
    // Park the settle's keyed ✅: the write is issued and unanswered.
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // The pass keeps reading the session while the write hangs: its own bound,
    // never the platform's, is what it waits on.
    wait_for_status_reads(&backend, "ses_test", 3).await;

    // The write is still in flight and owned by the queue: the ending stays
    // owed — the record waits for it — and no re-decision PATCHes behind it.
    assert!(
        app.cards_handle().feishu.keyed_write_covered(
            "om_frozen",
            0,
            crate::feishu::delivery::CardWriteIntent::Settle
        ),
        "the hung write still owns its key"
    );
    assert!(
        app.core.feishu.has_pending_card_update("om_frozen"),
        "the hung ending is still owed"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "the record waits for its ending"
    );
    assert!(
        patches_to(&platform, "om_frozen").await.is_empty(),
        "no retry PATCHes the card while the issued write is unanswered"
    );

    // Feishu finally answers: the hung write commits as the card's only PATCH.
    release.notify_one();
    wait_for_patches(&platform, "om_frozen", 1).await;
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "one issued write, never a retry behind it: {patches:?}"
    );
    assert_eq!(card_header(&patches[0]), "✅ 完成");
}

/// The reap pass itself never awaits an ending write (spec #571 review): with
/// the production ticket bound (30 s), a hung settle PATCH must not stop the
/// pass from reading its other records, and the ending's outcome handling —
/// the terminal record release — still arrives through the detached
/// continuation once the write resolves.
#[tokio::test]
async fn a_hung_ending_write_does_not_block_the_pass() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // A second live orphan the reap keeps reading: the pass clock.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Idle)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Park the ending's keyed ✅: the production ticket bound is what the pass
    // would otherwise wait on.
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // The pass keeps processing its other records while the write hangs — the
    // 5 s read bound cannot cover a 30 s ticket await.
    wait_for_status_reads(&backend, "ses_other", 3).await;

    // The write resolves: the detached continuation confirms the ending and
    // releases the record.
    release.notify_one();
    wait_for_record_gone(&app, "ses_test").await;
}

/// A hung card-view read never freezes Session Sync (spec #571 review): a
/// collect's preserved-ending read is bounded like the #443 stamp's own view
/// read, so a GET that never returns degrades to the bare collect — the
/// existing read-failure fallback — and the pass proceeds. (The bounded ticket
/// await only starts afterwards; this is the read before it.)
#[tokio::test]
async fn a_hung_card_view_does_not_block_a_collect() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Busy)).await;
    // The view read's own bound, tiny so the pass cannot hide behind it.
    app.cards_handle()
        .preserved_view_timeout_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    // A successor card owns the session in-process: the pass collects the
    // recorded orphan before its decision.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_new")).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
    // Park the orphan's view read: the collect's composition never returns.
    let (_entered, _release) = platform.pause("card_view", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    // The pass proceeds and lands the bare collect within the read bound.
    wait_for_patches(&platform, "om_frozen", 1).await;
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(card_header(&patches[0]), "⏳ 已由新卡片接管 · 已停止更新");
    assert!(
        patches[0]["body"]["elements"].as_array().unwrap().is_empty(),
        "a timed-out view read settles the bare ending: {}",
        patches[0]
    );
}

/// The settle path's view read is bounded too (spec #571 review): a GET that
/// never returns degrades to the bare ✅ — the existing read-failure fallback —
/// and the record is spent, instead of freezing the pass.
#[tokio::test]
async fn a_hung_card_view_does_not_block_a_settle() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Idle)).await;
    app.cards_handle()
        .preserved_view_timeout_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    // Park the ending's view read: the preserved composition never returns.
    let (_entered, _release) = platform.pause("card_view", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    wait_for_patches(&platform, "om_frozen", 1).await;
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(card_header(&patches[0]), "✅ 完成");
    assert!(
        patches[0]["body"]["elements"].as_array().unwrap().is_empty(),
        "a timed-out view read settles the bare ending: {}",
        patches[0]
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "the bare terminal spends the record"
    );
}

/// The reap pass never awaits the collect's card-view GET either (spec #571
/// review): with the preserved-card read PARKED (the production bound, never
/// released while the pass runs), the pass still processes its other records;
/// once the read returns, the collect lands exactly once.
#[tokio::test]
async fn a_hung_collect_card_view_does_not_block_the_pass() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_chain_record(
        &session_file,
        "om_old",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
    );
    // A second live orphan the reap keeps reading: the pass clock.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Idle)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    // The process's live card: a handover the record missed, so the pass
    // collects the recorded orphan before its decision.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_live")).await;
    platform.given_card_view("om_old", realistic_card_view());
    // Park the collect's card-view read: the composition is outstanding.
    let (entered, release) = platform.pause("card_view", "om_old");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // The pass proceeds with its other records while the GET is parked — the
    // 5 s read clock cannot cover a 30 s inline await.
    wait_for_status_reads(&backend, "ses_other", 3).await;

    // The read returns: the collect lands once, and never a second PATCH.
    release.notify_one();
    wait_for_patches(&platform, "om_old", 1).await;
    let patches = patches_to(&platform, "om_old").await;
    assert_eq!(patches.len(), 1, "the collect lands exactly once: {patches:?}");
    assert_eq!(card_header(&patches[0]), "⏳ 已由新卡片接管 · 已停止更新");
}

/// The settle path's card-view GET is off the pass too (spec #571 review):
/// with the preserved-card read parked, the pass keeps processing its other
/// records; once the read returns, the preserved ✅ lands and the record is
/// spent.
#[tokio::test]
async fn a_hung_settle_card_view_does_not_block_the_pass() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
        None,
    );

    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, completed(1_000), Some(SessionStatus::Idle)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Park the ending's card-view read: the preserved composition is
    // outstanding.
    let (entered, release) = platform.pause("card_view", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // The pass proceeds with its other records while the GET is parked.
    wait_for_status_reads(&backend, "ses_other", 3).await;

    // The read returns: the preserved ✅ lands once and spends the record.
    release.notify_one();
    wait_for_patches(&platform, "om_frozen", 1).await;
    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(patches.len(), 1, "the ending lands exactly once: {patches:?}");
    assert_eq!(card_header(&patches[0]), "✅ 完成");
    assert!(
        card_text(&patches[0]).contains("**正文** 第一段"),
        "the preserved body lands after the parked read: {}",
        patches[0]
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A Turn tracks its card the moment it becomes live — card id, message id,
/// the captured anchor and the session's directory — and a terminal drops the
/// record; a Waiting yield keeps it for the later true end.
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
        .chains
        .get("ses_test")
        .expect("a live Turn tracks its card");
    assert_eq!(record.card_message_id, "msg_reply", "the sent card is tracked");
    assert_eq!(record.message_id, MessageId::new("msg_cola_anchor"));
    assert_eq!(
        record.created_ms,
        Some(1_000),
        "the record carries the captured anchor's server time"
    );
    assert_eq!(
        record.directory.as_deref(),
        Some("/work"),
        "the record carries the session's route"
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
        app.cards_handle().chains.get("ses_test").is_none(),
        "the quiet true end is a terminal: its record is spent"
    );
    assert!(
        sidecar(&session_file).exists(),
        "the Chain Record file is kept when emptied (ADR-0069)"
    );
}

/// ADR-0063 amendment + ADR-0067: a terminal card whose final PATCH failed
/// keeps its durable record — the ending is still owed as a Pending Card
/// Update — and the record stays until the write is confirmed. Before the
/// amendment the record was dropped before the PATCH, so a restart (and the
/// still-running process alike) found nothing to repair.
#[tokio::test]
async fn a_failed_final_patch_keeps_the_record_until_delivery() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "答复。"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    // Every card write fails at the transport: nothing lands, and the turn's
    // ending is the newest failed payload.
    platform
        .fail_update_transport_count
        .store(100, std::sync::atomic::Ordering::SeqCst);

    Turn::run(&app.turn_handles(), ctx("ses_test", "你好"))
        .await
        .unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Done),
        "the turn still reaches its ending"
    );
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("a failed ending write keeps the record");
    let card_id = record.card_message_id.clone();
    assert!(
        app.core.feishu.has_pending_card_update(&card_id),
        "the ending is still owed"
    );

    // Feishu returns: a forced drain (the WS reconnect path) delivers the
    // newest payload.
    platform
        .fail_update_transport_count
        .store(0, std::sync::atomic::Ordering::SeqCst);
    app.core.feishu.drain_pending_card_updates(true).await;
    assert!(
        !app.core.feishu.has_pending_card_update(&card_id),
        "the drain delivered the ending"
    );

    // The next Session Sync pass's reap drops the now-confirmed record.
    spawn_sync(&app);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if app.cards_handle().chains.get("ses_test").is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the reap must clean the confirmed record");
}

/// The Session Sync pass itself retries a failed final PATCH once its backoff
/// is due — no restart and no forced drain: the pending outbox converges, and
/// the reap then cleans the record. The paused clock elapses the backoff
/// virtually.
#[tokio::test(start_paused = true)]
async fn the_session_sync_pass_retries_a_failed_final_patch() {
    let _wd = test_work_dir();
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "答复。"),
    ]);
    let (_dir, app, _backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;
    platform
        .fail_update_transport_count
        .store(100, std::sync::atomic::Ordering::SeqCst);

    Turn::run(&app.turn_handles(), ctx("ses_test", "你好"))
        .await
        .unwrap();
    let card_id = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("a failed ending write keeps the record")
        .card_message_id
        .clone();
    assert!(app.core.feishu.has_pending_card_update(&card_id));

    platform
        .fail_update_transport_count
        .store(0, std::sync::atomic::Ordering::SeqCst);
    spawn_sync(&app);
    // The pass drain retries once the 5s backoff elapses; the reap then drops
    // the confirmed record — both observed here, or the test times out.
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if app.cards_handle().chains.get("ses_test").is_none()
                && !app.core.feishu.has_pending_card_update(&card_id)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the pass drain converges the card and the reap cleans the record");
}

/// ADR-0067's full restart acceptance: life 1's final PATCH fails and its
/// record survives on disk; a fresh process (the in-memory pending payload died
/// with life 1, as the ADR accepts) reaps the card from transcript truth and
/// drops the record.
#[tokio::test]
async fn a_restart_after_a_failed_final_patch_reaps_the_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "你好"),
        assistant(2_000, "答复。"),
    ]);

    // Life 1: the turn ends while every card write fails at the transport.
    let (app, platform, _backend) = turn_capable_app(&session_file, transcript.clone()).await;
    platform
        .fail_update_transport_count
        .store(100, std::sync::atomic::Ordering::SeqCst);
    Turn::run(&app.turn_handles(), ctx("ses_test", "你好"))
        .await
        .unwrap();
    let card_id = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("a failed ending write keeps the record")
        .card_message_id
        .clone();
    drop(app); // life 1 ends; the pending payload was memory-only (ADR-0067)

    // Life 2: a fresh app over the same files. Nothing in memory knows the
    // card; the reap settles it from the transcript and cleans the record.
    let (restarted, platform2, _backend2) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    assert_eq!(
        restarted
            .cards_handle()
            .chains
            .get("ses_test")
            .map(|record| record.card_message_id),
        Some(card_id.clone()),
        "the restart loaded life 1's record"
    );

    spawn_sync(&restarted);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let settled = platform2.calls.lock().await.iter().any(|call| match call {
                PlatformCall::UpdateMessage { message_id, card } => {
                    message_id == &card_id && card_header(card).contains("✅")
                }
                _ => false,
            });
            if settled && restarted.cards_handle().chains.get("ses_test").is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the reap settles the orphaned card and drops its record");
}

// ---------------------------------------------------------------------------
// The ended-while-down projection (spec #561, ticket #563): a record whose
// confirmed Rendered Cursor covers a delivered prefix, over a transcript that
// grew while cola was down, projects exactly the missed tail plus the true
// ending onto a successor card — never repeating the prefix, never rebuilding
// the old card.
// ---------------------------------------------------------------------------

/// Seed a record the previous life left with a confirmed Rendered Cursor: its
/// card's last delivered frontier and live set (spec #561). The next app
/// loads it at construction exactly like a real restart.
fn seed_cursor_record(
    session_file: &Path,
    card_message_id: &str,
    message_id: &str,
    created_ms: Option<i64>,
    directory: Option<&str>,
    frontier: Option<CursorFrontier>,
    live_calls: &[&str],
) {
    seed_chain_record(session_file, card_message_id, message_id, created_ms, directory);
    let cursor = RenderedCursor {
        frontier,
        live_calls: live_calls.iter().map(|call_id| call_id.to_string()).collect(),
    };
    ChainRecords::load(sidecar(session_file)).advance_cursor("ses_test", card_message_id, &cursor);
}

/// [`seed_cursor_record`] carrying the chain's durable reply target (issue
/// #580): the Feishu user message the chain answers. The projection replies
/// there first, then falls to the recorded card.
#[allow(clippy::too_many_arguments)] // the seed's whole fixture
fn seed_cursor_record_reply_to(
    session_file: &Path,
    card_message_id: &str,
    message_id: &str,
    created_ms: Option<i64>,
    directory: Option<&str>,
    reply_to: &str,
    frontier: Option<CursorFrontier>,
    live_calls: &[&str],
) {
    seed_cursor_record(
        session_file,
        card_message_id,
        message_id,
        created_ms,
        directory,
        frontier,
        live_calls,
    );
    // Re-track the same facts with the reply target: the store carries the
    // confirmed cursor across the re-point (spec #561), exactly as a live
    // chain's later track does.
    ChainRecords::load(sidecar(session_file)).track(
        "ses_test",
        card_message_id,
        MessageId::new(message_id),
        created_ms,
        directory,
        Some(reply_to),
    );
}

/// The cursor frontier of the fixture: a text part at `msg_a_2000` delivered
/// up to `delivered_chars`.
fn text_frontier(delivered: &str) -> CursorFrontier {
    CursorFrontier {
        message_id: MessageId::new("msg_a_2000"),
        part_index: 0,
        kind: CursorPartKind::Text,
        started_at: Some(2_000),
        delivered_chars: delivered.chars().count(),
        prefix_digest: Some(cursor_prefix_digest(delivered)),
    }
}

/// Wait for the projection's successor create — a reply carrying an ending
/// header — returning its card and text. Panics after 5 s.
async fn wait_for_projection(platform: &RecordingPlatform, reply_to: &str) -> (serde_json::Value, String) {
    let wait = async {
        loop {
            let calls = platform.calls.lock().await;
            if let Some(card) = calls.iter().find_map(|call| match call {
                PlatformCall::ReplyCard { reply_to: to, card } if to == reply_to => Some(card.clone()),
                _ => None,
            }) {
                let text = card_text(&card);
                return (card, text);
            }
            drop(calls);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .unwrap_or_else(|_| panic!("the projection never posted a successor for {reply_to}"))
}

/// The successor's header as issue #561 requires it: the same session (the
/// subtitle names the thread, `{title} · {id-tail}`) and the chain's date
/// anchor (`MM-DD`). The reply-target and content assertions alone would still
/// pass if either disappeared.
fn assert_successor_header(card: &serde_json::Value, session_title: &str, anchor_ms: i64) {
    let date = crate::feishu::card::fmt_local_date(anchor_ms).expect("the anchor formats a date");
    assert_eq!(
        card["header"]["subtitle"]["content"].as_str().unwrap_or_default(),
        format!("{session_title} · test · {date}"),
        "the successor names its session and carries the turn's header date: {card}"
    );
}

/// The headline acceptance (spec #561, ticket #563): a restart with a
/// delivered prefix and a longer transcript projects exactly the delta plus
/// the true ending onto a successor — character-level no-dup, no omission —
/// while the old card is collected as taken over, keeping its own body.
#[tokio::test]
async fn a_restart_projects_the_missed_tail_of_an_ended_run() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段是在停机期间写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The successor's session subtitle reads the server's title (issue #561):
    // a projection that skipped the read would land a bare id tail instead.
    backend
        .session_titles
        .lock()
        .unwrap()
        .insert("ses_test".to_string(), "项目甲".to_string());

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert_eq!(
        card_header(&successor),
        "✅ 完成",
        "the successor carries the transcript's true ending"
    );
    assert_successor_header(&successor, "项目甲", 1_000);
    assert_eq!(
        successor_text.matches(missed).count(),
        1,
        "the missed tail lands exactly once: {successor}"
    );
    assert!(
        !successor_text.contains(delivered),
        "the delivered prefix is never repeated: {successor}"
    );

    // The old card is collected as taken over, keeping its own body — never
    // rebuilt with the tail.
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    assert!(
        card_header(&collect).contains("已由新卡片接管"),
        "the old card is collected as taken over: {collect}"
    );
    let collect_text = card_text(&collect);
    assert!(
        collect_text.contains("**正文** 第一段"),
        "the collect keeps the body the old card already showed: {collect}"
    );
    assert!(
        !collect_text.contains(missed),
        "the old card is never rebuilt with the missed tail: {collect}"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_none(),
        "a terminal successor spends the record once its create is confirmed"
    );

    // Later passes never replay: the successor's cursor covers the read, so
    // the Wake step's content diff owes nothing.
    let posts = card_posts(&platform).await;
    let reads = backend.transcript_calls.lock().await.len();
    wait_for_transcript_reads(&backend, "ses_test", reads + 2).await;
    assert_eq!(
        card_posts(&platform).await,
        posts,
        "no second successor after the projection: {:?}",
        platform.calls.lock().await
    );
}

/// Issue #580: the projection's successor replies to the chain's durable
/// reply target — the Feishu user message the chain answers — never the
/// OpenCode message id the record scopes its settle decision with. Live,
/// Feishu answered 400 (`not a valid open_message_id`) for the `msg_cola_*`
/// anchor and the projection retried the same id forever.
#[tokio::test]
async fn a_projection_replies_to_the_chains_durable_reply_target() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "停机期间写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record_reply_to(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        "om_user_msg",
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_user_msg").await;

    assert_eq!(card_header(&successor), "✅ 完成");
    assert!(successor_text.contains(missed), "{successor_text}");
    let anchored = platform.calls.lock().await.iter().any(|call| {
        matches!(
            call,
            PlatformCall::ReplyCard { reply_to, .. } if reply_to == "msg_cola_anchor"
        )
    });
    assert!(
        !anchored,
        "the OpenCode message id is never used as a Feishu reply target: {:?}",
        platform.calls.lock().await
    );
}

/// Issue #580: a target the platform refuses DEFINITIVELY (a 4xx — a
/// withdrawn message id, say) falls through to the next target — the
/// recorded card — instead of pinning the projection to it forever. The
/// successor still lands, exactly once.
#[tokio::test]
async fn a_projection_falls_back_past_a_refused_reply_target() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "停机期间写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record_reply_to(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        "om_withdrawn",
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The durable target is gone: Feishu answers a definite 400 for it.
    platform.given_reply_card_outcome(ReplyOutcome::Refused(400));

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert_eq!(card_header(&successor), "✅ 完成");
    assert!(successor_text.contains(missed), "{successor_text}");
    assert_eq!(
        platform
            .calls
            .lock()
            .await
            .iter()
            .filter(|call| matches!(
                call,
                PlatformCall::ReplyCard { reply_to, .. } if reply_to == "om_frozen"
            ))
            .count(),
        1,
        "one fallback successor, never a duplicate: {:?}",
        platform.calls.lock().await
    );
}

/// Issue #580: when every reply target is refused definitively, the Chat is
/// the ladder's last rung — the successor lands as a top-level message
/// instead of the projection giving up.
#[tokio::test]
async fn a_projection_falls_back_to_the_chat_when_every_reply_target_is_refused() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "停机期间写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record_reply_to(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        "om_withdrawn",
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Both reply targets are gone: definite 400s for each.
    platform.given_reply_card_outcome(ReplyOutcome::Refused(400));
    platform.given_reply_card_outcome(ReplyOutcome::Refused(400));

    spawn_sync(&app);
    // The successor lands at the Chat's top level.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let successor = loop {
        if let Some(card) = platform
            .sent_cards()
            .await
            .iter()
            .find(|card| card_text(card).contains(missed))
        {
            break card.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no top-level fallback successor: {:?}",
            platform.calls.lock().await
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(card_header(&successor), "✅ 完成");
}

/// #582: a projection whose create lands at the Chat's top level clears the
/// chain record's durable reply target in the SAME record write as the
/// re-point — no separate clear write a crash could lose, leaving the refused
/// target persisted. The Waiting settle keeps the record (#583), so the
/// persisted fact is observable deterministically.
#[tokio::test]
async fn a_top_level_projection_landing_clears_the_reply_target_in_one_write() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record_reply_to(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        "om_withdrawn",
        Some(text_frontier(delivered)),
        &[],
    );
    // The turn idles with a live Background Task (a Waiting settle): the
    // projected successor yields ⏳ and keeps its record (#583), so this test
    // can read the record the landing wrote.
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, delivered),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Both reply rungs are gone definitively: the Chat is the landing rung.
    platform.given_reply_card_outcome(ReplyOutcome::Refused(400));
    platform.given_reply_card_outcome(ReplyOutcome::Refused(400));

    spawn_sync(&app);
    // The top-level successor lands ⏳ and the landing's own write clears the
    // refused target. A carry would leave `om_withdrawn` on the record.
    let probe = async {
        loop {
            let landed = platform
                .sent_cards()
                .await
                .iter()
                .any(|card| card_header(card).contains("等待后台任务"));
            let cleared = app
                .cards_handle()
                .chains
                .get("ses_test")
                .is_some_and(|record| record.reply_to.is_none());
            if landed && cleared {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let observed = tokio::time::timeout(Duration::from_secs(5), probe).await.is_ok();
    assert!(
        observed,
        "the waiting successor never landed with the target cleared: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a waiting landing keeps its record"
    );
}

/// A tail cut inside a code fence (spec #561, ticket #563): the successor's
/// markdown reopens the fence, so the missed code stays code and the text
/// after the original closer is not swallowed.
#[tokio::test]
async fn a_projection_cut_inside_a_fence_renders_intact() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "说明\n```python\nprint(1)\n";
    let missed = "print(2)\n```\n后的文字";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;

    spawn_sync(&app);
    let (_successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert!(
        successor_text.contains("```\nprint(2)\n```\n后的文字"),
        "the cut tail reopens the fence and keeps the after-text intact: {successor_text}"
    );
    assert!(
        !successor_text.contains("print(1)"),
        "the delivered fence prefix is not repeated: {successor_text}"
    );
}

/// A tool the old card showed running (the cursor's live set) and which
/// settled while cola was down shows its result on the successor exactly
/// once, and the collected old card loses the frozen `⏳` panel (spec #561,
/// ticket #563).
#[tokio::test]
async fn a_projection_settles_a_tool_that_finished_while_cola_was_down() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier("第一段回答。")),
        &["call_1"],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, "第一段回答。"),
        tool_assistant(3_000, ToolStatus::Completed, "done"),
    ])
    .with_executions(vec![execution(3_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    // The old card showed a running `⏳ bash` panel; the successor resolves it.
    platform.given_card_view(
        "om_frozen",
        serde_json::json!({
            "schema": "2.0",
            "config": { "wide_screen_mode": true },
            "header": { "template": "blue",
                        "title": { "tag": "plain_text", "content": "✍️ 回复中" } },
            "body": { "elements": [
                { "tag": "markdown", "content": "**正文** 第一段" },
                { "tag": "collapsible_panel", "expanded": false, "element_id": "tool_call_1",
                  "header": { "title": { "tag": "plain_text", "content": "⏳ bash" } },
                  "elements": [ { "tag": "markdown", "content": "还在跑" } ] }
            ] }
        }),
    );

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert_eq!(card_header(&successor), "✅ 完成");
    assert_eq!(
        successor_text.matches("done").count(),
        1,
        "the settled result lands exactly once: {successor}"
    );
    assert!(
        !successor_text.contains("⏳"),
        "no frozen running marker on the successor: {successor}"
    );
    assert!(
        !successor_text.contains("第一段回答。"),
        "the delivered prefix is not repeated: {successor}"
    );

    // The collected old card drops the running panel the successor resolved,
    // keeping its display body.
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    let collect_text = card_text(&collect);
    assert!(
        collect_text.contains("**正文** 第一段"),
        "the collect keeps the old body: {collect}"
    );
    assert!(
        !collect_text.contains("⏳ bash") && !collect_text.contains("还在跑"),
        "the resolved running panel leaves the collected card: {collect}"
    );
}

/// A Waiting successor (live Background Tasks) keeps its record with the
/// cursor its confirmed create carried (spec #561, ticket #563): a restart
/// after the yield finds the whole body already delivered and cannot project
/// the same tail again.
#[tokio::test]
async fn a_projection_confirm_advances_the_cursor_it_was_confirmed_on() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段是在停机期间写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert_eq!(card_header(&successor), "⏳ 等待后台任务");
    assert!(successor_text.contains(missed), "{successor}");
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("a waiting successor keeps its record");
    assert_eq!(
        record.card_message_id, "msg_reply",
        "the record follows the successor the projection created"
    );
    assert!(
        record.waiting_reaped,
        "the projected yield is marked so no later pass projects it again"
    );
    assert_eq!(
        record
            .cursor
            .as_ref()
            .and_then(|cursor| cursor.frontier.as_ref())
            .map(|frontier| frontier.delivered_chars),
        Some(full.chars().count()),
        "the confirmed create advanced the cursor over the whole body"
    );
}

/// Issue #583: a restart whose Waiting settle finds the cursor covering the
/// whole read — the run idled on a live Background Task with nothing missed —
/// still projects a successor. The wait's card is the one the task's
/// completion Wake resumes in place (ADR-0066), so an empty delta must not
/// drop it: the successor posts the standard waiting header plus the
/// Background Task Ledger, replays nothing, and the old card is collected as
/// taken over. The record re-points at the successor with its waiting mark.
#[tokio::test]
async fn a_restart_projects_a_waiting_successor_even_when_the_delta_is_empty() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, delivered),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, waiting, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert_eq!(
        card_header(&successor),
        "⏳ 等待后台任务",
        "the empty-delta waiting projection keeps the wait's header"
    );
    assert!(
        successor_text.contains("⏳ 后台任务（"),
        "the successor carries the Background Task Ledger: {successor}"
    );
    assert!(
        !successor_text.contains(delivered),
        "the fully delivered prefix is never replayed: {successor}"
    );

    // The old card is collected as taken over...
    wait_for_update(&platform, "om_frozen", "the takeover collect", |card| {
        card_header(card).contains("已由新卡片接管")
    })
    .await;
    // ...and the record follows the successor, marked so no later pass
    // projects the same wait again.
    wait_for_record_card(&app, "ses_test", "msg_reply").await;
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("a waiting successor keeps its record");
    assert!(
        record.waiting_reaped,
        "the projected yield is marked so no later pass projects it again"
    );
}

/// Issue #583: the empty-delta waiting successor is a LIVE card, not a stamped
/// orphan — the retiring task's completion Wake resumes it IN PLACE (ADR-0066),
/// so the fixed completion entry, the resumed work and the true ending all
/// land on the same card, with no second post.
#[tokio::test]
async fn a_completion_wake_resumes_the_projected_waiting_successor_in_place() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, delivered),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, waiting, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    let (successor, _) = wait_for_projection(&platform, "om_frozen").await;
    assert_eq!(card_header(&successor), "⏳ 等待后台任务");
    let posts = card_posts(&platform).await;

    // The task retires and its completion Wake resumes the run: the resumed
    // work and the execution boundary arrive while cola watches.
    script_transcript(
        &backend,
        vec![
            SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "跑一下 CI"),
                assistant(2_000, delivered),
                assistant(3_100, "CI 通过了。"),
            ])
            .with_executions(vec![execution(2_500), execution(4_000)])
            .with_wakes(vec![shell_wake(2_900)]),
        ],
    )
    .await;
    wait_for_update(
        &platform,
        "msg_reply",
        "the resumed successor's true end",
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;

    let resumed = last_update_of(&platform, "msg_reply")
        .await
        .expect("the successor is resumed in place");
    assert_eq!(
        card_text(&resumed).matches("🔔 shell 完成：gh run watch").count(),
        1,
        "the retiring task's fixed entry lands on the successor exactly once: {resumed}"
    );
    assert!(
        !card_text(&resumed).contains("⏳ 后台任务（"),
        "the retired task's live row is gone: {resumed}"
    );
    assert_eq!(
        card_posts(&platform).await,
        posts,
        "an in-place resume posts no second card: {:?}",
        platform.calls.lock().await
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "an in-place resume writes no 承接 card: {:?}",
        platform.calls.lock().await
    );
}

/// The ended twin of the empty-delta rule (issue #583): a cursor covering the
/// whole read over an ENDED run still drops its armed successor and PATCHes
/// the old card's ending in place — content decides only the terminal case.
#[tokio::test]
async fn an_ended_projection_with_an_empty_delta_still_settles_in_place() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写完的回答。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let ended = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, delivered),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, ended, Some(SessionStatus::Idle)).await;

    spawn_sync(&app);
    wait_for_update(&platform, "om_frozen", "the in-place ✅ ending", |card| {
        card_header(card).contains("✅")
    })
    .await;

    assert_eq!(
        card_posts(&platform).await,
        0,
        "an ended empty-delta projection posts no successor: {:?}",
        platform.calls.lock().await
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// The accepted per-restart cost (issue #583, ADR-0071 amendment): the waiting
/// mark never survives a restart, so a second restart during the same wait
/// projects its own successor and collects the previous one — exactly what the
/// live adoption costs per restart.
#[tokio::test]
async fn each_restart_during_a_wait_projects_its_own_successor() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let waiting = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, delivered),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)]);

    // Life 1: the first restart projects its successor.
    let (app1, platform1, _backend1) =
        restarted_app_with_backend(&session_file, waiting.clone(), Some(SessionStatus::Idle)).await;
    platform1.given_card_view("om_frozen", realistic_card_view());
    platform1.given_reply_id("om_successor_1");
    spawn_sync(&app1);
    let (successor1, _) = wait_for_projection(&platform1, "om_frozen").await;
    assert_eq!(card_header(&successor1), "⏳ 等待后台任务");
    wait_for_record_card(&app1, "ses_test", "om_successor_1").await;
    drop(app1); // life 1 ends

    // Life 2: the same wait, still un-retired — and the in-memory waiting mark
    // is gone with life 1, so the projection runs again onto its own successor.
    let (app2, platform2, _backend2) =
        restarted_app_with_backend(&session_file, waiting, Some(SessionStatus::Idle)).await;
    platform2.given_card_view("om_successor_1", realistic_card_view());
    platform2.given_reply_id("om_successor_2");
    spawn_sync(&app2);
    let (successor2, _) = wait_for_projection(&platform2, "om_frozen").await;
    assert_eq!(card_header(&successor2), "⏳ 等待后台任务");
    wait_for_record_card(&app2, "ses_test", "om_successor_2").await;

    // The first successor is collected as taken over by the second, exactly
    // like any re-projection's predecessor.
    wait_for_update(&platform2, "om_successor_1", "the takeover collect", |card| {
        card_header(card).contains("已由新卡片接管")
    })
    .await;
}

/// A create that fails advances nothing (spec #561, tickets #563/#566): the
/// staged Wake Watermark stays unannounced — only a confirmed write advances
/// it — and the old card is never collected. The attempt is single-shot
/// (review #569): later passes never re-post; the reap settles the old card in
/// place by transcript truth instead.
#[tokio::test]
async fn a_failed_projection_create_advances_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段是在停机期间写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    // The read carries a Wake the projection would announce: the failed write
    // must not.
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    // Every successor create fails at the platform.
    platform
        .fail_reply_card_count
        .store(100, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    // The next pass never re-posts: it settles the old card in place by
    // transcript truth.
    wait_for_update(&platform, "om_frozen", "the in-place ending", |card| {
        card_header(card).contains("✅")
    })
    .await;
    assert_eq!(
        app.cards_handle().chains.announced("ses_test"),
        None,
        "a failed create announces nothing: only a confirmed write advances the Watermark"
    );
    assert_eq!(
        card_posts(&platform).await,
        0,
        "the failed projection never posts a successor: {:?}",
        platform.calls.lock().await
    );
    let patches = patches_to(&platform, "om_frozen").await;
    assert!(
        patches
            .iter()
            .all(|card| !card_header(card).contains("已由新卡片接管")),
        "the old card is settled in place, never collected: {patches:?}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A cursorless record follows today's settle path (spec #561, ticket #563):
/// the transcript's ending is PATCHed onto the recorded card in place, and no
/// successor is ever created.
#[tokio::test]
async fn a_cursorless_record_keeps_todays_in_place_settle() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    let delivered = "答复。";
    let tail = "补上的尾巴。";
    let transcript = || {
        SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant(2_000, &format!("{delivered}{tail}")),
        ])
        .with_executions(vec![execution(2_500)])
    };
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript(), Some(SessionStatus::Idle)).await;

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
    assert_eq!(
        card_posts(&platform).await,
        0,
        "a cursorless record never projects a successor: {:?}",
        platform.calls.lock().await
    );
    // The projection transition's first act is the successor's session-subtitle
    // read (spec #561): a cursorless record must never take that path at all —
    // not even one that would render nothing and fall back to the settle.
    assert!(
        backend.session_info_calls.lock().await.is_empty(),
        "the projection path is not taken for a cursorless record: {:?}",
        backend.session_info_calls.lock().await
    );
    // The transcript's undelivered tail never reaches a card: only the settle
    // ran. A record that took the projection path would have posted it.
    let seen: String = platform
        .updated_cards()
        .await
        .iter()
        .map(card_text)
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(
        !seen.contains(tail),
        "the cursorless fallback renders no transcript content: {seen}"
    );
    assert!(app.cards_handle().chains.get("ses_test").is_none());
    drop(app);
    drop(backend);

    // Positive control (spec #561, review #569): the SAME transcript with a
    // cursor-carrying record DOES project — so this test fails if the
    // projection path is reverted, not just if the cursorless fallback leaks
    // into it. The cursor covers the delivered prefix; the tail is undelivered.
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let (app2, platform2, _backend2) =
        restarted_app_with_backend(&session_file, transcript(), Some(SessionStatus::Idle)).await;
    platform2.given_card_view("om_frozen", realistic_card_view());
    spawn_sync(&app2);
    wait_for_posted_text(&platform2, tail).await;
}

/// A cursor-carrying record that names no card and has no Chat mapping has no
/// deliverable target at all (issue #580): the OpenCode message id is not a
/// Feishu target, so nothing is projected and nothing is settled — the record
/// claims nothing rather than PATCHing an empty card id.
#[tokio::test]
async fn a_record_with_no_deliverable_target_claims_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段。";
    // The previous life recorded no card and never captured an anchor; the
    // submitted message did land later, so the transcript alone decides the
    // ending (Complete).
    seed_cursor_record(
        &session_file,
        "",
        "msg_cola_anchor",
        None,
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &format!("{delivered}{missed}")),
    ])
    .with_executions(vec![execution(2_500)]);
    // NO session mapping either: no recorded card, no Chat — and an OpenCode
    // message id is never a deliverable Feishu target.
    let (app, platform, backend) = restarted_app_unmapped(&session_file, transcript).await;

    spawn_sync(&app);
    // Let several passes run: the record claims nothing, forever — no
    // projection create, no empty-id settle PATCH.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            platform.replied_cards().await.is_empty(),
            "no projection without a deliverable target: {:?}",
            platform.calls.lock().await
        );
        if backend.transcript_calls.lock().await.len() >= 3 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reap never observed the targetless record"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        patches_to(&platform, "").await.is_empty(),
        "nothing ever PATCHes an empty card id: {:?}",
        platform.calls.lock().await
    );
}

// ---------------------------------------------------------------------------
// The live-run adoption (spec #561, ticket #564): a record whose run is still
// running is followed from its confirmed cursor — the successor streams what
// the run produces after the seed and settles by transcript truth.
// ---------------------------------------------------------------------------

/// An assistant message still in flight: content without a terminal
/// `step-finish`, so its Turn reads as running. The part carries the message's
/// server time, like the server's own reads (and the `text_frontier`
/// fixtures): the Rendered Cursor frontier's identity includes it.
fn assistant_in_flight(created: i64, text: &str) -> TranscriptMessage {
    typed_message(
        &format!("msg_a_{created}"),
        MessageRole::Assistant,
        Some(created),
        vec![Part::Text(TextPart {
            text: text.to_string(),
            started_at: Some(created),
        })],
    )
}

/// Wait until `message_id` has a recorded update satisfying `check`, or panic
/// after 5 s. The follow's writes land out of turn, so a live-adoption test
/// waits on the card, not on a handle.
async fn wait_for_update(
    platform: &RecordingPlatform,
    message_id: &str,
    label: &str,
    check: impl Fn(&serde_json::Value) -> bool,
) {
    let probe = async {
        loop {
            if patches_to(platform, message_id).await.iter().any(&check) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("{message_id} never reached {label}"));
}

/// Wait until the session's record is gone (a terminal settle released it),
/// or panic after 5 s.
async fn wait_for_record_gone(app: &Arc<App>, session_id: &str) {
    let probe = async {
        loop {
            if app.cards_handle().chains.get(session_id).is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("the record was never released"));
}

/// Wait until the session's durable Wake Watermark covers `created_ms`, or
/// panic after 5 s.
async fn wait_for_announced(app: &Arc<App>, session_id: &str, created_ms: i64) {
    let probe = async {
        loop {
            if app
                .cards_handle()
                .chains
                .announced(session_id)
                .is_some_and(|mark| mark.created_ms >= created_ms)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("the Wake Watermark never reached {created_ms}"));
}

/// The headline acceptance (spec #561, ticket #564): a restart while the run
/// is still live arms a successor at the confirmed cursor and FOLLOWS the run
/// — content produced after the seed lands on the successor, the old card is
/// collected as taken over, and the run settles by transcript truth (✅) with
/// the missed tail exactly once.
#[tokio::test]
async fn a_restart_mid_run_follows_the_live_run_onto_a_successor() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段是重启后继续写出来的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    // The run is live and has produced nothing past the confirmed cursor yet.
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, delivered),
    ]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    // The successor is a live continuation — a working card, not a stamped
    // orphan — and the delivered prefix is never repeated.
    assert!(
        card_header(&successor).contains("回复中"),
        "the successor is a working card: {successor}"
    );
    assert!(
        !successor_text.contains(delivered),
        "the delivered prefix is never repeated: {successor}"
    );
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    assert!(
        card_header(&collect).contains("已由新卡片接管"),
        "the old card is collected as taken over: {collect}"
    );
    assert_eq!(
        card_posts(&platform).await,
        1,
        "the successor's create is the one send — the restart notification: {:?}",
        platform.calls.lock().await
    );
    wait_for_record_card(&app, "ses_test", "msg_reply").await;

    // The run keeps producing: the follow streams the growth onto the
    // successor (the reply's recorded card id).
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant_in_flight(2_000, &full),
            ])],
        )
        .await;
    wait_for_update(&platform, "msg_reply", "the missed tail", |card| {
        card_text(card).contains(missed)
    })
    .await;

    // The transcript's true ending settles it: ✅ on the successor, the tail
    // exactly once, and the record spent.
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant(2_000, &full),
            ])],
        )
        .await;
    wait_for_update(&platform, "msg_reply", "the ✅ ending", |card| {
        card_header(card).contains("✅")
    })
    .await;
    let settled = patches_to(&platform, "msg_reply").await;
    let settled = settled.last().expect("the successor was patched");
    let settled_text = card_text(settled);
    assert_eq!(
        settled_text.matches(missed).count(),
        1,
        "the tail lands exactly once: {settled}"
    );
    assert!(
        !settled_text.contains(delivered),
        "the delivered prefix is never repeated: {settled}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A tool still running across the restart rides the successor as a live carry
/// and joins its timeline exactly once at settle; the collected old card keeps
/// no frozen running marker (spec #561, ticket #564).
#[tokio::test]
async fn a_restart_mid_run_carries_a_running_tool_and_settles_it_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &["call_1"],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, delivered),
        tool_assistant(3_000, ToolStatus::Running, ""),
    ]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", running_panel_card_view("call_1"));

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert!(
        successor_text.contains("⏳ bash"),
        "the still-running call is carried live onto the successor: {successor}"
    );
    assert!(
        !successor_text.contains(delivered),
        "the delivered prefix is never repeated: {successor}"
    );
    // The old card is collected as taken over and drops the running marker the
    // successor resolves — no collected card looks busy.
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    assert!(
        !card_text(&collect).contains("⏳ bash") && !card_text(&collect).contains("还在跑"),
        "the resolved running panel leaves the collected card: {collect}"
    );
    assert!(
        card_text(&collect).contains("**正文** 已经写了一半。"),
        "{collect}"
    );

    // The call settles: the carry joins the successor's timeline exactly once.
    settle_tool(&backend, ToolStatus::Completed, "done").await;
    wait_for_update(&platform, "msg_reply", "the settled tool result", |card| {
        card_text(card).contains("done")
    })
    .await;

    // The run's true ending settles it, with the settled panel exactly once.
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant_in_flight(2_000, delivered),
                tool_assistant(3_000, ToolStatus::Completed, "done"),
                assistant(4_000, "收尾。"),
            ])],
        )
        .await;
    wait_for_update(&platform, "msg_reply", "the ✅ ending", |card| {
        card_header(card).contains("✅")
    })
    .await;
    let settled = patches_to(&platform, "msg_reply").await;
    let settled = settled.last().expect("the successor was patched");
    let settled_text = card_text(settled);
    assert_eq!(
        settled_text.matches("done").count(),
        1,
        "the settled tool joins the timeline exactly once: {settled}"
    );
    assert!(
        !settled_text.contains('⏳'),
        "no frozen running marker on the settled successor: {settled}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A tool that settled while cola was down shows its result on the successor
/// exactly once — the live-status twin of the ended projection's carry — and
/// the collected old card loses its frozen running marker (spec #561, ticket
/// #564).
#[tokio::test]
async fn a_restart_mid_run_settles_a_tool_that_finished_while_cola_was_down() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &["call_1"],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, delivered),
        tool_assistant(3_000, ToolStatus::Completed, "done"),
    ]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", running_panel_card_view("call_1"));

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;

    assert_eq!(
        successor_text.matches("done").count(),
        1,
        "the result that landed while cola was down shows exactly once: {successor}"
    );
    assert!(
        !successor_text.contains('⏳'),
        "no frozen running marker on the successor: {successor}"
    );
    assert!(
        !successor_text.contains(delivered),
        "the delivered prefix is never repeated: {successor}"
    );
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    assert!(
        !card_text(&collect).contains("⏳ bash"),
        "the resolved running panel leaves the collected card: {collect}"
    );
}

/// A Wake whose work a live adoption's successor rendered is announced by that
/// successor's confirmed create too (spec #561, ticket #566; ADR-0061): the
/// Watermark advances with the same staged drain the flush uses, so the
/// announcement is durable even before the follow's first own write.
#[tokio::test]
async fn a_live_adoption_announces_the_wake_its_successor_rendered() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经交给后台了。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    // The run is still live and the Wake resumed it: the successor's first
    // send renders the Wake's completion entry and its resumed work.
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant_in_flight(2_000, delivered),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // No PATCH can confirm: every update fails at the transport, so the
    // adoption's own create is the only write that can carry the announcement
    // to the record.
    platform
        .fail_update_transport_count
        .store(100, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;
    assert!(
        card_header(&successor).contains("回复中"),
        "the adoption follows the live run: {successor}"
    );
    assert!(successor_text.contains("CI 通过了。"), "{successor}");
    wait_for_announced(&app, "ses_test", 2_900).await;
}

/// The persisted interactive-surface record's raw JSON, or `None` when the
/// file is gone (the surfaces test's own read).
fn persisted_surfaces(session_file: &Path) -> Option<String> {
    std::fs::read_to_string(session_file.with_file_name("interactive_surfaces.json")).ok()
}

/// The adoption and the pending-request migration are one sequence (spec #561,
/// ticket #564 + the Interaction Receipts boundary): a restart with a
/// cursor-carrying record and a still-pending permission whose surface a
/// previous life persisted adopts the live run onto a successor, and the
/// existing re-adoption/re-host sweep lands the permission's controls on that
/// successor within its ≤2-sweep bound — while the collected old card keeps
/// the takeover presentation, never a stale rehost repaint that would
/// resurrect the pre-collect card.
#[tokio::test]
async fn an_adoption_carries_a_pending_request_onto_the_successor() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";

    // Life 1: the permission inlined on the live card, the surface persisted
    // beside the session store.
    {
        let mut backend = MockBackend::new(realistic_parts());
        backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
        let backend = Arc::new(backend);
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(
            App::new(test_config(&session_file), backend.clone(), platform.clone()).expect("life 1 builds"),
        );
        seed_session(&app, "ses_test", "/work").await;
        let cards = app.cards_handle();
        Turn::seed_card(&cards, "ses_test", Some("om_frozen")).await;
        Turn::set_card_state(&cards, "ses_test", CardState::Streaming).await;
        Turn::push_text(&cards, "ses_test", delivered).await;
        Turn::set_reply_target(&cards, "ses_test", "msg_1").await;

        let mut seen = std::collections::HashSet::new();
        app.permission.sweep(&app.flow_handles(), &mut seen).await;
        assert_eq!(
            app.card_handles.lock().await.message_of("per_1"),
            Some("om_frozen"),
            "precondition: life 1 inlined the permission on the live card"
        );
        let raw = persisted_surfaces(&session_file).expect("the surface is persisted");
        assert!(
            raw.contains("per_1") && raw.contains("om_frozen"),
            "the block and its card are in the record: {raw}"
        );
    }

    // The chain record the previous life's confirmed write left behind: its
    // cursor names the same card and the delivered prefix.
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );

    // Life 2: the same record and surface, the run still live.
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(perm_request("per_1", "ses_test", "ls -la"));
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant_in_flight(2_000, delivered),
        ])],
    );
    backend.with_session_status("ses_test", Some(SessionStatus::Busy));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(&session_file), backend.clone(), platform.clone())
            .expect("the restarted app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    platform.given_card_view("om_frozen", running_panel_card_view("call_1"));

    spawn_sync(&app);
    let (_successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;
    assert!(
        !successor_text.contains(delivered),
        "the adoption seeds past the delivered prefix: {successor_text}"
    );
    // The collect lands before the sweeps, so their own writes are the only
    // thing between the old card and the assertions.
    wait_for_update(&platform, "om_frozen", "the takeover collect", |card| {
        card_header(card).contains("已由新卡片接管")
    })
    .await;
    // ...and its cache release lands with it: the old card's cached JSON is
    // dropped so no later edit may repaint the pre-collect presentation.
    let released = async {
        loop {
            if app.card_handles.lock().await.cached_card("om_frozen").is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), released)
        .await
        .expect("the collect released the old card's stale handle cache");

    // The migration sequence within the sweep bound: at most two permission
    // sweeps from the adopted successor (the first re-adopts or inlines, the
    // second re-hosts).
    let mut seen = std::collections::HashSet::new();
    app.permission.sweep(&app.flow_handles(), &mut seen).await;
    app.permission.sweep(&app.flow_handles(), &mut seen).await;

    assert_eq!(
        app.card_handles.lock().await.message_of("per_1"),
        Some("msg_reply"),
        "the block's handle follows the successor within the bound"
    );
    let successor = patches_to(&platform, "msg_reply")
        .await
        .iter()
        .rev()
        .find(|card| card_text(card).contains("🔐 **权限请求**"))
        .cloned()
        .expect("the successor carries the permission's controls");
    assert!(
        card_text(&successor).contains("允许一次"),
        "the controls are live on the successor: {successor}"
    );

    // The old card's last word is the takeover collect: the controls left it,
    // and the collected presentation stays — a later rehost strip must never
    // rewrite the pre-collect card from the stale handle cache (spec #561:
    // no collected card looks live).
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card was collected");
    assert!(
        card_header(&collect).contains("已由新卡片接管"),
        "the old card's last write stays the takeover collect: {collect}"
    );
    assert!(
        !card_text(&collect).contains("🔐 **权限请求**"),
        "the controls left the collected card: {collect}"
    );
    assert!(
        card_text(&collect).contains("**正文** 已经写了一半。"),
        "the collect keeps the body the old card already showed: {collect}"
    );

    // Answerable on the successor: the click resolves the request and leaves
    // its receipt there.
    let result = app
        .host_action(serde_json::json!({
            "action": "perm",
            "reply": "once",
            "session_id": "ses_test",
            "directory": "/work",
            "request_id": "per_1",
            "open_message_id": "msg_reply",
            "perm_label": "✅ 已允许一次",
            "perm_color": "green",
            "perm_body": "bash",
        }))
        .await
        .expect("a card-action result");
    let ack = card_text(result.card.as_ref().expect("the ack carries the card"));
    assert!(
        ack.contains("✅ 已允许一次"),
        "the request resolves from the successor: {ack}"
    );
    assert_eq!(backend.reply_permission_calls.lock().await.len(), 1);
}

/// A run that dies with the server settles by transcript truth (spec #561,
/// ticket #564): the follow needs no status read, so the transcript's own
/// ending still lands — with no invented interruption anywhere.
#[tokio::test]
async fn a_run_that_dies_with_the_server_settles_by_transcript_truth() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段是在服务器结束前写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, delivered),
    ]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    wait_for_projection(&platform, "om_frozen").await;

    // The server goes away: its status reads fail from here on, while the
    // transcript (the follow's one read) carries the run's true end.
    backend
        .session_status_fails
        .store(true, std::sync::atomic::Ordering::SeqCst);
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant(2_000, &full),
            ])],
        )
        .await;
    wait_for_update(&platform, "msg_reply", "the ✅ ending", |card| {
        card_header(card).contains("✅")
    })
    .await;

    let settled = patches_to(&platform, "msg_reply").await;
    let settled = settled.last().expect("the successor was patched");
    let settled_text = card_text(settled);
    assert_eq!(
        settled_text.matches(missed).count(),
        1,
        "the tail that landed before the server died shows exactly once: {settled}"
    );
    assert!(
        !settled_text.contains("重启"),
        "no invented interruption on the successor: {settled}"
    );
    // The old card's last word is the takeover collect, never a restart stamp.
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    assert!(
        card_header(&collect).contains("已由新卡片接管"),
        "the old card is collected as taken over: {collect}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A wedged read is no ending (spec #561, ticket #564): while the transcript
/// read fails, the follow keeps polling its idle grace — the successor stays
/// live, the old card keeps its takeover collect, and no restart stamp is
/// invented — and the run still settles by transcript truth once the read
/// heals.
#[tokio::test]
async fn a_live_adoption_keeps_following_through_failed_reads() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, delivered),
    ]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    wait_for_projection(&platform, "om_frozen").await;
    let collects = patches_to(&platform, "om_frozen").await.len();

    // The transcript read starts failing: the follow keeps polling without
    // inventing an ending, and the old card is never stamped.
    backend.fail_transcript_for("ses_test").await;
    let reads = backend.transcript_calls.lock().await.len();
    wait_for_transcript_reads(&backend, "ses_test", reads + 3).await;
    assert!(
        patches_to(&platform, "msg_reply")
            .await
            .iter()
            .all(|card| { !card_header(card).contains("✅") && !card_header(card).contains("出错") }),
        "a failed read never settles the successor: {:?}",
        patches_to(&platform, "msg_reply").await
    );
    assert_eq!(
        patches_to(&platform, "om_frozen").await.len(),
        collects,
        "the old card keeps its takeover collect, never a restart stamp"
    );

    // The read heals with the run's true end: the successor settles.
    backend.heal_transcript("ses_test").await;
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant(2_000, &full),
            ])],
        )
        .await;
    wait_for_update(&platform, "msg_reply", "the ✅ ending", |card| {
        card_header(card).contains("✅")
    })
    .await;
    let settled = patches_to(&platform, "msg_reply").await;
    let settled = settled.last().expect("the successor was patched");
    assert_eq!(card_text(settled).matches(missed).count(), 1, "{settled}");
    wait_for_record_gone(&app, "ses_test").await;
}

/// A create this process life already attempted is single-shot (spec #561,
/// review #569): Feishu has no idempotency key (ADR-0067), so the ambiguous
/// failure is never retried — a retry could post a duplicate successor. The
/// record stays (a live run keeps observing), the old card is never stamped or
/// collected, and when the run ends the reap state-repairs it in place.
#[tokio::test]
async fn an_ambiguous_adoption_create_is_never_retried() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, delivered),
    ]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The first successor create fails; a retry would succeed — exactly the
    // duplicate a single-shot attempt must never post.
    platform
        .fail_reply_card_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    // One pass ran (the attempt happened): the record keeps the old card.
    let reads = backend.session_status_reads.lock().await.len();
    wait_for_status_reads(&backend, "ses_test", reads + 1).await;

    // Later passes must not re-post: a window of ticks proves the attempt is
    // single-shot (a retry would land on the very next one).
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        card_posts(&platform).await,
        0,
        "an ambiguous create is never repeated: {:?}",
        platform.calls.lock().await
    );

    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("a failed adoption keeps the record");
    assert_eq!(
        record.card_message_id, "om_frozen",
        "the record still names the old card"
    );
    assert_eq!(
        record.cursor,
        Some(RenderedCursor {
            frontier: Some(text_frontier(delivered)),
            live_calls: Default::default(),
        }),
        "a failed create advanced no cursor"
    );
    assert_eq!(
        patches_to(&platform, "om_frozen").await.len(),
        0,
        "a failed adoption never stamps or collects the old card"
    );
    // The attempt's write-ahead intent is durable (spec #561, review #569): a
    // crash between the create and the re-point leaves it behind, so the next
    // life treats the create as ambiguous too.
    let persisted =
        crate::bridge::chain::ChainRecords::load(session_file.with_file_name("chain_records.json"));
    assert!(
        persisted
            .get("ses_test")
            .expect("the failed attempt keeps the record")
            .projection_intent,
        "the ambiguous create leaves its durable write-ahead intent"
    );

    // The run ends: the reap settles the old card in place, still no successor.
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![
                SessionTranscript::new(vec![
                    user("msg_cola_anchor", 1_000, "问题"),
                    assistant(2_000, delivered),
                ])
                .with_executions(vec![execution(2_500)]),
            ],
        )
        .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    wait_for_update(&platform, "om_frozen", "the in-place ending", |card| {
        card_header(card).contains("✅")
    })
    .await;
    assert_eq!(
        card_posts(&platform).await,
        0,
        "no successor is ever posted after the ambiguous create: {:?}",
        platform.calls.lock().await
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A record carrying a Rendered Cursor is never stamped (spec #561, ticket
/// #566): a still-live run whose cursor this read cannot place waits for a read
/// the projection can seed — the #443 stamp is the cursorless fallback's alone,
/// so the old card is left exactly as it was and the record is kept.
#[tokio::test]
async fn a_cursor_carrying_live_orphan_is_never_stamped() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    // The frontier names a message this read does not carry: unplaceable.
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_gone"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: None,
            delivered_chars: 1,
            prefix_digest: Some(cursor_prefix_digest("答")),
        }),
        &[],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, "还在写。"),
    ]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    // A stamp PATCH would succeed if one were made.
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    wait_for_status_reads(&backend, "ses_test", 3).await;

    assert!(
        patches_to(&platform, "om_frozen").await.is_empty(),
        "a cursor-carrying live orphan is never stamped: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        card_posts(&platform).await,
        0,
        "an unplaceable cursor projects nothing either: {:?}",
        platform.calls.lock().await
    );
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the record waits for a read the projection can place");
    assert_eq!(record.card_message_id, "om_frozen");
    assert!(
        record.cursor.is_some(),
        "a no-decision pass leaves the cursor untouched"
    );
}

/// V1 and V2 share one rule (spec #561, ticket #564): the projection's reads
/// are generation-neutral, so the same restart mid-run adopts and follows
/// identically on either.
#[tokio::test]
async fn a_live_adoption_is_generation_neutral() {
    let _wd = test_work_dir();
    for durable in [false, true] {
        let generation = if durable { "V2" } else { "V1" };
        let dir = tempfile::tempdir().unwrap();
        let session_file = dir.path().join("sessions.json");
        let delivered = "已经写了一半。";
        let missed = "后半段。";
        let full = format!("{delivered}{missed}");
        seed_cursor_record(
            &session_file,
            "om_frozen",
            "msg_cola_anchor",
            Some(1_000),
            Some("/work"),
            Some(text_frontier(delivered)),
            &[],
        );
        // The mock's generation capabilities: V2 keeps the session selection
        // (and no upsert-continue); V1 is the opposite. Neither read the
        // projection makes is generation-specific.
        let mut backend = MockBackend::new(realistic_parts());
        backend.durable_selection = durable;
        backend.reuse_continues_an_admitted_turn = !durable;
        backend.resume_supported = durable;
        backend.given_transcript(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant_in_flight(2_000, delivered),
            ])],
        );
        backend.with_session_status("ses_test", Some(SessionStatus::Busy));
        // The successor's header reads the session's title and the chain's
        // date anchor (issue #561): pin both here so a projection that dropped
        // either cannot pass on its reply target and content alone.
        backend.with_session_title("ses_test", "项目乙");
        let backend = Arc::new(backend);
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(
            App::new(test_config(&session_file), backend.clone(), platform.clone())
                .expect("the restarted app builds"),
        );
        seed_session(&app, "ses_test", "/work").await;

        spawn_sync(&app);
        let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;
        assert!(
            card_header(&successor).contains("回复中"),
            "a working successor on the {generation} mock: {successor}"
        );
        assert_successor_header(&successor, "项目乙", 1_000);
        assert!(
            !successor_text.contains(delivered),
            "the delivered prefix is never repeated on the {generation} mock: {successor}"
        );

        backend
            .given_transcript_after_build(
                "ses_test",
                vec![SessionTranscript::new(vec![
                    user("msg_cola_anchor", 1_000, "问题"),
                    assistant(2_000, &full),
                ])],
            )
            .await;
        wait_for_update(&platform, "msg_reply", "the ✅ ending", |card| {
            card_header(card).contains("✅")
        })
        .await;
        let settled = patches_to(&platform, "msg_reply").await;
        let settled = settled.last().expect("the successor was patched");
        assert_eq!(
            card_text(settled).matches(missed).count(),
            1,
            "the tail lands exactly once on the {generation} mock: {settled}"
        );
        wait_for_record_gone(&app, "ses_test").await;
    }
}

// ---------------------------------------------------------------------------
// The message-first race (spec #561, ticket #565): a user message wins the
// race against the adoption. The fresh Turn takes the orphan chain over and
// its card surfaces the run's final undelivered tail through the Rendered
// Cursor — once, as a continuation, before the new Turn's own answer — so the
// collected orphan card leaves no running marker and the reap posts no second
// card.
// ---------------------------------------------------------------------------

/// The race's transcript: the orphaned run's own end — its in-flight answer
/// carrying the delivered prefix and the unseen tail, plus its still-running
/// call — and the user message that won the race. `answer` appends the new
/// Turn's reply once the run released the queue.
fn race_transcript(
    orphan_anchor: i64,
    text: &str,
    tool_status: ToolStatus,
    tool_output: &str,
    new_anchor: i64,
    answer: Option<&str>,
) -> SessionTranscript {
    let mut messages = vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        TranscriptMessage {
            id: MessageId::new("msg_a_orphan"),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created: orphan_anchor + 500,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Text(TextPart {
                text: text.to_string(),
                started_at: Some(orphan_anchor + 500),
            })],
        },
        in_flight_shell(
            "a_orphan_tool",
            orphan_anchor + 800,
            "call_sleep",
            tool_status,
            orphan_anchor + 900,
            tool_output,
        ),
        user("msg_cola_new", new_anchor, "新问题"),
    ];
    if let Some(answer) = answer {
        messages.push(assistant(new_anchor + 1_000, answer));
    }
    SessionTranscript::new(messages)
}

/// The orphaned card's view when the message landed: a written body and a
/// running `shell` panel riding its live tail (its element id names the call,
/// `tool_{call_id}`, as every real card's does).
fn orphan_running_card_view() -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "streaming_mode": true },
        "header": {
            "template": "blue",
            "title": { "tag": "plain_text", "content": "✍️ 回复中" }
        },
        "body": { "elements": [
            { "tag": "markdown", "content": "**正文** 已经写了一半。" },
            { "tag": "collapsible_panel", "expanded": false, "element_id": "tool_call_sleep",
              "header": { "title": { "tag": "plain_text", "content": "⏳ shell" } },
              "elements": [ { "tag": "markdown", "content": "还在跑" } ] }
        ] }
    })
}

/// A `⏳` running panel as a whole-card read returns it, named for the call it
/// renders (`tool_{call_id}`): the identity the takeover's per-call strip
/// matches against the resolved set (spec #561, review #569).
fn running_panel_card_view(call_id: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "streaming_mode": true },
        "header": {
            "template": "blue",
            "title": { "tag": "plain_text", "content": "✍️ 回复中" }
        },
        "body": { "elements": [
            { "tag": "markdown", "content": "**正文** 已经写了一半。" },
            { "tag": "collapsible_panel", "expanded": false,
              "element_id": format!("tool_{call_id}"),
              "header": { "title": { "tag": "plain_text", "content": "⏳ bash" } },
              "elements": [ { "tag": "markdown", "content": "还在跑" } ] }
        ] }
    })
}

/// The whole-card view of an old card carrying TWO running panels — one for a
/// call this read carries, one it does not (spec #561, review #569): the
/// takeover's strip must drop exactly the resolved one.
fn two_running_panels_card_view() -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "streaming_mode": true },
        "header": {
            "template": "blue",
            "title": { "tag": "plain_text", "content": "✍️ 回复中" }
        },
        "body": { "elements": [
            { "tag": "markdown", "content": "**正文** 已经写了一半。" },
            { "tag": "collapsible_panel", "expanded": false, "element_id": "tool_call_1",
              "header": { "title": { "tag": "plain_text", "content": "⏳ bash · 12:00" } },
              "elements": [ { "tag": "markdown", "content": "被带到新卡片的还在跑" } ] },
            { "tag": "collapsible_panel", "expanded": false, "element_id": "tool_call_gone",
              "header": { "title": { "tag": "plain_text", "content": "⏳ 没读到的工具" } },
              "elements": [ { "tag": "markdown", "content": "冻结的标记" } ] }
        ] }
    })
}

/// The headline acceptance (spec #561, ticket #565): a message sent in the
/// window before the adoption yields one card that shows the orphan's tail
/// exactly once, then the new Turn's answer — while the collected orphan card
/// loses the running marker the successor resolved, and the reap never posts
/// an adoption card over the winning Turn.
#[tokio::test]
async fn a_message_before_the_adoption_shows_the_orphans_tail_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let missed = "停机前没送达的尾巴。";
    let full = format!("{delivered}{missed}");
    // The orphan record confirmed the frontier inside its in-flight answer,
    // plus its still-running call (the Rendered Cursor, spec #561).
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_orphan"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &["call_sleep"],
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_test",
        vec![race_transcript(
            orphan_anchor,
            &full,
            ToolStatus::Running,
            "",
            new_anchor,
            None,
        )],
    );
    backend.with_session_status("ses_test", Some(SessionStatus::Busy));
    // The prompt is held: the takeover and its render window are observable
    // before the new run answers.
    let gate = backend.hold_prompts();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(&session_file), backend.clone(), platform.clone()).expect("the race app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.turn_drain_timeout_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);
    app.turn_follow_read_timeout_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    platform.given_card_view("om_frozen", orphan_running_card_view());

    // The message wins the race: the fresh Turn takes the chain over before
    // any reap pass would adopt the unowned record.
    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_update(&platform, "om_frozen", "the takeover collect", |card| {
        card_header(card).contains("已由新卡片接管")
    })
    .await;
    // Only now does the reap run; it must find the successor owning the chain.
    spawn_sync(&app);

    // The successor shows the orphan's undelivered tail exactly once, as a
    // continuation — the delivered prefix never repeats.
    wait_for_card_text(&platform, missed).await;
    let patched = patches_to(&platform, "msg_reply").await;
    let successor = patched.last().expect("the successor was patched");
    let successor_text = card_text(successor);
    assert_eq!(
        successor_text.matches(missed).count(),
        1,
        "the orphan's tail lands exactly once: {successor}"
    );
    assert!(
        !successor_text.contains(delivered),
        "the delivered prefix is never repeated: {successor}"
    );
    assert!(
        successor_text.contains("⏳ shell"),
        "the still-running call rides the successor's live tail: {successor}"
    );
    // The collected orphan card keeps its body but loses the running marker
    // the successor resolved; the collect is its only write.
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    let collect_text = card_text(&collect);
    assert!(
        !collect_text.contains("⏳ shell") && !collect_text.contains("还在跑"),
        "the resolved running panel leaves the collected card: {collect}"
    );
    assert!(
        collect_text.contains("**正文** 已经写了一半。"),
        "the collect keeps the body the old card already showed: {collect}"
    );
    assert_eq!(
        patches_to(&platform, "om_frozen").await.len(),
        1,
        "the takeover collect is the orphan's only write: {:?}",
        patches_to(&platform, "om_frozen").await
    );
    assert_eq!(
        card_posts(&platform).await,
        1,
        "the message won the race: the adoption never posts a second card: {:?}",
        platform.calls.lock().await
    );

    // The new run answers: the tail stays once and the new answer follows.
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![race_transcript(
                orphan_anchor,
                &full,
                ToolStatus::Completed,
                "slept",
                new_anchor,
                Some("新回答"),
            )],
        )
        .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
    wait_for_card_header(&platform, "✅").await;
    let seen = platform.updated_cards().await;
    let final_text = card_text(seen.last().unwrap());
    assert_eq!(
        final_text.matches(missed).count(),
        1,
        "the tail stays exactly once at settle: {final_text}"
    );
    assert!(
        !final_text.contains(delivered),
        "the delivered prefix stays out: {final_text}"
    );
    assert!(
        final_text.contains("新回答"),
        "the new Turn's own answer follows the tail: {final_text}"
    );
    assert_eq!(
        card_posts(&platform).await,
        1,
        "no adoption card ever appears: {:?}",
        platform.calls.lock().await
    );
    wait_for_record_gone(&app, "ses_test").await;
}

// ---------------------------------------------------------------------------
// The in-flight-create window (spec #561, Codex review on PR #569): the create
// itself is awaited, so a fresh Turn can win the race AFTER the projection
// armed its successor. The projection must then leave the Turn's chain alone —
// never attach its late card to that session, never re-point the record — and
// collect the late card so it cannot look live.
// ---------------------------------------------------------------------------

/// A user message starting a fresh Turn while the projection's create is in
/// flight: the Turn owns the card and the record when the create returns. The
/// projection's late card is collected as taken over and gets nothing else;
/// the Turn's card keeps the chain — the orphan's tail once, then the new
/// answer — and the record still names the Turn's own message.
#[tokio::test]
async fn a_turn_winning_the_create_window_keeps_the_chain_and_its_late_card_is_collected() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let missed = "停机前没送达的尾巴。";
    let full = format!("{delivered}{missed}");
    // The orphan record confirmed the frontier inside its in-flight answer,
    // plus its still-running call (the Rendered Cursor, spec #561).
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_orphan"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &["call_sleep"],
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_test",
        vec![race_transcript(
            orphan_anchor,
            &full,
            ToolStatus::Running,
            "",
            new_anchor,
            None,
        )],
    );
    backend.with_session_status("ses_test", Some(SessionStatus::Busy));
    // The new Turn's prompt is held: the takeover and its render window are
    // observable before the new run answers.
    let prompt_gate = backend.hold_prompts();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(&session_file), backend.clone(), platform.clone()).expect("the race app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.turn_drain_timeout_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);
    app.turn_follow_read_timeout_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    platform.given_card_view("om_frozen", orphan_running_card_view());
    // The late card's own view carries the tail it was created with: the same
    // tail the winning Turn's message-first seed re-rendered (review #569).
    platform.given_card_view("om_late_adoption", late_successor_view(missed));

    // The projection arms its successor and parks inside the awaited create.
    // The scripted id is the late card's own identity — the mock's default
    // `msg_reply` is the Turn's card, and the two must be tellable apart.
    let (create_entered, create_release) = platform.pause("reply", "om_frozen");
    platform.given_reply_id("om_late_adoption");
    spawn_sync_with_timeout(&app, 5_000);
    create_entered.notified().await;

    // The message wins the create window: the fresh Turn takes the chain over
    // while the projection's create is still in flight.
    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, missed).await;
    wait_for_record_card(&app, "ses_test", "msg_reply").await;

    // Release the create: the projection resumes onto a session it no longer
    // owns. Its late card is collected as taken over — its only write, with NO
    // body: the winner re-rendered that tail, so the reader must never see it
    // twice — and it never touches the Turn's session or the record.
    create_release.notify_one();
    wait_for_update(
        &platform,
        "om_late_adoption",
        "the late adoption's collect",
        |card| card_header(card).contains("已由新卡片接管"),
    )
    .await;
    let late = patches_to(&platform, "om_late_adoption").await;
    assert_eq!(
        late.len(),
        1,
        "the late card gets its collect and nothing else: {late:?}"
    );
    let late_collect = late.last().expect("the collect");
    assert!(
        !card_text(late_collect).contains(missed),
        "the late card is reduced to the taken-over marker, never the repeated tail: {late_collect:?}"
    );

    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the Turn's record survives the late projection");
    assert_eq!(
        record.card_message_id, "msg_reply",
        "the late projection never re-points the chain at its own card"
    );
    assert_eq!(
        record.message_id.as_str(),
        "msg_cola_new",
        "the record still names the Turn's own message"
    );
    let turn_patches = patches_to(&platform, "msg_reply").await;
    assert!(
        turn_patches.iter().any(|card| card_text(card).contains(missed)),
        "the Turn's card still carries the chain's content: {turn_patches:?}"
    );
    assert!(
        !turn_patches
            .iter()
            .any(|card| card_header(card).contains("已由新卡片接管")),
        "the Turn's card is never collected by the late projection: {turn_patches:?}"
    );
    assert_eq!(
        card_posts(&platform).await,
        2,
        "the Turn's card and the late adoption's card were created: {:?}",
        platform.calls.lock().await
    );

    // The run ends: the Turn's card still shows the orphan's tail exactly once
    // and then the new answer — nothing was eaten or duplicated.
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![race_transcript(
                orphan_anchor,
                &full,
                ToolStatus::Completed,
                "slept",
                new_anchor,
                Some("新回答"),
            )],
        )
        .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    prompt_gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
    wait_for_card_header(&platform, "✅").await;
    let final_text = card_text(platform.updated_cards().await.last().unwrap());
    assert_eq!(
        final_text.matches(missed).count(),
        1,
        "the tail stays exactly once at settle: {final_text}"
    );
    assert!(
        !final_text.contains(delivered),
        "the delivered prefix stays out: {final_text}"
    );
    assert!(
        final_text.contains("新回答"),
        "the new Turn's own answer follows the tail: {final_text}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// The ended-while-down twin of the create window: the projection's successor
/// already wears the transcript's true ending, and a fresh Turn starting while
/// its create is in flight must still keep the chain — the late terminal card
/// is collected like any other, never attached to the Turn's session and never
/// re-pointed into the record.
#[tokio::test]
async fn a_turn_winning_the_create_window_keeps_the_chain_when_the_run_ended_while_down() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let missed = "停机期间写完的尾巴。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_orphan"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    // The run ENDED while cola was down: its answer is completed, the session
    // idles — the ended projection's decision. `answer` appends the fresh
    // Turn's own reply once its prompt is released.
    let transcript = |answer: Option<&str>| {
        let mut messages = vec![
            user("msg_cola_anchor", orphan_anchor, "问题"),
            TranscriptMessage {
                id: MessageId::new("msg_a_orphan"),
                role: MessageRole::Assistant,
                time: Some(MessageTime {
                    created: orphan_anchor + 500,
                    completed: Some(orphan_anchor + 3_000),
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::Text(TextPart {
                    text: full.clone(),
                    started_at: Some(orphan_anchor + 500),
                })],
            },
            user("msg_cola_new", new_anchor, "新问题"),
        ];
        if let Some(answer) = answer {
            messages.push(assistant(new_anchor + 1_000, answer));
        }
        SessionTranscript::new(messages).with_executions(vec![execution(orphan_anchor + 2_500)])
    };
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript("ses_test", vec![transcript(None)]);
    backend.with_session_status("ses_test", Some(SessionStatus::Idle));
    // The new Turn's prompt is held: its takeover is observable before the
    // fresh run answers.
    let prompt_gate = backend.hold_prompts();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(&session_file), backend.clone(), platform.clone()).expect("the race app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.turn_drain_timeout_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);
    app.turn_follow_read_timeout_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    platform.given_card_view("om_frozen", realistic_card_view());
    // The late terminal card's own view carries the tail it was created with.
    platform.given_card_view("om_late_adoption", late_successor_view(missed));

    // The projection arms its terminal successor and parks inside the awaited
    // create; the scripted id is the late card's own identity.
    let (create_entered, create_release) = platform.pause("reply", "om_frozen");
    platform.given_reply_id("om_late_adoption");
    spawn_sync_with_timeout(&app, 5_000);
    create_entered.notified().await;

    // The message wins the create window.
    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, missed).await;
    wait_for_record_card(&app, "ses_test", "msg_reply").await;

    // Release the create: the late terminal card is collected bodyless — the
    // winner re-rendered that tail — never attached to the Turn's session nor
    // re-pointed into the record.
    create_release.notify_one();
    wait_for_update(
        &platform,
        "om_late_adoption",
        "the late terminal successor's collect",
        |card| card_header(card).contains("已由新卡片接管"),
    )
    .await;
    let late = patches_to(&platform, "om_late_adoption").await;
    assert_eq!(
        late.len(),
        1,
        "the late card gets its collect and nothing else: {late:?}"
    );
    let late_collect = late.last().expect("the collect");
    assert!(
        !card_text(late_collect).contains(missed),
        "the late card is reduced to the taken-over marker, never the repeated tail: {late_collect:?}"
    );
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the Turn's record survives the late projection");
    assert_eq!(
        record.card_message_id, "msg_reply",
        "the late projection never re-points the chain at its own card"
    );
    assert_eq!(
        record.message_id.as_str(),
        "msg_cola_new",
        "the record still names the Turn's own message"
    );
    let turn_patches = patches_to(&platform, "msg_reply").await;
    assert!(
        !turn_patches
            .iter()
            .any(|card| card_header(card).contains("已由新卡片接管")),
        "the Turn's card is never collected by the late projection: {turn_patches:?}"
    );

    // The fresh run answers and settles on its own card: the tail once, then
    // the new answer — nothing was eaten or duplicated.
    backend
        .given_transcript_after_build("ses_test", vec![transcript(Some("新回答"))])
        .await;
    prompt_gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
    wait_for_card_header(&platform, "✅").await;
    let final_text = card_text(platform.updated_cards().await.last().unwrap());
    assert_eq!(
        final_text.matches(missed).count(),
        1,
        "the tail stays exactly once at settle: {final_text}"
    );
    assert!(
        !final_text.contains(delivered),
        "the delivered prefix stays out: {final_text}"
    );
    assert!(
        final_text.contains("新回答"),
        "the new Turn's own answer follows the tail: {final_text}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

// ---------------------------------------------------------------------------
// A settled tool at the cursor (spec #561, review #569): the frontier names
// the newest delivered-final item of ANY content kind, so a restart does not
// re-render a panel the old card already showed.
// ---------------------------------------------------------------------------

/// Wait until the session's durable cursor satisfies `ready`, or panic after
/// 5 s.
async fn wait_for_cursor(app: &Arc<App>, mut ready: impl FnMut(&RenderedCursor) -> bool) -> RenderedCursor {
    let probe = async {
        loop {
            if let Some(cursor) = app.cards_handle().chains.cursor("ses_test")
                && ready(&cursor)
            {
                return cursor;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .expect("the record's cursor must reach the awaited shape")
}

/// The restart's read: an in-flight answer plus the `shell` call it made,
/// whose status the caller scripts. Both parts carry their server start times,
/// so the timeline orders them like the server's own read.
fn text_and_tool(text: &str, status: ToolStatus, output: &str) -> SessionTranscript {
    SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑个长命令"),
        TranscriptMessage {
            id: MessageId::new("msg_a_2000"),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created: 2_000,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Text(TextPart {
                text: text.to_string(),
                started_at: Some(2_000),
            })],
        },
        in_flight_shell("msg_tool_3000", 3_000, "call_sleep", status, 3_100, output),
    ])
}

/// A tool that settles while the run streams is part of the confirmed
/// frontier (spec #561, review #569): a restart over that cursor must NOT
/// re-render the panel the old card already showed, and text the run produces
/// after the restart still renders only its tail.
#[tokio::test]
async fn a_restart_does_not_re_render_a_settled_tool_delivered_after_the_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";

    // Life 1: a live turn streams the text, then the tool settles. The
    // confirmed write that settles it advances the persisted cursor.
    let mut backend1 = MockBackend::new(realistic_parts());
    backend1.given_transcript(
        "ses_test",
        vec![text_and_tool(delivered, ToolStatus::Running, "")],
    );
    backend1.with_session_status("ses_test", Some(SessionStatus::Busy));
    let backend1 = Arc::new(backend1);
    let platform1 = Arc::new(RecordingPlatform::new());
    let app1 = Arc::new(
        App::new(test_config(&session_file), backend1.clone(), platform1.clone())
            .expect("the first life builds"),
    );
    seed_session(&app1, "ses_test", "/work").await;
    app1.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app1.turn_drain_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);
    app1.turn_follow_read_timeout_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    let turn = spawn_turn(&app1, ctx("ses_test", "跑个长命令"));
    wait_for_card_text(&platform1, delivered).await;
    let result = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must hand off at the drain bound")
        .unwrap();
    result.unwrap();
    wait_for_cursor(&app1, |cursor| cursor.live_calls.contains("call_sleep")).await;

    // The server settles the call: the next confirmed write's frontier must
    // name it (the live set empties and the extent stays the text's).
    settle_tool(&backend1, ToolStatus::Completed, "done").await;
    wait_for_cursor(&app1, |cursor| {
        cursor.live_calls.is_empty()
            && cursor
                .frontier
                .as_ref()
                .is_some_and(|frontier| frontier.delivered_chars == delivered.chars().count())
    })
    .await;
    // Life 1 ends here; a fresh process takes the sidecar over.
    drop(app1);

    // Life 2: the restart finds the settled panel delivered, so neither it nor
    // the text is repeated on the successor.
    let mut backend2 = MockBackend::new(realistic_parts());
    backend2.given_transcript(
        "ses_test",
        vec![text_and_tool(delivered, ToolStatus::Completed, "done")],
    );
    backend2.with_session_status("ses_test", Some(SessionStatus::Busy));
    let backend2 = Arc::new(backend2);
    let platform2 = Arc::new(RecordingPlatform::new());
    let app2 = Arc::new(
        App::new(test_config(&session_file), backend2.clone(), platform2.clone())
            .expect("the restarted app builds"),
    );
    seed_session(&app2, "ses_test", "/work").await;
    spawn_sync(&app2);
    let (successor, successor_text) = wait_for_projection(&platform2, "msg_1").await;
    assert!(
        !successor_text.contains("done") && !successor_text.contains("sleep 3600"),
        "a settled panel the old card already showed is never re-rendered: {successor}"
    );
    assert!(
        !successor_text.contains(delivered),
        "the delivered text prefix is never repeated: {successor}"
    );

    // The run streams on: the text grows, and only the new tail renders.
    let tail = "重启之后又写了一部分。";
    let grown = format!("{delivered}{tail}");
    backend2
        .given_transcript_after_build(
            "ses_test",
            vec![text_and_tool(&grown, ToolStatus::Completed, "done")],
        )
        .await;
    wait_for_card_text(&platform2, tail).await;
    let update = platform2
        .updated_cards()
        .await
        .into_iter()
        .find(|card| card_text(card).contains(tail))
        .expect("the tail reached a card");
    let text = card_text(&update);
    assert_eq!(
        text.matches(tail).count(),
        1,
        "the grown tail lands exactly once: {text}"
    );
    assert!(
        !text.contains("done"),
        "the settled panel stays out of the continuation: {text}"
    );
}

/// The exact lost-response shape (spec #561, review #569): Feishu accepted
/// the successor create but the response never arrived. Feishu has no
/// idempotency key (ADR-0067), so the attempt is single-shot — later passes
/// post nothing further and the reap state-repairs the old card in place —
/// and the user never gets two successor cards.
#[tokio::test]
async fn an_ambiguous_create_after_a_lost_response_is_never_retried() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "停机期间写完的尾巴。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The successor create is accepted remotely, but its response is lost.
    platform
        .fail_reply_card_after_send_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    // The reap settles the old card in place — never a second successor.
    wait_for_update(&platform, "om_frozen", "the in-place ending", |card| {
        card_header(card).contains("✅")
    })
    .await;
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "the ambiguous create is never repeated: {:?}",
        platform.calls.lock().await
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A live-set call the read does not carry is never handed over (spec #561,
/// review #569): a V2 transcript truncated at its page cap leaves the call
/// outside the successor's sight, so the old card keeps the frozen running
/// marker instead of dropping a panel that would vanish from both.
///
/// Truncation is NOT detectable at this seam (review #569): the V2 transcript
/// read stops at its page cap with a log line, and the generation-neutral
/// `SessionTranscript` carries no truncation fact — so the projection cannot
/// withhold itself on a truncated read. This frozen `⏳` is therefore the
/// honest signal; a later complete read adopts normally (ADR-0071).
#[tokio::test]
async fn an_unobserved_running_tool_stays_on_the_old_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        // `call_gone` is NOT in the read: a truncated transcript left the
        // still-running call outside it.
        &["call_1", "call_gone"],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, delivered),
        tool_assistant(3_000, ToolStatus::Running, ""),
    ]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", running_panel_card_view("call_gone"));

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;
    assert!(
        successor_text.contains("⏳ bash"),
        "the read's own still-running call is carried live onto the successor: {successor}"
    );

    // The old card's collect withholds the strip: the successor cannot render
    // `call_gone`, so its marker stays as a frozen witness rather than
    // vanishing from both cards.
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    let collect_text = card_text(&collect);
    assert!(
        collect_text.contains("⏳ bash") && collect_text.contains("还在跑"),
        "a call the successor cannot render keeps its marker on the old card: {collect}"
    );
}

/// The strip is per resolved call (spec #561, review #569): when the read
/// carries one live-set call but not the other, only the RESOLVED call's
/// running marker leaves the collected old card — the unread call's stays a
/// frozen witness. The all-present case and the all-unresolved case keep their
/// own tests.
#[tokio::test]
async fn the_collect_strips_only_the_running_panels_the_seed_resolved() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        // The read carries `call_1` (a running `bash`); `call_gone` is NOT in
        // it — a truncated transcript left the still-running call outside.
        &["call_1", "call_gone"],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, delivered),
        tool_assistant(3_000, ToolStatus::Running, ""),
    ]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", two_running_panels_card_view());

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;
    assert!(
        successor_text.contains("⏳ bash"),
        "the read's own still-running call is carried live onto the successor: {successor}"
    );

    // The collect drops the resolved call's marker and keeps the unread
    // call's frozen one.
    let collect = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    let collect_text = card_text(&collect);
    assert!(
        !collect_text.contains("被带到新卡片的还在跑"),
        "the resolved call's running marker leaves the old card: {collect}"
    );
    assert!(
        collect_text.contains("没读到的工具") && collect_text.contains("冻结的标记"),
        "the unread call's frozen marker stays: {collect}"
    );
}

/// The oversized missed delta (spec #561, review #569): a restart whose
/// undelivered tail exceeds Feishu's card limits projects through the normal
/// splitter as a bounded chain — every slice in order, each confirmed only
/// after its own create, no content lost or duplicated.
#[tokio::test]
async fn an_oversized_missed_delta_lands_across_a_bounded_chain() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let marker = |i: usize| format!("【S{i:02}】");
    let tail: String = (0..40)
        .map(|i| format!("{}{}", marker(i), "长".repeat(400)))
        .collect();
    let full = format!("{delivered}{tail}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    // The last marker proves the whole tail landed across the created chain.
    wait_for_posted_text(&platform, &marker(39)).await;
    let posts = platform.replied_cards().await;
    assert!(
        posts.len() >= 2,
        "an oversized delta is a chain, not one card: {:?}",
        platform.calls.lock().await
    );
    let texts: Vec<String> = posts.iter().map(card_text).collect();
    let combined = texts.join("\n");
    let mut last_pos = 0usize;
    for i in 0..40 {
        let m = marker(i);
        assert_eq!(
            combined.matches(&m).count(),
            1,
            "{m} lands exactly once across the chain: {combined}"
        );
        let pos = combined.find(&m).expect("found");
        assert!(pos >= last_pos, "{m} lands in order: {combined}");
        last_pos = pos;
    }
    assert!(
        !combined.contains(delivered),
        "the delivered prefix is never repeated: {combined}"
    );
    for text in &texts {
        assert!(
            text.chars().count() <= crate::feishu::card::MAX_CARD_TEXT_CHARS + 2_000,
            "each slice is bounded ({} chars): {text}",
            text.chars().count()
        );
    }
    // The chain ends with the true ending on its last card, the old card is
    // collected, and the record is spent.
    assert!(
        card_header(posts.last().expect("the chain posted")).contains("✅"),
        "the chain's last card wears the transcript's true ending: {:?}",
        posts.last().unwrap()
    );
    let collected = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    assert!(
        card_header(&collected).contains("已由新卡片接管"),
        "the old card is collected: {collected}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A mid-chain create failure (spec #561, review #569): the cursor advances
/// only through the slices that landed, the possibly-landed slice is never
/// re-posted in this life, and the record — kept by the single-shot mark —
/// hands the rest to the next life's recovery.
#[tokio::test]
async fn a_mid_chain_projection_failure_keeps_only_the_landed_slices() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let marker = |i: usize| format!("【S{i:02}】");
    let tail: String = (0..40)
        .map(|i| format!("{}{}", marker(i), "长".repeat(400)))
        .collect();
    let full = format!("{delivered}{tail}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The first create lands; the second fails ambiguously.
    platform.given_reply_card_outcome(crate::bridge::test_support::ReplyOutcome::Lands);
    platform.given_reply_card_outcome(crate::bridge::test_support::ReplyOutcome::Ambiguous);

    spawn_sync(&app);
    let cursor = wait_for_cursor(&app, |cursor| {
        cursor
            .frontier
            .as_ref()
            .is_some_and(|frontier| frontier.delivered_chars > delivered.chars().count())
    })
    .await;
    let landed = cursor
        .frontier
        .as_ref()
        .map(|frontier| frontier.delivered_chars)
        .expect("the frontier is confirmed");
    assert!(
        landed < full.chars().count(),
        "only the landed slices are confirmed: {cursor:?}"
    );
    // Exactly one successor card: the failed slice is never re-posted.
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "the failed slice is not re-posted: {:?}",
        platform.calls.lock().await
    );
    let _ = &backend;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "later passes never re-post the stopped chain: {:?}",
        platform.calls.lock().await
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "the record stays for the next life's recovery"
    );
    let persisted =
        crate::bridge::chain::ChainRecords::load(session_file.with_file_name("chain_records.json"));
    assert_eq!(
        persisted
            .cursor("ses_test")
            .and_then(|cursor| cursor.frontier.map(|frontier| frontier.delivered_chars)),
        Some(landed),
        "the persisted cursor covers exactly the landed slices"
    );
    let first = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the landed slice");
    let first_text = card_text(&first);
    assert!(first_text.contains(&marker(0)), "{first_text}");
    assert!(!first_text.contains(&marker(39)), "{first_text}");
}

/// Wait until the platform observed at least `n` PATCH attempts for
/// `card_message_id` (failed ones included), or panic after 5 s.
async fn wait_for_update_attempts(platform: &RecordingPlatform, card_message_id: &str, n: usize) {
    let probe = async {
        loop {
            if patches_to(platform, card_message_id).await.len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .expect("the platform must see the write attempts");
}

/// Wait until a successfully POSTED (created) card carries `needle`, or panic
/// after 5 s — a projection's chain slices are creates, not PATCHes, so
/// [`wait_for_card_text`] (which reads updates) never sees them.
async fn wait_for_posted_text(platform: &RecordingPlatform, needle: &str) {
    let probe = async {
        loop {
            if platform
                .replied_cards()
                .await
                .iter()
                .any(|card| card_text(card).contains(needle))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("no posted card ever carried {needle:?}"));
}

/// An oversized LIVE delta (spec #561, review #569): the chain's last card is
/// the live card the follow continues on, so content the run produces after
/// the restart still streams onto the chain.
#[tokio::test]
async fn an_oversized_live_delta_chains_and_the_follow_continues_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let marker = |i: usize| format!("【S{i:02}】");
    let tail: String = (0..40)
        .map(|i| format!("{}{}", marker(i), "长".repeat(400)))
        .collect();
    let full = format!("{delivered}{tail}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, &full),
    ]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    wait_for_posted_text(&platform, &marker(39)).await;
    let posts = platform.replied_cards().await;
    assert!(
        posts.len() >= 2,
        "the oversized live delta is a chain: {:?}",
        platform.calls.lock().await
    );

    // The run streams on: the grown tail PATCHes the chain's live card.
    let grown = format!("{full}重启之后的尾巴。");
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant_in_flight(2_000, &grown),
            ])],
        )
        .await;
    wait_for_card_text(&platform, "重启之后的尾巴。").await;
    let update = platform
        .updated_cards()
        .await
        .into_iter()
        .find(|card| card_text(card).contains("重启之后的尾巴。"))
        .expect("the follow's tail reached the card");
    let text = card_text(&update);
    assert_eq!(
        text.matches("重启之后的尾巴。").count(),
        1,
        "the follow's tail lands exactly once: {text}"
    );
    assert!(
        !text.contains(delivered),
        "the delivered prefix is never repeated: {text}"
    );
}

/// A DEFINITE create failure is retryable (spec #561, review #569): a card
/// content rejection proves Feishu created no message, so the projection
/// re-renders the SAME slice through the flush's fenced fallback and the missed
/// tail still lands. The old behavior marked the attempt single-shot and let
/// the tail be state-repaired away.
#[tokio::test]
async fn a_definite_create_failure_recovers_with_the_fenced_fallback() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段是在停机期间写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The first create is refused as card content: Feishu created no message.
    platform.given_reply_card_outcome(crate::bridge::test_support::ReplyOutcome::Rejected);

    spawn_sync(&app);
    // The fenced retry lands the same slice.
    wait_for_posted_text(&platform, missed).await;
    let posts = platform.replied_cards().await;
    assert_eq!(
        posts.len(),
        1,
        "only the fenced retry landed: {:?}",
        platform.calls.lock().await
    );
    let text = card_text(&posts[0]);
    assert!(text.contains(missed), "the missed tail lands: {text}");
    assert!(
        text.contains("```"),
        "the retry degraded the model markdown to the flush's fenced form: {text}"
    );
    assert!(
        !text.contains(delivered),
        "the delivered prefix is never repeated: {text}"
    );
    let collected = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is collected as taken over");
    assert!(
        card_header(&collected).contains("已由新卡片接管"),
        "the old card is collected: {collected}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A definite HTTP refusal (a 4xx: the platform created no message) falls to
/// the next delivery target (issue #580); with no fallback left — no session
/// mapping, the recorded card is the only target — the record stays retryable,
/// so the NEXT reconcile pass re-attempts and the missed tail lands (spec
/// #561, review #569). An ambiguous failure (transport, 5xx) stays single-shot,
/// covered by the ambiguous tests.
#[tokio::test]
async fn a_definite_http_refusal_without_a_fallback_retries_on_the_next_pass() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段是在停机期间写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    // No session mapping: the recorded card is the ONLY deliverable target,
    // so the refusal leaves the projection with nowhere to fall — it must
    // stay retryable for the next pass.
    let (app, platform, _backend) = restarted_app_unmapped(&session_file, transcript).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The first create is refused with an explicit 4xx: no message was made.
    platform.given_reply_card_outcome(crate::bridge::test_support::ReplyOutcome::Refused(400));

    spawn_sync(&app);
    wait_for_posted_text(&platform, missed).await;
    let posts = platform.replied_cards().await;
    assert_eq!(
        posts.len(),
        1,
        "the refused attempt made no message; the retry landed: {:?}",
        platform.calls.lock().await
    );
    let text = card_text(&posts[0]);
    assert!(text.contains(missed), "the missed tail lands: {text}");
    assert!(
        !text.contains(delivered),
        "the delivered prefix is never repeated: {text}"
    );
    assert!(
        card_header(&posts[0]).contains("✅"),
        "the retry carried the transcript's true ending: {:?}",
        posts[0]
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// The late projection successor's card view (spec #561, review #569): the
/// card as Feishu would return it just before its collect — a working header
/// and the tail body the projection rendered.
fn late_successor_view(tail: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "streaming_mode": true },
        "header": {
            "template": "blue",
            "title": { "tag": "plain_text", "content": "✍️ 回复中" }
        },
        "body": { "elements": [
            { "tag": "markdown", "content": tail }
        ] }
    })
}

/// Rewrite the newest scripted assistant part's text in place (spec #561,
/// review #569): the server REPLACING a part's content instead of extending
/// it.
async fn rewrite_scripted_part(backend: &Arc<MockBackend>, text: &str) {
    let mut scripts = backend.transcript_scripts.lock().await;
    let transcript = &mut scripts.get_mut("ses_test").unwrap()[0];
    for message in transcript.messages.iter_mut() {
        if message.role != MessageRole::Assistant {
            continue;
        }
        for part in message.parts.iter_mut() {
            if let Part::Text(part) = part {
                part.text = text.to_string();
                return;
            }
        }
    }
    panic!("the scripted transcript carries no assistant text part to rewrite");
}

/// A rewritten part persists a RESOLVABLE cursor (spec #561, review #569):
/// after the server replaces a text part's content, the record's frontier must
/// carry the replacement's digest at the new full length, so a restart
/// resolves it — an ended run projects its missed tail and a live run is
/// adopted, instead of falling back to the legacy cursorless behavior.
#[tokio::test]
async fn a_rewritten_part_persists_a_resolvable_cursor_for_the_restart() {
    let _wd = test_work_dir();
    for live in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let session_file = dir.path().join("sessions.json");
        let first = "ABC";
        let rewritten = "XYZ";
        let tail = "更多";
        // Life 1: a live run renders "ABC", then the server rewrites the part
        // to "XYZ"; the confirmed write must persist the new frontier.
        let mut backend1 = MockBackend::new(realistic_parts());
        backend1.given_transcript(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant(2_000, first),
            ])],
        );
        backend1.with_session_status("ses_test", Some(SessionStatus::Busy));
        let backend1 = Arc::new(backend1);
        let platform1 = Arc::new(RecordingPlatform::new());
        let app1 = Arc::new(
            App::new(test_config(&session_file), backend1.clone(), platform1.clone())
                .expect("the first life builds"),
        );
        seed_session(&app1, "ses_test", "/work").await;
        app1.turn_render_poll_ms
            .store(5, std::sync::atomic::Ordering::Relaxed);
        app1.turn_drain_timeout_ms
            .store(30, std::sync::atomic::Ordering::Relaxed);
        app1.turn_follow_read_timeout_ms
            .store(50, std::sync::atomic::Ordering::Relaxed);
        let turn = spawn_turn(&app1, ctx("ses_test", "问题"));
        wait_for_card_text(&platform1, first).await;
        let result = tokio::time::timeout(Duration::from_secs(5), turn)
            .await
            .expect("the turn must hand off at the drain bound")
            .unwrap();
        result.unwrap();
        wait_for_cursor(&app1, |cursor| {
            cursor
                .frontier
                .as_ref()
                .is_some_and(|frontier| frontier.delivered_chars == first.chars().count())
        })
        .await;

        rewrite_scripted_part(&backend1, rewritten).await;
        wait_for_card_text(&platform1, rewritten).await;
        // The rewrite's confirmed write persists the replacement's digest at
        // its own full length — red before the fix, which accumulated the two
        // snapshots and left the cursor digestless.
        let cursor = wait_for_cursor(&app1, |cursor| {
            cursor.frontier.as_ref().is_some_and(|frontier| {
                frontier.delivered_chars == rewritten.chars().count()
                    && frontier.prefix_digest == Some(cursor_prefix_digest(rewritten))
            })
        })
        .await;
        drop(cursor);
        // Life 1 ends here; a fresh process takes the sidecar over.
        drop(app1);

        // Life 2: the run streams on past the rewrite (or ended while down):
        // the persisted cursor resolves, so the tail is projected.
        let full = format!("{rewritten}{tail}");
        let mut backend2 = MockBackend::new(realistic_parts());
        let mut transcript = SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            if live {
                assistant_in_flight(2_000, &full)
            } else {
                assistant(2_000, &full)
            },
        ]);
        if !live {
            transcript = transcript.with_executions(vec![execution(2_500)]);
        }
        backend2.given_transcript("ses_test", vec![transcript]);
        backend2.with_session_status(
            "ses_test",
            Some(if live {
                SessionStatus::Busy
            } else {
                SessionStatus::Idle
            }),
        );
        let backend2 = Arc::new(backend2);
        let platform2 = Arc::new(RecordingPlatform::new());
        let app2 = Arc::new(
            App::new(test_config(&session_file), backend2.clone(), platform2.clone())
                .expect("the restarted app builds"),
        );
        seed_session(&app2, "ses_test", "/work").await;
        spawn_sync(&app2);
        let (successor, successor_text) = wait_for_projection(&platform2, "msg_1").await;
        assert!(
            successor_text.contains(tail),
            "the missed tail follows the rewritten part (live={live}): {successor}"
        );
        assert!(
            !successor_text.contains(rewritten),
            "the rewritten-away prefix is never repeated (live={live}): {successor}"
        );
    }
}

/// A record whose `created_ms` was never captured and which carries no durable
/// reply target still delivers (issue #580's order: reply target → recorded
/// card → chat): the successor replies to the recorded card, and the anchor
/// re-derived from the read is NEVER a Feishu target (it is an OpenCode
/// message id).
#[tokio::test]
async fn a_successor_replies_to_the_recorded_card_without_a_durable_reply_target() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段。";
    let full = format!("{delivered}{missed}");
    // The record has no `created_ms`: its anchor can only be re-derived from
    // the read (the original user message is in the transcript).
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        None,
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    // The successor replies to the recorded card; the re-derived anchor is an
    // OpenCode id and is never used as a reply target.
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;
    assert!(successor_text.contains(missed), "{successor}");
    assert!(
        !platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::ReplyCard { reply_to, .. } if reply_to == "msg_cola_anchor"
        )),
        "the OpenCode anchor is never used as a Feishu reply target: {:?}",
        platform.calls.lock().await
    );
}

// ---------------------------------------------------------------------------
// The post-re-point window (spec #561, Codex review on PR #569, third round):
// the successor's create is awaited, and so is the old card's collect PATCH
// that follows the atomic takeover. A fresh Turn starting inside that collect
// must inherit the successor's CONFIRMED cursor from the record — never the
// predecessor's stale frontier, which would seed the already-delivered tail
// onto its own card while the collected successor keeps its body: the same
// text twice.
// ---------------------------------------------------------------------------

/// A user message starting a fresh Turn while the projection collects the old
/// card: the takeover has re-pointed the record, so the fresh Turn's seed must
/// read the successor's confirmed cursor and render nothing of the delivered
/// tail. The tail stays exactly once — on the successor card the Turn collects
/// with its body — and never on the Turn's own card.
#[tokio::test]
async fn a_turn_winning_the_collect_window_never_re_renders_the_delivered_tail() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let missed = "停机前没送达的尾巴。";
    let full = format!("{delivered}{missed}");
    // The orphan record confirmed the frontier inside its in-flight answer
    // (spec #561): the part the successor's seed cuts at.
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "问题"),
            assistant_in_flight(orphan_anchor + 500, &full),
        ])],
    );
    backend.with_session_status("ses_test", Some(SessionStatus::Busy));
    // The new Turn's prompt is held: its takeover and seed are observable
    // before the fresh run answers.
    let prompt_gate = backend.hold_prompts();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(&session_file), backend.clone(), platform.clone()).expect("the race app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.turn_drain_timeout_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);
    app.turn_follow_read_timeout_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    platform.given_card_view("om_frozen", realistic_card_view());
    // The successor's own view carries the tail it was created with: the Turn's
    // collect preserves that body, exactly like a real card read.
    platform.given_card_view("om_late_adoption", late_successor_view(missed));
    // The scripted id tells the projection's successor card apart from the
    // Turn's own loading card (the mock's default `msg_reply`).
    platform.given_reply_id("om_late_adoption");

    // The projection arms, creates, takes the chain over, and parks inside the
    // old card's collect PATCH.
    let (collect_entered, collect_release) = platform.pause("update", "om_frozen");
    spawn_sync_with_timeout(&app, 5_000);
    collect_entered.notified().await;
    wait_for_record_card(&app, "ses_test", "om_late_adoption").await;

    // The message wins the collect window: the fresh Turn takes the chain over
    // and seeds from the record's cursor.
    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_record_card(&app, "ses_test", "msg_reply").await;

    // Release the parked collect; the projection owes nothing more.
    collect_release.notify_one();

    // The run ends: the successor card keeps the tail once (the Turn's collect
    // preserved its body) and the Turn's card carries only its own answer.
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![
                SessionTranscript::new(vec![
                    user("msg_cola_anchor", orphan_anchor, "问题"),
                    TranscriptMessage {
                        id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
                        role: MessageRole::Assistant,
                        time: Some(MessageTime {
                            created: orphan_anchor + 500,
                            completed: Some(orphan_anchor + 3_000),
                        }),
                        model: None,
                        tokens: None,
                        error: None,
                        parts: vec![Part::Text(TextPart {
                            text: full.clone(),
                            started_at: Some(orphan_anchor + 500),
                        })],
                    },
                    user("msg_cola_new", new_anchor, "新问题"),
                ])
                .with_executions(vec![execution(orphan_anchor + 2_500)]),
            ],
        )
        .await;
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
    prompt_gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
    wait_for_card_header(&platform, "✅").await;

    let successor = last_update_of(&platform, "om_late_adoption")
        .await
        .expect("the successor's collect keeps its body");
    assert_eq!(
        card_text(&successor).matches(missed).count(),
        1,
        "the successor card keeps the tail exactly once: {successor}"
    );
    let turn_patches = patches_to(&platform, "msg_reply").await;
    assert!(
        !turn_patches.is_empty(),
        "the fresh Turn flushed its card: {:?}",
        platform.calls.lock().await
    );
    assert!(
        turn_patches.iter().all(|card| !card_text(card).contains(missed)),
        "the fresh Turn never re-renders the delivered tail: {turn_patches:?}"
    );
}

/// A live run REWRITES its frontier part under the seeded follow (spec #561,
/// review #569): the read no longer carries the prefix the successor already
/// delivered, so the follow must REPLACE the part's run with the rewritten
/// content — never cut the new text at the old extent, which would push a
/// stray suffix and claim the replacement delivered. On a match, the
/// suffix-only growth stays (the existing growth tests prove that half).
#[tokio::test]
async fn a_live_rewrite_of_the_frontier_part_replaces_the_successors_run() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let streamed = "尾巴一。";
    let adopted = format!("{delivered}{streamed}");
    let rewritten = "换了一段全新的答案内容补充。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_2000"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(2_000),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            assistant_in_flight(2_000, &adopted),
        ])],
    );
    backend.with_session_status("ses_test", Some(SessionStatus::Busy));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(&session_file), backend.clone(), platform.clone()).expect("the app builds"),
    );
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    app.turn_follow_read_timeout_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    platform.given_reply_id("om_successor");

    spawn_sync(&app);
    let (successor, successor_text) = wait_for_projection(&platform, "om_frozen").await;
    assert_eq!(
        successor_text.matches(streamed).count(),
        1,
        "the adoption shows the streamed tail once: {successor}"
    );
    wait_for_cursor(&app, |cursor| {
        cursor
            .frontier
            .as_ref()
            .is_some_and(|frontier| frontier.delivered_chars == adopted.chars().count())
    })
    .await;

    // The run rewrites the frontier part in place under the live follow.
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant_in_flight(2_000, rewritten),
            ])],
        )
        .await;
    // The follow processed the rewrite: its last two characters appear in both
    // worlds (the buggy cut keeps them as a stray suffix).
    wait_for_card_text(&platform, "补充。").await;
    let updates = patches_to(&platform, "om_successor").await;
    let update = updates.last().expect("the successor was patched");
    let text = card_text(update);
    assert!(
        text.contains(rewritten),
        "the rewritten part replaces the successor's run in full: {update}"
    );
    assert!(
        !text.contains(streamed),
        "the rewritten-away run leaves the successor's card: {update}"
    );
    // The cursor reflects the rewritten part.
    wait_for_cursor(&app, |cursor| {
        cursor.frontier.as_ref().is_some_and(|frontier| {
            frontier.delivered_chars == rewritten.chars().count()
                && frontier.prefix_digest == Some(cursor_prefix_digest(rewritten))
        })
    })
    .await;
}

/// A cursor this read cannot place keeps an ENDED record (spec #561, review
/// #569): settling the old card in place would release the record and lose the
/// tail the cursor still guards, while a later read that places the cursor can
/// still project it. The cursorless fallback and the Unreceived terminal are
/// untouched (their own tests).
#[tokio::test]
async fn an_unplaceable_cursor_keeps_an_ended_records_record() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    // The frontier names a message this read does not carry: unplaceable.
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_gone"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: None,
            delivered_chars: 1,
            prefix_digest: Some(cursor_prefix_digest("答")),
        }),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, "答复。"),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    // A settle PATCH would succeed if one were made.
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    // The reap either releases the record on its first pass (the loss this
    // test forbids) or keeps it and keeps observing it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            app.cards_handle().chains.get("ses_test").is_some(),
            "an unplaceable cursor must never settle and release the record"
        );
        if backend.transcript_calls.lock().await.len() >= 3 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reap never observed the kept record"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    assert!(
        patches_to(&platform, "om_frozen").await.is_empty(),
        "an unplaceable cursor never settles the old card: {:?}",
        platform.calls.lock().await
    );
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the record is kept for a later read that places the cursor");
    assert!(
        record.cursor.is_some(),
        "the unplaceable cursor is kept, never dropped"
    );
}

/// A cursor-bearing orphan whose first seed read fails records its undelivered
/// tail as a durable GAP (spec #561, review #569): the chain's cursor keeps
/// advancing with the new Turn's own content — a restart must never replay it
/// — while the gap waits for a read that can place it, lands on the new card
/// exactly once, and is cleared by the first confirmed write that carries it.
#[tokio::test]
async fn a_failed_seed_read_keeps_a_cursor_bearing_tail_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let missed = "停机前没送达的尾巴。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_orphan"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    // Phase A: the read does not carry the cursor's message, so the pending
    // seed cannot resolve while the new Turn's own content still streams.
    let phase_a = |answer: &str| {
        SessionTranscript::new(vec![
            user("msg_cola_new", new_anchor, "新问题"),
            assistant_in_flight(new_anchor + 1_000, answer),
        ])
    };
    let phase_b = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        TranscriptMessage {
            id: MessageId::new("msg_a_orphan"),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created: orphan_anchor + 500,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Text(TextPart {
                text: full.clone(),
                started_at: Some(orphan_anchor + 500),
            })],
        },
        user("msg_cola_new", new_anchor, "新问题"),
        assistant_in_flight(new_anchor + 1_000, "新回答一。新回答二。"),
    ]);
    let (app, platform, backend, gate) =
        seeded_app(&session_file, phase_a("新回答一。"), SessionStatus::Busy).await;
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![phase_a("新回答一。"), phase_a("新回答一。新回答二。")],
        )
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Exactly the SEED's read fails; every later read serves normally.
    backend.fail_transcript_reads(1);
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    // The Turn's own content lands across two writes: by the second, the first
    // confirmed write's cursor decision has run.
    wait_for_card_text(&platform, "新回答二。").await;
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the fresh Turn's record");
    assert_eq!(
        record
            .cursor
            .clone()
            .expect("the carried cursor survives")
            .frontier,
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_2001000"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(new_anchor + 1_000),
            delivered_chars: "新回答一。新回答二。".chars().count(),
            prefix_digest: Some(cursor_prefix_digest("新回答一。新回答二。")),
        }),
        "the delivered answer advances the durable cursor normally"
    );
    let gap = record
        .pending_gap
        .clone()
        .expect("the unplaceable gap is a durable fact");
    assert_eq!(
        gap.cursor.frontier.as_ref().map(|frontier| &frontier.message_id),
        Some(&MessageId::new("msg_a_orphan"))
    );
    assert_eq!(gap.anchor.message_id, MessageId::new("msg_cola_anchor"));
    assert!(
        !card_text(platform.updated_cards().await.last().expect("a card")).contains(missed),
        "the unseeded tail is not on the card yet"
    );

    // A read that carries the cursor now lands the pending seed: the orphan's
    // undelivered tail renders on the new card exactly once.
    backend
        .given_transcript_after_build("ses_test", vec![phase_b])
        .await;
    wait_for_card_text(&platform, missed).await;
    let updates = patches_to(&platform, "msg_reply").await;
    let text = card_text(updates.last().expect("the new card was patched"));
    assert_eq!(
        text.matches(missed).count(),
        1,
        "the orphan's tail lands exactly once: {text}"
    );
    assert!(
        !text.contains(delivered),
        "the delivered prefix is never repeated: {text}"
    );
    assert!(
        text.contains("新回答二。"),
        "the Turn's own content stays: {text}"
    );
    // The confirmed write that carried the gap clears the durable fact: a
    // restart renders it never again (spec #561, review #569).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let cleared = app
            .cards_handle()
            .chains
            .get("ses_test")
            .is_none_or(|record| record.pending_gap.is_none());
        if cleared {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the durable gap is never cleared"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(turn);
}

/// Wait until ANY card — a projection/create or an in-place update — carries
/// `needle`, or panic after 5 s. A stopped live chain's follow may post
/// continuations instead of PATCHing the last slice.
async fn wait_for_any_card_text(platform: &RecordingPlatform, needle: &str) {
    let probe = async {
        loop {
            let posted = platform
                .replied_cards()
                .await
                .iter()
                .any(|card| card_text(card).contains(needle));
            let updated = platform
                .updated_cards()
                .await
                .iter()
                .any(|card| card_text(card).contains(needle));
            if posted || updated {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("no card ever carried {needle:?}"));
}

/// A live projection whose CONTINUATION create failed still gets followed
/// (spec #561, review #569): the last landed slice's card is handed to the
/// follow, whose normal flush/continuation machinery carries the remaining
/// delta and the run's future output — instead of the run freezing until the
/// next cola restart.
#[tokio::test]
async fn a_stopped_live_chain_after_a_failed_continuation_is_still_followed() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let marker = |i: usize| format!("【S{i:02}】");
    let tail: String = (0..40)
        .map(|i| format!("{}{}", marker(i), "长".repeat(400)))
        .collect();
    let full = format!("{delivered}{tail}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, &full),
    ]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // The first create lands; the second (a continuation slice) fails.
    platform.given_reply_card_outcome(crate::bridge::test_support::ReplyOutcome::Lands);
    platform.given_reply_card_outcome(crate::bridge::test_support::ReplyOutcome::Ambiguous);

    spawn_sync(&app);
    // The follow carries the slices the stopped projection never posted.
    wait_for_posted_text(&platform, &marker(39)).await;

    // The run streams on: its future output reaches the same chain.
    let grown = format!("{full}重启之后的尾巴。");
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant_in_flight(2_000, &grown),
            ])],
        )
        .await;
    wait_for_any_card_text(&platform, "重启之后的尾巴。").await;
}

/// A live projection that stops at the CHAIN BOUND is still followed (spec
/// #561, review #569): the last landed slice's card carries the follow, so the
/// remaining delta and the run's future output land instead of freezing until
/// the next cola restart.
#[tokio::test]
async fn a_chain_bound_stop_of_a_live_projection_is_still_followed() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let marker = |i: usize| format!("【S{i:03}】");
    let tail: String = (0..160)
        .map(|i| format!("{}{}", marker(i), "长".repeat(500)))
        .collect();
    let full = format!("{delivered}{tail}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let running = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant_in_flight(2_000, &full),
    ]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, running, Some(SessionStatus::Busy)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    // The projection stops at the chain bound; the follow carries the rest.
    wait_for_posted_text(&platform, &marker(159)).await;

    // The run streams on: its future output reaches the same chain.
    let grown = format!("{full}重启之后的尾巴。");
    backend
        .given_transcript_after_build(
            "ses_test",
            vec![SessionTranscript::new(vec![
                user("msg_cola_anchor", 1_000, "问题"),
                assistant_in_flight(2_000, &grown),
            ])],
        )
        .await;
    wait_for_any_card_text(&platform, "重启之后的尾巴。").await;
}

/// The message-first race with an identical answer (spec #561, review #569):
/// the orphan's delivered text never replays, but the NEW Turn's own part with
/// the same content still renders — the seeded delivery must not suppress the
/// new answer.
#[tokio::test]
async fn an_identical_new_answer_renders_after_the_seeded_takeover() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let answer = "同一个答案。";
    // The orphan record's cursor covers its whole answer: nothing is left to
    // seed, but the content was delivered.
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_orphan"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: answer.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(answer)),
        }),
        &[],
    );
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        TranscriptMessage {
            id: MessageId::new("msg_a_orphan"),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created: orphan_anchor + 500,
                completed: Some(orphan_anchor + 900),
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Text(TextPart {
                text: answer.to_string(),
                started_at: Some(orphan_anchor + 500),
            })],
        },
        user("msg_cola_new", new_anchor, "新问题"),
        // The NEW Turn's own answer: the same text.
        assistant(new_anchor + 1_000, answer),
    ]);
    let (app, platform, _backend, gate) = seeded_app(&session_file, live, SessionStatus::Idle).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    let cards = platform.updated_cards().await;
    let text = card_text(cards.last().expect("the new Turn's card"));
    assert_eq!(
        text.matches(answer).count(),
        1,
        "the new Turn's identical answer renders exactly once: {text}"
    );
}

/// A crash after the successor create landed but before the takeover re-pointed
/// the record (spec #561, review #569): the next process still sees the old
/// card and cursor, but the write-ahead intent the projection persisted BEFORE
/// the create makes the attempt single-shot across lives — no second successor
/// is ever posted, and the old card takes the documented state-repair path.
#[tokio::test]
async fn a_crash_after_the_successor_create_never_re_posts_on_restart() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    // The state the crash leaves: the create landed (nothing re-pointed the
    // record) and only the intent is durable.
    crate::bridge::chain::ChainRecords::load(session_file.with_file_name("chain_records.json"))
        .note_projection_intent("ses_test", "om_frozen");

    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &format!("{delivered}后半段。")),
    ])
    .with_executions(vec![execution(2_500)]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    // The reap wrote the old card: on the fix its state-repair settle (✅); on
    // the bug the projection's collect of a re-posted successor.
    let _ = &backend;
    wait_for_update(
        &platform,
        "om_frozen",
        "the reap's write to the old card",
        |card| card_header(card).contains("✅") || card_header(card).contains("已由新卡片接管"),
    )
    .await;

    assert_eq!(
        card_posts(&platform).await,
        0,
        "the successor create is never re-posted after a crash: {:?}",
        platform.calls.lock().await
    );
    // The old card follows the documented state-repair path, and the record
    // releases.
    let settled = last_update_of(&platform, "om_frozen")
        .await
        .expect("the old card is written");
    assert_eq!(
        card_header(&settled),
        "✅ 完成",
        "the old card takes the state-repair path: {settled}"
    );
    wait_for_record_gone(&app, "ses_test").await;
}

/// A DEFINITE create failure with no fallback target left (no Chat mapping,
/// review #569 + issue #580) leaves no durable intent: the platform proved it
/// created no message anywhere, so a restart may retry and the missed tail
/// still lands exactly once.
#[tokio::test]
async fn a_definite_refusal_leaves_no_durable_intent_across_a_restart() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    let missed = "后半段是在停机期间写完的。";
    let full = format!("{delivered}{missed}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, &full),
    ])
    .with_executions(vec![execution(2_500)]);
    // Life 1: the create is refused with an explicit 4xx (no message made)
    // and the process dies before the retry lands. No session mapping, so the
    // recorded card is the only target and nothing falls through to a Chat.
    {
        let (app, platform, _backend) = restarted_app_unmapped(&session_file, transcript.clone()).await;
        platform.given_card_view("om_frozen", realistic_card_view());
        platform.given_reply_card_outcome(crate::bridge::test_support::ReplyOutcome::Refused(400));
        spawn_sync_with_timeout(&app, 300);
        wait_for_transcript_reads(&_backend, "ses_test", 1).await;
    }
    let persisted =
        crate::bridge::chain::ChainRecords::load(session_file.with_file_name("chain_records.json"));
    assert!(
        !persisted
            .get("ses_test")
            .expect("the refused attempt keeps the record")
            .projection_intent,
        "a definite non-delivery leaves no durable intent"
    );

    // Life 2: the restart retries and the missed tail lands exactly once.
    let (app, platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    spawn_sync(&app);
    wait_for_posted_text(&platform, missed).await;
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "the retry is the once-and-only post: {:?}",
        platform.calls.lock().await
    );
}

/// A capped transcript read never lets an ended projection settle as complete
/// (spec #561, review #569): the tail beyond the backend's page cap is unseen,
/// so the old card keeps waiting for a complete read instead of freezing
/// without it.
#[tokio::test]
async fn a_truncated_read_never_settles_an_ended_record_as_complete() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let delivered = "已经写了一半。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(1_000),
        Some("/work"),
        Some(text_frontier(delivered)),
        &[],
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, delivered),
    ])
    .with_executions(vec![execution(2_500)])
    .with_truncated();
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    platform.given_card_view("om_frozen", realistic_card_view());

    spawn_sync(&app);
    // The reap either settles and releases the record (the bug this test
    // forbids) or keeps it and keeps observing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            app.cards_handle().chains.get("ses_test").is_some(),
            "a truncated read must never settle and release the record"
        );
        if backend.transcript_calls.lock().await.len() >= 3 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reap never observed the kept record"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    assert!(
        patches_to(&platform, "om_frozen").await.is_empty(),
        "a truncated read never settles the old card: {:?}",
        platform.calls.lock().await
    );
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the record waits for a complete read");
    assert!(record.cursor.is_some());
}

/// A restart while the pending orphan gap is unresolved renders only the gap
/// (spec #561, review #569): the durable gap records the orphan's undelivered
/// tail, the Rendered Cursor advances with the delivered answer, and the
/// recovery never replays that answer.
#[tokio::test]
async fn a_restart_while_the_gap_is_pending_renders_only_the_gap() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let gap_tail = "停机前没写的尾巴。";
    let full = format!("{delivered}{gap_tail}");
    let answer = "新回答一。新回答二。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new("msg_a_orphan"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    // Phase A (life 1): the read does not carry the orphan's message, so the
    // gap cannot resolve; the new Turn's own answer still streams.
    let phase_a = SessionTranscript::new(vec![
        user("msg_cola_new", new_anchor, "新问题"),
        assistant_in_flight(new_anchor + 1_000, answer),
    ]);
    let (app, platform, backend, gate) = seeded_app(&session_file, phase_a, SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Exactly the SEED's read fails; every later read serves normally.
    backend.fail_transcript_reads(1);
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, answer).await;
    // The delivered answer advanced the durable cursor normally, and the
    // unplaceable gap is a durable fact of its own.
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the fresh Turn's record");
    assert_eq!(
        record
            .cursor
            .as_ref()
            .and_then(|cursor| cursor.frontier.as_ref())
            .map(|frontier| frontier.message_id.clone()),
        Some(MessageId::new(format!("msg_a_{}", new_anchor + 1_000))),
        "the delivered answer advances the durable cursor"
    );
    assert!(
        record.pending_gap.is_some(),
        "the unplaceable gap is a durable fact: {record:?}"
    );
    turn.abort();
    drop(app); // life 1 ends: a crash before any read placed the gap

    // Life 2: the read carries the orphan's message, so the projection renders
    // the gap — and never the answer the old card already showed.
    let life2 = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        TranscriptMessage {
            id: MessageId::new("msg_a_orphan"),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created: orphan_anchor + 500,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Text(TextPart {
                text: full.clone(),
                started_at: Some(orphan_anchor + 500),
            })],
        },
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, answer),
    ])
    .with_executions(vec![execution(orphan_anchor + 2_500)]);
    let (app2, platform2, _backend2) =
        restarted_app_with_backend(&session_file, life2, Some(SessionStatus::Idle)).await;
    platform2.given_card_view("msg_reply", realistic_card_view());

    spawn_sync(&app2);
    let (successor, successor_text) = wait_for_projection(&platform2, "msg_1").await;
    assert_eq!(
        successor_text.matches(gap_tail).count(),
        1,
        "the gap lands exactly once on the successor: {successor}"
    );
    assert!(
        !successor_text.contains(answer),
        "the delivered answer is never replayed on a restart: {successor}"
    );
}

/// A fresh Turn that takes over a record still owing an orphan gap carries the
/// durable fact (spec #561, review #569): the re-point rewrites the record, so
/// the gap is re-homed onto the new one and its tail renders on this card
/// exactly once — while the turns the durable cursor already covers are never
/// replayed — and the first confirmed write clears it.
#[tokio::test]
async fn a_fresh_turn_carries_a_pending_gap() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let prev_anchor = 2_000_000;
    let fresh_anchor = 3_000_000;
    let delivered = "已经写了一半。";
    let missed = "停机前没送达的尾巴。";
    let full = format!("{delivered}{missed}");
    let answer = "旧回答。";
    // The record names the previous Turn's card and its cursor already covers
    // that Turn's delivered answer, while the orphan gap is still owed.
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_prev",
        Some(prev_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new(format!("msg_a_{}", prev_anchor + 500)),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(prev_anchor + 500),
            delivered_chars: answer.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(answer)),
        }),
        &[],
    );
    ChainRecords::load(sidecar(&session_file)).note_pending_gap(
        "ses_test",
        "om_frozen",
        &PendingGap {
            cursor: RenderedCursor {
                frontier: Some(CursorFrontier {
                    message_id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
                    part_index: 0,
                    kind: CursorPartKind::Text,
                    started_at: Some(orphan_anchor + 500),
                    delivered_chars: delivered.chars().count(),
                    prefix_digest: Some(cursor_prefix_digest(delivered)),
                }),
                live_calls: Default::default(),
            },
            anchor: crate::backend::TurnAnchor {
                message_id: MessageId::new("msg_cola_anchor"),
                created_ms: orphan_anchor,
            },
            bound: Some(MessageId::new("msg_cola_prev")),
        },
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        assistant_in_flight(orphan_anchor + 500, &full),
        user("msg_cola_prev", prev_anchor, "旧问题"),
        assistant(prev_anchor + 500, answer),
        user("msg_cola_fresh", fresh_anchor, "新问题"),
        assistant_in_flight(fresh_anchor + 1_000, "新回答。"),
    ]);
    let (app, platform, _backend, gate) = seeded_app(&session_file, transcript, SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    platform.given_card_view("msg_reply", realistic_card_view());
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_fresh".into());
    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, missed).await;
    let updates = patches_to(&platform, "msg_reply").await;
    let text = card_text(updates.last().expect("the carried gap lands on the card"));
    assert_eq!(
        text.matches(missed).count(),
        1,
        "the carried gap renders exactly once: {text}"
    );
    assert!(
        !text.contains(delivered),
        "the gap's delivered prefix is never repeated: {text}"
    );
    assert!(
        !text.contains(answer),
        "the turn the durable cursor already covers is never replayed: {text}"
    );
    assert!(
        text.contains("新回答。"),
        "the fresh Turn's own content streams: {text}"
    );
    // The write that carried the gap clears the durable fact: no later Turn
    // re-renders the tail.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let cleared = app
            .cards_handle()
            .chains
            .get("ses_test")
            .is_none_or(|record| record.pending_gap.is_none());
        if cleared {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the carried gap is never cleared"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(turn);
}

/// A gap large enough to split across cards keeps its remaining tail owed
/// (spec #561, review #569): the first slice's confirmation only ADVANCES the
/// durable gap — it never clears it — so a restart before later slices land
/// re-renders the remainder exactly once, repeating nothing a card showed.
#[tokio::test]
async fn a_split_gap_resumes_from_its_confirmed_head_after_a_restart() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let prev_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let marker = |i: usize| format!("【S{i:02}】");
    let answer = "旧回答。";
    // The record names the previous Turn's card: its cursor covers that Turn's
    // delivered answer while the orphan's undelivered tail is owed as a gap.
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_prev",
        Some(prev_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new(format!("msg_a_{}", prev_anchor + 500)),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(prev_anchor + 500),
            delivered_chars: answer.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(answer)),
        }),
        &[],
    );
    ChainRecords::load(sidecar(&session_file)).note_pending_gap(
        "ses_test",
        "om_frozen",
        &PendingGap {
            cursor: RenderedCursor {
                frontier: Some(CursorFrontier {
                    message_id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
                    part_index: 0,
                    kind: CursorPartKind::Text,
                    started_at: Some(orphan_anchor + 500),
                    delivered_chars: delivered.chars().count(),
                    prefix_digest: Some(cursor_prefix_digest(delivered)),
                }),
                live_calls: Default::default(),
            },
            anchor: crate::backend::TurnAnchor {
                message_id: MessageId::new("msg_cola_anchor"),
                created_ms: orphan_anchor,
            },
            bound: Some(MessageId::new("msg_cola_prev")),
        },
    );
    // The orphan's tail is MANY parts, each its own timeline entry: the gap
    // then spans more than one card slice, which is the case under test.
    let transcript = || {
        let mut parts = vec![Part::Text(TextPart {
            text: delivered.to_string(),
            started_at: Some(orphan_anchor + 500),
        })];
        for i in 0..40usize {
            parts.push(Part::Text(TextPart {
                text: format!("{}{}", marker(i), "长".repeat(400)),
                started_at: Some(orphan_anchor + 501 + i as i64),
            }));
        }
        SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            TranscriptMessage {
                id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
                role: MessageRole::Assistant,
                time: Some(MessageTime {
                    created: orphan_anchor + 500,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts,
            },
            user("msg_cola_prev", prev_anchor, "旧问题"),
            assistant(prev_anchor + 500, answer),
        ])
        .with_executions(vec![execution(orphan_anchor + 2_500)])
    };

    // Life 1: the first slice lands, every later create fails DEFINITELY (no
    // card) — with no session mapping the reply target is the only rung, so
    // the rest of the gap stays owed.
    let (app, platform, _backend) = restarted_app_unmapped(&session_file, transcript()).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    platform.given_reply_card_outcome(crate::bridge::test_support::ReplyOutcome::Lands);
    for _ in 0..200 {
        platform.given_reply_card_outcome(crate::bridge::test_support::ReplyOutcome::Refused(400));
    }
    spawn_sync(&app);
    wait_for_posted_text(&platform, &marker(0)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let posted = platform.replied_cards().await;
    assert_eq!(
        posted.len(),
        1,
        "only the first slice lands: {:?}",
        platform.calls.lock().await
    );
    assert!(
        !card_text(&posted[0]).contains(&marker(39)),
        "the landed slice is a prefix"
    );
    // The part a card already showed must never be owed again, and the rest of
    // the tail must still be owed: the durable gap carries the confirmed head.
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the stopped chain stays for the next life");
    let gap = record
        .pending_gap
        .clone()
        .expect("a split gap stays owed until its end lands");
    assert!(
        gap.cursor
            .frontier
            .as_ref()
            .is_some_and(|frontier| frontier.delivered_chars > delivered.chars().count()),
        "the confirmed slice advanced the gap: {gap:?}"
    );
    drop(app);

    // Life 2: a restart resumes from the advanced gap and lands the remainder —
    // exactly once (still unmapped: the reply ladder is the recorded card).
    let (app2, platform2, _backend2) = restarted_app_unmapped(&session_file, transcript()).await;
    // Every create takes the mock's default id: the record names "msg_reply".
    platform2.given_card_view("msg_reply", realistic_card_view());
    spawn_sync(&app2);
    wait_for_posted_text(&platform2, &marker(39)).await;
    wait_for_record_gone(&app2, "ses_test").await;
    let resumed = platform2.replied_cards().await;
    let all: String = resumed.iter().map(card_text).collect::<Vec<_>>().join("\n");
    assert_eq!(
        all.matches(&marker(0)).count(),
        0,
        "nothing already delivered repeats: {all}"
    );
    assert_eq!(
        all.matches(&marker(39)).count(),
        1,
        "the remaining gap tail lands exactly once: {all}"
    );
    assert!(
        card_header(resumed.last().expect("the resumed chain posted")).contains("✅"),
        "the resumed chain ends on the transcript's true ending"
    );
}

/// Two text parts of one message sharing a server time (spec #561, review
/// #569): the render keeps each part's own source, so the confirmed cursor
/// resolves on the next life and NOTHING re-renders — the card the old life
/// showed already carries both parts. Merged into one entry under the first
/// part's digest, the second life's projection re-renders both onto a
/// successor card.
#[tokio::test]
async fn a_restart_re_renders_nothing_for_two_parts_at_one_server_time() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let first = "第一段回答。";
    let second = "第二段回答。";
    let transcript = || {
        SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "第一条消息"),
            two_parts_at_one_time(first, second),
        ])
    };
    // Life 1: a live turn renders both parts and confirms its cursor.
    let (app, platform, _backend, gate) = seeded_app(&session_file, transcript(), SessionStatus::Busy).await;
    platform.given_card_view("msg_reply", realistic_card_view());
    gate.add_permits(1);
    let mut context = ctx("ses_test", "第一条消息");
    context.cola_message_id = Some("msg_cola_anchor".into());
    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, second).await;
    let cursor = wait_for_cursor(&app, |cursor| cursor.frontier.is_some()).await;
    assert_eq!(
        cursor.frontier.as_ref().map(|frontier| frontier.part_index),
        Some(1),
        "the second part is its own frontier"
    );
    turn.abort();
    drop(app);

    // Life 2: the restart settles the card in place — no successor re-renders
    // what the old card already showed.
    let (app2, platform2, _backend2) =
        restarted_app_with_backend(&session_file, transcript(), Some(SessionStatus::Idle)).await;
    platform2.given_card_view("msg_reply", realistic_card_view());
    spawn_sync(&app2);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            platform2.replied_cards().await.is_empty(),
            "a restart re-renders nothing the old card showed: {:?}",
            platform2.calls.lock().await
        );
        let settled = patches_to(&platform2, "msg_reply")
            .await
            .iter()
            .any(|card| card_header(card).contains("✅"));
        if settled {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the recorded card never settles in place"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let settled = last_update_of(&platform2, "msg_reply")
        .await
        .expect("the in-place settle");
    let text = card_text(&settled);
    assert!(
        !text.contains(first) && !text.contains(second),
        "the delivered parts are never re-rendered: {text}"
    );
}

/// A takeover that resolves its seed still OWES the gap durably (spec #561,
/// review #569): the old card is collected at the takeover, and a crash before
/// the new Turn's first confirmed write would otherwise leave the record with
/// the old cursor and the new Turn's anchor — a recovery that cannot see the
/// orphaned Turn's window and omits its undelivered tail. The tail lands
/// exactly once after the crash.
#[tokio::test]
async fn a_crash_after_a_resolved_seed_still_lands_the_orphan_tail() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let tail = "停机前没送达的尾巴。";
    let full = format!("{delivered}{tail}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    let transcript = || {
        SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            TranscriptMessage {
                id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
                role: MessageRole::Assistant,
                time: Some(MessageTime {
                    created: orphan_anchor + 500,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::Text(TextPart {
                    text: full.clone(),
                    started_at: Some(orphan_anchor + 500),
                })],
            },
            user("msg_cola_new", new_anchor, "新问题"),
            assistant(new_anchor + 1_000, "新回答。"),
        ])
        .with_executions(vec![execution(orphan_anchor + 2_500)])
    };

    // Life 1: the fresh Turn takes the chain over and its seed RESOLVES — the
    // tail renders on the new card — but every card write fails, so nothing is
    // confirmed before the crash.
    let (app, platform, _backend, gate) = seeded_app(&session_file, transcript(), SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    platform
        .fail_update_transport_count
        .store(100, std::sync::atomic::Ordering::SeqCst);
    gate.add_permits(1);
    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_update_attempts(&platform, "msg_reply", 1).await;
    turn.abort();
    drop(app); // the crash: the tail is on a card that was never confirmed

    // Life 2: the recovery still lands the orphan's undelivered tail.
    let (app2, platform2, _backend2) =
        restarted_app_with_backend(&session_file, transcript(), Some(SessionStatus::Idle)).await;
    platform2.given_card_view("msg_reply", realistic_card_view());
    spawn_sync(&app2);
    wait_for_posted_text(&platform2, tail).await;
    let posted = platform2.replied_cards().await;
    let all: String = posted.iter().map(card_text).collect::<Vec<_>>().join("\n");
    assert_eq!(
        all.matches(tail).count(),
        1,
        "the orphaned tail lands exactly once after the crash: {all}"
    );
    assert!(
        !all.contains(delivered),
        "the delivered prefix is never repeated: {all}"
    );
}

/// A confirmation carries the cursor advance AND the gap advance/clear in ONE
/// durable write (spec #561, review #569): a crash between two writes would
/// leave the cursor ahead of the gap, and the recovery would re-render gap
/// content a card already showed.
#[tokio::test]
async fn one_confirmed_write_advances_cursor_and_gap_together() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let tail = "停机前没送达的尾巴。";
    let full = format!("{delivered}{tail}");
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    ChainRecords::load(sidecar(&session_file)).note_pending_gap(
        "ses_test",
        "om_frozen",
        &PendingGap {
            cursor: RenderedCursor {
                frontier: Some(CursorFrontier {
                    message_id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
                    part_index: 0,
                    kind: CursorPartKind::Text,
                    started_at: Some(orphan_anchor + 500),
                    delivered_chars: delivered.chars().count(),
                    prefix_digest: Some(cursor_prefix_digest(delivered)),
                }),
                live_calls: Default::default(),
            },
            anchor: crate::backend::TurnAnchor {
                message_id: MessageId::new("msg_cola_anchor"),
                created_ms: orphan_anchor,
            },
            bound: Some(MessageId::new("msg_cola_new")),
        },
    );
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        TranscriptMessage {
            id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created: orphan_anchor + 500,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![Part::Text(TextPart {
                text: full.clone(),
                started_at: Some(orphan_anchor + 500),
            })],
        },
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答。"),
    ]);
    let (app, platform, _backend, gate) = seeded_app(&session_file, transcript, SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Every card write fails at the transport until the drain.
    platform
        .fail_update_transport_count
        .store(100, std::sync::atomic::Ordering::SeqCst);
    gate.add_permits(1);
    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, tail).await;
    wait_for_update_attempts(&platform, "msg_reply", 1).await;

    let chains = app.cards_handle().chains.clone();
    let writes0 = chains.writes();
    // Feishu returns: the drain delivers the failed body, and the reconcile
    // confirms it. Cursor and gap must move in the SAME record write.
    platform
        .fail_update_transport_count
        .store(0, std::sync::atomic::Ordering::SeqCst);
    app.core.feishu.drain_pending_card_updates(true).await;
    crate::bridge::turn::reconcile_staged_cursors(&app.cards_handle()).await;

    assert_eq!(
        chains.writes() - writes0,
        1,
        "the confirmation persists one record write"
    );
    let record = app.cards_handle().chains.get("ses_test").expect("the record");
    assert!(
        record.pending_gap.is_none(),
        "the same write consumed the gap: {record:?}"
    );
    assert_eq!(
        record
            .cursor
            .and_then(|cursor| cursor.frontier)
            .map(|frontier| (frontier.message_id, frontier.delivered_chars)),
        Some((
            MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
            delivered.chars().count()
        )),
        "and keeps the chain's own frontier: gap content never moves it"
    );
    drop(turn);
}

/// A reasoning part longer than the card's reasoning cap (spec #561, review
/// #569): the confirmed cursor records only the characters the card displayed,
/// so a restart renders the undisclosed suffix on the successor. Before the
/// fix the cursor claimed the whole part, the frontier resolved at its full
/// length, and the recovery settled in place without ever showing the rest.
#[tokio::test]
async fn a_restart_renders_a_long_reasoning_parts_undisclosed_suffix() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let shown = "【已显示的前缀】";
    let hidden = "【未显示的后缀】";
    let reasoning = format!("{shown}{}{hidden}", "隐".repeat(800));
    let transcript = || {
        SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "问题"),
            typed_message(
                "msg_a_2000",
                MessageRole::Assistant,
                Some(2_000),
                vec![
                    Part::Reasoning(ReasoningPart {
                        text: reasoning.clone(),
                        started_at: Some(2_000),
                    }),
                    Part::StepFinish(StepFinish {
                        reason: FinishReason::Stop,
                    }),
                ],
            ),
        ])
        .with_executions(vec![execution(2_500)])
    };

    // Life 1: a live turn renders the reasoning part; the card element shows
    // its cap, and the confirmed cursor must record no more than that.
    let (app, platform, _backend, gate) = seeded_app(&session_file, transcript(), SessionStatus::Busy).await;
    platform.given_card_view("msg_reply", realistic_card_view());
    gate.add_permits(1);
    let mut context = ctx("ses_test", "问题");
    context.cola_message_id = Some("msg_cola_anchor".into());
    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, shown).await;
    let cursor = wait_for_cursor(&app, |cursor| cursor.frontier.is_some()).await;
    assert_eq!(
        cursor
            .frontier
            .as_ref()
            .map(|frontier| (frontier.kind, frontier.delivered_chars)),
        Some((CursorPartKind::Reasoning, crate::feishu::card::REASONING_TEXT_CAP)),
        "the frontier counts only the characters the card displayed"
    );
    turn.abort();
    drop(app);

    // Life 2: the restart renders the undisclosed suffix as a continuation.
    let (app2, platform2, _backend2) =
        restarted_app_with_backend(&session_file, transcript(), Some(SessionStatus::Idle)).await;
    platform2.given_card_view("msg_reply", realistic_card_view());
    spawn_sync(&app2);
    wait_for_posted_text(&platform2, hidden).await;
    let all: String = platform2
        .replied_cards()
        .await
        .iter()
        .map(card_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        all.matches(hidden).count(),
        1,
        "the undisclosed suffix lands exactly once: {all}"
    );
    assert!(!all.contains(shown), "the displayed prefix never repeats: {all}");
}

/// A split continuation's record re-point and its confirmed cursor settle in
/// ONE chains write (spec #561, review #569): two writes leave a window where
/// the record names the continuation card with the predecessor's frontier, and
/// a crash right after Feishu accepted the create makes the next recovery
/// project content the continuation already shows.
#[tokio::test]
async fn a_split_continuation_persists_its_repoint_and_cursor_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        assistant(2_000, "答复。"),
    ]);
    let (app, _platform, _backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Idle)).await;
    // The live card that splits: its accumulator is in the map with the Turn
    // facts the continuation's re-point reads.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_old")).await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
    let delivered = "答复。";
    let cursor = RenderedCursor {
        frontier: Some(CursorFrontier {
            message_id: MessageId::new("msg_a_2000"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(2_000),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        live_calls: Default::default(),
    };
    // The create's stage: a create's id is unknown until it lands, so the
    // stage carries none and the confirmation names the new card.
    let stage = Turn::stage_cursor(&app.cards_handle(), "ses_test", None, &cursor).await;
    let chains = app.cards_handle().chains.clone();
    let writes0 = chains.writes();

    Turn::track_continuation_card(&app.cards_handle(), "ses_test", "om_next", stage).await;

    assert_eq!(
        chains.writes() - writes0,
        1,
        "the re-point and the confirmed cursor persist in one record write"
    );
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the continuation's record");
    assert_eq!(record.card_message_id, "om_next");
    assert_eq!(record.cursor, Some(cursor), "the same write confirms the cursor");
}

/// A truncated message-first takeover must not consume the orphan gap (spec
/// #561, review #569): the read is the oldest prefix, so the successor's Turn
/// message — the gap's window bound — may sit beyond the page cap. Resolving
/// the seed against that prefix let the gap walk treat what it saw as the
/// whole gap, and the first confirmed write cleared the durable fact, losing
/// the messages beyond the cap. The gap stays owed (the visible prefix still
/// renders) and the full tail lands once a complete read arrives.
#[tokio::test]
async fn a_truncated_takeover_keeps_the_gap_until_a_complete_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let visible = "【可见的尾巴】";
    let hidden = "【没看见的尾巴】";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    let orphan_text = |tail: &str| TranscriptMessage {
        id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
        role: MessageRole::Assistant,
        time: Some(MessageTime {
            created: orphan_anchor + 500,
            completed: None,
        }),
        model: None,
        tokens: None,
        error: None,
        parts: vec![Part::Text(TextPart {
            text: format!("{delivered}{tail}"),
            started_at: Some(orphan_anchor + 500),
        })],
    };
    // The truncated read: it stops at the first orphaned message, so the later
    // one — and the successor's Turn message that bounds the gap — are beyond
    // the page cap.
    let phase_a = || {
        SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            orphan_text(visible),
        ])
        .with_truncated()
    };
    // The complete read: both orphaned messages and the new Turn.
    let phase_b = || {
        SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            orphan_text(visible),
            TranscriptMessage {
                id: MessageId::new(format!("msg_a_{}", orphan_anchor + 900)),
                role: MessageRole::Assistant,
                time: Some(MessageTime {
                    created: orphan_anchor + 900,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::Text(TextPart {
                    text: hidden.to_string(),
                    started_at: Some(orphan_anchor + 900),
                })],
            },
            user("msg_cola_new", new_anchor, "新问题"),
            assistant(new_anchor + 1_000, "新回答。"),
        ])
    };
    // Life 1: the takeover reads the truncated prefix. The visible tail
    // renders, and the durable gap must stay owed — nothing may mark it
    // complete off a prefix.
    let (app, platform, _backend, gate) = seeded_app(&session_file, phase_a(), SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    gate.add_permits(1);
    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_card_text(&platform, visible).await;
    let record = app
        .cards_handle()
        .chains
        .get("ses_test")
        .expect("the fresh Turn's record");
    assert!(
        record.pending_gap.is_some(),
        "a truncated read never consumes the gap: {record:?}"
    );
    turn.abort();
    drop(app);

    // Life 2: the complete read shows the gap's end, so the recovery lands the
    // unseen tail — content no read of the NEW Turn's window can reach.
    let (app2, platform2, _backend2) =
        restarted_app_with_backend(&session_file, phase_b(), Some(SessionStatus::Idle)).await;
    platform2.given_card_view("msg_reply", realistic_card_view());
    spawn_sync(&app2);
    wait_for_posted_text(&platform2, hidden).await;
    let all: String = platform2
        .replied_cards()
        .await
        .iter()
        .map(card_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        all.matches(hidden).count(),
        1,
        "the unseen tail lands exactly once: {all}"
    );
    assert!(
        !all.contains(visible),
        "the delivered visible tail never repeats: {all}"
    );
}

/// The takeover records the owed orphan gap in the SAME write that re-points
/// the record (spec #561, review #569): the facts are known at takeover time,
/// so a crash while the takeover's transcript read is still pending must leave
/// the gap already durable. Before the fix the record named the new,
/// not-yet-submitted message with the old cursor and NO gap, and the Unreceived
/// path settled and released the record — losing the orphan's tail.
#[tokio::test]
async fn a_takeover_records_the_gap_before_its_read_completes() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let delivered = "已经写了一半。";
    let tail = "停机前没送达的尾巴。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    let transcript = || {
        SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            TranscriptMessage {
                id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
                role: MessageRole::Assistant,
                time: Some(MessageTime {
                    created: orphan_anchor + 500,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::Text(TextPart {
                    text: format!("{delivered}{tail}"),
                    started_at: Some(orphan_anchor + 500),
                })],
            },
        ])
    };

    // Life 1: the fresh Turn takes over, but its seed read never returns — the
    // crash lands while it is pending.
    let (app, _platform, backend, _gate) = seeded_app(&session_file, transcript(), SessionStatus::Busy).await;
    backend.hold_transcripts();
    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    // The takeover re-pointed the record (its write is before the gated read).
    wait_for_record_card(&app, "ses_test", "msg_reply").await;
    let record = app.cards_handle().chains.get("ses_test").unwrap();
    assert!(
        record.pending_gap.is_some(),
        "the takeover itself owes the gap, before any read: {record:?}"
    );
    turn.abort();
    drop(app);

    // Life 2: a restart whose read never carries the new Turn's message — the
    // Unreceived ending — must keep the record (the gap is still owed).
    let (app2, platform2, backend2) =
        restarted_app_with_backend(&session_file, transcript(), Some(SessionStatus::Idle)).await;
    platform2.given_card_view("msg_reply", realistic_card_view());
    spawn_sync(&app2);
    // The reap runs: the record must survive every pass.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let kept = app2.cards_handle().chains.get("ses_test");
        assert!(
            kept.as_ref().is_some_and(|record| record.pending_gap.is_some()),
            "an Unreceived ending never releases an owed gap: {kept:?}"
        );
        let reads = backend2
            .session_status_reads
            .lock()
            .await
            .iter()
            .filter(|sid| sid.as_str() == "ses_test")
            .count();
        if reads >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reap never observed the kept record"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(app2);

    // Life 3: the next user Turn takes the chain over and lands the tail
    // exactly once.
    let (app3, platform3, _backend3, gate3) =
        seeded_app(&session_file, transcript(), SessionStatus::Busy).await;
    // The new Turn's loading card takes a DIFFERENT id than the record names,
    // so the fresh Turn really takes the chain over.
    platform3.given_reply_id("om_third");
    platform3.given_card_view("msg_reply", realistic_card_view());
    gate3.add_permits(1);
    let mut context = ctx("ses_test", "又一条消息");
    context.cola_message_id = Some("msg_cola_third".into());
    let turn3 = spawn_turn(&app3, context);
    wait_for_card_text(&platform3, tail).await;
    let text = card_text(platform3.updated_cards().await.last().expect("the takeover card"));
    assert_eq!(
        text.matches(tail).count(),
        1,
        "the orphaned tail lands exactly once: {text}"
    );
    assert!(
        !text.contains(delivered),
        "the delivered prefix is never repeated: {text}"
    );
    drop(turn3);
}

/// A fresh Turn's takeover never erases an unresolved projection intent
/// (spec #561, review #569): a projection create may be in flight (its card
/// landed, its takeover lost the race), and the winning chain must keep the
/// single-shot fact — or a restart projects the late card's tail again.
#[tokio::test]
async fn a_takeover_keeps_an_unresolved_projection_intent() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    let new_anchor = 2_000_000;
    let delivered = "已经写了一半。";
    let tail = "停机前没送达的尾巴。";
    seed_cursor_record(
        &session_file,
        "om_frozen",
        "msg_cola_anchor",
        Some(orphan_anchor),
        Some("/work"),
        Some(CursorFrontier {
            message_id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(orphan_anchor + 500),
            delivered_chars: delivered.chars().count(),
            prefix_digest: Some(cursor_prefix_digest(delivered)),
        }),
        &[],
    );
    // The previous life died mid-create: the write-ahead intent is durable.
    assert!(ChainRecords::load(sidecar(&session_file)).note_projection_intent("ses_test", "om_frozen"));
    let transcript = || {
        SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            TranscriptMessage {
                id: MessageId::new(format!("msg_a_{}", orphan_anchor + 500)),
                role: MessageRole::Assistant,
                time: Some(MessageTime {
                    created: orphan_anchor + 500,
                    completed: None,
                }),
                model: None,
                tokens: None,
                error: None,
                parts: vec![Part::Text(TextPart {
                    text: format!("{delivered}{tail}"),
                    started_at: Some(orphan_anchor + 500),
                })],
            },
            user("msg_cola_new", new_anchor, "新问题"),
            assistant(new_anchor + 1_000, "新回答。"),
        ])
    };
    // Life 1: a fresh Turn takes over; the ambiguous create's intent survives.
    let (app, platform, _backend, gate) = seeded_app(&session_file, transcript(), SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    gate.add_permits(1);
    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_record_card(&app, "ses_test", "msg_reply").await;
    let record = app.cards_handle().chains.get("ses_test").unwrap();
    assert!(
        record.projection_intent,
        "a takeover never erases an unresolved intent: {record:?}"
    );
    turn.abort();
    drop(app);

    // Life 2: the restart must never project again — the single-shot intent
    // blocks it, and the old card takes the in-place repair instead.
    let (app2, platform2, _backend2) =
        restarted_app_with_backend(&session_file, transcript(), Some(SessionStatus::Idle)).await;
    platform2.given_card_view("msg_reply", realistic_card_view());
    spawn_sync(&app2);
    // Either the old card is repaired in place (the fix) or a successor is
    // posted again (the bug this test forbids).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert_eq!(
            card_posts(&platform2).await,
            0,
            "the ambiguous intent blocks a second projection: {:?}",
            platform2.calls.lock().await
        );
        let repaired = patches_to(&platform2, "msg_reply")
            .await
            .iter()
            .any(|card| card_header(card).contains("✅"));
        if repaired {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the record never repairs the old card in place"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !card_text(
            &last_update_of(&platform2, "msg_reply")
                .await
                .expect("the repaired card")
        )
        .contains(tail),
        "the late card's tail is never projected again"
    );
}
