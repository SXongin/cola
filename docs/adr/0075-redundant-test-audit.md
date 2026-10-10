# Redundant tests are nominated cheaply and confirmed by a deletion experiment

## Context

Mutation testing answers "is this line tested?", never "is this test necessary?".
The `cargo-mutants` audit of #303 reported that *at least one* test killed every
non-equivalent mutant — it cannot say *which* one — so duplicate coverage is
invisible to it: delete a redundant test and the mutant report is unchanged.
Spec #298 deliberately set no deletion target, so the suite's redundancy was
never measured, and #311 asks for a repeatable, evidence-based method to find
redundant tests, applied as a pilot over the Turn modules.

Reconnaissance on the current tree (`ab98b3b`, 2026-10-10) fixed the tooling
reality the method has to live within:

- **`cargo-mutants` 27.1.0 is the only relevant tool present**, and it has **no
  per-test attribution feature** — no PIT-style `fullMutationMatrix`, nothing
  beyond `--list`, `--shard`, `--test-tool` and the `mutants.out/` outcome sets.
  "Which test killed this mutant" has no supported query; the per-mutant logs
  under `mutants.out/log/` do name the failing tests, so a run gives attribution
  by hand only, and the method does not rely on it.
- `cargo-llvm-cov`, `cargo-nextest` and `grcov` are **not installed**. crates.io
  is reachable and the `llvm-tools` rustup component is present, so per-test
  coverage *is* obtainable — at the cost of two new toolchains — but it is the
  weakest signal (same line ≠ same behaviour) and it answers "covered lines are
  a subset", which is neither necessary nor sufficient for redundancy.
- `--in-place` and `--jobs` are mutually exclusive, so an in-place run is
  **serial** (~2.5 min/mutant here: every mutant rebuilds and relinks the test
  binary). Copy mode with `-j 4` reaches ~77 s/mutant. A full run is therefore
  **hours**, and a pilot's scopes must be small.
- **#303's scopes are not reusable as a baseline.** The #617 split grew them from
  144 to 335 mutants in total (2.3×; `turn/flush.rs` 27→51, `turn/render.rs`
  89→198, and `turn/state.rs` shard 1/8 28→86 — the last ~3.1×) and moved every
  line; a kill set is valid only for the tree it was taken on.

The suite the method operates on is 1,938 test functions across 102 files;
`src/bridge/tests/` alone is ~56k lines.

## Decision

Adopt a **two-tier method**. Tier 1 nominates candidates cheaply; **only Tier 2,
a deletion experiment, authorises a deletion**. Every finding records its tier
and its error modes, and ends in an explicit disposition.

### Tier 1 — nomination (cheap, repeatable, never decisive)

Over the scope's test source — `#[cfg(test)] mod tests` bodies in `src/` and the
`src/bridge/tests/*.rs` files alike — nominate with these signals, each emitting
`file:line` plus a **witness** (the twin test, or the helper/table to switch to):

1. **Body similarity.** Strip comments, collapse string literals, tokenise, and
   compare bodies pairwise within size buckets (`difflib.SequenceMatcher`,
   ratio ≥ 0.90). The witness is the token-level diff of the non-equal opcodes.
2. **Duplicate names.** Tests sharing a function name across files — a twin, or
   a copy-paste collision.
3. **Implementation-detail assertions.** `to_string().contains(` on card JSON
   and literal-copy `.contains(` where a structural helper
   (`card_text`/`card_buttons`/`card_header`) or a copy table already exists —
   the residue #303 began collapsing.
4. **Same behaviour at two granularities.** A unit test on a module and an
   integration test that pin the same user-visible rule (e.g. the Turn Footer in
   `turn/state.rs` and in the `prompt_render.rs` turn).

Nomination is **not** evidence. It over-nominates input variants and
under-detects behavioural duplicates whose rigs differ; it exists only to make
Tier 2's denominator small.

### Tier 2 — the deletion experiment (the only arbiter)

1. Fix a **scope** small enough to run twice: a `cargo-mutants` file plus a `--re`
   on the function that owns the candidate's behaviour (or a `--shard`).
2. **Before** — run the scope with the candidate tests present; the
   `mutants.out/{caught,missed,timeout,unviable}.txt` files are the baseline kill
   set (`--baseline run` must be green).
3. Delete the **candidate batch** and re-run the *identical* scope.
4. Diff the four outcome sets. **Unchanged ⇒ the batch is redundant *within that
   scope*.** A mutant that flips `caught → missed` ⇒ the batch contained a
   necessary test: **keep it and cite the mutant**. Bisect a mixed batch to
   attribute.

"Unchanged" is evidence about the scope's mutant set, nothing more. That set is a
sample of the behaviour, and mutation testing carries the mutation-adequacy limit
in the general case: a candidate that uniquely exercises an input or branch for
which the scope generated no representative mutant looks redundant. So "unchanged"
authorises a deletion only after naming the mutant that stands for the candidate's
distinguishing behaviour; if none exists, the candidate is **kept**, not deleted.

