//! The live Codex contract suites (ticket #953).
//!
//! Two tiers, both `#[ignore]`-gated so the hermetic workspace run stays
//! credential-free and network-free:
//!
//! - **tier-1** ([`live_codex_tier1_scripted_transport`], every platform): the
//!   real pinned `codex` starts `app-server --listen unix://<short path>` in an
//!   isolated `CODEX_HOME`, a scripted Responses provider answers the model
//!   protocol, and cola attaches through `codex app-server proxy`. The suite
//!   asserts a `thread/start`, a completed turn carrying the scripted text, an
//!   approval asked on the first connection and answered from a second one
//!   subscribed with `thread/resume`, and a `turn/interrupt` that ends the
//!   turn as `interrupted`.
//! - **tier-2** ([`live_codex_tier2_managed_daemon`], Ubuntu and macOS): the
//!   real managed daemon started by Codex itself — discovery through the
//!   control socket's symlink, the `daemon version` report, `start_server =
//!   "never"` refusing a start, `auto` starting one, and the restart refusal.
//!
//! Run one tier with the pinned binary:
//!
//! ```text
//! COLA_LIVE_CODEX_BIN=/path/to/codex cargo test --locked -- --ignored live_codex_tier1
//! COLA_LIVE_CODEX_BIN=/path/to/codex cargo test --locked -- --ignored live_codex_tier2
//! ```
//!
//! `codex` on `PATH` is the fallback. Every child runs under a fresh temp
//! `CODEX_HOME` whose path fits the platform's `sun_path` limit, and each test
//! asserts its process, listener, proxy children and temp home are gone before
//! it returns. The user's own `~/.codex` is never read or written.

mod provider;
mod server;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::time::sleep;

use crate::backend::Backend;
#[cfg(unix)]
use crate::backend::lifecycle::{AttachOutcome, EnsureAttached, RestartOutcome, ServerLifecycle};
use crate::codex::client::{CodexClient, CodexOptions};
#[cfg(unix)]
use crate::codex::discovery::{self, DaemonState};
use crate::codex::schema::PinnedProbe;
use crate::codex::threads::{Adoption, ThreadModel, ThreadSettings};
use crate::codex::transport::Inbound;
#[cfg(unix)]
use crate::config::ServerStartPolicy;
use crate::config::{ApprovalPolicy, SandboxMode};

use provider::{
    APPROVAL_COMMAND, APPROVAL_PROMPT, APPROVAL_TEXT, INTERRUPT_PROMPT, ScriptedProvider, TEXT, TEXT_PROMPT,
};
use server::LiveAppServer;

/// Serialise the live tiers inside one test binary. tier-2 snapshots Codex's
/// shared `/tmp/codex-daemon-*` runtime directory to remove only the entries
/// its own daemon created; a concurrent tier-1 listener would look like one of
/// those entries. CI runs each tier in its own job, so this only affects a
/// local `cargo test -- --ignored live_codex`.
static LIVE_SUITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The binary the suite spawns: the env override first, `codex` on `PATH`
/// otherwise (mirroring the OpenCode live harness).
fn codex_binary() -> PathBuf {
    std::env::var_os("COLA_LIVE_CODEX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"))
}

/// The thread settings tier-1 pins: a read-only sandbox plus `untrusted`
/// approval, which turns the scripted `exec_command` into a real approval
/// request instead of silently running it.
fn live_settings() -> ThreadSettings {
    ThreadSettings {
        sandbox: Some(SandboxMode::ReadOnly),
        approval_policy: Some(ApprovalPolicy::OnRequest),
        apply_on_adopt: false,
        reasoning_summary: None,
    }
}

fn options(endpoint: crate::codex::transport::Endpoint, binary: &std::path::Path) -> CodexOptions {
    CodexOptions::new(endpoint)
        .with_proxy_binary(binary.to_path_buf())
        .with_thread_settings(live_settings())
}

async fn connect(endpoint: crate::codex::transport::Endpoint, binary: &std::path::Path) -> CodexClient {
    CodexClient::connect(&options(endpoint, binary), &PinnedProbe)
        .await
        .expect("attach cola through the codex proxy")
}

