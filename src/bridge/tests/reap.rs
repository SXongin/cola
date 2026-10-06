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
    assistant, ctx, script_transcript, scripted_app, spawn_sync, spawn_sync_with_timeout, spawn_turn, user,
    wait_for_card_header, wait_for_card_text,
};
use crate::backend::{
    ContentBlock, MessageId, MessageRole, MessageTime, Part, SessionTranscript, ToolCall, ToolIdentity,
    ToolOutput, ToolStatus, TranscriptMessage, TranscriptTail,
};
use crate::bridge::chain::ChainRecords;
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
/// panic after 5 s — the observation a stamp test needs before it asserts on
/// the writes a parked stamp PATCH let queue up behind it.
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

/// Wait until the record's restart stamp is marked permanently refused by
/// Feishu (#522), or panic after 5 s — proof the rejected attempt resolved
/// before the passes that must not retry it are asserted on.
async fn wait_for_rejected_stamp(app: &Arc<App>, session_id: &str) {
    let probe = async {
        loop {
            if app
                .cards_handle()
                .chains
                .get(session_id)
                .is_some_and(|record| record.restart_stamp_rejected)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("the stamp was never marked permanently refused"));
}

/// Wait until the fresh-Turn takeover's collect recorded its keep rule for
/// `card_message_id` (ADR-0068), or panic after 5 s. The recorded rule is the
/// observed point the takeover's collect reached its decision — before it
/// queues behind a parked stamp PATCH — so the stamp's post-PATCH ownership
/// check can no longer miss the successor.
async fn wait_for_predecessor_keep(
    app: &Arc<App>,
    session_id: &str,
    card_message_id: &str,
    strip_running_panels: bool,
) {
    let probe = async {
        loop {
            let recorded = app
                .cards_handle()
                .chains
                .get(session_id)
                .and_then(|record| record.predecessor_keep)
                .map(|keep| (keep.card_message_id, keep.strip_running_panels));
            if recorded == Some((card_message_id.to_string(), strip_running_panels)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("the takeover never recorded its collect rule for {card_message_id}"));
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
    // Several more observed passes, each reading the live status: the stamp
    // is one per process life, so no further PATCH may follow.
    wait_for_status_reads(&backend, "ses_test", 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "the restart stamp lands exactly once: {patches:?}"
    );
    assert_eq!(card_header(&patches[0]), "⏳ 已重启，等待运行结束");
    assert_preserved_body(&patches[0]);
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a still-live Session keeps its record"
    );
}

/// A successor that arms while the stamp's view read is in flight collects
/// the orphan before the read returns; the stamp re-checks ownership before
/// its PATCH and yields to that later terminal — an interim status never
/// overwrites a takeover (#443).
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
    );

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // Park the stamp's view read. A generous bound: the takeover below runs
    // while the read is parked, and this test must not race the bound.
    let (entered, release) = platform.pause("card_view", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // A successor arms over the orphan while the stamp's read is parked:
    // attach first, then collect — the order `take_over_card` owns.
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

/// A successor that owns the session by the time the stamp's PATCH returns is
/// re-collected: the post-PATCH ownership re-check gives the takeover the
/// card's last word even when the stamp's write landed over its collect
/// (#443). The check-to-PATCH interleaving itself cannot be parked (the
/// card's delivery lock serializes the two actual writes), so this test pins
/// the repair branch the interleaving would take.
#[tokio::test]
async fn a_successor_owning_the_session_recollects_after_the_stamp() {
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
    );

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;
    backend
        .set_session_status("ses_other", Some(SessionStatus::Busy))
        .await;
    // The orphan's view carries a running `⏳` panel and the ledger too: the
    // plain takeover settles `Everything`, so the repair must keep the whole
    // preserved body (ADR-0068's strip is scoped to the fresh-Turn path and
    // its repair).
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // Park the stamp's PATCH after its pre-PATCH ownership check passed.
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    // The successor arms and re-points the record while the stamp is parked;
    // its collect queues on the card's delivery lock behind the stamp.
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
    // Three more observed passes: the repair landed with the parked pass.
    wait_for_status_reads(&backend, "ses_other", before + 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        3,
        "the stamp, the takeover's collect and the post-PATCH re-collect: {patches:?}"
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
    // so the repair keeps today's preserved body — the running panel and the
    // ledger both stay, exactly as every Wake/external collect always did.
    let text = card_text(patches.last().unwrap());
    assert!(
        text.contains("⏳ shell") && text.contains("⏳ 后台任务") && text.contains("**正文** 已经写完的部分"),
        "the plain takeover's repair preserves the whole body: {text}"
    );
}

// --- The restart carry (ADR-0068) ------------------------------------------
//
// A fresh Turn's takeover over an orphaned card hands the orphan Turn's
// still-running tool calls to the successor: the bounded, newest-first tail
// read is scoped by the durable record's anchor, the calls ride the
// successor's live tail by identity, and a failed/timeout/cap-stopped read
// carries nothing. A tool that settled while cola was down stays out of scope
// (#505); the carry is display-only.

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

/// The whole-card view of an orphaned live card carrying the live tail
/// ADR-0068's takeover collect decides on: written content, a running tool
/// panel (`⏳`), a settled backgrounded launch (`🌙`) and the Background Task
/// Ledger.
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
            { "tag": "collapsible_panel", "expanded": false, "element_id": "tool_1",
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

/// The carry's ONE INFO decision line (spec #523, story 14) — `capture_logs`'s
/// output for the one test whose requirement is a log line. The behavioral
/// tests assert the cards, never the logs. Panics when the count is not one.
fn carry_info_line(logs: &str) -> &str {
    let matches: Vec<&str> = logs
        .lines()
        .filter(|line| line_level(line) == "INFO" && line.contains("restart carry"))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one INFO carry decision per takeover:\n{logs}"
    );
    matches[0]
}

/// A restarted app over `session_file` whose session serves `transcript` and
/// its bounded tail read `tail` (ADR-0068), with the tiny turn cadences the
/// carry tests need. The returned gate holds every prompt until the test
/// releases it — the live window in which the carry is observed.
async fn carried_app(
    session_file: &Path,
    transcript: SessionTranscript,
    tail: TranscriptTail,
    status: SessionStatus,
) -> (
    Arc<App>,
    Arc<RecordingPlatform>,
    Arc<MockBackend>,
    Arc<tokio::sync::Semaphore>,
) {
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript("ses_test", vec![transcript]);
    backend.given_transcript_tail("ses_test", vec![tail]);
    backend.with_session_status("ses_test", Some(status));
    let gate = backend.hold_prompts();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(
        App::new(test_config(session_file), backend.clone(), platform.clone()).expect("the carry app builds"),
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

/// ADR-0068 acceptance: a user message that starts a fresh Turn over the
/// orphaned card hands the orphan Turn's still-running call to the successor.
/// The panel rides the successor's live tail, its completion renders exactly
/// once after its message has gone stale, and the carry is display-only (the
/// durable record names the new Turn's message; the card still settles ✅).
#[tokio::test]
async fn a_restart_takeover_carries_the_orphans_running_tool() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    let orphan_anchor = 1_000_000;
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(orphan_anchor));
    let new_anchor = 2_000_000;
    // The call started long before the new message: outside its in-flight
    // window, so only the carry can reach the successor.
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
    // The tail the carry reads: the orphan Turn's own newest end.
    let tail = TranscriptTail {
        transcript: SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            orphan(ToolStatus::Running, ""),
        ]),
        complete: true,
    };
    // The session's newest end: the stale running call, then the new message
    // whose turn the held prompt keeps in flight.
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan(ToolStatus::Running, ""),
        user("msg_cola_new", new_anchor, "新问题"),
    ]);
    let (app, platform, backend, gate) = carried_app(&session_file, live, tail, SessionStatus::Busy).await;
    // The orphaned card's rendered view: the carried running panel rides the
    // successor, so the collect must drop it there (ADR-0068).
    platform.given_card_view("om_frozen", live_tail_orphan_view());

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);

    // The successor's live card carries the orphan's panel while the new
    // prompt is still in flight.
    wait_for_card_text(&platform, "⏳ shell").await;
    assert_eq!(
        backend.transcript_tail_calls.lock().await.as_slice(),
        &[("ses_test".to_string(), tail_anchor(orphan_anchor))],
        "the carry read is scoped by the orphan record's anchor"
    );
    // The takeover itself is today's: the old card is collected in place, once.
    let orphan_patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        orphan_patches.len(),
        1,
        "the takeover collect is the orphan's only write: {orphan_patches:?}"
    );
    assert_eq!(card_header(&orphan_patches[0]), "⏳ 已由新卡片接管 · 已停止更新");
    // ADR-0068: the carried panel is live on the successor, so the orphan's
    // preserved body drops it — and the ledger always — while the written body
    // and the settled launch stay.
    let orphan_text = card_text(&orphan_patches[0]);
    assert!(
        !orphan_text.contains("⏳ shell"),
        "the carried running panel leaves the collected card: {orphan_text}"
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
        "the carry moves no record: the successor owns the Turn"
    );

    // The call settles while its message stays stale (beyond the Turn window);
    // the new turn also ends. Every render read must reconcile the carried
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
        "a later transcript render does not duplicate the carried call"
    );

    // Release the held prompt: the Turn settles ✅ as usual, with the carried
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
        "the settled carried result stays exactly once: {final_text}"
    );
    assert!(
        final_text.contains("新回答"),
        "the new Turn's own reply renders as usual: {final_text}"
    );
}

