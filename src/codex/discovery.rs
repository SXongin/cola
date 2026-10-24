//! Daemon discovery under the shared `CODEX_HOME` (spec #860, ticket #877).
//!
//! Codex's peer surface is `codex app-server`, and the supported configuration
//! is the user's own client attached to the **app-server daemon** (ADR-0090
//! decision 3): the daemon owns the thread store, and every client — the TUI,
//! the desktop/mobile apps, cola — attaches to it. This module is the read side
//! of that configuration:
//!
//! - the control socket is `$CODEX_HOME/app-server-control/app-server-control.sock`,
//!   so a relocated `CODEX_HOME` moves it with the rest of Codex's state. It is
//!   normally a **symlink** into `/tmp/codex-daemon-0/<hash>`; the path is used
//!   verbatim and the OS follows the link at connect time (never canonicalised,
//!   never required to be a regular file);
//! - the binary that must be probed for the schema is the one the **daemon
//!   runs**, which is not the `codex` on `PATH`: the daemon's management CLI
//!   reports `managedCodexPath` (`codex app-server daemon version`), and the
//!   daemon package path under `CODEX_HOME` is the fallback;
//! - a local client running with `--no-daemon` keeps its app-server **in
//!   process**, so it shares nothing; that state is reported, never mirrored.
//!
//! Nothing here starts, stops, yields or reaps a daemon except
//! [`start`] — which runs Codex's own `daemon start` and nothing else. cola
//! keeps no pid of it and never signals it (ADR-0090 decision 3).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use super::error::{CodexError, Result};

/// The daemon's control directory, relative to `CODEX_HOME`.
pub(crate) const CONTROL_DIR: &str = "app-server-control";

/// The control socket's file name inside [`CONTROL_DIR`]. It is a symlink into
/// the daemon's runtime directory; the OS follows it.
pub(crate) const CONTROL_SOCKET: &str = "app-server-control.sock";

/// Where a managed daemon package installs its own binary, relative to
/// `CODEX_HOME`. Used when the management CLI cannot report `managedCodexPath`.
pub(crate) const MANAGED_BINARY: &str = "packages/app-server-daemon/current/bin/codex";

/// How long the management CLI gets to report the daemon's state.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `daemon start` gets to return. The command daemonizes and returns
/// promptly; a bound keeps a wedged CLI from holding an attach pass forever.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a freshly started daemon gets before its control socket must exist.
const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the readiness loop re-reads the socket path.
const SOCKET_READY_STEP: Duration = Duration::from_millis(100);

/// Everything discovery needs to know about the machine, passed explicitly so a
/// test never has to mutate the process environment.
///
/// `home` is the shared `CODEX_HOME`; the *same* value is passed to every
/// management command as `CODEX_HOME`, so a relocated home reaches the CLI even
/// when this process's environment says otherwise.
#[derive(Debug, Clone)]
pub(crate) struct Environment {
    pub(crate) home: PathBuf,
    /// The `codex` CLI used for `app-server daemon …`. Discovery does not
    /// require an absolute path: a bare `codex` is resolved through `PATH` like
    /// the user's own shell would.
    pub(crate) cli: PathBuf,
    pub(crate) version_timeout: Duration,
    pub(crate) start_timeout: Duration,
    pub(crate) socket_ready_timeout: Duration,
}

impl Environment {
    /// Resolve `CODEX_HOME` and the CLI from the process environment.
    pub(crate) fn from_env() -> Result<Self> {
        let home = resolve_home(
            std::env::var_os("CODEX_HOME").map(PathBuf::from),
            dirs::home_dir(),
        )?;
        Ok(Self {
            home,
            cli: PathBuf::from("codex"),
            version_timeout: VERSION_TIMEOUT,
            start_timeout: START_TIMEOUT,
            socket_ready_timeout: SOCKET_READY_TIMEOUT,
        })
    }

    /// The daemon's control socket under this home.
    pub(crate) fn socket(&self) -> PathBuf {
        self.home.join(CONTROL_DIR).join(CONTROL_SOCKET)
    }

    /// The managed daemon binary a standard install keeps under this home.
    fn package_binary(&self) -> PathBuf {
        self.home.join(MANAGED_BINARY)
    }
}

/// Resolve the shared `CODEX_HOME`: `$CODEX_HOME` when set and non-empty,
/// otherwise `~/.codex`. A relative value is refused — Codex itself requires an
/// absolute path, and silently resolving one against cola's cwd would attach to
/// a different store than the user's client reads.
fn resolve_home(from_env: Option<PathBuf>, home_dir: Option<PathBuf>) -> Result<PathBuf> {
    let configured = from_env.filter(|path| !path.as_os_str().is_empty());
    let home = configured
        .or_else(|| home_dir.map(|home| home.join(".codex")))
        .ok_or_else(|| {
            CodexError::CodexHome(
                "neither `CODEX_HOME` nor the user's home directory is set, so there is no Codex \
                 state directory to discover"
                    .to_string(),
            )
        })?;
    if !home.is_absolute() {
        return Err(CodexError::CodexHome(format!(
            "`{}` is not an absolute path; Codex requires an absolute `CODEX_HOME`",
            home.display()
        )));
    }
    Ok(home)
}

/// The identity of the control socket on disk, used to notice that a daemon was
/// replaced under the same path (a restart gives the daemon a new runtime
/// directory, so its symlink target's inode changes).
///
/// On a platform without unix-domain sockets the discovery is a named failure,
/// not a panic and not a silent "absent" (ADR-0090's open question 4, Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SocketIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

/// Read the control socket's identity, following a symlink. `Ok(None)` means
/// no socket is there (a dangling link counts as absent — the daemon is gone);
/// an existing non-socket is a named error, never a silent absence.
pub(crate) fn socket_identity(path: &Path) -> Result<Option<SocketIdentity>> {
    #[cfg(not(unix))]
    {
        Err(CodexError::UnsupportedPlatform {
            path: path.display().to_string(),
            platform: std::env::consts::OS,
        })
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};

        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(CodexError::DaemonSocket {
                    socket: path.display().to_string(),
                    detail: error.to_string(),
                });
            }
        };
        if !metadata.file_type().is_socket() {
            return Err(CodexError::DaemonSocket {
                socket: path.display().to_string(),
                detail: "the path exists but is not a unix domain socket".to_string(),
            });
        }
        Ok(Some(SocketIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
}

/// The JSON report of `codex app-server daemon version` (and `daemon start`),
/// field for field as Codex prints it. Only the daemon's actual binary and
/// versions are load-bearing here; the rest is evidence for `cola doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DaemonInfo {
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) backend: Option<String>,
    /// The binary the running daemon executes. This — not the `codex` on
    /// `PATH` — is what the schema probe must ask (spec #860 ticket #876's
    /// finding: a 0.163 CLI cannot vouch for a 0.164 daemon).
    #[serde(default)]
    pub(crate) managed_codex_path: Option<PathBuf>,
    #[serde(default)]
    pub(crate) managed_codex_version: Option<String>,
    #[serde(default)]
    pub(crate) socket_path: Option<PathBuf>,
    #[serde(default)]
    pub(crate) cli_version: Option<String>,
    #[serde(default)]
    pub(crate) app_server_version: Option<String>,
}

