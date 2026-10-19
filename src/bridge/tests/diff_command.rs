//! `/diff` — the repository review command (spec #803, ticket #804).
//!
//! These tests drive the real command seam (`send_command_in` → parse → gate →
//! `handle_command`) against real temporary git repositories and the recording
//! platform fake. They pin the artifact (exactly one `text/html` File Message,
//! named `review-…html`), the reply container (the command's own message id,
//! never a new thread), the config/explicit base override, the empty/error
//! text paths, and the per-chat cooldown.

use crate::bridge::test_support::*;
use crate::config::{ConversationKind, SessionEntry, ThreadKey};

/// A repo on branch `feature`, forked from `main`, carrying all three kinds of
/// change in one worktree: a commit on `feature`, an unstaged edit, and an
/// untracked file.
fn mixed_change_repo() -> tempfile::TempDir {
    let repo = git_repo();
    git_in(repo.path(), &["checkout", "-b", "feature"]);
    std::fs::write(repo.path().join("a.txt"), "hello\ncommitted").unwrap();
    git_in(repo.path(), &["add", "a.txt"]);
    git_in(repo.path(), &["commit", "-m", "feature change"]);
    std::fs::write(repo.path().join("a.txt"), "hello\ncommitted\nunstaged").unwrap();
    std::fs::write(repo.path().join("new.txt"), "untracked\n").unwrap();
    repo
}

fn short_head(repo: &tempfile::TempDir) -> String {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(out.status.success(), "rev-parse HEAD failed");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .chars()
        .take(7)
        .collect()
}

/// Seed a session whose directory is `repo` for `key`.
async fn seed_repo_session(
    app: &std::sync::Arc<crate::bridge::App>,
    key: ThreadKey,
    repo: &tempfile::TempDir,
) {
    seed_entry(
        app,
        SessionEntry::new(key, "ses_diff", repo.path().to_string_lossy().to_string()),
    )
    .await;
}

fn file_messages(calls: &[PlatformCall]) -> Vec<&PlatformCall> {
    calls
        .iter()
        .filter(|call| matches!(call, PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "file"))
        .collect()
}

fn uploads(calls: &[PlatformCall]) -> Vec<&PlatformCall> {
    calls
        .iter()
        .filter(|call| matches!(call, PlatformCall::UploadFile { .. }))
        .collect()
}

fn text_replies(calls: &[PlatformCall]) -> Vec<&str> {
    calls
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A mixed committed + unstaged + untracked worktree becomes exactly one HTML
/// File Message in the command's own container (its topic message id), with
/// the three changes present in the artifact.
#[tokio::test]
async fn diff_sends_one_html_file_into_the_command_container() {
    let repo = mixed_change_repo();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let key = ThreadKey::new("chat_diff_topic".into(), "omt_diff_topic".into());
    seed_repo_session(&app, key.clone(), &repo).await;
    send_command_in(
        &app,
        "/diff --base main",
        key,
        "msg_diff_topic",
        ConversationKind::Topic,
    )
    .await;

    let calls = platform.calls.lock().await.clone();
    let files = file_messages(&calls);
    assert_eq!(files.len(), 1, "exactly one File Message: {calls:?}");
    match files[0] {
        PlatformCall::SendMessageReply { message_id, .. } => assert_eq!(
            message_id, "msg_diff_topic",
            "the file replies to the command message (its topic)"
        ),
        _ => unreachable!(),
    }
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, PlatformCall::ReplyInThread { .. })),
        "the file must not start a new thread: {calls:?}"
    );

    let uploads = uploads(&calls);
    assert_eq!(uploads.len(), 1, "one upload: {calls:?}");
    let (name, mime, html) = match uploads[0] {
        PlatformCall::UploadFile {
            name, mime, content, ..
        } => (name, mime, content),
        _ => unreachable!(),
    };
    assert_eq!(mime, "text/html");
    assert_eq!(
        name,
        &format!("review-feature-{}.html", short_head(&repo)),
        "the branch and short SHA name the artifact"
    );
    let html = String::from_utf8_lossy(html);
    assert!(html.contains("a.txt"), "tracked file missing: {html}");
    assert!(
        html.contains("committed") && html.contains("unstaged"),
        "the committed and unstaged edits are both in the diff: {html}"
    );
    assert!(html.contains("new.txt"), "untracked file missing: {html}");
    assert!(
        text_replies(&calls).is_empty(),
        "a successful send needs no extra text: {calls:?}"
    );
}

/// `--base` wins over `[review] base` and reaches the producer: a bogus config
/// base cannot fail the call when the command names a good one, and the
/// subtitle shows the explicit ref.
#[tokio::test]
async fn explicit_base_overrides_the_configured_base() {
    let repo = mixed_change_repo();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.review.base = Some("does-not-exist".into());
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let key = ThreadKey::new("chat_diff_base".into(), "chat_diff_base".into());
    seed_repo_session(&app, key.clone(), &repo).await;
    send_command_in(
        &app,
        "/diff --base main",
        key,
        "msg_diff_base",
        ConversationKind::P2p,
    )
    .await;

    let calls = platform.calls.lock().await.clone();
    assert_eq!(file_messages(&calls).len(), 1, "{calls:?}");
    let html = match uploads(&calls)[0] {
        PlatformCall::UploadFile { content, .. } => String::from_utf8_lossy(content).into_owned(),
        _ => unreachable!(),
    };
    assert!(
        html.contains("main @"),
        "the subtitle names the explicit base: {html}"
    );
    assert!(
        !html.contains("does-not-exist"),
        "the configured base must be overridden: {html}"
    );
}

