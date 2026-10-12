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
//! - [`state::TaskState`] — the per-Session cell holding the four per-chain
//!   delivery sets (`announced_wakes`, `handed_over_wakes`, `wake_floor`,
//!   `render_marks`), moved out of `StreamAccumulator` in #681 behind thin
//!   forwarding methods so no caller changes.
//! - [`handle::TasksHandle`] — the module's entry point, created beside the
//!   durable **Chain Record**; today it owns the durable seed
//!   (`restore(record, scope)`), which is what a fresh cell is born from.
//!
//! The liveness overlay (#682) moves in next, behind the `read` / `apply`
//! interface. Nothing about the behaviour changes here.
//!
//! # Lock order (ADR-0079, ticket #681)
//!
//! The order is one-way: **`write_lock` → `cards` → task state**. A card write
//! takes its session's `write_lock` first and holds it across the whole
//! read-send-record sequence; the per-Session task cell is reached inside the
//! `cards` lock (the seed in `Turn::install_fresh_session` is the live
//! example, sharing the cards critical section on purpose — issue #671).
//! Whenever a later slice gives the module a per-Session lock it must be a
//! plain **synchronous** mutex: never awaited, never held across an await, so
//! acquiring it inside the `cards` lock cannot suspend while holding that lock.
//! No path may hold the task-state lock and then reach for `cards` or
//! `write_lock`.
//!
//! # Per-chain scoping (ADR-0072)
//!
//! The four sets are **per-chain**: they die with the `CardSession` that
//! replaced them and survive `continue_on_new_card` / an arm that finds a live
//! card. The chain **generation** is *not* the reset unit — it bumps only at a
//! record re-point (which can land after install) and is `0` for a recordless
//! session — so the cell is bound to the `CardSession` replacement, and the
//! generation stays the render's orthogonal fact. See [`state`] for the full
//! argument.

pub(crate) mod handle;
pub(crate) mod model;
pub(crate) mod state;

pub(crate) use handle::TasksHandle;
pub(crate) use state::TaskState;
