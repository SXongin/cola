#![cfg_attr(not(test), forbid(unsafe_code))]

mod autostart;
mod backend;
mod bridge;
mod completions;
mod config;
mod error;
mod feishu;
mod git;
mod logging;
mod opencode;
mod process;
mod review;
mod state;
mod status;
#[cfg(test)]
mod test_http;
#[cfg(test)]
mod test_ws;
mod update;
mod version;

use clap::Parser;
use std::io::IsTerminal;
use std::sync::Arc;
use tracing_subscriber::{EnvFilter, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "cola", about = "Bridge bot connecting OpenCode to Feishu")]
struct Cli {
    #[arg(short, long)]
    config: Option<String>,

    /// Log file to append to (default ~/.cola/cola.log, never truncates).
    /// Logs always go to the file; when stdout is a terminal they mirror there
    /// too. Restart won't wipe history since the file is appended, not
    /// overwritten.
    #[arg(short, long)]
    log_file: Option<std::path::PathBuf>,

    /// Take over the singleton lock by terminating any running cola instance,
    /// instead of refusing to start. Set automatically when cola restarts
    /// itself via `/restart`.
    #[arg(long)]
    replace: bool,

    /// Print cola's version and build provenance (release or dev build) and
    /// exit — before config, logging, or the singleton lock (ADR-0027).
    #[arg(short = 'V', long)]
    version: bool,

    #[command(subcommand)]
    subcommand: Option<Subcommand>,
}

/// Manage cola's boot-time launcher (ADR-0013): install / remove / query it.
#[derive(clap::Subcommand)]
enum Subcommand {
    /// Register cola to start at boot/login, or manage an existing registration.
    Autostart {
        #[command(subcommand)]
        action: autostart::AutostartAction,
    },
    /// Check for and apply a self-update from GitHub Releases (ADR-0015).
    Update {
        /// Only check for an update; download and apply nothing.
        #[arg(long)]
        check: bool,
    },
    /// Stop the running cola instance (Supervisor first, then the lock holder).
    Stop {
        /// Skip the confirmation prompt. A non-TTY run never prompts.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Restart the running cola instance (Supervisor restart, else a detached
    /// re-exec with the instance's own command line; ADR-0081).
    Restart {
        /// Skip the confirmation prompt. A non-TTY run never prompts.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Print a one-shot snapshot of the cola installation: run state, autostart,
    /// claim, config/log paths, and the shared OpenCode server (ADR-0081).
    Status {
        /// Emit the same facts as JSON for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Run the environment checks a support request needs (config, Feishu
    /// credentials, OpenCode binary/server, singleton lock, autostart path) and
    /// exit non-zero when a check fails (ADR-0081).
    Doctor,
    /// Print the tail of the log file; `-f` follows it (ADR-0081).
    Logs {
        /// Stream new lines as they are appended (blocks; Ctrl-C to stop).
        #[arg(short = 'f', long)]
        follow: bool,
        /// How many lines to print.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Read a rotated day's file (`cola-YYYY-MM-DD.log`) instead of today's.
        #[arg(long, value_name = "YYYY-MM-DD")]
        date: Option<String>,
    },
    /// Inspect the config file: its resolved path, effective values, or validity
    /// (ADR-0081).
    Config {
        #[command(subcommand)]
        action: config::ConfigAction,
    },
    /// Print a shell completion script to stdout, or install it into the
    /// shell's completion directory with `--install`.
    Completions {
        /// The shell to generate completions for. Optional with `--install`,
        /// which infers it from `$SHELL` when omitted.
        #[arg(value_enum, required_unless_present = "install")]
        shell: Option<clap_complete::Shell>,
        /// Write the script into the shell's user completion directory instead
        /// of printing it.
        #[arg(long)]
        install: bool,
        /// With `--install`, report the target path and write nothing.
        #[arg(long, requires = "install")]
        dry_run: bool,
    },
    /// Print cola's version and build provenance (same as `--version`).
    Version,
}

/// The default append log file, used when `--log-file` is not given.
fn default_log_path() -> std::path::PathBuf {
    state::StateLayout::default().log()
}

/// Resolve the config file, walking the fallback chain and giving a first-use
/// hint when none is found.
///
/// `--config` is used verbatim; otherwise `./cola.toml` then `~/.cola/cola.toml`
/// are tried. If nothing exists, prints a first-run guide (Feishu credentials
/// are mandatory, so silently running on defaults is pointless) and exits
/// non-zero.
fn resolve_config_or_exit(explicit: &Option<String>) -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    match config::resolve_path_from_env(explicit.as_deref()) {
        Some(path) => path,
        None => {
            let home_cfg = state::StateLayout::for_home(&home).config_file();
            eprintln!(
                "首次使用 cola：未找到配置文件（已依次查找 `./cola.toml`、`{}`）。\n\
                 请从仓库中的 `cola.toml.example` 复制并填写飞书 app_id / app_secret：\n\
                 \x20 cp cola.toml.example cola.toml   # 然后编辑 cola.toml\n\
                 \x20 mkdir -p ~/.cola && cp cola.toml.example ~/.cola/cola.toml   # 或放到统一状态目录",
                home_cfg.display()
            );
            std::process::exit(1);
        }
    }
}

/// The active Agent Client this build can actually construct (ADR-0090 §5).
///
/// OpenCode is the only adapter today, so a resolved `codex` client is refused
/// loudly here rather than booting with the legacy OpenCode settings the config
/// did not ask for. This is the *capability* gate; resolution itself stays pure
/// and `cola config validate` still calls such a config valid.
fn require_supported_client(active: &config::ActiveClient) -> anyhow::Result<()> {
    if active.opencode().is_none() {
        anyhow::bail!(
            "the active Agent Client `{}` has kind `{}`; this build can only attach to `opencode` clients",
            active.id,
            active.kind
        );
    }
    Ok(())
}

/// Attach at boot through the active client's lifecycle (spec #860, ticket
/// #875).
///
/// The client owns discovery, the generation probe and the optional start; the
/// daemon supplies only the policy-derived spawn allowance. `eager` is the one
/// boot mode that may spawn; `auto` and `never` attach if a server exists and
/// otherwise stay serverless (Lazy Start is the demand path). An inconclusive
/// generation probe is not fatal at boot: cola starts serverless and the
/// reconnect loop attaches once the probe classifies.
async fn attach_at_boot(
    client: &opencode::client::OpenCodeBackend,
    cfg: &config::Config,
) -> anyhow::Result<String> {
    let request = backend::EnsureAttached {
        allow_spawn: cfg.opencode.start_server == config::ServerStartPolicy::Eager,
        heal_when_busy: false,
        busy: false,
        boot: true,
    };
    match backend::ServerLifecycle::ensure_attached(client, request).await {
        Ok(backend::AttachOutcome::Attached) => Ok(client.base_url()),
        Ok(backend::AttachOutcome::Serverless) => {
            tracing::info!(
                "No shared OpenCode server running; {}",
                match cfg.opencode.start_server {
                    config::ServerStartPolicy::Never => "attach-only mode (`start_server = \"never\"`)",
                    _ => "lazy start — attach to a server or spawn an Owned Server at the first prompt",
                }
            );
            Ok(String::new())
        }
        Ok(backend::AttachOutcome::Inconclusive { url, evidence }) => {
            tracing::warn!("generation probe inconclusive at {url}; staying serverless: {evidence}");
            Ok(String::new())
        }
        Err(e) => Err(e.into()),
    }
}

/// Path of the singleton lock file — created atomically at startup so a second
/// cola process refuses to start and stays out of the Feishu WS + permission
/// flow (two instances double-answer permission cards and double-send events).
fn lock_file_path() -> std::path::PathBuf {
    state::StateLayout::default().lock()
}

/// Hold the singleton lock for the lifetime of the process. Fails fast if
/// another cola is already running (its PID is in the lock file). The lock file
/// is cleaned up on drop.
struct SingletonLock(std::path::PathBuf);

/// Outcome of trying to take the singleton lock.
enum AcquireOutcome {
    /// This process now holds the lock.
    Acquired(SingletonLock),
    /// A live cola instance holds the lock (owner PID reported).
    HeldByInstance { pid: i32 },
}

/// A snapshot of one process's identity via sysinfo: its executable path and
/// its argv[0] (None if the PID is not visible). Shared by the two identity
/// checks below so the refresh shape — the sysinfo call that must run before
/// `exe()`/`cmd()` are populated — lives in exactly one place.
fn process_identity(pid: i32) -> Option<(Option<std::path::PathBuf>, Option<String>)> {
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid as u32)]),
        false,
        sysinfo::ProcessRefreshKind::nothing()
            .with_exe(sysinfo::UpdateKind::Always)
            .with_cmd(sysinfo::UpdateKind::Always),
    );
    let proc_ = system.process(sysinfo::Pid::from_u32(pid as u32))?;
    Some((
        proc_.exe().map(|p| p.to_path_buf()),
        proc_.cmd().first().map(|s| s.to_string_lossy().into_owned()),
    ))
}

