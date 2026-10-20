//! **Terminal Commands** (ADR-0086): the Host-capability module that runs one
//! shell command for `/run` and for a `!`-prefixed message. Backend- and
//! generation-neutral, like the **File Viewer**'s `bridge::viewer`: this module
//! owns execution (spawn, wait, read the **Terminal Log**), while
//! `feishu::card::terminal` renders the **Terminal Card**. It never contacts the
//! OpenCode server — no `ensure_server`, no prompt, no session read — so it works
//! with `start_server = "never"` and while the server is down.
//!
//! Shape (ADR-0086 §2, §3, §13):
//! - one `sh -c` child, cwd on the **Path Scope**'s base, stdin closed, merged
//!   stdout/stderr written **straight into** the Terminal Log — never read
//!   through a pipe, so the command's bytes are on disk even if cola dies;
//! - the child leaves cola's process group with the safe `process_group(0)`
//!   route, so a terminal hangup does not reach it;
//! - the log lives in a 0700 directory as a 0600 file under
//!   `~/.cola/terminal/`;
//! - the card is posted live and patched on the render cadence while the child
//!   runs; the settling write is last and the loop stops on it (#823,
//!   decision 4);
//! - Unix only: Windows refuses with a stated reason and spawns nothing
//!   (decision 13). The kill/timeout/run-record half is later slices
//!   (#825/#826/#827); this tracer parses `--timeout` and carries it without
//!   acting on it, exactly as the ADR's cap stops *tailing*, never the wait.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::bridge::command::RunSpec;
use crate::bridge::handles::CommandHandles;
use crate::config::ThreadKey;
use crate::feishu::card::terminal::{
    TERMINAL_TAIL_CHARS, TerminalCard, TerminalFullOutput, TerminalState, build_terminal_card,
};

/// The **Terminal Log**'s **observed** stop threshold (ADR-0086 decision 7):
/// 29 MB. It is deliberately NOT a write limit — no portable safe primitive
/// bounds one descriptor's size (`RLIMIT_FSIZE` is per process and would cap
/// the command's own build artifacts, and truncating the live file does not
/// move the child's offset back). So a dedicated probe reads the file's
/// metadata and kills the process group once the file has passed this size;
/// a fast producer can overshoot it before the probe looks, and that is
/// stated rather than hidden. The threshold sits below the platform's 30 MB
/// delivery cap so ordinary runs get their whole output.
pub(crate) const TERMINAL_LOG_CAP_BYTES: u64 = 29 * 1024 * 1024;

/// How often the size probe reads the log's metadata — its own short interval,
/// never the slower render tick (ADR-0086 decision 7). A metadata read is
/// cheap; this is the interval that decides how far a fast producer can
/// overshoot.
pub(crate) const TERMINAL_SIZE_PROBE_MS: u64 = 250;

/// How long the probe's `SIGTERM` is given before the process group is
/// `SIGKILL`ed: enough for a well-behaved child to die, short enough that a
/// stopped runaway never lingers.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(500);

/// How often the termination sequence asks whether the group is empty yet.
#[cfg(unix)]
const GROUP_POLL: Duration = Duration::from_millis(10);

/// How long a `SIGKILL`ed group is given to disappear before the confirmation
/// gives up (a member in uninterruptible sleep, or a zombie awaiting its
/// reaper, cannot leave promptly). Reported, never silently ignored.
#[cfg(unix)]
const KILL_CONFIRM: Duration = Duration::from_secs(2);

/// The bound on ONE live-card PATCH. `Feishu::update_message` has no request
/// timeout of its own, so an unanswered write would otherwise pin the render
/// loop forever (the probe is independent, but the settle must stay
/// reachable).
const TERMINAL_PATCH_TIMEOUT_MS: u64 = 10_000;

