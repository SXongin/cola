//! The topic-opening transaction (ADR-0023, ADR-0028).
//!
//! Opening a topic is a protocol of five steps — create the session (or adopt
//! an existing one), send the Topic Cover Card and anchor the thread on it
//! (falling back to the command/card message when the cover cannot be sent),
//! seed the topic's first in-topic message, write the Session Mapping, and
//! record the cover title. This module owns the whole protocol so the callers
//! (`/topic`, `/topic --adopt`, the `/dir` card's 建话题 op and the switch
//! card's 建话题接管 op) only choose a [`TopicOpening`] and render the
//! outcome — the cover-card fallback can never be caller-remembered again.

use std::sync::Arc;

use crate::bridge::core::SharedCore;
use crate::bridge::display::{dir_basename, id_tail, model_display};
use crate::config::{SessionEntry, ThreadKey};
use crate::error::BridgeError;
use crate::opencode::SessionListInfo;

/// What kind of topic to open: one around a brand-new session (`/topic`, the
/// `/dir` card's 建话题 op) or one around an EXISTING server session
/// (`/topic --adopt`, the switch card's 建话题接管 op, ADR-0016).
pub(crate) enum TopicOpening {
    /// Create a session in `directory`, PATCH `name` as the title when given
    /// (ADR-0007 creation title policy), and open the topic around it. The
    /// cover shows `name` or the directory basename until the server
    /// auto-generates a real title.
    Fresh { directory: String, name: Option<String> },
    /// Open the topic around an existing session (ADR-0016): the Session
    /// Snapshot card (ADR-0028) becomes the topic's first in-topic message
    /// and anchor, gathered before the mapping write.
    Adopt { info: SessionListInfo },
}

