//! `/cat <path>` — the File Viewer (spec #669, ticket #738, ADR-0083 §2-§4).
//!
//! Every test drives the real command path (`send_command_in` → the
//! coordinator) against **real temp directories**, then asserts the card JSON
//! the Platform fake recorded — the same seam the `/dir` browse card's tests
//! use. What is pinned here is the ticket's contract: which file is named, how
//! its bytes are shaped, what a directory/missing/empty/unreadable path
//! answers, and that the viewer is a command card (no Chain Record, no reap).

use crate::bridge::test_support::*;

/// A seeded active SessionEntry rooted at `directory`.
fn seeded_entry(key: &crate::config::ThreadKey, directory: &str) -> crate::config::SessionEntry {
    crate::config::SessionEntry {
        thread_key: key.clone(),
        session_id: "ses_seed".into(),
        directory: directory.to_string(),
        agent: None,
        model: None,
        auto_accept: false,
        topic_anchor: None,
        topic_root: None,
        variant: None,
    }
}

/// The canonical spelling of `path` — what the viewer must name, since it
/// resolves the path instead of echoing what was typed.
fn canonical(path: &std::path::Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// The one card the command replied with.
async fn first_card(platform: &RecordingPlatform) -> serde_json::Value {
    platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the command replies with a card")
}

fn thread() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

/// `/cat` (no arg) answers usage and `/help cat` documents the command — and
/// neither reaches the Backend, while an unknown `/...` still does (ADR-0083
/// §2: `/cat` swallows nothing else).
#[tokio::test]
async fn cat_documents_itself_and_never_swallows_other_commands() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/cat", "msg_usage").await;
    let texts = platform.texts().await;
    let usage = texts
        .iter()
        .find(|t| t.contains("/cat <路径>"))
        .unwrap_or_else(|| panic!("bare /cat answers usage, got: {texts:?}"));
    assert!(
        usage.contains("~") && usage.contains("会话目录"),
        "the usage names the Path Scope: {usage}"
    );

    send_command(&app, "/help cat", "msg_help").await;
    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("/cat <directory>")),
        "/help cat documents the command: {texts:?}"
    );

    assert!(
        prompts.lock().await.is_empty(),
        "a local command never becomes a prompt"
    );

    // An unknown `/...` is still prompt text (only the exact `/cat` token is
    // registered).
    send_command(&app, "/catalog everything", "msg_unknown").await;
    assert!(
        prompts
            .lock()
            .await
            .iter()
            .any(|text| text.contains("/catalog everything")),
        "an unrecognized command still forwards to the Backend: {:?}",
        prompts.lock().await
    );
}

/// A text file opens as a File Viewer: the card names the RESOLVED path, the
/// size and the type, and its body is a fenced code block carrying the
/// extension's language hint. A relative path resolves against the Session's
/// directory — not the process cwd, which the harness deliberately points at a
/// different tempdir.
#[tokio::test]
async fn cat_opens_a_text_file_with_the_language_hint_and_the_resolved_path() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::create_dir(proj.path().join("src")).unwrap();
    let file = proj.path().join("src").join("main.rs");
    let body = "fn main() {\n    println!(\"hello\");\n}\n";
    std::fs::write(&file, body).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat src/main.rs",
        key.clone(),
        "msg_cat",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert_eq!(card_header(&card), "📄 main.rs", "the header names the file");
    assert!(
        text.contains(&canonical(&file)),
        "the RESOLVED path is named: {text}"
    );
    assert!(
        text.contains(&format!("`{}`", canonical(&file))),
        "the path is the resolved one, spelled in full: {text}"
    );
    assert!(
        text.contains(&format!("{} B", body.len())),
        "the size is named: {text}"
    );
    assert!(text.contains("文本（.rs）"), "the type is named: {text}");
    assert!(text.contains("```rust"), "the extension's language hint: {text}");
    assert!(text.contains("println!"), "the file's own bytes: {text}");
    assert!(!text.contains("已截断"), "a small file is not truncated: {text}");
}

