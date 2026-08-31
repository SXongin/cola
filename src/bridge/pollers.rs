use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::core::SharedCore;
use crate::bridge::discovery::{self, ServerCandidate};
use crate::bridge::handler::CardActionResult;
use crate::config::ServerStartPolicy;

/// How often the server-reconcile loop rescans for a changed or newly-appeared
/// server (the same cadence the old reconnect loop used).
const RECONNECT_POLL_INTERVAL_SECS: u64 = 5;

/// Whether any session has a prompt in flight (a turn is streaming). The yield
/// is deferred while busy so killing an Owned Server never truncates a
/// mid-stream generation (ADR-0013).
async fn busy(core: &SharedCore) -> bool {
    !core.inflight.lock().await.is_empty()
}

/// Whether the server cola is currently attached to is its Owned Server
/// (pid matches `self_pid`). None/false when serverless or attached to a
/// Coexistent Server.
fn current_is_owned(candidates: &[ServerCandidate], self_pid: Option<i32>, current: &str) -> bool {
    let Some(self_pid) = self_pid else { return false };
    let Some(port) = current.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) else {
        return false;
    };
    candidates.iter().any(|c| c.port == port && c.pid == self_pid)
}

/// Terminate cola's Owned Server (verified by pid against the live candidates)
/// and forget it, so the Shared Store returns to one server once cola attached
/// to a Coexistent one. The yield (ADR-0013). The pid file is cleared only
/// after the process is confirmed dead (SIGTERM, escalating to SIGKILL) — a
/// lingering process cola lost the record of could never be reaped.
async fn reap_owned_server(candidates: &[ServerCandidate], self_pid: Option<i32>) {
    let Some(self_pid) = self_pid else { return };
    if !candidates.iter().any(|c| c.pid == self_pid) {
        return;
    }
    tracing::info!("yielding: terminating own OpenCode server (pid {})", self_pid);
    if let Err(e) = discovery::terminate_process(self_pid) {
        tracing::warn!("yield: failed to terminate Owned Server {}: {}", self_pid, e);
        return;
    }
    if wait_dead(self_pid, 5000).await {
        discovery::clear_self_spawned();
        return;
    }
    tracing::warn!(
        "yield: Owned Server {} did not exit on SIGTERM; sending SIGKILL",
        self_pid
    );
    let _ = discovery::force_kill(self_pid);
    if wait_dead(self_pid, 5000).await {
        discovery::clear_self_spawned();
    } else {
        tracing::warn!(
            "yield: Owned Server {} did not exit after SIGKILL; keeping its pid record",
            self_pid
        );
    }
}

/// Poll until a process is dead (not alive / not a zombie), within `timeout_ms`.
async fn wait_dead(pid: i32, timeout_ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
    loop {
        if !discovery::process_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// One pass of the server-ownership reconcile (ADR-0013): attach to the
/// preferred default-store server (a Coexistent Server over cola's Owned),
/// lazily spawn an Owned Server when `allow_spawn` and none exists, and reap a
/// leftover Owned Server once a Coexistent Server is the pick. The reconnect
/// loop runs this with `allow_spawn=false` on a cadence; the Lazy Start hook
/// runs it with `allow_spawn=true` at the moment a server is needed.
///
/// Returns whether a server is attached (or was just started).
async fn reconcile(core: &Arc<SharedCore>, allow_spawn: bool) -> crate::error::Result<bool> {
    let candidates = discovery::scan_processes();
    let self_pid = discovery::self_spawned_pid();
    let Some(server) = discovery::pick_server(&candidates, core.preferred_port, self_pid) else {
        // No default-store server at all. Lazy Start: spawn an Owned Server on
        // demand; otherwise stay serverless (the caller replies that OpenCode
        // is unavailable).
        if allow_spawn && core.server_start.spawns_when_needed() {
            let spawned = discovery::spawn_own_server(core.preferred_port)
                .await
                .map_err(|e| crate::error::BridgeError::OpenCode(format!("lazy start failed: {e}")))?;
            core.opencode.reconnect(&spawned.url, &spawned.password).await?;
            tracing::info!("lazily started own OpenCode server at {}", spawned.url);
            return Ok(true);
        }
        // The attached server, if any, is gone — go serverless so a later
        // message re-attaches or lazily spawns instead of 502ing against the
        // dead endpoint forever.
        if !core.opencode.base_url().is_empty() {
            tracing::warn!("attached OpenCode server is gone; going serverless");
            core.opencode.reconnect("", "").await?;
        }
        return Ok(false);
    };
    let url = format!("http://localhost:{}", server.port);
    let current = core.opencode.base_url();
    let pick_is_owned = self_pid == Some(server.pid);

    if url != current {
        let current_is_owned = current_is_owned(&candidates, self_pid, &current);
        if current_is_owned && !pick_is_owned && busy(core).await {
            // Defer the yield: keep streaming on our Owned Server until idle,
            // so the in-flight generation isn't truncated.
            return Ok(true);
        }
        tracing::warn!("OpenCode server changed ({} -> {}); reconnecting", current, url);
        core.opencode.reconnect(&url, &server.password).await?;
    }

    // Attached to the preferred server. If it's a Coexistent Server, reap a
    // leftover Owned Server (the yield) — but never while a turn is in flight.
    if !pick_is_owned {
        if busy(core).await {
            return Ok(true);
        }
        reap_owned_server(&candidates, self_pid).await;
    }
    Ok(true)
}

/// Independent poller: the OpenCode server cola attaches to is managed by
/// another tool like OpenChamber, which can restart it (new pid/port/password)
/// or raise a Coexistent Server after cola started its own. Reconcile every few
/// seconds so cola re-attaches instead of 502ing against a dead port forever,
/// and yields its Owned Server when a Coexistent one appears. Never spawns —
/// Lazy Start is the demand path.
pub(crate) async fn reconnect_poll_loop(core: &Arc<SharedCore>) -> crate::error::Result<()> {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(RECONNECT_POLL_INTERVAL_SECS)).await;
        let _guard = core.server_lock.lock().await;
        if let Err(e) = reconcile(core, false).await {
            tracing::warn!("server reconcile failed: {}", e);
        }
    }
}

