use super::drain::spawn_sync;
use crate::backend::{FinishReason, MessageRole, Part, SessionTranscript, StepFinish, TranscriptMessage};
use crate::bridge::test_support::*;

/// A lobby adopt of a session that carries a pending permission ends in ONE
/// card — the snapshot embeds the block — and the poll loop does NOT pop a
/// duplicate standalone card for the claimed request (ADR-0028).
#[tokio::test]
async fn snapshot_claim_prevents_poller_duplicate() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    // The adopt claimed the embedded pending against the sent snapshot.
    let claims = app.core.snapshot_claims.lock().await;
    assert_eq!(
        claims.claim_of("per_1"),
        Some(("msg_reply", crate::bridge::snapshot_claims::ClaimKind::Permission))
    );

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let calls = platform.calls.lock().await.clone();
    let cards = calls
        .iter()
        .filter(|c| matches!(c, PlatformCall::ReplyCard { .. }))
        .count();
    assert_eq!(cards, 1, "claimed pending must not pop a second card: {calls:?}");
    assert!(
        calls.iter().all(|c| !matches!(c, PlatformCall::SendCard { .. })),
        "no standalone permission card: {calls:?}"
    );
}

/// Answering a claimed block via the snapshot's own buttons resolves the
/// request and PATCHES the snapshot in place — the ack returns the
/// re-rendered snapshot without the block, no new message (ADR-0028).
#[tokio::test]
async fn snapshot_block_answer_patches_snapshot() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    // Click 允许一次 on the snapshot's embedded block.
    let value = serde_json::json!({
        "action": "perm",
        "reply": "once",
        "session_id": "ses_alpha01",
        "request_id": "per_1",
        "directory": "/work/ext",
        "perm_label": "✅ 已允许一次",
        "perm_color": "green",
        "perm_body": "bash",
    });
    let result = app
        .host_action(value)
        .await
        .expect("claimed block click returns a result");
    let card = result.card.expect("the ack patches the snapshot");
    let card_str = card.to_string();
    assert!(
        card_str.contains("已接管 唯一外部标题"),
        "snapshot kept: {card_str}"
    );
    assert!(
        !card_str.contains("权限请求"),
        "resolved block removed from the snapshot: {card_str}"
    );
    assert_eq!(result.toast.as_deref(), Some("已允许本次执行"));
    assert_eq!(
        backend.reply_permission_calls.lock().await.as_slice(),
        &[("per_1".to_string(), "once".to_string())]
    );
    assert!(
        !app.core.snapshot_claims.lock().await.contains("per_1"),
        "the claim is dropped once the block is resolved"
    );
}

/// Resolving a claimed request from ANOTHER client drops the block from
/// the snapshot and leaves its Interaction Receipt (patched in place) — and
/// never marks the snapshot itself stale, since stale is for standalone cards
/// (ADR-0028, ADR-0038 rule 4, #175).
#[tokio::test]
async fn snapshot_block_resolved_elsewhere_leaves_a_receipt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;
    // Another client answers the request server-side.
    backend.permission_resolved_by_another("per_1").await;

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let calls = platform.calls.lock().await.clone();
    let patched = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_reply" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .next_back()
        .expect("the snapshot is re-rendered in place");
    assert!(
        patched.contains("已接管 唯一外部标题"),
        "snapshot kept, not marked stale: {patched}"
    );
    assert!(!patched.contains("权限请求"), "block dropped: {patched}");
    assert!(
        patched.contains("⏱ 已由其他客户端处理：⚡ 执行 Shell 命令 `ls -la`"),
        "the resolved claim leaves its Interaction Receipt: {patched}"
    );
    assert!(!patched.contains("已处理"), "no stale marker: {patched}");
    assert!(
        !app.core.snapshot_claims.lock().await.contains("per_1"),
        "the claim is dropped"
    );
}

/// A request that first appears AFTER the snapshot was sent is NOT claimed
/// and keeps today's standalone flow; the claimed adopt-time block is not
/// duplicated (ADR-0028).
#[tokio::test]
async fn post_adopt_requests_keep_standalone_flow() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    // One adopt-time pending (claimed by the snapshot)…
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;
    // …and a SECOND request arriving after the snapshot was sent.
    backend.ask_permission_later(perm_request("per_2", "ses_alpha01", "rm -rf /tmp/x"));

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let calls = platform.calls.lock().await.clone();
    // Only per_2 surfaces as a standalone card (per_1 is claimed).
    let standalone: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::SendCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(standalone.len(), 1, "one standalone card: {calls:?}");
    assert!(
        standalone[0].contains("per_2"),
        "the NEW request surfaces: {}",
        standalone[0]
    );
    assert!(
        !standalone[0].contains("per_1"),
        "the claimed request is not re-surfaced: {}",
        standalone[0]
    );
}