/// Whether a PID refers to a live process whose executable is named cola.
/// Guards the takeover path: never kill a PID that reuse has handed to an
/// unrelated program (the lock file may carry a stale PID). Uses the
/// executable name first (registered by the OS, not spoofable via argv), with
/// the command-line argv as a fallback.
fn is_cola_process(pid: i32) -> bool {
    let Some((exe, argv0)) = process_identity(pid) else {
        return false;
    };
    if exe.as_ref().is_some_and(|e| e.to_string_lossy().contains("cola")) {
        return true;
    }
    argv0.is_some_and(|a| a.contains("cola"))
}

/// Whether a PID refers to a live process. A zombie ('Z') is treated as DEAD:
/// it can no longer handle a permission or post an event, so a lock owned by
/// one is stale and reclaimable. This is what lets a `/restart` child start
/// while the old process lingers as a zombie until its parent (the interactive
/// shell) reaps it — without this, the child sees the old PID, refuses to start
/// ("Another cola instance"), and exits, leaving nothing running.
///
/// Cross-platform via sysinfo: a missing process is dead; an existing process
/// whose zombie state cannot be determined is treated as alive (conservative —
/// never steal a lock we can't verify is dead).
fn pid_alive(pid: i32) -> bool {
    process::alive(pid)
}

/// Whether a lock owner is still capable of processing events: a live
/// (non-zombie) process whose identity is still readable. A process whose
/// cmdline/exe have vanished has had its memory map torn down by the kernel
/// (`exit_mm` inside `do_exit`) but is not yet marked a zombie — it is
/// mid-exit and dying, so it must not block a `/restart` (the replacement
/// would see "alive but not a cola process" and refuse to replace it, leaving
/// nothing running — the exit-window bug, ADR-0021). "Alive" for the
/// singleton means "can still answer a permission or post an event", which a
/// dying process cannot.
fn owner_functionally_alive(pid: i32) -> bool {
    pid_alive(pid) && identity_readable(pid)
}

/// Whether a process's identity (`/proc/<pid>/exe` or cmdline) is still
/// readable. Unreadable means the kernel has already torn down the process's
/// memory map: it is inside `do_exit`, alive per its status but unable to do
/// anything further.
fn identity_readable(pid: i32) -> bool {
    process_identity(pid)
        .map(|(exe, argv0)| exe.is_some() || argv0.is_some())
        .unwrap_or(false)
}

impl SingletonLock {
    fn acquire() -> anyhow::Result<AcquireOutcome> {
        Self::acquire_at(lock_file_path())
    }

