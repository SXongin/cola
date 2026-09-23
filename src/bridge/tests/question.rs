use crate::bridge::test_support::*;

/// A minimal one-question request for the state-lifecycle tests (#130).
fn question_request(id: &str) -> opencode::types::QuestionRequest {
    opencode::types::QuestionRequest {
        id: id.into(),
        session_id: "ses_1".into(),
        questions: vec![opencode::types::QuestionInfo {
            question: "选择目录".into(),
            header: "目录".into(),
            options: vec![opencode::types::QuestionOption {
                label: "/a".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: None,
        }],
    }
}

/// Seed a session mapping so the poll sweep has one known directory (`/work`).
async fn seed_work_dir(app: &Arc<App>) {
    seed_entry(
        app,
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
}

/// The question kind rides the same poll loop, so the recovery guarantee
/// must hold for it too: a hung `list_questions` (half-open connection
/// after a server restart) cannot freeze the question poller.
#[tokio::test]
async fn question_poller_recovers_when_a_list_call_hangs() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_questions(vec![opencode::types::QuestionRequest {
        id: "que_hung".into(),
        session_id: "ses_test".into(),
        questions: vec![opencode::types::QuestionInfo {
            question: "继续吗？".into(),
            header: "下一步".into(),
            options: vec![opencode::types::QuestionOption {
                label: "继续".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: None,
        }],
    }]);
    // The first list call hangs forever, like a request in flight when the
    // server was SIGTERM'd; later calls serve normally.
    backend.hang_question_lists(1);
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
            let _ = app.question.poll_loop(&app.flow_handles()).await;
        }
    });

    // Without a bound on the list call the poller sits on the first hung
    // call forever and the question never surfaces.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let surfaced = Turn::has_interaction_in(&app.cards_handle(), "ses_test", "que_hung").await;
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
    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_1".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::types::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![
                        opencode::types::QuestionOption {
                            label: "/a".into(),
                            description: String::new(),
                        },
                        opencode::types::QuestionOption {
                            label: "/b".into(),
                            description: String::new(),
                        },
                    ],
                    multiple: None,
                    custom: None,
                }],
            },
            "/work",
        )
        .await;
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
    let result = app.host_action(value).await;
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

    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_1".into(),
                session_id: "ses_1".into(),
                questions: vec![
                    opencode::types::QuestionInfo {
                        question: "问题甲".into(),
                        header: String::new(),
                        options: vec![
                            opencode::types::QuestionOption {
                                label: "/a1".into(),
                                description: String::new(),
                            },
                            opencode::types::QuestionOption {
                                label: "/a2".into(),
                                description: String::new(),
                            },
                        ],
                        multiple: None,
                        custom: None,
                    },
                    opencode::types::QuestionInfo {
                        question: "问题乙".into(),
                        header: String::new(),
                        options: vec![
                            opencode::types::QuestionOption {
                                label: "/b1".into(),
                                description: String::new(),
                            },
                            opencode::types::QuestionOption {
                                label: "/b2".into(),
                                description: String::new(),
                            },
                        ],
                        multiple: None,
                        custom: None,
                    },
                ],
            },
            "/work",
        )
        .await;
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
    let first = app.host_action(value_for(0, "/a1")).await.unwrap();
    let first_card = first.card.unwrap().to_string();
    assert!(
        first_card.contains("问题乙"),
        "second question still open: {first_card}"
    );

    // Answering the LAST question submits once and replaces the card with a
    // summary of EVERY question and answer — not a card that only echoes
    // the last answer under a bogus "AI 的问题是" label.
    let done = app.host_action(value_for(1, "/b1")).await.unwrap();
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

    // The poll loop surfaced the request; its state is what a reject resolves.
    app.question
        .remember_question(&question_request("que_1"), "/work")
        .await;

    let value = serde_json::json!({
        "action": "question",
        "reply": "reject",
        "request_id": "que_1",
        "session_id": "ses_1",
        "directory": "/work",
    });
    let result = app.host_action(value).await;
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
    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_1".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::types::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::types::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                }],
            },
            "/work",
        )
        .await;

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
    let first = app.host_action(value.clone()).await;
    assert!(first.is_some());
    let first = first.unwrap();
    let second = app.host_action(value).await;
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
    backend.question_resolved_elsewhere();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_1".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::types::QuestionInfo {
                    question: "选择目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::types::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                }],
            },
            "/work",
        )
        .await;

    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_1",
        "session_id": "ses_1",
        "directory": "/work",
        "question_index": 0,
        "answer": "/a",
    });
    let result = app.host_action(value).await.expect("a result card");
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
            opencode::types::QuestionInfo {
                question: "选择目录".into(),
                header: "目录".into(),
                options: vec![opencode::types::QuestionOption {
                    label: "/a".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            },
            opencode::types::QuestionInfo {
                question: "选择分支".into(),
                header: "分支".into(),
                options: vec![opencode::types::QuestionOption {
                    label: "main".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            },
        ]
    };
    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_2".into(),
                session_id: "ses_1".into(),
                questions: mk_questions(),
            },
            "/work",
        )
        .await;

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
    let first = app.host_action(value(0, "/a")).await;
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
    let second = app.host_action(value(1, "main")).await;
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

    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_multi".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::types::QuestionInfo {
                    question: "选择水果".into(),
                    header: "水果".into(),
                    options: vec![
                        opencode::types::QuestionOption {
                            label: "苹果".into(),
                            description: String::new(),
                        },
                        opencode::types::QuestionOption {
                            label: "香蕉".into(),
                            description: String::new(),
                        },
                        opencode::types::QuestionOption {
                            label: "橙子".into(),
                            description: String::new(),
                        },
                    ],
                    multiple: Some(true),
                    custom: None,
                }],
            },
            "/work",
        )
        .await;

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
    let r1 = app.host_action(value("苹果")).await.expect("result");
    assert_eq!(r1.toast.as_deref(), Some("已添加选项"));
    let c1_card = r1.card.as_ref().expect("re-rendered card");
    let c1 = card_text(c1_card);
    assert!(c1.contains("已选：苹果"), "marker missing: {}", c1);
    assert!(c1.contains("可多选"), "multi hint missing: {}", c1);
    assert!(c1.contains("✅ 确定该题"), "confirm button missing: {}", c1);
    // The selected button shows its ✅/checked state in the card JSON.
    assert!(
        card_buttons(c1_card)
            .iter()
            .any(|b| b["value"]["answer"] == "苹果" && b["text"]["content"] == "✅ 苹果"),
        "selected button state missing: {}",
        c1
    );
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Click 香蕉 → accumulates a second label.
    let r2 = app.host_action(value("香蕉")).await.expect("result");
    let c2 = r2.card.as_ref().expect("re-rendered card").to_string();
    assert!(c2.contains("已选：苹果、香蕉"), "accumulate failed: {}", c2);
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Click 苹果 again → toggles it OFF, only 香蕉 remains.
    let r3 = app.host_action(value("苹果")).await.expect("result");
    assert_eq!(r3.toast.as_deref(), Some("已移除选项"));
    let c3 = r3.card.as_ref().expect("re-rendered card").to_string();
    assert!(c3.contains("已选：香蕉"), "toggle off failed: {}", c3);
    assert!(!c3.contains("已选：苹果、香蕉"), "toggle off kept 苹果: {}", c3);
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // 确定该题 → commits the toggled set; the single question is answered so
    // the request auto-submits with the accumulated set.
    let confirm = app
        .host_action(serde_json::json!({
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

    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_empty".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::types::QuestionInfo {
                    question: "选择水果".into(),
                    header: "水果".into(),
                    options: vec![opencode::types::QuestionOption {
                        label: "苹果".into(),
                        description: String::new(),
                    }],
                    multiple: Some(true),
                    custom: None,
                }],
            },
            "/work",
        )
        .await;

    // Toggle 苹果 on, then off → back to an open question with NO selection.
    app.host_action(serde_json::json!({
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
        .host_action(serde_json::json!({
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
        .host_action(serde_json::json!({
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

    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_stale".into(),
                session_id: "ses_1".into(),
                questions: vec![
                    opencode::types::QuestionInfo {
                        question: "选择目录".into(),
                        header: "目录".into(),
                        options: vec![opencode::types::QuestionOption {
                            label: "/a".into(),
                            description: String::new(),
                        }],
                        multiple: None,
                        custom: None,
                    },
                    opencode::types::QuestionInfo {
                        question: "选择水果".into(),
                        header: "水果".into(),
                        options: vec![opencode::types::QuestionOption {
                            label: "苹果".into(),
                            description: String::new(),
                        }],
                        multiple: Some(true),
                        custom: None,
                    },
                ],
            },
            "/work",
        )
        .await;

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
    app.host_action(serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_stale",
        "session_id": "ses_1",
        "directory": "/work",
        "question_index": 1,
        "answer": "苹果",
    }))
    .await;
    let r1 = app.host_action(confirm(1)).await.expect("result");
    assert_eq!(r1.toast.as_deref(), Some("已确定该题，还有 1 题未答"));
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // A stale second confirm on Q1 must NOT wipe 苹果 with an empty set.
    let r2 = app.host_action(confirm(1)).await.expect("result");
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
        .host_action(serde_json::json!({
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

    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_mix".into(),
                session_id: "ses_1".into(),
                questions: vec![
                    opencode::types::QuestionInfo {
                        question: "选择目录".into(),
                        header: "目录".into(),
                        options: vec![opencode::types::QuestionOption {
                            label: "/a".into(),
                            description: String::new(),
                        }],
                        multiple: None,
                        custom: None,
                    },
                    opencode::types::QuestionInfo {
                        question: "选择水果".into(),
                        header: "水果".into(),
                        options: vec![opencode::types::QuestionOption {
                            label: "苹果".into(),
                            description: String::new(),
                        }],
                        multiple: Some(true),
                        custom: None,
                    },
                ],
            },
            "/work",
        )
        .await;

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
    let r0 = app.host_action(answer(0, "/a")).await.expect("result");
    assert_eq!(r0.toast.as_deref(), Some("已记录答案，还有 1 题未答"));
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Toggle a multi-select option (Q1) → still no submit (not confirmed).
    let r1 = app.host_action(answer(1, "苹果")).await.expect("result");
    assert_eq!(r1.toast.as_deref(), Some("已添加选项"));
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);

    // Type a custom answer into the multi-select (reply "custom") → the
    // label is appended to the toggles, still no submit.
    let r2 = app
        .host_action(serde_json::json!({
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
        .host_action(serde_json::json!({
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

    app.question
        .remember_question(
            &opencode::types::QuestionRequest {
                id: "que_custom".into(),
                session_id: "ses_1".into(),
                questions: vec![opencode::types::QuestionInfo {
                    question: "选择水果".into(),
                    header: "水果".into(),
                    options: vec![opencode::types::QuestionOption {
                        label: "苹果".into(),
                        description: String::new(),
                    }],
                    multiple: Some(true),
                    custom: None,
                }],
            },
            "/work",
        )
        .await;

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
    let r1 = app.host_action(custom("自定\n答案")).await.expect("result");
    assert_eq!(r1.toast.as_deref(), Some("已添加自定义答案"));
    let c1_card = r1.card.as_ref().expect("re-rendered card");
    let c1 = card_text(c1_card);
    assert!(c1.contains("已选：自定"), "custom not in selection: {}", c1);
    // The removable chip keeps the RAW answer in its value; only the display
    // label collapses the newline.
    assert!(
        card_buttons(c1_card)
            .iter()
            .any(|b| b["value"]["answer"] == "自定\n答案"),
        "raw answer lost: {}",
        c1
    );
    assert!(
        c1.contains("✅ 自定 答案"),
        "collapsed chip label missing: {}",
        c1
    );

    // Re-submitting the same text is deduped, with its own toast.
    let r2 = app.host_action(custom("自定\n答案")).await.expect("result");
    assert_eq!(r2.toast.as_deref(), Some("该选项已在已选中"));

    // Clicking the chip (reply "answer" with the raw text) removes it.
    let r3 = app
        .host_action(serde_json::json!({
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
    let r4 = app.host_action(custom("")).await.expect("result");
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
    mock.ask_questions(vec![opencode::types::QuestionRequest {
        id: "que_inline".into(),
        session_id: "ses_test".into(),
        questions: vec![
            opencode::types::QuestionInfo {
                question: "选目录".into(),
                header: "目录".into(),
                options: vec![opencode::types::QuestionOption {
                    label: "/a".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            },
            opencode::types::QuestionInfo {
                question: "选分支".into(),
                header: "分支".into(),
                options: vec![opencode::types::QuestionOption {
                    label: "main".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            },
        ],
    }]);
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
        Turn::has_card(&app.cards_handle(), "ses_test").await,
        "accumulator expected"
    );

    // Run the question poller → the question is inlined on the accumulator.
    tokio::spawn({
        let app = app.clone();
        async move {
            app.question
                .poll_interval_ms
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.question.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let pending = Turn::live_questions(&app.cards_handle(), "ses_test").await;
    assert_eq!(pending.len(), 1, "question should be inlined");
    assert_eq!(pending[0].0, "que_inline");

    // Answer the first question → toast only, no card replacement.
    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_inline",
        "session_id": "ses_test",
        "question_index": 0,
        "answer": "/a",
    });
    let r1 = app.host_action(value).await.expect("result");
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
    let pending = Turn::live_questions(&app.cards_handle(), "ses_test").await;
    assert_eq!(pending[0].1[0], Some(vec!["/a".to_string()]));
    assert_eq!(pending[0].1[1], None);

    // Answer the second → finalized, reply called, inline section removed.
    let value = serde_json::json!({
        "action": "question",
        "reply": "answer",
        "request_id": "que_inline",
        "session_id": "ses_test",
        "question_index": 1,
        "answer": "main",
    });
    let r2 = app.host_action(value).await.expect("result");
    assert_eq!(r2.toast.as_deref(), Some("已回答"));
    // The ack carries the clicked card with the resolved block replaced by
    // its Interaction Receipt (ADR-0038) — the final answer is as instant as
    // the partial ones.
    let ack = r2
        .card
        .as_ref()
        .expect("inline final answer must carry the updated card in the ack")
        .to_string();
    assert!(
        ack.contains("✅ 已回答：目录 /a、分支 main"),
        "receipt missing: {}",
        ack
    );
    assert!(
        !ack.contains("无法回答") && !ack.contains("已选："),
        "the resolved block (and its controls) must be gone: {}",
        ack
    );
    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, vec![vec!["/a".to_string()], vec!["main".to_string()]]);
    assert!(
        Turn::live_questions(&app.cards_handle(), "ses_test")
            .await
            .is_empty()
    );
}

/// #130: a question resolved by another client leaves the pending list. The
/// sweep must drop its remembered state, while a still-pending request's state
/// survives so its card keeps resolving.
#[tokio::test]
async fn sweep_prunes_state_of_requests_resolved_elsewhere() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // que_live is still pending; que_gone was resolved elsewhere meanwhile.
    backend.ask_question(question_request("que_live"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
    seed_work_dir(&app).await;

    app.question
        .remember_question(&question_request("que_gone"), "/work")
        .await;
    app.question
        .remember_question(&question_request("que_live"), "/work")
        .await;

    let mut seen = std::collections::HashSet::new();
    app.question.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        !app.question.has_question("que_gone").await,
        "state of a request that left pending must be pruned"
    );
    assert!(
        app.question.has_question("que_live").await,
        "state of a pending request must survive the sweep"
    );
}

/// #130: a directory whose list call fails or times out this sweep says
/// nothing about its requests — their state must survive (unknown ≠ resolved).
#[tokio::test]
async fn sweep_keeps_state_when_the_directory_list_fails() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    // The first list call hangs (half-open connection after a restart); later
    // calls would serve normally.
    backend.hang_question_lists(1);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
    app.question
        .list_timeout_ms
        .store(30, std::sync::atomic::Ordering::Relaxed);
    seed_work_dir(&app).await;

    app.question
        .remember_question(&question_request("que_1"), "/work")
        .await;

    let mut seen = std::collections::HashSet::new();
    app.question.sweep(&app.flow_handles(), &mut seen).await;

    assert!(
        app.question.has_question("que_1").await,
        "a failed list must not be read as 'resolved'"
    );
}

/// #144: a directory whose list call fails must not clear its live question
/// surfaces — the standalone card, the inline section and the snapshot claim
/// all survive until a SUCCESSFUL list proves the request gone.
#[tokio::test]
async fn failed_directory_list_keeps_question_surfaces() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_work_dir(&app).await;

    assert_failed_dir_keeps_surfaces(
        &app,
        &app.question,
        &backend.hang_list_questions,
        &platform,
        FailedDirSurfaces {
            card_id: "que_card",
            card_message_id: "om_card",
            inline_id: "que_inline",
            snapshot_message_id: "om_snapshot",
            inline_request: crate::bridge::request::kind::PendingRequest::Question(question_request(
                "que_inline",
            )),
            claim: crate::bridge::request::kind::PendingRequest::Question(question_request("que_claim")),
        },
    )
    .await;
}

/// #130: the sweep must not wipe an in-progress multi-select. A refresh keeps
/// the live toggles, and the prune keeps the entry while it is pending.
#[tokio::test]
async fn sweep_keeps_partial_multi_select_toggles() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let multi = opencode::types::QuestionRequest {
        id: "que_multi".into(),
        session_id: "ses_1".into(),
        questions: vec![opencode::types::QuestionInfo {
            question: "选择水果".into(),
            header: "水果".into(),
            options: vec![
                opencode::types::QuestionOption {
                    label: "苹果".into(),
                    description: String::new(),
                },
                opencode::types::QuestionOption {
                    label: "香蕉".into(),
                    description: String::new(),
                },
            ],
            multiple: Some(true),
            custom: None,
        }],
    };
    let mut backend = MockBackend::new(realistic_parts());
    // The request is still pending, so the sweep refreshes instead of pruning.
    backend.ask_question(multi.clone());
    let backend = Arc::new(backend);
    let app = Arc::new(App::new(cfg, backend.clone(), Arc::new(RecordingPlatform::new())).unwrap());
    seed_work_dir(&app).await;
    app.question.remember_question(&multi, "/work").await;

    let value = |reply: &str, answer: &str| {
        serde_json::json!({
            "action": "question",
            "reply": reply,
            "request_id": "que_multi",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": answer,
        })
    };

    let toggled = app
        .host_action(value("answer", "苹果"))
        .await
        .expect("toggle result");
    assert!(
        card_text(&toggled.card.unwrap()).contains("已选：苹果"),
        "toggle must be live before the sweep"
    );

    let mut seen = std::collections::HashSet::new();
    app.question.sweep(&app.flow_handles(), &mut seen).await;

    // The live selection survived: confirming locks it and submits with it.
    app.host_action(value("confirm", ""))
        .await
        .expect("confirm result");
    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "confirming the surviving toggle must submit");
    assert_eq!(calls[0].1, vec![vec!["苹果".to_string()]]);
}

/// #130: a directory that leaves the session store stops being polled, so its
/// pruned state must not be classified as resolved later — the click gets the
/// truthful stale-card hint instead of a false "已处理".
#[tokio::test]
async fn vanished_directory_is_no_longer_treated_as_known() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_question(question_request("que_1"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
    seed_work_dir(&app).await;

    app.question
        .remember_question(&question_request("que_1"), "/work")
        .await;

    // First sweep: the request is pending → state kept, /work known.
    let mut seen = std::collections::HashSet::new();
    app.question.sweep(&app.flow_handles(), &mut seen).await;
    assert!(app.question.has_question("que_1").await);

    // The session mapping is forgotten: /work is no longer polled, so the
    // sweep has no evidence the request resolved — it only drops the state.
    app.core.sessions.lock().await.remove_persist("ses_1").unwrap();
    app.question.sweep(&app.flow_handles(), &mut seen).await;
    assert!(
        !app.question.has_question("que_1").await,
        "state of an unpolled directory is dropped"
    );

    let result = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": "/a",
        }))
        .await
        .expect("a late click must get a result");
    let card = result.card.expect("standalone card").to_string();
    assert!(card.contains("失效"), "stale-card hint expected: {card}");
    assert!(
        !card.contains("已处理"),
        "an unpolled directory must not be read as resolved: {card}"
    );
}

/// #130: an inline late click never replaces the streaming card — it gets a
/// card-less ack with the classified toast (neutral when the directory is
/// known, truthful stale otherwise).
#[tokio::test]
async fn late_inline_click_is_cardless_and_classified() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
    seed_work_dir(&app).await;
    Turn::seed_card(&app.cards_handle(), "ses_1", None).await;

    // A pruned sweep marks /work as a known directory.
    app.question
        .remember_question(&question_request("que_1"), "/work")
        .await;
    let mut seen = std::collections::HashSet::new();
    app.question.sweep(&app.flow_handles(), &mut seen).await;

    let value = |id: &str, directory: &str| {
        serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": id,
            "session_id": "ses_1",
            "directory": directory,
            "question_index": 0,
            "answer": "/a",
        })
    };

    // Known directory, state gone → neutral toast, no card replacement.
    let neutral = app.host_action(value("que_1", "/work")).await.expect("result");
    assert!(
        neutral.card.is_none(),
        "an inline answer must not replace the streaming card"
    );
    assert_eq!(neutral.toast.as_deref(), Some("该问题已处理"));

    // Unknown directory → truthful stale toast, still card-less.
    let stale = app.host_action(value("que_1", "/other")).await.expect("result");
    assert!(
        stale.card.is_none(),
        "an inline answer must not replace the streaming card"
    );
    assert_eq!(stale.toast.as_deref(), Some("此卡片已失效"));
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);
}

/// #130: after the sweep pruned a request resolved elsewhere, a late click on
/// its card gets the neutral result — no backend call, no silent no-op, and no
/// revived per-request state.
#[tokio::test]
async fn late_click_on_pruned_request_is_neutral_and_stateless() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
    seed_work_dir(&app).await;

    app.question
        .remember_question(&question_request("que_1"), "/work")
        .await;

    // A successful sweep with nothing pending prunes the state.
    let mut seen = std::collections::HashSet::new();
    app.question.sweep(&app.flow_handles(), &mut seen).await;
    assert!(!app.question.has_question("que_1").await);

    let result = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "directory": "/work",
            "question_index": 0,
            "answer": "/a",
        }))
        .await
        .expect("a late click must get a result, never a silent no-op");
    let card = result.card.expect("standalone card").to_string();
    assert!(card.contains("已处理"), "neutral card expected: {}", card);
    assert!(!card.contains("处理失败"), "must not be a failure: {}", card);
    assert_eq!(
        backend.reply_question_calls.lock().await.len(),
        0,
        "a pruned request must not be replied to"
    );
    assert!(
        !app.question.has_question("que_1").await,
        "the neutral response must not revive state"
    );
}

/// #130: missing state + a directory this process never listed successfully
/// (fresh restart before the first sweep, unknown directory) proves nothing —
/// the click must not claim the request was handled. Same truth without a
/// payload directory.
#[tokio::test]
async fn late_click_without_directory_knowledge_is_truthful_not_handled() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    let value = |directory: Option<&str>| {
        let mut v = serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_1",
            "session_id": "ses_1",
            "question_index": 0,
            "answer": "/a",
        });
        if let Some(d) = directory {
            v["directory"] = serde_json::Value::String(d.to_string());
        }
        v
    };

    for directory in [Some("/work"), None] {
        let result = app
            .host_action(value(directory))
            .await
            .expect("a late click must get a result, never a silent no-op");
        let card = result.card.expect("standalone card").to_string();
        assert!(
            card.contains("失效"),
            "stale-card hint expected (directory={directory:?}): {card}"
        );
        assert!(
            !card.contains("已处理"),
            "must not claim handled without knowledge (directory={directory:?}): {card}"
        );
        assert!(
            !card.contains("处理失败"),
            "must not be a failure (directory={directory:?}): {card}"
        );
    }
    assert_eq!(
        backend.reply_question_calls.lock().await.len(),
        0,
        "an unverifiable click must not be replied to"
    );
    assert!(
        !app.question.has_question("que_1").await,
        "the stale-card response must not create state"
    );
}

