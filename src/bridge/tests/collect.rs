//! Collecting a waiting card (ADR-0059, spec #405 ticket 5): no card that
//! yielded 「⏳ 等待后台任务」 may hang forever.
//!
//! A new Turn starting in the same thread collects it as
//! 「⏳ 部分完成 · 已由新消息接管」; a Session that stops being the thread's
//! Active Session — `/switch` away (or `/switch forget`) — collects it as
//! 「⏳ 已切换会话 · 后台任务仍在运行」. Neither collect sends a Completion
//! Notice (the notice belongs to a true end), the collected card stops
//! updating, and the background work itself is unaffected: a later Wake still
//! continues the chain on a new card, and switching back reports the Session
//! through the ADR-0028 snapshot.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::drain::{assistant, ctx, script_transcript, scripted_app, user};
use crate::backend::SessionTranscript;
use crate::bridge::test_support::*;
use crate::bridge::turn::Turn;
use crate::feishu::card::CardState;
use crate::opencode::types::SessionStatus;

/// The collected headers under test (the card module owns the copy).
const SUPERSEDED: &str = "已由新消息接管";
const SWITCHED: &str = "已切换会话 · 后台任务仍在运行";

/// The one user-facing marker a Wake continuation opens with.
const WAKE_LEAD: &str = "已恢复执行";

/// A Turn whose Execution idled with a live Background Task: the waiting
/// yield's fixture (spec #405 ticket 2).
fn waiting_transcript() -> SessionTranscript {
    SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)])
}

/// The waiting Turn plus the second message that supersedes it.
fn superseded_transcript() -> SessionTranscript {
    SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        user("msg_cola_next", 3_000, "新问题"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)])
}

/// The first Turn resumed by a Wake: the background task retired and its work
/// landed on a continuation card.
fn resumed_transcript() -> SessionTranscript {
    SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        assistant(3_100, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)])
}

/// A transcript whose newest user message is EXTERNAL: the switch-back
/// snapshot is not suppressed by ADR-0028's idle + cola-authored cell.
fn external_newest_transcript() -> SessionTranscript {
    SessionTranscript::new(vec![
        user("msg_cola_anchor", 1_000, "跑一下 CI"),
        assistant(2_000, "已经交给后台了。"),
        user("msg_ext_user", 5_000, "外部客户端的新消息"),
    ])
    .with_executions(vec![execution(2_500)])
    .with_background_tasks(vec![background_shell(2_100)])
}

/// Every in-place PATCH the platform recorded for `message_id`, in call order.
async fn patches_to(platform: &RecordingPlatform, message_id: &str) -> Vec<serde_json::Value> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::UpdateMessage {
                message_id: mid,
                card,
            } if mid == message_id => Some(card.clone()),
            _ => None,
        })
        .collect()
}

/// Whether any Completion Notice was sent.
async fn noticed(platform: &RecordingPlatform) -> bool {
    platform
        .calls
        .lock()
        .await
        .iter()
        .any(|call| matches!(call, PlatformCall::CompletionNotice { .. }))
}

/// The first card the platform replied to `reply_to` satisfying `check`, or
/// `None` — the recorded calls under one lock.
async fn replied_card_where(
    platform: &RecordingPlatform,
    reply_to: &str,
    check: impl Fn(&serde_json::Value) -> bool,
) -> Option<serde_json::Value> {
    platform.calls.lock().await.iter().find_map(|call| match call {
        PlatformCall::ReplyCard { reply_to: mid, card } if mid == reply_to && check(card) => {
            Some(card.clone())
        }
        _ => None,
    })
}

