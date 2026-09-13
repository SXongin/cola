use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::core::SharedCore;
use crate::bridge::handler::CardActionResult;
use crate::bridge::pollers::{
    CardTarget, inline_host_session, mark_stale_cards, resolve_card_target, result_card,
};
use crate::bridge::question::{
    MultiOutcome, QuestionState, action_toast, qa_completion_body, question_replay_card, stale_question_card,
};
use crate::bridge::snapshot_claims::ClaimKind;
use crate::bridge::streaming::StreamAccumulator;
use crate::opencode;

/// A pending request surfaced by a poll loop before it becomes a card. Carries
/// whichever backend payload the kind produced — permission or question.
#[derive(Clone)]
pub enum PendingRequest {
    Permission(opencode::client::PermissionRequest),
    Question(opencode::client::QuestionRequest),
}

impl PendingRequest {
    pub fn id(&self) -> &str {
        match self {
            PendingRequest::Permission(p) => &p.request_id,
            PendingRequest::Question(q) => &q.id,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            PendingRequest::Permission(p) => p.session_id.as_deref().unwrap_or(""),
            PendingRequest::Question(q) => &q.session_id,
        }
    }

    /// The snapshot-claim kind of this request (ADR-0028).
    pub fn claim_kind(&self) -> ClaimKind {
        match self {
            PendingRequest::Permission(_) => ClaimKind::Permission,
            PendingRequest::Question(_) => ClaimKind::Question,
        }
    }
}

/// The deltas that make a permission request and a question request different.
/// Everything else — the poll loop, the card delivery, the double-click guard
/// primitive and the error-result block — lives once in [`RequestFlow`].
#[async_trait::async_trait]
#[allow(clippy::too_many_arguments)] // the action context is the trait's whole point
pub trait RequestKind: Send + Sync {
    /// Human label for logs and the stale-card text ("权限" / "问答").
    fn label(&self) -> &'static str;

    /// List pending requests for one server instance (the directory is owned by
    /// the handle, ADR-0010).
    async fn list(
        &self,
        backend: &Arc<dyn opencode::DirectoryBackend>,
    ) -> crate::error::Result<Vec<PendingRequest>>;

    /// Runs once per newly-seen request before it becomes a card. Permissions
    /// answer `/autoaccept` sessions here and return true (no card); questions
    /// remember the full request (via the flow) for later card rebuilds and
    /// return false.
    async fn prepare(
        &self,
        flow: &RequestFlow,
        core: &Arc<SharedCore>,
        req: &PendingRequest,
        dir: &str,
    ) -> bool;

    /// Push the request onto a host streaming card's inline section. Returns
    /// true when it was pushed (dedup already applied by the caller).
    fn push_inline(&self, acc: &mut StreamAccumulator, req: &PendingRequest, dir: &str) -> bool;

    /// Build the standalone interactive card (used when no streaming card hosts
    /// the request inline).
    fn build_card(&self, req: &PendingRequest, dir: &str) -> serde_json::Value;

    /// The description shown on the card when it is marked stale (resolved by
    /// another client).
    fn summary(&self, req: &PendingRequest) -> String;

    /// Drop inline sections whose request vanished (answered elsewhere).
    fn retain_inline(&self, acc: &mut StreamAccumulator, pending: &std::collections::HashSet<String>);

    /// The snapshot-claim kind of this flow's requests (ADR-0028): each flow's
    /// poll sweep only drops claims of its own kind.
    fn claim_kind(&self) -> ClaimKind;

    /// Handle a card action click on this kind's card.
    async fn handle_action(
        &self,
        flow: &RequestFlow,
        core: &Arc<SharedCore>,
        session_id: &str,
        req_id: &str,
        reply: &str,
        value: &serde_json::Value,
        directory: Option<&str>,
        inline: bool,
        host: &Option<String>,
    ) -> Option<CardActionResult>;
}

/// The permission kind: `/autoaccept` sessions are answered automatically, and
/// the card carries a friendly description of what the AI wants to do.
pub struct PermissionKind;