/// ADR-0068's canonical restart window: the fresh Turn's message is queued
/// behind the still-running orphan run, so the server's transcript carries no
/// anchor for as long as that run lasts. A carried call is resolved by call
/// identity against the whole read, so its completion still reconciles onto
/// the successor's card — while the anchor is unobserved — exactly once, and
/// nothing duplicates when the message finally lands.
#[tokio::test]
async fn a_carried_call_reconciles_while_the_fresh_turn_is_still_queued() {
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
    // The tail the carry reads: the orphan Turn's own newest end.
    let tail = TranscriptTail {
        transcript: SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            orphan(ToolStatus::Running, ""),
        ]),
        complete: true,
    };
    // The session's newest end while the orphan run still holds the queue: the
    // fresh Turn's message is NOT in the transcript yet — the canonical
    // restart window, with no anchor to observe.
    let queued = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan(ToolStatus::Running, ""),
    ]);
    let (app, platform, backend, gate) = carried_app(&session_file, queued, tail, SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);

    // The carry seeds the successor's live tail while the submitted prompt is
    // held in flight.
    wait_for_card_text(&platform, "⏳ shell").await;
    assert_eq!(
        Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await,
        None,
        "the fresh message is queued behind the orphan run, so no anchor is observable"
    );

    // The call completes while its message stays stale AND the fresh message is
    // still absent: the carried identity must reconcile anyway, joining the
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
        "the carried completion stays exactly once: {final_text}"
    );
    assert!(
        final_text.contains("新回答"),
        "the new Turn's own reply renders as usual: {final_text}"
    );
}

