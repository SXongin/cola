//! The live Codex suites' child processes (ticket #953).
//!
//! **tier-1** starts a real pinned `codex app-server --listen unix://<path>`
//! in an isolated `CODEX_HOME`. The path is deliberately short: Windows binds
//! an AF_UNIX socket at the published path itself and enforces the 108-byte
//! `sun_path` limit (terminator included), so a temp root inherited from a
//! deep macOS `$TMPDIR` would make the test fail for a reason unrelated to the
//! adapter. The server's whole world is the temp tree — the store, config and
//! work directory — so it can never see the machine's own Codex state.
//!
//! **tier-2** starts the real managed daemon through Codex's own
//! `app-server daemon start`, using the same isolated-home guarantee.
//!
//! Both types own their children: dropping the child is a backstop, and the
//! tests call [`LiveAppServer::shutdown`] and [`LiveDaemon::shutdown`] so they
//! can assert the process, the control socket and the temp home are gone.

#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::time::sleep;

#[cfg(unix)]
use crate::codex::discovery::Environment;
use crate::codex::transport::Endpoint;

/// How long app-server gets to publish its control socket.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a killed child gets to be reaped.
const REAP_TIMEOUT: Duration = Duration::from_secs(10);
/// How often startup re-reads the socket path.
const STARTUP_STEP: Duration = Duration::from_millis(25);

/// The longest AF_UNIX path the temp root may produce. Unix `sun_path` is 104
/// bytes on macOS and 108 on Linux; Windows uses the Windows limit from
/// discovery. A one-byte margin keeps the terminating NUL inside the field.
#[cfg(windows)]
const SOCKET_PATH_LIMIT: usize = crate::codex::discovery::WINDOWS_SOCKET_PATH_LIMIT;
#[cfg(unix)]
const SOCKET_PATH_LIMIT: usize = 104;

/// A temp root short enough for the socket path the suite puts under it.
///
/// `tempfile::tempdir()` roots under `$TMPDIR`, which on macOS is routinely
/// long enough to overflow `sun_path` before the suite adds anything. Try the
/// default first (so an exec-restricted `/tmp` is not forced), then the short
/// Unix bases, and keep the first root whose socket path fits.
pub fn short_tempdir(prefix: &str) -> tempfile::TempDir {
    #[cfg(unix)]
    let bases = vec![
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
    ];
    #[cfg(not(unix))]
    let bases = vec![std::env::temp_dir()];
    for base in bases {
        let Ok(root) = tempfile::Builder::new().prefix(prefix).tempdir_in(&base) else {
            continue;
        };
        if socket_path_fits(root.path()) {
            return root;
        }
    }
    panic!(
        "no temp base keeps a codex live socket inside the {SOCKET_PATH_LIMIT}-byte `sun_path` \
         limit; set TMPDIR to a short directory"
    );
}

/// Whether a temp root's own listener path is inside the platform budget.
fn socket_path_fits(root: &Path) -> bool {
    root.join("app-server.sock").as_os_str().len() < SOCKET_PATH_LIMIT
}

/// One isolated `CODEX_HOME` plus the provider base URL it points at.
///
/// The directory is created pure: no `auth.json`, no ambient config. The suite
/// writes only the scripted provider entry, so a model call cannot reach the
/// network.
pub fn isolated_home(root: &Path, provider_base_url: &str) -> PathBuf {
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&home).expect("create the isolated CODEX_HOME");
    std::fs::create_dir_all(&work).expect("create the live work directory");
    let config = format!(
        "model = \"cola-live-scripted\"\n\
         model_provider = \"cola-live\"\n\
         approval_policy = \"on-request\"\n\
         sandbox_mode = \"read-only\"\n\
         \n\
         [model_providers.cola-live]\n\
         name = \"cola live scripted provider\"\n\
         base_url = \"{provider_base_url}/v1\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = false\n"
    );
    std::fs::write(home.join("config.toml"), config).expect("write the isolated config.toml");
    home
}

/// A real `codex app-server --listen unix://<path>` in an isolated home.
pub struct LiveAppServer {
    child: Option<tokio::process::Child>,
    _root: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    socket: PathBuf,
    log: PathBuf,
}

impl LiveAppServer {
    /// Start the pinned binary against a scripted provider.
    pub async fn start(binary: &Path, provider_base_url: &str) -> Self {
        let root = short_tempdir("cola-cx");
        let home = isolated_home(root.path(), provider_base_url);
        let work = root.path().join("work");
        let socket = root.path().join("app-server.sock");
        let log = root.path().join("app-server.log");
        let stderr = std::fs::File::create(&log).expect("create the app-server log");
        let stdout = std::fs::File::create(root.path().join("app-server.out"))
            .expect("create the app-server stdout log");

        let child = tokio::process::Command::new(binary)
            .arg("app-server")
            .arg("--listen")
            .arg(format!("unix://{}", socket.display()))
            .env("CODEX_HOME", &home)
            .current_dir(&work)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {} app-server: {error}", binary.display()));

        let mut server = Self {
            child: Some(child),
            _root: root,
            home,
            work,
            socket,
            log,
        };
        server.wait_until_listening().await;
        server
    }