    /// Try to take the singleton lock at `path`. A lock file whose PID is no
    /// longer a live cola is a stale leftover from a crashed/killed process and
    /// is reclaimed instead of failing forever. An owner that is dead, a
    /// zombie, or mid-`exit()` counts as stale (see [`owner_functionally_alive`]),
    /// which is what lets a `/restart` child start while the old process
    /// lingers as a zombie or exits. A LIVE owner that can still process
    /// events is reported so the caller can decide whether to replace it.
    fn acquire_at(path: std::path::PathBuf) -> anyhow::Result<AcquireOutcome> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        loop {
            if let Some(pid) = stale_lock_owner(&path) {
                tracing::warn!("Removing stale singleton lock (owner PID {} is not alive)", pid);
                let _ = std::fs::remove_file(&path);
            }
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    writeln!(file, "{}", std::process::id())?;
                    tracing::info!("Acquired singleton lock at {}", path.display());
                    return Ok(AcquireOutcome::Acquired(SingletonLock(path)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let owner = std::fs::read_to_string(&path).unwrap_or_default();
                    match owner.trim().parse::<i32>() {
                        // A live owner that can still process events is a
                        // genuine conflict.
                        Ok(pid) if owner_functionally_alive(pid) => {
                            return Ok(AcquireOutcome::HeldByInstance { pid });
                        }
                        // Owner died, turned zombie, or is mid-exit between the
                        // stale check and the open; loop to reclaim it and retry.
                        Ok(_) => {}
                        // Content that is not a PID: no owner to verify. Removing
                        // it here would race a concurrent starter — both read the
                        // same bogus content, one removes-and-creates a valid
                        // lock, the other then removes THAT and creates its own,
                        // and two colas hold the lock. Fail closed instead (the
                        // lock's existence is the mutex; the PID is only
                        // advisory), and let the operator clear it — `cola doctor`
                        // reports it as a failure.
                        Err(_) => {
                            anyhow::bail!(
                                "单例锁文件内容无法解析（{}）—— 为避免两个实例同时运行，\
                                 拒绝启动。确认没有 cola 在运行后删除该文件再启动。",
                                path.display()
                            )
                        }
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Terminate the running cola instance (SIGTERM, escalating to SIGKILL on unix;
/// `taskkill /F` on Windows) and wait for it to die. Only ever kills a PID
/// verified to be a live cola process — a stale lock file may hold a PID that
/// reuse has handed to an unrelated program, which must never be killed.
///
/// An owner that is already dead, a zombie, or mid-`exit()` is NOT an error: a
/// `/restart` parent exits via `std::process::exit` (no Drop) right after
/// spawning its replacement, so its PID can tip from "alive" (as the lock check
/// saw it) to mid-exit to zombie by the time we get here. Mid-exit a process
/// has no readable `/proc/exe` or cmdline, so `is_cola_process` reports false
/// and refusing here would kill every restart with nothing left running.
/// Nothing is alive to kill; the caller's re-acquire reclaims the now-stale
/// lock (mid-exit and zombie both count as stale in `stale_lock_owner`). Only
/// an owner that is STILL ALIVE and capable of processing yet not a cola
/// process is the PID-reuse danger this guard exists to refuse.
fn replace_instance(pid: i32) -> anyhow::Result<()> {
    if !owner_functionally_alive(pid) {
        tracing::warn!("lock owner PID {} is dead or exiting; nothing to replace", pid);
        return Ok(());
    }
    if !is_cola_process(pid) {
        anyhow::bail!(
            "lock owner PID {} is not a cola process; refusing to kill it \
             (stale PID reused by another program?)",
            pid
        );
    }
    process::terminate(pid)?;
    for _ in 0..50 {
        if !pid_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    tracing::warn!("PID {} did not exit on SIGTERM; sending SIGKILL", pid);
    process::force_kill(pid)?;
    for _ in 0..50 {
        if !pid_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("old cola instance PID {} did not exit after SIGKILL", pid)
}

/// Prompt on the terminal with a `[y/N]` question and parse the answer. Shared
/// by the takeover and stop confirmations so the read/parse convention cannot
/// drift between them.
fn confirm(prompt: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(parse_confirm(&line))
}

/// Ask the user (on a terminal) whether to replace the running instance.
/// Returns whether they agreed. Non-interactive startup never prompts: it uses
/// `--replace` or fails fast.
fn confirm_replace(pid: i32) -> anyhow::Result<bool> {
    confirm(&format!(
        "⚠️ 另一个 cola 实例（PID {pid}）正在运行。是否替换它并接管？[y/N] "
    ))
}

/// Ask the user (on a terminal) whether to stop the running instance; the
/// prompt names the cost — a running turn's card is truncated (ADR-0039).
fn confirm_stop(pid: i32) -> anyhow::Result<bool> {
    confirm(&format!(
        "⚠️ 停止运行中的 cola (PID {pid})？正在进行的对话会被截断。[y/N] "
    ))
}

/// Whether a user-typed answer to a `[y/N]` prompt means yes. Accepts y/yes,
/// case-insensitively; anything else (including empty/Enter) is no.
fn parse_confirm(line: &str) -> bool {
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Whether the PID recorded in a lock file is stale and the lock can be
/// reclaimed. `None` means "not reclaimable" — either the file can't be read,
/// the PID can't be parsed, or the owner is genuinely alive and capable of
/// processing events (see [`owner_functionally_alive`]). An owner that is
/// dead, a zombie, OR mid-`exit()` (alive per its status but identity already
/// torn down) is stale: in each case it can no longer process events.
fn stale_lock_owner(path: &std::path::Path) -> Option<i32> {
    let owner = std::fs::read_to_string(path).ok()?;
    let pid = owner.trim().parse::<i32>().ok()?;
    if owner_functionally_alive(pid) {
        None
    } else {
        Some(pid)
    }
}

/// The PID of a live cola daemon holding the singleton lock, if any. Used by
/// `cola update` to decide whether to talk about "restart" (a daemon is
/// running) or "start" (nothing is running) after replacing the binary, and by
/// `update::restart_cli` to verify a supervisor restart actually took effect.
pub(crate) fn running_daemon_pid() -> Option<i32> {
    running_daemon_pid_at(&lock_file_path())
}

/// [`running_daemon_pid`] against an explicit lock path, so the stop decision
/// can be tested without touching the real `~/.cola/cola.lock`.
fn running_daemon_pid_at(lock_path: &std::path::Path) -> Option<i32> {
    let raw = std::fs::read_to_string(lock_path).ok()?;
    let pid = raw.trim().parse::<i32>().ok()?;
    (pid_alive(pid) && is_cola_process(pid)).then_some(pid)
}

/// Outcome of `cola stop` / the stop half of `autostart disable` (ADR-0039).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// The running instance was stopped.
    Stopped { pid: i32 },
    /// No functionally-alive cola instance holds the Singleton Lock.
    NotRunning,
    /// The interactive user declined the confirmation.
    Declined,
}

/// The stop decision, injectable for tests (ADR-0039): `confirm` carries the
/// interactive answer (`None` = no prompt — non-TTY or `--yes`), `supervisor`
/// the platform stop command when an Autostart artifact is installed.
fn stop_running_cola_at(
    lock_path: &std::path::Path,
    confirm: Option<bool>,
    supervisor: Option<&str>,
) -> anyhow::Result<StopOutcome> {
    let Some(pid) = running_daemon_pid_at(lock_path) else {
        return Ok(StopOutcome::NotRunning);
    };
    if confirm == Some(false) {
        return Ok(StopOutcome::Declined);
    }
    // Supervisor first (ADR-0039): a launchd agent with `KeepAlive` would
    // respawn a directly-killed process, so the stop must go through it. A
    // failing supervisor command is not fatal — the PID path below is the
    // fallback.
    if let Some(cmd) = supervisor
        && let Err(e) = run_supervisor_command(cmd)
    {
        tracing::warn!("supervisor stop failed ({e}); falling back to PID {pid}");
    }
    // The supervisor may have terminated the Singleton Lock holder already;
    // skip the signal path then (an already-dead owner makes `replace_instance`
    // a noisy no-op).
    if owner_functionally_alive(pid) {
        replace_instance(pid)?;
    }
    Ok(StopOutcome::Stopped { pid })
}

/// How long a Supervisor command may run before it is killed. `systemctl
/// --user`/`launchctl` can stall on a wedged user manager; a bounded wait keeps
/// `cola restart`/`stop` responsive (the bounded PID check that follows would
/// otherwise never start).
const SUPERVISOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run a Supervisor command (`systemctl --user stop|restart ...` /
/// `launchctl bootout|kickstart ...`) through `sh -c`, so a failure is a value
/// the caller can fall back from instead of a hard error (ADR-0039). Bounded by
/// [`SUPERVISOR_TIMEOUT`]: a stalled command is killed and reported as a
/// failure.
fn run_supervisor_command(cmd: &str) -> anyhow::Result<()> {
    let mut child = std::process::Command::new("sh")
        .args(["-c", cmd])
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run `{cmd}`: {e}"))?;
    let deadline = std::time::Instant::now() + SUPERVISOR_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::ensure!(status.success(), "`{cmd}` exited with {status}");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("`{cmd}` 超时（{}s）", SUPERVISOR_TIMEOUT.as_secs());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Stop the running cola instance (ADR-0039). On an interactive terminal the
/// user confirms first unless `assume_yes`; a non-TTY run never prompts, so
/// scripts are not blocked. The Supervisor (if registered) goes first; the
/// Singleton Lock holder is terminated by PID afterwards.
pub(crate) fn stop_running_cola(assume_yes: bool) -> anyhow::Result<StopOutcome> {
    let lock = lock_file_path();
    let confirm = running_daemon_pid_at(&lock)
        .filter(|_| !assume_yes && std::io::stdin().is_terminal())
        .map(confirm_stop)
        .transpose()?;
    stop_running_cola_at(&lock, confirm, autostart::supervisor_stop_command().as_deref())
}

/// The stop half of `autostart disable` that [`stop_running_cola`] cannot
/// reach: a registered unit may be activating — the Singleton Lock is taken
/// only after config/log setup — or crash-looping, so no lock holder exists
/// and [`stop_running_cola`] reports 未运行 without ever consulting the
/// Supervisor. `cola stop` stays scoped to the lock holder (ADR-0039).
/// Returns whether an active unit was asked to stop.
pub(crate) fn stop_supervised_without_holder() -> anyhow::Result<bool> {
    let Some(cmd) = autostart::supervisor_stop_command() else {
        return Ok(false); // no registration: nothing to stop outside the lock
    };
    stop_supervised_without_holder_at(&lock_file_path(), Some(&cmd), autostart::supervisor_is_active())
}

/// [`stop_supervised_without_holder`] with the lock path, supervisor command,
/// and unit state injected for tests. `supervisor = None` means no
/// registration; `active = false` means an inactive unit (no stop to run).
fn stop_supervised_without_holder_at(
    lock_path: &std::path::Path,
    supervisor: Option<&str>,
    active: bool,
) -> anyhow::Result<bool> {
    if running_daemon_pid_at(lock_path).is_some() {
        // A live holder belongs to `stop_running_cola_at`, which already runs
        // the Supervisor (and honors a decline) — don't stop it twice.
        return Ok(false);
    }
    let Some(cmd) = supervisor else {
        return Ok(false);
    };
    if !active {
        return Ok(false);
    }
    run_supervisor_command(cmd)?;
    Ok(true)
}

/// `cola stop`: report the outcome and exit. Exit status stays 0 for a plain
/// no-op and for a decline — only a failing stop is an error.
fn stop_cli(assume_yes: bool) -> anyhow::Result<()> {
    match stop_running_cola(assume_yes)? {
        StopOutcome::Stopped { pid } => println!("cola 已停止（PID {pid}）。"),
        StopOutcome::NotRunning => println!("cola 未运行。"),
        StopOutcome::Declined => println!("已取消。"),
    }
    Ok(())
}

/// `cola restart` (ADR-0081). A registered Supervisor restarts the unit
/// (`systemctl --user restart` / `launchctl kickstart -k`), which also brings it
/// back up. Without one, the lock holder is terminated and a detached
/// replacement is spawned from the instance's OWN command line (the CLI has no
/// startup args of its own to reuse) plus `--replace`. The replacement's stdio
/// is null — logs already go to the log file.
fn restart_cli(assume_yes: bool) -> anyhow::Result<()> {
    let Some(pid) = running_daemon_pid() else {
        println!("cola 未运行。");
        return Ok(());
    };
    if !assume_yes
        && std::io::stdin().is_terminal()
        && !confirm(&format!(
            "⚠️ 重启运行中的 cola (PID {pid})？正在进行的对话会被截断。[y/N] "
        ))?
    {
        println!("已取消。");
        return Ok(());
    }
    // Read the instance's identity BEFORE stopping it: without a supervisor its
    // command line, executable and working directory are the only record of how
    // it was started.
    let argv = process::cmd(pid);
    let exe = process::exe(pid);
    let cwd = process::cwd(pid);
    // Supervisor first — but ONLY when the supervisor actually owns the running
    // process (ADR-0021): restarting the unit while an unrelated manual instance
    // holds the lock starts a process that cannot acquire it, which
    // `Restart=on-failure` then crash-loops. A command that FAILS falls back to
    // the direct path below instead of leaving cola down.
    if let Some(cmd) = autostart::supervisor_restart_command()
        && autostart::supervisor_owns(pid)
        && run_supervisor_command(&cmd).is_ok()
    {
        report_restart(wait_for_new_daemon(Some(pid)))?;
        return Ok(());
    }
    let Some(argv) = argv else {
        anyhow::bail!("无法读取运行中 cola 的启动参数；请先 `cola stop` 再手动启动。");
    };
    let Some((argv0, args)) = argv.split_first() else {
        anyhow::bail!("运行中 cola 的启动参数为空");
    };
    // Prefer the process's own executable path, resolved through
    // `live_exe_path`: it strips Linux's "/path/cola (deleted)" a replaced
    // binary renders as and checks a file actually exists there (issue #551),
    // which also protects the spawn from a phantom path. Fall back to argv[0]
    // only when it is a real file or a bare name on PATH; otherwise refuse
    // BEFORE touching the running instance, so a failed restart never leaves
    // cola down.
    let program = exe
        .as_deref()
        .and_then(crate::update::live_exe_path)
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty())
        .filter(|p| spawnable(p))
        .unwrap_or_else(|| argv0.clone());
    if !spawnable(&program) {
        anyhow::bail!("无法定位运行中 cola 的可执行文件（{program}）；请先 `cola stop` 再手动启动。");
    }
    // Spawn FIRST with `--replace`: the successor takes the lock by terminating
    // the old holder itself (the same takeover `/restart` uses), so there is no
    // window where cola is down. Re-exec in the instance's own working directory
    // so a relative `--config`/`./cola.toml` resolves as before.
    spawn_detached(&program, args, cwd.as_deref())?;
    report_restart(wait_for_new_daemon(Some(pid)))
}

/// Print the restart outcome; verification is what keeps the CLI honest — a
/// supervisor restart reports success even when the new instance never took the
/// lock, so an unconfirmed restart is a non-zero exit (ADR-0081), letting a
/// script notice.
fn report_restart(confirmed: bool) -> anyhow::Result<()> {
    if confirmed {
        println!("cola 已重启。");
        Ok(())
    } else {
        anyhow::bail!("重启已请求，但未确认新实例接管单例锁；请用 `cola status` 查看。")
    }
}

/// Whether `program` can be spawned: an existing file, or a bare name (no path
/// separator) that some `PATH` entry resolves. A path with a separator that does
/// not exist is not spawnable — the guard that keeps `cola restart` from
/// stopping an instance it then cannot bring back.
fn spawnable(program: &str) -> bool {
    let path = std::path::Path::new(program);
    if crate::update::is_executable_file(path) {
        return true;
    }
    if program.contains('/') || program.contains(std::path::MAIN_SEPARATOR) {
        return false;
    }
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| crate::update::is_executable_file(&d.join(program)))
    })
}

/// The arguments a detached restart re-execs with: the instance's own args plus
/// `--replace` (appended once) so the successor may take the singleton lock
/// while the old holder is still dying. Pure, so the `--replace` guarantee is
/// unit-testable without spawning.
fn restart_args(args: &[String]) -> Vec<String> {
    let mut all: Vec<String> = args.to_vec();
    if !all.iter().any(|a| a == "--replace") {
        all.push("--replace".to_string());
    }
    all
}

/// Spawn a detached cola from `exe` + `args`, appending `--replace` so it may
/// take the singleton lock left by the instance just terminated. `cwd` is the
/// instance's own working directory, when known.
fn spawn_detached(exe: &str, args: &[String], cwd: Option<&std::path::Path>) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(exe);
    command
        .args(restart_args(args))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    // Unix: put the child in its OWN process group, so a Ctrl-C in the caller's
    // terminal (SIGINT to the foreground process group) does not also hit the
    // replacement while `restart` waits for it to take the lock. Stdio is null,
    // so nothing is lost by leaving the terminal's group.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // Windows: without a new process group / no console, the child stays tied to
    // the caller's console and a console close (the user closing the window)
    // sends it CTRL_CLOSE_EVENT, killing the replacement.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command
        .spawn()
        .map_err(|e| anyhow::anyhow!("无法启动新的 cola 实例（{exe}）：{e}"))?;
    Ok(())
}

/// The Singleton Lock's state, for `cola doctor`/`cola status` (ADR-0081): a
/// live holder is healthy, a stale file (dead PID) is reclaimable, and a file
/// naming a non-cola process is the PID-reuse hazard the takeover guards
/// against.
pub(crate) enum LockState {
    /// No lock file.
    Absent,
    /// A live cola holds it.
    Held(i32),
    /// The file names a PID that is not alive.
    Stale(i32),
    /// The file names a live process that is not cola.
    Foreign(i32),
    /// The file exists but cannot be read, or its content is not a PID (empty,
    /// corrupted). A normal start reclaims it (see `SingletonLock::acquire_at`),
    /// but it is a misconfiguration the doctor must surface, not a healthy pass.
    Malformed,
}

/// Read the Singleton Lock's state. Never fails: an unreadable/malformed file is
/// "absent" (a broken lock blocks nothing — the next start reclaims it).
pub(crate) fn lock_state() -> LockState {
    lock_state_at(&lock_file_path())
}

fn lock_state_at(path: &std::path::Path) -> LockState {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        // No file at all is the healthy "not running" case; a file that exists
        // but cannot be read (permissions, I/O) is a problem.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LockState::Absent,
        Err(_) => return LockState::Malformed,
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return LockState::Malformed;
    };
    if !pid_alive(pid) {
        LockState::Stale(pid)
    } else if is_cola_process(pid) {
        LockState::Held(pid)
    } else {
        LockState::Foreign(pid)
    }
}

/// Wait until a daemon OTHER than `old_pid` holds the singleton lock — proof a
/// restart took effect. Bounded so a failed restart surfaces instead of hanging.
fn wait_for_new_daemon(old_pid: Option<i32>) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(pid) = running_daemon_pid()
            && Some(pid) != old_pid
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Build the active client for a read-only introspection command. The
/// composition root is the one place allowed to name the adapter; the report
/// only sees its [`backend::ServerLifecycle`].
fn status_client(state: &status::ConfigState) -> Option<Arc<dyn backend::Backend>> {
    let cfg = state.cfg()?;
    // Only an OpenCode client has a server lifecycle to report today. A
    // configured kind this build cannot construct reports no server rather
    // than an error — `cola status` must survive a broken/unsupported config.
    cfg.active_client().ok()?.opencode()?;
    Some(Arc::new(opencode::client::OpenCodeBackend::with_tasks(
        cfg.opencode.model.as_deref(),
        &cfg.opencode,
        state::StateLayout::from_config(cfg),
        bridge::tasks::TasksHandle::new(),
    )))
}

/// `cola status` (ADR-0081): resolve the config leniently, gather the snapshot,
/// and print it as text or JSON. A missing or broken config narrows the report
/// instead of failing.
async fn status_cli(
    json: bool,
    explicit_config: Option<&str>,
    log_path: &std::path::Path,
) -> anyhow::Result<()> {
    let state = status::ConfigState::from_env(explicit_config);
    let client = status_client(&state);
    let snapshot = status::gather(&state, log_path, client.as_deref()).await;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status::render_json(&snapshot))?
        );
    } else {
        print!("{}", status::render(&snapshot));
    }
    Ok(())
}

impl Drop for SingletonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Print the version before any config/log/lock machinery (ADR-0027): a
    // version query must work even for a bot whose config is broken.
    if cli.version {
        println!("{}", version::display_version());
        return Ok(());
    }