/// ADR-0068: the takeover's collect always drops the Background Task Ledger —
/// a ledger-only orphan (its launch settled 🌙 while the run is still live) is
/// collected without the stale live list, and the successor's own reads
/// rebuild it. Nothing is running to carry, so the settled launch and the
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
    // still running to carry; only the ledger lists its run.
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
    let tail = TranscriptTail {
        transcript: SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个构建"),
            launch.clone(),
        ]),
        complete: true,
    };
    // The Session's newest end: the background run is still live, so the
    // successor's own reads rebuild the ledger.
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个构建"),
        launch,
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ])
    .with_background_tasks(vec![background_shell(orphan_anchor + 600)]);
    let (app, platform, backend, gate) = carried_app(&session_file, live, tail, SessionStatus::Idle).await;
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
    // and the written body stay (nothing was carried).
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
    // The carry read ran and found nothing running to hand over.
    assert_eq!(
        backend.transcript_tail_calls.lock().await.as_slice(),
        &[("ses_test".to_string(), tail_anchor(orphan_anchor))],
        "the carry read is scoped by the orphan record's anchor"
    );

    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must end")
        .unwrap()
        .unwrap();
}

/// ADR-0068 + the best-effort collect rule (ticket #526): a takeover collect
/// whose PATCH fails only warns — the successor keeps ownership of the chain
/// and its carried panel — and never blocks the Turn. The ticket's acceptance
/// names exactly one warning for the failed collect, so that count is read
/// directly; the ownership behavior stands on the cards and the record, not
/// the log.
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
    let tail = TranscriptTail {
        transcript: SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            orphan.clone(),
        ]),
        complete: true,
    };
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan,
        user("msg_cola_new", new_anchor, "新问题"),
    ]);
    let (app, platform, backend, gate) = carried_app(&session_file, live, tail, SessionStatus::Busy).await;
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
        // The carried panel is live on the successor: the collect (and its
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

