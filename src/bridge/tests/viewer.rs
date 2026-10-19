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

/// Drive a viewer rebuild callback — a pager flip or the markdown view toggle
/// (#781) — through its asynchronous card PATCH. If the previous rebuild task is
/// still releasing its per-card claim, retry the same stateless payload until
/// the next patch lands.
async fn drive_viewer_rebuild(
    app: &std::sync::Arc<crate::bridge::App>,
    platform: &RecordingPlatform,
    mut payload: serde_json::Value,
    message_id: &str,
) -> serde_json::Value {
    payload["open_message_id"] = serde_json::json!(message_id);
    let previous_patches = patches_to(platform, message_id).await.len();
    let result = app
        .host_action(payload)
        .await
        .expect("the viewer rebuild callback is acknowledged");
    assert!(result.card.is_none(), "the rebuild stays off-callback");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let patches = patches_to(platform, message_id).await;
            if patches.len() > previous_patches {
                return patches.last().unwrap().clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the viewer page PATCH lands")
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

#[tokio::test]
async fn absolute_file_outside_session_directory_opens_without_a_scope_notice() {
    let _wd = test_work_dir();
    let session_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let file = outside_dir.path().join("notes.md");
    std::fs::write(&file, "private notes\n").unwrap();

    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &session_dir.path().to_string_lossy())).await;

    send_command_in(
        &app,
        &format!("/cat {}", file.display()),
        key,
        "m_outside",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let text = card_text(&first_card(&platform).await);
    assert!(text.contains("notes.md"), "the external file opens: {text}");
    assert!(
        text.contains("private notes"),
        "the file content is shown: {text}"
    );
    // ADR-0083 §4 (amended #771): the privacy statement belongs to the ADR and
    // the user guide, not to a line repeated on every viewer card.
    assert!(
        !text.contains("⚠️"),
        "an outside-session path carries no warning row: {text}"
    );
}

/// A `/cat` FIFO must fail promptly on the command path, without waiting for a
/// writer or pinning the message handler's runtime worker.
#[cfg(unix)]
#[tokio::test]
async fn cat_fifo_answers_without_waiting_for_a_writer() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let fifo = proj.path().join("waiting.pipe");
    let status = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success(), "create test FIFO");

    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        send_command_in(
            &app,
            "/cat waiting.pipe",
            key,
            "m_fifo",
            crate::config::ConversationKind::P2p,
        ),
    )
    .await
    .expect("the FIFO command answers promptly");
    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|text| text.contains("not a regular file")),
        "the special file gets an honest failure, not a hanging or empty viewer: {texts:?}"
    );
    assert!(platform.replied_cards().await.is_empty());
}

#[tokio::test]
async fn reread_without_card_context_names_the_missing_viewer_card() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("notes.txt"), "hello\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat notes.txt",
        key,
        "m_reread_missing_context",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let mut reread = card_buttons(&card)
        .into_iter()
        .find(|button| button["text"]["content"] == "🔄 重新读取")
        .expect("re-read control")
        .clone()["value"]
        .clone();
    reread.as_object_mut().unwrap().remove("open_message_id");

    let result = app.host_action(reread).await.expect("the callback is answered");
    assert!(result.card.is_none());
    assert_eq!(result.toast.as_deref(), Some("无法重新读取：找不到文件查看卡。"));
}

/// Linux procfs files report a zero stat size while returning real bytes: the
/// classifier's emptiness verdict and displayed size must use the bytes read.
#[cfg(target_os = "linux")]
#[test]
fn a_zero_stat_size_pseudo_file_is_classified_from_its_content() {
    let path = std::path::Path::new("/proc/self/status");
    assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
    let classified = crate::bridge::viewer::classify(path, 0).expect("proc status has content");
    assert_eq!(classified.kind, crate::bridge::viewer::ClassifiedKind::Text);
    assert!(classified.size > 0, "the bytes, not stat, determine size");
    assert!(
        classified
            .text
            .as_deref()
            .is_some_and(|text| text.starts_with("Name:")),
        "the pseudo-file contents are retained"
    );
}

/// The shared bounded-reader failure path reports each caller's exact copy and
/// keeps the slot occupied until a timed-out blocking closure really returns.
#[tokio::test]
async fn bounded_viewer_read_failure_copy_and_slot_lifetime_are_pinned() {
    let test_slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));

    let permit = std::sync::Arc::clone(&test_slots)
        .try_acquire_owned()
        .expect("test slot starts free");
    let busy = crate::bridge::handler::bounded_viewer_fs_read(
        std::sync::Arc::clone(&test_slots),
        100,
        "⚠️ 重新读取任务繁忙，请稍后再试。",
        "⚠️ 打开超时，请稍后再试。",
        || 1,
    )
    .await;
    assert_eq!(
        busy,
        Err(crate::bridge::handler::ViewerFsReadError::Busy(
            "⚠️ 重新读取任务繁忙，请稍后再试。".into()
        ))
    );
    let transfer_busy = crate::bridge::handler::bounded_viewer_fs_read(
        std::sync::Arc::clone(&test_slots),
        100,
        "⚠️ 文件读取繁忙，请稍后再试。",
        "⚠️ 打开超时，请稍后再试。",
        || 1,
    )
    .await;
    assert_eq!(
        transfer_busy,
        Err(crate::bridge::handler::ViewerFsReadError::Busy(
            "⚠️ 文件读取繁忙，请稍后再试。".into()
        ))
    );
    drop(permit);

    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let read = tokio::spawn(crate::bridge::handler::bounded_viewer_fs_read(
        std::sync::Arc::clone(&test_slots),
        20,
        "⚠️ 文件读取繁忙，请稍后再试。",
        "⚠️ 打开超时，请稍后再试。",
        move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            2
        },
    ));
    tokio::task::spawn_blocking(move || entered_rx.recv().unwrap())
        .await
        .unwrap();
    assert_eq!(
        read.await.unwrap(),
        Err(crate::bridge::handler::ViewerFsReadError::Timeout(
            "⚠️ 打开超时，请稍后再试。".into()
        ))
    );
    assert_eq!(
        test_slots.available_permits(),
        0,
        "timeout does not release a live worker's slot"
    );
    release_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while test_slots.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the completed blocking task releases its slot");

    let busy = crate::bridge::handler::bounded_viewer_fs_read(
        std::sync::Arc::clone(&test_slots),
        100,
        "⚠️ 文件读取繁忙，请稍后再试。",
        "⚠️ 打开超时，请稍后再试。",
        || 3,
    )
    .await
    .expect("the slot is available again");
    assert_eq!(busy, 3);
}

