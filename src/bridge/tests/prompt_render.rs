use crate::bridge::test_support::*;

use crate::backend::{
    MessageId, MessageRole, MessageTime, Part, SessionTranscript, ToolCall, ToolIdentity, ToolOutput,
    ToolStatus, TranscriptMessage,
};

/// A direct Turn context for the retry tests: the handler path is exercised by
/// `host_action`, this seeds the failed attempt. An empty `cola_message_id`
/// means "generate a fresh one".
fn retry_ctx(session_id: &str, text: &str, cola_message_id: &str) -> crate::bridge::turn::PromptContext {
    crate::bridge::turn::PromptContext {
        session_id: session_id.into(),
        thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        text: text.into(),
        message_id: "msg_1".into(),
        subtitle: "p2p".into(),
        is_retry: false,
        requester_open_id: None,
        is_group: false,
        cola_message_id: (!cola_message_id.is_empty()).then(|| cola_message_id.to_string()),
        images: Vec::new(),
        advisory_live: false,
    }
}

/// Await at least `len` recorded prompt message ids, or panic after 5 s. The
/// retry's id is recorded at prompt entry, before any gate the test installed,
/// so this stays deterministic when the prompt is held.
async fn wait_for_prompt_ids(
    ids: &Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
    len: usize,
) -> Vec<Option<String>> {
    let wait = async {
        loop {
            let current = ids.lock().await.clone();
            if current.len() >= len {
                return current;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(5), wait).await {
        Ok(ids) => ids,
        Err(_) => panic!("the retry prompt was never recorded (wanted {len} ids)"),
    }
}

/// Assert the retry click's immediate ack (spec #391): the generic toast, and
/// no card replacement — the failed card must never be overwritten.
fn assert_retry_ack(result: Option<crate::bridge::handler::CardActionResult>) {
    let ack = result.expect("retry ack expected");
    assert_eq!(ack.toast.as_deref(), Some("正在重试..."));
    assert!(
        ack.card.is_none(),
        "the retry ack must not overwrite the failed card"
    );
}

/// The `↩️ 已重试` marked card among the recorded platform calls.
async fn marked_card(platform: &RecordingPlatform) -> serde_json::Value {
    platform
        .calls
        .lock()
        .await
        .iter()
        .find_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } if card_header(card) == "↩️ 已重试" => {
                Some(card.clone())
            }
            _ => None,
        })
        .expect("the failed card must be marked Retried")
}

/// #378: the kill-shaped orphan — a run the server was killed in: no
/// completion stamp, a `running` tool part whose newest activity long predates
/// the Turn anchor (verified on 2.0.18: a restart settles neither), and the
/// failure the server had recorded before it died.
fn orphaned_in_flight_message(
    created: i64,
    tool_started: i64,
    output: &str,
    error: Option<&str>,
) -> TranscriptMessage {
    TranscriptMessage {
        id: MessageId::new("msg_zombie"),
        role: MessageRole::Assistant,
        time: Some(MessageTime {
            created,
            completed: None,
        }),
        model: None,
        tokens: None,
        error: error.map(str::to_string),
        parts: vec![Part::Tool(ToolCall {
            identity: ToolIdentity {
                name: "bash".into(),
                call_id: "call_zombie".into(),
            },
            status: ToolStatus::Running,
            started_at: Some(tool_started),
            input: Some(serde_json::json!({ "command": "sleep 120" })),
            metadata: None,
            output: ToolOutput {
                raw: Some(serde_json::json!(output)),
                blocks: Vec::new(),
                error: None,
            },
        })],
    }
}

#[tokio::test]
async fn handle_prompt_renders_reasoning_tools_and_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "分析一下目录".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    // First call must be the Loading reply card.
    assert!(matches!(calls.first(), Some(PlatformCall::ReplyCard { .. })));
    // At least one card update (flush) must follow.
    let updates = platform.updated_cards().await;
    assert!(!updates.is_empty(), "expected card updates, got: {:?}", calls);

    let final_card = updates.last().unwrap().clone();
    let text = final_card.to_string();
    assert!(text.contains("✅"), "final header should be Done: {}", text);
    assert!(text.contains("推理过程"), "reasoning panel missing: {}", text);
    assert!(text.contains("bash"), "tool panel missing: {}", text);
    assert!(
        text.contains("当前目录有 src/ 和 Cargo.toml。"),
        "text missing: {}",
        text
    );
    assert!(text.contains("ls -la"), "tool input missing: {}", text);
}

/// ADR-0019: the Turn Footer's work context is captured at turn start AND
/// refreshed at turn end — the final card shows the branch the AI landed on
/// (here one it created and committed to), not the one it started from.
#[tokio::test]
async fn turn_footer_shows_the_branch_the_ai_landed_on() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo();
    let repo_path = repo.path().to_path_buf();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo_path.clone());

    let mut mock = MockBackend::new(realistic_parts());
    mock.on_prompt(move || {
        // The AI's work: branch off and commit — the tree ends clean.
        git_in(&repo_path, &["switch", "-c", "feat/ai-work"]);
        std::fs::write(repo_path.join("b.txt"), "done").unwrap();
        git_in(&repo_path, &["add", "b.txt"]);
        git_in(&repo_path, &["commit", "-m", "ai work"]);
    });
    let (app, platform) = build_app(cfg, mock).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "干个活".into(),
        None,
    ))
    .await;

    let text = final_card(&platform).await.to_string();
    assert!(
        text.contains("· feat/ai-work"),
        "final footer must show the end branch: {text}"
    );
    assert!(
        !text.contains("· main"),
        "the start branch must be gone from the final card: {text}"
    );
    assert!(
        !text.contains("⚠"),
        "a committed tree must not show the dirty marker: {text}"
    );
}

/// ADR-0019: a turn that ends with uncommitted changes lights the ⚠ on the
/// final card even though the tree was clean when the turn started.
#[tokio::test]
async fn turn_footer_shows_dirty_left_by_the_ai() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo();
    let repo_path = repo.path().to_path_buf();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo_path.clone());

    let mut mock = MockBackend::new(realistic_parts());
    mock.on_prompt(move || {
        std::fs::write(repo_path.join("uncommitted.txt"), "left behind").unwrap();
    });
    let (app, platform) = build_app(cfg, mock).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "改点东西".into(),
        None,
    ))
    .await;

    let text = final_card(&platform).await.to_string();
    assert!(
        text.contains("· main ⚠"),
        "final footer must show the AI's uncommitted changes: {text}"
    );
}

/// ADR-0019: a PARTIAL turn-end read — the branch still resolves but the
/// `status` command fails (e.g. index lock contention) — must keep the
/// start capture. A failed status read is not a clean tree; clearing the
/// start ⚠ would silently lie about the working tree.
#[tokio::test]
async fn turn_footer_keeps_start_capture_when_the_end_status_read_fails() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo();
    let repo_path = repo.path().to_path_buf();
    // The tree starts dirty (untracked file): the start card shows `main ⚠`.
    std::fs::write(repo_path.join("pre-existing.txt"), "wip").unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo_path.clone());

    let mut mock = MockBackend::new(realistic_parts());
    mock.on_prompt(move || {
        // Break the index mid-turn: `rev-parse` reads HEAD and still
        // resolves `main`, while `git status --porcelain` fails.
        let index = repo_path.join(".git/index");
        std::fs::remove_file(&index).unwrap();
        std::fs::create_dir(&index).unwrap();
    });
    let (app, platform) = build_app(cfg, mock).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "干点活".into(),
        None,
    ))
    .await;

    let text = final_card(&platform).await.to_string();
    assert!(
        text.contains("· main ⚠"),
        "a failed end read must keep the start capture: {text}"
    );
}

#[tokio::test]
async fn prompt_error_renders_error_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.fail_prompt("Streaming response failed: [503] The request queue is full.");
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let updates: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !updates.is_empty(),
        "expected an error card update, got: {:?}",
        calls
    );
    let card = updates.last().unwrap().to_string();
    assert!(card.contains("❌"), "error card header missing: {}", card);
    assert!(card.contains("503"), "error text missing: {}", card);
}

/// A failure the server recorded on the assistant message (V1's `info.error`,
/// V2's message `error`) ends the submitted turn as an Error card: the
/// async-native Turn reads the turn's failure from the transcript it observes,
/// not from a blocking prompt response (ADR-0056).
#[tokio::test]
async fn transcript_recorded_failure_renders_error_card() {
    use crate::backend::{MessageRole, SessionTranscript};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // The turn the server settled: the admitted user message plus an assistant
    // message carrying the provider failure.
    let mut failed = typed_message(
        "msg_assist",
        MessageRole::Assistant,
        Some(2_000),
        vec![text_part("部分回答")],
    );
    failed.error = Some("provider 503".into());
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            typed_message(
                "msg_cola_failed",
                MessageRole::User,
                Some(1_000),
                vec![text_part("hi")],
            ),
            failed,
        ])],
    );
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);

    let context = crate::bridge::turn::PromptContext {
        session_id: "ses_test".into(),
        thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        text: "hi".into(),
        message_id: "msg_1".into(),
        subtitle: "p2p".into(),
        is_retry: false,
        requester_open_id: None,
        is_group: false,
        cola_message_id: Some("msg_cola_failed".into()),
        images: Vec::new(),
        advisory_live: false,
    };
    crate::bridge::turn::Turn::run(&app.turn_handles(), context)
        .await
        .unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = final_card.to_string();
    assert!(text.contains("❌"), "error card header missing: {text}");
    assert!(
        text.contains("provider 503"),
        "the transcript's failure must reach the card: {text}"
    );
    assert!(
        text.contains("部分回答"),
        "content produced before the failure still renders: {text}"
    );
}

