//! The per-Session **Background Task State** cell (ADR-0079, ticket #681).
//!
//! This is the volatile tier's home: the four per-chain delivery sets a card
//! accumulator used to own inline — `announced_wakes`, `handed_over_wakes`,
//! `wake_floor` and `render_marks` — held in [`TaskState`] and reached through
//! [`TaskCell`], the shared handle the accumulator and the per-Session store
//! both hold. `StreamAccumulator` keeps thin forwarding methods that delegate
//! to the cell, so no caller changes.
//!
//! Lock order and the generation-scoping rule are recorded once, canonically,
//! beside `crate::bridge::handles::CardsHandle`'s lock doc; this module keeps
//! only what is specific to the cell (see [`TaskCell`]).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::bridge::chain::TaskEndingClass;

use super::model::TaskRunId;

/// The per-Session Background Task State: the per-chain delivery exactly-once
/// sets. Every field is private to this module — no reader or writer outside
/// `crate::bridge::tasks` touches them directly; callers reach them only
/// through [`TaskCell`]'s methods.
#[derive(Debug, Default, Clone)]
pub(crate) struct TaskState {
    /// The Wakes whose completion this chain has already announced: marked by
    /// its own opening 承接 line (a Wake that opened a card) or by a
    /// completion entry — the merged-path render of a live card, or a yielded
    /// card's ledger refresh. One mark per Wake id; the set is kept across
    /// `continue_on_new_card`, so a Wake that resumes a wake-continuation card
    /// still marks exactly once (ADR-0059). Synthetic retirements have their own
    /// gate, [`Self::announced_endings`] (ADR-0080), because their identity is
    /// the run's ending rather than a Wake id. This is the ANNOUNCEMENT gate (it
    /// keeps an entry single); the Wake step's in-place decision reads
    /// [`Self::handed_over_wakes`], because an entry a ledger refresh placed is
    /// not a handoff. Written by [`Self::announce_entry`] (the one insert),
    /// read by [`Self::wake_announced`].
    announced_wakes: HashSet<String>,
    /// The Wakes whose resumed work this chain has HANDED OVER to a card
    /// (ADR-0066): a 承接 line's split (a continuation card takes it) or the
    /// in-place resume (the same card takes it). The Wake step's in-place gate
    /// keys on THIS, never on `announced_wakes`: a yielded card's ledger
    /// refresh places a completion entry while the Wake's work is still
    /// unrendered (live 2026-10-01 — the resumed message's text part was empty
    /// at the read that placed it), and that entry must not read as a handoff,
    /// or the work would split into a 承接 card when it arrives. Once a Wake IS
    /// handed over, a later tail past it is a Wake-less content diff: it
    /// renders in place on the chain's open yielded card, never a second
    /// continuation (spec #602). Like the announcement set, one mark per Wake
    /// id, kept across `continue_on_new_card`. Written by
    /// [`Self::hand_over_wake`]; read through [`Self::has_handed_over_wake`].
    handed_over_wakes: HashSet<String>,
    /// The chain's durable Wake Watermark as a floor (spec #561, review #569):
    /// every Wake at or below this `created_ms` was announced by an earlier
    /// card, so this accumulator's render neither re-inserts its completion
    /// entry nor stages a watermark advance for it. Seeded when a card starts
    /// rendering a session (`Turn`'s start, the projection/external arms, the
    /// Wake continuation); a strictly newer Wake still announces. `None` when
    /// the session announced no Wake. Seeded by [`Self::set_wake_floor`]
    /// (`Turn::seed_durable_announcements`, through
    /// [`super::TasksHandle::restore`]); read through [`Self::wake_floor`].
    wake_floor: Option<i64>,
    /// The Task Endings this chain has already placed an entry for (ADR-0080):
    /// `(run, class)` pairs marked by [`Self::announce_ending`] when a plan
    /// commits its entry. The retirement's own announce gate: a repeated read
    /// inside one life — the entry's body still owed, refused, or delivered —
    /// never inserts a second timeline entry. Replaces the pre-ADR key
    /// spellings (§`ended:`/`cleanup:`), so the class itself is the identity and
    /// a legacy spelling never gates. A later card seeds the durable half
    /// through the Chain Record's shown bits, not through here.
    announced_endings: Vec<(TaskRunId, TaskEndingClass)>,
    /// The synthetic completion keys an EARLIER card of this Session already
    /// rendered (issue #671, ticket #672): the durable `ended:<call_id>` /
    /// `cleanup:<call_id>` marks `ChainRecords` carries, seeded when a card
    /// starts rendering a session alongside [`Self::wake_floor`].
    ///
    /// **Legacy (ADR-0080).** No live gate reads it: the render asks the Chain
    /// Record's endings for the run's shown bit
    /// ([`crate::bridge::chain::EndingFacts`]), which is the same fact without a
    /// key spelling. The set and its seed stay until the shrinkage slice (#703)
    /// deletes them with the `rendered` section. Deliberately never a Wake fact:
    /// it does not join [`Self::wake_floor`], so a genuinely un-announced late
    /// Wake at or below a retirement's clock still renders and resumes (#590, PR
    /// #595).
    render_marks: HashSet<String>,
}

