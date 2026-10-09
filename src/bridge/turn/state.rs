//! The Turn's streaming state (spec #298, A3).
//!
//! The accumulator, its card session and their value types live behind the
//! Turn's interface (the `impl Turn` blocks in the parent module). Nothing
//! here is reachable from outside the Turn module: the coordinator's card map
//! only ever names [`CardSession`], whose fields are visible only inside the
//! Turn module, and every read or write goes through a `Turn::` method. The
//! accumulator's own tests are the module's internal seam.

use super::disposition::Disposition;
use crate::backend::{MessageId, Part, SessionTranscript, ToolCall, TurnAnchor};
use crate::bridge::chain::{CursorFrontier, CursorPartKind, RenderedCursor};
use crate::bridge::handles::CardsHandle;
use crate::feishu::card::first_n_chars_bytes;
use crate::feishu::card::ledger::{TaskCompletionEntry, TaskKind, TaskLedgerRow};
use crate::feishu::card::shell::CardBuilder;
use crate::feishu::card::tool_render::{TaskLiveness, ToolPanel, is_task_tool};
use crate::feishu::card::{AwaitingAction, CardState};
use indexmap::IndexMap;
use std::collections::HashMap;
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
    /// It names the entry's panel on the card (`reason_{seq}`; a tool panel
    /// uses the call's own id, `tool_{call_id}`, so the takeover's per-call
    /// strip can match it); unlike the entry's position it survives timeline
    /// insertions and merges, so the client's local panel state can't drift to
    /// another panel when the card re-renders.
    pub(super) seq: u64,
    pub(super) kind: TimelineKind,
    /// The typed transcript part this entry rendered from, for text/reasoning
    /// entries pushed by the render (spec #561's cursor frontier); `None` for
    /// synthetic entries (receipts, ledger entries) and for entries no
    /// frontier can name (tools).
    pub(super) source: Option<PartSource>,
    /// A markdown lead to emit before this entry's content (spec #561's cut
    /// tail): the fence opener or table header a projection's seeded render
    /// needs because the delivered prefix ended inside that construct. It is
    /// rendered, never counted as the part's own delivered characters.
    pub(super) lead: Option<String>,
}

/// Where a timeline text/reasoning entry came from: the typed transcript
/// part's own position (spec #561's cursor frontier). The payload carries no
/// stable part id (AGENTS.md #9), so identity is the message plus the part's
/// ordinal in it. Test and synthetic pushes carry none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PartSource {
    /// The assistant message that carried the part.
    pub(super) message_id: MessageId,
    /// The part's index in its message's typed `parts` vec.
    pub(super) index: usize,
    /// The part's model characters that were already delivered before this
    /// entry was rendered (spec #561's projection seed): the frontier's cut
    /// point. The cursor's delivered extent for the part is this offset plus
    /// the entry's own (model) characters; the markdown lead is not counted.
    /// `0` for every ordinary render.
    pub(super) delivered_before: usize,
    /// The digest of the part's delivered prefix at the moment this entry was
    /// pushed (spec #561, review #569): derived metadata, never content, kept
    /// so the frontier can tell a grown part from a same-slot replacement even
    /// when neither carries a server start time (V2). `None` for a source no
    /// render stamped.
    pub(super) prefix_digest: Option<u64>,
}

impl PartSource {
    /// An ordinary render's source: nothing delivered before it and no digest
    /// yet — the push stamps the digest of what it renders.
    pub(super) fn at(message_id: MessageId, index: usize) -> Self {
        Self {
            message_id,
            index,
            delivered_before: 0,
            prefix_digest: None,
        }
    }

    /// Whether `self` names the same transcript part as `other` (the position,
    /// not the offset or digest).
    pub(super) fn same_part(&self, other: &PartSource) -> bool {
        self.message_id == other.message_id && self.index == other.index
    }
}

/// The Rendered Cursor resolved against one transcript read (spec #561): the
/// frontier's absolute position in THIS read, so a seeded render can treat
/// everything at or before it as delivered. `None` from
/// [`CursorSeed::resolve`] when the cursor names a part this read cannot
/// place — the projection must then fall back rather than guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorSeed {
    pub(super) frontier: Option<SeedFrontier>,
    /// The tool call ids whose newest delivered state was `running` (spec
    /// #561's live set): resolved by identity against the whole read — still
    /// running renders display-only, settled joins the timeline once.
    pub(super) live_calls: std::collections::BTreeSet<String>,
    /// The live-set ids the resolving read actually carried (spec #561, review
    /// #569): a V2 read can be truncated at its page cap, so a call outside it
    /// cannot render on the successor — only a call the read carries may drop
    /// the old card's running panel.
    pub(super) resolved_live_calls: std::collections::BTreeSet<String>,
    /// The Turn window the seed renders beyond the accumulator's own (spec
    /// #561, ticket #565): the orphaned Turn's anchor carried onto a card
    /// whose own Turn is a different one (the message-first race). `None` when
    /// the seed's Turn IS the accumulator's (the projection), so one seed
    /// shape serves both and `render_turn_parts` walks the scope itself.
    pub(super) scope: Option<TurnAnchor>,
}

/// The frontier's resolved place in one read: the message's index in the
/// read's `messages` vec plus the part's ordinal in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SeedFrontier {
    pub(super) message_id: MessageId,
    pub(super) message_pos: usize,
    pub(super) part_index: usize,
    pub(super) kind: CursorPartKind,
    pub(super) delivered_chars: usize,
    /// The newest text/reasoning part at or before the frontier, when the
    /// frontier itself is a settled tool (spec #561, review #569): the part
    /// [`Self::delivered_chars`] belongs to, which still renders its growth.
    /// `None` when the frontier IS the text/reasoning part, or a tool frontier
    /// has no text before it.
    pub(super) extent_pos: Option<(usize, usize)>,
    /// Whether the part at the frontier is a REPLACEMENT this read carries in
    /// the recorded slot — a tool whose start time no longer matches (spec
    /// #561, review #569). The tool itself must render: nothing may be skipped
    /// past a part this read replaced, so [`CursorSeed::cut`] leaves it
    /// undelivered. Text/reasoning parts express the same rule with
    /// `delivered_chars = 0`.
    pub(super) replacement: bool,
}

/// A text/reasoning part's content, `""` for anything else.
fn text_part_text(part: &crate::backend::Part) -> &str {
    match part {
        crate::backend::Part::Text(text) => text.text.as_str(),
        crate::backend::Part::Reasoning(reasoning) => reasoning.text.as_str(),
        _ => "",
    }
}

/// A text/reasoning part's cursor kind, `None` for anything else.
fn text_part_kind(part: &crate::backend::Part) -> Option<CursorPartKind> {
    match part {
        crate::backend::Part::Text(_) => Some(CursorPartKind::Text),
        crate::backend::Part::Reasoning(_) => Some(CursorPartKind::Reasoning),
        _ => None,
    }
}

impl CursorSeed {
    /// Resolve `cursor` against `transcript` (spec #561): `None` when the
    /// frontier names a message/part this read does not carry at all, or its
    /// kind no longer matches — nothing may be skipped then. A text/reasoning
    /// part that only changed identity (a start time or delivered prefix that
    /// no longer matches) resolves as a CUT-AT-0 frontier instead: that part
    /// renders in full while everything before it stays delivered, so a
    /// rewrite cannot lose the tail (review #569).
    pub(crate) fn resolve(transcript: &SessionTranscript, cursor: &RenderedCursor) -> Option<Self> {
        let frontier = match &cursor.frontier {
            None => None,
            Some(frontier) => {
                let message_pos = transcript
                    .messages
                    .iter()
                    .position(|message| message.id == frontier.message_id)?;
                let part = transcript.messages[message_pos].parts.get(frontier.part_index)?;
                let (matches_kind, part_started_at) = match part {
                    crate::backend::Part::Text(text) => {
                        (frontier.kind == CursorPartKind::Text, text.started_at)
                    }
                    crate::backend::Part::Reasoning(reasoning) => {
                        (frontier.kind == CursorPartKind::Reasoning, reasoning.started_at)
                    }
                    crate::backend::Part::Tool(call) => {
                        (frontier.kind == CursorPartKind::Tool, call.started_at)
                    }
                    _ => (false, None),
                };
                // The frontier's identity is the message, the ordinal and the
                // kind: a part that is not even the same kind in that slot
                // cannot be placed at all, and the seed falls back. A
                // text/reasoning part whose server start time or delivered
                // prefix no longer matches, by contrast, is a rewrite or a
                // replacement — not a reason to drop the seed: the frontier is
                // CUT AT 0, so that part renders in full while everything
                // before it stays delivered (review #569). A grown part keeps
                // the matching prefix and still renders only its tail.
                if !matches_kind {
                    return None;
                }
                let start_matches = frontier.started_at == part_started_at;
                // A tool-kind frontier carries the newest text/reasoning part's
                // delivered extent: resolve that part in THIS read — the part
                // the cut and the digest belong to.
                let extent_target = if frontier.kind == CursorPartKind::Tool {
                    match transcript.newest_text_before(message_pos, frontier.part_index) {
                        Some((extent_pos, _chars)) => Some(extent_pos),
                        // No text before the tool: no extent may ride along.
                        None if frontier.delivered_chars == 0 => None,
                        None => return None,
                    }
                } else {
                    None
                };
                let extent_part = match extent_target {
                    Some((pos, index)) => transcript.messages[pos].parts.get(index)?,
                    None => part,
                };
                let extent_chars = text_part_text(extent_part).chars().count();
                // A cursor delivered past the read's content is not the read
                // the cursor came from (a rewrite, compaction, a recreated
                // part): the prefix cannot be verified, so the frontier cuts at
                // 0 and that part renders in full rather than skipping it.
                let verified = frontier.delivered_chars <= extent_chars;
                // A cursor carrying no digest — an older release — falls back
                // rather than guess (the one-release migration seam).
                let expected_digest = frontier.prefix_digest?;
                let delivered_prefix: String = text_part_text(extent_part)
                    .chars()
                    .take(frontier.delivered_chars)
                    .collect();
                let digest_matches = verified
                    && crate::bridge::chain::cursor_prefix_digest(&delivered_prefix) == expected_digest;
                let keep_extent = start_matches && digest_matches;
                let frontier = match frontier.kind {
                    CursorPartKind::Tool => match extent_target {
                        // The tool itself changed (or its text target was
                        // rewritten): degrade to the target as the frontier,
                        // so the tool and everything after it render normally
                        // and the target renders from its verified cut.
                        Some((target_pos, target_index)) if !keep_extent => {
                            let kind = text_part_kind(extent_part)?;
                            SeedFrontier {
                                message_id: frontier.message_id.clone(),
                                message_pos: target_pos,
                                part_index: target_index,
                                kind,
                                delivered_chars: if digest_matches {
                                    frontier.delivered_chars
                                } else {
                                    0
                                },
                                extent_pos: None,
                                replacement: false,
                            }
                        }
                        Some((target_pos, target_index)) => SeedFrontier {
                            message_id: frontier.message_id.clone(),
                            message_pos,
                            part_index: frontier.part_index,
                            kind: CursorPartKind::Tool,
                            delivered_chars: frontier.delivered_chars,
                            extent_pos: Some((target_pos, target_index)),
                            replacement: false,
                        },
                        // No text target: nothing is skipped past the tool. A
                        // tool whose start time no longer matches is a
                        // REPLACEMENT — like a rewritten text part, it is the
                        // frontier itself and renders (cut 0), so everything
                        // before it stays delivered and its result is never
                        // omitted (spec #561, review #569).
                        None => SeedFrontier {
                            message_id: frontier.message_id.clone(),
                            message_pos,
                            part_index: frontier.part_index,
                            kind: CursorPartKind::Tool,
                            delivered_chars: 0,
                            extent_pos: None,
                            replacement: !start_matches,
                        },
                    },
                    _ => SeedFrontier {
                        message_id: frontier.message_id.clone(),
                        message_pos,
                        part_index: frontier.part_index,
                        kind: frontier.kind,
                        delivered_chars: if keep_extent { frontier.delivered_chars } else { 0 },
                        extent_pos: None,
                        replacement: false,
                    },
                };
                Some(frontier)
            }
        };
        let resolved_live_calls = cursor
            .live_calls
            .iter()
            .filter(|call_id| transcript.tool_call(call_id).is_some())
            .cloned()
            .collect();
        Some(Self {
            frontier,
            live_calls: cursor.live_calls.clone(),
            resolved_live_calls,
            scope: None,
        })
    }

    /// The seed a fresh Turn's takeover resolves from the orphan record's
    /// Rendered Cursor (spec #561, ticket #565): the cursor against this read
    /// plus the orphaned Turn's own window as the seed's scope, so the
    /// successor's render continues the orphan's content past its own Turn.
    /// `None` when the read cannot place the cursor, so the caller keeps the
    /// gap pending and retries on a later read instead of consuming it (spec
    /// #561, review #569).
    pub(crate) fn for_orphan_resolving(
        transcript: &SessionTranscript,
        cursor: &RenderedCursor,
        scope: &TurnAnchor,
    ) -> Option<Self> {
        Self::resolve(transcript, cursor).map(|seed| Self {
            scope: Some(scope.clone()),
            ..seed
        })
    }

    /// The cursorless record's fallback seed (spec #561, ticket #565): no
    /// frontier — nothing replays — but the orphaned Turn's still-live calls
    /// enter the live set exactly as ADR-0068's carry did. A `todowrite` is
    /// excluded: the successor's own reads rebuild the todo list.
    pub(crate) fn live_calls_only(transcript: &SessionTranscript, scope: &TurnAnchor) -> Self {
        let live_calls: std::collections::BTreeSet<String> = transcript
            .turn_running_tools(scope)
            .iter()
            .filter(|call| call.identity.name != "todowrite")
            .map(|call| call.identity.call_id.clone())
            .collect();
        Self {
            frontier: None,
            resolved_live_calls: live_calls.clone(),
            live_calls,
            scope: None,
        }
    }

    /// Where the part at `(message_pos, part_index)` sits relative to the
    /// cut: delivered (skip), the frontier itself (render the suffix), or
    /// undelivered (render normally). A tool-kind frontier covers everything
    /// up to and including itself, while the delivered extent still belongs to
    /// the newest text/reasoning part at or before it — that part renders its
    /// growth (spec #561, review #569).
    pub(super) fn cut(&self, message_pos: usize, part_index: usize) -> SeedCut {
        let Some(frontier) = &self.frontier else {
            return SeedCut::Undelivered;
        };
        let pos = (message_pos, part_index);
        let frontier_pos = (frontier.message_pos, frontier.part_index);
        if let Some(extent_pos) = frontier.extent_pos {
            if pos < extent_pos {
                return SeedCut::Delivered;
            }
            if pos == extent_pos {
                return SeedCut::Frontier(frontier.delivered_chars);
            }
            return if pos <= frontier_pos {
                SeedCut::Delivered
            } else {
                SeedCut::Undelivered
            };
        }
        if frontier.kind == CursorPartKind::Tool {
            // A REPLACEMENT tool at the frontier renders (cut 0): nothing may
            // be skipped past a part this read replaced, or its result would be
            // omitted (spec #561, review #569).
            return if pos < frontier_pos || (pos == frontier_pos && !frontier.replacement) {
                SeedCut::Delivered
            } else {
                SeedCut::Undelivered
            };
        }
        match pos.cmp(&frontier_pos) {
            std::cmp::Ordering::Less => SeedCut::Delivered,
            std::cmp::Ordering::Equal => SeedCut::Frontier(frontier.delivered_chars),
            std::cmp::Ordering::Greater => SeedCut::Undelivered,
        }
    }

    /// Whether the cursor's live set named calls the resolving read ACTUALLY
    /// carried (spec #561, ticket #564, review #569): the successor resolves
    /// each one by identity — a still-running call renders display-only there,
    /// a settled one joins its timeline exactly once — so the collected old
    /// card must drop the running markers it left behind. A call outside the
    /// read (a truncated V2 transcript) cannot render on the successor: its
    /// panel stays on the old card as a frozen marker rather than vanishing
    /// from both. The per-call set the collect strips by (review #569); empty
    /// when no named call resolved.
    pub(crate) fn resolved_calls(&self) -> Vec<String> {
        self.resolved_live_calls.iter().cloned().collect()
    }
}

/// What a seeded render owes one transcript part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeedCut {
    /// At or before the frontier: already delivered, never rendered again.
    Delivered,
    /// The frontier part itself: only its undelivered suffix renders.
    Frontier(usize),
    /// After the frontier: an ordinary render.
    Undelivered,
}

/// Bookkeeping for a Tool Panel that is still live (ADR-0045): the timeline
/// key and element identity allocated when its call first appeared, so the
/// settle move into the timeline lands in the call's original order and keeps
/// the reader's fold state. The typed part position it rendered from travels
/// along (spec #561, review #569), so its settled entry can become the
/// Rendered Cursor frontier.
#[derive(Debug, Clone)]
struct LiveTool {
    key: i64,
    shown_at: Option<i64>,
    seq: u64,
    source: Option<PartSource>,
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
    /// A completed Background Task's ledger entry (ADR-0060): the mechanical
    /// completion line as a folded collapsible panel's title, the task's
    /// identity and timing in the fold. Keyed at the Wake's own server time,
    /// so the entry sits where the completion happened — never on a later
    /// card, and never doubled by a repeated poll.
    LedgerEntry(TaskCompletionEntry),
}

/// A card as built for one message: the JSON, whether the timeline had to
/// split (the caller then sends a continuation), and the element ranges of the
/// live interaction blocks this card renders — empty on finalized slices, whose
/// tail belongs to the newest card.
pub(super) struct BuiltCard {
    pub(super) card: serde_json::Value,
    pub(super) full: bool,
    pub(super) spans: Vec<crate::bridge::card_handles::BlockSpan>,
    /// The Rendered Cursor this body delivers (spec #561): the frontier of the
    /// newest text/reasoning part it renders and the tool ids whose delivered
    /// state it shows. The flush stages it before the write and drains it into
    /// the Chain Record once that write is confirmed.
    pub(super) cursor: RenderedCursor,
    /// How far this body carries the chain's pending orphan gap (spec #561,
    /// review #569), when one is owed and the body rendered any of it.
    pub(super) gap: Option<GapCoverage>,
}

/// How far ONE built card body carries the chain's pending orphan gap (spec
/// #561, review #569): captured per body, so a gap split across cards is not
/// mistaken for a delivered one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GapCoverage {
    /// The gap's cursor advanced to the newest gap content the body covers.
    /// A confirmed write persists it, so a restart — or the next slice's
    /// build — resumes after the delivered part instead of re-rendering it.
    pub(super) frontier: CursorFrontier,
    /// Whether the body reached the gap's END (its last rendered entry). Only
    /// then may a confirmation clear the durable gap; a body that stops
    /// mid-gap leaves the remaining tail owed.
    pub(super) complete: bool,
}

/// Estimated serialized size (bytes) of one collapsible tool panel, mirroring
/// [`StreamAccumulator::estimate_split_index`]'s accounting (element overhead
/// plus the capped input and output the renderer keeps). Shared by the
/// timeline's tool items and the todo tail reserve.
fn panel_estimate(p: &ToolPanel) -> usize {
    let input = p
        .input()
        .map(|x| x.to_string())
        .map(|s| first_n_chars_bytes(&s, 400))
        .unwrap_or(0);
    let output = p
        .output()
        .map(|s| first_n_chars_bytes(&s, crate::feishu::card::tool_render::TOOL_OUTPUT_MAX_CHARS))
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
    /// The message the continuation replies to (the newest queued split).
    pub(super) reply_to: String,
    pub(super) kind: super::SplitKind,
    pub(super) receipt_pushed: bool,
    /// The timeline key the receipt takes, when it must sort at a SERVER time
    /// instead of cola's "now": a Wake continuation's 承接 line precedes work
    /// whose server times are already in the past at poll time (ADR-0059).
    /// `None` — every Supplement and `/card` pull — keeps the
    /// resolution-moment key, unchanged. The line also names the Wake it
    /// covers, marked so the merged-path entry cannot double it.
    pub(super) line: Option<super::ContinuationLine>,
    /// Whether the outgoing card already received this split's ledger handover
    /// (ADR-0060): the read's remaining list left it and/or a retiring Wake's
    /// completion entry arrived. A terminal card that received one still owes
    /// the handover PATCH — its ending kept intact — while an untouched
    /// terminal card is left alone.
    pub(super) handover: bool,
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
    /// The chain's identity, handed out fresh by [`CardSession::new`]. A
    /// replacing session (a new Turn, an external arm, a Wake continuation
    /// armed from scratch) gets a new one, so a render loop can tell "my
    /// chain still exists" from "something else took the session over" even
    /// when the two share a Turn anchor — a Wake continuation continues the
    /// SAME Turn, so the anchor alone cannot tell them apart (ADR-0059).
    chain_id: u64,
    /// The Chat a continuation card goes to at the TOP LEVEL when the chain
    /// has no reply target: a Wake continuation armed after a restart sends
    /// its first card top-level (there is no user message to reply to), so a
    /// later size split must follow it there instead of stopping the chain
    /// mid-way. `None` — every reply-anchored chain — keeps the continuation
    /// reply-only, exactly as before.
    pub(super) fallback_chat: Option<String>,
}

