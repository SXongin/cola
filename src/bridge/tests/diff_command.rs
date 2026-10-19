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
    // A second base ref at the earlier (initial) commit, so the configured-base
    // test can prove `[review] base` is read instead of the `main` fallback.
    git_in(repo.path(), &["branch", "dev"]);
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

/// One `/diff` test fixture: a command app whose session is rooted at a test
/// directory, plus the recording platform. `chat_id` stays unique per test (it
/// also keys the cooldown bucket); `thread_id` differs from it only in the
/// topic test. The config dir must outlive the app's session writes.
struct DiffFixture {
    _config_dir: tempfile::TempDir,
    app: std::sync::Arc<crate::bridge::App>,
    platform: std::sync::Arc<RecordingPlatform>,
    key: ThreadKey,
}

impl DiffFixture {
    /// Send one command through the production parse → gate → dispatch seam.
    async fn send(&self, text: &str, message_id: &str, kind: ConversationKind) {
        send_command_in(&self.app, text, self.key.clone(), message_id, kind).await;
    }

    async fn calls(&self) -> Vec<PlatformCall> {
        self.platform.calls.lock().await.clone()
    }
}

/// Build a fixture whose session is rooted at `directory`, applying
/// `configure` to the test config before the app is built (`[review] base`
/// tests). Each caller keeps its own chat id.
async fn diff_fixture_with(
    directory: &std::path::Path,
    chat_id: &str,
    thread_id: &str,
    configure: impl FnOnce(&mut crate::config::Config),
) -> DiffFixture {
    let config_dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&config_dir.path().join("sessions.json"));
    configure(&mut cfg);
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = ThreadKey::new(chat_id.to_string(), thread_id.to_string());
    seed_entry(
        &app,
        SessionEntry::new(key.clone(), "ses_diff", directory.to_string_lossy().to_string()),
    )
    .await;
    DiffFixture {
        _config_dir: config_dir,
        app,
        platform,
        key,
    }
}

