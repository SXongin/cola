//! The request-kind adapters (spec #298, ticket C): everything that differs
//! between a permission wait and a question wait — the endpoint each lists,
//! the card each builds, the state each keeps, and the click semantics each
//! applies. The poll engine, the guards and the delivery seam are shared, so a
//! third request kind is one adapter here (plus its registration in
//! `SharedCore`).

use std::sync::Arc;

use crate::bridge::handler::CardActionResult;
use crate::bridge::handles::{CardsHandle, FlowHandles, RequestsHandle, SessionsHandle};
use crate::bridge::pollers::result_card;
use crate::bridge::question::{MultiOutcome, action_toast, qa_completion_body, question_replay_card};
use crate::bridge::snapshot_claims::ClaimKind;
use crate::bridge::turn::Turn;
use crate::opencode;

use super::delivery::{
    DENIED_PREFIX, HANDLED_ELSEWHERE_PREFIX, Origin, RECEIPT_MAX_CHARS, Residue, ack_inline_card,
    already_handled_result, denied_receipt, failed_result_card, handled_elsewhere_receipt, receipt_line,
    refresh_question_block, resolve_blocks, settle_ack, truncate,
};
use super::flow::{RequestFlow, session_belongs_to};

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

    /// The inline block elements this request contributes to a Session
    /// Snapshot (ADR-0028): the same controls the standalone card shows, so
    /// the snapshot looks and behaves like today's inline sections. Lives with
    /// the payload's adapters — a third kind adds one arm here, never a
    /// variant branch in the card layer.
    pub(crate) fn snapshot_block_elements(
        &self,
        directory: &str,
        question_state: &crate::feishu::snapshot_card::SnapshotQuestionState,
    ) -> Vec<serde_json::Value> {
        match self {
            PendingRequest::Permission(p) => {
                let body = describe_permission(p);
                let sid = p.session_id.as_deref().unwrap_or("");
                let body = crate::feishu::card::sanitize::CardMarkdown::new().clean(&body);
                let mut els = vec![
                    serde_json::json!({ "tag": "markdown", "content": format!("🔐 **权限请求**\n{body}") }),
                ];
                els.extend(crate::feishu::card::question::permission_buttons(
                    sid,
                    &p.request_id,
                    &body,
                    directory,
                ));
                els
            }
            PendingRequest::Question(q) => {
                let n = q.questions.len();
                let state = question_state.get(&q.id).cloned().unwrap_or(
                    crate::feishu::snapshot_card::QuestionBlockState {
                        display: vec![None; n],
                        done: vec![false; n],
                    },
                );
                crate::feishu::card::question::question_elements(
                    &q.id,
                    &q.session_id,
                    &q.questions,
                    directory,
                    &state.display,
                    &state.done,
                )
            }
        }
    }
}

