//! The Turn's streaming state (spec #298, A3).
//!
//! The accumulator, its card session and their value types live behind the
//! Turn's interface (the `impl Turn` blocks in the parent module). Nothing
//! here is reachable from outside the Turn module: the coordinator's card map
//! only ever names [`CardSession`], whose fields are visible only inside the
//! Turn module, and every read or write goes through a `Turn::` method. The
//! accumulator's own tests are the module's internal seam.

use crate::backend::TurnAnchor;
use crate::bridge::handles::CardsHandle;
use crate::feishu::card::shell::CardBuilder;
use crate::feishu::card::tool_render::ToolPanel;
use crate::feishu::card::{AwaitingAction, CardState};
use crate::opencode;
use indexmap::IndexMap;
use std::sync::Arc;

/// One part already rendered into this card, addressed by content: a part
/// payload carries no stable id (AGENTS.md #9), so text and reasoning dedupe
/// on the content itself — equal content never renders twice, whatever message
/// carried it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum RenderedPart {
    Text(String),
    Reasoning(String),
}

/// One entry on a turn's chronological timeline: a rendered item plus the
/// server-side start time (epoch ms) of the part it came from — its **key**.
/// The card renders the timeline in key order, matching how OpenChamber shows
/// the parts interleaved. `shown_at` is the same server time when the part
/// truly has one, or `None` when the key is a synthetic ordering device — the
/// card only ever shows server clocks (#183 follow-up).
#[derive(Debug, Clone)]
pub(super) struct TimelineItem {
    pub(super) key: i64,
    pub(super) shown_at: Option<i64>,
    /// Identity of this entry, assigned once at insertion and never reused.
    /// It names the entry's panel on the card (`tool_{seq}` / `reason_{seq}`);
    /// unlike the entry's position it survives timeline insertions and
    /// merges, so the client's local panel state can't drift to another panel
    /// when the card re-renders.
    pub(super) seq: u64,
    pub(super) kind: TimelineKind,
}

/// Bookkeeping for a Tool Panel that is still live (ADR-0045): the timeline
/// key and element identity allocated when its call first appeared, so the
/// settle move into the timeline lands in the call's original order and keeps
/// the reader's fold state.
#[derive(Debug, Clone)]
struct LiveTool {
    key: i64,
    shown_at: Option<i64>,
    seq: u64,
}

/// What a timeline entry renders.
#[derive(Debug, Clone)]
pub(super) enum TimelineKind {
    Text(String),
    Reasoning(String),
    Tool(String),
    /// A resolved interaction block's Interaction Receipt (ADR-0038, rule 4):
    /// one markdown line keyed by the moment it was resolved, so it sits below
    /// everything that was on the card when the Host clicked and above
    /// everything resolved afterwards.
    Receipt(String),
}

/// A card as built for one message: the JSON, whether the timeline had to
/// split (the caller then sends a continuation), and the element ranges of the
/// live interaction blocks this card renders — empty on finalized slices, whose
/// tail belongs to the newest card.
pub(super) struct BuiltCard {
    pub(super) card: serde_json::Value,
    pub(super) full: bool,
    pub(super) spans: Vec<crate::bridge::card_handles::BlockSpan>,
}

/// Estimated serialized size (bytes) of one collapsible tool panel, mirroring
/// [`StreamAccumulator::estimate_split_index`]'s accounting (element overhead
/// plus the capped input and output the renderer keeps). Shared by the
/// timeline's tool items and the todo tail reserve.
fn panel_estimate(p: &ToolPanel) -> usize {
    let first_n_bytes = |s: &str, n: usize| s.chars().take(n).map(|c| c.len_utf8()).sum::<usize>();
    let input = p
        .input()
        .map(|x| x.to_string())
        .map(|s| first_n_bytes(&s, 400))
        .unwrap_or(0);
    let output = p
        .output()
        .map(|s| first_n_bytes(&s, crate::feishu::card::tool_render::TOOL_OUTPUT_MAX_CHARS))
        .unwrap_or(0);
    400 + input + output
}

/// A permission request surfaced inline on the streaming card (instead of a
/// separate card), so the whole turn lives on ONE card.
#[derive(Debug, Clone)]
pub(super) struct PendingPermission {
    pub(super) session_id: String,
    pub(super) request_id: String,
    /// The full markdown body the block renders (action, patterns/diff).
    pub(super) body: String,
    /// Compact one-line form of the same request (action + first pattern /
    /// edited file) for the Interaction Receipt — receipts name their target
    /// even where the position cannot (ADR-0038).
    pub(super) target: String,
    pub(super) directory: String,
}

/// A `question` tool request surfaced inline on the streaming card. `answers[i]`
/// tracks which questions are already answered (None = open), kept in sync with
/// the flow's [`crate::bridge::question::QuestionState`].
#[derive(Debug, Clone)]
pub(super) struct PendingQuestion {
    pub(super) request_id: String,
    pub(super) session_id: String,
    pub(super) questions: Vec<crate::opencode::types::QuestionInfo>,
    pub(super) directory: String,
    /// Display selection per question (locked answer, or live multi-select
    /// toggles). Mirrors `question_elements`' `answered` slice.
    pub(super) answers: Vec<Option<Vec<String>>>,
    /// Whether each question is finalized (single-select answered, multi-select
    /// confirmed) — its controls collapse to a static 已选 line.
    pub(super) done: Vec<bool>,
}

/// One entry in a card's interaction section: a live permission or question
/// block, or the tombstone of the receipt left once resolved. The section has
/// ONE representation (this list), ONE mutation API (the `*_interaction`
/// methods on [`StreamAccumulator`]) and one render point for live blocks
/// (`build_card_inner`'s tail): every update — add, state change, resolve —
/// goes through that seam, so what the card renders cannot drift from the
/// accumulator's in-flight state (ADR-0038).
#[derive(Debug, Clone)]
pub(super) enum InteractionBlock {
    Permission(PendingPermission),
    Question(PendingQuestion),
    /// Tombstone for a resolved request (its id): the section keeps it so a
    /// racing poll cannot re-surface the block, while the rendered line lives
    /// in the timeline ([`TimelineKind::Receipt`]) — the one render source
    /// (ADR-0038, rule 4).
    Receipt(String),
}

impl InteractionBlock {
    /// The request this block belongs to (unique across kinds).
    pub(super) fn request_id(&self) -> &str {
        match self {
            InteractionBlock::Permission(p) => &p.request_id,
            InteractionBlock::Question(q) => &q.request_id,
            InteractionBlock::Receipt(request_id) => request_id,
        }
    }

    /// Whether the block still awaits the Host (a receipt is settled).
    pub(super) fn is_live(&self) -> bool {
        !matches!(self, InteractionBlock::Receipt(_))
    }

    /// The directory that owns the block's request — the scope a sweep judges
    /// a vanished request in. Empty for a receipt tombstone, whose request is
    /// already settled.
    pub(super) fn directory(&self) -> &str {
        match self {
            InteractionBlock::Permission(p) => &p.directory,
            InteractionBlock::Question(q) => &q.directory,
            InteractionBlock::Receipt(_) => "",
        }
    }

    /// The session that owns the block's request (a sub-task child carries its
    /// own id). Empty for a receipt tombstone.
    pub(super) fn session_id(&self) -> &str {
        match self {
            InteractionBlock::Permission(p) => &p.session_id,
            InteractionBlock::Question(q) => &q.session_id,
            InteractionBlock::Receipt(_) => "",
        }
    }

    /// The compact one-line target an Interaction Receipt names for this block
    /// (ADR-0038, rule 4): permissions carry their action + first pattern /
    /// edited file (computed at inline time), questions their headers (or a
    /// clipped question text). Derived from the block itself, never from a
    /// click payload, so a malformed callback cannot write arbitrary markdown
    /// onto a card.
    pub(super) fn receipt_target(&self) -> String {
        match self {
            InteractionBlock::Permission(p) => p.target.clone(),
            InteractionBlock::Question(q) => crate::feishu::card::question::question_target(&q.questions),
            InteractionBlock::Receipt(_) => String::new(),
        }
    }
}

/// Compact token count for the Turn Footer's context segment: `842`, `84k`,
/// `1.2M`. One decimal only under 10M, so a large window stays readable.
fn format_tokens(n: i64) -> String {
    if n >= 10_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{n}")
    }
}

/// The turn-start work context (ADR-0019): the session directory plus the git
/// halves the Turn Footer shows. Captured before the prompt runs, applied to
/// the live card separately (see [`StreamAccumulator::capture_work_context`]).
#[derive(Debug, Clone, Default)]
pub(super) struct WorkContext {
    pub(super) directory: String,
    pub(super) project_name: Option<String>,
    pub(super) git: crate::git::GitState,
}

/// One queued Card Chain split (ADR-0043): the message its continuation must
/// reply to, why it was requested, and whether its receipt line has already
/// been written into the accumulator. The flag keeps the receipt
/// exactly-once when a continuation send fails and the split is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingSplit {
    pub(super) reply_to: String,
    pub(super) kind: super::SplitKind,
    pub(super) receipt_pushed: bool,
}

/// One live card per session: the streaming accumulator plus the card identity
/// chain — the current live card's message id, updated in place by
/// `flush_card` (including continuation cards). Replaces the two per-session
/// maps kept in lockstep; owned by the cards handle's per-session map.
#[derive(Clone)]
pub struct CardSession {
    pub(super) acc: StreamAccumulator,
    pub(super) card_message_id: Option<String>,
    /// The header signature of the last flush, compared on each poll so the
    /// card is re-flushed when the progress timer / state changes even with no
    /// new content (ADR-0014).
    pub(super) last_header_sig: String,
    /// The context segment's render inputs as of the last flush (ADR-0044),
    /// compared each poll so a step's usage change flushes even when no part
    /// and no header second changed.
    pub(super) last_context_sig: (i64, Option<i64>),
    /// The Supplement split queue (ADR-0043), in arrival order — never
    /// coalesced. The serving rule: the flush that finalizes the live card
    /// writes one receipt per queued supplement (arrival order) and sends
    /// exactly ONE continuation, replied to the NEWEST queued supplement,
    /// carrying only the delta after the finalized slice; a successful send
    /// clears the queue. While the chain still owes that continuation (a failed
    /// send, or a finalized slice carrying the remainder), the newest entry
    /// stays the anchor and only supplements whose receipt was not written yet
    /// get one — so a retry duplicates nothing and a later arrival is still
    /// served.
    pub(super) pending_split: Vec<PendingSplit>,
    /// Whether the tracked card is still the live (growing) card that flushes
    /// update in place. True until a split finalizes it; a continuation that
    /// fits becomes the new live card, while one that is itself over the size
    /// budget stays FINALIZED (and is never overwritten). Persisted so a flush
    /// that exhausted the chain bound — or died between a finalize and its
    /// continuation — resumes the chain instead of losing the slice it sent.
    pub(super) card_is_live: bool,
}

impl CardSession {
    /// New session card: the header signature starts empty so the first poll
    /// always flushes (stamping the progress timer). `card_message_id` is the
    /// live card to update in place.
    pub(super) fn new(acc: StreamAccumulator, card_message_id: Option<String>) -> Self {
        let last_context_sig = acc.context_sig();
        Self {
            acc,
            card_message_id,
            last_header_sig: String::new(),
            last_context_sig,
            pending_split: Vec::new(),
            card_is_live: true,
        }
    }

