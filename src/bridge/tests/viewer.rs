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
/// resolves the path instead of echoing what was typed. Shared with the
/// browse-card tests (they assert the same resolved spelling on a file row and
/// on the viewer it opens): one helper, so the two suites cannot drift.
pub(super) fn canonical(path: &std::path::Path) -> String {
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

/// The pager's indicator text (`第 x/y 页 · 共 N 个`) on a card, when it has one.
fn pager_label(card: &serde_json::Value) -> Option<String> {
    card_texts(card)
        .into_iter()
        .find(|t| t.starts_with("第 ") && t.contains(" 页 · 共 "))
}

/// The pager button labelled `label` (上一页 / 下一页).
fn pager_button<'a>(card: &'a serde_json::Value, label: &str) -> &'a serde_json::Value {
    card_buttons(card)
        .into_iter()
        .find(|b| b["value"]["op"] == "page" && b["text"]["content"] == label)
        .unwrap_or_else(|| panic!("pager button `{label}` not found on {}", card_text(card)))
}

/// `lines` numbered lines, each short enough that one card holds thousands —
/// used to build a preview that genuinely outgrows one card.
fn numbered_lines(lines: usize) -> String {
    (1..=lines).map(|i| format!("line {i:04}\n")).collect()
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

/// A structurally real PNG (signature + IHDR declaring 4×4) — the magic bytes
/// `infer` reads, and dimensions Feishu's embed cap accepts.
fn png() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes
}

/// Every recorded upload of `platform`, in call order.
async fn uploads(platform: &RecordingPlatform) -> Vec<PlatformCall> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| {
            matches!(
                call,
                PlatformCall::UploadImage { .. } | PlatformCall::UploadFile { .. }
            )
        })
        .cloned()
        .collect()
}

/// Every File Message `platform` posted (`msg_type:"file"`).
async fn file_messages(platform: &RecordingPlatform) -> Vec<PlatformCall> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| matches!(call, PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "file"))
        .cloned()
        .collect()
}

/// Find the 「发为文件」 button's payload on a card.
fn send_as_file_payload(card: &serde_json::Value) -> serde_json::Value {
    card_buttons(card)
        .into_iter()
        .find(|b| b["text"]["content"] == "发为文件")
        .unwrap_or_else(|| panic!("发为文件 missing on {}", card_text(card)))["value"]
        .clone()
}

/// The three-way routing is by CONTENT, not by name (ADR-0083 §5, §6): a PNG
/// embeds, text previews, and a non-UTF-8 file goes out as exactly one File
/// Message — with each name deliberately lying about its bytes.
#[tokio::test]
async fn cat_routes_image_text_and_binary_by_content() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("picture.txt"), png()).unwrap();
    std::fs::write(proj.path().join("notes.bin"), "just text\n").unwrap();
    std::fs::write(proj.path().join("blob.txt"), [0xff, 0xfe, 0x00, 0x01]).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    // An image named `.txt` is recognised by magic bytes and embedded.
    send_command_in(
        &app,
        "/cat picture.txt",
        key.clone(),
        "m_img",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert_eq!(card_header(&card), "📄 picture.txt", "the header names the file");
    assert!(text.contains("图片（.txt）"), "the type is by content: {text}");
    assert!(text.contains("已内嵌"), "the image is embedded: {text}");
    let recorded = uploads(&platform).await;
    assert_eq!(recorded.len(), 1, "one image upload: {recorded:?}");
    assert!(
        matches!(recorded[0], PlatformCall::UploadImage { .. }),
        "{recorded:?}"
    );

    // A text file named `.bin` previews as text.
    send_command_in(
        &app,
        "/cat notes.bin",
        key.clone(),
        "m_txt",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = platform.replied_cards().await.last().cloned().expect("a card");
    let text = card_text(&card);
    assert!(text.contains("文本（.bin）"), "the type is by content: {text}");
    assert!(
        text.contains("```plain_text") && text.contains("just text"),
        "{text}"
    );
    assert_eq!(
        uploads(&platform).await.len(),
        1,
        "a text preview uploads nothing"
    );

    // A non-UTF-8 file named `.txt` is a binary: one File Message, never text.
    send_command_in(
        &app,
        "/cat blob.txt",
        key.clone(),
        "m_bin",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = platform.replied_cards().await.last().cloned().expect("a card");
    let text = card_text(&card);
    assert!(text.contains("二进制（.txt）"), "the type is by content: {text}");
    assert!(
        !text.contains("```"),
        "a non-UTF-8 file is never rendered as text: {text}"
    );
    assert!(
        text.contains("已发送为文件消息"),
        "the card shows the tracking line: {text}"
    );
    let sends = file_messages(&platform).await;
    assert_eq!(sends.len(), 1, "exactly one File Message: {sends:?}");
    let recorded = uploads(&platform).await;
    assert_eq!(recorded.len(), 2, "the binary was uploaded: {recorded:?}");
    assert!(
        matches!(recorded[1], PlatformCall::UploadFile { .. }),
        "{recorded:?}"
    );
}

/// Re-opening the same image uploads it once: the process-local content-hash
/// cache reuses the `image_key` (ADR-0083 §5, ADR-0076).
#[tokio::test]
async fn an_image_opened_twice_uploads_once() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("shot.png"), png()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    for message in ["m_one", "m_two"] {
        send_command_in(
            &app,
            "/cat shot.png",
            key.clone(),
            message,
            crate::config::ConversationKind::P2p,
        )
        .await;
    }

    assert_eq!(
        platform
            .upload_image_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the same bytes upload once"
    );
    for card in platform.replied_cards().await {
        assert!(card_text(&card).contains("已内嵌"), "{}", card_text(&card));
    }
}

/// A text file past the preview read cap is NOT previewed, but it is still
/// TEXT: it is named 「文本（过大，未预览）」 — never 二进制 — and takes the File
/// Message lane (ADR-0083 §5, §6; finding #739 §5). Its bytes never reach the
/// card.
#[tokio::test]
async fn a_text_file_past_the_preview_cap_is_text_too_large_not_binary() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let big = vec![b'a'; crate::bridge::viewer::PREVIEW_READ_CAP + 1];
    std::fs::write(proj.path().join("huge.log"), &big).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat huge.log",
        key.clone(),
        "m_huge",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("文本（过大，未预览）"),
        "a big UTF-8 file is named as text too large to preview: {text}"
    );
    assert!(!text.contains("二进制"), "it must never be called 二进制: {text}");
    assert!(text.contains("已发送为文件消息"), "{text}");
    assert!(!text.contains("```"), "the bytes are not previewed: {text}");
    assert_eq!(file_messages(&platform).await.len(), 1, "one File Message");
}

/// A binary past the preview read cap stays 二进制 — the label follows the
/// content, not the size (finding #739 §5's other side).
#[tokio::test]
async fn a_binary_past_the_preview_cap_stays_binary() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let mut blob = vec![0xffu8; crate::bridge::viewer::PREVIEW_READ_CAP + 1];
    // A NUL in the header means not text, whatever the size.
    blob[0] = 0;
    std::fs::write(proj.path().join("huge.bin"), &blob).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat huge.bin",
        key.clone(),
        "m_huge_bin",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("二进制（.bin）"), "{text}");
    assert!(!text.contains("文本（过大"), "{text}");
    assert!(text.contains("已发送为文件消息"), "{text}");
}

/// A file past Feishu's 30MB cap leaves only the 未发送 line — no upload, no
/// message (ADR-0083 §5); exactly at the cap stays deliverable.
#[tokio::test]
async fn a_file_past_the_upload_cap_leaves_unsent() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let over = proj.path().join("over.bin");
    std::fs::File::create(&over)
        .unwrap()
        .set_len(crate::feishu::file::MAX_FILE_BYTES + 1)
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat over.bin",
        key.clone(),
        "m_over",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("未发送"), "past the cap: {text}");
    assert!(
        uploads(&platform).await.is_empty(),
        "nothing is uploaded past the cap"
    );
    assert!(
        file_messages(&platform).await.is_empty(),
        "no File Message past the cap"
    );

    // Exactly at the cap: deliverable (the other side of the same boundary).
    let at = proj.path().join("at_cap.bin");
    std::fs::File::create(&at)
        .unwrap()
        .set_len(crate::feishu::file::MAX_FILE_BYTES)
        .unwrap();
    send_command_in(
        &app,
        "/cat at_cap.bin",
        key.clone(),
        "m_at",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = platform.replied_cards().await.last().cloned().expect("a card");
    assert!(
        card_text(&card).contains("已发送为文件消息"),
        "the boundary stays deliverable: {}",
        card_text(&card)
    );
    assert_eq!(file_messages(&platform).await.len(), 1, "one File Message");
}

