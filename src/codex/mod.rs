//! A client for `codex app-server` (spec #860, tickets #876/#877/#878/#879).
//!
//! This is the adapter's first slices: transport, handshake and the pinned
//! schema contract; daemon discovery and the lifecycle; thread discovery,
//! adoption and subscription; and the neutral read model served from the
//! adapter-owned subscription. The bridge never names this module — it reaches
//! the active client through `SharedCore::client` and the `crate::backend`
//! contract (ADR-0090); `cargo xtask check-client-seam` keeps the OpenCode
//! adapter's module path out of everything beside it.
//!
//! The surface the adapter is written against is pinned in
//! `docs/reference/codex-app-server-0.163.0.md`, generated from
//! `codex app-server generate-json-schema`. At construction the client reads
//! `initialize`'s result (the binary's identity and version), then checks that
//! the required methods are present before any prompt is possible. A version
//! other than the pin is either verified against the running binary's own
//! generated schema or refused loudly — never guessed (ADR-0090's
//! probe-and-degrade consequence, the shape ADR-0055 gave generation
//! detection).

// The composition root constructs this adapter by `ActiveClient.kind`, so the
// slice is reachable from `main`; the remaining test-only helpers are marked
// where they live rather than silencing the whole module.

pub(crate) mod authorship;
pub(crate) mod backend;
pub(crate) mod client;
pub(crate) mod discovery;
pub(crate) mod error;
pub(crate) mod lifecycle;
pub(crate) mod read_model;
pub(crate) mod schema;
pub(crate) mod threads;
pub(crate) mod transport;

#[cfg(test)]
pub(crate) mod test_server;

#[cfg(test)]
mod cards;

#[allow(unused_imports)]
pub(crate) use authorship::Authorship;
#[allow(unused_imports)]
pub(crate) use client::{CodexClient, CodexOptions, ServerInfo, ServerVersion, connect_for_active_client};
#[allow(unused_imports)]
pub(crate) use error::{CodexError, Result};
#[allow(unused_imports)]
pub(crate) use lifecycle::{CodexLifecycle, ManagedProbe, ProbeSource};
#[allow(unused_imports)]
pub(crate) use schema::{AutoProbe, PinnedProbe, Surface, SurfaceProbe};
#[allow(unused_imports)]
pub(crate) use threads::{
    Adoption, INTERACTIVE_SOURCE_KINDS, Thread, ThreadEvent, ThreadPage, ThreadSubscription, Threads,
    UnsubscribeStatus,
};
#[allow(unused_imports)]
pub(crate) use transport::{Endpoint, Inbound, RpcConnection};