/// The environment names cola's child must NOT inherit (ADR-0086 §8): the
/// server credentials cola itself holds. The same three names `discovery`
/// reads in the other direction; everything else is the Host's own shell.
#[cfg(unix)]
const SCRUBBED_ENV: [&str; 3] = [
    "OPENCODE_SERVER_PASSWORD",
    "OPENCODE_PASSWORD",
    "OPENCODE_SERVER_USERNAME",
];

/// The bare `/run` / refused-flag answer. It names the grammar, the Path Scope
/// base and the two practical limits the command's user meets first; `/help
/// run` (ticket #824) carries the long form.
pub(crate) const RUN_USAGE: &str = "\
用法：`/run [--timeout <n>[smh]] [--] <命令>` —— 在宿主机上执行一条 shell 命令，并把输出回复成终端卡片。
- 命令在**当前会话目录**执行；会话还没创建时用当前项目目录，都没有则用默认目录。
- `--timeout` 是本次运行的时限（默认 10 分钟，最大 24 小时）；`--` 结束选项解析，命令本身就可以以 `-` 开头。
- 没有终端：需要 TTY 的程序会快速失败，请用非交互参数或自己用 tmux。
例：`/run echo hi`、`/run --timeout 30s cargo test`、`/run -- --version`";

/// Why a Windows Host is refused (ADR-0086 §13): the hard half is terminating
/// the whole process tree, not the shell spelling, so V1 says so instead of
/// running a half-implemented lane.
#[cfg(not(unix))]
pub(crate) const WINDOWS_REFUSAL: &str = "\
⚠️ Windows 上暂不支持 /run：无法安全地终止整棵进程树（ADR-0086），因此不会执行这条命令。";

/// Whether this Host can run a Terminal Command; the reason when it cannot.
#[cfg(unix)]
fn unsupported_reason() -> Option<&'static str> {
    None
}

#[cfg(not(unix))]
fn unsupported_reason() -> Option<&'static str> {
    Some(WINDOWS_REFUSAL)
}

/// The `!` entry point (ADR-0086 §1): a message whose first character is `!`
/// and whose remainder is non-empty runs that remainder as the command. The
/// text is already mention-stripped, and it is trimmed here, so `@cola !ls`
/// and `  !ls` qualify. `!` is a message prefix, not a **Command**: `!` alone,
/// `! ` and a `!` anywhere but the first character stay prose. The form takes
/// no flags — everything after `!` is the command — so `!ls #caveman` runs the
/// literal command and loads no **Skill**. `None` means prose.
pub(crate) fn parse_bang(text: &str) -> Option<String> {
    let command = text.trim().strip_prefix('!')?.trim();
    (!command.is_empty()).then(|| command.to_string())
}

