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
#[cfg(unix)]
use crate::feishu::card::terminal::TERMINAL_TAIL_CHARS;

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

/// The card's settled state: the last in-place update. Since #823 the reply is
/// the live 「运行中」 card and the settled exit code always arrives by PATCH;
/// the reply is only a defensive fallback for a path that wrote no update.
#[cfg(unix)]
async fn settled_card(platform: &RecordingPlatform) -> serde_json::Value {
    if let Some(card) = platform.updated_cards().await.last() {
        return card.clone();
    }
    platform
        .replied_cards()
        .await
        .into_iter()
        .last()
        .expect("the command replies with a Terminal Card")
}

/// The body of the card's fenced output element (`**输出**` → the fence body),
/// so a budget assertion measures the tail alone rather than the whole card.
#[cfg(unix)]
fn output_body(card: &serde_json::Value) -> String {
    let content = card_texts(card)
        .into_iter()
        .find(|content| content.starts_with("**输出**"))
        .expect("the card carries an output element");
    let fenced = content
        .strip_prefix("**输出**\n")
        .expect("the output element starts with its label");
    let body = fenced.split_once('\n').expect("the fence has a body line").1;
    body.rsplit_once('\n').expect("the fence closes").0.to_string()
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
    let text = card_text(&settled_card(&platform).await);
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

    let text = card_text(&settled_card(&platform).await);
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

    let text = card_text(&settled_card(&platform).await);
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

    let text = card_text(&settled_card(&platform).await);
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
    let text = card_text(&settled_card(&platform).await);
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
    let text = card_text(&settled_card(&platform).await);
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
    settled_card(&platform).await;

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

    let text = card_text(&settled_card(&platform).await);
    assert!(
        text.contains("退出码 0"),
        "the child is its own process-group leader: {text}"
    );
}

