use crate::bridge::command::*;
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The adopt claimed the embedded pending against the sent snapshot.
    let claims = app.core.snapshot_claims.lock().await;
    assert_eq!(
        claims
            .claims
            .get("per_1")
            .map(|(mid, kind)| (mid.as_str(), *kind)),
        Some(("msg_reply", crate::bridge::request::ClaimKind::Permission))
    );

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.core).await;
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

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
        .handle_card_action(value)
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
/// the snapshot (patched in place) — and never marks the snapshot itself
/// stale, since stale is for standalone cards (ADR-0028).
#[tokio::test]
async fn snapshot_block_drops_when_resolved_elsewhere() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    // Another client answers the request server-side.
    backend
        .replied_permissions
        .lock()
        .await
        .insert("per_1".to_string());

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.core).await;
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    // One adopt-time pending (claimed by the snapshot)…
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    // …and a SECOND request arriving after the snapshot was sent.
    backend
        .extra_permissions
        .lock()
        .await
        .push(perm_request("per_2", "ses_alpha01", "rm -rf /tmp/x"));

    tokio::spawn({
        let app = app.clone();
        async move {
            app.permission
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.permission.poll_loop(&app.core).await;
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    let (app, platform) = build_app(cfg, backend).await;
    // The request was already surfaced as a standalone card.
    app.permission
        .sent_cards
        .lock()
        .await
        .insert("per_1".into(), ("om_existing".into(), "bash".into()));

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    let (app, platform) = build_app(cfg, backend).await;

    // First adopt: the snapshot claims the pending block.
    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // Re-`/switch` to the now-mapped session (mapped-hit branch).
    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch_2",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

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
    assert_eq!(registry.claims.len(), 1, "one claim, not re-claimed");
    assert_eq!(
        registry.message_of("per_1"),
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend.questions = vec![opencode::client::QuestionRequest {
        id: "q_1".into(),
        session_id: "ses_alpha01".into(),
        questions: vec![
            opencode::client::QuestionInfo {
                question: "选择语言".into(),
                header: "language".into(),
                options: vec![
                    opencode::client::QuestionOption {
                        label: "rust".into(),
                        description: String::new(),
                    },
                    opencode::client::QuestionOption {
                        label: "go".into(),
                        description: String::new(),
                    },
                ],
                multiple: Some(false),
                custom: Some(false),
            },
            opencode::client::QuestionInfo {
                question: "选择框架".into(),
                header: "framework".into(),
                options: vec![opencode::client::QuestionOption {
                    label: "axum".into(),
                    description: String::new(),
                }],
                multiple: Some(false),
                custom: Some(false),
            },
        ],
    }];
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The claim remembered the full question request (the poll loop never
    // saw it, so prepare() never ran).
    assert!(
        app.core
            .question
            .question_requests
            .lock()
            .await
            .contains_key("q_1")
    );

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
        .handle_card_action(value(0, "rust"))
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
        .handle_card_action(value(1, "axum"))
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend
        .session_statuses
        .insert("ses_alpha01".into(), Some(opencode::client::SessionStatus::Busy));
    backend
        .external_user_messages
        .insert("ses_alpha01".into(), "帮我重构这个模块".into());
    backend.external_reply_parts = Some(realistic_parts());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The follow armed: the snapshot card hosts a live accumulator for the
    // external turn (epoch = the newest user message's created time).
    let acc = app
        .core
        .cards
        .lock()
        .await
        .get("ses_alpha01")
        .cloned()
        .expect("follow armed a host accumulator");
    assert_eq!(acc.card_message_id.as_deref(), Some("msg_reply"));
    assert!(acc.acc.submit_epoch_ms.is_some(), "turn epoch set");
    assert!(
        app.core.snapshot_claims.lock().await.claims.is_empty(),
        "busy follow hosts its blocks inline, not claimed"
    );

    // The model answers: flip the reply ready → the parts stream into the
    // SAME snapshot message and finalize Done.
    backend
        .external_reply_ready
        .store(true, std::sync::atomic::Ordering::SeqCst);
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend
        .session_statuses
        .insert("ses_alpha01".into(), Some(opencode::client::SessionStatus::Busy));
    backend
        .external_user_messages
        .insert("ses_alpha01".into(), "帮我重构这个模块".into());
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    backend.external_reply_parts = Some(realistic_parts());
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The adopt-time pending block is pre-seeded as the host's inline
    // section (not claimed — the inline dedupe prevents duplicates).
    let acc = app
        .core
        .cards
        .lock()
        .await
        .get("ses_alpha01")
        .cloned()
        .expect("follow armed");
    assert_eq!(acc.acc.pending_permissions.len(), 1);
    assert!(app.core.snapshot_claims.lock().await.claims.is_empty());

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
        .handle_card_action(value)
        .await
        .expect("permission click returns a result");
    assert!(
        result.card.is_none(),
        "inline answer must not replace the follow card"
    );
    assert_eq!(result.toast.as_deref(), Some("已允许本次执行"));
    assert!(
        app.core
            .cards
            .lock()
            .await
            .get("ses_alpha01")
            .unwrap()
            .acc
            .pending_permissions
            .is_empty(),
        "the approved section is stripped from the follow card"
    );

    // The resumed turn streams into the SAME snapshot message and finishes.
    backend
        .external_reply_ready
        .store(true, std::sync::atomic::Ordering::SeqCst);
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    let (app, _platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    assert!(
        !app.core.cards.lock().await.contains_key("ses_alpha01"),
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend
        .external_user_messages
        .insert("ses_alpha01".into(), "帮我重构这个模块".into());
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    backend
        .status_busy_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (app, platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    assert!(
        !app.core.cards.lock().await.contains_key("ses_alpha01"),
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend
        .session_statuses
        .insert("ses_alpha01".into(), Some(opencode::client::SessionStatus::Busy));
    // The newest user message is cola's OWN (a cola prompt mid-turn).
    backend
        .cola_user_messages
        .insert("ses_alpha01".into(), "我在问的问题".into());
    backend.permissions = vec![perm_request("per_1", "ses_alpha01", "ls -la")];
    let (app, _platform) = build_app(cfg, backend).await;

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    assert!(
        !app.core.cards.lock().await.contains_key("ses_alpha01"),
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
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend
        .session_statuses
        .insert("ses_alpha01".into(), Some(opencode::client::SessionStatus::Busy));
    backend
        .external_user_messages
        .insert("ses_alpha01".into(), "帮我重构这个模块".into());
    backend.questions = vec![opencode::client::QuestionRequest {
        id: "q_1".into(),
        session_id: "ses_alpha01".into(),
        questions: vec![opencode::client::QuestionInfo {
            question: "选择语言".into(),
            header: "language".into(),
            options: vec![opencode::client::QuestionOption {
                label: "rust".into(),
                description: String::new(),
            }],
            multiple: Some(false),
            custom: Some(false),
        }],
    }];
    let (app, _platform) = build_app(cfg, backend).await;
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();

    // The follow pre-seeded the inline section AND remembered the request.
    let acc = app
        .core
        .cards
        .lock()
        .await
        .get("ses_alpha01")
        .cloned()
        .expect("follow armed");
    assert_eq!(acc.acc.pending_questions.len(), 1);
    assert!(
        app.core
            .question
            .question_requests
            .lock()
            .await
            .contains_key("q_1")
    );

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
        .handle_card_action(value)
        .await
        .expect("question click returns a result");
    assert_eq!(result.toast.as_deref(), Some("已回答"));
    assert!(
        app.core
            .cards
            .lock()
            .await
            .get("ses_alpha01")
            .unwrap()
            .acc
            .pending_questions
            .is_empty(),
        "the answered question block is stripped from the follow card"
    );
}

/// A cola prompt during the follow takes over: `run_prompt` replaces the
/// accumulator, the follow renderer exits, and the user's own turn
/// renders into its own card — the snapshot is never touched again.
#[tokio::test]
async fn user_prompt_during_follow_takes_over() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_alpha01", "唯一外部标题", "/work/ext", 100)];
    backend
        .session_statuses
        .insert("ses_alpha01".into(), Some(opencode::client::SessionStatus::Busy));
    backend
        .external_user_messages
        .insert("ses_alpha01".into(), "帮我重构这个模块".into());
    let (app, platform) = build_app(cfg, backend).await;
    app.core
        .external
        .render_poll_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    crate::bridge::command::handle_command(
        &app.core,
        Command::Switch(SwitchAction::Match("唯一外部标题".into())),
        crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        "msg_switch",
        crate::config::ConversationKind::P2p,
    )
    .await
    .unwrap();
    let follow_epoch = app
        .core
        .cards
        .lock()
        .await
        .get("ses_alpha01")
        .cloned()
        .expect("follow armed")
        .acc
        .submit_epoch_ms;

    // The user prompts cola in the thread.
    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "继续".into(),
        None,
    ))
    .await;

    let new_epoch = app
        .core
        .cards
        .lock()
        .await
        .get("ses_alpha01")
        .cloned()
        .expect("the prompt inserted its own accumulator")
        .acc
        .submit_epoch_ms;
    assert_ne!(new_epoch, follow_epoch, "the follow accumulator was replaced");
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, PlatformCall::ReplyCard { reply_to, .. } if reply_to == "msg_prompt")),
        "the user's own turn rendered into its own card: {calls:?}"
    );
}
