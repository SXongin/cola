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
//! This module is the first slice: it holds the read-model type DEFINITIONS and
//! no state. The per-chain delivery sets (#681) and the liveness overlay (#682)
//! move in next, behind the `read` / `apply` interface. Nothing about the
//! behaviour changes here.

pub(crate) mod model;