/// 「发为文件」 posts exactly one File Message carrying the file's bytes, and the
/// click itself does not carry the upload: the ack returns while the upload is
/// still parked (ADR-0083 §5 — the transfer is off the card callback's 3 s
/// budget).
#[tokio::test]
async fn send_as_file_transfers_off_the_callback_path() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("notes.txt"), "# notes\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat notes.txt",
        key.clone(),
        "m_view",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let mut payload = send_as_file_payload(&card);
    payload["open_message_id"] = serde_json::json!("msg_view_card");

    // Park the transfer's upload: the click must still ack immediately, and the
    // ack must carry no upload work.
    let (_entered, release) = platform.pause("upload_file", "");
    let result = app.host_action(payload).await.expect("the click acks");
    assert!(
        result.card.is_none(),
        "the click does not rebuild the card inline"
    );
    assert!(result.toast.is_some(), "the click acknowledges with a toast");
    assert!(
        uploads(&platform).await.is_empty(),
        "the click returned before the upload ran"
    );

    // Release the upload; the spawned transfer lands and PATCHes the card.
    release.notify_one();
    wait_for_card_update(
        &platform,
        "已发送为文件消息",
        CardUpdates::Latest,
        |card| card_text(card).contains("已发送为文件消息"),
    )
    .await;
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "exactly one File Message"
    );
    assert_eq!(
        platform
            .calls
            .lock()
            .await
            .iter()
            .filter(|c| matches!(c, PlatformCall::UploadFile { .. }))
            .count(),
        1,
        "uploaded once"
    );
    let patch = patches_to(&platform, "msg_view_card").await;
    let updated = patch.last().expect("the card was PATCHed");
    assert!(
        card_text(updated).contains(&canonical(&proj.path().join("notes.txt"))),
        "the PATCH keeps the card's own naming: {}",
        card_text(updated)
    );
}

/// The 「发为文件」 transfer uploads ONE byte snapshot: a text file replaced by a
/// PDF between its classification and the click goes out under the BYTES' own
/// label — `application/pdf`, and the PDF's length — never the stale
/// `text/plain` of the earlier verdict (finding #739, round 3).
///
/// The race itself cannot be driven through a click deterministically (the
/// handler re-classifies on the way in), so the transfer is called with exactly
/// the state a click hands it: a text preview's classification, whose file has
/// since been replaced.
#[tokio::test]
async fn the_transfer_labels_the_bytes_it_uploads() {
    let _wd = test_work_dir();
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("notes.txt");
    let text = b"# notes\n";
    std::fs::write(&path, text).unwrap();
    // What the viewer classified: text, and a preview that keeps no bytes.
    let mut classified =
        crate::bridge::viewer::classify(&path, text.len() as u64).expect("a text file classifies");
    assert_eq!(classified.kind, crate::bridge::viewer::ClassifiedKind::Text);
    assert!(classified.bytes.is_none(), "a text preview holds no bytes");

    // The same path holds other bytes by the time the click lands.
    let pdf = b"%PDF-1.7\nand not text at all\n";
    std::fs::write(&path, pdf).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let lane = crate::bridge::viewer::send_as_file(
        &app.feishu,
        &app.cards_handle(),
        "msg_view_card",
        &path,
        "notes.txt",
        &mut classified,
        app.cards_handle().viewer_transfer_timeout_ms(),
    )
    .await;

    assert_eq!(
        lane.delivery,
        crate::feishu::card::tool_render::FileDelivery::SentAsFile,
        "the transfer posts the file"
    );
    assert_eq!(
        lane.mime, "application/pdf",
        "the lane's label is the bytes it sent, never the stale text/plain"
    );
    assert_eq!(lane.size, pdf.len() as u64, "and so is its size");
    let recorded = uploads(&platform).await;
    assert_eq!(recorded.len(), 1, "exactly one upload: {recorded:?}");
    match &recorded[0] {
        PlatformCall::UploadFile { name, mime, size } => {
            assert_eq!(name, "notes.txt");
            assert_eq!(
                mime, "application/pdf",
                "the uploaded content's label names the uploaded bytes"
            );
            assert_eq!(
                *size,
                pdf.len() as u64,
                "the upload carries the PDF's bytes, not the text's"
            );
        }
        other => panic!("expected a file upload, got {other:?}"),
    }
    // The card the click PATCHes is built from this classification, so the
    // viewer and the File Message name the same bytes.
    assert_eq!(classified.mime, "application/pdf");
    assert_eq!(classified.size, pdf.len() as u64);
    assert!(classified.text.is_none(), "the stale preview text is gone");
}

/// A transfer of a file the viewer already read whole (an image/binary lane)
/// uploads THAT snapshot — no second read of the path (finding #739, round 3):
/// the file is gone by the click and the File Message still carries the
/// classified bytes under the classified label.
#[tokio::test]
async fn the_transfer_sends_the_snapshot_it_classified() {
    let _wd = test_work_dir();
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("report.pdf");
    let payload: Vec<u8> = b"%PDF-1.7\nclassified bytes only\n".to_vec();
    std::fs::write(&path, &payload).unwrap();
    let mut classified =
        crate::bridge::viewer::classify(&path, payload.len() as u64).expect("a PDF classifies");
    assert_eq!(classified.kind, crate::bridge::viewer::ClassifiedKind::File);
    assert_eq!(classified.mime, "application/pdf");

    // Nothing is re-read: a re-read would find no file at all.
    std::fs::remove_file(&path).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let lane = crate::bridge::viewer::send_as_file(
        &app.feishu,
        &app.cards_handle(),
        "msg_view_card",
        &path,
        "report.pdf",
        &mut classified,
        app.cards_handle().viewer_transfer_timeout_ms(),
    )
    .await;

    assert_eq!(
        lane.delivery,
        crate::feishu::card::tool_render::FileDelivery::SentAsFile,
        "the classified snapshot is deliverable without the path"
    );
    assert_eq!(lane.mime, "application/pdf");
    assert_eq!(lane.size, payload.len() as u64);
    match &uploads(&platform).await[..] {
        [PlatformCall::UploadFile { mime, size, .. }] => {
            assert_eq!(mime, "application/pdf");
            assert_eq!(*size, payload.len() as u64);
        }
        other => panic!("expected exactly one file upload, got {other:?}"),
    }
}

/// A failed upload surfaces as 未发送 on the card — a stated outcome, never a
/// dropped result — and posts no File Message (ADR-0083 §5).
#[tokio::test]
async fn a_failed_transfer_upload_surfaces_as_unsent() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("notes.txt"), "# notes\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat notes.txt",
        key.clone(),
        "m_view",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let mut payload = send_as_file_payload(&card);
    payload["open_message_id"] = serde_json::json!("msg_view_card");

    platform
        .fail_upload_file_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    app.host_action(payload).await.expect("the click acks");

    wait_for_card_update(&platform, "未发送", CardUpdates::Latest, |card| {
        card_text(card).contains("未发送")
    })
    .await;
    assert!(
        file_messages(&platform).await.is_empty(),
        "a failed upload posts no File Message"
    );
}

/// A transfer whose upload never answers is timed out off the callback path and
/// surfaces as 未发送 (ADR-0083 §5) — the click itself still returns at once.
#[tokio::test]
async fn a_timed_out_transfer_surfaces_as_unsent() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("notes.txt"), "# notes\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat notes.txt",
        key.clone(),
        "m_view",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let mut payload = send_as_file_payload(&card);
    payload["open_message_id"] = serde_json::json!("msg_view_card");

    // A transfer bound so short the parked upload cannot answer in time.
    app.viewer_transfer_timeout_ms
        .store(1, std::sync::atomic::Ordering::Relaxed);
    let (_entered, _release) = platform.pause("upload_file", "");
    app.host_action(payload).await.expect("the click acks");

    wait_for_card_update(&platform, "未发送 (timeout)", CardUpdates::Latest, |card| {
        card_text(card).contains("未发送")
    })
    .await;
    assert!(
        file_messages(&platform).await.is_empty(),
        "a timed-out upload posts no File Message"
    );
}

