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

/// The environment names a Terminal Command's child must not inherit
/// (ADR-0086 §8).
#[cfg(unix)]
static CREDENTIAL_ENV: [&str; 3] = [
    "OPENCODE_SERVER_PASSWORD",
    "OPENCODE_PASSWORD",
    "OPENCODE_SERVER_USERNAME",
];

/// Build an app whose **Terminal Log** root lives in a temp directory, so a
/// test never leaves command output under the real home. The returned
/// `TempDir` must stay alive for the test's duration.
#[cfg(unix)]
async fn build_terminal_app(
    cfg: crate::config::Config,
    backend: MockBackend,
) -> (
    std::sync::Arc<crate::bridge::App>,
    std::sync::Arc<RecordingPlatform>,
    tempfile::TempDir,
) {
    let logs = tempfile::tempdir().unwrap();
    let (app, platform) = build_app(cfg, backend).await;
    app.core
        .terminal_log_root
        .set(logs.path().join("terminal"))
        .expect("the Terminal Log root is set once per app");
    (app, platform, logs)
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
#[cfg(unix)]
fn canonical(path: &std::path::Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// The one card the command replied with.
#[cfg(unix)]
async fn first_card(platform: &RecordingPlatform) -> serde_json::Value {
    platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the command replies with a Terminal Card")
}

#[cfg(unix)]
#[tokio::test]
async fn run_echo_replies_with_a_terminal_card_naming_cwd_command_output_exit_and_elapsed() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform, _logs) = build_terminal_app(cfg, backend).await;
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
#[cfg(unix)]
#[tokio::test]
async fn a_failing_command_settles_with_its_exit_code_and_stderr() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
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
#[cfg(unix)]
#[tokio::test]
async fn a_pending_sessions_directory_is_the_cwd() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
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
#[cfg(unix)]
#[tokio::test]
async fn the_default_session_directory_is_the_cwd_without_a_session() {
    let _wd = test_work_dir();
    let fallback = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&state.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(fallback.path().to_path_buf());
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;

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
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
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
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
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

    let dir = app
        .core
        .terminal_log_root
        .get()
        .expect("the test installed a temp Terminal Log root")
        .clone();
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
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
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

/// ADR-0086 §8: the environment is inherited MINUS cola's own credentials.
/// The three names are set for the duration of this test (nothing else reads
/// them); `printenv` proves the child cannot see any of them, and a clean
/// `PATH` read proves the rest of the environment still is inherited.
#[cfg(unix)]
#[tokio::test]
async fn the_child_environment_scrubs_cola_credentials() {
    struct RestoreEnv(&'static [&'static str]);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            for name in self.0 {
                // The test runs no other env mutation, and no production code
                // reads these names in-process.
                unsafe { std::env::remove_var(name) };
            }
        }
    }

    for name in CREDENTIAL_ENV {
        // Rust 2024 marks env mutation unsafe (it is process-global); tests
        // are the crate's one place unsafe is allowed.
        unsafe { std::env::set_var(name, "cola-secret-822") };
    }
    let _restore = RestoreEnv(&CREDENTIAL_ENV);

    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    for (index, name) in CREDENTIAL_ENV.iter().enumerate() {
        send_command_in(
            &app,
            &format!("/run printenv {name}"),
            key.clone(),
            &format!("msg_env_{index}"),
            ConversationKind::P2p,
        )
        .await;
    }

    let cards = platform.replied_cards().await;
    assert_eq!(cards.len(), CREDENTIAL_ENV.len(), "one card per probe");
    for (card, name) in cards.iter().zip(CREDENTIAL_ENV) {
        let text = card_text(card);
        assert!(
            text.contains("退出码 1"),
            "`{name}` is unset in the child: {text}"
        );
        assert!(
            !text.contains("cola-secret-822"),
            "the credential value never reaches the child: {text}"
        );
    }
}

