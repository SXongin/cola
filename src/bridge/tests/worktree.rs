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

/// `/new --worktree` (ADR-0095) records an intent whose directory is the
/// SOURCE, creates nothing, and materialises the checkout on the first message.
#[tokio::test]
async fn new_worktree_declares_an_intent_and_materialises_lazily() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    cfg.worktree.root = std::path::PathBuf::from(".worktrees");
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let head = git_out(repo.path(), &["rev-parse", "HEAD"]);
    let target = repo_canonical(&repo).join(".worktrees").join("fix-login");

    send_command_in(
        &app,
        "/new --worktree --name Fix Login",
        key(),
        "msg_new_worktree",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("/new --worktree declares a pending");
    assert_eq!(
        pending.directory,
        repo.path().to_string_lossy(),
        "the pending's directory is the SOURCE checkout"
    );
    assert_eq!(pending.title.as_deref(), Some("Fix Login"));
    let intent = pending.worktree.as_ref().expect("the intent rides the pending");
    assert_eq!(intent.slug, "fix-login", "an ASCII title seeds the slug");
    assert_eq!(intent.base, "main", "the ladder resolves the integration line");
    assert_eq!(intent.base_sha, head);
    assert_eq!(intent.target, target.to_string_lossy());
    assert_eq!(intent.path, None);
    assert!(!target.is_dir(), "no checkout exists before the first message");
    assert!(created.lock().await.is_empty(), "no server session is created");
    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("已记下 worktree 意图"), "{texts}");
    assert!(texts.contains("wt/fix-login"), "{texts}");
    assert!(texts.contains("main @"), "{texts}");

    app.handle_message(incoming(
        "msg_new_worktree_after".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "开始吧".into(),
        None,
    ))
    .await;

    assert_eq!(
        created.lock().await.last().and_then(|dir| dir.as_deref()),
        Some(target.to_string_lossy().as_ref()),
        "the session is created in the checkout"
    );
    assert!(target.is_dir(), "the first message creates the checkout");
    git(repo.path(), &["rev-parse", "--verify", "refs/heads/wt/fix-login"]);
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "materialisation clears the pending"
    );
}

/// The topic form shows the target, branch and resolved base on the cover
/// BEFORE the first message, so the user can still change the name (ADR-0095).
#[tokio::test]
async fn topic_worktree_cover_names_the_target_branch_and_base() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    cfg.worktree.root = std::path::PathBuf::from(".worktrees");
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;

    send_command(
        &app,
        "/topic --worktree --name 修复登录 --slug fix-login",
        "msg_topic_worktree_new",
    )
    .await;

    let cover = platform
        .sent_cards()
        .await
        .into_iter()
        .next()
        .map(|card| card.to_string())
        .expect("a cover card is sent");
    assert!(cover.contains("修复登录"), "{cover}");
    assert!(
        cover.contains("🌲"),
        "the cover marks the future checkout: {cover}"
    );
    assert!(cover.contains("wt/fix-login"), "{cover}");
    assert!(cover.contains(".worktrees/fix-login"), "{cover}");
    assert!(cover.contains("main @"), "{cover}");

    let topic_key = crate::config::ThreadKey::new("chat_1".into(), "omt_created_topic".into());
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&topic_key)
        .cloned()
        .expect("the topic carries a pending");
    assert_eq!(pending.title.as_deref(), Some("修复登录"));
    assert_eq!(
        pending.directory,
        repo.path().to_string_lossy(),
        "pending-window commands read the source, not the target"
    );
    let intent = pending.worktree.as_ref().expect("the intent rides the pending");
    assert_eq!(intent.slug, "fix-login", "an explicit slug wins over the title");
    assert!(
        created.lock().await.is_empty(),
        "opening the topic creates no session"
    );
}

