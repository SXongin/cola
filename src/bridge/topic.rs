//! The topic-opening transaction (ADR-0023, ADR-0028).
//!
//! Opening a topic is a protocol of five steps — pick the session side (a
//! Pending Session for `/topic`, an existing one for `/topic --adopt`), send
//! the Topic Cover Card and anchor the thread on it (falling back to the
//! command/card message when the cover cannot be sent), seed the topic's first
//! in-topic message, write the Session Mapping (or the Pending Session) and
//! record the cover title. This module owns the whole protocol so the callers
//! (`/topic`, `/topic --adopt`, the `/dir` card's 建话题 op and the switch
//! card's 建话题接管 op) only choose a [`TopicOpening`] and render the
//! outcome — the cover-card fallback can never be caller-remembered again.

use std::sync::Arc;

use tracing::Instrument;

use crate::bridge::display::{dir_basename, id_tail, model_display};
use crate::bridge::handles::{CardsHandle, SessionsHandle, TopicHandles};
use crate::bridge::session::PendingEntry;
use crate::config::{SessionEntry, ThreadKey};
use crate::error::BridgeError;
use crate::feishu;
use crate::opencode::types::SessionListInfo;

/// What kind of topic to open: one around a brand-new session (`/topic`, the
/// `/dir` card's 建话题 op) or one around an EXISTING server session
/// (`/topic --adopt`, the switch card's 建话题接管 op, ADR-0016).
pub(crate) enum TopicOpening {
    /// Record a Pending Session in `directory`, PATCH `name` as the title when
    /// given (ADR-0007 creation title policy), and open the topic around it —
    /// no server session is created here (ADR-0041): the topic's first prompt
    /// materialises it. The cover shows `name` or the directory basename until
    /// the server auto-generates a real title.
    Fresh { directory: String, name: Option<String> },
    /// Open the topic around an existing session (ADR-0016): the Session
    /// Snapshot card (ADR-0028) becomes the topic's first in-topic message
    /// and anchor, gathered before the mapping write.
    Adopt { info: SessionListInfo },
}

/// The opened topic: the session it belongs to (if any) and the new Feishu
/// `thread_id`. A fresh `/topic` leaves a Pending Session, so its session id is
/// `None` until the topic's first prompt materialises one (ADR-0041). The
/// thread root and in-topic anchor (ADR-0023) are written to the Session
/// Mapping (or the pending) by the transaction, not returned — no caller needs
/// to re-place them.
pub(crate) struct OpenedTopic {
    pub session_id: Option<String>,
    pub thread_id: String,
}

/// Why a topic could not be opened. `NoThreadId` is the benign platform
/// no-support case — the reply succeeded but carries no `thread_id`, so no
/// topic exists and nothing was mapped; each surface renders its own hint.
/// Everything else is a real backend/platform failure the caller propagates.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenTopicError {
    #[error("platform returned no thread_id; no topic was created")]
    NoThreadId,
    #[error(transparent)]
    Failed(#[from] BridgeError),
}

/// Open a topic — the whole transaction. Fresh: record a Pending Session
/// carrying `directory`, the optional creation title and the topic's
/// root/anchor (ADR-0041; no server session exists yet), build the pending
/// cover brief, send the cover card to the chat's top level and anchor the
/// thread on it, falling back to `fallback_root` (the user's command message
/// or the card's own message) when the cover send fails (ADR-0023); seed the
/// first in-topic message and record the cover title. Adopt: additionally
/// gather the Session Snapshot BEFORE the mapping write (so the card reflects
/// the session's pre-adoption state), map the session to the new topic's
/// `ThreadKey`, and claim its embedded pendings against the in-topic snapshot
/// after the send (ADR-0028).
///
/// The whole transaction runs inside the opening's `topic` span (ADR-0048):
/// the adopted Session — a fresh `/topic` has none yet — plus the Chat it is
/// opened in (and the session's current Topic, when it already has one).
pub(crate) async fn open_topic(
    handles: &TopicHandles,
    chat_id: &str,
    fallback_root: &str,
    opening: TopicOpening,
) -> Result<OpenedTopic, OpenTopicError> {
    let span_session = match &opening {
        TopicOpening::Fresh { .. } => None,
        TopicOpening::Adopt { info } => Some(info.id.as_str()),
    };
    // The Topic being opened does not exist yet, so the span can only carry
    // the Chat — plus whatever Topic the adopted Session is already mapped to.
    let chat_key = ThreadKey::new(chat_id.to_string(), chat_id.to_string());
    let thread_key = match span_session {
        Some(id) => crate::bridge::span::thread_key_of(&handles.flow.sessions, id)
            .await
            .unwrap_or(chat_key),
        None => chat_key,
    };
    let span = crate::bridge::span::topic(span_session, Some(&thread_key));
    open_topic_inner(handles, chat_id, fallback_root, opening)
        .instrument(span)
        .await
}

