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
//! - [`handle::TasksHandle`] — the module's one handle and per-Session store,
//!   created by `src/main.rs` and threaded into the adapter **at construction**
//!   (#682). It keys each cell by `(session, chain generation)` — the
//!   generation selects the entry, and a re-install prunes the session's other
//!   generations — reads the durable tier a fresh cell is seeded from, re-keys
//!   a binding when its card moves to a fresh session
//!   ([`handle::TasksHandle::rekey`], `Turn::recreate`), and **owns the
//!   volatile liveness tier**: the process-local memoization (#682) the
//!   adapter applies to every read at the pinned apply seam and the Bridge
//!   records through the same handle. There is one instance, held by the
//!   adapter and the Bridge alike.
//!
//! Nothing about the behaviour changes here.
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
//! card. The chain **generation** is the store's key — it selects which of a
//! session's entries a lookup names — but it is *not* the reset unit: it bumps
//! only at a record re-point (which can land after install) and is `0` for a
//! recordless session, so the reset happens at the `CardSession` replacement
//! (which prunes the session's other generations) and the generation stays the
//! render's orthogonal fact. Because a re-point follows the install, a binding's
//! stamped generation can lag; the store's read re-stamps it at the chain's
//! current generation rather than resolving a stale key, and
//! [`handle::TasksHandle::rekey`] re-stamps and moves the binding when a
//! `Turn::recreate` remaps the card to a fresh session. See [`state::TaskCell`]
//! for the clone/lifetime argument.

pub(crate) mod handle;
pub(crate) mod model;
pub(crate) mod state;

pub(crate) use handle::TasksHandle;
pub(crate) use state::TaskCell;

/// The liveness tier's own unit tests (`src/backend/transcript.rs`) still name
/// the type they construct; it is the module's handle, since #682 folded the
/// overlay into it. Test-only: production reaches the tier through the handle.
#[cfg(test)]
pub(crate) type BackgroundTaskOverlay = TasksHandle;