/// Lazy Start hook (ADR-0013): called at the moment a message needs a server.
/// Attaches if a default-store server exists, spawns an Owned Server if none
/// does and the policy allows (`auto`/`eager`), and reports false when
/// attach-only (`never`) and no server exists. No-op for test mocks (they have
/// no process to spawn). Returns whether a server is ready.
pub(crate) async fn ensure_server(core: &Arc<SharedCore>) -> crate::error::Result<bool> {
    if !core.opencode.can_self_start_server() {
        return Ok(true);
    }
    if core.server_start == ServerStartPolicy::Never {
        // Attach-only: a non-empty base_url means we're attached to someone
        // else's server; otherwise stay serverless.
        return Ok(!core.opencode.base_url().is_empty());
    }
    // Already attached — the reconcile loop re-points us within a few seconds
    // if that server dies or a Coexistent one appears, so don't rescan the
    // process table on every message.
    if !core.opencode.base_url().is_empty() {
        return Ok(true);
    }
    let _guard = core.server_lock.lock().await;
    reconcile(core, true).await
}

/// Where to deliver a permission/question card.
pub enum CardTarget {
    /// Reply to a specific message (an in-flight prompt's streaming card, or a
    /// topic's anchor message).
    ReplyTo(String),
    /// Send into a chat (fallback when no replyable message is known).
    Chat(String),
}

/// Walk up a session's parent chain (sub-task children carry their own id and
/// are not in the SessionStore) until `predicate` returns `Some`, the chain
/// loops back on itself (a session whose parent is itself), or the 8-hop cap is
/// exhausted. The starting session is visited first. `directory` scopes the
/// `session_info` hops (ADR-0010); `None` restricts the walk to the starting
/// session only (the parent chain is not reachable without an instance handle).
///
/// The 8-hop cap and the "stop at self-parent" policy live here, exactly once —
/// every call site (card-target resolution, inline-host resolution, the
/// auto-accept flag walk, the descendant check) shares them.
pub(crate) async fn walk_parent_chain<F, Fut, T>(
    core: &SharedCore,
    start: &str,
    directory: Option<&str>,
    mut predicate: F,
) -> Option<T>
where
    F: FnMut(&str) -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let mut current = start.to_string();
    for _ in 0..8 {
        if let Some(t) = predicate(&current).await {
            return Some(t);
        }
        let dir = directory?;
        let info = core
            .opencode
            .clone()
            .for_directory(dir)
            .session_info(&current)
            .await
            .ok()?;
        let parent = info.parent_id.filter(|p| p != &current)?;
        current = parent;
    }
    None
}