/// The deltas that make a permission request and a question request different.
/// Everything else — the poll sweep, the double-click and claim guards, and
/// the card-delivery helpers — lives once in the flow engine and the delivery
/// module.
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

    /// Reject one of this kind's pending requests on the server — the #187
    /// turn-end leftover path — and settle any kind-side in-flight state the
    /// landed rejection leaves behind. `backend` is already scoped to the
    /// request's directory.
    async fn reject(
        &self,
        flow: &RequestFlow,
        backend: &Arc<dyn opencode::DirectoryBackend>,
        req: &PendingRequest,
    ) -> crate::error::Result<()>;

    /// Runs once per newly-seen request before it becomes a card. Permissions
    /// answer `/autoaccept` sessions here and return true (no card); questions
    /// remember the full request (via the flow) for later card rebuilds and
    /// return false.
    async fn prepare(
        &self,
        flow: &RequestFlow,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
    ) -> bool;

    /// Remember a request that reached a card WITHOUT the poll loop surfacing
    /// it (a Session Snapshot claim, a busy-follow host), so its block's
    /// buttons still resolve — `prepare` never runs for those. Defaults to
    /// nothing: a kind with no in-flight state has nothing to remember.
    async fn remember_surfaced(&self, _flow: &RequestFlow, _req: &PendingRequest, _dir: &str) {}

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

    /// Seed this kind's inline block on a NEW host card with the request's
    /// initial (adopt-time) state — the busy-follow host path, which
    /// reproduces the static snapshot's blocks before the renderer takes over
    /// and remembers the request right after. Defaults to [`Self::add_inline`]:
    /// a kind with no remembered state has nothing to restore anyway.
    async fn add_initial_inline(
        &self,
        flow: &RequestFlow,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        self.add_inline(flow, cards, host, req, dir).await
    }

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

    /// The live state this kind contributes to a Session Snapshot re-render
    /// (ADR-0028). Defaults to nothing: only the question kind has snapshot
    /// state (its 已选/✅ markers), which `rebuild`/`resolve` paint in place.
    async fn snapshot_state(
        &self,
        _flow: &RequestFlow,
        _pending: &[PendingRequest],
    ) -> crate::feishu::snapshot_card::SnapshotQuestionState {
        crate::feishu::snapshot_card::SnapshotQuestionState::new()
    }

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
        handles: &FlowHandles,
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

    async fn reject(
        &self,
        _flow: &RequestFlow,
        backend: &Arc<dyn opencode::DirectoryBackend>,
        req: &PendingRequest,
    ) -> crate::error::Result<()> {
        let PendingRequest::Permission(p) = req else {
            return Ok(());
        };
        backend.reply_permission(&p.request_id, "reject").await
    }

    async fn prepare(
        &self,
        _flow: &RequestFlow,
        handles: &FlowHandles,
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
        let auto = should_auto_accept(&handles.sessions, &handles.backend, &sid, dir).await;
        if !auto {
            return false;
        }
        match handles
            .backend
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
        handles: &FlowHandles,
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
                None => handles
                    .sessions
                    .store
                    .lock()
                    .await
                    .directory_for_session(session_id)
                    .unwrap_or_default(),
            };
            let mut cached = None;
            if flow.try_mark_answered(&handles.requests, req_id).await {
                let mut approved = set_auto_accept(
                    &handles.sessions,
                    &handles.requests,
                    &handles.backend,
                    session_id,
                    &dir,
                    true,
                )
                .await;
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
                    &handles.cards,
                    &handles.requests,
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
            settle_ack(&handles.cards, host, session_id, inline, cached, &mut r).await;
            // Re-served verbatim to a losing double-click.
            flow.remember_answered_result(req_id, &r).await;
            return Some(r);
        }

        // Double-click guard: the atomic claim decides who replies. A click
        // that loses the race re-serves the winning click's result; only the
        // winner may reach the backend.
        if !flow.try_mark_answered(&handles.requests, req_id).await {
            return flow.answered_result(req_id).await;
        }
        // Route the reply to the instance owning the session. The card
        // carries the owning directory (ADR-0010); without it the reply
        // can't be routed, so surface the failure instead of guessing at
        // the server cwd instance.
        let reply_result = match directory {
            Some(dir) => {
                handles
                    .backend
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
                    &handles.cards,
                    &handles.requests,
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
                    &handles.cards,
                    &handles.requests,
                    host,
                    session_id,
                    Origin::Click { clicked },
                    &[req_id.to_string()],
                    Residue::PerBlock(&handled_elsewhere_receipt),
                )
                .await;
                let mut r = already_handled_result(self.label(), inline, "该权限已处理");
                settle_ack(&handles.cards, host, session_id, inline, cached, &mut r).await;
                flow.remember_answered_result(req_id, &r).await;
                return Some(r);
            }
            // Genuine failure (network, routing): roll the claim back so a
            // retry can reply again.
            Err(e) => {
                tracing::error!("perm reply failed: {}", e);
                flow.unmark_answered(&handles.requests, req_id).await;
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
        settle_ack(&handles.cards, host, session_id, inline, cached, &mut r).await;
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
async fn should_auto_accept(
    sessions: &SessionsHandle,
    backend: &Arc<dyn opencode::Backend>,
    session_id: &str,
    directory: &str,
) -> bool {
    crate::bridge::pollers::walk_parent_chain(backend, session_id, Some(directory), |current| {
        let current = current.to_string();
        async move {
            sessions
                .store
                .lock()
                .await
                .entry_for_session(&current)
                .map(|e| e.auto_accept)
        }
    })
    .await
    .unwrap_or(false)
}

/// Turn a session's Auto-Accept flag on/off, resolving the owning session
/// (which may be a parent of a sub-task child) and approving any already-pending
/// permissions when turning on. Mirrors `/autoaccept` and is shared by the
/// permission-card toggle and the coordinator's wrapper so both paths stay in
/// lockstep. Returns the ids of the pending requests that were approved (empty
/// when `on` is false), so the caller can drop their inline card sections.
pub(crate) async fn set_auto_accept(
    sessions: &SessionsHandle,
    requests: &RequestsHandle,
    backend: &Arc<dyn opencode::Backend>,
    session_id: &str,
    directory: &str,
    on: bool,
) -> Vec<String> {
    let approved = if on {
        approve_pending_for_session(sessions, requests, backend, session_id, directory).await
    } else {
        Vec::new()
    };
    // Resolve the SessionStore entry that owns the flag: `session_id`
    // itself, or its nearest ancestor (sub-task children are not in the
    // store, ADR-0010). Walking the chain makes a child's card flip the
    // parent's flag, consistent with `should_auto_accept`.
    let owner = crate::bridge::pollers::walk_parent_chain(backend, session_id, Some(directory), |current| {
        let current = current.to_string();
        async move { sessions.entry_for_session(&current).await }
    })
    .await;
    if let Some(entry) = owner
        && let Err(e) = sessions.update(&entry.session_id, |e| e.auto_accept = on).await
    {
        tracing::warn!("set_auto_accept: persist failed: {}", e);
    }
    approved
}

/// After `/autoaccept on`: answer every permission request that is ALREADY
/// pending for `session_id` (or one of its sub-task child sessions) with
/// "once". The permission poller's `seen` set skips requests it has already
/// surfaced, so enabling autoaccept would otherwise leave old cards hanging
/// forever. Returns the ids of the requests that were approved.
pub(crate) async fn approve_pending_for_session(
    sessions: &SessionsHandle,
    requests: &RequestsHandle,
    backend: &Arc<dyn opencode::Backend>,
    session_id: &str,
    directory: &str,
) -> Vec<String> {
    let Ok(perms) = backend.clone().for_directory(directory).list_permissions().await else {
        return Vec::new();
    };
    let mut approved = Vec::new();
    for p in &perms {
        // Match the session itself or a sub-task child (its parent chain).
        let sid = p.session_id.clone().unwrap_or_default();
        if !session_belongs_to(sessions, backend, &sid, session_id, directory).await {
            continue;
        }
        // Take the settlement claim BEFORE the reply lands: the request
        // leaves the server's pending list the moment it is applied, and a
        // sweep landing in that window would otherwise read the
        // disappearance as another client's resolution and stamp
        // `⏱ 已由其他客户端处理` on the card — the lie the Host saw when
        // enabling auto-accept. `resolve_blocks` clears the claim once the
        // true receipt rendered; a failed reply releases it right here.
        let claimed_here = requests
            .settling_requests
            .lock()
            .await
            .insert(p.request_id.clone(), std::time::Instant::now())
            .is_none();
        match backend
            .clone()
            .for_directory(directory)
            .reply_permission(&p.request_id, "once")
            .await
        {
            Ok(()) => {
                tracing::info!(
                    "Auto-accepted pending permission {} on session {} ({})",
                    p.request_id,
                    sid,
                    p.permission.as_deref().unwrap_or("?")
                );
                approved.push(p.request_id.clone());
            }
            Err(e) => {
                if claimed_here {
                    requests.settling_requests.lock().await.remove(&p.request_id);
                }
                tracing::warn!("auto-accept pending {} on session {}: {}", p.request_id, sid, e);
            }
        }
    }
    approved
}

/// Receipt for the Auto-Accept toggle: it names the MODE change, never the
/// requests it resolved — naming a command made the line read as "only this
/// command is now auto-approved". One per toggle, however many pending blocks
/// it swallowed (the blocks are dismissed without lines of their own). Shared
/// by the card button and the `/autoaccept on` command.
pub(crate) const AUTOACCEPT_RECEIPT: &str = "🔄 已开启自动授权：后续权限请求将自动批准";

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

    async fn reject(
        &self,
        flow: &RequestFlow,
        backend: &Arc<dyn opencode::DirectoryBackend>,
        req: &PendingRequest,
    ) -> crate::error::Result<()> {
        let PendingRequest::Question(q) = req else {
            return Ok(());
        };
        backend.reject_question(&q.id).await?;
        // The reply landed: drop the in-flight state like a reject click does,
        // so nothing serves stale answers.
        flow.remove_question(&q.id).await;
        Ok(())
    }

    async fn prepare(
        &self,
        flow: &RequestFlow,
        _handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        if let PendingRequest::Question(q) = req {
            flow.remember_question(q, dir).await;
        }
        false
    }

    async fn remember_surfaced(&self, flow: &RequestFlow, req: &PendingRequest, dir: &str) {
        if let PendingRequest::Question(q) = req {
            flow.remember_question(q, dir).await;
        }
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
        let (answers, done) = flow
            .with_question_state(&q.id, |state| {
                let (_, display, done) = state.merge();
                (display, done)
            })
            .await
            .unwrap_or_else(|| (vec![None; q.questions.len()], vec![false; q.questions.len()]));
        Turn::add_question(cards, host, q, dir, &answers, &done).await
    }

    async fn add_initial_inline(
        &self,
        _flow: &RequestFlow,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        let PendingRequest::Question(q) = req else {
            return false;
        };
        // The follow seeds the adopt-time block before anything was
        // remembered, so it starts open; the remembered state is recorded
        // right after (`remember_surfaced`).
        Turn::add_question(
            cards,
            host,
            q,
            dir,
            &vec![None; q.questions.len()],
            &vec![false; q.questions.len()],
        )
        .await
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

    async fn snapshot_state(
        &self,
        flow: &RequestFlow,
        pending: &[PendingRequest],
    ) -> crate::feishu::snapshot_card::SnapshotQuestionState {
        flow.with_question_states(|states| {
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
        })
        .await
    }

    fn claim_kind(&self) -> ClaimKind {
        ClaimKind::Question
    }

    async fn handle_action(
        &self,
        flow: &RequestFlow,
        handles: &FlowHandles,
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
                if flow.is_answered(&handles.requests, req_id).await {
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
                let fetched = flow
                    .with_question_state(req_id, |state| (state.len(), state.is_multi(index)))
                    .await;
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
                let mutated = flow
                    .with_question_state(req_id, |state| {
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
                    .await;
                let Some((answered_count, display, done, outcome)) = mutated else {
                    return Some(flow.missing_question_result(directory, inline).await);
                };
                if answered_count == n {
                    // All questions answered → claim the request and submit.
                    // The claim is atomic: a click that loses the race re-serves
                    // the winning result instead of replying again.
                    if !flow.try_mark_answered(&handles.requests, req_id).await {
                        return Some(
                            flow.answered_result(req_id)
                                .await
                                .unwrap_or_else(|| question_replay_card(inline)),
                        );
                    }
                    // The claim does not pin the state: re-validate so a sweep
                    // racing the claim cannot submit an empty snapshot.
                    let Some((questions, answers)) = flow.question_snapshot(req_id).await else {
                        return Some(
                            flow.missing_after_claim(&handles.requests, req_id, directory, inline)
                                .await,
                        );
                    };
                    if let Some(r) = settle_question_reply(
                        flow,
                        &handles.cards,
                        &handles.requests,
                        req_id,
                        self.label(),
                        reply_question_scoped(&handles.backend, req_id, Some(&answers), directory).await,
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
                        &handles.cards,
                        &handles.requests,
                        host,
                        session_id,
                        Origin::Click { clicked },
                        &[req_id.to_string()],
                        Residue::PerBlock(&|_| question_receipt(&questions, &answers)),
                    )
                    .await;
                    let mut r = result_card("✅ 已回答", "green", &qa_completion_body(&questions, &answers));
                    r.toast = Some("已回答".to_string());
                    settle_ack(&handles.cards, host, session_id, inline, cached, &mut r).await;
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
                    let req = flow.question_request(req_id).await;
                    let card = match req {
                        Some(req) => {
                            refresh_question_block(
                                &handles.cards,
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
                            None => ack_inline_card(&handles.cards, host, session_id).await,
                        },
                        toast: Some(action_toast(reply, n - answered_count, outcome)),
                    };
                    Some(r)
                } else {
                    // Still questions left: return an updated card that shows the
                    // answered ones as done and the rest open.
                    let remaining = n - answered_count;
                    let req = flow.question_request(req_id).await;
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
                if flow.is_answered(&handles.requests, req_id).await {
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
                if !flow.try_mark_answered(&handles.requests, req_id).await {
                    return Some(
                        flow.answered_result(req_id)
                            .await
                            .unwrap_or_else(|| question_replay_card(inline)),
                    );
                }
                // The claim does not pin the state: re-validate so a sweep
                // racing the claim cannot turn this into a blind reply.
                let Some((questions, answers)) = flow.question_snapshot(req_id).await else {
                    return Some(
                        flow.missing_after_claim(&handles.requests, req_id, directory, inline)
                            .await,
                    );
                };
                if let Some(r) = settle_question_reply(
                    flow,
                    &handles.cards,
                    &handles.requests,
                    req_id,
                    self.label(),
                    reply_question_scoped(&handles.backend, req_id, Some(&answers), directory).await,
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
                    &handles.cards,
                    &handles.requests,
                    host,
                    session_id,
                    Origin::Click { clicked },
                    &[req_id.to_string()],
                    Residue::PerBlock(&|_| question_receipt(&questions, &answers)),
                )
                .await;
                let mut r = result_card("✅ 已回答", "green", &qa_completion_body(&questions, &answers));
                r.toast = Some("已提交".to_string());
                settle_ack(&handles.cards, host, session_id, inline, cached, &mut r).await;
                // Re-served verbatim to a losing double-click.
                flow.remember_answered_result(req_id, &r).await;
                Some(r)
            }
            "reject" => {
                if flow.is_answered(&handles.requests, req_id).await {
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
                if !flow.try_mark_answered(&handles.requests, req_id).await {
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
                    return Some(
                        flow.missing_after_claim(&handles.requests, req_id, directory, inline)
                            .await,
                    );
                }
                if let Some(r) = settle_question_reply(
                    flow,
                    &handles.cards,
                    &handles.requests,
                    req_id,
                    self.label(),
                    reply_question_scoped(&handles.backend, req_id, None, directory).await,
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
                    &handles.cards,
                    &handles.requests,
                    host,
                    session_id,
                    Origin::Click { clicked },
                    &[req_id.to_string()],
                    Residue::PerBlock(&denied_receipt),
                )
                .await;
                let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                r.toast = Some("已拒绝回答".to_string());
                settle_ack(&handles.cards, host, session_id, inline, cached, &mut r).await;
                // Re-served verbatim to a losing double-click.
                flow.remember_answered_result(req_id, &r).await;
                Some(r)
            }
            _ => None,
        }
    }
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
    cards: &CardsHandle,
    requests: &RequestsHandle,
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
                cards,
                requests,
                host,
                session_id,
                Origin::Click { clicked },
                &[req_id.to_string()],
                Residue::PerBlock(&handled_elsewhere_receipt),
            )
            .await;
            let mut r = already_handled_result(kind, inline, "该问题已处理");
            settle_ack(cards, host, session_id, inline, cached, &mut r).await;
            flow.remember_answered_result(req_id, &r).await;
            Some(r)
        }
        Err(e) => {
            tracing::error!("question reply failed: {}", e);
            flow.unmark_answered(requests, req_id).await;
            Some(failed_result_card(
                inline,
                "该问题处理失败，请重试。",
                "处理失败，请重试",
            ))
        }
    }
}

/// Route a question reply/reject to the instance owning the session. The card
/// carries the owning directory (ADR-0010); without it the reply can't be
/// routed, so surface the failure instead of guessing at the server cwd instance.
async fn reply_question_scoped(
    backend: &Arc<dyn opencode::Backend>,
    req_id: &str,
    answers: Option<&[Vec<String>]>,
    directory: Option<&str>,
) -> crate::error::Result<()> {
    let Some(dir) = directory else {
        return Err(crate::error::BridgeError::OpenCode(
            "question card carries no directory".into(),
        ));
    };
    let backend = backend.clone().for_directory(dir);
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
