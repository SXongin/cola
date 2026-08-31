use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::bridge::core::SharedCore;
use crate::bridge::handler::CardActionResult;
use crate::bridge::pollers::{
    CardTarget, inline_host_session, mark_stale_cards, resolve_card_target, result_card,
};
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
}

/// Partial answers recorded for a pending question request: `answers[i]` is
/// `None` until the user answers question `i`. A request is only submitted once
/// every slot is filled (or the user clicks "submit/skip").
pub type QuestionPartial = HashMap<String, Vec<Option<Vec<String>>>>;

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

    async fn handle_action(
        &self,
        flow: &RequestFlow,
        core: &Arc<SharedCore>,
        _session_id: &str,
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

        // Double-click guard: once answered, a second click only re-serves the
        // result.
        if !flow.is_answered(core, req_id).await {
            flow.mark_answered(core, req_id).await;
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
            if let Err(e) = reply_result {
                // The request is probably already resolved by another client (e.g.
                // OpenChamber) — show feedback instead of leaving the user with a
                // dead card and no response.
                tracing::error!("perm reply failed: {}", e);
                return Some(failed_result_card(
                    inline,
                    "该权限请求可能已在其他端处理。",
                    "可能已在其他端处理",
                ));
            }
            tracing::info!("Permission reply sent: {} session={}", reply, req_id);
            flow.sent_cards.lock().await.remove(req_id);
            if let Some(host) = host
                && let Some(acc) = core.cards.lock().await.get_mut(host).map(|c| &mut c.acc)
            {
                acc.pending_permissions.retain(|p| p.request_id != req_id);
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
/// remembered requests and partial slots live on the [`RequestFlow`] so tests
/// can seed them directly.
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
        _dir: &str,
    ) -> bool {
        if let PendingRequest::Question(q) = req {
            flow.question_requests
                .lock()
                .await
                .insert(q.id.clone(), q.clone());
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
            "answer" => {
                let index = value.get("question_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let answer = value
                    .get("answer")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if answer.is_empty() {
                    return None;
                }
                // A request is submitted ONLY when every question has an answer
                // (`reply_question` expects all of them).
                if flow.is_answered(core, req_id).await {
                    // finalized before (double-click): replay the result.
                    let mut r = result_card(
                        &format!("✅ 已回答：{}", answer),
                        "green",
                        &format!("AI 的问题是：\n{}", answer),
                    );
                    if inline {
                        r.card = None;
                    }
                    return Some(r);
                }
                let n = {
                    let reqs = flow.question_requests.lock().await;
                    reqs.get(req_id).map(|r| r.questions.len())
                };
                let Some(n) = n else {
                    return None; // stale card for an already-submitted request
                };
                // Record this question's answer.
                let (answered_count, slot) = {
                    let mut partial = flow.question_partial.lock().await;
                    let (count, slot) = record_answer(&mut partial, req_id, n, index, &answer);
                    (count, slot)
                };
                // Keep the inline card's partial answers in sync.
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
                    pq.answers = slot.clone();
                }
                if answered_count == n {
                    // All questions answered → submit the whole request.
                    flow.mark_answered(core, req_id).await;
                    let answers: Vec<Vec<String>> = flow
                        .question_partial
                        .lock()
                        .await
                        .remove(req_id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|a| a.unwrap_or_default())
                        .collect();
                    if let Err(e) = reply_question_scoped(core, req_id, Some(&answers), directory).await {
                        tracing::error!("question reply failed: {}", e);
                        return Some(failed_result_card(
                            inline,
                            "该问题可能已在其他端回答。",
                            "可能已在其他端回答",
                        ));
                    }
                    tracing::info!(
                        "Question answered: {} session={} -> {:?}",
                        req_id,
                        value.get("session_id").and_then(|v| v.as_str()).unwrap_or(""),
                        answers
                    );
                    flow.question_requests.lock().await.remove(req_id);
                    flow.sent_cards.lock().await.remove(req_id);
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
                    let mut r = result_card(
                        &format!("✅ 已回答：{}", answer),
                        "green",
                        &format!("AI 的问题是：\n{}", answer),
                    );
                    if inline {
                        r.card = None;
                    }
                    r.toast = Some("已回答".to_string());
                    if inline {
                        flush_inline_card(core, host, session_id).await;
                    }
                    Some(r)
                } else if inline {
                    // Inline: the streaming card re-renders with the updated
                    // partial answers — toast only.
                    let mut r = CardActionResult {
                        card: None,
                        toast: None,
                    };
                    r.toast = Some(format!("已记录答案，还有 {} 题未答", n - answered_count));
                    // Re-render the streaming card NOW so the answered question
                    // shows ✅/已选 and its buttons disappear. Without this the
                    // card stays frozen: a question-blocked prompt produces no
                    // new parts, so the render poll never flushes.
                    flush_inline_card(core, host, session_id).await;
                    Some(r)
                } else {
                    // Still questions left: return an updated card that shows the
                    // answered ones as done and the rest open.
                    let remaining = n - answered_count;
                    let req = {
                        let reqs = flow.question_requests.lock().await;
                        reqs.get(req_id).cloned()
                    };
                    let req = req?;
                    let partial = flow
                        .question_partial
                        .lock()
                        .await
                        .get(req_id)
                        .cloned()
                        .unwrap_or_else(|| vec![None; n]);
                    let card = crate::feishu::card::build_question_card(
                        req_id,
                        &req.session_id,
                        &req.questions,
                        directory.unwrap_or(""),
                        &partial,
                    );
                    let mut r = CardActionResult {
                        card: Some(card),
                        toast: None,
                    };
                    r.toast = Some(format!("已记录答案，还有 {} 题未答", remaining));
                    Some(r)
                }
            }
            "submit" => {
                // Finalize with whatever was answered (empty for the rest).
                if flow.is_answered(core, req_id).await {
                    let mut r = result_card("✅ 已回答", "green", "已提交 AI 的问题答案。");
                    if inline {
                        r.card = None;
                    }
                    return Some(r);
                }
                flow.mark_answered(core, req_id).await;
                let answers: Vec<Vec<String>> = flow
                    .question_partial
                    .lock()
                    .await
                    .remove(req_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| a.unwrap_or_default())
                    .collect();
                if let Err(e) = reply_question_scoped(core, req_id, Some(&answers), directory).await {
                    tracing::error!("question reply failed: {}", e);
                    return Some(failed_result_card(
                        inline,
                        "该问题可能已在其他端回答。",
                        "可能已在其他端回答",
                    ));
                }
                tracing::info!(
                    "Question submitted: {} session={} -> {:?}",
                    req_id,
                    value.get("session_id").and_then(|v| v.as_str()).unwrap_or(""),
                    answers
                );
                flow.question_requests.lock().await.remove(req_id);
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
                let labels: Vec<&str> = answers.iter().flatten().map(|s| s.as_str()).collect();
                let summary = if labels.is_empty() {
                    "已提交".to_string()
                } else {
                    labels.join("、")
                };
                let mut r = result_card(
                    &format!("✅ 已回答：{}", summary),
                    "green",
                    &format!("AI 的问题是：\n{}", summary),
                );
                if inline {
                    r.card = None;
                }
                r.toast = Some("已提交".to_string());
                if inline {
                    flush_inline_card(core, host, session_id).await;
                }
                Some(r)
            }
            "reject" => {
                if flow.is_answered(core, req_id).await {
                    let mut r = result_card("🚫 已拒绝回答", "red", "已拒绝回答 AI 的问题。");
                    if inline {
                        r.card = None;
                    }
                    return Some(r);
                }
                flow.mark_answered(core, req_id).await;
                if let Err(e) = reply_question_scoped(core, req_id, None, directory).await {
                    tracing::error!("question reject failed: {}", e);
                    return Some(failed_result_card(
                        inline,
                        "该问题可能已在其他端回答。",
                        "可能已在其他端回答",
                    ));
                }
                tracing::info!("Question rejected: {}", req_id);
                flow.question_requests.lock().await.remove(req_id);
                flow.question_partial.lock().await.remove(req_id);
                flow.sent_cards.lock().await.remove(req_id);
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
    /// request_id → (card message_id, description) of the card cola sent (used
    /// to mark a card stale when the request is resolved by ANOTHER client).
    pub sent_cards: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// request_id → pending question request (question kind only; the AI asks
    /// the user and cola posts the answer back from the question card).
    pub question_requests: Arc<Mutex<HashMap<String, opencode::client::QuestionRequest>>>,
    /// request_id → answers recorded so far (None = not answered yet; question
    /// kind only). A request is only submitted once EVERY question has an
    /// answer (or the user clicks "submit/skip"), because `reply_question`
    /// expects answers for all of them.
    pub question_partial: Arc<Mutex<QuestionPartial>>,
}

impl RequestFlow {
    pub fn new(kind: Box<dyn RequestKind>) -> Self {
        Self {
            kind,
            poll_interval_ms: std::sync::atomic::AtomicU64::new(3000),
            sent_cards: Arc::new(Mutex::new(HashMap::new())),
            question_requests: Arc::new(Mutex::new(HashMap::new())),
            question_partial: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The double-click guard primitive: whether `req_id` was already answered
    /// (the shared `answered_requests` set).
    pub(crate) async fn is_answered(&self, core: &Arc<SharedCore>, req_id: &str) -> bool {
        core.answered_requests.lock().await.contains(req_id)
    }

    /// The double-click guard primitive: record `req_id` as answered.
    pub(crate) async fn mark_answered(&self, core: &Arc<SharedCore>, req_id: &str) {
        core.answered_requests.lock().await.insert(req_id.to_string());
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
            // Serverless (Lazy Start hasn't attached/spawned yet): there is
            // nothing to poll — skip quietly until a server appears.
            if core.opencode.base_url().is_empty() {
                continue;
            }
            // Pending requests live in the server instance for the session's
            // directory; `GET /permission` / `GET /question` must be scoped with
            // `?directory=` or they only see the server cwd instance. Check every
            // known session directory.
            let directories = { core.sessions.lock().await.directories() };
            let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
            for dir in &directories {
                let backend = core.opencode.clone().for_directory(dir);
                match self.kind.list(&backend).await {
                    Ok(requests) => {
                        for req in &requests {
                            pending.insert(req.id().to_string());
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
                    Err(e) => tracing::warn!("poll {} ({}): {}", self.kind.label(), dir, e),
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
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let directory = match directory {
            Some(d) => Some(d),
            None => core.sessions.lock().await.directory_for_session(session_id),
        };
        let directory = directory.as_deref();

        let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
        let request_id = value.get("request_id").and_then(|v| v.as_str());
        let req_id = request_id?;
        // Inline interaction: the session (or its sub-task parent chain) has a
        // live streaming card, so the result is NOT returned as a replacement
        // card — the streaming card re-renders itself on the next poll.
        let host = if core.cards.lock().await.contains_key(session_id) {
            Some(session_id.to_string())
        } else {
            inline_host_session(core, session_id, directory).await
        };
        let inline = host.is_some();

        self.kind
            .handle_action(
                self, core, session_id, req_id, reply, value, directory, inline, &host,
            )
            .await
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

/// A friendly description of what a permission request asks to do, shown on the
/// permission card.
fn describe_permission(p: &opencode::client::PermissionRequest) -> String {
    let action = p.permission.as_deref().unwrap_or("?");
    let (emoji, label) = describe_action(action);
    let mut s = format!("{} **{}**\n", emoji, label);

    if !p.patterns.is_empty() {
        s.push_str("**对象**:\n");
        for r in &p.patterns {
            s.push_str(&format!("- `{}`\n", truncate(r, 150)));
        }
    }

    // Metadata often carries richer context (e.g. bash command, tool input)
    if let Some(meta) = &p.metadata {
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

/// Record one answer into the partial-answer slots for a question request.
/// Returns `(answered_count, slot)` where `slot` is the full (cloned) answer
/// vector — the caller syncs it to the inline card and submits once
/// `answered_count == n`.
fn record_answer(
    partial: &mut HashMap<String, Vec<Option<Vec<String>>>>,
    req_id: &str,
    n: usize,
    index: usize,
    answer: &str,
) -> (usize, Vec<Option<Vec<String>>>) {
    let slot = partial.entry(req_id.to_string()).or_insert_with(|| vec![None; n]);
    if index < n {
        slot[index] = Some(vec![answer.to_string()]);
    }
    let count = slot.iter().filter(|a| a.is_some()).count();
    (count, slot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_answer_starts_empty_and_fills_slots() {
        let mut partial = HashMap::new();
        // First answer, index 0 of 2.
        let (count, slot) = record_answer(&mut partial, "q1", 2, 0, "main");
        assert_eq!(count, 1);
        assert_eq!(slot, vec![Some(vec!["main".to_string()]), None]);
        // Second answer, index 1 → all filled.
        let (count, slot) = record_answer(&mut partial, "q1", 2, 1, "dev");
        assert_eq!(count, 2);
        assert_eq!(
            slot,
            vec![Some(vec!["main".to_string()]), Some(vec!["dev".to_string()])]
        );
    }

    #[test]
    fn record_answer_replacing_a_slot_keeps_count_stable() {
        let mut partial = HashMap::new();
        record_answer(&mut partial, "q1", 2, 0, "a");
        record_answer(&mut partial, "q1", 2, 1, "b");
        // Re-answering slot 0 replaces it without changing the count.
        let (count, slot) = record_answer(&mut partial, "q1", 2, 0, "c");
        assert_eq!(count, 2);
        assert_eq!(
            slot,
            vec![Some(vec!["c".to_string()]), Some(vec!["b".to_string()])]
        );
    }

    #[test]
    fn record_answer_out_of_range_index_is_ignored() {
        let mut partial = HashMap::new();
        let (count, slot) = record_answer(&mut partial, "q1", 2, 99, "x");
        assert_eq!(count, 0);
        assert_eq!(slot, vec![None, None]);
    }

    #[test]
    fn record_answer_isolates_requests_by_id() {
        let mut partial = HashMap::new();
        record_answer(&mut partial, "q1", 1, 0, "a");
        let (count, _) = record_answer(&mut partial, "q2", 1, 0, "b");
        assert_eq!(count, 1); // q2 has its own slots
    }
}