/// ADR-0086 §8: the environment is inherited MINUS cola's own credentials.
/// The values are injected through the spawn path's test-only inherited-env
/// list, so they sit in the child's environment exactly where the real process
/// environment would — without mutating the test process (unsound under
/// parallel tests). `printenv` proves the scrub removed each one.
#[cfg(unix)]
#[tokio::test]
async fn the_child_environment_scrubs_cola_credentials() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    {
        let mut inherited = app.core.terminal_test_env.lock().unwrap();
        for name in CREDENTIAL_ENV {
            inherited.push((name.to_string(), "cola-secret-822".to_string()));
        }
    }
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

    let settled: Vec<_> = platform
        .updated_cards()
        .await
        .into_iter()
        .filter(|card| card_text(card).contains("退出码"))
        .collect();
    assert_eq!(settled.len(), CREDENTIAL_ENV.len(), "one settled card per probe");
    for (card, name) in settled.iter().zip(CREDENTIAL_ENV) {
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

/// The card family's 3,000-character per-element budget (ADR-0086 §4): backtick
/// lines grow the fence with their longest run, so this is where clipping the
/// body before fencing would overrun the element or drop the newest line. A
/// 3,000-backtick prefix is followed by `ERROR: boom`: the card must stay inside
/// the budget and keep the diagnostic.
#[cfg(unix)]
#[tokio::test]
async fn a_backtick_heavy_output_keeps_the_newest_line_within_the_budget() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    // Eight 375-backtick lines (3,000 total, each under the per-line cap) then
    // the diagnostic. The fence is sized by the run, so the card must drop the
    // old prefix rather than the last line.
    send_command_in(
        &app,
        "/run for i in 1 2 3 4 5 6 7 8; do printf '%375s\\n' | tr ' ' '\\140'; done; printf 'ERROR: boom\\n'",
        key,
        "msg_run_backticks",
        ConversationKind::P2p,
    )
    .await;

    let card = settled_card(&platform).await;
    let texts = card_texts(&card);
    for text in &texts {
        assert!(
            text.chars().count() <= crate::feishu::card::MAX_ELEMENT_TEXT_CHARS,
            "every element stays inside the per-element budget ({} chars): {text}",
            text.chars().count()
        );
    }
    let joined = texts.join("\n");
    let output = texts
        .iter()
        .find(|text| text.starts_with("**输出**"))
        .expect("the card carries an output element");
    assert!(
        output.contains("ERROR: boom"),
        "the newest diagnostic survives the clip: {output}"
    );
    assert!(joined.contains("输出过长"), "a cut display says so: {texts:?}");
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
    let text = card_text(&settled_card(&platform).await);
    assert!(text.contains("capped"), "the capped form runs: {text}");

    send_command_in(
        &app,
        "/run -- echo terminator",
        key,
        "msg_terminator_ok",
        ConversationKind::P2p,
    )
    .await;
    let text = card_text(&settled_card(&platform).await);
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

    let text = card_text(&settled_card(&platform).await);
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
    let text = card_text(&settled_card(&platform).await);
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

    let text = card_text(&settled_card(&platform).await);
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
    let text = card_text(&settled_card(&platform).await);
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

    let text = card_text(&settled_card(&platform).await);
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

/// #823: the card is posted live and patched on the render cadence while the
/// command runs; the settling write is last and nothing follows it.
#[cfg(unix)]
#[tokio::test]
async fn the_card_is_live_while_the_command_runs_and_settles_last() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);

    send_command_in(
        &app,
        "/run printf 'early-token-823\\n'; sleep 1; printf 'late-token-823\\n'",
        key,
        "msg_live",
        ConversationKind::P2p,
    )
    .await;

    let replies = platform.replied_cards().await;
    assert_eq!(replies.len(), 1, "one Terminal Card: {replies:?}");
    let live = card_text(&replies[0]);
    assert!(
        live.contains("运行中"),
        "the first reply is the live card: {live}"
    );
    assert!(
        live.contains("等待输出"),
        "the live card starts before the child has written: {live}"
    );

    let updates = platform.updated_cards().await;
    assert!(
        updates.iter().any(|card| {
            let text = card_text(card);
            text.contains("运行中") && text.contains("early-token-823")
        }),
        "a live patch shows output written before the run settled: {updates:?}"
    );
    let settled = card_text(updates.last().expect("a settling write"));
    assert!(settled.contains("退出码 0"), "the last write settles: {settled}");
    assert!(
        settled.contains("late-token-823"),
        "it holds the full output: {settled}"
    );
    assert!(
        !settled.contains("运行中"),
        "the last write is not live: {settled}"
    );

    // The loop stopped on settle: no write follows the settling write.
    let count = platform.updated_cards().await.len();
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert_eq!(
        platform.updated_cards().await.len(),
        count,
        "no patch follows the settling write"
    );
}

/// #823: `\r` is an overwrite, so a progress line collapses to its last frame
/// instead of carrying every frame the command wrote.
#[cfg(unix)]
#[tokio::test]
async fn carriage_return_frames_collapse_to_the_last_frame() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(
        &app,
        "/run printf 'frame-aaa-823\\rframe-bbb-823\\rfinal-frame-823\\nplain-line-823\\n'; sleep 0.2",
        key,
        "msg_cr",
        ConversationKind::P2p,
    )
    .await;

    let body = output_body(&settled_card(&platform).await);
    assert!(
        body.contains("final-frame-823"),
        "the last frame survives: {body}"
    );
    assert!(body.contains("plain-line-823"), "the next line survives: {body}");
    assert!(!body.contains("frame-aaa-823"), "earlier frames are gone: {body}");
    assert!(
        !body.contains("frame-bbb-823"),
        "only the last frame remains: {body}"
    );
    assert_eq!(
        body.matches("final-frame-823").count(),
        1,
        "the last frame appears once: {body}"
    );
}

