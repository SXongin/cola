mod bridge;
mod config;
mod error;
mod feishu;
mod opencode;

use clap::Parser;
use std::io::IsTerminal;
use std::sync::Arc;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "cola", about = "Bridge bot connecting OpenCode to Feishu")]
struct Cli {
    #[arg(short, long, default_value = "cola.toml")]
    config: String,

    /// Also append logs to this file (never truncates). Without it logs go to
    /// stdout only. Handy under systemd or when stdout is redirected — restart
    /// won't wipe history since the file is appended, not overwritten.
    #[arg(short, long)]
    log_file: Option<std::path::PathBuf>,

    /// Take over the singleton lock by terminating any running cola instance,
    /// instead of refusing to start. Set automatically when cola restarts
    /// itself via `/restart`.
    #[arg(long)]
    replace: bool,
}

/// Resolve which OpenCode server cola talks to, mutating the config.
///
/// The invariant is the STORE: sessions are shared only when cola, OpenChamber
/// and the CLI read the same data directory (`~/.local/share/opencode`). So
/// cola attaches to whatever `opencode serve` is already running on the default
/// store (OpenChamber's managed server, a manual one — whoever started it) and
/// only starts its own when none exists. A server on a custom store is never
/// touched.
async fn resolve_opencode_server(cfg: &mut config::OpenCodeConfig) -> anyhow::Result<()> {
    let preferred_port = cfg.url.rsplit(':').next().and_then(|s| s.parse().ok());
    let candidates = bridge::discovery::scan_processes();

    if let Some(server) = bridge::discovery::select_server(&candidates, preferred_port).cloned() {
        cfg.url = format!("http://localhost:{}", server.port);
        cfg.password = Some(server.password);
        tracing::info!("Attached to OpenCode server at {}", cfg.url);
        return Ok(());
    }

    // No shared server running — start our own on the default store.
    let port = {
        let mut p = preferred_port.unwrap_or(4096);
        // Skip ports already held by a foreign (custom-store) server.
        while candidates.iter().any(|c| c.port == p) {
            p += 1;
        }
        p
    };
    let password = cfg.password.clone().unwrap_or_else(|| "cola-secret".to_string());
    tracing::info!("No shared OpenCode server found; starting one on port {}", port);
    spawn_self_server(port, &password)?;
    wait_for_port(port).await?;
    cfg.url = format!("http://localhost:{}", port);
    cfg.password = Some(password);
    Ok(())
}

/// Spawn cola's own `opencode serve` (default store, so sessions stay shared).
fn spawn_self_server(port: u16, password: &str) -> anyhow::Result<()> {
    bridge::discovery::spawn_self_server(port, password)?;
    Ok(())
}