    // Autostart registration doesn't need a config, a log file, or the
    // singleton lock — handle it before any of that machinery.
    if let Some(Subcommand::Autostart { action }) = cli.subcommand {
        return autostart::run(action);
    }
    // Self-update is the same: no config, no log file, no lock (it may even run
    // to fix a bot whose config is broken). It prints its own progress.
    if let Some(Subcommand::Update { check }) = cli.subcommand {
        return update_cli(check).await;
    }
    // Stopping is config-free too: the lock path is fixed, and the command
    // must work on a bot whose config is broken (ADR-0039).
    if let Some(Subcommand::Stop { yes }) = cli.subcommand {
        return stop_cli(yes);
    }
    // `cola version` mirrors `--version` as a subcommand for symmetry.
    if let Some(Subcommand::Version) = cli.subcommand {
        println!("{}", version::display_version());
        return Ok(());
    }
    // Shell completions are pure when printed; `--install` writes only into the
    // invoking user's own completion directory. Either way: no config, no lock.
    if let Some(Subcommand::Completions {
        shell,
        install,
        dry_run,
    }) = cli.subcommand
    {
        return completions::run(shell, install, dry_run);
    }
    // Introspection (ADR-0081): read-only, config-light, and lock-free — all
    // three must work while cola is running and against a broken install.
    if let Some(Subcommand::Logs { follow, lines, date }) = &cli.subcommand {
        let log_path = cli.log_file.clone().unwrap_or_else(default_log_path);
        return logging::run_logs(&log_path, *follow, *lines, date.as_deref()).map_err(Into::into);
    }
    if let Some(Subcommand::Config { action }) = cli.subcommand {
        return config::run(action, cli.config.as_deref());
    }
    if let Some(Subcommand::Restart { yes }) = cli.subcommand {
        return restart_cli(yes);
    }
    if let Some(Subcommand::Status { json }) = cli.subcommand {
        let log_path = cli.log_file.clone().unwrap_or_else(default_log_path);
        return status_cli(json, cli.config.as_deref(), &log_path).await;
    }
    if let Some(Subcommand::Doctor) = cli.subcommand {
        let log_path = cli.log_file.clone().unwrap_or_else(default_log_path);
        let state = status::ConfigState::from_env(cli.config.as_deref());
        let client = status_client(&state);
        if !status::run_doctor(&state, &log_path, client.as_deref()).await? {
            std::process::exit(1);
        }
        return Ok(());
    }

