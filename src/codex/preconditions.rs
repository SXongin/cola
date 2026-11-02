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
//!    the path upstream canonicalises for binding, so the measured path must
//!    fit `sun_path`'s 108 bytes including the terminating NUL.
//!
//! The probes are `#[cfg(windows)]`; the model and its decisions are compiled
//! everywhere so Linux and macOS tests pin the logic even though the Win32
//! calls themselves cannot run there.

#[cfg(any(windows, test))]
use std::path::Path;

#[cfg(windows)]
use std::process::{Command, Stdio};

#[cfg(any(windows, test))]
use super::discovery::measured_socket_path;
#[cfg(any(windows, test))]
use super::discovery::socket_path_budget;
#[cfg(windows)]
use super::discovery::{Environment, WINDOWS_SOCKET_PATH_LIMIT};
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
/// `Unknown` is a **failure**, not a non-elevated answer: the query error is
/// kept so doctor can warn and a start can be refused. Folding the failure
/// into "not elevated" would let a token nobody could read inherit into every
/// shared client.
///
/// The variants are constructed only by the Windows probe (or a test), so a
/// non-Windows non-test build reads them but never builds one.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Elevation {
    NotElevated,
    Elevated,
    Unknown(String),
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
    /// Not probed because an earlier precondition already blocks both the
    /// diagnosis and a start, so no `CreateProcess` is paid.
    Skipped(String),
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
    /// The control path could not be resolved because `CODEX_HOME`/home is
    /// unavailable. The token and launcher probes can still report.
    Unlocalized(String),
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
    /// The Chinese doctor-row instruction for a positively elevated token.
    ///
    /// The actionable text lives at the doctor layer; [`Self::start_refusal`]
    /// builds an English [`CodexError`] for the lifecycle path, and the path
    /// cases reuse the canonical #950 errors rather than restating their
    /// message here.
    pub(crate) fn elevation_instruction(&self) -> Option<&'static str> {
        (self.elevation == Elevation::Elevated).then_some(
            "当前进程持有提升的管理员令牌；共享客户端不应继承管理员权限。\
             请从非管理员终端启动 cola。",
        )
    }

    /// The Chinese doctor-row instruction when breakaway was provably
    /// rejected.
    pub(crate) fn breakaway_instruction(&self) -> Option<&'static str> {
        matches!(self.breakaway, Breakaway::Rejected).then_some(
            "启动器拒绝 breakaway 子进程（`cargo run` 或外层 job 对象会造成这种限制）。\
             请直接运行 codex.exe，而不是通过限制 job 的启动器。",
        )
    }

    /// The Chinese doctor-row instruction when the control path cannot be
    /// used. The display layer is deliberately separate from the canonical
    /// [`CodexError`] `start_refusal` returns.
    pub(crate) fn socket_instruction(&self) -> Option<String> {
        match &self.socket {
            SocketBudget::Fits { .. } | SocketBudget::Unlocalized(_) => None,
            SocketBudget::TooLong { path, length, limit } => Some(format!(
                "控制套接字路径 `{path}` 为 {length} 字节，超过 Windows AF_UNIX 地址的 \
                 {limit} 字节上限（含终止 NUL）。请缩短 CODEX_HOME。"
            )),
            SocketBudget::Unusable { path, detail } => Some(format!(
                "控制套接字路径 `{path}` 不能作为 AF_UNIX 地址：{detail}。请修正 CODEX_HOME。"
            )),
        }
    }

    /// Whether the read-only diagnosis may run.
    ///
    /// An elevated token blocks it: the daemon it would describe cannot exist
    /// under this token, and the path must be refused **before** the proxy
    /// child is spawned. An **unknown** elevation still allows the read-only
    /// diagnosis — attaching to an existing daemon writes nothing and is the
    /// one thing that can still answer — but is never reported as a pass,
    /// never allows a start, and `doctor` skips the real-turn probe (which
    /// writes a thread and may spend provider money).
    pub(crate) fn allows_diagnostics(&self) -> bool {
        !matches!(self.elevation, Elevation::Elevated)
            && !matches!(
                self.socket,
                SocketBudget::TooLong { .. } | SocketBudget::Unusable { .. }
            )
    }

    /// The canonical error a start is refused with, in the same order the
    /// doctor rows report the preconditions: elevation, launcher, control
    /// path. `None` when the machine satisfies every precondition.
    ///
    /// The path cases reuse the errors ticket #950 introduced, so the message
    /// a start prints and the message the rest of the adapter prints cannot
    /// drift. The elevation and launcher cases are English like every other
    /// `CodexError`; the Chinese instructions live in the doctor rows above.
    pub(crate) fn start_refusal(&self) -> Option<CodexError> {
        match &self.elevation {
            Elevation::Elevated => {
                return Some(CodexError::WindowsPrecondition(
                    "the process token is elevated; shared clients must not inherit \
                     administrator privileges — start cola from a non-elevated terminal"
                        .to_string(),
                ));
            }
            Elevation::Unknown(detail) => {
                return Some(CodexError::WindowsPrecondition(format!(
                    "the process token's elevation could not be read ({detail}); refusing to \
                     start a daemon without establishing that the token is non-elevated"
                )));
            }
            Elevation::NotElevated => {}
        }

        if let Breakaway::Rejected = self.breakaway {
            return Some(CodexError::WindowsPrecondition(
                "the launcher prevents a breakaway child (for example `cargo run` or an outer \
                 job object); run codex.exe directly"
                    .to_string(),
            ));
        }

        match &self.socket {
            SocketBudget::TooLong { path, length, limit } => {
                return Some(CodexError::ControlSocketPathTooLong {
                    path: path.clone(),
                    length: *length,
                    limit: *limit,
                });
            }
            SocketBudget::Unusable { path, detail } => {
                return Some(CodexError::DaemonSocket {
                    socket: path.clone(),
                    detail: detail.clone(),
                });
            }
            SocketBudget::Unlocalized(detail) => {
                return Some(CodexError::WindowsPrecondition(format!(
                    "the control-socket path could not be localized ({detail}); refusing to \
                     start without measuring it"
                )));
            }
            SocketBudget::Fits { .. } => {}
        }

        None
    }
}

