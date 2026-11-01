//! The Windows daemon preconditions `cola doctor` names (spec #948, ticket
//! #952).
//!
//! Codex's managed daemon refuses to start on Windows unless three machine
//! facts hold, and a violation is silent at the TUI level: it falls back to an
//! embedded server, so the visible symptom is a bare "attach failed". This
//! module probes the facts and models the decision; [`crate::status`] renders
//! them as doctor rows, and [`super::discovery::start`] refuses a start the
//! machine cannot satisfy.
//!
//! 1. **The token is not elevated.** A daemon started from an administrator
//!    token would hand every shared client administrator privileges.
//! 2. **The launcher permits a breakaway child.** Upstream starts the daemon
//!    with `CREATE_BREAKAWAY_FROM_JOB`; a launcher that puts its children in a
//!    job without breakaway (the classic case is `cargo run`, or an outer job
//!    object) makes that spawn fail with `ERROR_ACCESS_DENIED`.
//! 3. **The control-socket path fits.** Windows binds the AF_UNIX socket at
//!    the published path itself, so `$CODEX_HOME/app-server-control/app-server-control.sock`
//!    must fit `sun_path`'s 108 bytes including the terminating NUL.
//!
//! The probes are `#[cfg(windows)]`; the model and its decisions are compiled
//! everywhere so Linux and macOS tests pin the logic even though the Win32
//! calls themselves cannot run there.

#[cfg(any(windows, test))]
use std::path::Path;

#[cfg(windows)]
use std::process::{Command, Stdio};

#[cfg(any(windows, test))]
use super::discovery::socket_path_budget;
#[cfg(windows)]
use super::discovery::{Environment, WINDOWS_SOCKET_PATH_LIMIT};
#[cfg(any(windows, test))]
use super::error::CodexError;

/// `ERROR_ACCESS_DENIED`, the failure `CreateProcess` reports when the
/// launcher's job forbids breakaway. Spelled here rather than pulled from
/// `windows-sys`, whose constants sit beside `unsafe` functions this crate
/// forbids outside tests.
#[cfg(windows)]
const ERROR_ACCESS_DENIED: i32 = 5;

/// `CREATE_SUSPENDED`: the probe child must never execute a single instruction.
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// `DETACHED_PROCESS`: the probe child must not inherit cola's console.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// `CREATE_BREAKAWAY_FROM_JOB`: the flag upstream's daemon launch needs, and
/// the one a restricted launcher rejects.
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// The token's elevation, as far as the process can tell.
///
/// The variants are constructed only by the Windows probe (or a test), so a
/// non-Windows non-test build reads them but never builds one.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Elevation {
    NotElevated,
    Elevated,
}

/// Whether a suspended child can leave the launcher's Windows job.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Breakaway {
    /// The breakaway spawn succeeded (and was reaped).
    Allowed,
    /// The breakaway spawn was denied, but the same child spawned without the
    /// flag — so the job, not the executable, is what refuses.
    Rejected,
    /// The probe could not run at all. The launcher is not proven restricted,
    /// so this is a warning rather than a failure.
    Unavailable(String),
}

/// The resolved control-socket path against the Windows AF_UNIX budget.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SocketBudget {
    Fits {
        path: String,
        length: usize,
        limit: usize,
    },
    TooLong {
        path: String,
        length: usize,
        limit: usize,
    },
    /// The path cannot be an AF_UNIX address at all (for example, it is not
    /// valid UTF-8).
    Unusable { path: String, detail: String },
}

/// The three Windows preconditions, probed once and then answered as a pure
/// decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Preconditions {
    pub(crate) elevation: Elevation,
    pub(crate) breakaway: Breakaway,
    pub(crate) socket: SocketBudget,
}

impl Preconditions {
    /// The elevation instruction, when the token is elevated. A shared daemon
    /// must never inherit administrator privileges.
    pub(crate) fn elevation_refusal(&self) -> Option<&'static str> {
        (self.elevation == Elevation::Elevated).then_some(
            "当前进程持有提升的管理员令牌；共享客户端不应继承管理员权限。\
             请从非管理员终端启动 cola。",
        )
    }

    /// The launcher instruction, when breakaway was provably rejected.
    pub(crate) fn breakaway_refusal(&self) -> Option<&'static str> {
        matches!(self.breakaway, Breakaway::Rejected).then_some(
            "启动器拒绝 breakaway 子进程（`cargo run` 或外层 job 对象会造成这种限制）。\
             请直接运行 codex.exe，而不是通过限制 job 的启动器。",
        )
    }

    /// The control-socket instruction, when the path cannot be used.
    pub(crate) fn socket_refusal(&self) -> Option<String> {
        match &self.socket {
            SocketBudget::Fits { .. } => None,
            SocketBudget::TooLong { path, length, limit } => Some(format!(
                "控制套接字路径 `{path}` 为 {length} 字节，超过 Windows AF_UNIX 地址的 \
                 {limit} 字节上限（含终止 NUL）。请缩短 CODEX_HOME。"
            )),
            SocketBudget::Unusable { path, detail } => Some(format!(
                "控制套接字路径 `{path}` 不能作为 AF_UNIX 地址：{detail}。请修正 CODEX_HOME。"
            )),
        }
    }

    /// Whether the read-only diagnosis may run. An elevated token and an
    /// unusable control path both make the diagnostic meaningless — and the
    /// path must be refused **before** the proxy child is spawned.
    pub(crate) fn allows_diagnostics(&self) -> bool {
        self.elevation_refusal().is_none() && self.socket_refusal().is_none()
    }

    /// The first reason a start is refused, in the order the doctor rows
    /// report them. `None` when the machine satisfies every precondition.
    #[cfg(any(windows, test))]
    pub(crate) fn start_refusal(&self) -> Option<String> {
        self.elevation_refusal()
            .map(str::to_string)
            .or_else(|| self.socket_refusal())
            .or_else(|| self.breakaway_refusal().map(str::to_string))
    }
}