/// The card family's 3,000-character per-element budget (ADR-0086 §4): a body
/// of nothing but backticks grows the fence with it, so this is exactly where
/// clipping the body before fencing would overrun the element. The output is
/// cut and the card says so.
#[cfg(unix)]
#[tokio::test]
async fn a_backtick_heavy_output_stays_within_the_element_budget() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    // 3,000 backticks: past the tail budget, then fence-sized by `fenced_code`.
    send_command_in(
        &app,
        "/run printf '%3000s' | tr ' ' '\\140'",
        key,
        "msg_run_backticks",
        ConversationKind::P2p,
    )
    .await;

    let card = first_card(&platform).await;
    let texts = card_texts(&card);
    for text in &texts {
        assert!(
            text.chars().count() <= crate::feishu::card::MAX_ELEMENT_TEXT_CHARS,
            "every element stays inside the per-element budget ({} chars): {text}",
            text.chars().count()
        );
    }
    assert!(
        texts.join("\n").contains("输出过长"),
        "a cut display says so: {texts:?}"
    );
}

/// On Windows the command is refused with a stated reason and nothing is
/// spawned: no card, no prompt, no log.
#[cfg(not(unix))]
#[tokio::test]
async fn windows_refuses_the_command_with_a_reason_and_spawns_nothing() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(&app, "/run echo hi", key, "msg_windows", ConversationKind::P2p).await;

    let texts = platform.texts().await;
    assert!(
        texts
            .iter()
            .any(|text| text.contains("Windows") && text.contains("不支持")),
        "the refusal states why: {texts:?}"
    );
    assert!(
        platform.replied_cards().await.is_empty(),
        "a refused command posts no Terminal Card"
    );
    assert!(prompts.lock().await.is_empty(), "and becomes no prompt");
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
#[cfg(unix)]
#[tokio::test]
async fn run_usage_and_flag_forms_are_answered_without_spawning() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform, _logs) = build_terminal_app(cfg, backend).await;
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
#[cfg(unix)]
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
    let (app, platform, _logs) = build_terminal_app(cfg, backend).await;
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

/// The second entry point (spec #821, ticket #824; ADR-0086 §1): a message
/// whose first character is `!` runs the remainder as the command and answers
/// the same Terminal Card — the `/run` execution path, not a copy of it.
#[cfg(unix)]
#[tokio::test]
async fn a_bang_prefixed_message_runs_the_remainder_as_a_terminal_command() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform, _logs) = build_terminal_app(cfg, backend).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(&app, "!echo bang hi", key, "msg_bang", ConversationKind::P2p).await;

    let cards = platform.replied_cards().await;
    assert_eq!(cards.len(), 1, "exactly one Terminal Card: {cards:?}");
    let text = card_text(&cards[0]);
    assert!(
        text.contains(&canonical(proj.path())),
        "the card names the resolved cwd: {text}"
    );
    assert!(
        text.contains("echo bang hi"),
        "the card names the command: {text}"
    );
    assert!(text.contains("bang hi"), "the card carries the output: {text}");
    assert!(text.contains("退出码 0"), "the exit code is 0: {text}");
    assert!(text.contains("耗时"), "the elapsed time is named: {text}");
    assert!(
        prompts.lock().await.is_empty(),
        "a `!` message never becomes a prompt"
    );
}

/// Only the FIRST character opens the entry point: `!` alone, `! ` (spaces
/// only) and a `!` anywhere else stay prose and reach the model as before.
#[tokio::test]
async fn bang_alone_bang_space_and_a_mid_message_bang_stay_prose() {
    let _wd = test_work_dir();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    for (message_id, text) in [
        ("msg_bare_bang", "!"),
        ("msg_bang_space", "!   "),
        ("msg_mid_bang", "hello !ls"),
        ("msg_inner_bang", "ls ! la"),
    ] {
        send_command(&app, text, message_id).await;
    }

    let terminal_cards = platform
        .replied_cards()
        .await
        .iter()
        .filter(|card| card.to_string().contains("🖥️ 终端命令"))
        .count();
    assert_eq!(
        terminal_cards, 0,
        "no prose message spawns a command or posts a Terminal Card"
    );
    assert_eq!(
        *prompts.lock().await,
        vec![
            "!".to_string(),
            "!   ".to_string(),
            "hello !ls".to_string(),
            "ls ! la".to_string(),
        ],
        "every non-qualifying message reaches the model verbatim"
    );
}