/// The error-card retry's shape: a failed step followed by a clean one in the
/// SAME turn (the server re-runs a failed retry as a new step under the same
/// user anchor) must finish Done — the newest assistant message is
/// authoritative, so a recovered earlier step is not a failure.
#[tokio::test]
async fn transcript_recovered_step_finishes_done() {
    use crate::backend::{MessageRole, SessionTranscript};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    let mut failed = typed_message(
        "msg_failed",
        MessageRole::Assistant,
        Some(2_000),
        vec![text_part("失败的一步")],
    );
    failed.error = Some("provider 503".into());
    let recovered = typed_message(
        "msg_recovered",
        MessageRole::Assistant,
        Some(3_000),
        realistic_parts(),
    );
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            typed_message(
                "msg_cola_recovered",
                MessageRole::User,
                Some(1_000),
                vec![text_part("hi")],
            ),
            failed,
            recovered,
        ])],
    );
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);

    let context = crate::bridge::turn::PromptContext {
        session_id: "ses_test".into(),
        thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
        text: "hi".into(),
        message_id: "msg_1".into(),
        subtitle: "p2p".into(),
        is_retry: false,
        requester_open_id: None,
        is_group: false,
        cola_message_id: Some("msg_cola_recovered".into()),
        images: Vec::new(),
        advisory_live: false,
    };
    crate::bridge::turn::Turn::run(&app.turn_handles(), context)
        .await
        .unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = final_card.to_string();
    assert!(
        text.contains("✅"),
        "a recovered trailing step must finish Done: {text}"
    );
    assert!(
        !text.contains("provider 503"),
        "the recovered failure must not surface: {text}"
    );
}

/// The unstored-submission cell of the retry matrix (spec #391): the first
/// submit was rejected, so nothing persisted and there is no reply to settle.
/// The retry reuses the failed attempt's id (idempotent — the server may
/// create it) and, per the new contract, replies a NEW card below the failed
/// one instead of overwriting the failure.
#[tokio::test]
async fn error_card_retry_reuses_the_unstored_id_and_replies_a_new_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    // First prompt fails (provider hiccup); the retry must succeed.
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    let prompt_calls = mock.prompt_calls.clone();
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());

    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    // The error card must carry a retry button.
    let calls = platform.calls.lock().await.clone();
    let updates: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect();
    let error_card = updates.last().unwrap().clone();
    let err_text = error_card.to_string();
    assert!(err_text.contains("❌"), "error card missing: {}", err_text);
    assert!(err_text.contains("重试"), "retry button missing: {}", err_text);

    // The card the first attempt used: the loading reply card id.
    let card_id = match calls.first().unwrap() {
        PlatformCall::ReplyCard { card, .. } if card_text(card).contains("思考中") => "msg_reply",
        _ => panic!("expected a loading reply card first: {:?}", calls),
    };
    assert_eq!(card_id, "msg_reply");

    // User clicks the retry button.
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);

    // The retry's attempt renders on a NEW card; the failed card is marked
    // Retried first, keeping all its content.
    wait_for_card_update(&platform, "the retried Done card", CardUpdates::Latest, |card| {
        card_header(card) == "✅ 完成" && card_text(card).contains("当前目录有 src/ 和 Cargo.toml。")
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    let replies: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        replies.len(),
        2,
        "the retry must reply a new card below the failed one: {calls:?}"
    );

    let marked = marked_card(&platform).await;
    let marked_text = card_text(&marked);
    assert!(
        marked_text.contains("Simulated provider failure"),
        "the failed content must stay readable: {marked_text}"
    );
    assert!(
        marked_text.contains("已重试，见下方新卡片"),
        "the marker line is missing: {marked_text}"
    );
    assert!(
        card_buttons(&marked)
            .iter()
            .all(|b| { b["value"].get("action").and_then(|a| a.as_str()) != Some("retry") }),
        "a Retried card must not offer another retry: {marked}"
    );

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert_eq!(card_header(&final_card), "✅ 完成");
    assert!(
        card_text(&final_card).contains("当前目录有 src/ 和 Cargo.toml。"),
        "the new card must render the retry's answer: {final_card}"
    );

    let backend_calls = prompt_calls.lock().await.clone();
    assert_eq!(backend_calls, vec!["hi".to_string(), "hi".to_string()]);
    // Both attempts are the SAME logical user message: a fresh `msg_cola_`
    // id on the first send, REUSED by the retry (ADR-0026) because nothing of
    // the first submission persisted — the server creates it now.
    let message_ids = prompt_ids.lock().await.clone();
    assert_eq!(message_ids.len(), 2, "two prompt attempts expected");
    let first = message_ids[0].clone().expect("prompt must carry a message id");
    assert!(
        first.starts_with("msg_cola_"),
        "cola prompt must self-identify: {}",
        first
    );
    assert_eq!(
        message_ids[1].as_deref(),
        Some(first.as_str()),
        "an unstored submission must reuse the failed attempt's message id"
    );
}

/// The settled-failure cell of the retry matrix (spec #391): the failed turn's
/// newest assistant reply carries a terminal finish, so a same-id re-post
/// would be a server no-op. The retry takes a FRESH `msg_cola_` id, marks the
/// failed card `Retried` and replies a new card that renders only the new
/// attempt.
#[tokio::test]
async fn settled_failure_retry_submits_a_new_id_on_a_new_card() {
    use crate::backend::{FinishReason, StepFinish};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    const ANCHOR: &str = "msg_cola_settled";
    let mut mock = MockBackend::new(realistic_parts());
    // The retry's prompt is held after its id is recorded, so the test can
    // swap the transcript script between the decision read and the retry's
    // own render.
    let gate = mock.hold_prompts();
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    // The turn as the server settled it: the admitted user message plus an
    // assistant message carrying the provider failure and a terminal finish.
    let mut failed = typed_message(
        "msg_a_err",
        MessageRole::Assistant,
        Some(2_000),
        vec![
            text_part("失败尝试：OLD_TEXT"),
            Part::StepFinish(StepFinish {
                reason: FinishReason::Error,
            }),
        ],
    );
    failed.error = Some("provider 503".into());
    *backend
        .transcript_scripts
        .lock()
        .await
        .entry("ses_test".into())
        .or_default() = vec![SessionTranscript::new(vec![
        typed_message(ANCHOR, MessageRole::User, Some(1_000), vec![text_part("hi")]),
        failed,
    ])];

    // Attempt 1 (the settled failure): the transcript above ends it Error.
    gate.add_permits(1);
    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ANCHOR))
        .await
        .unwrap();
    wait_for_card_update(
        &platform,
        "the settled failure's Error card",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            text.contains("❌") && text.contains("OLD_TEXT")
        },
    )
    .await;

    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);

    // The retry's prompt blocks on the gate with its fresh id already
    // recorded: the decision read has happened (it saw the settled window) and
    // the failed card has been marked. Drop the script so the retry's own
    // render reads the mock's default transcript, which carries that fresh id.
    let ids = wait_for_prompt_ids(&prompt_ids, 2).await;
    backend.transcript_scripts.lock().await.remove("ses_test");
    gate.add_permits(1);

    let new_id = ids[1].clone().expect("the retry prompt carries an id");
    assert_ne!(
        ANCHOR,
        new_id.as_str(),
        "a settled failure must retry under a NEW msg_cola_ id"
    );

    wait_for_card_update(&platform, "the retry's Done card", CardUpdates::Latest, |card| {
        card_header(card) == "✅ 完成" && card_text(card).contains("当前目录有 src/ 和 Cargo.toml。")
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    let replies = calls
        .iter()
        .filter(|call| matches!(call, PlatformCall::ReplyCard { .. }))
        .count();
    assert_eq!(
        replies, 2,
        "the retry must reply a new card below the failed one: {calls:?}"
    );
    let marked = marked_card(&platform).await;
    let marked_text = card_text(&marked);
    assert!(
        marked_text.contains("OLD_TEXT") && marked_text.contains("provider 503"),
        "all failed content must stay on the marked card: {marked_text}"
    );
    assert!(
        marked_text.contains("已重试，见下方新卡片"),
        "the marker line is missing: {marked_text}"
    );
    assert!(
        card_buttons(&marked)
            .iter()
            .all(|b| { b["value"].get("action").and_then(|a| a.as_str()) != Some("retry") }),
        "a Retried card must not offer another retry: {marked}"
    );

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let final_text = card_text(&final_card);
    assert_eq!(card_header(&final_card), "✅ 完成");
    assert!(
        final_text.contains("当前目录有 src/ 和 Cargo.toml。"),
        "the new attempt must render on the new card: {final_text}"
    );
    assert!(
        !final_text.contains("OLD_TEXT"),
        "the failed attempt must not replay onto the new card: {final_text}"
    );
}

/// The unknown cell (spec #391): a failed status read is not guessed — the
/// click still has an effect, submitting a fresh attempt under a new id.
#[tokio::test]
async fn retry_after_a_failed_status_read_submits_a_new_id() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ""))
        .await
        .unwrap();
    wait_for_card_update(&platform, "the Error card", CardUpdates::Latest, |card| {
        card_header(card) == "❌ 出错"
    })
    .await;

    // From now on the status read fails: the decision is unknown.
    backend
        .session_status_fails
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "retry ack expected");

    let ids = wait_for_prompt_ids(&prompt_ids, 2).await;
    assert_ne!(
        ids[0], ids[1],
        "an unknown decision must submit a fresh id (click must have an effect)"
    );

    wait_for_card_update(&platform, "the retried Done card", CardUpdates::Latest, |card| {
        card_header(card) == "✅ 完成" && card_text(card).contains("当前目录有 src/ 和 Cargo.toml。")
    })
    .await;
}