/// #823: ANSI and control sequences are stripped from the display and kept
/// byte-for-byte in the Terminal Log.
#[cfg(unix)]
#[tokio::test]
async fn ansi_is_stripped_from_the_card_and_kept_in_the_log() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    let marker = "ansi-823-marker";
    send_command_in(
        &app,
        &format!("/run printf '\\033[31m{marker}\\033[0m\\a\\n'; sleep 0.2"),
        key,
        "msg_ansi",
        ConversationKind::P2p,
    )
    .await;

    let body = output_body(&settled_card(&platform).await);
    assert!(body.contains(marker), "the text itself renders: {body}");
    assert!(!body.contains('\u{1b}'), "no ESC reaches the display: {body:?}");
    assert!(
        !body.contains("[31m"),
        "no CSI body reaches the display: {body:?}"
    );
    assert!(!body.contains('\u{7}'), "no BEL reaches the display: {body:?}");

    let dir = app
        .core
        .terminal_log_root
        .get()
        .expect("the test installed a temp Terminal Log root")
        .clone();
    let log = find_log_containing(&dir, marker);
    let raw = std::fs::read(&log).expect("read the Terminal Log");
    assert!(
        raw.windows(5).any(|w| w == b"\x1b[31m"),
        "the log keeps the escape sequence unchanged"
    );
    assert!(raw.contains(&0x07), "the log keeps the control byte unchanged");
}

/// #823 / review: the other string controls — DCS (`ESC P … ST`) and APC
/// (`ESC _ … ST`) — carry payloads just like OSC and must not leak into the
/// display. Their bytes stay in the Terminal Log as evidence.
#[cfg(unix)]
#[tokio::test]
async fn dcs_and_apc_payloads_are_stripped_from_the_card() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    let dcs_payload = "dcs-payload-823";
    let apc_payload = "apc-payload-823";
    // `\033P…\033\\` is DCS, `\033_…\033\\` is APC; `printf` turns the octal
    // and the doubled backslash into the real introducer and ST.
    let command = format!(
        "/run printf 'dcs-before-823-\\033P1;2|{dcs_payload}\\033\\\\dcs-after-823\\n\\033_{apc_payload}\\033\\\\apc-after-823\\n'; sleep 0.2"
    );
    send_command_in(&app, &command, key, "msg_string_control", ConversationKind::P2p).await;

    let body = output_body(&settled_card(&platform).await);
    assert!(
        body.contains("dcs-before-823"),
        "text before the DCS survives: {body}"
    );
    assert!(
        body.contains("dcs-after-823"),
        "text after the DCS survives: {body}"
    );
    assert!(
        !body.contains(dcs_payload) && !body.contains("1;2|"),
        "the DCS payload is not displayed: {body}"
    );
    assert!(
        body.contains("apc-after-823"),
        "text after the APC survives: {body}"
    );
    assert!(
        !body.contains(apc_payload),
        "the APC payload is not displayed: {body}"
    );
    assert!(!body.contains('\u{1b}'), "no ESC reaches the display: {body:?}");

    let dir = app
        .core
        .terminal_log_root
        .get()
        .expect("the test installed a temp Terminal Log root")
        .clone();
    let raw = std::fs::read(find_log_containing(&dir, dcs_payload)).expect("read the Terminal Log");
    assert!(
        raw.windows(2).any(|w| w == b"\x1bP"),
        "the log keeps the DCS introducer unchanged"
    );
    assert!(
        raw.windows(2).any(|w| w == b"\x1b_"),
        "the log keeps the APC introducer unchanged"
    );
}

/// #823 / review: a fast producer keeps the log growing through the live
/// reads. Every patch still shows a bounded tail, and the newest output wins.
#[cfg(unix)]
#[tokio::test]
async fn a_fast_producer_still_yields_a_bounded_live_tail() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;
    app.turn_render_poll_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);

    send_command_in(
        &app,
        "/run i=0; while [ $i -lt 100000 ]; do echo \"burst-823-$i\"; i=$((i+1)); done",
        key,
        "msg_burst",
        ConversationKind::P2p,
    )
    .await;

    let updates = platform.updated_cards().await;
    assert!(!updates.is_empty(), "the live loop patched the card");
    for card in &updates {
        let body = output_body(card);
        assert!(
            body.chars().count() <= TERMINAL_TAIL_CHARS,
            "every live patch stays bounded ({} chars)",
            body.chars().count()
        );
    }
    let settled = card_text(&settled_card(&platform).await);
    assert!(
        settled.contains("burst-823-99999"),
        "the newest line wins under a fast producer: {settled}"
    );
}

