//! The live Codex suites' child processes (ticket #953).
//!
//! **tier-1** starts a real pinned `codex app-server --listen unix://<path>`
//! in an isolated `CODEX_HOME`. The path is deliberately short: Windows binds
//! an AF_UNIX socket at the published path itself and enforces the 108-byte
//! `sun_path` limit (terminator included), so a temp root inherited from a
//! deep macOS `$TMPDIR` would make the test fail for a reason unrelated to the
//! adapter.
//!
//! On Windows the socket must also live in a directory Codex itself creates:
//! `prepare_private_socket_directory` accepts a directory only when its DACL
//! is protected, owner-only and one inheritable ACE. A `tempfile` root under
//! `%TEMP%` inherits SYSTEM/Administrators ACEs, so the child exits
//! `PermissionDenied: socket directory is not private to the current user`.
//! The listener therefore lives at the discovered control-socket path,
//! `<home>/app-server-control/app-server-control.sock`, with
//! `app-server-control` **absent before the spawn** — the same shape as Codex's
//! own Windows tests — and Codex creates the private directory. Discovery and
//! the lifecycle both look there, so the tier drives the real attach path.
//!
//! The server's whole world is the temp tree — the store, config and work
//! directory — so it can never see the machine's own Codex state.
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

use crate::codex::discovery::{CONTROL_DIR, CONTROL_SOCKET, Environment};
use crate::codex::transport::Endpoint;

/// How long app-server gets to publish its control socket.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a killed child gets to be reaped.
const REAP_TIMEOUT: Duration = Duration::from_secs(10);
/// How often startup re-reads the socket path.
const STARTUP_STEP: Duration = Duration::from_millis(25);
/// How long a `codex app-server daemon …` management command gets before it is
/// treated as wedged. Every call is bounded so a hung CLI fails the job instead
/// of holding the 20-minute budget.
#[cfg(unix)]
const CLI_TIMEOUT: Duration = Duration::from_secs(30);
/// `daemon bootstrap` installs the managed package and starts it; the CLI is a
/// standalone release binary, so this is the only way to get a daemon package.
#[cfg(unix)]
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(120);
/// Windows can keep a directory handle open for a moment after a child exits;
/// a temp-tree removal is retried for this long, then reported with the OS
/// error rather than the assertion being weakened.
const TREE_REMOVE_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause between temp-tree removal attempts.
const TREE_REMOVE_STEP: Duration = Duration::from_millis(50);

/// The isolated `CODEX_HOME`'s directory name under the temp root, matching
/// [`isolated_home`]. One byte keeps the Windows listener path well inside the
/// 108-byte `sun_path` budget on a hosted `%TEMP%`.
const LIVE_HOME_DIR: &str = "h";

/// The tier-2 daemon's isolated `CODEX_HOME` directory name under the temp
/// root, matching [`LiveDaemon::new`]. It is longer than [`LIVE_HOME_DIR`], so
/// the socket budget check uses it as the worst case: a root that fits here
/// serves either tier.
#[cfg(unix)]
const LIVE_DAEMON_HOME_DIR: &str = "home";

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

/// The home directory name the budget is measured against: the longer of the
/// two tiers' names, so one check covers both.
fn home_dir_for_budget() -> &'static str {
    #[cfg(unix)]
    {
        LIVE_DAEMON_HOME_DIR
    }
    #[cfg(not(unix))]
    {
        LIVE_HOME_DIR
    }
}

/// Whether a temp root's listener path — including the private directory Codex
/// creates — is inside the platform budget. Measured against the worst-case
/// (`home`, the tier-2 name), so a near-limit root cannot pass this check and
/// then fail the daemon's own `ENAMETOOLONG` dial.
fn socket_path_fits(root: &Path) -> bool {
    root.join(home_dir_for_budget())
        .join(CONTROL_DIR)
        .join(CONTROL_SOCKET)
        .as_os_str()
        .len()
        < SOCKET_PATH_LIMIT
}

