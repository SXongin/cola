//! The Timeline: the ordered entries of a card body and the rule that places
//! content into them (spec #1040, tickets #1041–#1042).
//!
//! Private to the Turn module (ADR-0050, ADR-0104): the accumulator holds one
//! [`Timeline`] and delegates its timeline accessors here. The entries, the
//! message and read floors, the Interaction Receipt cut staircase, the
//! live-slice boundary (`render_from`), the sealed-entry set, merge-by-part
//! lookup and key/identity minting live in this file; nothing outside it
//! mutates one of them.
//!
//! The placement interface is a walk (ticket #1042): [`Timeline::walk`] yields
//! a [`Walk`], `walk.message(...)` yields a [`MessageScope`] whose `place`
//! returns a linear [`Placement`] and whose `push` consumes it. Both guards
//! clear their floors on `Drop`, and a live tool tail is pinned as a
//! [`TailPlacement`] and settled through [`Timeline::settle`].
//!
//! One private insert path ([`Timeline::seat`]) owns the live-slice clamp, the
//! index an entry takes and the continuation rule. A part's key is resolved by
//! its message scope's `place`, and the same slot seats the entry, so the merge
//! lookup and the insert cannot disagree.

use crate::backend::{Compaction, MessageId, Part, TranscriptMessage};
use crate::bridge::chain::EndingKey;
use crate::feishu::card::ledger::TaskCompletionEntry;
use crate::feishu::card::tool_render::ToolPanel;
use std::collections::{HashMap, HashSet};

/// An entry's read position (spec #1040): the message's position in the read,
/// then the part's index in that message. `None` for a source-less or
/// synthetic entry, which sorts before positioned entries at the same key.
pub(super) type ReadPosition = (usize, usize);

/// The tie-break order a fresh entry uses at its equal-key position: after
/// every entry already seated there (`order` breaks ties only within a class).
const AFTER_EQUAL_POSITION: u64 = u64::MAX;

/// One entry on a turn's chronological timeline: a rendered item plus the
/// server-side start time (epoch ms) of the part it came from — its **key**.
/// The card renders the timeline in key order, matching how OpenChamber shows
/// the parts interleaved. The card only ever shows server clocks (#183
/// follow-up).
#[derive(Debug, Clone)]
pub(super) struct TimelineItem {
    key: i64,
    /// The server start time the entry may display, or `None` when the key is
    /// a synthetic ordering device. Synthetic receipts, ledger entries, user
    /// messages and leads carry `None` (their card elements show no clock); a
    /// compaction keeps its start time only because the accumulator's
    /// `frontier_for` records it as the Rendered Cursor's `started_at`, still
    /// without displaying it.
    shown_at: Option<i64>,
    /// The entry's position within an equal-`key` run. An ordinary insert's
    /// `order` equals its `seq`; a chunked continuation shares its part entry's
    /// order, and the render-boundary clamp lowers it. `seq` is never
    /// renumbered, so the identity a panel is keyed by cannot drift (issue
    /// #1022).
    order: u64,
    /// Identity of this entry, assigned once at insertion and never reused.
    /// It names the entry's panel on the card (`reason_{seq}`; a tool panel
    /// uses the call's own id, `tool_{call_id}`, so the takeover's per-call
    /// strip can match it); unlike the entry's position it survives timeline
    /// insertions and merges, so the client's local panel state can't drift to
    /// another panel when the card re-renders.
    seq: u64,
    kind: TimelineKind,
    /// The typed transcript part this entry rendered from, for text/reasoning
    /// entries pushed by the render (spec #561's cursor frontier); `None` for
    /// synthetic entries (receipts, ledger entries) and for entries no
    /// frontier can name (tools).
    source: Option<PartSource>,
    /// A markdown lead to emit before this entry's content (spec #561's cut
    /// tail): the fence opener or table header a projection's seeded render
    /// needs because the delivered prefix ended inside that construct. It is
    /// rendered, never counted as the part's own delivered characters.
    lead: Option<String>,
    /// The entry's read position — its message's position in the read, then
    /// the part's index in that message — for the equal-key tie-break (spec
    /// #1040). `None` for a source-less or synthetic entry: those sort before
    /// positioned entries at the same key.
    position: Option<ReadPosition>,
}

impl TimelineItem {
    /// The entry's order key: the server-side part start time, or a synthetic
    /// ordering device. Never a display clock.
    #[cfg(test)]
    pub(super) fn key(&self) -> i64 {
        self.key
    }

    /// The server start time the entry may display, `None` for a synthetic
    /// key. A receipt, a notice, a user lead and a compaction row render no
    /// clock whatever this carries.
    pub(super) fn shown_at(&self) -> Option<i64> {
        self.shown_at
    }

    /// The entry's position within an equal-`key` run.
    #[cfg(test)]
    pub(super) fn order(&self) -> u64 {
        self.order
    }

    /// The entry's stable identity, used for panel element ids.
    pub(super) fn seq(&self) -> u64 {
        self.seq
    }

    /// What the entry renders.
    pub(super) fn kind(&self) -> &TimelineKind {
        &self.kind
    }

    /// The typed transcript part the entry rendered from, if any.
    pub(super) fn source(&self) -> Option<&PartSource> {
        self.source.as_ref()
    }

    /// The markdown lead emitted before the entry's content, if any.
    pub(super) fn lead(&self) -> Option<&str> {
        self.lead.as_deref()
    }

    /// A whole entry, for the fingerprint's unit test.
    #[cfg(test)]
    pub(super) fn test_item(
        key: i64,
        shown_at: Option<i64>,
        order: u64,
        seq: u64,
        kind: TimelineKind,
        source: Option<PartSource>,
        lead: Option<String>,
    ) -> Self {
        Self {
            key,
            shown_at,
            order,
            seq,
            kind,
            source,
            lead,
            position: None,
        }
    }
}

/// An Interaction Receipt's **server-time cut** (issue #1023): the asking tool
/// part's server time (`at`) and the receipt's own timeline key (`key`). Only
/// `at` is on the server's clock — the cut exists so a later read's part times
/// can be compared against the ask; `key` is the timeline position (cola's
/// scale) that content written after the ask may not precede.
///
/// The cut answers one question about content a later read delivers: was it
/// written after the Host clicked? A part whose `shown_at` is AFTER `at` was
/// server-timestamped after the request was asked, and the request blocks the
/// turn, so the Host's click cannot precede it — it follows the click and may
/// not sort above the receipt at `key` (ADR-0038 rule 4). A part at or before
/// `at` was content the Host could see when they clicked, and keeps its own
/// position, above the receipt if its key says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReceiptCut {
    at: i64,
    key: i64,
}

impl ReceiptCut {
    /// The timeline position content written after the ask may not precede.
    #[cfg(test)]
    pub(super) fn key(&self) -> i64 {
        self.key
    }
}

/// The two positions a click's receipt reservation holds (spec #1040): the key
/// the receipt itself will take, and the floor already installed for content
/// written during the reply. The Timeline mints both, so the accumulator never
/// derives the floor from the receipt key itself.
#[derive(Debug, Clone, Copy)]
pub(super) struct ReservationKeys {
    pub(super) receipt_key: i64,
    pub(super) floor_key: i64,
}

/// Where a timeline text/reasoning entry came from: the typed transcript
/// part's own position (spec #561's cursor frontier). The payload carries no
/// stable part id (AGENTS.md pitfall `wire-shapes`), so identity is the message plus the part's
/// ordinal in it. Test and synthetic pushes carry none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PartSource {
    /// The assistant message that carried the part.
    message_id: MessageId,
    /// The part's index in its message's typed `parts` vec.
    index: usize,
    /// The part's model characters that were already delivered before this
    /// entry was rendered (spec #561's projection seed): the frontier's cut
    /// point. The cursor's delivered extent for the part is this offset plus
    /// the entry's own (model) characters; the markdown lead is not counted.
    /// `0` for every ordinary render.
    delivered_before: usize,
    /// The digest of the part's delivered prefix at the moment this entry was
    /// pushed (spec #561, review #569): derived metadata, never content, kept
    /// so the frontier can tell a grown part from a same-slot replacement even
    /// when neither carries a server start time (V2). `None` for a source no
    /// render stamped.
    prefix_digest: Option<u64>,
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

    /// A source with every half explicit: a seeded render's offset, or a
    /// rewritten part's fresh digest.
    pub(super) fn new(
        message_id: MessageId,
        index: usize,
        delivered_before: usize,
        prefix_digest: Option<u64>,
    ) -> Self {
        Self {
            message_id,
            index,
            delivered_before,
            prefix_digest,
        }
    }

    /// The assistant message that carried the part.
    pub(super) fn message_id(&self) -> &MessageId {
        &self.message_id
    }

    /// The part's index in its message's typed `parts` vec.
    pub(super) fn index(&self) -> usize {
        self.index
    }

    /// The digest of the part's delivered prefix, when a push stamped one.
    pub(super) fn prefix_digest(&self) -> Option<u64> {
        self.prefix_digest
    }

    /// Stamp the digest a seeded push owes.
    fn set_prefix_digest(&mut self, digest: u64) {
        self.prefix_digest = Some(digest);
    }

    /// Whether `self` names the same transcript part as `other` (the position,
    /// not the offset or digest).
    pub(super) fn same_part(&self, other: &PartSource) -> bool {
        self.message_id == other.message_id && self.index == other.index
    }
}

/// The transcript part class a [`Placement`] expects (spec #1040): what kind
/// of content the part will push. A push whose [`TranscriptKind`] disagrees is
/// refused before it inserts anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PartForm {
    Text,
    Reasoning,
    Tool,
}

/// The facts a message scope needs to place one transcript part (spec #1040):
/// the part's own server start, the clock it may display, its merge identity
/// and its index in the message. `key_at` is the transcript-order policy's
/// resolved key ([`part_order_times`]); `None` falls back to `next_order`.
#[derive(Debug, Clone)]
pub(super) struct PartFacts {
    pub(super) form: PartForm,
    pub(super) key_at: Option<i64>,
    pub(super) shown_at: Option<i64>,
    pub(super) source: Option<PartSource>,
    pub(super) index: usize,
}

/// The content a transcript push carries (spec #1040). The lead is a seeded
/// cut's fence opener or table header — rendered, never counted. A tool's
/// panel body stays in the accumulator's `tools` map; the timeline entry only
/// names the call.
#[derive(Debug, Clone)]
pub(super) enum TranscriptKind {
    Text { chunk: String, lead: Option<String> },
    Reasoning { chunk: String, lead: Option<String> },
    Tool(String),
}

impl TranscriptKind {
    fn form(&self) -> PartForm {
        match self {
            Self::Text { .. } => PartForm::Text,
            Self::Reasoning { .. } => PartForm::Reasoning,
            Self::Tool(_) => PartForm::Tool,
        }
    }
}

/// Why a [`MessageScope::push`] was refused (spec #1040): a token from another
/// scope, a kind that disagrees with the part the token placed, or a rewrite
/// whose source names no part or another one. The timeline is untouched on
/// every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlacementError {
    /// The token came from a scope that has since been dropped (or another
    /// one entirely).
    Stale,
    /// The pushed content is not the transcript kind the token placed.
    KindMismatch,
    /// A rewrite names a source-less token or a different part.
    SourceMismatch,
}

/// An owned, linear placement token (spec #1040): the message scope's
/// `place` produces it and its `push` consumes it. Private fields and no
/// public constructor, neither `Clone` nor `Copy`, so reuse is a compile
/// error. It carries the resolved entry key, the merge identity, the display
/// clock, the floor it was placed under, the expected transcript kind and the
/// scope generation that minted it.
#[derive(Debug)]
pub(super) struct Placement {
    key: i64,
    source: Option<PartSource>,
    shown_at: Option<i64>,
    floor: Option<i64>,
    expected: PartForm,
    generation: u64,
    position: Option<ReadPosition>,
}

/// One live tail's fact, as the accumulator hands it to a walk or a settle
/// (spec #1040): the message that owns it, the part index it was pinned at and
/// its key. The Timeline keeps only the ones that precede the content being
/// placed.
#[derive(Debug, Clone)]
pub(super) struct TailFact {
    pub(super) message: MessageId,
    pub(super) part: usize,
    pub(super) key: i64,
}

/// The live, unsettled tool tail's placement (spec #1040): everything the
/// settle needs to re-seat the panel later. The panel body stays in the
/// accumulator's `tools` map; this token never enters [`MessageScope::push`].
#[derive(Debug, Clone)]
pub(super) struct TailPlacement {
    key: i64,
    shown_at: Option<i64>,
    source: Option<PartSource>,
    /// The floor the pin was placed under (message receipt cut and read
    /// floor). A later `rekey`/`settle` never drops below it.
    floor: Option<i64>,
    /// The owning message. Every pin on the guard names one: a walk's
    /// `MessageScope::pin` names the message it scoped, and the scripted test
    /// seam names its own out-of-read message.
    message: Option<MessageId>,
    created: Option<i64>,
    position: Option<ReadPosition>,
    call_id: String,
    seq: u64,
}

impl TailPlacement {
    pub(super) fn shown_at(&self) -> Option<i64> {
        self.shown_at
    }

    /// A later sighting may carry the part source the first one lacked.
    pub(super) fn adopt_source(&mut self, source: Option<PartSource>) {
        if self.source.is_none() {
            self.source = source;
        }
    }

    /// The fact a later walk or settle needs: which message owns the tail,
    /// which part pinned it and where it sits. A pin whose message carries no
    /// read position (the scripted test seam) still names its message and
    /// part, so the fact is well-formed; only a pin that named no message at
    /// all yields `None`.
    pub(super) fn fact(&self) -> Option<TailFact> {
        let message = self.message.clone()?;
        let part = self.position.map_or_else(
            || self.source.as_ref().map_or(0, |source| source.index()),
            |(_, part)| part,
        );
        Some(TailFact {
            message,
            part,
            key: self.key,
        })
    }
}

