use crate::bridge::test_support::*;

/// The question kind rides the same poll loop, so the recovery guarantee
/// must hold for it too: a hung `list_questions` (half-open connection
/// after a server restart) cannot freeze the question poller.
#[tokio::test]
async fn question_poller_recovers_when_a_list_call_hangs() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.questions = vec![opencode::client::QuestionRequest {
        id: "que_hung".into(),
        session_id: "ses_test".into(),
        questions: vec![opencode::client::QuestionInfo {
            question: "继续吗？".into(),
            header: "下一步".into(),
            options: vec![opencode::client::QuestionOption {
                label: "继续".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: None,
        }],
    }];
    // The first list call hangs forever, like a request in flight when the
    // server was SIGTERM'd; later calls serve normally.
    backend
        .hang_list_questions
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (app, _platform) = build_app(cfg, backend).await;

    // Seed a session + accumulator so the poller has a reply target.
    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;

    tokio::spawn({
        let app = app.clone();
        async move {
            app.question
                .poll_interval_ms
                .store(20, std::sync::atomic::Ordering::Relaxed);
            app.question
                .list_timeout_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.question.poll_loop(&app.core).await;
        }
    });

    // Without a bound on the list call the poller sits on the first hung
    // call forever and the question never surfaces.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let surfaced = app
            .cards
            .lock()
            .await
            .get("ses_test")
            .map(|c| c.acc.pending_questions.iter().any(|q| q.request_id == "que_hung"))
            .unwrap_or(false);
        if surfaced {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "question poller never recovered from the hung list call"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn question_card_action_posts_answer_back() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    // The poll loop has surfaced a pending question request.
    app.question.question_requests.lock().await.insert(
        "que_1".into(),
        opencode::client::QuestionRequest {
            id: "que_1".into(),
            session_id: "ses_1".into(),
            questions: vec![opencode::client::QuestionInfo {
                question: "选择目录".into(),
                header: "目录".into(),
                options: vec![
                    opencode::client::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    },
                    opencode::client::QuestionOption {
                        label: "/b".into(),
                        description: String::new(),
                    },
                ],
                multiple: None,
                custom: None,
            }],
        },
    );
    // Seed the session → directory mapping so the reply routes correctly.
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_1".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    // User clicks the "/a" option button.
    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_1",
        "session_id": "ses_1",
        "question_index": 0,
        "answer": "/a",
    });
    let result = app.handle_card_action(value).await;
    assert!(result.is_some());
    assert!(
        result
            .unwrap()
            .card
            .as_ref()
            .unwrap()
            .to_string()
            .contains("已回答")
    );

    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "expected one reply_question call");
    assert_eq!(calls[0].0, "que_1");
    assert_eq!(calls[0].1, vec![vec!["/a".to_string()]]);
}

#[tokio::test]
async fn completing_last_question_replaces_card_with_full_qa_summary() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    app.question.question_requests.lock().await.insert(
        "que_1".into(),
        opencode::client::QuestionRequest {
            id: "que_1".into(),
            session_id: "ses_1".into(),
            questions: vec![
                opencode::client::QuestionInfo {
                    question: "问题甲".into(),
                    header: String::new(),
                    options: vec![
                        opencode::client::QuestionOption {
                            label: "/a1".into(),
                            description: String::new(),
                        },
                        opencode::client::QuestionOption {
                            label: "/a2".into(),
                            description: String::new(),
                        },
                    ],
                    multiple: None,
                    custom: None,
                },
                opencode::client::QuestionInfo {
                    question: "问题乙".into(),
                    header: String::new(),
                    options: vec![
                        opencode::client::QuestionOption {
                            label: "/b1".into(),
                            description: String::new(),
                        },
                        opencode::client::QuestionOption {
                            label: "/b2".into(),
                            description: String::new(),
                        },
                    ],
                    multiple: None,
                    custom: None,
                },
            ],
        },
    );
    seed_entry(
        &app,
        crate::config::SessionEntry {
            thread_key: crate::config::ThreadKey::new("chat_1".into(), "chat_1".into()),
            session_id: "ses_1".into(),
            directory: "/work".into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        },
    )
    .await;

    let value_for = |index: usize, answer: &str| {
        serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "question_index": index,
            "answer": answer,
        })
    };

    // Answering only the first question returns the card with the first
    // question collapsed (已选) and the second still open.
    let first = app.handle_card_action(value_for(0, "/a1")).await.unwrap();
    let first_card = first.card.unwrap().to_string();
    assert!(
        first_card.contains("问题乙"),
        "second question still open: {first_card}"
    );

    // Answering the LAST question submits once and replaces the card with a
    // summary of EVERY question and answer — not a card that only echoes
    // the last answer under a bogus "AI 的问题是" label.
    let done = app.handle_card_action(value_for(1, "/b1")).await.unwrap();
    let done_card = done.card.unwrap().to_string();
    assert!(
        done_card.contains("问题甲"),
        "first question missing: {done_card}"
    );
    assert!(done_card.contains("/a1"), "first answer missing: {done_card}");
    assert!(
        done_card.contains("问题乙"),
        "second question missing: {done_card}"
    );
    assert!(done_card.contains("/b1"), "second answer missing: {done_card}");
    assert!(
        !done_card.contains("AI 的问题是"),
        "bogus question label: {done_card}"
    );

    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "expected one reply_question call");
    assert_eq!(calls[0].0, "que_1");
    assert_eq!(calls[0].1, vec![vec!["/a1".to_string()], vec!["/b1".to_string()]]);
}