/// The Windows worst-case shape this suite relies on: `%TEMP%` on a hosted
/// runner plus `cola-cxXXXXXX/h/app-server-control/app-server-control.sock`.
/// Kept as a test below so a future prefix or directory name cannot silently
/// overflow the 108-byte limit on a platform this host cannot run.
#[cfg(test)]
fn windows_socket_shape(temp: &str) -> String {
    format!(r"{temp}\cola-cxAbCdEf\{LIVE_HOME_DIR}\{CONTROL_DIR}\{CONTROL_SOCKET}")
}

/// One isolated `CODEX_HOME` plus the provider base URL it points at.
///
/// The directory is created pure: no `auth.json`, no ambient config. The suite
/// writes only the scripted provider entry, so a model call cannot reach the
/// network.
pub fn isolated_home(root: &Path, provider_base_url: &str) -> PathBuf {
    let home = root.join(LIVE_HOME_DIR);
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
    cli: PathBuf,
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
        // The discovered control-socket path, so the lifecycle's discovery and
        // dial look exactly where this listener published. Deliberately do not
        // create the parent: Codex creates it with the private Windows DACL
        // (see the module docs).
        let socket = home.join(CONTROL_DIR).join(CONTROL_SOCKET);
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
            cli: binary.to_path_buf(),
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

    /// The same isolated home and CLI the listener runs with, so the live tier
    /// drives the production discovery and attach path rather than a bespoke
    /// endpoint.
    pub fn environment(&self) -> Environment {
        Environment {
            home: self.home.clone(),
            cli: self.cli.clone(),
            version_timeout: Duration::from_secs(15),
            start_timeout: Duration::from_secs(60),
            socket_ready_timeout: Duration::from_secs(20),
        }
    }

    pub fn work(&self) -> &Path {
        &self.work
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The child process id, for the cleanup marker.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(tokio::process::Child::id)
    }

    /// Every path this listener must have removed by the end of the test: the
    /// published path and, on Unix, the runtime socket and lock it links to.
    pub fn runtime_paths(&self) -> Vec<PathBuf> {
        let paths = vec![self.socket.clone()];
        #[cfg(unix)]
        let paths = {
            let mut paths = paths;
            if let Ok(target) = std::fs::read_link(&self.socket) {
                paths.push(target.clone());
                paths.push(target.with_extension("lock"));
            }
            paths
        };
        paths
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix(self.socket.clone())
    }

    /// Kill the server, reap it, and remove the temp home/work explicitly; the
    /// test then proves the published socket path and both proxy children are
    /// gone too.
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
        if tokio::time::timeout(REAP_TIMEOUT, child.wait()).await.is_err() {
            // SIGKILL did not land within the bound; the process is still
            // alive, and the assertion below names it rather than hanging.
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
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
        // The reaped process is the proof no listener remains. Remove the
        // temp tree here rather than relying on `TempDir`'s drop: the test's
        // cleanup assertions are about what shutdown proved, not what a
        // best-effort destructor happens to do later. Windows can hold a
        // handle briefly, so the removal is retried.
        remove_tree_with_retry(&self.work).await;
        remove_tree_with_retry(&self.home).await;
    }
}