/// Run one Terminal Command (ADR-0086 §2) — the `/run` and `!` entry points
/// alike. `None` is the bare or refused `/run` form: answer usage and spawn
/// nothing.
pub(crate) async fn execute(
    handles: &CommandHandles,
    thread_key: &ThreadKey,
    message_id: &str,
    spec: Option<RunSpec>,
) -> crate::error::Result<()> {
    let platform = handles.flow.platform.as_ref();
    let Some(spec) = spec else {
        platform.reply_text(message_id, RUN_USAGE).await?;
        return Ok(());
    };
    if let Some(reason) = unsupported_reason() {
        platform.reply_text(message_id, reason).await?;
        return Ok(());
    }
    // The Path Scope (ADR-0083 §4, ADR-0086 §2): `current_project_directory` is
    // pending-first — the Pending Session's directory wins, else the active
    // session's, else the default — and the card names exactly this spelling.
    let cwd = handles.current_project_directory(thread_key).await;
    // The log root is `~/.cola/terminal` (ADR-0086 §7). No home means nowhere
    // for the output to go: refuse BEFORE spawning, never fall back to a
    // relative directory that could put command output in an unrelated tree.
    let Some(log_root) = handles
        .terminal_log_root
        .get()
        .cloned()
        .or_else(default_terminal_root)
    else {
        platform
            .reply_text(message_id, "⚠️ 无法创建终端日志：找不到 home 目录。")
            .await?;
        return Ok(());
    };
    // `--timeout` is carried in `RunSpec` for #825, which owns the cap: it
    // stops the live tail, never the ADR's `wait()`. This slice does not apply
    // or claim it — naming the owner here keeps the value from reading as an
    // enforced limit.
    tracing::debug!(
        "terminal: running in {} (timeout {:?} is carried for #825 to enforce, not applied here)",
        cwd,
        spec.timeout
    );
    let log = match TerminalLog::create(&log_root) {
        Ok(log) => log,
        Err(error) => {
            // A log that cannot be created is the one failure that must not
            // spawn: there would be nowhere for the output to go.
            platform
                .reply_text(message_id, &format!("⚠️ 无法创建终端日志：{error}"))
                .await?;
            return Ok(());
        }
    };
    // The test build can model environment entries cola's process would carry;
    // production passes none and simply inherits (then scrubs) the real one.
    #[cfg(test)]
    let inherited_env = handles
        .terminal_test_env
        .lock()
        .map(|env| env.clone())
        .unwrap_or_default();
    #[cfg(not(test))]
    let inherited_env: Vec<(String, String)> = Vec::new();

    // The live card goes out before the child exists (ADR-0086 decision 5's
    // card → record → spawn order, with the record a later slice): the run
    // always has a surface, and 「运行中」 is the state the loop patches.
    let live = build_terminal_card(TerminalCard {
        cwd: &cwd,
        command: &spec.command,
        output: "",
        output_truncated: false,
        output_over_limit: false,
        full_output: None,
        notice: None,
        state: TerminalState::Live,
    });
    let card_id = platform.reply_card(message_id, &live).await?;

    let started = Instant::now();
    let (mut child, spawn_error) = match spawn_in(&log, &cwd, &spec.command, &inherited_env) {
        Ok(child) => (Some(child), None),
        Err(error) => {
            // No child means no exit code; the card says why instead of
            // freezing on 「运行中」.
            tracing::debug!("terminal: spawn failed: {error}");
            (None, Some(error.to_string()))
        }
    };
    let mut wait_outcome = None;
    // The size probe runs on its OWN task (ADR-0086 decision 7). A card PATCH
    // can stall on the network, and a probe sharing the render loop's task
    // would be blind for exactly as long as that PATCH hangs — precisely when a
    // runaway producer keeps writing. Its own task reads the log on its own
    // short interval no matter what the card is doing; when it fires, it kills
    // the process group and the settle waits for that termination to complete.
    let over_limit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut probe_task = None;
    if let Some(child) = child.as_mut() {
        let render_ms = handles.config.render_poll_ms().max(1);
        let probe_ms = handles.terminal_size_probe_ms.load(Ordering::Relaxed).max(1);
        let cap = handles.terminal_log_cap_bytes.load(Ordering::Relaxed);
        let pgid = child.id();
        let (stop_probe, stop_rx) = tokio::sync::oneshot::channel();
        probe_task = Some((
            stop_probe,
            tokio::spawn(run_size_probe(
                log.path.clone(),
                cap,
                pgid,
                Duration::from_millis(probe_ms),
                std::sync::Arc::clone(&finished),
                std::sync::Arc::clone(&over_limit),
                stop_rx,
            )),
        ));
        let mut render = tokio::time::interval(Duration::from_millis(render_ms));
        // `interval`'s first tick is immediate; the card was just sent, so
        // consume it instead of patching now.
        render.tick().await;
        loop {
            tokio::select! {
                status = child.wait() => {
                    wait_outcome = Some(status);
                    break;
                }
                _ = render.tick() => {
                    // Bounded: an unanswered PATCH must not freeze the live-card
                    // loop forever. The probe is independent, but the settle
                    // still has to be reachable once the run ends.
                    if tokio::time::timeout(
                        Duration::from_millis(TERMINAL_PATCH_TIMEOUT_MS),
                        patch_live(platform, &card_id, &cwd, &spec.command, &log.path),
                    )
                    .await
                    .is_err()
                    {
                        tracing::warn!("terminal: live patch timed out");
                    }
                }
            }
        }
    }
    // Nothing may observe the log as a runaway after the run itself ended; a
    // command that finished on its own settles with its real exit code, even
    // when its output passed the threshold.
    finished.store(true, Ordering::SeqCst);
    if let Some((stop_probe, probe)) = probe_task {
        // If the probe fired, this JOIN is what makes the card wait for the
        // whole process group to be gone — not merely for `sh` to exit.
        let _ = stop_probe.send(());
        let _ = probe.await;
    }
    let output_over_limit = over_limit.load(Ordering::SeqCst);
    let (exit_code, spawn_error) = match wait_outcome {
        Some(Ok(status)) => (status.code(), spawn_error),
        Some(Err(error)) => (None, Some(error.to_string())),
        // The spawn itself failed; `child` was `None` above.
        None => (None, spawn_error),
    };
    let elapsed = started.elapsed();
    let (output, output_truncated) = read_tail(&log.path, TERMINAL_TAIL_CHARS).unwrap_or_default();
    let log_path = log.path.to_string_lossy().to_string();
    // A settled run's log is what 「📄 完整输出」 delivers; a launch that never
    // started has no output to retrieve, so it carries no button.
    let full_output = spawn_error.is_none().then(|| TerminalFullOutput {
        log_path: &log_path,
        chat_id: &thread_key.chat_id,
        thread_id: &thread_key.thread_id,
        exit_code,
        elapsed,
        output_over_limit,
    });
    let settled = build_terminal_card(TerminalCard {
        cwd: &cwd,
        command: &spec.command,
        output: &output,
        output_truncated,
        output_over_limit,
        full_output,
        notice: None,
        state: TerminalState::Settled {
            exit_code,
            spawn_error: spawn_error.as_deref(),
            elapsed,
        },
    });
    // The one settling write: the loop has already stopped, and nothing is
    // written after this.
    platform.update_message(&card_id, &settled).await?;
    Ok(())
}