#[tokio::test]
async fn question_card_action_rejects() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    let value = serde_json::json!({
        "action": "question",
        "reply": "reject",
        "request_id": "que_1",
        "session_id": "ses_1",
        "directory": "/work",
    });
    let result = app.handle_card_action(value).await;
    assert!(result.is_some());
    assert!(
        result
            .unwrap()
            .card
            .as_ref()
            .unwrap()
            .to_string()
            .contains("拒绝")
    );

    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "que_1");
    assert!(calls[0].1[0][0].contains("reject"));
}

#[tokio::test]
async fn double_click_on_same_request_replies_once() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    // A pending single-question request (the same one the card was built for).
    app.question.question_requests.lock().await.insert(
        "que_1".into(),
        opencode::client::QuestionRequest {
            id: "que_1".into(),
            session_id: "ses_1".into(),
            questions: vec![opencode::client::QuestionInfo {
                question: "选择目录".into(),
                header: "目录".into(),
                options: vec![opencode::client::QuestionOption {
                    label: "/a".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            }],
        },
    );

    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_1",
        "session_id": "ses_1",
        "directory": "/work",
        "question_index": 0,
        "answer": "/a",
    });

    // First click replies; second (a fast re-click before the result card
    // replaces the buttons) must NOT re-reply — same request, one answer —
    // and must re-serve the first click's completion card, not a generic
    // ack.
    let first = app.handle_card_action(value.clone()).await;
    assert!(first.is_some());
    let first = first.unwrap();
    let second = app.handle_card_action(value).await;
    assert!(second.is_some(), "second click still gets the result card");
    assert_eq!(
        first.card,
        second.unwrap().card,
        "second click re-serves the first result"
    );

    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "double click must not double-reply: {:?}", calls);
}

/// A 404 on a question reply means the question was already answered (or
/// rejected) elsewhere — a neutral "已处理" card, not the red failure card.
#[tokio::test]
async fn question_reply_404_renders_neutral_already_handled() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.reply_question_not_found = true;
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    app.question.question_requests.lock().await.insert(
        "que_1".into(),
        opencode::client::QuestionRequest {
            id: "que_1".into(),
            session_id: "ses_1".into(),
            questions: vec![opencode::client::QuestionInfo {
                question: "选择目录".into(),
                header: "目录".into(),
                options: vec![opencode::client::QuestionOption {
                    label: "/a".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            }],
        },
    );

    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_1",
        "session_id": "ses_1",
        "directory": "/work",
        "question_index": 0,
        "answer": "/a",
    });
    let result = app.handle_card_action(value).await.expect("a result card");
    let card = result.card.expect("standalone card").to_string();
    assert!(card.contains("已处理"), "neutral card expected: {}", card);
    assert!(
        !card.contains("处理失败"),
        "a 404 must not render as a failure: {}",
        card
    );
}

