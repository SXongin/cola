//! The `/switch` session picker's clicks (ADR-0012, ADR-0022, ADR-0052;
//! ticket #1122).
//!
//! Nine ops share the `action: "switch"` tag, told apart by `op`: `page`,
//! `scope`, `adopt`, `force_adopt`, `back`, `new`, `search`, `topic_adopt` and
//! `force_topic_adopt`. The body multiplexes them itself, so the registry has
//! one entry for the action — the same shape the dispatch's single arm had.
//!
//! The 接管/切换 ops patch the clicked list card in place to the compact 已切换
//! state (ADR-0100 / ADR-0052) and carry the same verb in the toast; page,
//! scope, search, new and the topic ops rebuild the list card with the active
//! keyword/scope/page, except that a search or a scope toggle resets to page 1.
//! `new` declares a Pending Session (ADR-0041) on the click's ONE deadline
//! (ADR-0089) — the `deadline` the dispatch derived, never a fresh one.

use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::card_action::VerbFuture;
use crate::bridge::command;
use crate::bridge::core::SharedCore;
use crate::bridge::handler::{
    App, CARD_STORE_TIMEOUT, CardActionResult, card_before, reject_nested_topic, thread_key_from_value,
};
use crate::config::ThreadKey;

/// The `/switch` card's callback. The body parses `op` and dispatches its nine
/// shapes; a click naming no known op (or a target the session list no longer
/// holds) stays unanswered.
pub(crate) fn run<'a>(
    app: &'a Arc<App>,
    value: &'a serde_json::Value,
    deadline: Instant,
    _callback_started: Instant,
) -> VerbFuture<'a> {
    Box::pin(async move { app.handle_switch_card_action(&app.core, value, deadline).await })
}

