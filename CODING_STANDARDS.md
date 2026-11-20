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
`clippy` on the bin target for staged `*.rs`; `pre-push`: the full
`clippy --all-targets` + the structure ratchet + audit).

`cargo xtask check-structure` is the growth ratchet (the code contract, #1109):
a function over cognitive complexity 25 or over 100 lines, a production file over
2,000 lines, a new `#[allow(clippy::too_many_arguments)]` without a reason and a
tracking issue, a new `#[cfg(test)]` production site, and a `["tag"]` probe
outside `src/feishu/card` all fail the build unless they are already in
`xtask/structure-baseline.json`. It only ratchets down: fix the offender and
re-bless to record a lower baseline, never edit a value up except in a reviewed
diff. The charter's ratchet table (`docs/adr/0110-the-refactoring-charter.md`)
says which rules a guard decides and which stay review-only.

## Tests

The test contract is ADR-0110's second decision; the cost evidence is #1106.

- **One observable behaviour per test.** Many assertions may assert that one
  behaviour; a test pinning two independent behaviours is two tests. Ordering
  and race tests, golden tests and wire-shape tests (a fake server) are the
  enumerated exceptions — their one behaviour is the order or the shape.
- **An absence assertion names its counterpart**: `assert!(!…)` says what would
  have made it true, or a rename turns a real check vacuous.
- **A new test-only production twin is banned** — a `#[cfg(test)]` seam that
  exists only so a test can reach a path. `check-structure` counts the sites;
  the existing twins are C9's deletion target.
- **Delete the cheapest duplicate first**: repeated negatives, then test-only
  twins, then duplicate helpers, then same-shape merges. A deletion keeps a
  surviving test that fails when the behaviour breaks; a deleted test with no
  such survivor is coverage removed, not redundancy.
- **CI targets hotspots, not the count.** The Test job's budget is 6 minutes; the
  module whose tests dominate the wall time is the one to fix. Nightly sampled
  mutation is the value signal; coverage ratio and `nextest` are not adopted.

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
- **A guard or a fallback for an input that cannot occur is a defect, not
  diligence** (#1109): it reads as robustness but is untested code on a path
  production never takes, and a test written to exercise it is not coverage.
  (Judgement-only; the gate may flag it, never block on it.)
- **A module's size is budgeted**: a new production file may not cross 2,000
  lines, a baselined one may not grow 10%, a function over cognitive complexity
  25 or 100 lines is an offender, and a new
  `#[allow(clippy::too_many_arguments)]` needs a reason and a tracking issue
  (ADR-0110). `check-structure` fails only on new or worse, so touching a
  function that can fit is the moment to cut it — not a separate ticket.
- **Card JSON lives in `feishu/card`**: a `["tag"]` probe elsewhere needs an
  allowlist entry, and the `CardElement` type layer is the segment that will own
  the schema. Run lifecycle has one owner, not four homes.
- **A comment carries an invariant, a wire/why trap, or an API contract the
  signature cannot express.** A bare pointer (an issue, an ADR) is fine; a
  retelling of the code is not — the docs contract found ~38% pure restatement
  in the largest modules (#1112).
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