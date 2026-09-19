use crate::bridge::core::SharedCore;
use crate::feishu::card::CardState;
use crate::feishu::card::shell::CardBuilder;
use crate::feishu::card::tool_render::ToolPanel;
use indexmap::IndexMap;
use std::sync::Arc;

/// One entry on a turn's chronological timeline: a rendered item plus the
/// server-side start time (epoch ms) of the part it came from — its **key**.
/// The card renders the timeline in key order, matching how OpenChamber shows
/// the parts interleaved. `shown_at` is the same server time when the part
/// truly has one, or `None` when the key is a synthetic ordering device — the
/// card only ever shows server clocks (#183 follow-up).
#[derive(Debug, Clone)]
pub struct TimelineItem {
    pub key: i64,
    pub shown_at: Option<i64>,
    /// Identity of this entry, assigned once at insertion and never reused.
    /// It names the entry's panel on the card (`tool_{seq}` / `reason_{seq}`);
    /// unlike the entry's position it survives timeline insertions and
    /// merges, so the client's local panel state can't drift to another panel
    /// when the card re-renders.
    pub seq: u64,
    pub kind: TimelineKind,
}

/// What a timeline entry renders.
#[derive(Debug, Clone)]
pub enum TimelineKind {
    Text(String),
    Reasoning(String),
    Tool(String),
    /// A resolved interaction block's Interaction Receipt (ADR-0038, rule 4):
    /// one markdown line keyed by the moment it was resolved, so it sits below
    /// everything that was on the card when the Host clicked and above
    /// everything resolved afterwards.
    Receipt(String),
}

/// The body-element range one live interaction block occupies on a built card.
/// Recorded at render time so the card handle can resolve or refresh the block
/// in place on the cached JSON, long after the accumulator that rendered it is
/// gone (ADR-0038, rule 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSpan {
    pub request_id: String,
    pub start: usize,
    pub end: usize,
}

/// A card as built for one message: the JSON, whether the timeline had to
/// split (the caller then sends a continuation), and the element ranges of the
/// live interaction blocks this card renders — empty on finalized slices, whose
/// tail belongs to the newest card.
pub struct BuiltCard {
    pub card: serde_json::Value,
    pub full: bool,
    pub spans: Vec<BlockSpan>,
}

/// Estimated serialized size (bytes) of one collapsible tool panel, mirroring
/// [`StreamAccumulator::estimate_split_index`]'s accounting (element overhead
/// plus the capped input and output the renderer keeps). Shared by the
/// timeline's tool items and the todo tail reserve.
fn panel_estimate(p: &ToolPanel) -> usize {
    let first_n_bytes = |s: &str, n: usize| s.chars().take(n).map(|c| c.len_utf8()).sum::<usize>();
    let input = p
        .input
        .as_ref()
        .map(|x| x.to_string())
        .map(|s| first_n_bytes(&s, 400))
        .unwrap_or(0);
    let output = p
        .output
        .as_deref()
        .map(|s| first_n_bytes(s, crate::feishu::card::tool_render::TOOL_OUTPUT_MAX_CHARS))
        .unwrap_or(0);
    400 + input + output
}

/// A permission request surfaced inline on the streaming card (instead of a
/// separate card), so the whole turn lives on ONE card.
#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub session_id: String,
    pub request_id: String,
    /// The full markdown body the block renders (action, patterns/diff).
    pub body: String,
    /// Compact one-line form of the same request (action + first pattern /
    /// edited file) for the Interaction Receipt — receipts name their target
    /// even where the position cannot (ADR-0038).
    pub target: String,
    pub directory: String,
}

/// A `question` tool request surfaced inline on the streaming card. `answers[i]`
/// tracks which questions are already answered (None = open), kept in sync with
/// the flow's [`crate::bridge::question::QuestionState`].
#[derive(Debug, Clone)]
pub struct PendingQuestion {
    pub request_id: String,
    pub session_id: String,
    pub questions: Vec<crate::opencode::types::QuestionInfo>,
    pub directory: String,
    /// Display selection per question (locked answer, or live multi-select
    /// toggles). Mirrors `question_elements`' `answered` slice.
    pub answers: Vec<Option<Vec<String>>>,
    /// Whether each question is finalized (single-select answered, multi-select
    /// confirmed) — its controls collapse to a static 已选 line.
    pub done: Vec<bool>,
}