impl App {
    /// Handle a `/switch` card button (ADR-0012, issue 04): adopt a session,
    /// create a new one, re-search, or flip the page. The 接管/切换 ops patch
    /// the clicked list card in place to the compact 已切换 state (ADR-0100 /
    /// ADR-0052; the toast carries the same verb) — one message per activation,
    /// never a new one. page/scope/search/new/topic_adopt return a refreshed
    /// list card carrying the active keyword/scope/page (ADR-0052), except that
    /// a search or scope toggle resets to page 1. Plus a Toast for instant
    /// feedback.
    async fn handle_switch_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        deadline: tokio::time::Instant,
    ) -> Option<CardActionResult> {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        let session_id = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let scope = crate::feishu::card::session::SwitchScope::parse(
            value.get("scope").and_then(|v| v.as_str()).unwrap_or(""),
        );
        // The card's stateless filter (ADR-0052): keyword, scope and page ride
        // every button. A missing/garbage page reads as 1; the builder clamps an
        // out-of-range page to the last page.
        let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;

        match op {
            "page" => Some(CardActionResult {
                card: Some(
                    self.build_switch_card_for(core, &thread_key, &keyword, scope, page)
                        .await,
                ),
                toast: None,
            }),
            "scope" => {
                // The 全部/本目录 toggle (ADR-0022): the button carries the
                // TARGET scope ("all"/"dir"); the outer `scope` above already
                // parsed it. Rebuild the card in that scope at page 1, keeping
                // the keyword so a scoped search survives the toggle (ADR-0052).
                Some(CardActionResult {
                    card: Some(
                        self.build_switch_card_for(core, &thread_key, &keyword, scope, 1)
                            .await,
                    ),
                    toast: None,
                })
            }
            "adopt" | "force_adopt" => {
                // Adopt/switch the target session into this thread. The plain
                // `adopt` op carries no `--force`: an occupied session patches
                // the card to the force-confirm card, whose `force_adopt`
                // button re-enters here. `force_adopt` skips the owner check and
                // lets the adoption's `set_active` steal the mapping. Either
                // op refuses a child the chat has not mapped: the card's rows
                // are roots-only, but a stale or forged callback can still name
                // one, and the general surfaces never adopt a child (spec
                // #344; the sanctioned path is `/sub attach`).
                let force = op == "force_adopt";
                let sessions = core.cached_session_list().await.ok()?;
                let target = sessions.iter().find(|s| s.id == session_id)?.clone();
                let child_refusal = {
                    let store = core.sessions.lock().await;
                    command::child_adoption_refusal(&store, &thread_key, &target)
                };
                if let Some(refusal) = child_refusal {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(refusal),
                    });
                }
                let already_active = {
                    let store = core.sessions.lock().await;
                    store
                        .get_active(&thread_key)
                        .map(|e| e.session_id == target.id)
                        .unwrap_or(false)
                };
                if already_active {
                    return Some(CardActionResult {
                        card: Some(
                            self.build_switch_card_for(core, &thread_key, &keyword, scope, page)
                                .await,
                        ),
                        // The target session is already this conversation's
                        // active session — nothing changed. 会话 here names the
                        // OpenCode session (ADR-0022 keeps it), not the Feishu
                        // side.
                        toast: Some("已在当前会话".to_string()),
                    });
                }
                if !force {
                    let owner = {
                        let store = core.sessions.lock().await;
                        store.thread_for_session(&target.id)
                    };
                    if let Some(owner_key) = owner
                        && owner_key != thread_key
                    {
                        let owner_label = owner_label(core, &owner_key).await;
                        return Some(CardActionResult {
                            card: Some(crate::feishu::card::session::build_force_confirm_card(
                                &thread_key,
                                &target,
                                &owner_label,
                                "force_adopt",
                                "强制接管",
                                scope,
                                &keyword,
                                page,
                            )),
                            toast: Some(format!("该会话被 {} 占用，请确认是否强制接管", owner_label)),
                        });
                    }
                }
                // ADR-0028 one-card rule: the switch card's OWN message becomes
                // the confirmation — the ack patches the list card in place to
                // the compact 已切换 state (ADR-0100/ADR-0052; the toast carries
                // the same verb). The list's filter rides along (ADR-0052): its
                // 返回列表 button rebuilds exactly this window.
                let open_message_id = value
                    .get("open_message_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let back = crate::feishu::card::session::BackToList {
                    thread_key: thread_key.clone(),
                    keyword: keyword.clone(),
                    scope,
                    page,
                };
                Some(
                    self.apply_card_adoption(core, &thread_key, &target, open_message_id, Some(&back))
                        .await,
                )
            }
            "back" => Some(CardActionResult {
                card: Some(
                    self.build_switch_card_for(core, &thread_key, &keyword, scope, page)
                        .await,
                ),
                toast: None,
            }),
            "new" => {
                // Lazy Session Creation (ADR-0041): the card form of `/new` —
                // declare a Pending Session; the first message materialises it.
                // The active filter survives (ADR-0052), so the refreshed list
                // stays on the same keyword/scope/page.
                //
                // The declaration and its write ride the click's ONE deadline
                // (ADR-0089): a click must never commit a pending after Feishu
                // has stopped listening for its answer.
                let toast = match core
                    .command_handles()
                    .declare_pending_in_current_project_until(&thread_key, None, deadline)
                    .await
                {
                    Ok(Some(_)) => "下一条消息创建会话".to_string(),
                    // The store lock was still held: nothing changed, so there
                    // is no refreshed card to show.
                    Ok(None) => {
                        return Some(CardActionResult {
                            card: None,
                            toast: Some(CARD_STORE_TIMEOUT.to_string()),
                        });
                    }
                    // The pending IS declared in memory; only the save failed.
                    Err(e) => {
                        tracing::warn!("switch card new: persist failed: {}", e);
                        format!("⚠️ 已新建待创建会话，但保存失败（重启后会丢失）：{e}")
                    }
                };
                Some(CardActionResult {
                    card: card_before(
                        deadline,
                        self.build_switch_card_for(core, &thread_key, &keyword, scope, page),
                    )
                    .await,
                    toast: Some(toast),
                })
            }
            "search" => {
                // A search is an explicit refresh request: drop the session-list
                // cache so the re-filter sees newly created/adopted sessions.
                // It always lands on page 1; the keyword is the typed one.
                core.invalidate_session_list_cache().await;
                Some(CardActionResult {
                    card: Some(
                        self.build_switch_card_for(core, &thread_key, &keyword, scope, 1)
                            .await,
                    ),
                    toast: None,
                })
            }
            "topic_adopt" | "force_topic_adopt" => {
                // "建话题接管" (ADR-0016): open a NEW Feishu topic around an
                // existing session, in one gesture from the session card. The
                // topic anchors on the card's own message (`open_message_id`,
                // threaded through by `extract_card_action_value`), so fallback
                // cards can reply inside it. The plain `topic_adopt` op patches
                // an occupied session to the force-confirm card (强制建话题接管),
                // whose `force_topic_adopt` button re-enters here and skips the
                // owner check.
                // A topic can only be created from a non-topic message
                // (ADR-0006, ADR-0025): the session card may legally open in a
                // never-bound topic, but its topic op must not nest there —
                // same guard as the `/dir` card's "建话题" op and the text
                // `/topic --adopt` forms.
                let force = op == "force_topic_adopt";
                if let Some(rejection) = reject_nested_topic(&thread_key) {
                    return Some(rejection);
                }
                let Some(open_message_id) = value.get("open_message_id").and_then(|v| v.as_str()) else {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some("无法创建话题（缺少卡片消息引用）".to_string()),
                    });
                };
                let open_message_id = open_message_id.to_string();
                let sessions = core.cached_session_list().await.ok()?;
                let target = sessions.iter().find(|s| s.id == session_id)?.clone();
                if target.is_child() {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some("子任务会话不支持接管".to_string()),
                    });
                }
                if !force {
                    let owner = {
                        let store = core.sessions.lock().await;
                        store.thread_for_session(&target.id)
                    };
                    if let Some(owner_key) = owner
                        && owner_key != thread_key
                    {
                        let owner_label = owner_label(core, &owner_key).await;
                        return Some(CardActionResult {
                            card: Some(crate::feishu::card::session::build_force_confirm_card(
                                &thread_key,
                                &target,
                                &owner_label,
                                "force_topic_adopt",
                                "强制建话题接管",
                                scope,
                                &keyword,
                                page,
                            )),
                            toast: Some(format!("会话被 {} 占用，请确认是否强制接管", owner_label)),
                        });
                    }
                }
                Some(
                    self.topic_adopt_target(
                        core,
                        &thread_key,
                        &target,
                        &open_message_id,
                        &keyword,
                        scope,
                        page,
                    )
                    .await,
                )
            }
            _ => None,
        }
    }

    /// Rebuild the `/switch` card for a thread with the given search keyword,
    /// list scope and 1-based page (ADR-0022, ADR-0052). The builder clamps an
    /// out-of-range page to the last page.
    async fn build_switch_card_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        keyword: &str,
        scope: crate::feishu::card::session::SwitchScope,
        page: usize,
    ) -> serde_json::Value {
        let (shown, active_id, mapped_ids, scope, current_dir) =
            crate::feishu::card::command::switch_card_data(
                &core.command_handles(),
                thread_key,
                keyword,
                scope,
            )
            .await;
        crate::feishu::card::session::build_switch_card(
            thread_key,
            &shown,
            keyword,
            scope,
            page,
            current_dir.as_deref(),
            active_id.as_deref(),
            &mapped_ids,
        )
    }

    /// The adoption tail shared by the card's 接管/切换 and 强制接管 ops: persist
    /// the new active entry, then patch the clicked card in place to the compact
    /// 已切换 state (ADR-0100/ADR-0052, which retires the Session Snapshot from
    /// this op). The card and the toast carry the SAME 已切换 verb whether this
    /// is a first adoption or a re-switch. The returned card IS the ack's
    /// replacement (ADR-0100's one-card rule), so no second message is sent.
    /// `back` is the switch card's list state the adoption came from
    /// (ADR-0052): the compact state carries its 返回列表 button.
    ///
    /// The steal is the atomic activation's: it replaces any existing entry
    /// with this session id in the same write, so a `force_adopt` from another
    /// thread leaves that owner sessionless there and its waiting card is
    /// collected only after the write persisted (ADR-0059, spec #405). No
    /// pre-remove — if the adoption work failed, the old mapping would
    /// otherwise be lost for nothing. A failed save therefore answers with a
    /// failure toast and **no** replacement card: patching the list to 已切换
    /// would claim an adoption that did not happen.
    async fn apply_card_adoption(
        &self,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        target: &crate::backend::types::SessionListInfo,
        open_message_id: Option<String>,
        back: Option<&crate::feishu::card::session::BackToList>,
    ) -> CardActionResult {
        // In a topic the patched card lives INSIDE it, so persist its own
        // message id as the fallback-card anchor (the Topic Anchor semantics of
        // ADR-0100 / ADR-0052): later permission/question cards reply
        // to it and stay in the topic. When the topic's session was still
        // pending (ADR-0041), carry its cover root too so the quote-injection
        // guard keeps excluding the topic's own root and the cover record can
        // be claimed for the adopted session.
        let pending_topic_root = {
            let store = core.sessions.lock().await;
            store.pending_for(thread_key).and_then(|p| p.topic_root.clone())
        };
        let topic_anchor = if thread_key.thread_id != thread_key.chat_id {
            open_message_id.clone()
        } else {
            None
        };
        let mut entry =
            crate::config::SessionEntry::new(thread_key.clone(), target.id.clone(), target.directory.clone());
        entry.agent = target.agent.clone();
        entry.topic_anchor = topic_anchor;
        entry.topic_root = pending_topic_root;
        // The ack's card is the confirmation, so it must not claim 已切换 for a
        // save that failed: the atomic activation leaves the Session with its
        // old owner (or unmapped), so answer with the failure and leave the
        // clicked list card as it was.
        if let Err(e) = core.activate_session(entry).await {
            tracing::warn!("switch card adopt: persist failed: {}", e);
            return CardActionResult {
                card: None,
                toast: Some(format!("⚠️ 接管失败：{e}")),
            };
        }
        crate::bridge::topic::claim_pending_cover(&core.topic_handles(), thread_key, &target.id).await;
        CardActionResult {
            card: Some(crate::feishu::card::session::build_switched_state_card(
                &target.title,
                &target.id,
                &target.directory,
                back,
            )),
            toast: Some(format!(
                "已切换「{}」",
                crate::bridge::display::title_or_id_tail(target)
            )),
        }
    }

    /// The topic-creation tail shared by the card's 建话题接管 and
    /// 强制建话题接管 ops: open a topic anchored on the card message and map
    /// the adopted session to the new topic's `ThreadKey` (shared with the text
    /// `/topic --adopt` form, ADR-0016), then refresh the list card in place at
    /// the same keyword/scope/page (ADR-0052).
    #[allow(clippy::too_many_arguments)] // the filter triple rides the rebuild: keyword + scope + page (#1122)
    async fn topic_adopt_target(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        target: &crate::backend::types::SessionListInfo,
        open_message_id: &str,
        keyword: &str,
        scope: crate::feishu::card::session::SwitchScope,
        page: usize,
    ) -> CardActionResult {
        let new_thread_id = match crate::bridge::topic::open_topic(
            &core.topic_handles(),
            &thread_key.chat_id,
            open_message_id,
            crate::bridge::topic::TopicOpening::Adopt { info: target.clone() },
        )
        .await
        {
            Ok(opened) => opened.thread_id,
            Err(crate::bridge::topic::OpenTopicError::NoThreadId) => {
                return CardActionResult {
                    card: None,
                    toast: Some("创建话题失败（未返回 thread_id）".to_string()),
                };
            }
            Err(crate::bridge::topic::OpenTopicError::Failed(e)) => {
                tracing::warn!("switch card topic_adopt: create topic failed: {}", e);
                return CardActionResult {
                    card: None,
                    toast: Some("创建话题失败".to_string()),
                };
            }
        };
        tracing::info!(
            "switch card topic_adopt: created topic {} for session {} in chat {}",
            new_thread_id,
            target.id,
            thread_key.chat_id
        );
        CardActionResult {
            card: Some(
                self.build_switch_card_for(core, thread_key, keyword, scope, page)
                    .await,
            ),
            toast: Some("已建话题接管".to_string()),
        }
    }
}

/// A display label for the Chat/Topic that owns a session, for the force-confirm
/// card: the chat's display name (falling back to its id), with `（话题）` when
/// the owner is a Topic rather than the Chat lobby. Vocabulary per GLOSSARY.md —
/// the Feishu side is a Chat/Topic, never a "conversation".
async fn owner_label(core: &Arc<SharedCore>, owner_key: &ThreadKey) -> String {
    let chat_name = core
        .feishu
        .chat_name(&owner_key.chat_id)
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| owner_key.chat_id.clone());
    if owner_key.thread_id != owner_key.chat_id {
        format!("{chat_name}（话题）")
    } else {
        chat_name
    }
}