#[tokio::test]
async fn question_with_multiple_parts_waits_for_all_answers() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    let mk_questions = || {
        vec![
            opencode::client::QuestionInfo {
                question: "选择目录".into(),
                header: "目录".into(),
                options: vec![opencode::client::QuestionOption {
                    label: "/a".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            },
            opencode::client::QuestionInfo {
                question: "选择分支".into(),
                header: "分支".into(),
                options: vec![opencode::client::QuestionOption {
                    label: "main".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            },
        ]
    };
    app.question.question_requests.lock().await.insert(
        "que_2".into(),
        opencode::client::QuestionRequest {
            id: "que_2".into(),
            session_id: "ses_1".into(),
            questions: mk_questions(),
        },
    );

    let value = |index: u64, answer: &str| {
        serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_2",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": index,
            "answer": answer,
        })
    };

    // Answer the FIRST question only: must NOT submit (the second is open).
    let first = app.handle_card_action(value(0, "/a")).await;
    assert!(first.is_some());
    let first = first.unwrap();
    assert_eq!(first.toast.as_deref(), Some("已记录答案，还有 1 题未答"));
    // The returned card is still a question card (not a result card).
    let first_card = first.card.as_ref().unwrap().to_string();
    assert!(first_card.contains("❓ AI 想问你"));
    assert!(first_card.contains("已选：/a"));
    assert!(
        !first_card.contains("已选：main"),
        "question 2 not answered yet: {}",
        first_card
    );
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Answer the SECOND question: now everything is answered → submits.
    let second = app.handle_card_action(value(1, "main")).await;
    assert!(second.is_some());
    assert_eq!(second.unwrap().toast.as_deref(), Some("已回答"));
    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "one reply_question call total");
    assert_eq!(calls[0].0, "que_2");
    assert_eq!(calls[0].1, vec![vec!["/a".to_string()], vec!["main".to_string()]]);
}

/// A multi-select question (`multiple: true`) never finalizes on an option
/// click: each click toggles the label in the answer set, the returned card
/// shows the running selection (已选) and a per-question 确定该题 button, and
/// only that confirm commits — once it does and every question is answered,
/// the request auto-submits with the accumulated set.
#[tokio::test]
async fn multi_select_question_toggles_until_submit() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());

    app.question.question_requests.lock().await.insert(
        "que_multi".into(),
        opencode::client::QuestionRequest {
            id: "que_multi".into(),
            session_id: "ses_1".into(),
            questions: vec![opencode::client::QuestionInfo {
                question: "选择水果".into(),
                header: "水果".into(),
                options: vec![
                    opencode::client::QuestionOption {
                        label: "苹果".into(),
                        description: String::new(),
                    },
                    opencode::client::QuestionOption {
                        label: "香蕉".into(),
                        description: String::new(),
                    },
                    opencode::client::QuestionOption {
                        label: "橙子".into(),
                        description: String::new(),
                    },
                ],
                multiple: Some(true),
                custom: None,
            }],
        },
    );

    let value = |answer: &str| {
        serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_multi",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": answer,
        })
    };

    // Click 苹果 → NOT submitted (multi-select toggles, never auto-submits).
    let r1 = app.handle_card_action(value("苹果")).await.expect("result");
    assert_eq!(r1.toast.as_deref(), Some("已添加选项"));
    let c1 = r1.card.as_ref().expect("re-rendered card").to_string();
    assert!(c1.contains("已选：苹果"), "marker missing: {}", c1);
    assert!(c1.contains("可多选"), "multi hint missing: {}", c1);
    assert!(c1.contains("✅ 确定该题"), "confirm button missing: {}", c1);
    // The selected button shows its ✅/checked state in the card JSON.
    assert!(
        c1.contains("\"content\":\"✅ 苹果\""),
        "selected button state missing: {}",
        c1
    );
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Click 香蕉 → accumulates a second label.
    let r2 = app.handle_card_action(value("香蕉")).await.expect("result");
    let c2 = r2.card.as_ref().expect("re-rendered card").to_string();
    assert!(c2.contains("已选：苹果、香蕉"), "accumulate failed: {}", c2);
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Click 苹果 again → toggles it OFF, only 香蕉 remains.
    let r3 = app.handle_card_action(value("苹果")).await.expect("result");
    assert_eq!(r3.toast.as_deref(), Some("已移除选项"));
    let c3 = r3.card.as_ref().expect("re-rendered card").to_string();
    assert!(c3.contains("已选：香蕉"), "toggle off failed: {}", c3);
    assert!(!c3.contains("已选：苹果、香蕉"), "toggle off kept 苹果: {}", c3);
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // 确定该题 → commits the toggled set; the single question is answered so
    // the request auto-submits with the accumulated set.
    let confirm = app
        .handle_card_action(serde_json::json!({
            "action": "question",
            "reply": "confirm",
            "request_id": "que_multi",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
        }))
        .await
        .expect("result");
    assert_eq!(confirm.toast.as_deref(), Some("已回答"));
    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, vec![vec!["香蕉".to_string()]]);
}