/// Probe the machine's Windows daemon preconditions. Windows only: every other
/// platform publishes a symlink into a short runtime directory, has no token
/// elevation, and has no job object, so there is nothing to report.
///
/// The order is deliberate: the pure socket-budget check runs first, then the
/// token query, and the breakaway **spawn** runs only when neither of those
/// already refuses the start. An over-budget path, an elevated token, an
/// unreadable token, or an unlocalized home therefore never pays a
/// `CreateProcess`.
#[cfg(windows)]
pub(crate) fn probe(environment: Option<&Environment>) -> Preconditions {
    let socket = match environment {
        Some(environment) => socket_budget(&environment.socket(), WINDOWS_SOCKET_PATH_LIMIT),
        None => SocketBudget::Unlocalized("cannot resolve CODEX_HOME or a home directory".to_string()),
    };
    let elevation = probe_elevation();
    let breakaway = match skip_breakaway(&elevation, &socket) {
        Some(reason) => Breakaway::Skipped(reason.to_string()),
        None => probe_breakaway(),
    };
    Preconditions {
        elevation,
        breakaway,
        socket,
    }
}

/// Why the breakaway probe is skipped, if an earlier precondition already
/// refuses a start. Pure so the decision is pinned on every platform.
#[cfg(any(windows, test))]
fn skip_breakaway(elevation: &Elevation, socket: &SocketBudget) -> Option<&'static str> {
    match (elevation, socket) {
        (Elevation::Elevated, _) => Some("令牌已提升"),
        (Elevation::Unknown(_), _) => Some("无法读取令牌提升状态"),
        (_, SocketBudget::TooLong { .. } | SocketBudget::Unusable { .. }) => Some("控制套接字路径不可用"),
        (_, SocketBudget::Unlocalized(_)) => Some("无法定位 CODEX_HOME"),
        _ => None,
    }
}