/// An extension with no honest language token renders as `plain_text` —
/// never a near-miss highlight (ADR-0083 §6; `.log`/`.env` have no token at
/// all). `.ts` is NOT one of them: measured 2026-10-18 on a live card,
/// `typescript` highlights a TypeScript file (the short `ts` does not), so the
/// long token is the one the viewer asks for.
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

    for name in ["build.log", ".env"] {
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
    }

    // The measured TypeScript token, not the documented-but-broken short one.
    send_command_in(
        &app,
        "/cat config.ts",
        key,
        "msg_config.ts",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let cards = platform.replied_cards().await;
    let text = card_text(cards.last().expect("a viewer card"));
    assert!(text.contains("```typescript"), "a .ts preview: {text}");
    assert!(!text.contains("```ts\n"), "never the short token: {text}");
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
        text.contains("```markdown") && text.contains("# notes"),
        "a .md file opens on its source (#781): {text}"
    );
    assert!(
        card_buttons(&card).iter().any(|b| b["value"]["op"] == "view"),
        "the source view offers its render: {text}"
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
    assert!(
        line.contains("a.txt/inside.txt"),
        "and names the failing path: {line}"
    );
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
        line.contains("locked/secret.txt"),
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

/// Sending an already embedded image as a file adds a File Message without
/// removing the image preview from the viewer card.
#[tokio::test]
async fn transferring_an_embedded_image_keeps_its_image_element() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("shot.png"), png()).unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat shot.png",
        key,
        "m_image",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    fn image_key(card: &serde_json::Value) -> Option<String> {
        match card {
            serde_json::Value::Object(object) => {
                if object.get("tag").and_then(serde_json::Value::as_str) == Some("img") {
                    return object
                        .get("img_key")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                }
                object.values().find_map(image_key)
            }
            serde_json::Value::Array(items) => items.iter().find_map(image_key),
            _ => None,
        }
    }
    let original_key = image_key(&original).expect("the opened image is embedded");
    let payload = send_as_file_payload(&original);
    assert_eq!(payload["viewer_embedded_image_key"], original_key);

    app.host_action_on_message(payload, "msg_image_viewer")
        .await
        .expect("the image transfer acknowledges");
    wait_for_card_update(&platform, "image transfer", CardUpdates::Latest, |card| {
        card_text(card).contains("已发送为文件消息")
    })
    .await;
    let updated = platform.updated_cards().await.pop().expect("viewer patch");
    assert_eq!(image_key(&updated).as_deref(), Some(original_key.as_str()));
    assert!(card_text(&updated).contains("图片（.png）"));
    assert!(card_text(&updated).contains("已发送为文件消息"));
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "the file copy is sent once"
    );
    let recorded = uploads(&platform).await;
    assert_eq!(recorded.len(), 2, "one image and one file upload: {recorded:?}");
    assert!(matches!(recorded[0], PlatformCall::UploadImage { .. }));
    assert!(matches!(recorded[1], PlatformCall::UploadFile { .. }));

    let reread = card_buttons(&updated)
        .into_iter()
        .find(|button| button["text"]["content"] == "🔄 重新读取")
        .expect("the transferred viewer still offers re-read")["value"]
        .clone();
    assert_eq!(reread["viewer_embedded_image_key"], original_key);
    app.host_action_on_message(reread, "msg_image_viewer")
        .await
        .expect("the image re-read acknowledges");
    wait_for_card_update(&platform, "image re-read", CardUpdates::Latest, |card| {
        image_key(card).as_deref() == Some(original_key.as_str())
            && card_text(card).contains("已发送为文件消息")
    })
    .await;
    let reread_card = platform.updated_cards().await.pop().expect("re-read patch");
    assert_eq!(image_key(&reread_card).as_deref(), Some(original_key.as_str()));
    assert_eq!(file_messages(&platform).await.len(), 1, "re-read does not resend");
    assert_eq!(
        platform
            .upload_image_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "re-read keeps the existing image upload"
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
    let (entered, release) = platform.pause("upload_file", "");
    let result = app.host_action(payload.clone()).await.expect("the click acks");
    assert!(
        result.card.is_none(),
        "the click does not rebuild the card inline"
    );
    assert_eq!(
        result.toast.as_deref(),
        Some("正在发送文件…"),
        "the callback's duplicate-safe acknowledgement copy is stable"
    );
    assert!(
        uploads(&platform).await.is_empty(),
        "the click returned before the upload ran"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the first click reaches the parked upload");
    let repeated = app
        .host_action(payload.clone())
        .await
        .expect("a duplicate send click is acknowledged");
    assert!(repeated.card.is_none());
    assert_eq!(repeated.toast.as_deref(), Some("正在发送文件…"));

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
    assert!(
        !card_buttons(updated)
            .iter()
            .any(|button| button["text"]["content"] == "发为文件"),
        "a successful transfer removes its button: {}",
        card_text(updated)
    );
    let after_success = app
        .host_action(payload)
        .await
        .expect("a stale successful-transfer callback is acknowledged");
    assert_eq!(after_success.toast.as_deref(), Some("文件已发送"));
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "a stale second click cannot post another File Message"
    );
}

/// A timed-out reply may already have landed: keep the identity, say the
/// outcome is unknown, and refuse both a stale click and re-read retry.
#[tokio::test]
async fn accepted_then_timed_out_file_message_is_never_retried() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "original text snapshot\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    app.viewer_transfer_timeout_ms
        .store(25, std::sync::atomic::Ordering::Relaxed);
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat notes.txt",
        key,
        "m_ambiguous_transfer",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    let mut transfer = send_as_file_payload(&original);
    transfer["open_message_id"] = serde_json::json!("msg_reply");
    let (entered, _release) = platform.pause_after_accept("send_message", "msg_reply");

    app.host_action(transfer.clone())
        .await
        .expect("the transfer click acknowledges before the send");
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the send reached Feishu before the response timed out");
    wait_for_card_update(&platform, "发送结果未知", CardUpdates::Latest, |card| {
        card_text(card).contains("发送结果未知")
    })
    .await;
    assert_eq!(file_messages(&platform).await.len(), 1, "Feishu accepted once");
    let uncertain = patches_to(&platform, "msg_reply")
        .await
        .last()
        .cloned()
        .expect("the uncertain result patches the viewer");
    assert!(
        !card_buttons(&uncertain)
            .iter()
            .any(|button| button["text"]["content"] == "发为文件"),
        "an ambiguous attempt must not offer a duplicate: {}",
        card_text(&uncertain)
    );

    let duplicate = app
        .host_action(transfer)
        .await
        .expect("a stale ambiguous-transfer click is acknowledged");
    assert_eq!(duplicate.toast.as_deref(), Some("发送结果未知，为避免重复未重试"));

    let reread = card_buttons(&uncertain)
        .into_iter()
        .find(|button| button["text"]["content"] == "🔄 重新读取")
        .expect("the viewer still offers re-read")["value"]
        .clone();
    app.host_action_on_message(reread, "msg_reply")
        .await
        .expect("re-read acknowledges");
    wait_for_card_update(&platform, "uncertain re-read", CardUpdates::Latest, |card| {
        card_text(card).contains("发送结果未知")
    })
    .await;
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "the same identity is not posted again after re-read"
    );
}

/// A transfer clicked from page 2 rebuilds that same page; a later pager flip
/// preserves the successful tracking lane and never re-offers delivery.
#[tokio::test]
async fn transfer_from_a_later_page_survives_a_pager_flip() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("long.txt");
    std::fs::write(&file, numbered_lines(3000)).unwrap();
    let proj_dir = proj.path().to_string_lossy().to_string();
    let file_path = file.to_string_lossy().to_string();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;
    let mut open = crate::feishu::card::session::browse_file_value(
        &key,
        &file_path,
        Some(crate::feishu::card::session::BrowseView {
            dir: &proj_dir,
            show_hidden: true,
            keyword: "long",
            page: 2,
        }),
    );
    open["open_message_id"] = serde_json::json!("msg_page_transfer");
    let result = app
        .host_action(open)
        .await
        .expect("browse-row open is acknowledged before the filesystem read");
    assert!(result.card.is_none());
    wait_for_card_update(&platform, "viewer open", CardUpdates::Latest, |card| {
        card_text(card).contains("line 0001")
            && card_buttons(card)
                .iter()
                .any(|button| button["text"]["content"] == "← 返回目录")
    })
    .await;
    let first = patches_to(&platform, "msg_page_transfer")
        .await
        .last()
        .cloned()
        .expect("browse-row viewer patch");
    let next = pager_button(&first, "下一页")["value"].clone();
    assert_eq!(next["return_dir"], proj_dir);
    assert_eq!(next["return_show_hidden"], "true");
    assert_eq!(next["return_keyword"], "long");
    assert_eq!(next["return_page"], "2");
    let page2 = drive_viewer_rebuild(&app, &platform, next, "msg_page_transfer").await;
    assert_eq!(pager_label(&page2).as_deref(), Some("第 2/2 页 · 共 3000 个"));

    let mut transfer = send_as_file_payload(&page2);
    assert_eq!(transfer["return_dir"], proj_dir);
    assert_eq!(transfer["return_show_hidden"], true);
    assert_eq!(transfer["return_keyword"], "long");
    assert_eq!(transfer["return_page"], 2);
    transfer["open_message_id"] = serde_json::json!("msg_page_transfer");
    let transfer_result = app
        .host_action(transfer.clone())
        .await
        .expect("transfer callback is acknowledged");
    assert!(transfer_result.card.is_none());
    wait_for_card_update(&platform, "page 2 transfer", CardUpdates::Latest, |card| {
        card_text(card).contains("已发送为文件消息") && card_text(card).contains("line 3000")
    })
    .await;
    assert_eq!(file_messages(&platform).await.len(), 1);
    let sent = patches_to(&platform, "msg_page_transfer")
        .await
        .last()
        .cloned()
        .expect("transfer patch");
    assert!(
        card_text(&sent).contains("line 3000"),
        "the clicked page stays: {}",
        card_text(&sent)
    );
    assert!(
        !card_buttons(&sent)
            .iter()
            .any(|button| button["text"]["content"] == "发为文件"),
        "a landed file message is not offered again"
    );

    let duplicate = app
        .host_action(transfer)
        .await
        .expect("stale transfer callbacks are acknowledged");
    assert_eq!(duplicate.toast.as_deref(), Some("文件已发送"));
    assert_eq!(file_messages(&platform).await.len(), 1, "no duplicate message");

    let mut previous = pager_button(&sent, "上一页")["value"].clone();
    previous["open_message_id"] = serde_json::json!("msg_page_transfer");
    let result = app
        .host_action(previous)
        .await
        .expect("the post-transfer flip is acknowledged");
    assert!(result.card.is_none());
    wait_for_card_update(&platform, "page 1 with sent lane", CardUpdates::Latest, |card| {
        card_text(card).contains("line 0001")
            && card_text(card).contains("已发送为文件消息")
            && !card_buttons(card)
                .iter()
                .any(|button| button["text"]["content"] == "发为文件")
    })
    .await;
    let back = patches_to(&platform, "msg_page_transfer")
        .await
        .last()
        .cloned()
        .expect("page 1 patch");
    assert_eq!(pager_label(&back).as_deref(), Some("第 1/2 页 · 共 3000 个"));
    assert!(card_text(&back).contains("已发送为文件消息"));
    let back_button = card_buttons(&back)
        .into_iter()
        .find(|button| button["text"]["content"] == "← 返回目录")
        .expect("the transferred page keeps the directory return button");
    assert_eq!(back_button["value"]["dir"], proj_dir);
    assert_eq!(back_button["value"]["show_hidden"], true);
    assert_eq!(back_button["value"]["keyword"], "long");
    assert_eq!(back_button["value"]["page"], 2);
    assert!(
        !card_buttons(&back)
            .iter()
            .any(|button| button["text"]["content"] == "发为文件")
    );
}