/// Re-switch dedupe: adopting a session whose pending request was ALREADY
/// surfaced (a standalone card exists in the flow) embeds nothing new —
/// the existing card stays authoritative, and no claim is registered.
#[tokio::test]
async fn adopt_skips_already_surfaced_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;
    // The request was already surfaced as a standalone card.
    app.permission.sent_cards.lock().await.insert(
        "per_1".into(),
        crate::bridge::request::flow::SentCard {
            message_id: "om_existing".into(),
            summary: "bash".into(),
            directory: "/work/ext".into(),
            session_id: "ses_alpha01".into(),
        },
    );

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    let calls = platform.calls.lock().await.clone();
    let card = calls
        .iter()
        .find_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("snapshot sent");
    assert!(
        !card.contains("权限请求"),
        "already-surfaced pending is not embedded: {card}"
    );
    assert!(
        !app.core.snapshot_claims.lock().await.contains("per_1"),
        "no claim for an already-surfaced request"
    );
}

/// Re-switch dedupe against an EARLIER snapshot: re-adopting a session
/// whose pending is still claimed by the first snapshot embeds nothing
/// new — the first snapshot stays authoritative, and the claim is not
/// re-pointed at the new card.
#[tokio::test]
async fn reeswitch_does_not_reclaim_snapshot_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    let (app, platform) = build_app(cfg, backend).await;

    // First adopt: the snapshot claims the pending block.
    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    // Re-`/switch` to the now-mapped session (mapped-hit branch).
    send_command(&app, "/switch 唯一外部标题", "msg_switch_2").await;

    let calls = platform.calls.lock().await.clone();
    let cards: Vec<String> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(cards.len(), 2, "two snapshots total: {calls:?}");
    assert!(
        !cards[1].contains("权限请求"),
        "the re-switch snapshot embeds no duplicate block: {}",
        cards[1]
    );
    let registry = app.core.snapshot_claims.lock().await;
    assert_eq!(registry.claim_count(), 1, "one claim, not re-claimed");
    assert_eq!(
        registry.claim_of("per_1").map(|(mid, _)| mid),
        Some("msg_reply"),
        "the first snapshot stays authoritative"
    );
}

/// A claimed QUESTION block works end-to-end: clicking an option records
/// the answer and the ack returns the re-rendered snapshot showing the
/// live ✅ state; answering the last question submits and drops the block.
#[tokio::test]
async fn snapshot_question_block_answers_and_patches() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.ask_questions(vec![opencode::types::QuestionRequest {
        id: "q_1".into(),
        session_id: "ses_alpha01".into(),
        questions: vec![
            opencode::types::QuestionInfo {
                question: "选择语言".into(),
                header: "language".into(),
                options: vec![
                    opencode::types::QuestionOption {
                        label: "rust".into(),
                        description: String::new(),
                        ..Default::default()
                    },
                    opencode::types::QuestionOption {
                        label: "go".into(),
                        description: String::new(),
                        ..Default::default()
                    },
                ],
                kind: crate::opencode::types::FormFieldKind::String,
                custom: Some(false),
                ..Default::default()
            },
            opencode::types::QuestionInfo {
                question: "选择框架".into(),
                header: "framework".into(),
                options: vec![opencode::types::QuestionOption {
                    label: "axum".into(),
                    description: String::new(),
                    ..Default::default()
                }],
                kind: crate::opencode::types::FormFieldKind::String,
                custom: Some(false),
                ..Default::default()
            },
        ],
        ..Default::default()
    }]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    // The claim remembered the full question request (the poll loop never
    // saw it, so prepare() never ran).
    assert!(app.core.question.has_question("q_1").await);

    // First question answered → still open overall → the snapshot re-renders
    // WITH the block, showing the live ✅ on the answered question.
    let value = |index: u64, answer: &str| {
        serde_json::json!({
            "action": "question",
            "reply": "answer",
            "session_id": "ses_alpha01",
            "request_id": "q_1",
            "directory": "/work/ext",
            "question_index": index,
            "answer": answer,
        })
    };
    let result = app
        .host_action(value(0, "rust"))
        .await
        .expect("option click returns a result");
    let card = result.card.expect("snapshot re-rendered");
    let card_str = card.to_string();
    assert!(card_str.contains("已接管"), "snapshot kept: {card_str}");
    assert!(
        card_str.contains("选择语言"),
        "block still shown with the answered question: {card_str}"
    );
    assert!(
        app.core.snapshot_claims.lock().await.contains("q_1"),
        "still claimed while questions remain"
    );

    // Last question answered → the request submits and the block drops.
    let result = app
        .host_action(value(1, "axum"))
        .await
        .expect("final click returns a result");
    let card = result.card.expect("snapshot re-rendered");
    let card_str = card.to_string();
    assert!(card_str.contains("已接管"), "snapshot kept: {card_str}");
    assert!(
        !card_str.contains("选择语言") && !card_str.contains("选择框架"),
        "resolved question block dropped: {card_str}"
    );
    assert_eq!(
        backend.reply_question_calls.lock().await.as_slice(),
        &[(
            "q_1".to_string(),
            vec![vec!["rust".to_string()], vec!["axum".to_string()]]
        )]
    );
    assert!(
        !app.core.snapshot_claims.lock().await.contains("q_1"),
        "claim dropped after submit"
    );
}