/// Resolve a `message_id` that lives INSIDE the given topic, so a card can
/// reply to it and land inside the topic.
///
/// Order:
/// 1. The session's persisted `topic_anchor` (the `/topic` confirmation card).
/// 2. The newest bot message in the thread (`list_messages` with
///    `container_id_type=thread`) — covers topic sessions created before the
///    anchor field existed, or whose anchor card was deleted.
///
/// Returns `None` when the session has no anchor and the thread query fails or
/// returns nothing usable.
pub(crate) async fn resolve_topic_anchor(
    core: &Arc<SharedCore>,
    thread_key: &crate::config::ThreadKey,
) -> Option<String> {
    if thread_key.thread_id == thread_key.chat_id {
        return None; // not a topic
    }
    // 1. Persisted anchor.
    let anchor = {
        let store = core.sessions.lock().await;
        store.get_active(thread_key).and_then(|e| e.topic_anchor.clone())
    };
    if let Some(a) = anchor {
        return Some(a);
    }
    // 2. Newest bot message inside the thread.
    let msgs = core
        .feishu
        .list_messages("thread", &thread_key.thread_id)
        .await
        .ok()?;
    msgs.iter()
        .find(|m| {
            m.sender
                .as_ref()
                .map(|s| s.sender_type.as_deref() == Some("app"))
                .unwrap_or(false)
        })
        .map(|m| m.message_id.clone())
}

/// Resolve where a card for `session_id` should be delivered.
///
/// Sessions cola created map to a Feishu chat directly. Sub-task sessions
/// (created by the `task` tool) are NOT in cola's SessionStore, so when the
/// direct lookup fails we walk up the parent chain (via `session_info`) until
/// a session cola knows is found. The directory is passed through so the
/// parent session is looked up in the same server instance.
pub(crate) async fn resolve_card_target(
    core: &Arc<SharedCore>,
    session_id: &str,
    directory: &str,
) -> Option<CardTarget> {
    walk_parent_chain(core, session_id, Some(directory), |current| {
        let current = current.to_string();
        async move {
            // In-flight prompt for this session → reply to its streaming card.
            let reply_to = {
                let cards = core.cards.lock().await;
                cards
                    .get(&current)
                    .and_then(|c| c.acc.reply_to_message_id.clone())
            };
            if let Some(msg_id) = reply_to {
                return Some(CardTarget::ReplyTo(msg_id));
            }
            // Session mapped to a chat. A topic-backed session must be reached by
            // replying to a message INSIDE the topic (the create API rejects
            // `receive_id_type=thread_id`). Resolve an in-topic anchor — the
            // persisted `/topic` confirmation card, or the newest bot message in
            // the thread (covers sessions created before the anchor existed).
            // Non-topic sessions, or topic sessions with no reachable anchor, fall
            // back to sending into the chat top level.
            let entry = {
                let store = core.sessions.lock().await;
                store.entry_for_session(&current).cloned()
            };
            if let Some(entry) = entry {
                let anchor = if entry.thread_key.thread_id != entry.thread_key.chat_id {
                    resolve_topic_anchor(core, &entry.thread_key).await
                } else {
                    None
                };
                if let Some(anchor) = anchor {
                    return Some(CardTarget::ReplyTo(anchor));
                }
                return Some(CardTarget::Chat(entry.thread_key.chat_id));
            }
            None
        }
    })
    .await
}

/// Find the session whose streaming card an inline permission/question should
/// be shown on: `session_id` itself when it has a live accumulator, else the
/// nearest ancestor that does. Sub-task child sessions carry their own id and
/// have no accumulator of their own, so the parent chain is walked (like
/// `resolve_card_target`); the host's card then carries the child's buttons,
/// and clicking them still replies to the actual (child) session via `directory`.
pub(crate) async fn inline_host_session(
    core: &Arc<SharedCore>,
    session_id: &str,
    directory: Option<&str>,
) -> Option<String> {
    walk_parent_chain(core, session_id, directory, |current| {
        let current = current.to_string();
        async move {
            let cards = core.cards.lock().await;
            if cards.contains_key(&current) {
                Some(current)
            } else {
                None
            }
        }
    })
    .await
}

/// A JSON 2.0 result card for a card-action response (must stay 2.0 to remain
/// update-compatible with the 2.0 interactive cards — Feishu err 200830).
/// Shared by the permission and question flows (and the retry action).
pub(crate) fn result_card(title: &str, template: &str, body: &str) -> CardActionResult {
    CardActionResult {
        card: Some(serde_json::json!({
            "schema": "2.0",
            "config": { "wide_screen_mode": true },
            "header": { "title": { "tag": "plain_text", "content": title }, "template": template },
            "body": { "elements": [ { "tag": "markdown", "content": body } ] }
        })),
        toast: None,
    }
}

