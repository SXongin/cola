# Coding Standards

How code in this repo is written and verified. The `/code-review` skill's
Standards axis reads this file, so keep it in sync with what the hooks and CI
actually enforce.

## Toolchain

Rust edition 2024. The toolchain is pinned by CI (stable). Local dev mirrors CI.

## Verification gates

These four are CI gates and the pre-PR bar:

| Gate | Command | Notes |
| --- | --- | --- |
| Format | `cargo fmt --all -- --check` | CI's Format job; clippy and rustc do **not** check formatting |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` | zero warnings, `-D warnings` |
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
- **Two OpenCode API generations are mounted — check which one a route belongs to** (cola exposes no HTTP API of its own — it is a client of the OpenCode backend, so this is about the endpoints `src/opencode/client.rs` calls). Historically `/api/*` was the older generation (its prompt appended a fresh message and emitted only v2 events that never appeared in readable tables) and the unprefixed paths were canonical; upstream has since made `/api/...` the current protocol (`@opencode-ai/protocol`, `sdk-next`, announced as V2) and kept the unprefixed routes as the V1 compatibility surface. Today cola's V1 read/prompt path is unprefixed — `POST /session/{id}/message`, `GET /permission`, `POST /permission/{id}/reply` — while **session creation goes through the current `POST /api/session`** (parsing the `{data: Session}` envelope), because `location.directory` exists only there. The wire test `create_session_posts_the_input_and_parses_the_data_envelope` pins that request.
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