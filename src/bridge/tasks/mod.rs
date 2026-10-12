//! The Background Task State module (ADR-0079).
//!
//! One owner for a **Background Task**'s lifecycle, per Session: which tasks
//! are live, what ended each one, and whether its completion entry has already
//! been shown — the facts today spread across the Backend's process-local
//! overlay, the `StreamAccumulator`'s delivery sets and the **Chain Record**'s
//! durable sections.
//!
//! Its interface is a read model — the rows the **Background Task Ledger**
//! renders plus the completion entries a read owes ([`model`]) — and never the
//! card itself: the pinned copy, formats and budgets stay in
//! `src/feishu/card/ledger.rs`.
//!
//! # What lives here now
//!
//! - [`model`] — the read-model type definitions (#680), the rows the
//!   **Background Task Ledger** renders.
//! - [`state::TaskState`] / [`state::TaskCell`] — the per-Session cell holding
//!   the four per-chain delivery sets (`announced_wakes`, `handed_over_wakes`,
//!   `wake_floor`, `render_marks`), and the shared handle to it. The cell moved
//!   out of `StreamAccumulator` in #681; the accumulator holds a `TaskCell` and
//!   keeps thin forwarding methods, so no caller changes.
//! - [`handle::TasksHandle`] — the per-Session store, created beside the durable
//!   **Chain Record**: it maps a session to its cell (recording the chain
//!   generation the binding was made under) and reads the durable tier a fresh
//!   cell is seeded from.
//!
//! The liveness overlay (#682) moves in next, behind the `read` / `apply`
//! interface. Nothing about the behaviour changes here.
//!
//! # Lock order
//!
//! Recorded once, canonically, beside
//! `crate::bridge::handles::CardsHandle`'s lock doc: `write_lock` → `cards` →
//! task state.
//!
//! # Per-chain scoping (ADR-0072)
//!
//! The four sets are **per-chain**: they die with the `CardSession` that
//! replaced them and survive `continue_on_new_card` / an arm that finds a live
//! card. The chain **generation** is recorded on the store's binding, but it is
//! *not* the reset unit — it bumps only at a record re-point (which can land
//! after install) and is `0` for a recordless session — so the reset happens at
//! the `CardSession` replacement, and the generation stays the render's
//! orthogonal fact. See [`state::TaskCell`] for the clone/lifetime argument.

pub(crate) mod handle;
pub(crate) mod model;
pub(crate) mod state;

pub(crate) use handle::TasksHandle;
pub(crate) use state::TaskCell;