/// What a timeline entry renders.
#[derive(Debug, Clone)]
pub(super) enum TimelineKind {
    Text(String),
    Reasoning(String),
    Tool(String),
    /// The External Message's own user block (ADR-0028): the text another
    /// client — the TUI, another IDE — sent into the Session, rendered as a
    /// blockquote that names the role. Keyed just before the Turn's anchor, so
    /// it heads the card; a distinct kind (never a `Text` entry) is what keeps
    /// it out of the reply's text run, which merges consecutive text chunks
    /// into one element (live 2026-10-26: the TUI's message and the answer read
    /// as one paragraph).
    UserMessage(String),
    /// A resolved interaction block's Interaction Receipt (ADR-0038, rule 4):
    /// one markdown line keyed by the moment it was resolved, so it sits below
    /// everything that was on the card when the Host clicked and above
    /// everything resolved afterwards.
    Receipt(String),
    /// A completed Background Task's ledger entry (ADR-0060): the mechanical
    /// completion line as a folded collapsible panel's title, the task's
    /// identity and timing in the fold. Keyed at the Wake's own server time,
    /// so the entry sits where the completion happened — never on a later
    /// card, and never doubled by a repeated poll. The second field is the
    /// **Task Ending** this entry renders (ADR-0080) — the run and the class
    /// whose shown bit this body's confirmed write flips, `None` for an entry
    /// whose read carried no identity to name. The body that renders an ending
    /// stages it, so a body that leaves the entry to a continuation claims
    /// nothing for it.
    // Boxed: the ending pair is large and this variant is the rarest in the
    // timeline, so the box keeps the enum (and the timeline it fills) small.
    LedgerEntry(TaskCompletionEntry, Option<Box<EndingKey>>),
    /// One **Compaction** (ADR-0082): a folded row naming its reason with the
    /// summary in the fold — or, while it is in flight or failed, a plain live
    /// or failure row — keyed at the compaction's own server time, so it sits in
    /// history where it happened rather than at the card's end. The typed fact
    /// rides the entry: the row's revision is what dedupes against it
    /// (`RenderedPart::compaction`).
    Compaction(Compaction),
}

/// One content-bearing entry that has no message scope (spec #1040): a receipt
/// line, a compaction row, a ledger entry, the External Message's lead or a
/// bot-authored notice. Each maps onto the stored [`TimelineKind`] its call
/// site uses today; the admission inserts directly and returns the key it took
/// (`None` when the variant owns no single key: a [`SyntheticEntry::Lead`] may
/// merge or chunk, and a blank [`SyntheticEntry::UserMessage`] renders nothing)
/// and never produces a pushable placement, so a synthetic row cannot travel
/// the transcript path.
///
/// Every variant's `at` is an **order key only** — a resolved click's reserved
/// receipt key, a server time, or a derived key just before the work it
/// introduces — never a display clock. `None` mints a strictly increasing key.
/// The one exception in the stored entry is a compaction's start time, which
/// rides `shown_at` for the cursor frontier's `started_at` while the card still
/// renders no clock for it.
pub(super) enum SyntheticEntry<'a> {
    /// One receipt line: a click's receipt, a merge opening, a receive hint,
    /// the residual marker, a skipped-content note or a split line.
    Receipt { text: &'a str, at: Option<i64> },
    /// One compaction row. `at` is the pre-floor seed, `message_time` its
    /// message's server time (the Interaction Receipt cut, `None` when
    /// timeless), and `row` its merge identity: a repeated fact at that
    /// position updates the row it already has, key and identity included.
    Compaction {
        fact: Compaction,
        at: Option<i64>,
        message_time: Option<i64>,
        row: PartSource,
    },
    /// One completed Background Task's ledger entry.
    Ledger {
        entry: TaskCompletionEntry,
        ending: Option<EndingKey>,
        at: Option<i64>,
    },
    /// The External Message's own user block.
    UserMessage { text: &'a str, at: Option<i64> },
    /// A bot-authored notice rendered as text: the fresh-card Wake 承接
    /// opener or a busy-follow static lead. It may merge into an existing text
    /// entry or chunk, so it returns no key.
    Lead { text: &'a str, at: Option<i64> },
}

/// The Timeline's ordered entries plus the state that decides where content
/// lands. Private to this module: the accumulator reads it through its
/// accessors and mutates it only through intent methods.
#[derive(Debug, Default, Clone)]
pub(super) struct Timeline {
    /// Text, reasoning, tool and receipt entries ordered by their key (the
    /// server-side part start time) — the card is built from this, so message ↔
    /// tool interleaving is preserved even when a part renders late.
    items: Vec<TimelineItem>,
    /// Fallback key source for pushes without a server part time
    /// ([`Self::next_order`]).
    order_seq: i64,
    /// Highest key inserted so far — keeps [`Self::next_order`] ahead of the
    /// timeline even when a server clock runs ahead of cola's.
    last_key: i64,
    /// Next [`TimelineItem::seq`] to hand out.
    item_seq: u64,
    /// Every anchored Interaction Receipt's server-time cut (issue #1023), in
    /// resolution order. A list, not one cut: two requests can be asked in one
    /// order and answered in another (Q1 asked at 1000, Q2 at 2000, Q2 answered
    /// first, Q1 second), and content written between the two asks must clear
    /// Q1's receipt while staying below the newer one — one "latest" cut cannot
    /// express that staircase.
    receipt_cuts: Vec<ReceiptCut>,
    /// The timeline floor of the transcript MESSAGE currently being rendered
    /// (issue #1023): a message the server created after a receipt's ask was
    /// produced after that click, so every part of it sorts after the receipt.
    /// Transient: the walks set it per message and clear it after, so a push
    /// outside a walk (a synthetic receipt, a wake line) is never floored.
    message_floor: Option<i64>,
    /// The highest key this render walk has placed so far (issue #1022): the
    /// next message's parts may not sort above content already read before
    /// them, even when a post-tool part took a synthetic `next_order` key and
    /// the next message's own server time is older. Transient, like
    /// `message_floor`: the walks reset it at their start and end.
    read_floor: Option<i64>,
    /// Timeline index the CURRENT card starts rendering from. When a card fills
    /// up (Feishu component limit) it is finalized with a "to be continued"
    /// marker and `render_from` advances — a fresh continuation card renders the
    /// remaining timeline from there. Private: advanced by the builds and
    /// re-based by [`Self::drop_source_run`]; the failed-write rewind goes
    /// through [`Self::restore_render_boundary`], the projection arm's fenced
    /// retry through [`Self::rewind_render_boundary`].
    render_from: usize,
    /// Timeline entries an abandonment skipped (issue #886). A later chunk of
    /// the same streaming part must start a NEW entry, never merge into a
    /// skipped one — no card will ever render it again.
    sealed_entries: HashSet<u64>,
    /// Bumped whenever a message scope is created (spec #1040): a `Placement`
    /// from an earlier scope is stale and refused.
    scope_generation: u64,
}

/// A resolved insertion position: the final key and the index the entry takes.
struct Slot {
    key: i64,
    index: usize,
    clamped: bool,
}

/// One entry to seat: everything the private insert path needs.
struct Entry {
    slot: Slot,
    seq: Option<u64>,
    order: Option<u64>,
    shown_at: Option<i64>,
    source: Option<PartSource>,
    kind: TimelineKind,
    lead: Option<String>,
    position: Option<ReadPosition>,
}

/// A render read's walk over the timeline (spec #1040): holds the read's
/// message positions and the timeline mutably. Its `Drop` clears the read
/// floor, so a finished — or abandoned — walk cannot leave one set.
pub(super) struct Walk<'a, 'r> {
    timeline: &'a mut Timeline,
    read_order: &'r HashMap<&'r str, usize>,
}

/// One message's scope within a [`Walk`] (spec #1040): holds the timeline
/// mutably and the per-message floor. Its `Drop` clears the message floor.
/// `place` produces a [`Placement`]; `push` consumes it.
pub(super) struct MessageScope<'a> {
    timeline: &'a mut Timeline,
    generation: u64,
    position: Option<usize>,
    message: MessageId,
    created: Option<i64>,
}

impl Drop for MessageScope<'_> {
    fn drop(&mut self) {
        self.timeline.message_floor = None;
    }
}

impl Drop for Walk<'_, '_> {
    fn drop(&mut self) {
        self.timeline.read_floor = None;
    }
}

/// The transcript-order policy (issue #1022), owned beside the Timeline (spec
/// #1040). V2's text item carries no time at all, so — for the parts BEFORE
/// the message's first tool call — the read's own order supplies it: a part
/// without a time borrows the floor (the previous timed part's start, or the
/// message's `time.created`), and a timed part behind the floor takes the floor
/// too. The first tool call and everything after it keep the pre-#1022 order
/// (the part's own time, or the render-order fallback): a part after a tool was
/// produced after that call's result, and the resolution receipt's own ordering
/// (ADR-0038 rule 4) stays untouched. Every entry's `shown_at` (and so the
/// Rendered Cursor frontier) still carries only the server's time.
pub(super) fn part_order_times(message: &TranscriptMessage) -> Vec<Option<i64>> {
    let first_tool = message
        .parts
        .iter()
        .position(|part| matches!(part, Part::Tool(_)));
    let mut floor = message.time.as_ref().map(|time| time.created);
    let mut orders = Vec::with_capacity(message.parts.len());
    for (index, part) in message.parts.iter().enumerate() {
        if first_tool.is_none_or(|first| index >= first) {
            orders.push(part.started_at());
            continue;
        }
        let order = match part {
            Part::Text(_) | Part::Reasoning(_) => match part.started_at() {
                Some(own) => Some(floor.map_or(own, |floor| floor.max(own))),
                None => floor,
            },
            _ => {
                orders.push(None);
                continue;
            }
        };
        floor = order;
        orders.push(order);
    }
    orders
}

/// The read position a transcript entry occupies (spec #1040): its own
/// recorded position, or — for a source-bearing entry seated without one (the
/// scripted test seam) — its source's message position in the read plus the
/// part index. Synthetic entries (a receipt, a compaction row, a ledger entry)
/// have none.
fn item_read_position(item: &TimelineItem, read_order: &HashMap<&str, usize>) -> Option<ReadPosition> {
    if !matches!(
        item.kind,
        TimelineKind::Text(_) | TimelineKind::Reasoning(_) | TimelineKind::Tool(_)
    ) {
        return None;
    }
    if let Some(position) = item.position {
        return Some(position);
    }
    let source = item.source.as_ref()?;
    let message = *read_order.get(source.message_id.as_str())?;
    Some((message, source.index()))
}