/// #823: one very long line is capped, with a marker that says so.
#[cfg(unix)]
#[tokio::test]
async fn a_line_past_the_cap_is_truncated_with_a_visible_marker() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(
        &app,
        "/run printf 'begin-823-'; head -c 4000 /dev/zero | tr '\\0' x; printf '%s\\n' '-end-823'; sleep 0.2",
        key,
        "msg_long_line",
        ConversationKind::P2p,
    )
    .await;

    let card = settled_card(&platform).await;
    let body = output_body(&card);
    assert!(body.contains("begin-823-"), "the line's head is shown: {body}");
    assert!(body.contains("本行过长"), "the marker is visible: {body}");
    assert!(
        !body.contains("-end-823"),
        "the long line is actually cut: {body}"
    );
    assert!(
        body.chars().count() <= TERMINAL_TAIL_CHARS,
        "the capped line stays inside the tail budget: {} chars",
        body.chars().count()
    );
}

/// #823: when the tail budget cuts content the card says so, and it keeps the
/// NEWEST output within the 2,500-character budget.
#[cfg(unix)]
#[tokio::test]
async fn the_tail_keeps_the_newest_output_and_says_when_it_cut_content() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(
        &app,
        "/run for i in $(seq -w 1 900); do echo \"line-$i-823\"; done; sleep 0.2",
        key,
        "msg_budget",
        ConversationKind::P2p,
    )
    .await;

    let card = settled_card(&platform).await;
    let text = card_text(&card);
    assert!(text.contains("line-900-823"), "the newest line survives: {text}");
    assert!(!text.contains("line-001-823"), "the oldest line is cut: {text}");
    assert!(text.contains("输出过长"), "the cut is stated explicitly: {text}");
    let body = output_body(&card);
    assert!(
        body.chars().count() <= TERMINAL_TAIL_CHARS,
        "the tail stays within its budget: {} chars",
        body.chars().count()
    );
}

/// #826: the dedicated size probe — its own short interval, not the render
/// tick — kills the process group once the **Terminal Log** has passed its
/// threshold, and the card reads 「输出超限」. The threshold is scaled down here
/// so a cheap producer crosses it while the run is live; production's is
/// 29 MB. That the producer is *stopped* is pinned by the log no longer
/// growing after the card settles.
#[cfg(unix)]
#[tokio::test]
async fn a_run_past_the_observed_threshold_is_stopped_and_reads_output_over_limit() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    app.core
        .terminal_log_cap_bytes
        .store(4 * 1024, std::sync::atomic::Ordering::Relaxed);
    app.core
        .terminal_size_probe_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    let marker = "probe-826-marker";
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        send_command_in(
            &app,
            &format!("/run i=0; while :; do printf '{marker}-%01000d\\n' $i; i=$((i+1)); sleep 0.02; done"),
            key,
            "msg_probe",
            ConversationKind::P2p,
        ),
    )
    .await
    .expect("the size probe stops the run instead of letting it write forever");

    let card = settled_card(&platform).await;
    let text = card_text(&card);
    assert!(
        text.contains("输出超限"),
        "the card states why the run stopped: {text}"
    );
    assert!(
        !text.contains("进程被信号中止"),
        "the stop is named as the observed threshold, not a bare signal: {text}"
    );

    let dir = app
        .core
        .terminal_log_root
        .get()
        .expect("the test installed a temp Terminal Log root")
        .clone();
    let log = find_log_containing(&dir, marker);
    let before = std::fs::metadata(&log).expect("the log exists").len();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let after = std::fs::metadata(&log).expect("the log exists").len();
    assert_eq!(before, after, "the killed producer writes nothing more");
}