/// A running daemon.
#[derive(Debug, Clone)]
pub(crate) struct Daemon {
    /// The control socket the daemon serves, as the path under `CODEX_HOME`.
    pub(crate) socket: PathBuf,
    /// The binary the daemon actually runs, for the schema probe.
    pub(crate) managed_binary: PathBuf,
    /// The management CLI's report, when it could produce one.
    // Read by `cola doctor` (#883); discovery keeps it decoded today.
    #[allow(dead_code)]
    pub(crate) info: Option<DaemonInfo>,
}

/// What discovery found. Absence is a state, not an error: `start_server =
/// "never"` treats it as the steady state, and `auto` is the one policy that may
/// start a daemon from here.
#[derive(Debug, Clone)]
pub(crate) enum DaemonState {
    Running(Daemon),
    /// No daemon: no socket at the path, or the path is not dialable. The
    /// evidence is for the log and `cola doctor`.
    Absent {
        // Read by `cola doctor` (#883).
        #[allow(dead_code)]
        evidence: String,
    },
}

impl DaemonState {
    pub(crate) fn daemon(&self) -> Option<&Daemon> {
        match self {
            Self::Running(daemon) => Some(daemon),
            Self::Absent { .. } => None,
        }
    }
}

/// A local Codex client whose app-server lives in its own process, so it shares
/// no thread with the daemon (ADR-0090 decision 3). Reported, never mirrored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmbeddedClient {
    pub(crate) pid: i32,
    pub(crate) binary: String,
    /// Why this process is read as an embedded-server client.
    pub(crate) evidence: &'static str,
}

/// A local Codex TUI proven to share the daemon: it holds a live unix-socket
/// connection to the daemon's control socket.
///
/// This is the positive half of ADR-0090 decision 3's diagnosis: the user's own
/// client is attached to the one runtime cola mirrors. An argv match is not
/// proof — a TUI started before the control symlink was re-pointed still names
/// the path while remaining connected to the old daemon — so [`prove_attachments`]
/// settles it from the process's open descriptors where the OS exposes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachedClient {
    pub(crate) pid: i32,
    pub(crate) binary: String,
    /// The endpoint spelling the process was launched with (or the daemon's
    /// control socket for an implicit attachment), for the report.
    pub(crate) endpoint: String,
    pub(crate) detail: String,
}

/// A local Codex TUI whose sharing the available evidence cannot settle.
///
/// A TUI launched with neither `--remote` nor `--no-daemon` normally attaches to
/// the shared daemon — but one started **before** the daemon appeared fell back
/// to its own embedded server and stays there. The process's argv cannot tell
/// the two apart. On Linux the open descriptors can, and [`prove_attachments`]
/// upgrades a live connection to [`AttachedClient`]; where that evidence is
/// unavailable the state stays **unverified**, never assumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnverifiedClient {
    pub(crate) pid: i32,
    pub(crate) binary: String,
    /// The `--remote` endpoint the client named, when it named one.
    pub(crate) endpoint: Option<String>,
    pub(crate) detail: String,
    /// The process's start time from `/proc/<pid>/stat` field 22, captured by
    /// the same scan that read its argv. The descriptor proof re-checks it
    /// before and after reading `/proc/<pid>/fd`, so a reused pid cannot lend
    /// another process's sockets to this client. `None` where the platform
    /// exposes no equivalent.
    pub(crate) start_time: Option<u64>,
}

/// A local Codex TUI pointed at an endpoint that is not the discovered daemon
/// (or at no reachable daemon at all). It cannot share this daemon's threads.
///
/// Deliberately not "embedded": a remote WebSocket is equally unmatched, and
/// calling it embedded would claim evidence the process table does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnreachableClient {
    pub(crate) pid: i32,
    pub(crate) binary: String,
    pub(crate) endpoint: String,
    pub(crate) detail: String,
}

/// The local Codex clients one process-table scan observed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LocalClients {
    /// Clients proven to run their own app-server.
    pub(crate) embedded: Vec<EmbeddedClient>,
    /// Clients proven to hold a live connection to the discovered daemon.
    pub(crate) attached: Vec<AttachedClient>,
    /// Clients whose sharing the available evidence cannot settle.
    pub(crate) unverified: Vec<UnverifiedClient>,
    /// Clients pointed at another (or no) endpoint.
    pub(crate) unreachable: Vec<UnreachableClient>,
}

/// A running daemon together with the control socket identity the **same**
/// discovery pass read for it.
///
/// The two are one fact and must stay paired: the managed binary to probe
/// belongs to the daemon behind that socket. Probing daemon B's binary while
/// dialling the socket a *later* read points at (daemon C) checks C's server
/// against B's schema — `AutoProbe` then refuses a healthy server with a
/// version mismatch, or worse, vouches for one build with another's surface.
/// Every pairing here therefore comes from one `discover` pass.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredDaemon {
    pub(crate) daemon: Daemon,
    pub(crate) identity: SocketIdentity,
}

/// One discovery pass.
#[derive(Debug, Clone)]
pub(crate) struct Discovery {
    pub(crate) daemon: DaemonState,
    /// The socket's on-disk identity when one exists; a later pass compares it
    /// to notice a replaced daemon.
    pub(crate) socket: Option<SocketIdentity>,
    /// Local Codex clients, in pid order.
    pub(crate) clients: LocalClients,
}

impl Discovery {
    /// The running daemon and the socket identity this pass observed for it, as
    /// one pair (see [`DiscoveredDaemon`]).
    ///
    /// `None` when no daemon runs. A `Running` state always carries the identity
    /// that gated it, so the pair cannot be half-built.
    pub(crate) fn running(&self) -> Option<DiscoveredDaemon> {
        match &self.daemon {
            DaemonState::Running(daemon) => self.socket.map(|identity| DiscoveredDaemon {
                daemon: daemon.clone(),
                identity,
            }),
            DaemonState::Absent { .. } => None,
        }
    }
}

/// One discovery pass, reduced to the running daemon pair.
pub(crate) async fn discover_daemon(environment: &Environment) -> Result<Option<DiscoveredDaemon>> {
    Ok(discover(environment).await?.running())
}

/// Discover the daemon under `environment.home`.
///
/// The socket's existence is the ground truth for "a daemon is there": the
/// management CLI is then asked which binary it runs, and a CLI that cannot
/// answer does not make a serving socket disappear (the package path is the
/// fallback). An unsupported platform or an undialable path is a named error.
pub(crate) async fn discover(environment: &Environment) -> Result<Discovery> {
    let socket = environment.socket();
    let identity = socket_identity(&socket)?;
    let daemon = match identity {
        None => DaemonState::Absent {
            evidence: format!("no control socket at {}", socket.display()),
        },
        Some(_) => {
            let info = match daemon_version(environment).await {
                Ok(info) => Some(info),
                Err(error) => {
                    // The socket is still the evidence that a daemon serves;
                    // the report only names its binary. Degrade to the package
                    // fallback and keep the reason at debug level — the poll
                    // loop re-runs this every few seconds, and a wrong fallback
                    // makes the probe refuse loudly rather than silently.
                    tracing::debug!(
                        "codex daemon control socket {} exists, but its version report failed: {}",
                        socket.display(),
                        error
                    );
                    None
                }
            };
            DaemonState::Running(Daemon {
                socket,
                managed_binary: managed_binary(environment, info.as_ref()),
                info,
            })
        }
    };
    let running = daemon.daemon();
    let clients = local_clients(running.map(|daemon| daemon.socket.as_path()));
    Ok(Discovery {
        daemon,
        socket: identity,
        clients,
    })
}

