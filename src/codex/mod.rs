//! A client for `codex app-server` (spec #860, ticket #876).
//!
//! This is the first adapter slice: transport, handshake and the pinned schema
//! contract, and nothing above them. The bridge never names this module — it
//! reaches the active client through `SharedCore::client` and the
//! `crate::backend` contract (ADR-0090); `cargo xtask check-client-seam` keeps
//! the OpenCode adapter's module path out of everything beside it.
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

// Nothing above this adapter exists yet: #877 adds daemon discovery and the
// lifecycle, #878+#879 the Backend implementation and the subscription read
// model. Until then the binary has no caller, and a bin crate's dead-code lint
// would flag the whole slice.
#![allow(dead_code)]

pub(crate) mod client;
pub(crate) mod discovery;
pub(crate) mod error;
pub(crate) mod schema;
pub(crate) mod transport;

#[cfg(test)]
pub(crate) mod test_server;

#[allow(unused_imports)]
pub(crate) use client::{CodexClient, CodexOptions, ServerInfo, ServerVersion, connect_for_active_client};
#[allow(unused_imports)]
pub(crate) use error::{CodexError, Result};
#[allow(unused_imports)]
pub(crate) use schema::{AutoProbe, PinnedProbe, Surface, SurfaceProbe};
#[allow(unused_imports)]
pub(crate) use transport::Endpoint;