/// 「发为文件」 on a viewer past the 30MB cap leaves the 未发送 line and posts
/// nothing — the same cap the open lane applies (ADR-0083 §5).
#[tokio::test]
async fn send_as_file_past_the_cap_leaves_unsent() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let over = proj.path().join("over.bin");
    std::fs::File::create(&over)
        .unwrap()
        .set_len(crate::feishu::file::MAX_FILE_BYTES + 1)
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat over.bin",
        key.clone(),
        "m_view",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    assert!(card_text(&card).contains("未发送"), "the open lane says so");
    let mut payload = send_as_file_payload(&card);
    payload["open_message_id"] = serde_json::json!("msg_view_card");

    app.host_action(payload).await.expect("the click acks");
    wait_for_card_update(&platform, "未发送", CardUpdates::Latest, |card| {
        card_text(card).contains("未发送")
    })
    .await;
    assert!(uploads(&platform).await.is_empty(), "nothing is read or uploaded");
    assert!(file_messages(&platform).await.is_empty(), "no File Message");
}

/// A path that vanished between the viewer and the 「发为文件」 click states the
/// outcome ON THE CARD — 找不到路径, no File Message, no upload — instead of
/// dropping the click's result (finding #739 §4).
#[tokio::test]
async fn a_vanished_transfer_file_states_the_failure_on_the_card() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "# notes\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat notes.txt",
        key.clone(),
        "m_view",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let mut payload = send_as_file_payload(&card);
    payload["open_message_id"] = serde_json::json!("msg_view_card");
    // The RESOLVED path the click carries — what the outcome card must name.
    let resolved = payload["path"].as_str().unwrap().to_string();

    // The file is gone by the time the click lands.
    std::fs::remove_file(&file).unwrap();
    app.host_action(payload).await.expect("the click acks");

    wait_for_card_update(&platform, "找不到路径", CardUpdates::Latest, |card| {
        card_text(card).contains("找不到路径")
    })
    .await;
    let patch = patches_to(&platform, "msg_view_card").await;
    let outcome = patch.last().expect("the card was PATCHed with the outcome");
    assert!(
        card_text(outcome).contains(&resolved),
        "the outcome names the resolved path: {}",
        card_text(outcome)
    );
    assert!(
        uploads(&platform).await.is_empty(),
        "nothing is read or uploaded for a vanished path"
    );
    assert!(
        file_messages(&platform).await.is_empty(),
        "a vanished path posts no File Message"
    );
}

/// A path that became unreadable between the viewer and the click states
/// 无法读取 ON THE CARD — never a separate text reply, never a dropped result
/// (finding #739 §4). A directory where the file was is unreadable at every
/// privilege level, so this holds for a root test runner too.
#[tokio::test]
async fn an_unreadable_transfer_file_states_the_failure_on_the_card() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "# notes\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat notes.txt",
        key.clone(),
        "m_view",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let mut payload = send_as_file_payload(&card);
    payload["open_message_id"] = serde_json::json!("msg_view_card");

    // The file is now a directory: reading it fails at every privilege level.
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    app.host_action(payload).await.expect("the click acks");

    wait_for_card_update(&platform, "无法读取", CardUpdates::Latest, |card| {
        card_text(card).contains("无法读取")
    })
    .await;
    assert!(
        platform.texts().await.is_empty(),
        "the outcome lands on the card, not as a separate text reply: {:?}",
        platform.texts().await
    );
    assert!(uploads(&platform).await.is_empty(), "nothing is uploaded");
    assert!(file_messages(&platform).await.is_empty(), "no File Message");
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
        text.contains("隐藏点开头的项"),
        "and its hidden toggle preserved: {text}"
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "the ack patches in place"
    );
}

/// A text preview longer than one card holds pages by **lines** inside the
/// viewer (ADR-0083 §6): the first screen shows a window of lines plus the
/// shared pager, the header still names the file, and the pager reports the
/// preview's TRUE line total, so a reader knows what they are not seeing.
#[tokio::test]
async fn a_long_text_file_pages_inside_the_viewer() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("long.txt"), numbered_lines(3000)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat long.txt",
        key.clone(),
        "m_long",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert_eq!(
        card_header(&card),
        "📄 long.txt",
        "the header still names the file"
    );
    assert!(
        text.contains("line 0001"),
        "the first window opens the file: {text}"
    );
    assert!(
        !text.contains("line 3000"),
        "the last line belongs to a later page: {text}"
    );
    assert_eq!(
        pager_label(&card).as_deref(),
        Some("第 1/2 页 · 共 3000 个"),
        "the pager reports the preview's true line total: {text}"
    );

    // Flipping renders the NEXT window: the last line arrives, the first is
    // gone, and the pager names the page it is on.
    let next = pager_button(&card, "下一页")["value"].clone();
    let page2 = app
        .host_action(next)
        .await
        .expect("the flip rebuilds the card")
        .card
        .expect("a card");
    let text = card_text(&page2);
    assert!(text.contains("line 3000"), "the last window: {text}");
    assert!(
        !text.contains("line 0001"),
        "the first window's lines are behind us: {text}"
    );
    assert_eq!(
        pager_label(&page2).as_deref(),
        Some("第 2/2 页 · 共 3000 个"),
        "{text}"
    );
    assert!(
        pager_button(&page2, "下一页")["disabled"]
            .as_bool()
            .unwrap_or(false),
        "the last page cannot flip forward: {text}"
    );
}

/// Page state round-trips through the button payload (ADR-0052): replaying a
/// flipped page's own pager value rebuilds the SAME window — the viewer keeps
/// no per-card state, so the payload is the whole state.
#[tokio::test]
async fn viewer_pager_rebuilds_the_same_window_from_its_payload() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("long.txt"), numbered_lines(3000)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat long.txt",
        key.clone(),
        "m_replay",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let first_text = card_text(&card);

    // Replaying the SAME flip value renders the identical card.
    let next = pager_button(&card, "下一页")["value"].clone();
    let page2 = app
        .host_action(next.clone())
        .await
        .expect("the flip rebuilds the card")
        .card
        .expect("a card");
    let page2_again = app
        .host_action(next)
        .await
        .expect("the replay rebuilds the card")
        .card
        .expect("a card");
    assert_eq!(
        page2_again, page2,
        "a rebuild from the payload renders the same window"
    );

    // And the way back lands exactly on the first window.
    let prev = pager_button(&page2, "上一页")["value"].clone();
    let back = app
        .host_action(prev)
        .await
        .expect("the flip back rebuilds the card")
        .card
        .expect("a card");
    assert_eq!(
        card_text(&back),
        first_text,
        "flipping back restores the first window"
    );
    assert_eq!(
        pager_label(&back).as_deref(),
        Some("第 1/2 页 · 共 3000 个"),
        "{}",
        card_text(&back)
    );
}

/// A preview that fits one card renders NO pager — no 上一页/下一页 buttons,
/// no page line (ADR-0083 §6).
#[tokio::test]
async fn a_short_text_file_renders_no_pager() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("short.txt"), "hello\nworld\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat short.txt",
        key.clone(),
        "m_short",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("hello") && text.contains("world"),
        "the whole preview is on the one card: {text}"
    );
    assert_eq!(
        pager_label(&card),
        None,
        "a fitting preview has no page line: {text}"
    );
    assert!(
        card_buttons(&card)
            .into_iter()
            .all(|b| b["value"]["op"] != "page"),
        "a fitting preview has no pager buttons: {text}"
    );
}