    // Config first: `[bridge] log_days` feeds the daily-rotating log writer.
    let config_path = resolve_config_or_exit(&cli.config);
    let mut cfg = config::load(&config_path)?;
    // ADR-0090 decision 5: one client is active per process. Resolve it once,
    // here — the resolved id is durable state (ticket #874), and folding its
    // OpenCode options into the effective config keeps every later reader on
    // one set of settings rather than re-interpreting `[clients.*]`.
    let active = cfg.resolve_active_client()?;
    require_supported_client(&active)?;
    // The one owner of the on-disk layout (ADR-0090 §6): every path below —
    // the log, the singleton lock, the Owned Server's pid record, the Session
    // sidecars — is derived from it rather than from a hand-joined `.cola`.
    let layout = state::StateLayout::from_config(&cfg);

    // Logs always append to a file (default ~/.cola/cola.log; --log-file
    // overrides) so a restart keeps history instead of wiping it like a shell
    // `>` redirect does. The file is ANSI-free and rotates daily: yesterday's
    // content moves to cola-YYYY-MM-DD.log, older files are swept after
    // `log_days`. When stdout is a real terminal it mirrors the logs too (with
    // ANSI); a redirected stdout gets no cola logs, so the redirect target
    // stays clean and the file is authoritative. `logging` owns the wiring —
    // including the mirror layer's own field formatter, which keeps its
    // span-fields extension private (see `logging::subscriber`).
    let log_path = cli.log_file.clone().unwrap_or_else(|| layout.log());
    let daily = logging::DailyLog::new(log_path.clone(), cfg.bridge.log_days)?;
    // The mirror layer's default writer is the `fn() -> Stdout` maker, which
    // is what implements `MakeWriter` (a bare `Stdout` does not).
    let mirror = std::io::stdout()
        .is_terminal()
        .then_some(std::io::stdout as fn() -> std::io::Stdout);
    logging::subscriber(
        daily,
        mirror,
        EnvFilter::try_from_default_env().unwrap_or_else(|_| "cola=info".into()),
    )
    .init();
    tracing::info!("logging to {}", log_path.display());
    // Every boot records the exact build in the log so a report can be traced
    // back to a release or a dev build (ADR-0027).
    tracing::info!("{}", version::display_version());

    // Grab the singleton lock BEFORE touching the network or the store — a
    // duplicate cola must die before it can double-connect to the Feishu WS. A
    // live owner is either taken over (via `--replace`, or interactively after
    // a y/N prompt) or refused with a clear message. `/restart` re-execs with
    // `--replace`, so the restarted process always holds replacement rights.
    let _lock = match SingletonLock::acquire()? {
        AcquireOutcome::Acquired(lock) => lock,
        AcquireOutcome::HeldByInstance { pid } => {
            let replace = cli.replace || (std::io::stdin().is_terminal() && confirm_replace(pid)?);
            if !replace {
                anyhow::bail!(
                    "另一个 cola 实例（PID {}）正在运行，拒绝启动重复实例（会双处理飞书事件）。\n\
                     要接管，请用 `cola --replace` 启动，或在飞书里给旧实例发 /restart。",
                    pid
                );
            }
            tracing::warn!("replacing running cola instance (PID {})", pid);
            replace_instance(pid)?;
            // The old owner is dead; re-acquire (it may linger as a zombie,
            // which `acquire_at` treats as stale and reclaims).
            match SingletonLock::acquire()? {
                AcquireOutcome::Acquired(lock) => lock,
                AcquireOutcome::HeldByInstance { pid } => {
                    anyhow::bail!("另一个 cola 实例（PID {}）在替换后仍占用锁，放弃启动", pid)
                }
            }
        }
    };

    let feishu_client = feishu::client::Client::new(cfg.feishu.clone());

    let opencode_client = opencode::client::OpenCodeBackend::with_tasks(
        cfg.opencode.model.as_deref(),
        &cfg.opencode,
        layout.clone(),
        bridge::tasks::TasksHandle::new(),
    );
    // The client owns discovery and attach; the daemon asks once at boot.
    let server_url = attach_at_boot(&opencode_client, &cfg).await?;

    tracing::info!(
        "cola starting — client: {} ({}), OpenCode: {}, Feishu app: {}",
        active.id,
        active.kind,
        if server_url.is_empty() {
            "<lazy — attach or spawn at the first prompt>"
        } else {
            server_url.as_str()
        },
        cfg.feishu.app_id
    );

    let app = Arc::new(bridge::App::with_layout(
        cfg.clone(),
        layout,
        Arc::new(opencode_client),
        Arc::new(feishu_client),
    )?);

    app.run().await?;

    Ok(())
}

/// Print progress to stdout for the `cola update` CLI.
struct CliReporter;

#[async_trait::async_trait]
impl update::UpdateReporter for CliReporter {
    async fn report(&self, msg: String) {
        println!("{msg}");
    }
}