/// #130: submit/reject ride the same gate, and a gated click must leave no
/// claim behind — a later legitimate click still replies, and once answered a
/// double click still re-serves the winning result.
#[tokio::test]
async fn gated_submit_leaves_no_claim_and_preserves_replays() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = Arc::new(MockBackend::new(realistic_parts()));
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());

    let value = |id: &str, reply: &str| {
        serde_json::json!({
            "action": "question",
            "reply": reply,
            "request_id": id,
            "session_id": "ses_1",
            "directory": "/work",
        })
    };

    // Gated: no state, unknown directory — no reply, no state revived.
    app.host_action(value("que_submit", "submit"))
        .await
        .expect("gated submit still answers");
    assert_eq!(backend.reply_question_calls.lock().await.len(), 0);
    assert!(!app.question.has_question("que_submit").await);

    // The request is re-listed and remembered: submit replies exactly once.
    app.question
        .remember_question(&question_request("que_submit"), "/work")
        .await;
    let first = app
        .host_action(value("que_submit", "submit"))
        .await
        .expect("submit result");
    assert_eq!(backend.reply_question_calls.lock().await.len(), 1);
    // A fast re-click re-serves the winning result, never a second reply.
    let second = app
        .host_action(value("que_submit", "submit"))
        .await
        .expect("replay result");
    assert_eq!(first.card, second.card, "double click replays the first result");
    assert_eq!(backend.reply_question_calls.lock().await.len(), 1);

    // Same composition for reject.
    app.host_action(value("que_reject", "reject"))
        .await
        .expect("gated reject still answers");
    assert_eq!(backend.reply_question_calls.lock().await.len(), 1);
    app.question
        .remember_question(&question_request("que_reject"), "/work")
        .await;
    let first = app
        .host_action(value("que_reject", "reject"))
        .await
        .expect("reject result");
    assert_eq!(backend.reply_question_calls.lock().await.len(), 2);
    let second = app
        .host_action(value("que_reject", "reject"))
        .await
        .expect("replay result");
    assert_eq!(first.card, second.card, "double click replays the first result");
    assert_eq!(backend.reply_question_calls.lock().await.len(), 2);
}

