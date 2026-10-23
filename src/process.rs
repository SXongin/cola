//! Process-table reads shared by the daemon's own lifecycle (the singleton
//! lock, `/restart`, autostart detection) and by the OpenCode adapter's server
//! lifecycle.
//!
//! These are deliberately client-neutral: nothing here knows what an
//! `opencode serve` is. The adapter's discovery predicates live with the
//! adapter's discovery module; sysinfo is the one implementation
//! on every platform (there is no `/proc` on macOS/Windows).

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// The command-line args of a process, from sysinfo (None if not visible).
pub fn cmd(pid: i32) -> Option<Vec<String>> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
        false,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    system
        .process(Pid::from_u32(pid as u32))
        .map(|p| p.cmd().iter().map(|s| s.to_string_lossy().into_owned()).collect())
}

/// The executable a process is running, from sysinfo (None if not visible or
/// the platform does not report it). `cola restart` uses it to re-exec the lock
/// holder by its own absolute binary path rather than a possibly-relative
/// argv[0] (ADR-0081).
pub fn exe(pid: i32) -> Option<std::path::PathBuf> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
        false,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    system
        .process(Pid::from_u32(pid as u32))
        .and_then(|p| p.exe().map(|e| e.to_path_buf()))
}

/// The environment of a process, from sysinfo (`None` if not visible). Used to
/// tell whether a supervisor owns a running cola (ADR-0021): systemd stamps
/// `INVOCATION_ID`/`SYSTEMD_EXEC_PID` into every unit process, launchd sets
/// `XPC_SERVICE_NAME`. Reading another process's env is best-effort — it is
/// limited to same-user processes on macOS/Windows.
pub fn environ(pid: i32) -> Option<Vec<String>> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
        false,
        ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
    );
    system.process(Pid::from_u32(pid as u32)).map(|p| {
        p.environ()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    })
}

/// The working directory of a process, from sysinfo (`None` if not visible).
/// `cola restart` re-execs the replacement in the instance's own directory so a
/// relative `--config` or a `./cola.toml` resolves exactly as it did.
pub fn cwd(pid: i32) -> Option<std::path::PathBuf> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]),
        false,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
    );
    system
        .process(Pid::from_u32(pid as u32))
        .and_then(|p| p.cwd().map(|c| c.to_path_buf()))
}

/// Whether a PID refers to a live process (not a zombie, not missing).
///
/// Conservative on platforms where sysinfo cannot determine zombie state: a
/// process that exists but whose status is unknown is treated as alive, so a
/// lock is never stolen from a process that might still be running.
pub fn alive(pid: i32) -> bool {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid as u32)]), false);
    match system.process(Pid::from_u32(pid as u32)) {
        Some(p) => p.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}

/// Terminate a process. On unix this sends SIGTERM (graceful: the process's
/// Drop handlers run, releasing the singleton lock and the Feishu WS); the
/// caller should poll [`alive`] and escalate to SIGKILL via [`force_kill`]. On
/// Windows there is no POSIX signal and the graceful `taskkill /PID` first
/// stage is a no-op for a windowless CLI, so we go straight to `taskkill /F`.
pub fn terminate(pid: i32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        Ok(())
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("taskkill /F failed for pid {}", pid)
        }
    }
}

/// Force-kill a process (SIGKILL on unix). No-op on Windows, where
/// [`terminate`] already force-terminated.
pub fn force_kill(pid: i32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = pid;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alive_detects_own_process() {
        assert!(alive(std::process::id() as i32));
    }

    #[test]
    fn cmd_reads_own_command_line() {
        let args = cmd(std::process::id() as i32).expect("the test process is visible");
        assert!(!args.is_empty(), "a live process has a command line: {args:?}");
    }
}