    /// True while this card belongs to a Turn that has not finished: the pull
    /// condition for `/card` (ADR-0043, 2026-09-22 amendment). A completed
    /// (Done) or failed (Error) card session stays in the cards handle's map until
    /// the next Turn replaces it, so the map's key alone does not mean a live
    /// card.
    pub(super) fn is_running(&self) -> bool {
        !matches!(
            self.acc.card_state,
            crate::feishu::card::CardState::Done | crate::feishu::card::CardState::Error
        )
    }

    /// Re-point the live card identity at a new message (ADR-0028: a re-adopt
    /// mid-turn sends a fresh snapshot; the follow renderer keeps updating the
    /// new card instead of the old one). The accumulator content is untouched.
    pub(super) fn repoint(&mut self, message_id: &str) {
        self.card_message_id = Some(message_id.to_string());
        self.acc.reply_to_message_id = Some(message_id.to_string());
        // The fresh snapshot is this chain's newest, growing card.
        self.card_is_live = true;
    }
}

/// The header phase driving the live progress timer. Distinct from
/// [`CardState`]: it tracks whether the card is actively working (thinking /
/// reasoning / a running tool / streaming text) so the timer resets exactly
/// when the visible phase changes (ADR-0014).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HeaderPhase {
    Loading,
    Reasoning,
    Tool,
    Streaming,
}

/// How a turn's card handles a Feishu content rejection (`230099`).
///
/// A rejected card fails on every retry — the platform refuses the same JSON
/// forever — so the flush escalates through these states instead of
/// re-PATCHing the rejected content.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CardFallback {
    /// Normal rendering: model markdown is sanitized for the card parser.
    #[default]
    None,
    /// Feishu rejected the card once: every model-markdown element renders as
    /// a fenced code block — the one form the parser accepts unconditionally.
    Fenced,
    /// The fenced card was rejected too: the same content cannot start
    /// succeeding, so the flush stops PATCHing this card instead of retrying
    /// it on every poll.
    Suspended,
}

impl CardFallback {
    /// Whether model markdown renders fenced.
    pub(super) fn fenced(self) -> bool {
        matches!(self, Self::Fenced | Self::Suspended)
    }
}

/// Accumulates streaming state for one session.
#[derive(Default, Clone)]
pub(super) struct StreamAccumulator {
    pub(super) card_state: CardState,
    /// This turn's card-content fallback (see [`CardFallback`]): starts at
    /// `None` and is advanced by the flush when Feishu rejects a card it
    /// built. A fresh turn starts clean and re-tries the normal rendering.
    pub(super) card_fallback: CardFallback,
    pub(super) text: String,
    pub(super) reasoning: String,
    /// Tool panels keyed by call ID (current state; `timeline` keeps order).
    pub(super) tools: IndexMap<String, ToolPanel>,
    /// Live (unfinished) Tool Panels keyed by call ID: the timeline key,
    /// server start time and element identity allocated when the call first
    /// appeared. A live panel renders in the card TAIL, so a split can never
    /// strand it on a closed card; it joins `timeline` — with the identity it
    /// was born with — only when the tool settles (ADR-0045).
    live_tools: IndexMap<String, LiveTool>,
    /// The latest `todowrite` panel of this turn, rendered as a card-TAIL
    /// status section instead of a timeline row. A timeline row would freeze on
    /// whichever card the call landed on: once that card finalizes (a long
    /// turn splits into several), later updates land on an already-sent card
    /// and stay invisible. The tail rides the live card, so every flush shows
    /// the current list. Each later call replaces it in place.
    pub(super) todo_panel: Option<ToolPanel>,
    /// The server start time of the todowrite call that last refreshed
    /// [`Self::todo_panel`] — its panel header shows when the list was last
    /// written.
    pub(super) todo_shown_at: Option<i64>,
    /// Text, reasoning, tool and receipt entries ordered by their key (the
    /// server-side part start time) — the card is built from this, so message ↔
    /// tool interleaving is preserved even when a part renders late.
    pub(super) timeline: Vec<TimelineItem>,
    /// Fallback key source for pushes without a server part time
    /// ([`Self::next_order`]).
    order_seq: i64,
    /// Highest key inserted so far — keeps [`Self::next_order`] ahead of the
    /// timeline even when a server clock runs ahead of cola's.
    last_key: i64,
    /// Next [`TimelineItem::seq`] to hand out (see [`Self::insert_kind`]).
    item_seq: u64,
    /// The card's interaction section: the live permission/question blocks
    /// (rendered in the tail) and the tombstones of resolved ones (their
    /// receipt is a timeline entry).
    pub(super) interactions: Vec<InteractionBlock>,
    /// Timeline index the CURRENT card starts rendering from. When a card fills
    /// up (Feishu component limit) it is finalized with a "to be continued"
    /// marker and `render_from` advances — a fresh continuation card renders the
    /// remaining timeline from there.
    pub(super) render_from: usize,
    /// Provider ID of the model answering this turn (e.g. "opencode-go").
    pub(super) provider_id: Option<String>,
    /// Model ID of the model answering this turn (e.g. "deepseek-v4-flash").
    pub(super) model_id: Option<String>,
    /// The `/think` variant cola sent this turn (e.g. "high"), shown as
    /// `model@variant` on the footer. Sourced from the session store — the
    /// server reports the model but not the variant.
    pub(super) variant: Option<String>,
    /// Context tokens the model consumed this turn (includes cached prefix), for
    /// the context-usage segment in the card footer.
    pub(super) context_tokens: i64,
    /// The answering model's context-window size (tokens), fetched from
    /// `GET /provider` and memoized for the turn — `None` before the lookup or
    /// when the server reports none (ADR-0044).
    pub(super) context_window: Option<i64>,
    /// The (provider, model) pair [`Self::context_window`] was fetched for; a
    /// mismatch triggers a re-fetch, since the answering model can change.
    pub(super) context_window_key: Option<(String, String)>,
    /// Working directory of the session, shown in the card footer.
    pub(super) directory: Option<String>,
    /// Project name (directory basename) for the Turn Footer's 📁 segment.
    pub(super) project_name: Option<String>,
    /// Git branch: captured at turn start, refreshed at turn end (ADR-0019);
    /// the short commit hash when detached.
    pub(super) branch: Option<String>,
    /// Working tree differs from HEAD, including untracked files: captured at
    /// turn start, refreshed at turn end. Only set alongside `branch`
    /// (ADR-0019: the halves are omitted together).
    pub(super) dirty: bool,
    /// The session/thread name; shown as the card subtitle so the header can
    /// stay focused on state (the question is already in the reply context).
    pub(super) title: String,
    pub(super) error: Option<String>,
    pub(super) reply_to_message_id: Option<String>,
    /// This turn's session id, carried on the error-card retry button so the
    /// card callback can find the accumulator + card to reuse.
    pub(super) session_id: Option<String>,
    /// The full original prompt text of this turn, kept so the error-card
    /// "retry" button can re-submit it without the user retyping.
    pub(super) prompt: Option<String>,
    /// The id this turn's user message carries (`msg_cola_…`, ADR-0026), so a
    /// later error-card retry reuses it and the server deduplicates by id.
    pub(super) cola_message_id: Option<String>,
    /// Who sent the prompt (Feishu open_id), so the group completion notice can
    /// be replied to them / @-mention them.
    pub(super) requester_open_id: Option<String>,
    /// Whether the prompt came from a group chat (completion notice is group-only).
    pub(super) is_group: bool,
    /// The Chat/Topic's turn generation at this turn's start (ADR-0043),
    /// assigned by [`crate::bridge::reminder::ReminderState::begin_turn`]. The Instant
    /// Reminder pin lifecycle reads it so every pin carries the turn it
    /// belongs to and a stale clear cannot unpin a newer turn's pin.
    pub(super) turn_generation: Option<u64>,
    /// Text/reasoning parts already rendered into this card, keyed by their
    /// typed content — dedupes incremental polling. Reasoning/text parts are
    /// written empty first and updated with full text, so they are only tracked
    /// once they have content. Tool panels are deduped separately: the current
    /// panel in `tools` / `todo_panel` IS the tool's rendered revision, so a
    /// re-render happens exactly when the typed call's visible content changed
    /// (including a `todowrite` list rewritten with same-length items).
    pub(super) rendered_parts: std::collections::HashSet<RenderedPart>,
    /// The Turn's anchor, captured as one fact: the identity of the user
    /// message this turn answers together with that message's server time. An
    /// external render arms with the external message's anchor directly; a
    /// cola-sent turn captures it from the stored user message on the first
    /// poll it appears in (matched by `cola_message_id`, ADR-0026). It is the
    /// turn's single anchor: the header date, the turn filter and the renderer
    /// replacement guard all read it, so cola's own clock is never compared
    /// against the server's (#183, #190).
    pub(super) turn_anchor: Option<TurnAnchor>,
    /// ADR-0014: progress/liveness signals for the header.
    /// The active header phase; None when the turn is not actively working
    /// (Done/Error/Continued show no timer).
    pub(super) current_phase: Option<HeaderPhase>,
    /// When the current phase started (wall clock); the header timer counts up
    /// from here.
    pub(super) phase_started_at: Option<std::time::Instant>,
}