/// The binary to probe: the one the daemon reports it runs, the managed package
/// under `CODEX_HOME`, or — last resort — the CLI on `PATH`. The probe itself
/// refuses a binary whose version does not match the server's, so a wrong last
/// resort fails loudly rather than vouching for the daemon.
fn managed_binary(environment: &Environment, info: Option<&DaemonInfo>) -> PathBuf {
    if let Some(path) = info
        .and_then(|info| info.managed_codex_path.clone())
        .filter(|path| !path.as_os_str().is_empty())
    {
        return path;
    }
    let package = environment.package_binary();
    if package.is_file() {
        return package;
    }
    environment.cli.clone()
}

/// Ask the management CLI what the daemon runs.
async fn daemon_version(environment: &Environment) -> Result<DaemonInfo> {
    let output = run_cli(
        environment,
        &["app-server", "daemon", "version"],
        environment.version_timeout,
    )
    .await?;
    if !output.status.success() {
        return Err(CodexError::Daemon(format!(
            "`{} app-server daemon version` exited with {}: {}",
            environment.cli.display(),
            output.status,
            first_line(&output.stderr)
        )));
    }
    parse_info(&output.stdout)
}

fn parse_info(stdout: &[u8]) -> Result<DaemonInfo> {
    let text = String::from_utf8_lossy(stdout);
    serde_json::from_str(text.trim()).map_err(|error| {
        CodexError::Daemon(format!(
            "`app-server daemon version` printed no daemon report: {error} (output: {:?})",
            first_line(stdout)
        ))
    })
}

/// Start the daemon through Codex's own management command and return the
/// daemon that is serving afterwards, paired with its socket identity.
///
/// This is the only place cola causes a daemon to exist, and it does so by
/// running the CLI's documented command — never by spawning the server itself,
/// writing a pid record, or holding a handle it could signal. A timeout that
/// drops the CLI's child does **not** kill it: cola does not reap what it does
/// not own.
///
/// A cold start is recognised by the control socket's identity *changing*: a
/// daemon that crashed can leave its Unix socket path behind, and treating that
/// corpse as the new daemon would report a dead server as started. Codex
/// re-links the path into a fresh runtime directory, so a successful cold start
/// always produces a new identity.
///
/// The command is idempotent, though, and the identity can change *before* it
/// runs (a replacement appearing while a failed dial was being decided): then
/// the command starts nothing and no identity changes. A daemon already serving
/// is the outcome we asked for, so that case returns the discovered daemon
/// instead of a timeout — the caller's attach still proves it answers.
pub(crate) async fn start(environment: &Environment) -> Result<DiscoveredDaemon> {
    let socket = environment.socket();
    // A socket path left behind by a crashed daemon is not a fresh one: the
    // launcher must produce a **new** socket (Codex re-links the path into a new
    // runtime directory), or the readiness check would pass on the corpse.
    let before = socket_identity(&socket)?;
    let output = run_cli(
        environment,
        &["app-server", "daemon", "start"],
        environment.start_timeout,
    )
    .await?;
    if !output.status.success() {
        return Err(CodexError::Daemon(format!(
            "`{} app-server daemon start` exited with {}: {}",
            environment.cli.display(),
            output.status,
            first_line(&output.stderr)
        )));
    }
    let deadline = tokio::time::Instant::now() + environment.socket_ready_timeout;
    loop {
        if let Some(identity) = socket_identity(&socket)?
            && Some(identity) != before
        {
            return discovered_after_start(environment).await;
        }
        if tokio::time::Instant::now() >= deadline {
            // `daemon start` is **idempotent**: a daemon that appeared between
            // our snapshot and the command (a replacement racing the failed
            // dial) makes the command a no-op, so no identity changes. A daemon
            // already serving is what we asked for — return it rather than a
            // timeout for a start that had nothing to do. The caller's attach
            // still proves the daemon answers; a corpse behind a stale path
            // fails there, never as "attached".
            if let Some(daemon) = discover_daemon(environment).await? {
                return Ok(daemon);
            }
            return Err(CodexError::Daemon(format!(
                "`{} app-server daemon start` reported success, but {} did not appear (or was \
                 not replaced) within {:?}",
                environment.cli.display(),
                socket.display(),
                environment.socket_ready_timeout
            )));
        }
        tokio::time::sleep(SOCKET_READY_STEP).await;
    }
}

/// The daemon a successful start produced (a changed identity), as one pair.
async fn discovered_after_start(environment: &Environment) -> Result<DiscoveredDaemon> {
    discover_daemon(environment).await?.ok_or_else(|| {
        CodexError::Daemon(format!(
            "`{} app-server daemon start` reported success, but the control socket at {} is not \
             discoverable",
            environment.cli.display(),
            environment.socket().display()
        ))
    })
}

/// Run one management command with `CODEX_HOME` pinned to the environment's
/// home, stdin closed, and a bound.
///
/// The child is deliberately **not** killed on drop: the command is Codex's own
/// launcher, never the daemon, but cola still reaps nothing it did not start.
async fn run_cli(
    environment: &Environment,
    arguments: &[&str],
    timeout: Duration,
) -> Result<std::process::Output> {
    let mut command = tokio::process::Command::new(&environment.cli);
    command
        .args(arguments)
        .env("CODEX_HOME", &environment.home)
        .stdin(std::process::Stdio::null());
    match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(CodexError::Daemon(format!(
            "run `{} {}`: {error}",
            environment.cli.display(),
            arguments.join(" ")
        ))),
        Err(_) => Err(CodexError::Daemon(format!(
            "`{} {}` did not return within {timeout:?}",
            environment.cli.display(),
            arguments.join(" ")
        ))),
    }
}

/// The first non-empty line of a command's stderr, flattened for one log line.
fn first_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no diagnostic output")
        .to_string()
}

/// Read the process table once for local Codex clients.
///
/// Thin I/O: the decisions are [`observe_client`], unit-tested on their own. The
/// table is read with sysinfo so this works on every platform (there is no
/// `/proc` on macOS or Windows).
///
/// `daemon` is the discovered control socket when a daemon is running. It is
/// what proves a `--remote` client shares this daemon; `None` means no daemon,
/// so a TUI with no `--remote` fell back to its own embedded server.
pub(crate) fn local_clients(daemon: Option<&Path>) -> LocalClients {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cmd(UpdateKind::Always),
    );
    let mut found = LocalClients::default();
    for process in system.processes().values() {
        let arguments: Vec<String> = process
            .cmd()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let pid = process.pid().as_u32() as i32;
        // Bracket the argv read with the start-time read: if the pid was
        // reused while sysinfo was reading it, the two reads disagree and the
        // client stays unverified instead of borrowing the new process's
        // identity.
        let before = process_start_time(pid);
        let observation = observe_client(pid, &process.name().to_string_lossy(), &arguments, daemon);
        let after = process_start_time(pid);
        let start_time = if before == after { before } else { None };
        match observation {
            Some(Observation::Embedded(client)) => found.embedded.push(client),
            Some(Observation::Unverified(mut client)) => {
                client.start_time = start_time;
                found.unverified.push(client);
            }
            Some(Observation::Unreachable(client)) => found.unreachable.push(client),
            None => {}
        }
    }
    // sysinfo enumerates in a randomized order; a report that changes shape
    // between two identical scans would be noise.
    found.embedded.sort_by_key(|client| client.pid);
    found.unverified.sort_by_key(|client| client.pid);
    found.unreachable.sort_by_key(|client| client.pid);
    found
}

