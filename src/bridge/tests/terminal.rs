//! `/run <cmd>` — the **Terminal Command** tracer bullet (spec #821, ticket
//! #822, ADR-0086).
//!
//! Every test drives the real message path (`send_command_in` → the
//! coordinator) against a **real `sh`** in a temporary cwd, then asserts the
//! **Terminal Card** JSON the Platform fake recorded — the same seam the
//! `/cat` and `/dir` suites use. What is pinned is the ticket's contract: the
//! resolved cwd, the command and its merged output, the real exit code, the
//! elapsed time, the closed stdin, the **Terminal Log**'s 0700/0600 location,
//! the child's own process group, the flag grammar, and that no OpenCode
//! server is contacted.

use crate::bridge::test_support::*;
use crate::config::{ConversationKind, SessionEntry, ThreadKey};

fn thread() -> ThreadKey {
    ThreadKey::new("chat_1".into(), "chat_1".into())
}

/// A seeded active session rooted at `directory`.
fn entry(key: &ThreadKey, directory: &std::path::Path) -> SessionEntry {
    SessionEntry {
        thread_key: key.clone(),
        session_id: "ses_seed".into(),
        directory: directory.to_string_lossy().to_string(),
        agent: None,
        model: None,
        auto_accept: false,
        topic_anchor: None,
        topic_root: None,
        variant: None,
    }
}

/// The canonical spelling of `path` — what the card must name, since the cwd it
/// ran in is the resolved one.
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
        .expect("the command replies with a Terminal Card")
}

#[tokio::test]
async fn run_echo_replies_with_a_terminal_card_naming_cwd_command_output_exit_and_elapsed() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(&app, "/run echo hi", key, "msg_run", ConversationKind::P2p).await;

    let cards = platform.replied_cards().await;
    assert_eq!(cards.len(), 1, "exactly one Terminal Card: {cards:?}");
    let text = card_text(&cards[0]);
    assert!(
        text.contains(&canonical(proj.path())),
        "the card names the resolved cwd: {text}"
    );
    assert!(text.contains("echo hi"), "the card names the command: {text}");
    assert!(text.contains("hi"), "the card carries the output: {text}");
    assert!(text.contains("退出码 0"), "the exit code is 0: {text}");
    assert!(text.contains("耗时"), "the elapsed time is named: {text}");
    assert!(
        prompts.lock().await.is_empty(),
        "a Terminal Command never becomes a prompt"
    );
}

/// A failing command settles with its REAL non-zero exit code and its stderr
/// (merged into the log), never an empty card.
#[tokio::test]
async fn a_failing_command_settles_with_its_exit_code_and_stderr() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(
        &app,
        "/run printf 'out-line\\n'; printf 'err-line\\n' >&2; exit 7",
        key,
        "msg_run_fail",
        ConversationKind::P2p,
    )
    .await;

    let text = card_text(&first_card(&platform).await);
    assert!(text.contains("退出码 7"), "the real exit code: {text}");
    assert!(text.contains("err-line"), "stderr is shown: {text}");
    assert!(text.contains("out-line"), "stdout is shown: {text}");
    assert!(
        !text.contains("（无输出）"),
        "a failing run is never empty: {text}"
    );
}

/// The command runs in the **Path Scope**'s base: a Pending Session's declared
/// directory wins, exactly as it does for `/cat` and `/diff`.
#[tokio::test]
async fn a_pending_sessions_directory_is_the_cwd() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();

    send_command_in(
        &app,
        &format!("/dir {}", proj.path().display()),
        key.clone(),
        "msg_dir",
        ConversationKind::P2p,
    )
    .await;
    send_command_in(&app, "/run pwd", key, "msg_pending_run", ConversationKind::P2p).await;

    let text = card_text(&first_card(&platform).await);
    let resolved = canonical(proj.path());
    assert!(text.contains(&resolved), "the card names the pending cwd: {text}");
    assert!(
        text.matches(&resolved).count() >= 2,
        "both the cwd line and `pwd`'s output carry it: {text}"
    );
}

/// With neither a Pending Session nor an active one, the run happens in the
/// default session directory and the card names it.
#[tokio::test]
async fn the_default_session_directory_is_the_cwd_without_a_session() {
    let _wd = test_work_dir();
    let fallback = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&state.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(fallback.path().to_path_buf());
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    send_command(&app, "/run pwd", "msg_default_run").await;

    let text = card_text(&first_card(&platform).await);
    let resolved = canonical(fallback.path());
    assert!(text.contains(&resolved), "the card names the default cwd: {text}");
}

/// stdin is closed: a command that reads it sees EOF at once instead of
/// waiting on cola's terminal.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn stdin_is_closed_so_a_reading_command_exits_immediately() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(
        &app,
        "/run readlink /proc/self/fd/0",
        key.clone(),
        "msg_stdin_probe",
        ConversationKind::P2p,
    )
    .await;
    let text = card_text(&first_card(&platform).await);
    assert!(
        text.contains("/dev/null"),
        "the child's stdin is /dev/null: {text}"
    );

    // The direct statement of the criterion: `cat` reads stdin, and with the
    // fd closed it must settle rather than hang. The probe above has already
    // proved the fd; this bounds the behaviour end to end.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        send_command_in(&app, "/run cat", key, "msg_stdin_cat", ConversationKind::P2p),
    )
    .await
    .expect("a command reading stdin exits instead of hanging");
    let cards = platform.replied_cards().await;
    let text = card_text(cards.last().expect("cat's card"));
    assert!(text.contains("退出码 0"), "cat ends on EOF: {text}");
}