// ===== ADR-0028 busy-adopt follow (ticket 06) =====

/// Adopting a BUSY foreign session arms the snapshot as the host of the
/// external-reply renderer: the running turn's reasoning/text streams
/// INTO the snapshot card (patched in place) and it finalizes Done when
/// the turn completes.
#[tokio::test]
async fn busy_adopt_streams_turn_into_snapshot() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.with_session_status("ses_alpha01", Some(opencode::types::SessionStatus::Busy));
    backend.external_message_for("ses_alpha01", "帮我重构这个模块");
    let reply_ready = backend.external_reply(realistic_parts());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    // The follow armed: the snapshot card hosts a live accumulator for the
    // external turn (epoch = the newest user message's created time).
    let cards = app.core.cards_handle();
    assert_eq!(
        Turn::card_message_id(&cards, "ses_alpha01").await.as_deref(),
        Some("msg_reply")
    );
    assert!(
        Turn::armed_turn_anchor(&cards, "ses_alpha01").await.is_some(),
        "turn anchor set"
    );
    assert!(
        app.core.snapshot_claims.lock().await.claim_count() == 0,
        "busy follow hosts its blocks inline, not claimed"
    );

    // The model answers: flip the reply ready → the parts stream into the
    // SAME snapshot message and finalize Done.
    reply_ready.store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let calls = platform.calls.lock().await.clone();
    let updates: Vec<String> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_reply" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect();
    assert!(
        !updates.is_empty(),
        "the snapshot card was re-rendered: {calls:?}"
    );
    let final_card = updates.last().unwrap();
    assert!(
        final_card.contains("已接管 唯一外部标题"),
        "the 已接管 identity stays visible: {final_card}"
    );
    assert!(
        final_card.contains("当前目录有 src/ 和 Cargo.toml。"),
        "the turn's text streamed into the snapshot: {final_card}"
    );
    assert!(
        final_card.contains("✅ 完成"),
        "the follow finalizes Done: {final_card}"
    );
}

/// The external follow terminates on completion: once the followed turn reads
/// complete, the renderer finalizes the card Done and stops reading the
/// transcript — no eternal poll over a finished run.
#[tokio::test]
async fn a_completed_external_follow_stops_reading() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.with_session_status("ses_alpha01", Some(opencode::types::SessionStatus::Busy));
    backend.external_message_for("ses_alpha01", "帮我重构这个模块");
    let reply_ready = backend.external_reply(realistic_parts());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(20, std::sync::atomic::Ordering::Relaxed);

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;
    assert!(
        Turn::armed_turn_anchor(&app.core.cards_handle(), "ses_alpha01")
            .await
            .is_some(),
        "the busy adopt must arm the external follow"
    );

    // The model answers: the follow finalizes the card Done and exits.
    reply_ready.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_for_card_update(
        &platform,
        "the external follow's Done card",
        CardUpdates::Latest,
        |card| card_header(card).contains("✅"),
    )
    .await;

    // The exit is silent: after the completion PATCH lands, no tick reads the
    // transcript again (the poll cadence is 20 ms).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let reads = backend.transcript_calls.lock().await.len();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        backend.transcript_calls.lock().await.len(),
        reads,
        "a completed external follow must stop reading"
    );
}