/// The carry read is scoped to the orphan Turn's projection: an older,
/// unrelated turn's stale `running` part is never resurrected on the
/// successor.
#[tokio::test]
async fn a_restart_takeover_never_carries_an_older_turns_stale_call() {
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
    let tail = TranscriptTail {
        transcript: SessionTranscript::new(vec![ghost.clone()]),
        complete: true,
    };
    let live = SessionTranscript::new(vec![
        ghost,
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, backend, gate) = carried_app(&session_file, live, tail, SessionStatus::Idle).await;
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    assert_eq!(
        backend.transcript_tail_calls.lock().await.as_slice(),
        &[("ses_test".to_string(), tail_anchor(1_000))],
        "the tail read ran, scoped by the orphan record's anchor"
    );
    wait_for_card_header(&platform, "✅").await;
    let cards = platform.updated_cards().await;
    let text = card_text(cards.last().unwrap());
    assert!(
        !text.contains("call_ghost") && !text.contains("⏳ shell"),
        "an older turn's stale running part is never resurrected: {text}"
    );
}

/// A cap-stopped tail read carries nothing: the page cap is a successful read
/// that never reached the boundary, and the carry must not guess from a
/// partial scan.
#[tokio::test]
async fn a_capped_carry_read_carries_nothing() {
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
    let tail = TranscriptTail {
        transcript: SessionTranscript::new(vec![orphan.clone()]),
        // The scan hit the page cap before the boundary.
        complete: false,
    };
    let live = SessionTranscript::new(vec![
        orphan,
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, backend, gate) = carried_app(&session_file, live, tail, SessionStatus::Idle).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    assert_eq!(
        backend.transcript_tail_calls.lock().await.len(),
        1,
        "the read ran"
    );
    let cards = platform.updated_cards().await;
    let text = card_text(cards.last().unwrap());
    assert!(!text.contains("call_sleep"), "nothing was carried: {text}");
    // A cap-stopped read carries nothing, so the orphan's running marker
    // stays in its preserved body — the ledger leaves it either way.
    let orphan_patches = patches_to(&platform, "om_frozen").await;
    let orphan_text = card_text(&orphan_patches[0]);
    assert!(
        orphan_text.contains("⏳ shell"),
        "a cap stop keeps the running marker: {orphan_text}"
    );
    assert!(
        !orphan_text.contains("⏳ 后台任务"),
        "the ledger leaves the collect: {orphan_text}"
    );
}

/// A failed carry read degrades to today's behavior: nothing is carried, the
/// takeover still collects the orphan, and the successor settles as usual.
#[tokio::test]
async fn a_failed_carry_read_carries_nothing() {
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
    let (app, platform, backend, gate) = carried_app(
        &session_file,
        live,
        TranscriptTail::default(),
        SessionStatus::Idle,
    )
    .await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    backend.fail_transcript_tail_for("ses_test").await;
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    let cards = platform.updated_cards().await;
    let text = card_text(cards.last().unwrap());
    assert!(!text.contains("call_sleep"), "nothing was carried: {text}");
    // A failed carry carries nothing, so the orphan's running marker stays in
    // its preserved body — while the ledger leaves it either way (ADR-0068).
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
        "a failed carry keeps the running marker: {orphan_text}"
    );
    assert!(
        !orphan_text.contains("⏳ 后台任务"),
        "the ledger leaves the collect even when nothing was carried: {orphan_text}"
    );
}

/// A timed-out carry read carries nothing and leaves the takeover as today —
/// the existing read timeout bounds the prompt's wait.
#[tokio::test]
async fn a_timed_out_carry_read_carries_nothing() {
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
    let (app, platform, backend, gate) = carried_app(
        &session_file,
        live,
        TranscriptTail::default(),
        SessionStatus::Idle,
    )
    .await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // The tail read hangs; the 20 ms read timeout ends it.
    backend
        .hang_transcript_tail
        .store(1, std::sync::atomic::Ordering::Relaxed);
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    let cards = platform.updated_cards().await;
    let text = card_text(cards.last().unwrap());
    assert!(!text.contains("call_sleep"), "nothing was carried: {text}");
    // A timed-out read carries nothing, so the orphan's running marker stays
    // in its preserved body — the ledger leaves it either way.
    let orphan_patches = patches_to(&platform, "om_frozen").await;
    let orphan_text = card_text(&orphan_patches[0]);
    assert!(
        orphan_text.contains("⏳ shell"),
        "a timed-out carry keeps the running marker: {orphan_text}"
    );
    assert!(
        !orphan_text.contains("⏳ 后台任务"),
        "the ledger leaves the collect: {orphan_text}"
    );
}

/// The #443 stamp's PATCH passes its pre-PATCH ownership check, then a fresh
/// Turn takes the orphan over while the stamp is in flight: the takeover's
/// collect queues behind the stamp's delivery lock, and the stamp's
/// post-PATCH repair lands last. The repair must reproduce the takeover
/// collect's keep rule (ADR-0068) — not `Everything` — so the stamp's stale
/// body never restores the tail the takeover removed. Carrying one running
/// call, that means no running `⏳` panel and no ledger on the orphan.
#[tokio::test]
async fn a_stamp_over_a_carrying_takeover_repairs_without_the_carried_tail() {
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
    let tail = TranscriptTail {
        transcript: SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            orphan.clone(),
        ]),
        complete: true,
    };
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan,
        user("msg_cola_new", new_anchor, "新问题"),
    ]);
    let (app, platform, backend, gate) = carried_app(&session_file, live, tail, SessionStatus::Busy).await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // Park the stamp PATCH after its pre-PATCH ownership check passed: it
    // holds the card's delivery lock, so the takeover's collect queues behind
    // it and the stamp's repair follows the collect.
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    // The recorded rule is the observed proof the takeover attached, carried
    // (at least one call) and reached its collect before the stamp is
    // released — so the post-PATCH repair can no longer miss the successor.
    wait_for_predecessor_keep(&app, "ses_test", "om_frozen", true).await;
    release.notify_one();
    // The repair is the orphan's third write: the parked stamp, the
    // takeover's queued collect, then the stamp's repair. Waiting for it pins
    // the interleaving this test creates.
    wait_for_patches(&platform, "om_frozen", 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        3,
        "the stamp, the takeover's collect and its repair: {patches:?}"
    );
    assert_eq!(
        card_header(&patches[0]),
        "⏳ 已重启，等待运行结束",
        "the parked stamp landed first: {patches:?}"
    );
    // Every write after the stamp is the takeover's collect or the stamp's
    // repair: none may restore the live tail the takeover removed.
    for patch in &patches[1..] {
        assert_eq!(
            card_header(patch),
            "⏳ 已由新卡片接管 · 已停止更新",
            "a post-stamp write is a takeover collect: {patch}"
        );
        let text = card_text(patch);
        assert!(
            !text.contains("⏳ shell"),
            "a carried running marker never returns to the collected card: {text}"
        );
        assert!(
            !text.contains("⏳ 后台任务"),
            "the ledger leaves every takeover collect and its repair: {text}"
        );
        assert!(
            text.contains("**正文** 已经写完的部分") && text.contains("🌙 shell"),
            "non-tail body content and the settled launch stay: {text}"
        );
    }

    // End the turn as usual: the carried call settles and the session idles.
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

