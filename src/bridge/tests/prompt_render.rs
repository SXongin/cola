use crate::bridge::test_support::*;

use crate::backend::{
    MessageId, MessageRole, MessageTime, Part, SessionTranscript, ToolCall, ToolIdentity, ToolOutput,
    ToolStatus, TranscriptMessage,
};

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
        existing_card_id: None,
        requester_open_id: None,
        is_group: false,
        cola_message_id: Some("msg_cola_failed".into()),
        images: Vec::new(),
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
        existing_card_id: None,
        requester_open_id: None,
        is_group: false,
        cola_message_id: Some("msg_cola_recovered".into()),
        images: Vec::new(),
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

#[tokio::test]
async fn error_card_retry_reuses_card_and_reruns_prompt() {
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

    // The card the retry will reuse: the loading reply card id.
    let card_id = match calls.first().unwrap() {
        PlatformCall::ReplyCard { card, .. } if card_text(card).contains("思考中") => "msg_reply",
        _ => panic!("expected a loading reply card first: {:?}", calls),
    };
    assert_eq!(card_id, "msg_reply");

    // User clicks the retry button.
    let retry = app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "retry ack card expected");
    assert_eq!(retry.unwrap().toast.as_deref(), Some("正在重试..."));

    // The spawned retry re-runs the prompt on the SAME card, not a new reply.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let calls = platform.calls.lock().await.clone();
    let updates: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            PlatformCall::UpdateMessage { message_id, card } => Some((message_id.clone(), card.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        updates.last().unwrap().0,
        "msg_reply",
        "retry must update the original card, not send a new one: {:?}",
        calls
    );
    let final_card = updates.last().unwrap().1.to_string();
    assert!(
        final_card.contains("✅"),
        "retry should finish Done: {}",
        final_card
    );
    assert!(
        final_card.contains("当前目录有 src/ 和 Cargo.toml。"),
        "retried answer missing: {}",
        final_card
    );

    let backend_calls = prompt_calls.lock().await.clone();
    assert_eq!(backend_calls, vec!["hi".to_string(), "hi".to_string()]);
    // Both attempts are the SAME logical user message: a fresh `msg_cola_`
    // id on the first send, REUSED by the retry (ADR-0026) so the server
    // deduplicates instead of appending a second user message.
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
        "retry must reuse the failed attempt's message id"
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

/// The failed attempt's scripted turn window, anchored on the user message the
/// retry reuses.
fn failed_attempt_window(anchor: &str) -> SessionTranscript {
    SessionTranscript::new(vec![
        typed_message(anchor, MessageRole::User, Some(1_000), vec![text_part("hi")]),
        failed_attempt_message(),
    ])
}

/// [`failed_attempt_window`] plus `extra`'s messages, newest last — an
/// attempt's window as the retry reads it.
fn window_with(anchor: &str, extra: Vec<TranscriptMessage>) -> SessionTranscript {
    let mut window = failed_attempt_window(anchor);
    window.messages.extend(extra);
    window
}

/// The first retry's own message; its recorded failure makes retry 1 end
/// Error again, so the chain can be retried once more.
fn first_retry_message() -> TranscriptMessage {
    let mut message = typed_message(
        "msg_new_1",
        MessageRole::Assistant,
        Some(3_000),
        vec![text_part("第一次重试：RETRY1_TEXT")],
    );
    message.error = Some("provider 503".into());
    message
}

/// #387: the retry reuses the failed attempt's `msg_cola_` user message
/// (ADR-0026), so the failed attempt's messages are still in the turn window
/// when the retry rebuilds the card. The retry must carry the failed attempt's
/// rendered baseline: the rebuilt card streams only the new attempt instead of
/// replaying every old part (122 tool panels on the live incident).
#[tokio::test]
async fn error_card_retry_does_not_replay_the_failed_attempt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    const ANCHOR: &str = "msg_cola_retry";
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
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
            existing_card_id: None,
            requester_open_id: None,
            is_group: false,
            cola_message_id: Some(ANCHOR.into()),
            images: Vec::new(),
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

/// #387: retries chain. Each retry's accumulator unions the baseline it
/// carried with every message its own attempt observed, so a retry after a
/// retry suppresses the original attempt and the first retry alike — only the
/// newest attempt renders into the rebuilt card.
#[tokio::test]
async fn retrying_again_suppresses_every_earlier_attempt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    const ANCHOR: &str = "msg_cola_chain";
    let mut mock = MockBackend::new(realistic_parts());
    mock.fail_prompts(1, "Simulated provider failure");
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
            existing_card_id: None,
            requester_open_id: None,
            is_group: false,
            cola_message_id: Some(ANCHOR.into()),
            images: Vec::new(),
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