/// An external run BLOCKED on a permission resumes from the snapshot: the
/// adopt-time block rides as an inline section on the follow card, the
/// approval takes the normal inline path, and the turn completes inside
/// the SAME card — no standalone permission card, no second snapshot.
#[tokio::test]
async fn busy_follow_permission_approved_resumes() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.with_session_status("ses_alpha01", Some(opencode::types::SessionStatus::Busy));
    backend.external_message_for("ses_alpha01", "帮我重构这个模块");
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    let reply_ready = backend.external_reply(realistic_parts());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    // The adopt-time pending block is pre-seeded as the host's inline
    // section (not claimed — the inline dedupe prevents duplicates).
    let cards = app.core.cards_handle();
    assert_eq!(Turn::live_permissions(&cards, "ses_alpha01").await.len(), 1);
    assert!(app.core.snapshot_claims.lock().await.claim_count() == 0);

    // Approve the block from the snapshot: the normal inline path replies
    // and strips the section — no standalone card, no replacement card.
    let value = serde_json::json!({
        "action": "perm",
        "reply": "once",
        "session_id": "ses_alpha01",
        "request_id": "per_1",
        "directory": "/work/ext",
        "perm_label": "✅ 已允许一次",
        "perm_color": "green",
        "perm_body": "bash",
    });
    let result = app
        .host_action(value)
        .await
        .expect("permission click returns a result");
    let ack = result
        .card
        .as_ref()
        .expect("inline click must carry the updated follow card in the ack")
        .to_string();
    assert!(
        ack.contains("✅ 已允许一次：⚡ 执行 Shell 命令 `ls -la`"),
        "receipt missing: {}",
        ack
    );
    assert!(
        !ack.contains("🔐 **权限请求**"),
        "the approved block must be gone: {}",
        ack
    );
    assert_eq!(result.toast.as_deref(), Some("已允许本次执行"));
    assert!(
        Turn::live_permissions(&app.cards_handle(), "ses_alpha01")
            .await
            .is_empty(),
        "the approved section is no longer live on the follow card"
    );

    // The resumed turn streams into the SAME snapshot message and finishes.
    reply_ready.store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let calls = platform.calls.lock().await.clone();
    let updates: Vec<String> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } if message_id == "msg_reply" => {
                Some(card.to_string())
            }
            _ => None,
        })
        .collect();
    let final_card = updates.last().expect("the follow card streams the resume");
    assert!(
        final_card.contains("✅ 完成") && final_card.contains("已接管"),
        "the run completed inside the snapshot card: {final_card}"
    );
}

/// Adopting an IDLE session stays a static one-shot snapshot: no renderer
/// armed, pendings claimed as usual.
#[tokio::test]
async fn idle_adopt_does_not_arm_follow() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    let (app, _platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    assert!(
        !Turn::has_card(&app.cards_handle(), "ses_alpha01").await,
        "no renderer armed for an idle session"
    );
    assert!(
        app.core.snapshot_claims.lock().await.contains("per_1"),
        "the static snapshot claims its pendings as usual"
    );
}

/// The busy→idle race: busy at gather, idle by arm time → the snapshot
/// stays static, NO renderer is armed, and the embedded pendings are
/// still claimed (never left to be duplicated by the poller).
#[tokio::test]
async fn busy_then_idle_race_stays_static() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.external_message_for("ses_alpha01", "帮我重构这个模块");
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    backend.busy_then_idle_once();
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    assert!(
        !Turn::has_card(&app.cards_handle(), "ses_alpha01").await,
        "no renderer armed for a turn that already finished"
    );
    assert!(
        app.core.snapshot_claims.lock().await.contains("per_1"),
        "the static fallback still claims the embedded pendings"
    );
    // The snapshot card itself was sent with the busy chip (gather saw
    // busy) and no renderer ever touches it.
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls
            .iter()
            .all(|c| !matches!(c, PlatformCall::UpdateMessage { .. })),
        "no re-render of the static snapshot: {calls:?}"
    );
}

