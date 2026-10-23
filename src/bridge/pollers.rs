use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::backend::lifecycle::{AttachOutcome, EnsureAttached};
use crate::bridge::handler::CardActionResult;
use crate::bridge::handles::{CardsHandle, FlowHandles, SessionsHandle, WaitsHandle};
use crate::bridge::request::flow::SentCard;
use crate::bridge::turn::Turn;

/// How often the server-reconcile loop rescans for a changed or newly-appeared
/// server (the same cadence the old reconnect loop used).
const RECONNECT_POLL_INTERVAL_SECS: u64 = 5;

/// Whether any session has a prompt in flight (a turn is streaming), or an
/// out-of-turn follow still rendering it (ADR-0059: the follow inherits the
/// inflight guard for its window). The yield is deferred while busy so killing
/// an Owned Server never truncates a mid-stream generation (ADR-0013) — and
/// since the follow holds the guard, a followed Session is never read as idle
/// here.
///
/// This is the one bridge fact the client's lifecycle needs: the client owns
/// every other part of the decision (spec #860, ticket #875).
async fn busy(waits: &WaitsHandle) -> bool {
    !waits.inflight.lock().await.is_empty()
}

/// One pass of the client-owned "ensure attached" entry point.
///
/// The whole discover / attach / health / optionally-start policy lives behind
/// [`crate::backend::ServerLifecycle::ensure_attached`]; the bridge supplies
/// only the three facts the client cannot know (is a message waiting, may a
/// poll heal a dead server, is a turn streaming) and maps the outcome to
/// attached/serverless, or to the one failure signal the latch reports.
///
/// Returns whether a server is attached (or was just started).
async fn reconcile(
    handles: &FlowHandles,
    allow_spawn: bool,
    heal_when_busy: bool,
) -> crate::error::Result<bool> {
    let request = EnsureAttached {
        allow_spawn,
        heal_when_busy,
        busy: busy(&handles.waits).await,
        boot: false,
    };
    match handles.backend.ensure_attached(request).await? {
        AttachOutcome::Attached => Ok(true),
        AttachOutcome::Serverless => Ok(false),
        AttachOutcome::Inconclusive { url, evidence } => Err(crate::error::BridgeError::OpenCode(format!(
            "generation probe inconclusive at {url}; staying serverless: {evidence}"
        ))),
    }
}

/// Independent poller: the OpenCode server cola attaches to is managed by
/// another tool like OpenChamber, which can restart it (new pid/port/password)
/// or raise a Coexistent Server after cola started its own. Reconcile every few
/// seconds so cola re-attaches instead of 502ing against a dead port forever,
/// and yields its Owned Server when a Coexistent one appears. Does not spawn
/// for idle sessions — Lazy Start is the demand path — but heals a server that
/// died while a turn was in flight (`heal_when_busy`), so a mid-stream
/// generation's next poll finds a live server.
///
/// The [`PollLoop`] seam owns the cadence and the failure policy: a failing
/// reconcile WARNs once per distinct error instead of on every tick, identical
/// repeats are DEBUG, and a pass that recovers logs INFO (ADR-0048). The loop
/// has no serverless guard — a server must be able to APPEAR (attach) as well
/// as disappear, so every tick reconciles.
///
/// [`PollLoop`]: crate::bridge::poll::PollLoop
pub(crate) async fn reconnect_poll_loop(handles: &FlowHandles) -> crate::error::Result<()> {
    let cadence_ms = std::sync::atomic::AtomicU64::new(RECONNECT_POLL_INTERVAL_SECS * 1_000);
    let mut poll = reconnect_loop(&cadence_ms);
    poll.poll(
        || true,
        move || async move {
            reconcile(handles, false, true)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        },
    )
    .await
}

/// The reconnect loop's [`PollLoop`]: the production condition name, wired to
/// the injected cadence. Split out so the loop's failure policy is testable at
/// the seam — a reconcile pass is client-owned, so a fake client that reports
/// "attached, nothing to do" drives the loop without any adapter symbols.
///
/// [`PollLoop`]: crate::bridge::poll::PollLoop
fn reconnect_loop(cadence_ms: &std::sync::atomic::AtomicU64) -> crate::bridge::poll::PollLoop<'_> {
    crate::bridge::poll::PollLoop::new(cadence_ms, "server reconcile")
}