/// An extension the platform has no language token for renders as `plain_text`
/// — honestly, never a near-miss highlight (ADR-0083 §6; `.ts` is ambiguous,
/// `.log`/`.env` have no token at all).
#[tokio::test]
async fn cat_unknown_extensions_degrade_to_plain_text() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    for name in ["build.log", "config.ts", ".env"] {
        std::fs::write(proj.path().join(name), "not highlighted\n").unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    for name in ["build.log", "config.ts", ".env"] {
        send_command_in(
            &app,
            &format!("/cat {name}"),
            key.clone(),
            &format!("msg_{name}"),
            crate::config::ConversationKind::P2p,
        )
        .await;
        let cards = platform.replied_cards().await;
        let card = cards.last().expect("a viewer card");
        let text = card_text(card);
        assert!(
            text.contains("```plain_text"),
            "`{name}` degrades to plain text: {text}"
        );
        assert!(
            !text.contains("```typescript") && !text.contains("```ts"),
            "`{name}` is never given a token it does not have: {text}"
        );
    }
}

/// A delimited-text file previews as a markdown table — a table is what those
/// bytes are (ADR-0083 §5) — for both `.csv` and `.tsv`, with quoted fields
/// kept whole.
#[tokio::test]
async fn cat_delimited_files_preview_as_markdown_tables() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(
        proj.path().join("people.csv"),
        "name,age\n\"Doe, Jane\",41\nbob,7\n",
    )
    .unwrap();
    std::fs::write(proj.path().join("tabs.tsv"), "key\tvalue\nhost\tlocalhost\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat people.csv",
        key.clone(),
        "m1",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("表格（.csv）"), "the type is a table: {text}");
    assert!(
        text.contains("| name | age |") && text.contains("| --- | --- |"),
        "a markdown table header: {text}"
    );
    assert!(
        text.contains("| Doe, Jane | 41 |"),
        "a quoted cell stays one cell: {text}"
    );
    assert!(text.contains("| bob | 7 |"), "the body rows: {text}");
    assert!(!text.contains("```"), "the table is not also fenced: {text}");

    send_command_in(
        &app,
        "/cat tabs.tsv",
        key.clone(),
        "m2",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = platform.replied_cards().await.last().cloned().expect("a card");
    let text = card_text(&card);
    assert!(text.contains("表格（.tsv）"), "{text}");
    assert!(
        text.contains("| key | value |") && text.contains("| host | localhost |"),
        "a tab-delimited file is a table too: {text}"
    );
}

/// The scope is advisory, not a boundary (ADR-0083 §4): an absolute path
/// outside the Session's directory opens like any other, naming its own file.
#[tokio::test]
async fn cat_accepts_an_absolute_path_outside_the_session_directory() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join("notes.md");
    std::fs::write(&outside, "# notes\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        &format!("/cat {}", outside.display()),
        key.clone(),
        "m_out",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("```markdown"),
        "a .md file is highlighted as markdown: {text}"
    );
    assert!(
        text.contains(&canonical(&outside)),
        "the path outside the session directory is named: {text}"
    );
}

/// `~` expands against the real home. A read-only HOME skips (the resolver's
/// rule is unit-tested in `bridge::viewer`).
#[tokio::test]
async fn cat_expands_a_tilde_path() {
    let _wd = test_work_dir();
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let Ok(home_dir) = tempfile::tempdir_in(&home) else {
        return; // a read-only HOME: nothing to create, nothing to assert
    };
    let file = home_dir.path().join("notes.md");
    std::fs::write(&file, "~ notes\n").unwrap();
    let relative = file.strip_prefix(&home).unwrap().to_string_lossy().to_string();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &dir.path().to_string_lossy())).await;

    send_command_in(
        &app,
        &format!("/cat ~/{relative}"),
        key.clone(),
        "m_tilde",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains(&canonical(&file)),
        "`~/…` resolves to the file under home: {text}"
    );
}