impl Timeline {
    /// How many entries the timeline holds — the flush's "did anything arrive
    /// while the refused write was in flight" probe and the card build's end.
    pub(super) fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the timeline has no entries.
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The entry list, for the accumulator's read-only walks.
    pub(super) fn iter(&self) -> std::slice::Iter<'_, TimelineItem> {
        self.items.iter()
    }

    /// The entry slice, for an indexed walk.
    #[cfg(test)]
    pub(super) fn entries(&self) -> &[TimelineItem] {
        &self.items
    }

    /// The receipt-cut staircase, for the reservation lifecycle's test read.
    #[cfg(test)]
    pub(super) fn receipt_cuts(&self) -> &[ReceiptCut] {
        &self.receipt_cuts
    }

    /// The entry slice `start..end`, clamped to the timeline's length like the
    /// accumulator's old direct indexing.
    pub(super) fn range(&self, start: usize, end: usize) -> &[TimelineItem] {
        let end = end.min(self.items.len());
        let start = start.min(end);
        &self.items[start..end]
    }

    /// The timeline index the current card starts rendering from.
    pub(super) fn render_from(&self) -> usize {
        self.render_from
    }

    /// Move the live-slice boundary forward (a split's handoff).
    pub(super) fn set_render_from(&mut self, index: usize) {
        self.render_from = index;
    }

    /// Rewind the render boundary to `to` unconditionally: the projection arm's
    /// fenced retry re-renders its first body from scratch.
    pub(super) fn rewind_render_boundary(&mut self, to: usize) {
        self.render_from = to;
    }

    /// Rewind the render boundary to `from` when the build still left it at
    /// `to` — the failed- and fenced-write restore. Returns whether it moved.
    pub(super) fn restore_render_boundary(&mut self, from: usize, to: usize) -> bool {
        if self.render_from != to {
            return false;
        }
        self.rewind_render_boundary(from);
        true
    }

    /// Skip the timeline slice `end` past refused content (issue #886): seal
    /// its entries so a later chunk starts a new one, and advance the boundary
    /// (never backwards).
    pub(super) fn skip_rejected_slice(&mut self, start: usize, end: usize) {
        for item in &self.items[start.min(end)..end] {
            self.sealed_entries.insert(item.seq);
        }
        if end > self.render_from {
            self.render_from = end;
        }
    }

    /// Whether any entry is an Interaction Receipt whose line starts with
    /// `prefix` (ADR-0038, rule 4).
    #[cfg(test)]
    pub(super) fn has_receipt_prefix(&self, prefix: &str) -> bool {
        self.items
            .iter()
            .any(|item| matches!(&item.kind, TimelineKind::Receipt(text) if text.starts_with(prefix)))
    }

    /// Record the server-time cut of a receipt written without a reservation
    /// (issue #1023). Every anchored receipt keeps its own cut: a later receipt
    /// is not necessarily for a later ask, and [`Self::receipt_cut_floor`]
    /// needs the whole staircase. A block without an anchor records nothing.
    pub(super) fn record_cut(&mut self, asked_at: i64, key: i64) {
        self.receipt_cuts.push(ReceiptCut { at: asked_at, key });
    }

    /// Mint a click's reserved receipt keys and install the floor cut (issue
    /// #1023): the receipt takes `receipt_key`, content written during the
    /// reply is floored at `floor_key` (`receipt_key + 1`). The floor key is
    /// consumed, so the next synthetic key cannot equal it. The keys and their
    /// relation are the Timeline's, so the floor derivation never leaks into
    /// the accumulator.
    pub(super) fn reserve_receipt_keys(&mut self, asked_at: i64) -> ReservationKeys {
        let receipt_key = self.next_order();
        let floor_key = receipt_key.saturating_add(1);
        self.receipt_cuts.push(ReceiptCut {
            at: asked_at,
            key: floor_key,
        });
        self.order_seq = floor_key;
        self.last_key = self.last_key.max(floor_key);
        ReservationKeys {
            receipt_key,
            floor_key,
        }
    }

    /// Drop a reservation's floor cut without writing its receipt.
    pub(super) fn forget_cut(&mut self, floor_key: i64) {
        self.receipt_cuts.retain(|cut| cut.key != floor_key);
    }

    /// Re-point a reservation's floor cut at the latest ask it covers.
    pub(super) fn retune_cut(&mut self, floor_key: i64, at: i64) {
        if let Some(cut) = self.receipt_cuts.iter_mut().find(|cut| cut.key == floor_key) {
            cut.at = cut.at.max(at);
        }
    }

    /// The key floor a transcript MESSAGE created at `created` earns from the
    /// Interaction Receipts (issue #1023): every receipt asked before the
    /// message existed was clicked before it was written, so none of its parts
    /// may sort above that receipt (ADR-0038 rule 4). The newest such receipt's
    /// key is the floor. `None` when there is no such receipt or the message
    /// predates every ask.
    pub(super) fn receipt_cut_floor(&self, created: i64) -> Option<i64> {
        self.receipt_cuts
            .iter()
            .filter(|cut| created > cut.at)
            .map(|cut| cut.key)
            .max()
    }

    /// Begin a render read's walk (spec #1040). The read's message positions
    /// order the equal-key tie-break and scope the read and pinned-tail
    /// floors; the walk's `Drop` clears the read floor, and each message
    /// scope's `Drop` clears the message floor.
    pub(super) fn walk<'a, 'r>(&'a mut self, read_order: &'r HashMap<&'r str, usize>) -> Walk<'a, 'r> {
        self.read_floor = None;
        self.message_floor = None;
        Walk {
            timeline: self,
            read_order,
        }
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

    /// Allocate the identity a live tool panel pins when it is born.
    pub(super) fn allocate_seq(&mut self) -> u64 {
        self.item_seq += 1;
        self.item_seq
    }

    /// The timeline key of the entry already held for `source`, when the live
    /// slice holds one (issue #1023): a placed part's position is fixed across
    /// reads, so a later chunk merges into it instead of starting a second
    /// entry at a freshly allocated key.
    fn placed_source_key(&self, source: &PartSource) -> Option<i64> {
        self.items[self.render_from..].iter().rev().find_map(|item| {
            item.source
                .as_ref()
                .filter(|held| held.same_part(source))
                .map(|_| item.key)
        })
    }

    /// The timeline key of the entry already held for `source` in the whole
    /// timeline, when it holds one.
    pub(super) fn entry_key_of(&self, source: &PartSource) -> Option<i64> {
        self.items.iter().rev().find_map(|item| {
            item.source
                .as_ref()
                .is_some_and(|item_source| item_source.same_part(source))
                .then_some(item.key)
        })
    }

    /// The live-slice clamp: an insertion index behind the finalized boundary
    /// is moved to the slice's start, and its key to the slice's first key, so
    /// the timeline stays sorted and key lookups stay sound.
    fn slot_at(&self, key: i64, index: usize) -> Slot {
        if index < self.render_from {
            let index = self.render_from;
            Slot {
                key: self.items.get(index).map_or(key, |item| item.key),
                index,
                clamped: true,
            }
        } else {
            Slot {
                key,
                index,
                clamped: false,
            }
        }
    }

    /// Where an entry at `key` sorts among its equal-key peers (spec #1040):
    /// a source-less or synthetic entry (no read position) sorts BEFORE every
    /// positioned entry; positioned entries sort by read position — the
    /// message's position in the read, then the part index — and `order` breaks
    /// ties only within a class.
    fn equal_key_cmp(item: &TimelineItem, position: Option<ReadPosition>, order: u64) -> std::cmp::Ordering {
        match (item.position, position) {
            (None, None) => item.order.cmp(&order),
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(item_position), Some(position)) => {
                item_position.cmp(&position).then_with(|| item.order.cmp(&order))
            }
        }
    }

    /// The slot a fresh entry takes at `key`: after every equal-key entry of
    /// its own class and read position, before the ones that read later.
    fn slot_for_new(&self, key: i64, position: Option<ReadPosition>) -> Slot {
        let index = self.items.partition_point(|item| {
            item.key < key
                || (item.key == key
                    && Self::equal_key_cmp(item, position, AFTER_EQUAL_POSITION)
                        != std::cmp::Ordering::Greater)
        });
        self.slot_at(key, index)
    }

    /// The slot a pre-allocated identity takes at `key`: its equal-key position
    /// is its own read position (then `order`), exactly as the settle move
    /// assumes.
    fn slot_for_seq(&self, key: i64, seq: u64, position: Option<ReadPosition>) -> Slot {
        let index = self.items.partition_point(|item| {
            item.key < key
                || (item.key == key
                    && Self::equal_key_cmp(item, position, seq) != std::cmp::Ordering::Greater)
        });
        self.slot_at(key, index)
    }

    /// The slot a continuation chunk takes: directly after its own entry, never
    /// past the live slice's start.
    fn slot_after(&self, after: usize, key: i64) -> Slot {
        self.slot_at(key, after + 1)
    }

    /// The one private insert path. Seats `kind` at the resolved slot,
    /// allocating an identity when `seq` is `None`; a clamped slot lowers the
    /// entry's `order` ahead of the equal-key run without touching `seq`.
    fn seat(&mut self, entry: Entry) -> usize {
        let Entry {
            slot,
            seq,
            order: order_hint,
            shown_at,
            source,
            kind,
            lead,
            position,
        } = entry;
        let seq = seq.unwrap_or_else(|| {
            self.item_seq += 1;
            self.item_seq
        });
        self.item_seq = self.item_seq.max(seq);
        let order = if slot.clamped {
            self.items[slot.index..]
                .iter()
                .take_while(|item| item.key == slot.key)
                .map(|item| item.order)
                .min()
                .map_or(order_hint.unwrap_or(seq), |first| first.saturating_sub(1))
        } else {
            order_hint.unwrap_or(seq)
        };
        self.items.insert(
            slot.index,
            TimelineItem {
                key: slot.key,
                shown_at,
                order,
                seq,
                kind,
                source,
                lead,
                position,
            },
        );
        self.last_key = self.last_key.max(slot.key);
        slot.index
    }

    /// The timeline index of the LAST entry keyed `key` that renders a kind
    /// `accepts` and belongs to the SAME transcript part as `source` (spec
    /// #561, review #569): the entry a push may merge into. Entries from
    /// different parts never merge — two parts of one message may share a
    /// server time. A `None` on either side demands an exact `None` match, so a
    /// synthetic push never absorbs a part's entry.
    fn source_entry(
        &self,
        key: i64,
        source: Option<&PartSource>,
        accepts: impl Fn(&TimelineKind) -> bool,
    ) -> Option<usize> {
        let end = self.items.partition_point(|item| item.key <= key);
        // The live slice only: a finalized entry is already on a sent card, so
        // a grown part starts a new entry on the continuation rather than
        // merging into a body nobody will render again.
        let start = self.render_from.min(end);
        for (offset, item) in self.items[start..end]
            .iter()
            .enumerate()
            .rev()
            .take_while(|(_, item)| item.key == key)
        {
            let idx = start + offset;
            // A sealed entry was abandoned by a refused payload (issue #886):
            // no card will render it again, so a later chunk of the same part
            // must start a NEW entry instead of growing the dead one.
            if self.sealed_entries.contains(&item.seq) {
                return None;
            }
            if !accepts(&item.kind) {
                continue;
            }
            let same = match (item.source.as_ref(), source) {
                (None, None) => true,
                (Some(item_source), Some(source)) => item_source.same_part(source),
                _ => false,
            };
            if same {
                return Some(idx);
            }
        }
        None
    }

    /// Fill the merge metadata a chunk leaves on its entry: a first chunk
    /// stamps the part source, a later chunk of the same part refreshes the
    /// prefix digest, and the markdown lead rides whichever entry first has
    /// room for it.
    fn merge_part_metadata(item: &mut TimelineItem, source: Option<&PartSource>, lead: &mut Option<String>) {
        if item.source.is_none() {
            item.source = source.cloned();
        }
        // A later push of the same part refreshes the entry's prefix digest:
        // it always covers the whole delivered prefix at each push (spec #561,
        // review #569).
        if let (Some(item_source), Some(pushed)) = (item.source.as_mut(), source)
            && item_source.same_part(pushed)
        {
            item_source.prefix_digest = pushed.prefix_digest;
        }
        if item.lead.is_none() {
            item.lead = lead.take();
        }
    }

    /// The merge/chunk walk shared by transcript text and a synthetic lead.
    fn push_text_at_slot(
        &mut self,
        slot: Slot,
        shown_at: Option<i64>,
        source: Option<PartSource>,
        chunk: &str,
        lead: Option<String>,
        position: Option<ReadPosition>,
    ) {
        let key = slot.key;
        let mut target =
            self.source_entry(key, source.as_ref(), |kind| matches!(kind, TimelineKind::Text(_)));
        let max = crate::feishu::card::MAX_CARD_TEXT_CHARS;
        let mut remaining = chunk;
        let mut lead = lead;
        let mut first = Some(slot);
        while !remaining.is_empty() {
            let space = match target.and_then(|idx| self.items.get(idx)) {
                Some(item) => match item.kind() {
                    TimelineKind::Text(last) => max.saturating_sub(last.chars().count()),
                    _ => 0,
                },
                None => 0,
            };
            if space == 0 {
                let take: String = remaining.chars().take(max).collect();
                let pushed = source.clone();
                target = Some(match target {
                    // Beside its own run: another part's equal-key entry may
                    // already follow it.
                    Some(idx) => {
                        let order = self.items.get(idx).map(|item| item.order);
                        let slot = self.slot_after(idx, key);
                        self.seat(Entry {
                            slot,
                            seq: None,
                            order,
                            shown_at,
                            source: pushed,
                            kind: TimelineKind::Text(take.clone()),
                            lead: lead.take(),
                            position,
                        })
                    }
                    None => {
                        let slot = first.take().unwrap_or_else(|| self.slot_for_new(key, position));
                        self.seat(Entry {
                            slot,
                            seq: None,
                            order: None,
                            shown_at,
                            source: pushed,
                            kind: TimelineKind::Text(take.clone()),
                            lead: lead.take(),
                            position,
                        })
                    }
                });
                remaining = &remaining[take.len()..];
                continue;
            }
            let take: String = remaining.chars().take(space).collect();
            let idx = target.expect("space came from it");
            match self.items.get_mut(idx) {
                Some(item) => {
                    if let TimelineKind::Text(last) = &mut item.kind {
                        last.push_str(&take);
                    }
                    Self::merge_part_metadata(item, source.as_ref(), &mut lead);
                }
                None => {
                    let order = self.items.get(idx).map(|item| item.order);
                    let slot = self.slot_after(idx, key);
                    target = Some(self.seat(Entry {
                        slot,
                        seq: None,
                        order,
                        shown_at,
                        source: source.clone(),
                        kind: TimelineKind::Text(take.clone()),
                        lead: lead.take(),
                        position,
                    }));
                }
            }
            remaining = &remaining[take.len()..];
        }
    }

    /// Append a reasoning chunk at an already-resolved slot and read position:
    /// chunks of the same part merge into one panel, a part keyed apart is its
    /// own panel.
    fn push_reasoning_at_slot(
        &mut self,
        slot: Slot,
        shown_at: Option<i64>,
        source: Option<PartSource>,
        chunk: &str,
        mut lead: Option<String>,
        position: Option<ReadPosition>,
    ) {
        let key = slot.key;
        let idx = self.source_entry(key, source.as_ref(), |kind| {
            matches!(kind, TimelineKind::Reasoning(_))
        });
        match idx.and_then(|i| self.items.get_mut(i)) {
            Some(item) => {
                if let TimelineKind::Reasoning(last) = &mut item.kind {
                    last.push_str(chunk);
                }
                Self::merge_part_metadata(item, source.as_ref(), &mut lead);
            }
            None => {
                self.seat(Entry {
                    slot,
                    seq: None,
                    order: None,
                    shown_at,
                    source,
                    kind: TimelineKind::Reasoning(chunk.to_string()),
                    lead,
                    position,
                });
            }
        }
    }

    /// A later sighting of a settled tool may carry the server start time the
    /// first one lacked: the entry keeps its key and gains the clock.
    pub(super) fn adopt_tool_shown_at(&mut self, call_id: &str, shown_at: Option<i64>) {
        if shown_at.is_some()
            && let Some(item) = self
                .items
                .iter_mut()
                .find(|item| matches!(&item.kind, TimelineKind::Tool(id) if id == call_id))
        {
            item.shown_at = item.shown_at.or(shown_at);
        }
    }

    /// Seat one synthetic entry and return the key it actually took, clamped
    /// to the live slice's first key when its position is behind the boundary.
    fn seat_synthetic(
        &mut self,
        key: i64,
        shown_at: Option<i64>,
        source: Option<PartSource>,
        kind: TimelineKind,
    ) -> i64 {
        let slot = self.slot_for_new(key, None);
        let index = self.seat(Entry {
            slot,
            seq: None,
            order: None,
            shown_at,
            source,
            kind,
            lead: None,
            position: None,
        });
        self.items[index].key
    }

    /// Admit one synthetic entry (spec #1040): it inserts directly, never takes
    /// the walk's READ floor (synthetic content has no read position) and never
    /// produces a pushable placement. The returned key is the one the entry
    /// took — clamped to the live slice's first key when the entry's position
    /// is behind it. `None` means the variant owns no single key: a lead may
    /// merge or chunk, and an empty user message renders nothing.
    ///
    /// The MESSAGE floor is the caller's: a synthetic entry admitted inside a
    /// message scope takes that scope's cut ([`Self::synthetic_seated`] via
    /// [`MessageScope::synthetic`], the compaction walk), while a
    /// session-scoped one (a receipt, a ledger row, a wake line) has no message
    /// and keeps its own key. The compaction admission additionally takes its
    /// own `receipt_cut_floor(message_time)`, which is the same cut for the
    /// row's own message and so idempotent.
    pub(super) fn synthetic(&mut self, entry: SyntheticEntry<'_>) -> Option<i64> {
        self.synthetic_seated(entry, None)
    }

    /// [`Self::synthetic`] with the caller's message floor: the scope a
    /// compaction row is admitted under supplies its cut, while the row's
    /// `shown_at` keeps the message's own server time (the cursor frontier's
    /// recorded start, never a display clock).
    fn synthetic_seated(&mut self, entry: SyntheticEntry<'_>, message_floor: Option<i64>) -> Option<i64> {
        match entry {
            SyntheticEntry::Receipt { text, at } => {
                let key = at.unwrap_or_else(|| self.next_order());
                return Some(self.seat_synthetic(key, None, None, TimelineKind::Receipt(text.to_string())));
            }
            SyntheticEntry::Compaction {
                fact,
                at,
                message_time,
                row,
            } => {
                if let Some(index) = self.carried_compaction(&row).map(|(index, _)| index) {
                    self.items[index].kind = TimelineKind::Compaction(fact);
                    return Some(self.items[index].key);
                }
                let key = at.unwrap_or_else(|| self.next_order());
                // The row takes the scope's message floor and the Interaction
                // Receipt cut its own message earns (ADR-0038 rule 4, the floor
                // `render_compactions` used to set around it); a timeless
                // compaction takes neither. The two are the same cut for a row
                // admitted under its own message, so applying both is
                // idempotent.
                let floor = message_floor
                    .into_iter()
                    .chain(message_time.and_then(|created| self.receipt_cut_floor(created)))
                    .max();
                let key = floor.map_or(key, |floor| key.max(floor));
                // `shown_at` is the cursor frontier's recorded start time, not
                // a displayed clock: the card renders the row without one
                // (`with_compaction`), while `frontier_for` reads it into
                // `CursorFrontier.started_at` so a delivered compaction
                // round-trips through the chain record.
                return Some(self.seat_synthetic(key, at, Some(row), TimelineKind::Compaction(fact)));
            }
            SyntheticEntry::Ledger { entry, ending, at } => {
                let key = at.unwrap_or_else(|| self.next_order());
                return Some(self.seat_synthetic(
                    key,
                    None,
                    None,
                    TimelineKind::LedgerEntry(entry, ending.map(Box::new)),
                ));
            }
            SyntheticEntry::UserMessage { text, at } => {
                if text.trim().is_empty() {
                    return None;
                }
                let key = at.unwrap_or_else(|| self.next_order());
                return Some(self.seat_synthetic(
                    key,
                    None,
                    None,
                    TimelineKind::UserMessage(text.to_string()),
                ));
            }
            SyntheticEntry::Lead { text, at } => {
                let key = at.unwrap_or_else(|| self.next_order());
                let slot = self.slot_for_new(key, None);
                // A lead is text today: it may merge into an equal-key run or
                // chunk, so it returns no key. It has no display clock (the
                // text path passes only `with_text`).
                self.push_text_at_slot(slot, None, None, text, None, None);
            }
        }
        None
    }

    /// The live-slice/clamp seat a pre-allocated identity takes, for the
    /// module's own clamp test: no production caller seats an arbitrary
    /// identity outside a settle move.
    #[cfg(test)]
    pub(super) fn seat_for_test(&mut self, key: i64, seq: u64, kind: TimelineKind) -> usize {
        let slot = self.slot_for_seq(key, seq, None);
        self.seat(Entry {
            slot,
            seq: Some(seq),
            order: None,
            shown_at: None,
            source: None,
            kind,
            lead: None,
            position: None,
        })
    }

    /// The Compaction row the timeline carries at `source`'s position, whatever
    /// its key or revision (ADR-0082): one scan for the in-place update and the
    /// dedup read.
    fn carried_compaction(&self, source: &PartSource) -> Option<(usize, &Compaction)> {
        self.items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match (&item.kind, item.source.as_ref()) {
                (TimelineKind::Compaction(fact), Some(item_source)) if item_source.same_part(source) => {
                    Some((index, fact))
                }
                _ => None,
            })
    }

    /// The Compaction row the timeline carries at `source`'s position.
    pub(super) fn compaction_row(&self, source: &PartSource) -> Option<&Compaction> {
        self.carried_compaction(source).map(|(_, fact)| fact)
    }

    /// The part characters this timeline delivers for `source` (spec #561,
    /// review #569): the un-rendered prefix a first entry's `delivered_before`
    /// carries, plus every same-source entry's own model characters. Only a
    /// part's FIRST entry may carry a nonzero offset, so it is counted once.
    pub(super) fn source_extent_in(&self, source: &PartSource, end: usize) -> usize {
        self.source_chars_in(source, end, Some(crate::feishu::card::REASONING_TEXT_CAP))
    }

    /// The part CONTENT pushed for `source`, counted from the part's start —
    /// the seeded cut's unit (issue #1025). No display cap applies.
    pub(super) fn source_held_chars(&self, source: &PartSource) -> usize {
        self.source_chars_in(source, self.items.len(), None)
    }

    /// The one walk behind [`Self::source_extent_in`] and
    /// [`Self::source_held_chars`], so the two readings cannot drift.
    fn source_chars_in(&self, source: &PartSource, end: usize, reasoning_cap: Option<usize>) -> usize {
        let mut chars = 0usize;
        let mut offset = 0usize;
        for item in &self.items[..end] {
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
                chars += match (&item.kind, reasoning_cap) {
                    (TimelineKind::Reasoning(_), Some(cap)) => text.chars().count().min(cap),
                    _ => text.chars().count(),
                };
            }
        }
        offset + chars
    }

    /// Stamp the delivered-prefix digest a seeded push owes (issue #1025).
    /// Returns whether an entry was found to stamp.
    pub(super) fn stamp_source_digest(&mut self, source: &PartSource, digest: u64) -> bool {
        if let Some(item_source) = self
            .items
            .iter_mut()
            .rev()
            .find(|item| item.source.as_ref().is_some_and(|s| s.same_part(source)))
            .and_then(|item| item.source.as_mut())
        {
            item_source.set_prefix_digest(digest);
            true
        } else {
            false
        }
    }

    /// Whether any entry names the part `source` (spec #561, review #569).
    pub(super) fn has_source(&self, source: &PartSource) -> bool {
        self.items
            .iter()
            .any(|item| item.source.as_ref().is_some_and(|s| s.same_part(source)))
    }

    /// The prefix offset the part's entries carry (spec #561's seeded cut).
    pub(super) fn source_delivered_before(&self, source: &PartSource) -> usize {
        self.items
            .iter()
            .filter(|item| item.source.as_ref().is_some_and(|s| s.same_part(source)))
            .filter_map(|item| item.source.as_ref().map(|s| s.delivered_before))
            .max()
            .unwrap_or(0)
    }

    /// The prefix digest of `source`'s newest entry — the one whose push last
    /// covered the part's whole delivered extent (spec #561, review #569).
    pub(super) fn source_digest(&self, source: &PartSource) -> Option<u64> {
        self.items
            .iter()
            .rev()
            .find(|item| item.source.as_ref().is_some_and(|s| s.same_part(source)))
            .and_then(|item| item.source.as_ref().and_then(|s| s.prefix_digest))
    }

    /// The part content this timeline already shows for `source` — the
    /// concatenation of its entries, in order — when that content is a plain
    /// prefix of the part (every entry offset-free).
    pub(super) fn source_rendered(&self, source: &PartSource) -> Option<String> {
        let mut rendered = String::new();
        let mut found = false;
        for item in &self.items {
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

    /// Drop every entry of `source` (spec #561, review #569), keeping
    /// `render_from` on the same logical position: entries before the finalized
    /// boundary belong to sent cards, and a rewritten part's entries after it
    /// are the live render's own.
    pub(super) fn drop_source_run(&mut self, source: &PartSource) {
        let boundary = self.render_from.min(self.items.len());
        let removed_before = self.items[..boundary]
            .iter()
            .filter(|item| item.source.as_ref().is_some_and(|s| s.same_part(source)))
            .count();
        self.items
            .retain(|item| !item.source.as_ref().is_some_and(|s| s.same_part(source)));
        self.render_from = self.render_from.saturating_sub(removed_before);
    }

    /// Re-key a live tail with the server start time the first sighting lacked
    /// (ADR-0045), never below the floor the pin carried (spec #1040).
    pub(super) fn rekey(&mut self, tail: &mut TailPlacement, started_at: Option<i64>) {
        if let Some(at) = started_at {
            tail.key = tail.floor.map_or(at, |floor| at.max(floor));
            tail.shown_at = tail.shown_at.or(Some(at));
        }
    }

    /// Settle a pinned live tail into the timeline (spec #1040): re-apply the
    /// CURRENT receipt cut for the tail's message and the CURRENT read floor
    /// for it — the earlier messages' placed entries and pinned tails, plus the
    /// panel's own preceding ones — then seat it at `max(current, floor)`,
    /// capped at the minimum key among the entries that follow it in read order
    /// at or above the floor. Placed content never moves and `seq` stays
    /// identity. `_panel` keeps #1040's `settle(tail, panel, …)` signature;
    /// the panel body stays in the accumulator's `tools` map and the Timeline
    /// never reads it.
    pub(super) fn settle(
        &mut self,
        tail: TailPlacement,
        _panel: &ToolPanel,
        read_order: &HashMap<&str, usize>,
        pinned_tails: &[TailFact],
    ) {
        let receipt = tail.created.and_then(|created| self.receipt_cut_floor(created));
        let read_floor = self.read_floor_for_tail(&tail, read_order, pinned_tails);
        let floor = tail.floor.into_iter().chain(receipt).chain(read_floor).max();
        let key = floor.map_or(tail.key, |floor| tail.key.max(floor));
        let cap = self.min_following_key(tail.position, floor.unwrap_or(i64::MIN), read_order);
        let key = cap.map_or(key, |cap| key.min(cap));
        let slot = self.slot_for_seq(key, tail.seq, tail.position);
        self.seat(Entry {
            slot,
            seq: Some(tail.seq),
            order: None,
            shown_at: tail.shown_at,
            source: tail.source,
            kind: TimelineKind::Tool(tail.call_id),
            lead: None,
            position: tail.position,
        });
    }

    /// The floor a placed part takes from the CURRENT message scope: the
    /// message's receipt cut and the walk's read floor, whichever is higher.
    fn current_floor(&self) -> Option<i64> {
        self.message_floor.into_iter().chain(self.read_floor).max()
    }

    /// The current read floor for a tail's message (spec #1040): every entry
    /// that precedes the tail in read order — an earlier message's, or an
    /// earlier part of the same message — and every pinned tail that precedes
    /// it, the panel's own preceding ones included.
    fn read_floor_for_tail(
        &self,
        tail: &TailPlacement,
        read_order: &HashMap<&str, usize>,
        pinned_tails: &[TailFact],
    ) -> Option<i64> {
        let tail_position = tail.position?;
        let mut floor: Option<i64> = None;
        for item in &self.items {
            let Some(position) = item_read_position(item, read_order) else {
                continue;
            };
            if position < tail_position {
                floor = Some(floor.map_or(item.key, |floor| floor.max(item.key)));
            }
        }
        for fact in pinned_tails {
            let Some(position) = read_order
                .get(fact.message.as_str())
                .copied()
                .map(|message| (message, fact.part))
            else {
                continue;
            };
            if position < tail_position {
                floor = Some(floor.map_or(fact.key, |floor| floor.max(fact.key)));
            }
        }
        floor
    }

    /// The minimum key among the entries that follow `position` in read order
    /// at or above `floor` — the settle clamp's cap (spec #1040). After a tool
    /// those keys need not be monotonic, so it is a minimum, never the first.
    fn min_following_key(
        &self,
        position: Option<ReadPosition>,
        floor: i64,
        read_order: &HashMap<&str, usize>,
    ) -> Option<i64> {
        let position = position?;
        self.items
            .iter()
            .filter_map(|item| {
                let item_position = item_read_position(item, read_order)?;
                (item_position > position && item.key >= floor).then_some(item.key)
            })
            .min()
    }
}

impl<'a, 'r> Walk<'a, 'r> {
    /// Scope the walk to one message (spec #1040): apply the message's receipt
    /// cut, and the read floor made of earlier messages' placed entries plus
    /// their pinned tails. The scope's `Drop` clears the message floor.
    pub(super) fn message<'s>(
        &'s mut self,
        created: Option<i64>,
        id: &MessageId,
        pinned_tail_keys: &[TailFact],
    ) -> MessageScope<'s> {
        let position = self.read_order.get(id.as_str()).copied();
        let read_floor = position.and_then(|position| {
            let mut floor: Option<i64> = None;
            for item in &self.timeline.items {
                let Some(item_position) = item_read_position(item, self.read_order) else {
                    continue;
                };
                if item_position.0 < position {
                    floor = Some(floor.map_or(item.key, |floor| floor.max(item.key)));
                }
            }
            for fact in pinned_tail_keys {
                if self
                    .read_order
                    .get(fact.message.as_str())
                    .is_some_and(|tail_position| *tail_position < position)
                {
                    floor = Some(floor.map_or(fact.key, |floor| floor.max(fact.key)));
                }
            }
            floor
        });
        self.timeline.read_floor = read_floor;
        self.timeline.message_floor = created.and_then(|created| self.timeline.receipt_cut_floor(created));
        self.timeline.scope_generation = self.timeline.scope_generation.saturating_add(1);
        let generation = self.timeline.scope_generation;
        MessageScope {
            timeline: self.timeline,
            generation,
            position,
            message: id.clone(),
            created,
        }
    }
}

