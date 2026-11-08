//! The request-kind adapters (spec #298, ticket C): everything that differs
//! between a permission wait and a question wait — the endpoint each lists,
//! the card each builds, the state each keeps, and the click semantics each
//! applies. The poll engine, the guards and the delivery seam are shared, so a
//! third request kind is one adapter here (plus its registration in
//! `SharedCore`).

use std::sync::Arc;

use tracing::Instrument;

use crate::backend::{PERMISSION_BODY_MAX_CHARS, PERMISSION_TARGET_MAX_CHARS};
use crate::bridge::handler::CardActionResult;
use crate::bridge::handles::{CardsHandle, FlowHandles, RequestsHandle, SessionsHandle};
use crate::bridge::pollers::result_card;
use crate::bridge::question::{MultiOutcome, action_toast, qa_completion_body, question_replay_card};
use crate::bridge::surfaces::CompanionDelivery;
use crate::bridge::turn::Turn;
use crate::feishu::card::lang::language_for_path;
use crate::feishu::card::question::FallbackLoss;
use crate::feishu::card::tool_render::{
    DIFF_MAX_ESCAPED_BYTES, DIFF_TRUNCATION_NOTE, clip_fenced_body, diff_from_metadata, parse_edit_diff,
};

use super::companion::{
    companion_target_digest, describe_permission_for, describe_permission_target_summary,
    patterns_target_digest,
};
use super::delivery::{
    DENIED_PREFIX, Origin, RECEIPT_MAX_CHARS, Residue, ack_inline_card, already_handled_result,
    denied_receipt, failed_result_card, handled_elsewhere_receipt, receipt_line, refresh_question_block,
    resolve_blocks, settle_ack, truncate,
};
use super::flow::{RequestFlow, session_belongs_to};

/// Which interaction-block kind a live card section belongs to (ADR-0038). One
/// registry holds both kinds' blocks, and each flow's poll sweep only touches
/// blocks of its own kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum InteractionKind {
    Permission,
    Question,
}

/// A pending request surfaced by a poll loop before it becomes a card. Carries
/// whichever backend payload the kind produced — permission or question.
#[derive(Clone)]
pub enum PendingRequest {
    Permission(crate::backend::types::PermissionRequest),
    Question(crate::backend::types::QuestionRequest),
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

    /// The interaction-block kind of this request.
    pub fn interaction_kind(&self) -> InteractionKind {
        match self {
            PendingRequest::Permission(_) => InteractionKind::Permission,
            PendingRequest::Question(_) => InteractionKind::Question,
        }
    }
}

/// The deltas that make a permission request and a question request different.
/// Everything else — the poll sweep, the double-click and claim guards, and
/// the card-delivery helpers — lives once in the flow engine and the delivery
/// module.
///
/// [`RequestKind::prepare`] runs before a card is built; a permission the card
/// cannot show takes the companion lane (ADR-0102), and a companion that cannot
/// be delivered takes the flow's fallback card. Only a request with no usable
/// card target is settled by the flow's decline path.
#[async_trait::async_trait]
#[allow(clippy::too_many_arguments)] // the action context is the trait's whole point
pub trait RequestKind: Send + Sync {
    /// Human label for logs and the stale-card text ("权限" / "问答").
    fn label(&self) -> &'static str;

    /// List pending requests for one server instance (the directory is owned by
    /// the handle, ADR-0010).
    async fn list(
        &self,
        backend: &Arc<dyn crate::backend::DirectoryBackend>,
    ) -> crate::error::Result<Vec<PendingRequest>>;

    /// Reject one of this kind's pending requests on the server — the #187
    /// turn-end leftover path — and settle any kind-side in-flight state the
    /// landed rejection leaves behind. `backend` is already scoped to the
    /// request's directory.
    async fn reject(
        &self,
        flow: &RequestFlow,
        backend: &Arc<dyn crate::backend::DirectoryBackend>,
        req: &PendingRequest,
    ) -> crate::error::Result<()>;

    /// Runs once per newly-seen request before it becomes a card.
    /// Permissions answer `/autoaccept` sessions here and return
    /// [`Prepared::Handled`] (no card); questions remember the full request
    /// (via the flow) for later card rebuilds and return
    /// [`Prepared::NotHandled`].
    async fn prepare(
        &self,
        flow: &RequestFlow,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
    ) -> Prepared;

    /// Remember a request that reached a card WITHOUT the poll loop surfacing
    /// it (a busy-follow host), so its block's buttons still resolve —
    /// `prepare` never runs for those. Defaults to nothing: a kind with no
    /// in-flight state has nothing to remember.
    async fn remember_surfaced(&self, _flow: &RequestFlow, _req: &PendingRequest, _dir: &str) {}

    /// The card JSON to repaint when the restart re-adoption re-hosts this
    /// kind's block on a card whose accumulator is gone (ADR-0038 restart
    /// re-adoption). Defaults to the cached JSON as-is: a kind whose block is
    /// deterministic from the request (a permission) has nothing to refresh.
    /// A kind with remembered display state (a question) overrides this to
    /// re-render the block from that state — its partial answers did not
    /// survive the restart, so the card must not keep showing them. `None`
    /// when the cached JSON cannot render the block.
    async fn refresh_cached_block(
        &self,
        _flow: &RequestFlow,
        cards: &CardsHandle,
        message_id: &str,
        _req: &PendingRequest,
        _dir: &str,
    ) -> Option<serde_json::Value> {
        cards.card_handles.lock().await.cached_card(message_id)
    }

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

    /// [`Self::add_inline`] for a caller that already holds the host's card
    /// write lock — the sweep's one surfacing decision (ADR-0100).
    async fn add_inline_locked(
        &self,
        flow: &RequestFlow,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool;

    /// Seed this kind's inline block on a NEW host card with the request's
    /// initial (adopt-time) state — the busy-follow host path, which
    /// reproduces the static read's blocks before the renderer takes over and
    /// remembers the request right after. The caller holds the host's card
    /// write lock: `/card`'s post, arm and seed are one critical section
    /// (ADR-0100).
    async fn add_initial_inline_locked(
        &self,
        flow: &RequestFlow,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        self.add_inline_locked(flow, cards, host, req, dir).await
    }

    /// Build the standalone interactive card (used when no streaming card hosts
    /// the request inline). The flow supplies the companion delivery a
    /// permission's summary needs.
    fn build_card(&self, flow: &RequestFlow, req: &PendingRequest, dir: &str) -> serde_json::Value;

    /// The description shown on the card when it is marked stale (resolved by
    /// another client). The flow supplies the companion delivery.
    fn summary(&self, flow: &RequestFlow, req: &PendingRequest) -> String;

    /// Resolve the kind's own inline blocks whose request vanished into their
    /// Interaction Receipts; `dead` names the owning sessions whose run is
    /// over, which get the interrupted line, every other session the neutral
    /// one. An item owned by a directory whose list call failed stays live:
    /// unknown must never be read as resolved (#130, #144). An item cola itself
    /// is answering (`answered`) also stays: the settlement owns its receipt,
    /// the sweep's line would be a lie. Returns the affected session ids — the
    /// sweep repaints each affected card so the receipt lands within one poll.
    async fn resolve_vanished_inline(
        &self,
        cards: &CardsHandle,
        pending: &std::collections::HashSet<String>,
        failed_dirs: &std::collections::HashSet<String>,
        cola_claimed: &std::collections::HashSet<String>,
        dead: &std::collections::HashSet<String>,
    ) -> Vec<String>;

    /// The interaction-block kind of this flow's requests: each flow's poll
    /// sweep only touches blocks of its own kind.
    fn interaction_kind(&self) -> InteractionKind;

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

/// Why a permission is being declined on the server. One reason type keeps the
/// reply skeleton shared while the copy follows the actual failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeclineReason {
    /// The card (and companion) cannot show the request in full: the target or
    /// rendered-body bound failed.
    Overflow(PermissionOverflow),
    /// The companion's delivery outcome is unknown; it may or may not have
    /// landed. An unconfirmed delivery is not a completeness proof.
    CompanionUncertain,
}

/// What [`decline_permission`] decided for one listed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Refused on the server. The request must not be surfaced, and any
    /// surface a previous process persisted for it is withdrawn. Carries the
    /// reason, so the reason card names it without re-deciding.
    Refused(DeclineReason),
    /// The server no longer holds the request: another client (or an earlier
    /// sweep) settled it. The stored surface is withdrawn **neutrally** — cola
    /// did not reject anything, so no auto-reject copy may be left behind — and
    /// nothing is posted.
    ResolvedElsewhere,
    /// The refusal could not be applied. The request is still pending, so it
    /// is retried on the next sweep — unmarked, and never surfaced meanwhile.
    /// A stored card is still made non-interactive, but with a "will retry"
    /// receipt that claims nothing.
    Failed,
}

/// What the click's verification read proved about the request it was
/// approving. One value drives the forced decision, the residue and the result
/// card, so the four states cannot drift.
enum ClickProof {
    /// No verification read (a denial, no directory, or a request already gone).
    None,
    /// The server holds the request; `delivery` is its confirmed companion (or
    /// `None` for an in-budget request).
    Confirmed {
        request: crate::backend::types::PermissionRequest,
        delivery: Option<CompanionDelivery>,
    },
    /// The user explicitly risk-confirmed an over-budget approval from the
    /// fallback card: no companion exists, but the button's digest matches the
    /// request the server still holds. This is the ONE deliberate exception to
    /// the delivery proof (ADR-0102 / #1013), and it is narrow: only the
    /// fallback confirmation button carries `risk_confirmed`.
    RiskConfirmed {
        request: crate::backend::types::PermissionRequest,
    },
    /// An over-budget request whose only record is an uncertain marker.
    Uncertain,
    /// An over-budget request with no matching companion record.
    Missing,
}

impl ClickProof {
    fn is_blocked(&self) -> bool {
        matches!(self, Self::Uncertain | Self::Missing)
    }
}

/// The toast for an ordinary permission decision.
fn toast_for(reply: &str) -> String {
    match reply {
        "once" => "已允许本次执行".to_string(),
        "always" => "已允许，后续将自动放行".to_string(),
        _ => "已拒绝".to_string(),
    }
}

/// The companion delivery proof for a request whose card showed a summary.
/// ANY overflow — the summed target budget or the rendered-body ceiling — needs
/// the one companion proof; neither is exempt. A stale record is `Missing`, an
/// uncertain marker is `Uncertain`; the caller turns either into a forced
/// refusal.
fn companion_proof(
    flow: &RequestFlow,
    req_id: &str,
    p: &crate::backend::types::PermissionRequest,
) -> ClickProof {
    let digest = companion_target_digest(p);
    match flow.surfaces.delivered_companion(req_id, digest) {
        Some(delivery) => ClickProof::Confirmed {
            request: p.clone(),
            delivery: Some(delivery),
        },
        None => {
            if flow.surfaces.matching_companion(req_id, digest).is_some() {
                ClickProof::Uncertain
            } else {
                ClickProof::Missing
            }
        }
    }
}

