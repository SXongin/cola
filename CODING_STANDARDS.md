# Coding Standards

How code in this repo is written and verified. The `/code-review` skill's
Standards axis reads this file, so keep it in sync with what the hooks and CI
actually enforce.

## Toolchain

Rust edition 2024. The toolchain is pinned by CI (stable). Local dev mirrors CI.

## Verification gates

These are the CI gates and the pre-PR bar:

| Gate | Command | Notes |
| --- | --- | --- |
| Format | `cargo fmt --all -- --check` | CI's Format job; clippy and rustc do **not** check formatting |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` | zero warnings, `-D warnings` |
| Generation guard | `cargo xtask check-generation` | V1 route literals and wire field names stay inside `src/opencode/v1/` (plus the tool-render allow-list); CI's Check job |
| Tests | `cargo test --workspace --locked` | unit + integration (`src/bridge/test_support.rs`) |
| Build | `cargo build --release --locked` | `--locked` keeps the lockfile authoritative |
| Dep audit | `cargo xtask audit` | cargo-deny over `deny.toml`; also runs in CI |

Run them before pushing; git hooks (lefthook) run the cheap subset
automatically (`pre-commit`: fmt + clippy on staged `*.rs`; `pre-push`: audit).

## Commits

Follow Conventional Commits — see `CONTRIBUTING.md`. The format is enforced by
`cargo xtask check-commit-msg` (subject ≤ 72 chars).

## Code conventions

- **OpenCode event JSON is camelCase**: `callID`, `sessionID`,
  `assistantMessageID`, `textID`, `reasoningID`. Serde structs need
  `#[serde(rename = "...")]` — a missing rename silently nulls the field.
- **Two OpenCode API generations are mounted — check which one a route belongs to** (cola exposes no HTTP API of its own — it is a client of the OpenCode backend). The unprefixed routes are the **V1** compatibility surface (`src/opencode/v1/`); `/api/...` is the current **V2** protocol (`src/opencode/v2/`). The attached generation is probed at attach (spec #364), and a per-generation **strategy** behind the generation-blind `OpenCodeBackend` owns that generation's paths, payloads, decoders and semantics (ADR-0055); the Bridge, Turn, pollers and cards never branch on a generation. V1's conversation routes stay unprefixed — the blocking prompt `POST /session/{id}/message`, the transcript read `GET /session/{id}/message` (decoded by the legacy wire decoder), and the global permission/question endpoints — because the two message stores do not project into each other and the other mounted client (OpenChamber) reads and writes the unprefixed store. V2's prompt is the admit-then-return `POST /api/session/{id}/prompt`, whose blocking trait contract is polyfilled by the experimental `session.wait` plus a confirmed `session.active` poll fallback, bounded by a timeout (ADR-0056; the async-native Turn slice deletes the polyfill). Session creation (`POST /api/session`, parsing the `{data: Session}` envelope, because `location.directory` exists only there) is served by both generations and stays on the adapter; compaction is not — the path is shared but the contract is not (V1 sends no body and answers 204, V2 sends `{}` and answers `{data}`), so each strategy owns its own call. Wire tests pin both sides (`create_session_posts_the_input_and_parses_the_data_envelope`, `v1_compact_posts_the_shared_route_without_a_body`).
- **Part payloads have no `id`**: a part's `id` is a database column not
  serialised into the part JSON. Dedupe text/reasoning on **content**, never on
  a part `id`.
- **Use the glossary's vocabulary** (`CONTEXT.md`): say *Bridge*, *Platform*,
  *Backend*, *Shared Store*, *Owned Server*, *Coexistent Server*, *Session*,
  *Card* — not synonyms the glossary explicitly avoids.
- **Surface ADR conflicts** rather than silently overriding them; record
  hard-to-reverse decisions as new ADRs in `docs/adr/`.
- Keep a change's diff focused (one module changing for one reason); extract
  shared logic instead of duplicating it.

## Domain docs

Single-context: one `CONTEXT.md` at the repo root, ADRs in `docs/adr/`,
engineering-skill configuration in `docs/agents/`. See `docs/agents/domain.md`.