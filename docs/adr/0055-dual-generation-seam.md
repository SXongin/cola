# Dual-generation OpenCode support: one generation-blind seam, generation strategies behind it

## Context

V2 is the current OpenCode protocol (`/api`-only); V1 (the unprefixed routes) is
the compatibility surface cola was built on. cola must speak both without
regressing V1, and V1 is retired only on the Host's explicit order. ADR-0053's
amendment parked the `/api` decoder while two clients disagreed about the shared
message store; that coordinated migration is now under way (OpenChamber v2), so
cola can re-add the generation.

## Decision

The `Backend` trait stays generation-blind. Protocol differences live in a
per-generation **strategy module** behind the one adapter: paths, payloads,
decoders, status/permission/form/prompt semantics. Attachment is single-server;
the generation is detected per attach via `GET /api/info` (200 = V2, 404 = V1)
with a `[opencode] generation` override as the escape hatch. `SessionStore`
stays generation-free; detection is observable in attach logs and never guessed.
Tool-payload knowledge stays Platform contract, not protocol coupling
(ADR-0053's porosity exception extends to V2 tool names).

## Considered Options

- **Branching by generation inside the Bridge/Turn.** Rejected: breaks
  ADR-0053, and retirement corrodes into the Bridge.
- **Two full `Backend` implementations.** Rejected: trait boilerplate
  duplicated, shared Turn/poller logic forked.
- **Per-call-site checks.** Rejected: the drift ADR-0053 removed.
- **Dual attach.** Rejected: one shared store, forked pollers/mapping.

## Consequences

- A protocol-generation change becomes one strategy module + one selection arm;
  V1 retirement is a deletion.
- Discovery gains a probe step, and V2 credentials come from the service
  registration file (plain `serve` stays on `/proc`).
- ~~The experimental `wait` endpoint is a temporary, recorded dependency until
  the async-native Turn slice (ADR-0056).~~ **Removed 2026-09-27**: the
  async-native Turn slice (spec #364 S8) deleted the polyfill, so cola no
  longer depends on the experimental `wait` endpoint (ADR-0056 amendment).
  `GET /experimental/session` — the cross-project session list of ADR-0008 —
  is a separate, older dependency and is unaffected.

The effort, its detection/override rules and its slicing are spec #364
(OpenCode V2 dual-generation support).

## Amendment (2026-09-27): the probe classifies by body, not by status

The Decision above says the generation is detected via `GET /api/info`
"200 = V2, 404 = V1". Measured on this machine, that status rule does not hold:
V1 serves its web UI through a catch-all, so `/api/info` on V1 is a **200
`text/html`** page, not a 404 (both the running 1.18.23 and the pinned
1.18.31). The status alone therefore cannot discriminate. The rule implemented
in `src/opencode/generation.rs` (spec #364, slice S3) is:

- **200 + V2's JSON info envelope = V2.** The envelope is recognised by its
  `version` string and `urls` array (V2 serves `{version, pid, urls, paths}`;
  only those two keys are required);
- **200 `text/html` = V1** (the web-UI catch-all);
- **404 = V1** (no `/api` surface);
- **everything else = inconclusive**: a 200 without the envelope (JSON or any
  other content type), 401, 503, a transport error. An inconclusive probe is
  never guessed at — cola stays serverless and Lazy Start / the reconnect scan
  retries;
- `[opencode] generation = "auto" | "v1" | "v2"` still forces the choice when
  the probe cannot be trusted, with a WARN on a contradicting classified probe.

Everything else in this ADR stands.