/// A special file is rejected promptly during a transfer, and the failure
/// releases the viewer action so a later regular-file transfer can proceed.
#[cfg(unix)]
#[tokio::test]
async fn a_fifo_transfer_fails_fast_and_releases_the_card_guard() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "initial preview\n").unwrap();

    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat notes.txt",
        key,
        "m_view",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let card = first_card(&platform).await;
    let mut payload = send_as_file_payload(&card);
    payload["open_message_id"] = serde_json::json!("msg_view_card");

    app.viewer_transfer_timeout_ms
        .store(200, std::sync::atomic::Ordering::Relaxed);
    std::fs::remove_file(&file).unwrap();
    let status = std::process::Command::new("mkfifo").arg(&file).status().unwrap();
    assert!(status.success(), "create a FIFO to park the transfer read");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.host_action(payload.clone()),
    )
    .await
    .expect("the FIFO transfer callback returns promptly")
    .expect("the transfer click acks before filesystem work");
    assert!(result.card.is_none());
    wait_for_card_update(
        &platform,
        "special-file transfer refusal",
        CardUpdates::Latest,
        |card| card_text(card).contains("not a regular file"),
    )
    .await;

    // A later card action must get a new guard and read this regular file.
    std::fs::remove_file(&file).unwrap();
    std::fs::write(&file, "read after timeout\n").unwrap();
    let (second_upload_entered, release_second_upload) = platform.pause("upload_file", "");
    let retried = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            app.host_action(payload.clone())
                .await
                .expect("a later send click is acknowledged");
            if tokio::time::timeout(
                std::time::Duration::from_millis(10),
                second_upload_entered.notified(),
            )
            .await
            .is_ok()
            {
                break;
            }
        }
    })
    .await;
    assert!(retried.is_ok(), "the timeout must release the card action guard");
    release_second_upload.notify_one();
    wait_for_card_update(
        &platform,
        "later transfer succeeds",
        CardUpdates::Latest,
        |card| card_text(card).contains("已发送为文件消息"),
    )
    .await;
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "only the later action sends"
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

    let prepared = crate::bridge::viewer::prepare_file_transfer(&path, "notes.txt", &mut classified);
    let lane = crate::bridge::viewer::send_prepared_file_transfer(
        &app.feishu,
        &app.cards_handle(),
        "msg_view_card",
        prepared,
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

    let prepared = crate::bridge::viewer::prepare_file_transfer(&path, "report.pdf", &mut classified);
    let lane = crate::bridge::viewer::send_prepared_file_transfer(
        &app.feishu,
        &app.cards_handle(),
        "msg_view_card",
        prepared,
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
        .store(500, std::sync::atomic::Ordering::Relaxed);
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
    let page2 = drive_viewer_rebuild(
        &app,
        &platform,
        pager_button(&card, "下一页")["value"].clone(),
        "msg_long_viewer",
    )
    .await;
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

/// A 500 KB minified UTF-8 preview is larger than one card's raw element
/// sequence, but still below the preview read cap: it belongs to the pager,
/// not the File Message overflow lane (#740, #669).
#[tokio::test]
async fn a_half_megabyte_minified_text_file_uses_the_viewer_pager() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("minified.txt"), "x".repeat(500 * 1024)).unwrap();

    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat minified.txt",
        key,
        "m_minified",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    assert_eq!(card_header(&card), "📄 minified.txt");
    assert!(pager_label(&card).is_some(), "the text preview has a pager");
    assert!(
        card_buttons(&card)
            .iter()
            .any(|button| button["value"]["op"] == "page"),
        "a next-page action is available"
    );
    assert!(
        file_messages(&platform).await.is_empty(),
        "no overflow File Message"
    );
}

/// A page callback's lane belongs to the old snapshot. When the file changes
/// from paged text to an image, the rebuild drops the stale text-delivery lane
/// and says the file changed — it must not report `未发送` for a transfer this
/// navigation pass never attempted (#750). The fresh bytes stay one
/// 「重新读取」 away, and 「发为文件」 is offered again.
#[tokio::test]
async fn flipping_a_page_after_a_kind_change_replaces_the_stale_lane() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("long.txt");
    std::fs::write(&file, numbered_lines(3000)).unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat long.txt",
        key,
        "m_changed_page",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    app.host_action_on_message(send_as_file_payload(&original), "msg_changed_page")
        .await
        .expect("transfer from the paged text viewer acknowledges");
    wait_for_card_update(&platform, "text file transfer", CardUpdates::Latest, |card| {
        card_text(card).contains("已发送为文件消息")
    })
    .await;
    let transferred = platform.updated_cards().await.pop().expect("transfer patch");
    let next = pager_button(&transferred, "下一页")["value"].clone();
    assert_eq!(next["viewer_lane_mime"], "text/plain", "the text lane is carried");
    assert_eq!(next["viewer_lane_delivery"], "sent_as_file");

    let image = png();
    std::fs::write(&file, &image).unwrap();
    let updated = drive_viewer_rebuild(&app, &platform, next, "msg_changed_page").await;
    let text = card_text(&updated);
    assert!(
        text.contains("图片（.txt）"),
        "the fresh content is classified: {text}"
    );
    assert!(
        text.contains("文件在打开后已变化"),
        "the rebuild names the change instead of a delivery outcome: {text}"
    );
    assert!(
        !text.contains("未发送"),
        "navigation attempted no transfer, so it says no such thing: {text}"
    );
    assert!(
        !text.contains("已发送为文件消息") && !text.contains("image/png"),
        "the stale delivery line and its lane are gone: {text}"
    );
    assert!(
        !text.contains("文本（") && !text.contains("line 3000"),
        "stale preview is gone: {text}"
    );
    assert_eq!(updated.to_string().matches("\"tag\":\"img\"").count(), 0);
    assert!(
        card_buttons(&updated)
            .iter()
            .any(|button| button["value"]["op"] == "send"),
        "the new bytes can be delivered: {text}"
    );
    assert!(
        updated.to_string().find("viewer_lane_mime").is_none(),
        "no stale lane rides the rebuilt payload: {updated}"
    );
}

/// The same rule when the file stays text: the payload's `已发送` lane describes
/// the OLD bytes, so a flip that re-reads different bytes drops it and names the
/// change instead of `未发送` (#750). The pager itself survives — the content is
/// still text and the new window is what the reader asked for.
#[tokio::test]
async fn flipping_a_page_after_a_text_change_drops_the_stale_lane() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("long.txt");
    std::fs::write(&file, numbered_lines(3000)).unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat long.txt",
        key,
        "m_changed_text_page",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    app.host_action_on_message(send_as_file_payload(&original), "msg_changed_text_page")
        .await
        .expect("transfer from the paged text viewer acknowledges");
    wait_for_card_update(&platform, "text file transfer", CardUpdates::Latest, |card| {
        card_text(card).contains("已发送为文件消息")
    })
    .await;
    let transferred = platform.updated_cards().await.pop().expect("transfer patch");
    let next = pager_button(&transferred, "下一页")["value"].clone();

    // Same MIME and same byte length: only the content identity moves.
    std::fs::write(&file, numbered_lines(3000).replace("line ", "LINE ")).unwrap();
    let updated = drive_viewer_rebuild(&app, &platform, next, "msg_changed_text_page").await;
    let text = card_text(&updated);
    assert!(
        text.contains("文件在打开后已变化"),
        "the rebuild names the change instead of a delivery outcome: {text}"
    );
    assert!(
        !text.contains("未发送"),
        "navigation attempted no transfer, so it says no such thing: {text}"
    );
    assert!(
        !text.contains("已发送为文件消息"),
        "the delivery status for the old bytes is gone: {text}"
    );
    assert!(
        text.contains("LINE ") && !text.contains("line "),
        "the fresh window is rendered: {text}"
    );
    assert!(pager_label(&updated).is_some(), "text still pages: {text}");
    assert!(
        card_buttons(&updated)
            .iter()
            .any(|button| button["value"]["op"] == "send"),
        "the new bytes can be delivered: {text}"
    );
}