/// One entry in a card's interaction section: a live permission or question
/// block, or the tombstone of the receipt left once resolved. The section has
/// ONE representation (this list), ONE mutation API (the `*_interaction`
/// methods on [`StreamAccumulator`]) and one render point for live blocks
/// (`build_card_inner`'s tail): every update — add, state change, resolve —
/// goes through that seam, so what the card renders cannot drift from the
/// accumulator's in-flight state (ADR-0038).
#[derive(Debug, Clone)]
pub enum InteractionBlock {
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
    pub fn request_id(&self) -> &str {
        match self {
            InteractionBlock::Permission(p) => &p.request_id,
            InteractionBlock::Question(q) => &q.request_id,
            InteractionBlock::Receipt(request_id) => request_id,
        }
    }

    /// Whether the block still awaits the Host (a receipt is settled).
    pub fn is_live(&self) -> bool {
        !matches!(self, InteractionBlock::Receipt(_))
    }

    /// The directory that owns the block's request — the scope a sweep judges
    /// a vanished request in. Empty for a receipt tombstone, whose request is
    /// already settled.
    pub fn directory(&self) -> &str {
        match self {
            InteractionBlock::Permission(p) => &p.directory,
            InteractionBlock::Question(q) => &q.directory,
            InteractionBlock::Receipt(_) => "",
        }
    }