/// Mark permission/question cards as "already handled" when the underlying
/// request disappeared without cola answering it (another client resolved it).
/// The stale card keeps the original request description so the user can see
/// what was handled. Shared by the permission and question flows.
pub(crate) async fn mark_stale_cards(
    core: &Arc<SharedCore>,
    pending: &std::collections::HashSet<String>,
    sent: &Arc<Mutex<HashMap<String, (String, String)>>>,
    kind: &str,
) {
    let stale: Vec<(String, String, String)> = {
        let sent_map = sent.lock().await.clone();
        let answered = core.answered_requests.lock().await;
        sent_map
            .into_iter()
            .filter(|(rid, _)| !pending.contains(rid) && !answered.contains(rid))
            .map(|(rid, (mid, desc))| (rid, mid, desc))
            .collect()
    };
    for (rid, mid, desc) in stale {
        sent.lock().await.remove(&rid);
        let card = crate::feishu::card::build_resolved_elsewhere_card(kind, &desc);
        if let Err(e) = core.feishu.update_message(&mid, &card).await {
            tracing::warn!("mark stale {} card {}: {}", kind, rid, e);
        } else {
            tracing::info!("Marked stale {} card {} as handled", kind, rid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::test_support::{MockBackend, RecordingPlatform, test_config};
    use std::sync::Arc;

    fn cand(pid: i32, port: u16) -> ServerCandidate {
        ServerCandidate {
            pid,
            port,
            username: "opencode".into(),
            password: "x".into(),
            uses_default_store: true,
        }
    }

    #[test]
    fn current_is_owned_only_when_pid_and_port_match() {
        let servers = vec![cand(7, 4096)];
        assert!(current_is_owned(&servers, Some(7), "http://localhost:4096"));
        assert!(!current_is_owned(&servers, Some(8), "http://localhost:4096"));
        assert!(!current_is_owned(&servers, None, "http://localhost:4096"));
        assert!(!current_is_owned(&servers, Some(7), "http://localhost:4097"));
        assert!(!current_is_owned(&servers, Some(7), "http://mock"));
        assert!(!current_is_owned(&servers, Some(7), ""));
    }

    /// A shared core whose `session_info` serves the given parent map, so the
    /// walker hops over scripted parent chains.
    async fn core_with_parents(parents: Vec<(String, String)>) -> Arc<SharedCore> {
        let mut backend = MockBackend::new(serde_json::json!([]));
        backend.session_parents = parents.into_iter().collect();
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let platform = Arc::new(RecordingPlatform::new());
        Arc::new(SharedCore::new(&cfg, Arc::new(backend), platform).unwrap())
    }

    #[tokio::test]
    async fn walker_matches_the_starting_session_without_a_hop() {
        let core = core_with_parents(vec![]).await;
        // Predicate matches the start id directly — no session_info call.
        let found = walk_parent_chain(&core, "s0", Some("/w"), |current| {
            let current = current.to_string();
            async move { (current == "s0").then_some(current) }
        })
        .await;
        assert_eq!(found.as_deref(), Some("s0"));
    }

    #[tokio::test]
    async fn walker_follows_the_chain_until_the_predicate_matches() {
        let core = core_with_parents(vec![("child".into(), "parent".into())]).await;
        let found = walk_parent_chain(&core, "child", Some("/w"), |current| {
            let current = current.to_string();
            async move { (current == "parent").then_some(current) }
        })
        .await;
        assert_eq!(found.as_deref(), Some("parent"));
    }

    #[tokio::test]
    async fn walker_stops_at_a_self_parent_loop() {
        let core = core_with_parents(vec![("a".into(), "a".into())]).await;
        let found = walk_parent_chain(&core, "a", Some("/w"), |_| async move { None::<String> }).await;
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn walker_caps_at_eight_hops() {
        // s0 -> s1 -> ... -> s8: the predicate never matches, so the walk hits
        // the 8-hop cap and gives up instead of looping forever.
        let parents: Vec<(String, String)> = (0..8)
            .map(|i| (format!("s{}", i), format!("s{}", i + 1)))
            .collect();
        let core = core_with_parents(parents).await;
        let found = walk_parent_chain(&core, "s0", Some("/w"), |_| async move { None::<String> }).await;
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn walker_without_a_directory_checks_only_the_start_session() {
        // No directory handle → the parent chain can't be walked (ADR-0010).
        let core = core_with_parents(vec![("child".into(), "parent".into())]).await;
        let found = walk_parent_chain(&core, "child", None, |current| {
            let current = current.to_string();
            async move { (current == "parent").then_some(current) }
        })
        .await;
        assert_eq!(found, None);
    }
}