/// A non-git source and a Chinese `--slug` refuse at command time, before any
/// pending is written (ADR-0095).
#[tokio::test]
async fn worktree_flag_refuses_a_non_git_source_and_a_chinese_slug() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let plain = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(plain.path().to_path_buf());
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    send_command(&app, "/new --worktree", "msg_wt_plain").await;
    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("不是 git 仓库"), "{texts}");
    assert!(app.sessions.lock().await.pending_for(&key()).is_none());

    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    let repo_cfg = cfg.clone();
    let (app, platform) = build_app(repo_cfg, MockBackend::new(realistic_parts())).await;
    send_command(&app, "/new --worktree --slug 用户指南", "msg_wt_cn").await;
    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("只能包含英文"), "{texts}");
    assert!(app.sessions.lock().await.pending_for(&key()).is_none());
}

/// A Chinese title with no `--slug` falls back to the deterministic date
/// token; the title itself stays Chinese and is PATCHed at materialisation.
#[tokio::test]
async fn a_chinese_title_falls_back_to_the_date_slug() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    send_command(&app, "/new --worktree --name 修复登录", "msg_wt_cn_title").await;

    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the command declares a pending");
    let intent = pending.worktree.expect("intent");
    let date = intent.slug.strip_suffix("-1").expect("the date token ends in -1");
    assert_eq!(date.len(), 8, "YYYYMMDD: {}", intent.slug);
    assert!(date.chars().all(|ch| ch.is_ascii_digit()), "{}", intent.slug);
}

/// A session-create failure keeps the pending and the message, and the retry
/// reuses the checkout the first attempt already made — never a `-2` sibling.
#[tokio::test]
async fn a_failed_session_create_retries_the_same_checkout() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    cfg.worktree.root = std::path::PathBuf::from(".worktrees");
    let mut backend = MockBackend::new(realistic_parts());
    backend.fail_create_sessions(1);
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let target = repo_canonical(&repo).join(".worktrees").join("retry");

    send_command(&app, "/new --worktree --name retry", "msg_wt_retry").await;
    app.handle_message(incoming(
        "msg_wt_retry_1".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "开始吧".into(),
        None,
    ))
    .await;
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("a failed session create keeps the pending");
    assert_eq!(
        pending.worktree.expect("intent").path.as_deref(),
        Some(target.to_string_lossy().as_ref()),
        "the created checkout is recorded before the session exists"
    );
    assert!(target.is_dir());

    app.handle_message(incoming(
        "msg_wt_retry_2".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "再试一次".into(),
        None,
    ))
    .await;
    let dirs = created.lock().await.clone();
    assert_eq!(
        dirs.last().and_then(|dir| dir.as_deref()),
        Some(target.to_string_lossy().as_ref()),
        "the retry reuses the same checkout: {dirs:?}"
    );
    assert!(
        !repo_canonical(&repo).join(".worktrees").join("retry-2").exists(),
        "no -2 sibling was created"
    );
    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("创建会话失败"), "{texts}");
}

/// A checkout-creation failure at materialisation keeps the pending (and the
/// message) so the next message retries (ADR-0095 decision 6).
#[tokio::test]
async fn a_failed_checkout_keeps_the_pending() {
    let _wd = test_work_dir();
    let sessions = tempfile::tempdir().unwrap();
    let repo = init_repo();
    let mut cfg = test_config(&sessions.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(repo.path().to_path_buf());
    cfg.worktree.root = std::path::PathBuf::from(".worktrees");
    let backend = MockBackend::new(realistic_parts());
    let created = backend.created_session_dirs.clone();
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/new --worktree --name gone", "msg_wt_gone").await;
    std::fs::remove_dir_all(repo.path()).unwrap();
    app.handle_message(incoming(
        "msg_wt_gone_after".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "开始吧".into(),
        None,
    ))
    .await;

    assert!(created.lock().await.is_empty(), "no session is created");
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_some(),
        "the pending survives for a retry"
    );
    let texts = platform.texts().await.join("\n");
    assert!(texts.contains("创建会话失败"), "{texts}");
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