/// #826: the threshold is **observed**, never a write limit (ADR-0086
/// decision 7). One burst writes far past it before the probe can look, and
/// the log really holds more than the threshold afterwards — what the probe
/// bounds is the run, not the descriptor.
#[cfg(unix)]
#[tokio::test]
async fn the_threshold_is_observed_not_a_write_limit() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let cap = 4 * 1024u64;
    app.core
        .terminal_log_cap_bytes
        .store(cap, std::sync::atomic::Ordering::Relaxed);
    app.core
        .terminal_size_probe_ms
        .store(5, std::sync::atomic::Ordering::Relaxed);
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    // One 64 KiB burst, then enough of a pause that the probe definitely sees
    // the size. A write limit would have cut it at the threshold; an observed
    // threshold can only stop the run afterwards.
    send_command_in(
        &app,
        "/run head -c 65536 /dev/zero | tr '\\0' x; sleep 0.2",
        key,
        "msg_burst",
        ConversationKind::P2p,
    )
    .await;

    let text = card_text(&settled_card(&platform).await);
    assert!(text.contains("输出超限"), "the run is stopped: {text}");

    let dir = app
        .core
        .terminal_log_root
        .get()
        .expect("the test installed a temp Terminal Log root")
        .clone();
    let log = std::fs::read_dir(&dir)
        .expect("read the Terminal Log directory")
        .next()
        .expect("one log")
        .expect("directory entry")
        .path();
    let written = std::fs::metadata(&log).expect("the log exists").len();
    assert!(
        written > cap,
        "the write was never limited at the threshold: {written} bytes > cap {cap}"
    );
}

/// The 「📄 完整输出」 button's payload, from the settled card the run PATCHed
/// and replied. The live card carries none: the button enables once the run
/// settles (ADR-0086 decision 4).
#[cfg(unix)]
async fn full_output_payload(platform: &RecordingPlatform) -> serde_json::Value {
    platform
        .button_values()
        .await
        .into_iter()
        .find(|value| value["op"] == "full_output")
        .unwrap_or_else(|| panic!("the settled card carries {FULL_OUTPUT_LABEL}"))
}

#[cfg(unix)]
const FULL_OUTPUT_LABEL: &str = "📄 完整输出";