/// Flipping a viewer page keeps the way back: the pager carries the browse
/// listing's whole state (`return_*`), so the rebuilt page still offers
/// 「← 返回目录」 with that listing intact (ADR-0083 §3, ADR-0052).
#[tokio::test]
async fn flipping_a_viewer_page_keeps_the_way_back_to_the_listing() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("long.txt");
    std::fs::write(&file, numbered_lines(3000)).unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let file_str = file.to_string_lossy().to_string();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    // Open the viewer the way a browse file row does: no op, the listing's
    // whole state in dir/show_hidden/keyword/page.
    let card = app
        .host_action(serde_json::json!({
            "action": "file",
            "path": file_str,
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "dir": proj_dir,
            "show_hidden": true,
            "keyword": "long",
            "page": 1,
        }))
        .await
        .expect("the row opens the viewer")
        .card
        .expect("a card");
    let back = card_buttons(&card)
        .into_iter()
        .find(|b| b["text"]["content"] == "← 返回目录")
        .expect("the row-opened viewer offers the way back");
    assert_eq!(back["value"]["op"], "restore");
    assert_eq!(back["value"]["keyword"], "long");

    // The pager carries the listing back as `return_*` — strings only, because
    // the shared pager holds string callback fields.
    let next = pager_button(&card, "下一页")["value"].clone();
    assert_eq!(next["op"], "page");
    assert_eq!(next["path"], file_str);
    assert_eq!(next["return_dir"], proj_dir);
    assert_eq!(next["return_show_hidden"], "true");
    assert_eq!(next["return_keyword"], "long");
    assert_eq!(next["return_page"], "1");

    // The flipped page still offers the same way back.
    let page2 = app
        .host_action(next)
        .await
        .expect("the flip rebuilds the card")
        .card
        .expect("a card");
    let back2 = card_buttons(&page2)
        .into_iter()
        .find(|b| b["text"]["content"] == "← 返回目录")
        .expect("the flipped page keeps the way back");
    assert_eq!(
        back2["value"], back["value"],
        "the listing state survives the flip"
    );
    assert_eq!(
        pager_label(&page2).as_deref(),
        Some("第 2/2 页 · 共 3000 个"),
        "{}",
        card_text(&page2)
    );
}

/// A Tool Panel's 「📂 打开」 click (ADR-0083 §8, #743): the payload carries the
/// Session, not the Chat — the click resolves the Session's Chat/Topic from
/// the SessionStore and its (possibly relative) structured path against the
/// Session's directory, then opens the File Viewer in place. No `op` rides
/// along, so the click routes to the opener, never to #739's transfer.
#[tokio::test]
async fn tool_panel_open_button_opens_the_viewer_through_the_session() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let body = "# report\n\nwritten by the agent\n";
    std::fs::write(proj.path().join("report.md"), body).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    // The payload the button carries: the Session plus the call's structured
    // path, spelled relative exactly as the tool recorded it.
    let payload = crate::feishu::card::session::tool_file_value("ses_seed", "report.md");
    assert!(
        payload.get("op").is_none(),
        "the button never transfers: {payload}"
    );
    let result = app.host_action(payload).await.expect("the click is handled");
    let opened = result.card.expect("the viewer opens in place");
    assert_eq!(card_header(&opened), "📄 report.md", "the viewer names the file");
    let text = card_text(&opened);
    assert!(
        text.contains(&canonical(&proj.path().join("report.md"))),
        "a relative tool path resolves against the Session's directory: {text}"
    );
    assert!(
        text.contains("written by the agent"),
        "the file's own bytes: {text}"
    );
    assert!(
        !text.contains("返回目录"),
        "no listing to return to, so no return control: {text}"
    );
    let buttons = card_buttons(&opened);
    assert!(
        buttons.iter().any(|b| b["text"]["content"] == "📂 所在目录"),
        "the viewer opens with working controls: {opened}"
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "the ack patches in place"
    );
}

/// A Tool Panel's 「📂 打开」 for an absolute path outside the Session's
/// directory still opens (the Path Scope passes absolute paths through,
/// ADR-0083 §4).
#[tokio::test]
async fn tool_panel_open_button_passes_absolute_paths_through() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("notes.md"), "elsewhere\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    let absolute = elsewhere.path().join("notes.md").to_string_lossy().to_string();
    let payload = crate::feishu::card::session::tool_file_value("ses_seed", &absolute);
    let result = app.host_action(payload).await.expect("the click is handled");
    let opened = result.card.expect("the viewer opens in place");
    assert_eq!(card_header(&opened), "📄 notes.md");
    assert!(
        card_text(&opened).contains(&canonical(&elsewhere.path().join("notes.md"))),
        "the absolute path opens as-is: {opened}"
    );
}

/// A Tool Panel's 「📂 打开」 for a Session the store no longer knows answers
/// honestly (a Toast, the card left standing) rather than a viewer whose
/// controls route nowhere (#743: no broken buttons).
#[tokio::test]
async fn tool_panel_open_button_with_an_unknown_session_answers_honestly() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    let payload = crate::feishu::card::session::tool_file_value("ses_gone", "report.md");
    let result = app.host_action(payload).await.expect("the click is handled");
    assert!(result.card.is_none(), "no viewer with dead controls");
    let toast = result.toast.expect("an honest Toast");
    assert!(toast.contains("report.md"), "the Toast names the path: {toast}");
}

/// Every Rich Text Message `platform` posted (`msg_type:"post"`).
async fn rich_text_messages(platform: &RecordingPlatform) -> Vec<PlatformCall> {
    platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| matches!(call, PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "post"))
        .cloned()
        .collect()
}

fn reread_payload(card: &serde_json::Value) -> serde_json::Value {
    card_buttons(card)
        .into_iter()
        .find(|button| button["text"]["content"] == "🔄 重新读取")
        .unwrap_or_else(|| panic!("viewer has no refresh button: {}", card_text(card)))["value"]
        .clone()
}

