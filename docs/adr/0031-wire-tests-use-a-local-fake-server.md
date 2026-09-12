# Wire tests run the real HTTP client against a local fake server

`feishu::Client` is cola's only code that speaks to the open internet: tenant
tokens, card sends, message reads, image downloads. It is also where the
hard-won bugs lived — Basic auth needing the username too, camelCase event
fields, the global permission endpoints — and it had no direct tests. The
`Platform` and `Backend` trait seams (ADR-0010) mock the *client*; they never
exercise the URLs, query strings, headers and bodies production actually sends,
so a wrong endpoint or a dropped header passed every test in the suite.

The live E2E harness was the only thing that touched the wire, and it rotted:
ignored in CI, requiring a second bot and a test group, and able to assert only
card fallbacks, never card bodies. Retiring it with the dead harness removed the
last nominal wire coverage without replacing it.

## Decision

1. **The client takes its base URL from its constructor.** `Client::new` uses
   `https://open.feishu.cn`; a production-usable `Client::with_base_url`
   builds a client against any Feishu-compatible host. Every HTTP call site
   builds its URL from that field — no hardcoded hosts remain, including the
   WebSocket endpoint fetch.
2. **Wire tests run the real client against a local HTTP server.** A shared
   `cfg(test)` module, `src/test_http.rs`, binds `127.0.0.1` on an ephemeral
   port, answers from a route table (method + path prefix → status + body) and
   records every request (method, path, query, headers, body). Tests assert
   both sides of the exchange: what went out on the socket and what the client
   parsed back. No HTTP library is mocked and no new dev-dependency is added.
3. **The trait seams stay.** `Platform`/`Backend` remain the seam for swapping
   the client (ADR-0010); the fake server adds the HTTP endpoint seam beneath
   them. They test different things: the traits drive bridge behavior with a
   scripted client, the wire tests pin the client's own protocol behavior.
4. **The fake server does not replace the release smoke test.** It can prove
   cola sends the bytes it means to send; only real Feishu can prove Feishu
   accepts a card schema. The CONTRIBUTING release checklist remains the
   acceptance check for that.

## Considered options

- **Mock `reqwest` (mockall-style) or add an HTTP-mocking crate.** Rejected:
  it mocks the library boundary rather than the wire, so a request built with
  the wrong path or a missing Bearer header still passes; and it adds a
  dev-dependency plus a mock layer for no behavior gained over a real socket.
- **Keep relying on the `Platform`/`Backend` trait seams.** Rejected: those
  tests replace the client entirely, so the URL/header/body construction — the
  historical bug class — is never executed.
- **Revive the live E2E harness.** Rejected: needs a second bot and a test
  group, cannot see card bodies, and had already rotted for a month while CI
  stayed green.
- **Record/replay HTTP cassettes (VCR pattern).** Rejected: requires
  credentials or committed fixtures, and replayed responses drift from the real
  API without an update ritual.

## Consequences

- Every public `feishu::Client` method is covered by a wire test that runs
  deterministically in CI, with no credentials and no network.
- `opencode::Client` can reuse `test_http` for its wire tests (a follow-up
  ticket), so the infrastructure cost is paid once.
- The hand-written fake server is itself test infrastructure: its request
  parsing and route matching carry two self-tests, and it must stay minimal.
- A changed URL, query or body now fails with the request log showing exactly
  what was sent — the diagnostic the old harness could not give.
