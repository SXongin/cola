//! A scripted `codex app-server` for the transport and thread tests (spec #860,
//! tickets #876/#878).
//!
//! It is a real WebSocket server on a real socket — TCP or unix — so the
//! production client (handshake, framing, correlation, subscriptions) runs
//! against the wire rather than a mock. It answers `initialize` with the fields
//! the pinned protocol requires, counts `initialized` notifications, and
//! scripts the thread surface: `thread/list` pages a seeded store, `thread/read`
//! returns one thread, `thread/resume` subscribes **only a thread that has
//! run**, `thread/name/set` updates the stored name and `thread/unsubscribe`
//! drops one connection's subscription without touching another's. It can push
//! `item/*` traffic to the subscribers of a thread, which is the only way to
//! prove the client's reader loop routes push traffic (ticket #878).
//!
//! Compiled only for tests.

#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Quote one value for `sh` with single quotes, escaping an embedded quote as
/// `'\''`. Test-only: a fixture path is interpolated into a fake binary's
/// script, and a temp path containing an apostrophe must not make a test fail
/// for a reason unrelated to the code under test.
#[cfg(unix)]
pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The control socket under a fixture's relocated home, as a **relative**
/// suffix: the deepest path any codex fixture puts under its temp root.
///
/// Relative on purpose: [`Path::join`] with an absolute argument *replaces* the
/// root, so a leading `/` here would measure the suffix alone and call every
/// root short enough.
#[cfg(unix)]
const DEEPEST_FIXTURE_SOCKET: &str = "codex-home/app-server-control/app-server-control.sock";

/// macOS is the tight one: `sun_path` is 104 bytes *including* the terminating
/// NUL, so a 103-byte path is the longest that binds there (Linux has 108).
#[cfg(unix)]
const SUN_PATH_MAX: usize = 104;

/// Whether `root` keeps its deepest fixture control socket inside `sun_path`.
#[cfg(unix)]
fn socket_path_fits(root: &Path) -> bool {
    root.join(DEEPEST_FIXTURE_SOCKET).as_os_str().len() < SUN_PATH_MAX
}

/// A temporary root for a fixture that binds (or dials) a **unix-domain
/// socket**.
///
/// The kernel copies a socket path verbatim into `sun_path` — it is never
/// resolved or canonicalised — so a path at or over the limit fails with
/// `ENAMETOOLONG`. `tempfile::tempdir()` roots under `$TMPDIR`, which on macOS
/// is `/var/folders/<xx>/<32 hex>/T/` (~51 bytes) before tempfile's own
/// `.tmpXXXXXX`; codex's control socket then adds
/// [`DEEPEST_FIXTURE_SOCKET`] (53 bytes, plus the joining separator),
/// overflowing the field. The `connect`
/// that overflows is reported as `Unreachable`, so the lifecycle correctly
/// reads "no answering daemon" and every attaching test fails as `Serverless`.
///
/// Start the root in a short base instead — the default temp directory when it
/// fits (so an exec-restricted `/tmp` is not forced), then `/tmp` and
/// `/var/tmp` — and prove the deepest fixture socket still fits.
#[cfg(unix)]
pub(crate) fn socket_tempdir() -> tempfile::TempDir {
    [
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
    ]
    .into_iter()
    .filter_map(|base| tempfile::Builder::new().prefix("cola").tempdir_in(base).ok())
    .find(|root| socket_path_fits(root.path()))
    .expect("a temp base that keeps a codex fixture's control socket inside `sun_path`")
}

/// A scripted `codex` CLI for discovery and lifecycle tests.
///
/// It answers the two management commands those paths use, logs every
/// invocation together with the `CODEX_HOME` it was given, and its
/// `app-server daemon start` links the control socket to a socket the test
/// already serves. That keeps a test's "daemon" a real WebSocket server on a
/// real unix socket while no real Codex state is touched.
#[cfg(unix)]
pub(crate) struct FakeCodex {
    pub(crate) binary: std::path::PathBuf,
    pub(crate) log: std::path::PathBuf,
    /// The file `daemon start` reads its link target from, so a test can point a
    /// later start at another socket **without rewriting the executable** (which
    /// can hit `ETXTBSY` while the previous instance is still mapped).
    target: Option<std::path::PathBuf>,
}

#[cfg(unix)]
impl FakeCodex {
    /// Write the fake CLI into `directory`. `real_socket` is what `daemon
    /// start` links the control socket to (`None`: a launcher that reports
    /// success but leaves no socket, the failure this path must name).
    pub(crate) fn write(
        directory: &Path,
        control_socket: &Path,
        real_socket: Option<&Path>,
        daemon_pid_record: Option<&Path>,
        managed_binary: &Path,
    ) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let binary = directory.join("codex");
        let log = directory.join("fake-codex.log");
        let report = json!({
            "status": "running",
            "backend": "pid",
            "managedCodexPath": managed_binary,
            "managedCodexVersion": "0.163.0",
            "socketPath": control_socket,
            "cliVersion": "0.163.0",
            "appServerVersion": "0.163.0",
        })
        .to_string();
        let (start, target) = match real_socket {
            Some(real_socket) => {
                let target = directory.join("link-target");
                std::fs::write(&target, real_socket.display().to_string())
                    .expect("write the fake link target");
                (
                    format!(
                        "mkdir -p {}\nln -sfn \"$(cat {})\" {}\nprintf '%s\\n' '{{\"status\":\"started\"}}'\n",
                        shell_single_quote(
                            &control_socket
                                .parent()
                                .expect("control dir")
                                .display()
                                .to_string()
                        ),
                        shell_single_quote(&target.display().to_string()),
                        shell_single_quote(&control_socket.display().to_string()),
                    ),
                    Some(target),
                )
            }
            None => ("printf '%s\\n' '{\"status\":\"started\"}'\n".to_string(), None),
        };
        // The fake daemon is a detached process of its own, with its own pid
        // record — the shape cola must never adopt as its own. Its stdio is
        // detached so `output()` does not wait on the pipe it inherits.
        let start = match daemon_pid_record {
            Some(record) => format!(
                "{}sleep 30 >/dev/null 2>&1 &\necho $! >> {}\n",
                start,
                shell_single_quote(&record.display().to_string())
            ),
            None => start,
        };
        let script = format!(
            "#!/bin/sh\n\
             printf 'CODEX_HOME=%s ARGS=%s\\n' \"$CODEX_HOME\" \"$*\" >> {log}\n\
             case \"$*\" in\n\
             \"app-server daemon version\")\n\
             \x20 if [ -e {socket} ]; then\n\
             \x20   printf '%s\\n' {report}\n\
             \x20   exit 0\n\
             \x20 fi\n\
             \x20 echo \"Error: failed to connect to {socket}\" >&2\n\
             \x20 exit 1\n\
             \x20 ;;\n\
             \"app-server daemon start\")\n\
             {start}\
             \x20 exit 0\n\
             \x20 ;;\n\
             *)\n\
             \x20 echo \"fake codex: unexpected command: $*\" >&2\n\
             \x20 exit 64\n\
             \x20 ;;\n\
             esac\n",
            log = shell_single_quote(&log.display().to_string()),
            socket = shell_single_quote(&control_socket.display().to_string()),
            report = shell_single_quote(&report),
        );
        std::fs::write(&binary, script).expect("write the fake codex CLI");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("make the fake codex CLI executable");
        Self { binary, log, target }
    }

    /// Point the next `daemon start` at another socket, without rewriting the
    /// script. Only meaningful when the CLI was written with a target.
    pub(crate) fn point_at(&self, socket: &Path) {
        let target = self
            .target
            .as_ref()
            .expect("this fake CLI was written without a link target");
        std::fs::write(target, socket.display().to_string()).expect("re-point the fake link target");
    }

    /// Every invocation, one `CODEX_HOME=… ARGS=…` line each.
    pub(crate) fn invocations(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// How many times `app-server daemon start` was run.
    pub(crate) fn starts(&self) -> usize {
        self.invocations()
            .iter()
            .filter(|line| line.ends_with("ARGS=app-server daemon start"))
            .count()
    }
}