/// The opened topic: the session it belongs to and the new Feishu `thread_id`.
/// The thread root and in-topic anchor (ADR-0023) are written to the Session
/// Mapping by the transaction, not returned — no caller needs to re-place them.
pub(crate) struct OpenedTopic {
    pub session_id: String,
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

/// Open a topic — the whole transaction. Fresh: create the session (with an
/// optional title PATCH), build the cover brief, send the cover card to the
/// chat's top level and anchor the thread on it, falling back to
/// `fallback_root` (the user's command message or the card's own message) when
/// the cover send fails (ADR-0023); seed the first in-topic message, map the
/// session to the new topic's `ThreadKey`, and record the cover title. Adopt:
/// additionally gather the Session Snapshot BEFORE the mapping write (so the
/// card reflects the session's pre-adoption state) and claim its embedded
/// pendings against the in-topic snapshot after the send (ADR-0028).
pub(crate) async fn open_topic(
    core: &Arc<SharedCore>,
    chat_id: &str,
    fallback_root: &str,
    opening: TopicOpening,
) -> Result<OpenedTopic, OpenTopicError> {
    // Step one: the session side. Fresh creates the session here (so no caller
    // can forget it), Adopt gathers the snapshot while the mapping is still
    // unwritten.
    let parts = match opening {
        TopicOpening::Fresh { directory, name } => {
            let session = core
                .opencode
                .create_session(&core.opencode.new_session_input(Some(&directory)))
                .await?;
            // Creation title policy (ADR-0007): a named `/topic` PATCHes the
            // title; without a name the server default is left for
            // auto-generation. The display name only drives the cover text.
            if let Some(n) = &name {
                core.opencode.update_session_title(&session.id, n).await?;
            }
            let display_title = name.unwrap_or_else(|| dir_basename(&directory));
            OpeningParts {
                session_id: session.id,
                directory,
                display_title,
                agent: None,
                model: None,
                seed: TopicSeed::ReplyHint,
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
            let (card, data) = crate::bridge::snapshot::snapshot_card_for(core, "接管", &info).await;
            OpeningParts {
                session_id: info.id,
                directory: info.directory,
                display_title: info.title.clone(),
                agent: info.agent,
                model: model_display(info.model.as_ref()),
                seed: TopicSeed::Snapshot(card),
                snapshot_claim: Some(SnapshotClaim {
                    title: info.title,
                    data,
                }),
            }
        }
    };

    // Step two: the shared cover/anchor/mapping tail. The fallback ladder
    // lives in `open_cover_topic`, so every surface gets it.
    let cover_text = topic_cover_text(
        &parts.display_title,
        &parts.directory,
        &parts.session_id,
        parts.agent.as_deref(),
        parts.model.as_deref(),
    )
    .await;
    let (anchor, thread_id, topic_root, cover_id) =
        open_cover_topic(core, chat_id, fallback_root, &cover_text, parts.seed).await?;
    let Some(thread_id) = thread_id else {
        tracing::warn!(
            "topic: no thread_id returned in chat {} for session {}; not mapping session",
            chat_id,
            parts.session_id
        );
        return Err(OpenTopicError::NoThreadId);
    };
    let topic_key = ThreadKey::new(chat_id.to_string(), thread_id.clone());
    let mut entry = SessionEntry::new(topic_key, parts.session_id.clone(), parts.directory);
    entry.agent = parts.agent;
    entry.topic_anchor = Some(anchor.clone());
    entry.topic_root = Some(topic_root);
    core.activate_session(entry).await?;
    record_cover_title(
        core,
        &parts.session_id,
        &parts.display_title,
        parts.model,
        cover_id.is_some(),
    )
    .await;
    // ADR-0028: claim the snapshot's embedded pendings against the in-topic
    // snapshot message (the topic's first message + anchor) so the poll loop
    // never duplicates them. Not reached when the topic could not be opened
    // (no thread_id) — the poller keeps today's standalone flow for them.
    if let Some(claim) = parts.snapshot_claim {
        crate::bridge::external::settle_snapshot_after_send(core, &anchor, "接管", &claim.title, &claim.data)
            .await;
    }
    Ok(OpenedTopic {
        session_id: parts.session_id,
        thread_id,
    })
}

/// The session-side facts the opening tail needs, assembled by each
/// [`TopicOpening`] arm so the cover/anchor/mapping sequence itself exists
/// once.
struct OpeningParts {
    session_id: String,
    directory: String,
    display_title: String,
    agent: Option<String>,
    model: Option<String>,
    seed: TopicSeed,
    /// ADR-0028: the adopt snapshot's title and claimable data, settled
    /// against the in-topic seed after the mapping write. `None` for fresh
    /// sessions.
    snapshot_claim: Option<SnapshotClaim>,
}

/// The snapshot claim (ADR-0028) an adopted topic settles after its in-topic
/// seed is sent: the title for the claim bookkeeping plus the filtered
/// snapshot data the poll loop must never duplicate.
struct SnapshotClaim {
    title: String,
    data: crate::bridge::snapshot::SnapshotData,
}

/// The reply hint seeded as a fresh topic's first in-topic message
/// (ADR-0023): it tells the user where to reply. Fresh-session topics
/// (`/topic`, the `/dir` card's 建话题 op) seed with this text; adopted
/// sessions seed with their Session Snapshot card instead (ADR-0028).
const TOPIC_REPLY_HINT: &str = "请在本话题内回复，即可和这个会话对话。";

/// What a newly created topic's FIRST in-topic message carries. That message
/// is also the persisted `topic_anchor` (fallback-card routing, ADR-0006).
enum TopicSeed {
    /// The reply hint text — fresh sessions created around a new session.
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
    let git = crate::git::read_state(dir).await;
    let mut s = format!("💬 `{title}`\n`{}`", dir_basename(dir));
    if let Some(branch) = git.branch.as_deref() {
        s.push_str(&format!(" · `{branch}`{}", if git.dirty { " ⚠" } else { "" }));
    }
    s.push_str(&format!("\n会话 `{}` · `{dir}`", id_tail(session_id)));
    if let Some(agent) = agent {
        s.push_str(&format!(" · agent `{agent}`"));
    }
    if let Some(model) = model {
        s.push_str(&format!(" · 模型 `{model}`"));
    }
    s
}

/// Send the topic cover card to the chat's top level. Returns the cover
/// message id, or `None` when sending fails (the caller then anchors the
/// thread on the user's command message instead).
async fn send_topic_cover(core: &Arc<SharedCore>, chat_id: &str, text: &str) -> Option<String> {
    let card = crate::feishu::client::markdown_card(text);
    match core.feishu.send_card("chat_id", chat_id, &card).await {
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
    core: &Arc<SharedCore>,
    chat_id: &str,
    fallback_root: &str,
    cover_text: &str,
    seed: TopicSeed,
) -> crate::error::Result<(String, Option<String>, String, Option<String>)> {
    let cover_id = send_topic_cover(core, chat_id, cover_text).await;
    let root = cover_id.clone().unwrap_or_else(|| fallback_root.to_string());
    let (anchor, thread_id) = match seed {
        TopicSeed::ReplyHint => core.feishu.reply_in_thread(&root, TOPIC_REPLY_HINT).await?,
        TopicSeed::Snapshot(card) => core.feishu.reply_card_in_thread(&root, &card).await?,
    };
    Ok((anchor, thread_id, root, cover_id))
}

/// ADR-0023: when the server's session title differs from the one shown on the
/// topic cover card, rebuild the card in place. The cover card is the thread
/// root, so this is what the chat-list topic entry displays — the patch keeps
/// the entry current after the first auto-generated title (post-turn hook) or
/// an immediate `/name`. The recorded title lives only in memory: after a
/// restart the next completed turn re-syncs the card once (same content,
/// harmless). Best effort; failures only log. Only cover-rooted topics are
/// patched — command-rooted fallback topics record nothing, so they
/// short-circuit.
///
/// Returns `true` when the card is settled (nothing more to do: no cover
/// topic, already synced, or patched) and `false` when the server title is
/// not (yet) available or the patch failed — callers like the post-turn retry
/// ladder use this to decide whether to try again later.
pub(crate) async fn sync_topic_cover_title(core: &Arc<SharedCore>, session_id: &str) -> bool {
    let (root_id, directory, agent, recorded) = {
        let store = core.sessions.lock().await;
        match store.entry_for_session(session_id) {
            Some(e) => {
                let recorded = core.cover_titles.lock().await.get(session_id).cloned();
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
    let Ok(info) = core.opencode.session_info(session_id, Some(&directory)).await else {
        return false;
    };
    // Never patch a default title over the recorded one: the server's initial
    // `New session - <ts>` (or empty) would otherwise replace the meaningful
    // creation title (e.g. the directory name) the moment the auto-title has
    // not (yet) been generated.
    let Some(title) = info
        .title
        .filter(|t| !t.is_empty() && !crate::feishu::card::clean_session_label(t).is_empty())
    else {
        return false;
    };
    if title == recorded.title {
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
    match core.feishu.update_message(&root_id, &card).await {
        Ok(()) => {
            tracing::info!("topic cover card updated for session {}: {}", session_id, title);
            core.cover_titles.lock().await.insert(
                session_id.to_string(),
                crate::bridge::core::CoverTitle {
                    title,
                    model: recorded.model,
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
pub(crate) fn spawn_cover_title_retry(core: &Arc<SharedCore>, session_id: &str) {
    spawn_cover_title_retry_at(
        core,
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
    core: &Arc<SharedCore>,
    session_id: &str,
    delays: &[std::time::Duration],
) {
    let core = Arc::clone(core);
    let sid = session_id.to_string();
    let delays = delays.to_vec();
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        for delay in delays {
            let elapsed = start.elapsed();
            if delay > elapsed {
                tokio::time::sleep(delay - elapsed).await;
            }
            if sync_topic_cover_title(&core, &sid).await {
                return;
            }
        }
    });
}

/// ADR-0023: record the title shown on a topic's cover card so the post-turn
/// hook can sync the server title onto the card. Only cover-rooted topics are
/// patchable (Feishu updates only the app's own cards) — when no cover card
/// was sent, any stale entry is dropped so nothing is ever patched onto a
/// user message.
async fn record_cover_title(
    core: &Arc<SharedCore>,
    session_id: &str,
    title: &str,
    model: Option<String>,
    cover_sent: bool,
) {
    let mut covers = core.cover_titles.lock().await;
    if cover_sent {
        covers.insert(
            session_id.to_string(),
            crate::bridge::core::CoverTitle {
                title: title.to_string(),
                model,
            },
        );
    } else {
        covers.remove(session_id);
    }
}