/// The unknown cell's timeout arm: a hung decision transcript read is bounded
/// (the follow's per-read timeout) and degrades to a fresh-id submit, so the
/// click can never silently do nothing.
#[tokio::test]
async fn retry_after_a_timed_out_transcript_read_submits_a_new_id() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ""))
        .await
        .unwrap();
    wait_for_card_update(&platform, "the Error card", CardUpdates::Latest, |card| {
        card_header(card) == "❌ 出错"
    })
    .await;

    // The decision's transcript read hangs; bound it tightly so the test stays
    // fast (the render/drain reads later consume no hang).
    app.turn_follow_read_timeout_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    backend
        .hang_transcript
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "retry ack expected");

    let ids = wait_for_prompt_ids(&prompt_ids, 2).await;
    assert_ne!(
        ids[0], ids[1],
        "a timed-out decision must submit a fresh id (click must have an effect)"
    );

    wait_for_card_update(&platform, "the retried Done card", CardUpdates::Latest, |card| {
        card_header(card) == "✅ 完成" && card_text(card).contains("当前目录有 src/ 和 Cargo.toml。")
    })
    .await;
}

/// A failed marker PATCH only warns (spec #391): the retry still submits onto
/// its new card, so a Feishu hiccup on the old card never blocks the retry.
#[tokio::test]
async fn a_failed_retried_marker_patch_does_not_block_the_retry() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ""))
        .await
        .unwrap();
    wait_for_card_update(&platform, "the Error card", CardUpdates::Latest, |card| {
        card_header(card) == "❌ 出错"
    })
    .await;

    // The mark's PATCH fails (one plain platform error); the retry must still
    // go ahead.
    platform
        .fail_update_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "retry ack expected");

    wait_for_card_update(&platform, "the retried Done card", CardUpdates::Latest, |card| {
        card_header(card) == "✅ 完成" && card_text(card).contains("当前目录有 src/ 和 Cargo.toml。")
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|call| matches!(
            call,
            PlatformCall::UpdateMessage { card, .. } if card_header(card) == "↩️ 已重试"
        )),
        "the marker PATCH must have been attempted: {calls:?}"
    );
    let ids = prompt_ids.lock().await.clone();
    assert_eq!(
        ids.len(),
        2,
        "the retry must submit despite the failed mark: {ids:?}"
    );
}

/// Double-click safety (spec #391): the first click claims the retry; a second
/// click finds the claim taken (or the card no longer Error) and submits
/// nothing.
#[tokio::test]
async fn double_clicked_retry_submits_only_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ""))
        .await
        .unwrap();
    wait_for_card_update(&platform, "the Error card", CardUpdates::Latest, |card| {
        card_header(card) == "❌ 出错"
    })
    .await;

    let first = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    let second = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(first.is_some(), "the first click must claim the retry");
    assert!(second.is_none(), "the second click must be refused");

    wait_for_card_update(&platform, "the retried Done card", CardUpdates::Latest, |card| {
        card_header(card) == "✅ 完成"
    })
    .await;
    let ids = prompt_ids.lock().await.clone();
    assert_eq!(ids.len(), 2, "exactly one retry submit: {ids:?}");
}

/// A retry that loses the inflight guard must mark NOTHING (spec #391): the
/// mark lives inside `Turn::start`, after the guard, so the failed card stays
/// `Error` with a working retry button and the claim is released for a later
/// click. This is the window between the handler's guard check and the submit.
#[tokio::test]
async fn retry_losing_the_inflight_guard_marks_nothing_and_releases_the_claim() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ""))
        .await
        .unwrap();
    wait_for_card_update(&platform, "the Error card", CardUpdates::Latest, |card| {
        card_header(card) == "❌ 出错"
    })
    .await;

    // The click claims the retry (the handler's gate)...
    let retry = crate::bridge::turn::Turn::claim_recovery(
        &app.cards_handle(),
        "ses_test",
        crate::feishu::card::CardState::Error,
    )
    .await;
    assert!(retry.is_some(), "the Error card must be retryable");
    // ...then another prompt takes the session's guard before this retry's
    // `Turn::start` runs.
    app.inflight.lock().await.insert("ses_test".to_string());

    let mut ctx = retry_ctx("ses_test", "hi", "msg_cola_lost");
    ctx.is_retry = true;
    crate::bridge::turn::Turn::run(&app.turn_handles(), ctx)
        .await
        .unwrap();

    assert_eq!(
        crate::bridge::turn::Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(crate::feishu::card::CardState::Error),
        "a retry that never submitted must leave the card Error"
    );
    assert!(
        !platform
            .updated_cards()
            .await
            .iter()
            .any(|card| card_header(card) == "↩️ 已重试"),
        "no Retried mark may reach Feishu"
    );
    assert_eq!(
        prompt_ids.lock().await.len(),
        1,
        "no retry prompt may be submitted"
    );
    assert!(
        crate::bridge::turn::Turn::claim_recovery(
            &app.cards_handle(),
            "ses_test",
            crate::feishu::card::CardState::Error
        )
        .await
        .is_some(),
        "the lost guard must release the claim for a later click"
    );
}

/// The double-click guard's other face: a card that is no longer `Error` (a
/// live or Done turn) offers no retry — the click claims nothing and submits
/// nothing.
#[tokio::test]
async fn retry_on_a_live_card_submits_nothing() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mock = MockBackend::new(realistic_parts());
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend, platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ""))
        .await
        .unwrap();
    wait_for_card_update(&platform, "the Done card", CardUpdates::Latest, |card| {
        card_header(card) == "✅ 完成"
    })
    .await;

    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_none(), "a live card must not claim a retry");
    let ids = prompt_ids.lock().await.clone();
    assert_eq!(ids.len(), 1, "no retry prompt may be submitted: {ids:?}");
}

/// The re-attach fixture's turn anchor: the user message the failed turn
/// answers. Later scripted windows keep it, so the follow keeps reading the
/// same turn (ticket #393).
const REATTACH_ANCHOR: &str = "msg_cola_reattach";

/// The re-attach fixture (spec #391, ticket #393): a `ses_test` app whose
/// first prompt failed. The scripted transcript carries the failed attempt —
/// the anchor user message plus a failed assistant reply — so the Error card
/// keeps the accumulator's anchor and prompt that a busy click re-attaches to.
/// The session is left idle; the caller scripts the click-time status.
async fn error_card_app(
    cfg: crate::config::Config,
    is_group: bool,
    requester: Option<String>,
) -> (Arc<App>, Arc<MockBackend>, Arc<RecordingPlatform>) {
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    // A tiny per-read bound: a hung follow read must fail fast.
    app.turn_follow_read_timeout_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);
    *backend
        .transcript_scripts
        .lock()
        .await
        .entry("ses_test".into())
        .or_default() = vec![failed_attempt_window(REATTACH_ANCHOR)];

    let mut context = retry_ctx("ses_test", "hi", REATTACH_ANCHOR);
    context.is_group = is_group;
    context.requester_open_id = requester;
    crate::bridge::turn::Turn::run(&app.turn_handles(), context)
        .await
        .unwrap();
    wait_for_card_update(&platform, "the Error card", CardUpdates::Latest, |card| {
        card_header(card) == "❌ 出错"
    })
    .await;
    (app, backend, platform)
}

/// The busy fallback (spec #391, ticket #393): a failed submission that
/// persisted nothing leaves the accumulator without an anchor, so there is no
/// run to follow. The click submits nothing and marks nothing, the card stays
/// Error, and the claim goes back so the retry is still clickable.
#[tokio::test]
async fn busy_retry_without_an_anchor_leaves_the_card_retryable() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ""))
        .await
        .unwrap();
    wait_for_card_update(&platform, "the Error card", CardUpdates::Latest, |card| {
        card_header(card) == "❌ 出错"
    })
    .await;

    *backend
        .session_statuses
        .lock()
        .await
        .entry("ses_test".into())
        .or_default() = Some(crate::opencode::types::SessionStatus::Busy);
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "the busy click still acks");

    // Let the spawned decision run: it must not submit, and it must give the
    // claim back (the no-anchor fallback leaves the button usable).
    let retryable = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if crate::bridge::turn::Turn::claim_recovery(
                &app.cards_handle(),
                "ses_test",
                crate::feishu::card::CardState::Error,
            )
            .await
            .is_some()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(retryable.is_ok(), "the fallback must release the claim");
    assert_eq!(
        crate::bridge::turn::Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(crate::feishu::card::CardState::Error),
        "without an anchor the card stays a retryable Error"
    );
    let ids = prompt_ids.lock().await.clone();
    assert_eq!(ids.len(), 1, "a busy run must not be re-prompted: {ids:?}");
}