/// [`open_topic`]'s transaction body, run inside its `topic` span.
async fn open_topic_inner(
    handles: &TopicHandles,
    chat_id: &str,
    fallback_root: &str,
    opening: TopicOpening,
) -> Result<OpenedTopic, OpenTopicError> {
    // Step one: the session side. Fresh records a Pending Session here (so no
    // caller can forget the ADR-0041 rule that nothing is created yet); Adopt
    // gathers the snapshot while the mapping is still unwritten.
    let parts = match opening {
        TopicOpening::Fresh { directory, name } => {
            let display_title = name.clone().unwrap_or_else(|| dir_basename(&directory));
            OpeningParts {
                directory,
                display_title,
                agent: None,
                model: None,
                seed: TopicSeed::ReplyHint,
                side: OpeningSide::Pending { title: name },
                snapshot_claim: None,
            }
        }
        TopicOpening::Adopt { info } => {
            // ADR-0028: every adoption ends in exactly ONE Session Snapshot
            // card. Gathered BEFORE the mapping write below, so the card
            // reflects the session's pre-adoption state; each field is
            // best-effort, so a read failure degrades that field rather than
            // blocking the adoption. The pendings are restricted to the
            // claimable ones (the session's own, not already surfaced).
            let (card, data) = crate::bridge::snapshot::snapshot_card_for(
                &handles.snapshot_handles(),
                "接管",
                &info,
                // The topic form has no switch list to return to.
                None,
            )
            .await;
            OpeningParts {
                directory: info.directory,
                display_title: info.title.clone(),
                agent: info.agent,
                model: model_display(info.model.as_ref()),
                seed: TopicSeed::Snapshot(card),
                side: OpeningSide::Adopt { session_id: info.id },
                snapshot_claim: Some(SnapshotClaim {
                    title: info.title,
                    data,
                }),
            }
        }
    };

    // Step two: the shared cover/anchor/write tail. The fallback ladder lives
    // in `open_cover_topic`, so every surface gets it. The cover differs by
    // side: a pending topic has no session line yet (ADR-0041).
    let cover_text = match &parts.side {
        OpeningSide::Pending { .. } => pending_topic_cover_text(&parts.display_title, &parts.directory).await,
        OpeningSide::Adopt { session_id } => {
            topic_cover_text(
                &parts.display_title,
                &parts.directory,
                session_id,
                parts.agent.as_deref(),
                parts.model.as_deref(),
            )
            .await
        }
    };
    let (anchor, thread_id, topic_root, cover_id) = open_cover_topic(
        &handles.flow.platform,
        chat_id,
        fallback_root,
        &cover_text,
        parts.seed,
    )
    .await?;
    let Some(thread_id) = thread_id else {
        tracing::warn!(
            "topic: no thread_id returned in chat {}; not recording the opening",
            chat_id
        );
        return Err(OpenTopicError::NoThreadId);
    };
    let topic_key = ThreadKey::new(chat_id.to_string(), thread_id.clone());
    let session_id = match parts.side {
        OpeningSide::Pending { title } => {
            // ADR-0041: no backend session yet — the topic's first prompt
            // creates it in this directory, applies the title and moves the
            // overrides onto the SessionEntry.
            let mut pending = PendingEntry::new(topic_key.clone(), parts.directory);
            pending.title = title;
            pending.topic_anchor = Some(anchor.clone());
            pending.topic_root = Some(topic_root);
            handles.flow.sessions.set_pending(pending).await?;
            // The cover record must survive until materialisation; keyed by the
            // pending's thread (it has no session id yet) and flagged pending so
            // the first successful sync re-renders the full brief.
            record_cover_title(
                &handles.flow.cards,
                &pending_cover_key(&topic_key),
                &parts.display_title,
                None,
                cover_id.is_some(),
                true,
            )
            .await;
            None
        }
        OpeningSide::Adopt { session_id } => {
            let mut entry = SessionEntry::new(topic_key, session_id.clone(), parts.directory);
            entry.agent = parts.agent;
            entry.topic_anchor = Some(anchor.clone());
            entry.topic_root = Some(topic_root);
            handles.flow.sessions.activate(entry).await?;
            record_cover_title(
                &handles.flow.cards,
                &session_id,
                &parts.display_title,
                parts.model,
                cover_id.is_some(),
                false,
            )
            .await;
            Some(session_id)
        }
    };
    // ADR-0028: claim the snapshot's embedded pendings against the in-topic
    // snapshot message (the topic's first message + anchor) so the poll loop
    // never duplicates them. Not reached when the topic could not be opened
    // (no thread_id) — the poller keeps today's standalone flow for them.
    if let Some(claim) = parts.snapshot_claim {
        crate::bridge::external::settle_snapshot_after_send(
            &handles.external,
            &handles.flow,
            &anchor,
            "接管",
            &claim.title,
            &claim.data,
            None,
        )
        .await;
    }
    Ok(OpenedTopic {
        session_id,
        thread_id,
    })
}

