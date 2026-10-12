//! The per-Session **Background Task State** cell (ADR-0079, ticket #681).
//!
//! This is the volatile tier's home: the four per-chain delivery sets a card
//! accumulator used to own inline — `announced_wakes`, `handed_over_wakes`,
//! `wake_floor` and `render_marks` — moved out of `StreamAccumulator` behind
//! [`TaskState`]'s methods. `StreamAccumulator` keeps thin forwarding methods
//! that delegate here, so no caller changes.
//!
//! # Lock order (ADR-0079, ticket #681)
//!
//! The documented order is one-way — **`write_lock` → `cards` → task state** —
//! and it is recorded beside [`crate::bridge::handles::CardsHandle`]'s own
//! lock doc. Today the cell is owned by value inside the accumulator, so it is
//! reached under the `cards` lock and carries no lock of its own. When a later
//! slice gives the module a per-Session lock, it must be a plain
//! **synchronous** mutex — never awaited and never held across an await — so
//! taking it inside the `cards` lock cannot suspend while holding that lock,
//! and no task-state guard may span an await. No path may hold the task-state
//! lock and then reach for `cards` or `write_lock`; the module's only durable
//! reach ([`super::TasksHandle`]) reads its Chain Record under an internal
//! [`std::sync::Mutex`] and never holds it across an await.
//!
//! # Generation scoping (ADR-0072; ticket #681 trap)
//!
//! The four sets are **per-chain**: like every other `StreamAccumulator`
//! field, they are born with the accumulator's `CardSession` and die with it —
//! reset by `Turn::install_fresh_session`'s unconditional insert (or the
//! `arm_*` paths' conditional one) and kept across `continue_on_new_card` and
//! an arm that finds a live card. The chain **generation**
//! ([`crate::bridge::chain::ChainRecords::generation`]) is *not* the unit of
//! reset: it bumps only at a record re-point, which for a takeover can land
//! after the insert, and it is `0` for a session with no record. A store keyed
//! by generation alone would therefore leak a previous card's sets into a
//! fresh accumulator (generation unchanged) or drop a live card's sets
//! mid-life (generation bumped) — so the sets stay bound to the `CardSession`
//! replacement, exactly as before, and the generation remains the render's
//! orthogonal fact (spec #571).

use std::collections::HashSet;

/// The per-Session Background Task State cell: the per-chain delivery
/// exactly-once sets. Every field is private to this module — no reader or
/// writer outside `crate::bridge::tasks` touches them directly; callers reach
/// them only through the methods below (and, across the seam,
/// `StreamAccumulator`'s forwarding methods).
///
/// It is owned **by value** by [`crate::bridge::turn::state::StreamAccumulator`],
/// which is what keeps its semantics byte-identical to the inline fields it
/// replaced: a fresh accumulator (including the ~170 session-less
/// `StreamAccumulator::new("test")` fixtures) starts from [`Default`], and
/// `StreamAccumulator`'s derived `Clone` deep-copies the cell — so a probe
/// (`Turn::arm_wake_continuation`'s split probe, `Turn::ack_card`'s) cannot
/// mutate the live card's sets through a shared handle.
#[derive(Debug, Default, Clone)]
pub(crate) struct TaskState {
    /// The Wakes whose completion this chain has already announced: marked by
    /// its own opening 承接 line (a Wake that opened a card) or by a
    /// completion entry — the merged-path render of a live card, or a yielded
    /// card's ledger refresh. One mark per Wake id; the set is kept across
    /// `continue_on_new_card`, so a Wake that resumes a wake-continuation card
    /// still marks exactly once (ADR-0059). Synthetic retirements — a
    /// runtime/child-evidence retirement or the user's cleanup — share the set
    /// under their `runtime:<call_id>` key, marked through
    /// [`Self::announce_entry`]: the same exactly-once gate, staging nothing
    /// durable (spec #588, review PR #595). This is the ANNOUNCEMENT gate (it
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
    /// [`super::TasksHandle::seed`]); read through [`Self::wake_floor`].
    wake_floor: Option<i64>,
    /// The synthetic completion keys an EARLIER card of this Session already
    /// rendered (issue #671, ticket #672): the durable `runtime:<call_id>`
    /// marks `ChainRecords` carries, seeded when a card starts rendering a
    /// session alongside [`Self::wake_floor`]. Every retirement here was shown
    /// by a delivered card write, so this accumulator's render never re-inserts
    /// its entry — the restart case the per-chain announce set cannot cover.
    /// Deliberately NOT a Wake fact: it never joins [`Self::wake_floor`], so a
    /// genuinely un-announced late Wake at or below a retirement's clock still
    /// renders and resumes (#590, PR #595). Seeded by
    /// [`Self::set_render_marks`] (`Turn::seed_durable_announcements`, through
    /// [`super::TasksHandle::seed`]); read through [`Self::render_recorded`].
    render_marks: HashSet<String>,
}

impl TaskState {
    /// The one exactly-once announcement gate every class shares (ADR-0059):
    /// insert the key and report whether it was new.
    pub(crate) fn announce_entry(&mut self, key: &str) -> bool {
        self.announced_wakes.insert(key.to_string())
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

    /// Seed the durable rendered marks of the session's earlier cards (empty
    /// for a session that rendered no retirement).
    pub(crate) fn set_render_marks(&mut self, marks: Vec<String>) {
        self.render_marks = marks.into_iter().collect();
    }

    /// Whether an earlier card of this Session already rendered this synthetic
    /// retirement's entry — the durable mark the render's gate consults beside
    /// [`Self::wake_announced`] (issue #671, ticket #672). Read-only: the plan
    /// runs before the commit that announces, so it must not mutate.
    pub(crate) fn render_recorded(&self, key: &str) -> bool {
        self.render_marks.contains(key)
    }
}