#[async_trait::async_trait]
impl RequestKind for PermissionKind {
    fn label(&self) -> &'static str {
        "权限"
    }

    async fn list(
        &self,
        backend: &Arc<dyn opencode::DirectoryBackend>,
    ) -> crate::error::Result<Vec<PendingRequest>> {
        backend
            .list_permissions()
            .await
            .map(|v| v.into_iter().map(PendingRequest::Permission).collect())
    }

    async fn prepare(
        &self,
        _flow: &RequestFlow,
        core: &Arc<SharedCore>,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        let PendingRequest::Permission(p) = req else {
            return false;
        };
        let sid = p.session_id.clone().unwrap_or_default();
        // `/autoaccept` sessions: answer the request automatically instead of
        // showing a card (mirrors OpenChamber's toggle). Resolves sub-task child
        // sessions up their parent chain so they inherit the parent's flag.
        let auto = should_auto_accept(core, &sid, dir).await;
        if !auto {
            return false;
        }
        match core
            .opencode
            .clone()
            .for_directory(dir)
            .reply_permission(&p.request_id, "once")
            .await
        {
            Ok(()) => tracing::info!(
                "Auto-accepted permission {} on session {} ({})",
                p.request_id,
                sid,
                p.permission.as_deref().unwrap_or("?")
            ),
            Err(e) => tracing::warn!("auto-accept {} on session {}: {}", p.request_id, sid, e),
        }
        true
    }

    fn push_inline(&self, acc: &mut StreamAccumulator, req: &PendingRequest, dir: &str) -> bool {
        let PendingRequest::Permission(p) = req else {
            return false;
        };
        let body = describe_permission(p);
        let sid = p.session_id.clone().unwrap_or_default();
        if acc
            .pending_permissions
            .iter()
            .any(|pp| pp.request_id == p.request_id)
        {
            return false;
        }
        acc.pending_permissions
            .push(crate::bridge::streaming::PendingPermission {
                session_id: sid,
                request_id: p.request_id.clone(),
                body,
                directory: dir.to_string(),
            });
        true
    }

    fn build_card(&self, req: &PendingRequest, dir: &str) -> serde_json::Value {
        let PendingRequest::Permission(p) = req else {
            return serde_json::json!({});
        };
        let body = describe_permission(p);
        crate::feishu::card::build_permission_card(
            p.session_id.as_deref().unwrap_or(""),
            &p.request_id,
            &body,
            dir,
        )
    }

    fn summary(&self, req: &PendingRequest) -> String {
        match req {
            PendingRequest::Permission(p) => describe_permission(p),
            PendingRequest::Question(_) => String::new(),
        }
    }

    fn retain_inline(&self, acc: &mut StreamAccumulator, pending: &std::collections::HashSet<String>) {
        acc.pending_permissions
            .retain(|p| pending.contains(&p.request_id));
    }

    fn claim_kind(&self) -> ClaimKind {
        ClaimKind::Permission
    }

    async fn handle_action(
        &self,
        flow: &RequestFlow,
        core: &Arc<SharedCore>,
        session_id: &str,
        req_id: &str,
        reply: &str,
        value: &serde_json::Value,
        directory: Option<&str>,
        inline: bool,
        host: &Option<String>,
    ) -> Option<CardActionResult> {
        let perm_label = value.get("perm_label").and_then(|v| v.as_str()).unwrap_or("");
        let perm_color = value
            .get("perm_color")
            .and_then(|v| v.as_str())
            .unwrap_or("green");
        let perm_body = value.get("perm_body").and_then(|v| v.as_str()).unwrap_or("");

        // "开启自动授权": turn on the session's Auto-Accept and approve the
        // current (plus any other) pending permission in one interaction — no
        // new message, unlike typing `/autoaccept on`. The backend never sees
        // "autoaccept" as a reply; `set_auto_accept` approves the pending ones
        // with "once" and flips the cola-side flag.
        if reply == "autoaccept" {
            let dir = match directory {
                Some(d) => d.to_string(),
                None => core
                    .sessions
                    .lock()
                    .await
                    .directory_for_session(session_id)
                    .unwrap_or_default(),
            };
            if flow.try_mark_answered(core, req_id).await {
                let approved = core.set_auto_accept(session_id, &dir, true).await;
                // Drop every inline section the toggle just approved (not just
                // the clicked one) so the streaming card re-renders without
                // them synchronously — the poller would otherwise leave them
                // lingering until the next poll notices the requests vanished.
                drop_surfaced(flow, core, host, &approved).await;
                tracing::info!(
                    "Auto-Accept enabled via permission card on session {} (approved {})",
                    session_id,
                    approved.len()
                );
            }
            let mut r = result_card("✅ 已开启自动授权", "blue", "该会话后续权限请求将自动批准。");
            if inline {
                r.card = None;
            }
            r.toast = Some("已开启自动授权".to_string());
            // Re-served verbatim to a losing double-click.
            flow.remember_answered_result(req_id, &r).await;
            return Some(r);
        }

        // Double-click guard: the atomic claim decides who replies. A click
        // that loses the race re-serves the winning click's result; only the
        // winner may reach the backend.
        if !flow.try_mark_answered(core, req_id).await {
            return flow.answered_result(req_id).await;
        }
        // Route the reply to the instance owning the session. The card
        // carries the owning directory (ADR-0010); without it the reply
        // can't be routed, so surface the failure instead of guessing at
        // the server cwd instance.
        let reply_result = match directory {
            Some(dir) => {
                core.opencode
                    .clone()
                    .for_directory(dir)
                    .reply_permission(req_id, reply)
                    .await
            }
            None => Err(crate::error::BridgeError::OpenCode(
                "permission card carries no directory".into(),
            )),
        };
        match reply_result {
            Ok(()) => {
                tracing::info!("Permission reply sent: {} session={}", reply, req_id);
                drop_surfaced(flow, core, host, &[req_id.to_string()]).await;
            }
            // 404: the permission is already resolved — by another client,
            // or by a click replayed after a restart cleared the in-memory
            // guard. Benign: keep the claim, drop its surfaced section, and
            // show the neutral handled card.
            Err(e) if e.is_not_found() => {
                tracing::info!("Permission already resolved: {}", e);
                drop_surfaced(flow, core, host, &[req_id.to_string()]).await;
                let r = already_handled_result(self.label(), inline, "该权限已处理");
                flow.remember_answered_result(req_id, &r).await;
                return Some(r);
            }
            // Genuine failure (network, routing): roll the claim back so a
            // retry can reply again.
            Err(e) => {
                tracing::error!("perm reply failed: {}", e);
                flow.unmark_answered(core, req_id).await;
                return Some(failed_result_card(
                    inline,
                    "该权限请求处理失败，请重试。",
                    "处理失败，请重试",
                ));
            }
        }
        // Result card: shows the decision, no buttons.
        let label = if !perm_label.is_empty() { perm_label } else { reply };
        let toast = match reply {
            "once" => "已允许本次执行".to_string(),
            "always" => "已允许，后续将自动放行".to_string(),
            _ => "已拒绝".to_string(),
        };
        let body = if perm_body.is_empty() {
            format!("Permission: {}", reply)
        } else {
            perm_body.to_string()
        };
        let mut r = result_card(label, perm_color, &body);
        if inline {
            r.card = None;
        }
        r.toast = Some(toast);
        flow.remember_answered_result(req_id, &r).await;
        Some(r)
    }
}

/// Whether a permission request for `session_id` should be auto-accepted.
///
/// Mirrors the `/autoaccept` flag, resolved like `resolve_card_target` does for
/// card delivery: a sub-task child session is NOT in cola's SessionStore, so a
/// direct lookup misses it and it would surface a card even though its parent
/// session has autoaccept on. Walking the parent chain makes the child inherit
/// the parent's flag, consistent with `approve_pending_for_session`.
async fn should_auto_accept(core: &Arc<SharedCore>, session_id: &str, directory: &str) -> bool {
    crate::bridge::pollers::walk_parent_chain(core, session_id, Some(directory), |current| {
        let current = current.to_string();
        async move {
            let sessions = core.sessions.lock().await;
            sessions.entry_for_session(&current).map(|e| e.auto_accept)
        }
    })
    .await
    .unwrap_or(false)
}

/// The question kind: remembers the full request (via the flow, to rebuild
/// cards with partial answers), accumulates answers across slots, and submits
/// once every question is answered (or the user clicks submit/skip). The
/// per-request state lives on the [`RequestFlow`] ([`QuestionState`]), so tests
/// seed it through [`RequestFlow::remember_question`].
pub struct QuestionKind;