/// [`diff_fixture_with`] on the default test config.
async fn diff_fixture(directory: &std::path::Path, chat_id: &str, thread_id: &str) -> DiffFixture {
    diff_fixture_with(directory, chat_id, thread_id, |_| {}).await
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
    let fx = diff_fixture(repo.path(), "chat_diff_topic", "omt_diff_topic").await;
    fx.send("/diff --base main", "msg_diff_topic", ConversationKind::Topic)
        .await;

    let calls = fx.calls().await;
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
    let fx = diff_fixture_with(repo.path(), "chat_diff_base", "chat_diff_base", |cfg| {
        cfg.review.base = Some("does-not-exist".into());
    })
    .await;
    fx.send("/diff --base main", "msg_diff_base", ConversationKind::P2p)
        .await;

    let calls = fx.calls().await;
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

/// With no `--base`, `[review] base` is the base the command uses. The fixture
/// has an earlier `dev` ref, so this fails if the config is ignored and the
/// producer falls back to `main`.
#[tokio::test]
async fn configured_base_is_used_without_a_flag() {
    let repo = mixed_change_repo();
    let fx = diff_fixture_with(repo.path(), "chat_diff_cfg", "chat_diff_cfg", |cfg| {
        cfg.review.base = Some("dev".into());
    })
    .await;
    fx.send("/diff", "msg_diff_cfg", ConversationKind::P2p).await;

    let calls = fx.calls().await;
    assert_eq!(file_messages(&calls).len(), 1, "{calls:?}");
    let html = match uploads(&calls)[0] {
        PlatformCall::UploadFile { content, .. } => String::from_utf8_lossy(content).into_owned(),
        _ => unreachable!(),
    };
    assert!(
        html.contains("dev @"),
        "the configured base names the subtitle: {html}"
    );
    assert!(
        !html.contains("main @"),
        "the `main` fallback must not win over `[review] base`: {html}"
    );
}

/// A clean repository answers one line and sends nothing.
#[tokio::test]
async fn a_clean_repository_answers_text_with_no_file() {
    let repo = git_repo();
    let fx = diff_fixture(repo.path(), "chat_diff_clean", "chat_diff_clean").await;
    fx.send("/diff --base main", "msg_diff_clean", ConversationKind::P2p)
        .await;

    let calls = fx.calls().await;
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
    let fx = diff_fixture(repo.path(), "chat_diff_nongit", "chat_diff_nongit").await;
    fx.send("/diff", "msg_diff_nongit", ConversationKind::P2p).await;

    let calls = fx.calls().await;
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
    let fx = diff_fixture(repo.path(), "chat_diff_nobase", "chat_diff_nobase").await;
    fx.send(
        "/diff --base no-such-base",
        "msg_diff_nobase",
        ConversationKind::P2p,
    )
    .await;

    let calls = fx.calls().await;
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
    let fx = diff_fixture(repo.path(), "chat_diff_repeat", "chat_diff_repeat").await;
    fx.send("/diff --base main", "msg_diff_first", ConversationKind::P2p)
        .await;
    fx.send("/diff --base main", "msg_diff_second", ConversationKind::P2p)
        .await;

    let calls = fx.calls().await;
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
    let repo = tempfile::tempdir().unwrap();
    let fx = diff_fixture(repo.path(), "chat_diff_usage", "chat_diff_usage").await;
    for (i, text) in ["/diff --other", "/diff --base", "/diff extra words"]
        .iter()
        .enumerate()
    {
        fx.send(text, &format!("msg_diff_usage_{i}"), ConversationKind::P2p)
            .await;
    }

    let calls = fx.calls().await;
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

/// A branch carrying `/` is sanitized into the artifact's name, which still
/// ends `.html` (the ticket's filename acceptance, end to end through `/diff`).
#[tokio::test]
async fn a_branch_with_a_slash_is_sanitized_in_the_file_name() {
    let repo = git_repo();
    git_in(repo.path(), &["checkout", "-b", "feature/diff"]);
    std::fs::write(repo.path().join("a.txt"), "hello\nbranch").unwrap();
    git_in(repo.path(), &["add", "a.txt"]);
    git_in(repo.path(), &["commit", "-m", "branch change"]);

    let fx = diff_fixture(repo.path(), "chat_diff_slash", "chat_diff_slash").await;
    fx.send("/diff --base main", "msg_diff_slash", ConversationKind::P2p)
        .await;

    let calls = fx.calls().await;
    assert_eq!(file_messages(&calls).len(), 1, "{calls:?}");
    let name = match uploads(&calls)[0] {
        PlatformCall::UploadFile { name, .. } => name,
        _ => unreachable!(),
    };
    assert_eq!(
        name,
        &format!("review-feature_diff-{}.html", short_head(&repo)),
        "the slash is sanitized and .html is kept"
    );
    assert!(name.ends_with(".html"), "{name}");
}

/// A detached HEAD is named `detached-<short>` and still ends `.html`.
#[tokio::test]
async fn a_detached_head_is_named_detached_html() {
    let repo = mixed_change_repo();
    git_in(repo.path(), &["checkout", "--detach", "HEAD"]);

    let fx = diff_fixture(repo.path(), "chat_diff_detached", "chat_diff_detached").await;
    fx.send("/diff --base main", "msg_diff_detached", ConversationKind::P2p)
        .await;

    let calls = fx.calls().await;
    assert_eq!(file_messages(&calls).len(), 1, "{calls:?}");
    let name = match uploads(&calls)[0] {
        PlatformCall::UploadFile { name, .. } => name,
        _ => unreachable!(),
    };
    assert_eq!(name, &format!("review-detached-{}.html", short_head(&repo)));
    assert!(name.ends_with(".html"), "{name}");
}

/// An unborn repository (no commit yet) is named `unborn-unborn` and still
/// ends `.html`; its staged file makes the review non-empty.
#[tokio::test]
async fn an_unborn_repository_is_named_unborn_unborn_html() {
    let repo = tempfile::tempdir().unwrap();
    git_in(repo.path(), &["init", "-b", "main"]);
    git_in(repo.path(), &["config", "user.email", "test@example.com"]);
    git_in(repo.path(), &["config", "user.name", "test"]);
    std::fs::write(repo.path().join("first.txt"), "first\n").unwrap();
    git_in(repo.path(), &["add", "first.txt"]);

    let fx = diff_fixture(repo.path(), "chat_diff_unborn", "chat_diff_unborn").await;
    fx.send("/diff", "msg_diff_unborn", ConversationKind::P2p).await;

    let calls = fx.calls().await;
    assert_eq!(file_messages(&calls).len(), 1, "{calls:?}");
    let name = match uploads(&calls)[0] {
        PlatformCall::UploadFile { name, .. } => name,
        _ => unreachable!(),
    };
    assert_eq!(name, "review-unborn-unborn.html");
    assert!(name.ends_with(".html"), "{name}");
}

/// An empty unborn repository (no commit, no files) has nothing to compare;
/// naming `unborn` relative to `unborn` would read like an error, so it answers
/// the no-commit line and sends no file.
#[tokio::test]
async fn an_empty_unborn_repository_answers_no_commits() {
    let repo = tempfile::tempdir().unwrap();
    git_in(repo.path(), &["init", "-b", "main"]);
    git_in(repo.path(), &["config", "user.email", "test@example.com"]);
    git_in(repo.path(), &["config", "user.name", "test"]);

    let fx = diff_fixture(repo.path(), "chat_diff_unborn_empty", "chat_diff_unborn_empty").await;
    fx.send("/diff", "msg_diff_unborn_empty", ConversationKind::P2p)
        .await;

    let calls = fx.calls().await;
    assert!(uploads(&calls).is_empty(), "no file: {calls:?}");
    assert!(file_messages(&calls).is_empty(), "no file: {calls:?}");
    let texts = text_replies(&calls);
    assert_eq!(texts.len(), 1, "{calls:?}");
    assert_eq!(texts[0], "仓库还没有提交，没有可对比的改动。");
    assert!(
        !texts[0].contains("unborn"),
        "the sentinel never reaches the user: {}",
        texts[0]
    );
}

/// An ambiguous send answers the shared unknown-send line, keeps its single
/// upload, and never retries (a retry could duplicate the File Message).
#[tokio::test]
async fn an_ambiguous_send_answers_unknown_and_does_not_retry() {
    let repo = mixed_change_repo();
    let fx = diff_fixture(repo.path(), "chat_diff_unknown", "chat_diff_unknown").await;
    fx.platform
        .ambiguous_send_message_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    fx.send("/diff --base main", "msg_diff_unknown", ConversationKind::P2p)
        .await;

    let calls = fx.calls().await;
    assert_eq!(uploads(&calls).len(), 1, "exactly one upload: {calls:?}");
    assert!(
        file_messages(&calls).is_empty(),
        "the ambiguous send records no accepted file: {calls:?}"
    );
    let texts = text_replies(&calls);
    assert_eq!(texts.len(), 1, "{calls:?}");
    assert_eq!(texts[0], crate::bridge::viewer::FILE_SEND_UNKNOWN_TEXT);
}

/// A definite upload failure answers one readable failure line and sends no
/// file.
#[tokio::test]
async fn a_definite_send_failure_answers_the_failure_line() {
    let repo = mixed_change_repo();
    let fx = diff_fixture(repo.path(), "chat_diff_failed", "chat_diff_failed").await;
    fx.platform
        .fail_upload_file_count
        .store(1, std::sync::atomic::Ordering::SeqCst);
    fx.send("/diff --base main", "msg_diff_failed", ConversationKind::P2p)
        .await;

    let calls = fx.calls().await;
    assert!(file_messages(&calls).is_empty(), "{calls:?}");
    let texts = text_replies(&calls);
    assert_eq!(texts.len(), 1, "{calls:?}");
    assert_eq!(texts[0], "⚠️ 文件发送失败，请稍后重试。");
}