/// The session-side facts the opening tail needs, assembled by each
/// [`TopicOpening`] arm so the cover/anchor/write sequence itself exists once.
struct OpeningParts {
    directory: String,
    display_title: String,
    agent: Option<String>,
    model: Option<String>,
    seed: TopicSeed,
    side: OpeningSide,
    /// ADR-0028: the adopt snapshot's title and claimable data, settled
    /// against the in-topic seed after the mapping write. `None` for fresh
    /// sessions.
    snapshot_claim: Option<SnapshotClaim>,
}

/// Which of the two ADR-0041 session sides this opening writes: a Pending
/// Session (fresh `/topic`) or a real Session Mapping (adopt).
enum OpeningSide {
    Pending { title: Option<String> },
    Adopt { session_id: String },
}

/// The snapshot claim (ADR-0028) an adopted topic settles after its in-topic
/// seed is sent: the title for the claim bookkeeping plus the filtered
/// snapshot data the poll loop must never duplicate.
struct SnapshotClaim {
    title: String,
    data: crate::bridge::snapshot::SnapshotData,
}

/// The reply hint seeded as a fresh topic's first in-topic message
/// (ADR-0023): it tells the user where to reply. Pending-session topics
/// (`/topic`, the `/dir` card's 建话题 op — the session is created by the
/// topic's first prompt, ADR-0041) seed with this text; adopted sessions seed
/// with their Session Snapshot card instead (ADR-0028).
const TOPIC_REPLY_HINT: &str = "请在本话题内回复，即可和这个会话对话。";

/// What a newly created topic's FIRST in-topic message carries. That message
/// is also the persisted `topic_anchor` (fallback-card routing, ADR-0006).
enum TopicSeed {
    /// The reply hint text — fresh (`/topic`) topics, pending or already
    /// materialised.
    ReplyHint,
    /// The adopted session's Session Snapshot card (ADR-0028) — `/topic
    /// --adopt` and the switch card's 建话题接管 op.
    Snapshot(serde_json::Value),
}

/// Build the topic cover card's text (ADR-0023): a session brief laid out for
/// the chat-list entry — the list shows the card's first ~3 lines, so line 1
/// is the title, line 2 the project + git state, line 3 the session + dir.
/// No creation verb and no footer: the reply hint lives only on the first
/// message inside the topic, where the user actually sees it. Used both at
/// creation and by the title-sync hook (patching the card keeps the list
/// entry current). Purely human-facing: the injection guard never feeds the
/// topic's own root or anchor back into prompts, so richness costs no tokens.
async fn topic_cover_text(
    title: &str,
    dir: &str,
    session_id: &str,
    agent: Option<&str>,
    model: Option<&str>,
) -> String {
    let mut s = topic_cover_head(title, dir).await;
    s.push_str(&format!("\n会话 `{}` · `{dir}`", id_tail(session_id)));
    if let Some(agent) = agent {
        s.push_str(&format!(" · agent `{agent}`"));
    }
    if let Some(model) = model {
        s.push_str(&format!(" · 模型 `{model}`"));
    }
    s
}