/// Patch the live card from the log's newest bytes. A failed live write is not
/// fatal: the run continues and the settling write still lands.
async fn patch_live(
    platform: &dyn crate::feishu::Platform,
    card_id: &str,
    cwd: &str,
    command: &str,
    log: &Path,
) {
    let (output, output_truncated) = read_tail(log, TERMINAL_TAIL_CHARS).unwrap_or_default();
    let card = build_terminal_card(TerminalCard {
        cwd,
        command,
        output: &output,
        output_truncated,
        output_over_limit: false,
        full_output: None,
        notice: None,
        state: TerminalState::Live,
    });
    if let Err(error) = platform.update_message(card_id, &card).await {
        tracing::warn!("terminal: live patch failed: {error}");
    }
}

/// Create the run's Terminal Log, spawn the child into it. `Err` means the
/// process could not start (missing cwd, no `sh`, …) — the card then settles
/// with that reason.
#[cfg(unix)]
fn spawn_in(
    log: &TerminalLog,
    cwd: &str,
    command: &str,
    inherited_env: &[(String, String)],
) -> std::io::Result<tokio::process::Child> {
    let stdout = log.writer.try_clone()?;
    let stderr = log.writer.try_clone()?;
    let mut child = tokio::process::Command::new("sh");
    child
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        // Closed stdin: a command that reads it sees EOF immediately instead
        // of waiting on cola's terminal (ADR-0086 §2, the no-terminal rule).
        .stdin(Stdio::null())
        // Both streams go straight into the log (decision 3): no pipe whose
        // read end cola's death would close under the child's next write.
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        // The safe route ADR-0085 established in place of `setsid`'s
        // `pre_exec`: the child leads its own process group, so a terminal
        // hangup does not reach it and 终止 will be able to signal the tree.
        .process_group(0);
    // ADR-0086 §8: the environment is inherited MINUS cola's own credentials.
    // The child runs as the Host and may read anything the Host can, but the
    // server password cola holds is not part of "the Host's shell". A test
    // injects values here first, so the scrub is exercised against a child that
    // would otherwise have inherited them — without mutating the test process's
    // environment.
    for (name, value) in inherited_env {
        child.env(name, value);
    }
    for name in SCRUBBED_ENV {
        child.env_remove(name);
    }
    child.spawn()
}