impl StreamAccumulator {
    pub(super) fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            reply_to_message_id: None,
            session_id: None,
            prompt: None,
            requester_open_id: None,
            is_group: false,
            rendered_parts: std::collections::HashSet::new(),
            // The card starts loading the moment it is created; the header
            // timer counts from here (ADR-0014).
            current_phase: Some(HeaderPhase::Loading),
            phase_started_at: Some(std::time::Instant::now()),
            ..Default::default()
        }
    }

    /// Capture the turn's work context without touching the accumulator — the
    /// async half of [`Self::attach_work_context`]. Split out so a caller can
    /// insert the live card first (a Supplement must always find one, ADR-0043)
    /// and attach the context after the card's own send, without holding the
    /// cards lock across the git read.
    pub(super) async fn capture_work_context(dir: &str) -> WorkContext {
        if dir.is_empty() {
            return WorkContext::default();
        }
        WorkContext {
            directory: dir.to_string(),
            project_name: crate::git::project_name(dir),
            git: crate::git::read_state(dir).await,
        }
    }

    /// Apply a context captured by [`Self::capture_work_context`].
    pub(super) fn apply_work_context(&mut self, ctx: WorkContext) {
        if ctx.directory.is_empty() {
            self.directory = None;
            return;
        }
        self.directory = Some(ctx.directory);
        self.project_name = ctx.project_name;
        self.apply_git_state(ctx.git);
    }

    /// Capture the turn's work context (ADR-0019): project name, git branch and
    /// dirty state — measured BEFORE the prompt runs, so the dirty flag reflects
    /// the state the AI operates on, not the changes it leaves behind. Best
    /// effort: an empty or non-git directory leaves the fields unset.
    /// `refresh_work_context` re-reads the git halves when the turn ends.
    pub(super) async fn attach_work_context(&mut self, dir: &str) {
        let ctx = Self::capture_work_context(dir).await;
        self.apply_work_context(ctx);
    }

    /// Apply freshly read git state to the work-context halves. The halves move
    /// together and never regress: only a resolved branch overwrites them, so a
    /// failed or empty read (transient git failure, repo gone) keeps the last
    /// known state rather than dropping `branch ⚠` from the footer.
    pub(super) fn apply_git_state(&mut self, state: crate::git::GitState) {
        if let Some(branch) = state.branch {
            self.branch = Some(branch);
            self.dirty = state.dirty;
        }
    }

    /// The header phase for the current state: None when the turn finished or
    /// errored (no timer shown).
    pub(super) fn active_phase(&self) -> Option<HeaderPhase> {
        match self.card_state {
            CardState::Loading => Some(HeaderPhase::Loading),
            CardState::Reasoning => Some(HeaderPhase::Reasoning),
            CardState::Streaming => {
                // A running todowrite is a tail panel, not a timeline tool, but
                // it is still a running tool: ADR-0014 gives it the Tool phase
                // (and the timer reset that comes with it), like any other.
                let running = self.tools.values().any(|t| t.is_running())
                    || self.todo_panel.as_ref().is_some_and(|t| t.is_running());
                if running {
                    Some(HeaderPhase::Tool)
                } else {
                    Some(HeaderPhase::Streaming)
                }
            }
            _ => None,
        }
    }

    /// Reset the phase timer whenever the active header phase changes.
    /// Idempotent — safe to call after every mutation.
    pub(super) fn refresh_phase(&mut self) {
        let new = self.active_phase();
        if new != self.current_phase {
            self.current_phase = new;
            self.phase_started_at = Some(std::time::Instant::now());
        }
    }

    /// Add an interaction block to the card unless a block for the same
    /// request is already present (the poll loop and the adopt-time snapshot
    /// both feed blocks in). Live blocks render in the card's tail; resolving
    /// one inserts its receipt into the timeline. Returns whether it was added.
    pub(super) fn add_interaction(&mut self, block: InteractionBlock) -> bool {
        if self.interaction(block.request_id()).is_some() {
            return false;
        }
        self.interactions.push(block);
        true
    }

    /// The block for `request_id`, if the card carries one.
    pub(super) fn interaction(&self, request_id: &str) -> Option<&InteractionBlock> {
        self.interactions.iter().find(|b| b.request_id() == request_id)
    }

    /// The index of the live block `request_id` names, if it is still there.
    fn live_index(&self, request_id: &str) -> Option<usize> {
        self.interactions
            .iter()
            .position(|b| b.request_id() == request_id && b.is_live())
    }

    /// Resolve the live block for `request_id` (ADR-0038, rule 4): the block's
    /// entry becomes a tombstone for dedupe, and its Interaction Receipt is
    /// inserted into the timeline, keyed at the RESOLUTION moment — not at the
    /// moment the block surfaced. The request poll can put a block on the card
    /// before the render poll has rendered the command and reasoning it belongs
    /// to, so a surfacing-time position would sit above its own request;
    /// resolution time is what the operator saw when they clicked, and the
    /// keyed timeline puts anything the server ordered before that click above
    /// the receipt no matter when it rendered. `line` derives the receipt text
    /// from the block being resolved, so the residue always names the target it
    /// actually resolved. Returns false when no live block matched (already
    /// resolved, or never on this card).
    pub(super) fn resolve_interaction(
        &mut self,
        request_id: &str,
        line: impl FnOnce(&InteractionBlock) -> String,
    ) -> bool {
        let Some(idx) = self.live_index(request_id) else {
            return false;
        };
        let text = line(&self.interactions[idx]);
        // `next_order` is the resolution moment with a monotonic guard: never
        // before the wall clock, never behind anything already on the card —
        // so a click's receipt always follows the content the Host was looking
        // at, even within the same millisecond. The key orders only: a receipt
        // resolves in cola's world, where no server time exists, and it shows
        // no clock rather than putting cola's in the panels' costume.
        let key = self.next_order();
        self.insert_kind(key, None, TimelineKind::Receipt(text));
        self.interactions[idx] = InteractionBlock::Receipt(request_id.to_string());
        true
    }

    /// Resolve a live block WITHOUT a timeline receipt of its own: a mode change
    /// (the Auto-Accept toggle) resolves several blocks at once and leaves ONE
    /// receipt covering them all, so the others only need their tombstone.
    /// Returns false when no live block matched (already resolved, or never on
    /// this card).
    pub(super) fn dismiss_interaction(&mut self, request_id: &str) -> bool {
        let Some(idx) = self.live_index(request_id) else {
            return false;
        };
        self.interactions[idx] = InteractionBlock::Receipt(request_id.to_string());
        true
    }

    /// Append one Interaction Receipt keyed at the resolution moment — the
    /// single residue a mode change leaves for every block it resolved.
    pub(super) fn push_receipt(&mut self, text: &str) {
        let key = self.next_order();
        self.insert_kind(key, None, TimelineKind::Receipt(text.to_string()));
    }

    /// Replace a question block's display state (the live 已选/✅ markers) in
    /// place. Returns false when the card has no question block for the
    /// request.
    pub(super) fn update_question_state(
        &mut self,
        request_id: &str,
        answers: &[Option<Vec<String>>],
        done: &[bool],
    ) -> bool {
        match self
            .interactions
            .iter_mut()
            .find(|b| b.request_id() == request_id)
        {
            Some(InteractionBlock::Question(q)) => {
                q.answers = answers.to_vec();
                q.done = done.to_vec();
                true
            }
            _ => false,
        }
    }

    /// Resolve every live block `vanished` accepts into its Interaction Receipt
    /// — a kind's sweep passes only its own kind's vanished blocks (see
    /// `RequestKind::resolve_vanished_inline`), so the receipt is written at
    /// the resolution moment and the tombstone keeps a racing poll from
    /// re-surfacing the block. Returns how many blocks were resolved; the
    /// caller repaints each affected card (ADR-0038, rule 5).
    pub(super) fn resolve_vanished(
        &mut self,
        vanished: impl Fn(&InteractionBlock) -> bool,
        line: impl Fn(&InteractionBlock) -> String,
    ) -> usize {
        let ids: Vec<String> = self
            .interactions
            .iter()
            .filter(|b| b.is_live() && vanished(b))
            .map(|b| b.request_id().to_string())
            .collect();
        for id in &ids {
            self.resolve_interaction(id, &line);
        }
        ids.len()
    }

    /// The live permission blocks, in render order. Currently test-only: the
    /// production paths mutate through the seam, and later tickets that need
    /// the view drop the `cfg` (ADR-0038 follow-ups).
    #[cfg(test)]
    pub(super) fn live_permissions(&self) -> Vec<PendingPermission> {
        self.interactions
            .iter()
            .filter_map(|b| match b {
                InteractionBlock::Permission(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// The live question blocks, in render order. Test-only like
    /// [`Self::live_permissions`].
    #[cfg(test)]
    pub(super) fn live_questions(&self) -> Vec<PendingQuestion> {
        self.interactions
            .iter()
            .filter_map(|b| match b {
                InteractionBlock::Question(q) => Some(q.clone()),
                _ => None,
            })
            .collect()
    }

    /// Which request kinds are live on the card — the header's awaiting state
    /// (ADR-0014). A permission and a question pending at once report `Both`,
    /// so the title names exactly what the operator must resolve.
    pub(super) fn awaiting_action(&self) -> AwaitingAction {
        let mut permission = false;
        let mut question = false;
        for block in &self.interactions {
            match block {
                InteractionBlock::Permission(_) => permission = true,
                InteractionBlock::Question(_) => question = true,
                InteractionBlock::Receipt(_) => {}
            }
        }
        match (permission, question) {
            (true, true) => AwaitingAction::Both,
            (true, false) => AwaitingAction::Permission,
            (false, true) => AwaitingAction::Question,
            (false, false) => AwaitingAction::None,
        }
    }

    /// Progress inputs for the header (ADR-0014): which request kinds await the
    /// operator, phase timer, and reasoning length. Elapsed is whole seconds so
    /// the header signature changes at most once per second — the flush
    /// throttle.
    pub(super) fn header_progress(&self) -> crate::feishu::card::HeaderProgress {
        crate::feishu::card::HeaderProgress {
            awaiting: self.awaiting_action(),
            elapsed: self.phase_started_at.map(|t| t.elapsed().as_secs()),
            reasoning_chars: self.reasoning.chars().count(),
        }
    }

    /// The tool the Turn is currently running: a live `tools` panel, else the
    /// live todo panel. This is TURN state, not slice state — the header uses
    /// it even on a card whose slice does not contain the panel, so a split
    /// continuation keeps saying "⏳ tool" instead of falling back to
    /// "回复中". A tool that finished is not `running`, so it drops out.
    /// Selection is deterministic: `IndexMap` preserves insertion order, so
    /// the first running tool (the one the Turn started first) wins.
    fn running_tool(&self) -> Option<&ToolPanel> {
        self.tools
            .values()
            .find(|t| t.is_running())
            .or_else(|| self.todo_panel.as_ref().filter(|t| t.is_running()))
    }

    /// The header (title, template) this accumulator's card should show: the
    /// state label, the awaiting-action override, the phase timer and the
    /// running-tool hint. Exposed so a click's ack can restamp a card's header
    /// in the same response — resolving the last live block must lift the
    /// "等待你的授权/回答" title immediately, not a poll later.
    pub(super) fn header_title_and_template(&self) -> (String, &'static str) {
        crate::feishu::card::shell::header_title_and_template(
            &self.card_state,
            self.running_tool(),
            &self.header_progress(),
        )
    }

    /// The card header's signature (title + template), compared across polls
    /// to decide whether the header changed enough to re-flush (ADR-0014).
    pub(super) fn header_sig(&self) -> String {
        let (title, template) = self.header_title_and_template();
        format!("{}|{}", title, template)
    }

    /// The memoized window, but only when it was fetched for the model that
    /// produced [`Self::context_tokens`]. A stale denominator paired with a
    /// newer model's usage would silently lie; ADR-0044 says an unknown window
    /// renders the used tokens alone.
    fn current_context_window(&self) -> Option<i64> {
        let (Some(provider), Some(model)) = (&self.provider_id, &self.model_id) else {
            return None;
        };
        match &self.context_window_key {
            Some((p, m)) if p == provider && m == model => self.context_window.filter(|w| *w > 0),
            _ => None,
        }
    }

    /// The context segment's render inputs `(used tokens, effective window)` —
    /// the signature the flush compares. [`Self::context_segment`] renders from
    /// exactly this, so the two cannot drift.
    pub(super) fn context_sig(&self) -> (i64, Option<i64>) {
        (self.context_tokens, self.current_context_window())
    }

    /// The Turn Footer's context-usage segment (ADR-0044), derived from the
    /// latest usage and the memoized window: `📊 上下文 84k/200k (42%)`, or the
    /// used tokens alone when the server reports no window. `None` until the
    /// first usage lands — a step that has not finished has no token data.
    pub(super) fn context_segment(&self) -> Option<String> {
        let (used, window) = self.context_sig();
        if used <= 0 {
            return None;
        }
        let used = format_tokens(used);
        match window {
            Some(window) => {
                let ratio = (self.context_tokens as f64 / window as f64).clamp(0.0, 1.0);
                Some(format!(
                    "📊 上下文 {}/{} ({:.0}%)",
                    used,
                    format_tokens(window),
                    ratio * 100.0
                ))
            }
            None => Some(format!("📊 上下文 {}", used)),
        }
    }

    /// Insert a timeline entry at its key's position: after every entry whose
    /// part started no later, before the ones that started later. The poll can
    /// deliver parts out of order (a panel whose content lands after later
    /// parts), so appending would put it after content that follows it — the
    /// ordering bug that moved a gated command below its receipt. Equal keys
    /// keep insertion order.
    ///
    /// A key behind the live slice inserts at its start: the finalized card
    /// that owned that position is already sent, and the top of the live card
    /// is the closest honest place left. Both the index AND the key clamp then
    /// (to the live slice's first key), so the timeline stays sorted and key
    /// lookups stay sound. `shown_at` records the part's server start time
    /// (or `None` for a synthetic key) as the instant a panel header may
    /// display.
    fn insert_kind(&mut self, key: i64, shown_at: Option<i64>, kind: TimelineKind) {
        self.item_seq += 1;
        self.insert_item(key, shown_at, self.item_seq, kind);
    }

    /// [`Self::insert_kind`] with a pre-allocated identity: a live Tool Panel
    /// keeps the seq it was born with when it settles into the timeline, so
    /// its card element — and the reader's fold state — survives the move
    /// (ADR-0045).
    fn insert_item(&mut self, key: i64, shown_at: Option<i64>, seq: u64, kind: TimelineKind) {
        let idx = self.timeline.partition_point(|item| item.key <= key);
        let (idx, key) = if idx < self.render_from {
            (
                self.render_from,
                self.timeline.get(self.render_from).map_or(key, |i| i.key),
            )
        } else {
            (idx, key)
        };
        self.item_seq = self.item_seq.max(seq);
        self.timeline.insert(
            idx,
            TimelineItem {
                key,
                shown_at,
                seq,
                kind,
            },
        );
        self.last_key = self.last_key.max(key);
    }

    /// Key for a push with no server part time (tests, synthetic content), and
    /// for an Interaction Receipt's resolution moment: strictly increasing,
    /// never before the wall clock, and never behind a key already inserted —
    /// so it keeps call order and stays after everything already on the card
    /// even under clock skew between cola and the server.
    pub(super) fn next_order(&mut self) -> i64 {
        self.order_seq = self
            .order_seq
            .saturating_add(1)
            .max(self.last_key.saturating_add(1))
            .max(chrono::Utc::now().timestamp_millis());
        self.order_seq
    }

    /// The timeline index of the entry keyed `key`, if one exists (equal keys
    /// are contiguous, so the predecessor of the insertion point is it).
    fn item_with_key(&self, key: i64) -> Option<usize> {
        let idx = self.timeline.partition_point(|item| item.key <= key);
        (idx > 0 && self.timeline.get(idx - 1).is_some_and(|i| i.key == key)).then(|| idx - 1)
    }

    /// Append a text chunk, keeping it in the chronological timeline (merging
    /// consecutive text chunks so the card doesn't produce one element each).
    /// Text is chunked so no single timeline item exceeds `MAX_CARD_TEXT_CHARS`
    /// — the card splitter can then break a long answer across cards at item
    /// boundaries instead of truncating it. Production renders go through
    /// [`Self::push_text_at`] for chunks with no server part time; this
    /// convenience form is test-only.
    #[cfg(test)]
    pub(super) fn push_text(&mut self, chunk: &str) {
        self.push_text_at(None, chunk);
    }

    /// [`Self::push_text`] for a chunk from the part that started at `at_ms`
    /// (the server's `time.start`), which also places it on the timeline.
    /// `None` for a payload with no server time: the item is then keyed by a
    /// monotonic fallback (call order) and shows no clock.
    pub(super) fn push_text_at(&mut self, at_ms: Option<i64>, chunk: &str) {
        let key = at_ms.unwrap_or_else(|| self.next_order());
        self.text.push_str(chunk);
        let max = crate::feishu::card::MAX_CARD_TEXT_CHARS;
        let mut remaining = chunk;
        while !remaining.is_empty() {
            let space = match self.item_with_key(key).and_then(|i| self.timeline.get(i)) {
                Some(TimelineItem {
                    kind: TimelineKind::Text(last),
                    ..
                }) => max.saturating_sub(last.chars().count()),
                _ => 0,
            };
            if space == 0 {
                let take: String = remaining.chars().take(max).collect();
                self.insert_kind(key, at_ms, TimelineKind::Text(take.clone()));
                remaining = &remaining[take.len()..];
                continue;
            }
            let take: String = remaining.chars().take(space).collect();
            let idx = self.item_with_key(key).expect("space came from it");
            match self.timeline.get_mut(idx) {
                Some(TimelineItem {
                    kind: TimelineKind::Text(last),
                    ..
                }) => last.push_str(&take),
                _ => self.insert_kind(key, at_ms, TimelineKind::Text(take.clone())),
            }
            remaining = &remaining[take.len()..];
        }
    }

    /// Append a reasoning chunk, keeping it in the chronological timeline
    /// (chunks of the same part merge into one panel; a part keyed apart is
    /// its own panel). Production renders go through [`Self::push_reasoning_at`];
    /// this convenience form is for tests and synthetic content.
    #[cfg(test)]
    pub(super) fn push_reasoning(&mut self, chunk: &str) {
        self.push_reasoning_at(None, chunk);
    }

    /// [`Self::push_reasoning`] for a reasoning part that started at `at_ms`
    /// (the server's `time.start`); `None` keys it by fallback and shows no
    /// clock.
    pub(super) fn push_reasoning_at(&mut self, at_ms: Option<i64>, chunk: &str) {
        let key = at_ms.unwrap_or_else(|| self.next_order());
        self.reasoning.push_str(chunk);
        let idx = self.item_with_key(key);
        match idx.and_then(|i| self.timeline.get_mut(i)) {
            Some(TimelineItem {
                kind: TimelineKind::Reasoning(last),
                ..
            }) => last.push_str(chunk),
            _ => self.insert_kind(key, at_ms, TimelineKind::Reasoning(chunk.to_string())),
        }
    }

    /// Insert/update a tool panel; the timeline gets a marker only on the FIRST
    /// appearance (state updates re-render in place). Production renders go
    /// through [`Self::push_tool_at`]; this convenience form is test-only.
    #[cfg(test)]
    pub(super) fn push_tool(&mut self, call_id: &str, panel: ToolPanel) {
        self.push_tool_at(None, call_id, panel);
    }

    /// [`Self::push_tool`] for a tool call that started at `at_ms` (the typed
    /// [`ToolCall::started_at`](crate::backend::ToolCall)). A tool first seen
    /// before the server stamped it (a pending call, no time) gains its start
    /// time on the later update: a live panel adopts it as its timeline key —
    /// nothing is placed yet — while a panel already in the timeline keeps its
    /// key and only gains the clock. A call with no server time keeps showing
    /// no clock.
    pub(super) fn push_tool_at(&mut self, at_ms: Option<i64>, call_id: &str, panel: ToolPanel) {
        let live = panel.is_live();
        let is_new = !self.tools.contains_key(call_id);
        self.tools.insert(call_id.to_string(), panel);
        if is_new {
            let key = at_ms.unwrap_or_else(|| self.next_order());
            if live {
                // Unfinished: TAIL content. Allocate its identity and key now,
                // but do not join the timeline — a split must never finalize a
                // panel whose tool is still running (ADR-0045).
                self.item_seq += 1;
                self.live_tools.insert(
                    call_id.to_string(),
                    LiveTool {
                        key,
                        shown_at: at_ms,
                        seq: self.item_seq,
                    },
                );
            } else {
                self.insert_kind(key, at_ms, TimelineKind::Tool(call_id.to_string()));
            }
        } else if self.live_tools.contains_key(call_id) {
            // A later sighting may carry the server start time the first one
            // lacked (a pending call with no time). The panel is not on
            // the timeline yet, so the real start time REPLACES the synthetic
            // fallback key — the settle move must land where the call started.
            if let Some(at) = at_ms {
                let entry = self.live_tools.get_mut(call_id).expect("checked");
                entry.shown_at = entry.shown_at.or(at_ms);
                entry.key = at;
            }
            if !live {
                // Settled: the panel joins the timeline at the key it was born
                // with — clamped to the top of the live slice when that key is
                // behind it (ADR-0038's late-part rule) — carrying the element
                // identity it was born with, so its fold state survives.
                let entry = self.live_tools.shift_remove(call_id).expect("checked");
                self.insert_item(
                    entry.key,
                    entry.shown_at,
                    entry.seq,
                    TimelineKind::Tool(call_id.to_string()),
                );
            }
        } else if at_ms.is_some()
            && let Some(item) = self
                .timeline
                .iter_mut()
                .find(|i| matches!(&i.kind, TimelineKind::Tool(id) if id == call_id))
        {
            item.shown_at = item.shown_at.or(at_ms);
        }
        // A running-tool phase starts/exits here (running → completed), so the
        // header timer must follow even though card_state stays Streaming.
        self.refresh_phase();
    }

    /// Build the whole card (tests + simple callers). Assembles the full
    /// timeline with the tail sections (inline interactions, buttons, footer).
    #[cfg(test)]
    pub(super) fn build_card(&self) -> serde_json::Value {
        self.build_card_inner(0, self.timeline.len(), true, None).0
    }

    /// Build the card from `render_from` onward, splitting when the estimated
    /// component count would exceed Feishu's card limit. When `full`, the card
    /// is finalized with the "部分完成，继续中…" header state and `render_from`
    /// has advanced past the split point — the caller then sends a fresh
    /// continuation card. When not full, the card includes the tail sections
    /// and is the turn's final visible card. The spans name each live
    /// interaction block's element range, for the card handle (ADR-0038,
    /// rule 2).
    pub(super) fn build_card_with_info(&mut self) -> BuiltCard {
        // The slice that fits on its own. When items remain, it must hold at
        // least one: an empty slice would leave `render_from` frozen and the
        // flush loop would re-send empty "部分完成" cards forever.
        let mut split = self.estimate_split_index(self.render_from, false);
        if self.render_from < self.timeline.len() {
            split = split.max(self.render_from + 1);
        }
        let mut full = split < self.timeline.len();
        // The tail rides only a non-full (live) card. When the remainder fits
        // without the tail but not with it — a long todo list, or running tool
        // panels (ADR-0045) — this card finalizes without the tail, sized so
        // the next card can carry it, instead of overflowing Feishu's limit.
        if !full {
            let with_tail = self.estimate_split_index(self.render_from, true);
            if with_tail < split {
                split = with_tail.max(self.render_from + 1).min(split);
                full = true;
            }
        }
        let state = if full { Some(CardState::Continued) } else { None };
        let (card, spans) = self.build_card_inner(self.render_from, split, !full, state);
        // Advance `render_from` ONLY on an actual split: while the card still
        // fits, subsequent flushes must re-render from the SAME start so the
        // content accumulates instead of only showing the latest delta.
        if full {
            self.render_from = split;
        }
        BuiltCard { card, full, spans }
    }

    /// [`Self::build_card_with_info`] for callers that don't need the block
    /// spans (the click ack's split probe, tests).
    pub(super) fn build_card_with_split(&mut self) -> (serde_json::Value, bool) {
        let built = self.build_card_with_info();
        (built.card, built.full)
    }

    /// Build the card's LIVE slice (`render_from` to the end) as a finalized
    /// card — the split header, no tail — and ADVANCE `render_from` past it.
    /// A Supplement forces this split even though the slice fits (ADR-0043):
    /// the finalized card keeps everything before the split, and the
    /// continuation renders only the delta appended afterwards — the same
    /// handoff a size split performs. Receipts queued after this build land
    /// past the new boundary, so they ride the continuation.
    pub(super) fn build_finalized_handoff(&mut self) -> serde_json::Value {
        let end = self.timeline.len();
        let card = self
            .build_card_inner(self.render_from, end, false, Some(CardState::Continued))
            .0;
        self.render_from = end;
        card
    }

    /// The request ids of every block the accumulator still awaits. A block
    /// resident here is the accumulator's own render source — the sweep
    /// resolves it through the timeline, not through the card handle.
    pub(super) fn live_request_ids(&self) -> Vec<&str> {
        self.interactions
            .iter()
            .filter(|b| b.is_live())
            .map(InteractionBlock::request_id)
            .collect()
    }

    /// First timeline index whose items would push the estimated component
    /// count over `MAX_CARD_COMPONENTS`, the estimated JSON size over
    /// `MAX_CARD_JSON_CHARS`, or the accumulated text over
    /// `MAX_CARD_TEXT_CHARS` (Feishu caps total card size too, not just the
    /// element count). Returns `timeline.len()` when everything fits.
    ///
    /// The size estimate is in **UTF-8 bytes** — Feishu rejects on bytes, not
    /// characters, so CJK content (3 bytes/char) counts 3x what a char-count
    /// estimate would. It adds each element's serialized JSON overhead
    /// (measured ~260-340 bytes for a collapsible panel, ~35 for a markdown
    /// element) so the estimate trails the real card size by only a few
    /// hundred bytes — the `MAX_CARD_JSON_CHARS` margin absorbs the rest.
    ///
    /// `reserve_tail` charges the panels the built card will ALSO carry in its
    /// tail — the todo list and every running tool's panel (ADR-0045) — to the
    /// same budget, keeping the card and its tail together under the cap.
    /// Callers pass it only when the slice is the final, tail-carrying one —
    /// see [`Self::build_card_with_info`].
    fn estimate_split_index(&self, start: usize, reserve_tail: bool) -> usize {
        // Byte length of the first `n` chars (mirrors `truncate_md`, which caps
        // rendered content by characters).
        let first_n_bytes = |s: &str, n: usize| s.chars().take(n).map(|c| c.len_utf8()).sum::<usize>();
        let mut comps = 0usize;
        let mut size = 0usize;
        let mut card_text = 0usize;
        if reserve_tail {
            // The tail rides only the live card, but it counts against that
            // card's budget: the todo list, then every running tool's panel
            // (ADR-0045). When they don't fit, the card finalizes without the
            // tail and the continuation carries it.
            if let Some(panel) = &self.todo_panel {
                comps += 1;
                size += panel_estimate(panel);
            }
            for call_id in self.live_tools.keys() {
                if let Some(panel) = self.tools.get(call_id) {
                    comps += 1;
                    size += panel_estimate(panel);
                }
            }
        }
        for (i, item) in self.timeline.iter().enumerate().skip(start) {
            let (c, s, t) = match &item.kind {
                TimelineKind::Reasoning(r) => (4, 300 + first_n_bytes(r, 800), 0),
                TimelineKind::Tool(call_id) => {
                    (4, self.tools.get(call_id).map(panel_estimate).unwrap_or(400), 0)
                }
                // Text is chunked to ≤ MAX_CARD_TEXT_CHARS per item, so a
                // single item never exceeds the per-card budget; the budget
                // accumulates across items and splits at the boundary. The
                // component estimate mirrors `with_text`, which splits each
                // element at MAX_ELEMENT_TEXT_CHARS.
                TimelineKind::Text(t) => {
                    let chars = t.chars().count();
                    let elements = chars.div_ceil(crate::feishu::card::MAX_ELEMENT_TEXT_CHARS);
                    (elements, t.len() + 40, chars)
                }
                // A receipt is one small markdown line.
                TimelineKind::Receipt(line) => (1, line.len() + 40, 0),
            };
            comps += c;
            size += s;
            card_text += t;
            if comps > crate::feishu::card::MAX_CARD_COMPONENTS
                || size > crate::feishu::card::MAX_CARD_JSON_CHARS
                || card_text > crate::feishu::card::MAX_CARD_TEXT_CHARS
            {
                return i;
            }
        }
        self.timeline.len()
    }

    /// Assemble the card JSON for `timeline[start..end]`. `include_tail` adds
    /// the non-timeline sections (inline permission/question, error, retry
    /// button) — only the live card should carry them. `state_override` forces
    /// the header state (e.g. "部分完成" on split cards). Returns the card and
    /// the element range of every live block the tail rendered (empty without a
    /// tail).
    fn build_card_inner(
        &self,
        start: usize,
        end: usize,
        include_tail: bool,
        state_override: Option<CardState>,
    ) -> (serde_json::Value, Vec<crate::bridge::card_handles::BlockSpan>) {
        let state = state_override.unwrap_or_else(|| self.card_state.clone());
        // The header shows the Turn's running tool even when THIS slice has no
        // panel for it (a split continuation after `sleep 30` started): pass
        // the accumulator's global selection as the builder's override.
        let mut builder = CardBuilder::new()
            .with_state(state)
            .with_progress(self.header_progress())
            .with_fenced_markdown(self.card_fallback.fenced())
            .with_header_running_tool(self.running_tool().cloned());

        // The card is a reply to the user's message, so the session/thread name
        // goes in the subtitle and the question is NOT echoed again. The date
        // anchor is the turn's SERVER time (#183 follow-up) — never cola's —
        // so it can't disagree with the panels and stays stable across flushes.
        builder = builder.with_subtitle(&self.title);
        if let Some(date) = self
            .turn_anchor
            .as_ref()
            .and_then(|anchor| crate::feishu::card::fmt_local_date(anchor.created_ms))
        {
            builder = builder.with_date(&date);
        }

        // Render text, reasoning, tool panels and receipts in the timeline's
        // key order (interleaved), like OpenChamber shows the parts. Text is
        // chunked at push time and bounded per card by `estimate_split_index`,
        // so everything in [start..end) renders in full — no preview
        // truncation, no separate plain-text message.
        let mut pending = String::new();
        for item in self.timeline.iter().take(end).skip(start) {
            match &item.kind {
                TimelineKind::Reasoning(r) => {
                    if !pending.is_empty() {
                        builder = builder.with_text(&pending);
                        pending.clear();
                    }
                    builder =
                        builder.with_reasoning_at(r, item.shown_at, Some(&format!("reason_{}", item.seq)));
                }
                TimelineKind::Text(t) => {
                    pending.push_str(t);
                }
                TimelineKind::Tool(call_id) => {
                    if !pending.is_empty() {
                        builder = builder.with_text(&pending);
                        pending.clear();
                    }
                    if let Some(panel) = self.tools.get(call_id) {
                        builder = builder.with_tool_at(
                            panel.clone(),
                            item.shown_at,
                            Some(&format!("tool_{}", item.seq)),
                        );
                    }
                }
                // A resolved block's residue: one receipt line, no controls,
                // in the transcript position it was resolved at (ADR-0038,
                // rule 4).
                TimelineKind::Receipt(line) => {
                    if !pending.is_empty() {
                        builder = builder.with_text(&pending);
                        pending.clear();
                    }
                    builder = builder.with_text(line);
                }
            }
        }
        if !pending.is_empty() {
            builder = builder.with_text(&pending);
        }

        let mut spans: Vec<crate::bridge::card_handles::BlockSpan> = Vec::new();
        if include_tail {
            // The live todo list opens the tail: it is the turn's current plan,
            // and the tail is the one section every flush re-renders, so the
            // list always lands on the LIVE card (a timeline row would freeze
            // on a finalized one). The block spans below are recorded from
            // `builder.body_len()`, so they stay correct with it in front.
            if let Some(todo) = &self.todo_panel {
                builder = builder.with_tool_at(todo.clone(), self.todo_shown_at, Some("todo"));
            }
            // Running tools are live content, so their panels ride the tail —
            // after the todo list and before the interaction blocks, keeping a
            // running tool's Permission/Question below its own panel. A split
            // finalizes the card without them; they continue on the newest
            // card and join the timeline once they settle (ADR-0045).
            for (call_id, live) in &self.live_tools {
                if let Some(panel) = self.tools.get(call_id) {
                    builder = builder.with_tool_at(
                        panel.clone(),
                        live.shown_at,
                        Some(&format!("tool_{}", live.seq)),
                    );
                }
            }
            // The card's tail: the live interaction blocks, in accumulated
            // order. A permission renders its buttons right here (the whole
            // turn lives on one card); a question renders its current display
            // answers. Resolved blocks are timeline receipts above, not here —
            // their entry is only a tombstone that keeps a racing poll from
            // re-surfacing the request. Each live block's element range is
            // recorded for the card handle.
            for block in &self.interactions {
                let start = builder.body_len();
                match block {
                    InteractionBlock::Permission(p) => {
                        builder = builder.with_text(&format!("🔐 **权限请求**\n{}", p.body));
                        for btn in crate::feishu::card::question::permission_buttons(
                            &p.session_id,
                            &p.request_id,
                            &p.body,
                            &p.directory,
                        ) {
                            builder = builder.with_element(btn);
                        }
                    }
                    InteractionBlock::Question(q) => {
                        for el in crate::feishu::card::question::question_elements(
                            &q.request_id,
                            &q.session_id,
                            &q.questions,
                            &q.directory,
                            &q.answers,
                            &q.done,
                        ) {
                            builder = builder.with_element(el);
                        }
                    }
                    InteractionBlock::Receipt(_) => continue,
                }
                spans.push(crate::bridge::card_handles::BlockSpan {
                    request_id: block.request_id().to_string(),
                    start,
                    end: builder.body_len(),
                });
            }

            if let Some(ref err) = self.error {
                builder = builder.with_text(&format!("\n**错误**: {}", err));
            }

            // Error card: offer a retry that re-submits the original prompt on
            // the same card, so the user doesn't have to retype it.
            if self.card_state == CardState::Error
                && let Some(sid) = &self.session_id
            {
                builder = builder.with_error_buttons(vec![crate::feishu::card::CardActionButton {
                    text: "🔄 重试".to_string(),
                    kind: "primary",
                    value: serde_json::json!({ "action": "retry", "session_id": sid }),
                }]);
            }
        }

        // Card footer — work context (ADR-0019). The 📁 segment (project ·
        // branch · dirty) is captured at turn start — so a wrong-branch run is
        // visible before it completes — and refreshed at turn end, so the final
        // card shows where the turn landed (a branch the AI created or switched
        // to, and whether it left uncommitted work). The 🤖 model and the 📊
        // context usage are captured while the turn streams, so both render on
        // EVERY card — including a split "部分完成" one — from the moment their
        // data exists (ADR-0020, ADR-0044).
        let mut footer_parts: Vec<String> = Vec::new();
        if let Some(dir) = &self.directory {
            let name = self.project_name.as_deref().unwrap_or(dir);
            let mut segment = format!("📁 {}", crate::feishu::message::strip_mention_tokens(name));
            match (&self.branch, self.dirty) {
                (Some(branch), true) => segment.push_str(&format!(" · {} ⚠", branch)),
                (Some(branch), false) => segment.push_str(&format!(" · {}", branch)),
                // git.rs guarantees dirty is only set alongside a branch
                // (ADR-0019: the halves are omitted together), so a missing
                // branch renders just the project name.
                (None, _) => {}
            }
            footer_parts.push(segment);
        }
        if let Some(model) = &self.model_id {
            let label = match self.provider_id.as_deref() {
                Some(p) => format!("{}/{}", p, model),
                None => model.clone(),
            };
            // The variant appends as `provider/model@variant` — provider is
            // part of model identity, and the variant is what cola sent
            // this turn (ADR-0020).
            let label = match &self.variant {
                Some(v) => format!("{}@{}", label, v),
                None => label,
            };
            footer_parts.push(format!("🤖 {}", label));
        }
        if let Some(segment) = self.context_segment() {
            footer_parts.push(segment);
        }
        if !footer_parts.is_empty() {
            builder = builder.with_footer(&footer_parts.join(" · "));
        }

        (builder.build(), spans)
    }
}

/// The shared sweep shape for both kinds (#175): resolve every LIVE block the
/// kind owns (`own`) whose request vanished into its
/// `⏱ 已由其他客户端处理` Interaction Receipt. A block owned by a directory
/// whose list call failed stays: that directory said nothing, so its request
/// may still be pending (#130, #144). A block cola itself is answering (or
/// answered) also stays: its disappearance from the pending list is cola's own
/// doing, and the sweep's neutral line would be a lie — the settlement leaves
/// the true receipt. Returns how many were resolved — the caller repaints each
/// affected card so the receipt lands within one poll.
pub(super) fn resolve_vanished_blocks(
    acc: &mut StreamAccumulator,
    pending: &std::collections::HashSet<String>,
    failed_dirs: &std::collections::HashSet<String>,
    cola_claimed: &std::collections::HashSet<String>,
    own: impl Fn(&InteractionBlock) -> bool,
) -> usize {
    acc.resolve_vanished(
        |block| {
            own(block)
                && !pending.contains(block.request_id())
                && !cola_claimed.contains(block.request_id())
                && !failed_dirs.contains(block.directory())
        },
        |block| crate::bridge::request::delivery::handled_elsewhere_receipt(&block.receipt_target()),
    )
}

/// Refresh the live card's work context at turn end (ADR-0019): re-read the
/// session directory's git state so the final card shows where the turn landed
/// — a branch the AI created or switched to, and whether it left uncommitted
/// work. The read shells out to git, so it runs OUTSIDE the cards lock; the
/// lock only wraps the field swap. Best effort: a missing card or directory is
/// a no-op, and a failed read keeps the start capture (`apply_git_state`).
pub(super) async fn refresh_work_context(cards: &CardsHandle, session_id: &str) {
    let dir = {
        let live = cards.cards.lock().await;
        live.get(session_id).and_then(|c| c.acc.directory.clone())
    };
    let Some(dir) = dir else { return };
    let state = crate::git::read_state(&dir).await;
    let mut live = cards.cards.lock().await;
    if let Some(card) = live.get_mut(session_id) {
        card.acc.apply_git_state(state);
    }
}

/// Fetch and memoize the answering model's context-window size for the live
/// turn (ADR-0044) — the denominator of the footer's 📊 segment. The lookup is
/// the only cost of the live refresh and is paid at most once per
/// (provider, model) per turn: the accumulator remembers what it was fetched
/// for, so later polls are a no-op. The request runs OUTSIDE the cards lock
/// (network); the lock only wraps the memo swap. Best effort: a missing card,
/// no usage yet, or a failed request leaves the memo unset so a later poll
/// retries.
pub(super) async fn refresh_context_window(
    cards: &CardsHandle,
    backend: &Arc<dyn opencode::Backend>,
    session_id: &str,
) {
    let key = {
        let live = cards.cards.lock().await;
        let Some(acc) = live.get(session_id).map(|c| &c.acc) else {
            return;
        };
        // Before the first usage there is nothing any card could show.
        if acc.context_tokens <= 0 {
            return;
        }
        let (Some(provider), Some(model)) = (&acc.provider_id, &acc.model_id) else {
            return;
        };
        let key = (provider.clone(), model.clone());
        if acc.context_window_key.as_ref() == Some(&key) {
            return;
        }
        key
    };
    let Ok(window) = backend.model_context_window(&key.0, &key.1).await else {
        return;
    };
    let mut live = cards.cards.lock().await;
    if let Some(acc) = live.get_mut(session_id).map(|c| &mut c.acc)
        // A concurrent refresh may have moved the memo to a newer model while
        // this request was in flight; never clobber it with a stale answer.
        && acc
            .context_window_key
            .as_ref()
            .is_none_or(|current| current == &key)
    {
        acc.context_window_key = Some(key);
        acc.context_window = window;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ToolStatus;
    use crate::feishu::card::MAX_CARD_TEXT_CHARS;

    #[test]
    fn card_footer_shows_directory_model_and_context() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.directory = Some("/root/workspace/dev/cola".into());
        acc.provider_id = Some("opencode-go".into());
        acc.model_id = Some("deepseek-v4-flash".into());
        acc.context_tokens = 84_200;
        acc.context_window = Some(200_000);
        acc.context_window_key = Some(("opencode-go".into(), "deepseek-v4-flash".into()));
        acc.push_text("结果");

        let card = acc.build_card();
        let text = card.to_string();
        assert!(
            text.contains("📁 /root/workspace/dev/cola"),
            "dir missing: {}",
            text
        );
        assert!(
            text.contains("🤖 opencode-go/deepseek-v4-flash"),
            "model missing: {}",
            text
        );
        assert!(
            text.contains("📊 上下文 84k/200k (42%)"),
            "context segment missing: {}",
            text
        );
    }

    /// ADR-0044: when the server reports no window size the segment degrades
    /// to the used tokens alone — no percentage against an unknown denominator.
    #[test]
    fn card_footer_shows_used_tokens_when_the_window_is_unknown() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.provider_id = Some("opencode-go".into());
        acc.model_id = Some("deepseek-v4-flash".into());
        acc.context_tokens = 84_200;
        acc.context_window = None;
        acc.push_text("结果");

        let text = acc.build_card().to_string();
        assert!(text.contains("📊 上下文 84k"), "used tokens missing: {}", text);
        assert!(!text.contains("84k/"), "no denominator expected: {}", text);

        // Nothing to show before the first usage lands.
        let mut fresh = StreamAccumulator::new("test");
        fresh.provider_id = Some("opencode-go".into());
        fresh.model_id = Some("m".into());
        fresh.context_window = Some(200_000);
        fresh.push_text("结果");
        assert!(!fresh.build_card().to_string().contains("📊"));
    }

    /// ADR-0044: a window memoized for ANOTHER model is never paired with the
    /// current model's usage (the denominator would silently lie). It stays
    /// used-only until the current model's lookup lands.
    #[test]
    fn stale_window_for_another_model_is_ignored() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.provider_id = Some("p".into());
        acc.model_id = Some("m2".into());
        acc.context_tokens = 42_000;
        acc.context_window = Some(200_000);
        acc.context_window_key = Some(("p".into(), "m1".into()));
        acc.push_text("结果");

        let text = acc.build_card().to_string();
        assert!(
            text.contains("📊 上下文 42k") && !text.contains("42k/"),
            "a stale denominator must not render: {text}"
        );
    }

    #[test]
    fn format_tokens_is_compact() {
        assert_eq!(format_tokens(842), "842");
        assert_eq!(format_tokens(84_200), "84k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(1_200_000), "1.2M");
        assert_eq!(format_tokens(20_000_000), "20M");
    }

    /// The footer model line renders the full identity `provider/model@variant`
    /// (ADR-0020): provider is part of model identity, and the variant is what
    /// cola sent this turn. Without a variant the line is just `provider/model`.
    #[test]
    fn card_footer_shows_model_with_variant() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.directory = Some("/root/workspace/dev/cola".into());
        acc.provider_id = Some("opencode-go".into());
        acc.model_id = Some("deepseek-v4-flash".into());
        acc.variant = Some("high".into());
        acc.push_text("结果");

        let card = acc.build_card();
        let text = card.to_string();
        assert!(
            text.contains("🤖 opencode-go/deepseek-v4-flash@high"),
            "model@variant missing: {}",
            text
        );

        let mut no_variant = StreamAccumulator::new("test");
        no_variant.card_state = CardState::Done;
        no_variant.provider_id = Some("opencode-go".into());
        no_variant.model_id = Some("deepseek-v4-flash".into());
        no_variant.push_text("结果");
        let text2 = no_variant.build_card().to_string();
        assert!(
            text2.contains("🤖 opencode-go/deepseek-v4-flash") && !text2.contains("@"),
            "unset variant must not render @: {}",
            text2
        );
    }

    /// The work-context 📁 segment (ADR-0019): project basename + branch + dirty
    /// marker, merged into one segment on the footer.
    #[test]
    fn card_footer_shows_project_branch_and_dirty() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.directory = Some("/root/workspace/dev/cola".into());
        acc.project_name = Some("cola".into());
        acc.branch = Some("main".into());
        acc.dirty = true;
        acc.push_text("结果");

        let text = acc.build_card().to_string();
        assert!(text.contains("📁 cola · main ⚠"), "missing: {}", text);
    }

    #[test]
    fn card_footer_branch_shows_clean_without_marker() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.directory = Some("/root/workspace/dev/cola".into());
        acc.project_name = Some("cola".into());
        acc.branch = Some("main".into());
        acc.dirty = false;
        acc.push_text("结果");

        let text = acc.build_card().to_string();
        assert!(text.contains("📁 cola · main"), "missing: {}", text);
        assert!(!text.contains("⚠"), "clean tree must not show ⚠: {}", text);
    }

    /// Non-git directory: only the project name, no branch/dirty halves.
    #[test]
    fn card_footer_non_git_shows_project_only() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.directory = Some("/tmp/plain-dir".into());
        acc.project_name = Some("plain-dir".into());
        acc.push_text("结果");

        let text = acc.build_card().to_string();
        assert!(text.contains("📁 plain-dir"), "missing: {}", text);
        assert!(!text.contains("⚠"), "no dirty marker expected: {}", text);
    }

    /// ADR-0019: the turn-end refresh overwrites both git halves (branch moves,
    /// dirty flips), while an unresolved read keeps the last known state so the
    /// footer never degrades below the start capture.
    #[test]
    fn apply_git_state_refreshes_halves_and_never_regresses() {
        use crate::git::GitState;

        let mut acc = StreamAccumulator::new("test");
        acc.apply_git_state(GitState {
            branch: Some("main".into()),
            dirty: true,
        });
        assert_eq!(acc.branch.as_deref(), Some("main"));
        assert!(acc.dirty);

        // Failed/empty end read: the start capture stays.
        acc.apply_git_state(GitState::default());
        assert_eq!(acc.branch.as_deref(), Some("main"));
        assert!(acc.dirty);

        // Successful end read: the AI switched branch and committed.
        acc.apply_git_state(GitState {
            branch: Some("feat/ai-work".into()),
            dirty: false,
        });
        assert_eq!(acc.branch.as_deref(), Some("feat/ai-work"));
        assert!(!acc.dirty);
    }

    /// ADR-0019/ADR-0044: the 📁 segment, the 🤖 model line and the 📊 context
    /// segment all render on every card once their data exists — including a
    /// finalized 「部分完成」 slice (`include_tail = false`), which cannot be
    /// updated afterwards.
    #[test]
    fn work_context_model_and_context_show_before_final_card() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Streaming;
        acc.directory = Some("/root/workspace/dev/cola".into());
        acc.project_name = Some("cola".into());
        acc.branch = Some("feat/x".into());
        acc.dirty = true;
        acc.provider_id = Some("opencode-go".into());
        acc.model_id = Some("deepseek-v4-flash".into());
        acc.context_tokens = 84_200;
        acc.context_window = Some(200_000);
        acc.context_window_key = Some(("opencode-go".into(), "deepseek-v4-flash".into()));
        acc.push_text("结果");

        let mid = acc
            .build_card_inner(0, acc.timeline.len(), false, None)
            .0
            .to_string();
        assert!(mid.contains("📁 cola · feat/x ⚠"), "mid missing: {}", mid);
        assert!(
            mid.contains("🤖 opencode-go/deepseek-v4-flash"),
            "model must show mid-turn: {}",
            mid
        );
        assert!(
            mid.contains("📊 上下文 84k/200k (42%)"),
            "context must show mid-turn: {}",
            mid
        );

        acc.card_state = CardState::Done;
        let final_card = acc.build_card().to_string();
        assert!(
            final_card.contains("🤖 opencode-go/deepseek-v4-flash"),
            "final: {}",
            final_card
        );
        assert!(
            final_card.contains("📊 上下文 84k/200k (42%)"),
            "final: {}",
            final_card
        );
    }

    /// While the card fits the component budget, successive flushes must
    /// ACCUMULATE — each build re-renders from the same start, so earlier
    /// reasoning/tools/text stay on the card (not just the latest delta).
    #[test]
    fn successive_builds_accumulate_without_splitting() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;

        acc.push_reasoning("先想想。");
        acc.push_text("第一条文本。");
        let (card, full) = acc.build_card_with_split();
        assert!(!full);
        assert!(card.to_string().contains("第一条文本"));
        assert!(card.to_string().contains("推理"));

        // More content arrives on the next poll.
        acc.push_tool(
            "call_1",
            ToolPanel::from_parts(
                "bash",
                ToolStatus::Completed,
                Some(serde_json::json!("ls")),
                Some("src"),
            ),
        );
        acc.push_text("第二条文本。");
        let (card2, full2) = acc.build_card_with_split();
        assert!(!full2);
        let text2 = card2.to_string();
        assert!(
            text2.contains("第一条文本") && text2.contains("第二条文本"),
            "content must accumulate, not only show the delta: {}",
            text2
        );
        assert!(text2.contains("推理"), "reasoning must persist");
        assert!(text2.contains("bash"), "tool must persist");
    }

    /// When a turn has too many parts to fit one card (Feishu component limit),
    /// the card splits: the first is finalized with a "to be continued" marker
    /// and a continuation card renders the remaining timeline.
    #[test]
    fn card_splits_into_continuation_when_over_component_limit() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        for i in 0..50 {
            acc.push_tool(
                &format!("call_{}", i),
                ToolPanel::from_parts(&format!("tool{}", i), ToolStatus::Completed, None, None),
            );
        }
        acc.push_text("最后的结论。");

        let (card, full) = acc.build_card_with_split();
        assert!(full, "50 tools must exceed the component budget");
        let card_text = card.to_string();
        assert!(
            card_text.contains("部分完成，继续中"),
            "split card must show a partial header: {}",
            card_text
        );
        assert!(acc.render_from > 0, "render_from must advance past the split");

        // The continuation holds the remaining content, nothing duplicated.
        let (rest, full2) = acc.build_card_with_split();
        assert!(!full2, "rest should fit: {:?}", rest);
        assert!(
            rest.to_string().contains("最后的结论。"),
            "conclusion must appear on a continuation"
        );
    }

    /// Feishu rejects cards whose serialized body approaches 30KB (documented
    /// limit; a real 44KB card fails with 230099 / "create universal card
    /// fail"). A turn heavy on tool panels must split on the JSON-size budget —
    /// not just the component count — so every card stays under the cap.
    #[test]
    fn card_splits_on_json_size_budget() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        // Each full-size tool panel serializes to ~3.4KB; 12 of them (~41KB of
        // body) exceed MAX_CARD_JSON_CHARS but not MAX_CARD_COMPONENTS, so only
        // the size budget can catch the overflow.
        let big_output = "o".repeat(crate::feishu::card::tool_render::TOOL_OUTPUT_MAX_CHARS);
        for i in 0..12 {
            acc.push_tool(
                &format!("call_{}", i),
                ToolPanel::from_parts(
                    &format!("tool{}", i),
                    ToolStatus::Completed,
                    Some(serde_json::json!("in")),
                    Some(&big_output),
                ),
            );
        }
        acc.push_text("最后的结论。");

        let mut cards = Vec::new();
        let mut full = true;
        while full {
            let (card, f) = acc.build_card_with_split();
            full = f;
            cards.push(card);
        }
        assert!(cards.len() >= 2, "12 big panels must split: {}", cards.len());
        for (i, card) in cards.iter().enumerate() {
            let bytes = serde_json::to_string(card).unwrap().len();
            assert!(
                bytes < crate::feishu::card::FEISHU_CARD_LIMIT_BYTES,
                "card {} exceeds Feishu's 30KB card limit: {} bytes",
                i,
                bytes
            );
        }
        let last = cards.last().unwrap();
        assert!(
            last.to_string().contains("最后的结论。"),
            "conclusion must survive on a continuation: {}",
            last
        );
    }

    /// The size budget is measured in UTF-8 BYTES, not characters: a CJK panel
    /// is ~3 bytes per char, so a card that "fits" by char count can still blow
    /// past Feishu's byte limit. A card heavy on Chinese tool output must split
    /// on the byte budget.
    #[test]
    fn card_splits_on_byte_budget_for_multibyte_content() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        // 7 panels of Chinese output: the CHAR estimate is 7 * ~3.4K = ~23.8K
        // chars (fits under MAX_CARD_JSON_CHARS), but the BYTE estimate is
        // 7 * ~9.4K = ~66K bytes (way over) — a char-counted estimate would
        // wrongly ship one ~66KB card and Feishu would reject it.
        let cjk_output = "中".repeat(crate::feishu::card::tool_render::TOOL_OUTPUT_MAX_CHARS);
        for i in 0..7 {
            acc.push_tool(
                &format!("call_{}", i),
                ToolPanel::from_parts(
                    &format!("tool{}", i),
                    ToolStatus::Completed,
                    Some(serde_json::json!("入参")),
                    Some(&cjk_output),
                ),
            );
        }
        acc.push_text("最后的结论。");

        let mut cards = Vec::new();
        let mut full = true;
        while full {
            let (card, f) = acc.build_card_with_split();
            full = f;
            cards.push(card);
        }
        assert!(
            cards.len() >= 2,
            "CJK panels must split on bytes: {}",
            cards.len()
        );
        for (i, card) in cards.iter().enumerate() {
            let bytes = serde_json::to_string(card).unwrap().len();
            assert!(
                bytes < crate::feishu::card::FEISHU_CARD_LIMIT_BYTES,
                "card {} exceeds Feishu's 30KB card limit: {} bytes",
                i,
                bytes
            );
        }
    }

    /// A long answer flows across continuation cards instead of being truncated
    /// to a preview (there is no plain-text fallback anymore). Text is chunked
    /// at push time so the split lands between chunks, and every chunk renders.
    #[test]
    fn long_text_splits_across_continuation_cards() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        let long = "很长的回答。".repeat(2000); // 12_000 chars > one card budget
        acc.push_text(&long);

        // First card: text-budget split marks it "to be continued".
        let (card, full) = acc.build_card_with_split();
        assert!(full, "long text must split past one card's budget");
        assert!(
            card.to_string().contains("部分完成，继续中"),
            "split card must show the partial header: {}",
            card
        );
        assert!(acc.render_from > 0, "render_from must advance");

        // The continuation carries the tail; between them all text is present.
        let (rest, full2) = acc.build_card_with_split();
        assert!(!full2, "tail should fit: {:?}", rest);
        let card_text = card.to_string();
        let rest_text = rest.to_string();
        let joined_len =
            card_text.chars().filter(|c| *c != '"').count() + rest_text.chars().filter(|c| *c != '"').count();
        assert!(
            joined_len >= long.chars().count(),
            "all text must survive the split (card={} rest={} long={})",
            card_text.len(),
            rest_text.len(),
            long.chars().count()
        );
        assert!(
            rest_text.contains("很长的回答。"),
            "tail text must appear on the continuation: {}",
            rest_text
        );
    }

    /// `push_text` chunks one large blob into bounded timeline items so a single
    /// item never exceeds the per-card text budget.
    #[test]
    fn push_text_chunks_long_blobs() {
        let mut acc = StreamAccumulator::new("test");
        let long = "字".repeat(MAX_CARD_TEXT_CHARS * 2 + 100);
        acc.push_text(&long);

        let text_items: Vec<&String> = acc
            .timeline
            .iter()
            .filter_map(|item| match &item.kind {
                TimelineKind::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(
            text_items.len() >= 3,
            "a long blob must be split into multiple text items, got {}",
            text_items.len()
        );
        for t in &text_items {
            assert!(
                t.chars().count() <= MAX_CARD_TEXT_CHARS,
                "text item over the per-card budget: {} chars",
                t.chars().count()
            );
        }
        let joined: String = text_items.iter().map(|t| t.as_str()).collect();
        assert_eq!(joined, long, "all text must be preserved across chunks");
    }

    /// Text and tool calls must render interleaved (chronological), not all
    /// tools on top and all text below — matching OpenChamber's presentation.
    #[test]
    fn card_interleaves_text_and_tools_in_timeline_order() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.push_text("先看一下目录。");
        acc.push_tool(
            "call_1",
            ToolPanel::from_parts(
                "bash",
                ToolStatus::Completed,
                Some(serde_json::json!("ls")),
                Some("src"),
            ),
        );
        acc.push_text("再看一下配置。");
        acc.push_tool(
            "call_2",
            ToolPanel::from_parts(
                "read",
                ToolStatus::Completed,
                Some(serde_json::json!("cola.toml")),
                Some("[bridge]"),
            ),
        );
        acc.push_text("结论是...");

        let card = acc.build_card();
        let elements = card["body"]["elements"].as_array().unwrap();
        // Element order: text, tool, text, tool, text.
        let kinds: Vec<&str> = elements
            .iter()
            .map(|e| {
                if e["tag"] == "collapsible_panel" {
                    "tool"
                } else {
                    "text"
                }
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["text", "tool", "text", "tool", "text"],
            "text/tools must interleave: {:?}",
            elements
        );
        assert!(elements[0]["content"].as_str().unwrap().contains("先看一下目录"));
        assert!(
            elements[1]["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("bash")
        );
        assert!(elements[2]["content"].as_str().unwrap().contains("再看一下配置"));
        assert!(
            elements[3]["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("read")
        );
        assert!(elements[4]["content"].as_str().unwrap().contains("结论是"));
    }

    /// The markdown hygiene and table budget are per CARD, shared by every
    /// element and panel the build emits: tables arriving through a tool panel
    /// count against the budget the text already consumed, and a tool output's
    /// raw markdown (a websearch body) is escaped like any other model text.
    #[test]
    fn card_markdown_hygiene_spans_text_and_tool_panels() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        let text = (1..=5)
            .map(|i| format!("| t{i} | b |\n|---|---|\n| 1 | 2 |"))
            .collect::<Vec<_>>()
            .join("\n\n");
        acc.push_text(&text);
        acc.push_tool(
            "call_web",
            ToolPanel::from_parts(
                "websearch",
                ToolStatus::Completed,
                None,
                Some("see <number_tag> and\n\n| p | q |\n|---|---|\n| 1 | 2 |"),
            ),
        );

        let card = acc.build_card();
        let joined = card.to_string();
        assert!(
            joined.contains("&#60;number_tag>"),
            "panel markdown is escaped: {joined}"
        );
        assert!(
            joined.contains("```\\n| p | q |\\n|---|---|\\n| 1 | 2 |\\n```"),
            "the panel's table exceeds the card's budget and is fenced: {joined}"
        );
    }

    /// Re-rendering the same tool (running → completed) must NOT duplicate its
    /// timeline marker.
    #[test]
    fn tool_state_update_does_not_duplicate_timeline_marker() {
        let mut acc = StreamAccumulator::new("test");
        acc.push_tool(
            "call_1",
            ToolPanel::from_parts("bash", ToolStatus::Running, Some(serde_json::json!("ls")), None),
        );
        acc.push_tool(
            "call_1",
            ToolPanel::from_parts(
                "bash",
                ToolStatus::Completed,
                Some(serde_json::json!("ls")),
                Some("src"),
            ),
        );
        assert_eq!(acc.tools.len(), 1);
        assert_eq!(acc.timeline.len(), 1, "one tool marker, no duplicates");
    }

    /// #243 / ADR-0045: a tool that is still running is LIVE content. A card
    /// split finalizes without its panel, and the continuation — the new live
    /// card — carries it, so the completion can never strand on a closed card.
    #[test]
    fn a_running_tool_panel_rides_the_live_continuation() {
        let bash = |status: ToolStatus, output: Option<&str>| {
            ToolPanel::from_parts(
                "bash",
                status,
                Some(serde_json::json!({ "command": "sleep 30" })),
                output,
            )
        };
        let mut acc = StreamAccumulator::new("test");
        acc.push_tool("call_bash", bash(ToolStatus::Running, None));
        // Enough text to push the timeline past one card's budget.
        acc.push_text(&"很长的回答。".repeat(2000));

        let (finalized, full) = acc.build_card_with_split();
        assert!(full, "long text must split");
        assert!(
            !finalized.to_string().contains("bash"),
            "a finalized card must not carry a running panel: {finalized}"
        );

        // The live continuation takes the panel over; completing it later
        // renders there, not on a card that can no longer change.
        let (continuation, full2) = acc.build_card_with_split();
        assert!(!full2, "the continuation should fit");
        assert!(
            continuation.to_string().contains("bash"),
            "the live continuation must carry the running panel: {continuation}"
        );
    }

    /// #243 / ADR-0045: a pending tool first sighted before the server
    /// stamped it has a synthetic fallback key; when the server start time
    /// arrives, the settle move must use THAT time, or the panel lands after
    /// content it actually preceded.
    #[test]
    fn a_late_server_start_time_orders_the_settled_panel() {
        let bash = |status: ToolStatus, output: Option<&str>| {
            ToolPanel::from_parts(
                "bash",
                status,
                Some(serde_json::json!({ "command": "sleep 30" })),
                output,
            )
        };
        let mut acc = StreamAccumulator::new("test");
        acc.push_tool_at(None, "call_bash", bash(ToolStatus::Pending, None));
        // Content that really started before and after the tool (server keys
        // 0 and 1000); the tool's own start time lands later.
        acc.push_text_at(Some(0), "第一段");
        acc.push_text_at(Some(1_000), "第二段");
        acc.push_tool_at(Some(500), "call_bash", bash(ToolStatus::Running, None));
        acc.push_tool_at(Some(500), "call_bash", bash(ToolStatus::Completed, Some("done")));

        let card = acc.build_card();
        let elements = card["body"]["elements"].as_array().unwrap();
        let order: Vec<&str> = elements
            .iter()
            .map(|e| {
                if e["tag"] == "collapsible_panel" {
                    "tool"
                } else {
                    e["content"].as_str().unwrap_or_default()
                }
            })
            .collect();
        assert_eq!(
            order,
            vec!["第一段", "tool", "第二段"],
            "the settled panel must sit at its server start time: {card}"
        );
    }

    /// #243 / ADR-0045: when the tool settles, its panel leaves the tail and
    /// joins the timeline — with the element identity it was born with, so the
    /// reader's fold state survives the move.
    #[test]
    fn a_settled_tool_panel_joins_the_timeline_keeping_its_identity() {
        let panel_ids = |card: &serde_json::Value| -> Vec<String> {
            card["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["tag"] == "collapsible_panel")
                .map(|e| e["element_id"].as_str().unwrap().to_string())
                .collect()
        };
        let bash = |status: ToolStatus, output: Option<&str>| {
            ToolPanel::from_parts(
                "bash",
                status,
                Some(serde_json::json!({ "command": "sleep 30" })),
                output,
            )
        };
        let mut acc = StreamAccumulator::new("test");
        acc.push_tool("call_bash", bash(ToolStatus::Running, None));
        // While the tool runs the panel is live: it renders on the card, but
        // has not joined the timeline.
        assert!(acc.timeline.is_empty(), "a running panel is not history yet");
        let running_card = acc.build_card();
        assert_eq!(panel_ids(&running_card).len(), 1, "{running_card}");

        acc.push_tool("call_bash", bash(ToolStatus::Completed, Some("done")));
        assert_eq!(acc.timeline.len(), 1, "the settled panel joins the timeline");
        let done_card = acc.build_card();
        assert!(
            done_card.to_string().contains("done"),
            "the result must render: {done_card}"
        );
        assert_eq!(
            panel_ids(&done_card),
            panel_ids(&running_card),
            "the settle move keeps the panel's element identity"
        );
    }

    /// The header phase (ADR-0014) follows state transitions: Loading until the
    /// first part, Reasoning while thinking, and a running tool gets its own
    /// Tool phase whose timer resets when it completes.
    #[test]
    fn header_phase_follows_state_transitions() {
        let mut acc = StreamAccumulator::new("test");
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Loading));
        acc.card_state = CardState::Reasoning;
        acc.refresh_phase();
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Reasoning));
        acc.card_state = CardState::Streaming;
        acc.refresh_phase();
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Streaming));
        // A running tool is its own phase.
        acc.push_tool(
            "call_1",
            ToolPanel::from_parts(
                "bash",
                ToolStatus::Running,
                Some(serde_json::json!("cargo test")),
                None,
            ),
        );
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Tool));
        // Tool completes → back to plain streaming (timer resets).
        acc.push_tool(
            "call_1",
            ToolPanel::from_parts(
                "bash",
                ToolStatus::Completed,
                Some(serde_json::json!("cargo test")),
                Some("ok"),
            ),
        );
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Streaming));
        // A running todowrite is a tail panel, not a timeline tool, but it is
        // still a running tool: it gets the Tool phase too, and a completed one
        // drops back. (Mutation audit, state.rs:548 — the todo_panel status
        // comparison survived the tools-only coverage.)
        acc.todo_panel = Some(ToolPanel::from_parts(
            "todowrite",
            ToolStatus::Running,
            None,
            None,
        ));
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Tool));
        acc.todo_panel = Some(ToolPanel::from_parts(
            "todowrite",
            ToolStatus::Completed,
            None,
            None,
        ));
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Streaming));
        // Finished turns show no timer phase.
        acc.card_state = CardState::Done;
        acc.refresh_phase();
        assert_eq!(acc.active_phase(), None);
    }

    /// The header signature carries the phase label, and flips to a title
    /// naming exactly which request kind is pending inline.
    #[test]
    fn header_sig_reflects_awaiting_kind_and_phase() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Reasoning;
        acc.refresh_phase();
        assert!(
            acc.header_sig().contains("推理中"),
            "reasoning phase in sig: {}",
            acc.header_sig()
        );
        let question = InteractionBlock::Question(PendingQuestion {
            request_id: "q".into(),
            session_id: "s".into(),
            questions: vec![crate::opencode::types::QuestionInfo {
                question: "继续?".into(),
                header: "确认".into(),
                options: vec![crate::opencode::types::QuestionOption {
                    label: "继续".into(),
                    description: String::new(),
                }],
                multiple: None,
                custom: None,
            }],
            directory: "/w".into(),
            answers: vec![None],
            done: vec![false],
        });
        acc.add_interaction(question);
        assert_eq!(acc.awaiting_action(), AwaitingAction::Question);
        assert!(
            acc.header_sig()
                .contains(crate::feishu::card::AWAITING_QUESTION_TITLE),
            "pending question must name the answer wait: {}",
            acc.header_sig()
        );
        acc.add_interaction(InteractionBlock::Permission(PendingPermission {
            session_id: "s".into(),
            request_id: "p".into(),
            body: "bash".into(),
            target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/w".into(),
        }));
        assert_eq!(acc.awaiting_action(), AwaitingAction::Both);
        assert!(
            acc.header_sig()
                .contains(crate::feishu::card::AWAITING_BOTH_TITLE),
            "permission + question must name both: {}",
            acc.header_sig()
        );
        // Resolving one of the two leaves the other kind's title.
        assert!(acc.dismiss_interaction("p"));
        assert_eq!(acc.awaiting_action(), AwaitingAction::Question);
        assert!(
            acc.header_sig()
                .contains(crate::feishu::card::AWAITING_QUESTION_TITLE),
            "resolving the permission must leave the question wait: {}",
            acc.header_sig()
        );
        // Resolving every live block lifts the awaiting title.
        assert!(acc.dismiss_interaction("q"));
        assert_eq!(acc.awaiting_action(), AwaitingAction::None);
        assert!(
            acc.header_sig().contains("推理中"),
            "awaiting title must lift back to the phase label: {}",
            acc.header_sig()
        );
    }
}