/// The pending variant of [`topic_cover_text`] (ADR-0041): no session exists
/// yet, so the session line is replaced by the creation verb and the directory
/// — 「会话」 stays reserved for a real Session. After materialisation the
/// title-sync hook rebuilds the card as the full brief.
async fn pending_topic_cover_text(title: &str, dir: &str) -> String {
    let mut s = topic_cover_head(title, dir).await;
    s.push_str(&format!("\n下一条消息创建 · `{dir}`"));
    s
}

/// The two cover cards' shared first lines (ADR-0023): the title and the
/// project + git state.
async fn topic_cover_head(title: &str, dir: &str) -> String {
    let git = crate::git::read_state(dir).await;
    let mut s = format!("💬 `{title}`\n`{}`", dir_basename(dir));
    if let Some(branch) = git.branch.as_deref() {
        s.push_str(&format!(" · `{branch}`{}", if git.dirty { " ⚠" } else { "" }));
    }
    s
}

/// Send the topic cover card to the chat's top level. Returns the cover
/// message id, or `None` when sending fails (the caller then anchors the
/// thread on the user's command message instead).
async fn send_topic_cover(platform: &Arc<dyn feishu::Platform>, chat_id: &str, text: &str) -> Option<String> {
    let card = crate::feishu::client::markdown_card(text);
    match platform.send_card("chat_id", chat_id, &card).await {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!("topic cover card send failed: {e}; anchoring on the command message");
            None
        }
    }
}

/// ADR-0023: open a topic whose root is the topic cover card — the session
/// brief sent to the chat's top level, so the chat-list topic entry shows it
/// permanently. Best-effort: when the cover send fails, the thread anchors on
/// `fallback_root` (the user's command message) instead. The topic's FIRST
/// in-topic message is `seed` (the reply hint for fresh sessions, the Session
/// Snapshot card for adopted ones, ADR-0028) and becomes the in-topic anchor.
/// Returns the created reply's message id (the anchor), the new thread_id, the
/// root message id (`topic_root`), and the cover id (`None` on fallback).
async fn open_cover_topic(
    platform: &Arc<dyn feishu::Platform>,
    chat_id: &str,
    fallback_root: &str,
    cover_text: &str,
    seed: TopicSeed,
) -> crate::error::Result<(String, Option<String>, String, Option<String>)> {
    let cover_id = send_topic_cover(platform, chat_id, cover_text).await;
    let root = cover_id.clone().unwrap_or_else(|| fallback_root.to_string());
    let (anchor, thread_id) = match seed {
        TopicSeed::ReplyHint => platform.reply_in_thread(&root, TOPIC_REPLY_HINT).await?,
        TopicSeed::Snapshot(card) => platform.reply_card_in_thread(&root, &card).await?,
    };
    Ok((anchor, thread_id, root, cover_id))
}

/// ADR-0023: when the server's session title differs from the one shown on the
/// topic cover card, rebuild the card in place. The cover card is the thread
/// root, so this is what the chat-list topic entry displays — the patch keeps
/// the entry current after the first auto-generated title (post-turn hook) or
/// an immediate `/name`. A record still flagged `pending` (ADR-0041: the card
/// was written while the session did not exist, so it shows 「下一条消息创建」)
/// is re-rendered even when the title matches. The recorded title lives only in
/// memory: after a restart the next completed turn re-syncs the card once (same
/// content, harmless). Best effort; failures only log. Only cover-rooted topics
/// are patched — command-rooted fallback topics record nothing, so they
/// short-circuit.
///
/// Returns `true` when the card is settled (nothing more to do: no cover
/// topic, already synced, or patched) and `false` when the server title is
/// not (yet) available or the patch failed — callers like the post-turn retry
/// ladder use this to decide whether to try again later.
///
/// The sync runs inside the Session's `topic` span (ADR-0048): the line that
/// records the retitle is the state transition a topic trace is read for.
pub(crate) async fn sync_topic_cover_title(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
) -> bool {
    let thread_key = crate::bridge::span::thread_key_of(sessions, session_id).await;
    let span = crate::bridge::span::topic(Some(session_id), thread_key.as_ref());
    sync_topic_cover_title_inner(cards, sessions, backend, session_id)
        .instrument(span)
        .await
}

