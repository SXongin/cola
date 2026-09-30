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
use crate::backend::{MessageId, SessionTranscript};
use crate::bridge::live_cards::{LiveCard, LiveCards};
use crate::bridge::test_support::*;
use crate::bridge::turn::Turn;
use crate::config::{SessionEntry, ThreadKey};
use crate::feishu::card::CardState;
use crate::opencode::types::{SessionListInfo, SessionStatus};

/// The sidecar path the app's own rule derives from the session file.
fn sidecar(session_file: &Path) -> PathBuf {
    session_file.with_file_name("live_cards.json")
}

/// Seed the record a previous cola life left behind — the card that was live
/// when the process died — directly into the sidecar, so the next app loads it
/// at construction exactly like a real restart.
fn seed_record(session_file: &Path, card_message_id: &str, message_id: &str, created_ms: Option<i64>) {
    seed_live_card(
        session_file,
        LiveCard::new(card_message_id, MessageId::new(message_id), created_ms),
    );
}

/// [`seed_record`] with the record built by the caller (a stored directory).
fn seed_live_card(session_file: &Path, card: LiveCard) {
    LiveCards::load(sidecar(session_file)).replace("ses_test", card);
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

/// Wait until the Session Sync pass has read `session_id`'s transcript at
/// least `n` times — the proof-of-pass for a test whose reap is for a session
/// it never reads.
async fn wait_for_transcript_reads(backend: &Arc<MockBackend>, session_id: &str, n: usize) {
    let probe = async {
        loop {
            let reads = backend
                .transcript_calls
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
        .unwrap_or_else(|_| panic!("Session Sync never read {session_id}'s transcript {n} times"));
}

/// Wait until the session's record names `card_message_id`, or panic after
/// 5 s.
async fn wait_for_record_card(app: &Arc<App>, session_id: &str, card_message_id: &str) {
    let probe = async {
        loop {
            if app
                .cards_handle()
                .live_cards
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

/// The transcript that has finished while cola was down: the submitted
/// message and a clean assistant answer.
fn completed(created_ms: i64) -> SessionTranscript {
    SessionTranscript::new(vec![
        user("msg_cola_anchor", created_ms, "问题"),
        assistant(created_ms + 1_000, "回答"),
    ])
    .with_executions(vec![execution(created_ms + 1_500)])
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
    seed_live_card(
        &session_file,
        LiveCard::new("om_frozen", MessageId::new("msg_cola_anchor"), Some(1_000))
            .with_directory(Some("/work".into())),
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
        app.cards_handle().live_cards.get("ses_test").is_none(),
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
    seed_live_card(
        &session_file,
        LiveCard::new("om_frozen", MessageId::new("msg_cola_anchor"), None)
            .with_directory(Some("/work".into())),
    );

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
        app.cards_handle().live_cards.get("ses_test").is_none(),
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
    seed_live_card(
        &session_file,
        LiveCard::new("om_frozen", MessageId::new("msg_cola_anchor"), Some(1_000))
            .with_directory(Some("/work".into())),
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
    seed_live_card(
        &session_file,
        LiveCard::new("om_frozen", MessageId::new("msg_cola_anchor"), Some(1_000))
            .with_directory(Some("/work".into())),
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
    seed_live_card(
        &session_file,
        LiveCard::new("om_frozen", MessageId::new("msg_cola_anchor"), Some(1_000))
            .with_directory(Some("/work".into())),
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
        app.cards_handle().live_cards.get("ses_test").is_none(),
        "the true end spends the record"
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

/// A still-live Session keeps the card and the record: the run may still be
/// answering it, and a restart must not invent an ending for it.
#[tokio::test]
async fn a_still_live_session_keeps_its_persisted_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_record(&session_file, "om_frozen", "msg_cola_anchor", Some(1_000));

    let transcript = SessionTranscript::new(vec![user("msg_cola_anchor", 1_000, "问题")]);
    let (app, platform, backend) =
        restarted_app_with_backend(&session_file, transcript, Some(SessionStatus::Busy)).await;

    spawn_sync(&app);
    // Several observed passes, each reading the live status: none may touch
    // the card.
    wait_for_status_reads(&backend, "ses_test", 3).await;
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
        app.cards_handle().live_cards.get("ses_test").is_some(),
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
        app.cards_handle().live_cards.get("ses_test").is_some(),
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
    seed_live_card(
        &session_file,
        LiveCard::new("om_frozen", MessageId::new("msg_cola_anchor"), Some(1_000))
            .with_directory(Some("/gone".into())),
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
    assert!(app.cards_handle().live_cards.get("ses_test").is_none());
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
        app.cards_handle().live_cards.get("ses_test").is_some(),
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
/// session (the record write raced the handover) collects the orphan in place
/// and re-points the record at the owner: no card is left looking live, and
/// the sidecar never keeps naming a card nothing will reap.
#[tokio::test]
async fn a_record_that_lagged_a_handover_collects_the_orphan_and_repoints() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("sessions.json");
    seed_live_card(
        &session_file,
        LiveCard::new("om_old", MessageId::new("msg_cola_anchor"), Some(1_000))
            .with_directory(Some("/work".into())),
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
    let record = app.cards_handle().live_cards.get("ses_test").unwrap();
    assert_eq!(
        record.directory.as_deref(),
        Some("/work"),
        "the re-point keeps the session's route"
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
        .live_cards
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
        app.cards_handle().live_cards.get("ses_test").is_none(),
        "the quiet true end is a terminal: its record is spent"
    );
    assert!(
        !sidecar(&session_file).exists(),
        "the emptied record removes the sidecar"
    );
}