#[async_trait::async_trait]
impl RequestKind for QuestionKind {
    fn label(&self) -> &'static str {
        "问答"
    }

    async fn list(
        &self,
        backend: &Arc<dyn opencode::DirectoryBackend>,
    ) -> crate::error::Result<Vec<PendingRequest>> {
        backend
            .list_questions()
            .await
            .map(|v| v.into_iter().map(PendingRequest::Question).collect())
    }

    async fn prepare(
        &self,
        flow: &RequestFlow,
        _core: &Arc<SharedCore>,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        if let PendingRequest::Question(q) = req {
            flow.remember_question(q, dir).await;
        }
        false
    }

    fn push_inline(&self, acc: &mut StreamAccumulator, req: &PendingRequest, dir: &str) -> bool {
        let PendingRequest::Question(q) = req else {
            return false;
        };
        if acc.pending_questions.iter().any(|pq| pq.request_id == q.id) {
            return false;
        }
        acc.pending_questions
            .push(crate::bridge::streaming::PendingQuestion {
                request_id: q.id.clone(),
                session_id: q.session_id.clone(),
                questions: q.questions.clone(),
                directory: dir.to_string(),
                answers: vec![None; q.questions.len()],
                done: vec![false; q.questions.len()],
            });
        true
    }

    fn build_card(&self, req: &PendingRequest, dir: &str) -> serde_json::Value {
        let PendingRequest::Question(q) = req else {
            return serde_json::json!({});
        };
        crate::feishu::card::build_question_card(
            &q.id,
            &q.session_id,
            &q.questions,
            dir,
            &vec![None; q.questions.len()],
            &vec![false; q.questions.len()],
        )
    }

    fn summary(&self, req: &PendingRequest) -> String {
        match req {
            PendingRequest::Question(q) => crate::feishu::card::question_summary(&q.questions),
            PendingRequest::Permission(_) => String::new(),
        }
    }

    fn retain_inline(&self, acc: &mut StreamAccumulator, pending: &std::collections::HashSet<String>) {
        acc.pending_questions.retain(|q| pending.contains(&q.request_id));
    }

    fn claim_kind(&self) -> ClaimKind {
        ClaimKind::Question
    }

    async fn handle_action(
        &self,
        flow: &RequestFlow,
        core: &Arc<SharedCore>,
        session_id: &str,
        req_id: &str,
        reply: &str,
        value: &serde_json::Value,
        directory: Option<&str>,
        inline: bool,
        host: &Option<String>,
    ) -> Option<CardActionResult> {
        match reply {
            // "answer" = an option click (single-select replaces, multi-select
            // toggles). "custom" = a typed custom answer from the form — a
            // multi-select ADDS it to the toggled set (single-select custom
            // answers flow through "answer" as before). "confirm" = the
            // per-question 确定该题 button of a multi-select: locks the toggled
            // set (empty allowed — "不选") into the final answer.
            "answer" | "custom" | "confirm" => {
                let index = value.get("question_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let answer = value
                    .get("answer")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // "confirm" carries no typed answer (the selection is already
                // recorded); every other reply needs one. A blank custom
                // submission is a common accident and gets an explicit hint; a
                // blank option click (malformed/stale payload) stays silent.
                if answer.is_empty() && reply != "confirm" {
                    if reply == "custom" {
                        return Some(CardActionResult {
                            card: None,
                            toast: Some("请输入自定义答案".to_string()),
                        });
                    }
                    return None;
                }
                // A request is submitted ONLY when every question has an answer
                // (`reply_question` expects all of them).
                if flow.is_answered(core, req_id).await {
                    // Finalized before (double-click): re-serve the winning
                    // click's result, falling back to the generic ack while it
                    // is still in flight.
                    return Some(
                        flow.answered_result(req_id)
                            .await
                            .unwrap_or_else(|| question_replay_card(inline)),
                    );
                }
                // #130: no state entry means the request is no longer ours —
                // never reply against a request cola can no longer see.
                if let Some(result) = flow.question_gate(req_id, directory, inline).await {
                    return Some(result);
                }
                // The state must still be live: a gone entry means the card is
                // stale (raced a sweep or a settle). Never a silent no-op.
                let fetched = {
                    let states = flow.question_state.lock().await;
                    states
                        .get(req_id)
                        .map(|state| (state.len(), state.is_multi(index)))
                };
                let Some((n, multi)) = fetched else {
                    return Some(flow.missing_question_result(directory, inline).await);
                };
                // "custom"/"confirm" only exist on multi-select questions.
                if (reply == "custom" || reply == "confirm") && !multi {
                    return None;
                }
                // Multi-select answers compose in the SEPARATE toggles slot;
                // the answers slot holds only FINAL answers (single-select
                // clicks, confirmed multi-selects). The separation is what lets
                // a multi-select stay open while the user keeps toggling, without
                // ever auto-submitting mid-composition.
                let mutated = {
                    let mut states = flow.question_state.lock().await;
                    states.get_mut(req_id).map(|state| {
                        let mut outcome = None;
                        if multi {
                            // Read the selection BEFORE mutating so the toast can
                            // tell add from remove, and a deduped custom from a
                            // fresh one.
                            let selected = state.toggle_selected(index, &answer);
                            match reply {
                                "custom" => {
                                    outcome = Some(if selected {
                                        MultiOutcome::Duplicate
                                    } else {
                                        MultiOutcome::Added
                                    });
                                    state.record_append(index, &answer);
                                }
                                // A stale card may re-confirm an already-done
                                // question (re-render hasn't removed the button
                                // yet, and other questions remain open); `confirm`
                                // is a no-op then, never overwriting the locked
                                // answer with the (now-empty) toggles slot.
                                "confirm" => state.confirm(index),
                                _ => {
                                    outcome = Some(if selected {
                                        MultiOutcome::Removed
                                    } else {
                                        MultiOutcome::Added
                                    });
                                    state.record_toggle(index, &answer);
                                }
                            }
                        } else {
                            state.record_answer(index, &answer);
                        }
                        let (count, display, done) = state.merge();
                        (count, display, done, outcome)
                    })
                };
                let Some((answered_count, display, done, outcome)) = mutated else {
                    return Some(flow.missing_question_result(directory, inline).await);
                };
                // Keep the inline card's display answers and done flags in sync.
                if inline
                    && let Some(pq) = core
                        .cards
                        .lock()
                        .await
                        .get_mut(host.as_deref().unwrap_or(session_id))
                        .map(|c| &mut c.acc)
                        .and_then(|acc| {
                            acc.pending_questions
                                .iter_mut()
                                .find(|pq| pq.request_id == req_id)
                        })
                {
                    pq.answers = display.clone();
                    pq.done = done.clone();
                }
                if answered_count == n {
                    // All questions answered → claim the request and submit.
                    // The claim is atomic: a click that loses the race re-serves
                    // the winning result instead of replying again.
                    if !flow.try_mark_answered(core, req_id).await {
                        return Some(
                            flow.answered_result(req_id)
                                .await
                                .unwrap_or_else(|| question_replay_card(inline)),
                        );
                    }
                    // The claim does not pin the state: re-validate so a sweep
                    // racing the claim cannot submit an empty snapshot.
                    let Some((questions, answers)) = flow.question_snapshot(req_id).await else {
                        return Some(flow.missing_after_claim(core, req_id, directory, inline).await);
                    };
                    if let Some(r) = settle_question_reply(
                        flow,
                        core,
                        req_id,
                        self.label(),
                        reply_question_scoped(core, req_id, Some(&answers), directory).await,
                        inline,
                    )
                    .await
                    {
                        return Some(r);
                    }
                    tracing::info!(
                        "Question answered: {} session={} -> {:?}",
                        req_id,
                        value.get("session_id").and_then(|v| v.as_str()).unwrap_or(""),
                        answers
                    );
                    if inline
                        && let Some(acc) = core
                            .cards
                            .lock()
                            .await
                            .get_mut(host.as_deref().unwrap_or(session_id))
                            .map(|c| &mut c.acc)
                    {
                        acc.pending_questions.retain(|pq| pq.request_id != req_id);
                    }
                    let mut r = result_card("✅ 已回答", "green", &qa_completion_body(&questions, &answers));
                    if inline {
                        r.card = None;
                    }
                    r.toast = Some("已回答".to_string());
                    if inline {
                        flush_inline_card(core, host, session_id).await;
                    }
                    // Re-served verbatim to a losing double-click.
                    flow.remember_answered_result(req_id, &r).await;
                    Some(r)
                } else if inline {
                    // Inline: the streaming card re-renders with the updated
                    // display state. Return the re-rendered card IN the
                    // callback response so Feishu updates the CLICKED card in
                    // place — the same mechanism standalone question cards use,
                    // and the only one that reliably refreshes the body after an
                    // interaction. A separate PATCH around the callback leaves
                    // the card on its pre-answer state (the markers/已选 never
                    // become visible), while a PATCH before the click did add the
                    // buttons.
                    let mut r = CardActionResult {
                        card: None,
                        toast: None,
                    };
                    r.toast = Some(action_toast(reply, n - answered_count, outcome));
                    // Build on a CLONE so a card-component-limit split can't
                    // advance the live accumulator's `render_from` from inside
                    // the click handler (flush_card owns that flow). Only take
                    // the card when it fits without splitting; otherwise fall
                    // back to the PATCH re-render, which finalizes the card and
                    // sends the continuation card.
                    let rebuilt = {
                        let mut cards = core.cards.lock().await;
                        cards.get_mut(host.as_deref().unwrap_or(session_id)).map(|c| {
                            let mut probe = c.acc.clone();
                            probe.build_card_with_split()
                        })
                    };
                    match rebuilt {
                        Some((card, false)) => r.card = Some(card),
                        _ => {
                            // Split needed, or no live accumulator (the turn
                            // ended between the click and now): PATCH re-render.
                            flush_inline_card(core, host, session_id).await;
                        }
                    }
                    Some(r)
                } else {
                    // Still questions left: return an updated card that shows the
                    // answered ones as done and the rest open.
                    let remaining = n - answered_count;
                    let req = {
                        let states = flow.question_state.lock().await;
                        states.get(req_id).map(|s| s.request().clone())
                    };
                    let Some(req) = req else {
                        return Some(flow.missing_question_result(directory, inline).await);
                    };
                    let card = crate::feishu::card::build_question_card(
                        req_id,
                        &req.session_id,
                        &req.questions,
                        directory.unwrap_or(""),
                        &display,
                        &done,
                    );
                    let mut r = CardActionResult {
                        card: Some(card),
                        toast: None,
                    };
                    r.toast = Some(action_toast(reply, remaining, outcome));
                    Some(r)
                }
            }
            "submit" => {
                // Finalize with whatever was answered (empty for the rest).
                if flow.is_answered(core, req_id).await {
                    return Some(
                        flow.answered_result(req_id)
                            .await
                            .unwrap_or_else(|| question_replay_card(inline)),
                    );
                }
                // #130: same gate as "answer" — a state-less click must not
                // blind-reply. Placed before the claim so a gated click leaves
                // no answered mark behind.
                if let Some(result) = flow.question_gate(req_id, directory, inline).await {
                    return Some(result);
                }
                // Atomic claim: a click that loses the race re-serves the
                // winning result.
                if !flow.try_mark_answered(core, req_id).await {
                    return Some(
                        flow.answered_result(req_id)
                            .await
                            .unwrap_or_else(|| question_replay_card(inline)),
                    );
                }
                // The claim does not pin the state: re-validate so a sweep
                // racing the claim cannot turn this into a blind reply.
                let Some((questions, answers)) = flow.question_snapshot(req_id).await else {
                    return Some(flow.missing_after_claim(core, req_id, directory, inline).await);
                };
                if let Some(r) = settle_question_reply(
                    flow,
                    core,
                    req_id,
                    self.label(),
                    reply_question_scoped(core, req_id, Some(&answers), directory).await,
                    inline,
                )
                .await
                {
                    return Some(r);
                }
                tracing::info!(
                    "Question submitted: {} session={} -> {:?}",
                    req_id,
                    value.get("session_id").and_then(|v| v.as_str()).unwrap_or(""),
                    answers
                );
                if inline
                    && let Some(acc) = core
                        .cards
                        .lock()
                        .await
                        .get_mut(host.as_deref().unwrap_or(session_id))
                        .map(|c| &mut c.acc)
                {
                    acc.pending_questions.retain(|pq| pq.request_id != req_id);
                }
                let mut r = result_card("✅ 已回答", "green", &qa_completion_body(&questions, &answers));
                if inline {
                    r.card = None;
                }
                r.toast = Some("已提交".to_string());
                if inline {
                    flush_inline_card(core, host, session_id).await;
                }
                // Re-served verbatim to a losing double-click.
                flow.remember_answered_result(req_id, &r).await;
                Some(r)
            }
            "reject" => {
                if flow.is_answered(core, req_id).await {
                    return Some(flow.answered_result(req_id).await.unwrap_or_else(|| {
                        let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                        if inline {
                            r.card = None;
                        }
                        r
                    }));
                }
                // #130: same gate as "answer" — a state-less click must not
                // blind-reply. Placed before the claim so a gated click leaves
                // no answered mark behind.
                if let Some(result) = flow.question_gate(req_id, directory, inline).await {
                    return Some(result);
                }
                // Atomic claim: a click that loses the race re-serves the
                // winning result.
                if !flow.try_mark_answered(core, req_id).await {
                    return Some(flow.answered_result(req_id).await.unwrap_or_else(|| {
                        let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                        if inline {
                            r.card = None;
                        }
                        r
                    }));
                }
                // The claim does not pin the state: re-validate so a sweep
                // racing the claim cannot reject against a vanished request.
                if !flow.question_state_live(req_id).await {
                    return Some(flow.missing_after_claim(core, req_id, directory, inline).await);
                }
                if let Some(r) = settle_question_reply(
                    flow,
                    core,
                    req_id,
                    self.label(),
                    reply_question_scoped(core, req_id, None, directory).await,
                    inline,
                )
                .await
                {
                    return Some(r);
                }
                tracing::info!("Question rejected: {}", req_id);
                if inline
                    && let Some(acc) = core
                        .cards
                        .lock()
                        .await
                        .get_mut(host.as_deref().unwrap_or(session_id))
                        .map(|c| &mut c.acc)
                {
                    acc.pending_questions.retain(|pq| pq.request_id != req_id);
                }
                let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                if inline {
                    r.card = None;
                }
                r.toast = Some("已拒绝回答".to_string());
                if inline {
                    flush_inline_card(core, host, session_id).await;
                }
                // Re-served verbatim to a losing double-click.
                flow.remember_answered_result(req_id, &r).await;
                Some(r)
            }
            _ => None,
        }
    }
}