/// One thread in the scripted app-server's store.
///
/// `has_history` is the spike's discriminator (ADR-0090's amendment, question
/// 2): `thread/start` creates a thread, but `thread/resume` answers `-32600 no
/// rollout found` until it has run a turn. A fixture with `has_history: false`
/// is exactly that never-run thread.
#[derive(Debug, Clone)]
pub(crate) struct ScriptedThread {
    pub(crate) id: String,
    /// The session tree root. Deliberately independent of `id`, so a client
    /// that derives it from the thread id instead of reading it fails a test.
    pub(crate) session_id: String,
    pub(crate) name: Option<String>,
    /// Set for a sub-agent thread; mapped onto the neutral list row's parent.
    pub(crate) parent_thread_id: Option<String>,
    pub(crate) preview: String,
    pub(crate) cwd: String,
    pub(crate) source: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) has_history: bool,
    /// The persisted turns `thread/read` and `thread/resume` carry. A real
    /// `thread/read` page is bounded, so a test can cap it with
    /// [`Self::read_turn_limit`] while `thread/turns/list` still pages the
    /// whole history.
    pub(crate) turns: Vec<Value>,
    /// How many turns a `thread/read`/`thread/resume` response carries. `None`
    /// carries every stored turn.
    pub(crate) read_turn_limit: Option<usize>,
    /// Answer `thread/turns/list` without the required `data` field, so a
    /// malformed page is a decode error rather than an empty success.
    pub(crate) malformed_turns_page: bool,
    /// Return a repeating `nextCursor` from `thread/turns/list`, so the client
    /// must refuse to loop and keep the projection incomplete.
    pub(crate) repeat_turns_cursor: bool,
    /// Delay a successful `thread/turns/list` reply without blocking the
    /// connection's push delivery, so a test can inject events while adoption
    /// is paging.
    pub(crate) turns_page_delay_ms: u64,
    /// The `model` the thread runs with, as `0.163.0` reports it (nullable).
    pub(crate) model: Option<String>,
    /// The `modelProvider` beside it (nullable), so a test can prove cola
    /// shows the real provider rather than a fabricated one.
    pub(crate) model_provider: Option<String>,
}

impl ScriptedThread {
    /// A thread that has run, so it can be adopted.
    pub(crate) fn adopted(id: &str, session_id: &str, cwd: &str, name: &str) -> Self {
        Self {
            id: id.to_string(),
            session_id: session_id.to_string(),
            name: Some(name.to_string()),
            parent_thread_id: None,
            preview: name.to_string(),
            cwd: cwd.to_string(),
            source: "cli".to_string(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            has_history: true,
            turns: Vec::new(),
            read_turn_limit: None,
            malformed_turns_page: false,
            repeat_turns_cursor: false,
            turns_page_delay_ms: 0,
            model: None,
            model_provider: None,
        }
    }

    /// A freshly `thread/start`-ed thread that has not run a turn: resume must
    /// fail with the documented no-rollout error.
    pub(crate) fn never_run(id: &str, session_id: &str, cwd: &str) -> Self {
        Self {
            has_history: false,
            name: None,
            ..Self::adopted(id, session_id, cwd, "")
        }
    }
}

/// One connection stream, tagged with its connection id so a release removes
/// exactly one subscriber.
type Stream = (usize, mpsc::UnboundedSender<Message>);

/// One answer a client sent to a server-initiated request, so a test can pair
/// it with the request the fake server broadcast.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ServerRequestReply {
    /// The connection that answered (1 is the first accepted).
    pub(crate) connection: usize,
    /// The JSON-RPC id, echoed verbatim by the client.
    pub(crate) id: Value,
    /// The `result` member, when the client answered with one.
    pub(crate) result: Option<Value>,
    /// The `error` member, when the client refused.
    pub(crate) error: Option<Value>,
}

#[derive(Debug, Clone)]
struct ServerRequest {
    id: Value,
    /// The request's thread, from `threadId`/`conversationId`; used to route
    /// the `serverRequest/resolved` broadcast.
    thread_id: Option<String>,
}

/// The state one fake server exposes to its test.
struct State {
    user_agent: String,
    reject_initialize: Option<String>,
    connections: AtomicUsize,
    initialized: AtomicUsize,
    /// `thread/loaded/list` calls: the lifecycle's liveness probe.
    liveness: AtomicUsize,
    /// Per-connection serving tasks, so a test can make the daemon *die*
    /// (break its live connections) while leaving its socket path on disk.
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    threads: std::sync::Mutex<Vec<ScriptedThread>>,
    /// Per-thread subscribers, each tagged with the connection's id so an
    /// unsubscribe removes exactly one stream.
    subscribers: std::sync::Mutex<std::collections::HashMap<String, Vec<Stream>>>,
    /// Every live connection, for a server-wide broadcast.
    live: std::sync::Mutex<Vec<Stream>>,
    next_connection: AtomicUsize,
    list_calls: AtomicUsize,
    list_params: std::sync::Mutex<Vec<Value>>,
    resume_calls: AtomicUsize,
    turns_calls: AtomicUsize,
    turns_params: std::sync::Mutex<Vec<Value>>,
    unsubscribe_calls: AtomicUsize,
    /// How many upcoming `thread/unsubscribe` calls answer with a JSON-RPC
    /// error, so the release path's failure arm is exercised.
    reject_unsubscribe: AtomicUsize,
    /// Answer `thread/resume` without a response, so a test can cancel a
    /// pending adopt.
    silent_resume: std::sync::atomic::AtomicBool,
    /// Fail this many `thread/turns/list` calls before answering normally.
    fail_turns_pages: AtomicUsize,
    /// Scripted notifications a `turn/start` emits to the thread's subscribers:
    /// the server-side origin of a turn a connection asked for.
    scripted_turns: std::sync::Mutex<std::collections::HashMap<String, Vec<(String, Value)>>>,
    /// `turn/start` calls received, so a test can prove which connection
    /// originated the turn.
    turn_starts: AtomicUsize,
    /// Every `turn/start` params object, per thread, in call order.
    turn_params: std::sync::Mutex<std::collections::HashMap<String, Vec<Value>>>,
    /// Every `turn/steer` params object, in call order.
    steer_params: std::sync::Mutex<Vec<Value>>,
    /// Every `turn/interrupt` params object, in call order.
    interrupt_params: std::sync::Mutex<Vec<Value>>,
    /// Every `thread/compact/start` params object, in call order.
    compact_params: std::sync::Mutex<Vec<Value>>,
    /// The live turn per thread: a second `turn/start` while one is running is
    /// folded into it and returns the same id (the spike's finding).
    live_turns: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// The next turn/item id the server assigns, as the real server does.
    next_turn: AtomicUsize,
    next_item: AtomicUsize,
    /// The next thread id `thread/start` assigns.
    next_thread: AtomicUsize,
    /// Threads deleted through `thread/delete`, in call order — the doctor
    /// probe's cleanup assertion.
    deleted_threads: std::sync::Mutex<Vec<String>>,
    /// How many upcoming `thread/delete` calls answer with a JSON-RPC error.
    reject_thread_deletes: AtomicUsize,
    /// Threads whose `turn/start` (and steer) echoes the request's own input as
    /// a server-generated `userMessage` item, so a test reads the prompt off the
    /// stream instead of hand-writing it.
    echo_prompts: std::sync::Mutex<std::collections::HashSet<String>>,
    /// The models `model/list` advertises, in server order.
    models: std::sync::Mutex<Vec<Value>>,
    /// The page size `model/list` uses when the caller sends no `limit`.
    model_page_size: AtomicUsize,
    /// The skills `skills/list` advertises.
    skills: std::sync::Mutex<Vec<Value>>,
    /// The rows `experimentalFeature/list` advertises.
    experimental_features: std::sync::Mutex<Vec<Value>>,
    /// `experimentalFeature/list` calls received.
    experimental_feature_calls: AtomicUsize,
    /// One-shot: answer the next `turn/start` with this JSON-RPC error.
    reject_turn_start: std::sync::Mutex<Option<String>>,
    /// One-shot: accept the next `turn/start` (persist and emit its stream) but
    /// close the connection without answering — an accepted request whose
    /// response was lost.
    drop_on_turn_start: std::sync::atomic::AtomicBool,
    /// One-shot: accept and persist the next `turn/start` (and emit its stream)
    /// but never answer, so the client cannot cache the turn id.
    silent_turn_start: std::sync::atomic::AtomicBool,
    /// One-shot: answer the next `thread/compact/start` with a JSON-RPC error.
    reject_compact: std::sync::atomic::AtomicBool,
    /// One-shot: accept the next `thread/compact/start` (persist and emit the
    /// item) but close the connection without answering.
    drop_on_compact: std::sync::atomic::AtomicBool,
    /// How many `thread/read` calls still hide a thread's in-progress turns, so
    /// a `/stop` can be exercised in the window before the server persists the
    /// turn.
    hide_live_turns: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    /// One-shot: leave the next `thread/read` unanswered (a stalled read).
    silent_thread_read: std::sync::atomic::AtomicBool,
    /// Answer the next `thread/read` with THIS thread instead of the requested
    /// one, modelling a response that names another thread.
    read_returns: std::sync::Mutex<Option<String>>,
    /// `model/list` calls received, so a paged catalog is proved by its pages.
    model_list_calls: AtomicUsize,
    /// The page size `thread/list` uses when the caller sends no `limit`.
    page_size: usize,
    /// Every `thread/start` request's params, in send order (issue #898).
    start_params: std::sync::Mutex<Vec<Value>>,
    /// Every `thread/resume` request's params, in send order.
    resume_params: std::sync::Mutex<Vec<Value>>,
    /// Server-initiated requests the fake server broadcast, in send order.
    server_requests: std::sync::Mutex<Vec<ServerRequest>>,
    /// Every answer a client sent to one of them.
    server_request_replies: std::sync::Mutex<Vec<ServerRequestReply>>,
    /// The next id the server assigns to a request it initiates.
    next_server_request: AtomicUsize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            user_agent: String::new(),
            reject_initialize: None,
            connections: AtomicUsize::new(0),
            initialized: AtomicUsize::new(0),
            liveness: AtomicUsize::new(0),
            tasks: std::sync::Mutex::new(Vec::new()),
            threads: std::sync::Mutex::new(Vec::new()),
            subscribers: std::sync::Mutex::new(std::collections::HashMap::new()),
            live: std::sync::Mutex::new(Vec::new()),
            next_connection: AtomicUsize::new(1),
            list_calls: AtomicUsize::new(0),
            list_params: std::sync::Mutex::new(Vec::new()),
            resume_calls: AtomicUsize::new(0),
            turns_calls: AtomicUsize::new(0),
            turns_params: std::sync::Mutex::new(Vec::new()),
            unsubscribe_calls: AtomicUsize::new(0),
            reject_unsubscribe: AtomicUsize::new(0),
            silent_resume: std::sync::atomic::AtomicBool::new(false),
            fail_turns_pages: AtomicUsize::new(0),
            scripted_turns: std::sync::Mutex::new(std::collections::HashMap::new()),
            turn_starts: AtomicUsize::new(0),
            turn_params: std::sync::Mutex::new(std::collections::HashMap::new()),
            steer_params: std::sync::Mutex::new(Vec::new()),
            interrupt_params: std::sync::Mutex::new(Vec::new()),
            compact_params: std::sync::Mutex::new(Vec::new()),
            live_turns: std::sync::Mutex::new(std::collections::HashMap::new()),
            next_turn: AtomicUsize::new(1),
            next_item: AtomicUsize::new(1),
            next_thread: AtomicUsize::new(1),
            deleted_threads: std::sync::Mutex::new(Vec::new()),
            reject_thread_deletes: AtomicUsize::new(0),
            echo_prompts: std::sync::Mutex::new(std::collections::HashSet::new()),
            models: std::sync::Mutex::new(Vec::new()),
            model_page_size: AtomicUsize::new(50),
            skills: std::sync::Mutex::new(Vec::new()),
            experimental_features: std::sync::Mutex::new(Vec::new()),
            experimental_feature_calls: AtomicUsize::new(0),
            reject_turn_start: std::sync::Mutex::new(None),
            drop_on_turn_start: std::sync::atomic::AtomicBool::new(false),
            silent_turn_start: std::sync::atomic::AtomicBool::new(false),
            reject_compact: std::sync::atomic::AtomicBool::new(false),
            drop_on_compact: std::sync::atomic::AtomicBool::new(false),
            hide_live_turns: std::sync::Mutex::new(std::collections::HashMap::new()),
            silent_thread_read: std::sync::atomic::AtomicBool::new(false),
            read_returns: std::sync::Mutex::new(None),
            model_list_calls: AtomicUsize::new(0),
            page_size: 2,
            start_params: std::sync::Mutex::new(Vec::new()),
            resume_params: std::sync::Mutex::new(Vec::new()),
            server_requests: std::sync::Mutex::new(Vec::new()),
            server_request_replies: std::sync::Mutex::new(Vec::new()),
            next_server_request: AtomicUsize::new(1000),
        }
    }
}

