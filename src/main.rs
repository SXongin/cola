mod autostart;
mod bridge;
mod config;
mod error;
mod feishu;
mod git;
mod logging;
mod opencode;
mod update;

use clap::Parser;
use std::io::IsTerminal;
use std::sync::Arc;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

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
}

/// The default append log file, used when `--log-file` is not given.
fn default_log_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".cola")
        .join("cola.log")
}

/// Resolve the config file, walking the fallback chain and giving a first-use
/// hint when none is found.
///
/// `--config` is used verbatim; otherwise `./cola.toml` then `~/.cola/cola.toml`
/// are tried. If nothing exists, prints a first-run guide (Feishu credentials
/// are mandatory, so silently running on defaults is pointless) and exits
/// non-zero.
fn resolve_config_or_exit(explicit: &Option<String>) -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    match config::resolve_config_path(explicit.as_deref(), &cwd, &home) {
        Some(path) => path,
        None => {
            let home_cfg = home.join(".cola").join("cola.toml");
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

/// Resolve which OpenCode server cola attaches to, honoring the `start_server`
/// policy (ADR-0013).
///
/// The invariant is the STORE: sessions are shared only when cola, OpenChamber
/// and the CLI read the same data directory (`~/.local/share/opencode`). So
/// cola attaches to whatever `opencode serve` is already running on the default
/// store (OpenChamber's, a manual one — whoever started it), a
/// Coexistent Server winning over cola's Owned. `eager` additionally spawns
/// cola's own server at boot when none exists. `auto` (lazy) and `never`
/// return `None` when nothing runs — `auto` spawns on first demand (Lazy
/// Start), `never` stays attach-only. Returns the resolved endpoint +
/// credentials (discovery always supplies both username and password).
async fn resolve_opencode_server(
    cfg: &config::OpenCodeConfig,
) -> anyhow::Result<Option<bridge::discovery::ResolvedServer>> {
    let candidates = bridge::discovery::scan_processes();
    let self_pid = bridge::discovery::self_spawned_pid();
    if let Some(server) = bridge::discovery::pick_server(&candidates, cfg.preferred_port(), self_pid) {
        tracing::info!("Attached to OpenCode server at http://localhost:{}", server.port);
        return Ok(Some(bridge::discovery::ResolvedServer {
            url: format!("http://localhost:{}", server.port),
            username: server.username.clone(),
            password: server.password.clone(),
        }));
    }

    if cfg.start_server == config::ServerStartPolicy::Eager {
        let spawned = bridge::discovery::spawn_own_server(cfg.preferred_port()).await?;
        tracing::info!("Started cola's own OpenCode server at {}", spawned.url);
        return Ok(Some(spawned));
    }

    tracing::info!(
        "No shared OpenCode server running; {}",
        match cfg.start_server {
            config::ServerStartPolicy::Never => "attach-only mode (`start_server = \"never\"`)",
            _ => "lazy start — attach to a server or spawn an Owned Server at the first prompt",
        }
    );
    Ok(None)
}

/// Path of the singleton lock file — created atomically at startup so a second
/// cola process refuses to start and stays out of the Feishu WS + permission
/// flow (two instances double-answer permission cards and double-send events).
fn lock_file_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".cola")
        .join("cola.lock")
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
    bridge::discovery::process_alive(pid)
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
                        _ => {}
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
    bridge::discovery::terminate_process(pid)?;
    for _ in 0..50 {
        if !pid_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    tracing::warn!("PID {} did not exit on SIGTERM; sending SIGKILL", pid);
    bridge::discovery::force_kill(pid)?;
    for _ in 0..50 {
        if !pid_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("old cola instance PID {} did not exit after SIGKILL", pid)
}

/// Ask the user (on a terminal) whether to replace the running instance.
/// Returns whether they agreed. Non-interactive startup never prompts: it uses
/// `--replace` or fails fast.
fn confirm_replace(pid: i32) -> anyhow::Result<bool> {
    use std::io::Write;
    print!(
        "⚠️ 另一个 cola 实例（PID {}）正在运行。是否替换它并接管？[y/N] ",
        pid
    );
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(parse_confirm(&line))
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
/// running) or "start" (nothing is running) after replacing the binary.
fn running_daemon_pid() -> Option<i32> {
    let raw = std::fs::read_to_string(lock_file_path()).ok()?;
    let pid = raw.trim().parse::<i32>().ok()?;
    (pid_alive(pid) && is_cola_process(pid)).then_some(pid)
}

impl Drop for SingletonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

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

    // Config first: `[bridge] log_days` feeds the daily-rotating log writer.
    let config_path = resolve_config_or_exit(&cli.config);
    let cfg = config::load(&config_path)?;

    // Logs always append to a file (default ~/.cola/cola.log; --log-file
    // overrides) so a restart keeps history instead of wiping it like a shell
    // `>` redirect does. The file is ANSI-free and rotates daily: yesterday's
    // content moves to cola-YYYY-MM-DD.log, older files are swept after
    // `log_days`. When stdout is a real terminal it mirrors the logs too (with
    // ANSI); a redirected stdout gets no cola logs, so the redirect target
    // stays clean and the file is authoritative.
    let log_path = cli.log_file.clone().unwrap_or_else(default_log_path);
    let daily = logging::DailyLog::new(log_path.clone(), cfg.bridge.log_days)?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(daily)
        .with_ansi(false);
    let stdout_layer = std::io::stdout()
        .is_terminal()
        .then(|| tracing_subscriber::fmt::layer().with_ansi(true));
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "cola=info".into()))
        .with(file_layer)
        .with(stdout_layer)
        .init();
    tracing::info!("logging to {}", log_path.display());

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

    let server = resolve_opencode_server(&cfg.opencode).await?;

    tracing::info!(
        "cola starting — OpenCode: {}, Feishu app: {}",
        server
            .as_ref()
            .map(|s| s.url.as_str())
            .unwrap_or("<lazy — attach or spawn at the first prompt>"),
        cfg.feishu.app_id
    );

    let feishu_client = feishu::Client::new(cfg.feishu.clone());

    let opencode_client = opencode::Client::new(cfg.opencode.model.as_deref(), server);
    let app = Arc::new(bridge::App::new(
        cfg.clone(),
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

/// `cola update [--check]`: report the update situation; on an available
/// update, download, verify and replace the binary. The daemon is then
/// restarted through its OS supervisor when one is registered; otherwise the
/// user gets a hint that depends on whether a daemon is actually running —
/// `/restart` in Feishu only makes sense when one is (ADR-0015).
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
        assert!(!is_cola_process(pid), "sh must not be considered a cola process");
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
    fn parse_confirm_accepts_only_explicit_yes() {
        assert!(parse_confirm("y"));
        assert!(parse_confirm("Y"));
        assert!(parse_confirm("yes"));
        assert!(!parse_confirm(""));
        assert!(!parse_confirm("n"));
        assert!(!parse_confirm("N"));
        assert!(!parse_confirm("maybe"));
    }
}
