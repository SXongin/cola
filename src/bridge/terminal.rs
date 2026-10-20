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
//! - Unix only: Windows refuses with a stated reason and spawns nothing
//!   (decision 13). The kill/timeout/run-record half is later slices
//!   (#825/#826/#827); this tracer parses `--timeout` and carries it without
//!   acting on it, exactly as the ADR's cap stops *tailing*, never the wait.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
#[cfg(unix)]
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::bridge::command::RunSpec;
use crate::bridge::handles::CommandHandles;
use crate::config::ThreadKey;
#[cfg(unix)]
use crate::feishu::card::terminal::TERMINAL_TAIL_CHARS;
use crate::feishu::card::terminal::{TerminalCard, build_terminal_card};

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
    let platform = &handles.flow.platform;
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
    let run = match run_in(&cwd, &spec.command, &log_root).await {
        Ok(run) => run,
        Err(error) => {
            // A log that cannot be created is the one failure that must not
            // spawn: there would be nowhere for the output to go.
            platform
                .reply_text(message_id, &format!("⚠️ 无法创建终端日志：{error}"))
                .await?;
            return Ok(());
        }
    };
    let card = build_terminal_card(TerminalCard {
        cwd: &cwd,
        command: &spec.command,
        output: &run.output,
        output_truncated: run.output_truncated,
        exit_code: run.exit_code,
        spawn_error: run.spawn_error.as_deref(),
        elapsed: run.elapsed,
    });
    platform.reply_card(message_id, &card).await?;
    Ok(())
}

/// One run's result: how the child ended, how long it took, and the bounded
/// tail of its Terminal Log.
struct TerminalRun {
    exit_code: Option<i32>,
    spawn_error: Option<String>,
    elapsed: Duration,
    output: String,
    output_truncated: bool,
}

/// Create the run's Terminal Log, spawn the child into it, wait, and read the
/// tail. `Err` means the log could not be created — the only failure that
/// happens **before** a child exists.
#[cfg(unix)]
async fn run_in(cwd: &str, command: &str, log_root: &Path) -> std::io::Result<TerminalRun> {
    let log = TerminalLog::create(log_root)?;
    let stdout = log.writer.try_clone()?;
    let stderr = log.writer.try_clone()?;
    let started = Instant::now();
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
    // server password cola holds is not part of "the Host's shell".
    for name in SCRUBBED_ENV {
        child.env_remove(name);
    }
    let status = child.status().await;
    let elapsed = started.elapsed();
    let (exit_code, spawn_error) = match status {
        Ok(status) => (status.code(), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let (output, output_truncated) = read_tail(&log.path, TERMINAL_TAIL_CHARS).unwrap_or_default();
    Ok(TerminalRun {
        exit_code,
        spawn_error,
        elapsed,
        output,
        output_truncated,
    })
}

/// Non-Unix hosts never reach here — [`unsupported_reason`] refuses first — but
/// the module still has to compile there.
#[cfg(not(unix))]
async fn run_in(_cwd: &str, _command: &str, _log_root: &Path) -> std::io::Result<TerminalRun> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        WINDOWS_REFUSAL,
    ))
}

/// The one **Terminal Log** a run writes: `~/.cola/terminal/<run-id>.log`, in a
/// 0700 directory, created 0600 (ADR-0086 §7, §12). It is command output, where
/// credentials leak, so the directory is cola's own and the file is private.
#[cfg(unix)]
struct TerminalLog {
    path: PathBuf,
    writer: std::fs::File,
}

#[cfg(unix)]
impl TerminalLog {
    fn create(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        {
            use std::os::unix::fs::PermissionsExt;
            // Set explicitly: `create_dir_all` under the usual umask makes a
            // 0755 directory, and this one holds command output.
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = root.join(format!("{}.log", run_id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
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
#[cfg(unix)]
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

/// The newest `max_chars` characters of `path`, and whether anything was cut.
/// UTF-8 is at most four bytes per character, so a window of `4 * max_chars`
/// bytes always holds enough; lossy decoding keeps a non-UTF-8 log readable
/// (ADR-0086 §4). The live tail's line and `\r` handling arrives with #823.
#[cfg(unix)]
fn read_tail(path: &Path, max_chars: usize) -> std::io::Result<(String, bool)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let window = (max_chars as u64).saturating_mul(4).saturating_add(4);
    let start = len.saturating_sub(window);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let decoded = String::from_utf8_lossy(&bytes).into_owned();
    let truncated = start > 0 || decoded.chars().count() > max_chars;
    let output = if truncated {
        let mut tail: Vec<char> = decoded.chars().rev().take(max_chars).collect();
        tail.reverse();
        tail.into_iter().collect()
    } else {
        decoded
    };
    Ok((output, truncated))
}

#[cfg(test)]
mod tests {
    use super::parse_bang;

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
}