/// Hands out a fresh [`CardSession::chain_id`] per created session.
fn next_chain_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
            chain_id: next_chain_id(),
            fallback_chat: None,
        }
    }

    /// This session's chain identity (see the field's docs).
    pub(super) fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Whether the accumulator still owes an unrendered orphan gap (spec #561,
    /// review #569): the durable fact on the Chain Record is the only record of
    /// that tail until a read places it and a confirmed write carries it, so
    /// the record must not be released while it waits.
    pub(crate) fn owes_pending_gap(&self) -> bool {
        self.acc.pending_gap.is_some() && !self.acc.gap_rendered
    }

    /// True while this card belongs to a Turn that has not finished: the pull
    /// condition for `/card` (ADR-0043, 2026-09-22 amendment). A terminal card
    /// (Done/Error/Retried/Stopped and the collected waiting states
    /// `Superseded`/`SwitchedAway`) stays in the cards handle's map until the
    /// next Turn replaces it, so the map's key alone does not mean a live card.
    /// A `Waiting` card reads as running here (it is not terminal) — it is
    /// still the chain's newest card; Session Sync's Wake step asks the
    /// ownership verdict's routing rule ([`super::CardOwnership::routing_label`])
    /// instead, which excludes the waiting yield because the next Wake
    /// continues that chain (ADR-0059).
    pub(super) fn is_running(&self) -> bool {
        !self.is_terminal()
    }

    /// Whether this card has reached a terminal ending — the card-state rule
    /// the Chain Record's release check reads off the locked card
    /// ([`crate::bridge::chain::release_spent`]).
    pub(crate) fn is_terminal(&self) -> bool {
        self.acc.card_state.is_terminal()
    }

    /// This session's tracked card message id, once a card was sent — the card
    /// the Chain Record names and the release check consults the delivery
    /// outbox for.
    pub(crate) fn card_message_id(&self) -> Option<&str> {
        self.card_message_id.as_deref()
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

    /// Give an unused recovery claim back (spec #391, #437): a click that
    /// neither submitted nor re-attached leaves the terminal card's action
    /// working for a later click.
    pub(super) fn release_recovery_claim(&mut self) {
        self.acc.recovery_claimed = false;
    }

    /// The shared tail of a recovery re-arm (spec #391, #437): the ending's
    /// recorded failure is cleared, a live header state is restored, the
    /// recovery claim goes back (a real failure must keep a working retry; a
    /// resumed card must be re-claimable if it ends unreceived again), and the
    /// follow's directory resolves — the caller's session-mapped one, else the
    /// accumulator's work context.
    fn rearm_common(&mut self, directory: Option<String>) -> String {
        self.acc.error = None;
        self.release_recovery_claim();
        self.acc.restore_live_state();
        directory
            .filter(|directory| !directory.is_empty())
            .or_else(|| self.acc.directory.clone())
            .unwrap_or_default()
    }

    /// Re-attach this card to a run that is still alive (spec #391, ticket
    /// #393): the coordinated Error → live transition. `None` — nothing
    /// changed — when the card is not in `Error` (a new Turn may have replaced
    /// it since the retry claim) or the accumulator carries no anchor to
    /// follow (nothing the failed submission stored can be ordered against a
    /// run). On success the Error is cleared with the content untouched, the
    /// claim goes back, a live header state is restored, and the follow's
    /// fixture is returned: the anchor it watches (always `Some` here — the
    /// failed run had one) and the directory its status reads route under.
    pub(super) fn reattach(&mut self, directory: Option<String>) -> Option<(Option<TurnAnchor>, String)> {
        if self.acc.card_state != CardState::Error {
            return None;
        }
        let anchor = self.acc.turn_anchor.clone()?;
        let directory = self.rearm_common(directory);
        Some((Some(anchor), directory))
    }

    /// Re-arm an Unreceived card for the resumed run (#437): the 重新发起
    /// click interrupted and resumed the Session, so the queued message is
    /// promoted at the new run's start and this SAME card renders it — through
    /// the unreceived watch, respawned under the card's identity, which
    /// captures the Turn anchor the moment the message lands (ADR-0062).
    /// `None` — nothing changed — when the card is no longer `Unreceived` (a
    /// new Turn may have replaced it since the claim). On success the ending is
    /// cleared, a live header state is restored, the claim goes back, and the
    /// follow's fixture is returned: the accumulator's anchor (`None` for a
    /// never-landed message — the watch's own scope) and the directory its
    /// status reads route under.
    pub(super) fn rearm_unreceived(
        &mut self,
        directory: Option<String>,
    ) -> Option<(Option<TurnAnchor>, String)> {
        if self.acc.card_state != CardState::Unreceived {
            return None;
        }
        let anchor = self.acc.turn_anchor.clone();
        let directory = self.rearm_common(directory);
        Some((anchor, directory))
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

/// A retry's render baseline (#387, restated by spec #391 and the
/// generation-aware correction). It keeps the failed attempt's messages out of
/// the fresh card's turn window, and is load-bearing wherever those messages
/// can still read as belonging to it: the reused-id branch (V1 only — V2's
/// admission key takes a fresh id there) and the fresh-id branch's V2
/// killed-run case, where an admitted, unfinished message with no completion
/// stamp can still fall inside the fresh anchor's in-flight window. On the
/// settled fresh-id case it rides along inert. It travels as one fact:
/// `suppressed` is the carried frontier (written once at retry time),
/// `observed` is what this attempt has examined (the next retry unions it in).
#[derive(Default, Clone)]
pub(super) struct AttemptBaseline {
    /// Assistant message ids from EARLIER attempts: never rendered, their
    /// footer model/token capture included.
    pub(super) suppressed: std::collections::HashSet<String>,
    /// Assistant message ids THIS attempt has examined: rendered normally,
    /// and unioned into a later retry's `suppressed`.
    pub(super) observed: std::collections::HashSet<String>,
}

/// The granularity at which a ledger read compares its clock against what the
/// card last rendered (ADR-0060). The stored clock is the rendered elapsed in
/// whole seconds; the cadence decides how much of it must move to owe a flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LedgerCadence {
    /// Whole minutes — the live render and its Wake handover, whose flushes are
    /// content-driven: a per-render second clock would owe a flush on nearly
    /// every render. A read inside the same rendered minute owes nothing.
    Minute,
    /// Whole seconds — the yielded refresh: the ~8 s Session Sync reads are a
    /// waiting card's only clock, so the moment a row's rendered second moves,
    /// the card owes the PATCH (≈0.125 QPS per waiting card, far below
    /// Feishu's per-message cap).
    Second,
}

/// What a ledger refresh moved (#457), split so the live render can tell real
/// work from clock churn: `rows` — a visible fact moved (membership, a row's
/// label or status, the fragment's words, the output window's text) — is
/// progress; `clock` — only a rendered elapsed, a fragment's age or the output
/// window's cutoff crossed the path's cadence — owes the flush but is not new
/// work. Both halves flush the card; the external renderer's idle bound renews
/// on `rows` alone, or a silent task's ticking age would keep its card live
/// forever.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct LedgerChange {
    pub(super) rows: bool,
    pub(super) clock: bool,
}

impl LedgerChange {
    /// Whether the card owes a flush (either half moved).
    pub(super) fn owes(self) -> bool {
        self.rows || self.clock
    }
}

/// The most staged Rendered Cursors one accumulator retains (spec #561,
/// review #569): the newest stage plus the delivered-but-unconfirmed ones a
/// delayed confirmation still needs. Real interleavings hold two; the cap only
/// bounds a pathological pile-up (every stage resolves on its own write's
/// outcome or the next drain).
const MAX_STAGED_CURSORS: usize = 4;

/// The most staged Wake Watermarks one accumulator retains (spec #561, review
/// #569): the same retention as the cursors, for the same reason — a delivered
/// mark keeps its drain. Wakes are rare; the cap is headroom, not a budget.
const MAX_STAGED_MARKS: usize = 4;

/// The Rendered Cursor staged for one built card body (spec #561): the flush
/// stages it before the write, and a confirmed write drains its exact stage.
/// In-memory only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StagedCursor {
    /// The stage generation, assigned by [`StreamAccumulator::stage_cursor`]:
    /// a confirmation names the generation it confirms, so a body staged later
    /// can never be advanced by an earlier write's confirmation (spec #561,
    /// review #569).
    pub(super) id: u64,
    /// The card the body is written to; `None` for a create (the id is only
    /// known once the send lands, and creates are never outbox-retried).
    pub(super) card_message_id: Option<String>,
    pub(super) cursor: RenderedCursor,
    /// The Pending Card Update sequence a recoverably failed write left owed:
    /// the drain reconcile confirms the cursor only once THAT payload
    /// delivers. `None` while no failure is owed.
    pub(super) awaiting_seq: Option<u64>,
    /// How far the body carried the chain's pending orphan gap (spec #561,
    /// review #569), captured per built body: the confirmation persists the
    /// advance, and clears the durable gap only when the body reached its end.
    pub(super) gap: Option<GapCoverage>,
}

impl StagedCursor {
    /// The exact write a confirmation may advance (spec #561, review #569):
    /// its stage generation and, for a drain, the Pending Card Update sequence
    /// whose delivery was verified. Whatever the accumulator staged since is
    /// left untouched for its own confirmation.
    fn identity(&self) -> StagedCursorId {
        StagedCursorId {
            id: self.id,
            awaiting_seq: self.awaiting_seq,
        }
    }
}

/// The exact staged write a confirmation may advance (spec #561, review #569):
/// its stage generation and, for a drain, the Pending Card Update sequence
/// whose delivery was verified. Whatever the accumulator staged since is left
/// untouched for its own confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StagedCursorId {
    pub(crate) id: u64,
    pub(crate) awaiting_seq: Option<u64>,
}

/// The Wake Watermark staged for the card body about to be written (ADR-0061):
/// the announce value the body carries, plus the stage generation a
/// confirmation must match (spec #561, review #569) so a watermark staged
/// after the write is never drained by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StagedWatermark {
    pub(super) wake_id: String,
    pub(super) created_ms: i64,
    pub(super) id: u64,
}

/// The fate of the write carrying a flush's **terminal slice** (spec #602,
/// ticket #607): whether delivery accepted it now, still owes it as a pending
/// retry, or can never carry it. `Default` is [`Self::Delivered`] so a
/// [`StreamAccumulator`] with no flush behind it does not suppress — before any
/// terminal write there is nothing to doubt. A later flush overwrites it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum EndingWrite {
    /// The terminal slice is on Feishu: a direct PATCH delivered, or a
    /// size-split continuation create landed it on the new card.
    #[default]
    Delivered,
    /// The terminal slice is still owed by the delivery layer's pending retry
    /// as the keyless write at this exact sequence. The Completion Notice arms
    /// against THIS sequence, never the card's current newest (spec #607).
    Owed(u64),
    /// The terminal slice is not on Feishu and nothing will carry it as this
    /// flush's terminal write: a permanently refused or suspended ending PATCH,
    /// a size-split continuation create that failed, a size split with no
    /// reachable continuation target, or a continuation create that is itself
    /// over the budget with further slices still owed (the next slice's send
    /// overwrites this transient state; if the flush stops first, the notice is
    /// suppressed). Never announced over (spec #607).
    Failed,
}

/// Accumulates streaming state for one session.
#[derive(Default, Clone)]
pub(super) struct StreamAccumulator {
    /// The card's display state. Private to this module: it moves only through
    /// intent methods — [`Self::apply_ending`] (an ending), the live-render
    /// marks ([`Self::mark_streaming`] / [`Self::mark_reasoning`] / the adoption
    /// promotion [`Self::mark_adopted_live`]), the recovery mark
    /// ([`Self::mark_retried`]), the resume/collect/reset transitions
    /// ([`Self::restore_live_state`], [`Self::set_resuming`],
    /// [`Self::continue_on_new_card`], [`Self::collect_waiting`]) and the
    /// residual restore ([`Self::restore_ending`]); [`Self::set_card_state`] is
    /// the `#[cfg(test)]` seam. Read through [`Self::card_state`].
    card_state: CardState,
    /// This turn's card-content fallback (see [`CardFallback`]): starts at
    /// `None` and is advanced when Feishu rejects a card it built. A fresh turn
    /// starts clean and re-tries the normal rendering. Private to this module:
    /// advanced by [`Self::advance_card_fallback`] and reset to `None` by
    /// [`Self::continue_on_new_card`]; read through [`Self::card_fallback`].
    card_fallback: CardFallback,
    /// The fate of the write carrying this card's flush's **terminal slice**
    /// (spec #602, ticket #607). The Completion Notice reads it: [`EndingWrite::Delivered`]
    /// announces at once, [`EndingWrite::Owed`] arms against that exact
    /// sequence, and [`EndingWrite::Failed`] suppresses a notice over a tail
    /// that never reached Feishu. Private to this module: set by
    /// [`Self::set_ending_write`]; read through [`Self::ending_write`].
    ending_write: EndingWrite,
    /// The card's streamed assistant text, in arrival order — the reply body.
    /// Private to this module: written only by [`Self::push_text_lead`] (the
    /// sink for `push_text` / `push_text_at` / `push_text_from` /
    /// `replace_text_run`); read through [`Self::text`].
    text: String,
    /// The card's streamed reasoning text, in arrival order. Private to this
    /// module: written only by [`Self::push_reasoning_lead`] (the sink for
    /// `push_reasoning` / `push_reasoning_from` / `replace_reasoning_run`); read
    /// through [`Self::reasoning`].
    reasoning: String,
    /// Tool panels keyed by call ID (current state; `timeline` keeps order).
    /// Private to this module: written by [`Self::push_tool_from`], the seeded
    /// delivery [`Self::mark_delivered_part`] and the liveness attach
    /// [`Self::set_tool_liveness`]; read through [`Self::tool_panel_current`],
    /// [`Self::tool_settled`], [`Self::tool_count`],
    /// [`Self::has_rendered_content`], [`Self::has_live_tool`],
    /// [`Self::running_tool`], [`Self::live_task_children`] and the card build.
    tools: IndexMap<String, ToolPanel>,
    /// Live (unfinished) Tool Panels keyed by call ID: the timeline key,
    /// server start time and element identity allocated when the call first
    /// appeared. A live panel renders in the card TAIL, so a split can never
    /// strand it on a closed card; it joins `timeline` — with the identity it
    /// was born with — only when the tool settles (ADR-0045).
    live_tools: IndexMap<String, LiveTool>,
    /// The seed's live set (spec #561): the tool call ids a takeover's
    /// projection seed handed this successor — the calls whose newest
    /// DELIVERED state was `running`, or, for a cursorless record's fallback,
    /// the orphaned Turn's still-live calls. The set is the reconciliation
    /// scope — every render read resolves each id against the WHOLE transcript,
    /// past the Turn window whose membership would drop a long-running call's
    /// message — so the panel keeps the transcript's current status and output
    /// and joins the timeline at its server start key once it settles (or once
    /// the Turn's own window renders it, which retires the id). A seeded call
    /// is display-only: it renders only while a live renderer owns the card (a
    /// settled/end-of-turn card omits a still-running one, so no `⏳` outlives
    /// the Turn) and never counts as the Turn's own unfinished tools for the
    /// settle guard. In-memory only, never durable; empty for every card that
    /// did not take over an orphan. Private to this module: seeded whole by
    /// [`Self::seed_projection`] (a takeover's projection seed) and each id
    /// retired by [`Self::retire_seeded_call`] (the renderer's own-window, gap
    /// and scope walks and the reconciliation); read through
    /// [`Self::seeded_call_ids`] and, inside this module, [`Self::has_live_tool`]
    /// and [`Self::omitted_live_seeded`], with the `#[cfg(test)]`
    /// [`Self::seeded_calls`] as the test read.
    seeded_calls: std::collections::HashSet<String>,
    /// The latest `todowrite` panel of this turn, rendered as a card-TAIL
    /// status section instead of a timeline row. A timeline row would freeze on
    /// whichever card the call landed on: once that card finalizes (a long
    /// turn splits into several), later updates land on an already-sent card
    /// and stay invisible. The tail rides the live card, so every flush shows
    /// the current list. Each later call replaces it in place. Private to this
    /// module: set (with its clock) only by [`Self::set_todo_panel`] — the
    /// renderer's and the seed's one write; read through
    /// [`Self::tool_panel_current`], [`Self::has_live_tool`],
    /// [`Self::has_rendered_content`] and the card tail render.
    todo_panel: Option<ToolPanel>,
    /// The server start time of the todowrite call that last refreshed
    /// [`Self::todo_panel`] — its panel header shows when the list was last
    /// written. Moves with the panel: private to this module, set only by
    /// [`Self::set_todo_panel`]; read through [`Self::tool_panel_current`] and
    /// the card tail render.
    todo_shown_at: Option<i64>,
    /// The Background Task Ledger (ADR-0060): the Session's live Background
    /// Tasks, rebuilt from each transcript read ([`Self::set_ledger_from_read`])
    /// and rendered as a card-TAIL section, so the list rides the newest (live)
    /// card. Empty renders nothing — V1 has no Background Task facts, so its
    /// ledger is always empty. Private to this module: replaced by the one
    /// read-writing site [`Self::set_ledger_change`] and cleared when the card
    /// is collected ([`Self::collect_waiting`]); read through
    /// [`Self::ledger_has_unconfirmed`] and the card tail render.
    ledger: Vec<TaskLedgerRow>,
    /// Each live Background Task's last successfully gathered child liveness,
    /// keyed by the task's call id (spec #501): a read whose child gather
    /// failed (or never names the child) keeps rendering this fragment — the
    /// timestamps inside are never refreshed, so its age grows truthfully
    /// (ADR-0054) — and a fresh gather replaces its call's entry. Pruned to the
    /// live subagents on every read, so a retired task's fragment leaves with
    /// its row and the map cannot grow past the session's current tasks.
    ledger_activity: HashMap<String, TaskLiveness>,
    /// Each live shell's last established output window, keyed by shell id
    /// (spec #588, #592): a read that spent no output read on a shell (the
    /// shared cycle's throttle) keeps rendering this window, while a read that
    /// spent one and found nothing clears it — a stale window never poses as
    /// current, and a failure never renders a placeholder. Pruned to the live
    /// shells on every read, like [`Self::ledger_activity`].
    ledger_outputs: HashMap<String, crate::backend::ShellOutputWindow>,
    /// The ledger's render clock ([`Self::set_ledger`]): each row's rendered
    /// numbers — a shell row's elapsed, and any row's activity fragment age, in
    /// whole seconds as of the read that last moved the section (ADR-0060, spec
    /// #501) — the very values the rows last rendered. A subagent row's total
    /// runtime is never one of them. The rows alone cannot tell that a yielded
    /// card's elapsed or activity age went stale, and the rendered seconds move
    /// on every read — so each path compares this key at its own cadence
    /// ([`LedgerCadence`]): the live render at whole minutes (no per-render
    /// clock churn), the yielded refresh at whole seconds (its reads are the
    /// card's only clock).
    ledger_clock: Vec<crate::feishu::card::ledger::LedgerRowClock>,
    /// Text, reasoning, tool and receipt entries ordered by their key (the
    /// server-side part start time) — the card is built from this, so message ↔
    /// tool interleaving is preserved even when a part renders late. Private to
    /// this module (no external writer): maintained by [`Self::insert_item`],
    /// [`Self::insert_kind_after`], [`Self::push_text_lead`],
    /// [`Self::push_reasoning_lead`], [`Self::push_tool_from`] and
    /// [`Self::drop_source_run`]; read through [`Self::has_receipt_prefix`] and
    /// the card build.
    timeline: Vec<TimelineItem>,
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
    /// receipt is a timeline entry). Private to this module (no external
    /// reader or writer): it moves only through [`Self::add_interaction`],
    /// [`Self::resolve_interaction`], [`Self::dismiss_interaction`] and
    /// [`Self::update_question_state`]; `resolve_vanished` sweeps through
    /// [`Self::resolve_interaction`].
    interactions: Vec<InteractionBlock>,
    /// Timeline index the CURRENT card starts rendering from. When a card fills
    /// up (Feishu component limit) it is finalized with a "to be continued"
    /// marker and `render_from` advances — a fresh continuation card renders the
    /// remaining timeline from there. Private to this module: advanced by the
    /// builds ([`Self::build_card_with_info`] / [`Self::build_finalized_handoff`])
    /// and the source prune [`Self::drop_source_run`] re-bases it; the failed-write
    /// rewind goes through [`Self::restore_render_boundary`], the projection
    /// arm's fenced retry through [`Self::rewind_render_boundary`]. Read through
    /// [`Self::render_from`].
    render_from: usize,
    /// Provider ID of the model answering this turn (e.g. "opencode-go").
    /// Private to this module: set by [`Self::apply_footer_model`] (a transcript
    /// read) and the `set_model` test seam; read only inside this module (the
    /// footer render and the context-window memo key).
    provider_id: Option<String>,
    /// Model ID of the model answering this turn (e.g. "deepseek-v4-flash").
    /// Private to this module: set by [`Self::apply_footer_model`] (a transcript
    /// read) and the `set_model` test seam; read inside this module and through
    /// the `#[cfg(test)]` [`Self::model_id`].
    model_id: Option<String>,
    /// The `/think` variant cola sent this turn (e.g. "high"), shown as
    /// `model@variant` on the footer. Sourced from the session store — the
    /// server reports the model but not the variant. Private to this module:
    /// set by [`Self::apply_footer_model`] and [`Self::set_variant`]; read
    /// inside this module and through the `#[cfg(test)]` [`Self::variant`].
    variant: Option<String>,
    /// Context tokens the model consumed this turn (includes cached prefix), for
    /// the context-usage segment in the card footer. Private to this module:
    /// set by [`Self::set_context_tokens`]; read inside this module and through
    /// the `#[cfg(test)]` [`Self::context_tokens`].
    context_tokens: i64,
    /// The answering model's context-window size (tokens), fetched from
    /// `GET /provider` and memoized for the turn — `None` before the lookup or
    /// when the server reports none (ADR-0044). Private to this module (no
    /// external reader or writer): written by `refresh_context_window`, read
    /// through [`Self::current_context_window`] / [`Self::context_sig`].
    context_window: Option<i64>,
    /// The (provider, model) pair [`Self::context_window`] was fetched for; a
    /// mismatch triggers a re-fetch, since the answering model can change.
    /// Private to this module (no external reader or writer): moves with
    /// [`Self::context_window`] in `refresh_context_window`.
    context_window_key: Option<(String, String)>,
    /// Working directory of the session, shown in the card footer. Private to
    /// this module: set by [`Self::apply_work_context`] (and
    /// [`Self::attach_work_context`]); read through [`Self::directory`].
    directory: Option<String>,
    /// Project name (directory basename) for the Turn Footer's 📁 segment.
    /// Private to this module (no external reader or writer): set by
    /// [`Self::apply_work_context`].
    project_name: Option<String>,
    /// Git branch: captured at turn start, refreshed at turn end (ADR-0019);
    /// the short commit hash when detached. Private to this module (no external
    /// reader or writer): set by [`Self::apply_git_state`].
    branch: Option<String>,
    /// Working tree differs from HEAD, including untracked files: captured at
    /// turn start, refreshed at turn end. Only set alongside `branch`
    /// (ADR-0019: the halves are omitted together). Private to this module (no
    /// external reader or writer): set by [`Self::apply_git_state`].
    dirty: bool,
    /// The session's directory is a linked git worktree (#433): the 📁
    /// segment marks it with 🌲. Moves with the branch/dirty halves. Private to
    /// this module (no external reader or writer): set by
    /// [`Self::apply_git_state`].
    worktree: bool,
    /// The session/thread name; shown as the card subtitle so the header can
    /// stay focused on state (the question is already in the reply context).
    /// Private to this module: initialised by [`Self::new`] and re-stamped
    /// mid-turn by [`Self::set_title`]; read through [`Self::title`].
    title: String,
    /// The failure line recorded on the card, if any. Private to this module:
    /// it moves only through [`Self::apply_ending`] (a failure ending records
    /// its line, a stop/unreceived ending clears one),
    /// [`Self::restore_ending`] (an in-place residual re-stamps the ending it
    /// carried) and [`Self::continue_on_new_card`] (a Wake continuation starts
    /// clean); a recovery re-arm clears it through `CardSession::rearm_common`.
    /// Read through [`Self::error`]; [`Self::set_error`] is the `#[cfg(test)]`
    /// seam.
    error: Option<String>,
    /// The message this card replies to — where a recovery click's
    /// re-submission posts and a split continuation's replies land. Private to
    /// this module: set by [`Self::set_reply_target`]; read through
    /// [`Self::reply_to_message_id`].
    reply_to_message_id: Option<String>,
    /// This turn's session id, carried on the error-card retry button so the
    /// card callback can find the accumulator + card to reuse. Private to this
    /// module: set by [`Self::set_session`]; read through [`Self::session_id`].
    session_id: Option<String>,
    /// The full original prompt text of this turn, kept so the error-card
    /// "retry" button can re-submit it without the user retyping. Private to
    /// this module: set by [`Self::set_prompt`], cleared by
    /// [`Self::continue_on_new_card`] (a Wake continuation carries no question
    /// to re-ask); read through [`Self::prompt`].
    prompt: Option<String>,
    /// Whether this card's terminal recovery action — spec #391's Error retry,
    /// #437's Unreceived 重新发起 — or its waiting-card cleanup (spec #588,
    /// #590) has been claimed. The click is acked immediately, so a second
    /// click can arrive before the action's own marking lands; the claim is
    /// checked and set under the cards lock, so exactly one click runs the
    /// action — and the cleanup's pipeline releases it once done (a recovery
    /// action keeps it, its card being terminal).
    pub(super) recovery_claimed: bool,
    /// The id this turn's user message carries (`msg_cola_…`, ADR-0026), so a
    /// later error-card retry reuses it and the server deduplicates by id.
    /// Private to this module: set by [`Self::set_cola_message_id`]; read
    /// through [`Self::cola_message_id`].
    cola_message_id: Option<String>,
    /// Who sent the prompt (Feishu open_id), so the group completion notice can
    /// be replied to them / @-mention them. Private to this module: set by
    /// [`Self::set_requester`]; read through [`Self::requester_open_id`].
    requester_open_id: Option<String>,
    /// Whether the prompt came from a group chat (completion notice is
    /// group-only). Private to this module: set by [`Self::set_is_group`]; read
    /// through [`Self::is_group`].
    is_group: bool,
    /// When the Turn that owns this card chain started (the same instant the
    /// Turn's own `started_at` records), for the Completion Notice's p2p
    /// long-task threshold. The quiet true end (ADR-0060) is settled by Session
    /// Sync, which has no Turn object: the card carries the notice's clock.
    /// `None` for a card no Turn started (an external render's, or a Wake
    /// continuation armed after a restart) — a Wake continuation never owes the
    /// notice anyway, its own send was the notification (ADR-0059). Private to
    /// this module: set by [`Self::set_turn_started_at`]; read through
    /// [`Self::turn_started_at`].
    turn_started_at: Option<std::time::Instant>,
    /// The Chat/Topic's turn generation at this turn's start (ADR-0043),
    /// assigned by [`crate::bridge::reminder::ReminderState::begin_turn`]. The Instant
    /// Reminder pin lifecycle reads it so every pin carries the turn it
    /// belongs to and a stale clear cannot unpin a newer turn's pin. Private to
    /// this module: set by [`Self::set_turn_generation`]; read through
    /// [`Self::turn_generation`].
    turn_generation: Option<u64>,
    /// Text/reasoning parts already rendered into this card, keyed by their
    /// typed content — dedupes incremental polling. Reasoning/text parts are
    /// written empty first and updated with full text, so they are only tracked
    /// once they have content. Tool panels are deduped separately: the current
    /// panel in `tools` / `todo_panel` IS the tool's rendered revision, so a
    /// re-render happens exactly when the typed call's visible content changed
    /// (including a `todowrite` list rewritten with same-length items). Private
    /// to this module: written only by [`Self::mark_rendered`] (the renderer's
    /// `render_part`); read through [`Self::has_rendered_part`],
    /// [`Self::rendered_part_count`] and [`Self::has_rendered_content`].
    rendered_parts: std::collections::HashSet<RenderedPart>,
    /// Text/reasoning content a SEEDED earlier card already delivered, keyed by
    /// the message it appeared in (spec #561, review #569): the seed's
    /// at/before-frontier parts and the frontier part itself, marked by
    /// [`Self::mark_delivered_part`]. Message-scoped on purpose — the content
    /// in `rendered_parts` would swallow a NEW Turn's own part that happens to
    /// have identical text, and the race must render the new answer. Private to
    /// this module: written only by [`Self::mark_delivered_part`]; read through
    /// [`Self::part_seeded_delivered`] and [`Self::has_rendered_content`].
    seeded_delivered: std::collections::HashSet<(crate::backend::MessageId, RenderedPart)>,
    /// Monotonic count of observable progress this accumulator has produced
    /// (#457): bumped by every render stage that changes content, a tool
    /// panel revision, a live fragment, ledger rows, or context tokens —
    /// never by the header tick or a rendered clock number. The external
    /// renderer renews its idle bound when this advances, so progress a pass
    /// had already rendered before it was abandoned by its timeout still
    /// counts (a cancelled pass cannot return its stats). Private to this
    /// module: advanced only by [`Self::bump_progress_mark`]; read through
    /// [`Self::progress_mark`].
    progress_mark: u64,
    /// Assistant message ids this accumulator must NOT render and the ones it
    /// has observed — the retry render baseline (#387). Kept as one fact: the
    /// two sets are seeded and consumed together, and only this type's docs
    /// carry the invariant. Private to this module: `suppressed` is set by
    /// [`Self::carry_attempt_baseline`] (a retry carries the prior attempt's
    /// frontier) and `observed` is grown by [`Self::observe_message`] (each
    /// render read records the message it examined); read through
    /// [`Self::baseline_suppresses`].
    baseline: AttemptBaseline,
    /// Whether this card continues its Turn after a Wake (ADR-0059): the
    /// chain moved on a new card after the previous one yielded or ended, so
    /// this card carries no question to re-ask — an Error ending never offers
    /// Retry, whatever the previous attempt's facts were. Private to this
    /// module: set by [`Self::mark_wake_continuation`] — from
    /// [`Self::continue_on_new_card`] and the fresh arms `arm_projected_card`
    /// and `arm_wake_continuation`; read through [`Self::wake_continuation`].
    wake_continuation: bool,
    /// The Wakes whose completion this chain has already announced: marked by
    /// its own opening 承接 line (a Wake that opened a card) or by a
    /// completion entry — the merged-path render of a live card, or a yielded
    /// card's ledger refresh. One mark per Wake id; the set is kept across
    /// [`Self::continue_on_new_card`], so a Wake that resumes a
    /// wake-continuation card still marks exactly once (ADR-0059). Synthetic
    /// retirements — a runtime/child-evidence retirement or the user's cleanup
    /// — share the set under their `runtime:<call_id>` key, marked through
    /// [`Self::announce_synthetic`]: the same exactly-once gate, staging
    /// nothing durable (spec #588, review PR #595). This is the ANNOUNCEMENT
    /// gate (it keeps an entry single); the Wake step's in-place decision reads
    /// [`Self::handed_over_wakes`], because an entry a ledger refresh placed is
    /// not a handoff. Private to this module (no external reader or writer):
    /// written by [`Self::announce_wake`] and [`Self::announce_synthetic`]
    /// through the private `announce_entry` (the one insert), read by
    /// [`Self::wake_announced`].
    announced_wakes: std::collections::HashSet<String>,
    /// The Wakes whose resumed work this chain has HANDED OVER to a card
    /// (ADR-0066): a 承接 line's split (a continuation card takes it) or the
    /// in-place resume (the same card takes it). The Wake step's in-place gate
    /// keys on THIS, never on `announced_wakes`: a yielded card's ledger
    /// refresh places a completion entry while the Wake's work is still
    /// unrendered (live 2026-10-01 — the resumed message's text part was empty
    /// at the read that placed it), and that entry must not read as a handoff,
    /// or the work would split into a 承接 card when it arrives. Once a Wake IS
    /// handed over, a later tail past it is a Wake-less content diff: it renders
    /// in place on the chain's open yielded card, never a second continuation
    /// (spec #602). Like the announcement set, one mark per Wake id, kept across
    /// [`Self::continue_on_new_card`]. Private to this module: written by
    /// [`Self::hand_over_wake`]; read through [`Self::has_handed_over_wake`].
    handed_over_wakes: std::collections::HashSet<String>,
    /// Whether this chain's ONE neutral residual continuation has already
    /// fired (spec #602, ticket #606): a Wake-less part the Backend wrote after
    /// the run reported idle renders on exactly one neutral card per request.
    /// Set when the residual split's receipt is written (the same exactly-once
    /// point the Wake handoff's marks are set), or — when the residual has no
    /// reachable reply target to split onto — when the tail lands in place on
    /// the existing card ([`Turn::render_residual_in_place`]), so the floor
    /// still routes every later late tail to that same in-place path. Kept
    /// across [`Self::continue_on_new_card`], so a later pass over further late
    /// content can never post a second neutral card. Distinct from
    /// [`Self::handed_over_wakes`] — the residual answers no Wake — and never
    /// persisted: a restart has no in-process chain to re-decide on, and the
    /// Fresh (recordless) path is Wake-scoped, so a Wake-less read there owes
    /// nothing. Private to this module: set by
    /// [`Self::mark_residual_card_posted`] (`flush_card_locked` and
    /// `Turn::render_residual_in_place`); read through
    /// [`Self::residual_card_posted`].
    residual_card_posted: bool,
    /// The Wake Watermarks this chain has staged but not yet drained:
    /// `(wake id, created_ms)` stages, advanced by [`Self::announce_wake`] and
    /// persisted into the durable Wake Watermark once a card write actually
    /// carries them (ADR-0061). In-memory `announced_wakes` is the
    /// exactly-once gate. Newest last; a delivered body's mark stays drainable
    /// even after a newer mark is staged (spec #561, review #569), so a
    /// restart can never re-announce a Wake the card already showed. Bounded
    /// by [`MAX_STAGED_MARKS`]. Private to this module (no external reader or
    /// writer): staged by [`Self::announce_wake`], read by
    /// [`Self::pending_watermark_id`], drained by
    /// [`Self::take_staged_watermark`].
    pending_watermarks: Vec<StagedWatermark>,
    /// The chain's last confirmed Rendered Cursor (spec #561): the in-memory
    /// mirror of the record's fact. Every staged candidate derives from it,
    /// and a fresh accumulator taking over a chain is seeded from the record
    /// (`Turn::track_live_card`), so a successor's first write can never clear
    /// the chain's frontier. Private to this module: its production writers are
    /// the confirmed take ([`Self::take_staged_cursor`]), the re-point restore
    /// ([`Self::seed_cursor_if_empty`]) and the projection's resolved seed
    /// ([`Self::seed_projection`], spec #561); the `#[cfg(test)]` `set_cursor`
    /// seam serves fixtures only.
    cursor: RenderedCursor,
    /// The projection's render seed (spec #561, ticket #563): resolved against
    /// the read being rendered, it makes everything at or before the cursor's
    /// frontier count as delivered — the frontier part renders only its
    /// undelivered suffix — while the live set resolves by identity against
    /// the whole read. `None` on every ordinary accumulator. Private to this
    /// module: seeded by [`Self::seed_projection`]; read through [`Self::seed`],
    /// its frontier advanced through [`Self::record_seed_frontier_delivered`].
    seed: Option<CursorSeed>,
    /// The orphan gap this accumulator owes (spec #561, review #569): the
    /// durable gap, rendered once by the first render read that can place it.
    /// The chain's Rendered Cursor keeps advancing past the delivered content
    /// that follows the gap — one frontier cannot express both — so the gap's
    /// recovery lives on the record instead. Private: read and seeded through
    /// [`Self::pending_gap`] / [`Self::set_pending_gap`].
    pending_gap: Option<crate::bridge::chain::PendingGap>,
    /// The messages whose parts the pending gap's own walk rendered (spec #561,
    /// review #569). Their content sits EARLIER in the read than the chain's
    /// confirmed frontier, so the chain cursor must never be built from their
    /// entries — while they are exactly what the gap's progress reads. Private
    /// to this module: grown by [`Self::mark_gap_message`]; read only inside
    /// this module.
    gap_messages: std::collections::HashSet<MessageId>,
    /// Whether this accumulator's timeline holds the pending gap's content
    /// (spec #561, review #569): the first confirmed write of a body that
    /// includes it clears the durable fact, so a crash before that write keeps
    /// the gap recoverable. Private to this module: set by
    /// [`Self::mark_gap_rendered`]; read through [`Self::gap_rendered`].
    gap_rendered: bool,
    /// Whether the gap's last render came from a TRUNCATED read (spec #561,
    /// review #569): the read is a prefix, so the gap's end may be beyond the
    /// page cap — its coverage can never be complete, and the gap stays owed
    /// until a complete read shows its end. Private to this module: set by
    /// [`Self::set_gap_truncated`]; read only inside this module.
    gap_truncated: bool,
    /// The chain's durable Wake Watermark as a floor (spec #561, review #569):
    /// every Wake at or below this `created_ms` was announced by an earlier
    /// card, so this accumulator's render neither re-inserts its completion
    /// entry nor stages a watermark advance for it. Seeded when a card starts
    /// rendering a session (`Turn`'s start, the projection/external arms, the
    /// Wake continuation); a strictly newer Wake still announces. `None` when
    /// the session announced no Wake. Private to this module: seeded by
    /// [`Self::set_wake_floor`] (`Turn::seed_wake_floor`); read through
    /// [`Self::wake_floor`].
    wake_floor: Option<i64>,
    /// The Rendered Cursors of the card bodies built and not yet confirmed,
    /// newest last (spec #561, review #569): the flush stages one before each
    /// write and a confirmed write drains its exact stage — while a stage whose
    /// payload a drain delivered but has not confirmed yet is RETAINED, so a
    /// newer body replacing the pending slot can never lose its confirmation.
    /// Bounded by [`MAX_STAGED_CURSORS`]. Private: staged and drained only by
    /// [`Self::stage_cursor`] / [`Self::take_staged_cursor`].
    staged_cursors: Vec<StagedCursor>,
    /// The stage generation handed to the next [`StagedCursor`] (spec #561,
    /// review #569): a confirmation names the generation it confirms, so a
    /// body staged later is never advanced by an earlier write's confirmation.
    cursor_stage_seq: u64,
    /// The generation of the newest stage whose cursor the base reflects
    /// (spec #561, review #569): a confirmation older than this is superseded
    /// and may never move the cursor backwards.
    confirmed_cursor_stage: u64,
    /// The stage generation handed to the next [`StagedWatermark`] — the same
    /// identity rule for the Wake Watermark's drain.
    watermark_stage_seq: u64,
    /// The Turn's anchor, captured as one fact: the identity of the user
    /// message this turn answers together with that message's server time. An
    /// external render arms with the external message's anchor directly; a
    /// cola-sent turn captures it from the stored user message on the first
    /// poll it appears in (matched by `cola_message_id`, ADR-0026). It is the
    /// turn's single anchor: the header date, the turn filter and the renderer
    /// replacement guard all read it, so cola's own clock is never compared
    /// against the server's (#183, #190).
    pub(super) turn_anchor: Option<TurnAnchor>,
    /// Whether this card has already shown the neutral waiting line
    /// 「⏳ 等待当前运行接收…」 (ADR-0062): the unreceived watch pushes it
    /// exactly once while the Session reads live but the submitted message
    /// has not landed — a genuine long tool call and a dead run look
    /// identical, so the line waits out the follow grace and never doubles.
    /// Private to this module: set by [`Self::mark_receive_hint_shown`] (the
    /// unreceived watch); read through [`Self::receive_hint_shown`].
    receive_hint_shown: bool,
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