/// [`sync_topic_cover_title`]'s body, run inside its Session's `topic` span.
async fn sync_topic_cover_title_inner(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
) -> bool {
    let (root_id, directory, agent, recorded) = {
        match sessions.entry_for_session(session_id).await {
            Some(e) => {
                let recorded = cards.cover_titles.lock().await.get(session_id).cloned();
                (
                    e.topic_root.clone(),
                    e.directory.clone(),
                    e.agent.clone(),
                    recorded,
                )
            }
            None => return true,
        }
    };
    let (Some(root_id), Some(recorded)) = (root_id, recorded) else {
        return true;
    };
    let Ok(info) = backend.session_info(session_id, Some(&directory)).await else {
        return false;
    };
    // Never patch a default title over the recorded one: the server's initial
    // `New session - <ts>` (or empty) would otherwise replace the meaningful
    // creation title (e.g. the directory name) the moment the auto-title has
    // not (yet) been generated. The pending flag does not lift this rule — a
    // pending cover waits for a real title too.
    let Some(title) = info
        .title
        .filter(|t| !t.is_empty() && !crate::feishu::card::clean_session_label(t).is_empty())
    else {
        return false;
    };
    if title == recorded.title && !recorded.pending {
        return true;
    }
    let text = topic_cover_text(
        &title,
        &directory,
        session_id,
        agent.as_deref(),
        recorded.model.as_deref(),
    )
    .await;
    let card = crate::feishu::client::markdown_card(&text);
    match cards.feishu.update_message(&root_id, &card).await {
        Ok(()) => {
            tracing::info!("topic cover card updated for session {}: {}", session_id, title);
            cards.cover_titles.lock().await.insert(
                session_id.to_string(),
                crate::bridge::core::CoverTitle {
                    title,
                    model: recorded.model,
                    pending: false,
                },
            );
            true
        }
        Err(e) => {
            tracing::warn!("topic cover card update failed for session {}: {}", session_id, e);
            false
        }
    }
}

/// ADR-0023: after a completed turn the auto-title may still be in flight —
/// the title agent races the turn, and on short turns it lands AFTER the turn
/// ends. Retry the cover sync at 10/30/60/120 s after the turn so the
/// chat-list topic entry follows even if the user stops here. Each attempt is
/// one cheap `session_info` GET and stops as soon as the title is settled
/// (patched, already equal, or no cover topic); the ladder gives up after two
/// minutes, leaving later turns' hooks to catch up. Detached task: holds no
/// locks across sleeps. Only meaningful when the initial sync did not settle —
/// callers gate on its return value.
pub(crate) fn spawn_cover_title_retry(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
) {
    spawn_cover_title_retry_at(
        cards,
        sessions,
        backend,
        session_id,
        &[
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(120),
        ],
    );
}

/// The delay-injectable form of [`spawn_cover_title_retry`] (tests use
/// millisecond delays). `delays` are ABSOLUTE offsets from the call: attempts
/// happen at each listed time after spawn, not after the previous attempt.
pub(crate) fn spawn_cover_title_retry_at(
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    delays: &[std::time::Duration],
) {
    let cards = cards.clone();
    let sessions = sessions.clone();
    let backend = Arc::clone(backend);
    let sid = session_id.to_string();
    let delays = delays.to_vec();
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        for delay in delays {
            let elapsed = start.elapsed();
            if delay > elapsed {
                tokio::time::sleep(delay - elapsed).await;
            }
            if sync_topic_cover_title(&cards, &sessions, &backend, &sid).await {
                return;
            }
        }
    });
}

