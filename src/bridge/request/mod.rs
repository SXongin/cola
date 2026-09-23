//! The request wait, split at three seams (spec #298, ticket C): the
//! request-kind adapters, the request-flow engine, and the card-delivery
//! helpers.
//!
//! - `kind` — [`PendingRequest`] and the [`RequestKind`] adapters
//!   ([`PermissionKind`], [`QuestionKind`]): the endpoint, card content, state
//!   and click semantics a request kind owns. A third wait costs one adapter
//!   here, not a tour of the module.
//! - `flow` — [`RequestFlow`]: the poll sweep (poll → prepare → render →
//!   resolve), the in-flight state, and the double-click and claim guards. The
//!   kind seam is its only per-kind dependency.
//! - `delivery` — the card-delivery helpers: [`resolve_blocks`] (the one
//!   resolution seam), the callback-ack settling, and the neutral receipt
//!   vocabulary.
//!
//! The re-exports below keep every caller on the paths it used before the
//! split (`crate::bridge::request::…`).

mod delivery;
mod flow;
mod kind;

pub use flow::{RequestFlow, SentCard};
pub use kind::{PendingRequest, PermissionKind, QuestionKind, RequestKind};

pub(crate) use delivery::{Origin, Residue, handled_elsewhere_receipt, resolve_blocks};
pub(crate) use flow::reject_leftovers_for_turn;
pub(crate) use kind::{
    AUTOACCEPT_RECEIPT, approve_pending_for_session, describe_permission, permission_target,
    snapshot_handled_elsewhere_receipt,
};

/// The coordinator's test drives the flag directly; production reaches it
/// through the permission card's toggle in `kind`.
#[cfg(test)]
pub(crate) use kind::set_auto_accept;
