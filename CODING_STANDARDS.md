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
- **OpenCode's API paths have no `/api` prefix** (cola exposes no HTTP API of its own — it is a client of the OpenCode backend, so this is about the endpoints `src/opencode/client.rs` calls): `POST /session/{id}/message`, `GET /permission`, `POST /permission/{id}/reply`. The old `/api/...` paths only emit v2 events that never appear in readable tables.
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