// ===== Interaction Receipts (ADR-0038) =====

/// Final submit ("跳过剩余") and reject both replace the inline block with
/// their Interaction Receipt in the same ack — the click's OWN card, updated
/// atomically, not a PATCH behind a toast.
#[tokio::test]
async fn inline_question_submit_and_reject_leave_receipts() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_questions(vec![
        opencode::types::QuestionRequest {
            id: "que_submit".into(),
            session_id: "ses_test".into(),
            questions: vec![
                opencode::types::QuestionInfo {
                    question: "选目录".into(),
                    header: "目录".into(),
                    options: vec![opencode::types::QuestionOption {
                        label: "/a".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
                opencode::types::QuestionInfo {
                    question: "选分支".into(),
                    header: "分支".into(),
                    options: vec![opencode::types::QuestionOption {
                        label: "main".into(),
                        description: String::new(),
                    }],
                    multiple: None,
                    custom: None,
                },
            ],
        },
        opencode::types::QuestionRequest {
            id: "que_reject".into(),
            session_id: "ses_test".into(),
            questions: vec![opencode::types::QuestionInfo {
                question: "继续吗？".into(),
                header: "下一步".into(),
                options: vec![opencode::types::QuestionOption {
                    label: "继续".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            }],
        },
    ]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
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
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.question.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        Turn::live_questions(&app.cards_handle(), "ses_test").await.len(),
        2,
        "both questions inlined"
    );

    // Partially answer que_submit, then submit ("跳过剩余").
    let partial = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_submit",
            "session_id": "ses_test",
            "question_index": 0,
            "answer": "/a",
        }))
        .await
        .expect("a card-action result");
    assert!(
        card_text(partial.card.as_ref().expect("partial answers carry the card")).contains("已选：/a"),
        "partial markers must stay live"
    );
    let submitted = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "submit",
            "request_id": "que_submit",
            "session_id": "ses_test",
        }))
        .await
        .expect("a card-action result");
    assert_eq!(submitted.toast.as_deref(), Some("已提交"));
    let ack_card = submitted
        .card
        .as_ref()
        .expect("submit must carry the updated card in the ack");
    let ack = card_text(ack_card);
    assert!(
        ack.contains("✅ 已回答：目录 /a、分支 （未作答）"),
        "submit receipt missing: {}",
        ack
    );
    assert!(
        !card_buttons(ack_card)
            .iter()
            .any(|b| b["value"]["request_id"] == "que_submit")
            && !ack.contains("已选："),
        "the submitted block (and its controls) is gone: {}",
        ack
    );
    // The other request's block is still live.
    assert!(ack.contains("继续吗？"), "que_reject must stay: {}", ack);

    // Reject the other request: a second receipt, no live block left.
    let rejected = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "reject",
            "request_id": "que_reject",
            "session_id": "ses_test",
        }))
        .await
        .expect("a card-action result");
    assert_eq!(rejected.toast.as_deref(), Some("已拒绝回答"));
    let ack_card = rejected
        .card
        .as_ref()
        .expect("reject must carry the updated card in the ack");
    let ack = card_text(ack_card);
    assert!(
        ack.contains("🚫 已拒绝：下一步"),
        "reject receipt missing: {}",
        ack
    );
    assert!(
        !card_buttons(ack_card)
            .iter()
            .any(|b| b["value"]["request_id"] == "que_reject")
            && !ack.contains("继续吗？"),
        "the rejected block (and its controls) is gone: {}",
        ack
    );
    // Both receipts are rendered from the accumulator.
    let cards = app.cards_handle();
    assert!(Turn::live_questions(&cards, "ses_test").await.is_empty());
    let rendered = Turn::rendered_card(&cards, "ses_test").await.unwrap().to_string();
    assert!(
        rendered.contains("✅ 已回答：目录 /a、分支 （未作答）"),
        "{}",
        rendered
    );
    assert!(rendered.contains("🚫 已拒绝：下一步"), "{}", rendered);

    let calls = backend.reply_question_calls.lock().await.clone();
    assert_eq!(calls.len(), 2, "one submit + one reject: {:?}", calls);
    assert_eq!(
        calls[0].1,
        vec![vec!["/a".to_string()], Vec::<String>::new()],
        "submit carries the answered slots and the skipped one empty"
    );
}