/// Non-Unix hosts never reach here — [`unsupported_reason`] refuses first — but
/// the module still has to compile there (ADR-0086 decision 13).
#[cfg(not(unix))]
fn spawn_in(
    _log: &TerminalLog,
    _cwd: &str,
    _command: &str,
    _inherited_env: &[(String, String)],
) -> std::io::Result<tokio::process::Child> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        WINDOWS_REFUSAL,
    ))
}

/// The one **Terminal Log** a run writes: `<root>/<run-id>.log`, in a 0700
/// directory, created 0600 (ADR-0086 §7, §12). It is command output, where
/// credentials leak, so the directory is cola's own and the file is private.
struct TerminalLog {
    path: PathBuf,
    #[cfg_attr(not(unix), allow(dead_code))]
    writer: std::fs::File,
}

impl TerminalLog {
    fn create(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Set explicitly: `create_dir_all` under the usual umask makes a
            // 0755 directory, and this one holds command output.
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = root.join(format!("{}.log", run_id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let writer = options.open(&path)?;
        Ok(Self { path, writer })
    }
}

/// The production **Terminal Log** root: `~/.cola/terminal` (glossary). `None`
/// when the Host has no home directory — [`execute`] then refuses rather than
/// inventing a relative path, which could place command output in an unrelated
/// working tree.
fn default_terminal_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".cola").join("terminal"))
}