    /// Carry a previous attempt's render baseline into this accumulator
    /// (#387): every message the failed attempt SUPPRESSED or OBSERVED is
    /// suppressed here. `Turn::start` calls this on an explicit retry signal
    /// (spec #391): the reused-id branch renders the same `msg_cola_` user
    /// message (ADR-0026), so without the baseline the fresh card would replay
    /// the whole failed turn window — the new attempt's messages alone render,
    /// and the union chains across repeated retries. The new-id branch carries
    /// it inertly: a fresh anchor's window holds none of the old ids.
    pub(super) fn carry_attempt_baseline(&mut self, previous: &Self) {
        self.baseline.suppressed = previous
            .baseline
            .suppressed
            .union(&previous.baseline.observed)
            .cloned()
            .collect();
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
            self.worktree = state.worktree;
        }
    }

    /// Whether any tool panel of this turn is still running — the header
    /// phase and the live-state resume both read it. Delegates to
    /// [`Self::running_tool`], so the selection rule (a timeline tool first,
    /// then the `todowrite` tail) lives in exactly one place.
    fn has_running_tool(&self) -> bool {
        self.running_tool().is_some()
    }

    /// Whether the current panel IS this tool call's rendered revision — the
    /// render dedup rule: a `todowrite`'s card-tail panel and its write clock,
    /// any other call's entry in the `tools` map. An unchanged poll then skips
    /// the part; a revision (running → settled, late output, a rewritten list)
    /// differs and re-renders.
    pub(super) fn tool_panel_current(&self, call: &ToolCall) -> bool {
        if call.identity.name == "todowrite" {
            self.todo_panel.as_ref().is_some_and(|panel| panel.call() == call)
                && self.todo_shown_at == call.started_at
        } else {
            self.tools
                .get(&call.identity.call_id)
                .is_some_and(|panel| panel.call() == call)
        }
    }

    /// Whether the call's current panel has SETTLED (exists and is no longer
    /// live) — the seeded-call reconciliation's retire test (spec #561): a
    /// seeded identity that settled joins the timeline once and leaves the
    /// seeded set.
    pub(super) fn tool_settled(&self, call_id: &str) -> bool {
        self.tools.get(call_id).is_some_and(|panel| !panel.is_live())
    }

    /// How many tool panels the current `tools` map holds — the final-render
    /// log's tool count.
    pub(super) fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Whether this accumulator has rendered any part — the external
    /// renderer's "partial reply" probe before it finalizes on timeout: any
    /// text/reasoning mark, a seeded delivery, a tool panel or the `todowrite`
    /// tail.
    pub(super) fn has_rendered_content(&self) -> bool {
        !self.rendered_parts.is_empty()
            || !self.seeded_delivered.is_empty()
            || !self.tools.is_empty()
            || self.todo_panel.is_some()
    }

    /// Whether this part's content was already rendered into the card — the
    /// content-keyed dedup read (`renders_part` in the renderer), so equal
    /// content never renders twice (parts carry no id, AGENTS.md #9).
    pub(super) fn has_rendered_part(&self, part: &RenderedPart) -> bool {
        self.rendered_parts.contains(part)
    }

    /// Mark one text/reasoning part's content as rendered into the card — the
    /// renderer's write, guarded by its `renders_part` dedup check (spec #298),
    /// so a later poll carrying the same content dedupes instead of doubling
    /// it. Production writer: `render_part` (text and reasoning parts).
    pub(super) fn mark_rendered(&mut self, part: RenderedPart) {
        self.rendered_parts.insert(part);
    }

    /// How many text/reasoning parts the card has rendered — the render pass's
    /// new-part count and the final-render log's tally.
    pub(super) fn rendered_part_count(&self) -> usize {
        self.rendered_parts.len()
    }

    /// The seeded live-set ids, snapshotted for the reconciliation walk
    /// (spec #561): the renderer resolves each id against the whole transcript
    /// while it retires the settled ones, so it iterates a copy rather than the
    /// set it mutates.
    pub(super) fn seeded_call_ids(&self) -> Vec<String> {
        self.seeded_calls.iter().cloned().collect()
    }

    /// Retire one id from the seeded live set (spec #561): the call settled, or
    /// the Turn's own window rendered it, so it is an ordinary panel again and
    /// the display-only omission no longer applies to it. Production writers:
    /// the renderer's own-window, gap and scope walks (`render_turn_parts`,
    /// `render_pending_gap`, `render_seed_scope`) and the reconciliation
    /// (`resolve_seeded_calls`).
    pub(super) fn retire_seeded_call(&mut self, call_id: &str) {
        self.seeded_calls.remove(call_id);
    }

    /// The seeded live-set ids — a test read of the identity carry the
    /// projection seed resolved.
    #[cfg(test)]
    pub(super) fn seeded_calls(&self) -> &std::collections::HashSet<String> {
        &self.seeded_calls
    }

    /// Whether `message_id` belongs to an EARLIER attempt and must not render
    /// (#387) — the retry baseline's suppression read, shared by the render
    /// window (`render_turn_parts`) and the content-diff probe
    /// (`renders_new_content`).
    pub(super) fn baseline_suppresses(&self, message_id: &str) -> bool {
        self.baseline.suppressed.contains(message_id)
    }

    /// Record that THIS attempt has examined `message_id` (#387): a later retry
    /// unions the observed set into its suppressed frontier, so the rebuilt
    /// card streams only the new attempt. The render window's one observation
    /// write (`render_turn_parts`).
    pub(super) fn observe_message(&mut self, message_id: &str) {
        self.baseline.observed.insert(message_id.to_string());
    }

    /// Whether any live Tool Panel the Turn owns is unfinished — a call whose
    /// status is `running` or `pending` (both render `⏳`). The drain follow's
    /// Done decision waits for these to settle (#284): the card must never read
    /// `✅ 完成` over a `⏳` panel. A still-running **seeded** call (spec #561's
    /// live set, ADR-0068's successor) is deliberately not one of the Turn's
    /// own: it is display-only, so it must not extend the settle decision.
    pub(super) fn has_live_tool(&self) -> bool {
        self.tools
            .iter()
            .any(|(call_id, panel)| !self.seeded_calls.contains(call_id) && panel.is_live())
            || self.todo_panel.as_ref().is_some_and(|panel| panel.is_live())
    }

    /// The current tool panels keyed by call ID — a test read of the tool
    /// facts the render derivations maintain.
    #[cfg(test)]
    pub(super) fn tools(&self) -> &IndexMap<String, ToolPanel> {
        &self.tools
    }

    /// The header phase for the current state: None when the turn finished,
    /// was stopped or errored (no timer shown). A resumed card (`Resuming`,
    /// ADR-0066) works on like streaming — same phase, same timer — so entering
    /// the resume resets the clock and the resumed run's tools count as the
    /// Tool phase, exactly as they do on a live card.
    pub(super) fn active_phase(&self) -> Option<HeaderPhase> {
        match self.card_state {
            CardState::Loading => Some(HeaderPhase::Loading),
            CardState::Reasoning => Some(HeaderPhase::Reasoning),
            CardState::Streaming | CardState::Resuming => {
                // A running todowrite is a tail panel, not a timeline tool, but
                // it is still a running tool: ADR-0014 gives it the Tool phase
                // (and the timer reset that comes with it), like any other.
                if self.has_running_tool() {
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

    /// Restore a live card after its Error was cleared (spec #391, ticket
    /// #393): the state the card's existing content implies — Streaming when
    /// text or a running tool is on it, Reasoning when only reasoning has
    /// streamed, Loading otherwise — and a fresh phase timer, because the card
    /// was frozen in Error while the run continued (the elapsed time it held
    /// measured nothing of this activity).
    pub(super) fn restore_live_state(&mut self) {
        self.card_state = if !self.text.is_empty() || self.has_running_tool() {
            CardState::Streaming
        } else if !self.reasoning.is_empty() {
            CardState::Reasoning
        } else {
            CardState::Loading
        };
        // The Error state held the active phase at None, but the phase could
        // still be Some from before the failure: clear first so the timer is
        // always restarted (`refresh_phase` only resets on a change).
        self.current_phase = None;
        self.refresh_phase();
    }

    /// The in-place resumption (ADR-0066): a shell/subagent completion Wake
    /// resumed THIS yielded card, so it works on again — header
    /// 「🔄 后台任务完成，继续处理中…」 with a fresh phase timer, because the
    /// waiting yield stopped the old one. Render-owned (`Resuming` is not
    /// `Waiting`), so the resumed run's own settle loop streams into the card
    /// and Session Sync never double-renders; not terminal, so it can end
    /// ✅/❌/⏹ or yield back to `Waiting` while Background Tasks remain live.
    /// The content the Turn already produced, its prompt and its reply target
    /// all stay: this is still the request's own card.
    pub(super) fn set_resuming(&mut self) {
        self.card_state = CardState::Resuming;
        self.refresh_phase();
    }

    /// Apply an ending disposition to this card — the one application every
    /// ending path shares (spec #538, #539, #541): the card's state, its
    /// failure field and its phase timer, all read from the table alone. A
    /// deliberate stop discards a recorded error and an Unreceived ending
    /// never was one; a failure ending records its line (the loop's two graces
    /// carry fixed copy); `Done` and `Waiting` leave the recorded line exactly
    /// as it was. [`Disposition::Observe`] is no ending: nothing is stamped.
    pub(super) fn apply_ending(&mut self, disposition: &Disposition) {
        let Some(state) = disposition.card_state() else {
            return;
        };
        if let Some(failure) = disposition.failure() {
            self.error = Some(failure.to_string());
        } else if disposition.clears_error() {
            self.error = None;
        }
        self.card_state = state;
        self.refresh_phase();
    }

    /// The card's current display state — the header phase, the flush's
    /// continuation decision, the ownership verdict and the recovery claim all
    /// read it. The field is private; this is the only read.
    pub(super) fn card_state(&self) -> &CardState {
        &self.card_state
    }

    /// The failure line this card records, if any — the footer render and the
    /// residual restore read it. The field is private; this is the only read.
    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Mark a live card Streaming: what a text part or a running tool renders
    /// (ADR-0014). A pure state write — the render pass refreshes the header
    /// phase itself once the part is in.
    pub(super) fn mark_streaming(&mut self) {
        self.card_state = CardState::Streaming;
    }

    /// Mark a live card Reasoning: what a reasoning part renders (ADR-0014).
    /// A pure state write — the render pass refreshes the header phase itself.
    pub(super) fn mark_reasoning(&mut self) {
        self.card_state = CardState::Reasoning;
    }

    /// A live adoption is a working card from its first send (spec #561): a
    /// successor that rendered nothing would otherwise show the initial
    /// Loading 「思考中」 over a run already in flight, so promote it to
    /// Streaming. A card past Loading is left exactly as it is.
    pub(super) fn mark_adopted_live(&mut self) {
        if self.card_state == CardState::Loading {
            self.card_state = CardState::Streaming;
        }
    }

    /// Mark the failed card Retried (spec #391): header 「↩️ 已重试」, the retry
    /// button suppressed, the marker line appended, the failed content kept.
    /// `Retried` is not an ending a [`Disposition`] can name, so it is its own
    /// mark rather than an [`Self::apply_ending`] call.
    pub(super) fn mark_retried(&mut self) {
        self.card_state = CardState::Retried;
    }

    /// Restore the ending a card carried before rendering an in-place late tail
    /// (ADR-0074): the render set a live state, but a card past its wait keeps
    /// the ending it recorded. Stamps the saved state and failure together, in
    /// one write; the caller refreshes the header phase.
    pub(super) fn restore_ending(&mut self, state: CardState, error: Option<String>) {
        self.card_state = state;
        self.error = error;
    }

    /// Set the card's display state directly: a test seam standing in for the
    /// production transition the fixture would otherwise drive. The field is
    /// private; every production writer goes through an intent method.
    #[cfg(test)]
    pub(super) fn set_card_state(&mut self, state: CardState) {
        self.card_state = state;
    }

    /// Record a failure line directly: a test seam standing in for a failure
    /// ending, so a clearing ending is visible as a change.
    #[cfg(test)]
    pub(super) fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    /// The Session this card's Turn runs on, if recorded — the reply fixture
    /// the recovery re-attach and the completion notice read. Private to this
    /// module: set by [`Self::set_session`].
    pub(super) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Set the Session this card's Turn runs on (ADR-0062): at `Turn::start`
    /// and `Turn::recreate` (a 404), and on every armed successor's fresh
    /// accumulator (`arm_projected_card`, `arm_external_render`,
    /// `arm_wake_continuation`).
    pub(super) fn set_session(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
    }

    /// The message this card replies to, if recorded — where a recovery click's
    /// re-submission posts. Private to this module: set by
    /// [`Self::set_reply_target`].
    pub(super) fn reply_to_message_id(&self) -> Option<&str> {
        self.reply_to_message_id.as_deref()
    }

    /// Set (or clear) the message this card replies to (ADR-0028, ADR-0062):
    /// at `Turn::start`, on every armed successor (`arm_external_render`,
    /// `arm_wake_continuation`, `attach_and_repoint`), by `flush_card_locked`
    /// when a served pending-split continuation becomes the chain's newest
    /// anchor, and by the `Turn::set_reply_target` test seam. `None` clears the
    /// target — a refused rung falls back to the Chat's top level.
    pub(super) fn set_reply_target(&mut self, reply_to_message_id: Option<String>) {
        self.reply_to_message_id = reply_to_message_id;
    }

    /// Who sent this turn's prompt (a Feishu open_id), if recorded — the group
    /// completion notice replies to them / @-mentions them. Private to this
    /// module: set by [`Self::set_requester`].
    pub(super) fn requester_open_id(&self) -> Option<&str> {
        self.requester_open_id.as_deref()
    }

    /// Record who sent this turn's prompt (`None` when unknown).
    pub(super) fn set_requester(&mut self, requester_open_id: Option<String>) {
        self.requester_open_id = requester_open_id;
    }

    /// Whether this turn's prompt came from a group chat (the completion notice
    /// is group-only). Private to this module: set by [`Self::set_is_group`].
    pub(super) fn is_group(&self) -> bool {
        self.is_group
    }

    /// Record whether this turn's prompt came from a group chat.
    pub(super) fn set_is_group(&mut self, is_group: bool) {
        self.is_group = is_group;
    }

    /// The full original prompt this turn submitted, if recorded — the
    /// error-card "retry" re-submits it (spec #391). Private to this module:
    /// set by [`Self::set_prompt`].
    pub(super) fn prompt(&self) -> Option<&str> {
        self.prompt.as_deref()
    }

    /// Record this turn's full original prompt (set once, at `Turn::start`).
    pub(super) fn set_prompt(&mut self, prompt: &str) {
        self.prompt = Some(prompt.to_string());
    }

    /// The `msg_cola_…` id this turn's user message carries (ADR-0026), if
    /// recorded — the Chain Record's message id and the retry's dedup key.
    /// Private to this module: set by [`Self::set_cola_message_id`].
    pub(super) fn cola_message_id(&self) -> Option<&str> {
        self.cola_message_id.as_deref()
    }

    /// Record the `msg_cola_…` id this turn's user message carries (set at
    /// `Turn::start`; the rendering fixtures set it through this method too).
    pub(super) fn set_cola_message_id(&mut self, cola_message_id: &str) {
        self.cola_message_id = Some(cola_message_id.to_string());
    }

    /// When the Turn that owns this chain started, if recorded — the Completion
    /// Notice's long-task clock. `None` for a card no Turn started. Private to
    /// this module: set by [`Self::set_turn_started_at`].
    pub(super) fn turn_started_at(&self) -> Option<std::time::Instant> {
        self.turn_started_at
    }

    /// Record when this chain's Turn started (set once, at `Turn::start`).
    pub(super) fn set_turn_started_at(&mut self, started_at: std::time::Instant) {
        self.turn_started_at = Some(started_at);
    }

    /// The Chat/Topic's turn generation this turn registered (ADR-0043), if
    /// recorded — the Instant Reminder pin lifecycle reads it. Private to this
    /// module: set by [`Self::set_turn_generation`].
    pub(super) fn turn_generation(&self) -> Option<u64> {
        self.turn_generation
    }

    /// Record this turn's Chat/Topic generation (set at `Turn::start` from
    /// [`crate::bridge::reminder::ReminderState::begin_turn`], and by the
    /// `set_turn_identity` test seam).
    pub(super) fn set_turn_generation(&mut self, generation: u64) {
        self.turn_generation = Some(generation);
    }

    /// The session's working directory, if recorded — the Turn Footer's 📁
    /// segment and every directory-routed read. Private to this module: set by
    /// [`Self::apply_work_context`]; read through this.
    pub(super) fn directory(&self) -> Option<&str> {
        self.directory.as_deref()
    }

    /// The session/thread name shown as the card subtitle. Private to this
    /// module: initialised by [`Self::new`], re-stamped by [`Self::set_title`].
    pub(super) fn title(&self) -> &str {
        &self.title
    }

    /// Re-stamp the card's subtitle when the server renamed the session
    /// (ADR-0023): set by `refresh_session_title` and the `Turn::set_title`
    /// test seam.
    pub(super) fn set_title(&mut self, title: &str) {
        self.title = title.to_string();
    }

    /// The model answering this turn, if recorded — a test read of the footer
    /// fact [`Self::apply_footer_model`] sets.
    #[cfg(test)]
    pub(super) fn model_id(&self) -> Option<&str> {
        self.model_id.as_deref()
    }

    /// The `/think` variant shown on the footer, if recorded — a test read of
    /// the footer fact [`Self::apply_footer_model`] / [`Self::set_variant`]
    /// set.
    #[cfg(test)]
    pub(super) fn variant(&self) -> Option<&str> {
        self.variant.as_deref()
    }

    /// Capture one transcript message's footer model facts (ADR-0019,
    /// ADR-0044): the answering model, and — when the decoder reports them — the
    /// provider and the variant. An EMPTY provider is V1's "none", so it leaves
    /// the last known provider in place; an absent variant likewise leaves the
    /// last known one. Both the render poll and the final reconcile call this
    /// through `capture_footer_model`, so the two cannot drift.
    pub(super) fn apply_footer_model(&mut self, model_id: &str, provider_id: &str, variant: Option<&str>) {
        self.model_id = Some(model_id.to_string());
        if !provider_id.is_empty() {
            self.provider_id = Some(provider_id.to_string());
        }
        if let Some(variant) = variant {
            self.variant = Some(variant.to_string());
        }
    }

    /// Set (or clear) the `/think` variant shown on the footer: a Wake
    /// successor's copied fixture and a Turn's own `/think` selection.
    pub(super) fn set_variant(&mut self, variant: Option<String>) {
        self.variant = variant;
    }

    /// The context tokens consumed this turn — a test read of the footer fact
    /// [`Self::set_context_tokens`] sets.
    #[cfg(test)]
    pub(super) fn context_tokens(&self) -> i64 {
        self.context_tokens
    }

    /// Record the context tokens a completed assistant message consumed
    /// (ADR-0044). The caller guards against an all-zero in-flight usage before
    /// calling.
    pub(super) fn set_context_tokens(&mut self, tokens: i64) {
        self.context_tokens = tokens;
    }

    /// The monotonic progress mark (#457). Private to this module: advanced by
    /// [`Self::bump_progress_mark`].
    pub(super) fn progress_mark(&self) -> u64 {
        self.progress_mark
    }

    /// Record observable progress: the external renderer renews its idle bound
    /// when the mark advances (#457).
    pub(super) fn bump_progress_mark(&mut self) {
        self.progress_mark += 1;
    }

    /// Whether this card continues its Turn after a Wake (ADR-0059) — an Error
    /// ending on it never offers Retry. Private to this module: set by
    /// [`Self::mark_wake_continuation`].
    pub(super) fn wake_continuation(&self) -> bool {
        self.wake_continuation
    }

    /// Mark this card as a Wake continuation: a 承接 line's fresh card
    /// (`arm_projected_card` / `arm_wake_continuation`), or a card resuming
    /// after its predecessor yielded ([`Self::continue_on_new_card`]).
    pub(super) fn mark_wake_continuation(&mut self) {
        self.wake_continuation = true;
    }

    /// Whether this chain has already HANDED a Wake's resumed work over to a
    /// card (ADR-0066). Private to this module: written by
    /// [`Self::hand_over_wake`].
    pub(super) fn has_handed_over_wake(&self, wake_id: &str) -> bool {
        self.handed_over_wakes.contains(wake_id)
    }

    /// Whether this chain's one neutral residual continuation has already fired
    /// (spec #602, ticket #606). Private to this module: set by
    /// [`Self::mark_residual_card_posted`].
    pub(super) fn residual_card_posted(&self) -> bool {
        self.residual_card_posted
    }

    /// Mark the chain's one neutral residual continuation as posted.
    pub(super) fn mark_residual_card_posted(&mut self) {
        self.residual_card_posted = true;
    }

    /// The chain's durable Wake Watermark floor, if any (spec #561): every Wake
    /// at or below this `created_ms` was announced by an earlier card. Private
    /// to this module: seeded by [`Self::set_wake_floor`].
    pub(super) fn wake_floor(&self) -> Option<i64> {
        self.wake_floor
    }

    /// Seed the Wake floor from the chain's durable Watermark (`None` when the
    /// session announced no Wake).
    pub(super) fn set_wake_floor(&mut self, floor: Option<i64>) {
        self.wake_floor = floor;
    }

    /// Whether the neutral waiting line 「⏳ 等待当前运行接收…」 was already shown
    /// (ADR-0062). Private to this module: set by
    /// [`Self::mark_receive_hint_shown`].
    pub(super) fn receive_hint_shown(&self) -> bool {
        self.receive_hint_shown
    }

    /// Mark the neutral waiting line as shown (pushed exactly once).
    pub(super) fn mark_receive_hint_shown(&mut self) {
        self.receive_hint_shown = true;
    }

    /// The projection's render seed, if this accumulator carries one (spec #561,
    /// ticket #563). Private to this module: seeded by [`Self::seed_projection`].
    pub(super) fn seed(&self) -> Option<&CursorSeed> {
        self.seed.as_ref()
    }

    /// Record how much of the seed's frontier part this render delivered
    /// (spec #561, review #569): the seed's frontier part then renders only its
    /// undelivered suffix on later reads. A no-op when the accumulator carries
    /// no seed, or the seed no frontier.
    pub(super) fn record_seed_frontier_delivered(&mut self, delivered_chars: usize) {
        if let Some(seed) = self.seed.as_mut()
            && let Some(frontier) = seed.frontier.as_mut()
        {
            frontier.delivered_chars = delivered_chars;
        }
    }

    /// Record a message whose parts the pending gap's own walk rendered
    /// (spec #561, review #569) — the chain cursor is never built from them.
    pub(super) fn mark_gap_message(&mut self, message_id: MessageId) {
        self.gap_messages.insert(message_id);
    }

    /// Whether this accumulator's timeline holds the pending gap's content
    /// (spec #561, review #569). Private to this module: set by
    /// [`Self::mark_gap_rendered`].
    pub(super) fn gap_rendered(&self) -> bool {
        self.gap_rendered
    }

    /// Mark the pending gap's content as rendered into this timeline.
    pub(super) fn mark_gap_rendered(&mut self) {
        self.gap_rendered = true;
    }

    /// Record whether the gap's last render came from a truncated read
    /// (spec #561, review #569).
    pub(super) fn set_gap_truncated(&mut self, truncated: bool) {
        self.gap_truncated = truncated;
    }

    /// The card's streamed assistant text. Private to this module: written by
    /// [`Self::push_text_lead`].
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// The card's streamed reasoning text. Private to this module: written by
    /// [`Self::push_reasoning_lead`].
    pub(super) fn reasoning(&self) -> &str {
        &self.reasoning
    }

    /// The card-content fallback state (spec #379). Private to this module:
    /// advanced only by [`Self::advance_card_fallback`].
    pub(super) fn card_fallback(&self) -> CardFallback {
        self.card_fallback
    }

    /// Advance the fallback after Feishu refused a body, returning whether the
    /// retry should be attempted: a card not yet fenced goes `None → Fenced`
    /// (`true`, retry the same body fenced); an already-fenced or suspended card
    /// goes to `Suspended` (`false`, stop).
    pub(super) fn advance_card_fallback(&mut self) -> bool {
        match self.card_fallback {
            CardFallback::None => {
                self.card_fallback = CardFallback::Fenced;
                true
            }
            CardFallback::Fenced | CardFallback::Suspended => {
                self.card_fallback = CardFallback::Suspended;
                false
            }
        }
    }

    /// The fate of the write carrying this flush's terminal slice (spec #602).
    /// Private to this module: set by [`Self::set_ending_write`].
    pub(super) fn ending_write(&self) -> EndingWrite {
        self.ending_write
    }

    /// Record the terminal slice's write fate.
    pub(super) fn set_ending_write(&mut self, ending: EndingWrite) {
        self.ending_write = ending;
    }

    /// A waiting card's collect (ADR-0059, spec #405): the card yielded
    /// 「⏳ 等待后台任务」 and then its wait was taken over — a new Turn in the
    /// thread superseded it, or the Session stopped being the thread's Active
    /// Session. Only a card still in `Waiting` is collected: a live card is
    /// somebody else's to finish and an ended card keeps the ending it
    /// recorded. The live list leaves with the collect (ADR-0060): the card
    /// stops updating, so it cannot keep a list that claims to be live — a
    /// superseding Turn's own card carries the remaining tasks, a switched-away
    /// chain's next Wake handover owns the section. Returns whether this call
    /// collected the card, so the caller flushes only a real transition.
    pub(super) fn collect_waiting(&mut self, collected: CardState) -> bool {
        if self.card_state != CardState::Waiting {
            return false;
        }
        self.card_state = collected;
        self.ledger.clear();
        // The refresh clock goes with the list: a card that no longer shows a
        // section must not owe a flush for having one.
        self.ledger_clock.clear();
        true
    }

    /// Start a Wake continuation on a NEW card from this accumulator
    /// (ADR-0059): the previous card's slice was already finalized with the
    /// facts it had, so the chain's newest card begins a fresh live phase —
    /// Loading, marked [`Self::wake_continuation`] (it carries no question to
    /// re-ask, so it never re-shows the ended attempt's failure nor offers
    /// Retry). The rendered content, the timeline boundary the handoff
    /// advanced and the interaction blocks stay: the continuation renders only
    /// what arrives after the split and keeps the live controls (ADR-0038).
    pub(super) fn continue_on_new_card(&mut self) {
        self.error = None;
        self.prompt = None;
        self.recovery_claimed = false;
        self.mark_wake_continuation();
        self.card_fallback = CardFallback::None;
        self.card_state = CardState::Loading;
        // The previous phase ended with the card; `refresh_phase` only resets
        // on a change, so clear first for a fresh timer.
        self.current_phase = None;
        self.refresh_phase();
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
        self.push_receipt_at(None, text);
    }

    /// [`Self::push_receipt`] with an explicit timeline key: `at_ms` places the
    /// receipt at a SERVER time instead of cola's "now". The Wake
    /// continuation's 承接 line needs this — the work it announces carries
    /// server times from before cola's poll, so a "now" key would sort the
    /// receipt after that work (ADR-0059; the live order bug). The key only
    /// orders: the receipt itself shows no clock (`shown_at` stays `None`,
    /// like every other receipt).
    pub(super) fn push_receipt_at(&mut self, at_ms: Option<i64>, text: &str) {
        let key = at_ms.unwrap_or_else(|| self.next_order());
        self.insert_kind(key, None, TimelineKind::Receipt(text.to_string()));
    }

    /// Mark `wake_id`'s resumed work as handed to a card: the handoff a 承接
    /// line's split or the in-place resume performs (ADR-0059, ADR-0066).
    /// Idempotent, and independent of [`Self::announce_wake`] — the in-place
    /// resume marks a Wake whose entry a yielded ledger refresh may already
    /// have placed, so the mark must not disturb the announcement set (the
    /// entry stays single).
    pub(super) fn hand_over_wake(&mut self, wake_id: &str) {
        self.handed_over_wakes.insert(wake_id.to_string());
    }

    /// Whether `wake_id`'s completion was already announced on this chain (spec
    /// #588, #593): the read-only half of [`Self::announce_wake`], so a render
    /// can plan its completion entries — and skip their output reads for the
    /// ones already placed — without mutating the accumulator.
    pub(super) fn wake_announced(&self, wake_id: &str) -> bool {
        self.announced_wakes.contains(wake_id)
    }

    /// Mark `wake_id`'s completion as announced on this chain — by the opening
    /// 承接 line or by the merged-path completion entry. Returns false when it
    /// already was: the exactly-once gate both paths honour (ADR-0059). A new
    /// WAKE mark also stages the durable Wake Watermark (ADR-0061), which only
    /// a delivering card write drains; the stage's generation is what a drain
    /// must match (spec #561, review #569).
    pub(super) fn announce_wake(&mut self, wake_id: &str, created_ms: i64) -> bool {
        if !self.announce_entry(wake_id) {
            return false;
        }
        if self
            .pending_watermarks
            .last()
            .is_none_or(|staged| created_ms > staged.created_ms)
        {
            self.watermark_stage_seq = self.watermark_stage_seq.wrapping_add(1);
            self.pending_watermarks.push(StagedWatermark {
                wake_id: wake_id.to_string(),
                created_ms,
                id: self.watermark_stage_seq,
            });
            if self.pending_watermarks.len() > MAX_STAGED_MARKS {
                let excess = self.pending_watermarks.len() - MAX_STAGED_MARKS;
                self.pending_watermarks.drain(..excess);
            }
        }
        true
    }

    /// Mark a SYNTHETIC completion — a runtime verdict's retirement, a child's
    /// own terminal transcript, or the user's cleanup, keyed
    /// `runtime:<call_id>` — as announced on this chain (spec #588, review PR
    /// #595): the same exactly-once gate [`Self::announce_wake`] applies, but
    /// NOTHING durable is staged. The durable Wake Watermark is the newest
    /// WAKE whose completion a card announced (ADR-0061); staging a synthetic
    /// entry's clock there would make a restart read a genuinely un-announced
    /// late Wake at or below it as covered and suppress its continuation —
    /// #590's "a late Wake for a cleared task behaves as before; nothing is
    /// suppressed".
    pub(super) fn announce_synthetic(&mut self, key: &str) -> bool {
        self.announce_entry(key)
    }

    /// The one exactly-once announcement gate every class shares (ADR-0059):
    /// insert the key and report whether it was new.
    fn announce_entry(&mut self, key: &str) -> bool {
        self.announced_wakes.insert(key.to_string())
    }

    /// Stage the Rendered Cursor of the body about to be written (spec #561):
    /// `card_message_id` is `None` for a create, whose id is only known once
    /// the send lands. Every stage is retained until its own write's outcome
    /// resolves it — a newer body staged since never loses an older stage's
    /// delivered confirmation (review #569). Returns the stage generation a
    /// confirmation must name (spec #561, review #569).
    pub(super) fn stage_cursor(
        &mut self,
        card_message_id: Option<&str>,
        cursor: RenderedCursor,
        gap: Option<GapCoverage>,
    ) -> u64 {
        self.cursor_stage_seq = self.cursor_stage_seq.wrapping_add(1);
        self.staged_cursors.push(StagedCursor {
            id: self.cursor_stage_seq,
            card_message_id: card_message_id.map(str::to_string),
            cursor,
            awaiting_seq: None,
            gap,
        });
        if self.staged_cursors.len() > MAX_STAGED_CURSORS {
            let excess = self.staged_cursors.len() - MAX_STAGED_CURSORS;
            self.staged_cursors.drain(..excess);
        }
        self.cursor_stage_seq
    }

    /// Every staged cursor whose owed payload a drain may need to confirm
    /// (spec #561, review #569), oldest first: its card, the sequence whose
    /// delivery verifies it, and its exact identity. The reconcile walks these
    /// and advances exactly what delivered — a stage the pending slot
    /// replaced is still here.
    pub(super) fn staged_cursor_due(&self) -> Vec<(String, u64, StagedCursorId)> {
        self.staged_cursors
            .iter()
            .filter_map(|staged| {
                Some((
                    staged.card_message_id.clone()?,
                    staged.awaiting_seq?,
                    staged.identity(),
                ))
            })
            .collect()
    }

    /// Take the staged Rendered Cursor a just-confirmed write carried (spec
    /// #561, review #569): mirror it into the confirmed base and hand it back
    /// for the Chain Record. Only the EXACT stage matches — a body staged
    /// since is left for its own confirmation — and the stage's card scope
    /// must accept `card_message_id` (a create's stage carries none). A stage
    /// OLDER than the one the base already reflects is dropped without
    /// applying: the durable cursor only ever moves forward. The projection's
    /// atomic takeover calls this inside its cards-map critical section, so the
    /// record can carry the successor's frontier from the first moment it
    /// names the successor card.
    pub(super) fn take_staged_cursor(
        &mut self,
        card_message_id: &str,
        expected: StagedCursorId,
    ) -> Option<(RenderedCursor, Option<GapCoverage>)> {
        let idx = self.staged_cursors.iter().position(|staged| {
            staged.id == expected.id
                && staged.awaiting_seq == expected.awaiting_seq
                && staged
                    .card_message_id
                    .as_deref()
                    .is_none_or(|id| id == card_message_id)
        })?;
        let staged = self.staged_cursors.remove(idx);
        if staged.id < self.confirmed_cursor_stage {
            return None;
        }
        self.confirmed_cursor_stage = staged.id;
        self.cursor = staged.cursor.clone();
        Some((staged.cursor, staged.gap))
    }

    /// Seed a fresh accumulator's empty base with a Rendered Cursor a re-point
    /// carried (spec #561, review #569): the chain's frontier must survive a new
    /// Turn or a takeover, while a continuation whose base already carries a
    /// value (its stage's take advanced it) keeps its own. Never moves the base
    /// backwards — an empty base only.
    pub(super) fn seed_cursor_if_empty(&mut self, cursor: RenderedCursor) {
        if self.cursor == RenderedCursor::default() {
            self.cursor = cursor;
        }
    }

    /// The orphan gap this accumulator owes, if any (spec #561, review #569):
    /// the durable gap, rendered once by the first render read that can place
    /// it.
    pub(super) fn pending_gap(&self) -> Option<&crate::bridge::chain::PendingGap> {
        self.pending_gap.as_ref()
    }

    /// Seed the durable orphan gap this accumulator owes (spec #561, review
    /// #569): set on a successor's accumulator so the first render read that
    /// can place it lands it there.
    pub(super) fn set_pending_gap(&mut self, gap: crate::bridge::chain::PendingGap) {
        self.pending_gap = Some(gap);
    }

    /// The accumulator's confirmed Rendered Cursor (spec #561): a test read of
    /// the frontier only a confirmed write moves.
    #[cfg(test)]
    pub(super) fn cursor(&self) -> &RenderedCursor {
        &self.cursor
    }

    /// Set the confirmed Rendered Cursor directly (spec #561): a test seam
    /// standing in for the confirmed write that mirrors a frontier into the
    /// base.
    #[cfg(test)]
    pub(super) fn set_cursor(&mut self, cursor: RenderedCursor) {
        self.cursor = cursor;
    }

    /// The newest staged Rendered Cursor, if any (spec #561, review #569): the
    /// stage a confirmation names — a test read for the confirmation seams.
    #[cfg(test)]
    pub(super) fn last_staged_cursor(&self) -> Option<&StagedCursor> {
        self.staged_cursors.last()
    }

    /// The stage generation of the NEWEST staged Wake Watermark, when one is
    /// staged: what the next write carries, and what its own drain names
    /// (spec #561, review #569).
    pub(super) fn pending_watermark_id(&self) -> Option<u64> {
        self.pending_watermarks.last().map(|staged| staged.id)
    }

    /// Take the exact staged Wake Watermark a delivered write carried (spec
    /// #561, review #569) and hand it back to persist. An older delivered mark
    /// is still drainable after a newer one is staged; a mark staged since is
    /// left for its own drain; the durable mark itself only moves forward
    /// (`ChainRecords::advance`).
    pub(super) fn take_staged_watermark(&mut self, expected: u64) -> Option<StagedWatermark> {
        let idx = self
            .pending_watermarks
            .iter()
            .position(|staged| staged.id == expected)?;
        Some(self.pending_watermarks.remove(idx))
    }

    /// Tie the EXACT staged cursor to the Pending Card Update sequence a
    /// recoverably failed write left owed (spec #561, review #569): the drain
    /// reconcile advances the cursor only once THAT payload delivers. A stage
    /// that names another card is left untouched.
    pub(super) fn await_cursor_write(&mut self, expected: StagedCursorId, card_message_id: &str, seq: u64) {
        if let Some(staged) = self
            .staged_cursors
            .iter_mut()
            .find(|staged| staged.id == expected.id)
            && staged
                .card_message_id
                .as_deref()
                .is_none_or(|id| id == card_message_id)
        {
            staged.awaiting_seq = Some(seq);
        }
    }

    /// Drop the EXACT staged cursor a failed write carried (spec #561, review
    /// #569): the write it described can never land (a permanent refusal), so
    /// nothing may advance it — and a stale discard names a stage already
    /// resolved, which is a no-op. The next build re-stages from the confirmed
    /// base.
    pub(super) fn discard_staged_cursor(&mut self, expected: StagedCursorId) {
        self.staged_cursors.retain(|staged| staged.id != expected.id);
    }

    /// The completed Background Task ledger entry a shell/subagent Wake leaves
    /// on the card that hosted its task (ADR-0060): one folded panel, keyed at
    /// the Wake's own server time so it sorts where the completion happened —
    /// before the resumed work it announces, and never on another card.
    pub(super) fn push_ledger_entry_at(&mut self, at_ms: Option<i64>, entry: TaskCompletionEntry) {
        let key = at_ms.unwrap_or_else(|| self.next_order());
        self.insert_kind(key, None, TimelineKind::LedgerEntry(entry));
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
        self.insert_kind_src(key, shown_at, None, kind);
    }

    /// [`Self::insert_kind`] carrying the typed transcript part a
    /// text/reasoning entry rendered from (spec #561's cursor frontier).
    fn insert_kind_src(
        &mut self,
        key: i64,
        shown_at: Option<i64>,
        source: Option<PartSource>,
        kind: TimelineKind,
    ) {
        self.insert_kind_lead(key, shown_at, source, kind, None);
    }

    /// [`Self::insert_kind_src`] with a markdown lead rendered before the
    /// entry's content (spec #561's cut tail): the seeded render's fence
    /// opener / table header.
    fn insert_kind_lead(
        &mut self,
        key: i64,
        shown_at: Option<i64>,
        source: Option<PartSource>,
        kind: TimelineKind,
        lead: Option<String>,
    ) -> usize {
        self.item_seq += 1;
        self.insert_item(key, shown_at, source, self.item_seq, kind, lead)
    }

    /// [`Self::insert_kind`] with a pre-allocated identity: a live Tool Panel
    /// keeps the seq it was born with when it settles into the timeline, so
    /// its card element — and the reader's fold state — survives the move
    /// (ADR-0045).
    fn insert_item(
        &mut self,
        key: i64,
        shown_at: Option<i64>,
        source: Option<PartSource>,
        seq: u64,
        kind: TimelineKind,
        lead: Option<String>,
    ) -> usize {
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
                source,
                lead,
            },
        );
        self.last_key = self.last_key.max(key);
        idx
    }

    /// The timeline index of the LAST entry keyed `key` that renders `kind`
    /// and belongs to the SAME transcript part as `source` (spec #561, review
    /// #569): the entry a push may merge into. Entries from different parts
    /// never merge — two parts of one message may share a server time — so the
    /// search walks this key's own run for the part's entry instead of taking
    /// the run's last one. A `None` on either side demands an exact `None`
    /// match, so a synthetic push never absorbs a part's entry.
    fn source_entry(
        &self,
        key: i64,
        source: Option<&PartSource>,
        accepts: impl Fn(&TimelineKind) -> bool,
    ) -> Option<usize> {
        let end = self.timeline.partition_point(|item| item.key <= key);
        self.timeline[..end]
            .iter()
            .enumerate()
            .rev()
            .take_while(|(_, item)| item.key == key)
            .find_map(|(idx, item)| {
                if !accepts(&item.kind) {
                    return None;
                }
                let same = match (item.source.as_ref(), source) {
                    (None, None) => true,
                    (Some(item_source), Some(source)) => item_source.same_part(source),
                    _ => false,
                };
                same.then_some(idx)
            })
    }

    /// Insert `kind` directly AFTER timeline index `after` (spec #561, review
    /// #569): a chunk continuing a part whose equal-key run another part's
    /// entry follows must stay beside its own entries, never jump past them.
    /// Mirrors [`Self::insert_item`]'s render-boundary clamp; returns the
    /// inserted index.
    fn insert_kind_after(
        &mut self,
        after: usize,
        key: i64,
        shown_at: Option<i64>,
        source: Option<PartSource>,
        kind: TimelineKind,
        lead: Option<String>,
    ) -> usize {
        self.item_seq += 1;
        let seq = self.item_seq;
        let (idx, key) = if after + 1 < self.render_from {
            (
                self.render_from,
                self.timeline.get(self.render_from).map_or(key, |item| item.key),
            )
        } else {
            (after + 1, key)
        };
        self.timeline.insert(
            idx,
            TimelineItem {
                key,
                shown_at,
                seq,
                kind,
                source,
                lead,
            },
        );
        self.last_key = self.last_key.max(key);
        idx
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
        self.push_text_from(at_ms, None, chunk);
    }

    /// [`Self::push_text_at`] carrying the typed transcript part this chunk
    /// rendered from (spec #561's cursor frontier). Synthetic pushes pass
    /// `None`.
    pub(super) fn push_text_from(&mut self, at_ms: Option<i64>, source: Option<PartSource>, chunk: &str) {
        self.push_text_lead(at_ms, source, chunk, None);
    }

    /// [`Self::push_text_from`] with a markdown lead emitted before the
    /// chunk's content (spec #561's cut tail): the lead renders (closing the
    /// construct the delivered prefix left open) while the entry's counted
    /// characters stay the model's suffix, so the Rendered Cursor's arithmetic
    /// is exact.
    pub(super) fn push_text_lead(
        &mut self,
        at_ms: Option<i64>,
        source: Option<PartSource>,
        chunk: &str,
        lead: Option<String>,
    ) {
        let key = at_ms.unwrap_or_else(|| self.next_order());
        self.text.push_str(chunk);
        let max = crate::feishu::card::MAX_CARD_TEXT_CHARS;
        let mut remaining = chunk;
        let mut lead = lead;
        // The entry THIS part merges into (spec #561, review #569): another
        // part's entry at the same server time is never a merge target, so
        // each part keeps its own source and extent. `None` until this part
        // has an entry under this key.
        let mut target =
            self.source_entry(key, source.as_ref(), |kind| matches!(kind, TimelineKind::Text(_)));
        while !remaining.is_empty() {
            let space = match target.and_then(|idx| self.timeline.get(idx)) {
                Some(TimelineItem {
                    kind: TimelineKind::Text(last),
                    ..
                }) => max.saturating_sub(last.chars().count()),
                _ => 0,
            };
            if space == 0 {
                let take: String = remaining.chars().take(max).collect();
                let pushed = source.clone();
                target = Some(match target {
                    // Beside its own run: another part's equal-key entry may
                    // already follow it.
                    Some(idx) => self.insert_kind_after(
                        idx,
                        key,
                        at_ms,
                        pushed,
                        TimelineKind::Text(take.clone()),
                        lead.take(),
                    ),
                    None => self.insert_kind_lead(
                        key,
                        at_ms,
                        pushed,
                        TimelineKind::Text(take.clone()),
                        lead.take(),
                    ),
                });
                remaining = &remaining[take.len()..];
                continue;
            }
            let take: String = remaining.chars().take(space).collect();
            let idx = target.expect("space came from it");
            match self.timeline.get_mut(idx) {
                Some(TimelineItem {
                    kind: TimelineKind::Text(last),
                    source: item_source,
                    lead: item_lead,
                    ..
                }) => {
                    last.push_str(&take);
                    if item_source.is_none() {
                        *item_source = source.clone();
                    }
                    // A later push of the same part refreshes the entry's
                    // prefix digest: it always covers the whole delivered
                    // prefix at each push (spec #561, review #569).
                    if let (Some(item_source), Some(pushed)) = (item_source.as_mut(), source.as_ref())
                        && item_source.same_part(pushed)
                    {
                        item_source.prefix_digest = pushed.prefix_digest;
                    }
                    if item_lead.is_none() {
                        *item_lead = lead.take();
                    }
                }
                _ => {
                    target = Some(self.insert_kind_after(
                        idx,
                        key,
                        at_ms,
                        source.clone(),
                        TimelineKind::Text(take.clone()),
                        lead.take(),
                    ));
                }
            }
            remaining = &remaining[take.len()..];
        }
    }

    /// Append a reasoning chunk, keeping it in the chronological timeline
    /// (chunks of the same part merge into one panel; a part keyed apart is
    /// its own panel). Production renders go through [`Self::push_reasoning_from`];
    /// this convenience form is for tests and synthetic content.
    #[cfg(test)]
    pub(super) fn push_reasoning(&mut self, chunk: &str) {
        self.push_reasoning_at(None, chunk);
    }

    /// [`Self::push_reasoning`] for a reasoning part that started at `at_ms`
    /// (the server's `time.start`); `None` keys it by fallback and shows no
    /// clock. Test-only like [`Self::push_reasoning`]: production renders go
    /// through [`Self::push_reasoning_from`].
    #[cfg(test)]
    pub(super) fn push_reasoning_at(&mut self, at_ms: Option<i64>, chunk: &str) {
        self.push_reasoning_from(at_ms, None, chunk);
    }

    /// [`Self::push_reasoning_at`] carrying the typed transcript part this
    /// chunk rendered from (spec #561's cursor frontier). Synthetic pushes
    /// pass `None`.
    pub(super) fn push_reasoning_from(
        &mut self,
        at_ms: Option<i64>,
        source: Option<PartSource>,
        chunk: &str,
    ) {
        self.push_reasoning_lead(at_ms, source, chunk, None);
    }

    /// [`Self::push_reasoning_from`] with a markdown lead emitted before the
    /// chunk's content (spec #561's cut tail), exactly like
    /// [`Self::push_text_lead`].
    pub(super) fn push_reasoning_lead(
        &mut self,
        at_ms: Option<i64>,
        source: Option<PartSource>,
        chunk: &str,
        lead: Option<String>,
    ) {
        let key = at_ms.unwrap_or_else(|| self.next_order());
        self.reasoning.push_str(chunk);
        // The entry THIS part merges into, never another part's at the same
        // server time (spec #561, review #569).
        let idx = self.source_entry(key, source.as_ref(), |kind| {
            matches!(kind, TimelineKind::Reasoning(_))
        });
        match idx.and_then(|i| self.timeline.get_mut(i)) {
            Some(TimelineItem {
                kind: TimelineKind::Reasoning(last),
                source: item_source,
                lead: item_lead,
                ..
            }) => {
                last.push_str(chunk);
                if item_source.is_none() {
                    *item_source = source.clone();
                }
                // A later push of the same part refreshes the entry's prefix
                // digest (spec #561, review #569).
                if let (Some(item_source), Some(pushed)) = (item_source.as_mut(), source.as_ref())
                    && item_source.same_part(pushed)
                {
                    item_source.prefix_digest = pushed.prefix_digest;
                }
                if item_lead.is_none() {
                    *item_lead = lead;
                }
            }
            _ => {
                self.insert_kind_lead(
                    key,
                    at_ms,
                    source,
                    TimelineKind::Reasoning(chunk.to_string()),
                    lead,
                );
            }
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
    #[cfg(test)]
    pub(super) fn push_tool_at(&mut self, at_ms: Option<i64>, call_id: &str, panel: ToolPanel) {
        self.push_tool_from(at_ms, call_id, panel, None);
    }

    /// [`Self::push_tool_at`] carrying the typed transcript part the call
    /// rendered from (spec #561, review #569): a settled panel's timeline entry
    /// keeps it, so it can become the Rendered Cursor frontier.
    pub(super) fn push_tool_from(
        &mut self,
        at_ms: Option<i64>,
        call_id: &str,
        panel: ToolPanel,
        source: Option<PartSource>,
    ) {
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
                        source,
                    },
                );
            } else {
                self.insert_kind_src(key, at_ms, source, TimelineKind::Tool(call_id.to_string()));
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
            if let Some(source) = source
                && let Some(entry) = self.live_tools.get_mut(call_id)
                && entry.source.is_none()
            {
                entry.source = Some(source);
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
                    entry.source,
                    entry.seq,
                    TimelineKind::Tool(call_id.to_string()),
                    None,
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

    /// Record the latest `todowrite` panel and its write clock — the single
    /// production write of the card-tail list: the renderer's `render_part`
    /// and the seed's [`Self::mark_delivered_part`] both go through it. Both
    /// move together: the clock is always reassigned, so a payload without a
    /// server time shows no clock rather than the previous call's (which would
    /// read as a write time this list never had).
    pub(super) fn set_todo_panel(&mut self, panel: ToolPanel, shown_at: Option<i64>) {
        self.todo_panel = Some(panel);
        self.todo_shown_at = shown_at;
    }

    /// The `todowrite` tail panel, if any — a test read of the card-tail list.
    #[cfg(test)]
    pub(super) fn todo_panel(&self) -> Option<&ToolPanel> {
        self.todo_panel.as_ref()
    }

    /// Seed a render from the chain's Rendered Cursor (spec #561, tickets
    /// #563/#564/#565): the resolved seed makes everything at or before its
    /// frontier count as delivered, its live set enters the identity
    /// reconciliation — which renders a settled call once into the timeline and
    /// keeps a still-running one display-only — and its scope (when the seed's
    /// Turn is not this accumulator's: the message-first race) marks the window
    /// the render walks past its own. The base cursor is the confirmed fact
    /// every later candidate derives from.
    pub(super) fn seed_projection(&mut self, cursor: &RenderedCursor, seed: CursorSeed) {
        self.cursor = cursor.clone();
        self.seed = Some(seed.clone());
        self.seeded_calls = seed.live_calls.into_iter().collect();
    }

    /// Mark one transcript part as delivered without rendering it (spec #561's
    /// projection seed): the seed's own walk and the Wake step's content-diff
    /// probe then see it exactly as delivered — nothing at or before the
    /// frontier renders again, and no Wake continuation replays it. The
    /// text/reasoning mark is scoped to the part's OWN message (review #569):
    /// the same text in the NEW Turn's window is the new turn's content and
    /// must still render. Tool panels are keyed by their call, which is unique
    /// per call, so their current-panel dedup cannot cross turns.
    pub(super) fn mark_delivered_part(&mut self, message_id: &crate::backend::MessageId, part: &Part) {
        match part {
            Part::Text(text) => {
                self.seeded_delivered
                    .insert((message_id.clone(), RenderedPart::Text(text.text.clone())));
            }
            Part::Reasoning(reasoning) => {
                self.seeded_delivered.insert((
                    message_id.clone(),
                    RenderedPart::Reasoning(reasoning.text.clone()),
                ));
            }
            Part::Tool(call) => {
                if call.identity.name == "todowrite" {
                    self.set_todo_panel(ToolPanel::new(call.clone()), call.started_at);
                } else {
                    self.tools
                        .insert(call.identity.call_id.clone(), ToolPanel::new(call.clone()));
                }
            }
            Part::StepStart(_) | Part::StepFinish(_) | Part::Patch(_) | Part::Other(_) => {}
        }
    }

    /// Whether the seed already delivered `part` in `message_id` (spec #561,
    /// review #569): what the Wake step's content diff reads, message-scoped so
    /// an identical part the seed never saw still counts as renderable.
    pub(super) fn part_seeded_delivered(&self, message_id: &crate::backend::MessageId, part: &Part) -> bool {
        match part {
            Part::Text(text) => self
                .seeded_delivered
                .contains(&(message_id.clone(), RenderedPart::Text(text.text.clone()))),
            Part::Reasoning(reasoning) => self.seeded_delivered.contains(&(
                message_id.clone(),
                RenderedPart::Reasoning(reasoning.text.clone()),
            )),
            // Tool panels dedupe by their rendered revision, never by content.
            _ => false,
        }
    }

    /// The live `task`/`subagent` panels' (call id, child Session id) pairs
    /// (ADR-0054): the render path reads each child's liveness for them. A
    /// child-spawning call with no recorded child id contributes nothing.
    pub(super) fn live_task_children(&self) -> Vec<(String, String)> {
        self.tools
            .iter()
            .filter(|(_, panel)| panel.is_live() && is_task_tool(panel.name()))
            .filter_map(|(call_id, panel)| {
                panel
                    .child_session_id()
                    .map(|child| (call_id.clone(), child.to_string()))
            })
            .collect()
    }

    /// Attach or clear one panel's gathered child liveness (ADR-0054). Returns
    /// true when the panel changed, so the caller can flush.
    pub(super) fn set_tool_liveness(&mut self, call_id: &str, liveness: Option<TaskLiveness>) -> bool {
        let Some(panel) = self.tools.get_mut(call_id) else {
            return false;
        };
        if panel.liveness() == liveness.as_ref() {
            return false;
        }
        panel.set_liveness(liveness);
        true
    }

    /// Whether the ledger still carries a row whose status is unconfirmed
    /// (⚠️ 状态待确认) — the render's cleanup-button predicate and the click's
    /// own claim guard read the same fact, so a card cannot offer a clearance
    /// its pipeline would refuse.
    pub(super) fn ledger_has_unconfirmed(&self) -> bool {
        self.ledger.iter().any(|row| row.unconfirmed)
    }

    /// Replace the live Background Task ledger from a transcript read
    /// (ADR-0060) — the ONE site a read's rows and clock enter the accumulator,
    /// shared by the live render and Session Sync's Wake handover / yielded
    /// refresh, so those paths cannot drift. `activities` is this read's
    /// gathered child liveness, keyed by call id (spec #501): the ledger's
    /// `subagent` rows carry it, and a call the gather could not establish
    /// keeps its previous fragment ([`Self::ledger_rows`]). `cadence` is the
    /// path's own comparison granularity ([`LedgerCadence`]). Returns what the
    /// read moved, split so the caller can tell a flush from progress
    /// ([`LedgerChange`]).
    pub(super) fn set_ledger_from_read(
        &mut self,
        transcript: &SessionTranscript,
        activities: &HashMap<String, TaskLiveness>,
        now_ms: i64,
        cadence: LedgerCadence,
    ) -> LedgerChange {
        let rows = self.ledger_rows(transcript, activities);
        self.set_ledger_change(rows, now_ms, cadence)
    }

    /// Replace the live Background Task ledger from an already-derived read
    /// (ADR-0060), stamped with the read's clock. The read is the authority: a
    /// task it no longer lists has retired and leaves the section, a new one
    /// joins in transcript order. Returns whether the card owes a flush: a
    /// RENDERED change — membership, a row's visible facts, or a rendered
    /// number (a shell row's elapsed, or a fragment's age, spec #501) moving at
    /// `cadence`. The comparison reads what the row renders, never the
    /// gathered liveness's hidden timestamps: a child part landing inside the
    /// second the card already shows owes nothing. The clock stores the
    /// rendered seconds — what the card last rendered at the path's cadence —
    /// so the live path compares whole minutes of it (the seconds inside a
    /// rendered minute never owe one and the card gains no per-render clock
    /// churn) while the yielded path compares whole seconds (its 8 s reads keep
    /// the visible numbers true). The read's rows and clock
    /// are stored whatever the decision, so the next age is measured from the
    /// freshest gathered timestamps rather than from a stale accepted read.
    /// Test-only convenience over [`Self::set_ledger_change`]: the flush
    /// decision alone, for the many cases that only pin what owes a PATCH. The
    /// live render reads the split (`rows` vs `clock`) so its idle bound can
    /// ignore clock churn (#457).
    #[cfg(test)]
    fn set_ledger(&mut self, rows: Vec<TaskLedgerRow>, now_ms: i64, cadence: LedgerCadence) -> bool {
        self.set_ledger_change(rows, now_ms, cadence).owes()
    }

    /// [`Self::set_ledger`]'s body, returning the two halves of the decision
    /// (#457): `rows` is a visible-fact change, `clock` a rendered number
    /// crossing `cadence`. See [`LedgerChange`] for why they are split.
    fn set_ledger_change(
        &mut self,
        rows: Vec<TaskLedgerRow>,
        now_ms: i64,
        cadence: LedgerCadence,
    ) -> LedgerChange {
        let clock = crate::feishu::card::ledger::task_ledger_clock(&rows, now_ms);
        let moved = |rendered: Option<u64>, new: Option<u64>| match cadence {
            LedgerCadence::Minute => rendered.map(|secs| secs / 60) != new.map(|secs| secs / 60),
            LedgerCadence::Second => rendered != new,
        };
        // A length mismatch is already a membership change (`rows_alike`
        // false); when the lengths match, the zips cover every row.
        let rows_alike = self.ledger.len() == rows.len()
            && self
                .ledger
                .iter()
                .zip(&rows)
                .all(|(rendered, new)| rendered.renders_like(new));
        let clock_alike = self.ledger_clock.iter().zip(&clock).all(|(rendered, new)| {
            !moved(rendered.elapsed, new.elapsed)
                && !moved(rendered.activity, new.activity)
                // The output window's cutoff (spec #592): a re-read inside the
                // minute the label shows owes nothing, a crossing minute owes
                // its flush — as a clock change, never the row half, so a
                // silent shell's window cannot renew the idle bound (#457).
                && !moved(rendered.output, new.output)
        });
        self.ledger = rows;
        self.ledger_clock = clock;
        LedgerChange {
            rows: !rows_alike,
            clock: !clock_alike,
        }
    }

    /// The live Background Task ledger a transcript read owes the card
    /// (ADR-0060): one row per live task, in the read's own (transcript) order,
    /// each labelled from the input of the tool part that started it — joined
    /// by the task's `call_id` over the WHOLE read, because a task can outlive
    /// the Turn that started it. The read is the authority: a task a Wake
    /// retired is no longer in `background_tasks`, so its row leaves the
    /// section.
    ///
    /// A `subagent` row carries the child liveness this read gathered
    /// (`activities`, keyed by call id, spec #501); a call the gather could not
    /// establish — a failed child read, or a path that gathers nothing — keeps
    /// the last fragment that DID establish one ([`Self::ledger_activity`]),
    /// whose stored timestamps are never refreshed, so its rendered age keeps
    /// growing truthfully (ADR-0054). A shell row never carries one. The cache
    /// is pruned to the live subagents on every read, so a retired task's
    /// fragment leaves with its row. The one derivation
    /// [`Self::set_ledger_from_read`] feeds, so the live render and Session
    /// Sync's ledger paths cannot disagree.
    fn ledger_rows(
        &mut self,
        transcript: &SessionTranscript,
        activities: &HashMap<String, TaskLiveness>,
    ) -> Vec<TaskLedgerRow> {
        // The common case (V1, or a session with no live task) does no scan: an
        // empty read clears an empty ledger — and every cached fragment and
        // window with it, so a retired task's facts cannot outlive its row.
        if transcript.background_tasks.is_empty() {
            self.ledger_activity.clear();
            self.ledger_outputs.clear();
            return Vec::new();
        }
        let rows: Vec<TaskLedgerRow> = transcript
            .background_tasks
            .iter()
            .map(|task| {
                // The kind is derived ONCE per task: the row's type noun, the
                // label arm and the activity arm below read the same value, so
                // they cannot disagree.
                let kind = task_kind(&task.tool.name);
                let call_id = task.tool.call_id.as_str();
                let activity = (kind == TaskKind::Subagent)
                    .then(|| {
                        activities
                            .get(call_id)
                            .or_else(|| self.ledger_activity.get(call_id))
                            .cloned()
                    })
                    .flatten();
                // The shell's output window (spec #592): the read's own outcome
                // when it spent one, the last established window when the
                // shared cycle's throttle spent nothing. An empty capture is a
                // success the row still omits (spec #588, review) — only the
                // entry's own read tells it apart from 「输出已不可用」.
                let output = if kind == TaskKind::Shell {
                    task.shell_id.as_deref().and_then(|shell_id| {
                        match transcript.shell_outputs.get(shell_id) {
                            // The read spent a tail read and found nothing
                            // renderable: an empty capture or an unavailable
                            // record. The window is omitted, and any stored one
                            // leaves with it — never a stale window posing as
                            // current, never a placeholder.
                            Some(read) => read.window().cloned(),
                            // No tail read this cycle (the shared throttle):
                            // the last established window stands, exactly like
                            // a subagent's stored activity fragment.
                            None => self.ledger_outputs.get(shell_id).cloned(),
                        }
                    })
                } else {
                    None
                };
                TaskLedgerRow {
                    kind,
                    label: task_label(kind, transcript.tool_input(call_id)),
                    started_at: task.started_at,
                    // A task the runtime could not confirm (issue #454): the row
                    // renders its own facts plus the unconfirmed marker, and stays
                    // live until its Wake or a positive terminal verdict retires it.
                    unconfirmed: transcript.unconfirmed_tasks.contains(call_id),
                    activity,
                    output,
                }
            })
            .collect();
        // Keep the cache to the live subagents (a retired task's fragment
        // leaves with its row) and fold in this read's fresh successes — a
        // failed child read contributed no entry, so its call keeps its last
        // successful fragment above.
        let live_subagents: std::collections::HashSet<&str> = transcript
            .background_tasks
            .iter()
            .filter(|task| task_kind(&task.tool.name) == TaskKind::Subagent)
            .map(|task| task.tool.call_id.as_str())
            .collect();
        self.ledger_activity
            .retain(|call_id, _| live_subagents.contains(call_id.as_str()));
        for (call_id, liveness) in activities {
            if live_subagents.contains(call_id.as_str()) {
                self.ledger_activity.insert(call_id.clone(), liveness.clone());
            }
        }
        // The output cache follows the same rule (spec #592): pruned to the
        // live shells, folded with this read's windows, and cleared for a shell
        // the read spent on and found nothing — so a failed read's window
        // leaves rather than posing as current, and the throttled reads
        // between cycles keep rendering the last real one.
        let live_shells: std::collections::HashSet<&str> = transcript
            .background_tasks
            .iter()
            .filter(|task| task_kind(&task.tool.name) == TaskKind::Shell)
            .filter_map(|task| task.shell_id.as_deref())
            .collect();
        self.ledger_outputs
            .retain(|shell_id, _| live_shells.contains(shell_id.as_str()));
        for (shell_id, output) in &transcript.shell_outputs {
            if !live_shells.contains(shell_id.as_str()) {
                continue;
            }
            // Only a readable window with text is established; an empty
            // capture and an unavailable record both clear the stored window,
            // so a failed or empty read's window leaves rather than posing as
            // current, and the throttled reads between cycles keep rendering
            // the last real one.
            match output.window() {
                Some(window) => {
                    self.ledger_outputs.insert(shell_id.clone(), window.clone());
                }
                None => {
                    self.ledger_outputs.remove(shell_id);
                }
            }
        }
        rows
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
        let (card, spans, cursor, gap) = self.build_card_inner(self.render_from, split, !full, state);
        // Advance `render_from` ONLY on an actual split: while the card still
        // fits, subsequent flushes must re-render from the SAME start so the
        // content accumulates instead of only showing the latest delta.
        if full {
            self.render_from = split;
        }
        BuiltCard {
            card,
            full,
            spans,
            cursor,
            gap,
        }
    }

    /// [`Self::build_card_with_info`] for callers that don't need the block
    /// spans (the click ack's split probe, tests).
    pub(super) fn build_card_with_split(&mut self) -> (serde_json::Value, bool) {
        let built = self.build_card_with_info();
        (built.card, built.full)
    }

    /// Build the whole live slice for a card that must NOT split — the yielded
    /// ledger refresh (ADR-0060, ticket #419): the remaining timeline PLUS the
    /// tail, rendered as the tracked card, with `render_from` untouched so the
    /// next flush still renders from the same place and no continuation is
    /// ever owed. Used only where a ledger-only change must not post a new
    /// card; the delta such a change adds (one entry, one row less, a second's
    /// elapsed) sits inside the splitter's own reserve margin, so the card
    /// stays under Feishu's hard cap even when its estimate crosses the split
    /// budget.
    pub(super) fn build_card_unsplit(&self) -> BuiltCard {
        let (card, spans, cursor, gap) =
            self.build_card_inner(self.render_from, self.timeline.len(), true, None);
        BuiltCard {
            card,
            full: false,
            spans,
            cursor,
            gap,
        }
    }

    /// Build the card's LIVE slice (`render_from` to the end) as a finalized
    /// card — no tail — and ADVANCE `render_from` past it. A Supplement forces
    /// this split even though the slice fits (ADR-0043): the finalized card
    /// keeps everything before the split, and the continuation renders only
    /// the delta appended afterwards — the same handoff a size split performs.
    /// Receipts queued after this build land past the new boundary, so they
    /// ride the continuation. `state` overrides the header for the finalized
    /// card (`None` takes the standard 「部分完成，继续中…」); a terminal card
    /// keeps its own recorded ending when the ledger handover still owes it a
    /// PATCH (ADR-0060).
    pub(super) fn build_finalized_handoff(&mut self, state: Option<CardState>) -> BuiltCard {
        let end = self.timeline.len();
        let built = self.build_card_inner(
            self.render_from,
            end,
            false,
            Some(state.unwrap_or(CardState::Continued)),
        );
        self.render_from = end;
        BuiltCard {
            card: built.0,
            full: false,
            spans: built.1,
            cursor: built.2,
            gap: built.3,
        }
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

    /// The timeline index the current card starts rendering from — the
    /// projection arm's remaining-slice probe and the flush's failed-write
    /// restore read it.
    pub(super) fn render_from(&self) -> usize {
        self.render_from
    }

    /// Rewind the render boundary to `from` when the build still left it at
    /// `to` — the failed- and fenced-write restore (spec #561, review #569):
    /// the slice the build already advanced past reached no card, so
    /// re-rendering it from `from` is what keeps it from being silently
    /// skipped. The equality check keeps a boundary moved elsewhere (defensive;
    /// the card-write lock serializes flushes) from being rewound. Returns
    /// whether the boundary moved. Production writers: the flush's finalized
    /// fenced retry and its failed continuation create.
    pub(super) fn restore_render_boundary(&mut self, from: usize, to: usize) -> bool {
        if self.render_from != to {
            return false;
        }
        self.rewind_render_boundary(from);
        true
    }

    /// Rewind the render boundary to `to` unconditionally: the projection arm's
    /// fenced retry re-renders its first body from scratch, so the boundary
    /// goes back to it (spec #561, review #569). Production writer:
    /// `fenced_projected_slice`.
    pub(super) fn rewind_render_boundary(&mut self, to: usize) {
        self.render_from = to;
    }

    /// Whether any timeline entry is an Interaction Receipt whose line starts
    /// with `prefix` (ADR-0038, rule 4) — a test read for the sweep rig's
    /// receipt probe. Test-gated because its only caller is the test seam
    /// [`Turn::has_receipt_prefix`], which itself lives in the
    /// `#[cfg(test)] impl Turn` block in `mod.rs`.
    #[cfg(test)]
    pub(super) fn has_receipt_prefix(&self, prefix: &str) -> bool {
        self.timeline
            .iter()
            .any(|item| matches!(&item.kind, TimelineKind::Receipt(text) if text.starts_with(prefix)))
    }

    /// Whether a still-running seeded call (spec #561's live set) is omitted
    /// from the card being built under `state`: it rides the tail only while a
    /// live renderer owns the card — once the card is settled (or yielded), no
    /// renderer will ever update the panel again, so it must not keep showing
    /// a `⏳` that can never move; it never outlives the Turn. `state` is the
    /// caller's EFFECTIVE state (the builder's `state_override` included), so
    /// the split estimator's tail reserve and the card builder's tail decide
    /// on the same state their card is built with and cannot drift; the
    /// estimator passes `self.card_state`. The reserve therefore never charges
    /// for what the build will not render. A seeded call that settled before
    /// the end is a timeline entry and is not covered by this rule.
    fn omitted_live_seeded(&self, state: &CardState, call_id: &str) -> bool {
        !state.is_render_owned() && self.seeded_calls.contains(call_id)
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
    /// tail — the todo list, the Background Task Ledger (ADR-0060) and every
    /// running tool's panel (ADR-0045) — to the same budget, keeping the card
    /// and its tail together under the cap. Callers pass it only when the
    /// slice is the final, tail-carrying one — see
    /// [`Self::build_card_with_info`].
    fn estimate_split_index(&self, start: usize, reserve_tail: bool) -> usize {
        let mut comps = 0usize;
        let mut size = 0usize;
        let mut card_text = 0usize;
        if reserve_tail {
            // The tail rides only the live card, but it counts against that
            // card's budget: the todo list, the Background Task Ledger, then
            // every running tool's panel (ADR-0045). When they don't fit, the
            // card finalizes without the tail and the continuation carries it.
            // A still-running seeded panel is excluded on a card no live
            // renderer owns, exactly as `build_card_inner` omits it
            // ([`Self::omitted_live_seeded`]): the reserve must not charge for
            // what the build will not render.
            if let Some(panel) = &self.todo_panel {
                comps += 1;
                size += panel_estimate(panel);
            }
            if !self.ledger.is_empty() {
                comps += 1;
                size += crate::feishu::card::ledger::task_ledger_estimate(&self.ledger);
            }
            for call_id in self.live_tools.keys() {
                if self.omitted_live_seeded(&self.card_state, call_id) {
                    continue;
                }
                if let Some(panel) = self.tools.get(call_id) {
                    comps += 1;
                    size += panel_estimate(panel);
                }
            }
        }
        for (i, item) in self.timeline.iter().enumerate().skip(start) {
            let (c, s, t) = match &item.kind {
                TimelineKind::Reasoning(r) => (4, 300 + first_n_chars_bytes(r, 800), 0),
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
                // A completion entry is one folded panel: its estimate is owned
                // beside its render, like the ledger section's.
                TimelineKind::LedgerEntry(entry) => {
                    (4, crate::feishu::card::ledger::task_entry_estimate(entry), 0)
                }
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
    /// the header state (e.g. "部分完成" on split cards). Returns the card, the
    /// element range of every live block the tail rendered (empty without a
    /// tail), and the Rendered Cursor this body delivers (spec #561).
    fn build_card_inner(
        &self,
        start: usize,
        end: usize,
        include_tail: bool,
        state_override: Option<CardState>,
    ) -> (
        serde_json::Value,
        Vec<crate::bridge::card_handles::BlockSpan>,
        RenderedCursor,
        Option<GapCoverage>,
    ) {
        let state = state_override.unwrap_or_else(|| self.card_state.clone());
        // The body's Rendered Cursor, captured before the builder consumes
        // nothing but `&self`: position and identity only (spec #561).
        let cursor = self.cursor_for_slice(end, include_tail, &state);
        // How far THIS body carries the chain's orphan gap (spec #561, review
        // #569): per body, never per accumulator — a gap split across cards
        // must not read as delivered.
        let gap = self.gap_coverage(end);
        // The header shows the Turn's running tool even when THIS slice has no
        // panel for it (a split continuation after `sleep 30` started): pass
        // the accumulator's global selection as the builder's override. The
        // state is cloned for the builder so the tail below can keep reading
        // the EFFECTIVE state through `omitted_live_seeded` after the move.
        let mut builder = CardBuilder::new()
            .with_state(state.clone())
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
                    // A seeded reasoning entry carries the lead its cut needs
                    // (spec #561): the construct the delivered prefix left
                    // open is closed inside this panel.
                    let r = match &item.lead {
                        Some(lead) => format!("{lead}{r}"),
                        None => r.clone(),
                    };
                    builder =
                        builder.with_reasoning_at(&r, item.shown_at, Some(&format!("reason_{}", item.seq)));
                }
                TimelineKind::Text(t) => {
                    if let Some(lead) = &item.lead {
                        pending.push_str(lead);
                    }
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
                            Some(&format!("tool_{call_id}")),
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
                // A completed Background Task's ledger entry (ADR-0060): one
                // folded panel in the Wake's own position, its `entry_{seq}`
                // id keeping the reader's fold state across re-renders.
                TimelineKind::LedgerEntry(entry) => {
                    if !pending.is_empty() {
                        builder = builder.with_text(&pending);
                        pending.clear();
                    }
                    builder = builder.with_task_entry(entry, Some(&format!("entry_{}", item.seq)));
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
            // The Background Task Ledger follows the todo list (ADR-0060): the
            // Session's live tasks, kept on the newest card by the tail's own
            // handover. Empty renders nothing, so V1 (no Background Task
            // facts) shows exactly what it always did.
            builder = builder.with_task_ledger(&self.ledger);
            // The cleanup exit (spec #588, ticket #590): a Waiting card that
            // carries at least one ⚠️ 状态待确认 row offers ONE section-level
            // button right below the ledger — the wait's home, never a live or
            // terminal card — and the button disappears with the last
            // unconfirmed row, because its presence is derived from the same
            // rows the ledger renders. The click's handler re-derives the rows
            // from the transcript (the card's own list is only the affordance).
            if state == CardState::Waiting
                && self.ledger_has_unconfirmed()
                && let Some(sid) = &self.session_id
            {
                builder = builder.with_element(crate::feishu::card::ledger::cleanup_button(sid));
            }
            // Running tools are live content, so their panels ride the tail —
            // after the todo list and before the interaction blocks, keeping a
            // running tool's Permission/Question below its own panel. A split
            // finalizes the card without them; they continue on the newest
            // card and join the timeline once they settle (ADR-0045).
            //
            // A still-running seeded panel (spec #561's live set) is omitted
            // when no live renderer owns the card ([`Self::omitted_live_seeded`],
            // under this build's effective `state`); a seeded call that
            // settled before the end is a timeline entry and still renders
            // below/above like any other.
            for (call_id, live) in &self.live_tools {
                if self.omitted_live_seeded(&state, call_id) {
                    continue;
                }
                if let Some(panel) = self.tools.get(call_id) {
                    builder =
                        builder.with_tool_at(panel.clone(), live.shown_at, Some(&format!("tool_{call_id}")));
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
                builder = builder.with_text(&crate::feishu::card::error_line(err));
            }

            // The retry marker (spec #391): this failed attempt was retried on
            // a new card below. All failed content stays; the marker line is
            // the card's terminal stamp. A Retried state also suppresses the
            // retry button below (only Error offers it).
            if self.card_state == CardState::Retried {
                builder = builder.with_text("已重试，见下方新卡片");
            }

            // The ONE recovery action a terminal card may offer: the Error
            // card's retry that re-submits the original prompt (spec #391) or
            // the Unreceived card's 重新发起 (ADR-0062, #437) — interrupt (a
            // no-op when idle) then resume, promoting the queued message at the
            // new run's start; on V1, which has no resume endpoint, it degrades
            // to resubmitting the message as a new Turn. `offers_recovery` is
            // the same predicate the card render and the click claim read, so
            // the action-owning states are named once. A Wake continuation's
            // Error never offers Retry: it carries no question to re-ask
            // (ADR-0059). The button carries only the session id — the handler
            // recovers the message from this card's own accumulator.
            if self.card_state.offers_recovery()
                && let Some(sid) = &self.session_id
            {
                let button = match self.card_state {
                    // An external render's ending carries no prompt to
                    // re-submit (#568): its Error must not offer the Retry the
                    // claim below would refuse — the same promptless fact
                    // `claim_recovery` reads, so the button and the click
                    // cannot disagree about whether anything is askable.
                    CardState::Error
                        if !self.wake_continuation
                            && self.prompt.as_deref().is_some_and(|prompt| !prompt.is_empty()) =>
                    {
                        Some(crate::feishu::card::CardActionButton {
                            text: "🔄 重试".to_string(),
                            kind: "primary",
                            value: serde_json::json!({ "action": "retry", "session_id": sid }),
                        })
                    }
                    CardState::Unreceived => Some(crate::feishu::card::CardActionButton {
                        text: "重新发起".to_string(),
                        kind: "primary",
                        value: serde_json::json!({ "action": "resume", "session_id": sid }),
                    }),
                    _ => None,
                };
                if let Some(button) = button {
                    builder = builder.with_recovery_buttons(vec![button]);
                }
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
            if self.worktree {
                segment.push_str(" 🌲");
            }
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

        (builder.build(), spans, cursor, gap)
    }

    /// The part characters this accumulator's timeline delivers for `source`
    /// (spec #561, review #569): the un-rendered prefix a first entry's
    /// `delivered_before` carries, plus every same-source entry's own model
    /// characters. The push semantics keep the entries disjoint — only a
    /// part's FIRST entry may carry a nonzero offset, every later entry (a
    /// server-grown snapshot, a split past the size budget) carries none — so
    /// the offset is counted once, never once per entry.
    pub(super) fn source_extent(&self, source: &PartSource) -> usize {
        self.source_extent_in(source, self.timeline.len())
    }

    /// [`Self::source_extent`] over the timeline prefix `..end`: one body's
    /// Rendered Cursor reads only what that body delivers.
    fn source_extent_in(&self, source: &PartSource, end: usize) -> usize {
        let mut chars = 0usize;
        let mut offset = 0usize;
        for item in &self.timeline[..end] {
            if !item
                .source
                .as_ref()
                .is_some_and(|s| s.message_id == source.message_id && s.index == source.index)
            {
                continue;
            }
            if let TimelineKind::Text(text) | TimelineKind::Reasoning(text) = &item.kind {
                offset = offset.max(item.source.as_ref().map_or(0, |s| s.delivered_before));
                // A reasoning entry shows at most the card's cap (spec #561,
                // review #569): the delivered extent counts only what the
                // element could display, so a restart still renders the
                // characters beyond the cap instead of skipping them.
                chars += match &item.kind {
                    TimelineKind::Reasoning(_) => {
                        text.chars().count().min(crate::feishu::card::REASONING_TEXT_CAP)
                    }
                    _ => text.chars().count(),
                };
            }
        }
        offset + chars
    }

    /// Whether any timeline entry names the part `source` (spec #561, review
    /// #569): the ordinary render reads this to tell a part's first push from
    /// a later one when the entry carries a seed offset and yields no plain
    /// rendered text.
    pub(super) fn has_source(&self, source: &PartSource) -> bool {
        self.timeline
            .iter()
            .any(|item| item.source.as_ref().is_some_and(|s| s.same_part(source)))
    }

    /// Whether the read's part text still carries what this accumulator (and
    /// the chain's cursor) delivered for `source` (spec #561, review #569): the
    /// newest same-part entry's digest — recorded over the part's whole
    /// delivered extent at its push — must hash the read's prefix of that
    /// extent. `false` when the part has no entry here, that entry carries no
    /// digest, the read is shorter than the extent, or the prefix no longer
    /// hashes to it: the caller must then cut at zero, because appending the
    /// read's `extent..` suffix would skip content the card never showed.
    pub(super) fn source_prefix_holds(&self, source: &PartSource, text: &str, extent: usize) -> bool {
        let prefix: String = text.chars().take(extent).collect();
        if prefix.chars().count() < extent {
            return false;
        }
        self.source_digest(source)
            .is_some_and(|digest| digest == crate::bridge::chain::cursor_prefix_digest(&prefix))
    }

    /// The prefix offset the part's entries carry (spec #561's seeded cut): the
    /// characters a PREVIOUS card delivered before them. `0` for the ordinary,
    /// offset-free render.
    pub(super) fn source_delivered_before(&self, source: &PartSource) -> usize {
        self.timeline
            .iter()
            .filter(|item| item.source.as_ref().is_some_and(|s| s.same_part(source)))
            .filter_map(|item| item.source.as_ref().map(|s| s.delivered_before))
            .max()
            .unwrap_or(0)
    }

    /// The prefix digest of `source`'s newest timeline entry — the one whose
    /// push last covered the part's whole delivered extent (spec #561, review
    /// #569). `None` when the part has no entry here, or that entry carries
    /// none: the caller then cannot tell a growth from a rewrite.
    fn source_digest(&self, source: &PartSource) -> Option<u64> {
        self.timeline
            .iter()
            .rev()
            .find(|item| item.source.as_ref().is_some_and(|s| s.same_part(source)))
            .and_then(|item| item.source.as_ref().and_then(|s| s.prefix_digest))
    }

    /// The part content this accumulator's timeline already shows for `source`
    /// — the concatenation of its entries, in order — when that content is a
    /// plain prefix of the part (every entry offset-free). `None` when the
    /// part has no entry here, or a seeded entry carries a prefix the card
    /// does not hold: the caller can then tell neither a growth nor its
    /// remainder. The delta test the ordinary render reads (review #569).
    pub(super) fn source_rendered(&self, source: &PartSource) -> Option<String> {
        let mut rendered = String::new();
        let mut found = false;
        for item in &self.timeline {
            let Some(item_source) = item.source.as_ref() else {
                continue;
            };
            if item_source.message_id != source.message_id || item_source.index != source.index {
                continue;
            }
            if item_source.delivered_before != 0 {
                return None;
            }
            match &item.kind {
                TimelineKind::Text(text) | TimelineKind::Reasoning(text) => {
                    found = true;
                    rendered.push_str(text);
                }
                _ => return None,
            }
        }
        found.then_some(rendered)
    }

    /// Whether `text` is a REWRITE of the part `source` (spec #561, review
    /// #569): the timeline holds content for the part and the new snapshot
    /// does not extend it. The render must then REPLACE the part's entries —
    /// the old content is gone from the read — instead of accumulating the
    /// new snapshot on top, which would double-count the part and leave the
    /// Rendered Cursor digestless.
    pub(super) fn source_rewritten(&self, source: &PartSource, text: &str) -> bool {
        self.source_rendered(source)
            .is_some_and(|rendered| !rendered.is_empty() && !text.starts_with(&rendered))
    }

    /// Replace `source`'s timeline entries with a fresh run of `text` (spec
    /// #561, review #569) — the server rewrote the part, so the replacement is
    /// its whole content now. The new run lands where the old one began
    /// (chunked like any push, keyed at the part's start time), its source
    /// carrying the new content's prefix digest and no offset: the cursor
    /// records the part's new full extent, and a restart resolves it.
    pub(super) fn replace_text_run(&mut self, source: &PartSource, text: &str, at_ms: Option<i64>) {
        self.drop_source_run(source);
        self.push_text_lead(at_ms, Some(self.rewrite_source(source, text)), text, None);
    }

    /// [`Self::replace_text_run`] for a rewritten reasoning part.
    pub(super) fn replace_reasoning_run(&mut self, source: &PartSource, text: &str, at_ms: Option<i64>) {
        self.drop_source_run(source);
        self.push_reasoning_lead(at_ms, Some(self.rewrite_source(source, text)), text, None);
    }

    /// The source a rewritten part's fresh run carries: its position with the
    /// new content's prefix digest and no offset.
    fn rewrite_source(&self, source: &PartSource, text: &str) -> PartSource {
        PartSource {
            message_id: source.message_id.clone(),
            index: source.index,
            delivered_before: 0,
            prefix_digest: Some(crate::bridge::chain::cursor_prefix_digest(text)),
        }
    }

    /// Drop every timeline entry of `source` (spec #561, review #569), keeping
    /// `render_from` on the same logical position: entries before the
    /// finalized boundary belong to sent cards, and a rewritten part's entries
    /// after it are the live render's own.
    fn drop_source_run(&mut self, source: &PartSource) {
        let boundary = self.render_from.min(self.timeline.len());
        let removed_before = self.timeline[..boundary]
            .iter()
            .filter(|item| item.source.as_ref().is_some_and(|s| s.same_part(source)))
            .count();
        self.timeline
            .retain(|item| !item.source.as_ref().is_some_and(|s| s.same_part(source)));
        self.render_from = self.render_from.saturating_sub(removed_before);
    }

    /// The Rendered Cursor of the card body [`Self::build_card_inner`] is
    /// about to render for `timeline[..end]` (spec #561): the chain's
    /// confirmed cursor plus what this body adds. The frontier names the
    /// newest delivered-final item included — text/reasoning with its
    /// cumulative delivered character extent, or a settled tool by position,
    /// the extent then carried from the newest text/reasoning part before it
    /// (review #569) — a body with none of its own keeps the base frontier
    /// (it must never regress). The live set adds the running calls this
    /// body's tail delivers and removes the calls it delivers settled into the
    /// timeline; a call the body omits (a finalized slice has no tail, a
    /// settled card omits a carried `⏳`) keeps its last delivered state.
    /// The newest entry before `end` whose source `wanted` accepts, as a
    /// [`CursorFrontier`] (spec #561): the chain cursor's own frontier on an
    /// ordinary body, and the gap's progress on a body that carries gap
    /// content (the `wanted` predicate tells the two sets apart). `None` when
    /// the body carries no such entry — the cursor keeps its base.
    fn frontier_for(&self, end: usize, wanted: impl Fn(&PartSource) -> bool) -> Option<CursorFrontier> {
        let end = end.min(self.timeline.len());
        let (idx, source, kind, started_at) =
            self.timeline[..end]
                .iter()
                .enumerate()
                .rev()
                .find_map(|(idx, item)| {
                    let source = item.source.as_ref()?;
                    if !wanted(source) {
                        return None;
                    }
                    let kind = match &item.kind {
                        TimelineKind::Text(_) => CursorPartKind::Text,
                        TimelineKind::Reasoning(_) => CursorPartKind::Reasoning,
                        // A timeline tool entry is settled by construction: a
                        // running panel lives in the tail, not the timeline.
                        TimelineKind::Tool(_) => CursorPartKind::Tool,
                        _ => return None,
                    };
                    Some((idx, source, kind, item.shown_at))
                })?;
        let (delivered_chars, prefix_digest) = match kind {
            // The settled tool has no extent of its own: the frontier
            // carries the newest text/reasoning part's, so that part still
            // renders its growth after a restart (review #569).
            CursorPartKind::Tool => {
                match self.timeline[..idx]
                    .iter()
                    .rev()
                    .find_map(|item| match &item.kind {
                        TimelineKind::Text(_) | TimelineKind::Reasoning(_) => item.source.as_ref(),
                        _ => None,
                    })
                    .filter(|source| wanted(source))
                {
                    Some(source) => (self.source_extent_in(source, end), source.prefix_digest),
                    // No text part to fingerprint: the empty prefix.
                    None => (0, Some(crate::bridge::chain::cursor_prefix_digest(""))),
                }
            }
            _ => (self.source_extent_in(source, end), source.prefix_digest),
        };
        Some(CursorFrontier {
            message_id: source.message_id.clone(),
            part_index: source.index,
            kind,
            started_at,
            delivered_chars,
            prefix_digest,
        })
    }

    /// How far the body ending at timeline index `end` carries the chain's
    /// pending orphan gap (spec #561, review #569): the newest gap entry it
    /// includes, and whether that already reaches the gap's last entry — only
    /// then is the whole gap on a card and the durable fact consumable. A body
    /// that stops mid-gap reports `complete: false`, so its confirmation only
    /// advances the gap and the remaining tail stays owed.
    fn gap_coverage(&self, end: usize) -> Option<GapCoverage> {
        let frontier = self.frontier_for(end, |source| self.gap_messages.contains(&source.message_id))?;
        let complete = !self.gap_truncated
            && self
                .timeline
                .iter()
                .rposition(|item| {
                    item.source
                        .as_ref()
                        .is_some_and(|source| self.gap_messages.contains(&source.message_id))
                })
                .is_some_and(|last| last < end.min(self.timeline.len()));
        Some(GapCoverage { frontier, complete })
    }

    fn cursor_for_slice(&self, end: usize, include_tail: bool, state: &CardState) -> RenderedCursor {
        let mut cursor = self.cursor.clone();
        // The chain cursor never names a gap entry (spec #561, review #569):
        // the gap's content sits EARLIER in the read than the confirmed
        // frontier, so taking it would lower the cursor below content a later
        // card already showed.
        if let Some(frontier) =
            self.frontier_for(end, |source| !self.gap_messages.contains(&source.message_id))
        {
            cursor.frontier = Some(frontier);
        }
        for item in &self.timeline[..end] {
            if let TimelineKind::Tool(call_id) = &item.kind {
                cursor.live_calls.remove(call_id);
            }
        }
        if include_tail {
            for call_id in self.live_tools.keys() {
                if !self.omitted_live_seeded(state, call_id) {
                    cursor.live_calls.insert(call_id.clone());
                }
            }
        }
        cursor
    }
}

/// Whether `block` belongs to `kind` — the one ClaimKind → block-family
/// predicate the kind wrappers, the session collector and the resolver share.
pub(super) fn block_of_kind(
    kind: crate::bridge::snapshot_claims::ClaimKind,
    block: &InteractionBlock,
) -> bool {
    use crate::bridge::snapshot_claims::ClaimKind;
    match kind {
        ClaimKind::Permission => matches!(block, InteractionBlock::Permission(_)),
        ClaimKind::Question => matches!(block, InteractionBlock::Question(_)),
    }
}

/// Whether `block` is one the vanished pass resolves: live, left the pending
/// list, not cola's own settlement, and its directory listed successfully. One
/// predicate, shared by the session collector and the resolver, so the
/// classifier and the resolution can never disagree about what vanished.
fn is_vanished(
    block: &InteractionBlock,
    pending: &std::collections::HashSet<String>,
    failed_dirs: &std::collections::HashSet<String>,
    cola_claimed: &std::collections::HashSet<String>,
) -> bool {
    block.is_live()
        && !pending.contains(block.request_id())
        && !cola_claimed.contains(block.request_id())
        && !failed_dirs.contains(block.directory())
}

/// The owning sessions (with their directory) of the live blocks
/// [`resolve_vanished_blocks`] would resolve — the sweep classifies these
/// before choosing the receipt line. Same predicate, so a block can never be
/// classified without being resolved or vice versa.
pub(super) fn vanished_block_sessions(
    acc: &StreamAccumulator,
    pending: &std::collections::HashSet<String>,
    failed_dirs: &std::collections::HashSet<String>,
    cola_claimed: &std::collections::HashSet<String>,
    own: impl Fn(&InteractionBlock) -> bool,
) -> Vec<(String, String)> {
    acc.interactions
        .iter()
        .filter(|b| own(b) && is_vanished(b, pending, failed_dirs, cola_claimed))
        .map(|b| (b.session_id().to_string(), b.directory().to_string()))
        .collect()
}

/// The shared sweep shape for both kinds (#175): resolve every LIVE block the
/// kind owns (`own`) whose request vanished, writing `line`'s Interaction
/// Receipt — the sweep classifies the owning session's run state and passes
/// the neutral or the interrupted line. A block owned by a directory whose
/// list call failed stays: that directory said nothing, so its request may
/// still be pending (#130, #144). A block cola itself is answering (or
/// answered) also stays: its disappearance from the pending list is cola's own
/// doing, and the sweep's line would be a lie — the settlement leaves the true
/// receipt. Returns how many were resolved — the caller repaints each affected
/// card so the receipt lands within one poll.
pub(super) fn resolve_vanished_blocks(
    acc: &mut StreamAccumulator,
    pending: &std::collections::HashSet<String>,
    failed_dirs: &std::collections::HashSet<String>,
    cola_claimed: &std::collections::HashSet<String>,
    own: impl Fn(&InteractionBlock) -> bool,
    line: impl Fn(&InteractionBlock) -> String,
) -> usize {
    acc.resolve_vanished(
        |block| own(block) && is_vanished(block, pending, failed_dirs, cola_claimed),
        line,
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
    backend: &Arc<dyn crate::backend::Backend>,
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

/// The ledger kind of a Background Task's tool. Only `shell` and `subagent`
/// ever background through the V2 tool shape (`decode_background_task` returns
/// `None` for every other name), so the pair is matched once, here.
pub(super) fn task_kind(tool: &str) -> TaskKind {
    match tool {
        "subagent" => TaskKind::Subagent,
        _ => TaskKind::Shell,
    }
}

/// The label the originating tool part's input names for a task of `kind`:
/// the shell's `command` (or its `description` when the payload carries no
/// command), the subagent's `description` — the `subagent` arm nothing else
/// needed. `None` when the input names no label, so the row renders bare
/// rather than inventing one (the completion entry's own rule).
pub(super) fn task_label(kind: TaskKind, input: Option<&serde_json::Value>) -> Option<String> {
    let label = match kind {
        TaskKind::Shell => input
            .and_then(|input| input.get("command"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                input
                    .and_then(|input| input.get("description"))
                    .and_then(serde_json::Value::as_str)
            }),
        TaskKind::Subagent => input
            .and_then(|input| input.get("description"))
            .and_then(serde_json::Value::as_str),
    }?;
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackgroundTask, ToolIdentity, ToolStatus};
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

    /// `apply_footer_model` always advances the model id, but an EMPTY provider
    /// (V1's decoder emits `""` for "none") or an absent variant leaves the last
    /// known fact in place — the footer never drops a provider/variant it had
    /// (ADR-0019, ADR-0044).
    #[test]
    fn apply_footer_model_keeps_the_last_provider_and_variant() {
        let mut acc = StreamAccumulator::new("test");
        acc.apply_footer_model("m1", "provider-a", Some("high"));
        assert_eq!(acc.model_id.as_deref(), Some("m1"));
        assert_eq!(acc.provider_id.as_deref(), Some("provider-a"));
        assert_eq!(acc.variant.as_deref(), Some("high"));

        // A later message with an empty provider and no variant keeps both.
        acc.apply_footer_model("m2", "", None);
        assert_eq!(
            acc.model_id.as_deref(),
            Some("m2"),
            "the model id always advances"
        );
        assert_eq!(
            acc.provider_id.as_deref(),
            Some("provider-a"),
            "an empty provider is not a provider"
        );
        assert_eq!(
            acc.variant.as_deref(),
            Some("high"),
            "an absent variant leaves the last one"
        );

        // A reported provider/variant replaces them.
        acc.apply_footer_model("m3", "provider-b", Some("low"));
        assert_eq!(acc.provider_id.as_deref(), Some("provider-b"));
        assert_eq!(acc.variant.as_deref(), Some("low"));
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

    /// #433: a session running in a linked worktree shows 🌲 between the
    /// project name and the branch — the project name is the worktree
    /// directory's basename, the branch is what it checks out.
    #[test]
    fn card_footer_marks_a_worktree() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Done;
        acc.directory = Some("/root/workspace/dev/cola/.worktrees/zh-user-guide".into());
        acc.project_name = Some("zh-user-guide".into());
        acc.branch = Some("docs/zh-user-guide".into());
        acc.worktree = true;
        acc.push_text("结果");

        let text = acc.build_card().to_string();
        assert!(
            text.contains("📁 zh-user-guide 🌲 · docs/zh-user-guide"),
            "missing: {text}"
        );
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
            worktree: false,
        });
        assert_eq!(acc.branch.as_deref(), Some("main"));
        assert!(acc.dirty);
        assert!(!acc.worktree);

        // Failed/empty end read: the start capture stays.
        acc.apply_git_state(GitState::default());
        assert_eq!(acc.branch.as_deref(), Some("main"));
        assert!(acc.dirty);
        assert!(!acc.worktree);

        // Successful end read: the AI switched branch and committed.
        acc.apply_git_state(GitState {
            branch: Some("feat/ai-work".into()),
            dirty: false,
            worktree: true,
        });
        assert_eq!(acc.branch.as_deref(), Some("feat/ai-work"));
        assert!(!acc.dirty);
        assert!(acc.worktree, "the worktree marker rides the refreshed halves");
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
            ToolPanel::for_test(
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
                ToolPanel::for_test(&format!("tool{}", i), ToolStatus::Completed, None, None),
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
                ToolPanel::for_test(
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
                ToolPanel::for_test(
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

    /// The yielded-refresh guard (ADR-0060, ticket #419): `build_card_unsplit`
    /// renders the whole live slice — tail included — on the tracked card and
    /// leaves `render_from` untouched, so a ledger-only change can never
    /// finalize the card and owe a continuation, even when the estimate
    /// crosses the split budget.
    #[test]
    fn build_card_unsplit_keeps_the_whole_slice_on_one_card() {
        let mut acc = StreamAccumulator::new("test");
        acc.card_state = CardState::Waiting;
        acc.set_ledger(
            vec![TaskLedgerRow {
                kind: TaskKind::Shell,
                label: Some("npm run build".into()),
                started_at: None,
                unconfirmed: false,
                activity: None,

                output: None,
            }],
            0,
            LedgerCadence::Minute,
        );
        acc.push_text(&"很长的回答。".repeat(2000)); // over one card's budget

        // The normal build finalizes this slice and advances the boundary.
        let (finalized, full) = acc.build_card_with_split();
        assert!(full, "the oversized slice must split");
        let boundary = acc.render_from;
        assert!(boundary > 0, "the split advances the boundary");
        assert!(
            finalized.to_string().contains("部分完成，继续中"),
            "the split card finalizes: {finalized}"
        );

        // The unsplit build keeps the remainder AND the tail on the tracked
        // card, with the boundary untouched: nothing is owed a continuation.
        let unsplit = acc.build_card_unsplit();
        assert!(!unsplit.full, "an unsplit build never finalizes");
        assert_eq!(acc.render_from, boundary, "the boundary is untouched");
        let text = unsplit.card.to_string();
        assert!(
            !text.contains("部分完成，继续中"),
            "the card keeps its own header: {}",
            unsplit.card
        );
        assert!(
            text.contains("⏳ 后台任务（1）") && text.contains("npm run build"),
            "the ledger tail rides the unsplit card: {}",
            unsplit.card
        );
    }

    /// Spec #588 / #590: the cleanup button rides the ledger section of a
    /// WAITING card carrying ≥1 unconfirmed row — one section-level element
    /// right below the ledger panel — and is absent for a confirmed-only
    /// ledger, a card without a session id, and every non-waiting state (a live
    /// card stays click-free).
    #[test]
    fn the_cleanup_button_rides_a_waiting_card_with_unconfirmed_rows_only() {
        let row = |unconfirmed: bool| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some("npm run build".into()),
            started_at: None,
            unconfirmed,
            activity: None,

            output: None,
        };
        let cleanup_element = |card: &serde_json::Value| {
            card["body"]["elements"]
                .as_array()
                .unwrap()
                .iter()
                .position(|el| el["value"]["action"] == "cleanup")
        };
        let build = |state: CardState, session: Option<&str>, unconfirmed: bool| {
            let mut acc = StreamAccumulator::new("test");
            acc.card_state = state;
            acc.session_id = session.map(str::to_string);
            acc.set_ledger(vec![row(unconfirmed)], 0, LedgerCadence::Minute);
            acc.build_card_unsplit().card
        };

        let waiting = build(CardState::Waiting, Some("ses_1"), true);
        let button = cleanup_element(&waiting).expect("the waiting card offers the cleanup button");
        let ledger = waiting["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .position(|el| el["element_id"] == "task_ledger")
            .expect("the ledger panel");
        assert!(
            button > ledger,
            "the button sits below the ledger section: {waiting}"
        );
        assert_eq!(
            waiting["body"]["elements"][button]["value"]["session_id"], "ses_1",
            "the click names the session it clears"
        );

        assert!(
            cleanup_element(&build(CardState::Waiting, Some("ses_1"), false)).is_none(),
            "no unconfirmed row, no button"
        );
        assert!(
            cleanup_element(&build(CardState::Waiting, None, true)).is_none(),
            "no session id, no actionable button"
        );
        assert!(
            cleanup_element(&build(CardState::Streaming, Some("ses_1"), true)).is_none(),
            "a live card stays click-free: the cleanup lives on the wait's home only"
        );
    }

    /// The ledger's clock records what the card last rendered — each row's
    /// elapsed in whole seconds — and each path compares it at its own cadence
    /// (ADR-0060, #423): two reads inside one rendered second owe nothing
    /// either way; the yielded path (whole seconds) owes the moment a rendered
    /// second moves, while the live path (whole minutes) waits for the minute
    /// and so gains no per-render clock churn.
    #[test]
    fn the_ledger_clock_owes_a_flush_at_each_paths_cadence() {
        let start = 1_800_000_000_000;
        let rows = || {
            vec![TaskLedgerRow {
                kind: TaskKind::Shell,
                label: Some("npm run build".into()),
                started_at: Some(start),
                unconfirmed: false,
                activity: None,

                output: None,
            }]
        };
        let cadence = |acc: &mut StreamAccumulator, at: i64, cadence| acc.set_ledger(rows(), at, cadence);
        let mut acc = StreamAccumulator::new("test");

        // The first read is a membership change (empty -> one row).
        assert!(cadence(&mut acc, start, LedgerCadence::Minute));
        // Two reads inside one rendered second owe nothing, either cadence.
        assert!(!cadence(&mut acc, start + 999, LedgerCadence::Second));
        assert!(!cadence(&mut acc, start + 999, LedgerCadence::Minute));
        // A rendered second moving owes the yielded PATCH, not the live one.
        assert!(cadence(&mut acc, start + 1_000, LedgerCadence::Second));
        assert!(!cadence(&mut acc, start + 1_000, LedgerCadence::Minute));
        // The seconds inside the minute keep owing on the yielded path and
        // never on the live one...
        for secs in [2_000, 30_000, 59_000] {
            assert!(cadence(&mut acc, start + secs, LedgerCadence::Second));
            assert!(!cadence(&mut acc, start + secs, LedgerCadence::Minute));
        }
        // ...until the whole minute turns, which owes the live flush too.
        assert!(cadence(&mut acc, start + 60_000, LedgerCadence::Minute));
        // A retired task is a membership change on both paths: the row leaves.
        assert!(acc.set_ledger(Vec::new(), start + 61_000, LedgerCadence::Minute));
        assert!(acc.set_ledger(rows(), start + 61_000, LedgerCadence::Second));
        // The same read repeated never owes one, on either cadence.
        assert!(!cadence(&mut acc, start + 61_500, LedgerCadence::Minute));
        assert!(!cadence(&mut acc, start + 61_500, LedgerCadence::Second));
    }

    /// Spec #588 / #592: the output window's cutoff is a rendered clock — a
    /// re-read inside the minute the label shows owes nothing, a crossing
    /// minute owes its flush through the CLOCK half only (a silent shell's
    /// window must not renew the renderer's idle bound, #457), and different
    /// tail text is a rendered-fact change on the row half.
    #[test]
    fn the_windows_cutoff_owes_only_a_clock_flush() {
        let start = 1_800_000_000_000;
        let row = |captured_ms: i64, text: &str| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some("npm run build".into()),
            started_at: None,
            unconfirmed: false,
            activity: None,
            output: Some(crate::backend::ShellOutputWindow {
                text: text.into(),
                clipped: false,
                captured_ms,
            }),
        };
        let mut acc = StreamAccumulator::new("test");
        assert!(
            acc.set_ledger_change(vec![row(start, "a")], start, LedgerCadence::Minute)
                .owes()
        );

        let change = acc.set_ledger_change(
            vec![row(start + 30_000, "a")],
            start + 30_000,
            LedgerCadence::Minute,
        );
        assert!(
            !change.owes() && !change.rows && !change.clock,
            "a re-read inside the label's minute moved nothing: {change:?}"
        );

        let change = acc.set_ledger_change(
            vec![row(start + 60_000, "a")],
            start + 60_000,
            LedgerCadence::Minute,
        );
        assert!(
            change.clock && !change.rows,
            "the cutoff crossing a minute is a clock-only flush: {change:?}"
        );

        let change = acc.set_ledger_change(
            vec![row(start + 60_000, "b")],
            start + 60_000,
            LedgerCadence::Minute,
        );
        assert!(
            change.rows && !change.clock,
            "different tail text is a row change: {change:?}"
        );
    }

    /// #457: the live render's idle-bound renewal reads the ledger's `rows`
    /// half only. A silent task's rendered elapsed/age crossing a whole minute
    /// is clock churn — it owes the flush but must not renew the bound, or the
    /// card of a run that stopped producing would stay live forever.
    #[test]
    fn a_ledger_clock_turn_is_not_progress() {
        let start = 1_800_000_000_000;
        let rows = || {
            vec![TaskLedgerRow {
                kind: TaskKind::Shell,
                label: Some("npm run build".into()),
                started_at: Some(start),
                unconfirmed: false,
                activity: None,

                output: None,
            }]
        };
        let mut acc = StreamAccumulator::new("test");
        // The first read is a membership change: progress.
        let first = acc.set_ledger_change(rows(), start, LedgerCadence::Minute);
        assert!(first.rows, "membership is progress");
        // The rendered minute turns with the same rows: clock only.
        let minute = acc.set_ledger_change(rows(), start + 60_000, LedgerCadence::Minute);
        assert!(!minute.rows, "a ticking elapsed is not progress");
        assert!(minute.clock && minute.owes(), "it still owes the live flush");
        // A visible fact moving is progress again.
        let mut relabelled = rows();
        relabelled[0].label = Some("npm run test".into());
        let moved = acc.set_ledger_change(relabelled, start + 60_000, LedgerCadence::Minute);
        assert!(moved.rows, "a visible fact is progress");
    }

    /// The flush decision reads the RENDERED row, not the gathered liveness's
    /// hidden timestamps (spec #501): a child's newest-part time moving inside
    /// the rendered second the card already shows owes nothing, while a
    /// rendered second turning — or the fragment's words changing — owes the
    /// PATCH. The freshest timestamps are stored either way, so the next age
    /// counts from the latest read, never from a stale accepted one.
    #[test]
    fn a_hidden_activity_timestamp_inside_the_rendered_second_owes_nothing() {
        let start = 1_800_000_000_000;
        let row = |activity: TaskLiveness| {
            vec![TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some("review the diff".into()),
                // No elapsed: the activity age is the card's only rendered
                // number, so each assertion isolates it.
                started_at: None,
                unconfirmed: false,
                activity: Some(activity),

                output: None,
            }]
        };
        let thinking = |last_activity_ms: i64| TaskLiveness {
            activity: crate::feishu::card::tool_render::ChildActivity::Thinking,
            last_activity_ms,
            wait: None,
        };
        let mut acc = StreamAccumulator::new("test");

        // The first read is a membership change (empty -> one row).
        assert!(acc.set_ledger(row(thinking(start)), start, LedgerCadence::Second));
        // A new child part 400 ms later: the fragment still renders `思考中 0s`,
        // so no PATCH — and the fresh timestamp IS stored.
        assert!(!acc.set_ledger(row(thinking(start + 400)), start + 400, LedgerCadence::Second));
        assert_eq!(
            acc.ledger[0].activity.as_ref().unwrap().last_activity_ms,
            start + 400,
            "the fresh timestamp is stored without a PATCH"
        );
        // The rendered second turns: 0s -> 1s, which owes.
        assert!(acc.set_ledger(row(thinking(start + 400)), start + 1_400, LedgerCadence::Second));
        // A shape change at the same instant owes too: the phase word...
        let replying = |at: i64| TaskLiveness {
            activity: crate::feishu::card::tool_render::ChildActivity::Replying,
            last_activity_ms: at,
            wait: None,
        };
        assert!(acc.set_ledger(row(replying(start + 1_400)), start + 1_400, LedgerCadence::Second));
        // ...and a wait joining an otherwise identical fragment.
        let mut waiting = row(replying(start + 1_400));
        waiting[0].activity.as_mut().unwrap().wait = Some(crate::feishu::card::AwaitingAction::Permission);
        assert!(acc.set_ledger(waiting, start + 1_400, LedgerCadence::Second));
    }

    /// The LIVE path's flush rule (spec #501, ticket #504): the activity age
    /// grows by whole seconds on every render, but the live card compares its
    /// clock at whole minutes, so the seconds alone never owe a PATCH — only
    /// the age's minute turning does (the same gate a shell row's elapsed has).
    /// A typed liveness change — a tool switch, a wait joining the fragment,
    /// the gather establishing or losing the child — is a rendered change and
    /// owes its flush at once, inside the same minute.
    #[test]
    fn live_ledger_activity_seconds_never_owe_but_typed_changes_do() {
        let start = 1_800_000_000_000;
        let row = |activity: Option<TaskLiveness>| {
            vec![TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some("review the diff".into()),
                // No elapsed: the activity age is the card's only rendered
                // number, so each assertion isolates it.
                started_at: None,
                unconfirmed: false,
                activity,
                output: None,
            }]
        };
        let tool = |name: &str, wait: Option<crate::feishu::card::AwaitingAction>| TaskLiveness {
            activity: crate::feishu::card::tool_render::ChildActivity::Tool {
                name: name.into(),
                started_at: Some(start),
            },
            last_activity_ms: start,
            wait,
        };
        let mut acc = StreamAccumulator::new("test");

        // The first read: the row arrives before its child is established.
        assert!(acc.set_ledger(row(None), start, LedgerCadence::Minute));
        // The gather establishes the child's running tool: a typed change owes.
        assert!(acc.set_ledger(row(Some(tool("bash", None))), start, LedgerCadence::Minute));
        // The age grows through the rendered minute: the live card's gate
        // never owes a PATCH for the seconds alone.
        assert!(!acc.set_ledger(
            row(Some(tool("bash", None))),
            start + 30_000,
            LedgerCadence::Minute
        ));
        // The age's whole minute turning owes — the same gate as the elapsed.
        assert!(acc.set_ledger(
            row(Some(tool("bash", None))),
            start + 60_000,
            LedgerCadence::Minute
        ));
        // A typed change inside the same minute owes at once: the tool switches…
        assert!(acc.set_ledger(
            row(Some(tool("edit", None))),
            start + 60_100,
            LedgerCadence::Minute
        ));
        // …a wait joins the otherwise identical fragment…
        assert!(acc.set_ledger(
            row(Some(tool(
                "edit",
                Some(crate::feishu::card::AwaitingAction::Permission)
            ))),
            start + 60_200,
            LedgerCadence::Minute
        ));
        // …the wait leaves again — a rendered change like the wait joining…
        assert!(acc.set_ledger(
            row(Some(tool("edit", None))),
            start + 60_300,
            LedgerCadence::Minute
        ));
        // …and the gather losing the child renders the bare row again.
        assert!(acc.set_ledger(row(None), start + 60_400, LedgerCadence::Minute));
    }

    /// Read recovery on the live path (spec #501, ticket #504): a FAILED gather
    /// (an empty map) keeps the last established fragment and owes nothing —
    /// the stored row still renders — while the gather healing to a new typed
    /// activity owes its flush at once, on the live path's minute gate.
    #[test]
    fn live_ledger_read_recovery_owes_a_flush() {
        let start = 1_800_000_000_000;
        let transcript = SessionTranscript::default().with_background_tasks(vec![BackgroundTask {
            tool: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_sub".into(),
            },
            shell_id: None,
            child_id: Some("ses_child".into()),
            started_at: None,
        }]);
        let bash = TaskLiveness {
            activity: crate::feishu::card::tool_render::ChildActivity::Tool {
                name: "bash".into(),
                started_at: Some(start),
            },
            last_activity_ms: start,
            wait: None,
        };
        let mut acc = StreamAccumulator::new("test");

        // The child is established (membership: empty -> a row with a fragment).
        assert!(
            acc.set_ledger_from_read(
                &transcript,
                &HashMap::from([("call_sub".to_string(), bash.clone())]),
                start,
                LedgerCadence::Minute
            )
            .owes()
        );
        // The gather fails: the stored fragment is kept, so the same rendered
        // row owes nothing — a failed read is not a change.
        assert!(
            !acc.set_ledger_from_read(
                &transcript,
                &HashMap::new(),
                start + 30_000,
                LedgerCadence::Minute
            )
            .owes()
        );
        assert!(
            acc.ledger[0].activity.is_some(),
            "a failed gather keeps the last established fragment"
        );
        // The gather heals to a new typed activity: that change owes at once.
        let replying = TaskLiveness {
            activity: crate::feishu::card::tool_render::ChildActivity::Replying,
            last_activity_ms: start + 31_000,
            wait: None,
        };
        assert!(
            acc.set_ledger_from_read(
                &transcript,
                &HashMap::from([("call_sub".to_string(), replying.clone())]),
                start + 31_000,
                LedgerCadence::Minute
            )
            .owes()
        );
        // A wait joins the healed fragment, then leaves it: each is a rendered
        // change and owes its flush.
        let mut waiting = replying.clone();
        waiting.wait = Some(crate::feishu::card::AwaitingAction::Permission);
        assert!(
            acc.set_ledger_from_read(
                &transcript,
                &HashMap::from([("call_sub".to_string(), waiting)]),
                start + 31_100,
                LedgerCadence::Minute
            )
            .owes()
        );
        assert!(
            acc.set_ledger_from_read(
                &transcript,
                &HashMap::from([("call_sub".to_string(), replying)]),
                start + 31_200,
                LedgerCadence::Minute
            )
            .owes()
        );
    }

    /// The flush decision compares only what the row RENDERS (spec #501,
    /// acceptance 5): a label differing past the renderer's clip, an empty
    /// label where none renders, and a start clock moving inside the displayed
    /// minute and elapsed second all render identically and owe nothing —
    /// while the clip itself changing, or the displayed minute or a rendered
    /// second moving, owes.
    #[test]
    fn rendered_label_and_clock_differences_never_owe_a_flush() {
        let base = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let shell = |label: Option<&str>, started_at: Option<i64>| {
            vec![TaskLedgerRow {
                kind: TaskKind::Shell,
                label: label.map(str::to_string),
                started_at,
                unconfirmed: false,
                activity: None,

                output: None,
            }]
        };
        let clipped = "x".repeat(crate::feishu::card::ledger::TASK_LABEL_CHARS);

        // Labels: a difference beyond the clip is not on the row.
        let mut acc = StreamAccumulator::new("test");
        assert!(acc.set_ledger(
            shell(Some(&format!("{clipped}AAA")), Some(base)),
            base,
            LedgerCadence::Second
        ));
        assert!(
            !acc.set_ledger(
                shell(Some(&format!("{clipped}BBB")), Some(base)),
                base,
                LedgerCadence::Second
            ),
            "text past the clip is clipped off the row"
        );
        // The visible prefix changing still owes.
        assert!(acc.set_ledger(shell(Some("short"), Some(base)), base, LedgerCadence::Second));
        // An empty label renders as no label at all — `None` is the same row.
        assert!(acc.set_ledger(shell(Some(""), Some(base)), base, LedgerCadence::Second));
        assert!(
            !acc.set_ledger(shell(None, Some(base)), base, LedgerCadence::Second),
            "an empty label and no label render the same row"
        );

        // Starts: the row shows `HH:MM`; the elapsed is the render clock's.
        let mut acc = StreamAccumulator::new("test");
        assert!(acc.set_ledger(shell(None, Some(base)), base, LedgerCadence::Second));
        assert!(
            !acc.set_ledger(shell(None, Some(base + 200)), base + 200, LedgerCadence::Second),
            "a start moving inside the displayed minute and second renders the same row"
        );
        // A start whose displayed minute moved owes, even where the (clamped)
        // elapsed second is unchanged.
        assert!(
            acc.set_ledger(
                shell(None, Some(base + 60_000)),
                base + 30_000,
                LedgerCadence::Second
            ),
            "the displayed minute is a visible fact"
        );
        // A rendered second crossing owes through the render clock (the read
        // clock has now passed the start's own second).
        assert!(acc.set_ledger(
            shell(None, Some(base + 60_000)),
            base + 61_200,
            LedgerCadence::Second
        ));
    }

    /// A subagent row never renders its total runtime (spec #501 format), so a
    /// start moving — even by whole seconds — can never owe a flush: only the
    /// fragment's age ticks. (The displayed start minute is still a visible
    /// fact, like any row's.)
    #[test]
    fn a_subagents_elapsed_movement_never_owes_a_flush() {
        // Mid-minute, so a few seconds of movement stay inside one `HH:MM`.
        let base = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2) + 30_000;
        let row = |started_at: i64, activity_at: i64| {
            vec![TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some("review the diff".into()),
                started_at: Some(started_at),
                unconfirmed: false,
                activity: Some(TaskLiveness {
                    activity: crate::feishu::card::tool_render::ChildActivity::Thinking,
                    last_activity_ms: activity_at,
                    wait: None,
                }),

                output: None,
            }]
        };
        let mut acc = StreamAccumulator::new("test");
        // Membership: empty -> one row, age 0s.
        assert!(acc.set_ledger(row(base, base), base, LedgerCadence::Second));
        // The start moves 5 s back (same displayed minute): a shell row's
        // elapsed would owe here — the subagent renders no elapsed, so it
        // does not.
        assert!(
            !acc.set_ledger(row(base - 5_000, base), base, LedgerCadence::Second),
            "a subagent row's elapsed is not rendered, so it cannot owe"
        );
        // The fragment's rendered second turning still owes.
        assert!(
            acc.set_ledger(row(base - 5_000, base), base + 1_000, LedgerCadence::Second),
            "the fragment's age is the subagent row's rendered clock"
        );
    }

    /// A row with no start time has no clock: a read that only re-reports it
    /// (the same label, no start) never owes a flush on either cadence — the
    /// clock tracks the rendered numbers, and there are none.
    #[test]
    fn a_start_less_row_never_ticks_the_ledger_clock() {
        let rows = || {
            vec![TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some("review the diff".into()),
                started_at: None,
                unconfirmed: false,
                activity: None,

                output: None,
            }]
        };
        let mut acc = StreamAccumulator::new("test");
        assert!(acc.set_ledger(rows(), 1_000, LedgerCadence::Minute));
        for at in [1_000, 5_000, 90_000] {
            assert!(!acc.set_ledger(rows(), at, LedgerCadence::Minute));
            assert!(!acc.set_ledger(rows(), at, LedgerCadence::Second));
        }
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
            ToolPanel::for_test(
                "bash",
                ToolStatus::Completed,
                Some(serde_json::json!("ls")),
                Some("src"),
            ),
        );
        acc.push_text("再看一下配置。");
        acc.push_tool(
            "call_2",
            ToolPanel::for_test(
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
            ToolPanel::for_test(
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
            ToolPanel::for_test("bash", ToolStatus::Running, Some(serde_json::json!("ls")), None),
        );
        acc.push_tool(
            "call_1",
            ToolPanel::for_test(
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
            ToolPanel::for_test(
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
            ToolPanel::for_test(
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
    /// reader's fold state survives the move. The id names the CALL
    /// (`tool_{call_id}`, spec #561, review #569), so a takeover's collect can
    /// also identify the marker of a call the successor resolved.
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
            ToolPanel::for_test(
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
        assert_eq!(
            panel_ids(&done_card),
            vec!["tool_call_bash".to_string()],
            "the panel's id names its call"
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
            ToolPanel::for_test(
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
            ToolPanel::for_test(
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
        acc.todo_panel = Some(ToolPanel::for_test("todowrite", ToolStatus::Running, None, None));
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Tool));
        acc.todo_panel = Some(ToolPanel::for_test(
            "todowrite",
            ToolStatus::Completed,
            None,
            None,
        ));
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Streaming));
        // A resumed card (ADR-0066) works on like streaming — same phase, same
        // timer: entering it resets the clock the waiting yield stopped.
        acc.card_state = CardState::Resuming;
        acc.refresh_phase();
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Streaming));
        // A tool of the resumed run is its own phase, like on a live card.
        acc.push_tool(
            "call_2",
            ToolPanel::for_test("bash", ToolStatus::Running, None, None),
        );
        assert_eq!(acc.active_phase(), Some(HeaderPhase::Tool));
        // Finished turns show no timer phase.
        acc.card_state = CardState::Done;
        acc.refresh_phase();
        assert_eq!(acc.active_phase(), None);
        // A stopped turn is terminal too: the stop terminal carries no timer
        // (#394).
        acc.card_state = CardState::Stopped;
        acc.refresh_phase();
        assert_eq!(acc.active_phase(), None);
        assert_eq!(acc.current_phase, None);
    }

    /// The one ending application (spec #538, #539): every disposition stamps
    /// its card state and failure line, a stop clears a recorded error, `Done`
    /// and `Waiting` leave it as it was, and `Observe` stamps nothing at all.
    #[test]
    fn apply_ending_stamps_every_disposition() {
        use super::super::disposition::{LOST_CONTACT_ERROR, STUCK_PANEL_ERROR};

        let apply = |disposition: &Disposition| {
            let mut acc = StreamAccumulator::new("test");
            acc.error = Some("previous failure".into());
            acc.apply_ending(disposition);
            (acc.card_state.clone(), acc.error.clone())
        };
        assert_eq!(
            apply(&Disposition::Observe),
            (CardState::Loading, Some("previous failure".into())),
            "Observe is no ending: nothing is stamped"
        );
        assert_eq!(
            apply(&Disposition::Waiting),
            (CardState::Waiting, Some("previous failure".into())),
            "the waiting yield leaves a recorded failure as it was"
        );
        assert_eq!(apply(&Disposition::Unreceived), (CardState::Unreceived, None));
        assert_eq!(
            apply(&Disposition::Done),
            (CardState::Done, Some("previous failure".into())),
            "the true end leaves a recorded failure as it was"
        );
        assert_eq!(
            apply(&Disposition::Failed("503".into())),
            (CardState::Error, Some("503".into()))
        );
        assert_eq!(apply(&Disposition::Stopped), (CardState::Stopped, None));
        assert_eq!(
            apply(&Disposition::LostContact),
            (CardState::Error, Some(LOST_CONTACT_ERROR.into()))
        );
        assert_eq!(
            apply(&Disposition::StuckPanel),
            (CardState::Error, Some(STUCK_PANEL_ERROR.into()))
        );
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
                    ..Default::default()
                }],
                kind: crate::opencode::types::FormFieldKind::String,
                custom: None,
                ..Default::default()
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

    /// The Error card's Retry button (spec #391) is gated by the card's KIND
    /// and by its having a question to re-ask: an Error card carrying its
    /// prompt offers it as before, while a Wake continuation never does — it
    /// carries no question to re-ask (ADR-0059) — and neither does a
    /// promptless card (an external follow's ending, #568), where the render
    /// must not offer the dead Retry `claim_recovery` refuses.
    #[test]
    fn an_error_card_offers_retry_only_with_a_question_to_reask() {
        let error_card = |wake_continuation: bool, prompt: Option<&str>| {
            let mut acc = StreamAccumulator::new("test");
            acc.card_state = CardState::Error;
            acc.session_id = Some("ses_1".into());
            acc.wake_continuation = wake_continuation;
            acc.prompt = prompt.map(str::to_string);
            acc.build_card().to_string()
        };
        assert!(
            error_card(false, Some("跑一下 CI")).contains("重试"),
            "an Error card with its prompt offers Retry exactly as before"
        );
        assert!(
            !error_card(true, Some("跑一下 CI")).contains("重试"),
            "a Wake continuation never offers Retry"
        );
        assert!(
            !error_card(false, None).contains("重试"),
            "a promptless Error card (an external follow) offers no dead Retry"
        );
    }

    /// `push_receipt_at` can key a receipt at a SERVER time, ahead of work
    /// whose server times are already in the past at poll time (the Wake
    /// continuation's 承接 line), while `push_receipt` keeps the
    /// resolution-moment key everything else uses (a click, a Supplement
    /// split) — after everything already on the card.
    #[test]
    fn a_receipt_can_be_keyed_before_the_work_it_announces() {
        let first_content = |card: &serde_json::Value| {
            card["body"]["elements"][0]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };

        // The resumed work is keyed at its server time; the 承接 receipt is
        // keyed just before it, so it opens the card.
        let mut acc = StreamAccumulator::new("test");
        acc.push_text_at(Some(1_000), "resumed work");
        acc.push_receipt_at(Some(999), "🔔 承接");
        assert_eq!(first_content(&acc.build_card()), "🔔 承接");

        // The default key is cola's "now": after content already on the card.
        let mut acc = StreamAccumulator::new("test");
        acc.push_text_at(Some(1_000), "earlier content");
        acc.push_receipt("📨 receipt");
        let card = acc.build_card();
        assert_eq!(first_content(&card), "earlier content");
        assert_eq!(card["body"]["elements"][1]["content"], "📨 receipt");
    }

    /// Two text parts of ONE message sharing a server time keep their own
    /// timeline entries — and their own sources (spec #561, review #569):
    /// merging them would record the combined character count under the FIRST
    /// part's digest, so the frontier could not resolve on a restart and both
    /// parts would render again. The card's content and order are unchanged.
    #[test]
    fn two_text_parts_at_one_server_time_keep_their_own_entries() {
        let message = MessageId::new("msg_a_2000");
        let first = "第一段回答。";
        let second = "第二段回答。";
        let source = |index: usize, text: &str| PartSource {
            message_id: message.clone(),
            index,
            delivered_before: 0,
            prefix_digest: Some(crate::bridge::chain::cursor_prefix_digest(text)),
        };
        let mut acc = StreamAccumulator::new("test");
        acc.push_text_lead(Some(2_000), Some(source(0, first)), first, None);
        acc.push_text_lead(Some(2_000), Some(source(1, second)), second, None);

        assert_eq!(
            acc.timeline
                .iter()
                .map(|item| item.source.as_ref().map(|source| source.index))
                .collect::<Vec<_>>(),
            vec![Some(0), Some(1)],
            "each part keeps its own entry and source: {:?}",
            acc.timeline
        );
        let built = acc.build_card_with_info();
        assert_eq!(
            built.cursor.frontier,
            Some(CursorFrontier {
                message_id: message.clone(),
                part_index: 1,
                kind: CursorPartKind::Text,
                started_at: Some(2_000),
                delivered_chars: second.chars().count(),
                prefix_digest: Some(crate::bridge::chain::cursor_prefix_digest(second)),
            }),
            "the frontier names the second part with its OWN extent"
        );

        // The frontier resolves against the read: part one stays delivered and
        // the second part renders only past its own cut.
        let transcript = SessionTranscript::new(vec![crate::backend::TranscriptMessage {
            id: message.clone(),
            role: crate::backend::MessageRole::Assistant,
            time: Some(crate::backend::MessageTime {
                created: 2_000,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: vec![
                crate::backend::Part::Text(crate::backend::TextPart {
                    text: first.to_string(),
                    started_at: Some(2_000),
                }),
                crate::backend::Part::Text(crate::backend::TextPart {
                    text: second.to_string(),
                    started_at: Some(2_000),
                }),
            ],
        }]);
        let seed = CursorSeed::resolve(&transcript, &built.cursor).expect("the frontier resolves");
        let frontier = seed.frontier.as_ref().expect("a frontier");
        assert_eq!(frontier.part_index, 1);
        assert_eq!(frontier.delivered_chars, second.chars().count());
    }

    /// The announcement split (spec #588, review PR #595): a synthetic
    /// retirement marks the chain's exactly-once set and stages NOTHING
    /// durable, while a real Wake still stages the Wake Watermark — the
    /// watermark stays Wake-scoped (ADR-0061), so a late real Wake at or below
    /// a synthetic clock is never read as covered after a restart.
    #[test]
    fn a_synthetic_announcement_marks_exactly_once_and_stages_nothing() {
        let mut acc = StreamAccumulator::new("test");
        assert!(
            acc.announce_synthetic("runtime:call_1"),
            "the first synthetic mark lands"
        );
        assert!(
            !acc.announce_synthetic("runtime:call_1"),
            "a repeated synthetic announce never doubles"
        );
        assert!(acc.wake_announced("runtime:call_1"));
        assert_eq!(
            acc.pending_watermark_id(),
            None,
            "a synthetic entry stages no durable Wake Watermark"
        );

        assert!(acc.announce_wake("msg_wake_1", 1_000));
        let id = acc.pending_watermark_id();
        let staged = id
            .and_then(|id| acc.take_staged_watermark(id))
            .expect("a real Wake stages its durable mark");
        assert_eq!(staged.wake_id, "msg_wake_1");
        assert_eq!(staged.created_ms, 1_000);
    }
}