/// `GetTokenInformation(TokenElevation)` through `check_elevation`.
/// A failed query is [`Elevation::Unknown`], never a silent "not elevated".
#[cfg(windows)]
fn probe_elevation() -> Elevation {
    match check_elevation::is_elevated() {
        Ok(true) => Elevation::Elevated,
        Ok(false) => Elevation::NotElevated,
        Err(error) => Elevation::Unknown(format!(
            "the Windows token-elevation query failed with error {error:#010x}"
        )),
    }
}

/// Classify a resolved control path against an AF_UNIX address budget.
///
/// Delegates to [`socket_path_budget`], which [`Environment::check_socket_budget`]
/// also uses, so the doctor row and the pre-spawn gate can never disagree. The
/// path reported here is the **canonical** path upstream binds, not the
/// published spelling — see [`measured_socket_path`].
#[cfg(any(windows, test))]
pub(crate) fn socket_budget(socket: &Path, limit: usize) -> SocketBudget {
    match socket_path_budget(socket, limit) {
        Ok(()) => {
            let measured = measured_socket_path(socket);
            // The budget check above proved the measured path is UTF-8.
            let length = measured
                .to_str()
                .expect("the budget check proved the path is UTF-8")
                .len();
            SocketBudget::Fits {
                path: measured.display().to_string(),
                length,
                limit,
            }
        }
        Err(CodexError::ControlSocketPathTooLong { path, length, limit }) => {
            SocketBudget::TooLong { path, length, limit }
        }
        // `DaemonSocket` already prefixes the path with "is unusable"; keep
        // only its detail so the doctor row does not nest the sentence.
        Err(CodexError::DaemonSocket { detail, .. }) => SocketBudget::Unusable {
            path: measured_socket_path(socket).display().to_string(),
            detail,
        },
        Err(error) => SocketBudget::Unusable {
            path: measured_socket_path(socket).display().to_string(),
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
    /// The child was created. `cleanup` records a kill/reap failure, which is
    /// logged but must not downgrade the spawn evidence.
    Spawned {
        cleanup: Option<String>,
    },
    AccessDenied,
    Failed(String),
}

/// Turn the two spawn attempts into the launcher verdict.
///
/// `CreateProcess`'s `ERROR_ACCESS_DENIED` is ambiguous on its own: it can mean
/// the file cannot execute, or that the job forbids breakaway. The retry
/// without `CREATE_BREAKAWAY_FROM_JOB` disambiguates it — only a succeeding
/// retry proves the job is the restriction. A retry that is also denied is not
/// proof, so it stays unavailable.
#[cfg(any(windows, test))]
fn classify_breakaway(first: SpawnAttempt, retry: Option<SpawnAttempt>) -> Breakaway {
    match (first, retry) {
        // A cleanup failure says the probe leaked a suspended child, not that
        // the launcher is restricted. The spawn evidence still classifies it;
        // the leak was logged at error! where it happened.
        (SpawnAttempt::Spawned { cleanup }, _) => {
            let _ = cleanup;
            Breakaway::Allowed
        }
        (SpawnAttempt::AccessDenied, Some(SpawnAttempt::Spawned { cleanup })) => {
            let _ = cleanup;
            Breakaway::Rejected
        }
        (SpawnAttempt::AccessDenied, Some(SpawnAttempt::Failed(detail))) => Breakaway::Unavailable(detail),
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
        Err(error) => {
            return Breakaway::Unavailable(format!("cannot resolve the probe executable: {error}"));
        }
    };
    let first = spawn_suspended(&executable, /*breakaway*/ true);
    let retry = matches!(first, SpawnAttempt::AccessDenied)
        .then(|| spawn_suspended(&executable, /*breakaway*/ false));
    classify_breakaway(first, retry)
}

/// One suspended spawn attempt, reaped before returning.
///
/// A failed `kill` is logged and retried once, but the child is never waited
/// on: it is `CREATE_SUSPENDED`, so a wait could never return while the
/// lifecycle pass lock is held. The spawn outcome is still `Spawned` — the
/// cleanup failure must not turn proven launcher evidence into "unavailable" —
/// and the cleanup detail says whether a suspended child was leaked.
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
            if let Err(error) = child.kill() {
                tracing::error!("the breakaway launch probe could not be terminated: {error}; retrying once");
                match child.kill() {
                    Ok(()) => SpawnAttempt::Spawned {
                        cleanup: Some(format!("the first termination attempt failed: {error}")),
                    },
                    Err(retry) => {
                        tracing::error!(
                            "the suspended breakaway launch probe was leaked: first kill \
                             failed: {error}; retry failed: {retry}"
                        );
                        SpawnAttempt::Spawned {
                            cleanup: Some(format!("could not terminate the suspended probe: {retry}")),
                        }
                    }
                }
            } else if let Err(error) = child.wait() {
                tracing::error!("the breakaway launch probe could not be reaped: {error}");
                SpawnAttempt::Spawned {
                    cleanup: Some(format!("could not reap the suspended probe: {error}")),
                }
            } else {
                SpawnAttempt::Spawned { cleanup: None }
            }
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

    /// A non-UTF-8 path cannot be a Winsock AF_UNIX address; it is reported as
    /// unusable rather than measured with a lossy conversion.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_control_path_is_unusable() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"/tmp/\xff-control.sock"));
        assert!(matches!(socket_budget(path, 108), SocketBudget::Unusable { .. }));
    }

    /// An elevated token fails the check with the non-elevated-terminal
    /// instruction, and neither diagnostics nor a start may follow.
    #[test]
    fn an_elevated_token_blocks_diagnostics_and_start() {
        let pre = preconditions(Elevation::Elevated, Breakaway::Allowed, fits());
        assert!(!pre.allows_diagnostics());
        let refusal = pre.start_refusal().expect("a refusal");
        assert!(matches!(refusal, CodexError::WindowsPrecondition(_)));
        let message = refusal.to_string();
        assert!(message.contains("non-elevated"), "{message}");
        assert!(message.contains("administrator privileges"), "{message}");
    }

    /// An unqueryable token is **not** a non-elevated one: doctor warns, a
    /// read-only diagnosis may still run, and a start is refused.
    #[test]
    fn an_unknown_elevation_allows_diagnostics_but_refuses_a_start() {
        let pre = preconditions(
            Elevation::Unknown("OpenProcessToken failed: access denied".into()),
            Breakaway::Allowed,
            fits(),
        );
        assert!(pre.allows_diagnostics());
        let refusal = pre.start_refusal().expect("a refusal");
        assert!(matches!(refusal, CodexError::WindowsPrecondition(_)));
        let message = refusal.to_string();
        assert!(message.contains("could not be read"), "{message}");
        assert!(message.contains("access denied"), "{message}");
        assert!(message.contains("refusing"), "{message}");
    }

    /// A launcher restriction is a start precondition, not an attach one: an
    /// existing daemon can still be read, but `daemon start` is refused.
    #[test]
    fn a_rejected_breakaway_blocks_start_but_not_diagnostics() {
        let pre = preconditions(Elevation::NotElevated, Breakaway::Rejected, fits());
        assert!(pre.allows_diagnostics());
        let refusal = pre.start_refusal().expect("a refusal");
        let message = refusal.to_string();
        assert!(message.contains("cargo run"), "{message}");
        assert!(message.contains("job object"), "{message}");
        assert!(message.contains("codex.exe"), "{message}");
    }

    /// A skipped probe is not a rejection: it says why it did not run and
    /// never refuses a start beyond the precondition that skipped it.
    #[test]
    fn a_skipped_breakaway_is_not_a_rejection() {
        let pre = preconditions(
            Elevation::NotElevated,
            Breakaway::Skipped("control path unusable".into()),
            fits(),
        );
        assert!(pre.allows_diagnostics());
        assert!(pre.start_refusal().is_none());
        assert!(pre.breakaway_instruction().is_none());
    }

    /// An over-budget path is refused before anything is spawned, with the
    /// canonical #950 error, and the instruction names the setting to shorten.
    #[test]
    fn an_over_budget_socket_blocks_before_any_spawn() {
        let socket = socket_budget(Path::new(&"b".repeat(108)), 108);
        let pre = preconditions(Elevation::NotElevated, Breakaway::Allowed, socket);
        assert!(!pre.allows_diagnostics());
        let refusal = pre.start_refusal().expect("a refusal");
        assert!(matches!(refusal, CodexError::ControlSocketPathTooLong { .. }));
        let message = refusal.to_string();
        assert!(message.contains("CODEX_HOME"), "{message}");
        assert!(message.contains("108"), "{message}");
    }

    /// The retry is what separates "job forbids breakaway" from "this file
    /// cannot run": only a succeeding retry is a named launcher rejection.
    #[test]
    fn breakaway_classification_requires_a_successful_retry() {
        assert_eq!(
            classify_breakaway(SpawnAttempt::Spawned { cleanup: None }, None),
            Breakaway::Allowed
        );
        assert_eq!(
            classify_breakaway(
                SpawnAttempt::AccessDenied,
                Some(SpawnAttempt::Spawned { cleanup: None })
            ),
            Breakaway::Rejected
        );
        assert!(matches!(
            classify_breakaway(
                SpawnAttempt::AccessDenied,
                Some(SpawnAttempt::Failed("cannot execute".into()))
            ),
            Breakaway::Unavailable(detail) if detail.contains("cannot execute")
        ));
        // A retry that is also denied proves nothing about the job.
        assert!(matches!(
            classify_breakaway(
                SpawnAttempt::AccessDenied,
                Some(SpawnAttempt::AccessDenied)
            ),
            Breakaway::Unavailable(detail)
                if detail.contains("without CREATE_BREAKAWAY_FROM_JOB")
        ));
        assert!(matches!(
            classify_breakaway(SpawnAttempt::Failed("no such file".into()), None),
            Breakaway::Unavailable(detail) if detail.contains("no such file")
        ));
    }

    /// A cleanup failure after a successful spawn is a leak to log, not proof
    /// that the launcher is restricted: the spawn evidence still wins.
    #[test]
    fn a_failed_cleanup_keeps_the_spawn_evidence() {
        assert_eq!(
            classify_breakaway(
                SpawnAttempt::Spawned {
                    cleanup: Some("kill failed".into())
                },
                None
            ),
            Breakaway::Allowed
        );
        assert_eq!(
            classify_breakaway(
                SpawnAttempt::AccessDenied,
                Some(SpawnAttempt::Spawned {
                    cleanup: Some("kill failed".into())
                })
            ),
            Breakaway::Rejected
        );
    }

    /// An unresolvable home still reports the token and skips the launcher
    /// spawn; the path row is the one that degrades to "unlocalized".
    #[cfg(windows)]
    #[test]
    fn an_unlocalized_home_still_probes_the_token() {
        let pre = probe(None);
        assert!(matches!(pre.socket, SocketBudget::Unlocalized(_)));
        assert!(matches!(pre.breakaway, Breakaway::Skipped(_)));
    }

    /// Every refusal that already blocks a start skips the breakaway spawn,
    /// including an unreadable token (which still allows read-only work).
    #[test]
    fn a_refused_start_skips_the_breakaway_spawn() {
        assert!(skip_breakaway(&Elevation::Elevated, &fits()).is_some());
        assert!(skip_breakaway(&Elevation::Unknown("query failed".into()), &fits()).is_some());
        assert_eq!(skip_breakaway(&Elevation::NotElevated, &fits()), None);
        assert!(
            skip_breakaway(
                &Elevation::NotElevated,
                &SocketBudget::Unlocalized("no home".into())
            )
            .is_some()
        );
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
        assert!(pre.breakaway_instruction().is_none());
    }
}
