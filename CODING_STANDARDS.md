# Coding Standards

How code in this repo is written and verified. The `/code-review` skill's
Standards axis reads this file, so keep it in sync with what the hooks and CI
actually enforce.

## Toolchain

Rust edition 2024. The toolchain is pinned by CI (stable). Local dev mirrors CI.

## Verification gates

`CONTRIBUTING.md` "Verification loop" is the single source: it lists the gates,
their commands, and what each `xtask` guard protects. Run that loop before
pushing. Linting here means zero warnings (`-D warnings`), and the
git hooks (lefthook) run the cheap subset automatically (`pre-commit`: fmt +
clippy on staged `*.rs`; `pre-push`: audit).

## Commits

Follow Conventional Commits — see `CONTRIBUTING.md`. The format is enforced by
`cargo xtask check-commit-msg` (subject ≤ 72 chars).

## Code conventions

- **OpenCode protocol traps live in `AGENTS.md` "Known pitfalls"** — the
  camelCase serde renames, the part payloads' missing `id`, the generation split.
  Read them before touching the wire layer: each one silently nulls a field,
  drops an event, or doubles a card's text when ignored.
- **Two OpenCode API generations are mounted — check which one a route belongs
  to.** The unprefixed routes are **V1** (`src/opencode/v1/`), `/api/...` is
  **V2** (`src/opencode/v2/`), and a per-generation **strategy** behind the
  generation-blind `OpenCodeBackend` owns its paths, payloads, decoders and
  semantics, so nothing above the adapter branches on a generation (ADR-0055).
  A shared path can still carry a different contract per generation, so a route's
  call belongs to its own strategy even when the path is shared (ADR-0082), and
  session creation is the one route that stays on the adapter.
- **A card callback runs on a deadline**: the action handler is raced against a
  short ack budget and a late result is dropped — the card is answered early and
  never updated — so the handler behind a click is cheap and bounded: filesystem
  or network work there caps what it scans (entries, bytes) or bounds the call
  with a timeout, rather than growing with the machine's contents or the
  network's latency.
- **A durable exactly-once fact must not ride a staged structure's lifecycle**:
  a stage is evicted at its capacity bound, discarded on a permanent refusal,
  superseded by a newer confirmation, and only sometimes confirmed — a fact
  attached to one can therefore be dropped without ever being written. Derive it
  from the body's own build (the slice the body really rendered), or keep it
  outside the stage; the stage may *carry* it, never *own* it.
- **Use the glossary's vocabulary** (`GLOSSARY.md`): say *Bridge*, *Platform*,
  *Backend*, *Shared Store*, *Owned Server*, *Coexistent Server*, *Session*,
  *Card* — not synonyms the glossary explicitly avoids.
- **Surface ADR conflicts** rather than silently overriding them; record
  hard-to-reverse decisions as new ADRs in `docs/adr/`.
- Keep a change's diff focused (one module changing for one reason); extract
  shared logic instead of duplicating it.

## Domain docs

Single-context: one `GLOSSARY.md` at the repo root, ADRs in `docs/adr/`,
engineering-skill configuration in `docs/agents/`. See `docs/agents/domain.md`.