/// The same stamp/takeover interleaving for a carry that moved nothing (a
/// failed read): the repair reproduces the takeover's rule, so the running
/// `⏳` marker stays — spec #523: "When nothing was carried … the running
/// marker stays" — while the ledger still leaves the collected card.
#[tokio::test]
async fn a_stamp_over_an_empty_takeover_repairs_without_the_ledger() {
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
    ]);
    let (app, platform, backend, gate) = carried_app(
        &session_file,
        live,
        TranscriptTail::default(),
        SessionStatus::Busy,
    )
    .await;
    platform.given_card_view("om_frozen", live_tail_orphan_view());
    // The carry read fails: nothing moves to the successor, so the collect —
    // and the repair behind it — keep the running panel and drop the ledger.
    backend.fail_transcript_tail_for("ses_test").await;
    let (entered, release) = platform.pause("update", "om_frozen");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    wait_for_predecessor_keep(&app, "ses_test", "om_frozen", false).await;
    release.notify_one();
    wait_for_patches(&platform, "om_frozen", 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        3,
        "the stamp, the takeover's collect and its repair: {patches:?}"
    );
    assert_eq!(
        card_header(&patches[0]),
        "⏳ 已重启，等待运行结束",
        "the parked stamp landed first: {patches:?}"
    );
    for patch in &patches[1..] {
        assert_eq!(
            card_header(patch),
            "⏳ 已由新卡片接管 · 已停止更新",
            "a post-stamp write is a takeover collect: {patch}"
        );
        let text = card_text(patch);
        assert!(
            text.contains("⏳ shell"),
            "an uncarried running marker stays: {text}"
        );
        assert!(
            !text.contains("⏳ 后台任务"),
            "the ledger leaves the collect and its repair: {text}"
        );
        assert!(
            text.contains("**正文** 已经写完的部分") && text.contains("🌙 shell"),
            "non-tail body content and the settled launch stay: {text}"
        );
    }

    // End the turn as usual: the never-carried call stays running in the
    // scripted read, so settle it and idle the session before the prompt.
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