    /// Block until the control socket is published, or fail with the child's
    /// own log. The server creates the path only after it is ready to accept,
    /// so existence is the readiness edge.
    async fn wait_until_listening(&mut self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Ok(Some(_)) = crate::codex::discovery::socket_identity(&self.socket) {
                return;
            }
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                panic!(
                    "codex app-server exited before listening ({status}):\n{}",
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            assert!(
                Instant::now() < deadline,
                "codex app-server did not publish {} within {STARTUP_TIMEOUT:?}:\n{}",
                self.socket.display(),
                std::fs::read_to_string(&self.log).unwrap_or_default()
            );
            sleep(STARTUP_STEP).await;
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn work(&self) -> &Path {
        &self.work
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix(self.socket.clone())
    }

    /// Kill the server and reap it; the test then proves the temp home, the
    /// published socket path and both proxy children are gone. The temp home
    /// is removed when `self` drops.
    pub async fn shutdown(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // app-server publishes the listener as a symlink into Codex's own
        // short runtime directory (`/tmp/codex-daemon-<uid>/<hash>`), which is
        // OUTSIDE the temp home. Remember the target so shutdown can remove
        // this suite's socket file; only a path under that known directory is
        // touched, never another daemon's.
        #[cfg(unix)]
        let socket_target = std::fs::read_link(&self.socket).ok().filter(|target| {
            let text = target.to_string_lossy();
            text.starts_with("/tmp/codex-daemon-") || text.starts_with("/private/tmp/codex-daemon-")
        });
        let pid = child.id();
        let _ = child.start_kill();
        let _ = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await;
        if let Some(pid) = pid {
            assert!(
                !process_alive(pid),
                "the live app-server process {pid} outlived shutdown"
            );
        }
        #[cfg(unix)]
        if let Some(target) = socket_target {
            // The listener's own lock file sits beside the socket; both are
            // this suite's, and both would otherwise outlive the temp home.
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::remove_file(target.with_extension("lock"));
            if let Some(parent) = target.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
        // The reaped process is the proof no listener remains; a socket file
        // can outlive SIGKILL, and the temp home (which owns the published
        // path) is removed when `self` drops. The test asserts that path is
        // gone after the drop.
    }
}

impl Drop for LiveAppServer {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// A real managed daemon started through `codex app-server daemon start` under
/// an isolated `CODEX_HOME`.
#[cfg(unix)]
pub struct LiveDaemon {
    _root: tempfile::TempDir,
    home: PathBuf,
    cli: PathBuf,
    /// The per-user runtime directories' entries before this daemon started.
    /// Codex's startup creates more than the published control symlink (a
    /// helper's socket and lock files), none of it inside `CODEX_HOME`;
    /// `shutdown` removes only entries this daemon added.
    runtime_before: BTreeMap<PathBuf, BTreeSet<OsString>>,
    /// Set once `shutdown` ran, so `Drop` does not run `daemon stop` a second
    /// time (which can spawn a fresh helper under the runtime directory after
    /// the test already proved the tree clean).
    stopped: std::cell::Cell<bool>,
}

#[cfg(unix)]
impl LiveDaemon {
    pub fn new(cli: PathBuf) -> Self {
        let root = short_tempdir("cola-t2");
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).expect("create the isolated CODEX_HOME");
        Self {
            _root: root,
            home,
            cli,
            runtime_before: runtime_snapshot(),
            stopped: std::cell::Cell::new(false),
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn cli(&self) -> &Path {
        &self.cli
    }

    pub fn environment(&self) -> Environment {
        Environment {
            home: self.home.clone(),
            cli: self.cli.clone(),
            version_timeout: Duration::from_secs(15),
            start_timeout: Duration::from_secs(60),
            socket_ready_timeout: Duration::from_secs(20),
        }
    }

    /// Whether a daemon process this suite started is still alive. The
    /// management CLI reports a pid only while it is serving, so absence here
    /// is "no daemon", and a leftover is a cleanup failure.
    pub async fn stop(&self) {
        let output = tokio::process::Command::new(&self.cli)
            .args(["app-server", "daemon", "stop"])
            .env("CODEX_HOME", &self.home)
            .output()
            .await
            .expect("run `codex app-server daemon stop`");
        assert!(
            output.status.success(),
            "`codex app-server daemon stop` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Stop the daemon and prove every process under this isolated home is
    /// gone. `daemon stop` reaps the managed daemon but Codex's
    /// `app-server daemon pid-update-loop` helper is detached; this suite
    /// started it, so it kills anything whose executable or argv is under the
    /// temp home (and nothing else).
    pub async fn shutdown(&self) {
        let socket = self.environment().socket();
        if socket.exists() {
            self.stop().await;
            self.stopped.set(true);
        }
        let deadline = Instant::now() + REAP_TIMEOUT;
        loop {
            let lingering = daemon_processes_under(&self.home);
            if lingering.is_empty() {
                break;
            }
            for pid in lingering {
                if let Some(process) = system().process(pid) {
                    let _ = process.kill();
                }
            }
            assert!(
                Instant::now() < deadline,
                "processes under the live CODEX_HOME {} outlived `daemon stop`",
                self.home.display()
            );
            sleep(STARTUP_STEP).await;
        }
        // The daemon's helper sockets live beside the published one; remove
        // every runtime entry this daemon added (the test holds the live lock,
        // so no sibling test's entries are new since `new`).
        cleanup_runtime(&self.runtime_before);
        let _ = std::fs::remove_file(&socket);
        assert!(
            !socket.exists(),
            "the managed daemon's control socket {} was left behind",
            socket.display()
        );
    }
}

#[cfg(unix)]
impl Drop for LiveDaemon {
    fn drop(&mut self) {
        // A failed test must not leave the daemon running: `daemon stop` is a
        // synchronous child and bounded by its own startup timeout, but Drop
        // cannot await, so the test's own `shutdown` is the asserted path and
        // this is only the backstop. After `shutdown` ran there is nothing to
        // stop, and a second call could spawn a helper under the runtime dir
        // after the test already proved it clean.
        if self.stopped.get() {
            return;
        }
        let _ = std::process::Command::new(&self.cli)
            .args(["app-server", "daemon", "stop"])
            .env("CODEX_HOME", &self.home)
            .output();
    }
}

/// Every `codex-daemon-*` runtime directory under `/tmp` and the names it
/// holds. Codex keeps its AF_UNIX sockets outside `CODEX_HOME`; the snapshot is
/// what lets `shutdown` remove only the entries a test's daemon added.
#[cfg(unix)]
fn runtime_snapshot() -> BTreeMap<PathBuf, BTreeSet<OsString>> {
    let mut snapshot = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return snapshot;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("codex-daemon-") {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let names = std::fs::read_dir(&path)
            .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
            .unwrap_or_default();
        snapshot.insert(path, names);
    }
    snapshot
}

/// Remove runtime entries that were not in the pre-test snapshot, then drop a
/// runtime directory the test created. A user daemon's pre-existing entries are
/// left untouched.
#[cfg(unix)]
fn cleanup_runtime(before: &BTreeMap<PathBuf, BTreeSet<OsString>>) {
    for (directory, names) in runtime_snapshot() {
        let baseline = before.get(&directory);
        for name in names {
            if baseline.is_none_or(|baseline| !baseline.contains(&name)) {
                let _ = std::fs::remove_file(directory.join(&name));
            }
        }
        let _ = std::fs::remove_dir(&directory);
    }
}

/// A fresh process table with command lines and executables, for the
/// isolated-home cleanup scans.
#[cfg(unix)]
fn system() -> sysinfo::System {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );
    system
}

/// Every process whose executable or argv lives under `home` — the isolated
/// tree this suite created. Nothing outside that path is ever matched.
#[cfg(unix)]
fn daemon_processes_under(home: &Path) -> Vec<sysinfo::Pid> {
    let system = system();
    system
        .processes()
        .iter()
        .filter(|(_, process)| {
            process.exe().is_some_and(|exe| exe.starts_with(home))
                || process.cmd().iter().any(|arg| {
                    arg.to_string_lossy()
                        .contains(&home.to_string_lossy().to_string())
                })
        })
        .map(|(pid, _)| *pid)
        .collect()
}

/// Whether `pid` names a live process. Uses sysinfo rather than `kill`, so it
/// is portable to the Windows tier-1 leg.
pub fn process_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some()
}

/// The `daemon version` JSON report, so tier-2 can assert the management CLI's
/// real answer rather than only the fields discovery decoded.
#[cfg(unix)]
pub async fn daemon_version(daemon: &LiveDaemon) -> serde_json::Value {
    let output = tokio::process::Command::new(daemon.cli())
        .args(["app-server", "daemon", "version"])
        .env("CODEX_HOME", daemon.home())
        .output()
        .await
        .expect("run `codex app-server daemon version`");
    assert!(
        output.status.success(),
        "`codex app-server daemon version` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("daemon version is JSON")
}