/// ADR-0023: record the title shown on a topic's cover card so the post-turn
/// hook can sync the server title onto the card. Only cover-rooted topics are
/// patchable (Feishu updates only the app's own cards) — when no cover card
/// was sent, any stale entry is dropped so nothing is ever patched onto a
/// user message. `key` is the session id, or [`pending_cover_key`] while the
/// topic's session is still pending (ADR-0041).
async fn record_cover_title(
    cards: &CardsHandle,
    key: &str,
    title: &str,
    model: Option<String>,
    cover_sent: bool,
    pending: bool,
) {
    let mut covers = cards.cover_titles.lock().await;
    if cover_sent {
        covers.insert(
            key.to_string(),
            crate::bridge::core::CoverTitle {
                title: title.to_string(),
                model,
                pending,
            },
        );
    } else {
        covers.remove(key);
    }
}

/// The cover-title cache key for a topic whose session is still pending
/// (ADR-0041): there is no session id yet, so the record rides the topic's
/// ThreadKey until materialisation or adoption claims it for a real session.
fn pending_cover_key(key: &ThreadKey) -> String {
    format!("pending:{}:{}", key.chat_id, key.thread_id)
}

/// ADR-0041 + ADR-0023: move a pending topic's cover record onto the real
/// session that replaced it and re-render the cover at once. The cover still
/// shows 「下一条消息创建」, so the first successful re-render replaces it with
/// the full brief; when the server title is not yet available, the post-turn
/// retry ladder finishes the job. No-op for topics without a sent cover card
/// (fallback-rooted: nothing was recorded) and for non-topic pendings.
pub(crate) async fn claim_pending_cover(handles: &TopicHandles, key: &ThreadKey, session_id: &str) {
    let claimed = {
        let mut covers = handles.flow.cards.cover_titles.lock().await;
        if let Some(cover) = covers.remove(&pending_cover_key(key)) {
            covers.insert(session_id.to_string(), cover);
            true
        } else {
            false
        }
    };
    if claimed {
        sync_topic_cover_title(
            &handles.flow.cards,
            &handles.flow.sessions,
            &handles.flow.backend,
            session_id,
        )
        .await;
    }
}

/// ADR-0041 + ADR-0023: `/name` on a pending topic re-renders its cover card
/// immediately — no server session exists yet, so the title-sync path cannot
/// run. The recorded title follows so the post-materialisation re-render
/// compares against what the card shows. A fallback-rooted topic (cover send
/// failed) recorded nothing and is left alone: its root is the user's command
/// message and cannot be patched.
///
/// The retitle runs inside the pending topic's `topic` span (ADR-0048) — no
/// Session exists yet, so the span carries the Chat/Topic alone.
pub(crate) async fn rename_pending_cover(handles: &TopicHandles, key: &ThreadKey, title: &str) {
    let span = crate::bridge::span::topic(None, Some(key));
    rename_pending_cover_inner(handles, key, title)
        .instrument(span)
        .await
}

/// [`rename_pending_cover`]'s body, run inside the pending topic's `topic` span.
async fn rename_pending_cover_inner(handles: &TopicHandles, key: &ThreadKey, title: &str) {
    let pending_root = {
        let store = handles.flow.sessions.store.lock().await;
        store
            .pending_for(key)
            .map(|p| (p.topic_root.clone(), p.directory.clone()))
    };
    let Some((Some(root_id), directory)) = pending_root else {
        return;
    };
    let cache_key = pending_cover_key(key);
    if !handles
        .flow
        .cards
        .cover_titles
        .lock()
        .await
        .contains_key(&cache_key)
    {
        return;
    }
    let text = pending_topic_cover_text(title, &directory).await;
    let card = crate::feishu::client::markdown_card(&text);
    match handles.flow.platform.update_message(&root_id, &card).await {
        Ok(()) => {
            if let Some(cover) = handles.flow.cards.cover_titles.lock().await.get_mut(&cache_key) {
                cover.title = title.to_string();
            }
            tracing::info!(
                "pending topic cover retitled to {:?} for {}",
                title,
                key.thread_id
            );
        }
        Err(e) => tracing::warn!("pending topic cover retitle failed for {}: {}", key.thread_id, e),
    }
}