/// The busy cell of the retry matrix (spec #391, ticket #393): a click on a
/// still-alive run submits NO prompt — the card is re-attached instead. It
/// leaves Error immediately (live header, failed content preserved), and when
/// the scripted run ends clean the follow finalizes Done from the transcript.
#[tokio::test]
async fn busy_retry_reattaches_and_finalizes_done_from_the_transcript() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, backend, platform) = error_card_app(cfg, false, None).await;

    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Busy))
        .await;
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);

    // The re-attach's own flush leaves Error before the follow's first tick:
    // live header, the failed attempt's content intact.
    wait_for_card_update(
        &platform,
        "the re-attached live card",
        CardUpdates::Latest,
        |card| card_header(card).contains("回复中") && card_text(card).contains("OLD_TEXT"),
    )
    .await;
    let ids = backend.prompt_message_ids.lock().await.clone();
    assert_eq!(ids.len(), 1, "a live run must not be re-prompted: {ids:?}");

    // The scripted run ends clean: the follow finalizes Done from it.
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![window_with(
        REATTACH_ANCHOR,
        vec![typed_message(
            "msg_clean_end",
            MessageRole::Assistant,
            Some(3_000),
            vec![text_part("重接后的干净收尾 CLEAN_END")],
        )],
    )];
    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Idle))
        .await;
    wait_for_card_update(
        &platform,
        "the re-attached Done card",
        CardUpdates::Latest,
        |card| card_header(card) == "✅ 完成" && card_text(card).contains("CLEAN_END"),
    )
    .await;
    assert_eq!(
        backend.prompt_calls.lock().await.len(),
        1,
        "the re-attach must never submit a prompt"
    );
}

/// A re-attached run that ends in failure finalizes Error with a WORKING
/// retry (spec #391, ticket #393): the claim was released with the re-attach,
/// so the new Error's click can act again.
#[tokio::test]
async fn busy_retry_reattach_finalizes_error_with_a_working_retry() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, backend, platform) = error_card_app(cfg, false, None).await;

    // `Retry` is the matrix's other live status: it must re-attach too.
    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Retry))
        .await;
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);
    wait_for_card_update(
        &platform,
        "the re-attached live card",
        CardUpdates::Latest,
        |card| card_header(card).contains("回复中"),
    )
    .await;

    // The run ends for real: the newest assistant message records a failure.
    let mut failed = typed_message(
        "msg_reattach_failure",
        MessageRole::Assistant,
        Some(3_000),
        vec![text_part("重接后的真实失败 REAL_FAILURE")],
    );
    failed.error = Some("真实运行失败".into());
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![window_with(REATTACH_ANCHOR, vec![failed])];
    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Idle))
        .await;
    wait_for_card_update(
        &platform,
        "the re-attached Error card",
        CardUpdates::Latest,
        |card| card_header(card) == "❌ 出错" && card_text(card).contains("REAL_FAILURE"),
    )
    .await;

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_buttons(&final_card).iter().any(|button| {
            button["value"].get("action").and_then(|action| action.as_str()) == Some("retry")
        }),
        "the re-attached failure must offer a retry: {final_card}"
    );
    assert!(
        crate::bridge::turn::Turn::claim_recovery(
            &app.cards_handle(),
            "ses_test",
            crate::feishu::card::CardState::Error
        )
        .await
        .is_some(),
        "the re-attach must release the claim so the new Error is retryable"
    );
    assert_eq!(
        backend.prompt_calls.lock().await.len(),
        1,
        "the re-attach must never submit a prompt"
    );
}

/// The re-attached card is an ordinary follow: `/stop` still ends it promptly
/// through the sticky stopped-session marker — re-attach adds no loop, so the
/// follow's stop handling is untouched (spec #391, ticket #393).
#[tokio::test]
async fn stop_after_a_reattach_ends_the_card_promptly() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, backend, platform) = error_card_app(cfg, false, None).await;
    // A ceiling the test would never wait out: only the stop can end it.
    app.turn_follow_grace_ms
        .store(60_000, std::sync::atomic::Ordering::Relaxed);

    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Busy))
        .await;
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);
    wait_for_card_update(
        &platform,
        "the re-attached live card",
        CardUpdates::Latest,
        |card| card_header(card).contains("回复中"),
    )
    .await;

    let started = std::time::Instant::now();
    app.handle_message(incoming(
        "msg_stop".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/stop".into(),
        None,
    ))
    .await;
    assert_eq!(
        backend.interrupt_calls.lock().await.as_slice(),
        &["ses_test".to_string()],
        "/stop must interrupt the re-attached session"
    );

    // The final PATCH is the observation point: `finalize_stopped` sets the
    // state before it refreshes the work context and flushes, so waiting on
    // the internal state alone would read the stale live card. The sibling
    // drain test waits on the card the same way.
    wait_for_card_update(
        &platform,
        "the re-attached card's stop header",
        CardUpdates::Latest,
        |card| card_header(card).contains("已停止"),
    )
    .await;
    assert_eq!(
        crate::bridge::turn::Turn::card_state(&app.cards_handle(), "ses_test").await,
        Some(crate::feishu::card::CardState::Stopped),
        "a deliberate stop finalizes Stopped, not Error or Done"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "the follow must end on the stop, not the 60 s grace"
    );
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_header(&final_card).contains("已停止"),
        "the re-attached card's stop must render the stop header: {final_card}"
    );
    assert!(
        card_buttons(&final_card).is_empty(),
        "a re-attached card finalized by a stop must offer no retry: {final_card}"
    );
}

/// ADR-0059's routing key on the re-attach path: the re-attached follow holds
/// the Session's guard for its window too (spec #391, ticket #393), so a
/// message arriving during it is a Supplement — the chain splits at the
/// message and the follow keeps rendering on the continuation, never a
/// competing Turn replacing the accumulator. When the run then truly ends, the
/// continuation finalizes Done.
#[tokio::test]
async fn a_message_during_a_reattach_follow_splits_the_live_chain() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, backend, platform) = error_card_app(cfg, false, None).await;

    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Busy))
        .await;
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);
    wait_for_card_update(
        &platform,
        "the re-attached live card",
        CardUpdates::Latest,
        |card| card_header(card).contains("回复中"),
    )
    .await;
    let followed = crate::bridge::turn::Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await;
    assert!(followed.is_some(), "the re-attach watches the turn anchor");
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the re-attached follow must hold the guard"
    );

    app.handle_message(incoming(
        "msg_next".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "接着问".into(),
        None,
    ))
    .await;

    // The message merged as a Supplement: submitted to the Backend, no busy
    // notice, and the follow's accumulator survives (no competing Turn).
    assert!(
        backend
            .prompt_calls
            .lock()
            .await
            .iter()
            .any(|text| text == "接着问"),
        "the Supplement must be submitted to the Backend: {:?}",
        backend.prompt_calls.lock().await
    );
    assert!(
        !platform
            .texts()
            .await
            .iter()
            .any(|text| text.contains("还在处理中")),
        "a follow-window message is never answered busy: {:?}",
        platform.calls.lock().await
    );
    assert_eq!(
        crate::bridge::turn::Turn::armed_turn_anchor(&app.cards_handle(), "ses_test").await,
        followed,
        "no competing Turn: the follow's accumulator survives"
    );

    // Exactly one card replies to the message — the split continuation, with
    // the Supplement receipt (the helper asserts both).
    supplement_continuation(&platform, "msg_next").await;

    // The run ends clean: the follow finalizes the continuation Done from the
    // transcript, carrying the new content.
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![window_with(
        REATTACH_ANCHOR,
        vec![typed_message(
            "msg_clean_end",
            MessageRole::Assistant,
            Some(3_000),
            vec![text_part("重接后的干净收尾 CLEAN_END")],
        )],
    )];
    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Idle))
        .await;
    wait_for_card_update(
        &platform,
        "the continuation's Done card",
        CardUpdates::Latest,
        |card| card_header(card) == "✅ 完成" && card_text(card).contains("CLEAN_END"),
    )
    .await;

    // The re-attach submitted nothing itself; the only extra prompt is the
    // Supplement.
    assert_eq!(
        backend.prompt_calls.lock().await.len(),
        2,
        "the re-attach must never submit a prompt: {:?}",
        backend.prompt_calls.lock().await
    );
    assert!(
        !app.inflight.lock().await.contains("ses_test"),
        "the follow releases the guard at its ending"
    );
}