/// A multi-select question can be confirmed with an EMPTY selection ("不选"):
/// the per-question 确定该题 button is always present, and confirming with
/// nothing toggled replies with an empty answer set.
#[tokio::test]
async fn multi_select_can_submit_empty_selection() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());

    app.question.question_requests.lock().await.insert(
        "que_empty".into(),
        opencode::client::QuestionRequest {
            id: "que_empty".into(),
            session_id: "ses_1".into(),
            questions: vec![opencode::client::QuestionInfo {
                question: "选择水果".into(),
                header: "水果".into(),
                options: vec![opencode::client::QuestionOption {
                    label: "苹果".into(),
                    description: String::new(),
                }],
                multiple: Some(true),
                custom: None,
            }],
        },
    );

    // Toggle 苹果 on, then off → back to an open question with NO selection.
    app.handle_card_action(serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_empty",
        "session_id": "ses_1",
        "directory": "/work",
        "question_index": 0,
        "answer": "苹果",
    }))
    .await;
    let r2 = app
        .handle_card_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_empty",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": "苹果",
        }))
        .await
        .expect("result");
    // The re-rendered card has NO selection but STILL shows the 确定该题
    // button, so "不选" is expressible.
    let c2 = r2.card.as_ref().expect("re-rendered card").to_string();
    assert!(!c2.contains("已选"), "selection must be cleared: {}", c2);
    assert!(c2.contains("✅ 确定该题"), "confirm must stay visible: {}", c2);
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Confirming with nothing selected replies with an empty set.
    let confirm = app
        .handle_card_action(serde_json::json!({
            "action": "question",
            "reply": "confirm",
            "request_id": "que_empty",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
        }))
        .await
        .expect("result");
    assert_eq!(confirm.toast.as_deref(), Some("已回答"));
    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, vec![Vec::<String>::new()]);
}

/// A stale card may re-confirm an already-confirmed multi-select question
/// (the re-render hasn't removed the button yet, while other questions stay
/// open). That second confirm must be a no-op — it must NOT wipe the locked
/// answer by overwriting it with the now-empty toggles slot.
#[tokio::test]
async fn stale_confirm_on_done_multi_select_is_a_no_op() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());

    app.question.question_requests.lock().await.insert(
        "que_stale".into(),
        opencode::client::QuestionRequest {
            id: "que_stale".into(),
            session_id: "ses_1".into(),
            questions: vec![
                opencode::client::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
                opencode::client::QuestionInfo {
                    question: "选择水果".into(),
                    header: "水果".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "苹果".into(),
                        description: String::new(),
                    }],
                    multiple: Some(true),
                    custom: None,
                },
            ],
        },
    );

    let confirm = |index: u64| {
        serde_json::json!({
            "action": "question",
            "reply": "confirm",
            "request_id": "que_stale",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": index,
        })
    };

    // Toggle 苹果 on Q1 then confirm it → Q1 locked, Q0 still open.
    app.handle_card_action(serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_stale",
        "session_id": "ses_1",
        "directory": "/work",
        "question_index": 1,
        "answer": "苹果",
    }))
    .await;
    let r1 = app.handle_card_action(confirm(1)).await.expect("result");
    assert_eq!(r1.toast.as_deref(), Some("已确定该题，还有 1 题未答"));
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // A stale second confirm on Q1 must NOT wipe 苹果 with an empty set.
    let r2 = app.handle_card_action(confirm(1)).await.expect("result");
    assert_eq!(r2.toast.as_deref(), Some("已确定该题，还有 1 题未答"));
    let c2 = r2.card.as_ref().expect("re-rendered card").to_string();
    assert!(
        c2.contains("已选：苹果"),
        "stale confirm wiped the answer: {}",
        c2
    );
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Answer Q0 → both done → submits with 苹果 intact.
    let ans = app
        .handle_card_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_stale",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": "/a",
        }))
        .await
        .expect("result");
    assert_eq!(ans.toast.as_deref(), Some("已回答"));
    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, vec![vec!["/a".to_string()], vec!["苹果".to_string()]]);
}