/// The output lands in a **Terminal Log** inside a 0700 directory, in a 0600
/// file — command output is where credentials leak (ADR-0086 §7, §12).
#[cfg(unix)]
#[tokio::test]
async fn the_terminal_log_lives_in_a_0700_directory_as_a_0600_file() {
    use std::os::unix::fs::PermissionsExt;

    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    let marker = "terminal_log_822_marker";
    send_command_in(
        &app,
        &format!("/run echo {marker}"),
        key,
        "msg_run_log",
        ConversationKind::P2p,
    )
    .await;
    first_card(&platform).await;

    let dir = crate::bridge::terminal::terminal_root();
    let mode = std::fs::metadata(&dir)
        .expect("the Terminal Log directory exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "the Terminal Log directory is private: {dir:?}");

    let log = find_log_containing(&dir, marker);
    let mode = std::fs::metadata(&log)
        .expect("the Terminal Log exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the Terminal Log file is private: {log:?}");
    let content = std::fs::read_to_string(&log).unwrap();
    assert!(content.contains(marker), "the log holds the command's bytes");
}

/// The child leaves cola's process group via `process_group(0)`: it is its own
/// group leader, so a terminal hangup (and later 终止) does not reach cola.
#[cfg(unix)]
#[tokio::test]
async fn the_child_leads_its_own_process_group() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    // True iff the shell's pgid is its own pid — the definition of a group
    // leader. An inherited group would carry cola's pgid instead.
    send_command_in(
        &app,
        "/run [ \"$(ps -o pgid= -p $$ | tr -d ' ')\" = \"$$\" ]",
        key,
        "msg_run_pgid",
        ConversationKind::P2p,
    )
    .await;

    let text = card_text(&first_card(&platform).await);
    assert!(
        text.contains("退出码 0"),
        "the child is its own process-group leader: {text}"
    );
}

/// Find the Terminal Log whose bytes carry `marker` (parallel tests share the
/// one 0700 directory, so the content — not the newest file — identifies it).
#[cfg(unix)]
fn find_log_containing(dir: &std::path::Path, marker: &str) -> std::path::PathBuf {
    for entry in std::fs::read_dir(dir).expect("read the Terminal Log directory") {
        let path = entry.expect("directory entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("log") {
            continue;
        }
        if std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains(marker)
        {
            return path;
        }
    }
    panic!("no Terminal Log under {dir:?} carries {marker}");
}

/// The bare `/run` and every refused flag form answer usage and spawn nothing;
/// `--timeout` and `--` parse as ADR-0086 §6 says.
#[tokio::test]
async fn run_usage_and_flag_forms_are_answered_without_spawning() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    for (message_id, text) in [
        ("msg_bare", "/run"),
        ("msg_bare_timeout", "/run --timeout"),
        ("msg_bad_timeout", "/run --timeout abc echo hi"),
        ("msg_long_timeout", "/run --timeout 25h echo hi"),
    ] {
        send_command_in(&app, text, key.clone(), message_id, ConversationKind::P2p).await;
    }
    assert!(
        platform.replied_cards().await.is_empty(),
        "no refused form spawns a command or posts a card"
    );
    let texts = platform.texts().await;
    assert!(
        texts.iter().all(|text| text.contains("用法")),
        "every refused form answers usage: {texts:?}"
    );
    assert!(prompts.lock().await.is_empty(), "none becomes a prompt");

    // The accepted forms do run: a cap, and the `--` terminator.
    send_command_in(
        &app,
        "/run --timeout 5s echo capped",
        key.clone(),
        "msg_timeout_ok",
        ConversationKind::P2p,
    )
    .await;
    let text = card_text(&first_card(&platform).await);
    assert!(text.contains("capped"), "the capped form runs: {text}");

    send_command_in(
        &app,
        "/run -- echo terminator",
        key,
        "msg_terminator_ok",
        ConversationKind::P2p,
    )
    .await;
    let cards = platform.replied_cards().await;
    let text = card_text(cards.last().expect("the terminator form's card"));
    assert!(text.contains("terminator"), "the `--` form runs: {text}");
}

/// No OpenCode server is contacted: `/run` works with `start_server = "never"`
/// and an unreachable server.
#[tokio::test]
async fn run_works_with_the_server_down_and_never_contacts_it() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&state.path().join("sessions.json"));
    cfg.opencode.start_server = crate::config::ServerStartPolicy::Never;
    cfg.opencode.url = Some("http://127.0.0.1:9".into());
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(
        &app,
        "/run echo offline",
        key,
        "msg_offline",
        ConversationKind::P2p,
    )
    .await;

    let text = card_text(&first_card(&platform).await);
    assert!(text.contains("offline"), "the command ran with no server: {text}");
    assert!(text.contains("退出码 0"), "and settled normally: {text}");
    assert!(prompts.lock().await.is_empty(), "no prompt reached the Backend");
}