impl TaskState {
    /// The one exactly-once announcement gate every class shares (ADR-0059):
    /// insert the key and report whether it was new.
    pub(crate) fn announce_entry(&mut self, key: &str) -> bool {
        self.announced_wakes.insert(key.to_string())
    }

    /// Mark `run`'s `class` entry as placed on this chain (ADR-0080) and report
    /// whether it was new — the synthetic retirement's exactly-once gate, keyed
    /// by the ending itself. A class the run already announced (under any id
    /// spelling) is not announced twice.
    pub(crate) fn announce_ending(&mut self, run: &TaskRunId, class: TaskEndingClass) -> bool {
        if self.ending_announced(run, class) {
            return false;
        }
        self.announced_endings.push((run.clone(), class));
        true
    }

    /// Whether `run`'s ending of `class` already had its entry placed on this
    /// chain (ADR-0080). The run matches by ANY id it is known by, so the
    /// retirement and the Wake naming one run differently still agree.
    pub(crate) fn ending_announced(&self, run: &TaskRunId, class: TaskEndingClass) -> bool {
        self.announced_endings
            .iter()
            .any(|(held, held_class)| *held_class == class && shares_id(held, run))
    }

    /// Whether ANY class of `run`'s ending had its entry placed on this chain:
    /// the retirement's rule is one entry per run, whatever class wrote it (as
    /// the whole family's "exactly once per call id" reads).
    pub(crate) fn ending_announced_any(&self, run: &TaskRunId) -> bool {
        self.announced_endings
            .iter()
            .any(|(held, _)| shares_id(held, run))
    }

    /// Whether `wake_id`'s completion was already announced on this chain (spec
    /// #588, #593): the read-only half of the announce gate, so a render can
    /// plan its completion entries — and skip their output reads for the ones
    /// already placed — without mutating the cell.
    pub(crate) fn wake_announced(&self, wake_id: &str) -> bool {
        self.announced_wakes.contains(wake_id)
    }

    /// Mark `wake_id`'s resumed work as handed to a card: the handoff a 承接
    /// line's split or the in-place resume performs (ADR-0059, ADR-0066).
    /// Idempotent, and independent of the announce set — the in-place resume
    /// marks a Wake whose entry a yielded ledger refresh may already have
    /// placed, so the mark must not disturb the announcement set (the entry
    /// stays single).
    pub(crate) fn hand_over_wake(&mut self, wake_id: &str) {
        self.handed_over_wakes.insert(wake_id.to_string());
    }

    /// Whether this chain has already HANDED a Wake's resumed work over to a
    /// card (ADR-0066).
    pub(crate) fn has_handed_over_wake(&self, wake_id: &str) -> bool {
        self.handed_over_wakes.contains(wake_id)
    }

    /// Seed the Wake floor from the chain's durable Watermark (`None` when the
    /// session announced no Wake).
    pub(crate) fn set_wake_floor(&mut self, floor: Option<i64>) {
        self.wake_floor = floor;
    }

    /// The chain's durable Wake Watermark floor, if any (spec #561): every Wake
    /// at or below this `created_ms` was announced by an earlier card.
    pub(crate) fn wake_floor(&self) -> Option<i64> {
        self.wake_floor
    }

    /// Seed the durable rendered marks of the session's earlier cards (empty for
    /// a session that rendered no retirement). **Legacy (ADR-0080)**: see
    /// [`Self::render_marks`]; deleted with it by the shrinkage slice #703.
    pub(crate) fn set_render_marks(&mut self, marks: Vec<String>) {
        self.render_marks = marks.into_iter().collect();
    }

    /// Whether an earlier card of this Session already rendered this key — the
    /// durable mark the pre-ADR render gate consulted (issue #671, ticket
    /// #672). **Legacy (ADR-0080)**: see [`TaskState::render_marks`]; the
    /// shrinkage slice (#703) deletes it with the section.
    #[allow(dead_code)]
    pub(crate) fn render_recorded(&self, key: &str) -> bool {
        self.render_marks.contains(key)
    }
}