/// The follow is scoped to EXTERNAL turns: a busy session whose newest user
/// message is cola-authored (cola itself is answering it) keeps the static
/// snapshot — cola's live accumulator is never re-pointed or overwritten.
#[tokio::test]
async fn busy_follow_skips_cola_authored_turn() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.with_session_status("ses_alpha01", Some(opencode::types::SessionStatus::Busy));
    // The newest user message is cola's OWN (a cola prompt mid-turn).
    backend.cola_message("ses_alpha01", "我在问的问题");
    backend.ask_permission(perm_request("per_1", "ses_alpha01", "ls -la"));
    let (app, _platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    assert!(
        !Turn::has_card(&app.cards_handle(), "ses_alpha01").await,
        "cola's own turn is never followed"
    );
    assert!(
        app.core.snapshot_claims.lock().await.contains("per_1"),
        "the static fallback claims the pendings"
    );
}

/// A QUESTION block on a busy-follow card works: the follow remembers the
/// full request (the poll never saw it), so clicking an option records the
/// answer through the normal inline path.
#[tokio::test]
async fn busy_follow_question_block_resolves() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.with_session_status("ses_alpha01", Some(opencode::types::SessionStatus::Busy));
    backend.external_message_for("ses_alpha01", "帮我重构这个模块");
    backend.ask_questions(vec![opencode::types::QuestionRequest {
        id: "q_1".into(),
        session_id: "ses_alpha01".into(),
        questions: vec![opencode::types::QuestionInfo {
            question: "选择语言".into(),
            header: "language".into(),
            options: vec![opencode::types::QuestionOption {
                label: "rust".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: crate::opencode::types::FormFieldKind::String,
            custom: Some(false),
            ..Default::default()
        }],
        ..Default::default()
    }]);
    let (app, _platform) = build_app(cfg, backend).await;
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    // The follow pre-seeded the inline section AND remembered the request.
    let cards = app.core.cards_handle();
    assert_eq!(Turn::live_questions(&cards, "ses_alpha01").await.len(), 1);
    assert!(app.core.question.has_question("q_1").await);

    // Click an option: the single question is answered → the request
    // submits and the section is stripped from the follow card.
    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "session_id": "ses_alpha01",
        "request_id": "q_1",
        "directory": "/work/ext",
        "question_index": 0,
        "answer": "rust",
    });
    let result = app
        .host_action(value)
        .await
        .expect("question click returns a result");
    assert_eq!(result.toast.as_deref(), Some("已回答"));
    assert!(
        Turn::live_questions(&app.cards_handle(), "ses_alpha01")
            .await
            .is_empty(),
        "the answered question block is stripped from the follow card"
    );
}

/// ADR-0062's ownership routing on an EXTERNAL run: the snapshot follow owns
/// the Session's live card chain (its external renderer does — the render-owned
/// card class, the same routing rule the Wake step reads), so a cola prompt
/// during the follow is a Supplement — it merges into the running work and the
/// Card Chain splits below it, instead of starting a competing Turn that
/// replaces the follow's accumulator. The external renderer keeps streaming
/// into the continuation.
#[tokio::test]
async fn user_prompt_during_follow_merges_as_a_supplement() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.with_session_status("ses_alpha01", Some(opencode::types::SessionStatus::Busy));
    backend.external_message_for("ses_alpha01", "帮我重构这个模块");
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;
    let follow_anchor = Turn::armed_turn_anchor(&app.cards_handle(), "ses_alpha01").await;
    assert!(follow_anchor.is_some(), "follow armed");

    // The user prompts cola in the thread: the live Execution makes it a
    // Supplement, not a competing Turn.
    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "继续".into(),
        None,
    ))
    .await;

    let cards = app.core.cards_handle();
    assert_eq!(
        Turn::armed_turn_anchor(&cards, "ses_alpha01").await,
        follow_anchor,
        "no competing Turn: the external follow keeps its accumulator"
    );
    assert!(
        backend.prompt_calls.lock().await.iter().any(|t| t == "继续"),
        "the Supplement must be submitted to the Backend: {:?}",
        backend.prompt_calls.lock().await
    );
    assert!(
        !platform.texts().await.iter().any(|t| t.contains("还在处理中")),
        "a follow-window message is never answered busy: {:?}",
        platform.calls.lock().await
    );

    // The chain split at the message: a continuation replies to it with the
    // Supplement receipt, and the followed card is finalized.
    supplement_continuation(&platform, "msg_prompt").await;
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            PlatformCall::UpdateMessage { card, .. } if card_header(card).contains("部分完成")
        )),
        "the followed card is finalized with the standard split header: {calls:?}"
    );
}

