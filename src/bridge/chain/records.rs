//! The Chain Record (ADR-0069): one module owns the durable per-Session facts
//! about a Card Chain, in one sidecar (`chain_records.json`).
//!
//! The sections share the file with independent lifetimes:
//!
//! - **records** — the live card record (ADR-0063): which card a Session is
//!   currently streaming into and the Turn it answers, plus the Session's
//!   directory and the per-process-life reconciliation marks. One entry owns
//!   its lifecycle: [`ChainRecords::track`] creates the record when a card
//!   becomes the Session's live card and re-points it when the chain
//!   continues, [`ChainRecords::release`] removes it once that card reaches a
//!   terminal or is collected, and [`release_spent`] carries the rule that
//!   decides the terminal case — release only once the ending write is
//!   confirmed (ADR-0063 amendment, ADR-0067).
//! - **announcements** — the Wake Watermark (ADR-0061): the newest Wake whose
//!   completion a card announced, advanced only after the carrying card write
//!   lands. Monotonic per Session and never removed, so a restart cannot
//!   re-announce a Wake a previous life already showed.
//! - **endings** — the per-run **Task Ending**s (ADR-0080): one
//!   [`TaskRunRecord`] per [`TaskRunId`], each carrying at most one ending per
//!   class and its entry's own state. Recorded where the ending is derived
//!   (never when a card write lands), so a restart re-derives nothing. Like the
//!   watermark it is session-scoped — `release` drops only the chain's record —
//!   because the fact is about the run, not about the card that will show it.
//!   Bounded: the OLDEST record with no owed ending goes first, and past the
//!   owed cap the oldest owed entry is surrendered with a `WARN` (its fact
//!   kept). The pre-ADR-0080 `rendered` section an older build wrote is
//!   translated into it once, at load — a `serde`-only field on
//!   [`ChainRecordFile`], never kept nor written again.
//!
//! The load fails open (a missing or corrupt file reads as empty) and the
//! write is best-effort and atomic (the shared [`sidecar`] mechanics). The
//! one deviation from the sidecar convention: the file is **kept even when
//! every section is empty**, because its presence is the one-time migration
//! marker from the pre-ADR-0069 sidecars (`live_cards.json`,
//! `wake_watermarks.json`). When it is absent, [`ChainRecords::load`] folds
//! those legacy files into the two sections once and they are never written
//! again; removing an emptied file would fall back to them and resurrect
//! state that was deliberately removed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::backend::{MessageId, TurnAnchor};
use crate::bridge::handles::CardsHandle;
use crate::bridge::sidecar;
use crate::bridge::tasks::model::TaskRunId;

/// The orphan gap a restart must still deliver (spec #561, review #569): the
/// message-first takeover's undelivered tail, recorded durably because the
/// chain's Rendered Cursor has already advanced past the delivered content that
/// follows it — one frontier cannot express "gap undelivered, later content
/// delivered".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingGap {
    /// The gap's own cursor: where the undelivered tail starts, inside the
    /// orphaned Turn's window — and, once a body covering part of the gap is
    /// confirmed, where the DELIVERED part ends, so a restart resumes after it.
    pub(crate) cursor: RenderedCursor,
    /// The orphaned Turn's anchor: the window the gap lives in.
    pub(crate) anchor: TurnAnchor,
    /// The user message that ENDS the gap's window — the Turn the chain moved
    /// to when the gap was recorded. Content from there on belongs to a later
    /// card the durable cursor already covers, while [`TurnAnchor`] membership
    /// has no upper end; the walk stops before this message. `None` when the
    /// successor's Turn message could not be named: a read-derived bound (the
    /// next user message after the gap's anchor) stands in.
    #[serde(default)]
    pub(crate) bound: Option<MessageId>,
}

/// The transcript kind of the part a [`CursorFrontier`] names: the two kinds
/// carrying renderable model content and a settled tool panel (spec #561,
/// review #569).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CursorPartKind {
    Text,
    Reasoning,
    /// A settled tool panel (spec #561, review #569): the frontier names its
    /// position so a restart never re-renders a panel the old card already
    /// showed. `CursorFrontier::delivered_chars` then belongs to the newest
    /// text/reasoning part at or before it — a tool has no extent of its own.
    Tool,
}

/// The digest a Rendered Cursor frontier stores for a part's delivered prefix
/// (spec #561, review #569): a small, derived fingerprint of the prefix
/// CONTENT — never the content itself, and never the payload journal #505
/// rejected. It lets resolution tell a grown part (same prefix) from a
/// replacement in the same slot when both parts carry no server start time
/// (V2 decodes text parts without `time.start`), where identity alone cannot.
pub(crate) fn cursor_prefix_digest(prefix: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(prefix.as_bytes());
    u64::from_be_bytes(hash[..8].try_into().expect("sha256 is 32 bytes"))
}

/// The transcript position of the newest delivered content a Card Chain has
/// confirmed delivered (spec #561): the message identity, the part's position
/// and — for a text/reasoning frontier — the delivered character extent of
/// that part. A settled tool may be the frontier too (review #569), by
/// position alone; the extent then belongs to the newest text/reasoning part
/// at or before it, so that part still renders its growth. Position and
/// identity only, plus the prefix's derived digest — never content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CursorFrontier {
    /// The assistant message that carried the part.
    pub(crate) message_id: MessageId,
    /// The part's index in its message's typed `parts` vec, so a later read
    /// resolves the same part.
    pub(crate) part_index: usize,
    pub(crate) kind: CursorPartKind,
    /// The part's server start time, when it reported one.
    #[serde(default)]
    pub(crate) started_at: Option<i64>,
    /// Unicode characters of the part's text confirmed delivered.
    pub(crate) delivered_chars: usize,
    /// The delivered prefix's [`cursor_prefix_digest`] (spec #561, review
    /// #569): derived metadata, never content. `None` on a cursor written by
    /// an older release — or by a body whose entries carried no digest —
    /// resolution then falls back (no projection) rather than guess, the same
    /// one-release seam a cursorless record uses.
    #[serde(default)]
    pub(crate) prefix_digest: Option<u64>,
}

/// The **Rendered Cursor** (spec #561): how far a Card Chain's card has
/// confirmed rendered. The frontier is the newest delivered text/reasoning
/// position; `live_calls` is the set of tool call ids whose newest delivered
/// state was `running`. Stored on the [Chain Record](ChainRecord) and
/// advanced only by a confirmed card write; `None` on the record means
/// cursorless — an older release, or a chain whose first write has not been
/// confirmed — which reads as the existing fallback behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RenderedCursor {
    #[serde(default)]
    pub(crate) frontier: Option<CursorFrontier>,
    #[serde(default)]
    pub(crate) live_calls: std::collections::BTreeSet<String>,
}

/// One Session's durable live-card facts. `card_message_id` is the Feishu
/// message the reap PATCHes; `message_id` is the Turn's own message (the
/// `msg_cola_` user message a cola Turn submitted, or the external message an
/// adopted run answers) and `created_ms` its server time, once a read carried
/// it — the two together are the [Turn anchor](TurnAnchor) the settle decision
/// is scoped with. `created_ms` stays `None` while the submitted message has
/// never been observed: the reap then probes the transcript with `message_id`,
/// and a read that still carries no such user message ends the card Unreceived
/// (ADR-0062), never ✅.
///
/// `directory` is the Session's working directory at track time: on a
/// generation whose reads route per directory (V1), the reap must ask the
/// instance the card belongs to, never the process's cwd — and an unmapped
/// Session has no mapping to fall back to (finding #438).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChainRecord {
    pub(crate) card_message_id: String,
    /// In-memory only: this chain's **Chain Generation** (spec #571, ticket
    /// #572) — a monotonic version of the tracked card identity, bumped
    /// whenever a track, a re-point or a takeover rewrites the record onto a
    /// DIFFERENT card. A decision captures it together with the snapshot it
    /// decided from, so a card write can submit with the state version it
    /// read. Never persisted: a fresh process life (and so a record loaded
    /// from disk) starts at zero, and the generation dies with the record it
    /// belongs to.
    #[serde(skip)]
    pub(crate) generation: u64,
    pub(crate) message_id: MessageId,
    #[serde(default)]
    pub(crate) created_ms: Option<i64>,
    #[serde(default)]
    pub(crate) directory: Option<String>,
    /// The Feishu message this chain's cards reply under (issue #580): the
    /// user message the Turn answered, as the accumulator's
    /// `reply_to_message_id` carried when the card was tracked. The
    /// projection's successor replies here first, then falls to
    /// `card_message_id`, then the Chat — an OpenCode message id (`msg_*`)
    /// is never a deliverable Feishu target. `None` on a record written by
    /// an older release, or one whose chain never knew a target: the
    /// projection falls back to the recorded card.
    #[serde(default)]
    pub(crate) reply_to: Option<String>,
    /// In-memory only: the reap already PATCHed this card into its waiting
    /// ending. A record that is rewritten (a new card becomes live) starts
    /// unmarked; the flag only suppresses PATCHing the same waiting card on
    /// every Session Sync tick, and never survives a restart.
    #[serde(skip)]
    pub(crate) waiting_reaped: bool,
    /// In-memory only: this chain's projection already attempted its successor
    /// create in this process life (spec #561, review #569). Feishu has no
    /// idempotency key (ADR-0067), so a create whose outcome is not a definite
    /// non-landing is never retried — it may have landed, and a retry would
    /// post a duplicate successor. The record stays for the reap's in-place
    /// state repair instead. A record rewritten for a new card starts
    /// unmarked, and the mark never survives a restart.
    #[serde(skip)]
    pub(crate) projection_attempted: bool,
    /// Durable (spec #561, review #569): a successor create is in flight for
    /// this chain. Persisted BEFORE the create — the projection's write-ahead
    /// intent — so a process that dies between the create and the takeover's
    /// re-point leaves the single-shot fact behind: the next life treats the
    /// create as ambiguous and never re-posts a duplicate, and the old card
    /// takes the state-repair path. Consumed when the projection succeeds (the
    /// takeover rewrites the record, and a fresh record starts unmarked) or
    /// cleared when a DEFINITE non-delivery proves no message was created, so a
    /// retry stays safe. Scoped to the record's generation by its own lifetime.
    #[serde(default)]
    pub(crate) projection_intent: bool,
    /// Durable (spec #561, review #569): the orphan gap this chain still owes —
    /// the message-first takeover's undelivered tail, whose start the Rendered
    /// Cursor can no longer express once the delivered content after it
    /// advanced the frontier. Rendered once by the projection that finds it (or
    /// by the fresh Turn that carries it), then consumed: a projection's
    /// re-point rewrites the record (while an unrendered gap is carried onto
    /// the new one), and a rendered gap's first confirmed write clears it.
    #[serde(default)]
    pub(crate) pending_gap: Option<PendingGap>,
    /// The Rendered Cursor (spec #561): how far this chain's card has
    /// confirmed rendered. `None` — cursorless — is an older release's record
    /// or a chain whose first confirmed write has not landed; both read as the
    /// existing fallback behavior.
    #[serde(default)]
    pub(crate) cursor: Option<RenderedCursor>,
}

impl ChainRecord {
    pub(crate) fn new(
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
    ) -> Self {
        Self {
            card_message_id: card_message_id.into(),
            generation: 0,
            message_id,
            created_ms,
            directory: None,
            reply_to: None,
            waiting_reaped: false,
            projection_attempted: false,
            projection_intent: false,
            pending_gap: None,
            cursor: None,
        }
    }

    /// Carry the Session's directory so the reap's reads route to the right
    /// instance. An empty directory is unknown, exactly like `None`.
    pub(crate) fn with_directory(mut self, directory: Option<String>) -> Self {
        self.directory = directory.filter(|directory| !directory.is_empty());
        self
    }

    /// The Turn anchor this record scopes a settle decision with, when the
    /// message's server time was captured. `None` means the submitted message
    /// was never observed in a transcript — the anchorless Unreceived scope.
    /// The message id travels with its time as one fact, like every anchor.
    pub(crate) fn anchor(&self) -> Option<TurnAnchor> {
        Some(TurnAnchor {
            message_id: self.message_id.clone(),
            created_ms: self.created_ms?,
        })
    }
}

/// The in-memory **Chain Generation** a record write leaves behind (spec #571,
/// ticket #572): a record replacing one that named the same card carries its
/// generation (a re-track is not an identity change), a record re-pointed onto
/// a different card bumps it by one, and a genuinely new chain starts at zero.
fn chain_generation(previous: Option<&ChainRecord>, card_message_id: &str) -> u64 {
    match previous {
        Some(previous) if previous.card_message_id == card_message_id => previous.generation,
        Some(previous) => previous.generation.saturating_add(1),
        None => 0,
    }
}

/// One session's newest announced Wake: its identity and server time travel
/// together, the same one-fact anchor rule as [`TurnAnchor`].
///
/// [`TurnAnchor`]: crate::backend::TurnAnchor
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WakeMark {
    pub(crate) wake_id: String,
    pub(crate) created_ms: i64,
}

/// A recoverably-failed body's endings, waiting on the delivery of the Pending
/// Card Update payload at [`Self::seq`] (ADR-0080). Held on the Chain Records,
/// keyed by card, so it survives a card accumulator being replaced, removed or
/// re-keyed before the payload delivers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwedRendered {
    /// The Session the ending is owed to (corrected by
    /// [`ChainRecords::rename`] when the Session is recreated).
    pub(crate) session_id: String,
    pub(crate) seq: u64,
    pub(crate) shown: Vec<EndingKey>,
}

/// One **Task Ending**'s class (ADR-0080): what ended a Background Task run. A
/// run carries at most one ending per class, because the classes are different
/// claims about it — an [`TaskEndingClass::Evidence`] ending says the run ENDED,
/// so it suppresses a late **Wake**'s completion entry (#659), while a
/// [`TaskEndingClass::Cleanup`] claims nothing about how the run ended, so a
/// cleaned run's late Wake is its own ending (#590).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskEndingClass {
    /// An evidence ending: a **Runtime Reconciliation** verdict, a shell the
    /// runtime no longer knows, or a subagent's own terminal transcript.
    Evidence,
    /// The user's own **Cleanup** (spec #588, #590).
    Cleanup,
    /// The backend's own completion **Wake**, recorded for a run that has no
    /// evidence ending.
    Wake,
    /// The pre-#659 `runtime:<call_id>` spelling, translated once at load: read
    /// by the retirement's own dedup, never by the Wake gate (#590).
    Legacy,
}

/// What a derivation had as evidence for a run's end (ADR-0080): the detail its
/// class does not carry. Kept as the derivation reported it, so a later read
/// synthesizes the entry without re-reading the transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskEndingEvidence {
    /// The runtime's terminal shell verdict, verbatim — a status this build does
    /// not know stays as it came.
    Ended(String),
    /// The runtime has no record of the task at all.
    Lost,
    /// The child session's own newest assistant message ended the run (#591).
    ChildEnded,
    /// The state a **Wake** reported (`completed` / `cancelled` / `error`),
    /// `None` when it reported none.
    Wake(Option<String>),
}

/// An ending's entry state (ADR-0080). Monotonic: it only ever moves AWAY from
/// [`TaskEndingState::Owed`] — a confirmed card write shows it, or the
/// saturation cap surrenders it — so no later write can return an entry to
/// `Owed` and re-offer it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskEndingState {
    /// The entry the next card still renders.
    Owed,
    /// A confirmed card write displayed the entry.
    Shown,
    /// The entry was given up (its fact stays), for one of two reasons: the
    /// saturation cap surrendered it — the one ratified loss, for a prolonged
    /// carrier failure — or the class-agnostic entry gate did, because the run's
    /// entry was already shown in some other class and this entry was therefore
    /// never renderable (no loss: nothing a card could have shown was given up).
    /// Only an owed ending is ever rendered, so a surrendered one is never
    /// re-offered.
    Surrendered,
}

/// The identity a [`BackgroundTask`] is re-synthesized from (ADR-0080): what an
/// entry needs when the read no longer carries the run's launch, so the
/// synthesis never depends on the transcript's own scope. The label and the
/// start clock are captured when the ending is DERIVED — that read necessarily
/// carries the launch — which makes a later card's entry immune to compaction
/// and to a truncated (V2 page-capped) read.
///
/// [`BackgroundTask`]: crate::backend::BackgroundTask
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskRunIdentity {
    /// The tool that launched the run (`shell` / `subagent`) — the kind the
    /// entry renders as.
    #[serde(default)]
    pub(crate) tool: Option<String>,
    /// The launch's own tool call id.
    #[serde(default)]
    pub(crate) call_id: Option<String>,
    /// The shell the run executes in.
    #[serde(default)]
    pub(crate) shell_id: Option<String>,
    /// The child session a subagent runs in.
    #[serde(default)]
    pub(crate) child_id: Option<String>,
    /// When the run started.
    #[serde(default)]
    pub(crate) started_at: Option<i64>,
    /// The label the launch's input carried, when it named one.
    #[serde(default)]
    pub(crate) label: Option<String>,
}

/// One durable **Task Ending** (ADR-0080): a claim about how one Background Task
/// run ended, recorded at derivation and outliving the record it was first
/// written beside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskEnding {
    pub(crate) class: TaskEndingClass,
    pub(crate) state: TaskEndingState,
    /// When the run ended — the Wake's own server time, the runtime's reported
    /// completion time, or `None` for an ending the runtime could not stamp.
    #[serde(default)]
    pub(crate) finished_at: Option<i64>,
    /// The class's own evidence, when the derivation reported any.
    #[serde(default)]
    pub(crate) evidence: Option<TaskEndingEvidence>,
    /// The identity a Background Task is re-synthesized from.
    #[serde(default)]
    pub(crate) identity: TaskRunIdentity,
}

