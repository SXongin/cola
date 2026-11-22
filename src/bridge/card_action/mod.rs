//! The card-action seam (#1156, #1119): one registry, one module per verb.
//!
//! [`App::handle_card_action`] keeps what every click shares — the Host gate,
//! the payload's `action`/`op` parse, the capability gates, the ONE ADR-0089
//! deadline that starts at the callback, the span context and its
//! `instrument` — and hands the matched verb the `App` (its handles), the
//! payload and the click's two clocks. A **verb module** owns one action tag's
//! body and exports only `run`, matching [`Run`]:
//!
//! ```ignore
//! pub(crate) fn run<'a>(
//!     app: &'a Arc<App>,
//!     value: &'a serde_json::Value,
//!     _deadline: Instant,          // name it `deadline` when the verb uses it
//!     _callback_started: Instant,  // likewise `callback_started`
//! ) -> VerbFuture<'a> {
//!     Box::pin(async move { /* claim, spawn, ack */ })
//! }
//! ```
//!
//! The rule a verb follows: it reads its own payload keys, mints no clock (the
//! dispatch owns `callback_started`), and never re-reads capabilities (it
//! declares a [`Gate`]). A verb bounding its own surface read against the **ack**
//! budget derives that from `callback_started`; the dispatch's `deadline` is
//! the tighter store budget every write settles on (ADR-0089). It returns the
//! click's [`CardActionResult`] or `None` when the click must go unanswered.
//! Adding a verb is one module plus one [`VERBS`] entry — the dispatch is never
//! edited.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::time::Instant;

use crate::bridge::core::SharedCore;
use crate::bridge::handler::{App, CardActionResult};

mod agent;
mod autoaccept;
mod cleanup;
mod mcp;
mod model;
mod perm;
mod question;
mod retry;
mod think;
mod turn_diff;

/// A verb's body, resolved inside the click's [`span`](crate::bridge::span).
pub(crate) type VerbFuture<'a> = Pin<Box<dyn Future<Output = Option<CardActionResult>> + Send + 'a>>;

/// The uniform verb signature: the `App` (and so its handles), the click's
/// payload, and the two clocks the dispatch derived for it — the ADR-0089
/// deadline and the callback's start. A verb names the clocks it does not use
/// with a leading underscore.
pub(crate) type Run = for<'a> fn(&'a Arc<App>, &'a serde_json::Value, Instant, Instant) -> VerbFuture<'a>;

/// Whether a verb's surface exists on the current attachment (ADR-0106).
///
/// The dispatch applies it before the body runs: a card persisted under a
/// client that had the surface may be clicked again after a reattach, so the
/// verb must not guess the capability from its payload.
#[derive(Clone, Copy)]
pub(crate) enum Gate {
    /// Every attachment has this surface.
    Always,
    /// The Backend reads session/turn diffs.
    Diffs,
    /// The Backend can select an agent (`/agent`) with a per-session override.
    Modes,
}

impl Gate {
    pub(crate) fn is_open(self, core: &SharedCore) -> bool {
        match self {
            Self::Always => true,
            Self::Diffs => core.flow_handles().backend.capabilities().diffs,
            Self::Modes => core.flow_handles().backend.capabilities().modes,
        }
    }
}

/// One registry entry.
pub(crate) struct Entry {
    /// The payload's `action` tag; empty for an op-only verb.
    pub(crate) action: &'static str,
    /// The payload's `op` tag, when the verb is selected by it. `None` means
    /// the action tag alone selects the verb.
    pub(crate) op: Option<&'static str>,
    /// The capability the verb's surface needs before it exists at all.
    pub(crate) gate: Gate,
    /// The verb module's body.
    pub(crate) run: Run,
}

/// Every verb behind the seam. Each `action` + `op` key is unique; see
/// [`lookup`] for which one a click selects.
pub(crate) const VERBS: &[Entry] = &[
    // The Turn card's 「🔍 查看完整变化」 carries `op` and NO `action`
    // (#477/spec #797): an op-only entry, matched before the action table.
    Entry {
        action: "",
        op: Some(crate::feishu::card::turn_diff::TURN_DIFF_OP),
        gate: Gate::Diffs,
        run: turn_diff::run,
    },
    Entry {
        action: "perm",
        op: None,
        gate: Gate::Always,
        run: perm::run,
    },
    Entry {
        action: "question",
        op: None,
        gate: Gate::Always,
        run: question::run,
    },
    Entry {
        action: "retry",
        op: None,
        gate: Gate::Always,
        run: retry::run,
    },
    Entry {
        action: "cleanup",
        op: None,
        gate: Gate::Always,
        run: cleanup::run,
    },
    // The mode/configuration group (#1121). `agent`/`agent_clear` need the
    // Backend's `modes` capability: the dispatch applies `Gate::Modes` before
    // the body, so neither module re-reads it.
    Entry {
        action: "agent",
        op: None,
        gate: Gate::Modes,
        run: agent::run,
    },
    Entry {
        action: "agent_clear",
        op: None,
        gate: Gate::Modes,
        run: agent::run_clear,
    },
    Entry {
        action: "autoaccept",
        op: None,
        gate: Gate::Always,
        run: autoaccept::run,
    },
    Entry {
        action: "mcp",
        op: None,
        gate: Gate::Always,
        run: mcp::run,
    },
    Entry {
        action: "model",
        op: None,
        gate: Gate::Always,
        run: model::run,
    },
    Entry {
        action: "think",
        op: None,
        gate: Gate::Always,
        run: think::run,
    },
    Entry {
        action: "think_clear",
        op: None,
        gate: Gate::Always,
        run: think::run_clear,
    },
];

/// The verb a click selects: an op-only entry (one with no `action`) wins over
/// the action table, so the Turn card's action-less diff click keeps routing
/// exactly where it routed before the seam existed.
pub(crate) fn lookup(action: &str, op: &str) -> Option<&'static Entry> {
    let by_op = VERBS
        .iter()
        .find(|entry| entry.action.is_empty() && entry.op == Some(op));
    by_op.or_else(|| {
        VERBS.iter().find(|entry| {
            !entry.action.is_empty() && entry.action == action && entry.op.is_none_or(|key| key == op)
        })
    })
}