/// Probe the machine's Windows daemon preconditions. Windows only: every other
/// platform publishes a symlink into a short runtime directory, has no token
/// elevation, and has no job object, so there is nothing to report.
#[cfg(windows)]
pub(crate) fn probe(environment: &Environment) -> Preconditions {
    Preconditions {
        elevation: probe_elevation(),
        breakaway: probe_breakaway(),
        socket: socket_budget(&environment.socket(), WINDOWS_SOCKET_PATH_LIMIT),
    }
}

/// `GetTokenInformation(TokenElevation)` through a safe wrapper. The wrapper
/// reports "not elevated" when the query fails; there is no error channel to
/// carry, and a false pass here is the same answer the daemon's own check
/// would reach only if its query also failed.
#[cfg(windows)]
fn probe_elevation() -> Elevation {
    if is_elevated::is_elevated() {
        Elevation::Elevated
    } else {
        Elevation::NotElevated
    }
}

/// Classify a resolved control path against an AF_UNIX address budget.
///
/// Delegates to [`socket_path_budget`], which [`Environment::check_socket_budget`]
/// also uses, so the doctor row and the pre-spawn gate can never disagree.
#[cfg(any(windows, test))]
pub(crate) fn socket_budget(socket: &Path, limit: usize) -> SocketBudget {
    match socket_path_budget(socket, limit) {
        Ok(()) => SocketBudget::Fits {
            path: socket.display().to_string(),
            length: socket.to_str().map(str::len).unwrap_or_default(),
            limit,
        },
        Err(CodexError::ControlSocketPathTooLong { path, length, limit }) => {
            SocketBudget::TooLong { path, length, limit }
        }
        Err(error) => SocketBudget::Unusable {
            path: socket.display().to_string(),
            detail: error.to_string(),
        },
    }
}

/// How one spawn attempt ended. Kept separate from [`Breakaway`] so the
/// classification — the part that carries the instruction — is pure and
/// testable on every platform.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpawnAttempt {
    Spawned,
    AccessDenied,
    Failed(String),
}

/// Turn the two spawn attempts into the launcher verdict.
///
/// `CreateProcess`'s `ERROR_ACCESS_DENIED` is ambiguous on its own: it can mean
/// the file cannot execute, or that the job forbids breakaway. The retry
/// without `CREATE_BREAKAWAY_FROM_JOB` disambiguates it — only a succeeding
/// retry proves the job is the restriction.
#[cfg(any(windows, test))]
fn classify_breakaway(first: SpawnAttempt, retry: Option<SpawnAttempt>) -> Breakaway {
    match (first, retry) {
        (SpawnAttempt::Spawned, _) => Breakaway::Allowed,
        (SpawnAttempt::AccessDenied, Some(SpawnAttempt::Spawned)) => Breakaway::Rejected,
        (SpawnAttempt::AccessDenied, Some(SpawnAttempt::Failed(detail))) => Breakaway::Unavailable(detail),
        // The retry without breakaway was also denied, or never ran: the job
        // cannot be named as the cause, so this is an unavailable probe.
        (SpawnAttempt::AccessDenied, Some(SpawnAttempt::AccessDenied))
        | (SpawnAttempt::AccessDenied, None) => {
            Breakaway::Unavailable("the probe could not spawn without CREATE_BREAKAWAY_FROM_JOB".to_string())
        }
        (SpawnAttempt::Failed(detail), _) => Breakaway::Unavailable(detail),
    }
}

/// Probe breakaway by spawning this executable suspended, then reaping it.
///
/// The suspended child runs no code. The current binary is used as the probe
/// target because a job restriction applies to every child of the launcher,
/// not to one executable — and unlike `codex` it is guaranteed to exist.
#[cfg(windows)]
fn probe_breakaway() -> Breakaway {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return Breakaway::Unavailable(format!("cannot resolve the probe executable: {error}")),
    };
    let first = spawn_suspended(&executable, /*breakaway*/ true);
    let retry = matches!(first, SpawnAttempt::AccessDenied)
        .then(|| spawn_suspended(&executable, /*breakaway*/ false));
    classify_breakaway(first, retry)
}