/// A lane that recorded no identity of its own (the Rich Text delivery) still
/// belongs to the card's snapshot, and the payload carries that identity. A
/// same-MIME/same-size edit must drop the lane — comparing MIME and size alone
/// would keep it (#750, gate finding).
#[tokio::test]
async fn a_lane_without_its_own_identity_still_compares_the_payload_identity() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("long.txt");
    std::fs::write(&file, numbered_lines(3000)).unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat long.txt",
        key,
        "m_identityless_lane",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let original = first_card(&platform).await;
    let mut next = pager_button(&original, "下一页")["value"].clone();
    // A Rich Text lane carries no content identity of its own; the card's
    // snapshot identity is the only proof of which bytes it describes.
    next["viewer_lane_mime"] = serde_json::json!("text/plain");
    next["viewer_lane_name"] = serde_json::json!("long.txt");
    next["viewer_lane_size"] = serde_json::json!(std::fs::metadata(&file).unwrap().len().to_string());
    next["viewer_lane_delivery"] = serde_json::json!("sent_as_rich_text");
    assert!(
        next.get("viewer_file_identity").is_some(),
        "the card carries its snapshot identity: {next}"
    );
    assert!(
        next.get("viewer_lane_content_identity").is_none(),
        "the lane itself carries none: {next}"
    );

    // Same MIME and same byte length: only the content identity moves.
    std::fs::write(&file, numbered_lines(3000).replace("line ", "LINE ")).unwrap();
    let updated = drive_viewer_rebuild(&app, &platform, next, "msg_identityless_lane").await;
    let text = card_text(&updated);
    assert!(
        text.contains("文件在打开后已变化"),
        "the identityless lane is compared against the payload: {text}"
    );
    assert!(!text.contains("📎"), "the stale lane is gone: {text}");
    assert!(
        !text.contains("未发送") && !text.contains("富文本"),
        "no delivery outcome this pass never produced: {text}"
    );
}

/// Page state round-trips through the button payload (ADR-0052): the viewer
/// keeps no per-card state, so each payload rebuilds its target window.
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

    // The target page renders its own window.
    let next = pager_button(&card, "下一页")["value"].clone();
    let page2 = drive_viewer_rebuild(&app, &platform, next, "msg_replay_viewer").await;
    assert!(card_text(&page2).contains("line 3000"));
    assert!(!card_text(&page2).contains("line 0001"));

    // And the way back lands exactly on the first window.
    let prev = pager_button(&page2, "上一页")["value"].clone();
    assert_eq!(prev["page"], 1, "previous-page payload: {prev}");
    let back = drive_viewer_rebuild(&app, &platform, prev, "msg_replay_viewer").await;
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

/// #781: the markdown view toggle is the SAME stateless rebuild as a page flip —
/// the click PATCHes the card it was clicked on (no new message, no File
/// Message), and the payload alone decides which view renders.
#[tokio::test]
async fn viewer_markdown_view_toggle_rebuilds_the_card_from_its_payload() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("notes.md"), "# Title\n\ntext body\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat notes.md",
        key.clone(),
        "m_md",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let source = card_text(&card);
    assert!(
        source.contains("```markdown") && source.contains("# Title"),
        "the default view is the source: {source}"
    );
    let to_render = card_buttons(&card)
        .into_iter()
        .find(|b| b["value"]["op"] == "view")
        .expect("the view toggle is on the card")["value"]
        .clone();
    assert_eq!(to_render["viewer_view"], "rendered");

    // The toggle PATCHes the clicked card: no second message, no File Message.
    let files_before = file_messages(&platform).await.len();
    let rendered_card = drive_viewer_rebuild(&app, &platform, to_render, "msg_md_viewer").await;
    let rendered = card_text(&rendered_card);
    assert!(
        rendered.contains("# Title") && !rendered.contains("```markdown"),
        "the rendered view is one markdown element: {rendered}"
    );
    assert_eq!(
        patches_to(&platform, "msg_md_viewer").await.len(),
        1,
        "the toggle rewrites the card it was clicked on"
    );
    assert_eq!(
        file_messages(&platform).await.len(),
        files_before,
        "the view toggle sends nothing"
    );

    // And back: the payload alone restores the source view exactly.
    let back = drive_viewer_rebuild(
        &app,
        &platform,
        card_buttons(&rendered_card)
            .into_iter()
            .find(|b| b["value"]["op"] == "view")
            .expect("the source toggle")["value"]
            .clone(),
        "msg_md_viewer",
    )
    .await;
    assert_eq!(
        card_text(&back),
        source,
        "the payload alone restores the source view"
    );

    // The OTHER two rebuild controls carry the view too (#781): a re-read and a
    // transfer each PATCH the card they were clicked on, so neither may reset an
    // explicit Rendered view to the default source.
    let rendered_card = drive_viewer_rebuild(
        &app,
        &platform,
        card_buttons(&back)
            .into_iter()
            .find(|b| b["value"]["op"] == "view")
            .expect("the render toggle")["value"]
            .clone(),
        "msg_md_viewer",
    )
    .await;
    assert!(!card_text(&rendered_card).contains("```markdown"));

    let reread = card_buttons(&rendered_card)
        .into_iter()
        .find(|b| b["text"]["content"] == "🔄 重新读取")
        .expect("the re-read button")["value"]
        .clone();
    assert_eq!(reread["viewer_view"], "rendered", "{reread}");
    let reread_card = drive_viewer_rebuild(&app, &platform, reread, "msg_md_viewer").await;
    assert!(
        !card_text(&reread_card).contains("```markdown") && card_text(&reread_card).contains("# Title"),
        "the re-read keeps the rendered view: {}",
        card_text(&reread_card)
    );

    let mut transfer = send_as_file_payload(&reread_card);
    assert_eq!(transfer["viewer_view"], "rendered", "{transfer}");
    transfer["open_message_id"] = serde_json::json!("msg_md_viewer");
    let result = app
        .host_action(transfer)
        .await
        .expect("the transfer callback is acknowledged");
    assert!(result.card.is_none());
    wait_for_card_update(
        &platform,
        "markdown rendered transfer",
        CardUpdates::Latest,
        |card| card_text(card).contains("已发送为文件消息"),
    )
    .await;
    let sent = patches_to(&platform, "msg_md_viewer")
        .await
        .last()
        .cloned()
        .expect("the transfer patch");
    let sent_text = card_text(&sent);
    assert!(
        !sent_text.contains("```markdown") && sent_text.contains("# Title"),
        "the transfer keeps the rendered view: {sent_text}"
    );
    assert!(sent_text.contains("已发送为文件消息"), "{sent_text}");
    assert_eq!(
        file_messages(&platform).await.len(),
        files_before + 1,
        "the transfer posts exactly one File Message"
    );
}

/// #781: a long markdown file opens on its paged SOURCE, and a flip keeps it —
/// page 2 is the source's second window, not the rendered text.
#[tokio::test]
async fn viewer_markdown_source_pages_and_the_flip_stays_on_it() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let body = format!("{}\n{}\n", "`".repeat(500), "x".repeat(20_000));
    std::fs::write(proj.path().join("paged.md"), body).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    send_command_in(
        &app,
        "/cat paged.md",
        key.clone(),
        "m_paged",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let source = card_text(&card);
    assert!(
        source.contains("```markdown"),
        "the default view is the source: {source}"
    );
    assert!(pager_label(&card).is_some(), "the source pages: {source}");
    assert!(
        card_buttons(&card).iter().any(|b| b["value"]["op"] == "view"),
        "the render fits, so it is still offered: {source}"
    );

    let next = card_buttons(&card)
        .into_iter()
        .find(|b| b["value"]["op"] == "page" && b["text"]["content"] == "下一页")
        .expect("the source pages")["value"]
        .clone();
    let page2 = drive_viewer_rebuild(&app, &platform, next, "msg_paged").await;
    let page2_text = card_text(&page2);
    assert!(
        page2_text.contains("```markdown"),
        "page 2 stays on the source view: {page2_text}"
    );
    assert!(
        pager_label(&page2).is_some_and(|label| label.contains("第 2/")),
        "the source pager names page 2: {page2_text}"
    );
    assert!(
        card_buttons(&page2).iter().any(|b| b["value"]["op"] == "view"),
        "page 2 still offers the render: {page2_text}"
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
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj_dir)).await;

    // Open the viewer the way a browse file row does: no op, the listing's
    // whole state in dir/show_hidden/keyword/page.
    let result = app
        .host_action(serde_json::json!({
            "action": "file",
            "path": file_str,
            "chat_id": "chat_1",
            "thread_id": "chat_1",
            "dir": proj_dir,
            "show_hidden": true,
            "keyword": "long",
            "page": 1,
            "open_message_id": "msg_listing_viewer",
        }))
        .await
        .expect("the row open is acknowledged");
    assert!(result.card.is_none(), "classification stays off the callback");
    wait_for_card_update(&platform, "viewer open", CardUpdates::Latest, |card| {
        card_text(card).contains("line 0001")
    })
    .await;
    let card = patches_to(&platform, "msg_listing_viewer")
        .await
        .last()
        .cloned()
        .expect("the bounded viewer open patches the clicked card");
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
    // The viewer resolves the path, so the pager carries the canonical
    // spelling — compare like-for-like (`tempfile` hands back the raw one,
    // which differs on macOS `/private/var` and Windows 8.3 names).
    assert_eq!(next["path"], canonical(&file));
    assert_eq!(next["return_dir"], proj_dir);
    assert_eq!(next["return_show_hidden"], "true");
    assert_eq!(next["return_keyword"], "long");
    assert_eq!(next["return_page"], "1");
    // The flipped page still offers the same way back.
    let page2 = drive_viewer_rebuild(&app, &platform, next, "msg_listing_viewer").await;
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