/// A process-unique run id for the log's name: the wall clock plus a counter,
/// so two commands in the same nanosecond cannot collide. The durable **Terminal
/// Run Record** (#827) will carry the same id.
fn run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!(
        "{}-{:09}-{}",
        now.as_secs(),
        now.subsec_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The log's current size, or 0 when it cannot be read: a probe that fails to
/// stat has nothing to stop, and a run is never killed on a read error.
fn observed_log_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// The size probe's own task (ADR-0086 decision 7): it watches the log on its
/// own short interval and, once the file has passed the **observed** threshold,
/// kills the whole process group and waits for that group to be gone. It does
/// not share a task with the render loop, so a stalled card PATCH cannot blind
/// it; `finished` is set the moment the run ends on its own, so a completed
/// command is never relabelled as a runaway.
async fn run_size_probe(
    path: PathBuf,
    cap: u64,
    pgid: Option<u32>,
    interval: Duration,
    finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
    over_limit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let mut tick = tokio::time::interval(interval);
    tick.tick().await;
    loop {
        tokio::select! {
            // Checked first: once the run ended there is nothing to stop.
            biased;
            _ = &mut stop => return,
            _ = tick.tick() => {
                if finished.load(Ordering::SeqCst) {
                    return;
                }
                if observed_log_len(&path) <= cap {
                    continue;
                }
                // `>` is the ADR's "passes 29 MB". Whatever the single write
                // that crossed it did is already on disk: this is an observed
                // threshold, not a write limit.
                over_limit.store(true, Ordering::SeqCst);
                terminate_process_group(pgid).await;
                return;
            }
        }
    }
}

/// Stop the run's whole process group and return only once no member remains
/// (ADR-0086 decisions 6 and 7): `SIGTERM` first, then `SIGKILL` after
/// [`KILL_GRACE`], and in both phases wait for the GROUP to empty — not merely
/// for `sh` to exit. A descendant that ignores `SIGTERM` must not keep writing
/// after the shell dies, or the observed threshold means nothing.
#[cfg(unix)]
async fn terminate_process_group(pgid: Option<u32>) {
    let Some(group) = group_pid(pgid) else {
        return;
    };
    signal_group(pgid, rustix::process::Signal::TERM);
    let grace = Instant::now() + KILL_GRACE;
    while group_alive(group) {
        if Instant::now() >= grace {
            signal_group(pgid, rustix::process::Signal::KILL);
            let confirm = Instant::now() + KILL_CONFIRM;
            while group_alive(group) && Instant::now() < confirm {
                tokio::time::sleep(GROUP_POLL).await;
            }
            if group_alive(group) {
                // SIGKILL cannot be ignored; a group still visible here is a
                // member in uninterruptible sleep (or a zombie awaiting its
                // reaper). Nothing can write after SIGKILL, so this is
                // reported, never silently swallowed.
                tracing::warn!("terminal: process group {pgid:?} still has members after SIGKILL");
            }
            return;
        }
        tokio::time::sleep(GROUP_POLL).await;
    }
}

/// Non-Unix hosts refuse before spawning (ADR-0086 decision 13), so the probe
/// never runs there and the module still has to compile.
#[cfg(not(unix))]
async fn terminate_process_group(_pgid: Option<u32>) {}

/// The `Pid` a process group is signalled through. `process_group(0)` makes the
/// child its own group leader, so its pid IS the group id. Pid 0/1 would
/// address cola's own group or init; `child.id()` is never either, but a guard
/// here is cheaper than trusting that forever.
#[cfg(unix)]
fn group_pid(pgid: Option<u32>) -> Option<rustix::process::Pid> {
    let pgid = pgid.filter(|pid| *pid > 1)?;
    let pid = i32::try_from(pgid).ok()?;
    rustix::process::Pid::from_raw(pid)
}

/// Whether any process still belongs to the group (`kill(-pgid, 0)`).
#[cfg(unix)]
fn group_alive(group: rustix::process::Pid) -> bool {
    match rustix::process::test_kill_process_group(group) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    }
}

/// `kill(-pgid, signal)` through rustix's safe wrapper. A failed signal is
/// logged, never fatal: the run still settles on the wait.
#[cfg(unix)]
fn signal_group(pgid: Option<u32>, signal: rustix::process::Signal) {
    let Some(group) = group_pid(pgid) else {
        return;
    };
    if let Err(error) = rustix::process::kill_process_group(group, signal) {
        tracing::warn!("terminal: signalling process group {pgid:?} failed: {error}");
    }
}

/// The copy 「📄 完整输出」 sends. Never a truncation of the log itself: the
/// child's descriptor offset does not move back (ADR-0086 decision 7), so a
/// log past the platform's cap is sent as a bounded *copy* of its first bytes
/// with a truncation line, and the file on disk is left exactly as the run
/// left it.
pub(crate) struct FullOutput {
    /// The bytes a File Message carries (`<= MAX_FILE_BYTES`).
    pub(crate) bytes: Vec<u8>,
    /// The log's own size when the copy was taken.
    pub(crate) total_size: u64,
    /// Whether the copy omits bytes past the platform's upload cap.
    pub(crate) truncated: bool,
}

/// Read the log for delivery, bounded by Feishu's File Message cap.
pub(crate) fn read_full_output(path: &Path) -> std::io::Result<FullOutput> {
    read_full_output_capped(path, crate::feishu::file::MAX_FILE_BYTES)
}