/// Whether two run identities name the same run: they share any id (the stored
/// key, the launch's call id, the shell/child id, the Wake id). The in-memory
/// counterpart of the Chain Record's alias resolution, for the announce gate.
fn shares_id(a: &TaskRunId, b: &TaskRunId) -> bool {
    a.ids().any(|id| b.names(id))
}

/// A handle to one per-Session Background Task State cell: a shared, mutable
/// [`TaskState`], held by the session's `StreamAccumulator` and, once bound, by
/// the session's entry in [`super::TasksHandle`]'s store.
///
/// **`Clone` deep-copies.** A cloned handle owns a FRESH [`TaskState`], so
/// `StreamAccumulator`'s derived `Clone` keeps today's exact semantics: the
/// split probes (`Turn::arm_wake_continuation`'s and `Turn::ack_card`'s),
/// `carry_attempt_baseline` and the ~170 session-less
/// `StreamAccumulator::new("test")` fixtures all get an independent,
/// unregistered cell — a speculative write can never reach the live card.
/// [`Self::share`] is the deliberate shallow handle the store keeps to the SAME
/// cell.
///
/// The inner lock is a plain [`Mutex`]: the methods that touch the inner state
/// take it and release it inside the call — never awaited, never held across an
/// await — while [`Self::new`] and [`Self::share`] never take it at all
/// (ADR-0079's task-state lock contract — see [`CardsHandle`]'s lock doc for
/// the one-way order).
///
/// [`CardsHandle`]: crate::bridge::handles::CardsHandle
pub(crate) struct TaskCell {
    inner: Arc<Mutex<TaskState>>,
}

impl TaskCell {
    /// A fresh, empty cell (a session-less accumulator starts here).
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TaskState::default())),
        }
    }

    /// A second handle to the SAME cell — the store's copy, so the session's
    /// registered state is the live card's state. Deliberately NOT `Clone`.
    pub(crate) fn share(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }

    /// The one exactly-once announcement gate (ADR-0059).
    pub(crate) fn announce_entry(&self, key: &str) -> bool {
        self.lock().announce_entry(key)
    }

    /// Mark `run`'s `class` entry as placed on this chain (ADR-0080).
    pub(crate) fn announce_ending(&self, run: &TaskRunId, class: TaskEndingClass) -> bool {
        self.lock().announce_ending(run, class)
    }

    /// Whether `run`'s ending of `class` already had its entry placed here.
    pub(crate) fn ending_announced(&self, run: &TaskRunId, class: TaskEndingClass) -> bool {
        self.lock().ending_announced(run, class)
    }

    /// Whether ANY class of `run`'s ending already had its entry placed here.
    pub(crate) fn ending_announced_any(&self, run: &TaskRunId) -> bool {
        self.lock().ending_announced_any(run)
    }

    /// Whether `wake_id`'s completion was already announced on this chain.
    pub(crate) fn wake_announced(&self, wake_id: &str) -> bool {
        self.lock().wake_announced(wake_id)
    }

    /// Mark `wake_id`'s resumed work as handed to a card (ADR-0066).
    pub(crate) fn hand_over_wake(&self, wake_id: &str) {
        self.lock().hand_over_wake(wake_id);
    }

    /// Whether this chain has already handed a Wake's resumed work over.
    pub(crate) fn has_handed_over_wake(&self, wake_id: &str) -> bool {
        self.lock().has_handed_over_wake(wake_id)
    }

    /// Seed the Wake floor from the chain's durable Watermark.
    pub(crate) fn set_wake_floor(&self, floor: Option<i64>) {
        self.lock().set_wake_floor(floor);
    }

    /// The chain's durable Wake Watermark floor, if any (spec #561).
    pub(crate) fn wake_floor(&self) -> Option<i64> {
        self.lock().wake_floor()
    }

    /// Seed the durable rendered marks of the session's earlier cards.
    /// **Legacy (ADR-0080)**: see [`TaskState::render_marks`].
    pub(crate) fn set_render_marks(&self, marks: Vec<String>) {
        self.lock().set_render_marks(marks);
    }

    /// Whether an earlier card of this Session already rendered this key.
    pub(crate) fn render_recorded(&self, key: &str) -> bool {
        self.lock().render_recorded(key)
    }

    fn lock(&self) -> MutexGuard<'_, TaskState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for TaskCell {
    fn default() -> Self {
        Self::new()
    }
}

// Deliberately a manual `Clone`: a cloned handle must own a FRESH cell, never
// share the live one (see the type's docs).
impl Clone for TaskCell {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::new(Mutex::new(self.lock().clone())),
        }
    }
}