/// Poll until a File Message (`msg_type:"file"`) is recorded, or fail.
#[cfg(unix)]
async fn wait_for_file_message(platform: &RecordingPlatform) -> PlatformCall {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(call) = platform.calls.lock().await.iter().find(
                |call| matches!(call, PlatformCall::SendMessageReply { msg_type, .. } if msg_type == "file"),
            ) {
                return call.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the Terminal Log is delivered as a File Message")
}

/// Poll until an in-place PATCH of `message_id` whose card carries `needle`
/// lands.
#[cfg(unix)]
async fn wait_for_patch_containing(
    platform: &RecordingPlatform,
    message_id: &str,
    needle: &str,
) -> serde_json::Value {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let found = platform.calls.lock().await.iter().find_map(|call| match call {
                PlatformCall::UpdateMessage {
                    message_id: target,
                    card,
                } if target == message_id && card_text(card).contains(needle) => Some(card.clone()),
                _ => None,
            });
            if let Some(card) = found {
                return card;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no PATCH of {message_id} contains {needle:?}"))
}

/// #826: 「📄 完整输出」 posts the **Terminal Log** as a File Message when it
/// fits the platform's 30 MB upload cap — the whole log, byte for byte, through
/// the viewer's File Message lane rather than a second upload path — and the
/// click is acknowledged with a toast before that upload finishes.
#[cfg(unix)]
#[tokio::test]
async fn full_output_posts_the_terminal_log_as_a_file_message() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    let marker = "full-output-826-marker";
    send_command_in(
        &app,
        &format!("/run printf '{marker}\\n'"),
        key,
        "msg_full",
        ConversationKind::P2p,
    )
    .await;
    settled_card(&platform).await;
    let live = platform
        .replied_cards()
        .await
        .into_iter()
        .next()
        .expect("the live card");
    assert!(
        !live.to_string().contains(FULL_OUTPUT_LABEL),
        "the live card carries no retrieval button: {}",
        card_text(&live)
    );

    let payload = full_output_payload(&platform).await;
    let dir = app
        .core
        .terminal_log_root
        .get()
        .expect("the test installed a temp Terminal Log root")
        .clone();
    let log = find_log_containing(&dir, marker);
    let bytes = std::fs::read(&log).expect("read the Terminal Log");
    assert_eq!(
        payload["path"].as_str(),
        Some(log.to_string_lossy().as_ref()),
        "the button names the log it delivers"
    );
    assert_eq!(payload["chat_id"], "chat_1");

    let result = app
        .host_action_on_message(payload, "msg_reply")
        .await
        .expect("the click is acknowledged");
    assert!(result.card.is_none(), "the upload never runs inside the callback");
    assert_eq!(result.toast.as_deref(), Some("正在发送完整输出…"));

    let sent = wait_for_file_message(&platform).await;
    match sent {
        PlatformCall::SendMessageReply {
            message_id, msg_type, ..
        } => {
            assert_eq!(msg_type, "file");
            assert_eq!(message_id, "msg_reply", "under the clicked Terminal Card");
        }
        other => panic!("expected a File Message, got {other:?}"),
    }

    let uploads: Vec<_> = platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| matches!(call, PlatformCall::UploadFile { .. }))
        .cloned()
        .collect();
    assert_eq!(uploads.len(), 1, "exactly one upload: {uploads:?}");
    match &uploads[0] {
        PlatformCall::UploadFile {
            name,
            mime,
            size,
            content,
        } => {
            assert_eq!(*size, bytes.len() as u64, "the whole log's size");
            assert_eq!(content, &bytes, "the whole log's bytes, unchanged");
            assert_eq!(mime, "text/plain");
            assert_eq!(
                name,
                &log.file_name().unwrap().to_string_lossy().into_owned(),
                "the log's own name, so path and artifact cannot drift"
            );
        }
        other => panic!("expected an upload, got {other:?}"),
    }

    let patched = wait_for_patch_containing(&platform, "msg_reply", "完整输出已作为文件消息发送").await;
    assert!(
        card_text(&patched).contains(marker),
        "the card keeps its output tail: {}",
        card_text(&patched)
    );
}

/// #826: past the 30 MB upload cap the click still retrieves something — the
/// first 30 MB from a bounded copy with a truncation line — the card states the
/// file was NOT sent in full, names where the bytes live, and the log on disk
/// is never truncated (the child's descriptor offset does not move back).
#[cfg(unix)]
#[tokio::test]
async fn full_output_past_the_upload_cap_sends_a_truncated_bounded_copy_and_says_so() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    let marker = "cap-826-marker";
    send_command_in(
        &app,
        &format!("/run printf '{marker}\\n'; sleep 0.1"),
        key,
        "msg_cap_full",
        ConversationKind::P2p,
    )
    .await;
    settled_card(&platform).await;

    let dir = app
        .core
        .terminal_log_root
        .get()
        .expect("the test installed a temp Terminal Log root")
        .clone();
    let log = find_log_containing(&dir, marker);
    // A log past the platform's cap. In production the size probe would have
    // stopped the run below the delivery cap; a log this large is exactly what
    // delivery must still handle (a restart's leftover, a lossy clock).
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        let pad = vec![b'z'; (crate::feishu::file::MAX_FILE_BYTES + 4096) as usize];
        file.write_all(&pad).unwrap();
    }
    let before = std::fs::metadata(&log).unwrap().len();
    assert!(before > crate::feishu::file::MAX_FILE_BYTES);

    let payload = full_output_payload(&platform).await;
    app.host_action_on_message(payload, "msg_reply").await;
    wait_for_file_message(&platform).await;

    let uploads: Vec<_> = platform
        .calls
        .lock()
        .await
        .iter()
        .filter(|call| matches!(call, PlatformCall::UploadFile { .. }))
        .cloned()
        .collect();
    assert_eq!(uploads.len(), 1, "exactly one bounded copy: {uploads:?}");
    match &uploads[0] {
        PlatformCall::UploadFile { size, content, .. } => {
            assert_eq!(*size, crate::feishu::file::MAX_FILE_BYTES);
            assert_eq!(content.len() as u64, crate::feishu::file::MAX_FILE_BYTES);
            let tail = String::from_utf8_lossy(&content[content.len() - 512..]);
            assert!(tail.contains("已截断"), "the truncation line is visible: {tail}");
            assert!(
                tail.contains(&*log.to_string_lossy()),
                "the truncation line names where the rest lives: {tail}"
            );
        }
        other => panic!("expected an upload, got {other:?}"),
    }

    let patched = wait_for_patch_containing(&platform, "msg_reply", "文件未完整发送").await;
    let text = card_text(&patched);
    assert!(
        text.contains(&*log.to_string_lossy()),
        "the card says where the bytes live: {text}"
    );
    assert!(
        text.contains("未发送"),
        "the card states the rest was not sent: {text}"
    );

    // Delivery copied; it never truncated the log the child wrote.
    assert_eq!(
        std::fs::metadata(&log).unwrap().len(),
        before,
        "the Terminal Log is left exactly as the run left it"
    );
}