impl TaskEnding {
    /// A just-derived ending: nothing has shown it yet, so it is OWED until a
    /// confirmed card write shows it (ADR-0080).
    pub(crate) fn owed(class: TaskEndingClass, identity: TaskRunIdentity) -> Self {
        Self {
            class,
            state: TaskEndingState::Owed,
            finished_at: None,
            evidence: None,
            identity,
        }
    }
}

/// One run's record in the `endings` section (ADR-0080): the ids the run is
/// known by and at most one ending per class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskRunRecord {
    /// The run's identity: its stored key and every id it is known by.
    pub(crate) run: TaskRunId,
    #[serde(default)]
    pub(crate) endings: Vec<TaskEnding>,
}

impl TaskRunRecord {
    /// Whether the record still owes an entry: an ending no card has shown and
    /// neither cause has surrendered — not the saturation cap, and not the
    /// class-agnostic entry gate (a run whose entry was already shown in ANY
    /// class; see [`insert_ending`] / [`mark_shown`]). Only an OWED ending is
    /// ever rendered, and only a record with no owed ending is the bound's
    /// eviction candidate.
    pub(crate) fn owes(&self) -> bool {
        self.endings
            .iter()
            .any(|ending| ending.state == TaskEndingState::Owed)
    }
}

/// One entry's ending as a built body stages it (ADR-0080): the run whose entry
/// it renders and that ending's class. The unit the Rendered Cursor's stage
/// carries, the owed mirror remembers and a confirmed write flips to SHOWN — so
/// exactly-once is the ending's own fact, never a bare "this was rendered"
/// claim.
pub(crate) type EndingKey = (TaskRunId, TaskEndingClass);

/// The durable **Task Ending** facts one render's exactly-once gates read
/// (ADR-0080) — the session's run records, plus the endings a recoverably-failed
/// body's delivered-but-unrecorded payload still owes (folded in as SHOWN: the
/// card really showed them, the drain just has not written the bit yet). One
/// snapshot per gate, so a plan and its re-check ask the same questions of one
/// instant.
#[derive(Clone, Debug, Default)]
pub(crate) struct EndingFacts {
    records: Vec<TaskRunRecord>,
    /// `(run, class)` pairs a delivered payload carries that the records below
    /// do not yet mark shown.
    owed: Vec<EndingKey>,
}

impl EndingFacts {
    /// A snapshot over bare run records — the gates' own entry and the unit
    /// tests'. The owed half starts empty (nothing delivered-but-unrecorded).
    pub(crate) fn new(records: Vec<TaskRunRecord>) -> Self {
        Self {
            records,
            owed: Vec::new(),
        }
    }

    /// Whether the run — resolved by the alias order — carries an ending of
    /// `class` in ANY state. The Wake's evidence gate (issue #659): an evidence
    /// ending claims the run ended, so a late Wake's completion entry is the
    /// same fact twice. A Cleanup claims nothing of the sort (#590), which is
    /// why this asks a class and not "has any ending".
    pub(crate) fn recorded(&self, run: &TaskRunId, class: TaskEndingClass) -> bool {
        self.ending(run, class).is_some()
    }

    /// Whether ANY ending of the run was SHOWN (ADR-0080) — the retirement's
    /// rule is one entry per run, whatever class wrote it. The **legacy** class
    /// the load-time translation mints is `shown` by construction, so a
    /// pre-#659 mark still covers a re-derived retirement and this is the only
    /// reader it has.
    pub(crate) fn shown_any(&self, run: &TaskRunId) -> bool {
        let shown = resolve_run(&self.records, run).is_some_and(|index| {
            self.records[index]
                .endings
                .iter()
                .any(|ending| ending.state == TaskEndingState::Shown)
        });
        shown
            || self
                .owed
                .iter()
                .any(|(owed_run, _)| run.ids().any(|id| owed_run.names(id)))
    }

    /// Whether the run's ending of `class` was SHOWN (ADR-0080) — by a
    /// confirmed card write, or by a delivered-but-unrecorded payload the
    /// records below do not yet mark. The **Wake**'s own exactly-once gate: a
    /// Wake whose ending a card already showed is not planned again, even when
    /// a crash between the confirmed write and the later Wake-Watermark write
    /// left the Watermark stale. Resolved by the alias order, so a caller
    /// holding any spelling of the run asks the same question.
    pub(crate) fn shown(&self, run: &TaskRunId, class: TaskEndingClass) -> bool {
        let recorded = resolve_run(&self.records, run).is_some_and(|index| {
            self.records[index]
                .endings
                .iter()
                .any(|ending| ending.class == class && ending.state == TaskEndingState::Shown)
        });
        recorded
            || self
                .owed
                .iter()
                .any(|(owed_run, owed_class)| *owed_class == class && run.ids().any(|id| owed_run.names(id)))
    }

    /// The run's ending of `class`, resolved by the alias order — the entry's
    /// identity, label and duration fall back to it when the read no longer
    /// carries the launch (compaction, a V2 page cap).
    pub(crate) fn ending(&self, run: &TaskRunId, class: TaskEndingClass) -> Option<&TaskEnding> {
        let record = resolve_run(&self.records, run)?;
        self.records[record]
            .endings
            .iter()
            .find(|ending| ending.class == class)
    }

    /// The identity a run's record carries, whatever class recorded it — the
    /// label, the start clock and the ids an entry falls back to when the read
    /// no longer carries the launch. Resolved PER FIELD, newest first (ADR-0080,
    /// CI gate): a partial identity can never drop a field a fuller ending
    /// already captured — a launchless Wake recorded with a label but no start
    /// clock still leaves the Cleanup's start clock in place. `None` when no
    /// ending captured a label or a start clock (nothing to fall back to).
    pub(crate) fn identity(&self, run: &TaskRunId) -> Option<TaskRunIdentity> {
        let record = resolve_run(&self.records, run)?;
        let mut identity = TaskRunIdentity::default();
        for ending in self.records[record].endings.iter().rev() {
            let held = &ending.identity;
            if identity.tool.is_none() {
                identity.tool = held.tool.clone();
            }
            if identity.call_id.is_none() {
                identity.call_id = held.call_id.clone();
            }
            if identity.shell_id.is_none() {
                identity.shell_id = held.shell_id.clone();
            }
            if identity.child_id.is_none() {
                identity.child_id = held.child_id.clone();
            }
            if identity.started_at.is_none() {
                identity.started_at = held.started_at;
            }
            if identity.label.is_none() {
                identity.label = held.label.clone();
            }
        }
        (identity.label.is_some() || identity.started_at.is_some()).then_some(identity)
    }

    /// Fold a delivered-but-unrecorded body's pairs into the snapshot as SHOWN.
    pub(crate) fn note_owed(&mut self, shown: &[EndingKey]) {
        self.owed.extend_from_slice(shown);
    }
}

/// The persisted Chain Record; a wrapper so the on-disk shape can grow.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ChainRecordFile {
    #[serde(default)]
    records: HashMap<String, ChainRecord>,
    #[serde(default)]
    announcements: HashMap<String, WakeMark>,
    /// The pre-ADR-0080 `rendered` section (ADR-0073's amendment, issue #671):
    /// the synthetic announce keys a delivered card write carried. **Migration
    /// input only** — [`ChainRecords::load`] translates it into `endings`
    /// through [`translate_legacy_rendered`], it is never held in [`State`], and
    /// `skip_serializing` keeps it off disk so a write after the migration
    /// drops the section for good.
    #[serde(default, skip_serializing)]
    rendered: HashMap<String, Vec<String>>,
    /// session_id → the Session's run records, oldest first (ADR-0080).
    #[serde(default)]
    endings: HashMap<String, Vec<TaskRunRecord>>,
}

/// How a re-point updates the chain's durable reply target (#580/#582): the
/// carry-only rule a plain re-point needs, and the landing rule the
/// projection's create paths need — the record adopts the rung the create
/// actually landed on, `Set(None)` included.
#[derive(Clone, Copy)]
enum ReplyToUpdate<'a> {
    /// Carry the previous record's target: a re-point that learns no new one
    /// never loses the one it had.
    Keep,
    /// Write the caller's target; `Set(None)` clears it. In the SAME record
    /// write as the re-point, so no crash can leave a refused target behind
    /// (#582).
    Set(Option<&'a str>),
}

/// [`ChainRecords::track`]'s rule as a [`ReplyToUpdate`]: a known target
/// sets, an unknown one carries.
fn known_or_carry(reply_to: Option<&str>) -> ReplyToUpdate<'_> {
    match reply_to {
        Some(reply_to) => ReplyToUpdate::Set(Some(reply_to)),
        None => ReplyToUpdate::Keep,
    }
}

/// The durable reply target a track leaves on the new record (issue #580):
/// the update's value, one rule for all three re-point entries.
fn resolved_reply_to(previous: Option<&ChainRecord>, update: ReplyToUpdate<'_>) -> Option<String> {
    match update {
        ReplyToUpdate::Keep => previous.and_then(|previous| previous.reply_to.clone()),
        ReplyToUpdate::Set(reply_to) => reply_to.map(str::to_string),
    }
}

/// Follow a Session's re-key forwarding to its CURRENT id (issue #671, ticket
/// #672): a writer that captured the id before a recreate must still record
/// under the recreated Session. The visited set bounds a corrupt cycle without
/// capping the depth — a Session recreated repeatedly keeps forwarding past any
/// fixed limit.
fn resolved_session(state: &State, session_id: &str) -> String {
    let mut current = session_id;
    let mut seen = std::collections::HashSet::new();
    while seen.insert(current) {
        match state.renamed.get(current) {
            Some(next) => current = next,
            None => break,
        }
    }
    current.to_string()
}

/// Flip the delivered body's endings to SHOWN (ADR-0080) — the Rendered Cursor's
/// own half of the confirmation. The one funnel every record write that can
/// carry an entry runs through, so the bit always shares its write with the
/// cursor or the re-point it was confirmed with: a crash between two writes
/// would leave a card having shown an entry the durable record still owes, and
/// a restart would post it again. Reports whether anything changed; a pair the
/// section does not know is a no-op (a confirmation never mints a record), and
/// the flip is monotonic, so an older confirmation can never clear a newer bit.
fn push_shown(state: &mut State, session_id: &str, shown: &[EndingKey]) -> bool {
    if shown.is_empty() {
        return false;
    }
    // Record under the Session's CURRENT id: a writer that captured the id
    // before a recreate must still land on the recreated Session, never the dead
    // one.
    let session_id = resolved_session(state, session_id);
    let Some(records) = state.endings.get_mut(&session_id) else {
        return false;
    };
    mark_shown(records, shown)
}

/// Resolve `held` to the index of `records`' record for the SAME run, or `None`
/// (the caller mints one). Strongest identity first (ADR-0080): an exact run id;
/// else an exact **Wake** id already on a record; else the NEWEST record naming
/// any other id the writer holds (#552's newest-match rule). The first match
/// wins, so the arrival order of a launchless Wake and a launch-carrying
/// reconciliation read cannot matter — a writer mints only when nothing matched.
fn resolve_run(records: &[TaskRunRecord], held: &TaskRunId) -> Option<usize> {
    if let Some(index) = records.iter().position(|record| record.run.key == held.key) {
        return Some(index);
    }
    if let Some(wake_id) = held.wake_id.as_deref()
        && let Some(index) = records
            .iter()
            .position(|record| record.run.wake_id.as_deref() == Some(wake_id))
    {
        return Some(index);
    }
    records
        .iter()
        .rposition(|record| held.ids().any(|id| record.run.names(id)))
}

/// Whether an ending's entry is decided by the run's CLASS-AGNOSTIC gate — the
/// retirement's one-entry-per-run rule ([`EndingFacts::shown_any`]), so a shown
/// ending of ANY class gives this ending's entry up. The **Wake**'s completion
/// is the one exception (#590): its entry is per its OWN ending
/// ([`EndingFacts::shown`], class `Wake`), so a shown Cleanup never gives up a
/// later Wake's completion — which is why a Wake ending stays OWED beside a
/// shown one, and why its record stays unevictable until the Wake's own bit
/// lands.
fn gated_by_any_shown(class: TaskEndingClass) -> bool {
    !matches!(class, TaskEndingClass::Wake)
}

/// Record `ending` on the run `held` names (ADR-0080), inside the session's
/// oldest-first `records`: resolve the run by the alias order, or mint one keyed
/// by the writer's own id; fold in every id the writer holds; and keep AT MOST
/// ONE ending per class — a class the run already carries is the derivation
/// that already happened, so re-deriving it can never clear its shown bit.
/// Returns whether the section changed — a newly folded-in ALIAS counts even
/// when the ending's class was already present, because the alias itself must
/// reach the durable record.
fn insert_ending(records: &mut Vec<TaskRunRecord>, held: &TaskRunId, ending: TaskEnding) -> bool {
    let (index, learned) = match resolve_run(records, held) {
        Some(index) => {
            let before: Vec<String> = records[index].run.ids().map(str::to_string).collect();
            records[index].run.absorb(held);
            let learned = records[index]
                .run
                .ids()
                .any(|id| !before.iter().any(|known| known == id));
            (index, learned)
        }
        None => {
            records.push(TaskRunRecord {
                run: held.clone(),
                endings: Vec::new(),
            });
            (records.len() - 1, true)
        }
    };
    let record = &mut records[index];
    if record.endings.iter().any(|known| known.class == ending.class) {
        // The class is already recorded, but a newly learned ALIAS is still a
        // change: the write must land, or a restart resolves the run by that
        // alias no longer and mints a second record for one run.
        return learned;
    }
    record.endings.push(ending);
    // The owed state must AGREE with the class-agnostic entry gate (ADR-0080,
    // CI gate): a run already carrying a SHOWN ending renders one retirement
    // entry, whatever class wrote it, so no other ending of it can ever owe one.
    // Recording it OWED would leave it unevictable and let repeated races
    // consume the owed cap with no prolonged carrier failure — so it is
    // surrendered (the fact is kept; its entry was never renderable). Checking
    // the whole record after the push covers both arrival orders: a new ending
    // recorded after a shown one is surrendered, and a newly recorded SHOWN
    // ending gives up an OWED ending already on the run. A Wake is exempt
    // ([`gated_by_any_shown`], #590).
    if record
        .endings
        .iter()
        .any(|known| known.state == TaskEndingState::Shown)
    {
        for known in &mut record.endings {
            if known.state == TaskEndingState::Owed && gated_by_any_shown(known.class) {
                known.state = TaskEndingState::Surrendered;
            }
        }
    }
    true
}