/// The long-task notice's start is the re-attach, not the original turn (spec
/// #391, ticket #393): the original start is no longer known. The failed turn
/// began well past the threshold ago; the re-attached run ends quickly, so the
/// notice must stay silent — a notice would mean the stale start leaked in.
#[tokio::test]
async fn a_quick_reattached_end_does_not_measure_the_original_turn() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.long_task_notice = true;
    let (app, backend, platform) = error_card_app(cfg, false, Some(TEST_HOST.to_string())).await;
    app.long_task_notice_ms
        .store(300, std::sync::atomic::Ordering::Relaxed);

    // The original turn is older than the threshold now. Its own fast finish
    // sent nothing (the threshold was the default then), so the notice list
    // starts empty.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        platform.completion_notices().await.is_empty(),
        "the failed turn was short: {:?}",
        platform.calls.lock().await
    );

    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Busy))
        .await;
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);
    wait_for_card_update(
        &platform,
        "the re-attached live card",
        CardUpdates::Latest,
        |card| card_header(card).contains("回复中"),
    )
    .await;

    // A clean end within the threshold of the re-attach.
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![window_with(
        REATTACH_ANCHOR,
        vec![typed_message(
            "msg_quick_end",
            MessageRole::Assistant,
            Some(3_000),
            vec![text_part("快速收尾 QUICK_END")],
        )],
    )];
    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Idle))
        .await;
    wait_for_card_update(
        &platform,
        "the quickly re-attached Done card",
        CardUpdates::Latest,
        |card| card_header(card) == "✅ 完成" && card_text(card).contains("QUICK_END"),
    )
    .await;
    assert!(
        platform.completion_notices().await.is_empty(),
        "the notice must measure from the re-attach, not the original turn: {:?}",
        platform.calls.lock().await
    );
}

/// The other side of the same rule: a re-attached stretch that itself
/// outlives the threshold still notifies on the real end (the follow carries
/// the re-attach's own start).
#[tokio::test]
async fn a_long_reattached_run_still_sends_the_notice() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.long_task_notice = true;
    let (app, backend, platform) = error_card_app(cfg, false, Some(TEST_HOST.to_string())).await;
    app.long_task_notice_ms
        .store(50, std::sync::atomic::Ordering::Relaxed);

    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Busy))
        .await;
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);
    wait_for_card_update(
        &platform,
        "the re-attached live card",
        CardUpdates::Latest,
        |card| card_header(card).contains("回复中"),
    )
    .await;

    // The re-attached stretch outlives the threshold on its own.
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![window_with(
        REATTACH_ANCHOR,
        vec![typed_message(
            "msg_long_end",
            MessageRole::Assistant,
            Some(4_000),
            vec![text_part("长任务收尾 LONG_END")],
        )],
    )];
    backend
        .set_session_status("ses_test", Some(crate::opencode::types::SessionStatus::Idle))
        .await;
    wait_for_card_update(
        &platform,
        "the long re-attached Done card",
        CardUpdates::Latest,
        |card| card_header(card) == "✅ 完成" && card_text(card).contains("LONG_END"),
    )
    .await;
    let notices = platform.completion_notices().await;
    assert!(
        notices.iter().any(|(_, _, _, text)| text.contains("已完成")),
        "a long re-attached run must notify: {notices:?}"
    );
}

/// One failed attempt's assistant message: distinctive text plus a completed
/// Tool Panel, so a replayed window is unmistakable on the rebuilt card.
fn failed_attempt_message() -> TranscriptMessage {
    typed_message(
        "msg_old",
        MessageRole::Assistant,
        Some(2_000),
        vec![
            text_part("旧尝试的结论：OLD_TEXT"),
            tool_part(
                "bash",
                "call_old",
                ToolStatus::Completed,
                serde_json::json!({ "command": "ls-old" }),
                "OLD_TOOL_OUTPUT",
            ),
        ],
    )
}

/// One attempt's scripted turn window: the admitted user message (`anchor`,
/// with server time `anchor_created`) plus the assistant message that attempt
/// left behind. A retry window uses a later `anchor_created`, so the earlier
/// attempts fall outside its membership.
fn attempt_window(anchor: &str, anchor_created: i64, assistant: TranscriptMessage) -> SessionTranscript {
    SessionTranscript::new(vec![
        typed_message(
            anchor,
            MessageRole::User,
            Some(anchor_created),
            vec![text_part("hi")],
        ),
        assistant,
    ])
}

/// The failed attempt's scripted turn window, anchored on the user message the
/// retry reuses.
fn failed_attempt_window(anchor: &str) -> SessionTranscript {
    attempt_window(anchor, 1_000, failed_attempt_message())
}

/// [`failed_attempt_window`] plus `extra`'s messages, newest last — an
/// attempt's window as the retry reads it.
fn window_with(anchor: &str, extra: Vec<TranscriptMessage>) -> SessionTranscript {
    let mut window = failed_attempt_window(anchor);
    window.messages.extend(extra);
    window
}

/// An assistant message that failed without a terminal finish: the retry's
/// decision must read its turn as unfinished (V1 continues it; V2 takes a
/// fresh id).
fn failed_assistant_message(id: &str, created: i64, text: &str) -> TranscriptMessage {
    let mut message = typed_message(id, MessageRole::Assistant, Some(created), vec![text_part(text)]);
    message.error = Some("provider 503".into());
    message
}

/// The first retry's own message; its recorded failure makes retry 1 end
/// Error again, so the chain can be retried once more.
fn first_retry_message() -> TranscriptMessage {
    failed_assistant_message("msg_new_1", 3_000, "第一次重试：RETRY1_TEXT")
}

/// #387, V1's reuse contract (spec #391 correction): the same-id re-post
/// continues an ADMITTED, unfinished turn on V1, so the failed attempt's
/// messages are still in the turn window when the retry rebuilds the card. The
/// retry must carry the failed attempt's rendered baseline: the rebuilt card
/// streams only the new attempt instead of replaying every old part (122 tool
/// panels on the live incident).
#[tokio::test]
async fn error_card_retry_does_not_replay_the_failed_attempt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    const ANCHOR: &str = "msg_cola_retry";
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    // V1's server contract: a same-id re-post continues the admitted turn.
    mock.with_reuse_continues_an_admitted_turn(true);
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    *backend
        .transcript_scripts
        .lock()
        .await
        .entry("ses_test".into())
        .or_default() = vec![failed_attempt_window(ANCHOR)];

    // Attempt 1 fails (provider hiccup); the Error card renders the failed
    // attempt's content — the baseline the retry must carry.
    crate::bridge::turn::Turn::run(
        &app.turn_handles(),
        crate::bridge::turn::PromptContext {
            session_id: "ses_test".into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            text: "hi".into(),
            message_id: "msg_1".into(),
            subtitle: "p2p".into(),
            is_retry: false,
            requester_open_id: None,
            is_group: false,
            cola_message_id: Some(ANCHOR.into()),
            images: Vec::new(),
            advisory_live: false,
        },
    )
    .await
    .unwrap();
    wait_for_card_update(
        &platform,
        "the failed attempt on the Error card",
        CardUpdates::Latest,
        |card| card_text(card).contains("OLD_TEXT"),
    )
    .await;

    // The retry's window: the same failed attempt plus the new run's message.
    let retried = window_with(
        ANCHOR,
        vec![typed_message(
            "msg_new",
            MessageRole::Assistant,
            Some(3_000),
            vec![
                text_part("新尝试的结论：NEW_TEXT"),
                tool_part(
                    "bash",
                    "call_new",
                    ToolStatus::Completed,
                    serde_json::json!({ "command": "ls-new" }),
                    "NEW_TOOL_OUTPUT",
                ),
            ],
        )],
    );
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![retried];

    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "retry ack card expected");
    wait_for_card_update(&platform, "the retry's Done card", CardUpdates::Latest, |card| {
        let text = card_text(card);
        text.contains("NEW_TEXT") && text.contains("✅")
    })
    .await;

    let ids = prompt_ids.lock().await.clone();
    assert_eq!(
        ids,
        vec![Some(ANCHOR.to_string()), Some(ANCHOR.to_string())],
        "the retry must re-submit the failed attempt's msg_cola_ id (ADR-0026)"
    );

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(text.contains("NEW_TEXT"), "the new attempt must render: {text}");
    assert!(
        !text.contains("OLD_TEXT"),
        "the failed attempt's text must not replay: {text}"
    );
    assert!(
        !text.contains("OLD_TOOL_OUTPUT"),
        "the failed attempt's Tool Panel must not replay: {text}"
    );
}