async fn start_turn(client: &CodexClient, thread_id: &str, text: &str) -> String {
    let result = client
        .connection()
        .request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": text, "text_elements": [] }],
            }),
        )
        .await
        .expect("turn/start");
    result["turn"]["id"]
        .as_str()
        .expect("turn/start names the turn")
        .to_string()
}

/// Wait for the exact turn's completion on this connection's inbound stream.
async fn wait_for_turn(events: &mut broadcast::Receiver<Inbound>, turn_id: &str) -> (Value, Vec<Value>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut items = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "turn {turn_id} did not complete within 30s");
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(Inbound::Notification { method, params, .. }))
                if method == "turn/completed" && params["turn"]["id"].as_str() == Some(turn_id) =>
            {
                return (params, items);
            }
            Ok(Ok(Inbound::Notification { method, params, .. }))
                if method == "item/completed" && params["turnId"].as_str() == Some(turn_id) =>
            {
                items.push(params["item"].clone());
            }
            Ok(Ok(Inbound::Notification { .. })) => {}
            Ok(Ok(Inbound::Closed)) => panic!("the connection closed before turn {turn_id} completed"),
            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                eprintln!("live tier-1 inbound stream lagged by {skipped}");
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                panic!("the inbound stream closed before turn {turn_id} completed")
            }
            Err(_) => panic!("turn {turn_id} did not complete within 30s"),
        }
    }
}