    /// The session that owns the block's request (a sub-task child carries its
    /// own id). Empty for a receipt tombstone.
    pub fn session_id(&self) -> &str {
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
    pub fn receipt_target(&self) -> String {
        match self {
            InteractionBlock::Permission(p) => p.target.clone(),
            InteractionBlock::Question(q) => question_target(&q.questions),
            InteractionBlock::Receipt(_) => String::new(),
        }
    }
}

/// Compact one-line target for a question's receipt: each question's header
/// (or a clipped question text), joined, clipped to stay a residue.
pub(crate) fn question_target(questions: &[crate::opencode::types::QuestionInfo]) -> String {
    crate::feishu::card::truncate_md(
        &questions
            .iter()
            .map(|qi| {
                if qi.header.is_empty() {
                    crate::feishu::card::truncate_md(&qi.question, 24)
                } else {
                    qi.header.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("、"),
        60,
    )
}

/// The turn-start work context (ADR-0019): the session directory plus the git
/// halves the Turn Footer shows. Captured before the prompt runs, applied to
/// the live card separately (see [`StreamAccumulator::capture_work_context`]).
#[derive(Debug, Clone, Default)]
pub struct WorkContext {
    pub directory: String,
    pub project_name: Option<String>,
    pub git: crate::git::GitState,
}

/// One Supplement waiting in a Card Chain's split queue (ADR-0043): the
/// message its continuation must reply to, and whether its receipt line has
/// already been written into the accumulator. The flag keeps the receipt
/// exactly-once when a continuation send fails and the split is retried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSplit {
    pub reply_to: String,
    pub receipt_pushed: bool,
}

/// One live card per session: the streaming accumulator plus the card identity
/// chain — the current live card's message id, updated in place by
/// `flush_card` (including continuation cards). Replaces the two per-session
/// maps kept in lockstep; owned by [`SharedCore::cards`].
#[derive(Clone)]
pub struct CardSession {
    pub acc: StreamAccumulator,
    pub card_message_id: Option<String>,
    /// The header signature of the last flush, compared on each poll so the
    /// card is re-flushed when the progress timer / state changes even with no
    /// new content (ADR-0014).
    pub last_header_sig: String,
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
    pub pending_split: Vec<PendingSplit>,
    /// Whether the tracked card is still the live (growing) card that flushes
    /// update in place. True until a split finalizes it; a continuation that
    /// fits becomes the new live card, while one that is itself over the size
    /// budget stays FINALIZED (and is never overwritten). Persisted so a flush
    /// that exhausted the chain bound — or died between a finalize and its
    /// continuation — resumes the chain instead of losing the slice it sent.
    pub card_is_live: bool,
}

impl CardSession {
    /// New session card: the header signature starts empty so the first poll
    /// always flushes (stamping the progress timer). `card_message_id` is the
    /// live card to update in place.
    pub fn new(acc: StreamAccumulator, card_message_id: Option<String>) -> Self {
        Self {
            acc,
            card_message_id,
            last_header_sig: String::new(),
            pending_split: Vec::new(),
            card_is_live: true,
        }
    }

    /// Re-point the live card identity at a new message (ADR-0028: a re-adopt
    /// mid-turn sends a fresh snapshot; the follow renderer keeps updating the
    /// new card instead of the old one). The accumulator content is untouched.
    pub fn repoint(&mut self, message_id: &str) {
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
pub enum HeaderPhase {
    Loading,
    Reasoning,
    Tool,
    Streaming,
}

/// Accumulates streaming state for one session.
#[derive(Default, Clone)]
pub struct StreamAccumulator {
    pub card_state: CardState,
    pub text: String,
    pub reasoning: String,
    /// Tool panels keyed by call ID (current state; `timeline` keeps order).
    pub tools: IndexMap<String, ToolPanel>,
    /// The latest `todowrite` panel of this turn, rendered as a card-TAIL
    /// status section instead of a timeline row. A timeline row would freeze on
    /// whichever card the call landed on: once that card finalizes (a long
    /// turn splits into several), later updates land on an already-sent card
    /// and stay invisible. The tail rides the live card, so every flush shows
    /// the current list. Each later call replaces it in place.
    pub todo_panel: Option<ToolPanel>,
    /// The server start time of the todowrite call that last refreshed
    /// [`Self::todo_panel`] — its panel header shows when the list was last
    /// written.
    pub todo_shown_at: Option<i64>,
    /// Text, reasoning, tool and receipt entries ordered by their key (the
    /// server-side part start time) — the card is built from this, so message ↔
    /// tool interleaving is preserved even when a part renders late.
    pub timeline: Vec<TimelineItem>,
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
    pub interactions: Vec<InteractionBlock>,
    /// Timeline index the CURRENT card starts rendering from. When a card fills
    /// up (Feishu component limit) it is finalized with a "to be continued"
    /// marker and `render_from` advances — a fresh continuation card renders the
    /// remaining timeline from there.
    pub render_from: usize,
    /// Provider ID of the model answering this turn (e.g. "opencode-go").
    pub provider_id: Option<String>,
    /// Model ID of the model answering this turn (e.g. "deepseek-v4-flash").
    pub model_id: Option<String>,
    /// The `/think` variant cola sent this turn (e.g. "high"), shown as
    /// `model@variant` on the footer. Sourced from the session store — the
    /// server reports the model but not the variant.
    pub variant: Option<String>,
    /// Context tokens the model consumed this turn (includes cached prefix), for
    /// the context-usage ratio in the card footer.
    pub context_tokens: i64,
    /// Estimated context-window usage (0..1); set when the turn completes.
    pub context_ratio: Option<f64>,
    /// Working directory of the session, shown in the card footer.
    pub directory: Option<String>,
    /// Project name (directory basename) for the Turn Footer's 📁 segment.
    pub project_name: Option<String>,
    /// Git branch: captured at turn start, refreshed at turn end (ADR-0019);
    /// the short commit hash when detached.
    pub branch: Option<String>,
    /// Working tree differs from HEAD, including untracked files: captured at
    /// turn start, refreshed at turn end. Only set alongside `branch`
    /// (ADR-0019: the halves are omitted together).
    pub dirty: bool,
    /// The session/thread name; shown as the card subtitle so the header can
    /// stay focused on state (the question is already in the reply context).
    pub title: String,
    pub error: Option<String>,
    pub reply_to_message_id: Option<String>,
    /// This turn's session id, carried on the error-card retry button so the
    /// card callback can find the accumulator + card to reuse.
    pub session_id: Option<String>,
    /// The full original prompt text of this turn, kept so the error-card
    /// "retry" button can re-submit it without the user retyping.
    pub prompt: Option<String>,
    /// The id this turn's user message carries (`msg_cola_…`, ADR-0026), so a
    /// later error-card retry reuses it and the server deduplicates by id.
    pub cola_message_id: Option<String>,
    /// Who sent the prompt (Feishu open_id), so the group completion notice can
    /// be replied to them / @-mention them.
    pub requester_open_id: Option<String>,
    /// Whether the prompt came from a group chat (completion notice is group-only).
    pub is_group: bool,
    /// The Chat/Topic's turn generation at this turn's start (ADR-0043),
    /// assigned by [`crate::bridge::pin::PinState::begin_turn`]. The Instant
    /// Reminder pin lifecycle reads it so every pin carries the turn it
    /// belongs to and a stale clear cannot unpin a newer turn's pin.
    pub turn_generation: Option<u64>,
    /// Part ids already rendered into this card — dedupes incremental polling.
    /// Reasoning/text parts are written empty first and updated with full text,
    /// so they are only tracked once they have content. Tool parts are tracked
    /// separately in `rendered_tool_states` because they get re-rendered on
    /// status changes (running → completed).
    pub rendered_parts: std::collections::HashSet<String>,
    /// callID → state signature for tool panels (status + output length; a
    /// todowrite's whole state, since its list can change without changing any
    /// length); a tool is re-rendered when its signature changes.
    pub rendered_tool_states: std::collections::HashMap<String, String>,
    /// The turn's start on the SERVER's clock: the created time of the user
    /// message this turn answers. External renders arm with it directly; a
    /// cola-sent turn captures it from the stored user message on the first
    /// poll it appears in (matched by `cola_message_id`, ADR-0026). It is the
    /// turn's single anchor: the header date, the turn filter
    /// (`created >= it`) and the renderer replacement guard all read it, so
    /// cola's own clock is never compared against the server's (#183, #190).
    pub turn_started_ms: Option<i64>,
    /// ADR-0014: progress/liveness signals for the header.
    /// The active header phase; None when the turn is not actively working
    /// (Done/Error/Continued show no timer).
    pub current_phase: Option<HeaderPhase>,
    /// When the current phase started (wall clock); the header timer counts up
    /// from here.
    pub phase_started_at: Option<std::time::Instant>,
}

impl StreamAccumulator {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            reply_to_message_id: None,
            session_id: None,
            prompt: None,
            requester_open_id: None,
            is_group: false,
            rendered_parts: std::collections::HashSet::new(),
            rendered_tool_states: std::collections::HashMap::new(),
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
    pub async fn capture_work_context(dir: &str) -> WorkContext {
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
    pub fn apply_work_context(&mut self, ctx: WorkContext) {
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
    pub async fn attach_work_context(&mut self, dir: &str) {
        let ctx = Self::capture_work_context(dir).await;
        self.apply_work_context(ctx);
    }

    /// Apply freshly read git state to the work-context halves. The halves move
    /// together and never regress: only a resolved branch overwrites them, so a
    /// failed or empty read (transient git failure, repo gone) keeps the last
    /// known state rather than dropping `branch ⚠` from the footer.
    pub fn apply_git_state(&mut self, state: crate::git::GitState) {
        if let Some(branch) = state.branch {
            self.branch = Some(branch);
            self.dirty = state.dirty;
        }
    }

    /// The header phase for the current state: None when the turn finished or
    /// errored (no timer shown).
    pub fn active_phase(&self) -> Option<HeaderPhase> {
        match self.card_state {
            CardState::Loading => Some(HeaderPhase::Loading),
            CardState::Reasoning => Some(HeaderPhase::Reasoning),
            CardState::Streaming => {
                // A running todowrite is a tail panel, not a timeline tool, but
                // it is still a running tool: ADR-0014 gives it the Tool phase
                // (and the timer reset that comes with it), like any other.
                let running = self.tools.values().any(|t| t.status == "running")
                    || self.todo_panel.as_ref().is_some_and(|t| t.status == "running");
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
    pub fn refresh_phase(&mut self) {
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
    pub fn add_interaction(&mut self, block: InteractionBlock) -> bool {
        if self.interaction(block.request_id()).is_some() {
            return false;
        }
        self.interactions.push(block);
        true
    }

    /// The block for `request_id`, if the card carries one.
    pub fn interaction(&self, request_id: &str) -> Option<&InteractionBlock> {
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
    pub fn resolve_interaction(
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
    pub fn dismiss_interaction(&mut self, request_id: &str) -> bool {
        let Some(idx) = self.live_index(request_id) else {
            return false;
        };
        self.interactions[idx] = InteractionBlock::Receipt(request_id.to_string());
        true
    }

    /// Append one Interaction Receipt keyed at the resolution moment — the
    /// single residue a mode change leaves for every block it resolved.
    pub fn push_receipt(&mut self, text: &str) {
        let key = self.next_order();
        self.insert_kind(key, None, TimelineKind::Receipt(text.to_string()));
    }

    /// Replace a question block's display state (the live 已选/✅ markers) in
    /// place. Returns false when the card has no question block for the
    /// request.
    pub fn update_question_state(
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
    pub fn resolve_vanished(
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
    pub fn live_permissions(&self) -> Vec<PendingPermission> {
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
    pub fn live_questions(&self) -> Vec<PendingQuestion> {
        self.interactions
            .iter()
            .filter_map(|b| match b {
                InteractionBlock::Question(q) => Some(q.clone()),
                _ => None,
            })
            .collect()
    }

    /// Whether the card carries any block awaiting the operator — drives the
    /// header's awaiting-action state. A receipt no longer awaits anything.
    pub fn has_live_interactions(&self) -> bool {
        self.interactions.iter().any(InteractionBlock::is_live)
    }

    /// Progress inputs for the header (ADR-0014): waiting flag, phase timer,
    /// and reasoning length. Elapsed is whole seconds so the header signature
    /// changes at most once per second — the flush throttle.
    pub fn header_progress(&self) -> crate::feishu::card::HeaderProgress {
        crate::feishu::card::HeaderProgress {
            waiting: self.has_live_interactions(),
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
            .find(|t| t.status == "running")
            .or_else(|| self.todo_panel.as_ref().filter(|t| t.status == "running"))
    }

    /// The header (title, template) this accumulator's card should show: the
    /// state label, the waiting override, the phase timer and the running-tool
    /// hint. Exposed so a click's ack can restamp a card's header in the same
    /// response — resolving the last live block must clear
    /// "等待你的授权/回答" immediately, not a poll later.
    pub fn header_title_and_template(&self) -> (String, &'static str) {
        crate::feishu::card::shell::header_title_and_template(
            &self.card_state,
            self.running_tool(),
            &self.header_progress(),
        )
    }

    /// The card header's signature (title + template), compared across polls
    /// to decide whether the header changed enough to re-flush (ADR-0014).
    pub fn header_sig(&self) -> String {
        let (title, template) = self.header_title_and_template();
        format!("{}|{}", title, template)
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
        let idx = self.timeline.partition_point(|item| item.key <= key);
        let (idx, key) = if idx < self.render_from {
            (
                self.render_from,
                self.timeline.get(self.render_from).map_or(key, |i| i.key),
            )
        } else {
            (idx, key)
        };
        self.item_seq += 1;
        self.timeline.insert(
            idx,
            TimelineItem {
                key,
                shown_at,
                seq: self.item_seq,
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
    pub fn next_order(&mut self) -> i64 {
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
    pub fn push_text(&mut self, chunk: &str) {
        self.push_text_at(None, chunk);
    }

    /// [`Self::push_text`] for a chunk from the part that started at `at_ms`
    /// (the server's `time.start`), which also places it on the timeline.
    /// `None` for a payload with no server time: the item is then keyed by a
    /// monotonic fallback (call order) and shows no clock.
    pub fn push_text_at(&mut self, at_ms: Option<i64>, chunk: &str) {
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
    pub fn push_reasoning(&mut self, chunk: &str) {
        self.push_reasoning_at(None, chunk);
    }

    /// [`Self::push_reasoning`] for a reasoning part that started at `at_ms`
    /// (the server's `time.start`); `None` keys it by fallback and shows no
    /// clock.
    pub fn push_reasoning_at(&mut self, at_ms: Option<i64>, chunk: &str) {
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
    pub fn push_tool(&mut self, call_id: &str, panel: ToolPanel) {
        self.push_tool_at(None, call_id, panel);
    }

    /// [`Self::push_tool`] for a tool part that started at `at_ms` (the
    /// server's `state.time.start`). A tool first seen before the server
    /// stamped it (a pending part, no time) gains its start time on the later
    /// update without moving its key; a part with no server time keeps showing
    /// no clock.
    pub fn push_tool_at(&mut self, at_ms: Option<i64>, call_id: &str, panel: ToolPanel) {
        let is_new = !self.tools.contains_key(call_id);
        self.tools.insert(call_id.to_string(), panel);
        if is_new {
            let key = at_ms.unwrap_or_else(|| self.next_order());
            self.insert_kind(key, at_ms, TimelineKind::Tool(call_id.to_string()));
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
    pub fn build_card(&self) -> serde_json::Value {
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
    pub fn build_card_with_info(&mut self) -> BuiltCard {
        // The slice that fits on its own. When items remain, it must hold at
        // least one: an empty slice would leave `render_from` frozen and the
        // flush loop would re-send empty "部分完成" cards forever.
        let mut split = self.estimate_split_index(self.render_from, None);
        if self.render_from < self.timeline.len() {
            split = split.max(self.render_from + 1);
        }
        let mut full = split < self.timeline.len();
        // The todo list rides the tail, which only a non-full (live) card
        // carries. When the remainder fits without the panel but not with it,
        // this card finalizes without the tail — sized so the next card, with
        // fewer items, can carry it — instead of overflowing Feishu's limit.
        if !full && let Some(todo) = &self.todo_panel {
            let with_tail = self.estimate_split_index(self.render_from, Some(todo));
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
    pub fn build_card_with_split(&mut self) -> (serde_json::Value, bool) {
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
    pub fn build_finalized_handoff(&mut self) -> serde_json::Value {
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
    pub fn live_request_ids(&self) -> Vec<&str> {
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
    /// `tail_reserve` is a panel the built card will ALSO carry in its tail
    /// (the todo list); charging its size to the same budget keeps the card
    /// and its tail together under the cap. Callers pass it only when the
    /// slice is the final, tail-carrying one — see
    /// [`Self::build_card_with_info`].
    fn estimate_split_index(&self, start: usize, tail_reserve: Option<&ToolPanel>) -> usize {
        // Byte length of the first `n` chars (mirrors `truncate_md`, which caps
        // rendered content by characters).
        let first_n_bytes = |s: &str, n: usize| s.chars().take(n).map(|c| c.len_utf8()).sum::<usize>();
        let mut comps = 0usize;
        let mut size = 0usize;
        let mut card_text = 0usize;
        if let Some(panel) = tail_reserve {
            comps += 1;
            size += panel_estimate(panel);
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
    /// button, context-ratio footer) — only the turn's final card should carry
    /// them. `state_override` forces the header state (e.g. "部分完成" on split
    /// cards). Returns the card and the element range of every live block the
    /// tail rendered (empty without a tail).
    fn build_card_inner(
        &self,
        start: usize,
        end: usize,
        include_tail: bool,
        state_override: Option<CardState>,
    ) -> (serde_json::Value, Vec<BlockSpan>) {
        let state = state_override.unwrap_or_else(|| self.card_state.clone());
        // The header shows the Turn's running tool even when THIS slice has no
        // panel for it (a split continuation after `sleep 30` started): pass
        // the accumulator's global selection as the builder's override.
        let mut builder = CardBuilder::new()
            .with_state(state)
            .with_progress(self.header_progress())
            .with_header_running_tool(self.running_tool().cloned());

        // The card is a reply to the user's message, so the session/thread name
        // goes in the subtitle and the question is NOT echoed again. The date
        // anchor is the turn's SERVER time (#183 follow-up) — never cola's —
        // so it can't disagree with the panels and stays stable across flushes.
        builder = builder.with_subtitle(&self.title);
        if let Some(date) = self.turn_started_ms.and_then(crate::feishu::card::fmt_local_date) {
            builder = builder.with_date(&date);
        }

        // Render text, reasoning, tool panels and receipts in the timeline's
        // key order (interleaved), like OpenChamber shows the parts. Text is
        // chunked at push time and bounded per card by `estimate_split_index`,
        // so everything in [start..end) renders in full — no preview
        // truncation, no separate plain-text message.
        let mut pending = String::new();
        let mut saw_content = false;
        for item in self.timeline.iter().take(end).skip(start) {
            match &item.kind {
                TimelineKind::Reasoning(r) => {
                    if !pending.is_empty() {
                        builder = builder.with_text(&pending);
                        pending.clear();
                    }
                    builder =
                        builder.with_reasoning_at(r, item.shown_at, Some(&format!("reason_{}", item.seq)));
                    saw_content = true;
                }
                TimelineKind::Text(t) => {
                    pending.push_str(t);
                    saw_content = true;
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

        let mut spans: Vec<BlockSpan> = Vec::new();
        if include_tail {
            // The live todo list opens the tail: it is the turn's current plan,
            // and the tail is the one section every flush re-renders, so the
            // list always lands on the LIVE card (a timeline row would freeze
            // on a finalized one). The block spans below are recorded from
            // `builder.body_len()`, so they stay correct with it in front.
            if let Some(todo) = &self.todo_panel {
                builder = builder.with_tool_at(todo.clone(), self.todo_shown_at, Some("todo"));
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
                spans.push(BlockSpan {
                    request_id: block.request_id().to_string(),
                    start,
                    end: builder.body_len(),
                });
            }

            if let Some(ref err) = self.error {
                builder = builder.with_text(&format!("\n**错误**: {}", err));
            }

            // Streaming/Loading with nothing rendered yet: keep the card alive.
            if !saw_content
                && self.error.is_none()
                && (self.card_state == CardState::Streaming || self.card_state == CardState::Loading)
            {
                builder = builder.with_text("⏳ ...");
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
        // to, and whether it left uncommitted work). The 🤖 model is captured
        // from the assistant message while the turn streams, so it renders on
        // EVERY card (a split "部分完成" card must still say which model is
        // answering); the context-window ratio is computed at turn end, so it
        // appends only on the final card (include_tail).
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
        if include_tail && let Some(ratio) = self.context_ratio {
            footer_parts.push(format!("📊 上下文 {:.0}%", ratio * 100.0));
        }
        if !footer_parts.is_empty() {
            builder = builder.with_footer(&footer_parts.join(" · "));
        }

        (builder.build(), spans)
    }
}

/// Refresh the live card's work context at turn end (ADR-0019): re-read the
/// session directory's git state so the final card shows where the turn landed
/// — a branch the AI created or switched to, and whether it left uncommitted
/// work. The read shells out to git, so it runs OUTSIDE the cards lock; the
/// lock only wraps the field swap. Best effort: a missing card or directory is
/// a no-op, and a failed read keeps the start capture (`apply_git_state`).
pub(crate) async fn refresh_work_context(core: &Arc<SharedCore>, session_id: &str) {
    let dir = {
        let cards = core.cards.lock().await;
        cards.get(session_id).and_then(|c| c.acc.directory.clone())
    };
    let Some(dir) = dir else { return };
    let state = crate::git::read_state(&dir).await;
    let mut cards = core.cards.lock().await;
    if let Some(card) = cards.get_mut(session_id) {
        card.acc.apply_git_state(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::card::MAX_CARD_TEXT_CHARS;

    #[test]
    fn card_footer_shows_directory_model_and_context() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.directory = Some("/root/workspace/dev/cola".into());
        acc.provider_id = Some("opencode-go".into());
        acc.model_id = Some("deepseek-v4-flash".into());
        acc.context_ratio = Some(0.36);
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
        assert!(text.contains("📊 上下文 36%"), "ratio missing: {}", text);
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

    /// ADR-0019: the 📁 segment and the 🤖 model line render on every card
    /// (the model is known while streaming), while the context ratio only
    /// appends on the final card.
    #[test]
    fn work_context_and_model_show_before_final_card_but_ratio_does_not() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Streaming;
        acc.directory = Some("/root/workspace/dev/cola".into());
        acc.project_name = Some("cola".into());
        acc.branch = Some("feat/x".into());
        acc.dirty = true;
        acc.provider_id = Some("opencode-go".into());
        acc.model_id = Some("deepseek-v4-flash".into());
        acc.context_ratio = Some(0.36);
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
        assert!(!mid.contains("📊"), "ratio must not show mid-turn: {}", mid);

        acc.card_state = CardState::Done;
        let final_card = acc.build_card().to_string();
        assert!(
            final_card.contains("🤖 opencode-go/deepseek-v4-flash"),
            "final: {}",
            final_card
        );
        assert!(final_card.contains("📊 上下文 36%"), "final: {}", final_card);
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
            ToolPanel {
                name: "bash".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("ls")),
                output: Some("src".into()),
            },
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
                ToolPanel {
                    name: format!("tool{}", i),
                    status: "completed".into(),
                    input: None,
                    output: None,
                },
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
                ToolPanel {
                    name: format!("tool{}", i),
                    status: "completed".into(),
                    input: Some(serde_json::json!("in")),
                    output: Some(big_output.clone()),
                },
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
                ToolPanel {
                    name: format!("tool{}", i),
                    status: "completed".into(),
                    input: Some(serde_json::json!("入参")),
                    output: Some(cjk_output.clone()),
                },
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
            ToolPanel {
                name: "bash".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("ls")),
                output: Some("src".into()),
            },
        );
        acc.push_text("再看一下配置。");
        acc.push_tool(
            "call_2",
            ToolPanel {
                name: "read".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("cola.toml")),
                output: Some("[bridge]".into()),
            },
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

    /// Re-rendering the same tool (running → completed) must NOT duplicate its
    /// timeline marker.
    #[test]
    fn tool_state_update_does_not_duplicate_timeline_marker() {
        let mut acc = StreamAccumulator::new("test");
        acc.push_tool(
            "call_1",
            ToolPanel {
                name: "bash".into(),
                status: "running".into(),
                input: Some(serde_json::json!("ls")),
                output: None,
            },
        );
        acc.push_tool(
            "call_1",
            ToolPanel {
                name: "bash".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("ls")),
                output: Some("src".into()),
            },
        );
        assert_eq!(acc.tools.len(), 1);
        assert_eq!(acc.timeline.len(), 1, "one tool marker, no duplicates");
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
            ToolPanel {
                name: "bash".into(),
                status: "running".into(),
                input: Some(serde_json::json!("cargo test")),
                output: None,
            },
        );
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Tool));
        // Tool completes → back to plain streaming (timer resets).
        acc.push_tool(
            "call_1",
            ToolPanel {
                name: "bash".into(),
                status: "completed".into(),
                input: Some(serde_json::json!("cargo test")),
                output: Some("ok".into()),
            },
        );
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Streaming));
        // Finished turns show no timer phase.
        acc.card_state = CardState::Done;
        acc.refresh_phase();
        assert_eq!(acc.active_phase(), None);
    }

    /// The header signature carries the phase label, and flips to a waiting
    /// state when a permission/question is pending inline.
    #[test]
    fn header_sig_reflects_waiting_and_phase() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Reasoning;
        acc.refresh_phase();
        assert!(
            acc.header_sig().contains("推理中"),
            "reasoning phase in sig: {}",
            acc.header_sig()
        );
        acc.add_interaction(InteractionBlock::Permission(PendingPermission {
            session_id: "s".into(),
            request_id: "p".into(),
            body: "bash".into(),
            target: "⚡ 执行 Shell 命令 `ls -la`".into(),
            directory: "/w".into(),
        }));
        assert!(
            acc.header_sig()
                .contains(crate::feishu::card::AWAITING_ACTION_TITLE),
            "pending permission must flip the header: {}",
            acc.header_sig()
        );
    }
}