/// The fused permission/question flow. The poll loop, card delivery, double-click
/// guard primitive and error-result block live here once; the kind supplies the
/// deltas.
pub struct RequestFlow {
    kind: Box<dyn RequestKind>,
    /// How often (milliseconds) the poll loop lists pending requests. Defaults
    /// to today's 3 s cadence; tests store a small value so every branch is
    /// exercisable without sleeping real seconds.
    pub poll_interval_ms: std::sync::atomic::AtomicU64,
    /// How long (milliseconds) one pending-request list call may take before
    /// the poll loop gives up on it and retries next tick. Bounds a request
    /// stuck on a half-open connection (e.g. across a server restart) so one
    /// hung call cannot freeze the poller forever. Defaults to 30 s; tests
    /// store a small value so the timeout branch runs without sleeping.
    pub list_timeout_ms: std::sync::atomic::AtomicU64,
    /// request_id → (card message_id, description) of the card cola sent (used
    /// to mark a card stale when the request is resolved by ANOTHER client).
    pub sent_cards: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// Directories whose list call has SUCCEEDED at least once in this process
    /// (#130). A question card only exists after its state was recorded, so
    /// for one of these directories a missing state entry proves the request
    /// left the pending list — the late-click classifier uses this instead of
    /// remembering every pruned request id.
    listed_dirs: Arc<Mutex<std::collections::HashSet<String>>>,
    /// request_id → the question request's whole in-flight state: the request,
    /// its owning directory, the finalized answers and the live multi-select
    /// toggles (question kind only; the permission flow's map stays empty). One
    /// entry per request, so no transition can update one field without the
    /// others.
    question_state: Arc<Mutex<HashMap<String, QuestionState>>>,
    /// request_id → the WINNING click's result. A losing click re-serves this
    /// instead of rendering its own decision — otherwise a rapid second click
    /// on a different button (允许一次 vs 总是允许) would flip the card to a
    /// decision the backend never received. In-memory like the claim set on
    /// `core.answered_requests`, and one card per answered request.
    answered_results: Arc<Mutex<HashMap<String, CardActionResult>>>,
}