/// A Pending Session resolves the scope against the conversation's current
/// Project, and a conversation with neither falls back to the default
/// directory (ADR-0083 §4).
#[tokio::test]
async fn cat_resolves_against_the_pending_project_then_the_fallback() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("pending.md"), "pending\n").unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("pending.md"), "fallback\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(work.path().to_path_buf());
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();

    // No session at all: the default directory (work_dir) is the base.
    send_command_in(
        &app,
        "/cat pending.md",
        key.clone(),
        "m_fallback",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    assert!(
        card_text(&card).contains(&canonical(&work.path().join("pending.md"))),
        "with neither a session nor a pending, the default directory is the base: {}",
        card_text(&card)
    );
    assert!(
        !card_text(&card).contains(&canonical(&proj.path().join("pending.md"))),
        "and not the pending project that does not exist yet"
    );

    // A Pending Session declared by `/dir` moves the base to that project —
    // no server session is created (ADR-0041).
    send_command_in(
        &app,
        &format!("/dir {}", proj.path().display()),
        key.clone(),
        "m_dir",
        crate::config::ConversationKind::P2p,
    )
    .await;
    send_command_in(
        &app,
        "/cat pending.md",
        key.clone(),
        "m_pending",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = platform.replied_cards().await.last().cloned().expect("a card");
    let text = card_text(&card);
    assert!(
        text.contains(&canonical(&proj.path().join("pending.md"))),
        "a Pending Session resolves against its project: {text}"
    );
    assert!(text.contains("pending"), "and shows that file's bytes: {text}");
}

/// `/cat <directory>` answers the Directory Browser at that directory instead
/// of erroring: `/cat` always means "show me this path" (ADR-0083 §2).
#[tokio::test]
async fn cat_on_a_directory_answers_the_directory_browser() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::create_dir(proj.path().join("alpha")).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &dir.path().to_string_lossy())).await;

    send_command_in(
        &app,
        &format!("/cat {}", proj.path().display()),
        key.clone(),
        "m_dir",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert_eq!(card_header(&card), "📁 浏览目录", "the browser, not a viewer");
    assert!(text.contains("📁 alpha"), "the directory's children: {text}");
    assert!(
        text.contains(&canonical(proj.path())),
        "the browsed directory is named: {text}"
    );
    assert!(platform.texts().await.is_empty(), "a directory is not an error");
}

/// A missing path and an empty file each answer distinctly, naming the path —
/// never an empty card.
#[tokio::test]
async fn cat_missing_and_empty_paths_answer_distinctly() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("empty.txt"), "").unwrap();
    let missing = proj.path().join("ghost.txt");

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat ghost.txt",
        key.clone(),
        "m_missing",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let texts = platform.texts().await;
    let line = texts.last().cloned().unwrap_or_default();
    assert!(line.contains("找不到路径"), "a missing path says so: {texts:?}");
    assert!(
        line.contains(&canonical(&missing)),
        "and names the resolved path: {line}"
    );

    send_command_in(
        &app,
        "/cat empty.txt",
        key.clone(),
        "m_empty",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let texts = platform.texts().await;
    let line = texts.last().cloned().unwrap_or_default();
    assert!(line.contains("空文件"), "an empty file says so: {texts:?}");
    assert!(line.contains("0 字节"), "and why it is empty: {line}");
    assert!(
        line.contains(&canonical(&proj.path().join("empty.txt"))),
        "{line}"
    );

    assert!(
        platform.replied_cards().await.is_empty(),
        "a failure is its own text answer — never an empty card"
    );
    assert_eq!(platform.texts().await.len(), 2, "one distinct answer per failure");
}