/// Wait until the self-started server accepts connections.
async fn wait_for_port(port: u16) -> anyhow::Result<()> {
    bridge::discovery::wait_for_port(port).await
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

/// Whether a PID refers to a live process whose command line names cola.
/// Guards the takeover path: never kill a PID that reuse has handed to an
/// unrelated program (the lock file may carry a stale PID).
fn is_cola_process(pid: i32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let cmdline = String::from_utf8_lossy(&cmdline);
    let args: Vec<&str> = cmdline.split('\0').collect();
    args.first().map(|a| a.contains("cola")).unwrap_or(false)
}

/// Whether a PID refers to a live process. On Linux this checks `/proc/{pid}`;
/// elsewhere it conservatively assumes the PID is alive so a lock is never
/// stolen from a real process (a stale lock on a non-Linux host is a manual
/// delete, same as before).
///
/// A zombie ('Z') is treated as DEAD: it can no longer handle a permission or
/// post an event, so a lock owned by one is stale and reclaimable. This is what
/// lets a `/restart` child start while the old process lingers as a zombie
/// until its parent (the interactive shell) reaps it — without this, the child
/// sees the old PID in /proc, refuses to start ("Another cola instance"), and
/// exits, leaving nothing running.
fn pid_alive(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // /proc/<pid>/stat is "pid (comm) state ppid ..." — the comm may itself
        // contain ')', so take the part after the LAST ')' and read its first
        // char (field 3 = state). Unknown/unparseable is treated as alive (the
        // conservative direction: never steal a lock we can't verify is dead).
        !matches!(
            stat.rsplit(')')
                .next()
                .and_then(|s| s.trim_start().chars().next()),
            Some('Z')
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        true
    }
}

impl SingletonLock {
    fn acquire() -> anyhow::Result<AcquireOutcome> {
        Self::acquire_at(lock_file_path())
    }

    /// Try to take the singleton lock at `path`. A lock file whose PID is no
    /// longer a live cola is a stale leftover from a crashed/killed process and
    /// is reclaimed instead of failing forever. A zombie counts as dead (see
    /// `pid_alive`), which is also what lets a `/restart` child start while the
    /// old process lingers as a zombie. A LIVE owner is reported so the caller
    /// can decide whether to replace it.
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
                        // A live owner is a genuine conflict.
                        Ok(pid) if pid_alive(pid) => {
                            return Ok(AcquireOutcome::HeldByInstance { pid });
                        }
                        // Owner died between the stale check and the open; loop
                        // to reclaim it and retry.
                        _ => {}
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Terminate the running cola instance (SIGTERM, escalating to SIGKILL) and
/// wait for it to die. Only ever kills a PID verified to be a cola process —
/// a stale lock file may hold a PID that reuse has handed to an unrelated
/// program, which must never be killed.
fn replace_instance(pid: i32) -> anyhow::Result<()> {
    if !is_cola_process(pid) {
        anyhow::bail!(
            "lock owner PID {} is not a cola process; refusing to kill it \
             (stale PID reused by another program?)",
            pid
        );
    }
    let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    for _ in 0..50 {
        if !pid_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    tracing::warn!("PID {} did not exit on SIGTERM; sending SIGKILL", pid);
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
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

/// Whether the PID recorded in a lock file is stale (dead or a zombie) and the
/// lock can be reclaimed. `None` means "not reclaimable" — either the file
/// can't be read, the PID can't be parsed, or the owner is genuinely alive.
fn stale_lock_owner(path: &std::path::Path) -> Option<i32> {
    let owner = std::fs::read_to_string(path).ok()?;
    let pid = owner.trim().parse::<i32>().ok()?;
    if pid_alive(pid) { None } else { Some(pid) }
}

impl Drop for SingletonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Append (never truncate) to the log file when given, so a restart keeps
    // history instead of wiping it like a shell `>` redirect does. Logs also
    // keep going to stdout.
    let make_writer: tracing_subscriber::fmt::writer::BoxMakeWriter = match &cli.log_file {
        Some(path) => {
            let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
            tracing_subscriber::fmt::writer::BoxMakeWriter::new(file)
        }
        None => tracing_subscriber::fmt::writer::BoxMakeWriter::new(|| std::io::stdout()),
    };
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "cola=info".into()))
        .with(tracing_subscriber::fmt::layer().with_writer(make_writer))
        .init();

    let mut cfg = config::load(&cli.config)?;

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

    resolve_opencode_server(&mut cfg.opencode).await?;

    tracing::info!(
        "cola starting — OpenCode: {}, Feishu app: {}",
        cfg.opencode.url,
        cfg.feishu.app_id
    );

    let feishu_client = feishu::Client::new(cfg.feishu.clone());

    let opencode_client = opencode::Client::new(cfg.opencode.clone());
    let app = Arc::new(bridge::App::new(
        cfg.clone(),
        Arc::new(opencode_client),
        Arc::new(feishu_client),
    )?);

    app.run().await?;

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
        // PID 1 is the init process and is always alive on Linux.
        assert!(pid_alive(1));
        // A far-out PID is almost certainly nonexistent.
        assert!(!pid_alive(i32::MAX - 1));
    }

    /// The core restart bug: a zombie process's /proc entry persists until its
    /// parent reaps it, so the old `pid_alive` (mere /proc existence) treated a
    /// zombie as alive. A `/restart` child therefore refused to start while the
    /// old cola lingered as a zombie, and the new process never came up.
    #[test]
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

    #[test]
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
