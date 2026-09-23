//! The request wait, split at three seams (spec #298, ticket C): the
//! request-kind adapters, the request-flow engine, and the card-delivery
//! helpers.
//!
//! - `kind` — `PendingRequest` and the `RequestKind` adapters
//!   (`PermissionKind`, `QuestionKind`): the endpoint, card content, state and
//!   click semantics a request kind owns. A third wait costs one adapter
//!   there, not a tour of the module.
//! - `flow` — `RequestFlow`: the poll sweep (poll → prepare → render →
//!   resolve), the in-flight state, and the double-click and claim guards. The
//!   kind seam is its only per-kind dependency.
//! - `delivery` — the card-delivery helpers: `resolve_blocks` (the one
//!   resolution seam), the callback-ack settling, and the neutral receipt
//!   vocabulary.
//!
//! Callers name the owning module (`request::flow::…`, `request::kind::…`,
//! `request::delivery::…`); the submodules are the API, not a re-export shim.

pub(crate) mod delivery;
pub(crate) mod flow;
pub(crate) mod kind;