/// #387, V1's chain (spec #391 correction): each retry's accumulator unions
/// the baseline it carried with every message its own attempt observed, so a
/// retry after a retry suppresses the original attempt and the first retry
/// alike — only the newest attempt renders into the rebuilt card.
#[tokio::test]
async fn retrying_again_suppresses_every_earlier_attempt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    const ANCHOR: &str = "msg_cola_chain";
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
    // V1's server contract: a same-id re-post continues the admitted turn.
    mock.with_reuse_continues_an_admitted_turn(true);
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    *backend
        .transcript_scripts
        .lock()
        .await
        .entry("ses_test".into())
        .or_default() = vec![failed_attempt_window(ANCHOR)];

    crate::bridge::turn::Turn::run(
        &app.turn_handles(),
        crate::bridge::turn::PromptContext {
            session_id: "ses_test".into(),
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            text: "hi".into(),
            message_id: "msg_1".into(),
            subtitle: "p2p".into(),
            is_retry: false,
            requester_open_id: None,
            is_group: false,
            cola_message_id: Some(ANCHOR.into()),
            images: Vec::new(),
            advisory_live: false,
        },
    )
    .await
    .unwrap();
    wait_for_card_update(
        &platform,
        "the failed attempt on the Error card",
        CardUpdates::Latest,
        |card| card_text(card).contains("OLD_TEXT"),
    )
    .await;

    // Retry 1's window: the failed attempt plus its own run, whose newest
    // message records a fresh provider failure — retry 1 ends Error again.
    let first_retry = window_with(ANCHOR, vec![first_retry_message()]);
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![first_retry];

    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "first retry ack card expected");
    wait_for_card_update(
        &platform,
        "the first retry's Error card",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            text.contains("RETRY1_TEXT") && text.contains("❌")
        },
    )
    .await;

    // Retry 2's window: everything so far plus the second retry's clean run.
    let second_retry = window_with(
        ANCHOR,
        vec![
            first_retry_message(),
            typed_message(
                "msg_new_2",
                MessageRole::Assistant,
                Some(4_000),
                vec![text_part("第二次重试：RETRY2_TEXT")],
            ),
        ],
    );
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![second_retry];

    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "second retry ack card expected");
    wait_for_card_update(
        &platform,
        "the second retry's Done card",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            text.contains("RETRY2_TEXT") && text.contains("✅")
        },
    )
    .await;

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(
        text.contains("RETRY2_TEXT"),
        "the newest attempt must render: {text}"
    );
    assert!(
        !text.contains("RETRY1_TEXT"),
        "the first retry's content must not replay: {text}"
    );
    assert!(
        !text.contains("OLD_TEXT"),
        "the original failed attempt must not replay: {text}"
    );
    assert!(
        !text.contains("OLD_TOOL_OUTPUT"),
        "the original failed attempt's Tool Panel must not replay: {text}"
    );

    // V1's id contract: the initial attempt and BOTH retries are the same
    // logical user message — every submit carries the one `msg_cola_` id.
    let ids = prompt_ids.lock().await.clone();
    assert_eq!(
        ids,
        vec![
            Some(ANCHOR.to_string()),
            Some(ANCHOR.to_string()),
            Some(ANCHOR.to_string())
        ],
        "every V1 retry must re-submit the failed attempt's msg_cola_ id"
    );
}

/// The assistant message a killed run leaves behind: no completion stamp, a
/// stuck running tool, the recorded failure — the turn never reads complete.
fn orphaned_attempt_message() -> TranscriptMessage {
    let mut orphan = typed_message(
        "msg_orphan",
        MessageRole::Assistant,
        Some(2_000),
        vec![
            text_part("旧尝试的结论：OLD_TEXT"),
            tool_part(
                "bash",
                "call_orphan",
                ToolStatus::Running,
                serde_json::json!({ "command": "sleep 600" }),
                "OLD_TOOL_OUTPUT",
            ),
        ],
    );
    orphan.time.as_mut().unwrap().completed = None;
    orphan.error = Some("运行被中断".into());
    orphan
}

/// The V2 kill-shaped turn window: the admitted user message plus the orphaned
/// assistant message, so the turn never reads complete.
fn orphaned_attempt_window(anchor: &str) -> SessionTranscript {
    attempt_window(anchor, 1_000, orphaned_attempt_message())
}

/// Spec #391's generation split, V2 side: the `msg_cola_` id is an admission
/// key, so a same-id re-post of an ADMITTED, unfinished (orphaned) turn would
/// be a silent no-op. The retry takes a FRESH id — pinned here over the V2
/// orphan shape (no completion stamp, stuck tool, recorded failure) — and the
/// orphan does not replay onto the new card. The fresh anchor is only 5 minutes
/// after the orphan's newest activity, inside the transcript's in-flight
/// window, so the orphan would read as belonging to the new turn: the render
/// baseline is exactly what keeps it off the new card.
#[tokio::test]
async fn v2_unfinished_retry_submits_a_new_id_and_never_replays_the_orphan() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    const ANCHOR: &str = "msg_cola_orphan";
    // 2_000 (the orphan's activity) + 300_000 = inside the 10-minute in-flight
    // window, so `turn_for_user` includes the orphan in the fresh turn.
    const FRESH_CREATED: i64 = 302_000;
    let mut mock = MockBackend::new(realistic_parts());
    // V2's server contract: admission makes a same-id re-post a no-op.
    mock.with_reuse_continues_an_admitted_turn(false);
    // The retry's prompt is held after its id is recorded, so the test can
    // swap the transcript script between the decision read and the retry's
    // own render.
    let gate = mock.hold_prompts();
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    *backend
        .transcript_scripts
        .lock()
        .await
        .entry("ses_test".into())
        .or_default() = vec![orphaned_attempt_window(ANCHOR)];

    // Attempt 1 (the orphaned turn): its recorded failure ends the card Error.
    gate.add_permits(1);
    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ANCHOR))
        .await
        .unwrap();
    wait_for_card_update(
        &platform,
        "the orphaned failure's Error card",
        CardUpdates::Latest,
        |card| {
            let text = card_text(card);
            text.contains("❌") && text.contains("OLD_TEXT")
        },
    )
    .await;

    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);

    // V2 must not re-post the admitted id: the retry's prompt carries a fresh
    // one. Swap in the full window before releasing it — the orphan stays in
    // it, still inside the fresh anchor's in-flight window, so only the render
    // baseline can keep it off the new card.
    let ids = wait_for_prompt_ids(&prompt_ids, 2).await;
    let new_id = ids[1].clone().expect("the retry prompt carries an id");
    assert_ne!(
        ANCHOR,
        new_id.as_str(),
        "V2 must not re-post an id the server already admitted"
    );
    let mut window = orphaned_attempt_window(ANCHOR);
    window.messages.push(typed_message(
        new_id.as_str(),
        MessageRole::User,
        Some(FRESH_CREATED),
        vec![text_part("hi")],
    ));
    window.messages.push(typed_message(
        "msg_fresh",
        MessageRole::Assistant,
        Some(FRESH_CREATED + 1_000),
        vec![
            text_part("新尝试的结论：NEW_TEXT"),
            tool_part(
                "bash",
                "call_fresh",
                ToolStatus::Completed,
                serde_json::json!({ "command": "ls-fresh" }),
                "NEW_TOOL_OUTPUT",
            ),
        ],
    ));
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![window];
    gate.add_permits(1);

    wait_for_card_update(&platform, "the retry's Done card", CardUpdates::Latest, |card| {
        card_header(card) == "✅ 完成" && card_text(card).contains("NEW_TEXT")
    })
    .await;

    let final_text = card_text(&platform.updated_cards().await.last().cloned().unwrap());
    assert!(
        final_text.contains("NEW_TEXT"),
        "the fresh attempt must render: {final_text}"
    );
    assert!(
        !final_text.contains("OLD_TEXT"),
        "the orphaned attempt's text must not replay: {final_text}"
    );
    assert!(
        !final_text.contains("OLD_TOOL_OUTPUT"),
        "the orphaned attempt's stuck Tool Panel must not replay: {final_text}"
    );
    let marked = marked_card(&platform).await;
    assert!(
        card_text(&marked).contains("OLD_TEXT"),
        "the failed card stays readable, marked Retried: {marked}"
    );
}