/// An ordinary turn with no durable record never carries and never reads a
/// tail: a takeover without an orphan behaves exactly as today.
#[tokio::test]
async fn a_takeover_with_no_orphan_record_reads_no_tail() {
    let _wd = test_work_dir();
    let new_anchor = 2_000_000;
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (_dir, app, backend, platform) = scripted_app(vec![transcript], Some(SessionStatus::Idle)).await;

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    assert!(
        backend.transcript_tail_calls.lock().await.is_empty(),
        "no orphan record, no carry read"
    );
}

/// A record with no captured anchor carries nothing — it cannot scope the
/// orphan Turn's projection — and reads no tail.
#[tokio::test]
async fn an_anchorless_record_reads_no_tail() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", None);
    let new_anchor = 2_000_000;
    let transcript = SessionTranscript::new(vec![
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, backend, gate) = carried_app(
        &session_file,
        transcript,
        TranscriptTail::default(),
        SessionStatus::Idle,
    )
    .await;
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    Turn::run(&app.turn_handles(), context).await.unwrap();

    wait_for_card_header(&platform, "✅").await;
    assert!(
        backend.transcript_tail_calls.lock().await.is_empty(),
        "an anchorless record cannot scope a carry read"
    );
}

/// The carry's operator-facing contract (spec #523, story 14): every takeover
/// logs exactly ONE INFO decision line — the session and a decision word,
/// never chat content. The spec's testing rule keeps log text out of the
/// behavioral tests; this is the one test that reads it, because the logging
/// requirement is itself a log line. The fixture is the simplest decision: an
/// orphan Turn with nothing running to carry.
#[tokio::test]
async fn a_carry_decision_logs_one_info_line() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    let new_anchor = 2_000_000;
    let tail = TranscriptTail {
        transcript: SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]),
        complete: true,
    };
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "问题"),
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, backend, gate) = carried_app(&session_file, live, tail, SessionStatus::Idle).await;
    gate.add_permits(1);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let ((), logs) = capture_logs(async {
        Turn::run(&app.turn_handles(), context).await.unwrap();
    })
    .await;

    // The user-visible outcome stands on its own: the takeover ran its one
    // bounded read and the successor settled.
    assert_eq!(
        backend.transcript_tail_calls.lock().await.len(),
        1,
        "the carry read ran"
    );
    wait_for_card_header(&platform, "✅").await;

    let line = carry_info_line(&logs);
    assert!(
        line.contains("ses_test") && line.contains("carried 0"),
        "the one INFO line names the session and its decision: {line}"
    );
}