#[tokio::test]
async fn viewer_row_without_callback_message_id_is_rejected_without_inline_open() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "notes\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    let result = app
        .host_action(serde_json::json!({
            "action": "file",
            "path": file,
            "chat_id": key.chat_id,
            "thread_id": key.thread_id,
            "dir": proj.path(),
            "page": 1,
        }))
        .await
        .expect("a callback without its message reference is handled");

    assert!(result.card.is_none(), "no inline viewer is built");
    assert_eq!(result.toast.as_deref(), Some("无法打开文件：找不到文件浏览卡。"));
    assert!(patches_to(&platform, "").await.is_empty());
}

/// A Tool Panel's 「📂 打开」 click (ADR-0083 §8, #743): the payload carries the
/// Session, not the Chat — the click resolves the Session's Chat/Topic from
/// the SessionStore and its (possibly relative) structured path against the
/// Session's directory, then sends a standalone File Viewer reply. No `op`
/// rides along, so the click routes to the opener, never to #739's transfer.
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
    let mut payload = crate::feishu::card::session::tool_file_value("ses_seed", "report.md");
    payload["open_message_id"] = serde_json::json!("msg_tool_open");
    assert!(
        payload.get("op").is_none(),
        "the button never transfers: {payload}"
    );
    let result = app.host_action(payload).await.expect("the click is acknowledged");
    assert!(result.card.is_none(), "the bounded open runs after ack");
    let opened = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(card) = platform.replied_cards().await.last().cloned() {
                break card;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the standalone viewer reply lands");
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
        patches_to(&platform, "msg_tool_open").await.is_empty(),
        "the Tool Panel's live card is never patched"
    );
    assert!(
        platform.calls.lock().await.iter().any(|call| matches!(
            call,
            PlatformCall::ReplyCard { reply_to, card }
                if reply_to == "msg_tool_open" && card_text(card).contains("written by the agent")
        )),
        "the viewer is a new reply in the live card's chat/thread"
    );
}

/// A Tool Panel open acknowledges before canonical path resolution. Park that
/// exact filesystem step in the bounded worker; the callback must still return
/// its ack while resolution is stalled (CODING_STANDARDS.md §4).
#[tokio::test]
async fn tool_panel_open_acknowledges_before_stalled_path_resolution() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("report.md");
    std::fs::write(&file, "viewer after parked resolve\n").unwrap();

    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    let mut payload = crate::feishu::card::session::tool_file_value("ses_seed", "report.md");
    payload["open_message_id"] = serde_json::json!("msg_tool_resolve");

    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
    crate::bridge::handler::install_viewer_resolve_test_hook(
        file.clone(),
        std::sync::Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }),
    );

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(std::time::Duration::from_millis(700), app.host_action(payload))
        .await
        .expect("the callback does not wait for filesystem work")
        .expect("the callback is acknowledged");
    assert!(result.card.is_none());
    assert!(started.elapsed() < std::time::Duration::from_millis(700));

    let entered = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tokio::task::spawn_blocking(move || entered_rx.recv()),
    )
    .await;
    crate::bridge::handler::clear_viewer_resolve_test_hook();
    release_tx.send(()).unwrap();
    entered
        .expect("path resolution reached the parked background step")
        .unwrap()
        .unwrap();

    let opened = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(card) = platform.replied_cards().await.last().cloned()
                && card_text(&card).contains("viewer after parked resolve")
            {
                break card;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the bounded Tool Panel open finishes after resolution is released");
    assert!(card_text(&opened).contains("viewer after parked resolve"));
}