/// Await a card replied to `reply_to` whose `check` passes, or panic after 5 s
/// (the Wake continuation is sent on the Session Sync poller's cadence).
async fn wait_for_replied_card(
    platform: &RecordingPlatform,
    reply_to: &str,
    label: &str,
    check: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let probe = async {
        loop {
            if let Some(card) = replied_card_where(platform, reply_to, &check).await {
                return card;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .unwrap_or_else(|_| panic!("no card replied to {reply_to:?} reached {label}"))
}

/// Start Session Sync's poll loop with tiny injected cadences.
fn spawn_sync(app: &Arc<App>) {
    app.external.poll_interval_ms.store(20, Ordering::Relaxed);
    app.external.render_poll_ms.store(5, Ordering::Relaxed);
    app.external.request_timeout_ms.store(50, Ordering::Relaxed);
    let app = app.clone();
    tokio::spawn(async move {
        let _ = app.external.poll_loop(&app.flow_handles()).await;
    });
}

/// Seed a card that has already yielded 「⏳ 等待后台任务」 for an active,
/// mapped session — with an explicit message id, so the collect's PATCH is
/// distinguishable from every later card (the recording platform serves one id
/// per send). The collect triggers under test are the Turn and switch paths;
/// the yield itself is pinned by spec #405 ticket 2.
async fn seed_waiting_card(app: &Arc<App>, session_id: &str, card_message_id: &str) {
    Turn::seed_card(&app.cards_handle(), session_id, Some(card_message_id)).await;
    Turn::set_card_state(&app.cards_handle(), session_id, CardState::Waiting).await;
}

/// An app whose chat_1 thread has `ses_test` mapped + active and `ses_other`
/// available in the store, with the mock's script for `ses_test` left to the
/// test. The TempDir must stay alive: the session store persists into it.
async fn switch_fixture() -> (
    tempfile::TempDir,
    Arc<App>,
    Arc<MockBackend>,
    Arc<RecordingPlatform>,
) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_test", "等待会话", "/work", 50),
        list_session("ses_other", "另一个会话", "/work/other", 100),
    ]);
    // A scripted entry for the waiting session, so `script_transcript` can
    // swap the snapshot for the cell under test.
    backend.given_transcript("ses_test", vec![waiting_transcript()]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    (dir, app, backend, platform)
}

/// Acceptance 1 + the no-notice rule: a message arriving while the card waits
/// starts the next Turn, which collects the waiting card as
/// 「⏳ 部分完成 · 已由新消息接管」 — no Completion Notice, the new Turn renders
/// on a NEW chain — and a later Wake still continues that newest chain, never
/// the collected card.
#[tokio::test]
async fn a_new_turn_collects_the_waiting_card() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) =
        scripted_app(vec![waiting_transcript()], Some(SessionStatus::Idle)).await;

    // The first Turn yields 「⏳ 等待后台任务」 and notifies nothing.
    let mut first = ctx("ses_test", "跑一下 CI");
    first.is_group = true;
    first.requester_open_id = Some(TEST_HOST.to_string());
    Turn::run(&app.turn_handles(), first).await.unwrap();
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::Waiting)
    );
    let waiting_chain = Turn::chain_id(&app.cards_handle(), "ses_test").await;
    assert!(!noticed(&platform).await, "the waiting yield sends no notice");

    // The user posts a new message in the same thread: the new Turn collects
    // the waiting card before it starts.
    script_transcript(&backend, vec![superseded_transcript()]).await;
    let mut second = ctx("ses_test", "新问题");
    second.message_id = "msg_next".into();
    second.cola_message_id = Some("msg_cola_next".into());
    second.is_group = true;
    second.requester_open_id = Some(TEST_HOST.to_string());
    Turn::run(&app.turn_handles(), second).await.unwrap();

    let updates = platform.updated_cards().await;
    let collected = updates
        .iter()
        .find(|card| card_header(card).contains(SUPERSEDED))
        .cloned()
        .unwrap_or_else(|| panic!("the waiting card must be collected: {updates:?}"));
    assert!(
        !card_header(&collected).contains("等待后台任务"),
        "the collected card no longer yields: {collected}"
    );
    assert!(
        card_text(&collected).contains("已经交给后台了。"),
        "the collect keeps the content the turn produced: {collected}"
    );
    assert_eq!(
        platform
            .updated_cards()
            .await
            .iter()
            .filter(|card| card_header(card).contains(SUPERSEDED))
            .count(),
        1,
        "the wait is collected exactly once: {:?}",
        platform.updated_cards().await
    );
    assert_ne!(
        Turn::chain_id(&app.cards_handle(), "ses_test").await,
        waiting_chain,
        "the new Turn renders on a new chain"
    );
    assert!(
        !noticed(&platform).await,
        "neither collect nor the new Turn's waiting yield notifies: {:?}",
        platform.calls.lock().await
    );

    // The Wake resumes the newest chain: the continuation is replied to the
    // second message, carries the Wake's own work — and the collected card is
    // never touched again.
    script_transcript(&backend, vec![resumed_transcript()]).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the Wake's done card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;

    let continuation = wait_for_replied_card(&platform, "msg_next", "the Wake continuation", |card| {
        card_text(card).contains(WAKE_LEAD)
    })
    .await;
    assert!(
        card_text(&continuation).contains(WAKE_LEAD),
        "the continuation opens with the 承接 line: {continuation}"
    );
    // The settle loop renders the resumed work INTO that same continuation
    // card and ends it ✅; the collected card's content never replays.
    let rendered = latest_card(&platform).await;
    assert!(
        card_header(&rendered).contains("✅"),
        "the Wake ends ✅: {rendered}"
    );
    assert!(
        card_text(&rendered).contains("CI 通过了。"),
        "the continuation renders the resumed work: {rendered}"
    );
    assert!(
        !card_text(&rendered).contains("已经交给后台了。"),
        "the continuation renders only the Wake's own work: {rendered}"
    );
    assert!(
        !noticed(&platform).await,
        "the continuation is the notification; no notice: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 2 + the no-notice rule: `/switch`ing away collects the waiting
/// card as 「⏳ 已切换会话 · 后台任务仍在运行」, sends no Completion Notice, and
/// the collected card stops updating (exactly the one collect PATCH).
#[tokio::test]
async fn switching_away_collects_the_waiting_card() {
    let _wd = test_work_dir();
    let (_dir, app, _backend, platform) = switch_fixture().await;
    seed_waiting_card(&app, "ses_test", "om_waiting").await;

    send_command(&app, "/switch 另一个会话", "msg_switch").await;

    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(
        patches.len(),
        1,
        "the switch collects the wait exactly once (and it stops updating): {patches:?}"
    );
    assert!(
        card_header(&patches[0]).contains(SWITCHED),
        "the collect names the switch: {}",
        patches[0]
    );
    assert!(
        !card_header(&patches[0]).contains("等待后台任务"),
        "the collected card no longer yields: {}",
        patches[0]
    );
    assert_eq!(
        Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(CardState::SwitchedAway),
        "the collected card is terminal"
    );
    assert!(
        !noticed(&platform).await,
        "the collect sends no Completion Notice: {:?}",
        platform.calls.lock().await
    );
}

/// `/switch forget` unmaps the Session without an activation, so it collects
/// the waiting card itself — the same switch-away collect, the other
/// unmapping path.
#[tokio::test]
async fn forgetting_a_session_collects_its_waiting_card() {
    let _wd = test_work_dir();
    let (_dir, app, _backend, platform) = switch_fixture().await;
    seed_waiting_card(&app, "ses_test", "om_waiting").await;

    send_command(&app, "/switch forget", "msg_forget").await;

    let patches = patches_to(&platform, "om_waiting").await;
    assert_eq!(patches.len(), 1, "forget collects the wait: {patches:?}");
    assert!(
        card_header(&patches[0]).contains(SWITCHED),
        "the collect names the switch: {}",
        patches[0]
    );
    assert!(
        !noticed(&platform).await,
        "the collect sends no Completion Notice: {:?}",
        platform.calls.lock().await
    );
}

/// Acceptance 3: after switching back the Session Snapshot reports the
/// Session's current state, and a later Wake still continues the collected
/// chain on a new card — the collected card itself never updates again.
#[tokio::test]
async fn a_collected_chain_still_takes_a_wake_after_switching_back() {
    let _wd = test_work_dir();
    let (_dir, app, backend, platform) = switch_fixture().await;
    seed_waiting_card(&app, "ses_test", "om_waiting").await;
    Turn::set_turn_anchor(&app.cards_handle(), "ses_test", &turn_anchor(1_000)).await;
    Turn::set_reply_target(&app.cards_handle(), "ses_test", "msg_1").await;

    // Switch away: the waiting card is collected...
    send_command(&app, "/switch 另一个会话", "msg_switch").await;
    assert_eq!(
        patches_to(&platform, "om_waiting").await.len(),
        1,
        "the switch collects the wait"
    );
    assert!(!noticed(&platform).await, "the collect sends no notice");

    // ...and switching back reports the Session: an external newest user makes
    // the ADR-0028 re-switch snapshot emit its full 切换 card rather than the
    // suppressed one-line state.
    script_transcript(&backend, vec![external_newest_transcript()]).await;
    send_command(&app, "/switch ses_test", "msg_switch_back").await;
    let snapshot = replied_card_where(&platform, "msg_switch_back", |card| {
        card_header(card).contains("已切换")
    })
    .await
    .expect("switching back must report the Session Snapshot");
    assert!(
        card_header(&snapshot).contains("等待会话"),
        "the snapshot names the session: {snapshot}"
    );

    // The Wake resumes the Session: its continuation is a new card below the
    // collected chain, and the collected card keeps its ending untouched.
    script_transcript(&backend, vec![resumed_transcript()]).await;
    spawn_sync(&app);
    wait_for_card_update(&platform, "the Wake's done card", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;

    let continuation = wait_for_replied_card(&platform, "msg_1", "the Wake continuation", |card| {
        card_text(card).contains(WAKE_LEAD)
    })
    .await;
    assert!(
        card_text(&continuation).contains(WAKE_LEAD),
        "the continuation opens with the 承接 line: {continuation}"
    );
    let rendered = latest_card(&platform).await;
    assert!(
        card_text(&rendered).contains("CI 通过了。"),
        "the continuation renders the resumed work: {rendered}"
    );
    assert_eq!(
        patches_to(&platform, "om_waiting").await.len(),
        1,
        "the collected card never updates again"
    );
}