/// Wait for one pending permission on this connection.
async fn await_permission(client: &CodexClient) -> crate::backend::types::PermissionRequest {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let permissions = client
            .list_permissions(None)
            .await
            .expect("list pending permissions");
        if let Some(permission) = permissions.into_iter().next() {
            return permission;
        }
        assert!(
            Instant::now() < deadline,
            "no approval request reached this client within 30s"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// Subscribe a second client, waiting out the rollout persistence that
/// `thread/resume` requires after the first turn (the spike's `NoHistoryYet`
/// answer can lag `turn/completed` by a scheduling tick).
async fn adopt_with_history(
    client: &CodexClient,
    thread_id: &str,
) -> crate::codex::threads::ThreadSubscription {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match client
            .threads()
            .adopt(thread_id)
            .await
            .expect("thread/resume on the second client")
        {
            Adoption::Subscribed(subscription) => return *subscription,
            Adoption::NoHistoryYet if Instant::now() < deadline => {
                sleep(Duration::from_millis(50)).await;
            }
            Adoption::NoHistoryYet => {
                panic!("the thread never gained the rollout thread/resume needs")
            }
        }
    }
}

async fn wait_until_no_permissions(client: &CodexClient) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let permissions = client
            .list_permissions(None)
            .await
            .expect("list pending permissions");
        if permissions.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the answered approval stayed pending: {permissions:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_until_dead(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while server::process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the codex proxy child {pid} outlived the test"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

async fn assert_path_gone(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "the live Codex temp path {} was left behind",
            path.display()
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// tier-1: the real pinned binary's listener, the scripted model protocol and
/// cola's proxy transport, on every platform.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live codex: set COLA_LIVE_CODEX_BIN (or put codex on PATH)"]
async fn live_codex_tier1_scripted_transport() {
    let _serial = LIVE_SUITE_LOCK.lock().await;
    let binary = codex_binary();
    let provider = ScriptedProvider::start().await;
    let mut server = LiveAppServer::start(&binary, &provider.base_url()).await;
    let endpoint = server.endpoint();
    let work = server.work().to_path_buf();
    let home = server.home().to_path_buf();
    let socket_path = server.socket().to_path_buf();
    let work_text = work.to_string_lossy().to_string();

    let client_a = connect(endpoint.clone(), &binary).await;
    let client_b = connect(endpoint, &binary).await;
    assert_eq!(client_a.server_info().version.to_string(), "0.166.0");
    assert_eq!(client_a.server_info().surface_counts, (106, 85, 10));

    // One thread, created by the production thread surface.
    let thread = client_a
        .threads()
        .start(Some(&work_text), ThreadModel::default())
        .await
        .expect("thread/start");
    assert!(
        thread.id.starts_with("01"),
        "the server named the thread: {}",
        thread.id
    );

    // A scripted turn: model text -> agentMessage -> turn/completed.
    let mut events = client_a.connection().inbound();
    let turn = start_turn(&client_a, &thread.id, TEXT_PROMPT).await;
    let (completed, _) = wait_for_turn(&mut events, &turn).await;
    assert_eq!(completed["turn"]["status"], "completed");
    assert!(
        completed["turn"]["items"].as_array().is_some_and(|items| items
            .iter()
            .any(|item| item["type"] == "agentMessage" && item["text"] == TEXT)),
        "the scripted text reached turn/completed: {completed}"
    );
    assert!(
        provider
            .requests()
            .iter()
            .any(|request| request.body.contains("cola-live-scripted")),
        "the model call used the isolated config's provider"
    );

    // A second cola client subscribes to the thread, then answers the approval
    // the first turn raises. The subscription needs history, so it is taken
    // after the first turn completed (the spike's `thread/resume` rule).
    let _adoption = adopt_with_history(&client_b, &thread.id).await;

    let mut events = client_a.connection().inbound();
    let turn = start_turn(&client_a, &thread.id, APPROVAL_PROMPT).await;
    let permission_b = await_permission(&client_b).await;
    let permission_a = await_permission(&client_a).await;
    assert_eq!(
        permission_a.permission.as_deref(),
        Some("command"),
        "the approval names the command it gates: {permission_a:?}"
    );
    assert!(
        permission_a.always_supported,
        "the 0.166 response schemas advertise the session decision"
    );
    assert_ne!(
        permission_a.request_id, permission_b.request_id,
        "the neutral id is per connection, never the bare server id"
    );
    client_b
        .reply_permission("", &permission_b.request_id, "once", None)
        .await
        .expect("the second client answers the approval");
    let (completed, items) = wait_for_turn(&mut events, &turn).await;
    assert_eq!(completed["turn"]["status"], "completed");
    assert!(
        completed["turn"]["items"].as_array().is_some_and(|items| items
            .iter()
            .any(|item| item["type"] == "agentMessage" && item["text"] == APPROVAL_TEXT)),
        "the approved turn completed with its closing text: {completed}"
    );
    assert!(
        items.iter().any(|item| item["type"] == "commandExecution"
            && item["command"]
                .as_str()
                .is_some_and(|cmd| cmd.contains(APPROVAL_COMMAND))),
        "the approved command completed on the turn: {items:?}"
    );
    wait_until_no_permissions(&client_a).await;

    // Interrupt a live model call: the provider parks until the test releases
    // it, so the interrupt always lands mid-turn.
    let mut events = client_a.connection().inbound();
    let turn = start_turn(&client_a, &thread.id, INTERRUPT_PROMPT).await;
    tokio::time::timeout(Duration::from_secs(30), provider.blocked())
        .await
        .expect("the scripted model call reached the provider");
    client_a
        .connection()
        .request("turn/interrupt", json!({ "threadId": thread.id, "turnId": turn }))
        .await
        .expect("turn/interrupt");
    provider.release();
    let (completed, _) = wait_for_turn(&mut events, &turn).await;
    assert_eq!(
        completed["turn"]["status"], "interrupted",
        "turn/interrupt ends the turn as interrupted: {completed}"
    );

    // The subscription holds a connection clone; release it before the
    // cleanup proof, or the proxy child would outlive the client handles.
    drop(_adoption);

    // Cleanup is part of the contract: the proxy children, listener and temp
    // home must all be gone before the test returns.
    let proxy_pids: Vec<u32> = [&client_a, &client_b]
        .into_iter()
        .filter_map(|client| client.connection().proxy_pid())
        .collect();
    assert_eq!(proxy_pids.len(), 2, "each client dialled through its own proxy");
    drop(client_b);
    drop(client_a);
    // Closing the listener is what makes each proxy child see EOF and exit;
    // the transport then reaps them as their connections drop.
    server.shutdown().await;
    for pid in proxy_pids {
        wait_until_dead(pid).await;
    }
    drop(server);
    assert_path_gone(&home).await;
    assert_path_gone(&work).await;
    assert_path_gone(&socket_path).await;
}

/// tier-2: the real managed daemon. The `never` refusal is asserted as a
/// behaviour, not a config read: if `start_server = "never"` were ignored, the
/// attach would start a daemon and return `Attached`, and this test fails here
/// before the `auto` leg runs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live codex daemon: set COLA_LIVE_CODEX_BIN (or put codex on PATH)"]
async fn live_codex_tier2_managed_daemon() {
    use crate::codex::lifecycle::{CodexLifecycle, ManagedProbe};

    let _serial = LIVE_SUITE_LOCK.lock().await;
    let binary = codex_binary();
    let daemon = server::LiveDaemon::new(binary);
    let environment = daemon.environment();
    let never_busy = || false;
    let request = || EnsureAttached {
        allow_spawn: true,
        heal_when_busy: false,
        busy: &never_busy,
        boot: false,
    };

    // Nothing starts under `never`, even when the request would allow a spawn
    // (the bridge's demand path). This is the assertion that fails if the
    // policy is ignored.
    let never = CodexLifecycle::new(
        crate::config::CodexConfig {
            start_server: ServerStartPolicy::Never,
            ..crate::config::CodexConfig::default()
        },
        environment.clone(),
        Box::new(ManagedProbe),
    );
    assert_eq!(
        never
            .ensure_attached(request())
            .await
            .expect("a read-only attach pass"),
        AttachOutcome::Serverless,
        "start_server = never must not start a daemon"
    );
    assert!(
        !environment.socket().exists(),
        "start_server = never left a control socket at {}",
        environment.socket().display()
    );

    // `auto` starts the real managed daemon and attaches to it.
    let auto = CodexLifecycle::new(
        crate::config::CodexConfig {
            start_server: ServerStartPolicy::Auto,
            ..crate::config::CodexConfig::default()
        },
        environment.clone(),
        Box::new(ManagedProbe),
    );
    assert_eq!(
        auto.ensure_attached(request())
            .await
            .expect("auto starts the managed daemon"),
        AttachOutcome::Attached
    );

    // Discovery through the control socket: the published path is a symlink
    // into the daemon's short runtime directory on Unix, and the report names
    // the binary the daemon actually runs.
    let discovered = discovery::discover(&environment)
        .await
        .expect("discover the running daemon");
    let DaemonState::Running(found) = &discovered.daemon else {
        panic!("auto started the daemon but discovery reports no running daemon");
    };
    let link = std::fs::symlink_metadata(&found.socket).expect("the control socket path");
    assert!(
        link.file_type().is_symlink(),
        "the control socket is discovered through its symlink: {}",
        found.socket.display()
    );
    assert!(
        found.managed_binary.is_file(),
        "the daemon's own binary is resolved: {}",
        found.managed_binary.display()
    );

    // The management CLI's real report, not only the decoded discovery fields.
    let report = server::daemon_version(&daemon).await;
    assert_eq!(report["status"], "running");
    assert_eq!(report["cliVersion"], "0.166.0");
    assert_eq!(report["appServerVersion"], "0.166.0");
    assert_eq!(report["managedCodexVersion"], "0.166.0");
    assert_eq!(
        report["socketPath"].as_str().map(std::path::Path::new),
        Some(found.socket.as_path())
    );
    let info = found.info.as_ref().expect("discovery kept the version report");
    assert_eq!(info.app_server_version.as_deref(), Some("0.166.0"));
    assert_eq!(info.cli_version.as_deref(), Some("0.166.0"));

    // A second attach is the cached steady state, not a new daemon.
    assert_eq!(
        auto.ensure_attached(request())
            .await
            .expect("the cached attachment answers"),
        AttachOutcome::Attached
    );

    // Restarting is never cola's call.
    assert_eq!(
        auto.restart_server().await.expect("restart policy"),
        RestartOutcome::NotOwned
    );

    let home = daemon.home().to_path_buf();
    daemon.shutdown().await;
    assert!(!environment.socket().exists());
    drop(daemon);
    assert_path_gone(&home).await;
}