impl Drop for LiveAppServer {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// Remove a temp tree, retrying transient Windows sharing violations, then
/// assert it is gone with the last OS error in the message.
async fn remove_tree_with_retry(path: &Path) {
    let deadline = Instant::now() + TREE_REMOVE_TIMEOUT;
    let last_error = loop {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                if Instant::now() >= deadline {
                    break error;
                }
                sleep(TREE_REMOVE_STEP).await;
            }
        }
    };
    panic!(
        "the live Codex temp path {} was left behind after {TREE_REMOVE_TIMEOUT:?}: {last_error}",
        path.display()
    );
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
        let home = root.path().join(LIVE_DAEMON_HOME_DIR);
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

    /// Every process the daemon started under this isolated home (the daemon,
    /// its `pid-update-loop` helper, and anything else it forked).
    pub fn process_ids(&self) -> Vec<u32> {
        daemon_processes_under(&self.home)
            .into_iter()
            .map(|pid| pid.as_u32())
            .collect()
    }

    /// Every path that must be gone after [`Self::shutdown`]: the published
    /// socket and every runtime socket/lock this daemon added.
    pub fn runtime_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.environment().socket()];
        if let Ok(target) = std::fs::read_link(self.environment().socket()) {
            paths.push(target.clone());
            paths.push(target.with_extension("lock"));
        }
        for (directory, names) in runtime_snapshot() {
            let baseline = self.runtime_before.get(&directory);
            for name in names {
                if baseline.is_none_or(|baseline| !baseline.contains(&name)) {
                    paths.push(directory.join(name));
                }
            }
        }
        paths
    }

    /// Stop the daemon. Bounded: a wedged management CLI fails the test
    /// instead of holding the job's whole 20-minute budget.
    pub async fn stop(&self) {
        let mut command = tokio::process::Command::new(&self.cli);
        command
            .args(["app-server", "daemon", "stop"])
            .env("CODEX_HOME", &self.home)
            .kill_on_drop(true);
        let output = tokio::time::timeout(CLI_TIMEOUT, command.output())
            .await
            .unwrap_or_else(|_| {
                panic!("`codex app-server daemon stop` did not answer within {CLI_TIMEOUT:?}")
            })
            .expect("run `codex app-server daemon stop`");
        assert!(
            output.status.success(),
            "`codex app-server daemon stop` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The managed package binary `daemon bootstrap`/`daemon start` installs.
    pub fn package_binary(&self) -> PathBuf {
        self.home.join(crate::codex::discovery::MANAGED_BINARY)
    }

    /// This daemon's own runtime directory, from the published symlink.
    fn runtime_dir(&self) -> Option<PathBuf> {
        std::fs::read_link(self.environment().socket())
            .ok()
            .and_then(|target| target.parent().map(Path::to_path_buf))
            .map(|directory| std::fs::canonicalize(&directory).unwrap_or(directory))
            .filter(|directory| {
                let text = directory.to_string_lossy();
                text.starts_with("/tmp/codex-daemon-") || text.starts_with("/private/tmp/codex-daemon-")
            })
    }

    /// Kill every process the daemon left under this isolated home (the
    /// `pid-update-loop` helper and plugin-fetch children included).
    async fn kill_lingering(&self) {
        let deadline = Instant::now() + REAP_TIMEOUT;
        loop {
            let lingering = daemon_processes_under(&self.home);
            if lingering.is_empty() {
                return;
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
    }

    /// Stop the daemon (if any), reap its helpers, remove only the runtime
    /// entries this test added, and assert the published socket is gone.
    async fn stop_and_remove_runtime(&self) {
        let socket = self.environment().socket();
        let runtime_dir = self.runtime_dir();
        if socket.exists() {
            self.stop().await;
        }
        self.kill_lingering().await;
        if let Some(directory) = runtime_dir.as_deref() {
            cleanup_runtime(directory, self.runtime_before.get(directory));
        }
        let _ = std::fs::remove_file(&socket);
        assert!(
            !socket.exists(),
            "the managed daemon's control socket {} was left behind",
            socket.display()
        );
    }

    /// Install the managed package from the standalone CLI and start it once,
    /// then stop it again. The GitHub release asset is a standalone `codex`,
    /// not a packaged daemon: `daemon start` alone answers "this CLI has no
    /// complete local package" (macOS) or refuses a `/tmp` helper install
    /// (Linux). `daemon bootstrap` is the supported entry point, and it leaves
    /// `$CODEX_HOME/packages/app-server-daemon/current/bin/codex` behind for
    /// the real `never`/`auto` flow.
    pub async fn bootstrap(&self) {
        let mut command = tokio::process::Command::new(&self.cli);
        command
            .args(["app-server", "daemon", "bootstrap"])
            .env("CODEX_HOME", &self.home)
            .kill_on_drop(true);
        let output = tokio::time::timeout(BOOTSTRAP_TIMEOUT, command.output())
            .await
            .unwrap_or_else(|_| {
                panic!("`codex app-server daemon bootstrap` did not answer within {BOOTSTRAP_TIMEOUT:?}")
            })
            .expect("run `codex app-server daemon bootstrap`");
        assert!(
            output.status.success(),
            "`codex app-server daemon bootstrap` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("bootstrap printed no JSON: {:?}", output.stdout));
        assert_eq!(
            report["status"], "bootstrapped",
            "unexpected bootstrap report: {report}"
        );
        let package = self.package_binary();
        assert!(
            package.is_file(),
            "bootstrap did not install the managed package at {}",
            package.display()
        );

        // The bootstrap daemon is setup evidence only: stop it before `never`
        // proves no daemon remains, and keep its package for `auto`.
        self.stop_and_remove_runtime().await;
        assert!(
            package.is_file(),
            "stopping the bootstrap daemon removed the managed package at {}",
            package.display()
        );
        assert!(
            !self.environment().socket().exists(),
            "the bootstrap daemon left a control socket at {}",
            self.environment().socket().display()
        );
    }

    /// Stop the daemon and prove every process under this isolated home is
    /// gone, then remove the temp home explicitly.
    pub async fn shutdown(&self) {
        self.stop_and_remove_runtime().await;
        self.stopped.set(true);
        remove_tree_with_retry(&self.home).await;
    }
}

#[cfg(unix)]
impl Drop for LiveDaemon {
    fn drop(&mut self) {
        // A failed test must not leave the daemon running. Drop cannot await,
        // so the test's own `shutdown` is the asserted path and this is only
        // the backstop. After `shutdown` ran there is nothing to stop, and a
        // second call could spawn a helper under the runtime dir after the
        // test already proved it clean.
        if self.stopped.get() {
            return;
        }
        // Drop cannot await, so bound the synchronous child with a wait loop;
        // a wedged `daemon stop` is killed rather than hanging the test binary
        // at exit (and with it the job).
        let Ok(mut child) = std::process::Command::new(&self.cli)
            .args(["app-server", "daemon", "stop"])
            .env("CODEX_HOME", &self.home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let deadline = Instant::now() + CLI_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(STARTUP_STEP),
            }
        }
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
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let names = std::fs::read_dir(&path)
            .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
            .unwrap_or_default();
        snapshot.insert(path, names);
    }
    snapshot
}

/// Remove runtime entries in `directory` that were not in the pre-test
/// snapshot, then drop the directory if it is now empty.
///
/// **Assumption**: the test holds the live suite lock and runs in isolation, so
/// any entry absent from the snapshot was created by this test's daemon. Pre-existing entries (a developer's daemon, or a prior
/// crashed run) are never touched. The caller passes the one directory the
/// daemon's own published socket named; other `codex-daemon-*` directories are
/// not scanned here.
#[cfg(unix)]
fn cleanup_runtime(directory: &Path, before: Option<&BTreeSet<OsString>>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if before.is_none_or(|before| !before.contains(&name)) {
            let _ = std::fs::remove_file(directory.join(name));
        }
    }
    let _ = std::fs::remove_dir(directory);
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

/// The spellings of `home` a process path may use: the path as given and its
/// canonical form. On macOS `/tmp` is a symlink to `/private/tmp`, so sysinfo's
/// executable path (the physical spelling) does not prefix-match a home under
/// `/tmp` unless both sides are resolved.
#[cfg(unix)]
fn home_spellings(home: &Path) -> Vec<PathBuf> {
    let mut spellings = vec![home.to_path_buf()];
    if let Ok(canonical) = std::fs::canonicalize(home)
        && canonical != home
    {
        spellings.push(canonical);
    }
    spellings
}

/// Whether `path` lives under one of `home`'s spellings. The path is resolved
/// where it still exists (a live executable), falling back to its raw spelling;
/// both are compared so a `/tmp` home and a `/private/tmp` executable still
/// match.
#[cfg(unix)]
fn path_is_under(path: &Path, spellings: &[PathBuf]) -> bool {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    spellings
        .iter()
        .any(|home| resolved.starts_with(home) || path.starts_with(home))
}

/// Every process whose executable or argv lives under `home` — the isolated
/// tree this suite created. Nothing outside that path is ever matched. The
/// executable is compared in both its raw and canonical spellings (macOS
/// resolves `/tmp` to `/private/tmp`), and argv stays a fallback for a process
/// whose executable the scan cannot read.
#[cfg(unix)]
fn daemon_processes_under(home: &Path) -> Vec<sysinfo::Pid> {
    let spellings = home_spellings(home);
    let system = system();
    system
        .processes()
        .iter()
        .filter(|(_, process)| {
            process.exe().is_some_and(|exe| path_is_under(exe, &spellings))
                || process.cmd().iter().any(|arg| {
                    let arg = arg.to_string_lossy();
                    spellings
                        .iter()
                        .any(|home| arg.contains(&home.to_string_lossy().to_string()))
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
    let mut command = tokio::process::Command::new(daemon.cli());
    command
        .args(["app-server", "daemon", "version"])
        .env("CODEX_HOME", daemon.home())
        .kill_on_drop(true);
    let output = tokio::time::timeout(CLI_TIMEOUT, command.output())
        .await
        .unwrap_or_else(|_| panic!("`codex app-server daemon version` did not answer within {CLI_TIMEOUT:?}"))
        .expect("run `codex app-server daemon version`");
    assert!(
        output.status.success(),
        "`codex app-server daemon version` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("daemon version is JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// macOS resolves `/tmp` to `/private/tmp`, so a process's executable path
    /// and the suite's unresolved temp home can spell the same tree
    /// differently. Both spellings of the home must match, and a sibling that
    /// merely shares the prefix must not.
    #[cfg(unix)]
    #[test]
    fn a_canonical_home_matches_both_spellings_of_a_process_path() {
        let root = tempfile::tempdir().expect("temp root");
        let real = root.path().join("real");
        std::fs::create_dir(&real).expect("real home");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlinked home");

        let spellings = home_spellings(&link);
        let canonical_real = std::fs::canonicalize(&real).expect("canonical real home");
        assert!(spellings.iter().any(|home| home == &link));
        assert!(spellings.iter().any(|home| home == &canonical_real));

        assert!(path_is_under(&link.join("bin/codex"), &spellings));
        assert!(path_is_under(&real.join("bin/codex"), &spellings));
        assert!(!path_is_under(&root.path().join("other/codex"), &spellings));
        assert!(!path_is_under(&root.path().join("realer/codex"), &spellings));
    }

    /// The Windows listener path this suite hands the child must fit the
    /// 108-byte `sun_path` field on a hosted runner. The literal is checked on
    /// every platform because this host cannot run the Windows leg, and a
    /// future prefix or directory name must not silently overflow it.
    #[test]
    fn the_windows_socket_shape_fits_the_hosted_runner_budget() {
        for temp in [
            r"C:\Users\runneradmin\AppData\Local\Temp",
            r"C:\Users\RUNNER~1\AppData\Local\Temp",
        ] {
            let shape = windows_socket_shape(temp);
            // 98 bytes with the one-byte home, leaving margin under the
            // 108-byte `sun_path` limit (terminator included).
            assert!(
                shape.len() < 100,
                "the Windows listener path is {} bytes (want < 100 for margin under the \
                 108-byte limit): {shape}",
                shape.len()
            );
        }
    }
}
