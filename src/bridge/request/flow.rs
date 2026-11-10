//! The request-flow engine (spec #298, ticket C): the poll sweep that lists
//! pending requests and surfaces them as cards (poll → prepare → render →
//! resolve), the per-kind in-flight state, and the double-click and claim
//! guards. Everything kind-specific is delegated through the
//! [`super::kind::RequestKind`] seam; the card-delivery helpers live in
//! [`super::delivery`].

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::Instrument;

use crate::bridge::handler::CardActionResult;
use crate::bridge::handles::{CardsHandle, FlowHandles, RequestsHandle, SessionsHandle};
use crate::bridge::pollers::{
    CardTarget, LocationTarget, inline_host_session, mark_stale_cards, resolve_card_target,
    resolve_location_form_target, run_is_over,
};
use crate::bridge::question::{QuestionState, stale_question_card};
use crate::bridge::surfaces::{CompanionDelivery, StandaloneSurface, Surfaces};
use crate::bridge::turn::Turn;

use super::companion::{
    CompanionOutcome, companion_target_digest, describe_permission_target_summary,
    ensure_permission_companion, patterns_target_digest,
};
use super::delivery::{
    Origin, Residue, already_handled_result, denied_receipt, handled_elsewhere_receipt, resolve_blocks,
    vanished_receipt,
};
use super::kind::{
    DeclineReason, InteractionKind, PendingRequest, PermissionOverflow, Prepared, Refusal, RequestKind,
    decline_permission, permission_overflow, permission_target,
};

/// One question request's captured reply inputs (spec #364, S6): the request's
/// fields and the index-aligned per-field answer slots (`answers[i]` is `None`
/// until the field is finalized). Named because the pair travels together
/// through the reply path; captured under one lock, cloned for the reply.
pub(crate) struct QuestionSnapshot {
    pub fields: Vec<crate::backend::types::QuestionInfo>,
    pub answers: Vec<Option<Vec<String>>>,
}

/// A request card cola sent: the live Message id, the summary shown when the
/// card is marked stale, and the owning session + directory. The directory
/// lets the sweep skip cleanups for a directory whose list call failed —
/// "absent from the pending list" only speaks for directories that listed
/// successfully (#130, #144) — and the session id lets `mark_stale_cards`
/// classify the stale copy by the owning run's state.
#[derive(Clone)]
pub struct SentCard {
    pub message_id: String,
    pub summary: String,
    pub directory: String,
    pub session_id: String,
}

/// The fused permission/question flow. The poll sweep, the in-flight state and
/// the double-click/claim guards live here once; the kind supplies the deltas
/// and the delivery module settles their cards.
pub struct RequestFlow {
    kind: Box<dyn RequestKind>,
    /// How often (milliseconds) the poll loop lists pending requests. Defaults
    /// to today's 3 s cadence; tests store a small value so every branch is
    /// exercisable without sleeping real seconds.
    pub poll_interval_ms: std::sync::atomic::AtomicU64,
    /// Test-only proof-of-pass: the sweeps this loop has completed, for
    /// `test_support::wait_for_poll_passes` (issue #958). Never read in
    /// production.
    #[cfg(test)]
    pub(crate) poll_passes: std::sync::atomic::AtomicUsize,
    /// How long (milliseconds) one pending-request list call may take before
    /// the poll loop gives up on it and retries next tick. Bounds a request
    /// stuck on a half-open connection (e.g. across a server restart) so one
    /// hung call cannot freeze the poller forever. Defaults to 30 s; tests
    /// store a small value so the timeout branch runs without sleeping.
    pub list_timeout_ms: std::sync::atomic::AtomicU64,
    /// How long (milliseconds) one withdrawal card PATCH may take before the
    /// sweep gives up on it and retries next tick. Bounds a hung Feishu PATCH
    /// so it cannot wedge the permission poller (and leave an old Allow card
    /// clickable behind a stalled sweep). Defaults to 5 s; tests store a small
    /// value.
    pub withdrawal_timeout_ms: std::sync::atomic::AtomicU64,
    /// The total budget (milliseconds) one sweep's withdrawal-retry phase may
    /// spend. The phase runs before the permission listing, so its worst case
    /// must stay near one poll interval even when several PATCHes hang; entries
    /// it does not reach are simply retried next sweep. Defaults to 2 s.
    pub withdrawal_retry_budget_ms: std::sync::atomic::AtomicU64,
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
    /// `handles.requests.answered_requests`, and one card per answered request.
    answered_results: Arc<Mutex<HashMap<String, CardActionResult>>>,
    /// Session ids with at least one pending request of this kind, as of the
    /// last COMPLETE sweep. The task liveness line (ADR-0054) reads it to show
    /// a child session's wait without listing pending requests per render poll.
    /// Replaced wholesale, and only when no directory failed — unknown is never
    /// read as resolved (#130), exactly like the reminder's own rule.
    pending_sessions: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Directories whose list call failed while the path was already gone from
    /// disk (#421). The server boots a directory's instance on every request,
    /// and `FileSystem.realPath` fails on a missing path, so a retry can never
    /// succeed — the sweep stops calling until the path exists again. The
    /// entries stay mapped, and a skipped directory still counts as FAILED
    /// (#130), so its pending state is never read as resolved.
    dead_dirs: Arc<Mutex<std::collections::HashSet<String>>>,
    /// The ADR-0048 warn-once policy for per-directory list failures: the first
    /// failure WARNs with its cause, an identical repeat is DEBUG, and a
    /// recovery logs INFO. Without it a persistently failing directory WARNed
    /// on every 3 s tick.
    list_failures: Arc<Mutex<crate::bridge::failure_latch::FailureLatch>>,
    /// The same ADR-0048 policy for Auto-Accept reply failures, keyed by
    /// request id: the sweep retries the approval every tick, so the first
    /// failure WARNs, an identical repeat is DEBUG, and a landed reply logs
    /// recovery INFO. Pruned to the pending list each sweep.
    autoaccept_failures: Arc<Mutex<crate::bridge::failure_latch::FailureLatch>>,
    /// The same ADR-0048 policy for a failed Auto-Accept **flag read** (a
    /// parent-chain hop), keyed by session id: the sweep retries it every tick,
    /// so the first failure WARNs, identical repeats DEBUG, and a successful
    /// read logs recovery INFO.
    autoaccept_read_failures: Arc<Mutex<crate::bridge::failure_latch::FailureLatch>>,
    /// The ADR-0048 warn-once policy for the common standalone-target walk,
    /// keyed by directory: a half-open `session_info` must WARN once, not once
    /// per request per sweep.
    target_walk_failures: Arc<Mutex<crate::bridge::failure_latch::FailureLatch>>,
    /// The same policy for the companion lane's reply-target walk.
    companion_walk_failures: Arc<Mutex<crate::bridge::failure_latch::FailureLatch>>,
    /// The persisted surface mirror (ADR-0038 restart re-adoption): every
    /// standalone card this flow sends is written through, and its persisted
    /// records seed [`Self::recovered`] at startup.
    pub(crate) surfaces: Arc<Surfaces>,
    /// request_id → the per-form write lock a Location Form's 「稍后」 and its
    /// reopen share (ADR-0103). They serialize their durable-state transitions
    /// on it, so a dismissal cannot interleave with a reopen's commit.
    location_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// request_ids whose reopen is between its generation read and its commit.
    /// The second click of a double-click sees the first in flight and does not
    /// send a second fresh card; the send itself still runs outside the
    /// per-form write lock, as ADR-0103 requires. A [`ReopenClaim`] owns its
    /// entry, so a cancelled/failed task releases it on drop instead of
    /// wedging the form forever.
    location_reopens: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// request_id → owning directory of the surfaces a PREVIOUS process
    /// persisted (ADR-0038 restart re-adoption). The first sweep that lists
    /// such a request re-adopts its card instead of posting a second one; an
    /// entry whose request left the pending list is reconciled away by the
    /// stale/inline cleanups, like every other in-memory surface.
    recovered: Arc<Mutex<HashMap<String, String>>>,
    /// request_id → owning directory of pending requests whose session id
    /// cannot name a session at all (#489): the `global` sentinel a
    /// session-less MCP elicitation carries, or a permission with none. No
    /// parent chain and no card target can ever resolve for them, so the sweep
    /// warns once (naming the id) and skips them entirely instead of paying a
    /// doomed session read every tick. Retained while the request — or its
    /// failed directory — may still be pending, like every other per-request
    /// registry here.
    session_less: Arc<Mutex<HashMap<String, String>>>,
    /// request_id → a surface withdrawal this flow decided but could not
    /// deliver (the card PATCH failed). The payload is rendered at decision
    /// time and replayed verbatim: re-deciding later could mislabel the card
    /// (the request may already be settled, or another client may have
    /// answered), and the retained registry handle is what makes the retry
    /// possible.
    pending_withdrawals: Arc<Mutex<HashMap<String, PendingWithdrawal>>>,
    /// Test-only record of the session ids [`Self::is_pending_for`] checked
    /// (spec #501): the per-render shared gather must ask each distinct child's
    /// wait at most once, so a test counts the checks this flow served. Absent
    /// from production builds.
    #[cfg(test)]
    pub(crate) wait_checks: Arc<Mutex<Vec<String>>>,
}

/// One decided-but-undelivered surface withdrawal: the replacement card for a
/// standalone surface (built at decision time, from that surface's summary) and
/// the inline receipt line to replay.
#[derive(Debug, Clone)]
struct PendingWithdrawal {
    /// What was decided, so the retry knows whether success also settles the
    /// request (`Refused` / `ResolvedElsewhere`) or still needs the decline
    /// retried (`Retrying`).
    how: Withdrawal,
    receipt: String,
    card: serde_json::Value,
}

/// How one attempt to neutralize a stored surface ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WithdrawalOutcome {
    /// There was no stored surface to patch.
    None,
    /// Every stored surface was patched (or had nothing to render).
    Updated,
    /// A stored surface exists but its PATCH failed: the handle and record are
    /// **kept**, so the withdrawal can be retried.
    Failed,
}

/// What applying a decline did to this sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefusalApplied {
    /// The request is settled and may be marked seen.
    Handled,
    /// The refusal did not land: offer nothing and retry next sweep.
    Retry,
}

/// What the restart re-adoption decided for one recovered request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadoptDecision {
    /// The persisted card is the live surface again.
    Adopted,
    /// The withdrawal of the unbacked card could not land: offer nothing and
    /// retry next sweep (the pending-withdrawal backlog holds it).
    Retry,
    /// The persisted card is gone, unusable, or was withdrawn because it was
    /// not backed by a confirmed companion: surface the request anew.
    NotAdopted,
}

/// What one `/mcp` reopen attempt did. The caller renders a truthful toast per
/// outcome: only [`Self::Reopened`] means a fresh card now exists;
/// [`Self::SendFailed`] and [`Self::NoTarget`] leave the dismissal in place and
/// retryable; [`Self::LostToDismissal`] keeps the newer 「稍后」.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReopenOutcome {
    /// The fresh card landed and its record replaced the dismissal.
    Reopened,
    /// No dismissal exists (already pushed, already reopened, or never
    /// pushed): there is nothing to reopen.
    NotDismissed,
    /// Another click on the same form is between its read and its commit.
    AlreadyReopening,
    /// The directory has no mapped Chat/Topic (or its topic anchor cannot be
    /// resolved), so no fresh card was sent.
    NoTarget,
    /// The fresh card send failed or timed out; the dismissal stayed in place.
    SendFailed,
    /// A 「稍后」 landed during the send: the fresh card was withdrawn again
    /// and the newer dismissal survives.
    LostToDismissal,
}

/// An in-flight reopen claim. Its `Drop` releases the id on every exit —
/// including a cancelled task — so a dropped send cannot leave the form
/// permanently "already reopening". The claim is inserted under the per-form
/// write lock and held until the reopen finishes.
struct ReopenClaim {
    id: String,
    claims: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl ReopenClaim {
    fn claim(claims: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>, id: &str) -> Option<Self> {
        let mut guard = claims.lock().unwrap_or_else(|error| error.into_inner());
        if !guard.insert(id.to_string()) {
            return None;
        }
        drop(guard);
        Some(Self {
            id: id.to_string(),
            claims: Arc::clone(claims),
        })
    }
}

impl Drop for ReopenClaim {
    fn drop(&mut self) {
        self.claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.id);
    }
}

/// Tell the chat why a permission was declined before it could be offered.
///
/// The normal first sighting has no card to withdraw, so this is the only
/// surface the refusal gets: routed exactly like a permission card (the reply
/// target of the session's prompt, or the chat). One chooser, so the overflow
/// and uncertain-outcome refusals cannot drift.
async fn post_withdrawal_notice(
    handles: &FlowHandles,
    p: &crate::backend::types::PermissionRequest,
    directory: &str,
    how: Withdrawal,
    walk_timeout_ms: u64,
) {
    let card = match how {
        Withdrawal::Refused(overflow) => {
            crate::feishu::card::question::build_unshowable_permission_card(overflow.budget())
        }
        Withdrawal::CompanionUncertain => crate::feishu::card::question::build_uncertain_permission_card(),
        _ => return,
    };
    post_refusal_card(handles, p, directory, card, walk_timeout_ms).await;
}

/// Route one non-interactive refusal card exactly like a permission card: the
/// reply target of the session's prompt, or the chat.
///
/// Best effort and **bounded**: this runs in the sweep, so a half-open
/// `session_info` or Feishu send must not freeze it. It changes no request
/// state — the decline already landed before this notice — so a timeout only
/// loses the notice, never the decision.
async fn post_refusal_card(
    handles: &FlowHandles,
    p: &crate::backend::types::PermissionRequest,
    directory: &str,
    card: serde_json::Value,
    walk_timeout_ms: u64,
) {
    let session_id = p.session_id.as_deref().unwrap_or("");
    let resolved = crate::bridge::bounded_call(
        &format!("permission {} notice target", p.request_id),
        walk_timeout_ms,
        async {
            Ok::<_, crate::error::BridgeError>(
                resolve_card_target(
                    &handles.sessions,
                    &handles.cards,
                    &handles.backend,
                    &handles.platform,
                    session_id,
                    directory,
                )
                .await,
            )
        },
    )
    .await;
    let Some(resolved) = resolved else {
        tracing::warn!(
            "permission {} on session {session_id}: notice target walk timed out; \
             skipping the best-effort notice",
            p.request_id
        );
        return;
    };
    match resolved.ok().flatten() {
        Some(CardTarget::ReplyTo(message_id)) => {
            match crate::bridge::bounded_call(
                &format!("permission {} notice reply", p.request_id),
                STANDALONE_SEND_TIMEOUT_MS,
                handles.platform.reply_card(&message_id, &card),
            )
            .await
            {
                Some(Ok(_)) => {}
                Some(Err(e)) => tracing::warn!("permission notice reply failed: {e}"),
                None => tracing::warn!("permission notice reply timed out"),
            }
        }
        Some(CardTarget::Chat(chat_id)) => {
            match crate::bridge::bounded_call(
                &format!("permission {} notice send", p.request_id),
                STANDALONE_SEND_TIMEOUT_MS,
                handles.platform.send_card("chat_id", &chat_id, &card),
            )
            .await
            {
                Some(Ok(_)) => {}
                Some(Err(e)) => tracing::warn!("permission notice send failed: {e}"),
                None => tracing::warn!("permission notice send timed out"),
            }
        }
        None => tracing::warn!(
            "permission {} on session {session_id}: no reply target for the notice",
            p.request_id
        ),
    }
}

/// What to leave behind when a stored surface must stop being interactive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Withdrawal {
    /// cola refused the request: the card carries the auto-reject copy naming
    /// the bound that failed.
    Refused(PermissionOverflow),
    /// The server no longer holds it (another client settled it): a neutral
    /// "handled elsewhere" surface, never a claim that cola rejected it.
    ResolvedElsewhere,
    /// The companion outcome is unknown (it may or may not have landed): the
    /// card is withdrawn and the request declined, because an unconfirmed
    /// delivery is not a completeness proof (ADR-0102).
    CompanionUncertain,
    /// A recovered card the new process cannot re-adopt because its full
    /// content was never delivered (no companion, or only an uncertain marker).
    /// The card is withdrawn neutrally and the request continues through the
    /// normal fresh path — prepare (Auto-Accept) or the companion lane — in the
    /// same sweep. It is neither approved nor declined in place (ADR-0101).
    Superseded,
    /// cola could not land its refusal and will retry: a "will retry" surface
    /// that claims nothing.
    Retrying,
}

/// The budget for one withdrawal card PATCH. A hung Feishu call must not wedge
/// the permission sweep — the retry map exists precisely to finish a
/// neutralization later, so a timeout is a retry, not a failure to record.
const WITHDRAWAL_PATCH_TIMEOUT_MS: u64 = 5_000;