/// The subcommands that are not the interactive TUI. A TUI is `codex`, `codex
/// <prompt>`, `codex resume …` or `codex fork …`; the rest either need the
/// shared daemon themselves (`agents`) or never start an app-server.
const NON_TUI_COMMANDS: &[&str] = &[
    "app-server",
    "exec",
    "e",
    "exec-server",
    "review",
    "agents",
    "login",
    "logout",
    "mcp",
    "plugin",
    "remote-control",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "queue",
    "archive",
    "delete",
    "migrate-rollouts",
    "unarchive",
    "cloud",
    "features",
    "help",
];

/// Options whose value is the next argv token. Used to find the subcommand
/// without mistaking a `-c model="x"` value for one.
const OPTIONS_WITH_VALUES: &[&str] = &[
    "--remote",
    "--remote-auth-token-env",
    "-c",
    "--config",
    "-m",
    "--model",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-C",
    "--cd",
    "-a",
    "--ask-for-approval",
    "-i",
    "--image",
    "--local-provider",
    "--enable",
    "--disable",
    "--code-mode-host",
];

/// Whether a process's argv is the interactive Codex TUI.
fn tui_invocation(arguments: &[String]) -> bool {
    let mut tokens = arguments.iter().skip(1);
    while let Some(token) = tokens.next() {
        if OPTIONS_WITH_VALUES.contains(&token.as_str()) {
            tokens.next();
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        return !NON_TUI_COMMANDS.contains(&token.as_str());
    }
    // No positional token at all: `codex` or `codex --model x` — the TUI.
    true
}

/// What a Codex TUI process proves about where its app-server lives.
#[derive(Debug, PartialEq, Eq)]
enum Observation {
    Embedded(EmbeddedClient),
    Unverified(UnverifiedClient),
    Unreachable(UnreachableClient),
}

/// Read one process as a local Codex TUI.
///
/// `--no-daemon` is the documented, decisive opt-out (`Run without the shared
/// background server`), and Codex only accepts it on the TUI, so that process
/// is **embedded**. A TUI with no daemon running and no `--remote` endpoint has
/// nowhere else to attach and is embedded too. A `--remote` TUI is
/// **unverified** when its endpoint names the discovered control socket (argv
/// is intent, not a live connection; [`prove_attachments`] settles it) and
/// **unreachable** when it names another endpoint or no daemon is running. With
/// a daemon present, a TUI's argv without `--remote` does **not** prove it
/// joined it — one started before the daemon appeared stays on its private
/// server — so it too is reported as unverified, never assumed to share.
fn observe_client(
    pid: i32,
    process_name: &str,
    arguments: &[String],
    daemon: Option<&Path>,
) -> Option<Observation> {
    let is_codex_client = arguments
        .first()
        .is_some_and(|binary| Path::new(binary).file_name().is_some_and(|name| name == "codex"))
        || process_name == "codex";
    if !is_codex_client || !tui_invocation(arguments) {
        return None;
    }
    let binary = arguments.first().cloned().unwrap_or_else(|| "codex".to_string());
    if arguments.iter().any(|argument| argument == "--no-daemon") {
        return Some(Observation::Embedded(EmbeddedClient {
            pid,
            binary,
            evidence: "runs with `--no-daemon`, so its app-server is in-process",
        }));
    }
    if let Some(endpoint) = remote_endpoint(arguments) {
        if daemon.is_some_and(|socket| endpoint_matches(&endpoint, socket)) {
            // The argv names this daemon, not proof of a live connection: a
            // TUI started before the control symlink was re-pointed still
            // names the path while attached to the old daemon.
            return Some(Observation::Unverified(UnverifiedClient {
                pid,
                binary,
                endpoint: Some(endpoint),
                detail: "its `--remote` names the daemon, but argv is not proof of a live \
                         connection"
                    .to_string(),
                start_time: None,
            }));
        }
        let detail = match daemon {
            Some(socket) => format!(
                "points at `{endpoint}`, not the discovered daemon at {}",
                socket.display()
            ),
            None => format!("points at `{endpoint}`, and no shared daemon is running"),
        };
        return Some(Observation::Unreachable(UnreachableClient {
            pid,
            binary,
            endpoint,
            detail,
        }));
    }
    if daemon.is_none() {
        return Some(Observation::Embedded(EmbeddedClient {
            pid,
            binary,
            evidence: "no daemon is running, so the TUI fell back to an embedded app-server",
        }));
    }
    Some(Observation::Unverified(UnverifiedClient {
        pid,
        binary,
        endpoint: None,
        detail: "names no `--remote`; whether it shares the daemon cannot be established from \
                 argv"
            .to_string(),
        start_time: None,
    }))
}

/// The endpoint a TUI's `--remote` names, in either `--remote <value>` or
/// `--remote=<value>` form.
fn remote_endpoint(arguments: &[String]) -> Option<String> {
    let mut tokens = arguments.iter().skip(1);
    while let Some(token) = tokens.next() {
        if token == "--remote" {
            return tokens.next().cloned();
        }
        if let Some(endpoint) = token.strip_prefix("--remote=") {
            return Some(endpoint.to_string());
        }
    }
    None
}

/// Whether a `--remote` endpoint names `daemon`. Only a unix endpoint can be
/// compared with the daemon's control socket; a `ws://` remote is somebody
/// else's listener as far as a process scan can tell, so it is never claimed.
fn endpoint_matches(endpoint: &str, daemon: &Path) -> bool {
    let Some(remote) = endpoint
        .strip_prefix("unix://")
        .or_else(|| endpoint.strip_prefix("unix:"))
        .map(Path::new)
    else {
        return false;
    };
    match (std::fs::canonicalize(remote), std::fs::canonicalize(daemon)) {
        (Ok(remote), Ok(daemon)) => remote == daemon,
        _ => remote == daemon,
    }
}

/// How long `ss` gets to report the kernel's established unix-socket table.
#[cfg(target_os = "linux")]
const SS_TIMEOUT: Duration = Duration::from_secs(2);

/// Upgrade local clients to **attached** only when their open descriptors prove
/// a live connection to `daemon` (ADR-0090 decision 3, ticket #883).
///
/// On Linux, `/proc/<pid>/fd/*` exposes each open socket's inode, and
/// `ss -x -e` pairs a socket with the filesystem inode of the socket at the
/// other end. Comparing the latter to the daemon control socket's own inode
/// proves the client is connected to *this* daemon: if the control-socket
/// symlink is re-pointed after the client connected, the old connection no
/// longer matches the new path and the client stays unverified. Where `/proc`,
/// `ss`, or the descriptor table is unavailable, every client stays unverified —
/// argv is never promoted to proof.
#[cfg(target_os = "linux")]
pub(crate) async fn prove_attachments(clients: &mut LocalClients, daemon: &Path) {
    use std::os::unix::fs::MetadataExt;

    let Ok(metadata) = std::fs::metadata(daemon) else {
        return;
    };
    let daemon_inode = metadata.ino();
    let Some(peers) = established_unix_peers().await else {
        return;
    };
    let mut attached = Vec::new();
    let mut unverified = Vec::new();
    for client in std::mem::take(&mut clients.unverified) {
        // Pin the PID: the scan that read the argv and this proof are two
        // separate `/proc` reads, and the id may be reused between them.
        if client.start_time.is_none() || process_start_time(client.pid) != client.start_time {
            unverified.push(client);
            continue;
        }
        let socket_inodes = process_socket_inodes(client.pid);
        if process_start_time(client.pid) != client.start_time {
            unverified.push(client);
            continue;
        }
        if socket_inodes.is_empty() || !peer_matches(&socket_inodes, &peers, daemon_inode) {
            unverified.push(client);
            continue;
        }
        attached.push(AttachedClient {
            pid: client.pid,
            binary: client.binary,
            endpoint: client
                .endpoint
                .unwrap_or_else(|| format!("unix://{}", daemon.display())),
            detail: format!(
                "holds a live unix-socket connection to the daemon at {}",
                daemon.display()
            ),
        });
    }
    clients.attached = attached;
    clients.unverified = unverified;
    clients.attached.sort_by_key(|client| client.pid);
}

/// Non-Linux platforms expose no equivalent descriptor evidence here; never
/// promote argv to proof.
#[cfg(not(target_os = "linux"))]
pub(crate) async fn prove_attachments(_clients: &mut LocalClients, _daemon: &Path) {}

/// The kernel's established unix-socket pairs as `(filesystem inode, peer
/// socket inode)`.
///
/// `ss -x -e` is the only portable way to learn a socket's **peer**: the
/// descriptor target (`socket:[inode]`) and `/proc/net/unix` carry no peer
/// information. A missing `ss` is not an error — it downgrades the diagnosis to
/// unverified.
#[cfg(target_os = "linux")]
async fn established_unix_peers() -> Option<Vec<(u64, u64)>> {
    for program in ["ss", "/usr/sbin/ss", "/usr/bin/ss", "/sbin/ss"] {
        let output = tokio::time::timeout(
            SS_TIMEOUT,
            tokio::process::Command::new(program).args(["-x", "-e"]).output(),
        )
        .await;
        if let Ok(Ok(output)) = output
            && output.status.success()
        {
            return Some(parse_established_unix_peers(&String::from_utf8_lossy(
                &output.stdout,
            )));
        }
    }
    None
}

/// Parse the `u_str ESTAB … <-> ino:<fs-inode> dev:<…>` rows of `ss -x -e`.
///
/// The `-e` suffix is fixed, so the fields are read from the right: the peer
/// inode is the last numeric before `<->`, and the local socket's filesystem
/// inode follows `ino:`. This survives a path containing spaces, which the
/// space-separated middle of the line does not.
#[cfg(target_os = "linux")]
fn parse_established_unix_peers(text: &str) -> Vec<(u64, u64)> {
    let mut peers = Vec::new();
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 8 || tokens[0] != "u_str" || tokens[1] != "ESTAB" {
            continue;
        }
        let Some(arrow) = tokens.get(tokens.len().saturating_sub(3)) else {
            continue;
        };
        if *arrow != "<->" {
            continue;
        }
        let Some(filesystem_inode) = tokens
            .get(tokens.len().saturating_sub(2))
            .and_then(|token| token.strip_prefix("ino:"))
            .and_then(|inode| inode.parse::<u64>().ok())
        else {
            continue;
        };
        let Some(peer) = tokens
            .get(tokens.len().saturating_sub(4))
            .and_then(|inode| inode.parse::<u64>().ok())
        else {
            continue;
        };
        peers.push((filesystem_inode, peer));
    }
    peers
}