pub(crate) struct FakeServer {
    state: Arc<State>,
    /// Set on a server that can be stopped; `None` for the TCP constructors,
    /// which no test kills.
    shutdown: Option<Arc<tokio::sync::Notify>>,
}

impl FakeServer {
    /// Serve a unix socket at `path`. The caller may point a symlink at it —
    /// that is the shipped control-socket shape. Unix-only, like the
    /// transport it exercises.
    #[cfg(unix)]
    pub(crate) async fn unix(path: &Path, user_agent: &str) -> Self {
        let listener = UnixListener::bind(path).expect("bind fake unix app-server");
        let state = Arc::new(State {
            user_agent: user_agent.to_string(),
            ..State::default()
        });
        let serving = Arc::clone(&state);
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let signal = Arc::clone(&shutdown);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = signal.notified() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        accept_connection(stream, Arc::clone(&serving));
                    }
                }
            }
            // Dropping the listener closes the socket, but the *path* stays on
            // disk: exactly a crashed daemon's stale control socket.
        });
        Self {
            state,
            shutdown: Some(shutdown),
        }
    }

    /// Make the fake daemon die: stop accepting, break every live connection,
    /// and leave the socket path behind. Unix only, matching [`Self::unix`].
    ///
    /// `tokio`'s `UnixListener` does not unlink on drop, so a later dial gets
    /// `ECONNREFUSED` at a path that still exists — the stale-socket shape the
    /// lifecycle must not read as a live daemon.
    #[cfg(unix)]
    pub(crate) async fn die(&self) {
        if let Some(shutdown) = &self.shutdown {
            shutdown.notify_one();
        }
        let tasks: Vec<_> = self
            .state
            .tasks
            .lock()
            .expect("connection tasks")
            .drain(..)
            .collect();
        for task in tasks {
            task.abort();
        }
        // Give the aborted tasks a tick to actually drop their sockets.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    /// Serve a loopback TCP socket; returns the server and its `ws://` URL.
    pub(crate) async fn tcp(user_agent: &str) -> (Self, String) {
        Self::tcp_with(user_agent, None).await
    }

    /// Serve a loopback TCP socket that answers `initialize` with a JSON-RPC
    /// error, for the handshake's failure path.
    pub(crate) async fn tcp_rejecting(message: &str) -> (Self, String) {
        Self::tcp_with("", Some(message)).await
    }

    async fn tcp_with(user_agent: &str, reject_initialize: Option<&str>) -> (Self, String) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake app-server");
        let address = listener.local_addr().expect("fake app-server address");
        let state = Arc::new(State {
            user_agent: user_agent.to_string(),
            reject_initialize: reject_initialize.map(str::to_string),
            ..State::default()
        });
        let serving = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accept_connection(stream, Arc::clone(&serving));
            }
        });
        (
            Self {
                state,
                shutdown: None,
            },
            format!("ws://{address}"),
        )
    }

    /// Add one thread to the stored list. The list is served in insertion
    /// order, which keeps paging assertions deterministic.
    pub(crate) fn seed_thread(&self, thread: ScriptedThread) {
        self.state.threads.lock().expect("scripted threads").push(thread);
    }

    /// Change a seeded thread's reported model, so a later `thread/read`
    /// answers with it — a peer client switching the model, or a server that
    /// stops naming one (both are `thread/model` facts, issue #904).
    pub(crate) fn set_thread_model(&self, thread_id: &str, model: Option<&str>, provider: Option<&str>) {
        let mut threads = self.state.threads.lock().expect("scripted threads");
        if let Some(thread) = threads.iter_mut().find(|thread| thread.id == thread_id) {
            thread.model = model.map(str::to_string);
            thread.model_provider = provider.map(str::to_string);
        }
    }

    /// Send a notification to every live connection, whether it subscribed or
    /// not (the `thread/started` / `thread/status/changed` shape).
    pub(crate) fn push_notification(&self, method: &str, params: Value) {
        let message = Message::Text(
            json!({ "jsonrpc": "2.0", "method": method, "params": params })
                .to_string()
                .into(),
        );
        self.state
            .live
            .lock()
            .expect("live connections")
            .retain(|(_, connection)| connection.send(message.clone()).is_ok());
    }

    /// Send one notification to the subscribers of `thread_id` only (the
    /// `turn/*` and `item/*` shape). Returns how many streams received it.
    pub(crate) fn push_to_thread(&self, thread_id: &str, method: &str, params: Value) -> usize {
        let message = Message::Text(
            json!({ "jsonrpc": "2.0", "method": method, "params": params })
                .to_string()
                .into(),
        );
        let mut subscribers = self.state.subscribers.lock().expect("subscribers");
        let Some(streams) = subscribers.get_mut(thread_id) else {
            return 0;
        };
        streams.retain(|(_, stream)| stream.send(message.clone()).is_ok());
        streams.len()
    }

    /// Send one server-initiated request to the subscribers of `thread_id` —
    /// the spike's shape: an approval reaches **every** subscriber, not only
    /// the turn's initiator. Returns the id the server chose.
    pub(crate) fn request_to_subscribers(&self, thread_id: &str, method: &str, mut params: Value) -> Value {
        let id = Value::from(self.state.next_server_request.fetch_add(1, Ordering::SeqCst) as u64);
        if let Value::Object(object) = &mut params
            && !object.contains_key("threadId")
        {
            object.insert("threadId".to_string(), json!(thread_id));
        }
        self.state
            .server_requests
            .lock()
            .expect("server requests")
            .push(ServerRequest {
                id: id.clone(),
                thread_id: Some(thread_id.to_string()),
            });
        let message = Message::Text(
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
                .to_string()
                .into(),
        );
        if let Some(streams) = self
            .state
            .subscribers
            .lock()
            .expect("subscribers")
            .get_mut(thread_id)
        {
            streams.retain(|(_, stream)| stream.send(message.clone()).is_ok());
        }
        id
    }

    /// Send one server-initiated request to every live connection, whether it
    /// subscribed or not (a server-global request, such as an account token
    /// refresh).
    pub(crate) fn request_to_all(&self, method: &str, params: Value) -> Value {
        let id = Value::from(self.state.next_server_request.fetch_add(1, Ordering::SeqCst) as u64);
        self.state
            .server_requests
            .lock()
            .expect("server requests")
            .push(ServerRequest {
                id: id.clone(),
                thread_id: None,
            });
        let message = Message::Text(
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
                .to_string()
                .into(),
        );
        self.state
            .live
            .lock()
            .expect("live connections")
            .retain(|(_, connection)| connection.send(message.clone()).is_ok());
        id
    }

    /// Every answer clients sent to a server-initiated request.
    pub(crate) fn server_request_replies(&self) -> Vec<ServerRequestReply> {
        self.state
            .server_request_replies
            .lock()
            .expect("server request replies")
            .clone()
    }

    /// Wait until at least `expected` answers arrived; the client's frame is
    /// asynchronous, so asserting in the same tick races its writer.
    pub(crate) async fn wait_for_server_request_replies(&self, expected: usize) {
        let replies = || self.server_request_replies().len();
        wait_until(replies, expected, "server request replies").await;
    }

    /// Push one completed agent-message item to a thread's subscribers.
    pub(crate) fn push_item(&self, thread_id: &str, item_id: &str, text: &str) -> usize {
        self.push_to_thread(
            thread_id,
            "item/completed",
            json!({
                "threadId": thread_id,
                "turnId": "turn-1",
                "completedAtMs": 1_700_000_001_000_i64,
                "item": { "type": "agentMessage", "id": item_id, "text": text },
            }),
        )
    }

    /// Connections accepted so far (one per client handshake).
    pub(crate) fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// `initialized` notifications received so far.
    pub(crate) fn initialized(&self) -> usize {
        self.state.initialized.load(Ordering::SeqCst)
    }

    /// Wait until the server task has observed `expected` `initialized`
    /// notifications; the client's send is asynchronous, so asserting in the
    /// same tick would race the fake server's read.
    pub(crate) async fn wait_for_initialized(&self, expected: usize) {
        wait_until(|| self.initialized(), expected, "initialized notifications").await;
    }

    /// Wait until `expected` connections have been accepted.
    pub(crate) async fn wait_for_connections(&self, expected: usize) {
        wait_until(|| self.connections(), expected, "connections").await;
    }

    /// `thread/loaded/list` calls received so far (the liveness probe).
    pub(crate) fn liveness_probes(&self) -> usize {
        self.state.liveness.load(Ordering::SeqCst)
    }

    /// Wait until `expected` liveness probes have been answered.
    pub(crate) async fn wait_for_liveness(&self, expected: usize) {
        wait_until(|| self.liveness_probes(), expected, "liveness probes").await;
    }

    /// `thread/list` calls received so far.
    pub(crate) fn list_calls(&self) -> usize {
        self.state.list_calls.load(Ordering::SeqCst)
    }

    /// Every `thread/list` params object, in call order.
    pub(crate) fn list_params(&self) -> Vec<Value> {
        self.state.list_params.lock().expect("list params").clone()
    }

    /// Every `thread/start` params object, in call order (issue #898).
    pub(crate) fn start_params(&self) -> Vec<Value> {
        self.state.start_params.lock().expect("start params").clone()
    }

    /// Every `thread/resume` params object, in call order.
    pub(crate) fn resume_params(&self) -> Vec<Value> {
        self.state.resume_params.lock().expect("resume params").clone()
    }

    /// Leave the next `thread/resume` unanswered (the connection stays open),
    /// so a test can cancel the adopt future mid-await. One-shot: the resume
    /// after it answers normally.
    pub(crate) fn silence_next_resume(&self) {
        self.state.silent_resume.store(true, Ordering::SeqCst);
    }

    /// `thread/resume` calls received so far.
    pub(crate) fn resume_calls(&self) -> usize {
        self.state.resume_calls.load(Ordering::SeqCst)
    }

    /// Wait until `expected` `thread/resume` calls have arrived.
    pub(crate) async fn wait_for_resume_calls(&self, expected: usize) {
        wait_until(|| self.resume_calls(), expected, "resume calls").await;
    }

    /// `thread/turns/list` calls received so far.
    pub(crate) fn turns_calls(&self) -> usize {
        self.state.turns_calls.load(Ordering::SeqCst)
    }

    /// Every `thread/turns/list` params object, in call order.
    pub(crate) fn turns_params(&self) -> Vec<Value> {
        self.state.turns_params.lock().expect("turns params").clone()
    }

    /// `thread/unsubscribe` calls received so far.
    pub(crate) fn unsubscribe_calls(&self) -> usize {
        self.state.unsubscribe_calls.load(Ordering::SeqCst)
    }

    /// Answer the next `thread/unsubscribe` calls with a JSON-RPC error, so a
    /// test can prove the release path survives a server that refuses it.
    pub(crate) fn reject_unsubscribe(&self, count: usize) {
        self.state.reject_unsubscribe.store(count, Ordering::SeqCst);
    }

    /// Wait until the server has recorded a live turn for a thread — the
    /// `turn/start` was accepted and persisted — returning `(thread id, turn
    /// id)`. A cancelled probe's cleanup depends on this record, not on the
    /// client having processed `turn/start`'s reply.
    pub(crate) async fn wait_for_live_turn_record(&self) -> (String, String) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some((thread, turn)) = self.state.live_turns.lock().expect("live turns").iter().next() {
                return (thread.clone(), turn.clone());
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the server never recorded a live turn"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// One-shot: accept and persist the next `turn/start` (and emit its
    /// `turn/started`) but never answer it, so a client cannot cache the turn
    /// id — the cancelled-cleanup path that must find it from the thread.
    pub(crate) fn silence_next_turn_start(&self) {
        self.state.silent_turn_start.store(true, Ordering::SeqCst);
    }

    /// How many connection streams are currently subscribed to `thread_id`.
    pub(crate) fn subscribers(&self, thread_id: &str) -> usize {
        self.state
            .subscribers
            .lock()
            .expect("subscribers")
            .get(thread_id)
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// Wait until `expected` streams subscribe to `thread_id`.
    pub(crate) async fn wait_for_subscribers(&self, thread_id: &str, expected: usize) {
        wait_until(|| self.subscribers(thread_id), expected, "thread subscribers").await;
    }

    /// The name the server currently stores for `thread_id`.
    pub(crate) fn thread_name(&self, thread_id: &str) -> Option<String> {
        self.state
            .threads
            .lock()
            .expect("scripted threads")
            .iter()
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.name.clone())
    }

    /// Give a never-run thread a rollout, as the user's own client's first turn
    /// would. The next `thread/resume` then succeeds.
    pub(crate) fn mark_history(&self, thread_id: &str) {
        if let Some(thread) = self
            .state
            .threads
            .lock()
            .expect("scripted threads")
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        {
            thread.has_history = true;
        }
    }

    /// Replace a thread's persisted turns, as a later server write would.
    pub(crate) fn set_turns(&self, thread_id: &str, turns: Vec<Value>) {
        if let Some(thread) = self
            .state
            .threads
            .lock()
            .expect("scripted threads")
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        {
            thread.turns = turns;
        }
    }

    /// Fail the next `count` `thread/turns/list` calls with a malformed page,
    /// so a retry path can be exercised.
    pub(crate) fn fail_turns_pages(&self, count: usize) {
        self.state.fail_turns_pages.store(count, Ordering::SeqCst);
    }

    /// Fail the next `count` `thread/delete` calls with a JSON-RPC error.
    pub(crate) fn reject_thread_deletes(&self, count: usize) {
        self.state.reject_thread_deletes.store(count, Ordering::SeqCst);
    }

    /// Threads deleted through `thread/delete`, in call order.
    pub(crate) fn deleted_threads(&self) -> Vec<String> {
        self.state
            .deleted_threads
            .lock()
            .expect("deleted threads")
            .clone()
    }

    /// Script the notifications a `turn/start` on `thread_id` emits to that
    /// thread's subscribers. The request itself is the origin: a test starts
    /// the turn from another connection, and the fake server — not the test —
    /// pushes the stream.
    ///
    /// `thread_id` of `"*"` applies to any thread whose own script is unset,
    /// so a test can script a turn for a thread the server has not created yet
    /// (`thread/start` assigns its id).
    pub(crate) fn script_turn(&self, thread_id: &str, events: Vec<Value>) {
        let events = events
            .into_iter()
            .filter_map(|event| {
                let method = event.get("method")?.as_str()?.to_string();
                let params = event.get("params").cloned().unwrap_or(Value::Null);
                Some((method, params))
            })
            .collect();
        self.state
            .scripted_turns
            .lock()
            .expect("scripted turns")
            .insert(thread_id.to_string(), events);
    }

    /// `turn/start` calls received.
    pub(crate) fn turn_starts(&self) -> usize {
        self.state.turn_starts.load(Ordering::SeqCst)
    }

    /// Every `turn/start` params object sent for `thread_id`, in call order.
    pub(crate) fn turn_params(&self, thread_id: &str) -> Vec<Value> {
        self.state
            .turn_params
            .lock()
            .expect("turn params")
            .get(thread_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Every `turn/steer` params object, in call order.
    pub(crate) fn steer_params(&self) -> Vec<Value> {
        self.state.steer_params.lock().expect("steer params").clone()
    }

    /// Every `turn/interrupt` params object, in call order.
    pub(crate) fn interrupt_params(&self) -> Vec<Value> {
        self.state
            .interrupt_params
            .lock()
            .expect("interrupt params")
            .clone()
    }

    /// Every `thread/compact/start` params object, in call order.
    pub(crate) fn compact_params(&self) -> Vec<Value> {
        self.state.compact_params.lock().expect("compact params").clone()
    }

    /// The turn the server still considers live for `thread_id`.
    pub(crate) fn live_turn(&self, thread_id: &str) -> Option<String> {
        self.state
            .live_turns
            .lock()
            .expect("live turns")
            .get(thread_id)
            .cloned()
    }

    /// Echo a thread's prompts (and steers) back as server-generated user
    /// messages: the turn's content then comes from the request itself.
    pub(crate) fn echo_prompts(&self, thread_id: &str) {
        self.state
            .echo_prompts
            .lock()
            .expect("echoed prompts")
            .insert(thread_id.to_string());
    }

    /// The models `model/list` serves.
    pub(crate) fn set_models(&self, models: Vec<Value>) {
        *self.state.models.lock().expect("models") = models;
    }

    /// The page size `model/list` uses when the caller sends no `limit`.
    pub(crate) fn set_model_page_size(&self, size: usize) {
        self.state.model_page_size.store(size, Ordering::SeqCst);
    }

    /// `model/list` calls received, in order (a paged read proves its pages).
    pub(crate) fn model_list_calls(&self) -> usize {
        self.state.model_list_calls.load(Ordering::SeqCst)
    }

    /// Answer the next `turn/start` with `message` as a JSON-RPC error: a
    /// definite refusal.
    pub(crate) fn reject_next_turn_start(&self, message: &str) {
        *self.state.reject_turn_start.lock().expect("reject turn start") = Some(message.to_string());
    }

    /// Accept the next `turn/start` (persisting and emitting its stream) but
    /// close the connection without answering: the request landed, its response
    /// was lost.
    pub(crate) fn drop_next_turn_start_response(&self) {
        self.state.drop_on_turn_start.store(true, Ordering::SeqCst);
    }

    /// Answer the next `thread/compact/start` with a JSON-RPC error.
    pub(crate) fn reject_next_compact(&self) {
        self.state.reject_compact.store(true, Ordering::SeqCst);
    }

    /// Accept the next `thread/compact/start` (persisting and emitting the
    /// compaction item) but close the connection without answering.
    pub(crate) fn drop_next_compact_response(&self) {
        self.state.drop_on_compact.store(true, Ordering::SeqCst);
    }

    /// Leave the next `thread/read` unanswered, modelling a stalled request.
    pub(crate) fn silence_next_thread_read(&self) {
        self.state.silent_thread_read.store(true, Ordering::SeqCst);
    }

    /// Answer the next `thread/read` with `thread_id`'s payload whatever was
    /// asked for: a response that names another thread than the request.
    pub(crate) fn answer_next_thread_read_with(&self, thread_id: &str) {
        *self.state.read_returns.lock().expect("read returns") = Some(thread_id.to_string());
    }

    /// Hide a thread's in-progress turns from the next `reads` `thread/read`
    /// answers, modelling `/stop` landing before the server persisted the turn.
    pub(crate) fn hide_live_turn_for(&self, thread_id: &str, reads: usize) {
        self.state
            .hide_live_turns
            .lock()
            .expect("hidden live turns")
            .insert(thread_id.to_string(), reads);
    }

    /// The skills `skills/list` serves.
    pub(crate) fn set_skills(&self, skills: Vec<Value>) {
        *self.state.skills.lock().expect("skills") = skills;
    }

    /// The rows `experimentalFeature/list` serves.
    pub(crate) fn set_experimental_features(&self, features: Vec<Value>) {
        *self
            .state
            .experimental_features
            .lock()
            .expect("experimental features") = features;
    }

    /// `experimentalFeature/list` calls received, so a test can prove the
    /// adapter asked the server rather than guessing.
    pub(crate) fn experimental_feature_calls(&self) -> usize {
        self.state.experimental_feature_calls.load(Ordering::SeqCst)
    }
}

/// Count one accepted connection and serve it on its own task. The accept loop
/// is the only platform-specific part of a fake server; this half is shared by
/// the TCP and unix listeners.
fn accept_connection<S>(stream: S, state: Arc<State>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    state.connections.fetch_add(1, Ordering::SeqCst);
    let connection_id = state.next_connection.fetch_add(1, Ordering::SeqCst);
    let (outgoing, incoming) = mpsc::unbounded_channel::<Message>();
    state
        .live
        .lock()
        .expect("live connections")
        .push((connection_id, outgoing.clone()));
    let serving = Arc::clone(&state);
    let cleanup = Arc::clone(&state);
    let handle = tokio::spawn(async move {
        serve(stream, serving, connection_id, outgoing, incoming).await;
        deregister(&cleanup, connection_id);
    });
    // Registered so a test can make the daemon die mid-life (`FakeServer::die`)
    // and break the live connection, not only the listener.
    state.tasks.lock().expect("connection tasks").push(handle);
}

/// Drop every trace of a connection that ended (or died).
fn deregister(state: &State, connection_id: usize) {
    state
        .live
        .lock()
        .expect("live connections")
        .retain(|(id, _)| *id != connection_id);
    for streams in state.subscribers.lock().expect("subscribers").values_mut() {
        streams.retain(|(id, _)| *id != connection_id);
    }
}

async fn wait_until(count: impl Fn() -> usize, expected: usize, what: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while count() < expected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expected} {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn serve<S>(
    stream: S,
    state: Arc<State>,
    connection_id: usize,
    outgoing: mpsc::UnboundedSender<Message>,
    mut pushed: mpsc::UnboundedReceiver<Message>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut incoming) = socket.split();
    loop {
        tokio::select! {
            message = incoming.next() => {
                let Some(Ok(message)) = message else { break };
                let Message::Text(text) = message else { continue };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                if !handle_call(&state, connection_id, &outgoing, &mut sink, value).await {
                    break;
                }
            }
            message = pushed.recv() => {
                let Some(message) = message else { break };
                if sink.send(message).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Answer one client call. Returns `false` when the connection should stop.
async fn handle_call<S>(
    state: &Arc<State>,
    connection_id: usize,
    outgoing: &mpsc::UnboundedSender<Message>,
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    value: Value,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // No method means this is the client's ANSWER to a request the server
    // initiated (JSON-RPC responses are the only method-less frames a client
    // sends). Record it, then resolve the request for every subscriber — the
    // real server's `serverRequest/resolved` shape, which is what withdraws
    // the card on the other client.
    if value.get("method").is_none() {
        handle_server_response(state, connection_id, &value);
        return true;
    }
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match value.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            if let Some(reject) = &state.reject_initialize {
                return reply_error(sink, id, -32600, reject).await;
            }
            // A broadcast that races the response: the client's correlation
            // loop must skip it, never read it as the reply.
            let broadcast = json!({
                "method": "remoteControl/status/changed",
                "params": { "status": "disabled" },
            });
            if send(sink, broadcast).await.is_err() {
                return false;
            }
            let result = json!({
                "userAgent": state.user_agent,
                "codexHome": "/tmp/codex-home",
                "platformFamily": "unix",
                "platformOs": "linux",
            });
            reply(sink, id, result).await
        }
        Some("initialized") => {
            state.initialized.fetch_add(1, Ordering::SeqCst);
            true
        }
        Some("thread/loaded/list") => {
            state.liveness.fetch_add(1, Ordering::SeqCst);
            reply(sink, id, json!({ "data": [] })).await
        }
        Some("thread/list") => {
            state.list_calls.fetch_add(1, Ordering::SeqCst);
            state
                .list_params
                .lock()
                .expect("list params")
                .push(params.clone());
            replying(sink, id, list_threads(state, &params)).await
        }
        Some("thread/start") => {
            state
                .start_params
                .lock()
                .expect("start params")
                .push(params.clone());
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("/tmp")
                .to_string();
            let thread_id = format!("thread-{}", state.next_thread.fetch_add(1, Ordering::SeqCst));
            let thread = ScriptedThread::never_run(&thread_id, &thread_id, &cwd);
            state
                .threads
                .lock()
                .expect("scripted threads")
                .push(thread.clone());
            reply(sink, id, json!({ "thread": thread_json(&thread) })).await
        }
        Some("thread/delete") => {
            let remaining = state.reject_thread_deletes.load(Ordering::SeqCst);
            if remaining > 0 {
                state.reject_thread_deletes.fetch_sub(1, Ordering::SeqCst);
                return reply_error(sink, id, -32000, "delete refused").await;
            }
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            state
                .threads
                .lock()
                .expect("scripted threads")
                .retain(|thread| thread.id != thread_id);
            state.subscribers.lock().expect("subscribers").remove(thread_id);
            state
                .deleted_threads
                .lock()
                .expect("deleted threads")
                .push(thread_id.to_string());
            reply(sink, id, json!({})).await
        }
        Some("thread/read") => {
            if state.silent_thread_read.swap(false, Ordering::SeqCst) {
                return true;
            }
            let requested = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            let substituted = state.read_returns.lock().expect("read returns").take();
            let thread_id = substituted.as_deref().unwrap_or(requested);
            let hide_live = {
                let mut hidden = state.hide_live_turns.lock().expect("hidden live turns");
                match hidden.get_mut(thread_id) {
                    Some(remaining) if *remaining > 0 => {
                        *remaining -= 1;
                        true
                    }
                    _ => false,
                }
            };
            match find_thread(state, thread_id) {
                Some(mut thread) => {
                    if hide_live {
                        for turn in &mut thread.turns {
                            if turn.get("status").and_then(Value::as_str) == Some("inProgress") {
                                turn["status"] = json!("completed");
                            }
                        }
                    }
                    replying(sink, id, json!({ "thread": thread_json(&thread) })).await
                }
                None => reply_error(sink, id, -32600, &format!("thread not found: {thread_id}")).await,
            }
        }
        Some("thread/turns/list") => {
            state.turns_calls.fetch_add(1, Ordering::SeqCst);
            state
                .turns_params
                .lock()
                .expect("turns params")
                .push(params.clone());
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            let Some(thread) = find_thread(state, thread_id) else {
                return reply_error(sink, id, -32600, &format!("thread not found: {thread_id}")).await;
            };
            if thread.malformed_turns_page {
                return replying(sink, id, json!({ "nextCursor": null, "backwardsCursor": null })).await;
            }
            let remaining = state.fail_turns_pages.load(Ordering::SeqCst);
            if remaining > 0 {
                state.fail_turns_pages.fetch_sub(1, Ordering::SeqCst);
                return replying(sink, id, json!({ "nextCursor": null, "backwardsCursor": null })).await;
            }
            let offset = params
                .get("cursor")
                .and_then(Value::as_str)
                .and_then(|cursor| cursor.parse::<usize>().ok())
                .unwrap_or(0)
                .min(thread.turns.len());
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|limit| limit as usize)
                .filter(|limit| *limit > 0)
                .unwrap_or(2);
            let end = (offset + limit).min(thread.turns.len());
            let next = if thread.repeat_turns_cursor {
                Some(json!("0"))
            } else {
                (end < thread.turns.len()).then(|| json!(end.to_string()))
            };
            let result = json!({
                "data": thread.turns[offset..end],
                "nextCursor": next,
                "backwardsCursor": null,
            });
            if thread.turns_page_delay_ms == 0 {
                return replying(sink, id, result).await;
            }
            // Reply from a spawned task so the serve loop keeps forwarding
            // pushed notifications while the page is "in flight".
            let outgoing = outgoing.clone();
            let delay = thread.turns_page_delay_ms;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                let _ = outgoing.send(Message::Text(
                    json!({ "jsonrpc": "2.0", "id": id, "result": result })
                        .to_string()
                        .into(),
                ));
            });
            true
        }
        Some("test/malformed") => {
            // A broken peer: a text frame that is not JSON. The client must
            // close the connection and fail the request instead of waiting.
            return send_text(sink, "this frame is not json").await;
        }
        Some("turn/start") => {
            state.turn_starts.fetch_add(1, Ordering::SeqCst);
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            state
                .turn_params
                .lock()
                .expect("turn params")
                .entry(thread_id.to_string())
                .or_default()
                .push(params.clone());
            let rejected = state.reject_turn_start.lock().expect("reject turn start").take();
            if let Some(message) = rejected {
                return reply_error(sink, id, -32000, &message).await;
            }
            let drop_response = state.drop_on_turn_start.swap(false, Ordering::SeqCst);
            let echo = state
                .echo_prompts
                .lock()
                .expect("echoed prompts")
                .contains(thread_id);
            // The spike's fold: a `turn/start` while a turn is active is
            // accepted into that turn and returns ITS id, never a second turn.
            let folded = state
                .live_turns
                .lock()
                .expect("live turns")
                .get(thread_id)
                .cloned();
            if let Some(live) = folded {
                if !drop_response
                    && !reply(
                        sink,
                        id,
                        json!({ "turn": { "id": live, "status": "inProgress" } }),
                    )
                    .await
                {
                    return false;
                }
                if echo {
                    emit_user_message(state, thread_id, &live, &params, false);
                }
                return !drop_response;
            }
            let turn_id = format!("turn-{}", state.next_turn.fetch_add(1, Ordering::SeqCst));
            state
                .live_turns
                .lock()
                .expect("live turns")
                .insert(thread_id.to_string(), turn_id.clone());
            // The initiating connection is a subscriber of its own turn (the
            // spike's A received the stream without an explicit resume), so
            // `turn/*`, `item/*` and `error` reach it.
            {
                let mut subscribers = state.subscribers.lock().expect("subscribers");
                let streams = subscribers.entry(thread_id.to_string()).or_default();
                if !streams.iter().any(|(id, _)| *id == connection_id) {
                    streams.push((connection_id, outgoing.clone()));
                }
            }
            let started_at = now_secs();
            let result = json!({
                "turn": { "id": turn_id, "status": "inProgress", "startedAt": started_at, "items": [] }
            });
            // A silent start persists and streams the turn but never answers:
            // the client can never cache the id (a cancelled cleanup must find
            // it from the thread itself).
            let silent = state.silent_turn_start.swap(false, Ordering::SeqCst);
            if !drop_response && !silent && !reply(sink, id, result).await {
                return false;
            }
            emit(
                state,
                thread_id,
                "turn/started",
                json!({
                    "threadId": thread_id,
                    "turn": { "id": turn_id, "status": "inProgress", "startedAt": started_at, "items": [] },
                }),
            );
            if echo {
                emit_user_message(state, thread_id, &turn_id, &params, true);
            }
            // The server, not the test, emits the stream: a turn started on
            // this connection fans out to every subscriber of the thread.
            let scripted = {
                let turns = state.scripted_turns.lock().expect("scripted turns");
                turns
                    .get(thread_id)
                    .or_else(|| turns.get("*"))
                    .cloned()
                    .unwrap_or_default()
            };
            for (method, params) in &scripted {
                // A real turn notification names its thread; a test script need
                // not repeat the id `thread/start` assigned.
                let mut params = params.clone();
                if let Some(object) = params.as_object_mut() {
                    object.entry("threadId").or_insert_with(|| json!(thread_id));
                }
                emit(state, thread_id, method, params);
                if method == "turn/completed" {
                    state.live_turns.lock().expect("live turns").remove(thread_id);
                }
            }
            !drop_response
        }
        Some("turn/steer") => {
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            state
                .steer_params
                .lock()
                .expect("steer params")
                .push(params.clone());
            let live = state
                .live_turns
                .lock()
                .expect("live turns")
                .get(thread_id)
                .cloned();
            let Some(live) = live else {
                // The protocol's precondition: `expectedTurnId` must name the
                // active turn, and an idle thread has none.
                return reply_error(sink, id, -32600, "no active turn to steer").await;
            };
            let expected = params.get("expectedTurnId").and_then(Value::as_str);
            if expected != Some(live.as_str()) {
                return reply_error(
                    sink,
                    id,
                    -32600,
                    &format!("expectedTurnId {expected:?} does not match the active turn {live}"),
                )
                .await;
            }
            if !reply(sink, id, json!({ "turnId": live })).await {
                return false;
            }
            // A steer is a user message inside the active turn: the adapter
            // renders it through the ordinary item path (ADR-0090's amendment).
            emit_user_message(state, thread_id, &live, &params, true);
            true
        }
        Some("turn/interrupt") => {
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            state
                .interrupt_params
                .lock()
                .expect("interrupt params")
                .push(params.clone());
            if !reply(sink, id, json!({})).await {
                return false;
            }
            let live = state.live_turns.lock().expect("live turns").remove(thread_id);
            if let Some(turn_id) = live {
                emit(
                    state,
                    thread_id,
                    "turn/completed",
                    json!({
                        "threadId": thread_id,
                        "turn": {
                            "id": turn_id,
                            "status": "interrupted",
                            "startedAt": now_secs(),
                            "completedAt": now_secs() + 1,
                        },
                    }),
                );
            }
            true
        }
        Some("thread/compact/start") => {
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            state
                .compact_params
                .lock()
                .expect("compact params")
                .push(params.clone());
            if state.reject_compact.swap(false, Ordering::SeqCst) {
                return reply_error(sink, id, -32000, "compaction refused").await;
            }
            let drop_response = state.drop_on_compact.swap(false, Ordering::SeqCst);
            if !drop_response && !reply(sink, id, json!({})).await {
                return false;
            }
            // Codex reports the compaction as a `contextCompaction` item inside
            // a turn; when none is running, the compaction turn is its own.
            let (turn_id, created) = match state
                .live_turns
                .lock()
                .expect("live turns")
                .get(thread_id)
                .cloned()
            {
                Some(turn_id) => (turn_id, false),
                None => {
                    let turn_id = format!("turn-{}", state.next_turn.fetch_add(1, Ordering::SeqCst));
                    (turn_id, true)
                }
            };
            if created {
                emit(
                    state,
                    thread_id,
                    "turn/started",
                    json!({
                        "threadId": thread_id,
                        "turn": { "id": turn_id, "status": "inProgress", "startedAt": now_secs() },
                    }),
                );
            }
            let item_id = format!("item-{}", state.next_item.fetch_add(1, Ordering::SeqCst));
            emit(
                state,
                thread_id,
                "item/completed",
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "completedAtMs": now_ms(),
                    "item": { "type": "contextCompaction", "id": item_id },
                }),
            );
            if created {
                emit(
                    state,
                    thread_id,
                    "turn/completed",
                    json!({
                        "threadId": thread_id,
                        "turn": {
                            "id": turn_id,
                            "status": "completed",
                            "startedAt": now_secs(),
                            "completedAt": now_secs() + 1,
                        },
                    }),
                );
            }
            !drop_response
        }
        Some("experimentalFeature/list") => {
            state.experimental_feature_calls.fetch_add(1, Ordering::SeqCst);
            let (data, next) = {
                let features = state.experimental_features.lock().expect("experimental features");
                let offset = params
                    .get("cursor")
                    .and_then(Value::as_str)
                    .and_then(|cursor| cursor.parse::<usize>().ok())
                    .unwrap_or(0)
                    .min(features.len());
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|limit| limit as usize)
                    .filter(|limit| *limit > 0)
                    .unwrap_or(200);
                let end = (offset + limit).min(features.len());
                let next = (end < features.len()).then(|| json!(end.to_string()));
                (features[offset..end].to_vec(), next)
            };
            replying(sink, id, json!({ "data": data, "nextCursor": next })).await
        }
        Some("model/list") => {
            state.model_list_calls.fetch_add(1, Ordering::SeqCst);
            let models = state.models.lock().expect("models").clone();
            let offset = params
                .get("cursor")
                .and_then(Value::as_str)
                .and_then(|cursor| cursor.parse::<usize>().ok())
                .unwrap_or(0)
                .min(models.len());
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|limit| limit as usize)
                .filter(|limit| *limit > 0)
                .unwrap_or_else(|| state.model_page_size.load(Ordering::SeqCst));
            let end = (offset + limit).min(models.len());
            let next = (end < models.len()).then(|| json!(end.to_string()));
            replying(
                sink,
                id,
                json!({ "data": models[offset..end], "nextCursor": next }),
            )
            .await
        }
        Some("skills/list") => {
            let cwd = params
                .get("cwds")
                .and_then(Value::as_array)
                .and_then(|cwds| cwds.first())
                .and_then(Value::as_str)
                .unwrap_or_default();
            let skills = state.skills.lock().expect("skills").clone();
            replying(
                sink,
                id,
                json!({ "data": [{ "cwd": cwd, "skills": skills, "errors": [] }] }),
            )
            .await
        }
        Some("thread/resume") => {
            state.resume_calls.fetch_add(1, Ordering::SeqCst);
            state
                .resume_params
                .lock()
                .expect("resume params")
                .push(params.clone());
            if state.silent_resume.swap(false, Ordering::SeqCst) {
                return true;
            }
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            let Some(thread) = find_thread(state, thread_id) else {
                return reply_error(sink, id, -32600, &format!("thread not found: {thread_id}")).await;
            };
            if !thread.has_history {
                // The spike's exact shape (ADR-0090 amendment, question 2).
                return reply_error(
                    sink,
                    id,
                    -32600,
                    &format!("no rollout found for thread id {thread_id}"),
                )
                .await;
            }
            {
                let mut subscribers = state.subscribers.lock().expect("subscribers");
                let streams = subscribers.entry(thread_id.to_string()).or_default();
                // Resuming twice on one connection is the same subscription,
                // not a second stream (the spike re-resumed successfully).
                if !streams.iter().any(|(id, _)| *id == connection_id) {
                    streams.push((connection_id, outgoing.clone()));
                }
            }
            let result = json!({
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "cwd": thread.cwd,
                "model": "gpt-5",
                "modelProvider": "openai",
                "sandbox": { "type": "readOnly", "networkAccess": false },
                "thread": thread_json(&thread),
            });
            reply(sink, id, result).await
        }
        Some("thread/unsubscribe") => {
            state.unsubscribe_calls.fetch_add(1, Ordering::SeqCst);
            if state.reject_unsubscribe.load(Ordering::SeqCst) > 0 {
                state.reject_unsubscribe.fetch_sub(1, Ordering::SeqCst);
                return reply_error(sink, id, -32600, "thread unsubscribe failed").await;
            }
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            let status = {
                let mut subscribers = state.subscribers.lock().expect("subscribers");
                let streams = subscribers.entry(thread_id.to_string()).or_default();
                let before = streams.len();
                // Only this connection leaves; another subscriber's stream is
                // untouched (the spike's question 2).
                streams.retain(|(id, _)| *id != connection_id);
                if streams.len() < before {
                    "unsubscribed"
                } else {
                    "notSubscribed"
                }
            };
            reply(sink, id, json!({ "status": status })).await
        }
        Some("thread/name/set") => {
            let thread_id = params.get("threadId").and_then(Value::as_str).unwrap_or("");
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let renamed = {
                let mut threads = state.threads.lock().expect("scripted threads");
                match threads.iter_mut().find(|thread| thread.id == thread_id) {
                    Some(thread) => {
                        thread.name = Some(name.to_string());
                        true
                    }
                    None => false,
                }
            };
            if renamed {
                reply(sink, id, json!({})).await
            } else {
                reply_error(sink, id, -32600, &format!("thread not found: {thread_id}")).await
            }
        }
        _ => true,
    }
}