/// [`read_full_output`] with the cap spelled out: production always uses the
/// platform's 30 MB, while a unit test proves the bounded-copy arithmetic
/// without writing 30 MB.
fn read_full_output_capped(path: &Path, cap: u64) -> std::io::Result<FullOutput> {
    use std::io::Read;

    // Whether the file is longer is decided by the bytes actually present, not
    // by an earlier stat: an orphaned group member may still be writing the
    // log, and a copy must never hand the platform more than its cap.
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    (&mut file).take(cap).read_to_end(&mut bytes)?;
    let mut extra = [0u8; 1];
    if file.read(&mut extra)? == 0 {
        return Ok(FullOutput {
            total_size: bytes.len() as u64,
            bytes,
            truncated: false,
        });
    }
    // The log is longer than the cap. The truncation line is part of the
    // delivered bytes, so it is charged against the same cap: room is made by
    // trimming the head, never by overrunning the platform's ceiling. At the
    // real 30 MB the line always fits; a degenerate cap smaller than the line
    // (a unit test only) sends the head without it.
    let total_size = std::fs::metadata(path)
        .map(|meta| meta.len())
        .unwrap_or_default()
        .max(bytes.len() as u64);
    let line = truncation_line(total_size, cap, path);
    if (line.len() as u64) <= cap {
        bytes.truncate((cap - line.len() as u64) as usize);
        bytes.extend_from_slice(line.as_bytes());
    }
    Ok(FullOutput {
        bytes,
        total_size,
        truncated: true,
    })
}

/// The last line of a bounded delivery copy: it says the file is not the whole
/// log, its true size, and where the rest lives.
fn truncation_line(total_size: u64, cap: u64, path: &Path) -> String {
    format!(
        "\n…（已截断：日志共 {}，本文件仅含前 {}；完整输出在 {}）\n",
        crate::backend::file_content::human_size(total_size),
        crate::backend::file_content::human_size(cap),
        path.display()
    )
}