/// The process's start time (`/proc/<pid>/stat` field 22, in clock ticks since
/// boot), read from the same process table the argv came from.
///
/// The `comm` field is parenthesised and may contain spaces, so the fields are
/// counted after the **last** `)`; `starttime` is field 22, i.e. the 20th
/// whitespace-separated token after the command name.
#[cfg(target_os = "linux")]
fn process_start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

/// No portable start-time source outside Linux; the descriptor proof is a
/// Linux-only path, so this is only reached by the scan's shared struct.
#[cfg(not(target_os = "linux"))]
fn process_start_time(_pid: i32) -> Option<u64> {
    None
}

/// The socket inodes a process holds open, from `/proc/<pid>/fd`.
#[cfg(target_os = "linux")]
fn process_socket_inodes(pid: i32) -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    let mut inodes = Vec::new();
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let Some(target) = target.to_str() else {
            continue;
        };
        let Some(rest) = target.strip_prefix("socket:[") else {
            continue;
        };
        let Some(inode) = rest.strip_suffix(']').and_then(|inode| inode.parse().ok()) else {
            continue;
        };
        inodes.push(inode);
    }
    inodes
}

/// Whether any of the process's sockets is the peer of a socket bound at
/// `daemon_inode`.
#[cfg(target_os = "linux")]
fn peer_matches(socket_inodes: &[u64], peers: &[(u64, u64)], daemon_inode: u64) -> bool {
    socket_inodes.iter().any(|inode| {
        peers
            .iter()
            .any(|(filesystem_inode, peer)| *filesystem_inode == daemon_inode && peer == inode)
    })
}