/// `cola update [--check]`: report the update situation per install channel
/// (ADR-0030). A GitHub Releases install downloads, verifies and replaces the
/// binary, then restarts the daemon through its OS supervisor when one is
/// registered; otherwise the user gets a hint that depends on whether a daemon
/// is actually running — `/restart` in Feishu only makes sense when one is
/// (ADR-0015). A cargo-tracked install downloads nothing: it prints the cargo
/// command that applies the update.
async fn update_cli(check_only: bool) -> anyhow::Result<()> {
    let mode = if check_only {
        update::UpdateMode::Check
    } else {
        update::UpdateMode::Apply
    };
    if let update::UpdateOutcome::Updated(v) = update::run_update(&CliReporter, mode).await {
        match update::restart_cli(running_daemon_pid().is_some()) {
            None => println!("已更新到 {v}，已通过系统监督者重启。"),
            Some(hint) => println!("已更新到 {v}。{hint}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_sees_own_process_as_alive() {
        assert!(pid_alive(std::process::id() as i32));
    }

    #[test]
    fn pid_alive_missing_pid_is_dead() {
        // A far-out PID is almost certainly nonexistent on every platform.
        assert!(!pid_alive(i32::MAX - 1));
    }

    // PID 1 is the init process, always alive — but only assert this where
    // sysinfo reliably enumerates it (Windows exposes System Idle as PID 0/1
    // with restricted visibility, so the invariant doesn't hold there).
    #[test]
    #[cfg(unix)]
    fn pid_alive_sees_init_process_as_alive() {
        assert!(pid_alive(1));
    }

    /// The core restart bug: a zombie process's /proc entry persists until its
    /// parent reaps it, so the old `pid_alive` (mere /proc existence) treated a
    /// zombie as alive. A `/restart` child therefore refused to start while the
    /// old cola lingered as a zombie, and the new process never came up.
    /// Linux-only: relies on `/proc` and POSIX zombie semantics.
    #[test]
    #[cfg(target_os = "linux")]
    fn pid_alive_treats_zombie_as_dead() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn zombie child");
        let pid = child.id() as i32;
        // Let the child exit; we must NOT wait() yet, so it stays a zombie.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("zombie stat readable");
        let state = stat
            .rsplit(')')
            .next()
            .and_then(|s| s.trim_start().chars().next());
        assert_eq!(state, Some('Z'), "child should be a zombie: {stat}");
        assert!(!pid_alive(pid), "a zombie must not be considered alive");
        let _ = child.wait(); // reap
    }

    /// The exit-window variant of the restart bug (ADR-0021): between the
    /// kernel's `exit_mm()` (memory map torn down) and the task being marked a
    /// zombie (`TASK_DEAD`), `/proc/<pid>/status` still reads non-zombie while
    /// `/proc/<pid>/cmdline` and `/proc/<pid>/exe` are already gone. The old
    /// code saw that as "alive but not a cola process", refused to replace it,
    /// and the `/restart` child died with nothing left running. A lock owned by
    /// such a process must be reclaimable. Linux-only: relies on `/proc` and
    /// the kernel exit sequence.
    #[test]
    #[cfg(target_os = "linux")]
    fn lock_owner_in_exit_window_is_reclaimable() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        let mut caught = false;
        for _ in 0..200 {
            // Child writes its own PID to the lock, then allocates a large
            // buffer (via dd) so exit_mm() takes long enough to observe, then
            // exits. Parent must NOT wait() until the loop finishes so the
            // child can't be reaped mid-observation.
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "echo $$ > '{}'; dd if=/dev/zero of=/dev/null bs=16M count=1",
                    lock.display()
                ))
                .spawn()
                .expect("spawn exit-window child");
            let pid = child.id() as i32;
            for _ in 0..100_000 {
                if !pid_alive(pid) {
                    break; // died (or zombie) before the window was observed
                }
                if !identity_readable(pid) {
                    // Mid-exit: alive per status, identity already gone. The
                    // lock must be reclaimable and actually acquirable — the
                    // old code bailed here instead.
                    assert_eq!(
                        stale_lock_owner(&lock),
                        Some(pid),
                        "a lock owned by a mid-exit process must be reclaimable"
                    );
                    match SingletonLock::acquire_at(lock.clone()).unwrap() {
                        AcquireOutcome::Acquired(_held) => {}
                        AcquireOutcome::HeldByInstance { .. } => {
                            panic!("a lock owned by a mid-exit process must be acquired")
                        }
                    }
                    caught = true;
                    break;
                }
            }
            let _ = child.wait(); // reap
            if caught {
                break;
            }
        }
        assert!(
            caught,
            "never observed the kernel exit-window state (flaky environment?)"
        );
    }

    #[test]
    fn identity_readable_for_live_and_missing() {
        assert!(identity_readable(std::process::id() as i32));
        assert!(!identity_readable(i32::MAX - 1));
    }

    #[test]
    fn owner_functionally_alive_live_and_dead() {
        assert!(owner_functionally_alive(std::process::id() as i32));
        assert!(!owner_functionally_alive(i32::MAX - 1));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn singleton_lock_reclaimed_from_zombie_owner() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");

        // Simulate the old cola: it acquires the lock and exits, leaving its
        // PID in the file as a zombie.
        let mut owner = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("echo $$ > '{}'; exit 0", lock.display()))
            .spawn()
            .expect("spawn lock owner");
        let owner_pid = owner.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!pid_alive(owner_pid), "owner should be a zombie");

        // A new cola can now reclaim the lock instead of refusing to start:
        // the stale-owner check must see the zombie as dead.
        assert_eq!(
            stale_lock_owner(&lock),
            Some(owner_pid),
            "a lock owned by a zombie must be stale/reclaimable"
        );
        let _ = owner.wait(); // reap the zombie
    }

    #[test]
    fn singleton_lock_with_live_owner_is_not_stale() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
        // Our own process is alive — the lock must NOT be reclaimable.
        assert_eq!(stale_lock_owner(&lock), None);
    }

    #[test]
    fn acquire_at_acquires_fresh_lock() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        match SingletonLock::acquire_at(lock.clone()).unwrap() {
            AcquireOutcome::Acquired(_held) => {
                // Lock file now carries our PID. `_held` keeps the lock alive so
                // its Drop doesn't remove the file before we read it.
                let owner = std::fs::read_to_string(&lock).unwrap();
                assert_eq!(owner.trim(), std::process::id().to_string());
            }
            AcquireOutcome::HeldByInstance { .. } => panic!("fresh lock must be acquired"),
        }
    }

    #[test]
    fn acquire_at_reports_live_owner() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        // Lock owned by OUR live PID (this test process).
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
        match SingletonLock::acquire_at(lock).unwrap() {
            AcquireOutcome::Acquired(_) => panic!("live owner must not be reclaimable"),
            AcquireOutcome::HeldByInstance { pid } => {
                assert_eq!(pid, std::process::id() as i32);
            }
        }
    }

    #[test]
    fn acquire_at_refuses_a_malformed_lock() {
        // Regression: a lock whose content is not a PID used to spin the acquire
        // loop forever (`stale_lock_owner` cannot parse it, and the open keeps
        // failing with AlreadyExists). It must neither hang NOR be reclaimed —
        // reclaiming races a concurrent starter into a double acquisition — so
        // it fails closed with an actionable error.
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        std::fs::write(&lock, "not a pid").unwrap();
        match SingletonLock::acquire_at(lock.clone()) {
            Ok(_) => panic!("a malformed lock must fail closed, not be acquired"),
            Err(e) => assert!(
                e.to_string().contains("无法解析"),
                "the error must explain the lock: {e}"
            ),
        }
        assert!(lock.exists(), "fail-closed must leave the file for the operator");
    }

    #[test]
    fn acquire_at_reclaims_stale_lock_and_acquires() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        // A dead owner (PID far out of range) is stale and gets reclaimed.
        std::fs::write(&lock, i32::MAX.to_string()).unwrap();
        match SingletonLock::acquire_at(lock).unwrap() {
            AcquireOutcome::Acquired(_) => {}
            AcquireOutcome::HeldByInstance { .. } => panic!("stale lock must be reclaimed"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn replace_instance_refuses_non_cola_process() {
        // A live `sh` is NOT a cola process — replacing it must refuse (never
        // kill an unrelated program that reused a stale lock PID).
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 0.5")
            .spawn()
            .expect("spawn sh");
        let pid = child.id() as i32;
        // CI has caught a transient where a freshly spawned child read back as
        // the PARENT image (this cola test binary) under load. Wait for the
        // identity read to settle before asserting, so the check tests what it
        // means to: a child that is not cola.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while is_cola_process(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            !is_cola_process(pid),
            "sh must not be considered a cola process: {:?}",
            process_identity(pid)
        );
        assert!(
            replace_instance(pid).is_err(),
            "must refuse to kill a non-cola PID"
        );
        assert!(pid_alive(pid), "the non-cola process must be left alive");
        let _ = child.wait();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn replace_instance_accepts_zombie_owner() {
        // The /restart incident: the old cola exits via `std::process::exit`
        // (no Drop, so the lock file survives) right after spawning its
        // replacement, lingering as a zombie until the launching shell reaps it.
        // The replacement's lock check can see the owner as "alive" a moment
        // before it tips into zombie; `replace_instance` must then treat the
        // owner as stale (nothing alive to kill) instead of refusing with
        // "not a cola process" — a zombie has no readable /proc/exe or cmdline.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn zombie owner");
        let pid = child.id() as i32;
        // Let the child exit; we must NOT wait() yet, so it stays a zombie.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!pid_alive(pid), "owner should be a zombie");
        assert!(
            !is_cola_process(pid),
            "a zombie must not look like a cola process"
        );
        replace_instance(pid).expect("a dead/zombie owner is stale and replaceable");
        let _ = child.wait(); // reap
    }

    #[test]
    fn is_cola_process_matches_own_binary() {
        // The test binary is named after the crate, so its own cmdline names
        // cola — the guard must recognise it (the takeover path depends on it).
        assert!(is_cola_process(std::process::id() as i32));
        assert!(!is_cola_process(i32::MAX - 1));
    }

    #[test]
    fn lock_state_at_distinguishes_absent_stale_and_held() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        assert!(matches!(lock_state_at(&lock), LockState::Absent));

        // A dead PID is stale, not absent (the doctor must not false-pass it).
        std::fs::write(&lock, i32::MAX.to_string()).unwrap();
        assert!(matches!(lock_state_at(&lock), LockState::Stale(p) if p == i32::MAX));

        // Our own process is a live cola (the test binary is named after the
        // crate), so the lock reads as held.
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
        assert!(matches!(lock_state_at(&lock), LockState::Held(p) if p == std::process::id() as i32));

        // A file that is not a PID is malformed: reclaimable by a start, but the
        // doctor must be able to tell it from a clean absence.
        std::fs::write(&lock, "not a pid").unwrap();
        assert!(matches!(lock_state_at(&lock), LockState::Malformed));
    }

    #[cfg(unix)]
    #[test]
    fn lock_state_at_reports_a_live_non_cola_owner_as_foreign() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 5")
            .spawn()
            .expect("spawn sh");
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        std::fs::write(&lock, child.id().to_string()).unwrap();
        // The child's identity may lag its spawn; retry until visible, like the
        // takeover tests do.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            if matches!(lock_state_at(&lock), LockState::Foreign(_)) {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(seen, "a live non-cola PID must read as Foreign");
    }

    #[test]
    fn report_restart_is_an_error_when_unconfirmed() {
        assert!(report_restart(true).is_ok());
        assert!(
            report_restart(false).is_err(),
            "an unverified restart must not exit 0"
        );
    }

    #[cfg(unix)]
    #[test]
    fn spawn_detached_runs_in_the_given_directory() {
        // The replacement must inherit the instance's cwd so a relative
        // `--config`/`./cola.toml` resolves as before.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("where");
        let arg0 = dir.path().join("arg0");
        let pgrp = dir.path().join("pgrp");
        let pid = dir.path().join("pid");
        // `$0` is the first word after the `-c` script — i.e. the argument
        // `spawn_detached` appends — so this pins that the replacement carries
        // `--replace` (the takeover the restart relies on). `$$` and its process
        // group prove the child leads its OWN group: a Ctrl-C in the caller's
        // terminal must not reach it.
        let script = format!(
            "pwd > {}; printf '%s' \"$0\" > {}; printf '%s' \"$$\" > {}; \
             printf '%s' \"$(ps -o pgid= -p $$ | tr -d ' ')\" > {}",
            marker.display(),
            arg0.display(),
            pid.display(),
            pgrp.display()
        );
        spawn_detached("sh", &["-c".to_string(), script], Some(dir.path())).unwrap();
        let all = [&marker, &arg0, &pgrp, &pid];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !all.iter().all(|p| p.exists()) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let printed = std::fs::read_to_string(&marker).expect("child wrote its cwd");
        // `pwd` may resolve symlinks; compare canonical paths.
        assert_eq!(
            std::fs::canonicalize(printed.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(&arg0).unwrap(),
            "--replace",
            "the detached replacement must carry --replace"
        );
        let child = std::fs::read_to_string(&pid).unwrap();
        let group = std::fs::read_to_string(&pgrp).unwrap();
        assert_eq!(
            group, child,
            "the replacement must lead its own process group (child {child}, pgrp {group})"
        );
    }

    #[test]
    fn restart_args_appends_replace_exactly_once() {
        let args = vec!["--config".to_string(), "x.toml".to_string()];
        assert_eq!(restart_args(&args), vec!["--config", "x.toml", "--replace"]);
        // Already carrying it: not duplicated.
        let with = vec!["--replace".to_string()];
        assert_eq!(restart_args(&with), vec!["--replace"]);
    }

    #[test]
    fn spawnable_accepts_an_executable_file_and_rejects_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cola");
        std::fs::write(&file, b"x").unwrap();
        make_executable(&file);
        assert!(spawnable(file.to_str().unwrap()));
        // A path with a separator that does not exist is not spawnable.
        assert!(!spawnable("/nonexistent/dir/cola"));
        assert!(!spawnable("./definitely-not-here"));
        // A bare name that no PATH entry resolves is not spawnable either.
        assert!(!spawnable("cola-definitely-not-on-path-xyz"));
    }

    #[cfg(unix)]
    #[test]
    fn spawnable_rejects_a_non_executable_file() {
        // A same-named file without an execute bit cannot be spawned.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cola");
        std::fs::write(&file, b"x").unwrap();
        assert!(!spawnable(file.to_str().unwrap()));
    }

    /// `chmod +x` where it exists; a no-op elsewhere.
    fn make_executable(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    #[test]
    fn parse_confirm_accepts_only_explicit_yes() {
        assert!(parse_confirm("y"));
        assert!(parse_confirm("Y"));
        assert!(parse_confirm("yes"));
        assert!(!parse_confirm(""));
        assert!(!parse_confirm("n"));
        assert!(!parse_confirm("N"));
        assert!(!parse_confirm("maybe"));
    }

    #[test]
    fn stop_without_a_lock_is_not_running() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        assert_eq!(
            stop_running_cola_at(&lock, None, None).unwrap(),
            StopOutcome::NotRunning
        );
    }

    /// Spawn a fake cola instance — a symlink named `cola` to the platform's
    /// `sleep`, so `is_cola_process` accepts its argv[0] — and record its PID
    /// in `lock` the way a real instance would. The caller must reap the child.
    #[cfg(unix)]
    fn spawn_fake_cola(lock: &std::path::Path) -> std::process::Child {
        let link = lock.parent().unwrap().join("cola");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/bin/sleep", &link).expect("symlink sleep");
        let child = std::process::Command::new(&link)
            .arg("30")
            .spawn()
            .expect("spawn fake cola");
        std::fs::write(lock, child.id().to_string()).unwrap();
        // sysinfo may not have the just-spawned process in its table yet; wait
        // until the stop path can see it, like the identity settle waits above.
        wait_until_fake_cola_visible(lock, &child);
        child
    }

    /// Wait until the stop decision can see the just-spawned fake instance. A
    /// targeted sysinfo refresh can transiently miss a fresh process on a
    /// loaded runner (llvm-cov on CI), so a silent deadline expiry would
    /// surface later as a misleading `NotRunning`; fail here instead, naming
    /// both identity signals.
    #[cfg(unix)]
    fn wait_until_fake_cola_visible(lock: &std::path::Path, child: &std::process::Child) {
        let pid = child.id() as i32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if running_daemon_pid_at(lock).is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!(
            "fake cola PID {pid} never became visible to sysinfo \
             (pid_alive={}, is_cola_process={})",
            pid_alive(pid),
            is_cola_process(pid)
        );
    }

    /// Run the stop decision once it can see the fake instance. The decision's
    /// first step re-reads the same sysinfo state the visibility wait reads; a
    /// transient miss must not be mistaken for the behavior under test
    /// (`NotRunning`). Retry within the test's deadline — the `NotRunning`
    /// branch has no side effects to replay.
    #[cfg(unix)]
    fn stop_when_visible(
        lock: &std::path::Path,
        confirm: Option<bool>,
        supervisor: Option<&str>,
    ) -> StopOutcome {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let outcome = stop_running_cola_at(lock, confirm, supervisor).expect("stop decision");
            if outcome != StopOutcome::NotRunning || std::time::Instant::now() >= deadline {
                return outcome;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Wait until the fake instance exits, reaping it — a deterministic
    /// waitpid where a sysinfo liveness read could transiently miss.
    #[cfg(unix)]
    fn wait_for_exit(child: &mut std::process::Child, who: &str) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{who} is still running after the deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn declined_stop_leaves_the_instance_and_the_supervisor_alone() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        let mut child = spawn_fake_cola(&lock);
        let marker = home.path().join("supervisor-ran");
        let supervisor = format!("touch {}", marker.display());
        assert_eq!(
            stop_when_visible(&lock, Some(false), Some(&supervisor)),
            StopOutcome::Declined
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "a declined stop must not kill the instance"
        );
        assert!(!marker.exists(), "a declined stop must not run the supervisor");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn stop_runs_the_supervisor_and_terminates_the_holder() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        let mut child = spawn_fake_cola(&lock);
        let marker = home.path().join("supervisor-ran");
        let supervisor = format!("touch {}", marker.display());
        assert_eq!(
            stop_when_visible(&lock, None, Some(&supervisor)),
            StopOutcome::Stopped {
                pid: child.id() as i32
            }
        );
        assert!(marker.exists(), "the supervisor stop command must run");
        let status = wait_for_exit(&mut child, "the lock holder");
        assert!(!status.success(), "the lock holder must be terminated");
    }

    #[cfg(unix)]
    #[test]
    fn stop_falls_back_to_the_pid_when_the_supervisor_command_fails() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        let mut child = spawn_fake_cola(&lock);
        assert_eq!(
            stop_when_visible(&lock, None, Some("false")),
            StopOutcome::Stopped {
                pid: child.id() as i32
            }
        );
        let status = wait_for_exit(&mut child, "the lock holder");
        assert!(
            !status.success(),
            "a failed supervisor command must fall back to the PID path"
        );
    }

    #[test]
    fn stop_without_a_live_holder_does_not_touch_the_supervisor() {
        // Scope boundary (ADR-0039) for `cola stop`: "running" is the live lock
        // holder, so a bare no-op (or a supervised crash loop) must not shell
        // out to the supervisor. `autostart disable` widens this deliberately —
        // see the `stop_supervised_without_holder_at` tests below.
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        let marker = home.path().join("supervisor-ran");
        let supervisor = format!("touch {}", marker.display());
        assert_eq!(
            stop_running_cola_at(&lock, None, Some(&supervisor)).unwrap(),
            StopOutcome::NotRunning
        );
        assert!(!marker.exists(), "no live holder means no supervisor call");
    }

    #[cfg(unix)]
    #[test]
    fn disable_stop_reaches_an_active_supervisor_without_a_holder() {
        // The startup/crash-loop window: a unit can be activating before it
        // ever writes the lock, so the holder gate must not suppress the stop.
        // unix-only: `run_supervisor_command` shells out via `sh -c`.
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        let marker = home.path().join("supervisor-ran");
        let supervisor = format!("touch {}", marker.display());
        assert!(stop_supervised_without_holder_at(&lock, Some(&supervisor), true).unwrap());
        assert!(
            marker.exists(),
            "an active unit must be stopped through the supervisor"
        );
    }

    #[test]
    fn disable_stop_skips_an_inactive_supervisor() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        let marker = home.path().join("supervisor-ran");
        let supervisor = format!("touch {}", marker.display());
        assert!(!stop_supervised_without_holder_at(&lock, Some(&supervisor), false).unwrap());
        assert!(!marker.exists(), "an inactive unit needs no supervisor call");
    }

    #[test]
    fn disable_stop_without_a_registration_is_a_no_op() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        assert!(!stop_supervised_without_holder_at(&lock, None, true).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn disable_stop_leaves_a_live_holder_to_the_lock_path() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        let mut child = spawn_fake_cola(&lock);
        let marker = home.path().join("supervisor-ran");
        let supervisor = format!("touch {}", marker.display());
        assert!(!stop_supervised_without_holder_at(&lock, Some(&supervisor), true).unwrap());
        assert!(
            !marker.exists(),
            "the holder path owns the supervisor stop, so it must not run here"
        );
        assert!(pid_alive(child.id() as i32));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn disable_stop_surfaces_a_failing_supervisor_command() {
        // disable unregisters anyway (the caller's primary act) and reports the
        // failure with a non-zero exit; here the error must reach the caller.
        // unix-only: `run_supervisor_command` shells out via `sh -c`.
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        assert!(stop_supervised_without_holder_at(&lock, Some("false"), true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stop_treats_a_non_cola_lock_owner_as_not_running() {
        let home = tempfile::tempdir().unwrap();
        let lock = home.path().join("cola.lock");
        // A live `sleep` (argv[0] carries no "cola") is a stale lock PID that
        // process reuse handed to another program, not an instance. Wait for
        // its identity to settle (as the replace_instance test does) so the
        // assertion tests what it means to.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while is_cola_process(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!is_cola_process(pid), "sleep must not look like cola");
        std::fs::write(&lock, pid.to_string()).unwrap();
        assert_eq!(
            stop_running_cola_at(&lock, None, None).unwrap(),
            StopOutcome::NotRunning
        );
        assert!(pid_alive(pid), "a non-cola PID must be left alone");
        let _ = child.kill();
        let _ = child.wait();
    }

    /// ADR-0090 §5: resolution accepts a `codex` Agent Client, but this build
    /// cannot attach to it — startup must refuse rather than run the legacy
    /// OpenCode settings the config did not choose. Deleting the gate turns
    /// this test red.
    #[test]
    fn an_active_codex_client_is_refused_at_startup() {
        let mut cfg: config::Config = toml::from_str(
            "[feishu]\napp_id = \"cli_test\"\napp_secret = \"secret\"\n\
             [bridge]\ndefault_client = \"ada\"\n\n[clients.ada]\nkind = \"codex\"\n",
        )
        .unwrap();
        let active = cfg.resolve_active_client().unwrap();
        assert_eq!(active.kind, config::ClientKind::Codex);

        let error = require_supported_client(&active).unwrap_err().to_string();
        assert!(error.contains("`ada`"), "names the Agent Client: {error}");
        assert!(error.contains("codex"), "names the kind: {error}");
    }

    #[test]
    fn the_default_opencode_client_passes_the_startup_gate() {
        let mut cfg: config::Config =
            toml::from_str("[feishu]\napp_id = \"cli_test\"\napp_secret = \"secret\"\n").unwrap();
        let active = cfg.resolve_active_client().unwrap();
        require_supported_client(&active).unwrap();
    }
}