/// What [`RequestKind::prepare`] did for one listed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prepared {
    /// The kind handled the request: no card is needed.
    Handled,
    /// Not this kind's pre-card case: the flow continues to the surface.
    NotHandled,
    /// The handling was attempted and failed. The request is still pending, so
    /// the flow must leave it unsurfaced and **unmarked**, and the next sweep
    /// retries it. Without this arm a transient reply failure would retire the
    /// request from `seen` while no card exists — pending, invisible, and
    /// impossible to approve.
    Retry,
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
        backend: &Arc<dyn crate::backend::DirectoryBackend>,
    ) -> crate::error::Result<Vec<PendingRequest>> {
        backend
            .list_permissions()
            .await
            .map(|v| v.into_iter().map(PendingRequest::Permission).collect())
    }

    async fn reject(
        &self,
        _flow: &RequestFlow,
        backend: &Arc<dyn crate::backend::DirectoryBackend>,
        req: &PendingRequest,
    ) -> crate::error::Result<()> {
        let PendingRequest::Permission(p) = req else {
            return Ok(());
        };
        backend
            .reply_permission(p.session_id.as_deref().unwrap_or(""), &p.request_id, "reject")
            .await
    }

    async fn prepare(
        &self,
        flow: &RequestFlow,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
    ) -> Prepared {
        let PendingRequest::Permission(p) = req else {
            return Prepared::NotHandled;
        };
        let sid = p.session_id.clone().unwrap_or_default();
        // `/autoaccept` sessions: answer the request automatically instead of
        // showing a card (mirrors OpenChamber's toggle). Resolves sub-task child
        // sessions up their parent chain so they inherit the parent's flag.
        //
        // Auto-Accept is a delegation of the decision, not a surface: it
        // returns before the companion step and renders no card. A target past
        // the card budget is approved unseen, and the log line names its length
        // — that record is what keeps the decision auditable without a card
        // (#892).
        match should_auto_accept(flow, &handles.sessions, &handles.backend, &sid, dir).await {
            AutoAcceptFlag::On => {}
            AutoAcceptFlag::Off | AutoAcceptFlag::Capped => return Prepared::NotHandled,
            // Unknown: retry rather than surface a card the toggle may have
            // replaced (see `should_auto_accept`).
            AutoAcceptFlag::Unknown => return Prepared::Retry,
        }
        match handles
            .backend
            .clone()
            .for_directory(dir)
            .reply_permission(&sid, &p.request_id, "once")
            .await
        {
            Ok(()) => {
                tracing::info!(
                    "Auto-accepted permission {} on session {} ({})",
                    p.request_id,
                    sid,
                    p.permission.as_deref().unwrap_or("?")
                );
                log_unshowable_auto_accept(p);
                flow.note_autoaccept_success(&p.request_id).await;
                Prepared::Handled
            }
            // The request is still pending: hand it back to the sweep instead
            // of retiring it in `seen` with no card (see `Prepared::Retry`).
            // The latch WARNs the first failure and DEBUGs identical repeats
            // (ADR-0048) — a broken reply path must not flood the log.
            Err(e) => {
                flow.note_autoaccept_failure(&p.request_id, &e).await;
                Prepared::Retry
            }
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
        let PendingRequest::Permission(p) = req else {
            return false;
        };
        let companion = flow.companion_for(req);
        Turn::add_permission_with_companion(cards, host, p, dir, companion).await
    }

    async fn add_inline_locked(
        &self,
        flow: &RequestFlow,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        let PendingRequest::Permission(p) = req else {
            return false;
        };
        let companion = flow.companion_for(req);
        Turn::add_permission_with_companion_locked(cards, host, p, dir, companion).await
    }

    fn build_card(&self, flow: &RequestFlow, req: &PendingRequest, dir: &str) -> serde_json::Value {
        let PendingRequest::Permission(p) = req else {
            return serde_json::json!({});
        };
        let companion = flow.companion_for(req);
        let body = describe_permission_for(p, companion.as_ref());
        let summary_digest = companion.as_ref().map(|_| patterns_target_digest(p));
        crate::feishu::card::question::build_permission_card(
            p.session_id.as_deref().unwrap_or(""),
            &p.request_id,
            &body,
            dir,
            p.always_supported,
            summary_digest,
        )
    }

    fn summary(&self, flow: &RequestFlow, req: &PendingRequest) -> String {
        match req {
            PendingRequest::Permission(p) => {
                let companion = flow.companion_for(req);
                describe_permission_for(p, companion.as_ref())
            }
            PendingRequest::Question(_) => String::new(),
        }
    }

    async fn resolve_vanished_inline(
        &self,
        cards: &CardsHandle,
        pending: &std::collections::HashSet<String>,
        failed_dirs: &std::collections::HashSet<String>,
        cola_claimed: &std::collections::HashSet<String>,
        dead: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        Turn::resolve_vanished_permissions(cards, pending, failed_dirs, cola_claimed, dead).await
    }

    fn interaction_kind(&self) -> InteractionKind {
        InteractionKind::Permission
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
        // The label the clicked button carried, or the raw decision when a
        // malformed/stale callback omitted it.
        let display_label = if perm_label.is_empty() { reply } else { perm_label };

        // A fallback card is STANDALONE by construction (ADR-0102 / #1013): its
        // click ack must replace that card, never a streaming card that happens
        // to be live for the same session. The first-step allow buttons and the
        // confirmation carry `fallback`; the confirmation also carries
        // `risk_confirmed`.
        let fallback_click = value.get("fallback").and_then(|v| v.as_bool()).unwrap_or(false);
        let risk_confirmed = value
            .get("risk_confirmed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let always_supported = value
            .get("always_supported")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let fallback_loss = FallbackLoss::from_key(value.get("loss").and_then(|v| v.as_str()));
        let summary_click = value.get("summary").and_then(|v| v.as_bool()).unwrap_or(false);
        // The risk path is the fallback confirmation only: `risk_confirmed`
        // without the fallback marker is a forged / malformed callback and
        // must take the ordinary proof.
        let risk_surface = risk_confirmed && fallback_click;
        // The digest rides the callback as lowercase hex: `value` is only
        // guaranteed to be a JSON object, and a u64 above 2^53 does not survive
        // a double/string round-trip. A missing or malformed digest is a
        // mismatch, never a panic.
        let risk_digest = value
            .get("target_digest")
            .and_then(|v| v.as_str())
            .and_then(|hex| u64::from_str_radix(hex, 16).ok());
        // Only approvals need the surface proof. Deny and Auto-Accept never
        // approve unseen text, so they must never be diverted into a rebuilt
        // fallback card by a changed target.
        let approval = matches!(reply, "once" | "always");
        // A fallback click always acks its own standalone card, never a live
        // streaming card (all fallback controls carry `fallback`).
        let inline = inline && !fallback_click;

        // The fallback card's allow buttons are FIRST-STEP only: they show a
        // confirmation card and must not reply, claim, or touch any block.
        if reply == "confirm" {
            let decision = match value.get("decision").and_then(|v| v.as_str()) {
                Some("always") => "always",
                _ => "once",
            };
            // A card from an older build carries a numeric digest (or none):
            // re-read the request and rebuild an actionable fallback card
            // instead of ending on a buttonless ack.
            if risk_digest.is_none() {
                return Some(
                    rebuild_fallback_from_server(
                        flow,
                        handles,
                        req_id,
                        directory,
                        session_id,
                        host,
                        fallback_loss,
                        inline,
                    )
                    .await,
                );
            }
            let confirmation = async {
                let dir = directory?;
                fallback_confirmation_card(
                    flow,
                    req_id,
                    session_id,
                    dir,
                    decision,
                    always_supported,
                    risk_digest,
                    fallback_loss,
                )
                .await
            }
            .await;
            let Some(card) = confirmation else {
                return Some(
                    rebuild_fallback_from_server(
                        flow,
                        handles,
                        req_id,
                        directory,
                        session_id,
                        host,
                        fallback_loss,
                        inline,
                    )
                    .await,
                );
            };
            return Some(CardActionResult {
                card: Some(card),
                toast: Some("请再次确认".to_string()),
            });
        }

        // 取消 on the confirmation card returns to the fallback card. Like the
        // first step it replies nothing and claims nothing.
        if reply == "cancel_confirm" {
            if risk_digest.is_none() {
                return Some(
                    rebuild_fallback_from_server(
                        flow,
                        handles,
                        req_id,
                        directory,
                        session_id,
                        host,
                        fallback_loss,
                        inline,
                    )
                    .await,
                );
            }
            let summary = flow
                .sent_cards
                .lock()
                .await
                .get(req_id)
                .map(|c| c.summary.clone());
            let Some(summary) = summary.filter(|s| !s.is_empty()) else {
                return Some(
                    rebuild_fallback_from_server(
                        flow,
                        handles,
                        req_id,
                        directory,
                        session_id,
                        host,
                        fallback_loss,
                        inline,
                    )
                    .await,
                );
            };
            let card = crate::feishu::card::question::build_fallback_permission_card(
                session_id,
                req_id,
                directory.unwrap_or(""),
                &summary,
                always_supported,
                risk_digest.unwrap_or_default(),
                fallback_loss,
            );
            return Some(CardActionResult {
                card: Some(card),
                toast: Some("已取消".to_string()),
            });
        }

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
                let Some(mut approved) = set_auto_accept(
                    &handles.sessions,
                    &handles.requests,
                    &handles.backend,
                    session_id,
                    &dir,
                    true,
                )
                .await
                else {
                    // The flag did not persist, so nothing was approved and
                    // the mode did not change. Release the click's claim and
                    // leave the CARD ALONE: replacing a standalone permission
                    // card with a buttonless result would strand a still-pending
                    // request (the sweep's `seen` set will not re-surface it),
                    // and the user must be able to retry either decision.
                    flow.unmark_answered(&handles.requests, req_id).await;
                    return Some(CardActionResult {
                        card: None,
                        toast: Some("自动授权状态保存失败，请重试。".to_string()),
                    });
                };
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
            let mut r = result_card(
                "✅ 已开启自动授权",
                "blue",
                "该会话后续权限请求将自动批准；卡片无法完整显示的目标也会批准（见日志）。",
            );
            r.toast = Some("已开启自动授权".to_string());
            settle_ack(&handles.cards, host, session_id, inline, cached, &mut r).await;
            // Re-served verbatim to a losing double-click.
            flow.remember_answered_result(req_id, &r).await;
            return Some(r);
        }

        // The completeness guard again, at the decision point (whole-branch
        // re-review #883): an Allow click must never approve a target the card
        // could not show in full. Since ADR-0102 the proof is a DELIVERY fact:
        // an over-budget approval is allowed only when the full target's
        // companion record exists for this request and its digest matches the
        // request the server still holds. The server's pending list is the
        // authority — the payload only describes what the card displayed — so
        // the click reads the request and refuses when the companion is missing
        // or stale. That same read rebuilds the result body: the callback no
        // longer carries it.
        let mut proof = ClickProof::None;
        if approval && let Some(dir) = directory {
            // A failed verification read must not replace ANY surface with a
            // buttonless result — inline block or standalone card alike: the
            // request stays `seen`, so nothing would resurface it. Ack with no
            // card and a retry toast, leaving the controls in place.
            let keep_surface_on_unverified = true;
            let listed = crate::bridge::bounded_call(
                "permission click: list pending",
                flow.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
                handles.backend.clone().for_directory(dir).list_permissions(),
            )
            .await;
            match listed {
                Some(Ok(perms)) => {
                    if let Some(p) = perms.iter().find(|p| p.request_id == req_id) {
                        let expected = patterns_target_digest(p);
                        if risk_surface || summary_click {
                            // A surface that carried a digest must still match
                            // the re-read request — regardless of whether the
                            // request is over budget NOW. Otherwise a shrunk
                            // target would borrow the surface's approval.
                            if risk_digest != Some(expected) {
                                let loss = if risk_surface {
                                    fallback_loss
                                } else {
                                    // A summary card's companion was confirmed
                                    // for the previous target; do not claim its
                                    // send was unknown.
                                    FallbackLoss::PreviousDelivered
                                };
                                return Some(
                                    recover_from_risk_mismatch(
                                        flow,
                                        handles,
                                        p,
                                        dir,
                                        session_id,
                                        host,
                                        req_id,
                                        loss,
                                        risk_digest.is_some(),
                                        inline,
                                    )
                                    .await,
                                );
                            }
                            proof = if risk_surface {
                                ClickProof::RiskConfirmed { request: p.clone() }
                            } else {
                                // A summary card's plain allow still needs the
                                // companion delivery proof for this text.
                                companion_proof(flow, req_id, p)
                            };
                        } else if permission_overflow(p).is_none() {
                            // A legacy summary card (no button digest) whose
                            // request now fits: a companion record still proves
                            // the card was a summary, so its delivery must
                            // match. A stale one offers the fallback card
                            // rather than approving unseen text.
                            if flow.surfaces.companion(req_id).is_some() {
                                match flow
                                    .surfaces
                                    .delivered_companion(req_id, companion_target_digest(p))
                                {
                                    Some(delivery) => {
                                        proof = ClickProof::Confirmed {
                                            request: p.clone(),
                                            delivery: Some(delivery),
                                        }
                                    }
                                    None => {
                                        return Some(
                                            recover_from_risk_mismatch(
                                                flow,
                                                handles,
                                                p,
                                                dir,
                                                session_id,
                                                host,
                                                req_id,
                                                FallbackLoss::PreviousDelivered,
                                                true,
                                                inline,
                                            )
                                            .await,
                                        );
                                    }
                                }
                            } else {
                                proof = ClickProof::Confirmed {
                                    request: p.clone(),
                                    delivery: None,
                                };
                            }
                        } else {
                            proof = companion_proof(flow, req_id, p);
                        }
                    }
                }
                // The target is unknown, so the approval is not verifiable:
                // refuse to decide this time and let the click be retried.
                Some(Err(e)) => {
                    tracing::warn!("permission {req_id}: could not verify its target: {e}");
                    return Some(failed_result_card(
                        keep_surface_on_unverified,
                        "无法校验该权限请求的目标，未批准；请重试。",
                        "无法校验目标，未批准",
                    ));
                }
                None => {
                    tracing::warn!("permission {req_id}: verifying its target timed out; not approved");
                    return Some(failed_result_card(
                        keep_surface_on_unverified,
                        "无法校验该权限请求的目标，未批准；请重试。",
                        "无法校验目标，未批准",
                    ));
                }
            }
        }
        // A blocked proof is always declined, whatever the button asked for:
        // the click sought an approval the user could not have reviewed.
        let reply = if proof.is_blocked() { "reject" } else { reply };

        // Double-click guard: the atomic claim decides who replies. A click
        // that loses the race re-serves the winning click's result; only the
        // winner may reach the backend.
        if !flow.try_mark_answered(&handles.requests, req_id).await {
            return flow.answered_result(req_id).await;
        }
        // A rejection runs no verification read (it approves nothing), so the
        // result body names what the click showed from what cola already
        // recorded: the standalone card's own summary when the click landed on
        // it, else the inline block's one-line target. Read before
        // `resolve_blocks` forgets either, and only when this click has no
        // verified request to describe — an allow never needs them.
        let clicked_body = if matches!(proof, ClickProof::None) {
            let block_target = handles
                .cards
                .card_handles
                .lock()
                .await
                .target_of(req_id)
                .map(str::to_string);
            let standalone = flow.sent_cards.lock().await.get(req_id).cloned();
            let standalone_clicked = match (clicked, standalone.as_ref()) {
                (Some(message_id), Some(sent)) => sent.message_id == message_id,
                // No clicked-card id: a session with no live card means the
                // click landed on the standalone surface.
                (None, Some(_)) => !inline,
                _ => false,
            };
            Some(unverified_result_body(
                display_label,
                standalone_clicked,
                standalone.as_ref().map(|sent| sent.summary.as_str()),
                block_target.as_deref(),
            ))
        } else {
            None
        };
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
                    .reply_permission(session_id, req_id, reply)
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
                if let ClickProof::RiskConfirmed { request } = &proof {
                    // The one auditable record of an approval the card could
                    // not fully show (ADR-0102 / #1013): WARN, with the request
                    // id, the exact target length and the fact that the user
                    // confirmed unseen text.
                    tracing::warn!(
                        "risk-confirmed permission approval: request={} session={} decision={} \
                         target_len={} — the user confirmed unseen text; the card did not show the \
                         complete target",
                        request.request_id,
                        session_id,
                        reply,
                        request.total_target_len()
                    );
                }
                // ADR-0038 rules 3+4: the clicked block becomes its
                // Interaction Receipt (rendered from the accumulator, so it
                // survives later flushes) and the ack below carries the
                // clicked card's updated JSON — no PATCH race, no dependence
                // on which card is current.
                let missing_receipt =
                    |_target: &str| crate::feishu::card::question::MISSING_PERMISSION_RECEIPT.to_string();
                let uncertain_receipt =
                    |_target: &str| crate::feishu::card::question::UNCERTAIN_PERMISSION_RECEIPT.to_string();
                let normal_receipt = |target: &str| permission_receipt(target, reply);
                let residue = match proof {
                    ClickProof::Uncertain => Residue::PerBlock(&uncertain_receipt),
                    ClickProof::Missing => Residue::PerBlock(&missing_receipt),
                    // A fallback decision has no inline block (the surface is
                    // withdrawn before the standalone card is posted), so its
                    // result card is the decision record (ADR-0102).
                    ClickProof::RiskConfirmed { .. } | ClickProof::Confirmed { .. } | ClickProof::None => {
                        Residue::PerBlock(&normal_receipt)
                    }
                };
                resolve_blocks(
                    flow,
                    &handles.cards,
                    &handles.requests,
                    host,
                    session_id,
                    Origin::Click { clicked },
                    &[req_id.to_string()],
                    residue,
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
        // Result card: shows the decision, no buttons. A forced refusal says
        // why — it is not the decision the button label claimed.
        let (label, toast, body, color) = match &proof {
            ClickProof::Confirmed { request, delivery } => (
                display_label,
                toast_for(reply),
                // The approval re-read the request from the server; the
                // result card describes exactly that request, never the
                // callback's stale copy.
                describe_permission_for(request, delivery.as_ref()),
                perm_color,
            ),
            ClickProof::Uncertain => (
                crate::feishu::card::question::UNCERTAIN_PERMISSION_CLICK_LABEL,
                "已拒绝：完整内容送达结果未知".to_string(),
                crate::feishu::card::question::uncertain_permission_note().to_string(),
                "red",
            ),
            ClickProof::Missing => (
                crate::feishu::card::question::MISSING_PERMISSION_CLICK_LABEL,
                "已拒绝：没有匹配的完整内容".to_string(),
                crate::feishu::card::question::missing_permission_click_note(),
                "red",
            ),
            ClickProof::RiskConfirmed { request } => (
                display_label,
                toast_for(reply),
                // The whole fallback summary — target summary, content claim
                // and risk paragraph — so the result names what was not fully
                // shown and why.
                crate::feishu::card::question::fallback_permission_summary(
                    &describe_permission_target_summary(request),
                    fallback_loss,
                    false,
                ),
                perm_color,
            ),
            ClickProof::None => (
                display_label,
                toast_for(reply),
                clicked_body.unwrap_or_else(|| display_label.to_string()),
                perm_color,
            ),
        };
        let mut r = result_card(label, color, &body);
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
/// What the parent-chain read says about a session's Auto-Accept flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoAcceptFlag {
    /// The session, or an ancestor, has it on.
    On,
    /// The chain was walked to its end and nothing mapped has it on.
    Off,
    /// A hop failed transiently: retrying can succeed.
    Unknown,
    /// The chain is deeper than the walk cap, so its end was never reached.
    /// Retrying cannot fix a structural bound; the surface decides instead.
    Capped,
}

/// `On` / `Off` for a resolved flag, `Unknown` when a hop failed (callers must
/// retry an unknown instead of reading it as off — declining on a guess is the
/// irreversible outcome #892 is about), and `Capped` when a too-deep chain left
/// the answer unreachable (a retry loop would hang the turn forever).
async fn should_auto_accept(
    flow: &RequestFlow,
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    directory: &str,
) -> AutoAcceptFlag {
    // The walk calls `session_info`, which has no timeout of its own; it runs in
    // the sweep before any surface exists, so a half-open server would freeze
    // permission surfacing for every session. Bound it like the companion and
    // host walks; a timeout is UNKNOWN, not "off" (ADR-0048/ADR-0102).
    let walk_timeout_ms = flow.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
    let found = crate::bridge::bounded_call_quiet(walk_timeout_ms, async {
        Ok::<_, crate::error::BridgeError>(
            crate::bridge::pollers::walk_parent_chain_result(
                backend,
                session_id,
                Some(directory),
                |current| {
                    let current = current.to_string();
                    async move {
                        sessions
                            .store
                            .lock()
                            .await
                            .entry_for_session(&current)
                            .map(|e| e.auto_accept)
                    }
                },
            )
            .await,
        )
    })
    .await;
    let Some(Ok(found)) = found else {
        // ADR-0048's latch: WARN once, DEBUG identical repeats, INFO on
        // recovery. A half-open server must not log a WARN every poll tick.
        flow.note_autoaccept_read_failure(
            session_id,
            format!("parent walk timed out after {walk_timeout_ms}ms"),
        )
        .await;
        return AutoAcceptFlag::Unknown;
    };
    match found {
        Ok(Some(true)) => {
            flow.note_autoaccept_read_success(session_id).await;
            AutoAcceptFlag::On
        }
        Ok(_) => {
            flow.note_autoaccept_read_success(session_id).await;
            AutoAcceptFlag::Off
        }
        Err(crate::bridge::pollers::WalkFailure::Transient(e)) => {
            // ADR-0048: the sweep retries this read every tick, so the latch
            // WARNs the first failure and DEBUGs identical repeats. The request
            // is never declined on the unknown.
            flow.note_autoaccept_read_failure(session_id, &e).await;
            AutoAcceptFlag::Unknown
        }
        Err(crate::bridge::pollers::WalkFailure::Capped) => {
            // Rare and permanent: log once per sweep at DEBUG and let the
            // ordinary surface rule decide rather than wait forever.
            tracing::debug!(
                "auto-accept: {session_id}'s parent chain exceeds the walk cap; \
                 deciding on the surface instead"
            );
            AutoAcceptFlag::Capped
        }
    }
}

/// Turn a session's Auto-Accept flag on/off, resolving the owning session
/// (which may be a parent of a sub-task child) and approving any already-pending
/// permissions when turning on. Mirrors `/autoaccept` and is shared by the
/// permission-card toggle and the coordinator's wrapper so both paths stay in
/// lockstep. Returns `None` when the flag could not be persisted (nothing was
/// approved: the mode did not change), else the ids of the pending requests that
/// were approved (empty when `on` is false), so the caller can drop their inline
/// card sections.
pub(crate) async fn set_auto_accept(
    sessions: &SessionsHandle,
    requests: &RequestsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    directory: &str,
    on: bool,
) -> Option<Vec<String>> {
    // Persist the flag BEFORE approving the pending requests: a sweep landing
    // in the approval window must already read the session as auto-accept, or
    // its `prepare` surfaces an over-budget request this very call is about to
    // approve (#892). A persist that did not take effect — a store error, or
    // the mapped entry vanishing between the walk and the write — approves
    // nothing: the mode never changed.
    //
    // Resolve the SessionStore entry that owns the flag: `session_id`
    // itself, or its nearest ancestor (sub-task children are not in the
    // store, ADR-0010). Walking the chain makes a child's card flip the
    // parent's flag, consistent with `should_auto_accept`.
    let owner = crate::bridge::pollers::walk_parent_chain(backend, session_id, Some(directory), |current| {
        let current = current.to_string();
        async move { sessions.entry_for_session(&current).await }
    })
    .await;
    let persisted = match owner {
        Some(entry) => match sessions.update(&entry.session_id, |e| e.auto_accept = on).await {
            // `Ok(None)` = the entry vanished between the walk and the write
            // (a concurrent forget/unmap). Nothing was persisted.
            Ok(Some(_)) => true,
            Ok(None) => {
                tracing::warn!(
                    "set_auto_accept: session {} disappeared before the flag was persisted",
                    entry.session_id
                );
                false
            }
            Err(e) => {
                tracing::warn!("set_auto_accept: persist failed: {}", e);
                false
            }
        },
        None => {
            tracing::warn!("set_auto_accept: no store entry to persist the flag on");
            false
        }
    };
    // Approving with the flag unpersisted would apply a mode the store does not
    // record: the next sweep reads it as off, and an over-budget request would
    // be approved unseen under a session that never opted in (#892).
    if !persisted {
        return None;
    }
    Some(if on {
        approve_pending_for_session(sessions, requests, backend, session_id, directory).await
    } else {
        Vec::new()
    })
}

/// Settle a permission on the server with `reject` — the ONE refusal skeleton
/// for both an over-budget bound and an uncertain companion outcome.
///
/// The turn is blocked on the reply, so it must land first; whether the user
/// gets the reason card or their persisted card is withdrawn is the flow's
/// decision (`withdraw_stored_surface`). **A failed reply is not a refusal**:
/// it returns [`Refusal::Failed`] so the flow keeps the request retryable
/// instead of reporting it settled. A request the server no longer lists
/// (another sweep or client settled it first) is already refused.
pub(crate) async fn decline_permission(
    handles: &FlowHandles,
    p: &crate::backend::types::PermissionRequest,
    directory: &str,
    reason: DeclineReason,
) -> Refusal {
    let session_id = p.session_id.clone().unwrap_or_default();
    match handles
        .backend
        .clone()
        .for_directory(directory)
        .reply_permission(&session_id, &p.request_id, "reject")
        .await
    {
        Ok(()) => {
            match reason {
                DeclineReason::Overflow(overflow) => tracing::warn!(
                    "declined permission {} on session {session_id}: its targets total {} \
                     characters (rendered content is {}), past the card's {} character budget",
                    p.request_id,
                    p.total_target_len(),
                    rendered_permission_chars(p),
                    overflow.budget()
                ),
                DeclineReason::CompanionUncertain => tracing::warn!(
                    "declined permission {} on session {session_id}: its companion delivery \
                     outcome is unknown; no interactive approval is offered",
                    p.request_id
                ),
            }
            Refusal::Refused(reason)
        }
        // Another client (or an earlier sweep) settled it before us: there is
        // nothing left to refuse, and nothing cola may claim it rejected.
        Err(crate::error::BridgeError::NotFound(_)) => Refusal::ResolvedElsewhere,
        Err(e) => {
            tracing::warn!(
                "declining permission {} on session {session_id} failed: {e}; it will be \
                 retried on the next sweep and is never offered in the meantime",
                p.request_id
            );
            Refusal::Failed
        }
    }
}

/// After `/autoaccept on`: answer every permission request that is ALREADY
/// pending for `session_id` (or one of its sub-task child sessions) with
/// "once". The permission poller's `seen` set skips requests it has already
/// surfaced, so enabling autoaccept would otherwise leave old cards hanging
/// forever. Returns the ids of the requests that were approved.
pub(crate) async fn approve_pending_for_session(
    sessions: &SessionsHandle,
    requests: &RequestsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    directory: &str,
) -> Vec<String> {
    // ADR-0048: `/autoaccept on` runs under the inbound-message span (chat and
    // topic only), so the unseen-approval audit line must take its `session=`
    // from an own span or `rg 'session=ses_x'` cannot retrieve it.
    let thread_key = sessions
        .store
        .lock()
        .await
        .entry_for_session(session_id)
        .map(|entry| entry.thread_key.clone());
    approve_pending_inner(sessions, requests, backend, session_id, directory)
        .instrument(crate::bridge::span::request(session_id, thread_key.as_ref()))
        .await
}

async fn approve_pending_inner(
    sessions: &SessionsHandle,
    requests: &RequestsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    directory: &str,
) -> Vec<String> {
    let perms = match backend.clone().for_directory(directory).list_permissions().await {
        Ok(perms) => perms,
        Err(e) => {
            // The read's body diagnostic rides at DEBUG (its poll caller owns
            // the WARN); this one-shot approval path must not drop the failure
            // silently.
            tracing::warn!("auto-accept: listing pending permissions failed: {e}");
            return Vec::new();
        }
    };
    let mut approved = Vec::new();
    for p in &perms {
        // Match the session itself or a sub-task child (its parent chain).
        let sid = p.session_id.clone().unwrap_or_default();
        if !session_belongs_to(sessions, backend, &sid, session_id, directory).await {
            continue;
        }
        // Auto-Accept returns before the companion step, so the completeness
        // proof must not reappear here: a target past the card budget is
        // approved unseen (this path renders no card either), and
        // `log_unshowable_auto_accept` records the length (#892).
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
            .reply_permission(&sid, &p.request_id, "once")
            .await
        {
            Ok(()) => {
                tracing::info!(
                    "Auto-accepted pending permission {} on session {} ({})",
                    p.request_id,
                    sid,
                    p.permission.as_deref().unwrap_or("?")
                );
                log_unshowable_auto_accept(p);
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

/// How much of an unseen target's first line the Auto-Accept audit record
/// carries. The whole target is exactly what did not fit, so the line names its
/// length and a bounded prefix.
const AUTO_ACCEPT_TARGET_LOG_CHARS: usize = 120;

/// The record an Auto-Accept approval leaves when its card could not show it in
/// full. Nothing was shown, so this line is the only trace of what the session
/// approved unseen (#892): it names the request, the rendered length that made
/// it unshowable (and the summed target length), and the target's first line.
/// INFO, like every other auto-accepted transition (ADR-0048); the "unseen"
/// wording is what sets it apart from an ordinary auto-accept.
fn log_unshowable_auto_accept(p: &crate::backend::types::PermissionRequest) {
    let Some(overflow) = permission_overflow(p) else {
        return;
    };
    let budget = overflow.budget();
    let first_line = p
        .longest_target()
        .map(|target| target.lines().next().unwrap_or(""))
        .unwrap_or("");
    tracing::info!(
        "auto-accept approved permission {} on session {} unseen: its rendered content is {} \
         characters (targets total {}), past the card's {} character budget; first line: {}",
        p.request_id,
        p.session_id.as_deref().unwrap_or(""),
        rendered_permission_chars(p),
        p.total_target_len(),
        budget,
        truncate(first_line, AUTO_ACCEPT_TARGET_LOG_CHARS)
    );
}

/// Receipt for the Auto-Accept toggle: it names the MODE change, never the
/// requests it resolved — naming a command made the line read as "only this
/// command is now auto-approved". One per toggle, however many pending blocks
/// it swallowed (the blocks are dismissed without lines of their own). Shared
/// by the card button and the `/autoaccept on` command. It names the trade the
/// mode accepts: a target the card cannot show in full is approved too, unseen
/// (#892).
pub(crate) const AUTOACCEPT_RECEIPT: &str =
    "🔄 已开启自动授权：后续权限请求将自动批准（卡片无法完整显示的也会批准）";

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

/// Build the second step's confirmation card from the summary the fallback
/// card already showed. The summary is the standalone record's, so the two
/// steps cannot disagree about the target. Returns `None` when no record backs
/// the click (a stale card from before the request was surfaced).
#[allow(clippy::too_many_arguments)] // the routing triple, decision and copy context
async fn fallback_confirmation_card(
    flow: &RequestFlow,
    req_id: &str,
    session_id: &str,
    directory: &str,
    decision: &str,
    always_supported: bool,
    target_digest: Option<u64>,
    loss: FallbackLoss,
) -> Option<serde_json::Value> {
    let summary = flow.sent_cards.lock().await.get(req_id)?.summary.clone();
    if summary.is_empty() {
        return None;
    }
    Some(
        crate::feishu::card::question::build_fallback_permission_confirm_card(
            session_id,
            req_id,
            directory,
            &summary,
            target_digest?,
            decision,
            always_supported,
            loss,
        ),
    )
}

/// Re-read a request whose surface digest did not parse (a card from an older
/// build, or a malformed callback) and rebuild an actionable fallback card with
/// a fresh digest. This keeps a legacy card from ending on a buttonless ack.
#[allow(clippy::too_many_arguments)]
async fn rebuild_fallback_from_server(
    flow: &RequestFlow,
    handles: &FlowHandles,
    req_id: &str,
    directory: Option<&str>,
    session_id: &str,
    host: &Option<String>,
    loss: FallbackLoss,
    inline: bool,
) -> CardActionResult {
    // A failed read must not replace the only surface with a buttonless
    // result: keep the controls and let the toast ask for a retry.
    let Some(dir) = directory else {
        return failed_result_card(
            true,
            "无法校验该权限请求的目标，未批准；请重试。",
            "无法校验目标，未批准",
        );
    };
    let listed = crate::bridge::bounded_call(
        "permission fallback: list pending",
        flow.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
        handles.backend.clone().for_directory(dir).list_permissions(),
    )
    .await;
    match listed {
        Some(Ok(perms)) => match perms.iter().find(|p| p.request_id == req_id) {
            Some(p) => {
                recover_from_risk_mismatch(
                    flow, handles, p, dir, session_id, host, req_id, loss, false, inline,
                )
                .await
            }
            None => already_handled_result("权限", inline, "该权限已处理"),
        },
        Some(Err(e)) => {
            tracing::warn!("permission {req_id}: could not verify its target: {e}");
            failed_result_card(
                true,
                "无法校验该权限请求的目标，未批准；请重试。",
                "无法校验目标，未批准",
            )
        }
        None => {
            tracing::warn!("permission {req_id}: verifying its target timed out; not approved");
            failed_result_card(
                true,
                "无法校验该权限请求的目标，未批准；请重试。",
                "无法校验目标，未批准",
            )
        }
    }
}

/// Answer a risk confirmation (or a summary allow) whose surface proof no
/// longer matches the re-read request. The ack replaces the clicked card with a
/// FRESH fallback card — all four controls, the new target's summary and digest,
/// and (when the target changed) a note saying so. Nothing is replied or
/// claimed, so the request stays actionable and the user can confirm again
/// (ADR-0102 / #1013).
#[allow(clippy::too_many_arguments)] // the click context is the helper's whole point
async fn recover_from_risk_mismatch(
    flow: &RequestFlow,
    handles: &FlowHandles,
    p: &crate::backend::types::PermissionRequest,
    dir: &str,
    session_id: &str,
    host: &Option<String>,
    req_id: &str,
    loss: FallbackLoss,
    changed: bool,
    inline: bool,
) -> CardActionResult {
    let target_summary = describe_permission_target_summary(p);
    // The changed-target note is ephemeral to the card that says it: the
    // stored summary (which seeds the next confirmation) stays clean.
    let stored_summary =
        crate::feishu::card::question::fallback_permission_summary(&target_summary, loss, false);
    let card_summary =
        crate::feishu::card::question::fallback_permission_summary(&target_summary, loss, changed);
    let digest = patterns_target_digest(p);
    // Refresh the standalone record once. `set_standalone` stamps the owning
    // client scope, so the empty placeholder here is not persisted as-is.
    let sent = {
        let mut cards = flow.sent_cards.lock().await;
        cards.get_mut(req_id).map(|entry| {
            entry.summary = stored_summary.clone();
            entry.clone()
        })
    };
    let had_sent = sent.is_some();
    if let Some(sent) = sent {
        flow.surfaces.set_standalone(
            req_id,
            crate::bridge::surfaces::StandaloneSurface {
                client_id: String::new(),
                kind: InteractionKind::Permission,
                message_id: sent.message_id,
                summary: stored_summary,
                directory: sent.directory,
                session_id: sent.session_id,
            },
        );
    }
    tracing::warn!(
        "permission {req_id}: the surface's target proof no longer matches the pending request; \
         rebuilding the fallback card without approving or rejecting"
    );
    let toast = if changed {
        "目标已变化，请查看新的确认卡片".to_string()
    } else {
        "请查看新的确认卡片".to_string()
    };
    if inline {
        // The old controls live on a streaming card: post a NEW standalone card
        // first (the ack must not replace the streaming card), then withdraw the
        // old block. A failed post leaves the old block live and retryable.
        if !flow.post_fallback_card(handles, p, dir, loss, changed).await {
            return failed_result_card(true, "无法发送新的权限卡片，请重试。", "无法发送新的权限卡片");
        }
        let req = PendingRequest::Permission(p.clone());
        if !flow
            .withdraw_block_for_fallback(handles, &req, host.as_deref())
            .await
        {
            // The new card is the actionable surface; the old block may still
            // be clickable, which the next sweep/click will recover again.
            tracing::warn!(
                "permission {req_id}: the replacement fallback card landed but its old block \
                 could not be withdrawn"
            );
        }
        return CardActionResult {
            // The ack leaves the streaming card in place (its block now carries
            // the move receipt when the withdrawal landed); the new fallback
            // card is its own message.
            card: None,
            toast: Some(toast),
        };
    }
    if !had_sent {
        // A rebuilt fallback with no standalone record would otherwise loop:
        // its next allow has no summary to seed the confirmation. Post a fresh
        // card (and its record) instead of acking an unusable one.
        if flow.post_fallback_card(handles, p, dir, loss, changed).await {
            return CardActionResult {
                card: None,
                toast: Some(toast),
            };
        }
        return failed_result_card(true, "无法发送新的权限卡片，请重试。", "无法发送新的权限卡片");
    }
    CardActionResult {
        card: Some(crate::feishu::card::question::build_fallback_permission_card(
            session_id,
            req_id,
            dir,
            &card_summary,
            p.always_supported,
            digest,
            loss,
        )),
        toast: Some(toast),
    }
}

/// The Interaction Receipt for a resolved question:
/// `✅ 已回答：Which color should I use? Red；Which size? L` — each question's
/// own text (not its short header) with the answers chosen for it; a skipped
/// slot reads 未作答. `answers[i]` is the finalized selection for field `i`
/// (`None` = never answered); option values render as their display labels.
///
/// The text is clipped per question so several fit the one-line residue, but it
/// still says what was asked: a Codex question's header is a 12-character label
/// (`Color`), which alone would leave the reader unable to tell what the card
/// had just answered.
fn question_receipt(
    questions: &[crate::backend::types::QuestionInfo],
    answers: &[Option<Vec<String>>],
) -> String {
    let parts: Vec<String> = questions
        .iter()
        .enumerate()
        .map(|(i, qi)| {
            let label = crate::feishu::card::question::question_label(qi, 40);
            match answers
                .get(i)
                .and_then(|slot| slot.as_ref())
                .filter(|a| !a.is_empty())
            {
                Some(values) => format!("{label} {}", qi.display_values(values).join("、")),
                None => format!("{label} （未作答）"),
            }
        })
        .collect();
    if parts.is_empty() {
        return "✅ 已回答".to_string();
    }
    truncate(&format!("✅ 已回答：{}", parts.join("；")), RECEIPT_MAX_CHARS)
}

/// Compact one-line target for a permission's receipt: the action plus the
/// first pattern (or the edited file for edit/patch) — what the decision was
/// about, for when the position alone cannot say (a sub-task child's block has
/// no command panel on the parent card; a toggle resolves several at once).
/// Backticks in a pattern are flattened so the markdown element cannot be
/// broken by server-provided content.
pub(crate) fn permission_target(p: &crate::backend::types::PermissionRequest) -> String {
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

/// The block's receipt target for an over-budget request: the action and first
/// target (its 60-character line), the summed length and the companion lane,
/// clipped to the one-line receipt budget. The card uses the longer summary;
/// the receipt carries this shorter budget-clipped form (ADR-0102).
pub(crate) fn permission_receipt_target(
    p: &crate::backend::types::PermissionRequest,
    companion: Option<&CompanionDelivery>,
) -> String {
    match companion {
        Some(delivery) => truncate(
            &format!(
                "{} · {} 字符 · {}",
                permission_target(p),
                p.total_target_len(),
                delivery.lane.short_label()
            ),
            RECEIPT_MAX_CHARS,
        ),
        None => permission_target(p),
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
        backend: &Arc<dyn crate::backend::DirectoryBackend>,
    ) -> crate::error::Result<Vec<PendingRequest>> {
        backend
            .list_questions()
            .await
            .map(|v| v.into_iter().map(PendingRequest::Question).collect())
    }

    async fn reject(
        &self,
        flow: &RequestFlow,
        backend: &Arc<dyn crate::backend::DirectoryBackend>,
        req: &PendingRequest,
    ) -> crate::error::Result<()> {
        let PendingRequest::Question(q) = req else {
            return Ok(());
        };
        backend.reject_question(&q.session_id, &q.id).await?;
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
    ) -> Prepared {
        if let PendingRequest::Question(q) = req {
            flow.remember_question(q, dir).await;
        }
        Prepared::NotHandled
    }

    async fn remember_surfaced(&self, flow: &RequestFlow, req: &PendingRequest, dir: &str) {
        if let PendingRequest::Question(q) = req {
            flow.remember_question(q, dir).await;
        }
    }

    async fn refresh_cached_block(
        &self,
        flow: &RequestFlow,
        cards: &CardsHandle,
        message_id: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> Option<serde_json::Value> {
        let PendingRequest::Question(q) = req else {
            return None;
        };
        // Re-render the block from the state this process holds (empty after a
        // restart): the persisted card may show 已选 markers whose toggles did
        // not survive, and a card that lies about a selection is worse than an
        // un-answered one.
        let paint = flow
            .with_question_state(&q.id, |state| state.paint())
            .await
            .unwrap_or_else(|| crate::bridge::question::QuestionPaint::blank(q.questions.len()));
        let elements = crate::feishu::card::question::question_elements(
            &q.title,
            q.uncollectable,
            &q.id,
            &q.session_id,
            &q.questions,
            dir,
            &paint.display,
            &paint.done,
            paint.open,
            &paint.drafts,
        );
        cards
            .card_handles
            .lock()
            .await
            .refresh_on(message_id, &q.id, elements)
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
        let paint = flow
            .with_question_state(&q.id, |state| state.paint())
            .await
            .unwrap_or_else(|| crate::bridge::question::QuestionPaint::blank(q.questions.len()));
        Turn::add_question(cards, host, q, dir, &paint).await
    }

    async fn add_inline_locked(
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
        let paint = flow
            .with_question_state(&q.id, |state| state.paint())
            .await
            .unwrap_or_else(|| crate::bridge::question::QuestionPaint::blank(q.questions.len()));
        Turn::add_question_locked(cards, host, q, dir, &paint).await
    }

    async fn add_initial_inline_locked(
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
        let paint = crate::bridge::question::QuestionPaint::blank(q.questions.len());
        Turn::add_question_locked(cards, host, q, dir, &paint).await
    }

    fn build_card(&self, _flow: &RequestFlow, req: &PendingRequest, dir: &str) -> serde_json::Value {
        let PendingRequest::Question(q) = req else {
            return serde_json::json!({});
        };
        if let Some(reason) = q.uncollectable {
            return crate::feishu::card::question::build_uncollectable_card(
                reason,
                &q.id,
                &q.session_id,
                dir,
            );
        }
        let paint = crate::bridge::question::QuestionPaint::blank(q.questions.len());
        crate::feishu::card::question::build_question_card(
            &q.title,
            &q.id,
            &q.session_id,
            &q.questions,
            dir,
            &paint.display,
            &paint.done,
            paint.open,
            &paint.drafts,
        )
    }

    fn summary(&self, _flow: &RequestFlow, req: &PendingRequest) -> String {
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
        dead: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        Turn::resolve_vanished_questions(cards, pending, failed_dirs, cola_claimed, dead).await
    }

    fn interaction_kind(&self) -> InteractionKind {
        InteractionKind::Question
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
                        let revision = state.revision();
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
                        let count = state.merge().0;
                        let mutated = state.revision() != revision;
                        (count, state.paint(), outcome, mutated)
                    })
                    .await;
                let Some((answered_count, paint, outcome, mutated)) = mutated else {
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
                    let Some(snapshot) = flow.question_snapshot(req_id).await else {
                        return Some(
                            flow.missing_after_claim(&handles.requests, req_id, directory, inline)
                                .await,
                        );
                    };
                    let answers =
                        crate::backend::types::build_form_answers(&snapshot.fields, &snapshot.answers);
                    if let Some(r) = settle_question_reply(
                        flow,
                        &handles.cards,
                        &handles.requests,
                        req_id,
                        self.label(),
                        reply_question_scoped(
                            &handles.backend,
                            session_id,
                            req_id,
                            Some(&answers),
                            directory,
                        )
                        .await,
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
                        Residue::PerBlock(&|_| question_receipt(&snapshot.fields, &snapshot.answers)),
                    )
                    .await;
                    let mut r = result_card(
                        "✅ 已回答",
                        "green",
                        &qa_completion_body(&snapshot.fields, &snapshot.answers),
                    );
                    // This click answered the last question: a claimed snapshot
                    // must repaint (or, when its liveness check is unknown, must
                    // not keep dead buttons).
                    r.toast = Some("已回答".to_string());
                    settle_ack(&handles.cards, host, session_id, inline, cached, &mut r).await;
                    // Re-served verbatim to a losing double-click.
                    flow.remember_answered_result(req_id, &r).await;
                    Some(r)
                } else if !mutated {
                    // A duplicate or stale click changed nothing, and a rebuild
                    // would recreate the open box from the server's draft. It
                    // must still answer — but only the settlement above needs
                    // to run for a click that DID change the state.
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(action_toast(reply, n - answered_count, outcome)),
                    });
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
                                flow,
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
                        &req.title,
                        req_id,
                        &req.session_id,
                        &req.questions,
                        directory.unwrap_or(""),
                        &paint.display,
                        &paint.done,
                        paint.open,
                        &paint.drafts,
                    );
                    let mut r = CardActionResult {
                        card: Some(card),
                        toast: None,
                    };
                    r.toast = Some(action_toast(reply, remaining, outcome));
                    Some(r)
                }
            }
            // A switch between questions (issue #917 part 3): the folded
            // question's 作答这道题 button is a form SUBMIT, so the text in the
            // open question's box arrives with it. Record every input the
            // submit carried as its question's draft, open the target, and
            // repaint — the new open question's box is seeded from that draft,
            // so switching never loses what the Host typed.
            "open" => {
                let target = value.get("question_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if flow.is_answered(&handles.requests, req_id).await {
                    return Some(
                        flow.answered_result(req_id)
                            .await
                            .unwrap_or_else(|| question_replay_card(inline)),
                    );
                }
                // #130: same gate as "answer" — a state-less click must not
                // repaint against a request cola can no longer see.
                if let Some(result) = flow.question_gate(req_id, directory, inline).await {
                    return Some(result);
                }
                let switched = flow
                    .with_question_state(req_id, |state| {
                        let drafts = draft_inputs(value);
                        // A switch rides the box it was submitted with, so it is
                        // honoured only while that box is still the open one: two
                        // taps racing on the pre-switch card (both carrying Q0's
                        // text) must not let the older one overwrite the newer
                        // choice once it lands last.
                        // Which box the switch rode: the button name carries it
                        // (a submit may deliver no `form_value` at all), and the
                        // payload's input name is the fallback for a card
                        // rendered before that segment existed.
                        let from = value
                            .get("from_index")
                            .and_then(|v| v.as_u64())
                            .map(|i| i as usize)
                            .or_else(|| drafts.first().map(|(index, _)| *index));
                        let stale = match from {
                            // The box it rides is no longer the open question.
                            Some(from) => state.open() != Some(from),
                            // No box at all: a boolean question's switch carries
                            // none. Fine unless the question open NOW renders a
                            // typed box — repainting would recreate it and erase
                            // what the Host is typing.
                            None => state.open().is_some_and(|open| {
                                state
                                    .request()
                                    .questions
                                    .get(open)
                                    .is_some_and(crate::feishu::card::question::renders_typed_input)
                            }),
                        };
                        if stale {
                            return SwitchApplied::Stale;
                        }
                        // A submit that carries a form payload WITHOUT the box
                        // it rode had that box emptied: record the clear, or the
                        // old draft would be seeded back. A callback with no
                        // `form_value` at all is a value-only client (ADR-0094),
                        // which never carried the text — clearing would erase a
                        // draft that client did not touch.
                        let carries_form = value.get("form_value").is_some();
                        if let Some(from) = from
                            && carries_form
                            && !drafts.iter().any(|(index, _)| *index == from)
                        {
                            state.record_draft(from, "");
                        }
                        for (index, text) in drafts {
                            state.record_draft(index, text);
                        }
                        if state.switch_open(target) {
                            SwitchApplied::Opened
                        } else {
                            SwitchApplied::Refused
                        }
                    })
                    .await;
                let Some(switched) = switched else {
                    return Some(flow.missing_question_result(directory, inline).await);
                };
                let Some(req) = flow.question_request(req_id).await else {
                    return Some(flow.missing_question_result(directory, inline).await);
                };
                // The paint is read AFTER the mutation, never carried across the
                // awaits: a concurrent answer that landed in between must not be
                // overwritten by this click's older view (ADR-0094).
                let paint = flow
                    .with_question_state(req_id, |state| state.paint())
                    .await
                    .unwrap_or_else(|| crate::bridge::question::QuestionPaint::blank(req.questions.len()));
                let toast = switch_toast(target, switched, &paint.done);
                // A dropped switch (stale, already answered, already open)
                // repaints only when nothing can be lost: with no typed box open
                // the repaint is what lets a card whose ack Feishu discarded
                // catch up. While a box IS open it must not — the rebuild would
                // seed it from the server's draft and erase what the client
                // alone holds.
                let typed_box_open =
                    crate::feishu::card::question::open_index(req.questions.len(), &paint.done, paint.open)
                        .and_then(|open| req.questions.get(open))
                        .is_some_and(crate::feishu::card::question::renders_typed_input);
                if matches!(switched, SwitchApplied::Refused | SwitchApplied::Stale) && typed_box_open {
                    return Some(CardActionResult {
                        // A refused switch may still have recorded its draft.
                        card: None,
                        toast: Some(toast),
                    });
                }
                if inline {
                    let card = refresh_question_block(
                        &handles.cards,
                        host,
                        session_id,
                        clicked,
                        req_id,
                        &req,
                        directory.unwrap_or(""),
                        flow,
                    )
                    .await;
                    return Some(CardActionResult {
                        card: match card {
                            Some(card) => Some(card),
                            None => ack_inline_card(&handles.cards, host, session_id).await,
                        },
                        toast: Some(toast),
                    });
                }
                Some(CardActionResult {
                    card: Some(crate::feishu::card::question::build_question_card(
                        &req.title,
                        req_id,
                        &req.session_id,
                        &req.questions,
                        directory.unwrap_or(""),
                        &paint.display,
                        &paint.done,
                        paint.open,
                        &paint.drafts,
                    )),
                    toast: Some(toast),
                })
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
                let Some(snapshot) = flow.question_snapshot(req_id).await else {
                    return Some(
                        flow.missing_after_claim(&handles.requests, req_id, directory, inline)
                            .await,
                    );
                };
                let answers = crate::backend::types::build_form_answers(&snapshot.fields, &snapshot.answers);
                if let Some(r) = settle_question_reply(
                    flow,
                    &handles.cards,
                    &handles.requests,
                    req_id,
                    self.label(),
                    reply_question_scoped(&handles.backend, session_id, req_id, Some(&answers), directory)
                        .await,
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
                    Residue::PerBlock(&|_| question_receipt(&snapshot.fields, &snapshot.answers)),
                )
                .await;
                let mut r = result_card(
                    "✅ 已回答",
                    "green",
                    &qa_completion_body(&snapshot.fields, &snapshot.answers),
                );
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
                    reply_question_scoped(&handles.backend, session_id, req_id, None, directory).await,
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

/// The typed drafts a switch submit carried. The card's one input is named
/// `custom_<question index>`, so the name itself says which question the text
/// belongs to — a stale card that was showing a different question open cannot
/// misfile the draft into the wrong slot.
fn draft_inputs(value: &serde_json::Value) -> Vec<(usize, &str)> {
    let Some(form) = value.get("form_value").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    form.iter()
        .filter_map(|(name, text)| {
            let index = name.strip_prefix("custom_")?.parse::<usize>().ok()?;
            Some((index, text.as_str()?))
        })
        .collect()
}

/// What a switch submit did to the request's state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SwitchApplied {
    /// The target is now the open question.
    Opened,
    /// The target is finalized, out of range, or was already open — only a stale
    /// card or a double tap can ask for those.
    Refused,
    /// The submit came from a box that is no longer the open question: an older
    /// tap landed after a newer one, so it is dropped and the newer choice
    /// stands.
    Stale,
}

/// Toast for a switch click: names the question now open, or says why nothing
/// moved.
fn switch_toast(target: usize, applied: SwitchApplied, done: &[bool]) -> String {
    match applied {
        SwitchApplied::Opened => format!("已切到第 {} 题", target + 1),
        SwitchApplied::Stale => "已按最新的选择更新".to_string(),
        // An index past the end can only come from a stale card: it names no
        // question, so it cannot be called answered.
        SwitchApplied::Refused if target >= done.len() => "这张卡片已过期".to_string(),
        SwitchApplied::Refused if done.get(target).copied().unwrap_or(false) => "这道题已经答完".to_string(),
        SwitchApplied::Refused => format!("已经停在第 {} 题", target + 1),
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
            flow.surfaces.remove_standalone(req_id);
            None
        }
        Err(e) if e.is_not_found() => {
            tracing::info!("Question already resolved: {}", e);
            flow.remove_question(req_id).await;
            flow.sent_cards.lock().await.remove(req_id);
            flow.surfaces.remove_standalone(req_id);
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

/// Route a question reply/cancel to the instance owning the session. The card
/// carries the owning directory (ADR-0010) and the form's session id (V2's
/// session-scoped reply routes); without the directory the reply can't be
/// routed, so surface the failure instead of guessing at the server cwd
/// instance.
async fn reply_question_scoped(
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    req_id: &str,
    answers: Option<&[crate::backend::types::FormAnswer]>,
    directory: Option<&str>,
) -> crate::error::Result<()> {
    let Some(dir) = directory else {
        return Err(crate::error::BridgeError::OpenCode(
            "question card carries no directory".into(),
        ));
    };
    let backend = backend.clone().for_directory(dir);
    match answers {
        Some(answers) => backend.reply_question(session_id, req_id, answers).await,
        None => backend.reject_question(session_id, req_id).await,
    }
}

/// Map an OpenCode permission action to a friendly emoji + Chinese description.
///
/// The action is the name of the tool asking, so the arms carry both
/// generations: V1's `bash` beside V2's `shell`, V1's `task` beside V2's
/// `subagent`, and the V2 tools whose permission assert names them directly
/// (`question`, `grep`, `glob`, `skill`, `websearch`). An action with no arm
/// still renders — as the generic `🔐 执行操作`.
pub(crate) fn describe_action(action: &str) -> (&'static str, &'static str) {
    match action {
        "bash" | "shell" | "command" => ("⚡", "执行 Shell 命令"),
        "permissions" => ("🔑", "申请额外权限"),
        "subagent" | "task" => ("🤖", "调用子代理"),
        "question" => ("❓", "向你提问"),
        "read" => ("📖", "读取文件"),
        "write" => ("✏️", "修改文件"),
        "edit" => ("✏️", "编辑文件"),
        "patch" => ("✏️", "应用补丁"),
        "webfetch" => ("🌐", "访问网页"),
        "fetch" => ("🌐", "获取网络资源"),
        "websearch" => ("🌐", "网络搜索"),
        "grep" => ("🔎", "搜索内容"),
        "glob" => ("🔍", "查找文件"),
        "skill" => ("🧩", "调用技能"),
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

/// How much of a permission's explanatory `reason` the card shows. The reason
/// is context, not the decision target, and it keeps its own short budget even
/// though the target budget is much larger — the spec's fixed 150 characters.
const PERMISSION_REASON_MAX_CHARS: usize = 150;

/// A friendly description of what a permission request asks to do, shown on the
/// permission card (and reused verbatim by its inline block, so the two look and
/// behave alike).
pub(crate) fn describe_permission(p: &crate::backend::types::PermissionRequest) -> String {
    let action = p.permission.as_deref().unwrap_or("?");
    let (emoji, label) = describe_action(action);
    let mut s = format!("{} **{}**\n", emoji, label);

    // A client-agnostic explanatory line: Codex's approval requests carry a
    // `reason` (a network rule, extra write access), and showing it is the
    // difference between "allow something" and knowing what is being allowed.
    if let Some(reason) = p
        .metadata
        .as_ref()
        .and_then(|m| m.get("reason"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        s.push_str(&format!(
            "**原因**: {}\n",
            truncate(reason, PERMISSION_REASON_MAX_CHARS)
        ));
    }

    if !p.patterns.is_empty() {
        s.push_str("**对象**:\n");
        // Every target shares the one display budget, charged against their
        // SUM. The permission flow proves the sum fits before offering the card
        // (`PermissionRequest::targets_are_fully_shown`), so a per-pattern clip
        // here is only reachable on a surface that bypasses that proof — never
        // on a button a user can approve.
        let budget = crate::backend::PERMISSION_TARGET_MAX_CHARS;
        for r in &p.patterns {
            s.push_str(&format!("- `{}`\n", truncate(r, budget)));
        }
    }

    // An edit/patch/apply_patch permission carries its unified diff — V1's
    // `{filepath, diff}`, V2's `{files: [{file, patch, …}]}` — and that IS the
    // request, so show the actual changed lines instead of dumping raw metadata
    // JSON (which used to make the card show a wall of escaped diff text).
    let diff = diff_from_metadata(p.metadata.as_ref());
    if matches!(action, "edit" | "patch" | "apply_patch")
        && let Some(diff) = diff.as_deref()
    {
        let parsed = parse_edit_diff(diff);
        let metadata_filepath = p
            .metadata
            .as_ref()
            .and_then(|m| m.get("filepath"))
            .and_then(|v| v.as_str());
        let path = metadata_filepath
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
        // The fence language follows the ONE path the card already selected for
        // its header — metadata `filepath`, else the diff's own path, else the
        // first pattern (#794). No agreement check runs across every touched
        // file, so a multi-file patch fences with the selected file's grammar.
        let language = language_for_path(path);
        let body = parsed.map(|d| d.body).unwrap_or_else(|| diff.to_string());
        // Clip BARE, then say so outside the fence: the `…` `truncate_md`
        // appends immediately before the closing fence stops Feishu rendering
        // the block as code at all (#788) — and this is the card where a reader
        // decides whether to approve the change, so losing the code block is
        // worse here than anywhere. The budget is the Tool Panel's own escaped
        // byte cap (#792): the same diff renders under the same limit on both
        // surfaces, and bytes are what the card's JSON budget counts. The
        // language passed here is the same one the fence below is given, so the
        // opening tag's bytes are inside the cap, not beside it.
        let clipped = clip_fenced_body(&body, Some(language), DIFF_MAX_ESCAPED_BYTES);
        let was_clipped = clipped.len() != body.len();
        s.push_str(&crate::feishu::card::fenced_code(&clipped, Some(language)));
        s.push('\n');
        if was_clipped {
            s.push_str(DIFF_TRUNCATION_NOTE);
            s.push('\n');
        }
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

/// The character count of the body `describe_permission` renders, after the
/// card's markdown sanitizer — exactly what the single card element carries.
/// The ONE measurement both the completeness proof and the card build use.
fn rendered_permission_chars(p: &crate::backend::types::PermissionRequest) -> usize {
    let body = describe_permission(p);
    crate::feishu::card::sanitize::CardMarkdown::new()
        .clean(&body)
        .chars()
        .count()
}

/// Which bound a permission card crosses when it cannot show the request in
/// full. One type, so no caller can mistake a budget for an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionOverflow {
    /// The summed targets exceed [`PERMISSION_TARGET_MAX_CHARS`].
    Targets,
    /// The rendered body exceeds the measured single-element ceiling
    /// [`PERMISSION_BODY_MAX_CHARS`].
    Body,
}

impl PermissionOverflow {
    /// The bound the fallback and refusal copy names.
    pub(crate) fn budget(self) -> usize {
        match self {
            Self::Targets => PERMISSION_TARGET_MAX_CHARS,
            Self::Body => PERMISSION_BODY_MAX_CHARS,
        }
    }
}

/// Whether `describe_permission` renders a diff for this request (the diff
/// carries its own escaped-byte cap and truncation note, #792/#794). Keyed on
/// the metadata actually carrying a diff, never on the action name alone: an
/// `edit` with no diff still renders its scaffolding in one element.
fn renders_diff_body(p: &crate::backend::types::PermissionRequest) -> bool {
    matches!(p.permission.as_deref(), Some("edit" | "patch" | "apply_patch"))
        && crate::feishu::card::tool_render::diff_from_metadata(p.metadata.as_ref()).is_some()
}

/// The ONE proof for "can the card show this request in full?".
///
/// `None` means yes. Otherwise the returned bound is the one that failed — the
/// summed target display budget or the rendered-body render invariant. The body
/// is measured from the renderer's own sanitized output, so the proof and the
/// element cannot disagree.
///
/// A request that renders a unified diff is exempt from the **summed-target**
/// budget: `describe_permission` shows the diff under its own escaped-byte cap
/// (`DIFF_MAX_ESCAPED_BYTES`) with a truncation note, so the target-overflow
/// companion (paths only) would deliver the diff nowhere. It is **not**
/// exempt from the rendered-body ceiling: the body still carries every pattern
/// before the diff, and a body past `PERMISSION_BODY_MAX_CHARS` must take the
/// companion, whose body is the rendered description (the clipped diff
/// included).
pub(crate) fn permission_overflow(
    p: &crate::backend::types::PermissionRequest,
) -> Option<PermissionOverflow> {
    if !renders_diff_body(p) && !p.targets_are_fully_shown() {
        return Some(PermissionOverflow::Targets);
    }
    (rendered_permission_chars(p) > PERMISSION_BODY_MAX_CHARS).then_some(PermissionOverflow::Body)
}

/// The result body for a click that ran no verification read (a rejection).
///
/// The surface the click landed on decides what it showed: a standalone card
/// contributes its own summary, an inline block its one-line receipt target
/// under the callback's display label, and a click with neither falls back to
/// the label. The source is chosen by surface, never by the text's shape (a
/// heredoc target contains newlines but is still a one-line summary).
fn unverified_result_body(
    display_label: &str,
    on_standalone: bool,
    standalone_summary: Option<&str>,
    block_target: Option<&str>,
) -> String {
    let summary = standalone_summary.filter(|summary| !summary.is_empty());
    if on_standalone && let Some(summary) = summary {
        return summary.to_string();
    }
    if let Some(target) = block_target.filter(|target| !target.is_empty()) {
        return format!("{display_label}：{target}");
    }
    summary.map_or_else(|| display_label.to_string(), str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perm(action: &str, metadata: serde_json::Value) -> crate::backend::types::PermissionRequest {
        crate::backend::types::PermissionRequest {
            request_id: "per_1".into(),
            session_id: Some("ses_1".into()),
            permission: Some(action.into()),
            patterns: vec!["/proj/src/main.rs".into()],
            metadata: Some(metadata),
            always: vec![],
            always_supported: true,
        }
    }

    /// A switch click's toast names what happened — including the arms a stale
    /// card can reach.
    #[test]
    fn a_switch_toast_names_the_outcome() {
        assert_eq!(
            switch_toast(1, SwitchApplied::Opened, &[false, false]),
            "已切到第 2 题"
        );
        assert_eq!(
            switch_toast(1, SwitchApplied::Stale, &[false, false]),
            "已按最新的选择更新"
        );
        // Out of range: the card names a question that no longer exists.
        assert_eq!(
            switch_toast(9, SwitchApplied::Refused, &[false, false]),
            "这张卡片已过期"
        );
        assert_eq!(
            switch_toast(1, SwitchApplied::Refused, &[false, true]),
            "这道题已经答完"
        );
        assert_eq!(
            switch_toast(1, SwitchApplied::Refused, &[false, false]),
            "已经停在第 2 题"
        );
    }

    /// A receipt is a one-line residue: a genuinely over-budget set of answers
    /// is clipped at the receipt budget, while the answered prefix stays.
    #[test]
    fn a_question_receipt_clips_at_the_receipt_budget() {
        let question = |index: usize| crate::backend::types::QuestionInfo {
            key: format!("q{index}"),
            // 40 characters is the per-question label budget; three of them
            // plus the answers and separators exceed the 120-char receipt.
            question: "哪".repeat(40),
            header: "选择".into(),
            options: vec![crate::backend::types::QuestionOption {
                value: "a".into(),
                label: "A".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let questions: Vec<_> = (0..3).map(question).collect();
        let answers: Vec<_> = (0..3).map(|_| Some(vec!["a".to_string()])).collect();
        let receipt = question_receipt(&questions, &answers);
        assert!(
            receipt.ends_with('…'),
            "an over-budget receipt must be clipped: {receipt}"
        );
        assert!(
            receipt.chars().count() <= RECEIPT_MAX_CHARS + 1,
            "receipt over its budget ({}): {receipt}",
            receipt.chars().count()
        );
        assert!(receipt.starts_with("✅ 已回答："), "{receipt}");
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
        assert!(
            s.contains("```rust\n@@ -1 +1 @@"),
            "the diff is fenced with the edited file's language: {}",
            s
        );
        assert!(s.contains("是否允许"), "closing prompt: {}", s);
        assert!(
            !s.contains(DIFF_TRUNCATION_NOTE),
            "a one-line change stays under the budget: {}",
            s
        );
        // The diff must not leak as escaped raw JSON metadata.
        assert!(!s.contains("\\n"), "no escaped JSON: {}", s);
    }

    /// The result body of an unverified click (a rejection) is chosen by the
    /// surface the click landed on, not by the text's shape: a standalone
    /// card's summary wins even when the same request also has an inline block;
    /// an inline click names the block target under the callback label.
    #[test]
    fn an_unverified_result_body_names_the_clicked_surface() {
        assert_eq!(
            unverified_result_body(
                "🚫 已拒绝",
                true,
                Some("⚡ **执行 Shell 命令**\n- `standalone`"),
                Some("⚡ 执行 Shell 命令 `inline`"),
            ),
            "⚡ **执行 Shell 命令**\n- `standalone`",
            "the standalone card's own summary wins"
        );
        assert_eq!(
            unverified_result_body(
                "🚫 已拒绝",
                false,
                Some("standalone summary"),
                Some("⚡ 执行 Shell 命令 `inline`"),
            ),
            "🚫 已拒绝：⚡ 执行 Shell 命令 `inline`",
            "an inline click names the block target under the label"
        );
        assert_eq!(
            unverified_result_body("🚫 已拒绝", false, Some("standalone summary"), None),
            "standalone summary",
            "no block target falls back to the recorded summary"
        );
        assert_eq!(
            unverified_result_body("🚫 已拒绝", false, None, None),
            "🚫 已拒绝",
            "nothing recorded falls back to the callback label"
        );
    }

    /// Every target shares ONE display budget: a command is rendered whole up
    /// to it (not clipped at a short pattern budget), and every other action
    /// gets the same budget. The renderer's clip is only a last resort for a
    /// surface that bypasses the completeness proof.
    #[test]
    fn describe_permission_renders_a_target_at_the_one_budget() {
        let budget = crate::backend::types::PERMISSION_TARGET_MAX_CHARS;
        let command = "x".repeat(budget);
        let p = crate::backend::types::PermissionRequest {
            request_id: "n:1".into(),
            session_id: Some("thread-1".into()),
            permission: Some("command".into()),
            patterns: vec![command.clone()],
            metadata: None,
            always: vec![],
            always_supported: true,
        };
        let rendered = describe_permission(&p);
        assert!(
            rendered.contains(&command),
            "the whole command is on the card at the shared budget"
        );
        assert!(!rendered.contains('…'), "nothing was clipped: {rendered}");

        // The same budget applies to a path: ordinary paths are no longer
        // clipped at the old 150-character pattern budget.
        let path = format!("/proj/{}", "d/".repeat(80));
        let mut p = p;
        p.permission = Some("write".into());
        p.patterns = vec![path.clone()];
        let rendered = describe_permission(&p);
        assert!(rendered.contains(&path), "an ordinary path fits: {rendered}");
        assert!(!rendered.contains('…'), "and is not clipped: {rendered}");

        // Only a bypassing surface can reach the renderer's clip.
        p.patterns = vec!["x".repeat(budget + 1)];
        let rendered = describe_permission(&p);
        assert!(rendered.contains('…'), "past the budget is clipped: {rendered}");
    }

    /// The card proof measures the body the renderer actually produces, so the
    /// scaffolding counts: a bare max-budget command fits the measured element
    /// ceiling, while the same command plus one metadata line does not.
    #[test]
    fn the_card_proof_charges_the_rendered_body_not_only_the_targets() {
        let command = "x".repeat(crate::backend::PERMISSION_TARGET_MAX_CHARS);
        let mut p = crate::backend::types::PermissionRequest {
            request_id: "n:1".into(),
            session_id: Some("thread-1".into()),
            permission: Some("bash".into()),
            patterns: vec![command],
            metadata: None,
            always: vec![],
            always_supported: true,
        };
        let bare = rendered_permission_chars(&p);
        assert!(
            bare > crate::backend::PERMISSION_TARGET_MAX_CHARS,
            "the bare body carries its scaffolding"
        );
        assert!(
            bare <= crate::backend::PERMISSION_BODY_MAX_CHARS,
            "a bare max-budget command fits the measured element ceiling: {bare}"
        );
        assert_eq!(permission_overflow(&p), None, "and is showable");

        // A real V1 bash permission carries `metadata.command`; the extra
        // metadata line is exactly the scaffolding the target-only proof missed.
        p.metadata = Some(serde_json::json!({ "command": "y".repeat(200) }));
        let with_metadata = rendered_permission_chars(&p);
        assert!(
            with_metadata > crate::backend::PERMISSION_BODY_MAX_CHARS,
            "the metadata line pushes the one element past the ceiling: {with_metadata}"
        );
        assert_eq!(
            permission_overflow(&p),
            Some(PermissionOverflow::Body),
            "the refusal names the body ceiling it crossed"
        );
    }

    /// The render ceiling is character-based and enforced at its exact
    /// boundary. A `<` inside a code span is ONE sanitized character (targets
    /// render in backticks), so the target budget governs it; outside a code
    /// span the same `<` costs five, which is why the body measure is the
    /// sanitized character count and not the authored one.
    #[test]
    fn the_render_ceiling_is_character_based_at_its_boundary() {
        let mut p = crate::backend::types::PermissionRequest {
            request_id: "n:1".into(),
            session_id: Some("thread-1".into()),
            permission: Some("bash".into()),
            patterns: vec!["x".repeat(crate::backend::PERMISSION_TARGET_MAX_CHARS)],
            metadata: Some(serde_json::json!({ "cwd": "" })),
            always: vec![],
            always_supported: true,
        };
        assert_eq!(
            rendered_permission_chars(&p),
            crate::backend::PERMISSION_BODY_MAX_CHARS,
            "the measured boundary itself"
        );
        assert_eq!(permission_overflow(&p), None, "exactly the ceiling fits");

        p.metadata = Some(serde_json::json!({ "cwd": "x" }));
        assert_eq!(
            rendered_permission_chars(&p),
            crate::backend::PERMISSION_BODY_MAX_CHARS + 1,
            "the boundary is character-counted"
        );
        assert_eq!(
            permission_overflow(&p),
            Some(PermissionOverflow::Body),
            "one character over the ceiling is refused"
        );

        // The same 3,000 `<` glyphs as a target are inside a code span: the
        // sanitizer leaves them at one character each, so the card fits.
        p.patterns = vec!["<".repeat(crate::backend::PERMISSION_TARGET_MAX_CHARS)];
        p.metadata = None;
        assert_eq!(rendered_permission_chars(&p), 3_049);
        assert_eq!(permission_overflow(&p), None);

        // Outside a code span (a reason), each `<` expands five-fold; a max
        // target plus a 150-`<` reason crosses the ceiling.
        p.metadata = Some(serde_json::json!({ "reason": "<".repeat(150), "cwd": "/w" }));
        assert!(
            rendered_permission_chars(&p) > crate::backend::PERMISSION_BODY_MAX_CHARS,
            "the five-byte expansion is charged against the element ceiling"
        );
        assert_eq!(permission_overflow(&p), Some(PermissionOverflow::Body));
    }

    /// The diff exemption is keyed on the diff actually being present in the
    /// metadata, not on the action name: a Codex `edit` file-change with no
    /// diff renders its scaffolding and must obey the ceiling.
    #[test]
    fn an_edit_without_a_rendered_diff_is_not_exempt_from_the_ceiling() {
        let mut p = crate::backend::types::PermissionRequest {
            request_id: "n:1".into(),
            session_id: Some("thread-1".into()),
            permission: Some("edit".into()),
            patterns: vec!["x".repeat(crate::backend::PERMISSION_TARGET_MAX_CHARS)],
            metadata: Some(serde_json::json!({
                "reason": "<".repeat(150),
                "command": "y".repeat(200),
                "cwd": "z".repeat(200),
                "description": "w".repeat(200),
            })),
            always: vec![],
            always_supported: true,
        };
        assert!(
            rendered_permission_chars(&p) > crate::backend::PERMISSION_BODY_MAX_CHARS,
            "the no-diff edit renders a large element"
        );
        assert_eq!(
            permission_overflow(&p),
            Some(PermissionOverflow::Body),
            "no diff means no exemption"
        );

        // A real diff renders and keeps its own escaped-byte cap.
        p.metadata = Some(serde_json::json!({
            "filepath": "/proj/src/main.rs",
            "diff": format!("--- a\n+++ b\n@@ -1 +1 @@\n-{}\n+new\n", "x".repeat(5_000)),
        }));
        assert!(
            rendered_permission_chars(&p) > crate::backend::PERMISSION_BODY_MAX_CHARS,
            "the diff body is large"
        );
        assert_eq!(
            permission_overflow(&p),
            Some(PermissionOverflow::Body),
            "a diff is exempt only from the target budget, not the element ceiling"
        );
    }

    /// A diff-carrying request skips the **summed-target** budget (the card
    /// carries the diff), but still obeys the rendered-body ceiling. Past that
    /// ceiling it takes the Body companion, whose payload is the rendered
    /// description — the clipped diff and its truncation note included.
    #[test]
    fn a_diff_request_skips_the_target_budget_but_not_the_render_ceiling() {
        let paths: Vec<String> = (0..25)
            .map(|i| format!("/proj/src/{:0>2}_{}.rs", i, "x".repeat(124)))
            .collect();
        let diff = format!(
            "--- /proj/src/00.rs\n+++ /proj/src/00.rs\n@@ -1 +1 @@\n-{}\n+new\n",
            "x".repeat(1_000)
        );
        let mut p = crate::backend::types::PermissionRequest {
            request_id: "per_diff".into(),
            session_id: Some("ses_1".into()),
            permission: Some("edit".into()),
            patterns: paths.clone(),
            metadata: Some(serde_json::json!({
                "files": [{ "file": paths[0], "patch": diff }]
            })),
            always: vec![],
            always_supported: true,
        };
        assert!(
            p.total_target_len() > crate::backend::PERMISSION_TARGET_MAX_CHARS,
            "the paths are over the summed budget: {}",
            p.total_target_len()
        );
        // The target budget is skipped, but the rendered body carries every
        // pattern plus the diff and crosses the element ceiling.
        assert_eq!(
            permission_overflow(&p),
            Some(PermissionOverflow::Body),
            "many paths plus a diff still overflow the rendered body"
        );
        let companion = crate::bridge::request::companion::companion_text(&p);
        assert!(
            companion.contains("@@ -1 +1 @@") && companion.contains("+new"),
            "the Body companion is the rendered description, diff included: {companion}"
        );

        // A diff past its escaped-byte cap travels in that same companion,
        // clipped by its own cap with its own note.
        p.metadata = Some(serde_json::json!({
            "files": [{
                "file": paths[0],
                "patch": format!(
                    "--- /proj/src/00.rs\n+++ /proj/src/00.rs\n@@ -1 +1 @@\n-{}\n+new\n",
                    "x".repeat(20_000)
                )
            }]
        }));
        let companion = crate::bridge::request::companion::companion_text(&p);
        assert!(
            companion.contains(crate::feishu::card::tool_render::DIFF_TRUNCATION_NOTE),
            "the oversized diff is clipped by its own cap, with the note: {companion}"
        );
        assert_eq!(
            permission_overflow(&p),
            Some(PermissionOverflow::Body),
            "a clipped diff is still a body overflow"
        );

        // A normal diff with one short path fits the card: no companion.
        p.patterns = vec!["/proj/src/main.rs".into()];
        p.metadata = Some(serde_json::json!({
            "filepath": "/proj/src/main.rs",
            "diff": "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n",
        }));
        assert_eq!(
            permission_overflow(&p),
            None,
            "a normal diff stays on the card with no companion"
        );
        let rendered = describe_permission(&p);
        assert!(
            rendered.contains("@@ -1 +1 @@") && rendered.contains("+new"),
            "the card shows the unified diff: {rendered}"
        );

        // Without a diff the same many paths are an ordinary target overflow.
        p.patterns = paths;
        p.metadata = None;
        assert_eq!(
            permission_overflow(&p),
            Some(PermissionOverflow::Targets),
            "no diff means the summed budget applies"
        );
    }

    /// The explanatory reason keeps its own short budget (150) even though the
    /// target budget is much larger: it is context, not the decision target.
    #[test]
    fn describe_permission_keeps_the_reason_at_its_own_short_budget() {
        let reason = "r".repeat(400);
        let rendered = describe_permission(&perm(
            "bash",
            serde_json::json!({ "reason": reason, "cwd": "/work" }),
        ));
        assert!(
            rendered.contains(&format!("**原因**: {}…", "r".repeat(150))),
            "the reason is clipped at its own budget: {rendered}"
        );
        assert!(
            !rendered.contains(&"r".repeat(151)),
            "no reason text survives past 150 characters: {rendered}"
        );
    }

    /// V2's `edit` records `{files: [{file, patch, status, additions,
    /// deletions}]}` with NO top-level `diff`. The permission card must read
    /// that shape, or the approve-an-edit card shows no change at all.
    #[test]
    fn describe_v2_edit_permission_renders_the_files_patch() {
        let p = perm(
            "edit",
            serde_json::json!({
                "files": [{
                    "file": "/proj/src/main.rs",
                    "patch": "===\n--- /proj/src/main.rs\n+++ /proj/src/main.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n context",
                    "status": "modified",
                    "additions": 1,
                    "deletions": 1
                }]
            }),
        );
        let s = describe_permission(&p);
        assert!(
            s.contains("**改动**: 📄 `/proj/src/main.rs` (+1 −1)"),
            "the header names the file and its patch's counts: {s}"
        );
        assert!(
            s.contains("```rust\n@@ -1,2 +1,2 @@"),
            "the patch is fenced with the file's language: {s}"
        );
        assert!(s.contains("-old") && s.contains("+new"), "hunks shown: {s}");
        assert!(
            !s.contains("**详情**"),
            "the diff never falls through to the generic metadata dump: {s}"
        );
    }

    /// A V2 edit can record several files: their patches join in file order and
    /// parse as one diff.
    #[test]
    fn describe_v2_edit_permission_joins_multiple_file_patches() {
        let p = perm(
            "edit",
            serde_json::json!({
                "files": [
                    {
                        "file": "/proj/src/a.rs",
                        "patch": "===\n--- /proj/src/a.rs\n+++ /proj/src/a.rs\n@@ -1 +1 @@\n-a\n+a2",
                        "status": "modified"
                    },
                    {
                        "file": "/proj/src/b.rs",
                        "patch": "===\n--- /proj/src/b.rs\n+++ /proj/src/b.rs\n@@ -1 +1 @@\n-b\n+b2",
                        "status": "modified"
                    }
                ]
            }),
        );
        let s = describe_permission(&p);
        assert!(
            s.contains("**改动**: 📄 `/proj/src/a.rs` (+2 −2)"),
            "the joined patches count both files: {s}"
        );
        for line in ["-a", "+a2", "-b", "+b2"] {
            assert!(s.contains(line), "`{line}` from the joined patches: {s}");
        }
        assert!(s.contains("```rust\n"), "one shared language: {s}");
    }

    /// With no patch anywhere — neither `diff` nor a non-empty `files[].patch`
    /// — an `edit` permission keeps the generic metadata rendering.
    #[test]
    fn describe_edit_permission_without_any_diff_keeps_generic_metadata() {
        let p = perm(
            "edit",
            serde_json::json!({ "files": [{ "file": "/proj/x.rs", "status": "modified" }] }),
        );
        let s = describe_permission(&p);
        assert!(s.contains("**详情**"), "generic metadata branch: {s}");
        assert!(!s.contains("```"), "no empty diff fence: {s}");
    }

    #[test]
    fn describe_permission_fences_by_the_touched_files_language() {
        for (path, diff_path, needle) in [
            ("/proj/notes.md", "/proj/notes.md", "```markdown\n@@ -1 +1 @@"),
            ("/proj/script.py", "/proj/script.py", "```python\n@@ -1 +1 @@"),
            (
                "/proj/thing.unknownext",
                "/proj/thing.unknownext",
                "```plain_text\n@@ -1 +1 @@",
            ),
        ] {
            let diff = format!("--- {diff_path}\n+++ {diff_path}\n@@ -1 +1 @@\n-old\n+new");
            let p = perm("edit", serde_json::json!({ "filepath": path, "diff": diff }));
            let s = describe_permission(&p);
            assert!(s.contains(needle), "`{path}` fence language: {s}");
        }
    }

    /// #792 review: the permission card passes the language it fences with into
    /// the byte budget, so the opening fence's tag is inside the cap. A body
    /// that fits at 11,998 bytes without a tag is pushed over by `.rs`'s 4-byte
    /// `rust` tag at 12,002, and must clip.
    #[test]
    fn a_permission_diff_counts_its_language_tag_inside_the_byte_budget() {
        // `@@ -1,1 +1,1 @@\n` (16) + `+` (1) + filler + `\n` (1).
        let filler = "a".repeat(11_972);
        let diff = format!("--- /proj/big.rs\n+++ /proj/big.rs\n@@ -1,1 +1,1 @@\n+{filler}\n");
        let parsed = parse_edit_diff(&diff).expect("a one-line hunk");
        assert_eq!(parsed.body.len(), 11_990, "the fixture sits at the tag edge");
        assert_eq!(
            parsed.body,
            clip_fenced_body(&parsed.body, None, DIFF_MAX_ESCAPED_BYTES),
            "the body and its fence fit without a language tag"
        );
        let p = perm(
            "edit",
            serde_json::json!({ "filepath": "/proj/big.rs", "diff": diff }),
        );
        let s = describe_permission(&p);
        assert!(
            s.contains(DIFF_TRUNCATION_NOTE),
            "the 4-byte rust tag pushes the block over the cap: {s}"
        );
        assert!(s.contains("```rust\n"), "and the fence keeps the tag: {s}");
    }

    /// #794's path precedence: one selected path — metadata `filepath`, else
    /// the diff's own path, else the first pattern — and the fence language is
    /// that path's language. No agreement check across a patch's files.
    #[test]
    fn describe_permission_fence_language_follows_the_path_precedence() {
        // 1. Metadata `filepath` wins over the diff's header, even when the
        //    two disagree: a `.md` permission is fenced as markdown, not
        //    `plain_text`.
        let diff = "--- /proj/src/main.rs\n+++ /proj/src/main.rs\n@@ -1 +1 @@\n-old\n+new";
        let p = perm(
            "edit",
            serde_json::json!({ "filepath": "/proj/README.md", "diff": diff }),
        );
        let s = describe_permission(&p);
        assert!(
            s.contains("```markdown\n@@ -1 +1 @@"),
            "metadata filepath wins: {s}"
        );
        assert!(!s.contains("```plain_text"), "metadata filepath wins: {s}");

        // 2. Without a metadata `filepath`, the diff's own path wins over the
        //    request's patterns (which name a `.rs` file here).
        let py = "--- /proj/script.py\n+++ /proj/script.py\n@@ -1 +1 @@\n-old\n+new";
        let p = perm("edit", serde_json::json!({ "diff": py }));
        let s = describe_permission(&p);
        assert!(
            s.contains("```python\n@@ -1 +1 @@"),
            "the diff's path beats the patterns: {s}"
        );

        // 3. Only when neither names a file does the first pattern decide.
        let p = perm("edit", serde_json::json!({ "diff": "not a unified diff" }));
        let s = describe_permission(&p);
        assert!(s.contains("```rust\n"), "the pattern decides: {s}");
    }

    /// A multi-file patch fences with the ONE selected path's language, not an
    /// agreement across every touched file: a mixed patch is not forced to
    /// `plain_text`. These are V2-shaped diffs (headers, no `Index:`), so the
    /// parsed path is the first file's.
    #[test]
    fn describe_permission_multi_file_diff_uses_one_selected_path() {
        let same = "\
--- /proj/src/a.rs
+++ /proj/src/a.rs
@@ -1 +1 @@
-a
+a2
===
--- /proj/src/b.rs
+++ /proj/src/b.rs
@@ -1 +1 @@
-b
+b2";
        let p = perm("apply_patch", serde_json::json!({ "diff": same }));
        let s = describe_permission(&p);
        assert!(s.contains("```rust\n@@ -1 +1 @@"), "shared .rs language: {s}");

        let mixed = same.replace("/proj/src/a.rs", "/proj/notes.md");
        let p = perm("apply_patch", serde_json::json!({ "diff": mixed }));
        let s = describe_permission(&p);
        assert!(
            s.contains("```markdown\n@@ -1 +1 @@"),
            "the first file's language fences a mixed patch: {s}"
        );
        assert!(
            !s.contains("```plain_text"),
            "a mixed patch no longer forces plain_text: {s}"
        );
    }

    /// #788/#792/#794: a permission diff past the byte budget clips BARE and
    /// says so after the fence. The `…` inside a fenced body stops Feishu
    /// rendering it as code — and this card is where the reader decides
    /// whether to approve.
    #[test]
    fn a_clipped_permission_diff_keeps_its_code_block_and_says_so() {
        let mut diff = String::from(
            "Index: /proj/src/main.rs\n===\n--- /proj/src/main.rs\n+++ /proj/src/main.rs\n@@ -1,1 +1,1000 @@\n",
        );
        for n in 1..=1000 {
            diff.push_str(&format!("+let value_{n} = compute_something_fairly_long({n});\n"));
        }
        let p = perm(
            "edit",
            serde_json::json!({ "filepath": "/proj/src/main.rs", "diff": diff }),
        );
        let s = describe_permission(&p);
        let note = DIFF_TRUNCATION_NOTE;
        assert!(
            s.contains(&format!("```\n{note}")),
            "the fence closes bare, right before the note: {s}"
        );
        assert!(s.contains("```rust\n"), "the fence keeps its language: {s}");
        assert!(!s.contains('…'), "no ellipsis inside the fence: {s}");
        assert!(s.contains("是否允许"), "closing prompt still present: {s}");
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

    /// #794: only the diff arm changed. Every non-diff branch — bash, a
    /// question, the generic metadata fallback — must stay byte-identical, so
    /// these pin the WHOLE description, not just a substring.
    #[test]
    fn non_diff_permission_descriptions_are_byte_identical() {
        let bash = describe_permission(&perm("bash", serde_json::json!({ "command": "cargo test" })));
        assert_eq!(
            bash,
            "⚡ **执行 Shell 命令**\n**对象**:\n- `/proj/src/main.rs`\n**命令**: `cargo test`\n\nAI 想要执行这个操作，是否允许？"
        );

        let question = describe_permission(&perm("question", serde_json::json!({ "input": "Which?" })));
        assert_eq!(
            question,
            "❓ **向你提问**\n**对象**:\n- `/proj/src/main.rs`\n**输入**: `Which?`\n\nAI 想要执行这个操作，是否允许？"
        );

        let generic = describe_permission(&perm(
            "webfetch",
            serde_json::json!({ "url": "https://example.com" }),
        ));
        assert_eq!(
            generic,
            "🌐 **访问网页**\n**对象**:\n- `/proj/src/main.rs`\n**详情**: `{\"url\":\"https://example.com\"}`\nAI 想要执行这个操作，是否允许？"
        );
    }

    /// V2's permission action is the tool name, so `shell`/`subagent` must not
    /// fall through to the generic label — the V1 `bash` arm alone left every
    /// V2 shell card reading `🔐 执行操作` (and the receipt said the same).
    #[test]
    fn describe_v2_permission_actions_get_specific_labels() {
        for (action, label) in [
            ("shell", "执行 Shell 命令"),
            ("subagent", "调用子代理"),
            ("question", "向你提问"),
        ] {
            let p = perm(action, serde_json::json!({ "command": "cargo test" }));
            let s = describe_permission(&p);
            assert!(s.contains(label), "{action} label: {s}");
            assert!(!s.contains("执行操作"), "{action} fell through: {s}");
            let target = permission_target(&p);
            assert!(target.contains(label), "{action} receipt target: {target}");
        }
    }
}