/// The budget for the 「稍后」 card PATCH, which runs inside a card callback
/// (a ~2 s ack budget) rather than the sweep. A timeout keeps the card and its
/// record, so the user can retry; it must never spend the whole ack budget.
///
/// A card PATCH is at-least-once, so a timeout can mean the write DID land.
/// The durable dismissal is written and checked BEFORE the PATCH (ADR-0103), so
/// a timed-out card write leaves the form reopenable: a card that did not land
/// stays live, and one that landed is terminal but already recorded dismissed.
const DEFER_WITHDRAWAL_TIMEOUT_MS: u64 = 1_500;

/// The budget for one standalone card send (`reply_card`/`send_card`). A card
/// callback runs on a deadline, and the sweep must not hang on a stalled
/// Feishu call either: a timeout is a retry next sweep, exactly like a send
/// error. It matches the companion lane's own send budget.
const STANDALONE_SEND_TIMEOUT_MS: u64 = 10_000;

/// The total time one sweep's withdrawal-retry phase may spend. Kept below the
/// per-PATCH budget and near one poll interval: the phase runs before the
/// listing, so a pile of hung PATCHes must not push permission polling back by
/// minutes. Entries it does not reach stay for the next sweep.
const WITHDRAWAL_RETRY_PHASE_BUDGET_MS: u64 = 2_000;

/// How many decided-but-undelivered withdrawals one sweep retries. Together
/// with the phase budget this bounds the sweep's start-up work even when every
/// entry is stuck; the rest follow on later sweeps, so a full backlog costs
/// several sweeps rather than one long stall.
const MAX_WITHDRAWAL_RETRIES_PER_SWEEP: usize = 4;

/// How many decided-but-undelivered withdrawals one flow retains.
///
/// Each entry carries its rendered replacement card, so an unbounded map would
/// retain payloads for as long as a card can stay unpatched (e.g. a permanently
/// refused PATCH). A backlog this size is already pathological; past it the new
/// entry is dropped rather than retained, and the vanished-surface pass still
/// neutralizes the card with its neutral receipt (only the precise copy is
/// lost).
const MAX_PENDING_WITHDRAWALS: usize = 32;

/// How many consecutive complete-sweep omissions a Location Form record
/// tolerates before its decision is pruned. One transient omission (a listing
/// that momentarily misses a still-pending form) must not end a 「稍后」; three
/// is long enough to ride one out and short enough that a genuinely gone form's
/// record does not leak (ADR-0103).
pub(crate) const LOCATION_FORM_OMISSION_GRACE_SWEEPS: u32 = 3;

/// How long a first surfacing's session drain may take (issue #1023), for both
/// its transcript read and its render pass. The drain exists to put a
/// request's pre-question text on the card before the block freezes it, but a
/// wedged connection must not delay surfacing more than a few seconds — past
/// the bound the request surfaces exactly as before, without the anchor. This
/// is shorter than the flow's list timeout on purpose: listing is what the
/// sweep is for, the drain is an improvement.
const SURFACING_DRAIN_TIMEOUT_MS: u64 = 5_000;

/// The asking tool part's server time in a surfacing read (issue #1023): the
/// newest server timestamp the read carries.
///
/// While a question or permission is pending its turn is blocked, so the tool
/// call that raised it is the newest part the server wrote — this is that
/// call's `time.created` in the ordinary case and, generally, a high-water mark
/// no content the Host could have seen when they clicked exceeds. Everything
/// the server writes after the click is timestamped above it, which is the only
/// property the receipt's cut needs.
/// `None` when the read carries no server time at all (the block then keeps the
/// old receipt behaviour).
fn asking_anchor(transcript: &crate::backend::SessionTranscript) -> Option<i64> {
    let timed_messages = transcript
        .messages
        .iter()
        .filter_map(|message| message.time.map(|time| message.newest_activity_ms(time.created)));
    // A message without a server time cannot be placed, but its parts may
    // still carry starts — include them rather than dropping the only clock.
    let untimed_parts = transcript
        .messages
        .iter()
        .filter(|message| message.time.is_none())
        .flat_map(|message| message.parts.iter())
        .filter_map(crate::backend::Part::started_at);
    timed_messages.chain(untimed_parts).max()
}

/// Whether `value` is answer state cola may persist for a Location Form.
///
/// A **multi-select** value is always persisted: its entries are deliberate
/// toggles, including a custom one the Host added to the selection (which the
/// card renders as a checked chip and a confirm will submit). A `String`
/// field stores only its clicked option values; a custom typed answer is
/// free text and stays client-side. A numeric input is likewise typed text.
/// A boolean is the fixed `"true"`/`"false"` button value, and an external
/// acknowledgement is the same fixed `"true"` — both are clicked button values,
/// not text the Host typed.
fn persisted_field_value(field: &crate::backend::types::QuestionInfo, value: &str) -> bool {
    use crate::backend::types::FormFieldKind;
    match field.kind {
        FormFieldKind::Boolean => value == "true" || value == "false",
        FormFieldKind::External => value == "true",
        FormFieldKind::Multiselect => true,
        FormFieldKind::String => field.options.iter().any(|option| option.answer_value() == value),
        FormFieldKind::Number | FormFieldKind::Integer => false,
    }
}