/// A Tool Panel embedded in a live Turn card opens a separate viewer message;
/// the Turn's later flush still owns and updates only its original card.
#[tokio::test]
async fn live_turn_tool_panel_open_keeps_the_viewer_after_the_turn_flushes() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("report.md"), "standalone viewer body\n").unwrap();

    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    // Seed and flush a real Turn CardSession with a live write panel. Its
    // rendered button supplies the actual Tool Panel callback payload.
    let cards = app.cards_handle();
    Turn::seed_card(&cards, "ses_seed", Some("msg_live_turn")).await;
    Turn::set_session(&cards, "ses_seed").await;
    Turn::set_card_state(&cards, "ses_seed", crate::feishu::card::CardState::Streaming).await;
    Turn::push_tool(
        &cards,
        "ses_seed",
        "call_write",
        crate::feishu::card::tool_render::ToolPanel::for_test(
            "write",
            crate::backend::ToolStatus::Running,
            Some(serde_json::json!({"filePath": "report.md", "content": "written"})),
            Some("written"),
        ),
    )
    .await;
    Turn::flush_card(&cards, "ses_seed").await;
    let turn_card = patches_to(&platform, "msg_live_turn")
        .await
        .last()
        .cloned()
        .expect("the live Turn card renders");
    let open_payload = card_buttons(&turn_card)
        .into_iter()
        .find(|button| button["text"]["content"] == "📂 打开")
        .unwrap_or_else(|| panic!("the live write panel has its open button: {turn_card}"))["value"]
        .clone();
    assert_eq!(open_payload["session_id"], "ses_seed");
    assert!(
        open_payload.get("dir").is_none(),
        "Tool Panel payload has no listing"
    );

    let result = app
        .host_action_on_message(open_payload, "msg_live_turn")
        .await
        .expect("the click is acknowledged");
    assert!(result.card.is_none(), "the bounded open runs after ack");

    let viewer = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(card) = platform.replied_cards().await.last().cloned()
                && card_text(&card).contains("standalone viewer body")
            {
                break card;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the standalone File Viewer is sent");
    assert_eq!(card_header(&viewer), "📄 report.md");
    assert_eq!(patches_to(&platform, "msg_live_turn").await.len(), 1);

    // The Turn continues and flushes after the viewer was sent. Its card gains
    // new Turn content without replacing either the live panel or viewer.
    Turn::push_text(&cards, "ses_seed", "turn continued after viewer open").await;
    Turn::flush_card(&cards, "ses_seed").await;
    let turn_updates = patches_to(&platform, "msg_live_turn").await;
    assert_eq!(turn_updates.len(), 2, "the Turn performed its subsequent flush");
    let continued_turn = turn_updates.last().unwrap();
    let turn_text = card_text(continued_turn);
    assert!(
        turn_text.contains("turn continued after viewer open"),
        "{turn_text}"
    );
    assert!(
        turn_text.contains("report.md"),
        "the Turn's Tool Panel survives: {turn_text}"
    );
    assert_eq!(
        platform.replied_cards().await.len(),
        1,
        "the separate viewer message survives the Turn's PATCH"
    );
    assert!(
        !turn_text.contains("standalone viewer body"),
        "the viewer preview never replaces the Turn card"
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
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    let absolute = elsewhere.path().join("notes.md").to_string_lossy().to_string();
    let mut payload = crate::feishu::card::session::tool_file_value("ses_seed", &absolute);
    payload["open_message_id"] = serde_json::json!("msg_tool_outside");
    let result = app.host_action(payload).await.expect("the click is acknowledged");
    assert!(result.card.is_none(), "the bounded open runs after ack");
    let opened = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(card) = platform.replied_cards().await.last().cloned()
                && card_text(&card).contains("elsewhere")
            {
                break card;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the standalone viewer reply lands");
    assert_eq!(card_header(&opened), "📄 notes.md");
    assert!(
        card_text(&opened).contains(&canonical(&elsewhere.path().join("notes.md"))),
        "the absolute path opens as-is: {opened}"
    );
    assert!(
        !card_text(&opened).contains("⚠️"),
        "an outside-session path carries no warning row: {opened}"
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

/// The first `reply_text` the platform recorded, once one lands — the reply a
/// card-less Tool Panel preview speaks its outcome with (#778): the target message
/// and the text.
async fn first_reply_text(platform: &RecordingPlatform) -> (String, String) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let replies: Vec<(String, String)> = platform
                .calls
                .lock()
                .await
                .iter()
                .filter_map(|call| match call {
                    PlatformCall::ReplyText { message_id, text } => Some((message_id.clone(), text.clone())),
                    _ => None,
                })
                .collect();
            if let Some(reply) = replies.into_iter().next() {
                break reply;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the reply lands")
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
        card_buttons(&card)
            .iter()
            .any(|b| b["text"]["content"] == "发为文件"),
        "Rich Text carries a preview, not the original bytes: {text}"
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

/// A binary File Message is delivered once per content snapshot. Re-reading
/// unchanged bytes keeps the card's sent line without sending another message;
/// changing the bytes rotates the identity and allows one new delivery (#744).
#[tokio::test]
async fn rereading_binary_content_sends_only_once_per_identity() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("report.pdf");
    std::fs::write(&file, b"%PDF-1.7\noriginal snapshot\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat report.pdf",
        key,
        "m_binary_reread",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let mut current = first_card(&platform).await;
    let first_payload = reread_payload(&current);
    let original_identity = first_payload["viewer_file_identity"]
        .as_str()
        .and_then(|identity| identity.parse::<u64>().ok())
        .expect("the card carries the uploaded content identity");
    assert_eq!(
        first_payload["viewer_sent_file_identity"]
            .as_str()
            .and_then(|identity| identity.parse::<u64>().ok()),
        Some(original_identity),
        "the re-read knows which snapshot was already delivered"
    );
    assert_eq!(file_messages(&platform).await.len(), 1);

    for _ in 0..2 {
        let before = patches_to(&platform, "msg_reply").await.len();
        app.host_action_on_message(reread_payload(&current), "msg_reply")
            .await
            .expect("unchanged binary re-read acknowledges");
        current = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let patches = patches_to(&platform, "msg_reply").await;
                if patches.len() > before {
                    break patches.last().unwrap().clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the same viewer card is refreshed");
        assert!(card_text(&current).contains("已发送为文件消息"));
        assert_eq!(
            file_messages(&platform).await.len(),
            1,
            "unchanged bytes are not resent"
        );
    }

    std::fs::write(&file, b"%PDF-1.7\nchanged snapshot with more bytes\n").unwrap();
    let before = patches_to(&platform, "msg_reply").await.len();
    app.host_action_on_message(reread_payload(&current), "msg_reply")
        .await
        .expect("changed binary re-read acknowledges");
    let changed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let patches = patches_to(&platform, "msg_reply").await;
            if patches.len() > before {
                break patches.last().unwrap().clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the changed viewer card is refreshed");
    assert!(card_text(&changed).contains("已发送为文件消息"));
    assert_ne!(
        reread_payload(&changed)["viewer_file_identity"]
            .as_str()
            .and_then(|identity| identity.parse::<u64>().ok()),
        Some(original_identity),
        "changed bytes rotate the stateless identity"
    );
    assert_eq!(
        file_messages(&platform).await.len(),
        2,
        "new bytes get one new message"
    );
}

/// A text transfer's once-guard is tied to the card's content identity. After a
/// re-read captures changed text, the new button can deliver that new snapshot
/// instead of hitting the old 「文件已发送」 guard (finding N1).
#[tokio::test]
async fn reread_rotates_the_text_transfer_once_guard() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let file = proj.path().join("notes.txt");
    std::fs::write(&file, "original text snapshot\n").unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;
    send_command_in(
        &app,
        "/cat notes.txt",
        key,
        "m_text_transfer",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let original = first_card(&platform).await;
    let old_identity = send_as_file_payload(&original)["viewer_file_identity"]
        .as_str()
        .and_then(|identity| identity.parse::<u64>().ok())
        .expect("the text transfer payload carries the snapshot identity");
    let mut transfer = send_as_file_payload(&original);
    transfer["open_message_id"] = serde_json::json!("msg_reply");
    app.host_action(transfer)
        .await
        .expect("the first transfer acknowledges");
    wait_for_card_update(&platform, "first text delivery", CardUpdates::Latest, |card| {
        card_text(card).contains("已发送为文件消息")
    })
    .await;
    assert_eq!(file_messages(&platform).await.len(), 1);

    std::fs::write(&file, "changed text snapshot after refresh\n").unwrap();
    let sent_card = platform
        .updated_cards()
        .await
        .last()
        .cloned()
        .expect("transfer card patch");
    let before = patches_to(&platform, "msg_reply").await.len();
    app.host_action_on_message(reread_payload(&sent_card), "msg_reply")
        .await
        .expect("the changed text re-read acknowledges");
    let refreshed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let patches = patches_to(&platform, "msg_reply").await;
            if patches.len() > before && card_text(patches.last().unwrap()).contains("changed text snapshot")
            {
                break patches.last().unwrap().clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the refreshed text snapshot lands on the same card");
    assert_ne!(
        send_as_file_payload(&refreshed)["viewer_file_identity"]
            .as_str()
            .and_then(|identity| identity.parse::<u64>().ok()),
        Some(old_identity),
        "the refreshed transfer control carries a new identity"
    );

    let mut refreshed_transfer = send_as_file_payload(&refreshed);
    refreshed_transfer["open_message_id"] = serde_json::json!("msg_reply");
    let result = app
        .host_action(refreshed_transfer)
        .await
        .expect("the refreshed content can be sent");
    assert_eq!(result.toast.as_deref(), Some("正在发送文件…"));
    wait_for_card_update(&platform, "second text delivery", CardUpdates::Latest, |card| {
        card_text(card).contains("已发送为文件消息")
    })
    .await;
    assert_eq!(
        file_messages(&platform).await.len(),
        2,
        "new identity is delivered once"
    );
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

/// A FIFO is rejected by the bounded read without waiting for a writer; the
/// click still returns its ack and the last-good viewer card stays in place.
#[cfg(unix)]
#[tokio::test]
async fn reread_fifo_fails_promptly_without_replacing_the_viewer_card() {
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
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let answered = platform.calls.lock().await.iter().any(|call| {
                matches!(call, PlatformCall::ReplyText { message_id, text }
                    if message_id == "msg_reply" && text.contains("not a regular file"))
            });
            if answered {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the FIFO read failure is answered promptly");
    assert_eq!(first_card(&platform).await, original);
    assert!(platform.updated_cards().await.is_empty());
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

/// 发为文件 delivers the original bytes as a File Message; it does NOT replace
/// the Rich Text Message carrying an overflow preview. The rebuilt card must
/// keep tracking that message, so a later 重新读取 edits it (with the new rows)
/// instead of posting a second copy and orphaning the first (#744, Codex gate
/// round 7 — the transfer used to drop the id).
#[tokio::test]
async fn reread_after_send_as_file_edits_the_same_overflow_message() {
    let (_work_dir, _project, _state, app, platform, file, original) = open_overflow_csv_viewer().await;

    // 发为文件 on the overflow viewer: one File Message, off the callback path.
    let result = app
        .host_action_on_message(send_as_file_payload(&original), "msg_csv_card")
        .await
        .expect("the transfer click acknowledges");
    assert!(result.card.is_none());
    wait_for_card_update(
        &platform,
        "已发送为文件消息",
        CardUpdates::Latest,
        |card| card_text(card).contains("已发送为文件消息"),
    )
    .await;
    let transferred = patches_to(&platform, "msg_csv_card")
        .await
        .last()
        .cloned()
        .expect("the transfer PATCHes the viewer card");

    // The rebuilt card still names the Rich Text Message the transfer kept.
    let payload = reread_payload(&transferred);
    assert_eq!(
        payload["rich_message_id"], "msg_file",
        "the transfer's rebuilt card keeps the existing Rich Text Message id: {payload}"
    );

    // New rows: a deliberate re-read must EDIT that same message, not post a
    // second one.
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
    app.host_action_on_message(payload, "msg_csv_card")
        .await
        .expect("the re-read acknowledges");
    // The re-read rebuilds the card with the overflow lane's tracking line,
    // which the transfer's "已发送为文件消息" patch cannot satisfy.
    wait_for_card_update(&platform, "r60 edited in place", CardUpdates::Latest, |card| {
        card_text(card).contains("已发送为富文本消息")
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    assert!(
        calls.iter().any(|call| matches!(call, PlatformCall::EditMessage {
            message_id, msg_type, content, outcome: crate::feishu::MessageEditOutcome::Updated,
            ..
        } if message_id == "msg_file" && msg_type == "post" && content.to_string().contains("r60"))),
        "the existing Rich Text Message is edited with the new row: {calls:?}"
    );
    assert_eq!(
        rich_text_messages(&platform).await.len(),
        1,
        "no second Rich Text Message is posted: {calls:?}"
    );
}

/// An UNCHANGED re-read after 发为文件 must leave the overflow preview alone.
/// The bytes are already in the File Message, so nothing was sent afresh and
/// the Rich Text Message still holds exactly this content: retiring it with a
/// "content updated" notice would be a lie and would destroy the only copy of
/// the table on the card (#741 keeps the preview, #744 keeps the message;
/// Codex gate round 7).
#[tokio::test]
async fn unchanged_reread_after_send_as_file_keeps_the_overflow_preview() {
    let (_work_dir, _project, _state, app, platform, _file, original) = open_overflow_csv_viewer().await;
    app.host_action_on_message(send_as_file_payload(&original), "msg_csv_card")
        .await
        .expect("the transfer click acknowledges");
    wait_for_card_update(
        &platform,
        "已发送为文件消息",
        CardUpdates::Latest,
        |card| card_text(card).contains("已发送为文件消息"),
    )
    .await;
    let transferred = patches_to(&platform, "msg_csv_card")
        .await
        .last()
        .cloned()
        .expect("the transfer PATCHes the viewer card");

    let before = patches_to(&platform, "msg_csv_card").await.len();
    app.host_action_on_message(reread_payload(&transferred), "msg_csv_card")
        .await
        .expect("the unchanged re-read acknowledges");
    let reread = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let patches = patches_to(&platform, "msg_csv_card").await;
            if patches.len() > before {
                break patches.last().unwrap().clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the unchanged re-read refreshes the card");

    let text = card_text(&reread);
    assert!(
        text.contains("| r0 | 0 |"),
        "the truncated table stays on the card: {text}"
    );
    assert!(
        text.contains("已发送为文件消息"),
        "the file message is still named: {text}"
    );
    assert!(
        !text.contains("文件内容已更新"),
        "no false update notice on an unchanged re-read: {text}"
    );
    assert_eq!(
        reread_payload(&reread)["rich_message_id"],
        "msg_file",
        "the card keeps tracking the rich copy for the next real change"
    );
    let calls = platform.calls.lock().await.clone();
    assert!(
        !calls.iter().any(|call| matches!(call, PlatformCall::EditMessage {
            message_id, content, ..
        } if message_id == "msg_file" && content.to_string().contains("文件内容已更新"))),
        "the rich copy is not replaced by a notice: {calls:?}"
    );
    assert_eq!(
        rich_text_messages(&platform).await.len(),
        1,
        "the rich copy is not re-posted: {calls:?}"
    );
}

/// Two re-read callbacks on the same card must not both reach the edit-limit
/// fallback and send duplicate Rich Text Messages.
#[tokio::test]
async fn duplicate_reread_click_sends_only_one_fresh_rich_message() {
    let (_work_dir, _project, _state, app, platform, file, original) = open_overflow_csv_viewer().await;
    let csv = format!(
        "name,age\n{}",
        (0..61)
            .map(|i| format!("r{i},{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(&file, csv).unwrap();
    platform
        .refuse_edit_message_count
        .store(2, std::sync::atomic::Ordering::SeqCst);
    platform.given_message_reply_id("msg_fresh");
    let (entered, release) = platform.pause("send_message", "msg_reply");
    let payload = reread_payload(&original);

    app.host_action_on_message(payload.clone(), "msg_reply")
        .await
        .expect("the first re-read acknowledges");
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the first re-read reaches the paused fresh-message send");
    app.host_action_on_message(payload, "msg_reply")
        .await
        .expect("the duplicate re-read acknowledges");
    release.notify_one();

    wait_for_card_update(&platform, "fresh rich tracking", CardUpdates::Latest, |card| {
        reread_payload(card)["rich_message_id"] == "msg_fresh"
    })
    .await;
    let calls = platform.calls.lock().await.clone();
    assert_eq!(
        calls
            .iter()
            .filter(
                |call| matches!(call, PlatformCall::SendMessageReply { msg_type, content, .. }
                if msg_type == "post" && content.to_string().contains("r60"))
            )
            .count(),
        1,
        "only the claimed re-read may send the refreshed rich body"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| matches!(call, PlatformCall::EditMessage { message_id, .. } if message_id == "msg_file"))
            .count(),
        1,
        "the duplicate callback does not start a second edit"
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

/// When a re-read's rich preview no longer fits the post budget, the old copy
/// is retired with the post-cap notice before the File Message takes over.
#[tokio::test]
async fn reread_past_post_cap_retires_the_old_rich_copy() {
    let (_work_dir, _project, _state, app, platform, file, original) = open_overflow_csv_viewer().await;
    let csv = format!(
        "name,age\n{}",
        (0..300)
            .map(|i| format!("r{i},{}", "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let rich_md = crate::feishu::card::viewer::rich_post_md(
        crate::feishu::card::viewer::ViewerKind::Delimited,
        Some("csv"),
        &csv,
    );
    assert!(
        rich_md.is_none(),
        "the refreshed full table exceeds the post budget: input {}, output {}",
        csv.len(),
        rich_md.as_ref().map_or(0, String::len)
    );
    std::fs::write(&file, csv).unwrap();

    app.host_action_on_message(reread_payload(&original), "msg_reply")
        .await
        .expect("the post-cap re-read acknowledges");
    wait_for_card_update(&platform, "post-cap reread patch", CardUpdates::Latest, |_| true).await;
    let updated = platform.updated_cards().await.pop().expect("viewer patch");
    assert!(
        card_text(&updated).contains("已发送为文件消息"),
        "post-cap fallback card: {}",
        card_text(&updated)
    );

    let calls = platform.calls.lock().await.clone();
    assert!(calls.iter().any(|call| matches!(call, PlatformCall::EditMessage {
        message_id,
        msg_type,
        content,
        outcome: crate::feishu::MessageEditOutcome::Updated,
        ..
    } if message_id == "msg_file" && msg_type == "post"
        && content.to_string().contains("最新内容以文件消息为准"))));
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "the new file lane lands once"
    );
}

/// A re-read that changes a rich-text preview into a whole-bytes image lane
/// retires the old rich copy with the viewer/file-message notice.
#[tokio::test]
async fn reread_to_whole_bytes_lane_retires_the_old_rich_copy() {
    let (_work_dir, _project, _state, app, platform, file, original) = open_overflow_csv_viewer().await;
    std::fs::write(&file, png()).unwrap();

    app.host_action_on_message(reread_payload(&original), "msg_reply")
        .await
        .expect("the whole-bytes re-read acknowledges");
    wait_for_card_update(&platform, "embedded image refresh", CardUpdates::Latest, |card| {
        card_text(card).contains("已内嵌")
    })
    .await;

    let calls = platform.calls.lock().await.clone();
    assert!(calls.iter().any(|call| matches!(call, PlatformCall::EditMessage {
        message_id,
        msg_type,
        content,
        outcome: crate::feishu::MessageEditOutcome::Updated,
        ..
    } if message_id == "msg_file" && msg_type == "post"
        && content.to_string().contains("最新内容以文件查看卡或文件消息为准"))));
    assert_eq!(
        rich_text_messages(&platform).await.len(),
        1,
        "no replacement rich copy"
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
        text.contains("| r0 | 0 |") && text.contains("后续行未显示"),
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
        card_buttons(updated)
            .iter()
            .any(|b| b["text"]["content"] == "发为文件"),
        "the Rich Text preview keeps a transfer to the original bytes: {text}"
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
/// a FIFO is rejected promptly after the immediate ack.
#[cfg(unix)]
#[tokio::test]
async fn row_open_fifo_fails_promptly_after_the_callback_ack() {
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
    wait_for_card_update(&platform, "special-file row refusal", CardUpdates::Any, |card| {
        card_text(card).contains("not a regular file")
    })
    .await;
}

/// #741: a structurally-overflowing file opened from a Tool Panel's
/// 「📂 打开」 (#743) takes the same lane — the send goes out off the callback
/// path and a standalone viewer is replied in the live Turn's chat/thread,
/// which carries no listing to return to and no second delivery.
#[tokio::test]
async fn a_tool_open_of_an_overflowing_preview_sends_then_replies() {
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

    let updated = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(card) = platform.replied_cards().await.last().cloned()
                && card_text(&card).contains("已发送为富文本消息")
            {
                break card;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the standalone overflow viewer reply lands");
    let posts = rich_text_messages(&platform).await;
    assert_eq!(posts.len(), 1, "exactly one Rich Text Message: {posts:?}");
    match &posts[0] {
        PlatformCall::SendMessageReply { message_id, .. } => {
            assert_eq!(message_id, "msg_turn_card", "the reply targets the clicked card");
        }
        other => panic!("expected a post reply, got {other:?}"),
    }
    let text = card_text(&updated);
    assert_eq!(card_header(&updated), "📄 people.csv");
    assert!(
        text.contains("已发送为富文本消息"),
        "the standalone viewer says where the content went: {text}"
    );
    assert!(
        !text.contains("返回目录"),
        "no listing to return to, so no return control: {text}"
    );
    assert!(
        card_buttons(&updated)
            .iter()
            .any(|b| b["text"]["content"] == "发为文件"),
        "Rich Text carries a preview, not the original bytes: {text}"
    );
    assert!(
        patches_to(&platform, "msg_turn_card").await.is_empty(),
        "the Tool Panel card is not replaced by the overflow viewer"
    );
}

/// #778: a Tool Panel of an HTML page leads with 「🌐 预览网页」, and its click
/// sends the page as exactly ONE File Message WITHOUT PATCHing the clicked
/// message — that message is the agent's **Turn card**, and the transfer's own
/// card PATCH would overwrite the panel and the reply. The panel's structured
/// path is relative, so this also pins the Path Scope resolution the open lane
/// applies. The button stays on the card, so a duplicate click is refused by the
/// send memory instead of a PATCH.
#[tokio::test]
async fn a_tool_panel_html_preview_sends_a_file_message_and_leaves_the_turn_card_alone() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("index.html"), "<!doctype html><p>hi</p>\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    // The payload the panel's 「🌐 预览网页」 button carries (the Session, never
    // the Chat), plus the clicked Turn card the platform threads in.
    let mut payload = crate::feishu::card::session::tool_page_value("ses_seed", "index.html");
    payload["open_message_id"] = serde_json::json!("msg_turn_card");
    let result = app.host_action(payload.clone()).await.expect("the click acks");
    assert!(result.card.is_none(), "the click does not rebuild inline");
    assert_eq!(
        result.toast.as_deref(),
        Some("正在发送文件…"),
        "the callback acknowledges immediately"
    );

    let posts = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let posts = file_messages(&platform).await;
            if !posts.is_empty() {
                break posts;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the preview's File Message lands");
    assert_eq!(posts.len(), 1, "exactly one File Message: {posts:?}");
    match &posts[0] {
        PlatformCall::SendMessageReply {
            message_id, msg_type, ..
        } => {
            assert_eq!(message_id, "msg_turn_card", "the reply targets the clicked card");
            assert_eq!(msg_type, "file", "the page rides a File Message");
        }
        other => panic!("expected a file reply, got {other:?}"),
    }
    assert!(
        rich_text_messages(&platform).await.is_empty(),
        "an HTML page goes out as the file itself, not a Rich Text preview"
    );
    assert!(
        patches_to(&platform, "msg_turn_card").await.is_empty(),
        "the Tool Panel's Turn card is never PATCHed"
    );

    // A duplicate click is refused once the transfer lands — the spawned task
    // drops its in-flight claim and remembers the send, so the refusal is the
    // observable completion. Either way no second File Message is posted.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let toast = app
                .host_action(payload.clone())
                .await
                .expect("a duplicate click is acknowledged")
                .toast;
            if toast.as_deref() == Some("文件已发送") {
                break;
            }
            assert_eq!(
                toast.as_deref(),
                Some("正在发送文件…"),
                "only a held claim or a remembered send answers here"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the send is remembered, so a stale click is refused");
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "a duplicate click cannot post a second File Message"
    );
    assert!(
        patches_to(&platform, "msg_turn_card").await.is_empty(),
        "a duplicate click still never PATCHes the Turn card"
    );
}

/// #778: a preview with no file to send replies the failure under the clicked
/// Turn card — there is no card of cola's own to write `未发送` on, and no card
/// is PATCHed; nothing is sent.
#[tokio::test]
async fn a_tool_panel_preview_failure_speaks_a_reply() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    let mut payload = crate::feishu::card::session::tool_page_value("ses_seed", "gone.html");
    payload["open_message_id"] = serde_json::json!("msg_turn_card");
    let result = app.host_action(payload).await.expect("the click acks");
    assert_eq!(result.toast.as_deref(), Some("正在发送文件…"));

    let (message_id, text) = first_reply_text(&platform).await;
    assert_eq!(message_id, "msg_turn_card", "the reply targets the clicked card");
    assert!(
        text.contains("找不到路径") && text.contains("gone.html"),
        "the reply names the cause and the file: {text}"
    );
    assert!(
        file_messages(&platform).await.is_empty(),
        "a failed read sends nothing"
    );
    assert!(
        patches_to(&platform, "msg_turn_card").await.is_empty(),
        "a failed preview never PATCHes the Turn card"
    );
}

/// #778: a preview whose bytes cannot be delivered (past Feishu's 30 MB cap)
/// names that cause in its reply — the panel decides the button by EXTENSION, so
/// a `.html`-named file past the cap still tries to send and reports why it did
/// not.
#[tokio::test]
async fn a_tool_panel_preview_past_the_upload_cap_names_the_cap() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::File::create(proj.path().join("huge.html"))
        .unwrap()
        .set_len(crate::feishu::file::MAX_FILE_BYTES + 1)
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    let mut payload = crate::feishu::card::session::tool_page_value("ses_seed", "huge.html");
    payload["open_message_id"] = serde_json::json!("msg_turn_card");
    app.host_action(payload).await.expect("the click acks");

    let (_, text) = first_reply_text(&platform).await;
    assert!(
        text.contains("超过 30 MB") && text.contains("huge.html"),
        "the reply names the cap and the file: {text}"
    );
    assert!(uploads(&platform).await.is_empty(), "nothing is uploaded");
    assert!(file_messages(&platform).await.is_empty(), "nothing is sent");
    assert!(
        patches_to(&platform, "msg_turn_card").await.is_empty(),
        "the Turn card is never PATCHed"
    );
}

/// #778: a preview whose File Message timed out after Feishu may have accepted it
/// replies the honest unknown — there is no card of cola's own to state
/// `发送结果未知` on — and a stale click is refused rather than posting a duplicate.
#[tokio::test]
async fn a_tool_panel_preview_with_an_ambiguous_send_replies_the_unknown() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("index.html"), "<p>hi</p>\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    // A transfer bound so short the parked send cannot answer in time, after the
    // mock has already ACCEPTED it (the ambiguous case).
    app.viewer_transfer_timeout_ms
        .store(25, std::sync::atomic::Ordering::Relaxed);
    let key = thread();
    seed_entry(&app, seeded_entry(&key, &proj.path().to_string_lossy())).await;

    let mut payload = crate::feishu::card::session::tool_page_value("ses_seed", "index.html");
    payload["open_message_id"] = serde_json::json!("msg_turn_card");
    let (entered, _release) = platform.pause_after_accept("send_message", "msg_turn_card");
    app.host_action(payload.clone()).await.expect("the click acks");
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("the send reached Feishu before the response timed out");

    let (message_id, text) = first_reply_text(&platform).await;
    assert_eq!(message_id, "msg_turn_card", "the reply targets the clicked card");
    assert!(
        text.contains("发送结果未知"),
        "the reply is the honest unknown, not a silent success: {text}"
    );
    assert_eq!(file_messages(&platform).await.len(), 1, "Feishu accepted once");
    assert!(
        patches_to(&platform, "msg_turn_card").await.is_empty(),
        "the Turn card is never PATCHed"
    );
    let duplicate = app.host_action(payload).await.expect("a stale click acks");
    assert_eq!(duplicate.toast.as_deref(), Some("发送结果未知，为避免重复未重试"));
    assert_eq!(
        file_messages(&platform).await.len(),
        1,
        "an ambiguous send is never retried"
    );
}

/// #778: a preview whose Session the store no longer knows cannot route a reply
/// and answers an honest Toast — the same rule the open lane already follows —
/// rather than sending into the void.
#[tokio::test]
async fn a_tool_panel_preview_with_an_unknown_session_answers_honestly() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("index.html"), "<p>hi</p>\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let mut payload = crate::feishu::card::session::tool_page_value("ses_gone", "index.html");
    payload["open_message_id"] = serde_json::json!("msg_turn_card");
    let result = app.host_action(payload).await.expect("the click is acknowledged");
    assert!(result.card.is_none(), "no transfer without a route");
    let toast = result.toast.expect("an honest Toast");
    assert!(
        toast.contains("会话已不在") && toast.contains("index.html"),
        "the Toast names the cause and the path: {toast}"
    );
    assert!(file_messages(&platform).await.is_empty(), "nothing is sent");
}

/// #741: content that fits the card's structural limits stays on the card —
/// this lane is not taken for size alone. A large single code block that fits
/// the budget previews in place with no message of any kind.
#[tokio::test]
async fn fitting_content_stays_on_the_card() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("small.csv"), "name,age\nann,3\nbob,7\n").unwrap();
    // ~17 KB of lines: one whole fenced block now that the preview is no
    // longer cut per raw chunk (#769), still within one card's budget.
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
    let blocks = card["body"]["elements"]
        .as_array()
        .expect("the card body carries elements")
        .iter()
        .filter(|element| {
            element["content"]
                .as_str()
                .is_some_and(|content| content.starts_with("```"))
        })
        .count();
    assert_eq!(
        blocks, 1,
        "the whole preview is ONE element, never one per raw chunk (#769): {text}"
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

/// #740/#769: even a text file far past one card's preview budget remains in
/// the card lane; the renderer windows it by LINES and supplies a pager rather
/// than treating its length as structural overflow.
#[tokio::test]
async fn element_overflow_text_stays_on_the_card_pager() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    // ~1 MB of short lines: far more lines than one card can hold, but still
    // within the 8 MB preview cap.
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

    assert!(rich_text_messages(&platform).await.is_empty());
    assert!(file_messages(&platform).await.is_empty(), "text stays pageable");
    let card = first_card(&platform).await;
    let text = card_text(&card);
    assert_eq!(card_header(&card), "📄 long.log");
    assert!(
        pager_label(&card).is_some(),
        "the card renders the first page: {text}"
    );
    assert!(
        card_buttons(&card)
            .iter()
            .any(|button| button["value"]["op"] == "page"),
        "the card carries pager controls"
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