/// With no `--base`, `[review] base` is the base the command uses.
#[tokio::test]
async fn configured_base_is_used_without_a_flag() {
    let repo = mixed_change_repo();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.review.base = Some("main".into());
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let key = ThreadKey::new("chat_diff_cfg".into(), "chat_diff_cfg".into());
    seed_repo_session(&app, key.clone(), &repo).await;
    send_command_in(&app, "/diff", key, "msg_diff_cfg", ConversationKind::P2p).await;

    let calls = platform.calls.lock().await.clone();
    assert_eq!(file_messages(&calls).len(), 1, "{calls:?}");
    let html = match uploads(&calls)[0] {
        PlatformCall::UploadFile { content, .. } => String::from_utf8_lossy(content).into_owned(),
        _ => unreachable!(),
    };
    assert!(html.contains("main @"), "{html}");
}

/// A clean repository answers one line and sends nothing.
#[tokio::test]
async fn a_clean_repository_answers_text_with_no_file() {
    let repo = git_repo();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let key = ThreadKey::new("chat_diff_clean".into(), "chat_diff_clean".into());
    seed_repo_session(&app, key.clone(), &repo).await;
    send_command_in(
        &app,
        "/diff --base main",
        key,
        "msg_diff_clean",
        ConversationKind::P2p,
    )
    .await;

    let calls = platform.calls.lock().await.clone();
    assert!(uploads(&calls).is_empty(), "no file: {calls:?}");
    assert!(file_messages(&calls).is_empty(), "no file: {calls:?}");
    let texts = text_replies(&calls);
    assert_eq!(texts.len(), 1, "{calls:?}");
    assert!(texts[0].contains("没有改动"), "{}", texts[0]);
    assert!(texts[0].contains("main"), "{}", texts[0]);
}

/// A non-git directory answers its own error, with no file.
#[tokio::test]
async fn a_non_git_directory_answers_its_error_with_no_file() {
    let repo = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let key = ThreadKey::new("chat_diff_nongit".into(), "chat_diff_nongit".into());
    seed_repo_session(&app, key.clone(), &repo).await;
    send_command_in(&app, "/diff", key, "msg_diff_nongit", ConversationKind::P2p).await;

    let calls = platform.calls.lock().await.clone();
    assert!(uploads(&calls).is_empty(), "{calls:?}");
    assert!(file_messages(&calls).is_empty(), "{calls:?}");
    let texts = text_replies(&calls);
    assert_eq!(texts.len(), 1, "{calls:?}");
    assert!(texts[0].contains("不是 git 仓库"), "{}", texts[0]);
}

/// A base ref that does not exist answers its own error, with no file.
#[tokio::test]
async fn a_missing_base_answers_its_error_with_no_file() {
    let repo = git_repo();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let key = ThreadKey::new("chat_diff_nobase".into(), "chat_diff_nobase".into());
    seed_repo_session(&app, key.clone(), &repo).await;
    send_command_in(
        &app,
        "/diff --base no-such-base",
        key,
        "msg_diff_nobase",
        ConversationKind::P2p,
    )
    .await;

    let calls = platform.calls.lock().await.clone();
    assert!(uploads(&calls).is_empty(), "{calls:?}");
    assert!(file_messages(&calls).is_empty(), "{calls:?}");
    let texts = text_replies(&calls);
    assert_eq!(texts.len(), 1, "{calls:?}");
    assert!(texts[0].contains("找不到基准分支"), "{}", texts[0]);
}

/// A repeat inside the 10-second window answers text and performs no second
/// upload (cooling down before the expensive path).
#[tokio::test]
async fn a_repeat_inside_the_cooldown_sends_no_second_file() {
    let repo = mixed_change_repo();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let key = ThreadKey::new("chat_diff_repeat".into(), "chat_diff_repeat".into());
    seed_repo_session(&app, key.clone(), &repo).await;
    send_command_in(
        &app,
        "/diff --base main",
        key.clone(),
        "msg_diff_first",
        ConversationKind::P2p,
    )
    .await;
    send_command_in(
        &app,
        "/diff --base main",
        key,
        "msg_diff_second",
        ConversationKind::P2p,
    )
    .await;

    let calls = platform.calls.lock().await.clone();
    assert_eq!(uploads(&calls).len(), 1, "one upload only: {calls:?}");
    assert_eq!(file_messages(&calls).len(), 1, "one file only: {calls:?}");
    let texts = text_replies(&calls);
    assert_eq!(texts.len(), 1, "{calls:?}");
    assert!(texts[0].contains("稍后再试"), "{}", texts[0]);
}

/// Unsupported argument shapes answer the one usage line without touching git
/// or the platform's file lane.
#[tokio::test]
async fn unsupported_arguments_answer_usage_only() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let key = ThreadKey::new("chat_diff_usage".into(), "chat_diff_usage".into());
    seed_entry(
        &app,
        SessionEntry::new(key.clone(), "ses_diff", dir.path().to_string_lossy().to_string()),
    )
    .await;
    for (i, text) in ["/diff --other", "/diff --base", "/diff extra words"]
        .iter()
        .enumerate()
    {
        send_command_in(
            &app,
            text,
            key.clone(),
            &format!("msg_diff_usage_{i}"),
            ConversationKind::P2p,
        )
        .await;
    }

    let calls = platform.calls.lock().await.clone();
    assert!(uploads(&calls).is_empty(), "{calls:?}");
    assert!(file_messages(&calls).is_empty(), "{calls:?}");
    let texts = text_replies(&calls);
    assert_eq!(texts.len(), 3, "{calls:?}");
    for text in texts {
        assert!(
            text.ends_with("用法：`/diff [--base <ref>]`"),
            "one usage line: {text}"
        );
    }
}