/// A mixed request (one single-select + one multi-select) only submits once
/// EVERY question is finalized: toggling multi-select options or typing a
/// custom answer must never auto-submit, and confirming the multi-select
/// while the single-select is still open stays in-progress. Only when both
/// are done does the request reply.
#[tokio::test]
async fn mixed_single_and_multi_question_waits_for_all_confirmed() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());

    app.question.question_requests.lock().await.insert(
        "que_mix".into(),
        opencode::client::QuestionRequest {
            id: "que_mix".into(),
            session_id: "ses_1".into(),
            questions: vec![
                opencode::client::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
                opencode::client::QuestionInfo {
                    question: "选择水果".into(),
                    header: "水果".into(),
                    options: vec![opencode::client::QuestionOption {
                        label: "苹果".into(),
                        description: String::new(),
                    }],
                    multiple: Some(true),
                    custom: None,
                },
            ],
        },
    );

    let answer = |index: u64, a: &str| {
        serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_mix",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": index,
            "answer": a,
        })
    };

    // Answer the single-select (Q0) → Q1 still open, NO auto-submit yet.
    let r0 = app.handle_card_action(answer(0, "/a")).await.expect("result");
    assert_eq!(r0.toast.as_deref(), Some("已记录答案，还有 1 题未答"));
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Toggle a multi-select option (Q1) → still no submit (not confirmed).
    let r1 = app.handle_card_action(answer(1, "苹果")).await.expect("result");
    assert_eq!(r1.toast.as_deref(), Some("已添加选项"));
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Type a custom answer into the multi-select (reply "custom") → the
    // label is appended to the toggles, still no submit.
    let r2 = app
        .handle_card_action(serde_json::json!({
            "action": "question",
            "reply": "custom",
            "request_id": "que_mix",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 1,
            "answer": "自定义水果",
        }))
        .await
        .expect("result");
    assert_eq!(r2.toast.as_deref(), Some("已添加自定义答案"));
    // The displayed selection now holds the option + the custom label.
    let c2 = r2.card.as_ref().expect("re-rendered card").to_string();
    assert!(c2.contains("已选：苹果、自定义水果"), "append failed: {}", c2);
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Confirm Q1 → both questions done → auto-submits with both answers.
    let confirm = app
        .handle_card_action(serde_json::json!({
            "action": "question",
            "reply": "confirm",
            "request_id": "que_mix",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 1,
        }))
        .await
        .expect("result");
    assert_eq!(confirm.toast.as_deref(), Some("已回答"));
    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].1,
        vec![
            vec!["/a".to_string()],
            vec!["苹果".to_string(), "自定义水果".to_string()]
        ]
    );
}

/// A Custom Answer in a multi-select: the submitted text is appended verbatim
/// (one entry per submission — newlines and punctuation are never split),
/// re-submitting the same text is a deduped no-op with its own toast, the card
/// renders the entry as a selected button, clicking that button removes it, and
/// a blank submission only hints.
#[tokio::test]
async fn multi_select_custom_answer_appends_dedupes_and_removes() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());

    app.question.question_requests.lock().await.insert(
        "que_custom".into(),
        opencode::client::QuestionRequest {
            id: "que_custom".into(),
            session_id: "ses_1".into(),
            questions: vec![opencode::client::QuestionInfo {
                question: "选择水果".into(),
                header: "水果".into(),
                options: vec![opencode::client::QuestionOption {
                    label: "苹果".into(),
                    description: String::new(),
                }],
                multiple: Some(true),
                custom: None,
            }],
        },
    );

    let custom = |answer: &str| {
        serde_json::json!({
            "action": "question",
            "reply": "custom",
            "request_id": "que_custom",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": answer,
        })
    };

    // Append raw text: the newline stays inside the single entry (no split).
    let r1 = app
        .handle_card_action(custom("自定\n答案"))
        .await
        .expect("result");
    assert_eq!(r1.toast.as_deref(), Some("已添加自定义答案"));
    let c1 = r1.card.as_ref().expect("re-rendered card").to_string();
    assert!(c1.contains("已选：自定"), "custom not in selection: {}", c1);
    // The removable chip keeps the RAW answer in its value; only the display
    // label collapses the newline.
    assert!(
        c1.contains("\"answer\":\"自定\\n答案\""),
        "raw answer lost: {}",
        c1
    );
    assert!(
        c1.contains("✅ 自定 答案"),
        "collapsed chip label missing: {}",
        c1
    );

    // Re-submitting the same text is deduped, with its own toast.
    let r2 = app
        .handle_card_action(custom("自定\n答案"))
        .await
        .expect("result");
    assert_eq!(r2.toast.as_deref(), Some("该选项已在已选中"));

    // Clicking the chip (reply "answer" with the raw text) removes it.
    let r3 = app
        .handle_card_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_custom",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": "自定\n答案",
        }))
        .await
        .expect("result");
    assert_eq!(r3.toast.as_deref(), Some("已移除选项"));
    let c3 = r3.card.as_ref().expect("re-rendered card").to_string();
    assert!(!c3.contains("已选：自定"), "custom kept after removal: {}", c3);

    // A blank submission only hints and changes nothing.
    let r4 = app.handle_card_action(custom("")).await.expect("result");
    assert_eq!(r4.toast.as_deref(), Some("请输入自定义答案"));
    assert!(r4.card.is_none());
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);
}