/// The waiting read the snapshot adopt gathers and the follow renders: an
/// answered external turn that idled with a live background shell.
fn snapshot_waiting_read() -> SessionTranscript {
    SessionTranscript::new(vec![user_text(1_000, "帮我重构这个模块"), finished_stop(2_000)])
        .with_executions(vec![execution(2_500)])
        .with_background_tasks(vec![background_shell(2_100)])
}

/// The woke read: the shell Wake retired the task and its run landed the
/// answer, with the Execution boundary after the Wake answering it.
fn snapshot_woken_read() -> SessionTranscript {
    SessionTranscript::new(vec![
        user_text(1_000, "帮我重构这个模块"),
        finished_stop(2_000),
        assistant_text(3_600, "CI 通过了。"),
    ])
    .with_executions(vec![execution(2_500), execution(4_000)])
    .with_wakes(vec![shell_wake(2_900)])
}

fn user_text(created: i64, text: &str) -> TranscriptMessage {
    typed_message(
        "msg_user_1000",
        MessageRole::User,
        Some(created),
        vec![text_part(text)],
    )
}

fn finished_stop(created: i64) -> TranscriptMessage {
    typed_message(
        "msg_ext_assist",
        MessageRole::Assistant,
        Some(created),
        vec![Part::StepFinish(StepFinish {
            reason: FinishReason::Stop,
        })],
    )
}

fn assistant_text(created: i64, text: &str) -> TranscriptMessage {
    typed_message(
        "msg_ext_resumed",
        MessageRole::Assistant,
        Some(created),
        vec![text_part(text)],
    )
}