async fn open_overflow_csv_viewer() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
    Arc<App>,
    Arc<RecordingPlatform>,
    std::path::PathBuf,
    serde_json::Value,
) {
    let work_dir = test_work_dir();
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("people.csv");
    let csv = format!(
        "name,age\n{}",
        (0..60)
            .map(|i| format!("r{i},{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(&file, csv).unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &project.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat people.csv",
        key,
        "m_csv",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    (work_dir, project, state, app, platform, file, card)
}

/// #741: a preview the card structurally cannot hold — a table past the
/// 49-row cap — goes out as exactly one Rich Text Message with the content
/// intact, while the card still renders with a tracking line saying so.
/// Nothing is truncated: the card carries no preview and no cut note.
#[tokio::test]
async fn structural_overflow_goes_out_as_one_rich_text_message() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = format!(
        "name,age\n{}",
        (0..60)
            .map(|i| format!("r{i},{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(proj.path().join("people.csv"), &csv).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat people.csv",
        key.clone(),
        "m_csv",
        crate::config::ConversationKind::P2p,
    )
    .await;

    // Exactly one Rich Text Message, with the content intact.
    let posts = rich_text_messages(&platform).await;
    assert_eq!(posts.len(), 1, "exactly one Rich Text Message: {posts:?}");
    match &posts[0] {
        PlatformCall::SendMessageReply {
            message_id,
            msg_type,
            content,
        } => {
            assert_eq!(message_id, "m_csv", "the reply targets the card's own command");
            assert_eq!(msg_type, "post");
            assert_eq!(content["zh_cn"]["content"][0][0]["tag"], "md");
            let md = content["zh_cn"]["content"][0][0]["text"]
                .as_str()
                .expect("the md text");
            assert!(md.contains("| name | age |"), "the header: {md}");
            assert!(md.contains("| r59 | 59 |"), "every row, none cut: {md}");
        }
        other => panic!("expected a post reply, got {other:?}"),
    }
    assert!(
        file_messages(&platform).await.is_empty(),
        "no File Message: the post cap holds this table"
    );

    // The card still renders — naming, the tracking line, the buttons — with
    // no preview and no truncation note.
    let card = first_card(&platform).await;
    assert_eq!(card_header(&card), "📄 people.csv");
    let text = card_text(&card);
    assert!(
        text.contains(&canonical(&proj.path().join("people.csv"))),
        "the resolved path is named: {text}"
    );
    assert!(text.contains("表格（.csv）"), "the type is named: {text}");
    assert!(
        text.contains("已发送为富文本消息"),
        "the tracking line says where the content went: {text}"
    );
    assert!(
        !text.contains("表格过长") && !text.contains("已截断"),
        "nothing is cut, so no cut is stated: {text}"
    );
    assert!(
        !text.contains("| r0 |"),
        "the preview lives in the message, not on the card: {text}"
    );
    assert!(
        !card_buttons(&card)
            .iter()
            .any(|b| b["text"]["content"] == "发为文件"),
        "a landed preview offers no second delivery: {text}"
    );
}

/// ADR-0083 §9: a deliberate re-read shows changed bytes on the same viewer
/// card; the callback keeps the file path and card identity in its payload.
#[tokio::test]
async fn reread_refreshes_the_viewer_card_in_place() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "before refresh\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat notes.txt",
        key,
        "m_cat",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    let payload = reread_payload(&original);
    assert_eq!(payload["op"], "reread");
    assert_eq!(payload["path"], canonical(&file));
    assert!(
        payload.get("open_message_id").is_none(),
        "Feishu supplies the containing message id as callback context, not card JSON"
    );

    std::fs::write(&file, "after refresh\n").unwrap();
    let result = app
        .host_action_on_message(payload, "msg_reply")
        .await
        .expect("refresh click is handled");
    assert!(result.card.is_none(), "the callback leaves work off its ack path");
    wait_for_card_update(&platform, "refreshed file contents", CardUpdates::Any, |card| {
        card_text(card).contains("after refresh")
    })
    .await;
    let updates = platform.updated_cards().await;
    let refreshed = updates.last().expect("a refresh patches the viewer");
    assert_eq!(card_header(refreshed), "📄 notes.txt");
    assert!(card_text(refreshed).contains("after refresh"));
    assert!(!card_text(refreshed).contains("before refresh"));
}

/// A disk change alone is inert: only a user click rebuilds the standalone
/// viewer card (there is no filesystem watcher or automatic refresh).
#[tokio::test]
async fn changed_file_does_not_refresh_the_viewer_without_a_click() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "before\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat notes.txt",
        key,
        "m_cat",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    assert_eq!(reread_payload(&original)["op"], "reread");
    std::fs::write(&file, "changed without a click\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(first_card(&platform).await, original);
    assert!(platform.updated_cards().await.is_empty());
    assert!(rich_text_messages(&platform).await.is_empty());
}

/// A failed local re-read is answered under the viewer after the immediate
/// callback ack and never replaces the last good viewer card.
#[tokio::test]
async fn failed_reread_keeps_the_viewer_card_and_answers_with_the_failure() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "still here\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat notes.txt",
        key,
        "m_cat",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    let payload = reread_payload(&original);
    std::fs::remove_file(&file).unwrap();

    let result = app
        .host_action_on_message(payload, "msg_reply")
        .await
        .expect("refresh click is handled");
    assert!(result.card.is_none(), "a failure leaves the existing card intact");
    assert!(
        result.toast.unwrap().contains("重新读取"),
        "the callback acks first"
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let answered = platform.calls.lock().await.iter().any(|call| {
                matches!(call, PlatformCall::ReplyText { message_id, text }
                    if message_id == "msg_reply" && text.contains("找不到路径"))
            });
            if answered {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the off-thread failure reply arrives");
    assert_eq!(first_card(&platform).await, original);
    assert!(platform.updated_cards().await.is_empty());
}

/// A FIFO stalls the read until a writer arrives. The click still returns its
/// ack before the read finishes, proving filesystem I/O is off the callback.
#[cfg(unix)]
#[tokio::test]
async fn reread_ack_is_not_blocked_by_a_stalled_filesystem_read() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "initial\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat notes.txt",
        key,
        "m_cat",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    let payload = reread_payload(&original);
    std::fs::remove_file(&file).unwrap();
    let status = std::process::Command::new("mkfifo").arg(&file).status().unwrap();
    assert!(status.success(), "create a FIFO to park the file read");

    let writer_path = file.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let mut fifo = std::fs::OpenOptions::new().write(true).open(writer_path).unwrap();
        std::io::Write::write_all(&mut fifo, b"read completed after the ack\n").unwrap();
    });
    let started = std::time::Instant::now();
    let result = app
        .host_action_on_message(payload, "msg_reply")
        .await
        .expect("refresh click is handled");
    assert!(result.toast.unwrap().contains("重新读取"));
    assert!(
        started.elapsed() < std::time::Duration::from_millis(700),
        "callback ack waited for the FIFO read: {:?}",
        started.elapsed()
    );
    wait_for_card_update(&platform, "FIFO read result", CardUpdates::Any, |card| {
        card_text(card).contains("read completed after the ack")
    })
    .await;
    writer.join().unwrap();
}

/// An overflow re-read edits the Rich Text Message at the id in the viewer
/// payload, then patches the viewer card with the new file snapshot.
#[tokio::test]
async fn reread_edits_the_same_overflow_message_and_updates_the_card() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = |rows: usize| {
        format!(
            "name,age\n{}",
            (0..rows)
                .map(|i| format!("r{i},{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let file = proj.path().join("people.csv");
    std::fs::write(&file, csv(60)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat people.csv",
        key,
        "m_csv",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    let payload = reread_payload(&original);
    assert_eq!(payload["rich_message_id"], "msg_file");
    std::fs::write(&file, csv(61)).unwrap();

    let result = app
        .host_action_on_message(payload, "msg_reply")
        .await
        .expect("refresh click is handled");
    assert!(result.card.is_none());
    wait_for_card_update(&platform, "new overflow tracking id", CardUpdates::Any, |card| {
        card_buttons(card)
            .iter()
            .any(|button| button["value"]["rich_message_id"] == "msg_file")
    })
    .await;
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|call| matches!(call, PlatformCall::EditMessage {
            message_id, msg_type, content, outcome: crate::feishu::MessageEditOutcome::Updated,
            ..
        } if message_id == "msg_file" && msg_type == "post" && content.to_string().contains("r60"))),
        "the original Rich Text Message is edited with the new row: {calls:?}"
    );
    let updated = platform.updated_cards().await;
    let card = updated.last().expect("the same viewer card is patched");
    assert_eq!(card_header(card), "📄 people.csv");
    assert_eq!(
        card_buttons(card)
            .into_iter()
            .find(|button| button["text"]["content"] == "🔄 重新读取")
            .unwrap()["value"]["rich_message_id"],
        "msg_file"
    );
}

/// When an overflowing preview becomes small enough for the card, the old rich
/// message is retired with a notice and the current content appears only on the
/// viewer card.
#[tokio::test]
async fn reread_from_overflow_to_fitting_retires_rich_copy() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = |rows: usize| {
        format!(
            "name,age\n{}",
            (0..rows)
                .map(|i| format!("r{i},{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let file = proj.path().join("people.csv");
    std::fs::write(&file, csv(60)).unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat people.csv",
        key,
        "m_overflow",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    let rich_id = reread_payload(&original)["rich_message_id"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(&file, csv(2)).unwrap();

    app.host_action_on_message(reread_payload(&original), "msg_reply")
        .await
        .expect("the re-read acknowledges");
    wait_for_card_update(&platform, "fitting refresh", CardUpdates::Latest, |card| {
        card_text(card).contains("| r1 | 1 |")
    })
    .await;

    let updated = platform.updated_cards().await.pop().expect("viewer patch");
    let text = card_text(&updated);
    assert_eq!(
        text.matches("| r0 | 0 |").count(),
        1,
        "current content occurs once: {text}"
    );
    assert!(
        text.contains("| r1 | 1 |"),
        "the current fitting table is on the card: {text}"
    );
    assert!(
        !card_buttons(&updated)
            .iter()
            .any(|button| button["value"]["rich_message_id"] == rich_id),
        "the retired rich message is no longer tracked"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|call| matches!(call, PlatformCall::EditMessage {
            message_id, msg_type, content,
            outcome: crate::feishu::MessageEditOutcome::Updated,
            ..
        } if message_id == &rich_id && msg_type == "post"
            && content.to_string().contains("最新内容以文件查看卡为准")
            && !content.to_string().contains("| r0 |"))),
        "the old rich copy is replaced with a retirement notice: {calls:?}"
    );
}

/// An exhausted edit budget cannot retire the old rich body. Keep its id and
/// the current fitting preview on the card, but never send an untracked notice.
#[tokio::test]
async fn fitting_reread_edit_limit_does_not_send_an_untracked_notice() {
    let (_work_dir, _project, _state, app, platform, file, original) = open_overflow_csv_viewer().await;
    std::fs::write(&file, "name,age\nann,3\n").unwrap();
    platform
        .refuse_edit_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    app.host_action_on_message(reread_payload(&original), "msg_reply")
        .await
        .expect("the fitting re-read acknowledges");
    wait_for_card_update(&platform, "unretired rich copy", CardUpdates::Latest, |card| {
        card_text(card).contains("| ann | 3 |") && card_text(card).contains("旧消息仍在 `msg_file`")
    })
    .await;

    let card = platform.updated_cards().await.pop().expect("viewer card patch");
    assert_eq!(reread_payload(&card)["rich_message_id"], "msg_file");
    assert_eq!(
        rich_text_messages(&platform).await.len(),
        1,
        "only the original overflow message exists; no fallback notice is sent"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(calls.iter().any(|call| matches!(call, PlatformCall::EditMessage {
        message_id,
        outcome: crate::feishu::MessageEditOutcome::EditLimitReached,
        ..
    } if message_id == "msg_file")));
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, PlatformCall::SendMessageReply {
            msg_type, content, ..
        } if msg_type == "post" && content.to_string().contains("最新内容以文件查看卡为准"))),
        "an edit-limit refusal does not create an untracked retirement notice"
    );
}

/// A fitting preview that grows past the structural limit moves to one rich
/// message, and the card retains only its tracking line.
#[tokio::test]
async fn reread_from_fitting_to_overflow_sends_and_tracks_rich_copy() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = |rows: usize| {
        format!(
            "name,age\n{}",
            (0..rows)
                .map(|i| format!("r{i},{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let file = proj.path().join("people.csv");
    std::fs::write(&file, csv(2)).unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat people.csv",
        key,
        "m_fitting",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    assert!(reread_payload(&original).get("rich_message_id").is_none());
    std::fs::write(&file, csv(60)).unwrap();

    app.host_action_on_message(reread_payload(&original), "msg_reply")
        .await
        .expect("the re-read acknowledges");
    wait_for_card_update(&platform, "overflow refresh", CardUpdates::Latest, |card| {
        card_text(card).contains("已发送为富文本消息")
    })
    .await;

    assert_eq!(
        rich_text_messages(&platform).await.len(),
        1,
        "one fresh rich message"
    );
    let updated = platform.updated_cards().await.pop().expect("viewer patch");
    let text = card_text(&updated);
    assert!(
        !text.contains("| r0 |"),
        "full content is not duplicated on the card: {text}"
    );
    assert!(
        card_buttons(&updated).iter().any(|button| {
            button["text"]["content"] == "🔄 重新读取"
                && button["value"]["rich_message_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
        }),
        "the card tracks the rich message for the next re-read: {text}"
    );
}

/// A definite edit refusal causes a fresh Rich Text Message send, and the
/// refreshed card tracks that new id instead of claiming the rejected edit.
#[tokio::test]
async fn refused_rich_message_edit_resends_and_tracks_the_fresh_message() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = |rows: usize| {
        format!(
            "name,age\n{}",
            (0..rows)
                .map(|i| format!("r{i},{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let file = proj.path().join("people.csv");
    std::fs::write(&file, csv(60)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat people.csv",
        key,
        "m_csv",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    let payload = reread_payload(&original);
    std::fs::write(&file, csv(61)).unwrap();
    platform
        .refuse_edit_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    platform.given_message_reply_id("msg_fresh");

    app.host_action_on_message(payload, "msg_reply")
        .await
        .expect("refresh click is handled");
    wait_for_card_update(
        &platform,
        "fresh rich-message tracking",
        CardUpdates::Any,
        |card| {
            card_buttons(card)
                .iter()
                .any(|button| button["value"]["rich_message_id"] == "msg_fresh")
        },
    )
    .await;
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|call| matches!(call, PlatformCall::EditMessage {
            message_id,
            outcome: crate::feishu::MessageEditOutcome::EditLimitReached,
            business_code: Some(230072),
            ..
        } if message_id == "msg_file")),
        "the original edit is explicitly refused: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, PlatformCall::SendMessageReply {
            msg_type, content, ..
        } if msg_type == "post" && content.to_string().contains("r60"))),
        "the refreshed content is sent as a fresh Rich Text Message: {calls:?}"
    );
    let card = platform.updated_cards().await.pop().expect("viewer card update");
    assert!(
        card_text(&card).contains("当前消息 `msg_fresh`"),
        "the visible tracking line identifies the fresh copy as current: {}",
        card_text(&card)
    );
    assert_eq!(
        card_buttons(&card)
            .into_iter()
            .find(|button| button["text"]["content"] == "🔄 重新读取")
            .unwrap()["value"]["rich_message_id"],
        "msg_fresh"
    );
}

/// A business refusal other than the edit-budget ceiling is not a license to
/// duplicate the overflow. The card keeps the old id and names it as current.
#[tokio::test]
async fn frequency_limit_230020_does_not_resend_rich_message() {
    let (_work_dir, _project, _state, app, platform, file, original) = open_overflow_csv_viewer().await;
    std::fs::write(
        &file,
        format!(
            "name,age\n{}",
            (0..61)
                .map(|i| format!("r{i},{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    )
    .unwrap();
    platform
        .reject_edit_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    let result = app
        .host_action_on_message(reread_payload(&original), "msg_reply")
        .await
        .expect("refresh click is handled");
    assert!(result.toast.unwrap().contains("重新读取"));
    wait_for_card_update(&platform, "honest edit refusal", CardUpdates::Any, |card| {
        card_text(card).contains("富文本刷新失败")
    })
    .await;
    let calls = platform.calls.lock().await.clone();
    assert!(calls.iter().any(|call| matches!(call, PlatformCall::EditMessage {
        message_id,
        outcome: crate::feishu::MessageEditOutcome::Refused,
        business_code: Some(230020),
        ..
    } if message_id == "msg_file")));
    assert_eq!(
        rich_text_messages(&platform).await.len(),
        1,
        "no duplicate message is sent"
    );
    let card = platform.updated_cards().await.pop().unwrap();
    assert!(card_text(&card).contains("旧消息仍在 `msg_file`"));
    assert_eq!(reread_payload(&card)["rich_message_id"], "msg_file");
}

/// An ambiguous transport failure is not treated as an edit-budget refusal:
/// no fresh message is sent, and the card keeps the carried message id.
#[tokio::test]
async fn ambiguous_rich_message_edit_does_not_resend_and_preserves_id() {
    let (_work_dir, _project, _state, app, platform, file, original) = open_overflow_csv_viewer().await;
    std::fs::write(
        &file,
        format!(
            "name,age\n{}",
            (0..61)
                .map(|i| format!("r{i},{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    )
    .unwrap();
    platform
        .fail_edit_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    app.host_action_on_message(reread_payload(&original), "msg_reply")
        .await
        .expect("refresh click is handled");
    wait_for_card_update(&platform, "ambiguous edit failure", CardUpdates::Any, |card| {
        card_text(card).contains("富文本刷新失败")
    })
    .await;
    let calls = platform.calls.lock().await.clone();
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, PlatformCall::EditMessageFailed {
        message_id, ..
    } if message_id == "msg_file"))
    );
    assert_eq!(
        rich_text_messages(&platform).await.len(),
        1,
        "ambiguous edits never duplicate content"
    );
    let card = platform.updated_cards().await.pop().unwrap();
    assert!(card_text(&card).contains("旧消息仍在 `msg_file`"));
    assert_eq!(reread_payload(&card)["rich_message_id"], "msg_file");
}

/// #741: a Rich Text Message send that FAILS drops nothing — the card keeps
/// its truncated preview with the honest cut note and the 「发为文件」 rescue
/// button, and the tracking line states the content did not land (never a
/// claim to a lane that missed).
#[tokio::test]
async fn a_failed_rich_text_send_keeps_the_preview_and_the_rescue() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = format!(
        "name,age\n{}",
        (0..60)
            .map(|i| format!("r{i},{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(proj.path().join("people.csv"), &csv).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    // The Rich Text Message send fails; the card reply still lands.
    platform
        .fail_send_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    send_command_in(
        &app,
        "/cat people.csv",
        key.clone(),
        "m_csv",
        crate::config::ConversationKind::P2p,
    )
    .await;

    assert!(
        rich_text_messages(&platform).await.is_empty(),
        "the failed send posts no Rich Text Message"
    );
    assert!(
        file_messages(&platform).await.is_empty(),
        "and no File Message either: the lane missed, nothing went out"
    );
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("| r0 | 0 |") && text.contains("表格过长"),
        "the truncated preview and its honest cut note stay: {text}"
    );
    assert!(
        text.contains("未发送"),
        "the tracking line states the content did not land: {text}"
    );
    assert!(!text.contains("富文本"), "no claim to a lane that missed: {text}");
    assert!(
        card_buttons(&card)
            .iter()
            .any(|b| b["text"]["content"] == "发为文件"),
        "the rescue button stays when nothing landed: {text}"
    );
}

/// #741: a structurally-overflowing file opened from a browse file row takes
/// the same lane as `/cat` — the click acks at once (a toast, no card), the
/// Rich Text Message goes out under the clicked card off the callback path,
/// and the clicked card is PATCHed with the viewer that says so. Content that
/// fits stays on the synchronous in-place path (pinned by #742's own test).
#[tokio::test]
async fn a_row_open_of_an_overflowing_preview_sends_then_patches() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = format!(
        "name,age\n{}",
        (0..60)
            .map(|i| format!("r{i},{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(proj.path().join("people.csv"), &csv).unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let file = proj.path().join("people.csv").to_string_lossy().to_string();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    // Park the row's send: the click must still ack immediately, and the ack
    // must carry no send work. The payload is what a browse file row carries,
    // plus the clicked card the platform threads in — the send's reply target
    // and the PATCH target.
    let (_entered, release) = platform.pause("send_message", "msg_browse_card");
    let result = app
        .host_action(serde_json::json!({
            "action": "file",
            "path": file,
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "dir": proj_dir,
            "show_hidden": false,
            "keyword": "",
            "page": 1,
            "open_message_id": "msg_browse_card",
        }))
        .await
        .expect("the click acks");
    assert!(result.card.is_none(), "the click does not rebuild inline");
    assert!(result.toast.is_some(), "the click acknowledges with a toast");
    assert!(
        rich_text_messages(&platform).await.is_empty(),
        "the click returned before the send ran"
    );

    // Release the send; the spawned lane lands and PATCHes the clicked card.
    release.notify_one();
    wait_for_card_update(
        &platform,
        "已发送为富文本消息",
        CardUpdates::Latest,
        |card| card_text(card).contains("已发送为富文本消息"),
    )
    .await;
    let posts = rich_text_messages(&platform).await;
    assert_eq!(posts.len(), 1, "exactly one Rich Text Message: {posts:?}");
    match &posts[0] {
        PlatformCall::SendMessageReply {
            message_id,
            msg_type,
            content,
        } => {
            assert_eq!(
                message_id, "msg_browse_card",
                "the reply targets the clicked card"
            );
            assert_eq!(msg_type, "post");
            let md = content["zh_cn"]["content"][0][0]["text"]
                .as_str()
                .expect("the md text");
            assert!(md.contains("| r59 | 59 |"), "every row, none cut: {md}");
        }
        other => panic!("expected a post reply, got {other:?}"),
    }
    assert!(
        file_messages(&platform).await.is_empty(),
        "no File Message: the post cap holds this table"
    );
    let patch = patches_to(&platform, "msg_browse_card").await;
    let updated = patch.last().expect("the clicked card was PATCHed");
    let text = card_text(updated);
    assert_eq!(card_header(updated), "📄 people.csv");
    assert!(
        text.contains("已发送为富文本消息"),
        "the PATCHed card says where the content went: {text}"
    );
    assert!(
        !text.contains("| r0 |"),
        "the preview lives in the message, not on the card: {text}"
    );
    assert!(
        !card_buttons(updated)
            .iter()
            .any(|b| b["text"]["content"] == "发为文件"),
        "a landed preview offers no second delivery: {text}"
    );
    assert!(
        text.contains("← 返回目录"),
        "the way back survives the PATCH: {text}"
    );
}

/// A browse-row open uses the same byte lanes as `/cat`: images are embedded
/// into the patched card, while non-text files get exactly one File Message.
#[tokio::test]
async fn a_row_open_embeds_images_and_sends_binary_once() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let image = proj.path().join("picture.png");
    let binary = proj.path().join("archive.bin");
    std::fs::write(&image, png()).unwrap();
    std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x01]).unwrap();

    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    let result = app
        .host_action_on_message(
            serde_json::json!({
                "action": "file",
                "path": image.to_string_lossy(),
                "chat_id": "chat_1",
                "thread_id": "chat_1",
            }),
            "msg_image_row",
        )
        .await
        .expect("the image row acknowledges");
    assert!(result.card.is_none(), "row opens are acknowledged before I/O");
    wait_for_card_update(&platform, "embedded image viewer", CardUpdates::Latest, |card| {
        card_text(card).contains("已内嵌")
    })
    .await;
    let image_card = platform
        .updated_cards()
        .await
        .pop()
        .expect("patched image viewer");
    assert!(
        image_card.to_string().contains("\"tag\":\"img\""),
        "the image is embedded: {image_card}"
    );
    assert!(
        card_buttons(&image_card)
            .iter()
            .any(|button| button["text"]["content"] == "发为文件"),
        "the embedded image remains visible and keeps the explicit File Message action"
    );
    assert_eq!(uploads(&platform).await.len(), 1, "one image upload");
    assert!(
        file_messages(&platform).await.is_empty(),
        "an embeddable image is not a file message"
    );

    let binary_row = serde_json::json!({
        "action": "file",
        "path": binary.to_string_lossy(),
        "chat_id": "chat_1",
        "thread_id": "chat_1",
    });
    let (entered, release) = platform.pause("send_message", "msg_binary_row");
    let result = app
        .host_action_on_message(binary_row.clone(), "msg_binary_row")
        .await
        .expect("the binary row acknowledges");
    assert!(result.card.is_none());
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the first binary send reaches the paused send call");

    let repeated = app
        .host_action_on_message(binary_row, "msg_binary_row")
        .await
        .expect("a repeated binary row click is acknowledged");
    assert!(repeated.card.is_none());
    release.notify_one();
    wait_for_card_update(&platform, "binary viewer", CardUpdates::Latest, |card| {
        card_text(card).contains("已发送为文件消息")
    })
    .await;
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "the repeated click does not send a second File Message"
    );
    let binary_card = platform
        .updated_cards()
        .await
        .pop()
        .expect("patched binary viewer");
    let text = card_text(&binary_card);
    assert!(text.contains("已发送为文件消息"), "the outcome is stated: {text}");
    assert!(
        !card_buttons(&binary_card)
            .iter()
            .any(|button| button["text"]["content"] == "发为文件"),
        "the delivered bytes cannot be sent a second time"
    );
}

/// Fitting text takes no delivery lane: the row's own card is replaced in
/// place, with no reply, Rich Text Message, or File Message.
#[tokio::test]
async fn a_fitting_row_open_patches_in_place_without_sending() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "only on the viewer card\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    let result = app
        .host_action_on_message(
            serde_json::json!({
                "action": "file",
                "path": file.to_string_lossy(),
                "chat_id": "chat_1",
                "thread_id": "chat_1",
                "dir": proj.path().to_string_lossy(),
                "show_hidden": false,
                "keyword": "",
                "page": 1,
            }),
            "msg_fitting_row",
        )
        .await
        .expect("the fitting row acknowledges");
    assert!(result.card.is_none());
    wait_for_card_update(&platform, "fitting row viewer", CardUpdates::Latest, |card| {
        card_text(card).contains("only on the viewer card")
    })
    .await;
    assert_eq!(
        patches_to(&platform, "msg_fitting_row").await.len(),
        1,
        "the viewer replaces the clicked card in place"
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "no replacement message is sent"
    );
    assert!(
        rich_text_messages(&platform).await.is_empty(),
        "no Rich Text Message"
    );
    assert!(file_messages(&platform).await.is_empty(), "no File Message");
    assert!(uploads(&platform).await.is_empty(), "no upload");
}

/// Classification of a file-row open cannot hold the 3 s card callback open:
/// a FIFO stalls the reader until after the immediate ack.
#[cfg(unix)]
#[tokio::test]
async fn row_open_ack_is_not_blocked_by_a_stalled_filesystem_read() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("stalled.txt");
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    let status = std::process::Command::new("mkfifo").arg(&file).status().unwrap();
    assert!(status.success(), "create a FIFO to park classification");

    let writer_path = file.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        let mut fifo = std::fs::OpenOptions::new().write(true).open(writer_path).unwrap();
        std::io::Write::write_all(&mut fifo, b"read completed after the ack\n").unwrap();
    });
    let started = std::time::Instant::now();
    let result = app
        .host_action_on_message(
            serde_json::json!({
                "action": "file",
                "path": file.to_string_lossy(),
                "chat_id": "chat_1",
                "thread_id": "chat_1",
            }),
            "msg_stalled_row",
        )
        .await
        .expect("the row callback acknowledges");
    assert!(result.card.is_none());
    assert!(result.toast.is_some());
    assert!(
        started.elapsed() < std::time::Duration::from_millis(700),
        "callback ack waited for classification: {:?}",
        started.elapsed()
    );
    wait_for_card_update(&platform, "FIFO row result", CardUpdates::Any, |card| {
        card_text(card).contains("read completed after the ack")
    })
    .await;
    writer.join().unwrap();
}

/// #741: a structurally-overflowing file opened from a Tool Panel's
/// 「📂 打开」 (#743) takes the same lane — the send goes out off the callback
/// path and the clicked card is PATCHed with the landed viewer, which carries
/// no listing to return to and no second delivery.
#[tokio::test]
async fn a_tool_open_of_an_overflowing_preview_sends_then_patches() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = format!(
        "name,age\n{}",
        (0..60)
            .map(|i| format!("r{i},{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(proj.path().join("people.csv"), &csv).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    // The payload the button carries (the Session, never the Chat), plus the
    // clicked card the platform threads in.
    let mut payload = crate::feishu::card::session::tool_file_value("ses_seed", "people.csv");
    payload["open_message_id"] = serde_json::json!("msg_turn_card");
    let result = app.host_action(payload).await.expect("the click acks");
    assert!(result.card.is_none(), "the click does not rebuild inline");
    assert!(result.toast.is_some(), "the click acknowledges with a toast");

    wait_for_card_update(
        &platform,
        "已发送为富文本消息",
        CardUpdates::Latest,
        |card| card_text(card).contains("已发送为富文本消息"),
    )
    .await;
    let posts = rich_text_messages(&platform).await;
    assert_eq!(posts.len(), 1, "exactly one Rich Text Message: {posts:?}");
    match &posts[0] {
        PlatformCall::SendMessageReply { message_id, .. } => {
            assert_eq!(message_id, "msg_turn_card", "the reply targets the clicked card");
        }
        other => panic!("expected a post reply, got {other:?}"),
    }
    let patch = patches_to(&platform, "msg_turn_card").await;
    let updated = patch.last().expect("the clicked card was PATCHed");
    let text = card_text(updated);
    assert_eq!(card_header(updated), "📄 people.csv");
    assert!(
        text.contains("已发送为富文本消息"),
        "the PATCHed card says where the content went: {text}"
    );
    assert!(
        !text.contains("返回目录"),
        "no listing to return to, so no return control: {text}"
    );
    assert!(
        !card_buttons(updated)
            .iter()
            .any(|b| b["text"]["content"] == "发为文件"),
        "a landed preview offers no second delivery: {text}"
    );
}

/// #741: content that fits the card's structural limits stays on the card —
/// this lane is not taken for size alone. A large single code block that fits
/// the budget previews in place with no message of any kind.
#[tokio::test]
async fn fitting_content_stays_on_the_card() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("small.csv"), "name,age\nann,3\nbob,7\n").unwrap();
    // ~15 KB: several fenced elements, within one card's budget.
    std::fs::write(proj.path().join("big.rs"), "let x = 1;\n".repeat(1_400)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat small.csv",
        key.clone(),
        "m_small",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("| ann | 3 |"),
        "the table previews on the card: {text}"
    );
    assert!(
        !text.contains("富文本"),
        "no tracking line for content that fits: {text}"
    );

    send_command_in(
        &app,
        "/cat big.rs",
        key.clone(),
        "m_big",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = platform.replied_cards().await.last().cloned().expect("a card");
    let text = card_text(&card);
    assert!(
        text.contains("```rust") && text.contains("let x = 1;"),
        "a large fitting block stays on the card: {text}"
    );

    assert!(
        rich_text_messages(&platform).await.is_empty(),
        "no Rich Text Message for fitting content"
    );
    assert!(
        file_messages(&platform).await.is_empty(),
        "and no File Message either"
    );
}

/// #741: the lane is bounded by the same 30 KB body cap as a card — content
/// whose rich-text body would exceed it goes to the File Message lane, and
/// the card says so.
#[tokio::test]
async fn a_post_body_past_the_cap_goes_to_the_file_lane() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = format!(
        "name,age\n{}",
        (0..4_000)
            .map(|i| format!("r{i},{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(proj.path().join("huge.csv"), &csv).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat huge.csv",
        key.clone(),
        "m_huge",
        crate::config::ConversationKind::P2p,
    )
    .await;

    assert!(
        rich_text_messages(&platform).await.is_empty(),
        "no Rich Text Message past the post cap"
    );
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "exactly one File Message instead"
    );
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("已发送为文件消息"),
        "the card says where the content went: {text}"
    );
    assert!(
        !text.contains("富文本"),
        "and does not claim the lane it did not take: {text}"
    );
}

/// #741: a text preview past the element budget cannot ride a post body
/// either — its bytes dwarf the same 30 KB cap — so it goes to the File
/// Message lane with the card saying so.
#[tokio::test]
async fn element_overflow_text_goes_to_the_file_lane() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    // ~1 MB of short lines: hundreds of fenced elements, past the card's
    // element budget and far past the post cap, within the 8 MB preview cap.
    std::fs::write(proj.path().join("long.log"), "x\n".repeat(500_000)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat long.log",
        key.clone(),
        "m_long",
        crate::config::ConversationKind::P2p,
    )
    .await;

    assert!(
        rich_text_messages(&platform).await.is_empty(),
        "no Rich Text Message past the post cap"
    );
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "exactly one File Message instead"
    );
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert_eq!(card_header(&card), "📄 long.log");
    assert!(
        text.contains("已发送为文件消息"),
        "the card says where the content went: {text}"
    );
}

/// #741: the Rich Text Message lands as a reply in the card's own container —
/// the generic reply path omits `reply_in_thread`, so in a lobby it creates
/// no topic. Pinned here for a group lobby and for a p2p chat alike.
#[tokio::test]
async fn the_rich_text_reply_targets_the_cards_own_container() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let csv = format!(
        "k,v\n{}",
        (0..60)
            .map(|i| format!("a{i},{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(proj.path().join("data.csv"), &csv).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    // A group lobby: top-level group message, no topic.
    let lobby = crate::config::ThreadKey::new("chat_group".into(), "chat_group".into());
    seed_entry(&app, seeded_entry(&lobby, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat data.csv",
        lobby,
        "m_lobby",
        crate::config::ConversationKind::GroupLobby,
    )
    .await;
    // A p2p chat.
    let p2p = thread();
    seed_entry(&app, seeded_entry(&p2p, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat data.csv",
        p2p,
        "m_p2p",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let posts = rich_text_messages(&platform).await;
    assert_eq!(posts.len(), 2, "one Rich Text Message per command: {posts:?}");
    let targets: Vec<String> = posts
        .iter()
        .map(|call| match call {
            PlatformCall::SendMessageReply { message_id, .. } => message_id.clone(),
            other => panic!("expected a post reply, got {other:?}"),
        })
        .collect();
    assert!(
        targets.contains(&"m_lobby".to_string()) && targets.contains(&"m_p2p".to_string()),
        "each reply targets its own card's command: {targets:?}"
    );
}