/// A question raised during an active turn is surfaced INLINE on the
/// streaming card; answering one of several only toasts and returns the
/// re-rendered streaming card (markers/已选 in the callback response — the
/// mechanism Feishu actually refreshes the clicked card with), while the
/// final answer finalizes the request and drops the inline section.
#[tokio::test]
async fn inline_question_answered_on_streaming_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut mock = MockBackend::new(realistic_parts());
    mock.questions = vec![opencode::client::QuestionRequest {
        id: "que_inline".into(),
        session_id: "ses_test".into(),
        questions: vec![
            opencode::client::QuestionInfo {
                question: "选目录".into(),
                header: "目录".into(),
                options: vec![opencode::client::QuestionOption {
                    label: "/a".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            },
            opencode::client::QuestionInfo {
                question: "选分支".into(),
                header: "分支".into(),
                options: vec![opencode::client::QuestionOption {
                    label: "main".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            },
        ],
    }];
    let backend = Arc::new(mock);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    // Seed a session + active accumulator (an in-flight turn).
    app.handle_message(incoming(
        "msg_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert!(
        app.cards.lock().await.contains_key("ses_test"),
        "accumulator expected"
    );

    // Run the question poller → the question is inlined on the accumulator.
    tokio::spawn({
        let app = app.clone();
        async move {
            app.question
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.question.poll_loop(&app.core).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let pending = app
        .cards
        .lock()
        .await
        .get("ses_test")
        .unwrap()
        .acc
        .pending_questions
        .clone();
    assert_eq!(pending.len(), 1, "question should be inlined");
    assert_eq!(pending[0].request_id, "que_inline");

    // Answer the first question → toast only, no card replacement.
    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_inline",
        "session_id": "ses_test",
        "question_index": 0,
        "answer": "/a",
    });
    let r1 = app.handle_card_action(value).await.expect("result");
    assert_eq!(r1.toast.as_deref(), Some("已记录答案，还有 1 题未答"));
    // The returned card is the RE-RENDERED streaming card (markers in the
    // callback response — the reliable card-update mechanism). Feishu's
    // PATCH alone leaves the clicked card on its pre-answer state.
    let r1_card = r1
        .card
        .as_ref()
        .expect("inline answer must return the rebuilt card");
    let r1_text = r1_card.to_string();
    assert!(r1_text.contains("已选：/a"), "marker missing: {}", r1_text);
    assert!(!r1_text.contains("已选：main"), "q2 must stay open: {}", r1_text);
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);
    // The accumulator's inline question reflects the partial answer.
    let pending = app
        .cards
        .lock()
        .await
        .get("ses_test")
        .unwrap()
        .acc
        .pending_questions
        .clone();
    assert_eq!(pending[0].answers[0], Some(vec!["/a".to_string()]));
    assert_eq!(pending[0].answers[1], None);

    // Answer the second → finalized, reply called, inline section removed.
    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_inline",
        "session_id": "ses_test",
        "question_index": 1,
        "answer": "main",
    });
    let r2 = app.handle_card_action(value).await.expect("result");
    assert_eq!(r2.toast.as_deref(), Some("已回答"));
    assert!(r2.card.is_none(), "inline final answer must not replace the card");
    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, vec![vec!["/a".to_string()], vec!["main".to_string()]]);
    assert!(
        app.cards
            .lock()
            .await
            .get("ses_test")
            .unwrap()
            .acc
            .pending_questions
            .is_empty()
    );
}