/// One suspended spawn attempt, reaped before returning.
#[cfg(windows)]
fn spawn_suspended(executable: &Path, breakaway: bool) -> SpawnAttempt {
    use std::os::windows::process::CommandExt;

    let mut command = Command::new(executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut flags = CREATE_SUSPENDED | DETACHED_PROCESS;
    if breakaway {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    command.creation_flags(flags);
    match command.spawn() {
        Ok(mut child) => {
            // A suspended child cannot have done anything, and it must not be
            // left behind: terminate it, then reap the handle.
            if let Err(error) = child.kill() {
                tracing::warn!("the breakaway launch probe could not be terminated: {error}");
            }
            if let Err(error) = child.wait() {
                tracing::warn!("the breakaway launch probe could not be reaped: {error}");
            }
            SpawnAttempt::Spawned
        }
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => SpawnAttempt::AccessDenied,
        Err(error) => SpawnAttempt::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fits() -> SocketBudget {
        SocketBudget::Fits {
            path: "/short/control.sock".to_string(),
            length: 18,
            limit: 108,
        }
    }

    fn preconditions(elevation: Elevation, breakaway: Breakaway, socket: SocketBudget) -> Preconditions {
        Preconditions {
            elevation,
            breakaway,
            socket,
        }
    }

    /// The boundary is upstream's: the terminating NUL is included, so a path
    /// of 107 bytes fits and 108 does not.
    #[test]
    fn the_socket_budget_includes_the_terminating_nul() {
        let fitting = "a".repeat(107);
        let path = Path::new(&fitting);
        assert!(matches!(
            socket_budget(path, 108),
            SocketBudget::Fits {
                length: 107,
                limit: 108,
                ..
            }
        ));

        let over = "a".repeat(108);
        let path = Path::new(&over);
        let SocketBudget::TooLong {
            length,
            limit,
            path: rendered,
        } = socket_budget(path, 108)
        else {
            panic!("108 bytes does not fit a 108-byte budget");
        };
        assert_eq!(length, 108);
        assert_eq!(limit, 108);
        assert_eq!(rendered, "a".repeat(108));
    }

    /// An elevated token fails the check with the non-elevated-terminal
    /// instruction, and neither diagnostics nor a start may follow.
    #[test]
    fn an_elevated_token_blocks_diagnostics_and_start() {
        let pre = preconditions(Elevation::Elevated, Breakaway::Allowed, fits());
        assert!(!pre.allows_diagnostics());
        let refusal = pre.start_refusal().expect("a refusal");
        assert!(refusal.contains("非管理员终端"), "{refusal}");
        assert!(refusal.contains("管理员权限"), "{refusal}");
    }

    /// A launcher restriction is a start precondition, not an attach one: an
    /// existing daemon can still be read, but `daemon start` is refused.
    #[test]
    fn a_rejected_breakaway_blocks_start_but_not_diagnostics() {
        let pre = preconditions(Elevation::NotElevated, Breakaway::Rejected, fits());
        assert!(pre.allows_diagnostics());
        let refusal = pre.start_refusal().expect("a refusal");
        assert!(refusal.contains("cargo run"), "{refusal}");
        assert!(refusal.contains("job 对象"), "{refusal}");
        assert!(refusal.contains("codex.exe"), "{refusal}");
    }

    /// An over-budget path is refused before anything is spawned, and the
    /// instruction names the setting to shorten.
    #[test]
    fn an_over_budget_socket_blocks_before_any_spawn() {
        let socket = socket_budget(Path::new(&"b".repeat(108)), 108);
        let pre = preconditions(Elevation::NotElevated, Breakaway::Allowed, socket);
        assert!(!pre.allows_diagnostics());
        let refusal = pre.start_refusal().expect("a refusal");
        assert!(refusal.contains("CODEX_HOME"), "{refusal}");
        assert!(refusal.contains("108"), "{refusal}");
    }

    /// The retry is what separates "job forbids breakaway" from "this file
    /// cannot run": only a succeeding retry is a named launcher rejection.
    #[test]
    fn breakaway_classification_requires_a_successful_retry() {
        assert_eq!(
            classify_breakaway(SpawnAttempt::Spawned, None),
            Breakaway::Allowed
        );
        assert_eq!(
            classify_breakaway(SpawnAttempt::AccessDenied, Some(SpawnAttempt::Spawned)),
            Breakaway::Rejected
        );
        assert!(matches!(
            classify_breakaway(
                SpawnAttempt::AccessDenied,
                Some(SpawnAttempt::Failed("cannot execute".into()))
            ),
            Breakaway::Unavailable(detail) if detail.contains("cannot execute")
        ));
        assert!(matches!(
            classify_breakaway(SpawnAttempt::Failed("no such file".into()), None),
            Breakaway::Unavailable(detail) if detail.contains("no such file")
        ));
    }

    /// A probe that could not run is never reported as a rejection.
    #[test]
    fn an_unavailable_probe_is_not_a_launcher_rejection() {
        let pre = preconditions(
            Elevation::NotElevated,
            Breakaway::Unavailable("spawn failed".into()),
            fits(),
        );
        assert!(pre.allows_diagnostics());
        assert!(pre.start_refusal().is_none());
        assert!(pre.breakaway_refusal().is_none());
    }
}