/// Resolve the Codex client binary the way a spawn would: an absolute or
/// explicit path is checked as given, a bare name is looked up on `PATH`.
/// Windows resolves executables by extension, so only `codex.exe` counts there.
pub(crate) fn which_client(cli: &Path) -> Option<PathBuf> {
    let explicit = cli.components().count() > 1 || cli.is_absolute();
    if explicit {
        return crate::update::is_executable_file(cli).then(|| cli.to_path_buf());
    }
    let name = cli.to_string_lossy();
    for directory in std::env::split_paths(&std::env::var_os("PATH")?) {
        #[cfg(windows)]
        let candidate = directory.join(format!("{name}.exe"));
        #[cfg(not(windows))]
        let candidate = directory.join(name.as_ref());
        if crate::update::is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    /// The pure resolver is where `CODEX_HOME` is honoured: the variable wins
    /// over the home directory, and the socket follows it.
    #[test]
    fn codex_home_follows_the_variable() {
        let environment = Environment {
            home: resolve_home(
                Some(PathBuf::from("/elsewhere/codex-home")),
                Some(PathBuf::from("/home/user")),
            )
            .expect("an absolute CODEX_HOME resolves"),
            cli: PathBuf::from("codex"),
            version_timeout: VERSION_TIMEOUT,
            start_timeout: START_TIMEOUT,
            socket_ready_timeout: SOCKET_READY_TIMEOUT,
        };
        assert_eq!(
            environment.socket(),
            Path::new("/elsewhere/codex-home/app-server-control/app-server-control.sock")
        );
        assert_eq!(
            environment.home.join(MANAGED_BINARY),
            Path::new("/elsewhere/codex-home/packages/app-server-daemon/current/bin/codex")
        );
    }

    #[test]
    fn an_absent_or_empty_variable_falls_back_to_the_default_home() {
        assert_eq!(
            resolve_home(None, Some(PathBuf::from("/home/user"))).expect("default home"),
            Path::new("/home/user/.codex")
        );
        assert_eq!(
            resolve_home(Some(PathBuf::new()), Some(PathBuf::from("/home/user"))).expect("empty means unset"),
            Path::new("/home/user/.codex")
        );
        let error = resolve_home(None, None).expect_err("no home is a named failure");
        assert!(error.to_string().contains("CODEX_HOME"), "{error}");
    }

    /// A relative `CODEX_HOME` is refused by name: resolving it against cola's
    /// cwd would attach to a different store than the user's client.
    #[test]
    fn a_relative_codex_home_is_refused() {
        let error = resolve_home(Some(PathBuf::from(".codex")), Some(PathBuf::from("/home/u")))
            .expect_err("a relative home is refused");
        assert!(matches!(error, CodexError::CodexHome(_)), "{error}");
        assert!(error.to_string().contains("absolute"), "{error}");
    }

    /// The daemon's own report shape, as `codex app-server daemon version`
    /// prints it on a machine with a managed 0.164 daemon and a 0.163 CLI.
    #[test]
    fn the_daemon_report_parses_the_running_binary() {
        let info = parse_info(
            br#"{"status":"running","backend":"pid","managedCodexPath":"/home/u/.codex/packages/app-server-daemon/current/bin/codex","managedCodexVersion":"0.164.0","socketPath":"/home/u/.codex/app-server-control/app-server-control.sock","cliVersion":"0.163.0","appServerVersion":"0.164.0"}"#,
        )
        .expect("the report parses");
        assert_eq!(info.status, "running");
        assert_eq!(
            info.managed_codex_path.as_deref(),
            Some(Path::new(
                "/home/u/.codex/packages/app-server-daemon/current/bin/codex"
            ))
        );
        assert_eq!(info.app_server_version.as_deref(), Some("0.164.0"));
        assert_eq!(info.cli_version.as_deref(), Some("0.163.0"));
        assert!(parse_info(b"not json").is_err());
    }

    /// The binary the daemon runs is preferred over anything else; without a
    /// report, the managed package path is the fallback and the CLI on `PATH`
    /// the last resort.
    #[test]
    fn the_probe_binary_follows_the_daemon_not_the_path_cli() {
        let home = tempfile::tempdir().expect("tempdir");
        let environment = Environment {
            home: home.path().to_path_buf(),
            cli: PathBuf::from("/usr/bin/codex"),
            version_timeout: VERSION_TIMEOUT,
            start_timeout: START_TIMEOUT,
            socket_ready_timeout: SOCKET_READY_TIMEOUT,
        };
        let reported = DaemonInfo {
            managed_codex_path: Some(PathBuf::from("/managed/codex")),
            ..DaemonInfo::default()
        };
        assert_eq!(
            managed_binary(&environment, Some(&reported)),
            Path::new("/managed/codex")
        );
        assert_eq!(managed_binary(&environment, None), Path::new("/usr/bin/codex"));

        let package = environment.package_binary();
        std::fs::create_dir_all(package.parent().expect("package parent")).expect("package dir");
        std::fs::write(&package, "").expect("package binary");
        assert_eq!(managed_binary(&environment, None), package);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_socket_is_absent_and_a_dangling_link_counts_as_gone() {
        let directory = tempfile::tempdir().expect("tempdir");
        let socket = directory.path().join("app-server-control.sock");
        assert_eq!(socket_identity(&socket).expect("missing is not an error"), None);

        std::os::unix::fs::symlink(directory.path().join("gone"), &socket).expect("dangling link");
        assert_eq!(socket_identity(&socket).expect("a dangling link is absent"), None);
    }

    /// A path that exists but is not a socket is a named failure: silently
    /// calling it absent would hide a misconfiguration.
    #[cfg(unix)]
    #[test]
    fn a_regular_file_at_the_control_path_is_a_named_failure() {
        let directory = tempfile::tempdir().expect("tempdir");
        let socket = directory.path().join("app-server-control.sock");
        std::fs::write(&socket, "").expect("regular file");
        let error = socket_identity(&socket).expect_err("not a socket");
        assert!(matches!(error, CodexError::DaemonSocket { .. }), "{error}");
    }

    /// The explicit opt-out is the decisive signal, on a TUI only.
    #[test]
    fn a_no_daemon_tui_is_an_embedded_client() {
        let daemon = Path::new("/tmp/codex-daemon.sock");
        let Some(Observation::Embedded(client)) =
            observe_client(42, "codex", &args("codex --no-daemon"), Some(daemon))
        else {
            panic!("--no-daemon is embedded even with a daemon running");
        };
        assert_eq!(client.pid, 42);
        assert!(client.evidence.contains("--no-daemon"), "{}", client.evidence);

        // A prompt is still the TUI.
        assert!(embedded(7, &args("codex --no-daemon fix the tests"), None));
        // Options before the flag do not hide it.
        assert!(embedded(7, &args("codex -m gpt-5 --no-daemon"), None));
    }

    /// One convenience predicate over [`observe_client`] for the cases whose
    /// outcome is "embedded, or not reported at all".
    fn embedded(pid: i32, arguments: &[String], daemon: Option<&Path>) -> bool {
        matches!(
            observe_client(pid, "codex", arguments, daemon),
            Some(Observation::Embedded(_))
        )
    }

    /// A non-TUI subcommand never starts an embedded TUI server, even when the
    /// flag is misapplied or the daemon is down.
    #[test]
    fn a_codex_subcommand_is_not_a_tui() {
        for line in [
            "codex app-server --listen unix:///tmp/x.sock",
            "codex exec do the thing",
            "codex doctor",
            "codex agents",
            "codex mcp list",
        ] {
            assert!(
                observe_client(7, "codex", &args(line), None).is_none(),
                "{line} is not a TUI"
            );
        }
    }

    /// With no daemon, a TUI has nowhere to attach and fell back to embedded —
    /// unless it names a remote server.
    #[test]
    fn a_tui_without_a_daemon_fell_back_to_embedded() {
        let Some(Observation::Embedded(client)) = observe_client(9, "codex", &args("codex"), None) else {
            panic!("no daemon means the TUI fell back to an embedded app-server");
        };
        assert_eq!(client.pid, 9);
        assert!(client.evidence.contains("embedded"), "{}", client.evidence);
        // A remote endpoint is not embedded: it names a server, even when the
        // endpoint is not this daemon (a ws listener cannot be compared from
        // the process table). It is an unreachable remote, not an embedded one.
        let Some(Observation::Unreachable(remote)) =
            observe_client(9, "codex", &args("codex --remote ws://host:1"), None)
        else {
            panic!("a remote client is not an embedded one");
        };
        assert_eq!(remote.endpoint, "ws://host:1");
        assert!(remote.detail.contains("no shared daemon"), "{}", remote.detail);
    }

    /// The transition the scan cannot settle: a daemon exists, and a TUI names
    /// no `--remote`. It may be attached (the supported configuration) or it may
    /// have started embedded before the daemon appeared and stayed there. That
    /// is **unverified**, not sharing-and-not-embedded: reporting nothing would
    /// be the silent assumption ADR-0090 decision 3 rejects.
    #[test]
    fn a_tui_with_a_daemon_is_unverified_not_assumed_sharing() {
        let daemon = Path::new("/tmp/codex-daemon.sock");
        let Some(Observation::Unverified(client)) = observe_client(9, "codex", &args("codex"), Some(daemon))
        else {
            panic!("a daemon-present TUI is recorded, not assumed to share");
        };
        assert_eq!(client.pid, 9);
        assert!(!embedded(9, &args("codex"), Some(daemon)));

        // `--no-daemon` stays decisive even with a daemon running. A `--remote`
        // that names another listener is an unreachable remote, never embedded.
        assert!(embedded(9, &args("codex --no-daemon"), Some(daemon)));
        let Some(Observation::Unreachable(remote)) =
            observe_client(9, "codex", &args("codex --remote ws://host:1"), Some(daemon))
        else {
            panic!("a remote naming another listener is not embedded");
        };
        assert!(
            remote.detail.contains("not the discovered daemon"),
            "{}",
            remote.detail
        );
    }

    /// An argv match names the daemon but is **not** proof of a live
    /// connection (a TUI started before the control symlink moved still names
    /// the path). It is unverified until [`prove_attachments`] settles it; a
    /// `--remote` that names another socket is an unreachable remote.
    #[test]
    fn a_remote_tui_naming_the_daemon_is_unverified_until_proven() {
        let daemon = Path::new("/tmp/codex-daemon-0/app-server-control.sock");
        let Some(Observation::Unverified(client)) = observe_client(
            11,
            "codex",
            &args("codex --remote unix:///tmp/codex-daemon-0/app-server-control.sock"),
            Some(daemon),
        ) else {
            panic!("an argv match is unverified, not proven attached");
        };
        assert_eq!(client.pid, 11);
        assert_eq!(
            client.endpoint.as_deref(),
            Some("unix:///tmp/codex-daemon-0/app-server-control.sock")
        );
        assert!(client.detail.contains("not proof"), "{}", client.detail);

        let Some(Observation::Unreachable(other)) = observe_client(
            11,
            "codex",
            &args("codex --remote unix:///tmp/other/app-server-control.sock"),
            Some(daemon),
        ) else {
            panic!("another server is not this daemon");
        };
        assert!(
            other.detail.contains("not the discovered daemon"),
            "{}",
            other.detail
        );

        assert!(
            matches!(
                observe_client(
                    11,
                    "codex",
                    &args("codex --remote=unix:///tmp/codex-daemon-0/app-server-control.sock"),
                    Some(daemon)
                ),
                Some(Observation::Unverified(_))
            ),
            "the --remote=<value> spelling names the same endpoint"
        );
    }

    /// The `ss -x -e` parser reads the fixed suffix from the right, so a path
    /// containing spaces and the unrelated rows do not confuse it.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_unix_peer_parser_reads_the_fixed_suffix() {
        let text = "\n\
             u_str ESTAB 0 0 /tmp/a path/daemon.sock 28465257 * 28465256 <-> ino:1077950 dev:0/71\n\
             u_str LISTEN 0 4096 /tmp/daemon.sock 28465255 * 0 <-> ino:1077950 dev:0/71\n\
             u_str ESTAB 0 0 * 5944 * 1765 -->\n";
        assert_eq!(parse_established_unix_peers(text), vec![(1_077_950, 28_465_256)]);
    }

    /// Replacement: a client connected to the **old** socket is not proven
    /// attached to the new one, even when argv still names the path.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_connection_to_the_old_socket_does_not_prove_the_new_daemon() {
        let client_sockets = [28_465_256];
        let old_peers = [(1_077_950, 28_465_256)];
        assert!(peer_matches(&client_sockets, &old_peers, 1_077_950));
        assert!(
            !peer_matches(&client_sockets, &old_peers, 1_077_951),
            "a re-pointed symlink has a different inode and must not match"
        );
    }

    /// End to end over `/proc` and `ss`, where both are available: the test
    /// process holds a live connection, and the proof upgrades it; pointing at
    /// a different listener does not.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_live_descriptor_connection_is_proven_and_a_replaced_one_is_not() {
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().expect("tempdir");
        let old = root.path().join("old.sock");
        let new = root.path().join("new.sock");
        let control = root.path().join("app-server-control.sock");
        let old_listener = UnixListener::bind(&old).expect("bind old");
        let _new_listener = UnixListener::bind(&new).expect("bind new");
        std::os::unix::fs::symlink(&old, &control).expect("symlink to old");
        // The TUI names the control path and connects through it.
        let _client = std::os::unix::net::UnixStream::connect(&control).expect("connect old");
        let _accepted = old_listener.accept().expect("accept old");

        let pid = std::process::id() as i32;
        let endpoint = format!("unix://{}", control.display());
        let mut clients = LocalClients {
            unverified: vec![UnverifiedClient {
                pid,
                binary: "codex".into(),
                endpoint: Some(endpoint.clone()),
                detail: "argv".into(),
                start_time: process_start_time(pid),
            }],
            ..LocalClients::default()
        };
        // A missing `ss` is the documented degraded path: nothing is promoted.
        if established_unix_peers().await.is_none() {
            eprintln!("skipping descriptor proof: `ss` is unavailable");
            return;
        }
        prove_attachments(&mut clients, &control).await;
        assert_eq!(clients.attached.len(), 1, "the live connection is proven");
        assert_eq!(clients.attached[0].pid, pid);

        // The operator re-points the control symlink at a new daemon. The TUI's
        // argv still names the same path, but its live socket belongs to the
        // old daemon, so it must not be reported attached to the new one.
        std::fs::remove_file(&control).expect("remove the old link");
        std::os::unix::fs::symlink(&new, &control).expect("symlink to new");
        let mut clients = LocalClients {
            unverified: vec![UnverifiedClient {
                pid,
                binary: "codex".into(),
                endpoint: Some(endpoint.clone()),
                detail: "argv".into(),
                start_time: process_start_time(pid),
            }],
            ..LocalClients::default()
        };
        prove_attachments(&mut clients, &control).await;
        assert!(
            clients.attached.is_empty(),
            "the old connection is not the replacement daemon"
        );
        assert_eq!(clients.unverified.len(), 1);

        // A reused PID: the connection is real, but the start time captured
        // with the argv belongs to the old process, so the descriptors must
        // not be trusted for this one.
        let mut clients = LocalClients {
            unverified: vec![UnverifiedClient {
                pid,
                binary: "codex".into(),
                endpoint: Some(endpoint),
                detail: "argv".into(),
                start_time: Some(0),
            }],
            ..LocalClients::default()
        };
        prove_attachments(&mut clients, &control).await;
        assert!(
            clients.attached.is_empty(),
            "a mismatched start time means the pid may be reused"
        );
        assert_eq!(clients.unverified.len(), 1);
    }

    /// The start-time pin reads a real process and refuses a missing one.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_process_start_time_pins_the_pid() {
        let own = process_start_time(std::process::id() as i32).expect("own start time");
        assert!(own > 0);
        assert_eq!(
            process_start_time(i32::MAX - 1),
            None,
            "a missing process has no start time"
        );
    }

    /// An unexpected `ss` table (the format is not ours, or the tool changed)
    /// proves nothing: every client stays unverified rather than guessed.
    #[cfg(target_os = "linux")]
    #[test]
    fn unexpected_ss_output_proves_nothing() {
        let peers = parse_established_unix_peers(
            "u_str ESTAB 0 0 not-the-expected-suffix\n\
             some other tool output entirely\n",
        );
        assert!(peers.is_empty(), "{peers:?}");
        assert!(!peer_matches(&[28_465_256], &peers, 1_077_950));
    }

    #[test]
    fn a_process_that_is_not_codex_is_never_reported() {
        assert!(observe_client(9, "node", &args("node server.js --no-daemon"), None).is_none());
        assert!(
            observe_client(
                9,
                "not-codex",
                &args("/opt/other/bin/not-codex --no-daemon"),
                None
            )
            .is_none()
        );
    }

    /// A relocated `CODEX_HOME` is honoured end to end: the socket is read from
    /// the variable, a **symlinked** socket is accepted, the CLI is told the
    /// same home, and the daemon's own binary (not the CLI on `PATH`) is what
    /// the probe will ask.
    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_honours_a_relocated_home_and_a_symlinked_socket() {
        use crate::codex::test_server::FakeCodex;

        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join("relocated-codex-home");
        let environment = test_environment(&home);
        let control = environment.socket();

        // The daemon serves a real unix socket somewhere outside the home; the
        // control path is a symlink to it, exactly like `/tmp/codex-daemon-0/<hash>`.
        std::fs::create_dir_all(control.parent().expect("control dir")).expect("control dir");
        let daemon_socket = root.path().join("daemon-0").join("app-server-control");
        std::fs::create_dir_all(daemon_socket.parent().expect("daemon dir")).expect("daemon dir");
        let _listener =
            std::os::unix::net::UnixListener::bind(&daemon_socket).expect("bind the daemon socket");
        std::os::unix::fs::symlink(&daemon_socket, &control).expect("symlink the control socket");

        let managed = root.path().join("managed-codex");
        let cli = FakeCodex::write(root.path(), &control, Some(&daemon_socket), None, &managed);
        let environment = Environment {
            cli: cli.binary.clone(),
            ..environment
        };

        let found = discover(&environment).await.expect("discovery");
        let daemon = found.daemon.daemon().expect("a running daemon");
        assert_eq!(daemon.socket, control);
        assert_eq!(
            daemon.managed_binary, managed,
            "the probe must get the binary the daemon runs"
        );
        assert_eq!(
            daemon
                .info
                .as_ref()
                .and_then(|info| info.app_server_version.as_deref()),
            Some("0.163.0")
        );
        assert!(
            found.socket.is_some(),
            "a symlinked socket has an identity to compare later"
        );
        let invocations = cli.invocations();
        assert_eq!(invocations.len(), 1, "{invocations:?}");
        assert!(
            invocations[0].starts_with(&format!("CODEX_HOME={}", home.display())),
            "the relocated home must reach the CLI: {invocations:?}"
        );
    }

    /// Absence is read from the filesystem alone: no CLI runs, so a
    /// `start_server = "never"` pass cannot even accidentally start anything.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_absent_daemon_is_a_state_and_runs_no_command() {
        use crate::codex::test_server::FakeCodex;

        let root = tempfile::tempdir().expect("tempdir");
        let environment = test_environment(&root.path().join("codex-home"));
        let cli = FakeCodex::write(
            root.path(),
            &environment.socket(),
            Some(&root.path().join("unused.sock")),
            None,
            &root.path().join("managed-codex"),
        );
        let environment = Environment {
            cli: cli.binary.clone(),
            ..environment
        };

        let found = discover(&environment).await.expect("discovery");
        let DaemonState::Absent { evidence } = &found.daemon else {
            panic!("expected an absent daemon, got {:?}", found.daemon);
        };
        assert!(evidence.contains("app-server-control.sock"), "{evidence}");
        assert!(found.socket.is_none());
        assert!(
            cli.invocations().is_empty(),
            "discovery must not run the CLI to learn a socket is missing"
        );
    }

    /// `daemon start` is Codex's own command: it runs once, wait for the socket
    /// it links, and a launcher that reports success without one fails by name.
    #[cfg(unix)]
    #[tokio::test]
    async fn start_waits_for_the_control_socket_and_names_a_launcher_that_leaves_none() {
        use crate::codex::test_server::FakeCodex;

        let root = tempfile::tempdir().expect("tempdir");
        let environment = test_environment(&root.path().join("codex-home"));
        std::fs::create_dir_all(environment.socket().parent().expect("control dir")).expect("control dir");
        let daemon_socket = root.path().join("daemon.sock");
        let _listener =
            std::os::unix::net::UnixListener::bind(&daemon_socket).expect("bind the daemon socket");
        let cli = FakeCodex::write(
            root.path(),
            &environment.socket(),
            Some(&daemon_socket),
            None,
            &root.path().join("managed-codex"),
        );
        let environment = Environment {
            cli: cli.binary.clone(),
            ..environment
        };

        start(&environment).await.expect("start links the socket");
        assert_eq!(cli.starts(), 1);
        assert!(
            socket_identity(&environment.socket())
                .expect("identity")
                .is_some()
        );

        // A different home whose launcher leaves nothing behind: the wait is
        // bounded and the failure names the missing socket.
        let empty_home = root.path().join("empty-home");
        let empty_socket = empty_home.join(CONTROL_DIR).join(CONTROL_SOCKET);
        let second_cli = root.path().join("cli-2");
        std::fs::create_dir_all(&second_cli).expect("second cli dir");
        let cli = FakeCodex::write(
            &second_cli,
            &empty_socket,
            None,
            None,
            &root.path().join("managed-codex"),
        );
        let empty = Environment {
            home: empty_home,
            cli: cli.binary.clone(),
            socket_ready_timeout: Duration::from_millis(200),
            ..environment
        };
        let error = start(&empty).await.expect_err("no socket is a named failure");
        assert!(error.to_string().contains("did not appear"), "{error}");
    }

    /// `daemon start` is idempotent: a daemon that appeared between the
    /// snapshot and the command (a replacement racing a failed dial) makes it a
    /// no-op, so no identity changes. That must return the serving daemon, not
    /// a timeout for a start that had nothing to do.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_idempotent_start_returns_the_daemon_already_serving() {
        use crate::codex::test_server::FakeCodex;

        let root = tempfile::tempdir().expect("tempdir");
        let environment = test_environment(&root.path().join("codex-home"));
        let control = environment.socket();
        std::fs::create_dir_all(control.parent().expect("control dir")).expect("control dir");
        // A daemon already serves, and the path already names it: the launcher's
        // `start` is a no-op (the fake CLI written without a link target).
        let daemon_socket = root.path().join("daemon.sock");
        std::fs::create_dir_all(daemon_socket.parent().expect("dir")).expect("dir");
        let _listener =
            std::os::unix::net::UnixListener::bind(&daemon_socket).expect("bind the daemon socket");
        std::os::unix::fs::symlink(&daemon_socket, &control).expect("symlink the control socket");
        let managed = root.path().join("managed-codex");
        let cli = FakeCodex::write(root.path(), &control, None, None, &managed);
        let environment = Environment {
            cli: cli.binary.clone(),
            socket_ready_timeout: Duration::from_millis(200),
            ..environment
        };

        let started = start(&environment)
            .await
            .expect("an idempotent start returns the daemon already serving");
        assert_eq!(started.daemon.socket, control);
        assert_eq!(started.daemon.managed_binary, managed);
        assert_eq!(
            socket_identity(&control).expect("identity"),
            Some(started.identity),
            "the returned identity is the one the same pass read"
        );
        assert_eq!(cli.starts(), 1, "the launcher was asked exactly once");
    }

    #[cfg(unix)]
    fn test_environment(home: &Path) -> Environment {
        Environment {
            home: home.to_path_buf(),
            cli: PathBuf::from("codex"),
            version_timeout: Duration::from_secs(5),
            start_timeout: Duration::from_secs(5),
            socket_ready_timeout: Duration::from_millis(500),
        }
    }
}
