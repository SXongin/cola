//! A client for `codex app-server` (spec #860, tickets #876/#877/#878).
//!
//! This is the adapter's first slices: transport, handshake and the pinned
//! schema contract; daemon discovery and the lifecycle; and thread discovery,
//! adoption and subscription. The bridge never names this module — it reaches
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

// Nothing in the binary constructs the Codex adapter yet: #877 landed daemon
// discovery and the lifecycle, but the composition root and the `Backend`
// implementation are #879's, and `main`'s capability gate still refuses a
// `codex` client. Until that root exists nothing here is reachable from
// `main`, and dead-code analysis (a bin crate's) would flag the whole slice —
// including the paths #877 *did* give real callers inside the slice
// (`CodexLifecycle::ensure_attached` reaches discovery, the handshake, the
// managed-binary probe and the neutral error). The silence is removed by the
// ticket that constructs the adapter, not item by item.
#![allow(dead_code)]

pub(crate) mod client;
pub(crate) mod discovery;
pub(crate) mod error;
pub(crate) mod lifecycle;
pub(crate) mod schema;
pub(crate) mod threads;
pub(crate) mod transport;

#[cfg(test)]
pub(crate) mod test_server;

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