/// The text the bridge parses is already mention-stripped and trimmed, so
/// `@cola !ls` (the bot's mention dropped upstream) and `  !ls` qualify —
/// while the stripped result is still prose when its first character is not
/// `!`.
#[cfg(unix)]
#[tokio::test]
async fn a_mention_stripped_or_indented_bang_still_qualifies() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform, _logs) = build_terminal_app(cfg, backend).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    // The production WS path strips the bot's own mention and trims; the test
    // composes that same step instead of pretending the raw token arrives.
    let mention = crate::feishu::event::Mention {
        key: Some("@_user_1".into()),
        id: Some(crate::feishu::event::MentionId {
            open_id: Some("ou_bot".into()),
        }),
        name: Some("cola".into()),
    };
    let stripped = crate::feishu::message::strip_mentions("@_user_1 !echo mentioned", &[mention], "ou_bot");
    assert_eq!(
        stripped, "!echo mentioned",
        "the mention strip leaves the prefix first"
    );

    app.handle_message(incoming(
        "msg_mentioned_bang".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        stripped,
        None,
    ))
    .await;

    let text = card_text(&first_card(&platform).await);
    assert!(text.contains("mentioned"), "the stripped `!` command ran: {text}");
    assert!(text.contains("退出码 0"), "and settled normally: {text}");

    send_command_in(
        &app,
        "  !echo indented",
        key,
        "msg_indented_bang",
        ConversationKind::P2p,
    )
    .await;
    let cards = platform.replied_cards().await;
    let text = card_text(cards.last().expect("the indented form's card"));
    assert!(text.contains("indented"), "a leading-whitespace `!` runs: {text}");
    assert!(prompts.lock().await.is_empty(), "neither form became a prompt");
}

/// A `!` message never scans for `#<id>` Skill tokens: `!ls #caveman` runs the
/// literal command (the `#caveman` is part of it) and loads no Skill, so no
/// skill list is read and no prompt is submitted.
#[cfg(unix)]
#[tokio::test]
async fn a_bang_command_with_a_hash_token_loads_no_skill() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_skills(vec![crate::backend::SkillInfo {
        id: "caveman".into(),
        name: "Caveman".into(),
        description: Some("Talk like caveman.".into()),
        content: Some("# Caveman".into()),
    }]);
    let prompts = backend.prompt_calls.clone();
    let skill_reads = backend.list_skills_calls.clone();
    let (app, platform, _logs) = build_terminal_app(cfg, backend).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(
        &app,
        "!echo '#caveman'",
        key,
        "msg_bang_hash",
        ConversationKind::P2p,
    )
    .await;

    let text = card_text(&first_card(&platform).await);
    assert!(
        text.contains("#caveman"),
        "the literal `#caveman` is the command's argument: {text}"
    );
    assert!(text.contains("退出码 0"), "the literal command ran: {text}");
    assert!(
        prompts.lock().await.is_empty(),
        "the message never became a prompt"
    );
    assert_eq!(
        skill_reads.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no Skill list was read"
    );
}

/// An unrelated `/foo` still forwards to the model exactly as before: the `!`
/// interception is a first-character check, not a change to the command parser.
#[tokio::test]
async fn an_unrelated_slash_command_still_forwards_to_the_model() {
    let _wd = test_work_dir();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompts = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    send_command(&app, "/please-read", "msg_forward").await;

    assert_eq!(
        *prompts.lock().await,
        vec!["/please-read".to_string()],
        "an unknown `/` command is forwarded verbatim"
    );
    assert!(
        platform
            .replied_cards()
            .await
            .iter()
            .all(|card| !card.to_string().contains("🖥️ 终端命令")),
        "the forward posts no Terminal Card"
    );
}

/// `/run` is in the command help list, and `/help run` documents both entry
/// points, the cap and `--timeout`, the absent terminal and the Windows
/// refusal.
#[tokio::test]
async fn help_lists_run_and_documents_both_entry_points() {
    let _wd = test_work_dir();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    send_command(&app, "/help", "msg_help").await;
    let cards = platform.replied_cards().await;
    assert_eq!(cards.len(), 1, "`/help` replies with the command card");
    let listed = card_text(&cards[0]);
    assert!(listed.contains("/run"), "the help card lists `/run`: {listed}");
    assert!(
        listed.contains("!命令") || listed.contains("!<命令>"),
        "the help card names the `!` entry point: {listed}"
    );

    send_command(&app, "/help run", "msg_help_run").await;
    let texts = platform.texts().await;
    let help = texts.last().expect("`/help run` replies with text");
    for expected in [
        "/run",
        "!<cmd>",
        "10 minutes",
        "24 hours",
        "--timeout",
        "terminal",
        "Windows",
    ] {
        assert!(
            help.contains(expected),
            "`/help run` must document {expected:?}: {help}"
        );
    }
}