/// #568: the snapshot follow's in-flight external turn yields ⏳ on a live
/// Background Task (never ✅), and Session Sync's completion Wake resumes the
/// snapshot card in place to the true end — the reply arm's contract, driven
/// through the `/switch` busy-adopt arm.
#[tokio::test]
async fn busy_snapshot_follow_yields_and_resumes_in_place() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    backend.with_session_status("ses_alpha01", Some(opencode::types::SessionStatus::Busy));
    backend.given_transcript("ses_alpha01", vec![snapshot_waiting_read()]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    // The followed turn idles with a live Background Task: the snapshot card
    // yields ⏳ in place.
    wait_for_card_update(
        &platform,
        "the snapshot's waiting yield",
        CardUpdates::Latest,
        |card| card_header(card).contains("等待后台任务"),
    )
    .await;

    // The task retires and its run lands the answer; Session Sync resumes the
    // snapshot card in place and settles ✅ there.
    // The task retires and its run lands the answer; the server goes idle, so
    // the Wake's settlement loop can read the true end. Session Sync resumes
    // the snapshot card in place.
    spawn_sync(&app);
    backend
        .set_session_status("ses_alpha01", Some(opencode::types::SessionStatus::Idle))
        .await;
    backend
        .given_transcript_after_build("ses_alpha01", vec![snapshot_woken_read()])
        .await;
    wait_for_card_update(&platform, "the true end", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    let last = calls
        .iter()
        .rev()
        .find_map(|call| match call {
            PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
            _ => None,
        })
        .expect("the snapshot card was updated");
    assert_eq!(last.0, "msg_reply", "the snapshot card resumes in place");
    let text = card_text(&last.1);
    assert!(
        text.contains("已接管 唯一外部标题"),
        "the snapshot identity stays visible: {last:?}"
    );
    assert!(
        text.contains("CI 通过了。"),
        "the resumed content lands: {last:?}"
    );
    assert!(
        text.contains("🔔 shell 完成：gh run watch"),
        "the completion entry lands on the same card: {last:?}"
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "an in-place resume posts no continuation card: {calls:?}"
    );
}

/// #586: an IDLE-but-Waiting external session adopted via `/switch` — the
/// server reads idle while a live Background Task remains (ADR-0059) — is
/// followed exactly like a busy one: the snapshot carries the 等待后台任务
/// chip, the follow's first settle read yields ⏳ with the ledger in place,
/// and Session Sync resumes the card in place to the true end. Before the
/// fix this adoption froze the static 空闲 snapshot and every later Wake was
/// invisible.
#[tokio::test]
async fn waiting_adopt_yields_and_resumes_in_place() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    // The gather's read: the Execution ended (the server reads idle) with the
    // Background Task still live.
    backend.given_transcript("ses_alpha01", vec![snapshot_waiting_read()]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    // The snapshot names the wait instead of 空闲, and the follow is armed.
    let calls = platform.calls.lock().await.clone();
    let snapshot = calls
        .iter()
        .find_map(|call| match call {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .expect("the snapshot was sent");
    assert!(
        snapshot.contains(crate::feishu::snapshot_card::WAITING_BACKGROUND_CHIP),
        "the waiting chip names the wait: {snapshot}"
    );
    assert!(
        snapshot.contains(crate::feishu::snapshot_card::FOLLOW_HINT),
        "the waiting chip carries the follow hint: {snapshot}"
    );
    assert!(
        !snapshot.contains(crate::feishu::snapshot_card::IDLE_CHIP),
        "the static 空闲 chip must not render: {snapshot}"
    );
    assert!(
        Turn::armed_turn_anchor(&app.core.cards_handle(), "ses_alpha01")
            .await
            .is_some(),
        "a waiting adopt arms the follow"
    );

    // The follow's first settle read idles with the live task: ⏳ in place.
    // The follow's first settle read idles with the live task: ⏳ in place,
    // carrying the Background Task Ledger.
    wait_for_card_update(&platform, "the waiting yield", CardUpdates::Latest, |card| {
        card_header(card).contains("等待后台任务")
            && card.to_string().contains("task_ledger")
            && card.to_string().contains("后台任务（1）")
    })
    .await;

    // The task retires and its run lands the answer; Session Sync resumes the
    // snapshot card in place and settles ✅ there.
    spawn_sync(&app);
    backend
        .given_transcript_after_build("ses_alpha01", vec![snapshot_woken_read()])
        .await;
    wait_for_card_update(&platform, "the true end", CardUpdates::Latest, |card| {
        card_header(card).contains("✅")
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    let last = calls
        .iter()
        .rev()
        .find_map(|call| match call {
            PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
            _ => None,
        })
        .expect("the snapshot card was updated");
    assert_eq!(last.0, "msg_reply", "the snapshot card resumes in place");
    let text = card_text(&last.1);
    assert!(
        text.contains("已接管 唯一外部标题"),
        "the snapshot identity stays visible: {last:?}"
    );
    assert!(
        text.contains("等待后台任务完成"),
        "the waiting-copy static line survives: {last:?}"
    );
    assert!(
        text.contains("CI 通过了。"),
        "the resumed content lands: {last:?}"
    );
    assert!(
        text.contains("🔔 shell 完成：gh run watch"),
        "the completion entry lands on the same card: {last:?}"
    );
    assert!(
        continuation_sends(&platform).await.is_empty(),
        "an in-place resume posts no continuation card: {calls:?}"
    );
}

/// #586 boundary: a Waiting session whose newest user message is
/// cola-authored keeps the static snapshot — cola's own accumulator is never
/// re-pointed, exactly like the busy guard.
#[tokio::test]
async fn waiting_adopt_cola_authored_turn_stays_static() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![list_session(
        "ses_alpha01",
        "唯一外部标题",
        "/work/ext",
        100,
    )]);
    // Cola's own prompt is the newest user message, and its finished Turn
    // idles with a live Background Task (idle = the mock's absent-session
    // default) — so only the cola-authored guard keeps this static.
    backend.given_transcript(
        "ses_alpha01",
        vec![
            SessionTranscript::new(vec![
                typed_message(
                    "msg_cola_1",
                    MessageRole::User,
                    Some(1_000),
                    vec![text_part("我在问的问题")],
                ),
                typed_message(
                    "msg_a1",
                    MessageRole::Assistant,
                    Some(2_000),
                    vec![Part::StepFinish(StepFinish {
                        reason: FinishReason::Stop,
                    })],
                ),
            ])
            .with_executions(vec![execution(2_500)])
            .with_background_tasks(vec![background_shell(1_100)]),
        ],
    );
    let (app, _platform) = build_app(cfg, backend).await;

    send_command(&app, "/switch 唯一外部标题", "msg_switch").await;

    assert!(
        !Turn::has_card(&app.cards_handle(), "ses_alpha01").await,
        "cola's own turn is never followed"
    );
}