/// Record one client answer to a server-initiated request and broadcast the
/// resolution, modelling the real server's peer behaviour: whichever client
/// answers, every subscriber sees `serverRequest/resolved`.
fn handle_server_response(state: &State, connection_id: usize, value: &Value) {
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let result = value.get("result").cloned();
    let error = value.get("error").cloned();
    state
        .server_request_replies
        .lock()
        .expect("server request replies")
        .push(ServerRequestReply {
            connection: connection_id,
            id: id.clone(),
            result,
            error,
        });

    let request = state
        .server_requests
        .lock()
        .expect("server requests")
        .iter()
        .find(|request| request.id == id)
        .cloned();
    let Some(request) = request else {
        // A response to something this server never asked (a client bug, or a
        // second answer after the first resolved it). Recorded above; nothing
        // to broadcast.
        return;
    };
    match request.thread_id {
        Some(thread_id) => emit(
            state,
            &thread_id,
            "serverRequest/resolved",
            json!({ "threadId": thread_id, "requestId": id }),
        ),
        None => {
            let message = Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "serverRequest/resolved",
                    "params": { "requestId": id },
                })
                .to_string()
                .into(),
            );
            state
                .live
                .lock()
                .expect("live connections")
                .retain(|(_, connection)| connection.send(message.clone()).is_ok());
        }
    }
}