impl RequestFlow {
    pub fn new(kind: Box<dyn RequestKind>, surfaces: Arc<Surfaces>) -> Self {
        // Hydrate this kind's half of the persisted record: the standalone
        // cards it sent and the requests whose surfaces the next sweep must
        // re-adopt (both an inline block and a standalone card count).
        let state = surfaces.snapshot();
        let interaction_kind = kind.interaction_kind();
        // A dismissed Location Form is a deliberately surface-less pending
        // request (ADR-0038's 2026-11-09 amendment): its persisted decision is
        // authoritative, so neither its card nor its recovery belongs in this
        // process's state. The push decision reads the same record and will not
        // re-post it.
        let dismissed: std::collections::HashSet<&str> = state
            .location_forms
            .iter()
            .filter(|(_, record)| record.is_dismissed())
            .map(|(id, _)| id.as_str())
            .collect();
        let sent_cards: HashMap<String, SentCard> = state
            .standalone
            .iter()
            .filter(|(id, s)| s.kind == interaction_kind && !dismissed.contains(id.as_str()))
            .map(|(id, s)| {
                (
                    id.clone(),
                    SentCard {
                        message_id: s.message_id.clone(),
                        summary: s.summary.clone(),
                        directory: s.directory.clone(),
                        session_id: s.session_id.clone(),
                    },
                )
            })
            .collect();
        let recovered: HashMap<String, String> = state
            .inline
            .iter()
            .filter(|(_, s)| s.kind == interaction_kind)
            .map(|(id, s)| (id.clone(), s.directory.clone()))
            .chain(
                state
                    .standalone
                    .iter()
                    .filter(|(id, s)| s.kind == interaction_kind && !dismissed.contains(id.as_str()))
                    .map(|(id, s)| (id.clone(), s.directory.clone())),
            )
            .collect();
        Self {
            kind,
            poll_interval_ms: std::sync::atomic::AtomicU64::new(3000),
            #[cfg(test)]
            poll_passes: std::sync::atomic::AtomicUsize::new(0),
            list_timeout_ms: std::sync::atomic::AtomicU64::new(30_000),
            withdrawal_timeout_ms: std::sync::atomic::AtomicU64::new(WITHDRAWAL_PATCH_TIMEOUT_MS),
            withdrawal_retry_budget_ms: std::sync::atomic::AtomicU64::new(WITHDRAWAL_RETRY_PHASE_BUDGET_MS),
            sent_cards: Arc::new(Mutex::new(sent_cards)),
            listed_dirs: Arc::new(Mutex::new(std::collections::HashSet::new())),
            question_state: Arc::new(Mutex::new(HashMap::new())),
            answered_results: Arc::new(Mutex::new(HashMap::new())),
            pending_sessions: Arc::new(Mutex::new(std::collections::HashSet::new())),
            dead_dirs: Arc::new(Mutex::new(std::collections::HashSet::new())),
            list_failures: Arc::new(Mutex::new(crate::bridge::failure_latch::FailureLatch::default())),
            autoaccept_failures: Arc::new(Mutex::new(crate::bridge::failure_latch::FailureLatch::default())),
            autoaccept_read_failures: Arc::new(Mutex::new(
                crate::bridge::failure_latch::FailureLatch::default(),
            )),
            target_walk_failures: Arc::new(Mutex::new(crate::bridge::failure_latch::FailureLatch::default())),
            companion_walk_failures: Arc::new(Mutex::new(
                crate::bridge::failure_latch::FailureLatch::default(),
            )),
            surfaces,
            location_locks: Arc::new(Mutex::new(HashMap::new())),
            location_reopens: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            recovered: Arc::new(Mutex::new(recovered)),
            session_less: Arc::new(Mutex::new(HashMap::new())),
            pending_withdrawals: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            wait_checks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Record an Auto-Accept reply failure for `request_id` with ADR-0048's
    /// warn-once cadence: the sweep retries the approval every tick, so the
    /// first failure WARNs and an identical repeat is DEBUG.
    pub(crate) async fn note_autoaccept_failure(&self, request_id: &str, error: impl std::fmt::Display) {
        self.autoaccept_failures
            .lock()
            .await
            .failed(request_id, "auto-accept", "", error);
    }

    /// Clear the Auto-Accept latch for `request_id` after a landed approval
    /// (INFO once when it had been failing).
    pub(crate) async fn note_autoaccept_success(&self, request_id: &str) {
        self.autoaccept_failures
            .lock()
            .await
            .succeeded(request_id, "auto-accept");
    }

    /// Record a failed Auto-Accept flag read for `session_id` (ADR-0048: the
    /// first WARNs, identical repeats DEBUG).
    pub(crate) async fn note_autoaccept_read_failure(&self, session_id: &str, error: impl std::fmt::Display) {
        self.autoaccept_read_failures
            .lock()
            .await
            .failed(session_id, "auto-accept flag read", "", error);
    }

    /// Clear the flag-read latch for `session_id` (INFO once when it had been
    /// failing).
    pub(crate) async fn note_autoaccept_read_success(&self, session_id: &str) {
        self.autoaccept_read_failures
            .lock()
            .await
            .succeeded(session_id, "auto-accept flag read");
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

    /// Whether `session_id` has a pending request of this kind, per the last
    /// complete sweep (ADR-0054). A session never read as pending, or one whose
    /// sweep failed, reads as false.
    pub(crate) async fn is_pending_for(&self, session_id: &str) -> bool {
        #[cfg(test)]
        self.wait_checks.lock().await.push(session_id.to_string());
        self.pending_sessions.lock().await.contains(session_id)
    }

    /// Roll back a claim made by [`Self::try_mark_answered`] after a GENUINE
    /// reply failure, so the user can retry. A benign 404 ("already resolved")
    /// keeps the claim — the request really is gone.
    pub(crate) async fn unmark_answered(&self, requests: &RequestsHandle, req_id: &str) {
        requests.answered_requests.lock().await.remove(req_id);
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
    pub(crate) async fn remember_question(&self, req: &crate::backend::types::QuestionRequest, dir: &str) {
        // A Location Form's partial answer state is persisted with its surface
        // record (ADR-0103): a restart re-adopts the live card WITHOUT
        // repainting it (that would wipe a typed answer), so the state the
        // card's next confirm/submit reads must be restored from disk or the
        // visible 已选 markers and the submitted answer disagree.
        let partial = req
            .is_location()
            .then(|| self.surfaces.location_form_partial(&req.id))
            .flatten();
        let mut states = self.question_state.lock().await;
        match states.entry(req.id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().refresh(req, dir);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let mut state = QuestionState::new(req.clone(), dir.to_string());
                if let Some(partial) = partial {
                    state.restore_paint(crate::bridge::question::QuestionPaint {
                        display: partial.display,
                        done: partial.done,
                        open: partial.open,
                        // Typed text is never persisted: it stays in the
                        // client (the card-repaints rule), and a submit
                        // carries it as the form value it arrives with.
                        drafts: Vec::new(),
                    });
                }
                entry.insert(state);
            }
        }
    }

    /// Persist the partial answer state a Location Form's card currently
    /// shows, for the restart re-adoption that must not repaint the card
    /// (ADR-0103). A no-op for a session-backed question, or for a form whose
    /// state is no longer remembered; an empty selection clears the entry.
    pub(crate) async fn persist_location_form_partial(&self, req_id: &str) {
        let partial = self
            .with_question_state(req_id, |state| {
                let request = state.request();
                if !request.is_location() {
                    return None;
                }
                let paint = state.paint();
                let mut display = Vec::with_capacity(paint.display.len());
                let mut done = paint.done.clone();
                for (index, values) in paint.display.into_iter().enumerate() {
                    let Some(field) = request.questions.get(index) else {
                        done[index] = false;
                        display.push(None);
                        continue;
                    };
                    let Some(values) = values else {
                        done[index] = false;
                        display.push(None);
                        continue;
                    };
                    let before = values.len();
                    // A multi-select entry is a deliberate toggle (including a
                    // custom one the card shows as a checked chip), so it is
                    // persisted. A typed single-select/free-text answer and a
                    // numeric input are not: the card-repaints rule keeps that
                    // text client-side.
                    let kept: Vec<String> = values
                        .into_iter()
                        .filter(|value| persisted_field_value(field, value))
                        .collect();
                    if kept.len() != before {
                        done[index] = false;
                    }
                    // Keep a confirmed empty selection (`不选`) as `Some(vec![])`
                    // so the restart still knows the slot is final; an open
                    // slot with nothing selected stays `None`.
                    display.push((!kept.is_empty() || done[index]).then_some(kept));
                }
                Some(crate::bridge::surfaces::LocationFormPartial {
                    display,
                    done,
                    open: paint.open,
                })
            })
            .await
            .flatten();
        if let Some(partial) = partial {
            self.surfaces.set_location_form_partial(req_id, partial);
        }
    }

    /// Run `f` against one remembered question's state under the state lock.
    /// `None` when the request is no longer remembered (a sweep raced the
    /// click, #130) — the caller must classify that, never reply from an
    /// empty snapshot.
    pub(crate) async fn with_question_state<R>(
        &self,
        req_id: &str,
        f: impl FnOnce(&mut QuestionState) -> R,
    ) -> Option<R> {
        self.question_state.lock().await.get_mut(req_id).map(f)
    }

    /// The remembered question request, cloned for a card rebuild.
    pub(crate) async fn question_request(
        &self,
        req_id: &str,
    ) -> Option<crate::backend::types::QuestionRequest> {
        self.question_state
            .lock()
            .await
            .get(req_id)
            .map(|s| s.request().clone())
    }

    /// Whether `req_id` is a remembered directory-owned request (a Location
    /// Form). A late click whose state is gone cannot say, and needs no answer:
    /// it takes the neutral classification path instead of a reply.
    pub(crate) async fn is_location_question(&self, req_id: &str) -> bool {
        self.question_state
            .lock()
            .await
            .get(req_id)
            .is_some_and(|state| state.request().is_location())
    }

    /// The durable push/dismissal state `/mcp` renders for `req_id`: `None`
    /// when the form was never pushed, else the one record's state. One read,
    /// so the listed label cannot come from two different moments.
    pub(crate) fn location_form_state(
        &self,
        req_id: &str,
    ) -> Option<crate::bridge::surfaces::LocationFormState> {
        self.surfaces.location_form(req_id).map(|record| record.state)
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

    /// Withdraw a request's standalone card **without settling it**: the
    /// Location Form 「稍后」 control.
    ///
    /// The durable dismissal is written **first**, and the card is only made
    /// terminal once the record on disk says dismissed (ADR-0103). Two
    /// failures follow from that order:
    ///
    /// - a failed dismissal write leaves the live card and its pushed record
    ///   untouched — still answerable, and a later `/mcp`/sweep sees a form
    ///   that was never withdrawn;
    /// - a failed (or timed-out) card PATCH after a durable dismissal leaves a
    ///   live card with a dismissed record. The form is recoverable either
    ///   way: the live card still answers, and `/mcp` can reopen it. A
    ///   timed-out PATCH that actually landed is the same state — the record
    ///   already says dismissed, so the terminal card is reopenable rather
    ///   than stranded behind a `Pushed` record. Once the dismissal is durable
    ///   the form is no longer a waiting push, so its `sent_cards` entry is
    ///   dropped and its Message Pin released regardless of the PATCH result;
    ///   a live card left by an unlanded PATCH is the accepted orphan.
    ///
    /// The per-form write lock serializes this with a reopen (ADR-0103): a
    /// reopen reads the dismissal generation under the lock, sends outside it,
    /// and clears only that generation, so a dismissal that lands during the
    /// send survives. Returns whether the **dismissal is committed** — not
    /// whether the card PATCH landed; the in-flight state stays, because the
    /// form is still pending.
    pub(crate) async fn defer_standalone_surface(
        &self,
        handles: &FlowHandles,
        req_id: &str,
        directory: &str,
        replacement: &serde_json::Value,
    ) -> bool {
        let lock = self.location_form_lock(req_id).await;
        let _guard = lock.lock().await;
        // The decision that must never be missing comes first, and its write
        // is checked: a card must not become control-less while the record
        // still says it is live (a restart would re-adopt the terminal card
        // and leave the form unreachable).
        if !self.surfaces.dismiss_location_form(req_id, directory) {
            tracing::warn!(
                "{} {}: the durable dismissal write failed; keeping the live card and its record",
                self.kind.label(),
                req_id
            );
            return false;
        }
        let sent = self.sent_cards.lock().await.get(req_id).cloned();
        if let Some(sent) = sent
            && !self
                .withdraw_card(handles, req_id, &sent.message_id, replacement, "defer withdrawal")
                .await
        {
            // The dismissal is committed, so the form is no longer a waiting
            // push: drop the sent-card entry and let the caller release the
            // pin. A definite failure leaves the live card answerable; an
            // ambiguous one may have landed. Either way the old card is now an
            // accepted orphan/duplicate and `/mcp` reopens the form.
            tracing::warn!(
                "{} {}: dismissal committed but the card PATCH did not land; the live card is no longer tracked",
                self.kind.label(),
                req_id
            );
        }
        self.sent_cards.lock().await.remove(req_id);
        true
    }

    /// The per-form write lock a Location Form's 「稍后」 and reopen share.
    async fn location_form_lock(&self, req_id: &str) -> Arc<Mutex<()>> {
        self.location_locks
            .lock()
            .await
            .entry(req_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// The remembered questions and their current answers, captured for the
    /// reply payload and the completion card. The answers are CLONED, not
    /// consumed: a genuine reply failure rolls the guard claim back and the
    /// state must stay intact so the click can be retried. `None` when the
    /// state vanished (a sweep raced the click) — the caller must classify
    /// that (#130), never reply from an empty snapshot.
    pub(crate) async fn question_snapshot(&self, req_id: &str) -> Option<QuestionSnapshot> {
        let states = self.question_state.lock().await;
        let state = states.get(req_id)?;
        Some(QuestionSnapshot {
            fields: state.request().questions.clone(),
            answers: state.answers().to_vec(),
        })
    }

    /// Let the kind remember a request surfaced without the poll loop (a
    /// busy-follow host), so its block's buttons still resolve. A kind with no
    /// in-flight state has nothing to remember.
    pub(crate) async fn remember_surfaced(&self, req: &PendingRequest, dir: &str) {
        self.kind.remember_surfaced(self, req, dir).await;
    }

    /// Seed this kind's inline block on a NEW host card with its initial
    /// state, for a caller that already holds the host's card write lock —
    /// `/card`'s post, arm, seed and pointer are one critical section
    /// (ADR-0100).
    pub(crate) async fn add_initial_inline_locked(
        &self,
        cards: &CardsHandle,
        host: &str,
        req: &PendingRequest,
        dir: &str,
    ) -> bool {
        // An over-budget permission must never be seeded onto a busy-follow card
        // without its companion: the block would render a clipped full target
        // and its Allow button would be a dead end (the click guard would refuse
        // it). Leaving it to the sweep lets the sweep deliver the companion and
        // surface the summary first (ADR-0102).
        if self.companion_for(req).is_none()
            && let PendingRequest::Permission(p) = req
            && permission_overflow(p).is_some()
        {
            return false;
        }
        self.kind
            .add_initial_inline_locked(self, cards, host, req, dir, None)
            .await
    }

    /// The result for a click whose question state is gone (#130). It never
    /// reaches the backend and never records anything: a directory this
    /// process has listed successfully means the request provably left the
    /// pending list → neutral "已处理"; otherwise cola cannot know (fresh
    /// process before the first sweep, failing lists, unknown directory) and
    /// must not claim the request was handled.
    pub(crate) async fn missing_question_result(
        &self,
        directory: Option<&str>,
        inline: bool,
    ) -> CardActionResult {
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
    pub(crate) async fn question_state_live(&self, req_id: &str) -> bool {
        self.question_state.lock().await.contains_key(req_id)
    }

    /// The #130 gate in front of every question click: `Some(result)` when the
    /// state is gone — the click must not reply, so it gets the classified
    /// result; `None` when the state is live and the caller may proceed.
    pub(crate) async fn question_gate(
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
    pub(crate) async fn missing_after_claim(
        &self,
        requests: &RequestsHandle,
        req_id: &str,
        directory: Option<&str>,
        inline: bool,
    ) -> CardActionResult {
        self.unmark_answered(requests, req_id).await;
        self.missing_question_result(directory, inline).await
    }

    /// #187: reject every request of this kind that is still pending for
    /// `session_id` or one of its sub-task descendants, when the turn that
    /// would have consumed it ended without completing (`/stop`, interrupt,
    /// prompt error). The tool fiber behind such a request is dead, so
    /// approving it is meaningless; leaving it alive only lets the next turn
    /// re-host a ghost block (ADR-0038, rule 1). Mirrors
    /// [`approve_pending_for_session`]'s session/descendant filter.
    ///
    /// Only a KNOWN state is resolved: a failed/timed-out list, an empty
    /// session id, and another session all stay untouched — unknown is never
    /// read as resolved (#130/#144). Returns the ids the
    /// server actually rejected, in list order, so the caller turns their
    /// blocks into `🚫 已拒绝` receipts through `resolve_blocks`.
    pub(crate) async fn reject_pending_for_session(
        &self,
        requests: &RequestsHandle,
        sessions: &SessionsHandle,
        backend: &Arc<dyn crate::backend::Backend>,
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
            let result = self.kind.reject(self, &dir_backend, req).await;
            match result {
                Ok(()) => {
                    tracing::info!(
                        "Rejected leftover {} {} on session {} (its turn ended without completing)",
                        self.kind.label(),
                        req.id(),
                        sid
                    );
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
    /// request. Spawned once per kind at App startup. The [`PollLoop`] seam
    /// owns the cadence ([`Self::poll_interval_ms`], injectable), the
    /// serverless guard and the failure latch; the pass is one sweep.
    ///
    /// The pass returns `Ok(())` by design (ADR-0048): every per-item condition
    /// of a sweep — one directory's list call, one request's surfacing, one
    /// card repaint — is reported where it happens with its own finer
    /// key/scope, so there is no tick-level failure for the loop's keyless
    /// latch to name. That latch is consumed only by a pass that fails as a
    /// whole; the server-reconcile loop is the one such pass today. A future
    /// pass returns `Err` only when the tick itself could not do its job, never
    /// to relay a per-item condition.
    ///
    /// [`PollLoop`]: crate::bridge::poll::PollLoop
    pub(crate) async fn poll_loop(&self, handles: &FlowHandles) -> crate::error::Result<()> {
        // The pass returns its tick's future, so the loop's cross-tick memory
        // (the requests it has already surfaced) travels through an Arc rather
        // than a borrowed local; this flow is its only owner.
        let seen = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let mut poll = crate::bridge::poll::PollLoop::new(&self.poll_interval_ms, "request poll");
        poll.poll(
            || !handles.backend.base_url().is_empty(),
            move || {
                let seen = Arc::clone(&seen);
                async move {
                    let mut seen = seen.lock().await;
                    self.sweep(handles, &mut seen).await;
                    // Proof-of-pass for the tests waiting on this loop (#958).
                    #[cfg(test)]
                    self.poll_passes
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Per-item failures were logged inside `sweep`; nothing
                    // tick-level to latch (see the doc above).
                    Ok(())
                }
            },
        )
        .await
    }

    /// The standalone cards whose request has provably left the pending list:
    /// not pending, not answered by cola, and owned by a directory that listed
    /// **successfully this sweep**. A card whose directory is mapped but whose
    /// list failed, or whose directory is no longer mapped at all, said
    /// nothing: it must stay live (and keep its Location Form record/partial
    /// state) until a sweep that can actually speak for it. The sweep
    /// classifies these cards' sessions together with the inline candidates —
    /// one status read per session per pass — and then hands the list to
    /// [`mark_stale_cards`] to repaint.
    async fn stale_standalone_cards(
        &self,
        requests: &RequestsHandle,
        pending: &std::collections::HashSet<String>,
        directories: &[String],
        failed_dirs: &std::collections::HashSet<String>,
    ) -> Vec<(String, SentCard)> {
        let answered = requests.answered_requests.lock().await;
        self.sent_cards
            .lock()
            .await
            .iter()
            .filter(|(rid, card)| {
                !pending.contains(*rid)
                    && !answered.contains(*rid)
                    && directories.iter().any(|dir| dir == &card.directory)
                    && !failed_dirs.contains(&card.directory)
            })
            .map(|(rid, card)| (rid.clone(), card.clone()))
            .collect()
    }

    /// The candidate sessions whose run is provably over: one bounded
    /// `session_status` read per distinct `(session → directory)` candidate. A
    /// failed or unrecognised read leaves its session out — unknown is never
    /// read as an interruption (the neutral receipt stands, exactly as before
    /// the classifier existed). An empty session id (a standalone record
    /// written before the owner was persisted) cannot name a run either:
    /// skipped, so its request keeps the neutral copy. Reads nothing when no
    /// request vanished.
    async fn dead_run_sessions(
        &self,
        handles: &FlowHandles,
        candidates: &HashMap<String, String>,
    ) -> std::collections::HashSet<String> {
        let timeout_ms = self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        let mut dead = std::collections::HashSet::new();
        for (session_id, directory) in candidates {
            if session_id.is_empty() {
                continue;
            }
            if run_is_over(&handles.backend, session_id, Some(directory), timeout_ms).await {
                dead.insert(session_id.clone());
            }
        }
        dead
    }

    /// One poll iteration: list pending requests per known session directory,
    /// surface the unseen ones, then reconcile everything that left the list
    /// (stale standalone cards, inline sections, and the remembered question
    /// state). Extracted from the loop so tests can drive
    /// one deterministic sweep; the loop's serverless guard (Lazy Start hasn't
    /// attached or spawned yet) lives in [`Self::poll_loop`], so a direct
    /// caller on a serverless app must skip it itself.
    pub(crate) async fn sweep(&self, handles: &FlowHandles, seen: &mut std::collections::HashSet<String>) {
        // Deliver any withdrawal a previous sweep decided but could not patch.
        // It cannot ride the per-request loop once the request left the pending
        // list (a decline that landed before its card PATCH failed), so it runs
        // before the listing and completes the neutralization first.
        let retried_withdrawals = self.retry_pending_withdrawals(handles).await;
        // Pending requests live in the server instance for the session's
        // directory; `GET /permission` / `GET /question` must be scoped with
        // `?directory=` or they only see the server cwd instance. Check every
        // known session directory.
        let directories = { handles.sessions.store.lock().await.directories() };
        let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
        // (request id, session id) pairs for the liveness record (ADR-0054);
        // auto-resolved requests are filtered out when the record is written.
        let mut wait_candidates: Vec<(String, String)> = Vec::new();
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
        let what = format!("poll {}", self.kind.label());
        for dir in &directories {
            let backend = handles.backend.clone().for_directory(dir);
            // #421: a directory whose last list failed while the path was
            // already gone from disk can never list again — the server boots
            // the directory's instance on every request and `realPath` fails
            // on a missing path — so the call is skipped until the path
            // exists again. It still counts as a FAILED directory: unknown is
            // never read as resolved (#130). The check runs AFTER a failure,
            // not before every call: ADR-0046 rejected per-row filesystem
            // checks for the `/dir` card, and here one stat replaces the next
            // tick's doomed HTTP call.
            if self.dead_dirs.lock().await.contains(dir) {
                if !std::path::Path::new(dir).exists() {
                    failed_dirs.insert(dir.clone());
                    continue;
                }
                // The path is back: poll it again (a failure re-marks it if
                // the path is gone once more).
                self.dead_dirs.lock().await.remove(dir);
            }
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
                    // `bounded_call` already logged this one line, and unlike
                    // the structural 500 below a timeout is transient — the
                    // next tick retries and the connection may heal.
                    failed_dirs.insert(dir.clone());
                    continue;
                }
            };
            match listed {
                Ok(requests) => {
                    self.list_failures.lock().await.succeeded(dir, &what);
                    listed_now.insert(dir.clone());
                    for req in &requests {
                        pending.insert(req.id().to_string());
                        // A Location Form (ADR-0103) has no Session by
                        // construction: its owner is the directory, and the
                        // adapter already cleared the wire's session
                        // placeholder. It never joins the per-session liveness
                        // set, the Instant Reminder, or the inline-host walk —
                        // only the message-pin registry, later.
                        let location_form = req.is_location();
                        // #489: a session id that cannot name a session at all
                        // (the `global` sentinel, an empty optional id) is a
                        // structural, terminal condition — no parent chain and
                        // no card target can ever resolve, so every sweep would
                        // pay a doomed session read. Warn once, naming the id,
                        // and skip the request: it is never surfaced, never
                        // pinned, never walked. `pending` still records it, so
                        // unknown is never read as resolved (#130). Only an
                        // UNMARKED session-less request reaches this: a marked
                        // MCP elicitation is a Location Form and takes its own
                        // path below.
                        if !location_form && !handles.backend.is_addressable_session_id(req.session_id()) {
                            let first = self
                                .session_less
                                .lock()
                                .await
                                .insert(req.id().to_string(), dir.clone())
                                .is_none();
                            if first {
                                tracing::warn!(
                                    "{} ({}): {} on session {:?} — not a session id; the request cannot be delivered, skipping it",
                                    self.kind.label(),
                                    dir,
                                    req.id(),
                                    req.session_id()
                                );
                            }
                            continue;
                        }
                        if !location_form {
                            wait_candidates.push((req.id().to_string(), req.session_id().to_string()));
                        }
                        // Both pin surfaces need the listed candidates: the
                        // reminder's chat-level target and the waiting-card
                        // registry's host message. A Location Form is collected
                        // here only for the message pin; the reminder loop below
                        // skips it (it has no Turn to own a reminder).
                        if handles.waits.reminder.enabled() || handles.waits.message_pins.enabled() {
                            pin_candidates.push((req.clone(), dir.clone()));
                        }
                        // One span per request (ADR-0048): its `prepare`, its
                        // re-host and its card delivery are all retrievable by
                        // the session it belongs to. `chat`/`topic` ride along
                        // when the store maps that session — a sub-task child
                        // is not mapped and carries `session` alone.
                        let thread_key = {
                            let store = handles.sessions.store.lock().await;
                            store
                                .entry_for_session(req.session_id())
                                .map(|e| e.thread_key.clone())
                        };
                        let span = crate::bridge::span::request(req.session_id(), thread_key.as_ref());
                        if self
                            .surface(handles, req, dir, seen, &retried_withdrawals)
                            .instrument(span)
                            .await
                        {
                            auto_resolved.insert(req.id().to_string());
                        }
                    }
                }
                Err(e) => {
                    failed_dirs.insert(dir.clone());
                    if std::path::Path::new(dir).exists() {
                        // Transient (or unknown): retry next tick, WARN once
                        // per distinct cause and DEBUG the identical repeats
                        // (ADR-0048).
                        self.list_failures.lock().await.failed(dir, &what, "", e);
                    } else {
                        // Structural: the path is gone, so no retry can
                        // succeed (#421). Stop calling until it returns; the
                        // WARN names the state change.
                        self.dead_dirs.lock().await.insert(dir.clone());
                        self.list_failures.lock().await.failed(
                            dir,
                            &what,
                            "",
                            format!("{e}; the directory is gone — skipping it until it returns"),
                        );
                    }
                }
            }
        }
        // A companion record lives only as long as its request. When every
        // directory listed successfully, `pending` speaks for all of them, so a
        // request absent from it has resolved and its record goes. A failed
        // directory leaves every record alone — its request may still be
        // pending (#130).
        if self.kind.interaction_kind() == super::kind::InteractionKind::Permission && failed_dirs.is_empty()
        {
            self.surfaces.retain_companions(|id| pending.contains(id));
        }
        // Auto-Accept latches live by request id; drop the ones that left the
        // pending list (approved, settled elsewhere, or their directory failed
        // to list — a directory that returns may WARN again, which is a fresh
        // failure, not a repeat).
        self.autoaccept_failures
            .lock()
            .await
            .retain(|id| pending.contains(id));
        let waiting_sessions: std::collections::HashSet<String> = wait_candidates
            .iter()
            .map(|(_, session)| session.clone())
            .collect();
        self.autoaccept_read_failures
            .lock()
            .await
            .retain(|session_id| waiting_sessions.contains(session_id));
        // ADR-0054: record which sessions wait on this kind, so the task
        // liveness line reads it without listing pending requests per render
        // poll. Only a complete sweep may replace the record — a failed
        // directory said nothing, and unknown is never read as resolved (#130),
        // the reminder's own rule below.
        if failed_dirs.is_empty() {
            let waiting: std::collections::HashSet<String> = wait_candidates
                .iter()
                .filter(|(id, _)| !auto_resolved.contains(id))
                .map(|(_, session)| session.clone())
                .collect();
            *self.pending_sessions.lock().await = waiting;
        }
        // Instant Reminder (ADR-0043): reconcile this kind's pending requests
        // with the pin state. A sweep where any directory failed must not
        // move it — that directory said nothing, and unknown is never read as
        // resolved (#130) — so the pin survives until a complete sweep. A
        // request auto-resolved in this sweep (prepare said handled) is not a
        // wait, so it never pins.
        if handles.waits.reminder.enabled() && failed_dirs.is_empty() {
            let mut pin_targets: Vec<crate::bridge::reminder::ReminderTarget> = Vec::new();
            for (req, dir) in &pin_candidates {
                if auto_resolved.contains(req.id()) {
                    continue;
                }
                // A Location Form has no Turn and no requester: it never owns
                // an Instant Reminder (ADR-0103), only a Message Pin.
                if req.is_location() {
                    continue;
                }
                // `None` (an external turn, or a request pending across a
                // restart) cannot be pinned — there is no requester to target.
                if let Some(target) = crate::bridge::reminder::reminder_target(
                    &handles.sessions,
                    &handles.cards,
                    &handles.backend,
                    req.session_id(),
                    dir,
                )
                .await
                {
                    pin_targets.push(target);
                }
            }
            handles
                .waits
                .reminder
                .sync(&handles.platform, self.kind.interaction_kind(), &pin_targets)
                .await;
        }
        // Requests cola itself is answering or has answered (a click's claim,
        // an auto-accept approval): their block belongs to the settlement
        // writing the true receipt, so the sweep must not read their
        // disappearance from the pending list as another client's work.
        // Snapshot once — every pass below must judge the same moment.
        let cola_claimed = handles.requests.claimed_requests().await;
        // Card handles (ADR-0038, rule 2): a live block on a card whose
        // accumulator is gone (a replaced/aborted turn) is repainted from the
        // cached JSON — the accumulator pass below only reaches the card its
        // own flush would repaint. `flush_owned` maps each accumulator-owned
        // block to that card, so a block whose handle already names it is left
        // to the flush (never resolved twice), while one whose handle names an
        // older card still gets that stale card repainted.
        let flush_owned = Turn::flush_owned_blocks(&handles.cards).await;
        // Classify the sessions the vanished blocks belong to BEFORE resolving
        // them: a request that left the pending list while its run is over
        // (an interruption, a location eviction, a restart) was never answered
        // — the neutral "another client handled it" line would claim a
        // decision nobody made. The candidates are gathered across all three
        // surfaces first, so one sweep reads each session's status at most
        // once, and a sweep with nothing vanished makes no status call at all.
        let stale_cards = self
            .stale_standalone_cards(&handles.requests, &pending, &directories, &failed_dirs)
            .await;
        let mut candidates: HashMap<String, String> = HashMap::new();
        for (_, card) in &stale_cards {
            // An empty id lands in the map but `dead_run_sessions` skips it:
            // UNKNOWN, classified as live (the neutral copy).
            candidates
                .entry(card.session_id.clone())
                .or_insert(card.directory.clone());
        }
        for (session_id, directory) in handles.cards.card_handles.lock().await.vanished_sessions(
            self.kind.interaction_kind(),
            &pending,
            &failed_dirs,
            &cola_claimed,
            &flush_owned,
        ) {
            candidates.entry(session_id).or_insert(directory);
        }
        for (session_id, directory) in Turn::vanished_block_sessions(
            &handles.cards,
            self.kind.interaction_kind(),
            &pending,
            &failed_dirs,
            &cola_claimed,
        )
        .await
        {
            candidates.entry(session_id).or_insert(directory);
        }
        let dead = self.dead_run_sessions(handles, &candidates).await;
        // Mark stale: a card cola sent whose request is no longer pending and
        // was NOT answered by cola. The stale copy is classified the same way
        // (the card's own session). A card owned by a directory whose list
        // failed stays live (#144).
        mark_stale_cards(
            &handles.platform,
            stale_cards,
            &self.sent_cards,
            &self.surfaces,
            self.kind.label(),
            &dead,
            seen,
        )
        .await;
        let dropped = handles.cards.card_handles.lock().await.drop_vanished(
            self.kind.interaction_kind(),
            &pending,
            &failed_dirs,
            &cola_claimed,
            &flush_owned,
            |session_id, target| vanished_receipt(session_id, target, &dead),
        );
        for (message_id, card) in dropped {
            if let Err(e) = handles.platform.update_message(&message_id, &card).await {
                tracing::warn!("vanished block repaint failed on {}: {}", message_id, e);
            } else {
                tracing::info!(
                    "{} block repainted from its card handle: {}",
                    self.kind.label(),
                    message_id
                );
            }
        }
        // Resolve inline blocks whose request vanished into their Interaction
        // Receipts and repaint the cards that host them — the receipt lands
        // within this sweep, with no reliance on the render tick (which stops
        // when a turn ends). Blocks owned by a failed directory stay (#144).
        let repaint = self
            .kind
            .resolve_vanished_inline(&handles.cards, &pending, &failed_dirs, &cola_claimed, &dead)
            .await;
        for session_id in &repaint {
            Turn::flush_card(&handles.cards, session_id).await;
        }
        // Waiting-card pins (ADR-0043 amendment): keep every card still
        // waiting on the user in its chat's pinned-message list, so the
        // Chat/Topic's Instant Reminder nudge leads to the exact message once
        // the user opens the chat. The host is the card rendering the live
        // block (the streaming card, possibly a re-hosted newer one) or the
        // standalone card the request was sent as. A request cola is already
        // answering or one auto-resolved in this sweep is not a wait and never
        // pins. Best-effort, like the reminder; a failed directory's tracked
        // pin stays (#130).
        if handles.waits.message_pins.enabled() {
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
                let handles = handles.cards.card_handles.lock().await;
                pin_candidates
                    .iter()
                    .filter(|(req, _)| !auto_resolved.contains(req.id()) && !cola_claimed.contains(req.id()))
                    .filter_map(|(req, dir)| {
                        let message_id = handles
                            .message_of(req.id())
                            .map(str::to_string)
                            .or_else(|| sent_ids.get(req.id()).cloned())?;
                        Some(crate::bridge::message_pins::WaitingCard {
                            request_id: req.id().to_string(),
                            directory: dir.clone(),
                            message_id,
                        })
                    })
                    .collect()
            };
            handles
                .waits
                .message_pins
                .sync(
                    &handles.platform,
                    self.kind.interaction_kind(),
                    &waiting,
                    &failed_dirs,
                )
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
        // #421: a dead directory whose mapping was removed from the store (it
        // was forgotten) is dropped with it; a still-mapped one stays dead
        // until its path returns. The failure latch is pruned the same way: a
        // forgotten condition must not suppress the next WARN, and neither map
        // grows with directories no longer watched.
        {
            self.dead_dirs
                .lock()
                .await
                .retain(|dir| directories.contains(dir));
            self.list_failures
                .lock()
                .await
                .retain(|dir| directories.iter().any(|mapped| mapped == dir));
            self.target_walk_failures
                .lock()
                .await
                .retain(|dir| directories.iter().any(|mapped| mapped == dir));
            self.companion_walk_failures
                .lock()
                .await
                .retain(|dir| directories.iter().any(|mapped| mapped == dir));
        }
        // #130: forget question state whose request left the pending list —
        // the flip side of the stale-card / inline / claim cleanups above.
        // A directory that failed to list is retained: we do not know it
        // resolved, and unknown must never be read as resolved.
        self.question_state
            .lock()
            .await
            .retain(|id, state| pending.contains(id) || failed_dirs.contains(state.dir()));
        // The recovered-surface memory is one-shot: an entry the sweep did not
        // list (the request was resolved while cola was down) has no card left
        // to re-adopt — its surfaces were reconciled by the stale/inline
        // cleanups above. A failed directory's entries stay: it said nothing
        // (#130).
        self.recovered
            .lock()
            .await
            .retain(|id, dir| pending.contains(id) || failed_dirs.contains(dir));
        // ADR-0103: a Location Form's push/dismissal record is pruned only when
        // every mapped directory listed, so a failed list never reads the form
        // as gone. Only the Question flow owns Location Forms: the Permission
        // flow's `pending` never contains their ids, so an ungated prune would
        // delete every decision on each healthy permission tick. A record whose
        // directory is no longer mapped is retained too: the directory may
        // return still holding the suppressed form, and a re-push would undo
        // the Host's 「稍后」. An empty directory is not a mapped one and is
        // pruned with the request, so it cannot be retained forever.
        if self.kind.interaction_kind() == InteractionKind::Question && failed_dirs.is_empty() {
            use crate::bridge::surfaces::LocationFormRetention;
            let pruned = self.surfaces.retain_location_forms(|id, record| {
                if pending.contains(id) {
                    record.missing_sweeps = 0;
                    return LocationFormRetention::Keep;
                }
                if !record.directory.is_empty() && !directories.contains(&record.directory) {
                    // The directory is no longer mapped, so this sweep cannot
                    // speak for the form; neither count nor prune it.
                    return LocationFormRetention::Keep;
                }
                record.missing_sweeps = record.missing_sweeps.saturating_add(1);
                if record.missing_sweeps >= LOCATION_FORM_OMISSION_GRACE_SWEEPS {
                    LocationFormRetention::Prune
                } else {
                    LocationFormRetention::Keep
                }
            });
            // A dismissed form has NO standalone card, so this prune is the
            // only place that can reconcile its push-once `seen` entry. Left
            // in `seen`, the id would short-circuit `surface_location_form`
            // before its (now absent) record check and the re-listed form could
            // never be pushed again without a restart (ADR-0103).
            for id in pruned {
                seen.remove(&id);
            }
        }
        // The per-form write locks are keyed by the same ids as the records:
        // forget one once neither a pending form nor a retained record names it
        // (each map here is pruned, and this one must not grow unbounded).
        {
            let mut locks = self.location_locks.lock().await;
            locks.retain(|id, _| pending.contains(id) || self.surfaces.location_form(id).is_some());
        }
        // #489: the same rule for the session-less registry — a request that
        // left the pending list, or a directory that said nothing, keeps its
        // entry until a complete sweep can speak for it.
        self.session_less
            .lock()
            .await
            .retain(|id, dir| pending.contains(id) || failed_dirs.contains(dir));
    }

    /// Read `session_id`'s transcript once and render it into its live card
    /// before a request block is inserted (issue #1023), returning the asking
    /// tool part's server time — the Interaction Receipt's server-time cut.
    ///
    /// Bounded by the flow's read timeout twice (the transcript read and the
    /// render pass): the pass also reads the server (title, context window,
    /// child liveness), and one wedged call must not park the sweep. A timeout
    /// or failure is not fatal: the request surfaces exactly as before, without
    /// the anchor. The parts render before the pass's other stages, so a timed
    /// out pass normally still landed the text this drain exists for.
    async fn drain_and_anchor(&self, handles: &FlowHandles, session_id: &str) -> Option<i64> {
        // A finished/idle card is no host (ADR-0100): nothing will render into
        // it, and the sweep is about to fall through to the standalone path.
        if !Turn::is_running(&handles.cards, session_id).await {
            return None;
        }
        let timeout_ms = self
            .list_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(SURFACING_DRAIN_TIMEOUT_MS);
        let read = crate::bridge::bounded_call(
            &format!("{} surfacing drain ({session_id})", self.kind.label()),
            timeout_ms,
            handles.backend.transcript(session_id),
        )
        .await;
        let transcript = match read {
            Some(Ok(transcript)) => transcript,
            Some(Err(error)) => {
                tracing::warn!("{} surfacing drain transcript: {}", self.kind.label(), error);
                return None;
            }
            // `bounded_call` already logged the timeout.
            None => return None,
        };
        let asked_at = asking_anchor(&transcript);
        // The pass is BOUNDED, but the write it owes is not (ADR-0100: a card
        // create carries no idempotency key, so a timeout must never abandon
        // one). The pass therefore renders only, and the flush below — outside
        // the bound — carries whatever it applied. The progress mark is the
        // "did this pass render anything" signal (#457): it advances as each
        // stage applies, so a pass the timeout abandoned half way still owes
        // its flush, while a pass that found nothing new owes no card write at
        // all (the surfacing insert's own flush follows immediately).
        let mark_before = Turn::progress_mark(&handles.cards, session_id).await;
        if tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            Turn::render_and_flush_draining(
                &handles.cards,
                &handles.sessions,
                &handles.backend,
                &handles.requests,
                session_id,
                &transcript,
            ),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                "{} surfacing drain render timed out after {timeout_ms}ms",
                self.kind.label()
            );
        }
        if Turn::progress_mark(&handles.cards, session_id).await != mark_before {
            // The helper re-checks the live-form freeze under the write lock
            // (#917): a question that landed since the apply refuses the write,
            // and its own resolution flush carries the content instead.
            Turn::flush_card_unless_typed_answer(&handles.cards, session_id).await;
        }
        asked_at
    }

    /// Everything one sweep does with ONE listed request, wrapped by the
    /// caller in that request's session span (ADR-0048): the re-host for a
    /// request already surfaced, the companion refusal (a delivery that failed),
    /// the kind's `prepare` (auto-accept / remember), the companion delivery,
    /// and the card delivery — inline on the session's live card, else a
    /// standalone card.
    /// Returns whether the request was handled without a card (a refusal or an
    /// auto-accept), which the sweep must exclude from its reminder/pin
    /// reconciliation.
    ///
    /// `seen` records the requests this process has surfaced: a re-host needs
    /// the record (`seen.contains`), a first surfacing inserts it — so the
    /// poller never surfaces one request twice. The insertion happens only
    /// where the request ACTUALLY reached a surface: a refusal only once the
    /// server accepted it, an inline block only once it landed, a standalone
    /// card only once the send returned an id. A failed send — including "no
    /// reply target or chat yet", which is exactly an unmapped session the user
    /// has not adopted (ADR-0100: nothing claims its pending any more) — leaves
    /// it unmarked so the next sweep retries; marking it early would bury the
    /// request forever because every later sweep would treat it as surfaced and
    /// `rehost_block` has no card to re-host it on.
    ///
    /// The inline/standalone decision holds ONE Session's card write lock at a
    /// time (ADR-0038's 2026-11-07 amendment, ADR-0100). The host is resolved
    /// before any lock; the inline path then takes the HOST's lock and
    /// re-checks its liveness before inserting/flushing. If it is no longer
    /// live, that lock is released and the standalone path takes the REQUEST
    /// Session's own lock for the decision, send and `sent_cards` record. The
    /// two are never nested. `/card`'s post-and-arm takes the same host lock,
    /// so the two cannot both deliver one request: whichever runs second sees
    /// the first's effect (a live card carrying the block, or a recorded
    /// standalone).
    #[allow(clippy::too_many_arguments)] // the sweep context: flow + request + seen/retried
    async fn surface(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
        seen: &mut std::collections::HashSet<String>,
        retried_withdrawals: &std::collections::HashSet<String>,
    ) -> bool {
        if seen.contains(req.id()) {
            tracing::debug!(
                "{} {}: already surfaced; re-hosting on the current card",
                self.kind.label(),
                req.id()
            );
            // Already surfaced: if it outlived the turn that hosted it,
            // re-host it onto the session's current card so the live controls
            // follow the newest card (ADR-0038, rule 1).
            self.rehost_block(handles, req, dir).await;
            return false;
        }
        // A withdrawal this flow already decided but could not deliver is
        // replayed with its original payload: re-deciding now could mislabel the
        // card (the request may already be settled, or another client may have
        // answered it). A `Refused`/`ResolvedElsewhere` retry that succeeds
        // settles the request; a `Retrying` or `Superseded` one must still fall
        // through to the path it belongs to (the refusal retry, or the fresh
        // surfacing path after an unbacked card was withdrawn).
        if !retried_withdrawals.contains(req.id())
            && let Some(withdrawal) = self.pending_withdrawals.lock().await.get(req.id()).cloned()
        {
            let outcome = self.apply_stored_withdrawal(handles, req.id(), &withdrawal).await;
            if outcome != WithdrawalOutcome::Failed {
                self.pending_withdrawals.lock().await.remove(req.id());
                if withdrawal.how == Withdrawal::Superseded {
                    // The old card is neutral now; fall through to the fresh
                    // path (prepare/Auto-Accept, companion, or a new card).
                } else if withdrawal.how != Withdrawal::Retrying {
                    // The settled withdrawal finally landed (and its companion
                    // proof was already forgotten by apply_stored_withdrawal).
                    seen.insert(req.id().to_string());
                    return true;
                }
            } else if withdrawal.how != Withdrawal::Retrying {
                // A settled decision whose card still cannot be patched stays
                // retryable and is never offered.
                return false;
            }
        }
        // A withdrawal the retry phase already claimed this sweep is not
        // re-attempted here (the budget would be defeated) and, if it is still
        // pending, must block surfacing: the old card may still be live.
        if retried_withdrawals.contains(req.id())
            && self.pending_withdrawals.lock().await.contains_key(req.id())
        {
            return false;
        }
        // ADR-0038 restart re-adoption: a request whose card a PREVIOUS
        // process persisted is already surfaced. Re-adopt that card — remember
        // what its buttons need to resolve, repaint it — instead of posting a
        // second one. The entry is one-shot: it exists exactly for the first
        // sweep that meets the request after the restart. A recovered card
        // that cannot be repainted falls through to the normal path, which
        // surfaces the request as a standalone card.
        if self.recovered.lock().await.remove(req.id()).is_some() {
            match self.readopt_surface(handles, req, dir).await {
                ReadoptDecision::Adopted => {
                    seen.insert(req.id().to_string());
                    return false;
                }
                // The withdrawal PATCH failed: the recovered entry is kept and
                // the next sweep retries it; nothing else may be offered.
                ReadoptDecision::Retry => return false,
                ReadoptDecision::NotAdopted => {}
            }
        }
        // Kind-specific pre-card handling (auto-accept / remember). `Handled`
        // → no card needed; `Retry` → the handling failed and the request is
        // still pending, so it must stay unmarked and the next sweep retries.
        // Only a handled request joins `seen`; a retried one must never be
        // retired from the sweep while no card exists.
        match self.kind.prepare(self, handles, req, dir).await {
            Prepared::Handled => {
                seen.insert(req.id().to_string());
                return true;
            }
            Prepared::Retry => return false,
            Prepared::NotHandled => {}
        }
        // A Location Form (ADR-0103) has no Session to inline onto and no
        // per-session pending/reminder surface: it is pushed ONCE as its own
        // card into the directory's newest mapped Chat/Topic. Everything below
        // this point (companion, inline host, per-session send) assumes a
        // Session and must not run for it.
        if req.is_location() {
            return self.surface_location_form(handles, req, dir, seen).await;
        }
        // A request that now fits the card has no companion surface: forget a
        // stale delivery record so an ordinary allow is not mistaken for a
        // summary surface (and bounced into a fallback card after a shrink).
        if let PendingRequest::Permission(p) = req
            && permission_overflow(p).is_none()
        {
            self.surfaces.remove_companion(req.id());
        }
        // An over-budget permission's full content must land BEFORE the inline
        // block or standalone card becomes interactive (ADR-0102). Auto-Accept
        // returned above, so a delegated session still renders nothing. When
        // the companion cannot be delivered — a definite failure, or an
        // uncertain outcome that is not a proof — the request is NOT refused:
        // a standalone fallback card names the risk and offers a two-step
        // approval (#1013). It is built below, after any stale block has been
        // withdrawn, and is always a standalone surface so its click ack can
        // replace it without touching a streaming card.
        let mut fallback_summary: Option<String> = None;
        let mut fallback_cause: Option<DeclineReason> = None;
        let mut fallback_loss = crate::feishu::card::question::FallbackLoss::NotDelivered;
        if let PendingRequest::Permission(p) = req
            && let Some(overflow) = permission_overflow(p)
            && self.companion_for(req).is_none()
        {
            match ensure_permission_companion(
                handles,
                &self.surfaces,
                p,
                dir,
                self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
                &self.companion_walk_failures,
            )
            .await
            {
                CompanionOutcome::Delivered => {}
                CompanionOutcome::Uncertain => {
                    fallback_cause = Some(DeclineReason::CompanionUncertain);
                    fallback_loss = crate::feishu::card::question::FallbackLoss::OutcomeUnknown;
                    fallback_summary = Some(crate::feishu::card::question::fallback_permission_summary(
                        &describe_permission_target_summary(p),
                        fallback_loss,
                        false,
                    ));
                }
                CompanionOutcome::Failed => {
                    fallback_cause = Some(DeclineReason::Overflow(overflow));
                    fallback_summary = Some(crate::feishu::card::question::fallback_permission_summary(
                        &describe_permission_target_summary(p),
                        fallback_loss,
                        false,
                    ));
                }
            }
        }
        // The surfacing line is emitted only for a request this pass actually
        // offers: a `Retry` above returns before it, so a repeatedly failing
        // auto-accept does not re-announce the same request on every tick.
        tracing::info!(
            "{} ({}): {} on session {}",
            self.kind.label(),
            dir,
            req.id(),
            req.session_id()
        );
        // One-card-per-turn: surface the request INLINE on the streaming card
        // of the session that owns it — the session itself, or (sub-task
        // children) its nearest ancestor with a live card. Only a separate card
        // when there is no active card (e.g. external turns or restarts).
        //
        // LOCK ORDER (ADR-0038's amendment): never hold two Sessions' card
        // locks. The host is resolved first (bounded); the inline decision then
        // takes the HOST's write lock — the same lock `/card`'s post-and-arm and
        // a busy follow take — re-checks the host's liveness and inserts/flushes
        // under it. If the host is gone, that lock is released before the
        // standalone path takes the REQUEST Session's own lock for its decision,
        // send and `sent_cards` record. A sub-task child's lock is never taken
        // while the ancestor host's is, so the old nested acquisition (and its
        // host-check-under-the-child's-lock race) cannot recur.
        // Resolve the standalone target BEFORE any lock: this walk reads the
        // parent chain and the server's session metadata, which has no card
        // state to serialize with. Bound it by the flow's read timeout too: a
        // half-open `session_info` would otherwise freeze the sweep. A timeout
        // is UNKNOWN, not "no target" — reading it as none would run the
        // last-resort decline — so the request stays pending and unmarked for
        // the next sweep.
        let walk_timeout_ms = self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        let resolved_target = crate::bridge::bounded_call_quiet(walk_timeout_ms, async {
            Ok::<_, crate::error::BridgeError>(
                resolve_card_target(
                    &handles.sessions,
                    &handles.cards,
                    &handles.backend,
                    &handles.platform,
                    req.session_id(),
                    dir,
                )
                .await,
            )
        })
        .await;
        let walk_condition = format!("{} target walk", self.kind.label());
        let Some(resolved_target) = resolved_target else {
            self.target_walk_failures.lock().await.failed(
                dir,
                &walk_condition,
                "",
                format!("timed out after {walk_timeout_ms}ms"),
            );
            return false;
        };
        self.target_walk_failures
            .lock()
            .await
            .succeeded(dir, &walk_condition);
        let target = resolved_target.ok().flatten();
        // A fallback card needs somewhere to land. With no reply target and no
        // chat, no card can ever show the risk, so the request must not stay
        // pending invisibly: keep the old refusal path as the last resort and
        // log why the fallback could not be offered.
        if fallback_summary.is_some() && target.is_none() {
            tracing::warn!(
                "{} {}: no reply target or chat for the fallback card; following the refusal path",
                self.kind.label(),
                req.id()
            );
            if let PendingRequest::Permission(p) = req
                && let Some(reason) = fallback_cause
            {
                let handled = self.settle_decline(handles, p, dir, reason).await;
                if handled {
                    seen.insert(req.id().to_string());
                }
                return handled;
            }
        }
        // Resolve the inline host BEFORE the lock too, and BOUND it by the
        // flow's read timeout: the parent-chain walk calls `session_info`,
        // which has no timeout of its own, so a half-open connection would
        // otherwise hold a card write lock indefinitely — blocking flushes,
        // `/card`, install and the rest of that sweep. On timeout the request
        // falls through to the standalone path (retried next tick if no card
        // lands). The liveness check is re-run under the HOST's lock below, so
        // a `/card` read or a follow that committed in this window wins
        // instead of letting the insert land on a card that stopped being live.
        let resolved_host = crate::bridge::bounded_call(
            &format!("{} inline host ({})", self.kind.label(), req.session_id()),
            self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            async {
                Ok::<_, crate::error::BridgeError>(
                    inline_host_session(&handles.cards, &handles.backend, req.session_id(), Some(dir)).await,
                )
            },
        )
        .await
        .and_then(Result::ok)
        .flatten();
        // A fallback card is standalone: withdraw any block a previous pass
        // already inserted for this request before posting it, so exactly one
        // surface carries the controls (ADR-0102 / #1013).
        if fallback_summary.is_some()
            && !self
                .withdraw_block_for_fallback(handles, req, resolved_host.as_deref())
                .await
        {
            // The old block could not be neutralized: do not offer a second
            // surface beside it. Leaving the request unmarked lets the next
            // sweep retry the withdrawal.
            return false;
        }
        // Issue #1023: the request sweep can see a pending request before the
        // session's render loop has written the assistant text that precedes
        // it. Inserting the block first freezes the card (any repaint would
        // wipe an unsubmitted answer, issue #917), so that text would stay
        // held back until after the answer — the Host is asked to choose
        // blind. Read the session ONCE and render it BEFORE the block lands,
        // so the explanation is on the card when the question appears. The
        // same read yields the asking tool part's server time, carried on the
        // block as its Interaction Receipt's server-time cut.
        //
        // Bounded and best-effort: a failed or timed-out read falls back to
        // the old order and the request still surfaces. Only the request's
        // OWN session is drained — an ancestor host renders no part of a
        // child's transcript, and a standalone card has no live card to drain.
        let asked_at = if fallback_summary.is_some() {
            None
        } else {
            let drain = match resolved_host.as_deref() {
                Some(host) if host == req.session_id() => Some(host),
                Some(_) => None,
                None => Some(req.session_id()),
            };
            match drain {
                Some(session_id) => self.drain_and_anchor(handles, session_id).await,
                None => None,
            }
        };
        // Inline path: take the HOST's write lock — never the request
        // Session's while also holding the host's, and never the reverse. A
        // sub-task child has no card of its own, so its lock would serialize
        // nothing with `/card` or the follow (which take the host's); the
        // host's lock is what makes the liveness check and the insert one
        // critical section (ADR-0038's amendment, ADR-0100).
        if fallback_summary.is_none()
            && let Some(host) = resolved_host.as_deref()
        {
            let host_lock = handles.cards.write_lock(host).await;
            let host_guard = host_lock.lock().await;
            // Re-check under the host's lock: the parent may have ended and
            // `/card` installed an Idle card during the bounded host walk. An
            // insert must never land on a card that stopped being live.
            if Turn::is_running(&handles.cards, host).await {
                let pushed = self
                    .kind
                    .add_inline_locked(self, &handles.cards, host, req, dir, asked_at)
                    .await;
                if pushed {
                    // The block landed: this request is surfaced from now on.
                    seen.insert(req.id().to_string());
                    tracing::info!(
                        "{} {} inlined on session {} card",
                        self.kind.label(),
                        req.id(),
                        host
                    );
                    // Flush so the inline section appears NOW — the render loop
                    // only flushes on new parts, and a blocked prompt produces
                    // none.
                    Turn::flush_card_locked(&handles.cards, host).await;
                    return false;
                }
                // `add_inline_locked` false is ambiguous: the card session
                // vanished, OR it already carries the block. A busy follow
                // seeds its read's pendings without the sweep's `seen`, so the
                // latter is the normal case for a request `/card` just put on
                // the card — falling through here would post a duplicate
                // standalone card. Only a card that still carries the request
                // is "already surfaced"; a vanished one still needs its own.
                if Turn::has_interaction_in(&handles.cards, host, req.id()).await {
                    seen.insert(req.id().to_string());
                    tracing::info!(
                        "{} {} already carried by session {} card",
                        self.kind.label(),
                        req.id(),
                        host
                    );
                    return false;
                }
            }
            // The host stopped being live (or its card vanished mid-pass):
            // release it and let the request surface on its own card. Nothing
            // has been inserted or repainted.
            drop(host_guard);
        }
        // Standalone path: the request Session's OWN write lock covers its
        // decision, send and surface record. The host's lock is already
        // released here — the two are never nested.
        let owning_lock = handles.cards.write_lock(req.session_id()).await;
        let guard = owning_lock.lock().await;
        // A card may have appeared on the request Session itself during the
        // walk (a follow that committed after host resolution). Re-check under
        // its own lock so the standalone send cannot duplicate a block that
        // just landed there. A fallback stays standalone by construction.
        if fallback_summary.is_none() && Turn::is_running(&handles.cards, req.session_id()).await {
            let pushed = self
                .kind
                .add_inline_locked(self, &handles.cards, req.session_id(), req, dir, asked_at)
                .await;
            if pushed {
                seen.insert(req.id().to_string());
                tracing::info!(
                    "{} {} inlined on session {} card",
                    self.kind.label(),
                    req.id(),
                    req.session_id()
                );
                Turn::flush_card_locked(&handles.cards, req.session_id()).await;
                return false;
            }
            if Turn::has_interaction_in(&handles.cards, req.session_id(), req.id()).await {
                seen.insert(req.id().to_string());
                tracing::info!(
                    "{} {} already carried by session {} card",
                    self.kind.label(),
                    req.id(),
                    req.session_id()
                );
                return false;
            }
        }
        let card = match (&fallback_summary, req) {
            (Some(summary), PendingRequest::Permission(p)) => {
                crate::feishu::card::question::build_fallback_permission_card(
                    p.session_id.as_deref().unwrap_or(""),
                    &p.request_id,
                    dir,
                    summary,
                    p.always_supported,
                    patterns_target_digest(p),
                    fallback_loss,
                )
            }
            _ => self.kind.build_card(self, req, dir),
        };
        // Reply to the message that triggered the prompt for this session; fall
        // back to sending into the chat when the accumulator is gone (e.g.
        // after a cola restart). Sub-task sessions resolve up the parent chain;
        // the target was resolved before the lock. The send and the surface
        // record stay under the owning Session's lock so no follow can commit
        // between them.
        let summary = fallback_summary
            .clone()
            .unwrap_or_else(|| self.kind.summary(self, req));
        if self
            .send_standalone_surface(handles, req, dir, target, &card, &summary)
            .await
        {
            // The card landed: this request is surfaced from now on. A failed
            // or impossible send stays unmarked so the next sweep retries it
            // (ADR-0100).
            seen.insert(req.id().to_string());
        }
        drop(guard);
        false
    }

    /// Push one **Location Form** as its own card (ADR-0103). The card goes to
    /// the directory's newest mapped Chat/Topic — never inline, never into a
    /// per-session set. Delivery outcomes:
    ///
    /// - no mapped Chat/Topic, or a topic whose anchor cannot be resolved:
    ///   nothing is sent and nothing suppressed, so the next complete sweep
    ///   retries;
    /// - a failed or timed-out send: not recorded, retried next sweep (a
    ///   timed-out send may duplicate, exactly like every standalone send);
    /// - success: recorded in `sent_cards`/the surface file, and `seen`, so
    ///   this process never pushes the same form twice.
    async fn surface_location_form(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
        seen: &mut std::collections::HashSet<String>,
    ) -> bool {
        // Push-once: the poll loop's `seen` survives its sweeps, the sent-card
        // registry survives a fresh `seen` (tests), and the durable record
        // survives a restart. A dismissed form is deliberately surface-less
        // (ADR-0038's 2026-11-09 amendment): its persisted decision is
        // authoritative, so no sweep re-posts it.
        if seen.contains(req.id()) || self.sent_cards.lock().await.contains_key(req.id()) {
            return false;
        }
        if self.surfaces.location_form(req.id()).is_some() {
            return false;
        }
        let Some(target) = self.resolve_location_target(handles, req, dir).await else {
            return false;
        };
        let card = self.location_form_card(req, dir).await;
        let summary = self.kind.summary(self, req);
        if self
            .send_standalone_surface(handles, req, dir, Some(target), &card, &summary)
            .await
        {
            // The card just sent is built from the remembered state; persist
            // that state beside its new record so a restart restores it (the
            // recovered card is not repainted — ADR-0103). This also covers a
            // re-surface after the persisted card was deleted, where the
            // partial entry was captured into `QuestionState` before the old
            // record was removed.
            self.persist_location_form_partial(req.id()).await;
            tracing::info!(
                "{} ({}): {} delivered as a directory-owned form card",
                self.kind.label(),
                dir,
                req.id()
            );
            seen.insert(req.id().to_string());
        }
        false
    }

    /// Resolve a Location Form's delivery target, bounded like every sweep
    /// read. `None` means "send nothing and suppress nothing": no mapped
    /// Chat/Topic, an unresolved topic anchor, or an unknown walk result all
    /// retry next sweep.
    async fn resolve_location_target(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
    ) -> Option<CardTarget> {
        // The walk reaches Feishu's `list_messages` (a topic's anchor fallback)
        // and a half-open call must not wedge the whole request sweep. A
        // timeout is UNKNOWN, so the push is deferred and retried next sweep.
        let timeout_ms = self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed);
        let walk_condition = format!("{} location target", self.kind.label());
        let resolved = crate::bridge::bounded_call(
            &format!("{} {} location target ({})", self.kind.label(), req.id(), dir),
            timeout_ms,
            async {
                Ok::<_, crate::error::BridgeError>(
                    resolve_location_form_target(&handles.sessions, &handles.platform, dir).await,
                )
            },
        )
        .await;
        let Some(resolved) = resolved else {
            self.target_walk_failures.lock().await.failed(
                dir,
                &walk_condition,
                "",
                format!("timed out after {timeout_ms}ms"),
            );
            return None;
        };
        self.target_walk_failures
            .lock()
            .await
            .succeeded(dir, &walk_condition);
        match resolved {
            Ok(LocationTarget::Target(target)) => Some(target),
            Ok(LocationTarget::NoMapping) => {
                tracing::debug!(
                    "{} {}: directory {} has no mapped Chat/Topic; nothing sent",
                    self.kind.label(),
                    req.id(),
                    dir
                );
                None
            }
            Ok(LocationTarget::Deferred) => {
                tracing::debug!(
                    "{} {}: directory {}'s topic anchor could not be resolved; deferring the push",
                    self.kind.label(),
                    req.id(),
                    dir
                );
                None
            }
            Err(e) => {
                // The target walk only reports Ok today; a future failure is
                // unknown, never "no target".
                tracing::warn!(
                    "{} {}: location target walk failed ({}); deferring the push",
                    self.kind.label(),
                    req.id(),
                    e
                );
                None
            }
        }
    }

    /// Reopen a dismissed Location Form: send the fresh card **first**, then
    /// clear the dismissal and record the new standalone surface in one
    /// mutation. A failed send leaves the form dismissed and retryable, so the
    /// two records cannot disagree.
    ///
    /// The dismissal generation is read under the per-form write lock (shared
    /// with 「稍后」) and released for the send; the commit clears only that
    /// generation, so a dismissal that lands during the send survives. The
    /// fresh card is withdrawn again in that race. `/mcp`'s reopen control is
    /// the caller.
    pub(crate) async fn reopen_location_form(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
    ) -> ReopenOutcome {
        let id = req.id();
        let lock = self.location_form_lock(id).await;
        // Read the generation this reopen may clear, under the lock, and claim
        // the in-flight slot. The send happens outside the lock: a network
        // call must not serialize every 「稍后」 click behind it, and the
        // generation check is what makes that safe. The claim's Drop releases
        // the slot on every exit, including a cancelled task.
        let (_claim, generation) = {
            let _guard = lock.lock().await;
            let Some(claim) = ReopenClaim::claim(&self.location_reopens, id) else {
                return ReopenOutcome::AlreadyReopening;
            };
            match self.surfaces.location_form(id) {
                Some(record) if record.is_dismissed() => (claim, record.dismissal_generation),
                _ => return ReopenOutcome::NotDismissed,
            }
        };
        // Restore the partial answer state BEFORE building the card: after a
        // restart this is the first time the reopen has seen the form, and the
        // fresh card must paint what the restored state will submit.
        self.kind.remember_surfaced(self, req, dir).await;
        let Some(target) = self.resolve_location_target(handles, req, dir).await else {
            return ReopenOutcome::NoTarget;
        };
        // Reopen from the remembered question state (ADR-0103): 「稍后」 never
        // clears the Host's partial answer, so a fresh card that painted
        // `QuestionPaint::blank` would hide selections the hidden state still
        // holds — and the next toggle would act on a selection the card never
        // showed. The post-restart case has no remembered state and stays
        // blank; `readopt_location_form` deliberately does not repaint there,
        // because a repaint would wipe an unanswered typed field (ADR-0038's
        // card-repaints rule).
        let card = self.location_form_card(req, dir).await;
        let summary = self.kind.summary(self, req);
        let Some(message_id) = self.send_standalone_card(handles, req, Some(target), &card).await else {
            tracing::warn!(
                "{} {}: reopen send failed; the form stays dismissed and retryable",
                self.kind.label(),
                id
            );
            return ReopenOutcome::SendFailed;
        };
        let surface = self.standalone_surface(req, dir, message_id.clone(), &summary);
        // Clear the dismissal and register the new surface in ONE lock
        // acquisition: a 「稍后」 either lands before this (mismatched
        // generation, clear refused) or after it (it finds the registered
        // card and withdraws it). It can never slip between the two writes.
        let cleared = {
            let _guard = lock.lock().await;
            let cleared = self
                .surfaces
                .clear_location_form_dismissal(id, generation, surface);
            if cleared {
                self.sent_cards.lock().await.insert(
                    id.to_string(),
                    SentCard {
                        message_id: message_id.clone(),
                        summary: summary.clone(),
                        directory: dir.to_string(),
                        session_id: req.session_id().to_string(),
                    },
                );
            }
            cleared
        };
        if !cleared {
            // A newer dismissal (generation moved) wins; otherwise the clear
            // write itself failed and the rollback left the form dismissed.
            // Either way the fresh card must not stay live over a dismissed
            // record, or `/mcp` would offer a reopen for a card that already
            // exists.
            let lost_to_dismissal = self
                .surfaces
                .location_form(id)
                .is_some_and(|record| record.is_dismissed() && record.dismissal_generation != generation);
            let purpose = if lost_to_dismissal {
                tracing::info!(
                    "{} {}: a dismissal landed during the reopen; withdrawing the fresh card",
                    self.kind.label(),
                    id
                );
                "reopen lost to a dismissal"
            } else {
                tracing::warn!(
                    "{} {}: the cleared-dismissal write failed; withdrawing the fresh card and keeping the form dismissed",
                    self.kind.label(),
                    id
                );
                "reopen clear write failed"
            };
            self.withdraw_location_card(handles, req, &message_id, purpose)
                .await;
            return if lost_to_dismissal {
                ReopenOutcome::LostToDismissal
            } else {
                ReopenOutcome::SendFailed
            };
        }
        tracing::info!(
            "{} ({}): {} reopened as a fresh directory-owned form card",
            self.kind.label(),
            dir,
            id
        );
        ReopenOutcome::Reopened
    }

    /// Test-only: drive the Location Form push decision for one request
    /// directly. The sweep only lists directories it found in the SessionStore,
    /// so the no-mapped-target branch is otherwise reachable only as a
    /// listing→delivery race; this exposes the real surface path so a test can
    /// prove "no send, no suppression" observably.
    #[cfg(test)]
    pub(crate) async fn surface_location_form_for_test(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
        seen: &mut std::collections::HashSet<String>,
    ) -> bool {
        self.surface_location_form(handles, req, dir, seen).await
    }

    /// Send one standalone request card, bounded by [`STANDALONE_SEND_TIMEOUT_MS`].
    /// Returns the message id, or `None` on a failed or timed-out send. No
    /// Session lock is held here: the caller commits the surface record under
    /// the owning Session's write lock, so a slow Feishu call never pins it.
    async fn send_standalone_card(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        target: Option<CardTarget>,
        card: &serde_json::Value,
    ) -> Option<String> {
        let what = format!("{} card send on {}", self.kind.label(), req.session_id());
        let sent = match target {
            Some(CardTarget::ReplyTo(msg_id)) => {
                crate::bridge::bounded_call(
                    &what,
                    STANDALONE_SEND_TIMEOUT_MS,
                    handles.platform.reply_card(&msg_id, card),
                )
                .await
            }
            Some(CardTarget::Chat(chat_id)) => {
                crate::bridge::bounded_call(
                    &what,
                    STANDALONE_SEND_TIMEOUT_MS,
                    handles.platform.send_card("chat_id", &chat_id, card),
                )
                .await
            }
            None => {
                tracing::warn!(
                    "No reply target or chat for {} on session {}",
                    self.kind.label(),
                    req.session_id()
                );
                return None;
            }
        };
        match sent {
            Some(Ok(mid)) => Some(mid),
            Some(Err(e)) => {
                tracing::warn!(
                    "{} card send failed on session {}: {e}",
                    self.kind.label(),
                    req.session_id()
                );
                None
            }
            None => None,
        }
    }

    /// The persisted companion of one standalone card, shared by the push and
    /// reopen paths so the two cannot drift.
    fn standalone_surface(
        &self,
        req: &PendingRequest,
        dir: &str,
        message_id: String,
        summary: &str,
    ) -> StandaloneSurface {
        StandaloneSurface {
            client_id: String::new(),
            kind: self.kind.interaction_kind(),
            message_id,
            summary: summary.to_string(),
            directory: dir.to_string(),
            session_id: req.session_id().to_string(),
        }
    }

    /// Commit one standalone card's surface record. The caller holds the
    /// owning Session's write lock so no follow can commit over it.
    ///
    /// A Location Form's commit takes the per-form lock, and refuses to
    /// overwrite a dismissal that landed while the send was in flight: the
    /// caller then withdraws the fresh card. Returns whether the record was
    /// written (always true for a Session-backed request).
    async fn record_standalone_surface(
        &self,
        req: &PendingRequest,
        dir: &str,
        message_id: String,
        summary: &str,
    ) -> bool {
        let surface = self.standalone_surface(req, dir, message_id.clone(), summary);
        let sent = SentCard {
            message_id,
            summary: summary.to_string(),
            directory: dir.to_string(),
            session_id: req.session_id().to_string(),
        };
        if req.is_location() {
            let lock = self.location_form_lock(req.id()).await;
            let _guard = lock.lock().await;
            if !self.surfaces.set_location_form_pushed(req.id(), surface, dir) {
                return false;
            }
            self.sent_cards.lock().await.insert(req.id().to_string(), sent);
            return true;
        }
        self.sent_cards.lock().await.insert(req.id().to_string(), sent);
        // Persist the standalone surface so a restart re-adopts it instead of
        // posting a second card (ADR-0038 restart re-adoption).
        self.surfaces.set_standalone(req.id(), surface);
        true
    }

    /// Withdraw one already-sent card by PATCHing `replacement` onto it. The
    /// single bounded write both the 「稍后」 control and every "the send lost a
    /// race, do not leave it live" path share. A failed or timed-out PATCH is
    /// logged and reported, never silently dropped.
    async fn withdraw_card(
        &self,
        handles: &FlowHandles,
        req_id: &str,
        message_id: &str,
        replacement: &serde_json::Value,
        purpose: &str,
    ) -> bool {
        // A card callback has a ~2 s ack budget: leave it room, while still
        // letting tests lower the bound.
        let timeout_ms = self
            .withdrawal_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(DEFER_WITHDRAWAL_TIMEOUT_MS);
        let patched = crate::bridge::bounded_call(
            &format!("{} {} {}", self.kind.label(), req_id, purpose),
            timeout_ms,
            handles.platform.update_message(message_id, replacement),
        )
        .await;
        match patched {
            Some(Ok(())) => true,
            Some(Err(e)) => {
                tracing::warn!(
                    "withdrawing {} ({purpose}) on {message_id} failed: {e}; the record decides",
                    req_id
                );
                false
            }
            None => {
                tracing::warn!(
                    "withdrawing {} ({purpose}) on {message_id} timed out after {timeout_ms}ms; \
                     the record decides",
                    req_id
                );
                false
            }
        }
    }

    /// Build a Location Form card from the question state this process holds:
    /// its partial selections, finalized slots, open question and drafts. A
    /// fresh push and a reopen share this seam, so neither can paint a blank
    /// card over state the next click will act on (ADR-0103). Request state
    /// that is not remembered (the post-restart reopen) falls back to blank.
    async fn location_form_card(&self, req: &PendingRequest, dir: &str) -> serde_json::Value {
        let PendingRequest::Question(q) = req else {
            return serde_json::json!({});
        };
        let paint = self
            .with_question_state(&q.id, |state| state.paint())
            .await
            .unwrap_or_else(|| crate::bridge::question::QuestionPaint::blank(q.questions.len()));
        crate::feishu::card::question::build_location_form_card(
            q,
            dir,
            &paint.display,
            &paint.done,
            paint.open,
            &paint.drafts,
        )
    }

    /// The deferred replacement card for a Location Form request, or `None` for
    /// a session-backed request (which never takes this path).
    fn deferred_card(req: &PendingRequest) -> Option<serde_json::Value> {
        let PendingRequest::Question(q) = req else {
            return None;
        };
        Some(crate::feishu::card::question::build_location_form_deferred_card(
            q.server.as_deref(),
            q.message.as_deref(),
        ))
    }

    /// Withdraw a fresh Location Form card that must not stay live: the send
    /// lost a race to a dismissal, or a reopen lost its generation check.
    async fn withdraw_location_card(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        message_id: &str,
        purpose: &str,
    ) -> bool {
        let Some(deferred) = Self::deferred_card(req) else {
            return false;
        };
        self.withdraw_card(handles, req.id(), message_id, &deferred, purpose)
            .await
    }

    /// Send and record one standalone request card. The caller holds the
    /// owning Session's write lock across the send and the surface record, so
    /// no follow can commit between them. The send itself is bounded, so the
    /// lock is never held on a stalled Feishu call forever. Returns whether
    /// the card landed and its record was committed.
    async fn send_standalone_surface(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
        target: Option<CardTarget>,
        card: &serde_json::Value,
        summary: &str,
    ) -> bool {
        let Some(mid) = self.send_standalone_card(handles, req, target, card).await else {
            return false;
        };
        if self
            .record_standalone_surface(req, dir, mid.clone(), summary)
            .await
        {
            return true;
        }
        // A 「稍后」 landed while this send was in flight. The dismissal is
        // authoritative, so the fresh card must not stay live and untracked.
        self.withdraw_location_card(handles, req, &mid, "push losing to defer")
            .await;
        false
    }

    /// Send a fresh standalone fallback card for a request whose surface proof
    /// changed after it was shown (a summary card whose target changed, or a
    /// legacy digest that no longer parses). The stored summary omits the
    /// ephemeral changed-target note; only the sent card carries it.
    pub(crate) async fn post_fallback_card(
        &self,
        handles: &FlowHandles,
        p: &crate::backend::types::PermissionRequest,
        dir: &str,
        loss: crate::feishu::card::question::FallbackLoss,
        changed: bool,
    ) -> bool {
        let target = crate::bridge::bounded_call(
            &format!("permission {} fallback target", p.request_id),
            self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
            async {
                Ok::<_, crate::error::BridgeError>(
                    resolve_card_target(
                        &handles.sessions,
                        &handles.cards,
                        &handles.backend,
                        &handles.platform,
                        p.session_id.as_deref().unwrap_or(""),
                        dir,
                    )
                    .await,
                )
            },
        )
        .await
        .and_then(Result::ok)
        .flatten();
        if target.is_none() {
            tracing::warn!(
                "permission {}: no reply target or chat for its replacement fallback card",
                p.request_id
            );
            return false;
        }
        let owning_lock = handles
            .cards
            .write_lock(p.session_id.as_deref().unwrap_or(""))
            .await;
        // Compute the card, its stored summary and the request record under the
        // lock, then release it for the send. A card callback runs on a
        // deadline: the send is bounded by [`STANDALONE_SEND_TIMEOUT_MS`] and
        // never holds the Session write lock (a stalled Feishu call would
        // otherwise block flushes, `/card` and install for that Session). The
        // record is committed under the lock after the send lands, following
        // the preview → commit pattern the withdrawal PATCHes use. A timed-out
        // send returns `false`, which the click turns into a retry ack.
        let (req, card, stored_summary) = {
            let _guard = owning_lock.lock().await;
            let target_summary = describe_permission_target_summary(p);
            let stored_summary =
                crate::feishu::card::question::fallback_permission_summary(&target_summary, loss, false);
            let card_summary =
                crate::feishu::card::question::fallback_permission_summary(&target_summary, loss, changed);
            let card = crate::feishu::card::question::build_fallback_permission_card(
                p.session_id.as_deref().unwrap_or(""),
                &p.request_id,
                dir,
                &card_summary,
                p.always_supported,
                patterns_target_digest(p),
                loss,
            );
            (PendingRequest::Permission(p.clone()), card, stored_summary)
        };
        let Some(message_id) = self.send_standalone_card(handles, &req, target, &card).await else {
            return false;
        };
        let _guard = owning_lock.lock().await;
        self.record_standalone_surface(&req, dir, message_id, &stored_summary)
            .await;
        true
    }

    /// Re-host a still-pending inline block onto the session's current card
    /// (ADR-0038, rule 1): a request that outlived the turn which surfaced it
    /// follows the newest card, and the card that used to show it is repainted
    /// without the controls — so a live block lives on exactly one card. Called
    /// from the sweep for requests already in `seen` (the poller would
    /// otherwise never touch them again).
    async fn rehost_block(&self, handles: &FlowHandles, req: &PendingRequest, dir: &str) {
        let id = req.id();
        // Only a block the flush recorded (an inline card) follows the card:
        // a standalone card keeps its own lifecycle (ADR-0038, rule 6).
        let Some(old_message) = handles
            .cards
            .card_handles
            .lock()
            .await
            .message_of(id)
            .map(str::to_string)
        else {
            return;
        };
        let Some(host) =
            inline_host_session(&handles.cards, &handles.backend, req.session_id(), Some(dir)).await
        else {
            return;
        };
        // Re-validate under this second await: the host must still be running,
        // not merely present in the map (ADR-0100).
        if !Turn::is_running(&handles.cards, &host).await {
            return;
        }
        // The accumulator still carries the block (its own flush owns the
        // current card), or the handle already names that card: nothing to
        // move.
        if Turn::has_interaction_in(&handles.cards, &host, id).await
            || Turn::card_message_id(&handles.cards, &host).await.as_deref() == Some(old_message.as_str())
        {
            return;
        }
        // The block keeps the anchor its first surfacing captured (issue
        // #1023): the asking tool's server time is a property of the request,
        // not of the card it currently lives on, so a handoff must not reset
        // the receipt's cut to "no anchor".
        let asked_at = Turn::interaction_asked_at(&handles.cards, id).await;
        let pushed = self
            .kind
            .add_inline(self, &handles.cards, &host, req, dir, asked_at)
            .await;
        if !pushed {
            return;
        }
        // Flush first: the current card renders the block and the handle moves
        // with it.
        Turn::flush_card(&handles.cards, &host).await;
        tracing::info!(
            "{} {} re-hosted on session {} card (old {})",
            self.kind.label(),
            id,
            host,
            old_message
        );
        // Then strip the old card's copy through its handle. The block moved,
        // not resolved: no receipt — the controls simply leave that card.
        if let Some(card) = handles
            .cards
            .card_handles
            .lock()
            .await
            .remove_on(&old_message, id)
            && let Err(e) = handles.platform.update_message(&old_message, &card).await
        {
            tracing::warn!("re-host old-card repaint failed on {}: {}", old_message, e);
        }
    }

    /// Apply a decline decision's effects. `Handled` means the request is
    /// settled and the caller marks it seen; `Retry` means this pass must offer
    /// nothing and leave it retryable.
    async fn apply_refusal(
        &self,
        handles: &FlowHandles,
        p: &crate::backend::types::PermissionRequest,
        dir: &str,
        refusal: Refusal,
    ) -> RefusalApplied {
        match refusal {
            Refusal::Refused(reason) => {
                let how = match reason {
                    DeclineReason::Overflow(overflow) => Withdrawal::Refused(overflow),
                    DeclineReason::CompanionUncertain => Withdrawal::CompanionUncertain,
                };
                match self.withdraw_stored_surface(handles, p, how).await {
                    // The PATCH failed; the stored surface (and the payload)
                    // are kept so the next sweep retries the withdrawal.
                    WithdrawalOutcome::Failed => RefusalApplied::Retry,
                    WithdrawalOutcome::Updated => RefusalApplied::Handled,
                    WithdrawalOutcome::None => {
                        post_withdrawal_notice(
                            handles,
                            p,
                            dir,
                            how,
                            self.list_timeout_ms.load(std::sync::atomic::Ordering::Relaxed),
                        )
                        .await;
                        RefusalApplied::Handled
                    }
                }
            }
            // Someone else settled it: withdraw a stored card without claiming
            // cola rejected anything, post nothing, and treat it as handled.
            Refusal::ResolvedElsewhere => match self
                .withdraw_stored_surface(handles, p, Withdrawal::ResolvedElsewhere)
                .await
            {
                WithdrawalOutcome::Failed => RefusalApplied::Retry,
                _ => RefusalApplied::Handled,
            },
            // The refusal did not land. The request stays pending and unmarked
            // so the next sweep retries it, but a stored card must not stay
            // clickable in the meantime: neutralize it (claiming nothing).
            Refusal::Failed => {
                self.withdraw_stored_surface(handles, p, Withdrawal::Retrying)
                    .await;
                RefusalApplied::Retry
            }
        }
    }

    /// Settle a permission through the ONE refusal skeleton: the reason selects
    /// the withdrawal copy/receipt, while the server reply, not-found and
    /// failure handling are shared.
    async fn settle_decline(
        &self,
        handles: &FlowHandles,
        p: &crate::backend::types::PermissionRequest,
        dir: &str,
        reason: DeclineReason,
    ) -> bool {
        let refusal = decline_permission(handles, p, dir, reason).await;
        matches!(
            self.apply_refusal(handles, p, dir, refusal).await,
            RefusalApplied::Handled
        )
    }

    /// The companion delivery that proves `req`'s exact content, when one
    /// matches. A stale record is not returned: the surfacing path replaces it
    /// before any card is built.
    pub(crate) fn companion_for(&self, req: &PendingRequest) -> Option<CompanionDelivery> {
        let PendingRequest::Permission(p) = req else {
            return None;
        };
        permission_overflow(p)?;
        self.surfaces
            .delivered_companion(req.id(), companion_target_digest(p))
    }

    /// Withdraw whatever surface a previous process persisted for a request
    /// this flow has just settled or refused.
    ///
    /// A persisted inline block is replaced on its cached card by the receipt
    /// `how` names and forgotten; a persisted standalone card is repainted as
    /// `how`'s card and its record forgotten. Neither can be clicked again.
    /// Returns whether any stored surface was found (the caller posts the chat
    /// reason card only when there was none).
    async fn withdraw_stored_surface(
        &self,
        handles: &FlowHandles,
        p: &crate::backend::types::PermissionRequest,
        how: Withdrawal,
    ) -> WithdrawalOutcome {
        let id = p.request_id.as_str();
        self.recovered.lock().await.remove(id);
        let receipt = match how {
            Withdrawal::Refused(_) => crate::feishu::card::question::unshowable_permission_receipt(),
            Withdrawal::ResolvedElsewhere => handled_elsewhere_receipt(&permission_target(p)),
            Withdrawal::CompanionUncertain => {
                crate::feishu::card::question::UNCERTAIN_PERMISSION_RECEIPT.to_string()
            }
            Withdrawal::Superseded => {
                crate::feishu::card::question::SUPERSEDED_PERMISSION_RECEIPT.to_string()
            }
            Withdrawal::Retrying => {
                crate::feishu::card::question::UNSHOWABLE_PERMISSION_RETRY_RECEIPT.to_string()
            }
        };
        // Rendered now, from the record that names the card: a retry replays
        // this exact payload rather than re-deciding what the card should say.
        let standalone = self.sent_cards.lock().await.get(id).cloned();
        let card = match (how, &standalone) {
            (Withdrawal::Refused(overflow), _) => {
                crate::feishu::card::question::build_unshowable_permission_card(overflow.budget())
            }
            (Withdrawal::ResolvedElsewhere, Some(sent)) => {
                crate::feishu::card::notify::build_resolved_elsewhere_card(self.kind.label(), &sent.summary)
            }
            (Withdrawal::ResolvedElsewhere, None) => serde_json::Value::Null,
            (Withdrawal::CompanionUncertain, _) => {
                crate::feishu::card::question::build_uncertain_permission_card()
            }
            (Withdrawal::Superseded, _) => crate::feishu::card::question::build_superseded_permission_card(),
            (Withdrawal::Retrying, _) => {
                crate::feishu::card::question::build_unshowable_permission_retry_card()
            }
        };
        let withdrawal = PendingWithdrawal { how, receipt, card };
        let outcome = self.apply_stored_withdrawal(handles, id, &withdrawal).await;
        if outcome == WithdrawalOutcome::Failed {
            // The handle/record are still there (apply only mutates after a
            // landed PATCH), so both the listed path and the sweep-level retry
            // can deliver this later. The map is bounded: past the cap the new
            // entry is dropped rather than retaining its card forever, and the
            // vanished-surface pass still neutralizes the card with its neutral
            // receipt.
            let mut pending = self.pending_withdrawals.lock().await;
            if pending.contains_key(id) || pending.len() < MAX_PENDING_WITHDRAWALS {
                pending.insert(id.to_string(), withdrawal);
            } else {
                tracing::error!(
                    "withdrawal backlog is full ({MAX_PENDING_WITHDRAWALS}); not retaining {} — \
                     the vanished-surface pass will still neutralize its card",
                    id
                );
            }
        }
        outcome
    }

    /// Withdraw a live inline block for `id` before a standalone fallback card
    /// replaces it (ADR-0102 / #1013). The request stays pending, so this is not
    /// a decision: the block collapses to a neutral "moved" receipt, its handle
    /// is forgotten, and the cached card is patched so no control lingers. The
    /// companion marker (if any) is deliberately **not** touched — it is the
    /// no-duplicate guarantee for the failed send.
    ///
    /// The PATCH is previewed first and only committed locally after it lands,
    /// and it is bounded by the flow's withdrawal timeout — exactly like
    /// [`Self::patch_stored_surfaces`]. A failed PATCH keeps the handle so the
    /// next sweep can retry; the caller then leaves the request unmarked and
    /// offers no second surface.
    pub(crate) async fn withdraw_block_for_fallback(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        host: Option<&str>,
    ) -> bool {
        let id = req.id();
        let session = host.unwrap_or(req.session_id());
        let receipt = crate::feishu::card::question::FALLBACK_PERMISSION_MOVED_RECEIPT;
        let timeout_ms = self
            .withdrawal_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        // The same write lock the flush takes: the block's disappearance and
        // the cached card's edit must not race a render pass.
        let write_lock = handles.cards.write_lock(session).await;
        let _guard = write_lock.lock().await;
        let Some(message_id) = handles
            .cards
            .card_handles
            .lock()
            .await
            .message_of(id)
            .map(str::to_string)
        else {
            // No cached card/handle yet: the block can still live in the
            // accumulator (a flush is what records the handle). There is no
            // surface to PATCH, so resolving the accumulator is the whole
            // withdrawal.
            let residue = crate::bridge::turn::InlineResidue::PerBlock(&|_: &str| receipt.to_string());
            crate::bridge::turn::Turn::resolve_interactions(
                &handles.cards,
                session,
                &[id.to_string()],
                &residue,
            )
            .await;
            return true;
        };
        // Preview the cached card's edit without committing it: a failed PATCH
        // must leave the live block (and its handle) in place for a retry.
        let preview = handles
            .cards
            .card_handles
            .lock()
            .await
            .resolved_preview(&message_id, id, receipt);
        let Some(preview) = preview else {
            // The cached card cannot render the block: nothing there is
            // clickable, so the handle has nothing left to neutralize.
            handles.cards.card_handles.lock().await.forget(id);
            let residue = crate::bridge::turn::InlineResidue::PerBlock(&|_: &str| receipt.to_string());
            crate::bridge::turn::Turn::resolve_interactions(
                &handles.cards,
                session,
                &[id.to_string()],
                &residue,
            )
            .await;
            return true;
        };
        let patched = crate::bridge::bounded_call(
            &format!("fallback {} withdraw block on {}", id, message_id),
            timeout_ms,
            handles.platform.update_message(&message_id, &preview),
        )
        .await;
        match patched {
            Some(Ok(())) => {
                {
                    let mut registry = handles.cards.card_handles.lock().await;
                    registry.resolve_on(&message_id, id, receipt);
                    registry.forget(id);
                }
                // The accumulator is the render source: record the receipt
                // there only once the PATCH has landed.
                let residue = crate::bridge::turn::InlineResidue::PerBlock(&|_: &str| receipt.to_string());
                crate::bridge::turn::Turn::resolve_interactions(
                    &handles.cards,
                    session,
                    &[id.to_string()],
                    &residue,
                )
                .await;
                tracing::info!("withdrew {} inline block for the standalone fallback card", id);
                true
            }
            Some(Err(e)) if e.is_message_gone() => {
                let mut registry = handles.cards.card_handles.lock().await;
                registry.forget(id);
                registry.release_cache(&message_id);
                drop(registry);
                let residue = crate::bridge::turn::InlineResidue::PerBlock(&|_: &str| receipt.to_string());
                crate::bridge::turn::Turn::resolve_interactions(
                    &handles.cards,
                    session,
                    &[id.to_string()],
                    &residue,
                )
                .await;
                tracing::info!(
                    "withdrew {} inline block: message {} is gone ({e})",
                    id,
                    message_id
                );
                true
            }
            other => {
                match other {
                    Some(Err(e)) => tracing::warn!(
                        "fallback {}: withdrawing its block on {} failed: {e}; keeping it for a retry",
                        id,
                        message_id
                    ),
                    _ => tracing::warn!(
                        "fallback {}: withdrawing its block on {} timed out after {}ms; keeping it \
                         for a retry",
                        id,
                        message_id,
                        timeout_ms
                    ),
                }
                false
            }
        }
    }

    /// Patch every stored surface for `id` with the replacement `receipt` and
    /// `card`, optionally committing the registry/record mutation after each
    /// successful PATCH. Returns `(attempted, failed)`. One loop serves both the
    /// committed withdrawal and the preview neutralization, so they cannot
    /// drift.
    async fn patch_stored_surfaces(
        &self,
        handles: &FlowHandles,
        id: &str,
        receipt: &str,
        card: &serde_json::Value,
        commit: bool,
    ) -> (bool, bool) {
        let mut attempted = false;
        let mut failed = false;
        let timeout_ms = self
            .withdrawal_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        // Inline: preview the block's replacement on the cached card, PATCH,
        // then (on commit) apply the edit and drop the handle.
        let inline = {
            let registry = handles.cards.card_handles.lock().await;
            registry
                .message_of(id)
                .map(str::to_string)
                .and_then(|message_id| {
                    registry
                        .resolved_preview(&message_id, id, receipt)
                        .map(|preview| (message_id, preview))
                })
        };
        if let Some((message_id, preview)) = inline {
            attempted = true;
            let patched = crate::bridge::bounded_call(
                &format!("withdraw {} block on {}", id, message_id),
                timeout_ms,
                handles.platform.update_message(&message_id, &preview),
            )
            .await;
            match patched {
                Some(Ok(())) => {
                    if commit {
                        let mut registry = handles.cards.card_handles.lock().await;
                        registry.resolve_on(&message_id, id, receipt);
                        registry.forget(id);
                    }
                    tracing::info!("withdrew {} inline block on {}", id, message_id);
                }
                Some(Err(e)) if e.is_message_gone() => {
                    // The message no longer exists: nothing is clickable, so
                    // the handle — and the dead card's cached JSON — have
                    // nothing left to neutralize.
                    if commit {
                        let mut registry = handles.cards.card_handles.lock().await;
                        registry.forget(id);
                        registry.release_cache(&message_id);
                    }
                    tracing::info!(
                        "withdrew {} inline block: message {} is gone ({})",
                        id,
                        message_id,
                        e
                    );
                }
                other => {
                    failed = true;
                    match other {
                        Some(Err(e)) => tracing::warn!(
                            "withdrawing {} inline block on {} failed: {}; the handle is kept so \
                             the withdrawal can be retried",
                            id,
                            message_id,
                            e
                        ),
                        _ => tracing::warn!(
                            "withdrawing {} inline block on {} timed out after {}ms; the handle is \
                             kept so the withdrawal can be retried",
                            id,
                            message_id,
                            timeout_ms
                        ),
                    }
                }
            }
        } else if commit {
            // A handle whose cached card no longer renders the block is
            // unusable, and nothing there can be clicked: drop it.
            let mut registry = handles.cards.card_handles.lock().await;
            if registry.message_of(id).is_some() {
                registry.forget(id);
            }
        }
        // Standalone: PATCH the record's card, then (on commit) forget it.
        let standalone = self.sent_cards.lock().await.get(id).cloned();
        if let Some(sent) = standalone {
            attempted = true;
            let patched = crate::bridge::bounded_call(
                &format!("withdraw {} card on {}", id, sent.message_id),
                timeout_ms,
                handles.platform.update_message(&sent.message_id, card),
            )
            .await;
            match patched {
                Some(Ok(())) => {
                    if commit {
                        self.sent_cards.lock().await.remove(id);
                        self.surfaces.remove_standalone(id);
                    }
                    tracing::info!("withdrew {} standalone card on {}", id, sent.message_id);
                }
                Some(Err(e)) if e.is_message_gone() => {
                    if commit {
                        self.sent_cards.lock().await.remove(id);
                        self.surfaces.remove_standalone(id);
                    }
                    tracing::info!(
                        "withdrew {} standalone card: message {} is gone ({})",
                        id,
                        sent.message_id,
                        e
                    );
                }
                other => {
                    failed = true;
                    match other {
                        Some(Err(e)) => tracing::warn!(
                            "withdrawing {} standalone card on {} failed: {}; the record is kept \
                             so the withdrawal can be retried",
                            id,
                            sent.message_id,
                            e
                        ),
                        _ => tracing::warn!(
                            "withdrawing {} standalone card on {} timed out after {}ms; the record \
                             is kept so the withdrawal can be retried",
                            id,
                            sent.message_id,
                            timeout_ms
                        ),
                    }
                }
            }
        }
        (attempted, failed)
    }

    /// Neutralize a stored surface with a replacement payload **without
    /// committing** the registry/sent-card mutation. The readoption failure
    /// path uses this so the old card stops being interactive immediately while
    /// the pending SUPERSEDED retry still paints the final receipt. Returns
    /// whether every stored surface was patched.
    async fn neutralize_stored_surface_preview(
        &self,
        handles: &FlowHandles,
        id: &str,
        receipt: &str,
        card: &serde_json::Value,
    ) -> bool {
        let (_, failed) = self
            .patch_stored_surfaces(handles, id, receipt, card, false)
            .await;
        !failed
    }

    /// Patch every stored surface for `id` and commit the withdrawal.
    async fn apply_stored_withdrawal(
        &self,
        handles: &FlowHandles,
        id: &str,
        withdrawal: &PendingWithdrawal,
    ) -> WithdrawalOutcome {
        let (attempted, failed) = self
            .patch_stored_surfaces(handles, id, &withdrawal.receipt, &withdrawal.card, true)
            .await;
        if !failed
            && matches!(
                withdrawal.how,
                Withdrawal::Refused(_) | Withdrawal::ResolvedElsewhere | Withdrawal::CompanionUncertain
            )
        {
            // A settled request's delivery proof/marker is no longer needed,
            // wherever the withdrawal landed (listed path or retry phase).
            self.surfaces.remove_companion(id);
        }
        if failed {
            WithdrawalOutcome::Failed
        } else if attempted {
            WithdrawalOutcome::Updated
        } else {
            WithdrawalOutcome::None
        }
    }

    /// Test-only: how many decided-but-undelivered withdrawals are retained.
    #[cfg(test)]
    pub(crate) async fn pending_withdrawal_count(&self) -> usize {
        self.pending_withdrawals.lock().await.len()
    }

    /// Retry a bounded slice of the decided-but-undelivered withdrawals. The
    /// request may already have left the pending list (a decline that landed
    /// before its card PATCH failed), so this cannot wait for the per-request
    /// sweep.
    ///
    /// The phase runs before the listing, so its cost is capped twice: at most
    /// [`MAX_WITHDRAWAL_RETRIES_PER_SWEEP`] entries are attempted, and the whole
    /// phase is cut off at `withdrawal_retry_budget_ms`. Entries not reached (or
    /// cut off mid-PATCH) are left untouched for the next sweep — the
    /// cancellation is safe because a withdrawal only mutates state after a
    /// landed PATCH. Returns every id the map held at the start of the phase:
    /// the per-request pass must not re-attempt them in the same sweep, or the
    /// budget would be defeated by the request loop.
    async fn retry_pending_withdrawals(&self, handles: &FlowHandles) -> std::collections::HashSet<String> {
        let all: Vec<(String, PendingWithdrawal)> = self
            .pending_withdrawals
            .lock()
            .await
            .iter()
            .map(|(id, withdrawal)| (id.clone(), withdrawal.clone()))
            .collect();
        let claimed: std::collections::HashSet<String> = all.iter().map(|(id, _)| id.clone()).collect();
        let pending: Vec<(String, PendingWithdrawal)> =
            all.into_iter().take(MAX_WITHDRAWAL_RETRIES_PER_SWEEP).collect();
        if pending.is_empty() {
            return claimed;
        }
        let budget = std::time::Duration::from_millis(
            self.withdrawal_retry_budget_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        let phase = async {
            for (id, withdrawal) in pending {
                if self.apply_stored_withdrawal(handles, &id, &withdrawal).await != WithdrawalOutcome::Failed
                {
                    self.pending_withdrawals.lock().await.remove(&id);
                }
            }
        };
        if tokio::time::timeout(budget, phase).await.is_err() {
            tracing::warn!(
                "withdrawal retry phase used its {}ms budget; the remaining entries retry next \
                 sweep",
                budget.as_millis()
            );
        }
        claimed
    }

    /// Re-adopt the card a previous process surfaced this request on (ADR-0038
    /// restart re-adoption). The kind's in-flight state is remembered first, so
    /// a click on the card resolves against this process; then the persisted
    /// card is repainted — refreshed from that state for a question, whose
    /// partial answers did not survive the restart.
    ///
    /// An over-budget permission is re-adopted only when a matching confirmed
    /// companion record proves its full content was delivered. Without one the
    /// card is withdrawn neutrally and the request takes the normal fresh path
    /// — prepare (Auto-Accept), the companion lane, or the fallback card — in
    /// this same sweep; the old decision is never approved or declined in place
    /// (ADR-0101, ADR-0102). [`ReadoptDecision::NotAdopted`] means the card is
    /// gone, unusable, or was withdrawn for that reason, so the caller surfaces
    /// the request normally.
    async fn readopt_surface(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
    ) -> ReadoptDecision {
        let id = req.id();
        // A dismissed Location Form must not be re-adopted: its persisted
        // decision is authoritative and the card is deliberately withdrawn
        // (ADR-0038's 2026-11-09 amendment). The push decision below also
        // refuses it; this guard keeps the re-adoption path from repainting a
        // card whose record says it is gone.
        if self
            .surfaces
            .location_form(id)
            .is_some_and(|record| record.is_dismissed())
        {
            return ReadoptDecision::NotAdopted;
        }
        // A Location Form's standalone card is not in the card-handle registry,
        // so it takes its own re-adoption path: repaint the persisted card and
        // prove it still exists (a deleted card must be re-surfaced, ADR-0038).
        if req.is_location() {
            return self.readopt_location_form(handles, req, dir).await;
        }
        if let PendingRequest::Permission(p) = req
            && permission_overflow(p).is_some()
        {
            let usable = self
                .surfaces
                .delivered_companion(id, companion_target_digest(p))
                .is_some();
            if !usable {
                // The persisted card was never backed by a delivered full
                // content (no companion, or only an uncertain marker). Withdraw
                // it neutrally and let the request take the normal fresh path —
                // prepare (Auto-Accept) or the companion lane — in this sweep.
                // The old decision is never silently taken over (ADR-0101).
                tracing::warn!(
                    "{} {}: recovered card has no usable companion; withdrawing it and reprocessing the request",
                    self.kind.label(),
                    id
                );
                return match self
                    .withdraw_stored_surface(handles, p, Withdrawal::Superseded)
                    .await
                {
                    WithdrawalOutcome::Failed => {
                        // The pending SUPERSEDED entry retries the final
                        // withdrawal next sweep; neutralize the still-live old
                        // card now WITHOUT forgetting its handle, so that retry
                        // can still paint the final receipt.
                        let _ = self
                            .neutralize_stored_surface_preview(
                                handles,
                                id,
                                crate::feishu::card::question::SUPERSEDED_PERMISSION_PREVIEW_RECEIPT,
                                &crate::feishu::card::question::build_superseded_permission_preview_card(),
                            )
                            .await;
                        ReadoptDecision::Retry
                    }
                    _ => ReadoptDecision::NotAdopted,
                };
            }
        }
        // Remembering the kind's state is what makes the card's buttons resolve
        // — the poller never ran `prepare` for a request it considered already
        // surfaced.
        self.kind.remember_surfaced(self, req, dir).await;
        // A recovered standalone card needs no repaint: it is already the live
        // surface and its buttons carry everything a click needs (ADR-0038,
        // rule 6).
        let Some(message_id) = handles
            .cards
            .card_handles
            .lock()
            .await
            .message_of(id)
            .map(str::to_string)
        else {
            return ReadoptDecision::Adopted;
        };
        // Inline: repaint the persisted card — refreshed from the state this
        // process holds for a question, so partial answers that did not
        // survive the restart are not shown as selected. Bounded like every
        // other sweep PATCH: a hung Feishu call must not wedge the sweep.
        let repaint_timeout_ms = self
            .withdrawal_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let repainted = match self
            .kind
            .refresh_cached_block(self, &handles.cards, &message_id, req, dir)
            .await
        {
            Some(card) => match crate::bridge::bounded_call(
                &format!("{} {} re-adopt repaint on {}", self.kind.label(), id, message_id),
                repaint_timeout_ms,
                handles.platform.update_message(&message_id, &card),
            )
            .await
            {
                Some(Ok(())) => {
                    tracing::info!(
                        "{} {} re-adopted on card {} after restart",
                        self.kind.label(),
                        id,
                        message_id
                    );
                    true
                }
                Some(Err(e)) => {
                    tracing::warn!(
                        "{} {} re-adoption repaint failed on {}: {} — surfacing it anew",
                        self.kind.label(),
                        id,
                        message_id,
                        e
                    );
                    false
                }
                None => {
                    tracing::warn!(
                        "{} {} re-adoption repaint timed out after {}ms on {} — surfacing it anew",
                        self.kind.label(),
                        id,
                        repaint_timeout_ms,
                        message_id
                    );
                    false
                }
            },
            // The persisted JSON cannot render the block: the handle is dead.
            None => false,
        };
        if !repainted {
            // The card is gone (or unusable): forget the stale surface, in
            // memory and in the record, and let the caller surface the request
            // normally.
            let mut handles = handles.cards.card_handles.lock().await;
            handles.remove_on(&message_id, id);
            handles.forget(id);
        }
        if repainted {
            ReadoptDecision::Adopted
        } else {
            ReadoptDecision::NotAdopted
        }
    }

    /// Re-adopt a persisted Location Form card by reading it: the read proves
    /// the message still exists without repainting it, so an unanswered typed
    /// field keeps the text the Host already entered (the card-repaint rule).
    /// A proven-gone card (Feishu `230002`) forgets its surface record and lets
    /// the caller post a fresh one in the same sweep (ADR-0038); any other
    /// failure is unknown, never read as deletion, so the live surface is kept
    /// and no duplicate is posted.
    async fn readopt_location_form(
        &self,
        handles: &FlowHandles,
        req: &PendingRequest,
        dir: &str,
    ) -> ReadoptDecision {
        let id = req.id();
        let Some(sent) = self.sent_cards.lock().await.get(id).cloned() else {
            // No persisted card to adopt: let the push path decide.
            return ReadoptDecision::NotAdopted;
        };
        let timeout_ms = self
            .withdrawal_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let read = crate::bridge::bounded_call(
            &format!(
                "{} {} re-adopt read on {}",
                self.kind.label(),
                id,
                sent.message_id
            ),
            timeout_ms,
            handles.platform.get_card_view(&sent.message_id),
        )
        .await;
        match read {
            Some(Ok(_)) => {
                self.kind.remember_surfaced(self, req, dir).await;
                tracing::info!(
                    "{} {} re-adopted on card {} after restart",
                    self.kind.label(),
                    id,
                    sent.message_id
                );
                ReadoptDecision::Adopted
            }
            Some(Err(e)) if e.is_message_gone() => {
                tracing::warn!(
                    "{} {}: recovered card {} was deleted ({}); forgetting it and surfacing the form anew",
                    self.kind.label(),
                    id,
                    sent.message_id,
                    e
                );
                // Capture the persisted partial state into the in-memory state
                // BEFORE the record goes, so the fresh card this sweep posts
                // (and the state its controls submit) still agree.
                self.kind.remember_surfaced(self, req, dir).await;
                self.sent_cards.lock().await.remove(id);
                self.surfaces.remove_location_form(id);
                ReadoptDecision::NotAdopted
            }
            Some(Err(e)) => {
                // Unknown is never read as gone (#130): a transient failure
                // must not post a second card over a live one.
                tracing::warn!(
                    "{} {}: re-adoption read on {} failed ({}); keeping the surface",
                    self.kind.label(),
                    id,
                    sent.message_id,
                    e
                );
                self.kind.remember_surfaced(self, req, dir).await;
                ReadoptDecision::Adopted
            }
            None => {
                tracing::warn!(
                    "{} {}: re-adoption read on {} timed out after {}ms; keeping the surface",
                    self.kind.label(),
                    id,
                    sent.message_id,
                    timeout_ms
                );
                self.kind.remember_surfaced(self, req, dir).await;
                ReadoptDecision::Adopted
            }
        }
    }

    /// The owning directory of a request this flow knows, when a callback
    /// carries none and the session store cannot resolve it: the question's
    /// remembered request, the standalone card's record, or the persisted
    /// block's handle (a re-adopted card after a restart).
    async fn remembered_directory(&self, cards: &CardsHandle, req_id: &str) -> Option<String> {
        if let Some(dir) = self.question_dir(req_id).await {
            return Some(dir);
        }
        if let Some(card) = self.sent_cards.lock().await.get(req_id) {
            return Some(card.directory.clone());
        }
        cards
            .card_handles
            .lock()
            .await
            .directory_of(req_id)
            .map(str::to_string)
    }

    /// Handle a card action on this kind's card: answer / submit / reject. The
    /// shared skeleton resolves the delivery context and the double-click guard;
    /// the kind applies its own semantics.
    pub(crate) async fn handle_card_action(
        &self,
        handles: &FlowHandles,
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
                let from_store = handles
                    .sessions
                    .store
                    .lock()
                    .await
                    .directory_for_session(session_id);
                if from_store.is_some() {
                    from_store
                } else {
                    self.remembered_directory(&handles.cards, req_id).await
                }
            }
        };
        let directory = directory.as_deref();

        // A Location Form has no Session to inline onto: its card is always
        // standalone, so skip every host/inline probe (including the doomed
        // `session_info("")` read). The card's own callback marker covers a
        // plain click; the remembered owner covers a form submit whose routing
        // was rebuilt from the button name without the extra field.
        let location_form = value
            .get("location_form")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || self.is_location_question(req_id).await;
        let owner = if location_form {
            crate::backend::types::RequestScope::Location
        } else {
            crate::backend::types::RequestScope::Session
        };
        let reply = value.get("reply").and_then(|v| v.as_str()).unwrap_or("reject");
        // The card the click landed on, so its handle can update THAT card
        // atomically in the ack (ADR-0038, rule 3) — the callback context
        // carries it (ws.rs injects `open_message_id`).
        let clicked = value.get("open_message_id").and_then(|v| v.as_str());
        // Inline interaction: the session (or its sub-task parent chain) has a
        // LIVE streaming card, so the result is NOT returned as a replacement
        // card — the streaming card re-renders itself on the next poll. The
        // card map's key is not liveness (a finished card stays there, and an
        // idle Session Card is terminal), so the decision asks
        // `Turn::is_running` (ADR-0100). The clicked card rendering the block
        // through its handle counts too: a re-adopted card after a restart has
        // no accumulator, but its cached JSON still carries the block and the
        // ack can edit it in place.
        let host = if location_form {
            None
        } else if Turn::is_running(&handles.cards, session_id).await {
            Some(session_id.to_string())
        } else {
            inline_host_session(&handles.cards, &handles.backend, session_id, directory).await
        };
        let inline = !location_form
            && (host.is_some()
                || match clicked {
                    Some(message_id) => handles
                        .cards
                        .card_handles
                        .lock()
                        .await
                        .renders(message_id, req_id),
                    None => false,
                });

        self.kind
            .handle_action(
                self, handles, session_id, req_id, reply, value, directory, inline, &host, clicked, owner,
            )
            .await
    }
}

/// Whether session `candidate` belongs to `session_id` — the session itself or
/// one of its sub-task descendants (parent-chain walk). The ownership filter
/// `/autoaccept`'s pending approval and the turn-end leftover rejection (#187)
/// share; an empty id matches nothing.
pub(crate) async fn session_belongs_to(
    sessions: &SessionsHandle,
    backend: &Arc<dyn crate::backend::Backend>,
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
    backend: &Arc<dyn crate::backend::Backend>,
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

#[cfg(test)]
mod tests {
    use super::asking_anchor;
    use crate::backend::{MessageRole, Part, SessionTranscript, ToolCall, ToolIdentity, ToolStatus};
    use crate::bridge::test_support::{text_part, typed_message};

    /// The surfacing anchor is the read's newest server timestamp: the asking
    /// tool call is the newest part while a request blocks its turn, so this is
    /// its start time — and nothing the Host could have seen is above it.
    #[test]
    fn the_anchor_is_the_newest_server_activity_in_the_read() {
        let transcript = SessionTranscript::new(vec![
            typed_message("u1", MessageRole::User, Some(1_000), vec![text_part("问题")]),
            typed_message(
                "a1",
                MessageRole::Assistant,
                Some(1_100),
                vec![
                    text_part("先解释。"),
                    Part::Tool(ToolCall {
                        identity: ToolIdentity {
                            name: "question".into(),
                            call_id: "call_q".into(),
                        },
                        status: ToolStatus::Running,
                        started_at: Some(1_250),
                        input: None,
                        metadata: None,
                        output: Default::default(),
                    }),
                ],
            ),
        ]);
        assert_eq!(asking_anchor(&transcript), Some(1_250));
    }

    /// A message the wire carried no time for still contributes its parts'
    /// starts — a read whose only clock is the asking tool's must anchor.
    #[test]
    fn an_untimed_message_contributes_its_part_starts() {
        let transcript = SessionTranscript::new(vec![typed_message(
            "a1",
            MessageRole::Assistant,
            None,
            vec![
                text_part("无时间的文本"),
                Part::Tool(ToolCall {
                    identity: ToolIdentity {
                        name: "question".into(),
                        call_id: "call_q".into(),
                    },
                    status: ToolStatus::Running,
                    started_at: Some(900),
                    input: None,
                    metadata: None,
                    output: Default::default(),
                }),
            ],
        )]);
        assert_eq!(asking_anchor(&transcript), Some(900));
    }

    /// A read with no server time anywhere leaves the block without an anchor
    /// (the old receipt behaviour) rather than inventing a clock.
    #[test]
    fn a_timeless_read_has_no_anchor() {
        let transcript = SessionTranscript::new(vec![typed_message(
            "a1",
            MessageRole::Assistant,
            None,
            vec![text_part("无时间")],
        )]);
        assert_eq!(asking_anchor(&transcript), None);
    }
}