Interpretation is uniform: **only a fully unchanged four-set diff authorises a
deletion.** Any transition — `caught → missed`, `caught → timeout`, a moving
`timeout` count — is evidence the batch changed the scope's behaviour, so keep the
batch and bisect to attribute. The one ambiguous case is a `timeout`/`caught` move
caused by load (the suite runs slow and crosses the timeout); re-run the scope
before reading it as a real change.

The commands are ordinary `cargo-mutants`:

```
# footprint before (scope: the function that owns the candidates' behaviour)
cargo mutants --timeout 120 --baseline run -j 4 --copy-target true \
  -f src/bridge/turn/state.rs -F build_card_inner -o /tmp/mut/before
# delete the candidate batch, then the identical scope
cargo mutants --timeout 120 --baseline run -j 4 --copy-target true \
  -f src/bridge/turn/state.rs -F build_card_inner -o /tmp/mut/after
# diff every outcome set, not just the kills
for f in caught missed timeout unviable; do
  diff <(sort /tmp/mut/before/mutants.out/$f.txt) \
       <(sort /tmp/mut/after/mutants.out/$f.txt) || echo "=> $f changed"
done
```

### Disposition

Every candidate ends in exactly one of:

- **deleted** — cite the unchanged kill-set diff;
- **kept deliberately** — say why (it runs in milliseconds where its integration
  twin needs a git repo; it is the surviving test for a mutant the integration
  test does not reach; it owns a seam the twin does not);
- **rewired** — collapse a variant *family* to a table, or assert through one
  shared helper, without deleting coverage.

There is **no target count**. A redundant test may be kept; a necessary one is
never deleted to hit a number.

### Error modes (recorded with every finding)

1. **Over-nomination.** A near-identical body whose only difference is the
   *input* is a variant, not a duplicate. In the pilot, essentially every
   ≥0.90 pair outside a wildcard set was this: `switch.rs`'s adopt-by-id vs
   adopt-by-title differ only in the session id and title literals; `autostart`'s
   exec-path trio differ only in the path.
2. **Under-detection.** Two tests with different text (different rigs, different
   helpers) can exercise one behaviour; text similarity is blind to it. That is
   what Tier 2 is for.
3. **Scope-bounded inference / mutation adequacy.** "Redundant within this
   scope" ≠ "redundant overall". The gate is Tier 2 step 4 — name the mutant that
   represents the candidate's distinguishing behaviour, or keep it. Report the
   scope with every deletion; never claim global redundancy.
4. **Aggregation hides a member.** A batch that leaves the kill set unchanged is
   safe, but a batch that *changes* it may hide one necessary test among
   redundant ones — bisect before attributing.
5. **Equivalent mutants.** An unchanged kill set can hide a test that killed only
   an equivalent mutant. Harmless, but "unchanged" is evidence of redundancy,
   not proof.
6. **Stale baselines.** Never reuse a kill set across a refactor. #303's scopes
   moved and grew under #617; take a fresh before-run on the tree under
   examination.

### Guards

- The #303 surviving-test baseline and the `test_http`/`test_ws` wire tests
  (ADR-0031) are **off-limits**.
- No CI job: the method is local and one-off, and nothing here changes production
  behaviour or the test seams themselves.
- The residue counts and line numbers in #618's C11 comment are **stale** — its
  `~125` `to_string().contains(` predates #303's 94 conversions (the tree now has
  70), and its `turn/` line citations no longer resolve after #617. Re-derive on
  the current tree; never cite the issue comment as evidence.

## Considered options

- **Per-test mutation attribution** (the PIT `fullMutationMatrix` precedent).
  Not adopted — `cargo-mutants` 27.1.0 exposes no equivalent, and although its
  per-mutant logs name the failing tests, attribution from a run's logs is manual
  and impractical at hundreds of mutants.
- **Per-test coverage overlap** (`cargo-llvm-cov` + `cargo-nextest`). Kept as a
  future *nomination* signal only: two new toolchains, and the coverage-subset
  relation is neither necessary nor sufficient for behavioural redundancy.
- **Deletion experiment alone.** Rejected as the whole method — correct and
  strong, but a scan costs hours and produces no candidate list; the cheap tier
  exists to aim it.
- **Intent audit alone, no mutation.** Rejected — fastest, and it catches the
  exact twins and the impl-detail residue, but shape similarity is the very
  signal error mode 1 shows to be unsound; a deletion must not rest on it.
- **Reusing #303's kill set as the baseline.** Rejected — the #617 split
  invalidated it (scopes tripled, lines moved).

## Consequences

- The method is repeatable from this document alone and stays local: no CI job,
  no new toolchain, no production change.
- The ADR carries the method; the issue carries the evidence. The pilot's
  Tier-1 nomination, its raw counts and the Tier-2 before/after diff are
  recorded on #311.
- The pilot's worked example is the Turn's `card_footer_*` unit tests against the
  `prompt_render.rs` turn-footer integration tests — the two-granularity case.
- Follow-ups this opens, deliberately out of scope here: whether to install
  `cargo-llvm-cov` + `cargo-nextest` for a mechanical nomination pass, and the
  un-audited modules #303 named (`command.rs`, `request/*`, `handler.rs`).

Related: #298, #303, #311, #618 (C11), #617, ADR-0031.