/// Send one notification to a thread's subscribers only (the `turn/*` and
/// `item/*` shape). Never to the whole server: a broadcast would also reach a
/// connection that never subscribed.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn emit(state: &State, thread_id: &str, method: &str, params: Value) {
    record_event(state, thread_id, method, &params);
    let message = Message::Text(
        json!({ "jsonrpc": "2.0", "method": method, "params": params })
            .to_string()
            .into(),
    );
    if let Some(streams) = state.subscribers.lock().expect("subscribers").get_mut(thread_id) {
        streams.retain(|(_, stream)| stream.send(message.clone()).is_ok());
    }
}

/// Fold a server-originated notification into the thread's stored history, as
/// the real daemon persists a rollout. A later `thread/resume` then carries the
/// turn even though this connection emitted its stream before anyone
/// subscribed — and `has_history` flips exactly as the spike observed (a
/// thread with a completed turn is resumable).
fn record_event(state: &State, thread_id: &str, method: &str, params: &Value) {
    let mut threads = state.threads.lock().expect("scripted threads");
    let Some(thread) = threads.iter_mut().find(|thread| thread.id == thread_id) else {
        return;
    };
    match method {
        "turn/started" => {
            let Some(turn) = params.get("turn") else { return };
            let Some(id) = turn.get("id").and_then(Value::as_str) else {
                return;
            };
            entry(thread, id, turn);
        }
        "item/started" | "item/completed" => {
            let (Some(item), Some(turn_id)) =
                (params.get("item"), params.get("turnId").and_then(Value::as_str))
            else {
                return;
            };
            let Some(item_id) = item.get("id").and_then(Value::as_str) else {
                return;
            };
            let Some(recorded) = thread
                .turns
                .iter_mut()
                .find(|turn| turn.get("id").and_then(Value::as_str) == Some(turn_id))
            else {
                return;
            };
            let Some(items) = recorded.get_mut("items").and_then(Value::as_array_mut) else {
                return;
            };
            match items
                .iter_mut()
                .find(|existing| existing.get("id").and_then(Value::as_str) == Some(item_id))
            {
                Some(existing) => *existing = item.clone(),
                None => items.push(item.clone()),
            }
        }
        "turn/completed" => {
            let Some(turn) = params.get("turn") else { return };
            let Some(id) = turn.get("id").and_then(Value::as_str) else {
                return;
            };
            entry(thread, id, turn);
        }
        _ => {}
    }
}

