//! `/worktree` (ADR-0088): the read-only card and the create command. Every
//! worktree here is a real one in a throwaway git repository — the point of the
//! command is git's own state, so a mock would test nothing.

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

/// The repository path as cola stores it: canonical, without Windows' verbatim
/// prefix (git rejects `\\?\` paths, so production strips it too).
fn repo_canonical(repo: &tempfile::TempDir) -> std::path::PathBuf {
    crate::git::plain_path(std::fs::canonicalize(repo.path()).unwrap())
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
        crate::git::plain_path(std::fs::canonicalize(&listed_repo).unwrap()),
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

    let card = crate::feishu::card::session::build_worktree_card(&rows, &listed_repo).to_string();
    assert!(card.contains("主检出"), "{card}");
    assert!(card.contains("wt/feature"), "{card}");
    assert!(card.contains("有未提交改动"), "{card}");
    assert!(card.contains("1 个会话"), "{card}");
    assert!(
        !card.contains("\"tag\":\"button\""),
        "the list is read-only: {card}"
    );
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
/// A payload directory that exists but is NOT a listed checkout is refused:
/// a stale card must not turn an ordinary folder into the project directory.
/// A checkout removed between render and click is a toast, never a pending on
/// a missing directory (ADR-0088).
/// A name the slug rule refuses answers the reason and declares nothing.
#[tokio::test]
async fn worktree_new_refuses_a_chinese_name() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    send_command_in(
        &app,
        "/worktree new 用户指南",
        key(),
        "msg_worktree_cn",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("失败"), "{texts}");
    assert!(texts.contains("只能包含英文"), "{texts}");
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "a refused name declares no pending"
    );
}

/// `--base` reaches git and the reply names it, so the flag is not merely
/// parsed (the git test pins the commit; this pins the surface).
#[tokio::test]
async fn worktree_new_passes_the_base_ref_through() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let first = git_out(repo.path(), &["rev-parse", "HEAD"]);
    std::fs::write(repo.path().join("b.txt"), "two").unwrap();
    git(repo.path(), &["add", "b.txt"]);
    git(repo.path(), &["commit", "-m", "second"]);
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    send_command_in(
        &app,
        &format!("/worktree new based --base {first}"),
        key(),
        "msg_worktree_base",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the command declares a pending");
    assert!(std::path::Path::new(&pending.directory).is_dir());
    assert_eq!(
        git_out(std::path::Path::new(&pending.directory), &["rev-parse", "HEAD"]),
        first
    );
    let texts = platform.texts().await.join("\n");
    assert!(texts.contains(&first), "the reply names the base: {texts}");
}

/// Picking the checkout the conversation already sits in is a no-op toast.
/// Unknown ops and empty payloads are answered without touching git or the
/// store.
/// The card in a project that is not a git repository answers the reason
/// instead of rendering an empty list.
/// `git rev-parse …` in a test repository, trimmed.
fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The issue's first acceptance bullet end to end: after `/worktree new`, the
/// conversation's next message creates its session in the checkout.
#[tokio::test]
async fn the_next_message_creates_the_session_in_the_worktree() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    send_command_in(
        &app,
        "/worktree new next-msg",
        key(),
        "msg_worktree_next",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let expected = repo_canonical(&repo)
        .join(".worktrees")
        .join("next-msg")
        .to_string_lossy()
        .to_string();
    assert!(
        created.lock().await.is_empty(),
        "the command declares, it never creates"
    );

    app.handle_message(incoming(
        "msg_worktree_after".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "开始吧".into(),
        None,
    ))
    .await;

    assert_eq!(
        created.lock().await.last().and_then(|dir| dir.as_deref()),
        Some(expected.as_str()),
        "the next message materialises the session in the worktree"
    );
}

/// #450: a Session whose checkout was removed ends with an explicit ending
/// before dispatch. The server would answer with its own error ("the directory's
/// instance cannot boot"), so the prompt path checks locally first and names
/// the one route out; no card, no claim, no prompt.
#[tokio::test]
async fn a_prompt_in_a_removed_worktree_ends_before_dispatch() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    git(
        repo.path(),
        &["worktree", "add", "-b", "wt/gone", ".worktrees/gone"],
    );
    let gone = repo_canonical(&repo).join(".worktrees").join("gone");
    git(repo.path(), &["worktree", "remove", ".worktrees/gone"]);
    assert!(!gone.exists(), "the checkout is really removed");

    let cfg = test_config(&sessions.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;
    app.check_missing_directories
        .store(true, std::sync::atomic::Ordering::Relaxed);
    seed_session(&app, "ses_gone", &gone.to_string_lossy()).await;

    app.handle_message(incoming(
        "msg_gone".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "继续".into(),
        None,
    ))
    .await;

    assert!(
        prompts.lock().await.is_empty(),
        "a removed directory never reaches the prompt"
    );
    assert!(created.lock().await.is_empty(), "and never creates a session");
    assert!(
        platform.replied_cards().await.is_empty(),
        "no turn card opens for a prompt that cannot run"
    );
    let texts = platform.texts().await.join("\n");
    assert!(
        texts.contains("工作目录已不存在"),
        "the ending names the missing directory: {texts}"
    );
    assert!(
        texts.contains(".worktrees") && texts.contains("gone"),
        "the ending names the removed checkout (Windows uses `\\`): {texts}"
    );
    assert!(
        texts.contains("`/dir") && texts.contains("`/switch"),
        "the ending names both routes out: {texts}"
    );
}

/// #450 + ADR-0041: a Pending Session whose directory disappeared is refused
/// the same way — its first message must not try to create a Session there —
/// and the pending stays intact so `/dir` can re-root it without losing the
/// chosen overrides.
#[tokio::test]
async fn a_pending_in_a_removed_worktree_ends_before_materialising() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    cfg.worktree.root = std::path::PathBuf::from(".worktrees");
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;
    app.check_missing_directories
        .store(true, std::sync::atomic::Ordering::Relaxed);

    send_command(&app, "/worktree new gone", "msg_worktree_gone").await;
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("/worktree new declares a pending");
    git(repo.path(), &["worktree", "remove", ".worktrees/gone"]);
    assert!(
        !std::path::Path::new(&pending.directory).exists(),
        "the pending's checkout is gone"
    );

    app.handle_message(incoming(
        "msg_pending_gone".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "开始吧".into(),
        None,
    ))
    .await;

    assert!(
        created.lock().await.is_empty(),
        "a pending in a removed directory creates nothing"
    );
    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("工作目录已不存在"), "{texts}");
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_some(),
        "the pending survives so /dir can re-root it"
    );
}