/// Spec #391's generation split, V2's chain: every retry of an admitted,
/// unfinished turn takes a fresh id (the admission key no-ops a re-post), and
/// a retry after a retry still renders only the newest attempt — the fresh
/// anchors exclude the earlier attempts.
#[tokio::test]
async fn v2_retries_chain_under_fresh_ids() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    const ANCHOR: &str = "msg_cola_v2_chain";
    const RETRY1_CREATED: i64 = 4_000_000;
    const RETRY2_CREATED: i64 = 6_000_000;
    let mut mock = MockBackend::new(realistic_parts());
    // V2's server contract: admission makes a same-id re-post a no-op.
    mock.with_reuse_continues_an_admitted_turn(false);
    let gate = mock.hold_prompts();
    let prompt_ids = mock.prompt_message_ids.clone();
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;

    *backend
        .transcript_scripts
        .lock()
        .await
        .entry("ses_test".into())
        .or_default() = vec![orphaned_attempt_window(ANCHOR)];

    gate.add_permits(1);
    crate::bridge::turn::Turn::run(&app.turn_handles(), retry_ctx("ses_test", "hi", ANCHOR))
        .await
        .unwrap();
    wait_for_card_update(
        &platform,
        "the orphaned failure's Error card",
        CardUpdates::Latest,
        |card| card_header(card) == "❌ 出错" && card_text(card).contains("OLD_TEXT"),
    )
    .await;

    // Retry 1: a fresh id, and its window records a fresh failure — the retry
    // ends Error again, so it can be retried once more.
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);
    let ids = wait_for_prompt_ids(&prompt_ids, 2).await;
    let retry1 = ids[1].clone().expect("retry 1's id");
    assert_ne!(ANCHOR, retry1.as_str(), "retry 1 must take a fresh id");
    let retry1_failed = failed_assistant_message("msg_v2_retry1", RETRY1_CREATED, "第一次重试：RETRY1_TEXT");
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![attempt_window(
        retry1.as_str(),
        RETRY1_CREATED - 1_000,
        retry1_failed,
    )];
    gate.add_permits(1);
    wait_for_card_update(&platform, "retry 1's Error card", CardUpdates::Latest, |card| {
        card_header(card) == "❌ 出错" && card_text(card).contains("RETRY1_TEXT")
    })
    .await;

    // Retry 2: another fresh id; the clean run finishes Done with only its own
    // content.
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert_retry_ack(retry);
    let ids = wait_for_prompt_ids(&prompt_ids, 3).await;
    let retry2 = ids[2].clone().expect("retry 2's id");
    assert_ne!(retry1, retry2, "retry 2 must take a fresh id too");
    assert_ne!(ANCHOR, retry2.as_str(), "nor re-post the original id");
    *backend
        .transcript_scripts
        .lock()
        .await
        .get_mut("ses_test")
        .unwrap() = vec![attempt_window(
        retry2.as_str(),
        RETRY2_CREATED - 1_000,
        typed_message(
            "msg_v2_retry2",
            MessageRole::Assistant,
            Some(RETRY2_CREATED),
            vec![text_part("第二次重试：RETRY2_TEXT")],
        ),
    )];
    gate.add_permits(1);
    wait_for_card_update(&platform, "retry 2's Done card", CardUpdates::Latest, |card| {
        card_header(card) == "✅ 完成" && card_text(card).contains("RETRY2_TEXT")
    })
    .await;

    let final_text = card_text(&platform.updated_cards().await.last().cloned().unwrap());
    assert!(
        final_text.contains("RETRY2_TEXT"),
        "the newest attempt must render: {final_text}"
    );
    assert!(
        !final_text.contains("RETRY1_TEXT"),
        "the first retry's content must not replay: {final_text}"
    );
    assert!(
        !final_text.contains("OLD_TEXT") && !final_text.contains("OLD_TOOL_OUTPUT"),
        "the original orphan must not replay: {final_text}"
    );
}

#[tokio::test]
async fn group_completion_sends_notice_to_requester() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        "hi".into(),
        Some(TEST_HOST.into()),
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let notices: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::CompletionNotice {
                reply_to,
                open_id,
                text,
                ..
            } => Some((reply_to.clone(), open_id.clone(), text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        vec![(
            "msg_1".to_string(),
            TEST_HOST.to_string(),
            "✅ 已完成。".to_string()
        )]
    );
}

#[tokio::test]
async fn group_completion_at_mentions_requester_when_name_resolvable() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut rp = RecordingPlatform::new();
    rp.user_names.insert(TEST_HOST.to_string(), "李明".to_string());
    let platform = Arc::new(rp);
    let app = Arc::new(
        App::new(
            cfg,
            Arc::new(MockBackend::new(realistic_parts())),
            platform.clone(),
        )
        .unwrap(),
    );

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_group_1".into(),
        "group".into(),
        None,
        "hi".into(),
        Some(TEST_HOST.into()),
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    let notices: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::CompletionNotice {
                reply_to,
                open_id,
                name,
                text,
            } => Some((reply_to.clone(), open_id.clone(), name.clone(), text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        vec![(
            "msg_1".to_string(),
            TEST_HOST.to_string(),
            Some("李明".to_string()),
            "✅ 已完成。".to_string()
        )]
    );
}

#[tokio::test]
async fn p2p_prompt_sends_no_completion_notice() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        Some(TEST_HOST.into()),
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, PlatformCall::CompletionNotice { .. })),
        "p2p must not send a completion notice: {:?}",
        calls
    );
}

#[tokio::test]
async fn subtitle_falls_back_to_id_tail_without_server_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_01ba0ed03ffeRvYNWua6mg8d9c".into(),
            directory: "/tmp/x".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    // No server title → the id-tail alone identifies the session (no cola
    // side name to fall back on; the current prompt is never echoed).
    assert_eq!(
        crate::bridge::turn::Turn::session_subtitle(
            &app.sessions_handle(),
            &app.opencode,
            &key,
            "另一个问题"
        )
        .await,
        "01ba0ed"
    );
    assert_eq!(
        crate::bridge::turn::Turn::session_subtitle(&app.sessions_handle(), &app.opencode, &key, "你好")
            .await,
        "01ba0ed"
    );
}

/// The session-title fetch must degrade, not hang the turn, when the
/// server swallows the request — a freshly spawned Owned Server's startup
/// window is exactly this: the first request is read but never dispatched
/// (the Lazy Start silent-hang incident).
#[tokio::test]
async fn subtitle_degrades_when_session_info_hangs() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mock = MockBackend::new(realistic_parts());
    mock.hang_session_info_reads(usize::MAX);
    let (app, _) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_01ba0ed03ffeRvYNWua6mg8d9c".into(),
            directory: "/tmp/x".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    // The fetch hangs forever; the subtitle must still return (id-tail
    // only) within the bound instead of hanging the prompt flow.
    let subtitle = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        crate::bridge::turn::Turn::session_subtitle(&app.sessions_handle(), &app.opencode, &key, "问题"),
    )
    .await
    .expect("session_subtitle must not hang when session_info never returns");
    assert_eq!(subtitle, "01ba0ed");
}

/// The Lazy Start readiness probe must keep retrying through wedged
/// attempts (the spawned server's startup window) and only return once
/// the server actually serves a request.
#[tokio::test]
async fn readiness_wait_recovers_after_wedged_attempts() {
    let mock = MockBackend::new(realistic_parts());
    mock.hang_session_lists(2);
    let backend: Arc<dyn crate::backend::Backend> = Arc::new(mock);
    let result =
        crate::bridge::pollers::wait_for_server_ready(&backend, std::time::Duration::from_secs(10)).await;
    assert!(
        result.is_ok(),
        "readiness wait must recover after the startup window: {result:?}"
    );
}

/// A server that never serves a request must fail the readiness wait, so
/// the lazy start reports the failure instead of hanging the turn
/// silently.
#[tokio::test]
async fn readiness_wait_fails_when_server_never_serves() {
    let mock = MockBackend::new(realistic_parts());
    mock.hang_session_lists(usize::MAX);
    let backend: Arc<dyn crate::backend::Backend> = Arc::new(mock);
    let result =
        crate::bridge::pollers::wait_for_server_ready(&backend, std::time::Duration::from_secs(2)).await;
    assert!(
        result.is_err(),
        "readiness wait must fail when the server never serves"
    );
}

/// A server default title (`New session - ...`) is treated as absent — the
/// id-tail is shown until the server generates a real title.
#[tokio::test]
async fn subtitle_ignores_server_default_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.with_session_title("ses_00ea4e77cffez1fo4wrNuJyHF0", "New session - 2026-08-28");
    let (app, _) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_00ea4e77cffez1fo4wrNuJyHF0".into(),
            directory: "/tmp/y".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    assert_eq!(
        crate::bridge::turn::Turn::session_subtitle(
            &app.sessions_handle(),
            &app.opencode,
            &key,
            "另一个问题"
        )
        .await,
        "00ea4e7"
    );
}

/// The card subtitle prefers the OpenCode server's own session title (what
/// OpenChamber shows), not cola's `/new`-generated names.
#[tokio::test]
async fn subtitle_prefers_server_title() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.with_session_title("ses_test", "OpenChamber 显示的标题");
    let (app, _) = build_app(cfg, mock).await;
    let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());

    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: key.clone(),
            session_id: "ses_test".into(),
            directory: "/tmp/x".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;
    assert_eq!(
        crate::bridge::turn::Turn::session_subtitle(&app.sessions_handle(), &app.opencode, &key, "问题")
            .await,
        "OpenChamber 显示的标题 · test"
    );
}

#[tokio::test]
async fn long_answer_splits_across_cards_no_plain_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_prompt("hi", long_answer_parts());
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    // Every message cola sent must be a CARD (ReplyCard / UpdateMessage) —
    // no separate plain-text message for long answers.
    assert!(
        calls.iter().all(|c| {
            matches!(
                c,
                PlatformCall::ReplyCard { .. } | PlatformCall::UpdateMessage { .. }
            )
        }),
        "long answer must stay on cards, got: {:?}",
        calls
    );
    // The FULL text must be present across the cards (not truncated).
    let all_cards: String = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::ReplyCard { card, .. } => Some(card.to_string()),
            PlatformCall::UpdateMessage { card, .. } => Some(card.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        all_cards.contains("很长的回答。"),
        "full answer must appear on a card: {}",
        all_cards
    );
    let expected = "很长的回答。".repeat(1200);
    assert!(
        all_cards.chars().filter(|c| *c != '"').count() >= expected.chars().count(),
        "all text must be delivered (preview-only would lose the tail)"
    );
}