/// Insert or update one stored turn. An existing turn keeps its items (a
/// completion carries only the turn's status and stamps).
fn entry(thread: &mut ScriptedThread, id: &str, turn: &Value) {
    thread.has_history = true;
    if let Some(recorded) = thread
        .turns
        .iter_mut()
        .find(|recorded| recorded.get("id").and_then(Value::as_str) == Some(id))
    {
        recorded["status"] = turn.get("status").cloned().unwrap_or(json!("inProgress"));
        recorded["completedAt"] = turn.get("completedAt").cloned().unwrap_or(Value::Null);
        recorded["error"] = turn.get("error").cloned().unwrap_or(Value::Null);
        return;
    }
    thread.turns.push(json!({
        "id": id,
        "status": turn.get("status").cloned().unwrap_or(json!("inProgress")),
        "startedAt": turn.get("startedAt").cloned().unwrap_or(Value::Null),
        "completedAt": turn.get("completedAt").cloned().unwrap_or(Value::Null),
        "error": turn.get("error").cloned().unwrap_or(Value::Null),
        "items": turn.get("items").cloned().unwrap_or(json!([])),
    }));
}

/// Persist the request's own `input` as a server-generated `userMessage` item:
/// what cola sent, as the server writes it. `opened` also emits the item's
/// `item/started`; a steer emits the completed item alone, so the text appears
/// exactly once in the transcript either way.
fn emit_user_message(state: &State, thread_id: &str, turn_id: &str, params: &Value, opened: bool) {
    let Some(text) = params.get("input").and_then(Value::as_array).and_then(|input| {
        input.iter().find_map(|part| {
            (part.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| part.get("text").and_then(Value::as_str))
                .flatten()
        })
    }) else {
        return;
    };
    let item_id = format!("item-user-{}", state.next_item.fetch_add(1, Ordering::SeqCst));
    let client_id = params.get("clientUserMessageId").cloned().unwrap_or(Value::Null);
    let item = json!({
        "type": "userMessage",
        "id": item_id,
        "clientId": client_id,
        "content": [{ "type": "text", "text": text, "textElements": [] }],
    });
    if opened {
        emit(
            state,
            thread_id,
            "item/started",
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "startedAtMs": now_secs() * 1000,
                "item": item,
            }),
        );
    }
    emit(
        state,
        thread_id,
        "item/completed",
        json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "completedAtMs": now_secs() * 1000 + 1,
            "item": item,
        }),
    );
}