/// A path cola cannot read answers distinctly from a missing one (ADR-0083
/// §2), naming the path — never an empty card. The first case holds for every
/// user (a file used as a directory component); the mode-bit case is only
/// observable unprivileged, so as root it stops there.
#[cfg(unix)]
#[tokio::test]
async fn cat_unreadable_paths_answer_distinctly() {
    use std::os::unix::fs::PermissionsExt;

    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    // A regular file where the path needs a directory: `ENOTDIR`, which no
    // privilege level turns into a readable file.
    let file = proj.path().join("a.txt");
    std::fs::write(&file, "x\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        &format!("/cat {}/inside.txt", file.display()),
        key.clone(),
        "m_notdir",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let line = platform.texts().await.last().cloned().unwrap_or_default();
    assert!(
        line.contains("无法读取"),
        "a path that cannot be read says so: {line}"
    );
    assert!(line.contains(&canonical(&file)), "and names the path: {line}");
    assert!(platform.replied_cards().await.is_empty(), "never an empty card");

    // A permission-denied path: mode bits only deny an unprivileged reader.
    let locked = proj.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let secret = locked.join("secret.txt");
    std::fs::write(&secret, "hidden\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::metadata(&secret).is_ok() {
        // Running as root: no denial to observe (CI's runners are unprivileged).
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    send_command_in(
        &app,
        "/cat locked/secret.txt",
        key.clone(),
        "m_denied",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let line = platform.texts().await.last().cloned().unwrap_or_default();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(line.contains("无法读取"), "a denied path says so: {line}");
    assert!(
        line.contains(&canonical(&secret)),
        "and names the path it was denied: {line}"
    );
}

/// The viewer is a command card (ADR-0083 §3): it joins no Card Chain, writes
/// no Chain Record entry and therefore can never be reaped, and it dispatches
/// no Turn — nothing about the session moves.
#[tokio::test]
async fn viewer_card_joins_no_chain_and_is_never_reaped() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("a.txt"), "hi\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat a.txt",
        key.clone(),
        "m_cat",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert!(!platform.replied_cards().await.is_empty(), "a viewer card landed");

    let cards = app.cards_handle();
    assert!(
        cards.chains.entries().is_empty(),
        "the viewer writes no Chain Record entry: {:?}",
        cards.chains.entries()
    );
    assert!(
        !cards.chains.recorded("ses_seed"),
        "and nothing is recorded for the session either"
    );
    assert!(
        cards.cards.lock().await.is_empty(),
        "no live CardSession is installed for a command card"
    );
    assert!(prompts.lock().await.is_empty(), "the viewer dispatches no Turn");
    // The session mapping is untouched: `/cat` is not a session command.
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key)
            .map(|e| e.session_id.clone()),
        Some("ses_seed".to_string())
    );
}

/// 「📂 所在目录」 opens the Directory Browser at the file's parent directory —
/// and its payload is the browse handler's own shape, which the card click
/// proves by actually rendering the browser (ADR-0083 §3).
#[tokio::test]
async fn parent_directory_button_opens_the_browser_at_the_files_parent() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::create_dir(proj.path().join("src")).unwrap();
    std::fs::create_dir(proj.path().join("src").join("inner")).unwrap();
    std::fs::write(proj.path().join("src").join("lib.rs"), "pub fn f() {}\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat src/lib.rs",
        key.clone(),
        "m_cat",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let button = card_buttons(&card)
        .into_iter()
        .find(|b| b["text"]["content"] == "📂 所在目录")
        .expect("the viewer carries 所在目录")
        .clone();
    let payload = button["value"].clone();
    assert_eq!(payload["action"], "dir", "the payload routes to the browser");

    let result = app.host_action(payload).await.expect("the click produces a card");
    let opened = result.card.expect("a browse card");
    let text = card_text(&opened);
    assert_eq!(card_header(&opened), "📁 浏览目录");
    assert!(
        text.contains(&canonical(&proj.path().join("src"))),
        "the browser opens at the file's parent: {text}"
    );
    assert!(
        text.contains("📁 inner"),
        "with that directory's children: {text}"
    );
}

/// A viewer's 「← 返回目录」 payload re-renders the browse listing it was opened
/// from, filter/page/hidden intact — the round trip #742's file rows will wire
/// through `ViewerCard::return_to` (ADR-0083 §3).
#[tokio::test]
async fn return_to_directory_restores_the_listing_state() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    for name in ["alpha", "beta"] {
        std::fs::create_dir(proj.path().join(name)).unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    // The `restore` payload a viewer's 返回目录 carries (the card builder emits
    // exactly this shape from `ViewerCard::return_to`).
    let payload = serde_json::json!({
        "action": "dir",
        "view": "browse",
        "op": "restore",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "dir": proj.path().to_string_lossy().to_string(),
        "show_hidden": true,
        "keyword": "alph",
        "page": 1,
    });
    let result = app.host_action(payload).await.expect("a card");
    let text = card_text(&result.card.expect("the listing"));
    assert!(text.contains("📁 alpha"), "the listing comes back: {text}");
    assert!(
        !text.contains("📁 beta"),
        "with its active filter preserved: {text}"
    );
    assert!(
        text.contains("隐藏点目录"),
        "and its hidden toggle preserved: {text}"
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "the ack patches in place"
    );
}