/// Lazy Start hook (ADR-0013): called at the moment a message needs a server.
/// The active client's lifecycle attaches if a server exists, starts one when
/// its policy allows, and reports serverless for attach-only. Returns whether a
/// server is ready.
pub(crate) async fn ensure_server(handles: &FlowHandles) -> crate::error::Result<bool> {
    reconcile(handles, true, true).await
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
    backend: &Arc<dyn crate::backend::Backend>,
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
        let info = backend
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
    sessions: &SessionsHandle,
    platform: &Arc<dyn crate::feishu::Platform>,
    thread_key: &crate::config::ThreadKey,
) -> Option<String> {
    if thread_key.thread_id == thread_key.chat_id {
        return None; // not a topic
    }
    // 1. Persisted anchor.
    let anchor = sessions
        .active_entry(thread_key)
        .await
        .and_then(|e| e.topic_anchor.clone());
    if let Some(a) = anchor {
        return Some(a);
    }
    // 2. Newest bot message inside the thread.
    let msgs = platform
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
    sessions: &SessionsHandle,
    cards: &CardsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    platform: &Arc<dyn crate::feishu::Platform>,
    session_id: &str,
    directory: &str,
) -> Option<CardTarget> {
    walk_parent_chain(backend, session_id, Some(directory), |current| {
        let current = current.to_string();
        async move {
            // In-flight prompt for this session → reply to its streaming card.
            if let Some(msg_id) = Turn::reply_target(cards, &current).await {
                return Some(CardTarget::ReplyTo(msg_id));
            }
            // Session mapped to a chat. A topic-backed session must be reached by
            // replying to a message INSIDE the topic (the create API rejects
            // `receive_id_type=thread_id`). Resolve an in-topic anchor — the
            // persisted `/topic` confirmation card, or the newest bot message in
            // the thread (covers sessions created before the anchor existed).
            // Non-topic sessions, or topic sessions with no reachable anchor, fall
            // back to sending into the chat top level.
            let entry = sessions.entry_for_session(&current).await;
            if let Some(entry) = entry {
                let anchor = if entry.thread_key.thread_id != entry.thread_key.chat_id {
                    resolve_topic_anchor(sessions, platform, &entry.thread_key).await
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
    cards: &CardsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    directory: Option<&str>,
) -> Option<String> {
    walk_parent_chain(backend, session_id, directory, |current| {
        let current = current.to_string();
        async move {
            if Turn::has_card(cards, &current).await {
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
        card: Some(crate::feishu::card::shell::card_shell(
            title,
            template,
            vec![serde_json::json!({ "tag": "markdown", "content": body })],
        )),
        toast: None,
    }
}

/// Whether the run that owned a request is provably over: a successful,
/// recognised `session_status` read that is not live (ADR-0059). Any failed or
/// unrecognised read answers `false` — unknown is never read as an
/// interruption, exactly as a failed list is never read as resolved (#130).
/// Bounded like every sweep read: a half-open connection must not stall the
/// pass.
pub(crate) async fn run_is_over(
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    directory: Option<&str>,
    timeout_ms: u64,
) -> bool {
    let read = crate::bridge::bounded_call(
        &format!("session status ({session_id})"),
        timeout_ms,
        backend.session_status(session_id, directory),
    )
    .await;
    match read {
        Some(Ok(Some(status))) => !status.is_live(),
        // Present with an unrecognised type: never guessed.
        Some(Ok(None)) => false,
        Some(Err(e)) => {
            tracing::debug!("session status ({session_id}): {e}; keeping the neutral receipt");
            false
        }
        None => {
            tracing::debug!("session status ({session_id}) timed out; keeping the neutral receipt");
            false
        }
    }
}

/// Mark permission/question cards as "already handled" when the underlying
/// request disappeared without cola answering it. The copy is classified by
/// the owning session's run state (the sweep's `dead` set): a still-live run
/// was resolved by another client (`✅ 已处理` / "已在其他端处理"), a run that is
/// over left the request unhandled (`⏱ 已随会话中断` / "未处理") — never
/// claiming a decision nobody made. An empty owning session (a record written
/// before the id was persisted) is UNKNOWN and keeps the neutral copy. The
/// stale card keeps the original request description so the user can see what
/// it was about. The caller passes the stale list it already classified; this
/// pass only repaints and forgets the surfaces. Shared by the permission and
/// question flows.
pub(crate) async fn mark_stale_cards(
    platform: &Arc<dyn crate::feishu::Platform>,
    stale: Vec<(String, SentCard)>,
    sent: &Arc<Mutex<HashMap<String, SentCard>>>,
    surfaces: &crate::bridge::surfaces::Surfaces,
    kind: &str,
    dead: &std::collections::HashSet<String>,
) {
    for (rid, card) in stale {
        sent.lock().await.remove(&rid);
        surfaces.remove_standalone(&rid);
        let over = !card.session_id.is_empty() && dead.contains(&card.session_id);
        let card_json = if over {
            crate::feishu::card::notify::build_interrupted_card(kind, &card.summary)
        } else {
            crate::feishu::card::notify::build_resolved_elsewhere_card(kind, &card.summary)
        };
        if let Err(e) = platform.update_message(&card.message_id, &card_json).await {
            tracing::warn!("mark stale {} card {}: {}", kind, rid, e);
        } else if over {
            tracing::info!("Marked stale {} card {} as interrupted", kind, rid);
        } else {
            tracing::info!("Marked stale {} card {} as handled", kind, rid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::core::SharedCore;
    use crate::bridge::test_support::{MockBackend, RecordingPlatform, test_config};
    use std::sync::Arc;

    /// The poll loop's seam, driven by a client with no lifecycle of its own:
    /// it reports "attached, nothing to do", so the pass is a clean success
    /// and this test names no adapter symbol (spec #860, ticket #875).
    #[tokio::test]
    async fn a_fake_client_that_is_attached_drives_the_poll_loop() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let layout = crate::state::StateLayout::from_config(&cfg);
        let platform = Arc::new(RecordingPlatform::new());
        let core = Arc::new(
            SharedCore::new(&cfg, layout, Arc::new(MockBackend::new(Vec::new())), platform).unwrap(),
        );

        assert!(
            reconcile(&core.flow_handles(), false, true).await.unwrap(),
            "a client that reports attached leaves the pass successful"
        );
    }

    /// The reconnect loop's failure policy (ADR-0048): a failing reconcile
    /// WARNs once with the cause, the identical repeat is DEBUG at the
    /// production 5 s cadence (injected as 1 ms here) — not a WARN every tick.
    /// Driven at the loop's own seam, with a pass closure standing in for the
    /// client-owned reconcile.
    #[tokio::test]
    async fn the_reconnect_loop_latches_its_failures() {
        let cadence = std::sync::atomic::AtomicU64::new(1);
        let mut poll = reconnect_loop(&cadence);
        let (_, logs) = crate::bridge::test_support::capture_logs(async {
            let mut pass = || async { Err("connection refused".to_string()) };
            poll.tick(|| true, &mut pass).await;
            poll.tick(|| true, &mut pass).await;
        })
        .await;

        use crate::bridge::test_support::level_count;
        assert_eq!(
            level_count(&logs, "server reconcile failed", "WARN"),
            1,
            "one warning, not one per tick:\n{logs}"
        );
        assert_eq!(
            level_count(&logs, "server reconcile still failing", "DEBUG"),
            1,
            "the identical repeat is DEBUG:\n{logs}"
        );
        assert!(
            logs.contains("connection refused"),
            "the warning carries the cause:\n{logs}"
        );
        assert!(!logs.contains("scope ;"), "no empty scope segment:\n{logs}");
    }

    /// A shared core whose `session_info` serves the given parent map, so the
    /// walker hops over scripted parent chains.
    async fn core_with_parents(parents: Vec<(String, String)>) -> Arc<SharedCore> {
        let mut backend = MockBackend::new(Vec::new());
        for (child, parent) in parents {
            backend.with_session_parent(&child, &parent);
        }
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&dir.path().join("sessions.json"));
        let layout = crate::state::StateLayout::from_config(&cfg);
        let platform = Arc::new(RecordingPlatform::new());
        Arc::new(SharedCore::new(&cfg, layout, Arc::new(backend), platform).unwrap())
    }

    #[tokio::test]
    async fn walker_matches_the_starting_session_without_a_hop() {
        let core = core_with_parents(vec![]).await;
        // Predicate matches the start id directly — no session_info call.
        let found = walk_parent_chain(&core.client, "s0", Some("/w"), |current| {
            let current = current.to_string();
            async move { (current == "s0").then_some(current) }
        })
        .await;
        assert_eq!(found.as_deref(), Some("s0"));
    }

    #[tokio::test]
    async fn walker_follows_the_chain_until_the_predicate_matches() {
        let core = core_with_parents(vec![("child".into(), "parent".into())]).await;
        let found = walk_parent_chain(&core.client, "child", Some("/w"), |current| {
            let current = current.to_string();
            async move { (current == "parent").then_some(current) }
        })
        .await;
        assert_eq!(found.as_deref(), Some("parent"));
    }

    #[tokio::test]
    async fn walker_stops_at_a_self_parent_loop() {
        let core = core_with_parents(vec![("a".into(), "a".into())]).await;
        let found = walk_parent_chain(&core.client, "a", Some("/w"), |_| async move { None::<String> }).await;
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
        let found =
            walk_parent_chain(&core.client, "s0", Some("/w"), |_| async move { None::<String> }).await;
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn walker_without_a_directory_checks_only_the_start_session() {
        // No directory handle → the parent chain can't be walked (ADR-0010).
        let core = core_with_parents(vec![("child".into(), "parent".into())]).await;
        let found = walk_parent_chain(&core.client, "child", None, |current| {
            let current = current.to_string();
            async move { (current == "parent").then_some(current) }
        })
        .await;
        assert_eq!(found, None);
    }
}