fn find_thread(state: &State, thread_id: &str) -> Option<ScriptedThread> {
    state
        .threads
        .lock()
        .expect("scripted threads")
        .iter()
        .find(|thread| thread.id == thread_id)
        .cloned()
}

/// The page `thread/list` answers: cwd/source filters, an opaque cursor as an
/// offset, and `nextCursor` only while another page remains.
fn list_threads(state: &State, params: &Value) -> Value {
    let cwd = params.get("cwd");
    let sources = params
        .get("sourceKinds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let matching: Vec<ScriptedThread> = state
        .threads
        .lock()
        .expect("scripted threads")
        .iter()
        .filter(|thread| match cwd {
            Some(Value::String(path)) => thread.cwd == *path,
            Some(Value::Array(paths)) => paths
                .iter()
                .filter_map(Value::as_str)
                .any(|path| thread.cwd == path),
            _ => true,
        })
        .filter(|thread| {
            sources.is_empty()
                || sources
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|source| thread.source == source)
        })
        .cloned()
        .collect();

    let offset = params
        .get("cursor")
        .and_then(Value::as_str)
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0)
        .min(matching.len());
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|limit| limit as usize)
        .filter(|limit| *limit > 0)
        .unwrap_or(state.page_size);
    let end = (offset + limit).min(matching.len());
    let data: Vec<Value> = matching[offset..end].iter().map(thread_json).collect();
    let next = (end < matching.len()).then(|| json!(end.to_string()));
    json!({ "data": data, "nextCursor": next })
}

