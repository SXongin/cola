//! `/worktree` (ADR-0088): the card, the create command, and the pick that
//! declares a Pending Session in a checkout. Every worktree here is a real one
//! in a throwaway git repository — the point of the command is git's own
//! state, so a mock would test nothing.

use crate::bridge::test_support::*;

fn key() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A committed repository — `git worktree add` needs a HEAD.
fn init_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "test"]);
    std::fs::write(repo.path().join("a.txt"), "hello").unwrap();
    git(repo.path(), &["add", "a.txt"]);
    git(repo.path(), &["commit", "-m", "init"]);
    repo
}

fn repo_canonical(repo: &tempfile::TempDir) -> std::path::PathBuf {
    std::fs::canonicalize(repo.path()).unwrap()
}

/// `/worktree new` creates the checkout, declares a Pending Session titled
/// with the slug, and creates NO server session (ADR-0041 + ADR-0088).
#[tokio::test]
async fn worktree_new_creates_a_checkout_and_declares_a_pending() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    cfg.worktree.root = std::path::PathBuf::from(".worktrees");
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;

    send_command_in(
        &app,
        "/worktree new Fix Login",
        key(),
        "msg_worktree_new",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let expected = repo_canonical(&repo).join(".worktrees").join("fix-login");
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("/worktree new declares a pending");
    assert_eq!(pending.directory, expected.to_string_lossy());
    assert_eq!(
        pending.title.as_deref(),
        Some("fix-login"),
        "the pending is titled with the slug"
    );
    assert!(expected.is_dir(), "the checkout exists on disk");
    assert!(created.lock().await.is_empty(), "no server session is created");
    git(repo.path(), &["rev-parse", "--verify", "refs/heads/wt/fix-login"]);

    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("已创建 worktree"), "{texts}");
    assert!(texts.contains("wt/fix-login"), "{texts}");
    assert!(
        texts.contains("未被 `.gitignore` 忽略"),
        "an unignored root is reported: {texts}"
    );
}

/// The card's data carries the main checkout first, the linked one with its
/// branch, the dirty state and the session count (ADR-0088).
#[tokio::test]
async fn worktree_card_data_lists_main_and_linked_checkouts() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    git(
        repo.path(),
        &["worktree", "add", "-b", "wt/feature", ".worktrees/feature"],
    );
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    let repo_dir = repo_canonical(&repo).to_string_lossy().to_string();
    let worktree_dir = repo_canonical(&repo)
        .join(".worktrees/feature")
        .to_string_lossy()
        .to_string();
    let mut backend = MockBackend::new(realistic_parts());
    backend.given_sessions(vec![
        list_session("ses_main", "主会话", &repo_dir, 100),
        list_session("ses_wt", "工作树会话", &worktree_dir, 200),
    ]);
    let (app, _platform) = build_app(cfg, backend).await;

    let (rows, listed_repo) =
        crate::feishu::card::command::worktree_card_data(&app.command_handles(), &key())
            .await
            .expect("a git repository lists its checkouts");
    assert_eq!(
        std::fs::canonicalize(&listed_repo).unwrap(),
        repo_canonical(&repo),
        "the card names the project directory"
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows[0].main, "the main checkout is first and labelled");
    assert_eq!(rows[0].branch.as_deref(), Some("main"));
    assert_eq!(rows[0].sessions, 1);
    assert!(!rows[1].main);
    assert_eq!(rows[1].branch.as_deref(), Some("wt/feature"));
    assert_eq!(rows[1].sessions, 1);
    assert_eq!(rows[0].dirty, Some(true), "the untracked root dirties main");
    assert_eq!(rows[1].dirty, Some(false));
    assert!(
        rows[0].current,
        "with no session or pending, the current marker falls back to work_dir (ADR-0088)"
    );
    assert!(!rows[1].current);

    let card = crate::feishu::card::session::build_worktree_card(&key(), &rows, &listed_repo).to_string();
    assert!(card.contains("主检出"), "{card}");
    assert!(card.contains("wt/feature"), "{card}");
    assert!(card.contains("有未提交改动"), "{card}");
    assert!(card.contains("1 个会话"), "{card}");
    assert!(card.contains("\"action\":\"worktree\""), "{card}");
}

/// A non-git project answers text, not an empty card.
#[tokio::test]
async fn worktree_card_refuses_a_non_git_project() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let plain = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(plain.path().to_path_buf());
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    send_command_in(
        &app,
        "/worktree",
        key(),
        "msg_worktree_plain",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("不是 git 仓库"), "{texts}");
}

/// Picking a checkout declares a Pending Session there, titled with the row's
/// name, and refreshes the card with that row marked 当前 (ADR-0088).
#[tokio::test]
async fn worktree_card_pick_declares_a_pending() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    git(
        repo.path(),
        &["worktree", "add", "-b", "wt/feature", ".worktrees/feature"],
    );
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let worktree = repo_canonical(&repo).join(".worktrees/feature");

    let value = serde_json::json!({
        "action": "worktree",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": worktree.to_string_lossy(),
        "name": "feature",
    });
    let result = app
        .host_action(value)
        .await
        .expect("worktree pick should return a result");
    let toast = result.toast.clone().unwrap_or_default();
    assert!(toast.contains("下一条消息"), "{toast}");
    assert!(toast.contains(".worktrees/feature"), "{toast}");

    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pick declares a pending");
    assert_eq!(pending.directory, worktree.to_string_lossy());
    assert_eq!(pending.title.as_deref(), Some("feature"));

    let card = result.card.expect("the pick refreshes the card").to_string();
    assert!(
        card.contains("**feature** · ✅ 当前"),
        "the picked checkout is the current row: {card}"
    );
}

/// A payload directory that exists but is NOT a listed checkout is refused:
/// a stale card must not turn an ordinary folder into the project directory.
#[tokio::test]
async fn worktree_card_pick_refuses_a_directory_that_is_not_a_worktree() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    std::fs::create_dir_all(repo.path().join(".worktrees/plain")).unwrap();
    let plain = repo_canonical(&repo).join(".worktrees/plain");
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let value = serde_json::json!({
        "action": "worktree",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": plain.to_string_lossy(),
        "name": "plain",
    });
    let result = app
        .host_action(value)
        .await
        .expect("worktree pick should return a result");
    let toast = result.toast.clone().unwrap_or_default();
    assert!(toast.contains("已不是 worktree"), "{toast}");
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "a non-worktree directory never becomes a pending"
    );
}

/// A checkout removed between render and click is a toast, never a pending on
/// a missing directory (ADR-0088).
#[tokio::test]
async fn worktree_card_pick_of_a_missing_checkout_toasts() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let missing = repo_canonical(&repo).join(".worktrees").join("gone");
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    let value = serde_json::json!({
        "action": "worktree",
        "op": "pick",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "directory": missing.to_string_lossy(),
        "name": "gone",
    });
    let result = app
        .host_action(value)
        .await
        .expect("worktree pick should return a result");
    let toast = result.toast.clone().unwrap_or_default();
    assert!(toast.contains("目录已不存在"), "{toast}");
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "a missing directory never becomes a pending"
    );
}