/// A killed run's carried call does not outlive the Turn: the transcript never
/// settles it, yet the follow still ends ✅ by the Turn's own settle decision
/// (the carried panel is display-only), and the settled card omits the
/// still-running panel — no permanent `⏳` survives the Turn. A genuinely
/// running call keeps its panel while the successor is live; only the
/// end-of-turn card drops it, because no renderer will ever update it again.
#[tokio::test]
async fn a_killed_runs_carried_call_does_not_outlive_the_turn() {
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
    let tail = TranscriptTail {
        transcript: SessionTranscript::new(vec![
            user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
            orphan.clone(),
        ]),
        complete: true,
    };
    // The call NEVER settles (a killed run leaves its part behind), while the
    // new turn's reply lands terminal.
    let live = SessionTranscript::new(vec![
        user("msg_cola_anchor", orphan_anchor, "跑个长命令"),
        orphan,
        user("msg_cola_new", new_anchor, "新问题"),
        assistant(new_anchor + 1_000, "新回答"),
    ]);
    let (app, platform, backend, gate) = carried_app(&session_file, live, tail, SessionStatus::Busy).await;
    // The drain bound hands the card to the follow, where the live-panel guard
    // would otherwise keep waiting on the carried call.
    app.turn_drain_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);

    let mut context = ctx("ses_test", "新问题");
    context.cola_message_id = Some("msg_cola_new".into());
    let turn = spawn_turn(&app, context);
    // The carried panel is live on the successor while the prompt is held.
    wait_for_card_text(&platform, "⏳ shell").await;

    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must hand off at the drain bound")
        .unwrap()
        .unwrap();
    // The follow observes an idle session; the carried call is display-only, so
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
        "a killed run's carried call must not leave a permanent running panel: {text}"
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

/// A stamp PATCH that fails recoverably is not marked: the next pass retries
/// it until one lands, and the mark then holds (#443).
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
    // not have landed, so the record stays unmarked and the next pass retries.
    platform
        .fail_update_transport_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    wait_for_card_update(&platform, "the retried restart stamp", CardUpdates::Any, |card| {
        card_header(card).contains("已重启，等待运行结束")
    })
    .await;
    // Further observed passes: the mark holds — no third PATCH.
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

/// #522: a stamp PATCH Feishu permanently refuses — a typed
/// `CardContentRejected`, deterministic by ADR-0067's own treatment — is
/// attempted once and given up for this process life: no later Session Sync
/// tick retries the identical payload. The record stays (the pre-#443
/// behavior for that rare card), and the real ending still supersedes it.
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
    // pass would send the identical payload, so none may be made.
    platform
        .fail_update_card_content_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    wait_for_rejected_stamp(&app, "ses_test").await;
    // Several observed passes after the rejection: the mark holds.
    wait_for_status_reads(&backend, "ses_test", 4).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert_eq!(
        patches.len(),
        1,
        "the refused stamp is attempted once, never retried: {patches:?}"
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

/// A stamp PATCH that hangs never wedges the pass: the attempt is detached
/// and the pass keeps reconciling other records while the write waits. The
/// write is never cancelled — when Feishu finally answers, the stamp lands
/// exactly once (#443).
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

    // Feishu answers: the never-cancelled write lands and is marked once.
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

/// A run that ends while the stamp's write hangs is settled only after the
/// attempt resolves: the terminal must not race the never-cancelled write
/// (which could then land over it), so nothing is written while the stamp is
/// in flight, and the ✅ follows the stamp as the card's last word (#443).
#[tokio::test]
async fn a_terminal_waits_for_an_in_flight_stamp() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));
    // The pass clock: this record's reads keep ticking — the claimed record
    // returns before its own status read.
    ChainRecords::load(sidecar(&session_file)).track(
        "ses_other",
        "om_other",
        MessageId::new("msg_cola_other"),
        Some(2_000),
        Some("/work"),
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
    // The run ends while the stamp write hangs: the settle must wait for the
    // attempt, never race it.
    backend
        .set_session_status("ses_test", Some(SessionStatus::Idle))
        .await;
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
        "no settle while the stamp is in flight: {:?}",
        platform.calls.lock().await
    );

    // The write lands, and the settled ✅ is the card's last word.
    release.notify_one();
    wait_for_card_update(&platform, "the ✅ over the stamp", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;

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
    // The orphaned card's view: the Wake arm's collect is NOT the fresh-Turn
    // takeover, so ADR-0068's strip must leave this body untouched.
    platform.given_card_view("om_frozen", live_tail_orphan_view());

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
    // The Wake continuation's collect keeps the preserved body as today (the
    // spec's Trigger scopes the ADR-0068 strip to the fresh-Turn takeover):
    // the running panel and the ledger both stay.
    let collected_text = card_text(&collected);
    assert!(
        collected_text.contains("⏳ shell") && collected_text.contains("⏳ 后台任务"),
        "the Wake arm's collect is untouched by ADR-0068: {collected_text}"
    );

    // The record followed the successor: after the continuation's own settle
    // (it ended ✅ in the same read) the record is spent, but the send that
    // took over re-pointed it first — proven by the successor's own send
    // having tracked a card id that is not the orphan's.
    let record = app.cards_handle().chains.get("ses_test");
    assert!(
        record
            .as_ref()
            .is_none_or(|record| record.card_message_id != "om_frozen"),
        "the record never keeps naming the collected orphan: {record:?}"
    );
}