/// One thread in the wire shape `0.163.0` defines. The full required set is
/// present so the client decodes the real contract, not a relaxed fixture.
fn thread_json(thread: &ScriptedThread) -> Value {
    let turns = match thread.read_turn_limit {
        Some(limit) => &thread.turns[..thread.turns.len().min(limit)],
        None => &thread.turns[..],
    };
    json!({
        "id": thread.id,
        "sessionId": thread.session_id,
        "name": thread.name,
        "parentThreadId": thread.parent_thread_id,
        "preview": thread.preview,
        "cwd": thread.cwd,
        "createdAt": thread.created_at,
        "updatedAt": thread.updated_at,
        "cliVersion": "0.163.0",
        "model": thread.model,
        "modelProvider": thread.model_provider,
        "ephemeral": false,
        "projectId": null,
        "source": thread.source,
        "status": { "type": "notLoaded" },
        "turns": turns,
    })
}

async fn send_text<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    text: &str,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    sink.send(Message::Text(text.to_string().into())).await.is_ok()
}

async fn send<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    value: Value,
) -> std::result::Result<(), ()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    sink.send(Message::Text(value.to_string().into()))
        .await
        .map_err(|_| ())
}

async fn replying<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    id: Value,
    result: Value,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    reply(sink, id, result).await
}

async fn reply<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    id: Value,
    result: Value,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send(sink, json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        .await
        .is_ok()
}

async fn reply_error<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    id: Value,
    code: i64,
    message: &str,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send(
        sink,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    )
    .await
    .is_ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The fit check must measure the root **plus** its suffix. `Path::join`
    /// replaces the root when the argument is absolute, so a leading `/` on the
    /// suffix would silently measure 53 bytes and pronounce every root short —
    /// the exact no-op that let a macOS `$TMPDIR` root through. This pins both
    /// sides on every unix, not only where a socket actually fails to bind.
    #[test]
    fn a_fixture_root_is_measured_with_its_socket_suffix() {
        assert!(socket_path_fits(Path::new("/tmp/colaAbCdEf")));
        // A real macOS `$TMPDIR`: `/var/folders/<xx>/<32 hex>/T/` plus ours.
        assert!(!socket_path_fits(Path::new(
            "/var/folders/2x/abcdefghijklmnopqrstuvwxyz012345/T/colaAbCdEf"
        )));
    }

    /// The helper returns a root whose deepest control socket can bind where
    /// `sun_path` is tightest (macOS), and it is a short base rather than the
    /// long `$TMPDIR` one.
    #[test]
    fn the_socket_fixture_root_is_short_enough_to_bind() {
        let root = socket_tempdir();
        assert!(socket_path_fits(root.path()), "{}", root.path().display());
        assert!(
            root.path().join(DEEPEST_FIXTURE_SOCKET).as_os_str().len() < SUN_PATH_MAX,
            "{}",
            root.path().join(DEEPEST_FIXTURE_SOCKET).display()
        );
    }
}