impl RequestFlow {
    pub fn new(kind: Box<dyn RequestKind>) -> Self {
        Self {
            kind,
            poll_interval_ms: std::sync::atomic::AtomicU64::new(3000),
            list_timeout_ms: std::sync::atomic::AtomicU64::new(30_000),
            sent_cards: Arc::new(Mutex::new(HashMap::new())),
            listed_dirs: Arc::new(Mutex::new(std::collections::HashSet::new())),
            question_state: Arc::new(Mutex::new(HashMap::new())),
            answered_results: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The double-click guard's read-only check: whether `req_id` was already
    /// answered. Used to re-serve a result to a late click; the atomic
    /// [`Self::try_mark_answered`] is what decides who may reply.
    pub(crate) async fn is_answered(&self, core: &Arc<SharedCore>, req_id: &str) -> bool {
        core.answered_requests.lock().await.contains(req_id)
    }

    /// Atomically claim `req_id` for this click: `true` when it was not yet
    /// answered (this click may reply), `false` when another click already
    /// claimed it (re-serve the first result, never reply again). The
    /// check-and-insert happens under ONE lock, so two near-simultaneous clicks
    /// cannot both win the way the old `is_answered` + `mark_answered` pair
    /// could.
    pub(crate) async fn try_mark_answered(&self, core: &Arc<SharedCore>, req_id: &str) -> bool {
        core.answered_requests.lock().await.insert(req_id.to_string())
    }

    /// Roll back a claim made by [`Self::try_mark_answered`] after a GENUINE
    /// reply failure, so the user can retry. A benign 404 ("already resolved")
    /// keeps the claim — the request really is gone.
    pub(crate) async fn unmark_answered(&self, core: &Arc<SharedCore>, req_id: &str) {
        core.answered_requests.lock().await.remove(req_id);
    }

    /// Record the winning click's result so a click that loses the guard race
    /// re-serves the SAME result instead of rendering its own decision (which
    /// could disagree with what actually reached the backend).
    pub(crate) async fn remember_answered_result(&self, req_id: &str, result: &CardActionResult) {
        self.answered_results
            .lock()
            .await
            .insert(req_id.to_string(), result.clone());
    }

    /// The winning click's result, if it has settled. `None` while the winner
    /// is still in flight — the losing click then renders a generic replay.
    pub(crate) async fn answered_result(&self, req_id: &str) -> Option<CardActionResult> {
        self.answered_results.lock().await.get(req_id).cloned()
    }

    /// Remember a pending question request so its card/block buttons can
    /// resolve: the full request (card rebuilds) and its owning directory
    /// (routing a reply whose callback no longer carries one). Refreshing an
    /// already-remembered request keeps its recorded answers/toggles.
    pub(crate) async fn remember_question(&self, req: &opencode::client::QuestionRequest, dir: &str) {
        let mut states = self.question_state.lock().await;
        match states.entry(req.id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().refresh(req, dir);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(QuestionState::new(req.clone(), dir.to_string()));
            }
        }
    }

    /// Whether `req_id` is a remembered (pending) question request. Test-only
    /// today: the production poll loop tracks self-surfaced ids in its own
    /// `seen` set.
    #[cfg(test)]
    pub(crate) async fn has_question(&self, req_id: &str) -> bool {
        self.question_state.lock().await.contains_key(req_id)
    }

    /// The remembered owning directory of a question request, used as the
    /// fallback when a card callback carries none.
    pub(crate) async fn question_dir(&self, req_id: &str) -> Option<String> {
        self.question_state
            .lock()
            .await
            .get(req_id)
            .map(|s| s.dir().to_string())
    }

    /// Drop a question request's in-flight state once it is finalized: the
    /// remembered request, its owning directory, the final answers and any live
    /// multi-select toggles. `sent_cards` is cleared only where the request was
    /// actually delivered as its own card.
    pub(crate) async fn remove_question(&self, req_id: &str) {
        self.question_state.lock().await.remove(req_id);
    }

    /// The remembered questions and their current answers, captured for the
    /// reply payload and the completion card. The answers are CLONED, not
    /// consumed: a genuine reply failure rolls the guard claim back and the
    /// state must stay intact so the click can be retried. `None` when the
    /// state vanished (a sweep raced the click) — the caller must classify
    /// that (#130), never reply from an empty snapshot.
    async fn question_snapshot(
        &self,
        req_id: &str,
    ) -> Option<(Vec<opencode::client::QuestionInfo>, Vec<Vec<String>>)> {
        let states = self.question_state.lock().await;
        let state = states.get(req_id)?;
        let answers = state
            .answers()
            .iter()
            .map(|a| a.clone().unwrap_or_default())
            .collect();
        Some((state.request().questions.clone(), answers))
    }

    /// The live answer state (display + done flags) of the question requests
    /// among `pending`, for a snapshot re-render after an interaction — the
    /// 已选/✅ markers the standalone question cards show.
    pub(crate) async fn live_snapshot_state(
        &self,
        pending: &[PendingRequest],
    ) -> crate::feishu::snapshot_card::SnapshotQuestionState {
        let states = self.question_state.lock().await;
        let mut out = crate::feishu::snapshot_card::SnapshotQuestionState::new();
        for req in pending {
            if let PendingRequest::Question(q) = req {
                let (_, display, done) = match states.get(&q.id) {
                    Some(state) => state.merge(),
                    None => (0, vec![None; q.questions.len()], vec![false; q.questions.len()]),
                };
                out.insert(
                    q.id.clone(),
                    crate::feishu::snapshot_card::QuestionBlockState { display, done },
                );
            }
        }
        out
    }

    /// The result for a click whose question state is gone (#130). It never
    /// reaches the backend and never records anything: a directory this
    /// process has listed successfully means the request provably left the
    /// pending list → neutral "已处理"; otherwise cola cannot know (fresh
    /// process before the first sweep, failing lists, unknown directory) and
    /// must not claim the request was handled.
    async fn missing_question_result(&self, directory: Option<&str>, inline: bool) -> CardActionResult {
        let classified = match directory {
            Some(dir) => self.listed_dirs.lock().await.contains(dir),
            None => false,
        };
        if classified {
            already_handled_result(self.kind.label(), inline, "该问题已处理")
        } else {
            stale_question_card(inline)
        }
    }

    /// Whether `req_id` still has live question state.
    async fn question_state_live(&self, req_id: &str) -> bool {
        self.question_state.lock().await.contains_key(req_id)
    }

    /// The #130 gate in front of every question click: `Some(result)` when the
    /// state is gone — the click must not reply, so it gets the classified
    /// result; `None` when the state is live and the caller may proceed.
    async fn question_gate(
        &self,
        req_id: &str,
        directory: Option<&str>,
        inline: bool,
    ) -> Option<CardActionResult> {
        if self.question_state_live(req_id).await {
            return None;
        }
        Some(self.missing_question_result(directory, inline).await)
    }

    /// The classified result for a click whose state vanished AFTER it claimed
    /// the request: roll the claim back so the reply never happens and the
    /// fresh card can still answer.
    async fn missing_after_claim(
        &self,
        core: &Arc<SharedCore>,
        req_id: &str,
        directory: Option<&str>,
        inline: bool,
    ) -> CardActionResult {
        self.unmark_answered(core, req_id).await;
        self.missing_question_result(directory, inline).await
    }

    /// Independent poller: surfaces pending requests as cards (inline on a
    /// streaming card when possible, else a separate card), auto-resolves where
    /// the kind says so, and marks stale cards when another client resolves a
    /// request. Spawned once per kind at App startup.
    pub(crate) async fn poll_loop(&self, core: &Arc<SharedCore>) -> crate::error::Result<()> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(
                self.poll_interval_ms.load(std::sync::atomic::Ordering::Relaxed),
            ))
            .await;
            self.sweep(core, &mut seen).await;
        }
    }

    /// One poll iteration: list pending requests per known session directory,
    /// surface the unseen ones, then reconcile everything that left the list
    /// (stale standalone cards, inline sections, snapshot claims, and the
    /// remembered question state). Extracted from the loop so tests can drive
    /// one deterministic sweep.
    pub(crate) async fn sweep(&self, core: &Arc<SharedCore>, seen: &mut std::collections::HashSet<String>) {
        // Serverless (Lazy Start hasn't attached/spawned yet): there is
        // nothing to poll — skip quietly until a server appears.
        if core.opencode.base_url().is_empty() {
            return;
        }
        // Pending requests live in the server instance for the session's
        // directory; `GET /permission` / `GET /question` must be scoped with
        // `?directory=` or they only see the server cwd instance. Check every
        // known session directory.
        let directories = { core.sessions.lock().await.directories() };
        let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Directories whose list call failed (error or timeout) this sweep.
        // `pending` only speaks for directories that listed SUCCESSFULLY, so
        // their state must NOT be read as resolved (#130).
        let mut failed_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Directories that DID list successfully this sweep, remembered on the
        // flow so a later late click can classify a missing state entry.
        let mut listed_now: std::collections::HashSet<String> = std::collections::HashSet::new();
        for dir in &directories {
            let backend = core.opencode.clone().for_directory(dir);
            // Bound the list call: a server restart can leave an in-flight
            // request on a half-open connection, and without a timeout the
            // poller would sit on it forever (no cards, no auto-accept).
            // On timeout the call is cancelled and the next tick retries.
            let listed = match crate::bridge::bounded_call(
                &format!("poll {} ({}) list", self.kind.label(), dir),
                self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
                self.kind.list(&backend),
            )
            .await
            {
                Some(listed) => listed,
                None => {
                    failed_dirs.insert(dir.clone());
                    continue;
                }
            };
            match listed {
                Ok(requests) => {
                    listed_now.insert(dir.clone());
                    for req in &requests {
                        pending.insert(req.id().to_string());
                        // ADR-0028: a claimed request is hosted by a
                        // snapshot card — already surfaced, never a
                        // standalone card, never re-inlined.
                        if core.snapshot_claims.lock().await.contains(req.id()) {
                            continue;
                        }
                        if seen.contains(req.id()) {
                            continue;
                        }
                        seen.insert(req.id().to_string());
                        tracing::info!(
                            "{} ({}): {} on session {}",
                            self.kind.label(),
                            dir,
                            req.id(),
                            req.session_id()
                        );
                        // Kind-specific pre-card handling (auto-accept /
                        // remember). true → handled, no card needed.
                        if self.kind.prepare(self, core, req, dir).await {
                            continue;
                        }
                        // One-card-per-turn: surface the request INLINE on the
                        // streaming card of the session that owns it — the
                        // session itself, or (sub-task children) its nearest
                        // ancestor with a live card. Only a separate card when
                        // there is no active card (e.g. external turns or
                        // restarts).
                        if let Some(host) = inline_host_session(core, req.session_id(), Some(dir)).await {
                            let mut cards = core.cards.lock().await;
                            if let Some(acc) = cards.get_mut(&host).map(|c| &mut c.acc)
                                && self.kind.push_inline(acc, req, dir)
                            {
                                tracing::info!(
                                    "{} {} inlined on session {} card",
                                    self.kind.label(),
                                    req.id(),
                                    host
                                );
                                drop(cards);
                                // Flush so the inline section appears NOW — the
                                // render loop only flushes on new parts, and a
                                // blocked prompt produces none.
                                crate::bridge::render::flush_card(core, &host).await;
                            }
                            continue;
                        }
                        let card = self.kind.build_card(req, dir);
                        // Reply to the message that triggered the prompt for
                        // this session; fall back to sending into the chat when
                        // the accumulator is gone (e.g. after a cola restart).
                        // Sub-task sessions resolve up the parent chain.
                        let sent_id = match resolve_card_target(core, req.session_id(), dir).await {
                            Some(CardTarget::ReplyTo(msg_id)) => {
                                core.feishu.reply_card(&msg_id, &card).await.ok()
                            }
                            Some(CardTarget::Chat(chat_id)) => {
                                core.feishu.send_card("chat_id", &chat_id, &card).await.ok()
                            }
                            None => {
                                tracing::warn!(
                                    "No reply target or chat for {} on session {}",
                                    self.kind.label(),
                                    req.session_id()
                                );
                                None
                            }
                        };
                        if let Some(mid) = sent_id {
                            self.sent_cards
                                .lock()
                                .await
                                .insert(req.id().to_string(), (mid, self.kind.summary(req)));
                        } else {
                            tracing::warn!(
                                "{} card send failed on session {}",
                                self.kind.label(),
                                req.session_id()
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("poll {} ({}): {}", self.kind.label(), dir, e);
                    failed_dirs.insert(dir.clone());
                }
            }
        }
        // Mark stale: a card cola sent whose request is no longer pending
        // (resolved by another client) and was NOT answered by cola.
        mark_stale_cards(core, &pending, &self.sent_cards, self.kind.label()).await;
        // Drop inline sections whose request vanished (answered elsewhere) —
        // the streaming card re-renders without them.
        {
            let mut cards = core.cards.lock().await;
            for card in cards.values_mut() {
                self.kind.retain_inline(&mut card.acc, &pending);
            }
        }
        // ADR-0028: a claimed request that left the pending list was
        // resolved — by the snapshot's own buttons (the click handler
        // re-renders synchronously; this just cleans the registry) or by
        // another client (the block must drop from the snapshot). The
        // snapshot card itself is never marked stale — that targets
        // standalone cards. The registry returns the rebuilt cards; the
        // flow patches them (never holding the registry lock across the
        // Feishu call).
        let dropped = core
            .snapshot_claims
            .lock()
            .await
            .drop_vanished(self.kind.claim_kind(), &pending);
        for (message_id, card) in dropped {
            if let Err(e) = core.feishu.update_message(&message_id, &card).await {
                tracing::warn!("snapshot claim drop: card update failed: {}", e);
            } else {
                tracing::info!("snapshot {} re-rendered without resolved claims", message_id);
            }
        }
        // #130: remember which directories listed successfully — with that
        // knowledge a missing state entry proves the request left pending.
        // A directory that left the session store is forgotten: pending can no
        // longer speak for it, so its pruned state must NOT be classified as
        // resolved later. (Scoped so the lock never overlaps question_state.)
        {
            let mut known = self.listed_dirs.lock().await;
            known.retain(|dir| directories.contains(dir));
            known.extend(listed_now);
        }
        // #130: forget question state whose request left the pending list —
        // the flip side of the stale-card / inline / claim cleanups above.
        // A directory that failed to list is retained: we do not know it
        // resolved, and unknown must never be read as resolved.
        self.question_state
            .lock()
            .await
            .retain(|id, state| pending.contains(id) || failed_dirs.contains(state.dir()));
    }

    /// Handle a card action on this kind's card: answer / submit / reject. The
    /// shared skeleton resolves the delivery context and the double-click guard;
    /// the kind applies its own semantics.
    pub(crate) async fn handle_card_action(
        &self,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let session_id = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        let request_id = value.get("request_id").and_then(|v| v.as_str());
        let req_id = request_id?;
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let directory = match directory {
            Some(d) => Some(d),
            None => {
                let from_store = core.sessions.lock().await.directory_for_session(session_id);
                if from_store.is_some() {
                    from_store
                } else {
                    self.question_dir(req_id).await
                }
            }
        };
        let directory = directory.as_deref();

        let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
        // ADR-0028: a claimed request's block lives on a static snapshot card,
        // NOT a streaming card — its click is not "inline", and the ack must
        // patch the snapshot (block updated or dropped), never a standalone
        // card.
        let claimed_message = {
            let registry = core.snapshot_claims.lock().await;
            registry.claim_of(req_id).map(|(m, _)| m.to_string())
        };
        // Inline interaction: the session (or its sub-task parent chain) has a
        // live streaming card, so the result is NOT returned as a replacement
        // card — the streaming card re-renders itself on the next poll.
        let host = if claimed_message.is_some() {
            None
        } else if core.cards.lock().await.contains_key(session_id) {
            Some(session_id.to_string())
        } else {
            inline_host_session(core, session_id, directory).await
        };
        let inline = host.is_some();

        let mut r = self
            .kind
            .handle_action(
                self, core, session_id, req_id, reply, value, directory, inline, &host,
            )
            .await;

        // A late second click on a block already resolved from this snapshot
        // (the double-click race: the first click's re-render is still in
        // flight): keep patching the snapshot — never replace the whole card
        // with a standalone result card.
        if claimed_message.is_none()
            && core.snapshot_claims.lock().await.is_tombstoned(req_id)
            && let Some(result) = &mut r
        {
            result.card = None;
        }
        if let Some(result) = &mut r
            && let Some(message_id) = claimed_message
        {
            // Did THIS click resolve the request? The double-click guard
            // cannot tell a successful reply from a failed one, so check the
            // server: the block drops only once the request is no longer
            // pending. A failed reply (already resolved elsewhere) also leaves
            // it pending → the block stays and the poll sweep cleans it.
            let still_pending = match directory {
                Some(dir) => self
                    .kind
                    .list(&core.opencode.clone().for_directory(dir))
                    .await
                    .map(|reqs| reqs.iter().any(|r| r.id() == req_id))
                    .unwrap_or(true),
                None => true,
            };
            if still_pending {
                // The click recorded state (question) or was a no-op
                // (double-click): keep the block and show the live answer
                // state, so the snapshot mirrors the standalone card's
                // 已选/✅ markers.
                let pending = core
                    .snapshot_claims
                    .lock()
                    .await
                    .host_pending(&message_id)
                    .unwrap_or_default();
                let state = core.question.live_snapshot_state(&pending).await;
                result.card = core.snapshot_claims.lock().await.rebuild(&message_id, &state);
            } else {
                // Resolved: drop the block from the snapshot and patch it in
                // place — no new message. `resolve` does the whole transition
                // (unclaim → tombstone → rebuild → prune) under one lock.
                let state = crate::feishu::snapshot_card::SnapshotQuestionState::new();
                result.card = core
                    .snapshot_claims
                    .lock()
                    .await
                    .resolve(req_id, &message_id, &state);
            }
        }
        r
    }
}

/// The error-result block, shared by every kind: a red "处理失败" card (kept in
/// place when answered inline), with a toast that explains the failure.
fn failed_result_card(inline: bool, body: &str, toast: &str) -> CardActionResult {
    let mut r = result_card("⚠️ 处理失败", "red", body);
    if inline {
        r.card = None;
    }
    r.toast = Some(toast.to_string());
    r
}

/// The neutral result for a request the backend no longer has (404): it was
/// already resolved — by another client, or by a click replayed after a cola
/// restart cleared the in-memory guard. NOT a failure, so it must never render
/// the red failure card. Reuses the stale-card body so both "resolved
/// elsewhere" paths look identical.
fn already_handled_result(kind: &str, inline: bool, toast: &str) -> CardActionResult {
    let mut r = CardActionResult {
        card: Some(crate::feishu::card::build_resolved_elsewhere_card(kind, "")),
        toast: Some(toast.to_string()),
    };
    if inline {
        r.card = None;
    }
    r
}

/// Apply a question reply's outcome to the guard and the in-flight state:
/// - `Ok`: the reply landed — drop the request's remembered state and return
///   `None` (the caller renders its completion card).
/// - 404: resolved elsewhere — drop the state, remember the neutral card (so a
///   losing click re-serves it) and return it.
/// - other: genuine failure — keep the state, roll the claim back for a retry,
///   and return the failure card.
async fn settle_question_reply(
    flow: &RequestFlow,
    core: &Arc<SharedCore>,
    req_id: &str,
    kind: &str,
    result: crate::error::Result<()>,
    inline: bool,
) -> Option<CardActionResult> {
    match result {
        Ok(()) => {
            flow.remove_question(req_id).await;
            flow.sent_cards.lock().await.remove(req_id);
            None
        }
        Err(e) if e.is_not_found() => {
            tracing::info!("Question already resolved: {}", e);
            flow.remove_question(req_id).await;
            flow.sent_cards.lock().await.remove(req_id);
            let r = already_handled_result(kind, inline, "该问题已处理");
            flow.remember_answered_result(req_id, &r).await;
            Some(r)
        }
        Err(e) => {
            tracing::error!("question reply failed: {}", e);
            flow.unmark_answered(core, req_id).await;
            Some(failed_result_card(
                inline,
                "该问题处理失败，请重试。",
                "处理失败，请重试",
            ))
        }
    }
}

/// Drop surfaced cards for a set of answered request ids: remove the ids from
/// the flow's `sent_cards` (so the poller doesn't keep delivering their cards)
/// and strip their sections from the host streaming card's inline accumulator.
/// A single request passes one id; the Auto-Accept toggle passes every id it
/// approved so all inline sections re-render away at once.
///
/// When an inline section was actually stripped, the host card is re-rendered
/// immediately: a permission-blocked prompt produces no new parts, so the
/// render poll would otherwise leave the resolved section (and its still-live
/// buttons) on the Feishu card until the AI resumes — or forever, if the turn
/// was stopped. Same reason the question paths flush explicitly.
async fn drop_surfaced(flow: &RequestFlow, core: &Arc<SharedCore>, host: &Option<String>, ids: &[String]) {
    {
        let mut sent = flow.sent_cards.lock().await;
        for id in ids {
            sent.remove(id);
        }
    }
    let mut dropped_inline = false;
    if let Some(host) = host
        && let Some(acc) = core.cards.lock().await.get_mut(host).map(|c| &mut c.acc)
    {
        let before = acc.pending_permissions.len();
        acc.pending_permissions.retain(|p| !ids.contains(&p.request_id));
        dropped_inline = acc.pending_permissions.len() != before;
    }
    if dropped_inline {
        // `dropped_inline` implies `host` is Some; the fallback is unused.
        flush_inline_card(core, host, host.as_deref().unwrap_or_default()).await;
    }
}

/// Re-render the live streaming card so inline question state (✅/已选, removed
/// buttons) shows immediately. A question-blocked prompt produces no new parts,
/// so without an explicit flush the render poll never fires until the AI
/// resumes — the card would stay frozen on the pre-answer state.
async fn flush_inline_card(core: &Arc<SharedCore>, host: &Option<String>, session_id: &str) {
    crate::bridge::render::flush_card(core, host.as_deref().unwrap_or(session_id)).await;
}

/// Route a question reply/reject to the instance owning the session. The card
/// carries the owning directory (ADR-0010); without it the reply can't be
/// routed, so surface the failure instead of guessing at the server cwd instance.
async fn reply_question_scoped(
    core: &Arc<SharedCore>,
    req_id: &str,
    answers: Option<&[Vec<String>]>,
    directory: Option<&str>,
) -> crate::error::Result<()> {
    let Some(dir) = directory else {
        return Err(crate::error::BridgeError::OpenCode(
            "question card carries no directory".into(),
        ));
    };
    let backend = core.opencode.clone().for_directory(dir);
    match answers {
        Some(a) => backend.reply_question(req_id, a).await,
        None => backend.reject_question(req_id).await,
    }
}

/// Map an OpenCode permission action to a friendly emoji + Chinese description.
fn describe_action(action: &str) -> (&'static str, &'static str) {
    match action {
        "bash" => ("⚡", "执行 Shell 命令"),
        "read" => ("📖", "读取文件"),
        "write" => ("✏️", "修改文件"),
        "edit" => ("✏️", "编辑文件"),
        "patch" => ("✏️", "应用补丁"),
        "webfetch" => ("🌐", "访问网页"),
        "fetch" => ("🌐", "获取网络资源"),
        "external_directory" => ("📁", "访问外部目录"),
        "kill" => ("🛑", "终止进程"),
        "ls" => ("📂", "列出目录"),
        "list" => ("📂", "列出内容"),
        "rename" => ("🔤", "重命名"),
        "remove" | "delete" | "rm" => ("🗑️", "删除文件"),
        "mkdir" => ("📁", "创建目录"),
        "move" | "mv" => ("📦", "移动文件"),
        "copy" | "cp" => ("📋", "复制文件"),
        "link" | "ln" => ("🔗", "创建链接"),
        "install" => ("📥", "安装依赖"),
        "test" => ("🧪", "运行测试"),
        "build" => ("🏗️", "构建项目"),
        "git" => ("🌿", "执行 Git 操作"),
        _ => ("🔐", "执行操作"),
    }
}

/// Clip `s` to at most `max` characters, appending a "…" marker when it was
/// cut. Character-counted (not bytes) so CJK paths/values truncate at the same
/// visual length as ASCII — mirrors `card::truncate_md`.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// Cap for an edit diff shown on a permission card. The request body is one
/// card element with no further chunking on the standalone card, so a huge
/// diff must be clipped here to stay under Feishu's card size limit.
const PERMISSION_DIFF_MAX_CHARS: usize = 1200;

/// A friendly description of what a permission request asks to do, shown on the
/// permission card (and reused verbatim by the snapshot card's pending block,
/// ADR-0028 — the adopt-time block must look and behave like today's cards).
pub(crate) fn describe_permission(p: &opencode::client::PermissionRequest) -> String {
    let action = p.permission.as_deref().unwrap_or("?");
    let (emoji, label) = describe_action(action);
    let mut s = format!("{} **{}**\n", emoji, label);

    if !p.patterns.is_empty() {
        s.push_str("**对象**:\n");
        for r in &p.patterns {
            s.push_str(&format!("- `{}`\n", truncate(r, 150)));
        }
    }

    // An edit/patch/apply_patch permission carries its unified diff (metadata
    // `{filepath, diff}` from OpenCode's edit tool) — that IS the request, so
    // show the actual changed lines instead of dumping raw metadata JSON (which
    // used to make the card show a wall of escaped diff text).
    let diff = p
        .metadata
        .as_ref()
        .and_then(|m| m.get("diff"))
        .and_then(|v| v.as_str())
        .filter(|d| !d.is_empty());
    if matches!(action, "edit" | "patch" | "apply_patch")
        && let Some(diff) = diff
    {
        let parsed = crate::feishu::card::parse_edit_diff(diff);
        let path = p
            .metadata
            .as_ref()
            .and_then(|m| m.get("filepath"))
            .and_then(|v| v.as_str())
            .or_else(|| parsed.as_ref().and_then(|d| d.path.as_deref()))
            .or_else(|| p.patterns.first().map(String::as_str))
            .unwrap_or("?");
        match parsed.as_ref() {
            Some(d) => s.push_str(&format!(
                "**改动**: 📄 `{}` (+{} −{})\n",
                truncate(path, 200),
                d.additions,
                d.deletions
            )),
            None => s.push_str(&format!("**改动**: 📄 `{}`\n", truncate(path, 200))),
        }
        let body = parsed.map(|d| d.body).unwrap_or_else(|| diff.to_string());
        let body = crate::feishu::card::truncate_md(&body, PERMISSION_DIFF_MAX_CHARS);
        s.push_str(&crate::feishu::card::fenced_code(&body, None));
        s.push('\n');
    } else if let Some(meta) = &p.metadata {
        // Metadata often carries richer context (e.g. bash command, tool input)
        let mut shown = 0;
        for (key, label) in [
            ("command", "命令"),
            ("cwd", "目录"),
            ("description", "说明"),
            ("input", "输入"),
            ("path", "路径"),
        ] {
            if let Some(v) = meta.get(key) {
                let val = v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string());
                s.push_str(&format!("**{}**: `{}`\n", label, truncate(&val, 200)));
                shown += 1;
                if shown >= 3 {
                    break;
                }
            }
        }
        if shown == 0 {
            let compact = serde_json::to_string(meta).unwrap_or_default();
            if compact.len() > 300 {
                s.push_str(&format!("**详情**: `{}`", truncate(&compact, 300)));
            } else if !compact.is_empty() && compact != "{}" {
                s.push_str(&format!("**详情**: `{}`", compact));
            }
        }
    }

    s.push_str("\nAI 想要执行这个操作，是否允许？");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perm(action: &str, metadata: serde_json::Value) -> crate::opencode::client::PermissionRequest {
        crate::opencode::client::PermissionRequest {
            request_id: "per_1".into(),
            session_id: Some("ses_1".into()),
            permission: Some(action.into()),
            patterns: vec!["/proj/src/main.rs".into()],
            metadata: Some(metadata),
            always: vec![],
        }
    }

    #[test]
    fn describe_edit_permission_shows_diff_not_raw_metadata() {
        let diff = "\
Index: /proj/src/main.rs
===================================================================
--- /proj/src/main.rs
+++ /proj/src/main.rs
@@ -1 +1 @@
-old line
+new line";
        let p = perm(
            "edit",
            serde_json::json!({ "filepath": "/proj/src/main.rs", "diff": diff }),
        );
        let s = describe_permission(&p);
        assert!(
            s.contains("**改动**: 📄 `/proj/src/main.rs` (+1 −1)"),
            "count line: {}",
            s
        );
        assert!(s.contains("@@ -1 +1 @@"), "hunk header: {}", s);
        assert!(s.contains("-old line"), "removed line: {}", s);
        assert!(s.contains("+new line"), "added line: {}", s);
        assert!(s.contains("```"), "diff fenced: {}", s);
        assert!(s.contains("是否允许"), "closing prompt: {}", s);
        // The diff must not leak as escaped raw JSON metadata.
        assert!(!s.contains("\\n"), "no escaped JSON: {}", s);
    }

    #[test]
    fn describe_edit_permission_without_parseable_diff_shows_raw_diff() {
        let p = perm(
            "edit",
            serde_json::json!({ "filepath": "x.rs", "diff": "not a unified diff" }),
        );
        let s = describe_permission(&p);
        assert!(s.contains("not a unified diff"), "raw diff kept: {}", s);
        assert!(s.contains("是否允许"));
    }

    #[test]
    fn describe_bash_permission_stays_generic() {
        let p = perm("bash", serde_json::json!({ "command": "cargo test" }));
        let s = describe_permission(&p);
        assert!(s.contains("cargo test"), "command shown: {}", s);
        assert!(!s.contains("```"), "no diff fence for bash: {}", s);
        assert!(s.contains("是否允许"));
    }
}