/// A click that finds the question already resolved by another client gets the
/// neutral "handled elsewhere" receipt on the clicked card, carried in the
/// same ack.
#[tokio::test]
async fn inline_question_click_after_remote_resolution_gets_receipt() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_questions(vec![opencode::types::QuestionRequest {
        id: "que_gone".into(),
        session_id: "ses_test".into(),
        questions: vec![opencode::types::QuestionInfo {
            question: "选目录".into(),
            header: "目录".into(),
            options: vec![opencode::types::QuestionOption {
                label: "/a".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: None,
        }],
    }]);
    backend.question_resolved_elsewhere();
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform).unwrap());
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
                .store(50, std::sync::atomic::Ordering::Relaxed);
            let _ = app.question.poll_loop(&app.flow_handles()).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let result = app
        .host_action(serde_json::json!({
            "action": "question",
            "reply": "answer",
            "request_id": "que_gone",
            "session_id": "ses_test",
            "question_index": 0,
            "answer": "/a",
        }))
        .await
        .expect("a card-action result");
    assert_eq!(result.toast.as_deref(), Some("该问题已处理"));
    let ack = result
        .card
        .expect("the ack must carry the clicked card")
        .to_string();
    assert!(
        ack.contains("⏱ 已由其他客户端处理：目录"),
        "neutral receipt missing: {}",
        ack
    );
    assert!(
        !ack.contains("无法回答"),
        "the resolved block must be gone: {}",
        ack
    );
    assert!(
        Turn::live_questions(&app.cards_handle(), "ses_test")
            .await
            .is_empty()
    );
}
