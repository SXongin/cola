use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::Instrument;

use crate::bridge::core::SharedCore;
use crate::bridge::handler::CardActionResult;
use crate::bridge::handles::{CardsHandle, RequestsHandle, SessionsHandle};
use crate::bridge::pollers::{
    CardTarget, inline_host_session, mark_stale_cards, resolve_card_target, result_card,
};
use crate::bridge::question::{
    MultiOutcome, QuestionState, action_toast, qa_completion_body, question_replay_card, stale_question_card,
};
use crate::bridge::snapshot_claims::ClaimKind;
use crate::bridge::turn::Turn;
use crate::opencode;

/// A pending request surfaced by a poll loop before it becomes a card. Carries
/// whichever backend payload the kind produced — permission or question.
#[derive(Clone)]
pub enum PendingRequest {
    Permission(opencode::types::PermissionRequest),
    Question(opencode::types::QuestionRequest),
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

    /// Add this kind's inline block to the `host` card (the poller path and
    /// the cross-turn re-host). Permissions rebuild deterministically from the
    /// request; questions restore their remembered partial state (已选 /
    /// finalized slots), so a block that moves onto a new turn's card keeps
    /// what the Host already answered (ADR-0038, rule 1). Returns whether the
    /// block was added (false when the card already carries it, or is gone).
    async fn add_inline(
        &self,
        flow: &RequestFlow,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool;

    /// Build the standalone interactive card (used when no streaming card hosts
    /// the request inline).
    fn build_card(&self, req: &PendingRequest, dir: &str) -> serde_json::Value;

    /// The description shown on the card when it is marked stale (resolved by
    /// another client).
    fn summary(&self, req: &PendingRequest) -> String;

    /// Resolve the kind's own inline blocks whose request vanished (resolved
    /// by another client) into their Interaction Receipts; an item owned by a
    /// directory whose list call failed stays live: unknown must never be read
    /// as resolved (#130, #144). An item cola itself is answering (`answered`)
    /// also stays: the settlement owns its receipt, the sweep's neutral one
    /// would be a lie. Returns the affected session ids — the sweep repaints
    /// each affected card so the receipt lands within one poll.
    async fn resolve_vanished_inline(
        &self,
        cards: &CardsHandle,
        pending: &std::collections::HashSet<String>,
        failed_dirs: &std::collections::HashSet<String>,
        cola_claimed: &std::collections::HashSet<String>,
    ) -> Vec<String>;

    /// The snapshot-claim kind of this flow's requests (ADR-0028): each flow's
    /// poll sweep only drops claims of its own kind.
    fn claim_kind(&self) -> ClaimKind;

    /// Handle a card action click on this kind's card. `clicked` is the message
    /// id the click landed on (`open_message_id`), so the ack can update THAT
    /// card through its handle even when it is no longer the accumulator's
    /// current card (ADR-0038, rule 3).
    #[allow(clippy::too_many_arguments)] // the click context is the trait's whole point
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
        clicked: Option<&str>,
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

    async fn add_inline(
        &self,
        _flow: &RequestFlow,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        let PendingRequest::Permission(p) = req else {
            return false;
        };
        Turn::add_permission(cards, host, p, dir).await
    }

    fn build_card(&self, req: &PendingRequest, dir: &str) -> serde_json::Value {
        let PendingRequest::Permission(p) = req else {
            return serde_json::json!({});
        };
        let body = describe_permission(p);
        crate::feishu::card::question::build_permission_card(
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

    async fn resolve_vanished_inline(
        &self,
        cards: &CardsHandle,
        pending: &std::collections::HashSet<String>,
        failed_dirs: &std::collections::HashSet<String>,
        cola_claimed: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        Turn::resolve_vanished_permissions(cards, pending, failed_dirs, cola_claimed).await
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
        clicked: Option<&str>,
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
            let mut cached = None;
            if flow.try_mark_answered(&core.requests_handle(), req_id).await {
                let mut approved = core.set_auto_accept(session_id, &dir, true).await;
                // The clicked request is always resolved by the toggle, even
                // if the backend list raced past it.
                if !approved.iter().any(|id| id == req_id) {
                    approved.push(req_id.to_string());
                }
                // Every block the toggle resolves (not just the clicked one)
                // becomes its Interaction Receipt so the streaming card
                // re-renders without them synchronously — the poller would
                // otherwise leave them lingering until the next poll notices
                // the requests vanished (ADR-0038, rule 4).
                cached = resolve_blocks(
                    flow,
                    &core.cards_handle(),
                    &core.requests_handle(),
                    host,
                    session_id,
                    Origin::Click { clicked },
                    &approved,
                    Residue::Single(AUTOACCEPT_RECEIPT),
                )
                .await;
                tracing::info!(
                    "Auto-Accept enabled via permission card on session {} (approved {})",
                    session_id,
                    approved.len()
                );
            }
            let mut r = result_card("✅ 已开启自动授权", "blue", "该会话后续权限请求将自动批准。");
            r.toast = Some("已开启自动授权".to_string());
            settle_ack(core, host, session_id, inline, cached, &mut r).await;
            // Re-served verbatim to a losing double-click.
            flow.remember_answered_result(req_id, &r).await;
            return Some(r);
        }

        // Double-click guard: the atomic claim decides who replies. A click
        // that loses the race re-serves the winning click's result; only the
        // winner may reach the backend.
        if !flow.try_mark_answered(&core.requests_handle(), req_id).await {
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
        let cached = match reply_result {
            Ok(()) => {
                // The span already carries the session; the id here is the
                // request's, so it is named as one.
                tracing::info!("Permission reply sent: {} request={}", reply, req_id);
                // ADR-0038 rules 3+4: the clicked block becomes its
                // Interaction Receipt (rendered from the accumulator, so it
                // survives later flushes) and the ack below carries the
                // clicked card's updated JSON — no PATCH race, no dependence
                // on which card is current.
                resolve_blocks(
                    flow,
                    &core.cards_handle(),
                    &core.requests_handle(),
                    host,
                    session_id,
                    Origin::Click { clicked },
                    &[req_id.to_string()],
                    Residue::PerBlock(&|target| permission_receipt(target, reply)),
                )
                .await
            }
            // 404: the permission is already resolved — by another client,
            // or by a click replayed after a restart cleared the in-memory
            // guard. Benign: keep the claim, leave the neutral receipt, and
            // show the neutral handled card.
            Err(e) if e.is_not_found() => {
                tracing::info!("Permission already resolved: {}", e);
                let cached = resolve_blocks(
                    flow,
                    &core.cards_handle(),
                    &core.requests_handle(),
                    host,
                    session_id,
                    Origin::Click { clicked },
                    &[req_id.to_string()],
                    Residue::PerBlock(&handled_elsewhere_receipt),
                )
                .await;
                let mut r = already_handled_result(self.label(), inline, "该权限已处理");
                settle_ack(core, host, session_id, inline, cached, &mut r).await;
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
        };
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
        r.toast = Some(toast);
        settle_ack(core, host, session_id, inline, cached, &mut r).await;
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
    crate::bridge::pollers::walk_parent_chain(&core.opencode, session_id, Some(directory), |current| {
        let current = current.to_string();
        async move {
            let sessions = core.sessions.lock().await;
            sessions.entry_for_session(&current).map(|e| e.auto_accept)
        }
    })
    .await
    .unwrap_or(false)
}

/// Prefix of the receipt left when a click discovers the request was already
/// resolved by another client (a 404 reply): neutral — never claims cola
/// decided. The sweep adopts the same line for a block resolved remotely
/// (#175).
const HANDLED_ELSEWHERE_PREFIX: &str = "⏱ 已由其他客户端处理";

/// Receipt for the Auto-Accept toggle: it names the MODE change, never the
/// requests it resolved — naming a command made the line read as "only this
/// command is now auto-approved". One per toggle, however many pending blocks
/// it swallowed (the blocks are dismissed without lines of their own). Shared
/// by the card button and the `/autoaccept on` command.
pub(crate) const AUTOACCEPT_RECEIPT: &str = "🔄 已开启自动授权：后续权限请求将自动批准";

/// Receipt prefix for a denied permission or a rejected question.
const DENIED_PREFIX: &str = "🚫 已拒绝";

/// Cap for an Interaction Receipt line: a receipt is a one-line residue, not
/// a report.
const RECEIPT_MAX_CHARS: usize = 120;

/// Compose one receipt line: `prefix：detail`, clipped to stay a residue. A
/// block with no derivable detail (should not happen for a live block) keeps
/// the bare prefix.
fn receipt_line(prefix: &str, detail: &str) -> String {
    if detail.is_empty() {
        prefix.to_string()
    } else {
        truncate(&format!("{prefix}：{detail}"), RECEIPT_MAX_CHARS)
    }
}

/// The Interaction Receipt for a Session Snapshot's claimed block resolved by
/// another client (#175, ADR-0038 rule 4): the same neutral line an inline
/// block leaves, derived from the adopt-time request — a snapshot keeps no
/// `InteractionBlock` to read the target from.
pub(crate) fn snapshot_handled_elsewhere_receipt(req: &PendingRequest) -> String {
    let target = match req {
        PendingRequest::Permission(p) => permission_target(p),
        PendingRequest::Question(q) => crate::feishu::card::question::question_target(&q.questions),
    };
    receipt_line(HANDLED_ELSEWHERE_PREFIX, &target)
}

/// The Interaction Receipt for a permission decision (ADR-0038, rule 4):
/// `✅ 已允许一次：⚡ 执行 Shell 命令 \`ls -la\``. The target comes from the
/// block being resolved, so the residue always names what it resolved. It
/// carries no clock: the decision happens in cola's world, and a cola time in
/// the same `· HH:MM` costume as the server-stamped panels would mix two
/// clocks on one card (#183 follow-up). Its timeline position already places
/// it between what the Host could see and what followed.
fn permission_receipt(target: &str, reply: &str) -> String {
    let line = match reply {
        once @ ("once" | "always") => {
            let prefix = if once == "once" {
                "✅ 已允许一次"
            } else {
                "✅ 已始终允许"
            };
            if target.is_empty() {
                prefix.to_string()
            } else {
                format!("{prefix}：{target}")
            }
        }
        _ => return receipt_line(DENIED_PREFIX, target),
    };
    truncate(&line, RECEIPT_MAX_CHARS)
}

/// The Interaction Receipt for a resolved question:
/// `✅ 已回答：目录 /a、分支 main` — each question's header (or a clipped
/// question text) with the answers chosen for it; a skipped slot reads
/// 未作答. `answers[i]` is the selection submitted for question `i`.
fn question_receipt(questions: &[opencode::types::QuestionInfo], answers: &[Vec<String>]) -> String {
    let parts: Vec<String> = questions
        .iter()
        .enumerate()
        .map(|(i, qi)| {
            let label = if qi.header.is_empty() {
                truncate(&qi.question, 24)
            } else {
                truncate(&qi.header, 24)
            };
            match answers.get(i).filter(|a| !a.is_empty()) {
                Some(a) => format!("{label} {}", a.join("、")),
                None => format!("{label} （未作答）"),
            }
        })
        .collect();
    if parts.is_empty() {
        return "✅ 已回答".to_string();
    }
    truncate(&format!("✅ 已回答：{}", parts.join("、")), RECEIPT_MAX_CHARS)
}

/// Receipt for a block a click found already resolved elsewhere.
pub(crate) fn handled_elsewhere_receipt(target: &str) -> String {
    receipt_line(HANDLED_ELSEWHERE_PREFIX, target)
}

/// Receipt for a denied permission or a rejected question.
fn denied_receipt(target: &str) -> String {
    receipt_line(DENIED_PREFIX, target)
}

/// Compact one-line target for a permission's receipt: the action plus the
/// first pattern (or the edited file for edit/patch) — what the decision was
/// about, for when the position alone cannot say (a sub-task child's block has
/// no command panel on the parent card; a toggle resolves several at once).
/// Backticks in a pattern are flattened so the markdown element cannot be
/// broken by server-provided content.
pub(crate) fn permission_target(p: &opencode::types::PermissionRequest) -> String {
    let action = p.permission.as_deref().unwrap_or("?");
    let (emoji, label) = describe_action(action);
    let pattern = p
        .patterns
        .first()
        .map(|s| s.trim().replace('`', "'"))
        .filter(|s| !s.is_empty())
        .map(|s| truncate(&s, 60));
    let file = p
        .metadata
        .as_ref()
        .and_then(|m| m.get("filepath"))
        .and_then(|v| v.as_str())
        .filter(|f| !f.is_empty())
        .map(|f| truncate(f, 60));
    // An edit/patch is about the file (the body shows its diff); every other
    // action is about its first pattern.
    let object = if matches!(action, "edit" | "patch" | "apply_patch") {
        file.or(pattern)
    } else {
        pattern.or(file)
    };
    match object {
        Some(object) => format!("{emoji} {label} `{object}`"),
        None => format!("{emoji} {label}"),
    }
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

    async fn add_inline(
        &self,
        flow: &RequestFlow,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        let PendingRequest::Question(q) = req else {
            return false;
        };
        // Restore what the Host already answered: a re-hosted question keeps
        // its 已选 markers and finalized slots (ADR-0038, rule 1).
        let (answers, done) = match flow.question_state.lock().await.get(&q.id) {
            Some(state) => {
                let (_, display, done) = state.merge();
                (display, done)
            }
            None => (vec![None; q.questions.len()], vec![false; q.questions.len()]),
        };
        Turn::add_question(cards, host, q, dir, &answers, &done).await
    }

    fn build_card(&self, req: &PendingRequest, dir: &str) -> serde_json::Value {
        let PendingRequest::Question(q) = req else {
            return serde_json::json!({});
        };
        crate::feishu::card::question::build_question_card(
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
            PendingRequest::Question(q) => crate::feishu::card::question::question_summary(&q.questions),
            PendingRequest::Permission(_) => String::new(),
        }
    }

    async fn resolve_vanished_inline(
        &self,
        cards: &CardsHandle,
        pending: &std::collections::HashSet<String>,
        failed_dirs: &std::collections::HashSet<String>,
        cola_claimed: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        Turn::resolve_vanished_questions(cards, pending, failed_dirs, cola_claimed).await
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
        clicked: Option<&str>,
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
                if flow.is_answered(&core.requests_handle(), req_id).await {
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
                if answered_count == n {
                    // All questions answered → claim the request and submit.
                    // The claim is atomic: a click that loses the race re-serves
                    // the winning result instead of replying again.
                    if !flow.try_mark_answered(&core.requests_handle(), req_id).await {
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
                        host,
                        session_id,
                        clicked,
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
                    let cached = resolve_blocks(
                        flow,
                        &core.cards_handle(),
                        &core.requests_handle(),
                        host,
                        session_id,
                        Origin::Click { clicked },
                        &[req_id.to_string()],
                        Residue::PerBlock(&|_| question_receipt(&questions, &answers)),
                    )
                    .await;
                    let mut r = result_card("✅ 已回答", "green", &qa_completion_body(&questions, &answers));
                    r.toast = Some("已回答".to_string());
                    settle_ack(core, host, session_id, inline, cached, &mut r).await;
                    // Re-served verbatim to a losing double-click.
                    flow.remember_answered_result(req_id, &r).await;
                    Some(r)
                } else if inline {
                    // Inline: the ack carries the clicked card with the updated
                    // display state (已选/✅ markers) — the mechanism that
                    // reliably refreshes the clicked card after a callback; a
                    // PATCH around the callback leaves it on its pre-answer
                    // state. The clicked card is refreshed through its handle
                    // when one is known, so the update lands even on a card
                    // that is no longer the accumulator's current card.
                    let req = {
                        let states = flow.question_state.lock().await;
                        states.get(req_id).map(|s| s.request().clone())
                    };
                    let card = match req {
                        Some(req) => {
                            refresh_question_block(
                                core,
                                host,
                                session_id,
                                clicked,
                                req_id,
                                &req,
                                directory.unwrap_or(""),
                                &display,
                                &done,
                            )
                            .await
                        }
                        None => None,
                    };
                    let r = CardActionResult {
                        card: match card {
                            Some(card) => Some(card),
                            None => ack_inline_card(core, host, session_id).await,
                        },
                        toast: Some(action_toast(reply, n - answered_count, outcome)),
                    };
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
                    let card = crate::feishu::card::question::build_question_card(
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
                if flow.is_answered(&core.requests_handle(), req_id).await {
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
                if !flow.try_mark_answered(&core.requests_handle(), req_id).await {
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
                    host,
                    session_id,
                    clicked,
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
                let cached = resolve_blocks(
                    flow,
                    &core.cards_handle(),
                    &core.requests_handle(),
                    host,
                    session_id,
                    Origin::Click { clicked },
                    &[req_id.to_string()],
                    Residue::PerBlock(&|_| question_receipt(&questions, &answers)),
                )
                .await;
                let mut r = result_card("✅ 已回答", "green", &qa_completion_body(&questions, &answers));
                r.toast = Some("已提交".to_string());
                settle_ack(core, host, session_id, inline, cached, &mut r).await;
                // Re-served verbatim to a losing double-click.
                flow.remember_answered_result(req_id, &r).await;
                Some(r)
            }
            "reject" => {
                if flow.is_answered(&core.requests_handle(), req_id).await {
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
                if !flow.try_mark_answered(&core.requests_handle(), req_id).await {
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
                    host,
                    session_id,
                    clicked,
                )
                .await
                {
                    return Some(r);
                }
                tracing::info!("Question rejected: {}", req_id);
                let cached = resolve_blocks(
                    flow,
                    &core.cards_handle(),
                    &core.requests_handle(),
                    host,
                    session_id,
                    Origin::Click { clicked },
                    &[req_id.to_string()],
                    Residue::PerBlock(&denied_receipt),
                )
                .await;
                let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                r.toast = Some("已拒绝回答".to_string());
                settle_ack(core, host, session_id, inline, cached, &mut r).await;
                // Re-served verbatim to a losing double-click.
                flow.remember_answered_result(req_id, &r).await;
                Some(r)
            }
            _ => None,
        }
    }
}

/// A request card cola sent: the live Message id, the summary shown when the
/// card is marked stale, and the owning directory. The directory lets the
/// sweep skip cleanups for a directory whose list call failed — "absent from
/// the pending list" only speaks for directories that listed successfully
/// (#130, #144).
#[derive(Clone)]
pub struct SentCard {
    pub message_id: String,
    pub summary: String,
    pub directory: String,
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
    /// request_id → the card cola sent (used to mark a card stale when the
    /// request is resolved by ANOTHER client).
    pub sent_cards: Arc<Mutex<HashMap<String, SentCard>>>,
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
    pub(crate) async fn is_answered(&self, requests: &RequestsHandle, req_id: &str) -> bool {
        requests.answered_requests.lock().await.contains(req_id)
    }

    /// Atomically claim `req_id` for this click: `true` when it was not yet
    /// answered (this click may reply), `false` when another click already
    /// claimed it (re-serve the first result, never reply again). The
    /// check-and-insert happens under ONE lock, so two near-simultaneous clicks
    /// cannot both win the way the old `is_answered` + `mark_answered` pair
    /// could.
    pub(crate) async fn try_mark_answered(&self, requests: &RequestsHandle, req_id: &str) -> bool {
        requests.answered_requests.lock().await.insert(req_id.to_string())
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
    pub(crate) async fn remember_question(&self, req: &opencode::types::QuestionRequest, dir: &str) {
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
    ) -> Option<(Vec<opencode::types::QuestionInfo>, Vec<Vec<String>>)> {
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

    /// #187: reject every request of this kind that is still pending for
    /// `session_id` or one of its sub-task descendants, when the turn that
    /// would have consumed it ended without completing (`/stop`, interrupt,
    /// prompt error). The tool fiber behind such a request is dead, so
    /// approving it is meaningless; leaving it alive only lets the next turn
    /// re-host a ghost block (ADR-0038, rule 1). Mirrors
    /// `SharedCore::approve_pending_for_session`'s session/descendant filter.
    ///
    /// Only a KNOWN state is resolved: a failed/timed-out list, an empty
    /// session id, another session, and a snapshot claim all stay untouched —
    /// unknown is never read as resolved (#130/#144). Returns the ids the
    /// server actually rejected, in list order, so the caller turns their
    /// blocks into `🚫 已拒绝` receipts through `resolve_blocks`.
    pub(crate) async fn reject_pending_for_session(
        &self,
        requests: &RequestsHandle,
        sessions: &SessionsHandle,
        backend: &Arc<dyn opencode::Backend>,
        session_id: &str,
        directory: &str,
    ) -> Vec<String> {
        let dir_backend = backend.clone().for_directory(directory);
        // Bounded like the sweep's list: a half-open connection must not stall
        // the turn's finish behind a request that will never answer.
        let listed = match crate::bridge::bounded_call(
            &format!("turn end {} ({}) list", self.kind.label(), directory),
            self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            self.kind.list(&dir_backend),
        )
        .await
        {
            Some(Ok(listed)) => listed,
            Some(Err(e)) => {
                tracing::warn!("turn end {} ({}): {}", self.kind.label(), directory, e);
                return Vec::new();
            }
            None => return Vec::new(),
        };
        let mut rejected = Vec::new();
        for req in &listed {
            let sid = req.session_id();
            if !session_belongs_to(sessions, backend, sid, session_id, directory).await {
                continue;
            }
            // An answered request was already resolved (or is being resolved)
            // by its click: never reply a second time.
            if self.is_answered(requests, req.id()).await {
                continue;
            }
            // A claimed request's block lives on a snapshot card, whose
            // lifecycle is its own (ADR-0038, rule 6).
            if requests.snapshot_claims.lock().await.contains(req.id()) {
                continue;
            }
            let result = match req {
                PendingRequest::Permission(p) => dir_backend.reply_permission(&p.request_id, "reject").await,
                PendingRequest::Question(q) => dir_backend.reject_question(&q.id).await,
            };
            match result {
                Ok(()) => {
                    tracing::info!(
                        "Rejected leftover {} {} on session {} (its turn ended without completing)",
                        self.kind.label(),
                        req.id(),
                        sid
                    );
                    if let PendingRequest::Question(q) = req {
                        // The reply landed: drop the in-flight state like a
                        // reject click does, so nothing serves stale answers.
                        self.remove_question(&q.id).await;
                    }
                    rejected.push(req.id().to_string());
                }
                // Resolved elsewhere in the meantime: the sweep's next pass
                // leaves the neutral receipt for it — cola did not decide.
                Err(e) if e.is_not_found() => {
                    tracing::info!(
                        "leftover {} {} was already resolved: {}",
                        self.kind.label(),
                        req.id(),
                        e
                    )
                }
                // Genuine failure (network, routing): the request may still be
                // pending, so leave its block live — the Host can still answer
                // it, and the sweep keeps polling.
                Err(e) => tracing::warn!("reject leftover {} {}: {}", self.kind.label(), req.id(), e),
            }
        }
        rejected
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
        // Feishu's Instant Reminder (ADR-0043): this kind's pin candidates,
        // collected here and resolved after the sweep. Collected (not resolved
        // inline) so a request auto-resolved in the same sweep (e.g.
        // `/autoaccept`) never pins — it was never waiting on the user.
        let mut pin_candidates: Vec<(PendingRequest, String)> = Vec::new();
        let mut auto_resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
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
                        // Both pin surfaces need the listed candidates: the
                        // reminder's chat-level target and the waiting-card
                        // registry's host message.
                        if core.reminder.enabled() || core.message_pins.enabled() {
                            pin_candidates.push((req.clone(), dir.clone()));
                        }
                        // One span per request (ADR-0048): its `prepare`, its
                        // re-host and its card delivery are all retrievable by
                        // the session it belongs to. `chat`/`topic` ride along
                        // when the store maps that session — a sub-task child
                        // is not mapped and carries `session` alone.
                        let thread_key = {
                            let store = core.sessions.lock().await;
                            store
                                .entry_for_session(req.session_id())
                                .map(|e| e.thread_key.clone())
                        };
                        let span = crate::bridge::span::request(req.session_id(), thread_key.as_ref());
                        if self.surface(core, req, dir, seen).instrument(span).await {
                            auto_resolved.insert(req.id().to_string());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("poll {} ({}): {}", self.kind.label(), dir, e);
                    failed_dirs.insert(dir.clone());
                }
            }
        }
        // Instant Reminder (ADR-0043): reconcile this kind's pending requests
        // with the pin state. A sweep where any directory failed must not
        // move it — that directory said nothing, and unknown is never read as
        // resolved (#130) — so the pin survives until a complete sweep. A
        // request auto-resolved in this sweep (prepare said handled) is not a
        // wait, so it never pins.
        if core.reminder.enabled() && failed_dirs.is_empty() {
            let mut pin_targets: Vec<crate::bridge::reminder::ReminderTarget> = Vec::new();
            for (req, dir) in &pin_candidates {
                if auto_resolved.contains(req.id()) {
                    continue;
                }
                // `None` (an external turn, or a request pending across a
                // restart) cannot be pinned — there is no requester to target.
                if let Some(target) =
                    crate::bridge::reminder::reminder_target(core, req.session_id(), dir).await
                {
                    pin_targets.push(target);
                }
            }
            core.reminder
                .sync(&core.feishu, self.kind.claim_kind(), &pin_targets)
                .await;
        }
        // Mark stale: a card cola sent whose request is no longer pending
        // (resolved by another client) and was NOT answered by cola. A card
        // owned by a directory whose list failed stays live (#144).
        mark_stale_cards(core, &pending, &self.sent_cards, &failed_dirs, self.kind.label()).await;
        // Requests cola itself is answering or has answered (a click's claim,
        // an auto-accept approval): their block belongs to the settlement
        // writing the true receipt, so the sweep must not read their
        // disappearance from the pending list as another client's work.
        // Snapshot once — every pass below must judge the same moment.
        let cola_claimed = core.claimed_requests().await;
        // Card handles (ADR-0038, rule 2): a live block on a card whose
        // accumulator is gone (a replaced/aborted turn) is repainted from the
        // cached JSON — the accumulator pass below only reaches the card its
        // own flush would repaint. `flush_owned` maps each accumulator-owned
        // block to that card, so a block whose handle already names it is left
        // to the flush (never resolved twice), while one whose handle names an
        // older card still gets that stale card repainted.
        let flush_owned = Turn::flush_owned_blocks(&core.cards_handle()).await;
        let dropped = core.card_handles.lock().await.drop_vanished(
            self.kind.claim_kind(),
            &pending,
            &failed_dirs,
            &cola_claimed,
            &flush_owned,
            handled_elsewhere_receipt,
        );
        for (message_id, card) in dropped {
            if let Err(e) = core.feishu.update_message(&message_id, &card).await {
                tracing::warn!("vanished block repaint failed on {}: {}", message_id, e);
            } else {
                tracing::info!(
                    "{} block repainted from its card handle: {}",
                    self.kind.label(),
                    message_id
                );
            }
        }
        // Resolve inline blocks whose request vanished (resolved by another
        // client) into their Interaction Receipts and repaint the cards that
        // host them — the receipt lands within this sweep, with no reliance on
        // the render tick (which stops when a turn ends). Blocks owned by a
        // failed directory stay (#144).
        let repaint = self
            .kind
            .resolve_vanished_inline(&core.cards_handle(), &pending, &failed_dirs, &cola_claimed)
            .await;
        for session_id in &repaint {
            Turn::flush_card(&core.cards_handle(), session_id).await;
        }
        // ADR-0028: a claimed request that left the pending list was
        // resolved — by the snapshot's own buttons (the click handler
        // re-renders synchronously; this just cleans the registry) or by
        // another client (the block must drop from the snapshot). The
        // snapshot card itself is never marked stale — that targets
        // standalone cards. A claim hosted from a directory whose list failed
        // stays: that directory said nothing (#144). The registry returns the
        // rebuilt cards; the flow patches them (never holding the registry
        // lock across the Feishu call).
        let dropped =
            core.snapshot_claims
                .lock()
                .await
                .drop_vanished(self.kind.claim_kind(), &pending, &failed_dirs);
        for (message_id, card) in dropped {
            if let Err(e) = core.feishu.update_message(&message_id, &card).await {
                tracing::warn!("snapshot claim drop: card update failed: {}", e);
            } else {
                tracing::info!("snapshot {} re-rendered without resolved claims", message_id);
            }
        }
        // Waiting-card pins (ADR-0043 amendment): keep every card still
        // waiting on the user in its chat's pinned-message list, so the
        // Chat/Topic's Instant Reminder nudge leads to the exact message once
        // the user opens the chat. The host is the card rendering the live
        // block (the streaming card, possibly a re-hosted newer one), the
        // standalone card the request was sent as, or — for a request claimed
        // by a Session Snapshot (ADR-0028) — the snapshot card itself, whose
        // lifecycle owns the claimed block. A request cola is already
        // answering or one auto-resolved in this sweep is not a wait and never
        // pins. Best-effort, like the reminder; a failed directory's tracked
        // pin stays (#130).
        if core.message_pins.enabled() {
            let claimed_hosts: std::collections::HashMap<String, String> = {
                let claims = core.snapshot_claims.lock().await;
                pin_candidates
                    .iter()
                    .filter_map(|(req, _)| {
                        claims
                            .claim_of(req.id())
                            .map(|(message_id, _)| (req.id().to_string(), message_id.to_string()))
                    })
                    .collect()
            };
            // Snapshot the standalone cards first: never hold `sent_cards` and
            // `card_handles` at once (one lock order, no deadlock surface).
            let sent_ids: std::collections::HashMap<String, String> = self
                .sent_cards
                .lock()
                .await
                .iter()
                .map(|(id, card)| (id.clone(), card.message_id.clone()))
                .collect();
            let waiting: Vec<crate::bridge::message_pins::WaitingCard> = {
                let handles = core.card_handles.lock().await;
                pin_candidates
                    .iter()
                    .filter(|(req, _)| !auto_resolved.contains(req.id()) && !cola_claimed.contains(req.id()))
                    .filter_map(|(req, dir)| {
                        let message_id = claimed_hosts
                            .get(req.id())
                            .cloned()
                            .or_else(|| handles.message_of(req.id()).map(str::to_string))
                            .or_else(|| sent_ids.get(req.id()).cloned())?;
                        Some(crate::bridge::message_pins::WaitingCard {
                            request_id: req.id().to_string(),
                            directory: dir.clone(),
                            message_id,
                        })
                    })
                    .collect()
            };
            core.message_pins
                .sync(&core.feishu, self.kind.claim_kind(), &waiting, &failed_dirs)
                .await;
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

    /// Everything one sweep does with ONE listed request, wrapped by the
    /// caller in that request's session span (ADR-0048): the snapshot-claim
    /// skip, the re-host for a request already surfaced, the kind's `prepare`
    /// (auto-accept / remember), and the card delivery — inline on the
    /// session's live card, else a standalone card. Returns whether `prepare`
    /// handled the request (an auto-accept), which the sweep must exclude from
    /// its reminder/pin reconciliation.
    ///
    /// `seen` records the requests this process has surfaced: a re-host needs
    /// the record (`seen.contains`), a first surfacing inserts it — so the
    /// poller never surfaces one request twice.
    async fn surface(
        &self,
        core: &Arc<SharedCore>,
        req: &PendingRequest,
        dir: &str,
        seen: &mut std::collections::HashSet<String>,
    ) -> bool {
        // ADR-0028: a claimed request is hosted by a snapshot card — already
        // surfaced, never a standalone card, never re-inlined.
        if core.snapshot_claims.lock().await.contains(req.id()) {
            return false;
        }
        if seen.contains(req.id()) {
            // Already surfaced: if it outlived the turn that hosted it,
            // re-host it onto the session's current card so the live controls
            // follow the newest card (ADR-0038, rule 1).
            self.rehost_block(core, req, dir).await;
            return false;
        }
        seen.insert(req.id().to_string());
        tracing::info!(
            "{} ({}): {} on session {}",
            self.kind.label(),
            dir,
            req.id(),
            req.session_id()
        );
        // Kind-specific pre-card handling (auto-accept / remember). true →
        // handled, no card needed.
        if self.kind.prepare(self, core, req, dir).await {
            return true;
        }
        // One-card-per-turn: surface the request INLINE on the streaming card
        // of the session that owns it — the session itself, or (sub-task
        // children) its nearest ancestor with a live card. Only a separate card
        // when there is no active card (e.g. external turns or restarts).
        if let Some(host) = inline_host_session(core, req.session_id(), Some(dir)).await {
            let pushed = self
                .kind
                .add_inline(self, &core.cards_handle(), &host, req, dir)
                .await;
            if pushed {
                tracing::info!(
                    "{} {} inlined on session {} card",
                    self.kind.label(),
                    req.id(),
                    host
                );
                // Flush so the inline section appears NOW — the render loop
                // only flushes on new parts, and a blocked prompt produces
                // none.
                Turn::flush_card(&core.cards_handle(), &host).await;
            }
            return false;
        }
        let card = self.kind.build_card(req, dir);
        // Reply to the message that triggered the prompt for this session; fall
        // back to sending into the chat when the accumulator is gone (e.g.
        // after a cola restart). Sub-task sessions resolve up the parent chain.
        let sent_id = match resolve_card_target(core, req.session_id(), dir).await {
            Some(CardTarget::ReplyTo(msg_id)) => core.feishu.reply_card(&msg_id, &card).await.ok(),
            Some(CardTarget::Chat(chat_id)) => core.feishu.send_card("chat_id", &chat_id, &card).await.ok(),
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
            self.sent_cards.lock().await.insert(
                req.id().to_string(),
                SentCard {
                    message_id: mid,
                    summary: self.kind.summary(req),
                    directory: dir.to_string(),
                },
            );
        } else {
            tracing::warn!(
                "{} card send failed on session {}",
                self.kind.label(),
                req.session_id()
            );
        }
        false
    }

    /// Re-host a still-pending inline block onto the session's current card
    /// (ADR-0038, rule 1): a request that outlived the turn which surfaced it
    /// follows the newest card, and the card that used to show it is repainted
    /// without the controls — so a live block lives on exactly one card. Called
    /// from the sweep for requests already in `seen` (the poller would
    /// otherwise never touch them again).
    async fn rehost_block(&self, core: &Arc<SharedCore>, req: &PendingRequest, dir: &str) {
        let id = req.id();
        // Only a block the flush recorded (an inline card) follows the card:
        // a standalone card and a snapshot claim keep their lifecycles
        // (ADR-0038, rule 6).
        let Some(old_message) = core.card_handles.lock().await.message_of(id).map(str::to_string) else {
            return;
        };
        let Some(host) = inline_host_session(core, req.session_id(), Some(dir)).await else {
            return;
        };
        if !Turn::has_card(&core.cards_handle(), &host).await {
            return;
        }
        // The accumulator still carries the block (its own flush owns the
        // current card), or the handle already names that card: nothing to
        // move.
        if Turn::has_interaction_in(&core.cards_handle(), &host, id).await
            || Turn::card_message_id(&core.cards_handle(), &host)
                .await
                .as_deref()
                == Some(old_message.as_str())
        {
            return;
        }
        let pushed = self
            .kind
            .add_inline(self, &core.cards_handle(), &host, req, dir)
            .await;
        if !pushed {
            return;
        }
        // Flush first: the current card renders the block and the handle moves
        // with it.
        Turn::flush_card(&core.cards_handle(), &host).await;
        tracing::info!(
            "{} {} re-hosted on session {} card (old {})",
            self.kind.label(),
            id,
            host,
            old_message
        );
        // Then strip the old card's copy through its handle. The block moved,
        // not resolved: no receipt — the controls simply leave that card.
        if let Some(card) = core.card_handles.lock().await.remove_on(&old_message, id)
            && let Err(e) = core.feishu.update_message(&old_message, &card).await
        {
            tracing::warn!("re-host old-card repaint failed on {}: {}", old_message, e);
        }
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
        // The card the click landed on, so its handle can update THAT card
        // atomically in the ack (ADR-0038, rule 3) — the callback context
        // carries it (ws.rs injects `open_message_id`).
        let clicked = value.get("open_message_id").and_then(|v| v.as_str());
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
        } else if Turn::has_card(&core.cards_handle(), session_id).await {
            Some(session_id.to_string())
        } else {
            inline_host_session(core, session_id, directory).await
        };
        let inline = host.is_some();

        let mut r = self
            .kind
            .handle_action(
                self, core, session_id, req_id, reply, value, directory, inline, &host, clicked,
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
        card: Some(crate::feishu::card::notify::build_resolved_elsewhere_card(
            kind, "",
        )),
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
#[allow(clippy::too_many_arguments)] // the click context is what the helper settles
async fn settle_question_reply(
    flow: &RequestFlow,
    core: &Arc<SharedCore>,
    req_id: &str,
    kind: &str,
    result: crate::error::Result<()>,
    inline: bool,
    host: &Option<String>,
    session_id: &str,
    clicked: Option<&str>,
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
            // The click found the request gone: leave the neutral "handled
            // elsewhere" receipt on the clicked card, carried in the ack like
            // any other resolution (ADR-0038, rules 3+4).
            let cached = resolve_blocks(
                flow,
                &core.cards_handle(),
                &core.requests_handle(),
                host,
                session_id,
                Origin::Click { clicked },
                &[req_id.to_string()],
                Residue::PerBlock(&handled_elsewhere_receipt),
            )
            .await;
            let mut r = already_handled_result(kind, inline, "该问题已处理");
            settle_ack(core, host, session_id, inline, cached, &mut r).await;
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

/// Where a resolution came from — decides how its cards are settled.
pub(crate) enum Origin<'a> {
    /// A card callback: `clicked` is the message id the callback carried (the
    /// card the Host actually clicked, when the platform supplies it). That
    /// card's edited JSON is the atomic ack (ADR-0038, rule 3), and a
    /// DIFFERENT registered card is patched. Without an id the ack is rebuilt
    /// from the accumulator and NO card is patched — the ack IS the update, so
    /// no PATCH may race behind it.
    Click { clicked: Option<&'a str> },
    /// No callback at all (a command, e.g. `/autoaccept on`): every card that
    /// renders a block is patched eagerly.
    Command,
}

/// How a resolution leaves its residue on the card and in the timeline
/// (ADR-0038, rule 4).
pub(crate) enum Residue<'a> {
    /// One receipt per resolved block, naming that block's own target — a
    /// click on the block's own controls.
    PerBlock(&'a (dyn Fn(&str) -> String + Send + Sync)),
    /// ONE receipt for the whole resolution: a mode change resolves every
    /// pending block at once and reports the mode, not the requests it
    /// swallowed. The rest are dismissed without lines of their own, so a card
    /// never carries the same line twice.
    Single(&'a str),
}

/// Apply one block's residue to the cached card `card_id`: a per-block receipt
/// line, or — for a mode change — the single mode line on the first block that
/// card renders and a plain removal for the rest. `stamped_cards` remembers which
/// cards already carry the mode line. `None` when that card does not render the
/// block.
fn residue_edit(
    handles: &mut crate::bridge::card_handles::CardHandles,
    card_id: &str,
    request_id: &str,
    residue: &Residue<'_>,
    stamped_cards: &mut std::collections::HashSet<String>,
) -> Option<serde_json::Value> {
    match residue {
        Residue::PerBlock(line) => {
            let text = handles.target_of(request_id).map(line)?;
            handles.resolve_on(card_id, request_id, &text)
        }
        Residue::Single(text) => {
            if stamped_cards.contains(card_id) {
                return handles.remove_on(card_id, request_id);
            }
            let card = handles.resolve_on(card_id, request_id, text)?;
            stamped_cards.insert(card_id.to_string());
            Some(card)
        }
    }
}

/// Apply one block's residue to the cached card `card_id` and stack the
/// repainted card into `patches` (one PATCH per card), restamping its header
/// from the accumulator's post-resolution state.
fn repaint_card(
    handles: &mut crate::bridge::card_handles::CardHandles,
    card_id: &str,
    request_id: &str,
    residue: &Residue<'_>,
    stamped_cards: &mut std::collections::HashSet<String>,
    header: Option<&(String, &'static str)>,
    patches: &mut Vec<(String, serde_json::Value)>,
) {
    if let Some(card) = residue_edit(handles, card_id, request_id, residue, stamped_cards) {
        crate::bridge::card_handles::merge_patch(
            patches,
            card_id.to_string(),
            restamped_header(card, header),
        );
    }
}

/// Resolve a set of answered request ids on EVERY surface that renders their
/// blocks (ADR-0038, rule 2) — the one mutation seam, so the accumulator and
/// the card handles cannot drift:
///
/// 1. `sent_cards` forgets them (the poller stops delivering standalone cards).
/// 2. The host accumulator that still carries a block resolves it into its
///    timeline receipt; the next flush (new part, header tick, split)
///    re-renders it in its transcript position.
/// 3. Every card handle that renders one of them gets its cached JSON edited
///    in place — the clicked card's edit becomes the callback ack, and another
///    card's edit is patched eagerly. The registry entry is then dropped, so a
///    resolved block never lingers.
///
/// `origin` decides how the cards are settled and `residue` what each block
/// leaves behind (ADR-0038, rules 3+4). Returns the clicked card's edited
/// JSON, when the click landed on a card carrying one of the blocks.
#[allow(clippy::too_many_arguments)] // the resolution seam: flow + the two handles it settles on + origin/residue
pub(crate) async fn resolve_blocks(
    flow: &RequestFlow,
    cards: &CardsHandle,
    requests: &RequestsHandle,
    host: &Option<String>,
    session_id: &str,
    origin: Origin<'_>,
    ids: &[String],
    residue: Residue<'_>,
) -> Option<serde_json::Value> {
    // The session's card-write lock, shared with `flush_card`: a resolution
    // settles the accumulator and the cached cards, and a flush that
    // snapshotted before it must not record its stale copy after — the block
    // would come back to life and the sweep would report cola's own decision
    // as another client's.
    let write_lock = cards.write_lock(host.as_deref().unwrap_or(session_id)).await;
    let _guard = write_lock.lock().await;
    // A click settles the standalone surface too, so its `sent_cards` entry
    // goes. A COMMAND does not: an id with no inline block and no card handle
    // is a standalone card, and clearing its entry here would strand its live
    // buttons — `mark_stale_cards` repaints standalone cards and owns that
    // lifecycle (ADR-0038, rule 6). Mixed surfaces (standalone copy + inline
    // block) keep their entry; the sweep marks the standalone copy while this
    // seam settles the inline one.
    if matches!(origin, Origin::Click { .. }) {
        let mut sent = flow.sent_cards.lock().await;
        for id in ids {
            sent.remove(id);
        }
    }
    // 1. The accumulator that still carries a block is the render source: a
    //    receipt must live in its timeline, or the next flush re-adds the
    //    block to the card the ack just cleaned. Resolving the last live block
    //    also changes the HEADER (the awaiting-action title lifts), so
    //    the post-resolution header is captured here and restamped onto every
    //    card this resolution edits — otherwise a click leaves the clicked
    //    card titled "waiting" until the next render-poll flush (~2 s).
    //    Only an accumulator that ACTUALLY resolved one of the blocks may
    //    restamp: a card whose own turn is gone (its block lives only in the
    //    handle cache) has no header source, and wearing another turn's live
    //    header would be a lie. The restamp also cannot overwrite a newer
    //    card's header: a flush that ran after this resolution rebuilt the
    //    card without this block's span, so `resolve_on` finds nothing and
    //    the ack falls back to a fresh rebuild.
    let residue_mode = match &residue {
        Residue::PerBlock(line) => crate::bridge::turn::InlineResidue::PerBlock(*line),
        Residue::Single(text) => crate::bridge::turn::InlineResidue::Single(text),
    };
    let post_resolution_header =
        Turn::resolve_interactions(cards, host.as_deref().unwrap_or(session_id), ids, &residue_mode).await;
    // 2. The card handles: every card that rendered one of the blocks is
    //    edited from its cache. The clicked card's edit is the ack; any other
    //    card's edit is patched so its controls do not linger.
    let mut ack = None;
    let mut patches: Vec<(String, serde_json::Value)> = Vec::new();
    // Cards that already carry a mode line; their remaining blocks are removed
    // without one (see `residue_edit`).
    let mut stamped_cards: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut handles = cards.card_handles.lock().await;
        for id in ids {
            match origin {
                Origin::Click {
                    clicked: Some(clicked_id),
                } => {
                    // The clicked card carried the block: its edited JSON is
                    // the atomic callback ack (ADR-0038, rule 3).
                    if let Some(card) =
                        residue_edit(&mut handles, clicked_id, id, &residue, &mut stamped_cards)
                    {
                        ack = Some(restamped_header(card, post_resolution_header.as_ref()));
                    }
                    // The registered card is a different one (the block moved,
                    // or the click landed on a stale copy): repaint it so its
                    // controls do not linger.
                    if let Some(registered) = handles.message_of(id).map(str::to_string)
                        && registered != clicked_id
                    {
                        repaint_card(
                            &mut handles,
                            &registered,
                            id,
                            &residue,
                            &mut stamped_cards,
                            post_resolution_header.as_ref(),
                            &mut patches,
                        );
                    }
                }
                // A callback with no card id: the ack is rebuilt from the
                // accumulator, so no card may be patched behind it.
                Origin::Click { clicked: None } => {}
                // No callback (a command resolved the block): every card that
                // still renders it is patched — the registered one and any
                // older copy a re-host left behind. Only the registered card
                // (the accumulator's current card) gets the header restamp:
                // an older copy may belong to a previous turn, and wearing
                // another turn's live header would be a lie.
                Origin::Command => {
                    for card_id in handles.cards_rendering(id) {
                        let registered = handles.message_of(id) == Some(card_id.as_str());
                        let header = if registered {
                            post_resolution_header.as_ref()
                        } else {
                            None
                        };
                        repaint_card(
                            &mut handles,
                            &card_id,
                            id,
                            &residue,
                            &mut stamped_cards,
                            header,
                            &mut patches,
                        );
                    }
                }
            }
            handles.forget(id);
        }
    }
    for (message_id, card) in patches {
        if let Err(e) = cards.feishu.update_message(&message_id, &card).await {
            tracing::warn!("resolved block repaint failed on {}: {}", message_id, e);
        }
    }
    // The settlement rendered (or found nothing to render): release the
    // in-flight claim, so a standalone copy of the same request is left to
    // `mark_stale_cards` again.
    {
        let mut settling = requests.settling_requests.lock().await;
        for id in ids {
            settling.remove(id);
        }
    }
    ack
}

/// Whether session `candidate` belongs to `session_id` — the session itself or
/// one of its sub-task descendants (parent-chain walk). The ownership filter
/// `/autoaccept`'s pending approval and the turn-end leftover rejection (#187)
/// share; an empty id matches nothing.
pub(crate) async fn session_belongs_to(
    sessions: &SessionsHandle,
    backend: &Arc<dyn opencode::Backend>,
    candidate: &str,
    session_id: &str,
    directory: &str,
) -> bool {
    !candidate.is_empty()
        && (candidate == session_id
            || sessions
                .descends_from(backend, candidate, session_id, directory)
                .await)
}

/// #187: settle what a turn that ended without completing left behind. Reject
/// the still-pending Permission/Question requests of the turn's session and
/// its sub-task descendants on the server, then resolve their blocks through
/// the one seam into `🚫 已拒绝` receipts — nobody else handled them, so the
/// neutral `⏱ 已由其他客户端处理` line would be a lie. A no-op without a mapped
/// directory and on a failed list (unknown is never resolved). Returns how
/// many requests were rejected.
pub(crate) async fn reject_leftovers_for_turn(
    requests: &RequestsHandle,
    cards: &CardsHandle,
    sessions: &SessionsHandle,
    backend: &Arc<dyn opencode::Backend>,
    session_id: &str,
) -> usize {
    let directory = sessions.directory_for_session(session_id).await;
    let Some(directory) = directory.filter(|d| !d.is_empty()) else {
        return 0;
    };
    let mut rejected = 0;
    for flow in [&requests.permission, &requests.question] {
        let ids = flow
            .reject_pending_for_session(requests, sessions, backend, session_id, &directory)
            .await;
        if ids.is_empty() {
            continue;
        }
        rejected += ids.len();
        resolve_blocks(
            flow,
            cards,
            requests,
            &Some(session_id.to_string()),
            session_id,
            Origin::Command,
            &ids,
            Residue::PerBlock(&denied_receipt),
        )
        .await;
    }
    if rejected > 0 {
        tracing::info!(
            "turn {} ended without completing: rejected {} leftover request(s)",
            session_id,
            rejected
        );
    }
    rejected
}

/// Restamp an edited card's header title and template from the accumulator's
/// post-resolution state, keeping the header's subtitle (the session/date
/// line) untouched. The cached card's own header was captured while the block
/// was still live, so without this a click would leave the "等待你的授权 /
/// 回答" title showing until the next render-poll flush.
fn restamped_header(
    mut card: serde_json::Value,
    header: Option<&(String, &'static str)>,
) -> serde_json::Value {
    if let Some((title, template)) = header
        && let Some(h) = card.get_mut("header").and_then(|h| h.as_object_mut())
    {
        h.insert(
            "title".to_string(),
            serde_json::json!({ "tag": "plain_text", "content": title }),
        );
        h.insert("template".to_string(), serde_json::json!(template));
    }
    card
}

/// Settle the callback ack for a click (ADR-0038, rule 3): a card-handle edit
/// (the clicked card's cached JSON, updated through the seam) wins; otherwise
/// an inline click re-renders the accumulator's current card and a standalone
/// click keeps its result card.
async fn settle_ack(
    core: &Arc<SharedCore>,
    host: &Option<String>,
    session_id: &str,
    inline: bool,
    cached: Option<serde_json::Value>,
    result: &mut CardActionResult,
) {
    if let Some(card) = cached {
        result.card = Some(card);
    } else if inline {
        result.card = ack_inline_card(core, host, session_id).await;
    }
}

/// Keep a live question block's display state in step everywhere it renders
/// (ADR-0038, rule 2): the accumulator's section — the render source every
/// later flush re-renders the markers from — and, through its handle, the
/// clicked card's cached JSON, whose refreshed copy is the callback ack. One
/// seam, so the two cannot drift. Returns the refreshed clicked card when a
/// handle carries the block.
#[allow(clippy::too_many_arguments)] // the display state is what the seam keeps in step
async fn refresh_question_block(
    core: &Arc<SharedCore>,
    host: &Option<String>,
    session_id: &str,
    clicked: Option<&str>,
    req_id: &str,
    request: &opencode::types::QuestionRequest,
    directory: &str,
    display: &[Option<Vec<String>>],
    done: &[bool],
) -> Option<serde_json::Value> {
    Turn::update_question_state(
        &core.cards_handle(),
        host.as_deref().unwrap_or(session_id),
        req_id,
        display,
        done,
    )
    .await;
    let message_id = clicked?;
    let elements = crate::feishu::card::question::question_elements(
        req_id,
        &request.session_id,
        &request.questions,
        directory,
        display,
        done,
    );
    core.card_handles
        .lock()
        .await
        .refresh_on(message_id, req_id, elements)
}

/// The clicked card's updated JSON, built from a CLONE of the host accumulator
/// — a split probe that cannot advance the live `render_from` from inside a
/// click handler (`flush_card` owns that flow). `None` when the card needs a
/// split or no accumulator exists; the caller falls back to the PATCH flush.
async fn inline_ack_card(
    core: &Arc<SharedCore>,
    host: &Option<String>,
    session_id: &str,
) -> Option<serde_json::Value> {
    Turn::ack_card(&core.cards_handle(), host.as_deref().unwrap_or(session_id)).await
}

/// Deliver the clicked card's updated card in the callback ack (ADR-0038,
/// rule 3): the host accumulator was already mutated through the seam, so the
/// ack carries the receipt / partial state atomically — no PATCH race and no
/// dependence on which card is current. Falls back to the PATCH flush when the
/// card needs a split or the accumulator is gone (the turn ended between the
/// click and now).
async fn ack_inline_card(
    core: &Arc<SharedCore>,
    host: &Option<String>,
    session_id: &str,
) -> Option<serde_json::Value> {
    let card = inline_ack_card(core, host, session_id).await;
    if card.is_none() {
        flush_inline_card(core, host, session_id).await;
    }
    card
}

/// Re-render the live streaming card so inline interaction state (✅/已选,
/// receipts, removed buttons) shows immediately. An interaction-blocked prompt
/// produces no new parts, so without an explicit flush the render poll never
/// fires until the AI resumes — the card would stay frozen on the pre-answer
/// state. Same reason the question paths flush explicitly.
async fn flush_inline_card(core: &Arc<SharedCore>, host: &Option<String>, session_id: &str) {
    Turn::flush_card(&core.cards_handle(), host.as_deref().unwrap_or(session_id)).await;
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
pub(crate) fn describe_permission(p: &opencode::types::PermissionRequest) -> String {
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
        let parsed = crate::feishu::card::tool_render::parse_edit_diff(diff);
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

    fn perm(action: &str, metadata: serde_json::Value) -> crate::opencode::types::PermissionRequest {
        crate::opencode::types::PermissionRequest {
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