impl MessageScope<'_> {
    /// Place one transcript part (spec #1040): apply the message's receipt
    /// cut, the walk's read floor, placed-part stability and the live-slice
    /// clamp, then hand back the linear token `push` consumes.
    pub(super) fn place(&mut self, facts: PartFacts) -> Placement {
        let position = facts
            .source
            .as_ref()
            .and_then(|_| self.position.map(|message| (message, facts.index)));
        let floor = self.timeline.current_floor();
        let key = match facts
            .source
            .as_ref()
            .and_then(|source| self.timeline.placed_source_key(source))
        {
            // A part already on the live slice keeps its key across reads.
            Some(key) => key,
            None => {
                let raw = facts.key_at.unwrap_or_else(|| self.timeline.next_order());
                floor.map_or(raw, |floor| raw.max(floor))
            }
        };
        let slot = self.timeline.slot_for_new(key, position);
        Placement {
            key: slot.key,
            source: facts.source,
            shown_at: facts.shown_at,
            floor,
            expected: facts.form,
            generation: self.generation,
            position,
        }
    }

    /// Consume a placement and insert (or merge) its content (spec #1040).
    /// Validates completely before mutating: a stale token or a kind mismatch
    /// inserts nothing.
    pub(super) fn push(&mut self, placement: Placement, kind: TranscriptKind) -> Result<(), PlacementError> {
        self.validate(&placement, &kind)?;
        let position = placement.position;
        let key = placement.key;
        let source = placement.source;
        let shown_at = placement.shown_at;
        match kind {
            TranscriptKind::Text { chunk, lead } => {
                let slot = self.timeline.slot_for_new(key, position);
                self.timeline
                    .push_text_at_slot(slot, shown_at, source, &chunk, lead, position);
            }
            TranscriptKind::Reasoning { chunk, lead } => {
                let slot = self.timeline.slot_for_new(key, position);
                self.timeline
                    .push_reasoning_at_slot(slot, shown_at, source, &chunk, lead, position);
            }
            TranscriptKind::Tool(call_id) => {
                let slot = self.timeline.slot_for_new(key, position);
                self.timeline.seat(Entry {
                    slot,
                    seq: None,
                    order: None,
                    shown_at,
                    source,
                    kind: TimelineKind::Tool(call_id),
                    lead: None,
                    position,
                });
            }
        }
        Ok(())
    }

    /// Replace a part's entry run with fresh content (spec #561, review #569):
    /// the token must name the same part, and the old run is dropped across the
    /// live-slice boundary before the fresh one is inserted under the token.
    /// #1044 owns the public replace path; this keeps the turn walk's rewrite
    /// correct and validates everything — generation, kind and source — before
    /// mutating anything.
    pub(super) fn replace(
        &mut self,
        mut placement: Placement,
        source: &PartSource,
        kind: TranscriptKind,
    ) -> Result<(), PlacementError> {
        self.validate(&placement, &kind)?;
        let Some(placement_source) = placement.source.as_ref() else {
            return Err(PlacementError::SourceMismatch);
        };
        if !placement_source.same_part(source) {
            return Err(PlacementError::SourceMismatch);
        }
        // A rewrite re-floors at its placement key exactly as the pre-#1040
        // replace did: `place` applied the message and read floor before the
        // run dropped, and this explicit `max` restores the same floor after
        // it — `push` applies no floor of its own, it seats at the key it is
        // given. Parity, not a tightening.
        if let Some(floor) = placement.floor {
            placement.key = placement.key.max(floor);
        }
        self.timeline.drop_source_run(source);
        self.push(placement, kind)
    }

    /// The checks shared by `push` and `replace` (spec #1040): nothing may be
    /// mutated until the token's scope and the content's kind both agree.
    fn validate(&self, placement: &Placement, kind: &TranscriptKind) -> Result<(), PlacementError> {
        if placement.generation != self.generation {
            return Err(PlacementError::Stale);
        }
        if placement.expected != kind.form() {
            return Err(PlacementError::KindMismatch);
        }
        Ok(())
    }

    /// Pin a live tool tail (spec #1040): the key, display clock, merge
    /// identity, the floor it was placed under, the owning message and part,
    /// the call and the pinned `seq`. It never enters `push`.
    pub(super) fn pin(&mut self, facts: PartFacts, call_id: String) -> TailPlacement {
        let key = facts.key_at.unwrap_or_else(|| self.timeline.next_order());
        let floor = self.timeline.current_floor();
        let seq = self.timeline.allocate_seq();
        TailPlacement {
            key,
            shown_at: facts.shown_at,
            source: facts.source,
            floor,
            message: Some(self.message.clone()),
            created: self.created,
            position: self.position.map(|message| (message, facts.index)),
            call_id,
            seq,
        }
    }

    /// Admit one synthetic entry inside this message scope (spec #1043).
    ///
    /// A compaction row takes the scope's **message** floor before the
    /// admission seats it: the scope's receipt cut and the admission's own
    /// `receipt_cut_floor(message_time)` are the same cut when the row's
    /// message is the one the scope names, so applying both is idempotent —
    /// but the scope's floor, not a caller's, is what makes the row follow a
    /// click. Other variants keep their own keying (the admission takes no
    /// read or message floor for them).
    pub(super) fn synthetic(&mut self, entry: SyntheticEntry<'_>) -> Option<i64> {
        let floor = self.timeline.message_floor;
        self.timeline.synthetic_seated(entry, floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        Compaction, CompactionReason, CompactionStatus, MessageId, MessageRole, MessageTime,
        SessionTranscript, ToolStatus, TranscriptMessage,
    };
    use crate::bridge::chain::{CursorFrontier, CursorPartKind, cursor_prefix_digest};
    use crate::bridge::turn::state::{
        CursorSeed, InteractionBlock, PendingPermission, PendingQuestion, StreamAccumulator,
    };
    use crate::feishu::card::MAX_CARD_TEXT_CHARS;
    use crate::feishu::card::ledger::{TaskCompletionEntry, TaskEnding, TaskKind};
    use crate::feishu::card::tool_render::ToolPanel;

    /// Push one text part through a fresh message scope: `source` names the
    /// transcript part (and its index), `None` a scripted source-less chunk.
    /// The guard and rewrite tests read the entry back from the timeline.
    fn push_text(
        timeline: &mut Timeline,
        order: &HashMap<&str, usize>,
        message: &str,
        key: i64,
        source: Option<PartSource>,
        chunk: &str,
    ) {
        let id = MessageId::new(message);
        let index = source.as_ref().map_or(0, |source| source.index());
        let mut walk = timeline.walk(order);
        let mut scope = walk.message(None, &id, &[]);
        let facts = PartFacts {
            form: PartForm::Text,
            key_at: Some(key),
            shown_at: Some(key),
            source,
            index,
        };
        let placement = scope.place(facts);
        scope
            .push(
                placement,
                TranscriptKind::Text {
                    chunk: chunk.into(),
                    lead: None,
                },
            )
            .expect("the scope accepts its own token");
    }

    /// `push_text` chunks one large blob into bounded timeline items so a single
    /// item never exceeds the per-card text budget.
    #[test]
    fn push_text_chunks_long_blobs() {
        let mut acc = StreamAccumulator::new("test");
        let long = "字".repeat(MAX_CARD_TEXT_CHARS * 2 + 100);
        acc.push_text(&long);

        let text_items: Vec<&String> = acc
            .timeline()
            .iter()
            .filter_map(|item| match item.kind() {
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
        assert!(acc.timeline().is_empty(), "a running panel is not history yet");
        let running_card = acc.build_card();
        assert_eq!(panel_ids(&running_card).len(), 1, "{running_card}");

        acc.push_tool("call_bash", bash(ToolStatus::Completed, Some("done")));
        assert_eq!(acc.timeline().len(), 1, "the settled panel joins the timeline");
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

    /// The render-boundary clamp puts a late entry at the top of the live slice
    /// and lowers only its ORDER, never the identity `seq` a panel's element id
    /// is built from (ADR-0040; issue #1022).
    #[test]
    fn a_clamped_entry_keeps_its_identity_seq() {
        let mut acc = StreamAccumulator::new("test");
        acc.push_text_at(Some(1_000), "前");
        acc.push_text_at(Some(2_000), "后");
        // Finalize the first entry: the live slice starts at the second, and an
        // entry keyed behind it must clamp to the slice's first key.
        acc.skip_rejected_slice(0, 1);
        assert_eq!(acc.render_from(), 1);

        // An older identity (7) with a key behind the slice.
        acc.test_seat_identity(500, 7, TimelineKind::Text("迟到".into()));
        let clamped = &acc.timeline().entries()[acc.render_from()];
        assert_eq!(clamped.seq(), 7, "the identity is never renumbered");
        assert!(
            clamped.order() < acc.timeline().entries()[acc.render_from() + 1].order(),
            "the order sorts it ahead of the equal-key run"
        );
        assert_eq!(
            clamped.key(),
            acc.timeline().entries()[acc.render_from() + 1].key()
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

    /// Issue #1023: a block surfaced by a read that saw the asking tool call
    /// carries its server time. The receipt records it as a cut, so a part the
    /// server timestamped AFTER the ask — written after the click, whatever
    /// cola's clock says — lands below the receipt even though its own server
    /// key is far behind the receipt's `next_order` key.
    #[test]
    fn a_post_click_part_lands_below_the_receipt_that_carries_an_ask_time() {
        let question = |asked_at: Option<i64>| {
            InteractionBlock::Question(PendingQuestion {
                request_id: "q".into(),
                session_id: "s".into(),
                title: String::new(),
                questions: vec![crate::backend::types::QuestionInfo {
                    question: "继续?".into(),
                    header: "继续".into(),
                    ..Default::default()
                }],
                uncollectable: None,
                directory: "/w".into(),
                answers: vec![None],
                done: vec![false],
                open: 0,
                drafts: vec![String::new()],
                asked_at,
            })
        };

        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(question(Some(5_000)));
        assert!(acc.resolve_interaction("q", |_| "✅ 已回答".to_string(), true));
        assert!(
            acc.receipt_cut_floor(5_100).is_some(),
            "content after the ask earns a floor"
        );

        // The server's post-click continuation: its own time is 5_100, but the
        // receipt's cola-clock key is ~now, so without the cut it would sort
        // ABOVE the receipt (ADR-0038 rule 4 forbids that). The floor is the
        // message's: a continuation message created after the ask.
        acc.script_message(Some(5_100));
        acc.push_text_at(Some(5_100), "continuation");
        acc.script_message(None);
        let card = acc.build_card().to_string();
        assert!(
            card.find("✅ 已回答").unwrap() < card.find("continuation").unwrap(),
            "post-click content must follow the receipt: {card}"
        );

        // Content written BEFORE the ask keeps the position its key earns,
        // above the receipt — the cut only floors what came after it.
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(question(Some(5_000)));
        assert!(acc.resolve_interaction("q", |_| "✅ 已回答".to_string(), true));
        acc.push_text_at(Some(4_900), "explanation");
        let card = acc.build_card().to_string();
        assert!(
            card.find("explanation").unwrap() < card.find("✅ 已回答").unwrap(),
            "pre-click content stays above the receipt: {card}"
        );

        // A part longer than one card entry is chunked: every continuation
        // chunk is floored too, so the timeline's key order cannot break under
        // the receipt (issue #1023).
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(question(Some(5_000)));
        assert!(acc.resolve_interaction("q", |_| "✅ 已回答".to_string(), true));
        acc.script_message(Some(5_100));
        acc.push_text_at(
            Some(5_100),
            &"长".repeat(crate::feishu::card::MAX_CARD_TEXT_CHARS + 10),
        );
        acc.script_message(None);
        assert!(
            acc.timeline()
                .entries()
                .windows(2)
                .all(|pair| pair[0].key() <= pair[1].key()),
            "the floored chunks keep the timeline key-sorted"
        );
        let card = acc.build_card().to_string();
        assert!(
            card.find("✅ 已回答").unwrap() < card.find("长长长").unwrap(),
            "every chunk stays below the receipt: {card}"
        );

        // A render poll can deliver the continuation in the gap between the
        // backend reply and the receipt's write, so it is already placed when
        // the receipt lands. On a LOCAL click the receipt's key is reserved
        // first, so the gap content is floored above it and the receipt lands
        // below — without moving anything placed before the click.
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(question(Some(5_000)));
        assert!(
            acc.reserve_receipt("q").is_some(),
            "the click reserves the receipt key"
        );
        acc.script_message(Some(5_100));
        acc.push_text_at(Some(5_100), "竞态到达的继续");
        acc.script_message(None);
        assert!(acc.resolve_interaction("q", |_| "✅ 已回答".to_string(), true));
        let card = acc.build_card().to_string();
        assert!(
            card.find("✅ 已回答").unwrap() < card.find("竞态到达的继续").unwrap(),
            "gap content follows the reserved receipt: {card}"
        );

        // Without a reservation (a remote client's answer, noticed by a sweep)
        // already-placed content keeps its position — it may have been visible
        // before the remote click, and there is no click time to re-floor
        // against (the protocol limit). Future content is cut: the click
        // precedes the receipt, so anything server-stamped later follows it.
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(question(Some(5_000)));
        acc.push_text_at(Some(5_100), "远端作答前已可见");
        assert!(acc.resolve_interaction("q", |_| "✅ 已回答".to_string(), true));
        acc.script_message(Some(5_200));
        acc.push_text_at(Some(5_200), "回执之后的内容");
        acc.script_message(None);
        let card = acc.build_card().to_string();
        assert!(
            card.find("远端作答前已可见").unwrap() < card.find("✅ 已回答").unwrap()
                && card.find("✅ 已回答").unwrap() < card.find("回执之后的内容").unwrap(),
            "placed content keeps its position; later content follows the receipt: {card}"
        );

        // After a card split the live slice starts past the receipt's key: the
        // first chunk is clamped to the slice's first key, and the next chunk
        // must look the part up at that SAME clamped key (otherwise it starts
        // a second entry — a duplicated text run or a new reasoning panel).
        let mut acc = StreamAccumulator::new("test");
        acc.push_receipt_at(Some(1_000), "✅ 已回答");
        acc.push_text_at(Some(5_000), "已发送卡片");
        let head = PartSource::at(MessageId::new("msg_head"), 0);
        acc.push_text_at_key(Some(9_000), Some(9_000), Some(head), "新切片", None);
        // Move the live slice past both pre-split entries WITHOUT sealing them:
        // the merge lookup must cross an unsealed boundary.
        acc.test_set_render_from(2);
        acc.record_receipt_cut(500, 1_000);
        let part = PartSource::at(MessageId::new("msg_part"), 0);
        acc.push_text_at_key(Some(3_000), Some(3_000), Some(part.clone()), "甲", None);
        acc.push_text_at_key(Some(3_000), Some(3_000), Some(part), "乙", None);
        acc.script_message(None);
        let texts: Vec<&str> = acc
            .timeline()
            .iter()
            .filter_map(|item| match item.kind() {
                TimelineKind::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            texts.contains(&"甲乙") && !texts.contains(&"乙"),
            "the clamped slice key is used by the merge lookup: {texts:?}"
        );

        // A PARALLEL part in the asking message: it started after the ask
        // (2000 > 1000) and is delivered after the click, but its message was
        // created before the click (900), so the Host could have seen it. The
        // message-scoped floor leaves it above the receipt.
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(question(Some(1_000)));
        assert!(
            acc.reserve_receipt("q").is_some(),
            "the click reserves the receipt key"
        );
        acc.script_message(Some(900));
        acc.push_text_at(Some(2_000), "并行工具的结果");
        acc.script_message(None);
        assert!(acc.resolve_interaction("q", |_| "✅ 已回答".to_string(), true));
        let card = acc.build_card().to_string();
        assert!(
            card.find("并行工具的结果").unwrap() < card.find("✅ 已回答").unwrap(),
            "pre-click parallel work stays above the receipt: {card}"
        );

        // A part that SPANS the cut: it started before the click in a pre-click
        // message, rendered once, then the click happens and it grows. Every
        // chunk keeps the part's own entry (no floor, no second panel).
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(question(Some(1_000)));
        acc.script_message(Some(900));
        acc.push_text_at(Some(2_000), "跨点击的");
        assert!(
            acc.reserve_receipt("q").is_some(),
            "the click reserves the receipt key"
        );
        acc.push_text_at(Some(2_000), "继续增长");
        acc.script_message(None);
        assert!(acc.resolve_interaction("q", |_| "✅ 已回答".to_string(), true));
        let texts: Vec<&str> = acc
            .timeline()
            .iter()
            .filter_map(|item| match item.kind() {
                TimelineKind::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["跨点击的继续增长"], "one entry across the cut");
        let card = acc.build_card().to_string();
        assert!(
            card.find("跨点击的继续增长").unwrap() < card.find("✅ 已回答").unwrap(),
            "the spanning part stays above the receipt: {card}"
        );

        // The SAME part arriving across two render reads still merges into one
        // entry: the merge lookup uses the floored key too (a raw-key lookup
        // would start a second entry — a new reasoning panel, a split text run).
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(question(Some(5_000)));
        assert!(acc.resolve_interaction("q", |_| "✅ 已回答".to_string(), true));
        acc.script_message(Some(5_100));
        acc.push_text_at(Some(5_100), "第一段");
        acc.push_text_at(Some(5_100), "第二段");
        acc.script_message(None);
        acc.script_message(Some(5_200));
        acc.push_reasoning_at(Some(5_200), "想一");
        acc.push_reasoning_at(Some(5_200), "想二");
        acc.script_message(None);
        let texts: Vec<&str> = acc
            .timeline()
            .iter()
            .filter_map(|item| match item.kind() {
                TimelineKind::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let reasoning: Vec<&str> = acc
            .timeline()
            .iter()
            .filter_map(|item| match item.kind() {
                TimelineKind::Reasoning(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["第一段第二段"], "one text entry per part");
        assert_eq!(reasoning, vec!["想一想二"], "one reasoning panel per part");
    }

    /// Issues can be asked in one order and answered in another (issue #1023):
    /// Q1 asked at 1000, Q2 at 2000, Q2 answered first, Q1 second. Content the
    /// server timestamped between the asks (2100) is after both asks, so it is
    /// post-click content for BOTH receipts and must clear the one the Host
    /// clicked last. One "latest-asked" cut would floor it only to Q2's key,
    /// leaving it between the two receipts — above Q1's receipt, which the last
    /// click wrote.
    #[test]
    fn a_later_asked_but_earlier_answered_receipt_still_cuts() {
        let question = |id: &str, asked_at: i64| {
            InteractionBlock::Question(PendingQuestion {
                request_id: id.into(),
                session_id: "s".into(),
                title: String::new(),
                questions: vec![crate::backend::types::QuestionInfo {
                    question: "继续?".into(),
                    header: "继续".into(),
                    ..Default::default()
                }],
                uncollectable: None,
                directory: "/w".into(),
                answers: vec![None],
                done: vec![false],
                open: 0,
                drafts: vec![String::new()],
                asked_at: Some(asked_at),
            })
        };
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(question("q1", 1_000));
        acc.add_interaction(question("q2", 2_000));
        assert!(acc.resolve_interaction("q2", |_| "✅ 已回答 Q2".to_string(), true));
        assert!(acc.resolve_interaction("q1", |_| "✅ 已回答 Q1".to_string(), true));
        acc.script_message(Some(2_100));
        acc.push_text_at(Some(2_100), "两次提问之后写出的内容");
        acc.script_message(None);
        let card = acc.build_card().to_string();
        let q2 = card.find("✅ 已回答 Q2").unwrap();
        let q1 = card.find("✅ 已回答 Q1").unwrap();
        let after = card.find("两次提问之后写出的内容").unwrap();
        assert!(
            q2 < q1 && q1 < after,
            "the content clears both receipts, the last click included: {card}"
        );
    }

    /// A permission's remote (or unreserved) receipt cuts a new MESSAGE after
    /// the answer, while parallel work in the asking message (created before
    /// the ask) stays above it (issue #1023).
    #[test]
    fn a_permission_receipt_cuts_post_click_messages_only() {
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(InteractionBlock::Permission(PendingPermission {
            session_id: "s".into(),
            request_id: "p".into(),
            body: String::new(),
            target: "⚡ ls".into(),
            directory: "/w".into(),
            always_supported: true,
            summary_digest: None,
            asked_at: Some(1_000),
        }));
        // A parallel part delivered late, in the ASKING message (created 900).
        acc.script_message(Some(900));
        acc.push_text_at(Some(2_000), "并行工具的结果");
        acc.script_message(None);
        assert!(acc.resolve_interaction("p", |_| "✅ 已允许".to_string(), false));
        acc.script_message(Some(1_500));
        acc.push_text_at(Some(1_500), "答复后的继续");
        acc.script_message(None);
        let card = acc.build_card().to_string();
        assert!(
            card.find("并行工具的结果").unwrap() < card.find("✅ 已允许").unwrap()
                && card.find("✅ 已允许").unwrap() < card.find("答复后的继续").unwrap(),
            "message-scoped: parallel pre-click work above, new message below: {card}"
        );
    }

    /// V2 decodes EVERY text part with no start time, and the server can grow
    /// a part across reads. A placed part keeps its entry: the suffix merges
    /// above the receipt it was rendered before, instead of starting a second
    /// entry at a fresh key below (issue #1023).
    #[test]
    fn an_untimed_part_that_grows_after_a_receipt_keeps_its_entry() {
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(InteractionBlock::Question(PendingQuestion {
            request_id: "q".into(),
            session_id: "s".into(),
            title: String::new(),
            questions: vec![crate::backend::types::QuestionInfo {
                question: "继续?".into(),
                header: "继续".into(),
                ..Default::default()
            }],
            uncollectable: None,
            directory: "/w".into(),
            answers: vec![None],
            done: vec![false],
            open: 0,
            drafts: vec![String::new()],
            asked_at: Some(5_000),
        }));
        let part = PartSource::at(MessageId::new("msg_v2"), 0);
        acc.push_text_at_key(None, None, Some(part.clone()), "第一段", None);
        assert!(acc.resolve_interaction("q", |_| "✅ 已回答".to_string(), true));
        acc.push_text_at_key(None, None, Some(part), "继续", None);
        let texts: Vec<&str> = acc
            .timeline()
            .iter()
            .filter_map(|item| match item.kind() {
                TimelineKind::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["第一段继续"], "the part keeps one entry");
        let card = acc.build_card().to_string();
        assert!(
            card.find("第一段继续").unwrap() < card.find("✅ 已回答").unwrap(),
            "the grown part stays above the receipt it preceded: {card}"
        );
    }

    /// A block surfaced without a read (a re-host, a busy-follow seed, a
    /// restart re-adoption) carries no anchor, so its receipt records no cut
    /// and nothing a later read delivers is re-placed — the old behaviour.
    #[test]
    fn a_block_without_an_ask_time_records_no_cut() {
        let mut acc = StreamAccumulator::new("test");
        acc.add_interaction(InteractionBlock::Permission(PendingPermission {
            session_id: "s".into(),
            request_id: "p".into(),
            body: String::new(),
            target: "⚡ ls".into(),
            directory: "/w".into(),
            always_supported: true,
            summary_digest: None,
            asked_at: None,
        }));
        assert!(acc.resolve_interaction("p", |_| "✅ 已允许".to_string(), true));
        assert!(acc.timeline().receipt_cuts().is_empty());
        assert_eq!(acc.receipt_cut_floor(1), None);
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
        acc.push_text_at_key(Some(2_000), Some(2_000), Some(source(0, first)), first, None);
        acc.push_text_at_key(Some(2_000), Some(2_000), Some(source(1, second)), second, None);

        assert_eq!(
            acc.timeline()
                .iter()
                .map(|item| item.source().map(|source| source.index()))
                .collect::<Vec<_>>(),
            vec![Some(0), Some(1)],
            "each part keeps its own entry and source: {:?}",
            acc.timeline()
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

    /// The scripted seam keeps the timeline ORDER key apart from the clock the
    /// entry displays (issue #1022): `key_at` positions the entry and
    /// `shown_at` is what the card may show, so a swap must fail.
    #[test]
    fn the_scripted_seam_keeps_the_order_key_apart_from_the_displayed_clock() {
        let mut acc = StreamAccumulator::new("test");
        acc.push_text_at_key(Some(1_000), Some(7_000), None, "有序无钟", None);

        let entry = acc.timeline().entries().last().expect("one entry");
        assert_eq!(entry.key(), 1_000, "the order key is `key_at`");
        assert_eq!(entry.shown_at(), Some(7_000), "the displayed clock is `shown_at`");
    }

    /// The seam's `lead` rides the entry, rendered but never counted as the
    /// part's own characters (spec #561's cut tail): the reply text stays the
    /// chunk alone.
    #[test]
    fn the_scripted_seam_keeps_the_lead_on_the_entry_uncounted() {
        let mut acc = StreamAccumulator::new("test");
        acc.push_text_at_key(Some(1_000), Some(1_000), None, "正文", Some("```\n".to_string()));

        let entry = acc.timeline().entries().last().expect("one entry");
        assert_eq!(entry.lead(), Some("```\n"), "the lead rides the entry");
        assert!(
            matches!(entry.kind(), TimelineKind::Text(text) if text == "正文"),
            "the counted content is the chunk alone: {:?}",
            entry.kind()
        );
        assert_eq!(acc.text(), "正文", "the lead is not part of the reply text");
    }

    /// A chunk that arrives AFTER an abandonment must not merge into the
    /// skipped entry (issue #886 review): it starts a new entry the
    /// continuation renders.
    #[test]
    fn a_chunk_after_an_abandonment_starts_a_new_entry() {
        let mut acc = StreamAccumulator::new("test");
        acc.push_text_at(Some(1_000), "被拒绝的内容");
        acc.skip_rejected_slice(0, 1);
        acc.push_text_at(Some(1_000), "后来的内容");
        assert_eq!(
            acc.timeline().len(),
            2,
            "the chunk did not merge into the skipped entry"
        );
        let card = acc.build_card_with_info().card.to_string();
        assert!(
            card.contains("后来的内容"),
            "the continuation renders the new chunk: {card}"
        );
        assert!(
            !card.contains("被拒绝的内容"),
            "the skipped entry stays off the card: {card}"
        );
    }

    /// The synthetic admission (spec #1040): every kind inserts directly, the
    /// compaction row takes its message's receipt cut and updates in place by
    /// its `PartSource`, a receipt behind the live slice returns the clamped
    /// key, and a lead merges as text without owning a key.
    #[test]
    fn synthetic_admission_takes_cuts_merges_and_returns_the_clamped_key() {
        let compaction = |summary: &str| Compaction {
            status: CompactionStatus::Completed,
            reason: CompactionReason::Auto,
            summary: summary.to_string(),
            usage: None,
            error: None,
            recent: String::new(),
        };
        let mut acc = StreamAccumulator::new("test");
        acc.record_receipt_cut(1_000, 9_000);
        let source = PartSource::at(MessageId::new("msg_comp"), 0);
        acc.push_compaction_at(Some(2_000), source.clone(), &compaction("old"));
        let row = acc.timeline().entries()[0].clone();
        assert_eq!(row.key(), 9_000, "the row takes its message's receipt cut");
        assert_eq!(
            row.shown_at(),
            Some(2_000),
            "the compaction keeps its start time for the cursor frontier; the row displays no clock"
        );

        acc.push_compaction_at(Some(2_000), source, &compaction("new"));
        assert_eq!(acc.timeline().len(), 1, "the repeated row updates in place");
        let TimelineKind::Compaction(fact) = acc.timeline().entries()[0].kind() else {
            panic!("the row is a compaction");
        };
        assert_eq!(fact.summary, "new");

        // A receipt keyed behind the live slice clamps to the slice's first key
        // and returns the clamped key.
        let mut acc = StreamAccumulator::new("test");
        acc.push_text_at(Some(1_000), "sent");
        acc.push_text_at(Some(2_000), "live");
        acc.skip_rejected_slice(0, 1);
        assert_eq!(
            acc.push_receipt_at(Some(500), "late"),
            2_000,
            "the receipt lands at the live slice's first key"
        );

        // A lead owns no key: it merges into one text entry like any text.
        let mut acc = StreamAccumulator::new("test");
        acc.push_lead(Some(10), "a");
        acc.push_lead(Some(10), "b");
        assert_eq!(acc.timeline().len(), 1);
        let TimelineKind::Text(text) = acc.timeline().entries()[0].kind() else {
            panic!("the lead is text");
        };
        assert_eq!(text, "ab");
        assert_eq!(
            acc.timeline().entries()[0].shown_at(),
            None,
            "a lead shows no clock"
        );

        // A ledger entry shows its payload's `finished_at`, not the keyed
        // clock; a user message shows no clock; a blank user message owns no
        // entry.
        let mut acc = StreamAccumulator::new("test");
        acc.push_ledger_entry_at(
            Some(4_000),
            TaskCompletionEntry {
                kind: TaskKind::Shell,
                label: Some("sleep".into()),
                id: None,
                started_at: None,
                finished_at: Some(4_000),
                ending: TaskEnding::RuntimeEnded,
                output: None,
            },
            None,
        );
        acc.push_user_message(Some(5_000), "external");
        let ledger = &acc.timeline().entries()[0];
        assert_eq!(ledger.shown_at(), None, "a ledger entry shows no clock");
        let user = &acc.timeline().entries()[1];
        assert_eq!(user.shown_at(), None, "a user message shows no clock");
        let before = acc.timeline().len();
        acc.push_user_message(Some(6_000), "   ");
        assert_eq!(
            acc.timeline().len(),
            before,
            "a blank user message renders nothing"
        );
    }

    /// Spec #1043: the message scope's floor reaches a compaction row. The row
    /// names a `message_time` that predates the receipt — so its admission earns
    /// no cut of its own — while the scope it is admitted under belongs to a
    /// message created after the ask: the row follows the scope's floor.
    #[test]
    fn a_message_scope_floors_a_compaction_row() {
        let compaction = Compaction {
            status: CompactionStatus::Completed,
            reason: CompactionReason::Auto,
            summary: "floored".into(),
            usage: None,
            error: None,
            recent: String::new(),
        };
        let mut timeline = Timeline::default();
        timeline.record_cut(1_000, 9_000);
        let order = HashMap::from([("msg_comp", 0usize)]);
        let scope_message = MessageId::new("msg_scope");
        let row = PartSource::at(MessageId::new("msg_comp"), 0);
        let key = {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(Some(2_000), &scope_message, &[]);
            scope
                .synthetic(SyntheticEntry::Compaction {
                    fact: compaction,
                    at: Some(500),
                    // The row's own message predates the ask: no cut of its own.
                    message_time: Some(1_000),
                    row,
                })
                .expect("a compaction owns one entry")
        };
        assert_eq!(
            key, 9_000,
            "the scope's message floor, not the row's own cut, places it"
        );
        assert_eq!(timeline.entries()[0].key(), 9_000);
    }

    /// Spec #1040's guard invariant: a message scope's `Drop` clears its
    /// floor, so content pushed after it is not floored by the dropped scope.
    #[test]
    fn a_dropped_message_scope_clears_its_floor() {
        let mut timeline = Timeline::default();
        timeline.record_cut(1_000, 9_000);
        let order = HashMap::from([("m1", 0usize)]);
        let message = MessageId::new("m1");
        let facts = |key: i64, source: bool| PartFacts {
            form: PartForm::Text,
            key_at: Some(key),
            shown_at: Some(key),
            source: source.then(|| PartSource::at(MessageId::new("m1"), 0)),
            index: 0,
        };
        {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(Some(2_000), &message, &[]);
            let placement = scope.place(facts(2_000, true));
            scope
                .push(
                    placement,
                    TranscriptKind::Text {
                        chunk: "点击之后".into(),
                        lead: None,
                    },
                )
                .expect("the scope's own token pushes");
        }
        assert_eq!(
            timeline.entries()[0].key(),
            9_000,
            "a message created after the ask takes the receipt cut"
        );
        // The scope is gone: a push through a fresh one is no longer floored by
        // it (no walk writes a floor outside its own scope).
        push_text(&mut timeline, &order, "after", 2_000, None, "无作用域");
        let after = timeline
            .entries()
            .iter()
            .find(|item| matches!(item.kind(), TimelineKind::Text(text) if text == "无作用域"))
            .expect("the after-scope push landed");
        assert_eq!(after.key(), 2_000, "the dropped scope's floor is cleared");
    }

    /// Spec #1040's guard invariant: a walk's `Drop` clears the read floor. A
    /// scope in the same walk sets 9000 as the floor; after the walk drops, a
    /// push outside any walk and a FRESH scope whose message is first in the
    /// new read both land at their own server key, never the stale floor.
    #[test]
    fn a_dropped_walk_clears_the_read_floor() {
        let mut timeline = Timeline::default();
        let first_order = HashMap::from([("m0", 0usize), ("m1", 1usize)]);
        let second_order = HashMap::from([("m1", 0usize), ("m0", 1usize)]);
        let m0 = MessageId::new("m0");
        let m1 = MessageId::new("m1");
        let facts = |message: &MessageId, key: i64| PartFacts {
            form: PartForm::Text,
            key_at: Some(key),
            shown_at: Some(key),
            source: Some(PartSource::at(message.clone(), 0)),
            index: 0,
        };
        {
            let mut walk = timeline.walk(&first_order);
            {
                let mut scope = walk.message(None, &m0, &[]);
                let placement = scope.place(facts(&m0, 9_000));
                scope
                    .push(
                        placement,
                        TranscriptKind::Text {
                            chunk: "m0".into(),
                            lead: None,
                        },
                    )
                    .unwrap();
            }
            // A later message in this walk sees m0's 9000 as its read floor;
            // the scope is enough to set it (no part has to render).
            let _later = walk.message(None, &m1, &[]);
        }
        // A push outside the dropped walk is not floored by it.
        push_text(
            &mut timeline,
            &second_order,
            "outside_msg",
            1_000,
            None,
            "outside",
        );
        let outside = timeline
            .entries()
            .iter()
            .find(|item| matches!(item.kind(), TimelineKind::Text(text) if text == "outside"))
            .expect("the after-walk push landed");
        assert_eq!(outside.key(), 1_000, "the dropped walk's read floor is cleared");

        // A fresh read where m1 precedes m0: the stored m0 entry is not an
        // earlier message, so m1's own server key wins below the stale floor.
        let mut walk = timeline.walk(&second_order);
        let mut scope = walk.message(None, &m1, &[]);
        let placement = scope.place(facts(&m1, 1_000));
        assert_eq!(
            placement.key, 1_000,
            "the fresh scope lands at its own server time, not the stale floor"
        );
    }

    /// At equal keys a positioned entry sorts by read position — a later
    /// message's part is pushed first, an earlier message's part later, and the
    /// earlier one stays ahead — while a synthetic entry (no position) sorts
    /// before every positioned entry (ADR-0038 rule 4).
    #[test]
    fn equal_keys_order_by_read_position_then_class() {
        let mut timeline = Timeline::default();
        let order = HashMap::from([("m0", 0usize), ("m1", 1usize)]);
        let push = |timeline: &mut Timeline, message: &str, index: usize, chunk: &str| {
            let id = MessageId::new(message);
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(None, &id, &[]);
            let facts = PartFacts {
                form: PartForm::Text,
                key_at: Some(1_000),
                shown_at: Some(1_000),
                source: Some(PartSource::at(id.clone(), index)),
                index,
            };
            let placement = scope.place(facts);
            scope
                .push(
                    placement,
                    TranscriptKind::Text {
                        chunk: chunk.to_string(),
                        lead: None,
                    },
                )
                .unwrap();
        };
        push(&mut timeline, "m1", 0, "later message");
        push(&mut timeline, "m0", 5, "earlier message");
        timeline.synthetic(SyntheticEntry::Receipt {
            text: "回执",
            at: Some(1_000),
        });
        let kinds: Vec<&str> = timeline
            .entries()
            .iter()
            .map(|item| match item.kind() {
                TimelineKind::Text(text) => text.as_str(),
                TimelineKind::Receipt(text) => text.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["回执", "earlier message", "later message"],
            "source-less first, then read position"
        );
    }

    /// A placement is linear and validated before it mutates: a token from a
    /// dropped scope and a mismatched kind are both refused with `Err` and
    /// insert nothing.
    #[test]
    fn a_stale_or_mismatched_placement_is_refused_before_inserting() {
        let mut timeline = Timeline::default();
        let order = HashMap::from([("m1", 0usize)]);
        let message = MessageId::new("m1");
        let facts = || PartFacts {
            form: PartForm::Text,
            key_at: Some(1_000),
            shown_at: Some(1_000),
            source: Some(PartSource::at(MessageId::new("m1"), 0)),
            index: 0,
        };

        let (stale, mismatch) = {
            let mut walk = timeline.walk(&order);
            let mut first = walk.message(None, &message, &[]);
            let token = first.place(facts());
            drop(first);
            let mut second = walk.message(None, &message, &[]);
            let stale = second.push(
                token,
                TranscriptKind::Text {
                    chunk: "stale".into(),
                    lead: None,
                },
            );
            let token = second.place(facts());
            let mismatch = second.push(
                token,
                TranscriptKind::Reasoning {
                    chunk: "wrong kind".into(),
                    lead: None,
                },
            );
            (stale, mismatch)
        };
        assert_eq!(stale, Err(PlacementError::Stale));
        assert_eq!(mismatch, Err(PlacementError::KindMismatch));
        assert!(timeline.is_empty(), "a refused placement inserts nothing");
    }

    /// A replace validates generation, kind and source BEFORE dropping the
    /// part's run: every refusal leaves the original entries intact.
    #[test]
    fn a_refused_replace_keeps_the_run() {
        let mut timeline = Timeline::default();
        let order = HashMap::from([("m1", 0usize)]);
        let message = MessageId::new("m1");
        let source = PartSource::at(MessageId::new("m1"), 0);
        {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(None, &message, &[]);
            let facts = PartFacts {
                form: PartForm::Text,
                key_at: Some(1_000),
                shown_at: Some(1_000),
                source: Some(source.clone()),
                index: 0,
            };
            let placement = scope.place(facts);
            scope
                .push(
                    placement,
                    TranscriptKind::Text {
                        chunk: "原始".into(),
                        lead: None,
                    },
                )
                .unwrap();
        }
        let (mismatch, source_less, other_source) = {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(None, &message, &[]);
            let text = || TranscriptKind::Text {
                chunk: "改写".into(),
                lead: None,
            };
            let facts = |source: Option<PartSource>, index: usize| PartFacts {
                form: PartForm::Text,
                key_at: Some(1_000),
                shown_at: Some(1_000),
                source,
                index,
            };
            let token = scope.place(facts(Some(source.clone()), 0));
            let mismatch = scope.replace(
                token,
                &source,
                TranscriptKind::Reasoning {
                    chunk: "错类".into(),
                    lead: None,
                },
            );
            let token = scope.place(facts(None, 0));
            let source_less = scope.replace(token, &source, text());
            let other = PartSource::at(MessageId::new("m1"), 1);
            let token = scope.place(facts(Some(other), 1));
            let other_source = scope.replace(token, &source, text());
            (mismatch, source_less, other_source)
        };
        assert_eq!(mismatch, Err(PlacementError::KindMismatch));
        assert_eq!(source_less, Err(PlacementError::SourceMismatch));
        assert_eq!(other_source, Err(PlacementError::SourceMismatch));
        assert_eq!(timeline.entries().len(), 1, "the original run survives");
        assert!(
            matches!(timeline.entries()[0].kind(), TimelineKind::Text(text) if text == "原始"),
            "no refused replace dropped or overwrote the run"
        );
    }

    /// A rewritten part whose run straddles the live-slice boundary (spec
    /// #561's replace rule, ticket #1044): `drop_source_run` lowers
    /// `render_from` by exactly the run entries that sat BEFORE it, never by
    /// the whole removed run, so the sent part of the timeline stays sent and
    /// the surviving next entry stays live. Driven through
    /// [`MessageScope::replace`], so the pre-drop placement and the post-drop
    /// clamp are the ones production uses.
    #[test]
    fn a_rewritten_run_keeps_the_boundary_on_its_entry() {
        let mut timeline = Timeline::default();
        let order = HashMap::from([("m1", 0usize)]);
        let source = PartSource::at(MessageId::new("m1"), 1);
        push_text(
            &mut timeline,
            &order,
            "m1",
            500,
            Some(PartSource::at(MessageId::new("m1"), 0)),
            "A",
        );
        // A chunked run: three entries at the part's key, one before the
        // boundary and two after it.
        push_text(
            &mut timeline,
            &order,
            "m1",
            1_000,
            Some(source.clone()),
            &"长".repeat(MAX_CARD_TEXT_CHARS * 2 + 10),
        );
        push_text(
            &mut timeline,
            &order,
            "m1",
            2_000,
            Some(PartSource::at(MessageId::new("m1"), 2)),
            "X",
        );
        assert_eq!(timeline.entries().len(), 5, "A, three chunks, X");
        // Sent: A and the run's first two chunks. Live: its last chunk and X.
        timeline.set_render_from(3);

        let rewritten = PartSource::new(MessageId::new("m1"), 1, 0, Some(cursor_prefix_digest("XYZ")));
        {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(None, &MessageId::new("m1"), &[]);
            let facts = PartFacts {
                form: PartForm::Text,
                key_at: Some(1_000),
                shown_at: Some(1_000),
                source: Some(rewritten),
                index: 1,
            };
            let placement = scope.place(facts);
            scope
                .replace(
                    placement,
                    &source,
                    TranscriptKind::Text {
                        chunk: "XYZ".into(),
                        lead: None,
                    },
                )
                .expect("the scope accepts its own rewrite");
        }

        assert_eq!(
            timeline.entries().len(),
            3,
            "the rewritten run replaced all three chunks"
        );
        assert_eq!(
            timeline.render_from(),
            1,
            "the boundary dropped only the two run entries before it, not all three"
        );
        assert!(
            matches!(timeline.entries()[0].kind(), TimelineKind::Text(text) if text == "A"),
            "the already-sent entry stays before the boundary"
        );
        assert!(
            matches!(timeline.entries()[timeline.render_from()].kind(), TimelineKind::Text(text) if text == "XYZ"),
            "the rewrite's new run starts the live slice"
        );
        assert!(
            matches!(timeline.entries()[2].kind(), TimelineKind::Text(text) if text == "X"),
            "the entry that followed the run stays live, after the new run"
        );
    }

    /// A later chunk of a rewritten part merges into the run the rewrite
    /// installed instead of starting a second entry (spec #561, ticket #1044):
    /// the new run is the part's one run, so the card cannot duplicate its text.
    #[test]
    fn a_later_chunk_merges_into_the_rewritten_run() {
        let mut timeline = Timeline::default();
        let order = HashMap::from([("m1", 0usize)]);
        let message = MessageId::new("m1");
        let source = PartSource::at(message.clone(), 0);
        push_text(&mut timeline, &order, "m1", 2_000, Some(source.clone()), "ABC");

        let rewritten = PartSource::new(message.clone(), 0, 0, Some(cursor_prefix_digest("XYZ")));
        {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(None, &message, &[]);
            let facts = PartFacts {
                form: PartForm::Text,
                key_at: Some(2_000),
                shown_at: Some(2_000),
                source: Some(rewritten),
                index: 0,
            };
            let placement = scope.place(facts);
            scope
                .replace(
                    placement,
                    &source,
                    TranscriptKind::Text {
                        chunk: "XYZ".into(),
                        lead: None,
                    },
                )
                .expect("the scope accepts its own rewrite");
        }
        assert_eq!(timeline.entries().len(), 1, "the rewrite replaced the run");
        assert!(
            matches!(timeline.entries()[0].kind(), TimelineKind::Text(text) if text == "XYZ"),
            "the rewrite's own content is the run"
        );
        let rewritten_seq = timeline.entries()[0].seq();

        let grown = PartSource::new(message.clone(), 0, 0, Some(cursor_prefix_digest("XYZ更多")));
        push_text(&mut timeline, &order, "m1", 2_000, Some(grown), "更多");

        assert_eq!(
            timeline.entries().len(),
            1,
            "the later chunk did not start a second entry"
        );
        assert_eq!(
            timeline.entries()[0].seq(),
            rewritten_seq,
            "the chunk merged into the rewrite's entry"
        );
        assert!(
            matches!(timeline.entries()[0].kind(), TimelineKind::Text(text) if text == "XYZ更多"),
            "the rewritten run grew by the later chunk"
        );
    }

    /// A receipt recorded AFTER the pin still floors the settle (ADR-0038
    /// rule 4): the pin's own floor is not the last word.
    #[test]
    fn a_receipt_recorded_after_the_pin_floors_the_settle() {
        let mut timeline = Timeline::default();
        let order = HashMap::from([("m1", 0usize)]);
        let message = MessageId::new("m1");
        let panel = ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            Some(serde_json::json!("ls")),
            Some("src"),
        );
        let tail = {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(Some(5_000), &message, &[]);
            scope.pin(
                PartFacts {
                    form: PartForm::Tool,
                    key_at: Some(2_000),
                    shown_at: Some(2_000),
                    source: Some(PartSource::at(MessageId::new("m1"), 0)),
                    index: 0,
                },
                "call_bash".into(),
            )
        };
        timeline.record_cut(4_000, 9_000);
        timeline.settle(tail, &panel, &order, &[]);
        assert_eq!(
            timeline.entries()[0].key(),
            9_000,
            "the later receipt floors the settle"
        );
    }

    /// ADR-0045's late start replaces the pinned key but never below the floor
    /// the pin carried (spec #1040's deliberate clamp): a start behind a
    /// receipt cuts cannot drag the panel above it.
    #[test]
    fn a_rekey_never_drops_below_the_pins_floor() {
        let mut timeline = Timeline::default();
        let order = HashMap::from([("m1", 0usize)]);
        timeline.record_cut(4_000, 9_000);
        let panel = ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            Some(serde_json::json!("ls")),
            Some("src"),
        );
        let mut tail = {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(Some(5_000), &MessageId::new("m1"), &[]);
            scope.pin(
                PartFacts {
                    form: PartForm::Tool,
                    key_at: Some(2_000),
                    shown_at: None,
                    source: Some(PartSource::at(MessageId::new("m1"), 0)),
                    index: 0,
                },
                "call_bash".into(),
            )
        };
        timeline.rekey(&mut tail, Some(1_000));
        assert_eq!(tail.key, 9_000, "the rekey clamps to the pin's floor");
        timeline.settle(tail, &panel, &order, &[]);
        assert_eq!(timeline.entries()[0].key(), 9_000);
    }

    /// The settle clamp is a MINIMUM over the entries that follow the panel in
    /// read order at or above its floor, never the first one: a start time
    /// beyond every following key lands at the lowest of them, and a
    /// non-monotonic run cannot be jumped.
    #[test]
    fn a_late_start_is_capped_at_the_following_minimum() {
        let mut timeline = Timeline::default();
        let order = HashMap::from([("m1", 0usize)]);
        let message = MessageId::new("m1");
        let panel = ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            Some(serde_json::json!("ls")),
            Some("src"),
        );
        // The message is post-click: content may not precede the receipt's
        // key, so the entries at 3000 and 2000 are at/above the floor.
        timeline.record_cut(4_000, 1_500);
        let mut tail = None;
        for (index, key) in [(0, 5_000), (1, 3_000), (2, 2_000)] {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(Some(5_000), &message, &[]);
            if index == 0 {
                tail = Some(scope.pin(
                    PartFacts {
                        form: PartForm::Tool,
                        key_at: Some(key),
                        shown_at: Some(key),
                        source: Some(PartSource::at(MessageId::new("m1"), index)),
                        index,
                    },
                    "call_bash".into(),
                ));
            } else {
                let facts = PartFacts {
                    form: PartForm::Text,
                    key_at: Some(key),
                    shown_at: Some(key),
                    source: Some(PartSource::at(MessageId::new("m1"), index)),
                    index,
                };
                let placement = scope.place(facts);
                scope
                    .push(
                        placement,
                        TranscriptKind::Text {
                            chunk: format!("正文{index}"),
                            lead: None,
                        },
                    )
                    .unwrap();
            }
        }
        // A positioned follower BELOW the floor never caps the settle: it
        // keeps its key and the panel lands after it.
        timeline.items.insert(
            0,
            TimelineItem {
                key: 1_000,
                shown_at: None,
                order: 0,
                seq: 99,
                kind: TimelineKind::Text("floor 之下".into()),
                source: Some(PartSource::at(MessageId::new("m1"), 3)),
                lead: None,
                position: Some((0, 3)),
            },
        );
        let mut tail = tail.expect("the tail was pinned");
        // The server's late start time is beyond every following key.
        timeline.rekey(&mut tail, Some(9_000));
        timeline.settle(tail, &panel, &order, &[]);
        let tool = timeline
            .entries()
            .iter()
            .position(|item| matches!(item.kind(), TimelineKind::Tool(_)))
            .expect("the panel settled");
        assert_eq!(
            timeline.entries()[tool].key(),
            2_000,
            "the settle lands at the minimum following key at/above the floor"
        );
        assert_eq!(
            timeline.entries()[0].key(),
            1_000,
            "a follower below the floor stays put"
        );
        assert!(tool > 0, "the panel lands after the below-floor follower");
    }

    /// An earlier message's pinned tail floors a later message's part (spec
    /// #1040's deliberate extension): its older server key cannot place it
    /// above the live panel, and the panel keeps the read order at the tie.
    #[test]
    fn an_earlier_messages_pinned_tail_floors_a_later_message() {
        let mut timeline = Timeline::default();
        let order = HashMap::from([("m0", 0usize), ("m1", 1usize)]);
        let panel = ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            Some(serde_json::json!("ls")),
            Some("src"),
        );
        let tail = {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(Some(5_000), &MessageId::new("m0"), &[]);
            scope.pin(
                PartFacts {
                    form: PartForm::Tool,
                    key_at: Some(9_000),
                    shown_at: Some(9_000),
                    source: Some(PartSource::at(MessageId::new("m0"), 0)),
                    index: 0,
                },
                "call_bash".into(),
            )
        };
        let pinned = vec![tail.fact().expect("a scoped pin names its message")];
        {
            let mut walk = timeline.walk(&order);
            let mut scope = walk.message(Some(2_000), &MessageId::new("m1"), &pinned);
            let facts = PartFacts {
                form: PartForm::Text,
                key_at: Some(1_000),
                shown_at: Some(1_000),
                source: Some(PartSource::at(MessageId::new("m1"), 0)),
                index: 0,
            };
            let placement = scope.place(facts);
            assert_eq!(
                placement.key, 9_000,
                "the earlier message's pinned tail is the later message's floor"
            );
            scope
                .push(
                    placement,
                    TranscriptKind::Text {
                        chunk: "later".into(),
                        lead: None,
                    },
                )
                .unwrap();
        }
        timeline.settle(tail, &panel, &order, &[]);
        let kinds: Vec<&str> = timeline
            .entries()
            .iter()
            .map(|item| match item.kind() {
                TimelineKind::Tool(_) => "tool",
                TimelineKind::Text(text) => text.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["tool", "later"],
            "the panel keeps the earlier read position at the tied key"
        );
    }

    /// The accumulator's scoped path feeds every pinned tail into the walk
    /// (spec #1040): a live tool pinned in an earlier message floors a later
    /// message's part through `pinned_tail_facts`, not by hand.
    #[test]
    fn the_accumulators_scoped_push_uses_the_pinned_tail_floor() {
        let message = |id: &str, created: i64| TranscriptMessage {
            id: MessageId::new(id),
            role: MessageRole::Assistant,
            time: Some(MessageTime {
                created,
                completed: None,
            }),
            model: None,
            tokens: None,
            error: None,
            parts: Vec::new(),
        };
        let mut acc = StreamAccumulator::new("test");
        let read_positions = HashMap::from([("m0", 0usize), ("m1", 1usize)]);
        let m0 = message("m0", 5_000);
        let m1 = message("m1", 2_000);
        acc.push_tool_scoped(
            &read_positions,
            &m0,
            0,
            Some(9_000),
            Some(9_000),
            "call_bash",
            ToolPanel::for_test(
                "bash",
                ToolStatus::Running,
                Some(serde_json::json!({ "command": "sleep 30" })),
                None,
            ),
            Some(PartSource::at(m0.id.clone(), 0)),
        )
        .expect("the live tool pins");
        let facts = PartFacts {
            form: PartForm::Text,
            key_at: Some(1_000),
            shown_at: Some(1_000),
            source: Some(PartSource::at(m1.id.clone(), 0)),
            index: 0,
        };
        acc.push_text_scoped(&read_positions, &m1, facts, "later".into(), None)
            .expect("the later part places");
        let later = acc
            .timeline()
            .iter()
            .find(|item| matches!(item.kind(), TimelineKind::Text(text) if text == "later"))
            .expect("the later part landed");
        assert_eq!(
            later.key(),
            9_000,
            "the earlier message's pinned tail is the later part's floor"
        );
    }

    /// A settled tool joins the timeline under the message's receipt cut: the
    /// floor belongs to the module's insert path, not the caller.
    #[test]
    fn a_settled_tool_takes_the_message_floor() {
        let mut acc = StreamAccumulator::new("test");
        acc.record_receipt_cut(1_000, 9_000);
        acc.script_message(Some(2_000));
        acc.push_tool_at(
            Some(2_000),
            "call_1",
            ToolPanel::for_test(
                "bash",
                ToolStatus::Completed,
                Some(serde_json::json!("ls")),
                Some("src"),
            ),
        );
        acc.script_message(None);
        assert_eq!(acc.timeline().len(), 1);
        assert_eq!(
            acc.timeline().entries()[0].key(),
            9_000,
            "the settled panel takes the message's receipt cut"
        );
    }
}