#[tokio::test]
async fn short_answer_stays_in_card_no_extra_message() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    app.handle_message(incoming(
        "msg_1".into(),
        "oc_p2p_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().all(|c| matches!(
            c,
            PlatformCall::ReplyCard { .. } | PlatformCall::UpdateMessage { .. }
        )),
        "answer must stay on cards: {:?}",
        calls
    );
}

/// ADR-0044: the shared render poll refreshes the footer's context segment as
/// soon as a step's usage lands — the LIVE card carries it, not only the final
/// one — and the `GET /provider` window lookup is memoized per
/// (provider, model) for the turn, so later polls cost no extra request.
#[tokio::test]
async fn render_poll_shows_live_context_and_memoizes_the_window() {
    use crate::backend::{
        MessageId, MessageRole, MessageTime, ModelIdentity, SessionTranscript, TokenUsage, TranscriptMessage,
    };
    use crate::bridge::turn::Turn;

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mock = MockBackend::new(realistic_parts());
    let window_calls = mock.context_window_calls.clone();
    let (app, platform) = build_app(cfg, mock).await;

    let sid = "ses_live";
    {
        let cards = app.core.cards_handle();
        Turn::seed_card(&cards, sid, Some("om_live")).await;
        // An armed anchor: every assistant message counts as this turn's.
        Turn::set_turn_anchor(&cards, sid, &turn_anchor(0)).await;
        Turn::set_model(&cards, sid, "p", "m").await;
    }
    // A typed assistant step: the answering model and its usage, plus text.
    let transcript = |total: i64| {
        SessionTranscript::new(vec![TranscriptMessage {
            id: MessageId::new("a1"),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created: 1_000,
                completed: Some(1_000),
            }),
            model: Some(ModelIdentity {
                provider_id: "p".into(),
                model_id: "m".into(),
                variant: None,
            }),
            tokens: Some(TokenUsage {
                total,
                ..Default::default()
            }),
            error: None,
            parts: vec![text_part("回答")],
        }])
    };

    let _ = Turn::render_and_flush(
        &app.cards_handle(),
        &app.sessions_handle(),
        &app.opencode,
        &app.requests_handle(),
        sid,
        &transcript(42_000),
    )
    .await;
    let updates = platform.updated_cards().await;
    let text = updates.last().expect("a live flush").to_string();
    assert!(
        text.contains("📊 上下文 42k/100k (42%)"),
        "the live card must carry the context segment: {text}"
    );
    assert_eq!(
        window_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the first usage triggers exactly one window lookup"
    );

    // A later step's usage refreshes the segment; the memo serves the window.
    // The text is the SAME (deduped) and the header second has not moved, so
    // only the context signature can trigger this flush.
    let _ = Turn::render_and_flush(
        &app.cards_handle(),
        &app.sessions_handle(),
        &app.opencode,
        &app.requests_handle(),
        sid,
        &transcript(55_000),
    )
    .await;
    let updates = platform.updated_cards().await;
    assert!(
        updates
            .last()
            .unwrap()
            .to_string()
            .contains("📊 上下文 55k/100k (55%)"),
        "the refreshed usage must render on the live card"
    );
    assert_eq!(
        window_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the per-turn memo must not re-fetch the window"
    );
}

/// #310: a still-running assistant message created BEFORE the new Turn's anchor
/// must render on the live card while the run is in flight — not leave the card
/// blank until finalization. The previous Turn released its guard while the
/// session stayed busy (#284), so the new message started a fresh Turn whose
/// anchor postdates the in-flight step.
#[tokio::test]
async fn an_in_flight_step_before_the_anchor_renders_live() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::backend::{
        MessageId, MessageRole, MessageTime, Part, ReasoningPart, SessionTranscript, ToolStatus,
        TranscriptMessage,
    };

    use super::drain::{ctx, spawn_turn, user, wait_for_card_text};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // Park the prompt: the turn is still running while the test asserts.
    let gate = backend.hold_prompts();
    // The step the previous run left streaming: created BEFORE the new turn's
    // anchor, no completion stamp, a `task` still running.
    let in_flight = |completed: Option<i64>, status: ToolStatus, output: &str| TranscriptMessage {
        id: MessageId::new("msg_prev"),
        role: MessageRole::Assistant,
        time: Some(MessageTime {
            created: 500,
            completed,
        }),
        model: None,
        tokens: None,
        error: None,
        parts: vec![
            Part::Reasoning(ReasoningPart {
                text: "还在研究".into(),
                started_at: None,
            }),
            tool_part(
                "task",
                "call_task",
                status,
                serde_json::json!({ "description": "research" }),
                output,
            ),
        ],
    };
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 1_000, "我的问题你回答了吗"),
            in_flight(None, ToolStatus::Running, ""),
        ])],
    );
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);

    let turn = spawn_turn(&app, ctx("ses_test", "我的问题你回答了吗"));

    // The in-flight step renders on the live card while the prompt is still
    // held — no finalization has run yet.
    wait_for_card_text(&platform, "⏳ task").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the prompt must still be in flight when the panel renders"
    );

    // The step completes (completion stamp AFTER the anchor): its settled panel
    // lands on the live card too, still before finalization.
    {
        let mut scripts = backend.transcript_scripts.lock().await;
        let transcript = &mut scripts.get_mut("ses_test").unwrap()[0];
        transcript.messages[1] = in_flight(Some(2_500), ToolStatus::Completed, "research done");
    }
    wait_for_card_text(&platform, "research done").await;
    assert!(
        app.inflight.lock().await.contains("ses_test"),
        "the settled panel must render before finalization"
    );

    // Release the prompt: the turn finalizes with the content still on the card.
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish")
        .unwrap()
        .unwrap();
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    assert!(
        card_text(&final_card).contains("research done"),
        "the final card keeps the content: {final_card}"
    );
    assert!(card_header(&final_card).contains("完成"), "final card Done");
}

/// #378: an orphaned in-flight message must not replay its tool panel into a
/// later Turn's card — the new card renders its own content only.
#[tokio::test]
async fn an_orphaned_in_flight_message_does_not_render_on_a_later_turn() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::drain::{ctx, spawn_turn, user, wait_for_card_text};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // Park the prompt: the turn is still running while the test asserts.
    let gate = backend.hold_prompts();
    // The new Turn's own in-flight reply, created after the anchor.
    let live = TranscriptMessage {
        id: MessageId::new("msg_live"),
        role: MessageRole::Assistant,
        time: Some(MessageTime {
            created: 2_000_500,
            completed: None,
        }),
        model: None,
        tokens: None,
        error: None,
        parts: vec![text_part("新回合的内容")],
    };
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 2_000_000, "新消息"),
            // ~25 minutes of silence before the anchor: the killed run.
            orphaned_in_flight_message(500_000, 510_000, "zombie panel output", Some("被杀死的运行")),
            live,
        ])],
    );
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);

    let turn = spawn_turn(&app, ctx("ses_test", "新消息"));

    // The new Turn's own content renders live …
    wait_for_card_text(&platform, "新回合的内容").await;
    // … and further poll ticks never replay the orphan.
    tokio::time::sleep(Duration::from_millis(50)).await;
    for card in platform.updated_cards().await {
        let text = card_text(&card);
        assert!(
            !text.contains("sleep 120"),
            "the orphan's panel must not render: {text}"
        );
        assert!(
            !text.contains("zombie panel output"),
            "the orphan's output must not render: {text}"
        );
    }

    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish")
        .unwrap()
        .unwrap();
    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(
        !text.contains("sleep 120"),
        "the final card must not replay the orphan: {text}"
    );
    assert!(
        text.contains("新回合的内容"),
        "the new Turn's content must survive: {text}"
    );
    assert!(
        card_header(&final_card).contains("完成"),
        "the clean Turn finalizes Done: {text}"
    );
}

/// #378: a later Turn whose only assistant message in the window would be the
/// orphan finalizes clean — the dead run's recorded failure must not become
/// this Turn's error.
#[tokio::test]
async fn a_later_turn_does_not_inherit_an_orphans_failure() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::drain::{ctx, spawn_turn, user};

    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_transcript(
        "ses_test",
        vec![SessionTranscript::new(vec![
            user("msg_cola_anchor", 2_000_000, "新消息"),
            orphaned_in_flight_message(500_000, 510_000, "zombie panel output", Some("被杀死的运行")),
        ])],
    );
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_test", "/work").await;
    app.turn_render_poll_ms.store(5, Ordering::Relaxed);
    app.turn_drain_timeout_ms.store(60_000, Ordering::Relaxed);

    let turn = spawn_turn(&app, ctx("ses_test", "新消息"));
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the turn must finish")
        .unwrap()
        .unwrap();

    let final_card = platform.updated_cards().await.last().cloned().unwrap();
    let text = card_text(&final_card);
    assert!(!text.contains("sleep 120"), "the orphan must not render: {text}");
    assert!(
        !text.contains("执行失败"),
        "the dead run's failure must not leak into this Turn: {text}"
    );
    assert!(
        card_header(&final_card).contains("完成"),
        "the later Turn must finalize Done: {text}"
    );
}