/// The Wake continuation arm never carries (ADR-0061's no-replay scope): the
/// restart's Session Sync posts its continuation with no tail read, so a stale
/// running call from the lost chain is not replayed onto it.
#[tokio::test]
async fn a_restart_wake_continuation_never_carries() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000_000));

    // The lost chain: its running call sits beyond the Wake's own in-flight
    // window, so only a carry could ever replay it.
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
    let (app, platform, backend) = restarted_app_with_backend(&session_file, transcript, None).await;

    spawn_sync(&app);
    wait_for_card_update(
        &platform,
        "the restart continuation's done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅") && card_text(card).contains("CI 通过了。"),
    )
    .await;

    assert!(
        backend.transcript_tail_calls.lock().await.is_empty(),
        "the Wake continuation arm never reads (or carries) the tail"
    );
    let cards = platform.updated_cards().await;
    assert!(
        !card_text(cards.last().unwrap()).contains("call_sleep"),
        "the lost chain's stale running call is never replayed: {cards:?}"
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

/// A successor can be attached before its Turn anchor is armed, and the
/// collect's awaited PATCH is the window where the anchor lands. The pre-split
/// pass re-read the armed anchor AFTER the collect, so an anchor that appears
/// while the collect is parked must still re-point the record at the
/// successor — never be read as absent and release it, which would leave the
/// live card unreaped after a later restart.
#[tokio::test]
async fn an_anchor_armed_during_the_collect_still_repoints_the_record() {
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
    // The process's live card: a handover the record missed — attached, but
    // its anchor is armed only while the collect is in flight.
    Turn::seed_card(&app.cards_handle(), "ses_test", Some("om_live")).await;
    // Park the collect's PATCH after the pass entered the successor branch.
    let (entered, release) = platform.pause("update", "om_old");

    spawn_sync_with_timeout(&app, 5_000);
    entered.notified().await;
    // The takeover arms its anchor while the collect awaits.
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
    release.notify_one();

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
}

/// A PATCH failure that is NOT a definite card-content rejection — here a
/// transport-class failure, the shape a timeout or dropped connection surfaces
/// as — is never retried bare: the preserved attempt may already have landed,
/// and the bare ending would then wipe the body this path exists to keep. The
/// record stays, so the next tick tries the settle again (#434 acceptance
/// feedback).
#[tokio::test]
async fn a_transport_failed_preserved_ending_does_not_retry_bare() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let (app, platform, backend) = restarted_app_with_backend(&session_file, completed(1_000), None).await;
    platform.given_card_view("om_frozen", realistic_card_view());
    // A transport failure on every attempt, so each observed tick records
    // what it sent before failing.
    platform
        .fail_update_transport_count
        .store(10, std::sync::atomic::Ordering::SeqCst);

    spawn_sync(&app);
    // Three observed status reads: at least two passes reached their PATCH.
    wait_for_status_reads(&backend, "ses_test", 3).await;

    let patches = patches_to(&platform, "om_frozen").await;
    assert!(
        patches.len() >= 2,
        "the observed ticks retried the settle: {patches:?}"
    );
    assert!(
        patches
            .iter()
            .all(|card| card_text(card).contains("**正文** 第一段")),
        "no transport failure falls back to the bare ending: {patches:?}"
    );
    assert!(
        app.cards_handle().chains.get("ses_test").is_some(),
        "a failed settle keeps the record for the next tick"
    );
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