/// Flip the entries `shown` names to [`TaskEndingState::Shown`] (ADR-0080) —
/// the confirmed card write's own half, riding the SAME record write as the
/// Rendered Cursor it was confirmed with. Additive and monotonic: it only ever
/// SETS a bit, so an older confirmation (a subset of a newer one's, or an empty
/// one) can never clear a newer shown bit. The SAME write also surrenders the
/// run's other OWED endings whose entry the class-agnostic gate has just given
/// up ([`gated_by_any_shown`]) — never a shown bit, never a Wake. Returns
/// whether anything changed.
fn mark_shown(records: &mut [TaskRunRecord], shown: &[(TaskRunId, TaskEndingClass)]) -> bool {
    let mut changed = false;
    for (run, class) in shown {
        let Some(index) = resolve_run(records, run) else {
            continue;
        };
        let record = &mut records[index];
        for ending in &mut record.endings {
            if ending.class == *class && ending.state != TaskEndingState::Shown {
                ending.state = TaskEndingState::Shown;
                changed = true;
            }
        }
        // The confirmed write has just SHOWN this run's entry, so the
        // class-agnostic gate now covers the run: every OTHER owed ending's
        // entry is given up in this SAME write, or it would stay owed —
        // unevictable, consuming the owed cap — while the gate suppresses its
        // entry forever. Only OWED endings move; a shown bit is never cleared,
        // and a Wake's own entry is exempt ([`gated_by_any_shown`], #590).
        if record
            .endings
            .iter()
            .any(|ending| ending.state == TaskEndingState::Shown)
        {
            for ending in &mut record.endings {
                if ending.class != *class
                    && ending.state == TaskEndingState::Owed
                    && gated_by_any_shown(ending.class)
                {
                    ending.state = TaskEndingState::Surrendered;
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Bound one Session's run records (ADR-0080). The saturation cap first: past
/// [`MAX_OWED_RUN_RECORDS`] the OLDEST owed ending becomes
/// [`TaskEndingState::Surrendered`] with a `WARN` — the fact is kept (the run
/// stays ended, no card resurrects it) and only its entry is given up, so an
/// unbounded backlog can never grow the sidecar — which is also what makes its
/// record ordinary eviction material below. Then the shared bound: the OLDEST
/// record with no owed ending goes first, and an owed record is NEVER the
/// eviction candidate.
fn bound_run_records(session_id: &str, records: &mut Vec<TaskRunRecord>) -> bool {
    let mut changed = false;
    // Recompute the count of OWING RECORDS each time: one record may carry
    // several owed classes, so a single surrender does not necessarily make it
    // stop owing. Decrementing once per ending surrendered (rather than once
    // per record that stops owing) would end the loop early and leave more than
    // `MAX_OWED_RUN_RECORDS` records owing.
    while records.iter().filter(|record| record.owes()).count() > MAX_OWED_RUN_RECORDS {
        let Some(record) = records.iter_mut().find(|record| record.owes()) else {
            break;
        };
        let Some(ending) = record
            .endings
            .iter_mut()
            .find(|ending| ending.state == TaskEndingState::Owed)
        else {
            break;
        };
        ending.state = TaskEndingState::Surrendered;
        tracing::warn!(
            session = session_id,
            run = record.run.key,
            class = ?ending.class,
            "Background Task Ending backlog past {MAX_OWED_RUN_RECORDS}: the oldest owed entry is surrendered, its ending kept"
        );
        changed = true;
    }
    let mut excess = records.len().saturating_sub(MAX_RUN_RECORDS);
    while excess > 0 {
        let Some(index) = records.iter().position(|record| !record.owes()) else {
            break;
        };
        records.remove(index);
        excess -= 1;
        changed = true;
    }
    changed
}

/// Translate the `rendered` section's legacy keys into the `endings` section
/// ONCE, at load (ADR-0080): `ended:<call_id>` is an evidence ending,
/// `cleanup:<call_id>` the user's own **Cleanup**, and the pre-#659
/// `runtime:<call_id>` a [`TaskEndingClass::Legacy`] ending the retirement's own
/// dedup reads — never the Wake gate (#590). Every key was written by a
/// DELIVERED card write, so its translated ending is `shown`: a restart must not
/// re-post the entry. A key this build does not know is dropped, and a key that
/// resolves a run the file's own `endings` section already carries merges into
/// it instead of minting a second record.
fn translate_legacy_rendered(rendered: &HashMap<String, Vec<String>>) -> HashMap<String, Vec<TaskRunRecord>> {
    let mut endings: HashMap<String, Vec<TaskRunRecord>> = HashMap::new();
    for (session_id, keys) in rendered {
        let records = endings.entry(session_id.clone()).or_default();
        for key in keys {
            let Some((class, call_id)) = legacy_ending_key(key) else {
                continue;
            };
            let run = TaskRunId {
                key: call_id.clone(),
                call_id: Some(call_id),
                shell_id: None,
                child_id: None,
                job_id: None,
                wake_id: None,
            };
            // Every legacy key was written by a DELIVERED card write.
            let ending = TaskEnding {
                state: TaskEndingState::Shown,
                ..TaskEnding::owed(class, TaskRunIdentity::default())
            };
            insert_ending(records, &run, ending);
        }
        bound_run_records(session_id, records);
    }
    endings
}

/// The class and call id a legacy `rendered` key names (ADR-0080) — the three
/// spellings `src/bridge/turn/state.rs` used to mint, read once here as the
/// section it wrote is translated. `None` for a key this build does not know.
fn legacy_ending_key(key: &str) -> Option<(TaskEndingClass, String)> {
    if let Some(call_id) = key.strip_prefix("ended:") {
        return Some((TaskEndingClass::Evidence, call_id.to_string()));
    }
    if let Some(call_id) = key.strip_prefix("cleanup:") {
        return Some((TaskEndingClass::Cleanup, call_id.to_string()));
    }
    key.strip_prefix("runtime:")
        .map(|call_id| (TaskEndingClass::Legacy, call_id.to_string()))
}

/// The pre-ADR-0069 shape, read once when `chain_records.json` is absent and
/// never written again. The explicit `bound` keeps serde's `#[serde(default)]`
/// inference from demanding `T: Default` (only the map is defaulted).
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct LegacySessions<T> {
    #[serde(default)]
    sessions: HashMap<String, T>,
}

impl<T> Default for LegacySessions<T> {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }
}

/// The in-memory mirror of the Chain Record, under one lock: every mutation
/// happens and persists under it, so the two sections can never be observed
/// half-written against each other.
struct State {
    records: HashMap<String, ChainRecord>,
    announcements: HashMap<String, WakeMark>,
    /// card_message_id → the endings a recoverably-failed body still owes the
    /// durable Record, waiting on the Pending Card Update payload at
    /// [`OwedRendered::seq`] (ADR-0080). In-memory only, and held here rather
    /// than on an accumulator on purpose: a card's accumulator can be replaced,
    /// removed or re-keyed between the failure and the delivery, and the pairs
    /// are this fact's only copy. Keyed by the card, whose id is stable across
    /// all of that; the Session id it names is corrected by
    /// [`ChainRecords::rename`].
    owed: HashMap<String, OwedRendered>,
    /// A recreated Session's OLD id → its new one (issue #671, ticket #672):
    /// in-memory forwarding so a concurrent writer that captured the id before
    /// [`ChainRecords::rename`] still lands under the recreated Session, never
    /// the dead one. Not persisted — the rename moved the durable facts, and no
    /// writer outlives the process.
    renamed: HashMap<String, String>,
    /// session_id → the Session's run records, oldest first (ADR-0080): one
    /// record per [`TaskRunId`], each carrying at most one ending per class.
    /// Insertion order is what the bound evicts from — the OLDEST record with no
    /// owed ending first. Session-scoped like the announcements: `release`
    /// drops only the chain's record.
    endings: HashMap<String, Vec<TaskRunRecord>>,
}

/// The most run records one Session retains (ADR-0080) — the shared bound the
/// pre-ADR `rendered` marks' own bound (64) becomes, exactly as many as the
/// marks it replaces: one run record is what a retirement key used to name. The
/// eviction candidate is the OLDEST record with no owed ending, so an ending a
/// card still owes is never lost; past it a very old SHOWN ending may render
/// once more after a restart, the same duplicate-over-loss side the family
/// trades toward.
const MAX_RUN_RECORDS: usize = 64;

/// The most run records one Session may hold an OWED ending for (ADR-0080):
/// past this the OLDEST owed ending becomes [`TaskEndingState::Surrendered`]
/// with a `WARN` — the family's one ratified loss, taken so a carrier the
/// platform permanently refuses while run after run keeps ending cannot grow
/// the sidecar without bound. The ending itself is kept (the run stays ended;
/// no card resurrects it); only its entry is given up. Strictly below
/// [`MAX_RUN_RECORDS`], so the total bound always has a non-owed record left to
/// evict.
const MAX_OWED_RUN_RECORDS: usize = 48;

/// The write-through mirror of the Chain Record. Locked independently of the
/// cards and the session store: every reader (the card paths that track a
/// live card, the reap, the Fresh gate) reaches it through `CardsHandle`, and
/// no lock ordering against `cards.cards` matters because every write is a
/// plain synchronous file write.
pub(crate) struct ChainRecords {
    path: PathBuf,
    state: Mutex<State>,
    /// Test-only: how often the sidecar was actually persisted (spec #561's
    /// write-cost measurement). The counter exists only in test builds, so
    /// production pays nothing for the measurement.
    #[cfg(test)]
    writes: std::sync::atomic::AtomicUsize,
}

impl ChainRecords {
    /// Load the Chain Record — from `chain_records.json` when it exists, else
    /// by folding the legacy `live_cards.json` / `wake_watermarks.json` beside
    /// it (ADR-0069's one-time migration). A missing or corrupt file is an
    /// empty record, never an error: a lost record only means a restart reaps
    /// nothing and a Fresh path may re-announce once.
    pub(crate) fn load(path: PathBuf) -> Self {
        let folded = !path.exists();
        let state = if folded {
            let records = sidecar::load::<LegacySessions<ChainRecord>>(
                &path.with_file_name("live_cards.json"),
                "live-card record",
            )
            .sessions;
            let announcements = sidecar::load::<LegacySessions<WakeMark>>(
                &path.with_file_name("wake_watermarks.json"),
                "Wake watermark",
            )
            .sessions;
            State {
                records,
                announcements,
                owed: HashMap::new(),
                renamed: HashMap::new(),
                endings: HashMap::new(),
            }
        } else {
            let file = sidecar::load::<ChainRecordFile>(&path, "Chain Record");
            // The legacy `rendered` marks are the OLDER shape, so they are
            // translated first and the file's own `endings` merge into them
            // (ADR-0080): the section keeps its oldest-first order, a run the
            // marks already name resolves instead of being minted a second time,
            // and a class both shapes carry stays the derivation the marks
            // already recorded. The translated section is never carried into
            // [`State`]: this read is the migration's only consumer.
            let mut endings = translate_legacy_rendered(&file.rendered);
            for (session_id, records) in file.endings {
                if records.is_empty() {
                    continue;
                }
                let kept = endings.entry(session_id.clone()).or_default();
                for record in records {
                    for ending in record.endings {
                        insert_ending(kept, &record.run, ending);
                    }
                }
                bound_run_records(&session_id, kept);
            }
            State {
                records: file.records,
                announcements: file.announcements,
                endings,
                owed: HashMap::new(),
                renamed: HashMap::new(),
            }
        };
        let store = Self {
            path,
            state: Mutex::new(state),
            #[cfg(test)]
            writes: std::sync::atomic::AtomicUsize::new(0),
        };
        if folded {
            // Materialize the marker now: the folded state is what the legacy
            // files held, and the first load is what settles the migration, not
            // the first later mutation. The subsequent loads never look back.
            let state = store.lock();
            store.write(&state);
        }
        store
    }

    /// The session's record, when its card is (believed to be) live. The test
    /// seam: production reads the whole record through [`Self::entries`] (the
    /// reap's pass) or the previous value [`Self::track`] returns.
    #[cfg(test)]
    pub(crate) fn get(&self, session_id: &str) -> Option<ChainRecord> {
        self.lock().records.get(session_id).cloned()
    }

    /// Every record, as one snapshot — Session Sync's reap iterates this so a
    /// record whose session is no longer mapped is still reconciled.
    pub(crate) fn entries(&self) -> Vec<(String, ChainRecord)> {
        self.lock()
            .records
            .iter()
            .map(|(session_id, card)| (session_id.clone(), card.clone()))
            .collect()
    }

    /// The chain's **current** Chain Generation (spec #571, ticket #574) — the
    /// version a card write submitted now must carry, read at submission so a
    /// takeover that landed since the caller's own snapshot (a new collect)
    /// outranks that snapshot's older generation instead of being refused by
    /// it. A released chain has no version left, and no writer of it remains;
    /// zero is what a fresh chain starts at.
    pub(crate) fn generation(&self, session_id: &str) -> u64 {
        self.lock()
            .records
            .get(session_id)
            .map_or(0, |record| record.generation)
    }

    /// The one creation/re-point entry: make the record naming
    /// `card_message_id` the session's record from the current Turn facts and
    /// the Session's directory, whether that card opens a chain, continues it
    /// or takes it over from a predecessor — so the same facts can never build
    /// two different records. Returns the previous record, the predecessor a
    /// takeover acts on. Best-effort: a failed write leaves the in-memory
    /// record correct (the next write retries the file), which at worst means
    /// this card is not reaped by the next restart.
    ///
    /// `reply_to` is the chain's new durable reply target (issue #580), when
    /// the caller knows one; `None` carries the previous record's — a re-point
    /// that learns no new target never loses the one it had. A create that
    /// landed must use [`Self::track_landing`] instead: it must be able to
    /// clear the target explicitly (#582).
    pub(crate) fn track(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
        reply_to: Option<&str>,
    ) -> Option<ChainRecord> {
        self.track_with(
            session_id,
            card_message_id,
            message_id,
            created_ms,
            directory,
            known_or_carry(reply_to),
            &[],
        )
    }

    /// [`Self::track`] for a create that LANDED on its rung (#582): the chain's
    /// durable reply target becomes exactly what the create reached —
    /// `Some(id)` for a reply target, `None` for the top level (an explicit
    /// clear) — in the SAME record write as the re-point. The projection's
    /// landing paths use this so a crash can never leave the record naming a
    /// target the landing just refused; the ladder's bounded fall-through
    /// stays as defense in depth for records older builds wrote. `shown` is
    /// the delivered body's endings (ADR-0080): they ride this same write, so a
    /// takeover whose staged cursor was evicted can never leave the shown bit
    /// apart from the re-point.
    #[allow(clippy::too_many_arguments)] // the re-point's whole fixture
    pub(crate) fn track_landing(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
        reply_to: Option<&str>,
        shown: &[EndingKey],
    ) -> Option<ChainRecord> {
        self.track_with(
            session_id,
            card_message_id,
            message_id,
            created_ms,
            directory,
            ReplyToUpdate::Set(reply_to),
            shown,
        )
    }

    /// [`Self::track_landing`] for a continuation whose reply target follows
    /// [`Self::track`]'s known-or-carry rule, carrying the delivered body's
    /// endings in the SAME write (ADR-0080): a continuation create whose staged
    /// cursor was evicted still shows what its create delivered, atomically
    /// with the re-point.
    #[allow(clippy::too_many_arguments)] // the re-point's whole fixture
    pub(crate) fn track_carrying_rendered(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
        reply_to: Option<&str>,
        shown: &[EndingKey],
    ) -> Option<ChainRecord> {
        self.track_with(
            session_id,
            card_message_id,
            message_id,
            created_ms,
            directory,
            known_or_carry(reply_to),
            shown,
        )
    }

    #[allow(clippy::too_many_arguments)] // the record write's whole fixture
    fn track_with(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
        reply_to: ReplyToUpdate<'_>,
        shown: &[EndingKey],
    ) -> Option<ChainRecord> {
        let mut state = self.lock();
        // A re-point within the chain carries the Rendered Cursor (spec #561):
        // a split continuation or a new Turn on the same chain must not clear
        // the chain's frontier — only a genuinely new chain (no previous
        // record) starts cursorless.
        let previous = state.records.get(session_id).cloned();
        let mut record = ChainRecord::new(card_message_id, message_id, created_ms)
            .with_directory(directory.map(str::to_string));
        record.generation = chain_generation(previous.as_ref(), &record.card_message_id);
        record.reply_to = resolved_reply_to(previous.as_ref(), reply_to);
        if let Some(previous) = &previous {
            record.cursor = previous.cursor.clone();
            // The owed orphan gap is chain-level too (spec #561, review #569):
            // a split continuation, a Wake continuation or a fresh Turn's
            // takeover must not drop the tail a later read still owes. The
            // projection's own re-point confirms its coverage separately —
            // consuming the fact when the body reached the gap's end.
            record.pending_gap = previous.pending_gap.clone();
            // So is the projection's write-ahead intent (spec #561, review
            // #569): a create it marked may have landed with its takeover lost
            // to this re-point, and a chain that drops the mark would let a
            // later restart post that tail a second time. The projection's own
            // take-over consumes it (a fresh record without the mark, or an
            // explicit clear on the loser's late-card collect).
            record.projection_intent = previous.projection_intent;
        }
        state.records.insert(session_id.to_string(), record);
        // The delivered body's endings ride this SAME write (ADR-0080): a crash
        // must never leave a shown bit apart from the re-point that confirmed
        // it.
        push_shown(&mut state, session_id, shown);
        self.write(&state);
        previous
    }

    /// A fresh Turn's TAKEOVER (spec #561, review #569): re-point the record
    /// onto the new card AND record the owed orphan gap in ONE write — the
    /// facts are known here, before any transcript read: the previous record's
    /// cursor and anchor are the orphaned Turn's frontier and window, and the
    /// new Turn's message is the window's bound. Nothing may depend on the
    /// later read to become owed: a crash while that read is pending would
    /// otherwise leave a record that settles Unreceived and releases the
    /// orphan's undelivered tail. The previous cursor carries, the previous
    /// gap wins when there is one (its cursor may have advanced past the
    /// derived frontier), and an unresolved write-ahead intent carries too.
    /// `reply_to` follows [`Self::track`]'s rule: the new Turn's own target
    /// when it has one, else the previous record's.
    pub(crate) fn track_takeover(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
        reply_to: Option<&str>,
    ) -> Option<ChainRecord> {
        let mut state = self.lock();
        let previous = state.records.get(session_id).cloned();
        let mut record = ChainRecord::new(card_message_id, message_id.clone(), created_ms)
            .with_directory(directory.map(str::to_string));
        record.generation = chain_generation(previous.as_ref(), &record.card_message_id);
        record.reply_to = resolved_reply_to(previous.as_ref(), known_or_carry(reply_to));
        if let Some(previous) = &previous {
            record.cursor = previous.cursor.clone();
            record.pending_gap = previous.pending_gap.clone().or_else(|| {
                Some(PendingGap {
                    cursor: previous.cursor.clone()?,
                    anchor: previous.anchor()?,
                    bound: Some(message_id.clone()),
                })
            });
            record.projection_intent = previous.projection_intent;
        }
        state.records.insert(session_id.to_string(), record);
        self.write(&state);
        previous
    }

    /// Consume the write-ahead intent whatever card the session's record now
    /// names (spec #561, review #569): a projection that LOST its create
    /// window to a fresh Turn collects its late card here, and the winning
    /// chain — which carried the unresolved intent through its own takeover —
    /// must not keep it, or a later restart could project the late card's tail
    /// again.
    pub(crate) fn clear_projection_intent_any(&self, session_id: &str) -> bool {
        let mut state = self.lock();
        let Some(card) = state.records.get_mut(session_id) else {
            return false;
        };
        if !card.projection_intent {
            return false;
        }
        card.projection_intent = false;
        self.write(&state);
        true
    }

    /// Re-point the session's record AND set its Rendered Cursor in ONE chains
    /// critical section (spec #561, review #569): a projection's atomic
    /// takeover. A separate [`Self::track`] + [`Self::advance_cursor`] would
    /// leave the record naming the successor card with the PREDECESSOR's
    /// frontier in between, and a fresh Turn that snapshots the record in that
    /// window would seed from the stale cursor and re-render the successor's
    /// already-delivered tail onto its own card — while the collected successor
    /// keeps its body: the same text twice. Returns the previous record,
    /// exactly like [`Self::track`]. `reply_to` follows [`Self::track`]'s rule;
    /// a create that landed uses [`Self::track_carrying_cursor_landing`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn track_carrying_cursor(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
        reply_to: Option<&str>,
        cursor: &RenderedCursor,
        gap: Option<(&CursorFrontier, bool)>,
        shown: &[EndingKey],
    ) -> Option<ChainRecord> {
        self.track_carrying_cursor_with(
            session_id,
            card_message_id,
            message_id,
            created_ms,
            directory,
            known_or_carry(reply_to),
            cursor,
            gap,
            shown,
        )
    }

    /// [`Self::track_carrying_cursor`] for a create that LANDED on its rung
    /// (#582): the confirmed cursor AND the landing's reply target — an
    /// explicit `None` clears — ride the re-point in the same record write.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn track_carrying_cursor_landing(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
        reply_to: Option<&str>,
        cursor: &RenderedCursor,
        gap: Option<(&CursorFrontier, bool)>,
        shown: &[EndingKey],
    ) -> Option<ChainRecord> {
        self.track_carrying_cursor_with(
            session_id,
            card_message_id,
            message_id,
            created_ms,
            directory,
            ReplyToUpdate::Set(reply_to),
            cursor,
            gap,
            shown,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn track_carrying_cursor_with(
        &self,
        session_id: &str,
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
        directory: Option<&str>,
        reply_to: ReplyToUpdate<'_>,
        cursor: &RenderedCursor,
        gap: Option<(&CursorFrontier, bool)>,
        shown: &[EndingKey],
    ) -> Option<ChainRecord> {
        let mut state = self.lock();
        let previous = state.records.get(session_id).cloned();
        let mut record = ChainRecord::new(card_message_id, message_id, created_ms)
            .with_directory(directory.map(str::to_string));
        record.generation = chain_generation(previous.as_ref(), &record.card_message_id);
        record.reply_to = resolved_reply_to(previous.as_ref(), reply_to);
        record.cursor = Some(cursor.clone());
        // The gap rides the re-point exactly as it does through [`Self::track`]
        // (spec #561, review #569), and the confirmed body's coverage settles
        // it in this SAME record write: the cursor and the gap can never be
        // observable apart — a crash between two updates would leave the
        // cursor ahead of the gap, and the recovery would re-render gap
        // content a card already showed.
        record.pending_gap = previous
            .as_ref()
            .and_then(|previous| previous.pending_gap.clone());
        match (record.pending_gap.as_mut(), gap) {
            // A body that reached the gap's end consumes it with the re-point.
            (Some(_), Some((_, true))) => record.pending_gap = None,
            // A partial body advances the gap past what the successor shows.
            (Some(recorded), Some((frontier, false))) => {
                recorded.cursor.frontier = Some(frontier.clone());
            }
            _ => {}
        }
        state.records.insert(session_id.to_string(), record);
        // The entries this landed body carried leave their shown bits in the
        // SAME write as the re-point (ADR-0080): a crash between two writes
        // could drop the bit and let the entry replay on a later card.
        push_shown(&mut state, session_id, shown);
        self.write(&state);
        previous
    }

    /// The one removal: drop the session's record — the card reached a
    /// terminal or was collected, so nothing is owed a reap. Its announcement,
    /// if any, stays: the watermark outlives the record it was announced on.
    pub(crate) fn release(&self, session_id: &str) {
        let mut state = self.lock();
        if state.records.remove(session_id).is_some() {
            self.write(&state);
        }
    }

    /// Drop `session_id`'s record only while it still names
    /// `card_message_id` (spec #561, review #569): the ending that just landed
    /// belongs to that card, and a chain that moved on meanwhile — a fresh Turn
    /// taking the session over — keeps its own record. Returns whether a
    /// record was released.
    pub(crate) fn release_if_card(&self, session_id: &str, card_message_id: &str) -> bool {
        let mut state = self.lock();
        if state
            .records
            .get(session_id)
            .is_none_or(|record| record.card_message_id != card_message_id)
        {
            return false;
        }
        state.records.remove(session_id);
        self.write(&state);
        true
    }

    /// Re-key a record when the session it names is recreated under a fresh id
    /// (the 404 recreate): the card is still live, only its session id moved.
    /// The Session's announcement moves with it — the recreated id names the
    /// same logical Session, so a Wake the old id announced must not be
    /// re-announced under the new one — and so do the run records its endings
    /// live on (ADR-0080): the recreated id is the same logical Session, so a
    /// retirement it already showed must not replay under the new one.
    pub(crate) fn rename(&self, from: &str, to: &str) {
        let mut state = self.lock();
        // Forward the old id so a concurrent writer that captured it before this
        // rename still lands under the recreated Session (issue #671, ticket
        // #672). Also re-point the owed mirrors' Session.
        state.renamed.insert(from.to_string(), to.to_string());
        for owed in state.owed.values_mut() {
            if owed.session_id == from {
                owed.session_id = to.to_string();
            }
        }
        let record = state.records.remove(from);
        let announcement = state.announcements.remove(from);
        // The run records move with the logical Session too (ADR-0080): the
        // recreated id names the same Session, so an ending (and its shown bit)
        // must not replay under it.
        let endings = state.endings.remove(from);
        if record.is_none() && announcement.is_none() && endings.is_none() {
            return;
        }
        if let Some(record) = record {
            state.records.insert(to.to_string(), record);
        }
        if let Some(announcement) = announcement {
            state.announcements.insert(to.to_string(), announcement);
        }
        if let Some(endings) = endings {
            state.endings.insert(to.to_string(), endings);
        }
        self.write(&state);
    }

    /// Refresh the anchor of the record naming `card_message_id` — the render
    /// path's write once a read carries the submitted message's server time.
    /// A no-op when the record names another card (the chain moved on, a
    /// successor owns the session) or the anchor is already current, so the
    /// frequent render polls do not rewrite the file.
    pub(crate) fn set_anchor(&self, session_id: &str, card_message_id: &str, anchor: &TurnAnchor) {
        let mut state = self.lock();
        let Some(card) = state.records.get_mut(session_id) else {
            return;
        };
        if card.card_message_id != card_message_id
            || (card.message_id == anchor.message_id && card.created_ms == Some(anchor.created_ms))
        {
            return;
        }
        card.message_id = anchor.message_id.clone();
        card.created_ms = Some(anchor.created_ms);
        self.write(&state);
    }

    /// The session's Rendered Cursor, when its record carries one (spec #561).
    /// `None` for a missing record or a cursorless one — a legacy record or a
    /// chain whose first confirmed write has not landed. The projection reads
    /// it at apply time (ticket #563): the record snapshot the reap took
    /// before its server reads can be older than a concurrent confirmed write.
    pub(crate) fn cursor(&self, session_id: &str) -> Option<RenderedCursor> {
        self.lock()
            .records
            .get(session_id)
            .and_then(|record| record.cursor.clone())
    }

    /// Whether the session has a durable Chain Record at all — the Fresh
    /// gate's one read (spec #561, ticket #566). A recorded chain's Wake
    /// belongs to the projection (or, cursorless, to the reap's fallback),
    /// so only a recordless post uses the Wake Watermark's announcement rule.
    pub(crate) fn recorded(&self, session_id: &str) -> bool {
        self.lock().records.contains_key(session_id)
    }

    /// Advance the Rendered Cursor of the record naming `card_message_id`
    /// (spec #561) with no gap riding along — the test seam and the seeding
    /// helpers' form of [`Self::advance_cursor_and_gap`].
    #[cfg(test)]
    pub(crate) fn advance_cursor(&self, session_id: &str, card_message_id: &str, cursor: &RenderedCursor) {
        self.advance_cursor_and_gap(session_id, card_message_id, cursor, None, &[]);
    }

    /// Advance the Rendered Cursor AND settle the orphan gap in ONE record
    /// write (spec #561, review #569): two separate updates would leave a
    /// crash window between them — the cursor ahead of the gap — and the
    /// recovery would re-render gap content a card already showed. `gap` is
    /// `Some((frontier, complete))` when the confirmed body carried gap
    /// content: a partial coverage advances the gap's cursor (so a restart
    /// resumes after what a card showed and repeats nothing), and a body that
    /// reached the gap's end consumes the durable fact. Best-effort and
    /// atomic, like every record write.
    pub(crate) fn advance_cursor_and_gap(
        &self,
        session_id: &str,
        card_message_id: &str,
        cursor: &RenderedCursor,
        gap: Option<(&CursorFrontier, bool)>,
        shown: &[EndingKey],
    ) {
        let mut state = self.lock();
        let mut cursor_change = false;
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                let cursor_current = card.cursor.as_ref() == Some(cursor);
                let gap_change = match (card.pending_gap.as_mut(), gap) {
                    (Some(recorded), Some((frontier, complete))) => {
                        if complete {
                            card.pending_gap = None;
                            true
                        } else if recorded.cursor.frontier.as_ref() == Some(frontier) {
                            false
                        } else {
                            recorded.cursor.frontier = Some(frontier.clone());
                            true
                        }
                    }
                    _ => false,
                };
                if !cursor_current || gap_change {
                    card.cursor = Some(cursor.clone());
                    cursor_change = true;
                }
            }
            // No record naming this card: the cursor and the gap are
            // record-scoped, so their advance is dropped exactly as before (a
            // confirmation for a record that moved on). The body's rendered
            // marks are SESSION-scoped and still land below — they outlive the
            // record, like the Wake Watermark.
            _ => {}
        }
        // The body's completion entries ride the SAME write as the cursor and
        // the gap it confirmed (ADR-0080): a crash between two writes would
        // leave the card having shown the entry with its bit still owed, and a
        // restart would re-post it.
        let shown_change = push_shown(&mut state, session_id, shown);
        if !cursor_change && !shown_change {
            return;
        }
        self.write(&state);
    }

    /// Mark that `card_message_id`'s waiting ending was already PATCHed, so
    /// the reap does not repeat it every tick. In-memory only: a restart
    /// re-PATCHes once, which is idempotent. Returns whether the record still
    /// names that card (a stale mark for a replaced card is dropped).
    pub(crate) fn mark_waiting_reaped(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_reap_flag(session_id, card_message_id, |card| card.waiting_reaped = true)
    }

    /// Mark that `card_message_id`'s projection already attempted its successor
    /// create in this process life (spec #561, review #569): Feishu has no
    /// idempotency key (ADR-0067), so the create is single-shot — a later pass
    /// must state-repair the old card instead of re-posting a successor that
    /// may already have landed. In-memory only; a record rewritten for a new
    /// card starts unmarked. Returns whether the record still names that card.
    pub(crate) fn mark_projection_attempted(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_reap_flag(session_id, card_message_id, |card| {
            card.projection_attempted = true
        })
    }

    /// Persist the projection's write-ahead intent BEFORE its successor create
    /// (spec #561, review #569): a process that dies between the create and the
    /// takeover's re-point leaves this durable single-shot fact behind, so the
    /// next life treats the create as ambiguous and never re-posts. Scoped to
    /// the record naming `card_message_id`: a later re-point rewrites the
    /// record, which starts unmarked.
    pub(crate) fn note_projection_intent(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_projection_intent(session_id, card_message_id, true)
    }

    /// Clear the write-ahead intent once a DEFINITE non-delivery proves the
    /// platform created no message, so a later pass or restart may retry
    /// (spec #561, review #569). Scoped to the record naming `card_message_id`,
    /// exactly like [`Self::note_projection_intent`].
    pub(crate) fn clear_projection_intent(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_projection_intent(session_id, card_message_id, false)
    }

    /// The shared body of the write-ahead intent's set/clear: apply it while
    /// the record still names `card_message_id` and persist the change. A value
    /// already current is left alone, so repeated notes never rewrite the file.
    /// Returns whether the record was the named one.
    fn set_projection_intent(&self, session_id: &str, card_message_id: &str, intent: bool) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                if card.projection_intent != intent {
                    card.projection_intent = intent;
                    self.write(&state);
                }
                true
            }
            _ => false,
        }
    }

    /// Persist the orphan gap a takeover owes (spec #561, review #569), scoped
    /// to the record naming `card_message_id`: the fresh Turn's failed seed and
    /// a carried predecessor's gap both record it here, so a restart renders
    /// exactly the gap and nothing the old cards already showed.
    pub(crate) fn note_pending_gap(&self, session_id: &str, card_message_id: &str, gap: &PendingGap) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                if card.pending_gap.as_ref() != Some(gap) {
                    card.pending_gap = Some(gap.clone());
                    self.write(&state);
                }
                true
            }
            _ => false,
        }
    }

    /// Record how far a confirmed write carried the chain's pending orphan gap
    /// (spec #561, review #569), scoped like [`Self::note_pending_gap`]: a body
    /// that stopped mid-gap ADVANCES the gap's cursor — so a restart (or the
    /// next slice) resumes after the content a card already showed instead of
    /// re-rendering it — while a body that reached the gap's end consumes the
    /// durable fact. Production confirmations go through the ONE-write
    /// [`Self::advance_cursor_and_gap`]; this is the store tests' seam.
    #[cfg(test)]
    pub(crate) fn advance_pending_gap(
        &self,
        session_id: &str,
        card_message_id: &str,
        frontier: &CursorFrontier,
        complete: bool,
    ) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                let Some(gap) = card.pending_gap.as_mut() else {
                    return false;
                };
                if complete {
                    card.pending_gap = None;
                } else {
                    gap.cursor.frontier = Some(frontier.clone());
                }
                self.write(&state);
                true
            }
            _ => false,
        }
    }

    /// Set `flag` on the record naming `card_message_id` — the shared body of
    /// the record-scoped flag mutations (the reap's one-per-life marks). Returns
    /// whether the record still names that card (a stale mutation for a
    /// replaced card is dropped).
    fn set_reap_flag(
        &self,
        session_id: &str,
        card_message_id: &str,
        flag: impl FnOnce(&mut ChainRecord),
    ) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                flag(card);
                true
            }
            _ => false,
        }
    }

    /// The session's mark, when one exists — the Fresh path's gate: a Wake at
    /// or below it was already announced.
    pub(crate) fn announced(&self, session_id: &str) -> Option<WakeMark> {
        self.lock().announcements.get(session_id).cloned()
    }

    /// Advance the session's mark to `(wake_id, created_ms)` and persist, when
    /// strictly newer than the current one. Best-effort: a failed write leaves
    /// the in-memory mark advanced (the next advance retries the file), which
    /// suppresses at most the announcement whose write raced the crash — the
    /// same duplicate-over-loss trade the drain itself makes.
    pub(crate) fn advance(&self, session_id: &str, wake_id: &str, created_ms: i64) {
        let mut state = self.lock();
        if state
            .announcements
            .get(session_id)
            .is_some_and(|mark| mark.created_ms >= created_ms)
        {
            return;
        }
        state.announcements.insert(
            session_id.to_string(),
            WakeMark {
                wake_id: wake_id.to_string(),
                created_ms,
            },
        );
        self.write(&state);
    }

    /// Record the endings a DELIVERED card write carried, for the one landed
    /// body with no record write to ride (ADR-0080) — a projection takeover
    /// whose armed session a fresh Turn replaced meanwhile. The delivered
    /// body's endings normally ride [`Self::advance_cursor_and_gap`] /
    /// [`Self::track_carrying_cursor`], so they land in the SAME write as the
    /// cursor they were confirmed with; this plain form is that path's
    /// fallback, session-scoped like the endings themselves. Returns whether
    /// anything changed.
    pub(crate) fn record_shown(&self, session_id: &str, shown: &[EndingKey]) -> bool {
        let mut state = self.lock();
        if push_shown(&mut state, session_id, shown) {
            self.write(&state);
            return true;
        }
        false
    }

    /// Record one **Task Ending** (ADR-0080), resolving its run first — an exact
    /// run id; else an exact Wake id already on a record; else the NEWEST record
    /// naming any other id the writer holds; else a fresh record keyed by the
    /// writer's own id — so two writers naming one run differently can never
    /// make two records for it. The first derivation of a class is the fact: a
    /// class the run already carries is not recorded again, and re-deriving it
    /// can never clear its shown bit. The run's ids all ride the resolved
    /// record. One synchronous Chain Record write at derivation, the fact about
    /// the run rather than about the card that will show it. Returns whether the
    /// section changed.
    pub(crate) fn record_ending(&self, session_id: &str, run: &TaskRunId, ending: TaskEnding) -> bool {
        let mut state = self.lock();
        let session_id = resolved_session(&state, session_id);
        let records = state.endings.entry(session_id.clone()).or_default();
        let changed = insert_ending(records, run, ending) | bound_run_records(&session_id, records);
        if changed {
            self.write(&state);
        }
        changed
    }

    /// Flip the entries `shown` names — a `(run, class)` pair each — to SHOWN
    /// (ADR-0080), the confirmed card write's own half: a PATCH `Ok`, a create
    /// `Ok`, or a queued payload the drain delivered. Monotonic and additive, so
    /// an older confirmation can never clear a newer shown bit. Returns whether
    /// the section changed. The confirmed-write paths flip bits through
    /// [`Self::advance_cursor_and_gap`] / [`Self::record_shown`] and the
    /// `track_*` re-points; this is the store's own seam beside them.
    #[allow(dead_code)]
    pub(crate) fn mark_endings_shown(
        &self,
        session_id: &str,
        shown: &[(TaskRunId, TaskEndingClass)],
    ) -> bool {
        let mut state = self.lock();
        let session_id = resolved_session(&state, session_id);
        let Some(records) = state.endings.get_mut(&session_id) else {
            return false;
        };
        if !mark_shown(records, shown) {
            return false;
        }
        self.write(&state);
        true
    }

    /// `session_id`'s run records, oldest first (ADR-0080) — the one read every
    /// ending consumer goes through. A session that recorded no ending answers
    /// empty.
    pub(crate) fn endings(&self, session_id: &str) -> Vec<TaskRunRecord> {
        let state = self.lock();
        let session_id = resolved_session(&state, session_id);
        state.endings.get(&session_id).cloned().unwrap_or_default()
    }

    /// Whether the run's ending of `class` was SHOWN by a confirmed card write
    /// (ADR-0080) — the exactly-once read the retirement's dedup and the Wake
    /// gate ask. The run resolves by the alias order, so a caller holding any
    /// spelling of it asks the same question; an owed or surrendered entry, and
    /// a run the section does not know, answer `false`. The render reads the
    /// same fact through [`EndingFacts`]; this is the store's own seam.
    #[allow(dead_code)]
    pub(crate) fn ending_shown(&self, session_id: &str, run: &TaskRunId, class: TaskEndingClass) -> bool {
        let state = self.lock();
        let session_id = resolved_session(&state, session_id);
        let Some(records) = state.endings.get(&session_id) else {
            return false;
        };
        let Some(record) = resolve_run(records, run).map(|index| &records[index]) else {
            return false;
        };
        record
            .endings
            .iter()
            .any(|ending| ending.class == class && ending.state == TaskEndingState::Shown)
    }

    /// Remember the endings a recoverably-failed body owes the durable Record,
    /// tied to the Pending Card Update payload at `seq` (ADR-0080). Held here,
    /// keyed by the CARD — whose id is stable across a Session's replace, remove
    /// or re-key — so no card-accumulator lifecycle can drop the pair, which is
    /// this fact's only copy. A newer failure for the same card supersedes the
    /// older entry, exactly as the delivery layer supersedes its payload.
    /// In-memory only: a restart that loses it lets the entry re-render once,
    /// the same duplicate-over-loss trade the bound makes.
    pub(crate) fn note_owed_rendered(
        &self,
        card_message_id: &str,
        session_id: &str,
        seq: u64,
        shown: &[EndingKey],
    ) {
        if shown.is_empty() {
            return;
        }
        self.lock().owed.insert(
            card_message_id.to_string(),
            OwedRendered {
                session_id: session_id.to_string(),
                seq,
                shown: shown.to_vec(),
            },
        );
    }

    /// Every card's owed endings, with the Session they belong to and the
    /// sequence whose delivery records them (ADR-0080).
    pub(crate) fn owed_rendered(&self) -> Vec<(String, OwedRendered)> {
        self.lock()
            .owed
            .iter()
            .map(|(card, owed)| (card.clone(), owed.clone()))
            .collect()
    }

    /// `session_id`'s run records together with its owed mirrors, read under ONE
    /// lock (ADR-0080): a commit's snapshot must not mix a record and an owed
    /// entry from two instants, or a concurrent accepted write could slip
    /// between the two reads and be missed by both.
    pub(crate) fn endings_and_owed(
        &self,
        session_id: &str,
    ) -> (Vec<TaskRunRecord>, Vec<(String, OwedRendered)>) {
        let state = self.lock();
        let session = resolved_session(&state, session_id);
        let records = state.endings.get(&session).cloned().unwrap_or_default();
        let owed = state
            .owed
            .iter()
            .filter(|(_, owed)| owed.session_id == session)
            .map(|(card, owed)| (card.clone(), owed.clone()))
            .collect();
        (records, owed)
    }

    /// Drop a card's owed endings once their payload's fate is settled —
    /// delivered (their shown bits flipped) or never delivering (nothing may be
    /// recorded). A stale sequence names an entry a newer failure replaced, so
    /// it is a no-op.
    pub(crate) fn clear_owed_rendered(&self, card_message_id: &str, seq: u64) {
        let mut state = self.lock();
        if state
            .owed
            .get(card_message_id)
            .is_some_and(|owed| owed.seq == seq)
        {
            state.owed.remove(card_message_id);
        }
    }

    /// The payload at `seq` for `card_message_id` delivered: flip its owed
    /// endings' shown bits AND drop the mirror, in ONE chains critical section
    /// and write — so a [`Self::rename`] racing the delivery can neither move
    /// the entry out from under the write nor leave it cleared without its bit
    /// (ADR-0080). Returns the Session and the pairs, for the caller's
    /// live-gate insert; `None` when no entry matches `seq` (a newer failure
    /// replaced it, or it was already settled).
    pub(crate) fn confirm_owed_rendered(
        &self,
        card_message_id: &str,
        seq: u64,
    ) -> Option<(String, Vec<EndingKey>)> {
        let mut state = self.lock();
        if state.owed.get(card_message_id).is_none_or(|owed| owed.seq != seq) {
            return None;
        }
        let owed = state.owed.remove(card_message_id).expect("checked above");
        if push_shown(&mut state, &owed.session_id, &owed.shown) {
            self.write(&state);
        }
        Some((owed.session_id, owed.shown))
    }

    /// Persist the durable sections through the shared sidecar writer. The file
    /// is kept even when every section is empty: its presence is the one-time
    /// migration marker that stops [`Self::load`] folding the legacy files
    /// again (ADR-0069). The migration-only `rendered` field is
    /// `skip_serializing`, so the first write after a load drops the old
    /// section for good.
    fn write(&self, state: &State) {
        #[cfg(test)]
        self.writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        sidecar::store(
            &self.path,
            "Chain Record",
            &ChainRecordFile {
                records: state.records.clone(),
                announcements: state.announcements.clone(),
                rendered: HashMap::new(),
                endings: state.endings.clone(),
            },
            false,
        );
    }

    /// Test-only: how often this store persisted its sidecar (spec #561's
    /// write-cost measurement). Reads the counter the test builds carry.
    #[cfg(test)]
    pub(crate) fn writes(&self) -> usize {
        self.writes.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Drop `session_id`'s durable record once its terminal card's ending write
/// is **confirmed** (ADR-0063 amendment; the pre-0067 rule dropped it before
/// the PATCH). A terminal card whose newest write failed recoverably is
/// still owed a Pending Card Update (ADR-0067), so its record stays: this
/// life's drain, or the next restart's reap, repairs the card and the reap
/// cleans the record once the write confirmed. A yielded `Waiting` card
/// keeps it (the reap settles its true end later) and a live card keeps it
/// (still owed). A card whose accumulator still holds an UNRENDERED orphan gap
/// keeps it too (spec #561, review #569): the durable fact on the record is the
/// only record of the tail that will land in it, so releasing it would strand
/// that tail.
/// Called after every ending PATCH in the flush path and by the
/// reap's same-card probe; the rule lives here, beside the record it releases.
pub(crate) async fn release_spent(cards: &CardsHandle, session_id: &str) {
    let card_message_id = {
        let cards = cards.cards.lock().await;
        let Some(card) = cards.get(session_id) else {
            return;
        };
        if !card.is_terminal() || card.owes_pending_gap() {
            return;
        }
        card.card_message_id().map(str::to_string)
    };
    if let Some(card_message_id) = card_message_id
        && cards.feishu.has_pending_card_update(&card_message_id)
    {
        return;
    }
    cards.chains.release(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Wake, WakeSource};
    use crate::bridge::test_support::launch_task;

    const FILE: &str = "chain_records.json";

    /// The durable reply target (issue #580): a track sets it, a re-point
    /// without a new one carries it, and a new one wins — the fact the
    /// projection replies to after a restart.
    #[test]
    fn a_repoint_carries_or_overrides_the_reply_target() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        chains.track(
            "ses_a",
            "om_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            Some("om_user_1"),
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").reply_to.as_deref(),
            Some("om_user_1")
        );

        chains.track("ses_a", "om_2", MessageId::new("msg_cola_1"), None, None, None);
        assert_eq!(
            chains.get("ses_a").expect("the record").reply_to.as_deref(),
            Some("om_user_1"),
            "a re-point that learns no new target keeps the one it had"
        );

        chains.track(
            "ses_a",
            "om_3",
            MessageId::new("msg_cola_2"),
            None,
            None,
            Some("om_user_2"),
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").reply_to.as_deref(),
            Some("om_user_2"),
            "a new target wins"
        );
    }

    /// The landing re-point (#582): `track_landing` writes the rung the create
    /// actually landed on IN THE SAME record write — `Some(id)` sets it, and a
    /// top-level landing clears it, which the carry-only `track` cannot
    /// express. The two-write window the old `clear_reply_to` left is gone.
    #[test]
    fn a_landing_track_sets_or_clears_the_reply_target_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        chains.track(
            "ses_a",
            "om_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            Some("om_user_1"),
        );

        // A top-level landing clears the refused target in the same write.
        let writes = chains.writes();
        chains.track_landing(
            "ses_a",
            "om_2",
            MessageId::new("msg_cola_1"),
            None,
            None,
            None,
            &[],
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").reply_to,
            None,
            "the top-level landing clears the target"
        );
        assert_eq!(
            chains.writes() - writes,
            1,
            "the clear rides the re-point's OWN write — never a second one a crash could lose"
        );

        // A reply-rung landing sets its own target.
        let writes = chains.writes();
        chains.track_landing(
            "ses_a",
            "om_3",
            MessageId::new("msg_cola_1"),
            None,
            None,
            Some("om_user_2"),
            &[],
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").reply_to.as_deref(),
            Some("om_user_2"),
            "the reply landing records its rung"
        );
        assert_eq!(chains.writes() - writes, 1, "the set is one write too");
    }

    /// [`a_landing_track_sets_or_clears_the_reply_target_atomically`] for the
    /// cursor-carrying landing: the confirmed cursor and the cleared target
    /// ride ONE write.
    #[test]
    fn a_landing_cursor_track_clears_the_reply_target_in_the_same_write() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        chains.track(
            "ses_a",
            "om_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            Some("om_user_1"),
        );
        let writes = chains.writes();
        chains.track_carrying_cursor_landing(
            "ses_a",
            "om_2",
            MessageId::new("msg_cola_1"),
            None,
            None,
            None,
            &RenderedCursor::default(),
            None,
            &[],
        );
        let record = chains.get("ses_a").expect("the record");
        assert_eq!(record.reply_to, None, "the landing cleared the target");
        assert!(
            record.cursor.is_some(),
            "the confirmed cursor rode the same write"
        );
        assert_eq!(
            chains.writes() - writes,
            1,
            "cursor confirm + rung clear are ONE write"
        );
    }

    /// The orphan gap is chain-level (spec #561, review #569): a re-point —
    /// a split continuation, a Wake continuation, a fresh Turn's takeover —
    /// carries the owed tail, a confirmed partial body advances it, and only a
    /// body that reached the gap's end consumes it.
    #[test]
    fn the_pending_gap_persists_advances_and_is_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_old",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        let gap = PendingGap {
            cursor: RenderedCursor::default(),
            anchor: TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 1_000,
            },
            bound: Some(MessageId::new("msg_cola_2")),
        };
        assert!(chains.note_pending_gap("ses_a", "om_old", &gap));
        assert_eq!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .pending_gap,
            Some(gap.clone()),
            "the gap survives a restart"
        );

        chains.track_carrying_cursor(
            "ses_a",
            "om_new",
            MessageId::new("msg_cola_2"),
            Some(2_000),
            None,
            None,
            &RenderedCursor::default(),
            None,
            &[],
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").pending_gap,
            Some(gap.clone()),
            "a projection's re-point carries the gap"
        );

        let frontier = CursorFrontier {
            message_id: MessageId::new("msg_a_1000"),
            part_index: 0,
            kind: CursorPartKind::Text,
            started_at: Some(1_500),
            delivered_chars: 5,
            prefix_digest: Some(cursor_prefix_digest("尾巴")),
        };
        assert!(chains.advance_pending_gap("ses_a", "om_new", &frontier, false));
        assert_eq!(
            chains
                .get("ses_a")
                .expect("the record")
                .pending_gap
                .expect("the gap stays owed")
                .cursor
                .frontier,
            Some(frontier.clone()),
            "a partial body advances the gap's cursor"
        );

        chains.track(
            "ses_a",
            "om_third",
            MessageId::new("msg_cola_3"),
            Some(3_000),
            None,
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").pending_gap,
            Some(PendingGap {
                cursor: RenderedCursor {
                    frontier: Some(frontier.clone()),
                    ..RenderedCursor::default()
                },
                ..gap.clone()
            }),
            "an ordinary re-point carries the advanced gap"
        );

        assert!(chains.advance_pending_gap("ses_a", "om_third", &frontier, true));
        assert!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .pending_gap
                .is_none(),
            "the body that reached the gap's end consumes it"
        );
        chains.track(
            "ses_a",
            "om_fourth",
            MessageId::new("msg_cola_4"),
            Some(4_000),
            None,
            None,
        );
        assert!(
            chains.get("ses_a").expect("the record").pending_gap.is_none(),
            "a consumed gap does not come back on a re-point"
        );
    }

    /// The projection's write-ahead intent (spec #561, review #569) persists
    /// across a restart, clears on demand, and is scoped to the record's card:
    /// a re-point (or a clear naming another card) never touches a newer
    /// generation.
    #[test]
    fn the_projection_intent_persists_clears_and_stays_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_old",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );

        assert!(chains.note_projection_intent("ses_a", "om_old"));
        assert!(
            chains.get("ses_a").expect("the record").projection_intent,
            "the intent is set in memory"
        );
        assert!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .projection_intent,
            "the intent survives a restart"
        );
        assert!(
            !chains.clear_projection_intent("ses_a", "om_other"),
            "a clear for another card never touches this record"
        );
        assert!(chains.get("ses_a").expect("the record").projection_intent);
        assert!(chains.clear_projection_intent("ses_a", "om_old"));
        assert!(
            !ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .projection_intent,
            "the clear persists"
        );

        // A re-point CARRIES the unresolved intent (spec #561, review #569):
        // the create it marked may have landed, and a takeover that erased the
        // mark would let a later restart post that tail again.
        assert!(chains.note_projection_intent("ses_a", "om_old"));
        chains.track(
            "ses_a",
            "om_new",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        assert!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the re-pointed record")
                .projection_intent,
            "a re-point carries the unresolved intent"
        );
        // The landed create's late-card collect consumes it on the winning
        // chain, whatever card that chain now names.
        assert!(chains.clear_projection_intent_any("ses_a"));
        assert!(
            !ChainRecords::load(path)
                .get("ses_a")
                .expect("the record")
                .projection_intent,
            "the consumed intent stays consumed"
        );
        assert!(
            !chains.clear_projection_intent_any("ses_a"),
            "consuming an absent intent writes nothing"
        );
    }

    #[test]
    fn track_persists_and_reloads_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        assert_eq!(
            chains.track(
                "ses_a",
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000),
                None,
                None
            ),
            None
        );
        chains.track(
            "ses_b",
            "om_card_2",
            MessageId::new("msg_cola_2"),
            None,
            None,
            None,
        );

        let reloaded = ChainRecords::load(path.clone());
        assert_eq!(
            reloaded.get("ses_a"),
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
        assert_eq!(
            reloaded.get("ses_b"),
            Some(ChainRecord::new("om_card_2", MessageId::new("msg_cola_2"), None))
        );
        assert_eq!(reloaded.get("ses_c"), None);

        // Re-tracking returns the previous record and keeps one per session.
        let previous = chains.track(
            "ses_a",
            "om_card_3",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        assert_eq!(
            previous,
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
        assert_eq!(
            ChainRecords::load(path.clone()).get("ses_a"),
            Some(ChainRecord::new(
                "om_card_3",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );

        // Releasing the last record keeps the file (the migration marker) but
        // drops the records themselves.
        chains.release("ses_a");
        chains.release("ses_b");
        assert!(path.exists(), "the Chain Record file is kept when empty");
        assert_eq!(chains.entries().len(), 0);
        assert_eq!(ChainRecords::load(path).entries().len(), 0);
    }

    /// The release entry is the one removal: the record is gone from memory and
    /// stays gone across a reload (a later load cannot resurrect it from the
    /// file).
    #[test]
    fn release_removes_the_record_and_persists_across_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );

        chains.release("ses_a");
        chains.release("ses_missing");
        assert_eq!(chains.get("ses_a"), None);
        assert_eq!(
            ChainRecords::load(path).get("ses_a"),
            None,
            "the removal persists across a reload"
        );
    }

    /// [`ChainRecords::release_if_card`] releases only the record that still
    /// names the card: a chain that moved on — a fresh Turn taking the session
    /// over, review #569 — keeps its record.
    #[test]
    fn release_if_card_drops_only_the_record_naming_that_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );

        assert!(
            !chains.release_if_card("ses_a", "om_stale"),
            "another card's record is never released"
        );
        assert_eq!(
            chains.get("ses_a").expect("the record survives").card_message_id,
            "om_card_1"
        );

        assert!(
            chains.release_if_card("ses_a", "om_card_1"),
            "the record naming the settled card is released"
        );
        assert_eq!(chains.get("ses_a"), None);
        assert_eq!(
            ChainRecords::load(path).get("ses_a"),
            None,
            "the guarded removal persists across a reload"
        );
    }

    #[test]
    fn rename_rekeys_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_old",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );

        chains.rename("ses_old", "ses_fresh");
        assert_eq!(chains.get("ses_old"), None);
        assert_eq!(
            ChainRecords::load(path).get("ses_fresh"),
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
    }

    /// A 404 recreate names the same logical Session: its announcement moves
    /// with the record, so the fresh id cannot re-announce a Wake the old id
    /// already showed.
    #[test]
    fn rename_rekeys_the_announcement_with_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_old",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        chains.advance("ses_old", "msg_wake_1", 1_000);

        chains.rename("ses_old", "ses_fresh");
        let reloaded = ChainRecords::load(path);
        assert_eq!(
            reloaded.announced("ses_fresh"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            })
        );
        assert_eq!(reloaded.announced("ses_old"), None);
    }

    #[test]
    fn a_corrupt_or_missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        assert_eq!(ChainRecords::load(path.clone()).get("ses_a"), None);

        std::fs::write(&path, "not json").unwrap();
        let chains = ChainRecords::load(path.clone());
        assert_eq!(chains.get("ses_a"), None, "a corrupt file reads as empty");
        // The next write replaces the corrupt file with a valid one.
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        assert_eq!(
            ChainRecords::load(path).get("ses_a"),
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
    }

    /// The waiting mark is in-memory only: it survives the record's own life
    /// but never persists, and it only ever marks the card the record names.
    #[test]
    fn the_waiting_mark_is_in_memory_and_card_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );

        assert!(!chains.mark_waiting_reaped("ses_a", "om_other"));
        assert!(!chains.get("ses_a").unwrap().waiting_reaped);
        assert!(chains.mark_waiting_reaped("ses_a", "om_card_1"));
        assert!(chains.get("ses_a").unwrap().waiting_reaped);

        // A reload (a restart) forgets it; a rewrite (a new live card) resets it.
        assert!(
            !ChainRecords::load(path.clone())
                .get("ses_a")
                .unwrap()
                .waiting_reaped
        );
        chains.track(
            "ses_a",
            "om_card_2",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        assert!(!chains.get("ses_a").unwrap().waiting_reaped);
    }

    /// The stored directory round-trips (the reap's V1 route); an empty string
    /// is unknown, exactly like `None`.
    #[test]
    fn the_directory_round_trips_and_an_empty_one_reads_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            Some("/work"),
            None,
        );
        chains.track(
            "ses_b",
            "om_card_2",
            MessageId::new("msg_cola_2"),
            None,
            Some(""),
            None,
        );

        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a").unwrap().directory.as_deref(), Some("/work"));
        assert_eq!(reloaded.get("ses_b").unwrap().directory, None);
    }

    #[test]
    fn the_anchor_travels_only_once_its_server_time_is_known() {
        let anchorless = ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), None);
        assert_eq!(anchorless.anchor(), None);
        let anchored = ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000));
        assert_eq!(
            anchored.anchor(),
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 1_000,
            })
        );
    }

    #[test]
    fn advance_persists_and_reloads_the_newest_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        chains.advance("ses_a", "msg_wake_1", 1_000);
        // Older or equal sends do not move the mark.
        chains.advance("ses_a", "msg_wake_0", 900);
        chains.advance("ses_a", "msg_wake_1b", 1_000);
        chains.advance("ses_b", "msg_wake_2", 2_000);

        let reloaded = ChainRecords::load(path);
        assert_eq!(
            reloaded.announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            })
        );
        assert_eq!(
            reloaded.announced("ses_b"),
            Some(WakeMark {
                wake_id: "msg_wake_2".into(),
                created_ms: 2_000,
            })
        );
        assert_eq!(reloaded.announced("ses_c"), None);
    }

    /// The two sections have independent lifetimes: removing a Session's
    /// record leaves its announcement in place.
    #[test]
    fn the_announcements_outlive_the_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        chains.advance("ses_a", "msg_wake_1", 1_000);

        chains.release("ses_a");
        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a"), None);
        assert_eq!(
            reloaded.announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            }),
            "the announcement is not removed with the record"
        );
    }

    /// ADR-0080: the durable Task Endings are recorded once per class, persist
    /// across a reload, are session-scoped, and — like the announcement —
    /// outlive the record they were written beside, because the whole point is
    /// the NEXT life's read. (The shared bound's eviction is pinned by
    /// [`the_bound_evicts_an_unowed_record_and_never_an_owed_one`].)
    #[test]
    fn endings_persist_are_idempotent_and_outlive_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        for session in ["ses_a", "ses_b"] {
            chains.track(
                session,
                format!("om_card_{session}"),
                MessageId::new("msg_cola_1"),
                Some(1_000),
                None,
                None,
            );
        }
        let run_a = TaskRunId::of_launch(&launch_task("call_1", None, None));
        let run_b = TaskRunId::of_launch(&launch_task("call_2", None, None));
        // A class recorded twice is stored once.
        for _ in 0..2 {
            chains.record_ending(
                "ses_a",
                &run_a,
                TaskEnding::owed(TaskEndingClass::Evidence, identity("a")),
            );
        }
        chains.record_ending(
            "ses_a",
            &run_b,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("b")),
        );

        let reloaded = ChainRecords::load(path.clone());
        assert_eq!(
            reloaded.endings("ses_a").len(),
            2,
            "one record per run, in arrival order"
        );
        assert_eq!(
            classes(&reloaded.endings("ses_a")[0]),
            vec![TaskEndingClass::Evidence],
            "a class recorded twice is stored once"
        );
        assert!(
            reloaded.endings("ses_b").is_empty(),
            "another session is untouched"
        );

        chains.release("ses_a");
        assert_eq!(
            classes(&ChainRecords::load(path).endings("ses_a")[0]),
            vec![TaskEndingClass::Evidence],
            "the ending is not removed with the record"
        );
    }

    /// ADR-0080 (CI gate): the owed mirror is keyed by the CARD — whose id is
    /// stable across a Session's replace, remove and re-key — so nothing about a
    /// card accumulator can drop it; a newer failure for the same card supersedes
    /// the older entry, a rename re-keys the Session it names, and an exact clear
    /// (not a stale one) drops it. A delivery the drain confirms flips the
    /// ending's shown bit under the RECREATED id, never the dead one.
    #[test]
    fn owed_endings_are_card_keyed_and_follow_a_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let delivered = staged("call_1", TaskEndingClass::Evidence);
        let superseded = staged("call_3", TaskEndingClass::Evidence);
        let confirmed = staged("call_4", TaskEndingClass::Evidence);
        for (run, class) in [&delivered, &superseded, &confirmed] {
            chains.record_ending("ses_1", run, TaskEnding::owed(*class, identity("a command")));
        }

        chains.note_owed_rendered("om_a", "ses_1", 3, std::slice::from_ref(&delivered));
        chains.note_owed_rendered("om_b", "ses_1", 4, &[staged("call_2", TaskEndingClass::Evidence)]);
        // A newer failure for the same card supersedes the older entry.
        chains.note_owed_rendered("om_a", "ses_1", 9, std::slice::from_ref(&superseded));
        let mut owed = chains.owed_rendered();
        owed.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(owed.len(), 2);
        assert_eq!(
            owed[0],
            (
                "om_a".to_string(),
                OwedRendered {
                    session_id: "ses_1".to_string(),
                    seq: 9,
                    shown: vec![superseded.clone()],
                }
            ),
            "a newer failure for the same card replaces the older entry"
        );

        // A rename re-keys the Session the mirror names.
        chains.rename("ses_1", "ses_2");
        assert!(
            chains
                .owed_rendered()
                .iter()
                .all(|(_, owed)| owed.session_id == "ses_2"),
            "a recreated Session id moves the owed mirror with it"
        );
        // A writer that captured the OLD id — a PATCH accepted after the rename —
        // flips the bit under the recreated Session, never the dead one.
        chains.record_shown("ses_1", std::slice::from_ref(&delivered));
        assert!(
            chains.ending_shown("ses_2", &delivered.0, TaskEndingClass::Evidence),
            "a stale id resolves through the rename"
        );

        // A stale clear is a no-op; the exact sequence drops the entry.
        chains.clear_owed_rendered("om_a", 3);
        assert!(chains.owed_rendered().iter().any(|(card, _)| card == "om_a"));
        chains.clear_owed_rendered("om_a", 9);
        assert!(!chains.owed_rendered().iter().any(|(card, _)| card == "om_a"));

        // A delivery confirmed AFTER a rename flips the bit under the NEW id and
        // drops the mirror, in one operation (a rename can never split the two).
        chains.note_owed_rendered("om_c", "ses_2", 5, std::slice::from_ref(&confirmed));
        chains.rename("ses_2", "ses_3");
        let (session, shown) = chains
            .confirm_owed_rendered("om_c", 5)
            .expect("the owed entry confirms");
        assert_eq!(session, "ses_3", "the delivery honors the rename");
        assert_eq!(shown, vec![confirmed.clone()]);
        assert!(
            chains.ending_shown("ses_3", &confirmed.0, TaskEndingClass::Evidence),
            "the delivered ending's bit lands under the recreated id"
        );
        assert!(!chains.owed_rendered().iter().any(|(card, _)| card == "om_c"));

        // In-memory only: a reload never carries an owed mirror.
        assert!(ChainRecords::load(path).owed_rendered().is_empty());
    }

    /// Issue #671 (CI gate): the re-key forwarding follows a LONG chain of
    /// recreates — no fixed depth cap — so a stale writer still lands under the
    /// current id; the visited set only stops a corrupt cycle.
    #[test]
    fn the_endings_follow_a_long_rename_chain() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        for i in 0..40 {
            chains.rename(&format!("ses_{i}"), &format!("ses_{}", i + 1));
        }
        let run = TaskRunId::of_launch(&launch_task("call_1", None, None));
        chains.record_ending(
            "ses_0",
            &run,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("a")),
        );
        assert_eq!(
            classes(&chains.endings("ses_40")[0]),
            vec![TaskEndingClass::Evidence],
            "a stale id resolves through the whole chain"
        );
    }

    /// Issue #671: a recreated session id names the same logical session, so
    /// the run records its earlier cards wrote move with the announcement — a
    /// retirement already shown must not replay under the new id.
    #[test]
    fn the_endings_follow_the_recreated_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let run = TaskRunId::of_launch(&launch_task("call_1", None, None));
        chains.record_ending(
            "ses_old",
            &run,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("a")),
        );
        chains.rename("ses_old", "ses_new");

        let reloaded = ChainRecords::load(path);
        assert_eq!(
            classes(&reloaded.endings("ses_new")[0]),
            vec![TaskEndingClass::Evidence]
        );
        assert!(reloaded.endings("ses_old").is_empty());
    }

    /// ADR-0080: a confirmed body flips its endings' shown bits in the SAME
    /// write as its cursor — and they still land when the record no longer names
    /// that card, because the bit is session-scoped while the cursor is
    /// record-scoped. A run the section does not know is a no-op: a confirmation
    /// never mints a record.
    #[test]
    fn a_confirmed_body_flips_its_endings_shown_bits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        let one = staged("call_1", TaskEndingClass::Evidence);
        let two = staged("call_2", TaskEndingClass::Evidence);
        chains.record_ending("ses_a", &one.0, TaskEnding::owed(one.1, identity("one")));
        chains.record_ending("ses_a", &two.0, TaskEnding::owed(two.1, identity("two")));

        chains.advance_cursor_and_gap(
            "ses_a",
            "om_card_1",
            &RenderedCursor::default(),
            None,
            std::slice::from_ref(&one),
        );
        assert!(
            ChainRecords::load(path.clone()).ending_shown("ses_a", &one.0, TaskEndingClass::Evidence),
            "the confirmed body's ending persists shown with its cursor"
        );

        // A confirmation for a record that moved on drops the cursor advance
        // (it is record-scoped) but still flips the bit.
        chains.release("ses_a");
        chains.advance_cursor_and_gap(
            "ses_a",
            "om_card_1",
            &RenderedCursor::default(),
            None,
            std::slice::from_ref(&two),
        );
        assert!(
            ChainRecords::load(path).ending_shown("ses_a", &two.0, TaskEndingClass::Evidence),
            "the session-scoped shown bit survives the record's release"
        );

        // A confirmation naming a run the section does not know flips nothing.
        assert!(!chains.mark_endings_shown("ses_a", &[staged("call_unknown", TaskEndingClass::Evidence)]));
    }

    /// ADR-0069's migration: without `chain_records.json`, the legacy sidecars
    /// are folded in once; afterwards they are never read again, so a record
    /// removed in the new store cannot be resurrected from the stale files.
    #[test]
    fn folds_the_legacy_sidecars_once_and_never_refolds_a_cleared_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(
            dir.path().join("live_cards.json"),
            r#"{"sessions":{"ses_a":{"card_message_id":"om_card_1","message_id":"msg_cola_1","created_ms":1000,"directory":"/work"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("wake_watermarks.json"),
            r#"{"sessions":{"ses_a":{"wake_id":"msg_wake_1","created_ms":1000}}}"#,
        )
        .unwrap();

        let chains = ChainRecords::load(path.clone());
        assert!(
            path.exists(),
            "the fold materializes the Chain Record file immediately"
        );
        assert_eq!(
            chains.get("ses_a"),
            Some(
                ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000))
                    .with_directory(Some("/work".into()))
            ),
            "the legacy record is folded in"
        );
        assert_eq!(
            chains.announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            }),
            "the legacy watermark is folded in"
        );

        // Clearing the folded record persists the new file; the stale legacy
        // record must not come back on the next load.
        chains.release("ses_a");
        assert!(path.exists());
        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a"), None);
    }

    /// The fold settles the migration at the first load: the marker file is
    /// materialized then, so the legacy files can vanish before any later
    /// mutation without losing the folded facts.
    #[test]
    fn the_fold_materializes_the_marker_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let legacy = dir.path().join("live_cards.json");
        std::fs::write(
            &legacy,
            r#"{"sessions":{"ses_a":{"card_message_id":"om_card_1","message_id":"msg_cola_1"}}}"#,
        )
        .unwrap();

        let chains = ChainRecords::load(path.clone());
        assert!(path.exists(), "the first load writes the merged file");
        assert_eq!(chains.get("ses_a").unwrap().card_message_id, "om_card_1");

        // The legacy file is no longer needed; a restart reads only the new
        // file.
        std::fs::remove_file(&legacy).unwrap();
        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a").unwrap().card_message_id, "om_card_1");
    }

    /// The Rendered Cursor (spec #561) round-trips on its record, advances
    /// only for the card the record names, and leaves with the record.
    #[test]
    fn the_rendered_cursor_round_trips_and_advances_only_the_named_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        assert_eq!(chains.cursor("ses_a"), None, "a fresh record is cursorless");

        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_1"),
                part_index: 2,
                kind: CursorPartKind::Text,
                started_at: Some(2_000),
                delivered_chars: 42,
                prefix_digest: Some(7),
            }),
            live_calls: ["call_1".to_string()].into_iter().collect(),
        };
        chains.advance_cursor("ses_a", "om_other", &cursor);
        assert_eq!(chains.cursor("ses_a"), None, "a stale write advances nothing");

        chains.advance_cursor("ses_a", "om_card_1", &cursor);
        assert_eq!(
            ChainRecords::load(path.clone()).cursor("ses_a"),
            Some(cursor.clone()),
            "the cursor persists across a reload"
        );

        chains.release("ses_a");
        assert_eq!(
            ChainRecords::load(path).cursor("ses_a"),
            None,
            "the cursor is released with the record"
        );
    }

    /// A re-point within the chain (`track`) carries the cursor; a session
    /// with no previous record starts cursorless, and so does a record an
    /// older release wrote (no cursor field).
    #[test]
    fn track_carries_the_rendered_cursor_and_a_legacy_record_reads_cursorless() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let cursor = RenderedCursor {
            frontier: Some(CursorFrontier {
                message_id: MessageId::new("msg_a_1"),
                part_index: 0,
                kind: CursorPartKind::Reasoning,
                started_at: None,
                delivered_chars: 7,
                prefix_digest: Some(7),
            }),
            live_calls: ["call_1".to_string()].into_iter().collect(),
        };
        chains.track(
            "ses_a",
            "om_card_1",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        chains.advance_cursor("ses_a", "om_card_1", &cursor);

        let previous = chains
            .track(
                "ses_a",
                "om_card_2",
                MessageId::new("msg_cola_1"),
                Some(1_000),
                None,
                None,
            )
            .expect("the re-point returns the previous record");
        assert_eq!(previous.cursor, Some(cursor.clone()));
        assert_eq!(
            chains.cursor("ses_a"),
            Some(cursor.clone()),
            "the split continuation's record inherits the chain's cursor"
        );

        chains.track(
            "ses_b",
            "om_card_3",
            MessageId::new("msg_cola_2"),
            None,
            None,
            None,
        );
        assert_eq!(
            chains.cursor("ses_b"),
            None,
            "a genuinely new chain starts cursorless"
        );

        // A record written without the cursor field (an older release) folds
        // in cursorless and can still be advanced by this life.
        std::fs::write(
            dir.path().join("legacy.json"),
            r#"{"records":{"ses_legacy":{"card_message_id":"om_card","message_id":"msg_cola_1"}},"announcements":{}}"#,
        )
        .unwrap();
        let legacy = ChainRecords::load(dir.path().join("legacy.json"));
        assert_eq!(legacy.cursor("ses_legacy"), None);
        assert_eq!(
            legacy.get("ses_legacy").unwrap().card_message_id,
            "om_card",
            "the legacy record still reads"
        );
    }

    /// Once `chain_records.json` exists, it wins: the legacy files beside it
    /// are never folded again.
    #[test]
    fn the_chain_file_wins_over_the_legacy_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_new",
            "om_new",
            MessageId::new("msg_cola_new"),
            None,
            None,
            None,
        );
        std::fs::write(
            dir.path().join("live_cards.json"),
            r#"{"sessions":{"ses_old":{"card_message_id":"om_old","message_id":"msg_cola_old"}}}"#,
        )
        .unwrap();

        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_new").unwrap().card_message_id, "om_new");
        assert_eq!(reloaded.get("ses_old"), None);
    }

    /// The Chain Generation (spec #571, ticket #572): one in-memory monotonic
    /// version per chain, minted when the tracked card identity changes. A
    /// fresh chain starts at zero; a plain re-point onto a new card bumps by
    /// one.
    #[test]
    fn a_track_re_point_bumps_the_chain_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        chains.track(
            "ses_a",
            "om_first",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the record").generation,
            0,
            "a fresh chain starts at zero"
        );

        chains.track(
            "ses_a",
            "om_second",
            MessageId::new("msg_cola_2"),
            Some(2_000),
            None,
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the re-pointed record").generation,
            1,
            "a re-point onto a new card bumps the generation"
        );

        // A re-track of the SAME card is not an identity change: the record
        // keeps its generation (and a monotonic version can never drop back).
        chains.track(
            "ses_a",
            "om_second",
            MessageId::new("msg_cola_2"),
            Some(2_000),
            None,
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the re-tracked record").generation,
            1,
            "re-tracking the same card carries the generation"
        );
    }

    /// The two takeover entries mint the same way: a fresh Turn's takeover and
    /// the projection's atomic re-point bump the generation when they rewrite
    /// the record onto a different card.
    #[test]
    fn a_takeover_and_a_projection_re_point_bump_the_chain_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        chains.track(
            "ses_a",
            "om_old",
            MessageId::new("msg_cola_1"),
            Some(1_000),
            None,
            None,
        );
        chains.track_takeover(
            "ses_a",
            "om_new",
            MessageId::new("msg_cola_2"),
            Some(2_000),
            None,
            None,
        );
        assert_eq!(
            chains.get("ses_a").expect("the successor").generation,
            1,
            "a fresh Turn's takeover bumps the generation"
        );

        chains.track_carrying_cursor(
            "ses_a",
            "om_projected",
            MessageId::new("msg_cola_3"),
            Some(3_000),
            None,
            None,
            &RenderedCursor::default(),
            None,
            &[],
        );
        assert_eq!(
            chains
                .get("ses_a")
                .expect("the projection's successor")
                .generation,
            2,
            "the projection's carrying re-point bumps again"
        );
    }

    /// The unrelated record writes — the anchor fill-in and the cursor
    /// advance — mutate the record in place, so the version the last identity
    /// change minted survives them untouched.
    #[test]
    fn anchor_and_cursor_writes_never_bump_the_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_first",
            MessageId::new("msg_cola_1"),
            None,
            None,
            None,
        );
        chains.track(
            "ses_a",
            "om_second",
            MessageId::new("msg_cola_2"),
            None,
            None,
            None,
        );
        let minted = chains.get("ses_a").expect("the record").generation;
        assert_eq!(minted, 1, "the re-point minted a version");

        chains.set_anchor(
            "ses_a",
            "om_second",
            &TurnAnchor {
                message_id: MessageId::new("msg_cola_2"),
                created_ms: 2_000,
            },
        );
        assert_eq!(
            chains.get("ses_a").expect("the anchored record").generation,
            minted,
            "the anchor fill-in never bumps the generation"
        );

        chains.advance_cursor("ses_a", "om_second", &RenderedCursor::default());
        assert_eq!(
            chains.get("ses_a").expect("the advanced record").generation,
            minted,
            "the cursor advance never bumps the generation"
        );
    }

    /// The record snapshot decisions read carries the version it was read at:
    /// every entry [`ChainRecords::entries`] hands out, and the predecessor a
    /// re-point returns, name their own generation — not the newest one.
    #[test]
    fn the_record_snapshot_carries_the_generation_it_was_read_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_first",
            MessageId::new("msg_cola_1"),
            None,
            None,
            None,
        );
        let previous = chains
            .track(
                "ses_a",
                "om_second",
                MessageId::new("msg_cola_2"),
                None,
                None,
                None,
            )
            .expect("the re-point returns the predecessor");
        assert_eq!(
            previous.generation, 0,
            "the predecessor carries the version its decision read"
        );

        let entries: HashMap<String, ChainRecord> = chains.entries().into_iter().collect();
        assert_eq!(
            entries["ses_a"].generation, 1,
            "the snapshot carries the record's current version"
        );
        assert_eq!(
            chains.generation("ses_a"),
            1,
            "the live read names the chain's current version"
        );
        assert_eq!(
            chains.generation("ses_missing"),
            0,
            "a chain with no record has no version to submit under"
        );
    }

    /// The generation is process-local: releasing the record drops it (a chain
    /// re-tracked after a release starts at zero again), and so does a fresh
    /// load — no durable field carries the version across lives.
    #[test]
    fn the_generation_dies_with_the_record_and_a_fresh_load_starts_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.track(
            "ses_a",
            "om_first",
            MessageId::new("msg_cola_1"),
            None,
            None,
            None,
        );
        chains.track(
            "ses_a",
            "om_second",
            MessageId::new("msg_cola_2"),
            None,
            None,
            None,
        );
        assert_eq!(chains.get("ses_a").expect("the record").generation, 1);

        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(
            !persisted.contains("\"generation\""),
            "the generation never reaches the durable file: {persisted}"
        );
        assert_eq!(
            ChainRecords::load(path.clone())
                .get("ses_a")
                .expect("the reloaded record")
                .generation,
            0,
            "a fresh load starts at zero"
        );

        chains.release("ses_a");
        chains.track(
            "ses_a",
            "om_third",
            MessageId::new("msg_cola_3"),
            None,
            None,
            None,
        );
        assert_eq!(
            chains
                .get("ses_a")
                .expect("the chain after the release")
                .generation,
            0,
            "the released version never leaks into the next chain"
        );
    }

    // ===== The durable endings section (ADR-0080, spec #699) =====

    /// The identity a Background Task is re-synthesized from, as these tests
    /// record it.
    fn identity(label: &str) -> TaskRunIdentity {
        TaskRunIdentity {
            tool: Some("shell".into()),
            call_id: Some("call_1".into()),
            shell_id: Some("shell_1".into()),
            child_id: None,
            started_at: Some(1_000),
            label: Some(label.into()),
        }
    }

    /// A Wake naming the ids its payload carried.
    fn wake(id: &str, shell_id: Option<&str>, child_id: Option<&str>) -> Wake {
        Wake {
            id: MessageId::new(id),
            created_ms: Some(2_000),
            source: WakeSource::Shell,
            shell_id: shell_id.map(str::to_string),
            job_id: None,
            child_id: child_id.map(str::to_string),
            state: Some("completed".into()),
            label: None,
        }
    }

    /// One delivered body's staged ending (ADR-0080): the launch's run plus the
    /// class whose entry that body rendered.
    fn staged(call_id: &str, class: TaskEndingClass) -> EndingKey {
        (TaskRunId::of_launch(&launch_task(call_id, None, None)), class)
    }

    fn classes(record: &TaskRunRecord) -> Vec<TaskEndingClass> {
        record.endings.iter().map(|ending| ending.class).collect()
    }

    /// The section's own shape: one record per run, at most one ending per
    /// class, every field round-tripping through the sidecar, and the shown read
    /// answering per class.
    #[test]
    fn a_run_record_round_trips_with_one_ending_per_class() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let run = TaskRunId::of_launch(&launch_task("call_1", Some("shell_1"), None));

        assert!(chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("cargo test")),
        ));
        let mut ended = TaskEnding::owed(TaskEndingClass::Cleanup, identity("cargo test"));
        ended.finished_at = Some(2_000);
        ended.evidence = Some(TaskEndingEvidence::Ended("exited".into()));
        chains.record_ending("ses_a", &run, ended);
        // A cleaned run's late Wake is its OWN ending (#590): a Cleanup claims
        // nothing about how the run ended.
        chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Wake, identity("cargo test")),
        );

        let reloaded = ChainRecords::load(path.clone());
        let records = reloaded.endings("ses_a");
        assert_eq!(records.len(), 1, "one record per run: {records:?}");
        assert_eq!(records[0].run, run, "the record keeps the run's ids");
        assert_eq!(
            classes(&records[0]),
            vec![
                TaskEndingClass::Evidence,
                TaskEndingClass::Cleanup,
                TaskEndingClass::Wake
            ],
            "at most one ending per class, in derivation order"
        );
        let evidence = &records[0].endings[0];
        assert_eq!(
            evidence.state,
            TaskEndingState::Owed,
            "a derived ending starts owed"
        );
        assert_eq!(evidence.identity.label.as_deref(), Some("cargo test"));
        assert_eq!(evidence.identity.started_at, Some(1_000));
        assert!(
            evidence
                .identity
                .tool
                .as_deref()
                .is_some_and(|tool| tool == "shell")
        );
        assert_eq!(
            records[0].endings[1].finished_at,
            Some(2_000),
            "the clock round-trips"
        );
        assert_eq!(
            records[0].endings[1].evidence,
            Some(TaskEndingEvidence::Ended("exited".into())),
            "the class's own evidence round-trips"
        );
        assert!(!reloaded.ending_shown("ses_a", &run, TaskEndingClass::Evidence));

        // The first derivation is the fact: recording the same class again
        // neither duplicates it nor moves its state.
        assert!(!chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("again")),
        ));
        assert_eq!(
            ChainRecords::load(path.clone()).endings("ses_a")[0].endings.len(),
            3,
            "a re-recorded class is not appended twice"
        );
        assert!(
            ChainRecords::load(path).endings("ses_b").is_empty(),
            "another session is untouched"
        );
    }

    /// The alias order (ADR-0080): an exact run id; else an exact Wake id
    /// already on a record; else the NEWEST record naming any other id the
    /// writer holds (#552). The two arrival orders must agree — one run, one
    /// record, however the run is named.
    #[test]
    fn the_alias_order_resolves_a_run_named_from_either_side() {
        // A launchless Wake first, the launch-carrying read second.
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        let from_wake = TaskRunId::of_wake(&wake("msg_wake_1", Some("shell_1"), None))
            .expect("the shell id names the run");
        let from_launch = TaskRunId::of_launch(&launch_task("call_1", Some("shell_1"), None));
        chains.record_ending(
            "ses_a",
            &from_wake,
            TaskEnding::owed(TaskEndingClass::Wake, identity("wake")),
        );
        chains.record_ending(
            "ses_a",
            &from_launch,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("evidence")),
        );

        let records = chains.endings("ses_a");
        assert_eq!(
            records.len(),
            1,
            "the read resolves the Wake's record: {records:?}"
        );
        assert_eq!(
            records[0].run.key, "shell_1",
            "the run keeps the key it was first minted under"
        );
        assert!(
            records[0].run.names("call_1")
                && records[0].run.names("shell_1")
                && records[0].run.names("msg_wake_1"),
            "the record carries every id the run is known by: {:?}",
            records[0].run
        );
        assert_eq!(
            classes(&records[0]),
            vec![TaskEndingClass::Wake, TaskEndingClass::Evidence]
        );

        // The reverse arrival order agrees: the launch first, the Wake second.
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        chains.record_ending(
            "ses_b",
            &from_launch,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("evidence")),
        );
        chains.record_ending(
            "ses_b",
            &from_wake,
            TaskEnding::owed(TaskEndingClass::Wake, identity("wake")),
        );

        let records = chains.endings("ses_b");
        assert_eq!(
            records.len(),
            1,
            "the Wake resolves the read's record: {records:?}"
        );
        assert_eq!(records[0].run.key, "call_1");
        assert_eq!(
            classes(&records[0]),
            vec![TaskEndingClass::Evidence, TaskEndingClass::Wake]
        );
    }

    /// ADR-0080: "the record carries every id the run is known by". A Wake
    /// reporting BOTH a shell id and a DISTINCT job id stores its record under
    /// the shell id (the precedence is unchanged) but must carry the job id as
    /// an alias, so a later writer naming the run by that job id resolves the
    /// one record instead of minting a second one for a single run.
    #[test]
    fn a_wake_with_a_distinct_job_id_records_one_resolvable_run() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        let mut shell_and_job = wake("msg_wake_1", Some("shell_1"), None);
        shell_and_job.job_id = Some("job_1".into());
        let from_wake = TaskRunId::of_wake(&shell_and_job).expect("the shell id names the run");
        assert_eq!(from_wake.key, "shell_1", "the shell id keeps precedence");

        chains.record_ending(
            "ses_a",
            &from_wake,
            TaskEnding::owed(TaskEndingClass::Wake, identity("wake")),
        );
        // A later writer names the same run by the JOB id alone.
        let by_job = TaskRunId {
            key: "job_1".into(),
            call_id: None,
            shell_id: None,
            child_id: None,
            wake_id: None,
            job_id: Some("job_1".into()),
        };
        chains.record_ending(
            "ses_a",
            &by_job,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("evidence")),
        );

        let records = chains.endings("ses_a");
        assert_eq!(
            records.len(),
            1,
            "the job id resolves the Wake's one record: {records:?}"
        );
        assert!(
            records[0].run.names("job_1"),
            "the record carries the job id the Wake reported: {:?}",
            records[0].run
        );
        assert_eq!(
            classes(&records[0]),
            vec![TaskEndingClass::Wake, TaskEndingClass::Evidence]
        );
    }

    /// ADR-0080: "the record carries every id the run is known by" — and that
    /// alias must become DURABLE. When a second writer resolves a run and folds
    /// in a new id but the ending's class is already on the record, the ending
    /// is not appended again, yet the newly learned id must still be written: a
    /// restart that lost it would mint a second record for the run.
    #[test]
    fn an_absorbed_alias_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let from_wake = TaskRunId::of_wake(&wake("msg_wake_1", Some("shell_1"), None))
            .expect("the shell id names the run");
        let from_launch = TaskRunId::of_launch(&launch_task("call_1", Some("shell_1"), None));

        chains.record_ending(
            "ses_a",
            &from_wake,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("evidence")),
        );
        // The same class again, from a writer naming the run by its call id:
        // the class is not appended twice, but the call id is newly learned.
        chains.record_ending(
            "ses_a",
            &from_launch,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("evidence")),
        );

        let reloaded = ChainRecords::load(path);
        let records = reloaded.endings("ses_a");
        assert_eq!(records.len(), 1, "still one record for the run: {records:?}");
        assert!(
            records[0].run.names("call_1"),
            "the absorbed alias is durable across a reload: {:?}",
            records[0].run
        );

        // A later writer holding only the call id resolves the one record.
        let by_call = TaskRunId {
            key: "call_1".into(),
            call_id: Some("call_1".into()),
            shell_id: None,
            child_id: None,
            job_id: None,
            wake_id: None,
        };
        assert_eq!(resolve_run(&records, &by_call), Some(0));
    }

    /// The section's own resolution rule, on the one shape no writer's own ids
    /// can produce: a writer holding an id a record names always resolves that
    /// record, so two records can never share a live alias — the tie-break below
    /// is the section's defensive rule (#552's newest-match), pinned here
    /// directly.
    #[test]
    fn the_alias_order_prefers_the_exact_ids_and_takes_the_newest_alias() {
        let mut records = vec![
            TaskRunRecord {
                run: TaskRunId {
                    key: "shell_1".into(),
                    call_id: None,
                    shell_id: Some("shell_1".into()),
                    child_id: None,
                    job_id: None,
                    wake_id: Some("msg_wake_1".into()),
                },
                endings: Vec::new(),
            },
            TaskRunRecord {
                run: TaskRunId {
                    key: "ses_child".into(),
                    call_id: None,
                    shell_id: None,
                    child_id: Some("ses_child".into()),
                    job_id: None,
                    wake_id: None,
                },
                endings: Vec::new(),
            },
        ];

        // 1. An exact run id names its own record, whatever else it holds.
        let exact = TaskRunId {
            key: "shell_1".into(),
            call_id: Some("call_1".into()),
            shell_id: Some("shell_1".into()),
            child_id: Some("ses_child".into()),
            job_id: None,
            wake_id: None,
        };
        assert_eq!(resolve_run(&records, &exact), Some(0));

        // 2. An exact Wake id already on a record beats a shared alias.
        let re_encounter = TaskRunId {
            key: "job_1".into(),
            call_id: None,
            shell_id: Some("shell_1".into()),
            child_id: Some("ses_child".into()),
            job_id: None,
            wake_id: Some("msg_wake_1".into()),
        };
        assert_eq!(resolve_run(&records, &re_encounter), Some(0));

        // 3. Otherwise the NEWEST record naming any id the writer holds (#552:
        // a resumed child's later launch supersedes the earlier one).
        let by_alias = TaskRunId {
            key: "job_2".into(),
            call_id: None,
            shell_id: None,
            child_id: Some("ses_child".into()),
            job_id: None,
            wake_id: None,
        };
        assert_eq!(resolve_run(&records, &by_alias), Some(1));

        // A writer holding an id no record names mints its own.
        let unknown = TaskRunId {
            key: "job_3".into(),
            call_id: None,
            shell_id: None,
            child_id: None,
            job_id: None,
            wake_id: None,
        };
        assert_eq!(resolve_run(&records, &unknown), None);
        assert!(insert_ending(
            &mut records,
            &unknown,
            TaskEnding::owed(TaskEndingClass::Wake, identity("run"))
        ));
        assert_eq!(records.len(), 3);
    }

    /// The section is SESSION-scoped (ADR-0080): it outlives the Chain Record it
    /// was written beside — a restart after a terminal release still knows the
    /// fact — and an unrelated Session's release never touches it.
    #[test]
    fn the_endings_outlive_the_record_they_were_written_beside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let run = TaskRunId::of_launch(&launch_task("call_1", Some("shell_1"), None));
        for session in ["ses_a", "ses_b"] {
            chains.track(
                session,
                format!("om_card_{session}"),
                MessageId::new("msg_cola_1"),
                Some(1_000),
                None,
                None,
            );
        }
        chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("a")),
        );
        chains.record_ending(
            "ses_b",
            &run,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("b")),
        );

        chains.release("ses_a");
        let reloaded = ChainRecords::load(path.clone());
        assert_eq!(reloaded.get("ses_a"), None, "the record is gone");
        assert_eq!(
            classes(&reloaded.endings("ses_a")[0]),
            vec![TaskEndingClass::Evidence],
            "the ending is not removed with the record"
        );
        assert_eq!(
            classes(&reloaded.endings("ses_b")[0]),
            vec![TaskEndingClass::Evidence],
            "an unrelated Session's endings are untouched"
        );

        // A recreated Session id names the same logical Session, so its run
        // records and their shown bits move with it.
        chains.rename("ses_a", "ses_a2");
        assert!(ChainRecords::load(path.clone()).endings("ses_a").is_empty());
        assert_eq!(
            classes(&ChainRecords::load(path).endings("ses_a2")[0]),
            vec![TaskEndingClass::Evidence]
        );
    }

    /// The shared bound (ADR-0080) evicts the OLDEST record with no owed ending
    /// and NEVER an owed one.
    #[test]
    fn the_bound_evicts_an_unowed_record_and_never_an_owed_one() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        let call = |i: usize| TaskRunId::of_launch(&launch_task(&format!("call_{i}"), None, None));
        // The oldest record has been SHOWN, so it is the eviction candidate.
        chains.record_ending(
            "ses_a",
            &call(0),
            TaskEnding::owed(TaskEndingClass::Evidence, identity("shown")),
        );
        chains.mark_endings_shown("ses_a", &[(call(0), TaskEndingClass::Evidence)]);
        for i in 1..MAX_RUN_RECORDS {
            chains.record_ending(
                "ses_a",
                &call(i),
                TaskEnding::owed(TaskEndingClass::Evidence, identity("owed")),
            );
        }
        assert_eq!(chains.endings("ses_a").len(), MAX_RUN_RECORDS);

        // The one over the bound evicts the shown record — the oldest record
        // with no owed ending — and nothing else.
        chains.record_ending(
            "ses_a",
            &call(MAX_RUN_RECORDS),
            TaskEnding::owed(TaskEndingClass::Evidence, identity("owed")),
        );
        let records = chains.endings("ses_a");
        assert_eq!(records.len(), MAX_RUN_RECORDS, "the section stays bounded");
        assert!(
            !records.iter().any(|record| record.run.key == "call_0"),
            "the oldest unowed record went"
        );
        assert!(
            records
                .iter()
                .any(|record| record.run.key == format!("call_{MAX_RUN_RECORDS}")),
            "the newest record stays"
        );

        // An owed record is never the eviction candidate: a session whose
        // records are ALL owed keeps every owed entry — past the saturation cap
        // the OLDEST entry is surrendered instead, which is what makes its
        // record evictable, and the fact is kept either way.
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        for i in 0..MAX_OWED_RUN_RECORDS + 2 {
            chains.record_ending(
                "ses_b",
                &call(i),
                TaskEnding::owed(TaskEndingClass::Evidence, identity("owed")),
            );
        }
        let records = chains.endings("ses_b");
        assert_eq!(
            records.len(),
            MAX_OWED_RUN_RECORDS + 2,
            "no owed record is evicted under the shared bound"
        );
        assert!(
            records.iter().all(|record| !record.endings.is_empty()),
            "every run keeps its ending's fact, surrendered or owed"
        );
        assert_eq!(
            records.iter().filter(|record| record.owes()).count(),
            MAX_OWED_RUN_RECORDS,
            "the owed backlog is capped"
        );
        assert_eq!(
            records
                .iter()
                .take(2)
                .map(|record| record.endings[0].state)
                .collect::<Vec<_>>(),
            vec![TaskEndingState::Surrendered; 2],
            "the OLDEST owed entries are the ones surrendered"
        );

        // A surrendered entry makes its record ordinary eviction material: once
        // the section passes the shared bound, those records go — never an owed
        // one.
        for i in MAX_OWED_RUN_RECORDS + 2..MAX_RUN_RECORDS + 3 {
            chains.record_ending(
                "ses_b",
                &call(i),
                TaskEnding::owed(TaskEndingClass::Evidence, identity("owed")),
            );
        }
        let records = chains.endings("ses_b");
        assert_eq!(records.len(), MAX_RUN_RECORDS, "the shared bound holds");
        assert!(
            !records
                .iter()
                .any(|record| record.run.key == "call_0" || record.run.key == "call_1"),
            "the surrendered records are evictable: {:?}",
            records.iter().map(|record| &record.run.key).collect::<Vec<_>>()
        );
        assert_eq!(
            records.iter().filter(|record| record.owes()).count(),
            MAX_OWED_RUN_RECORDS,
            "no owed entry was evicted for the bound"
        );
    }

    /// The owed cap counts RECORDS that owe, not endings surrendered: `owed`
    /// must fall only when a record stops owing. A record carrying two owed
    /// classes would otherwise free a slot after one surrender and leave the
    /// backlog over the cap, so the count is recomputed each time.
    #[test]
    fn the_owed_cap_counts_records_not_endings() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        let call = |i: usize| TaskRunId::of_launch(&launch_task(&format!("call_{i}"), None, None));
        // The OLDEST record carries TWO owed classes, so one surrender does not
        // stop it owing.
        chains.record_ending(
            "ses_a",
            &call(0),
            TaskEnding::owed(TaskEndingClass::Evidence, identity("a")),
        );
        chains.record_ending(
            "ses_a",
            &call(0),
            TaskEnding::owed(TaskEndingClass::Cleanup, identity("a")),
        );
        for i in 1..=MAX_OWED_RUN_RECORDS {
            chains.record_ending(
                "ses_a",
                &call(i),
                TaskEnding::owed(TaskEndingClass::Evidence, identity("owed")),
            );
        }

        let records = chains.endings("ses_a");
        assert_eq!(
            records.len(),
            MAX_OWED_RUN_RECORDS + 1,
            "no owed record is evicted under the shared bound"
        );
        assert!(
            records.iter().filter(|record| record.owes()).count() <= MAX_OWED_RUN_RECORDS,
            "the number of owing records ends at or below the cap: {}",
            records.iter().filter(|record| record.owes()).count()
        );
    }

    /// Past the owed cap the OLDEST owed ending becomes SURRENDERED with a
    /// `WARN` — its fact retained — which is what makes its record ordinary
    /// eviction material, and a surrendered entry is never owed to a render.
    #[tokio::test]
    async fn the_owed_cap_surrenders_the_oldest_entry_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        let call = |i: usize| TaskRunId::of_launch(&launch_task(&format!("call_{i}"), None, None));
        let ((), logs) = crate::bridge::test_support::capture_logs(async {
            for i in 0..=MAX_OWED_RUN_RECORDS {
                chains.record_ending(
                    "ses_a",
                    &call(i),
                    TaskEnding::owed(TaskEndingClass::Evidence, identity("owed")),
                );
            }
        })
        .await;
        let warn = crate::bridge::test_support::assert_line_level(
            &logs,
            "the oldest owed entry is surrendered",
            "WARN",
        );
        assert!(
            warn.contains("call_0"),
            "the WARN names the surrendered run: {warn}"
        );

        let records = chains.endings("ses_a");
        assert_eq!(records.len(), MAX_OWED_RUN_RECORDS + 1);
        let surrendered = records
            .iter()
            .find(|record| record.run.key == "call_0")
            .expect("the oldest run");
        assert_eq!(
            surrendered.endings[0].state,
            TaskEndingState::Surrendered,
            "the oldest owed ending is surrendered"
        );
        assert!(
            !surrendered.owes(),
            "a surrendered entry is never re-offered, so its record owes nothing"
        );
        assert!(
            records.iter().filter(|record| record.owes()).count() == MAX_OWED_RUN_RECORDS,
            "the owed backlog is capped"
        );
    }

    /// The legacy `rendered` keys are translated ONCE at load (ADR-0080):
    /// `ended:<call_id>` is evidence, `cleanup:<call_id>` the user's Cleanup,
    /// and the pre-#659 `runtime:<call_id>` a legacy class read only by the
    /// retirement's own dedup. Every one was written by a DELIVERED card write,
    /// so it translates SHOWN.
    #[test]
    fn the_legacy_rendered_keys_translate_to_their_classes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(
            &path,
            r#"{"rendered":{"ses_a":["ended:call_1","cleanup:call_2","runtime:call_3"]}}"#,
        )
        .unwrap();
        let chains = ChainRecords::load(path.clone());

        let records = chains.endings("ses_a");
        assert_eq!(records.len(), 3, "one record per legacy run: {records:?}");
        let class_of = |key: &str| {
            let record = records
                .iter()
                .find(|record| record.run.key == key)
                .expect("the run");
            record.endings[0].class
        };
        assert_eq!(class_of("call_1"), TaskEndingClass::Evidence);
        assert_eq!(class_of("call_2"), TaskEndingClass::Cleanup);
        assert_eq!(class_of("call_3"), TaskEndingClass::Legacy);
        for record in &records {
            assert_eq!(
                record.endings[0].state,
                TaskEndingState::Shown,
                "a translated mark was already delivered"
            );
            assert!(!record.owes(), "a legacy mark is never re-offered to a render");
        }
        assert!(chains.ending_shown(
            "ses_a",
            &TaskRunId::of_launch(&launch_task("call_1", None, None)),
            TaskEndingClass::Evidence
        ));

        // Both shapes can meet in ONE file (an older build's `rendered` beside a
        // newer build's `endings`): the translated marks are the OLDER shape, so
        // the file's own records merge into them without double-counting — a run
        // both name keeps the class the marks already recorded, shown bit and
        // all.
        let both = dir.path().join("chain_records_both.json");
        std::fs::write(
            &both,
            r#"{"rendered":{"ses_a":["ended:call_1","ended:call_4"]},
                "endings":{"ses_a":[{"run":{"key":"call_1"},
                "endings":[{"class":"evidence","state":"owed"}]}]}}"#,
        )
        .unwrap();
        let merged = ChainRecords::load(both).endings("ses_a");
        let call_1 = merged
            .iter()
            .find(|record| record.run.key == "call_1")
            .expect("call_1");
        assert_eq!(
            call_1.endings.len(),
            1,
            "a class both shapes carry is not appended twice: {merged:?}"
        );
        assert_eq!(
            call_1.endings[0].state,
            TaskEndingState::Shown,
            "the marks' own SHOWN bit wins for a class both shapes carry"
        );
        assert!(
            merged.iter().any(|record| record.run.key == "call_4"),
            "a mark only the legacy section carries still lands: {merged:?}"
        );
    }

    /// The shown bit is MONOTONIC (ADR-0080): a confirmation only ever sets it,
    /// so an older confirmation — a subset of a newer one's, or an empty one
    /// landing after it — can never clear a newer shown bit.
    #[test]
    fn the_shown_bit_is_only_ever_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let call = |i: usize| TaskRunId::of_launch(&launch_task(&format!("call_{i}"), None, None));
        chains.record_ending(
            "ses_a",
            &call(1),
            TaskEnding::owed(TaskEndingClass::Evidence, identity("one")),
        );
        chains.record_ending(
            "ses_a",
            &call(2),
            TaskEnding::owed(TaskEndingClass::Cleanup, identity("two")),
        );

        assert!(chains.mark_endings_shown(
            "ses_a",
            &[
                (call(1), TaskEndingClass::Evidence),
                (call(2), TaskEndingClass::Cleanup)
            ]
        ));
        assert!(chains.ending_shown("ses_a", &call(1), TaskEndingClass::Evidence));
        assert!(chains.ending_shown("ses_a", &call(2), TaskEndingClass::Cleanup));
        assert!(
            !chains.ending_shown("ses_a", &call(1), TaskEndingClass::Cleanup),
            "another class is not shown"
        );

        // An OLDER body's confirmation names a subset (here, one run): the newer
        // bit it does not name must survive, and a bit already set is a no-op.
        assert!(!chains.mark_endings_shown("ses_a", &[(call(1), TaskEndingClass::Evidence)]));
        assert!(!chains.mark_endings_shown("ses_a", &[]));
        assert!(
            chains.ending_shown("ses_a", &call(2), TaskEndingClass::Cleanup),
            "an older confirmation never clears the newer shown bit"
        );

        // A shown ending is not owed, so it is never the record's owed entry;
        // and a class recorded AGAIN after being shown keeps its shown bit.
        assert!(!chains.endings("ses_a").iter().any(|record| record.owes()));
        chains.record_ending(
            "ses_a",
            &call(1),
            TaskEnding::owed(TaskEndingClass::Evidence, identity("again")),
        );
        let reloaded = ChainRecords::load(path);
        assert!(
            reloaded.ending_shown("ses_a", &call(1), TaskEndingClass::Evidence),
            "re-deriving a class never clears its shown bit"
        );
    }

    /// The class-agnostic gate and the owed state must AGREE (ADR-0080, CI
    /// gate): a stale reconciliation can finish AFTER a Cleanup entry was shown
    /// and record a new Evidence ending. The gate suppresses that ending's entry
    /// forever — one entry per run, whatever class — so the ending must NOT stay
    /// owed: it would be unevictable, and repeated races could consume the owed
    /// cap with no prolonged carrier failure. It is SURRENDERED (the fact kept),
    /// no second entry renders, and its record is ordinary eviction material.
    #[test]
    fn a_shown_ending_surrenders_a_later_ending_of_another_class() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let run = TaskRunId::of_launch(&launch_task("call_gate", Some("shell_1"), None));

        // A Cleanup entry shown first (the click's own ending, delivered) ...
        chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Cleanup, identity("clean")),
        );
        chains.mark_endings_shown("ses_a", &[(run.clone(), TaskEndingClass::Cleanup)]);
        // ... then a STALE reconciliation derives an Evidence ending for the run.
        chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("stale")),
        );

        let records = chains.endings("ses_a");
        let record = &records[0];
        let evidence = record
            .endings
            .iter()
            .find(|ending| ending.class == TaskEndingClass::Evidence)
            .expect("the stale evidence ending");
        assert_eq!(
            evidence.state,
            TaskEndingState::Surrendered,
            "the gate never renders its entry, so it is not owed: {record:?}"
        );
        assert!(!record.owes(), "the record owes nothing: {record:?}");
        // The very predicate the render's gate reads (`plan_runtime_entries` →
        // `facts.shown_any`): the shown Cleanup covers the run whatever class
        // wrote it, so no second entry renders.
        assert!(EndingFacts::new(records.clone()).shown_any(&run));

        // The record is ordinary eviction material: it is the OLDEST unowed
        // record, so it goes first once the section passes the shared bound.
        let call = |i: usize| TaskRunId::of_launch(&launch_task(&format!("call_{i}"), None, None));
        for i in 0..MAX_RUN_RECORDS {
            let run_i = call(i);
            chains.record_ending(
                "ses_a",
                &run_i,
                TaskEnding::owed(TaskEndingClass::Evidence, identity("shown")),
            );
            chains.mark_endings_shown("ses_a", &[(run_i, TaskEndingClass::Evidence)]);
        }
        let records = ChainRecords::load(path).endings("ses_a");
        assert_eq!(records.len(), MAX_RUN_RECORDS, "the section stays bounded");
        assert!(
            !records.iter().any(|record| record.run.key == "call_gate"),
            "the gate-surrendered record is evictable, and it was the oldest unowed: {:?}",
            records.iter().map(|record| &record.run.key).collect::<Vec<_>>()
        );
    }

    /// The reverse arrival order (ADR-0080, CI gate): an Evidence ending is
    /// recorded OWED, then the Cleanup's own entry is shown by a confirmed
    /// write. That SAME write gives the Evidence entry up — the class-agnostic
    /// gate now covers the run — so its state flips to SURRENDERED with the
    /// shown bit, persisted in one Chain Record write.
    #[test]
    fn a_shown_ending_surrenders_an_owed_ending_in_the_same_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        let run = TaskRunId::of_launch(&launch_task("call_1", Some("shell_1"), None));
        chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Evidence, identity("evidence")),
        );
        chains.record_ending(
            "ses_a",
            &run,
            TaskEnding::owed(TaskEndingClass::Cleanup, identity("clean")),
        );

        assert!(chains.mark_endings_shown("ses_a", &[(run.clone(), TaskEndingClass::Cleanup)]));

        // The flip rode the SAME write as the shown bit: the persisted file
        // carries it.
        let record = &ChainRecords::load(path).endings("ses_a")[0];
        let evidence = record
            .endings
            .iter()
            .find(|ending| ending.class == TaskEndingClass::Evidence)
            .expect("the evidence ending");
        let cleanup = record
            .endings
            .iter()
            .find(|ending| ending.class == TaskEndingClass::Cleanup)
            .expect("the cleanup ending");
        assert_eq!(cleanup.state, TaskEndingState::Shown);
        assert_eq!(
            evidence.state,
            TaskEndingState::Surrendered,
            "the shown entry gave the owed one up: {record:?}"
        );
        assert!(!record.owes(), "the record owes nothing: {record:?}");
    }

    /// A gate-surrendered ending does not consume `MAX_OWED_RUN_RECORDS`
    /// (ADR-0080, CI gate): the cap bounds entries a card can still place, and a
    /// record the class-agnostic gate has given up owes none. So it never counts
    /// — the cap's own `WARN` case still fires only for a genuine backlog, and
    /// it names an OWED run, never the gate-surrendered one.
    #[tokio::test]
    async fn a_gate_surrendered_ending_does_not_consume_the_owed_cap() {
        let dir = tempfile::tempdir().unwrap();
        let chains = ChainRecords::load(dir.path().join(FILE));
        let call = |i: usize| TaskRunId::of_launch(&launch_task(&format!("call_{i}"), None, None));
        let ((), logs) = crate::bridge::test_support::capture_logs(async {
            // One record whose entry the gate gave up: a shown Cleanup, then a
            // stale Evidence ending.
            chains.record_ending(
                "ses_a",
                &call(0),
                TaskEnding::owed(TaskEndingClass::Cleanup, identity("clean")),
            );
            chains.mark_endings_shown("ses_a", &[(call(0), TaskEndingClass::Cleanup)]);
            chains.record_ending(
                "ses_a",
                &call(0),
                TaskEnding::owed(TaskEndingClass::Evidence, identity("stale")),
            );
            // Exactly the cap's worth of GENUINELY owed records: with the gate's
            // record not counting, the cap is not exceeded.
            for i in 1..=MAX_OWED_RUN_RECORDS {
                chains.record_ending(
                    "ses_a",
                    &call(i),
                    TaskEnding::owed(TaskEndingClass::Evidence, identity("owed")),
                );
            }
        })
        .await;
        assert!(
            !logs.contains("the oldest owed entry is surrendered"),
            "the gate-surrendered record does not consume a cap slot: {logs}"
        );
        assert_eq!(
            chains
                .endings("ses_a")
                .iter()
                .filter(|record| record.owes())
                .count(),
            MAX_OWED_RUN_RECORDS,
            "the owed backlog sits exactly at the cap"
        );

        // One more genuinely owed record DOES trip the cap, and the WARN names
        // that oldest OWED run — never the gate-surrendered one.
        let ((), logs) = crate::bridge::test_support::capture_logs(async {
            chains.record_ending(
                "ses_a",
                &call(MAX_OWED_RUN_RECORDS + 1),
                TaskEnding::owed(TaskEndingClass::Evidence, identity("owed")),
            );
        })
        .await;
        let warn = crate::bridge::test_support::assert_line_level(
            &logs,
            "the oldest owed entry is surrendered",
            "WARN",
        );
        assert!(
            warn.contains("call_1"),
            "the cap surrenders the oldest OWED run: {warn}"
        );
        assert!(!warn.contains("call_0"), "never the gate-surrendered run: {warn}");
    }
}