/// #826: the delivery runs off the card callback and is bounded by its own
/// timeout — a parked upload neither holds the ack nor leaves the clicker
/// without a result. The callback returns while the upload is still in flight,
/// and the outcome lands afterwards as a card PATCH.
#[cfg(unix)]
#[tokio::test]
async fn full_output_delivery_runs_off_the_card_callback() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    send_command_in(
        &app,
        "/run echo off-callback-826",
        key,
        "msg_off",
        ConversationKind::P2p,
    )
    .await;
    settled_card(&platform).await;
    let payload = full_output_payload(&platform).await;

    let (entered, release) = platform.pause("upload_file", "");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        app.host_action_on_message(payload, "msg_reply"),
    )
    .await
    .expect("the callback acks while the upload is parked")
    .expect("the click is acknowledged");
    assert_eq!(result.toast.as_deref(), Some("正在发送完整输出…"));

    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .expect("the upload starts on the spawned task");
    release.notify_one();

    wait_for_file_message(&platform).await;
    wait_for_patch_containing(&platform, "msg_reply", "完整输出已作为文件消息发送").await;
}

/// #826: a log that vanished (swept, or a run record from another process) is
/// reported on the card with its path instead of a silent click — 「📄 完整输出」
/// always answers.
#[cfg(unix)]
#[tokio::test]
async fn full_output_reports_a_missing_log_instead_of_staying_silent() {
    let _wd = test_work_dir();
    let proj = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let cfg = test_config(&state.path().join("sessions.json"));
    let (app, platform, _logs) = build_terminal_app(cfg, MockBackend::new(realistic_parts())).await;
    let key = thread();
    seed_entry(&app, entry(&key, proj.path())).await;

    let marker = "missing-826-marker";
    send_command_in(
        &app,
        &format!("/run printf '{marker}\\n'"),
        key,
        "msg_missing",
        ConversationKind::P2p,
    )
    .await;
    settled_card(&platform).await;
    let dir = app
        .core
        .terminal_log_root
        .get()
        .expect("the test installed a temp Terminal Log root")
        .clone();
    let log = find_log_containing(&dir, marker);
    let path = log.to_string_lossy().to_string();
    std::fs::remove_file(&log).expect("remove the Terminal Log");

    let payload = full_output_payload(&platform).await;
    app.host_action_on_message(payload, "msg_reply").await;

    let patched = wait_for_patch_containing(&platform, "msg_reply", "无法读取终端日志").await;
    assert!(
        card_text(&patched).contains(&path),
        "the outcome names the path it could not read: {}",
        card_text(&patched)
    );
    assert!(
        !platform
            .calls
            .lock()
            .await
            .iter()
            .any(|call| matches!(call, PlatformCall::UploadFile { .. })),
        "nothing is uploaded for a log that is gone"
    );
}