/// The newest window of `path`, and whether the log is longer than that window.
/// UTF-8 is at most four bytes per character, so a window of `4 * max_chars`
/// bytes always holds enough for the display tail's own budget; lossy decoding
/// keeps a non-UTF-8 log readable (ADR-0086 §4). The window is returned
/// **raw**: `\r` collapsing, line caps and ANSI stripping are the card's
/// display concerns, so the per-line cap can still see a whole long line.
///
/// The read is **size-bounded**, not `read_to_end` on the file: a live command
/// is still appending, so EOF keeps moving and a continuous producer such as
/// `/run yes` would grow this allocation without limit. Seeking to the last
/// `window` bytes and then taking at most that many is what makes one tick cost
/// a fixed amount of memory no matter how fast the child writes.
pub(crate) fn read_tail(path: &Path, max_chars: usize) -> std::io::Result<(String, bool)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let window = (max_chars as u64).saturating_mul(4).saturating_add(4);
    let truncated = len > window;
    if truncated {
        // From the END, so the window is the newest bytes at the moment of the
        // seek rather than a start computed from an already-stale length.
        let offset = i64::try_from(window).unwrap_or(i64::MAX);
        file.seek(SeekFrom::End(-offset))?;
    }
    let mut bytes = Vec::with_capacity(window.min(1_048_576) as usize);
    (&mut file).take(window).read_to_end(&mut bytes)?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicBool;

    /// A command's log is written while cola reads it. The read must take a
    /// fixed-size suffix, never follow EOF into however much the producer keeps
    /// appending: `read_to_end` on a growing file allocates without bound and a
    /// `/run yes` would exhaust cola's memory. This drives a real append loop
    /// and asserts one read returns — promptly — with at most the window.
    #[test]
    fn a_continuously_growing_log_reads_a_bounded_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("growth.log");
        // Pre-fill past the window so the read definitely seeks and truncates,
        // and the producer starts from a known state.
        std::fs::write(&path, vec![b'y'; 64 * 1024]).unwrap();

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let path = path.clone();
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .expect("open the growing log");
                let chunk = [b'y'; 64 * 1024];
                // Bound the writer too, so even the unfixed read cannot run
                // away with the disk while the test waits out its timeout.
                for _ in 0..64 {
                    if stop.load(Ordering::Relaxed) || file.write_all(&chunk).is_err() {
                        break;
                    }
                }
            })
        };

        let max_chars = 2_500;
        let window = (max_chars as u64).saturating_mul(4).saturating_add(4);
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = {
            let path = path.clone();
            std::thread::spawn(move || {
                sender.send(read_tail(&path, max_chars)).ok();
            })
        };
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("read_tail must return on a continuously growing log");
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        reader.join().unwrap();

        let (output, truncated) = result.expect("read the growing log");
        assert!(truncated, "a log larger than the window is truncated");
        assert!(
            output.len() as u64 <= window,
            "the read stays inside the {window}-byte window, got {}",
            output.len()
        );
    }

    /// The `!` prefix (ADR-0086 §1): the entry point opens only at the FIRST
    /// character, and only when a command remains. `!` alone, `! ` and a
    /// mid-message `!` stay prose; the `!` form takes no flags, so everything
    /// after it — a `#<id>` Skill token included — is the literal command.
    #[test]
    fn bang_opens_only_at_the_first_character_and_needs_a_remainder() {
        assert_eq!(parse_bang("!ls"), Some("ls".to_string()));
        assert_eq!(
            parse_bang("!ls -la /tmp"),
            Some("ls -la /tmp".to_string()),
            "the whole remainder is the command"
        );
        assert_eq!(
            parse_bang("  !ls  "),
            Some("ls".to_string()),
            "leading whitespace is trimmed before the check"
        );
        assert_eq!(
            parse_bang("!ls #caveman"),
            Some("ls #caveman".to_string()),
            "the `!` form accepts no flags and no Skill scan: the token is literal"
        );
        assert_eq!(
            parse_bang("!--timeout 5s echo hi"),
            Some("--timeout 5s echo hi".to_string()),
            "everything after `!` is the command, flags-looking words included"
        );
        assert_eq!(parse_bang("!"), None, "`!` alone is prose");
        assert_eq!(parse_bang("! "), None, "`! ` with only spaces is empty");
        assert_eq!(parse_bang("!   \t "), None, "only spaces means empty");
        assert_eq!(parse_bang("hello !ls"), None, "a mid-message `!` is prose");
        assert_eq!(parse_bang("ls!"), None, "a trailing `!` is prose");
        assert_eq!(parse_bang(""), None);
    }

    /// #826: the delivery copy is bounded at the platform's cap and carries a
    /// truncation line naming the real size and where the rest lives, while the
    /// log itself is left untouched — truncating the log would not move the
    /// child's descriptor offset back (ADR-0086 decision 7). The cap is spelled
    /// out because production's 30 MB is too much to write in a unit test.
    #[test]
    fn the_delivery_copy_is_bounded_and_never_touches_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        std::fs::write(&path, vec![b'a'; 5_000]).unwrap();

        let copy = read_full_output_capped(&path, 1_024).unwrap();
        assert!(copy.truncated, "the copy admits it is not the whole log");
        assert_eq!(copy.total_size, 5_000, "the log's real size is reported");
        assert_eq!(copy.bytes.len(), 1_024, "the copy never exceeds the cap");
        let text = String::from_utf8_lossy(&copy.bytes).into_owned();
        assert!(text.starts_with("aaaa"), "the copy starts at the log's head");
        assert!(
            text.contains("已截断") && text.contains(&*path.display().to_string()),
            "the truncation line says what was cut and where the rest lives: {text}"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            5_000,
            "the Terminal Log is never truncated for delivery"
        );

        // Under the cap the whole log goes out, with no truncation line.
        let small = dir.path().join("small.log");
        std::fs::write(&small, b"short log\n").unwrap();
        let copy = read_full_output_capped(&small, 1_024).unwrap();
        assert!(!copy.truncated);
        assert_eq!(copy.bytes, b"short log\n");
        assert_eq!(copy.total_size, 10);
    }
}
