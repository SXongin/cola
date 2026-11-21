# Growth is budgeted, tests are behaviours: the refactoring charter

> **Amended (2026-11-21)**: the gate's round policy is autonomous rounds plus
> stop predicates, recorded in `CONTRIBUTING.md` "Reviewing". §4 and
> "Unchanged, with evidence" point there instead of restating the three-round
> budget.

> **Amended (2026-11-22, #1131)**: `check-doc-links` now fails a dangling
> citation and warns on an ADR's 3+ `… by ADR-NNNN` banners, so the ratchet
> table below gains its row; the paragraph that said the guard "will fail"
> is read through this banner.

The four contracts a refactor here is settled by — code (#1109), tests (#1111),
docs (#1112), flow (#1110) — and the segment that applies them first (#1115),
one decision, on evidence cited from #1105–#1108.

## Context

`src` doubled in six weeks, 163.6k → 362k lines, while `#[test]` functions
stayed flat (~2,011 → 1,990) — coverage per line roughly halved — and files over
2,000 lines went 27 → 64; C1/C2's extraction re-bloated `render.rs` 6.1k → 14.2k
and `handler.rs` 2.5k → 7.6k (#1108). The gate is not the strictness problem:
206 of 210 findings across 257 gate reviews were real defects (98%), 3 spec gaps
and 1 style, 0 defensive, 0 pre-existing; rounds track shared mutable
ordering/lifecycle state, not diff size — +9,329 lines passed in one round, +157
took forty-seven (#1105). CI is execution- and hotspot-bound: 208 tests (4.7%)
own 77% of the 693 s serial wall time, and three `feishu::card::compaction` tests
are 45% of the top-50 (#1106). The docs are about half load-bearing: ~20% of the
109 ADRs folds away with no live decision lost, and the largest modules'
comments are ~38% pure restatement (#1107).

## Decision

### 1. Growth is ratcheted, not capped

A production function over cognitive complexity 25 or 100 lines, a production
file over 2,000 lines (or 10% over its baselined size), a new
`#[allow(clippy::too_many_arguments)]` without a reason and a tracking issue, a
new `#[cfg(test)]` production site, and a `["tag"]` card probe outside
`src/feishu/card` all fail `check-structure` unless already in
`xtask/structure-baseline.json`. It ratchets down only: `--bless` lowers and
refuses an upward value, and an entry is identified by its function's body, so a
rename or a move is not new growth and a same-score replacement still is.
`clippy.toml` pins the two thresholds so a toolchain bump cannot move the bar.
Card JSON belongs to `feishu/card`, the `CardElement` type layer is the segment
that will own the schema (#1138), and run lifecycle has one owner, not four
homes (#1140, #1141).

### 2. One observable behaviour per test

A test pins one behaviour; many assertions may assert that one behaviour, and
ordering/race, golden and wire-shape tests (a fake server) are the enumerated
exceptions — their one behaviour is the order or the shape. An absence
assertion must name its counterpart, or a rename turns it vacuous. Deletion
goes cheapest-first: repeated negatives, then test-only twins, then duplicate
helpers, then same-shape merges, and a deletion needs a surviving test that
fails when the behaviour breaks. A new test-only production twin is banned —
`Option<&Gate>`, `*_for_test`, and `#[cfg(test)]` production branches alike; the
existing twins are C9's deletion target (#1118). CI targets hotspots, not the
count: the Test job's target is 6 minutes (its current timeout is 30). Nightly
sampled mutation is where the value signal goes, and #1114's baseline is what
measures the change; until that lands the Coverage job keeps its advisory
statuses, and no coverage ratio becomes a gate. No `nextest`, no Linux Test job.

### 3. Docs have one home, and an ADR is read by its banners

A banner is the default way a later decision reaches a standing ADR; a fold is
the one exception to append-only, needs its own docs-only change, and lands only
when the surviving body is mostly dead — a minority of the file, or a whole
section has died. An ADR carrying 3+ banners is a mandatory read at review,
not an automatic fold. A superseded or retired ADR folds, whole, to a
5-line pointer — title, `Status:`, the one-line decision, `Superseded/Retired by
ADR-NNNN`, and the originating issue — within one release of the decision; git
is the record. A new ADR is soft-capped at 150 lines: longer is two decisions. A
comment carries an invariant, a wire/why trap, or an API contract the signature
cannot express; a bare pointer is fine, a retelling is not. History lives in the
issue, not in the document. A live fact gets one home, and the docs batches
below (#1126–#1131) move the existing restatements there.

### 4. The gate is not the cost; the round cycle is

The branch loop, the local pair, deferral and commit granularity stay: wall time
tracks rounds, and a 36-commit PR has passed in one (#1110). The gate keeps its
cap of 3 blocking findings in full (plus 5 non-blocking); its round policy is
the stop-predicate rule in `CONTRIBUTING.md` "Reviewing" (#1145), which replaced
the three-round budget this charter inherited. A
`ready-for-agent` ticket's every acceptance criterion names what decides it — a
code symbol, a wire field, a recorded fixture, or a command the reviewer can
run — enforced at triage, not by a template. The review prompt is unchanged
except to skip what tooling now decides.

## The ratchet

What a guard decides, and what only a reviewer can:

| Rule | Enforced by |
|---|---|
| formatting, lints (`-D warnings`) | `cargo fmt --all -- --check`, `cargo clippy` |
| the growth ratchet's five counters (decision 1) | `check-structure` |
| V1 route/field literals outside the V1 strategy | `check-generation` |
| `crate::opencode` outside the adapter | `check-client-seam` |
| fixture formatting | `check-fixture-format` |
| user-guide translation shape | `check-doc-drift` |
| dangling doc citations; an ADR's 3+ `by ADR-NNNN` banners (warning) | `check-doc-links` |
| Conventional Commits subject | `check-commit-msg` |
| dependencies and advisories | `cargo deny` |

`check-doc-links` (#1131) will fail a dangling reference and warn on an ADR's
banner count; the banner rule stays reviewable until something enforces it.

Review-only, because a predicate cannot decide them: the one-behaviour test
rule and its exceptions; a new `Option<&Gate>` or `*_for_test` production
branch (the counter sees only `#[cfg(test)]`); the comment rule; the 150-line
ADR cap; the fold conditions; over-defense against guards for impossible inputs
(a finding, never a block); the gate's round policy; the no-diff-cap rule;
measure-don't-assert; a feature cut waiting on user evidence; boy-scout
lowering; the triage criterion rule.

A review-only rule becomes a guard only when a measurement ticket shows it being
missed — then it lands with its baseline, never a blanket threshold.

## Unchanged, with evidence

**No diff-size cap or split rule** (#1015's +9,329 lines passed in one round;
#1047's +157 took forty-seven — #1105), **one Test job with no Linux leg**
(#1106), **no PR template or out-of-scope section**, and **a bounded round
policy** (now the stop-predicate rule, `CONTRIBUTING.md` "Reviewing") stay: the
cost is the round, and the bound is what makes the local pair spend the second
one.

## The measure

Three numbers decide whether this worked: median blocking gate rounds for a
≤300-line change ≤1; CI wall time ≤6 minutes; no new module over 2,000 lines and
the `fix` commit share falling. #1114 turns them into one command and records the
baseline before the first segment; no rule changes on an assertion.

## The backlog

1. **The first construction segment heads this backlog (#1115)** — its baseline
   is #1114's report (the three numbers, plus the test-cost tickets it names:
   the hotspot tests and the repeated negatives no other ticket owns), measured
   before the first branch opens. Then C9 deletes the named test-only twins and
   gates (#1118), and C6 puts the card-action dispatch behind one seam (#1119)
   and moves its five verb groups (#1120–#1124). `feat` work is frozen from
   #1118's branch until the segment's four observable criteria hold — no
   per-verb body in the dispatch, characterization-pinned payloads and
   `CardActionResult`, no `Option<&Gate>` in production, lowered
   `check-structure` entries — with no calendar cap and verb-at-a-time rollback,
   the dispatch staying alive; the tickets own those bounds.
2. **Then the missing seams** — C12, one run-lifecycle owner, starts with the
   design (#1140) and lands as #1141; then C5 (#1135), C3 (#1136), C10 (#1137),
   C4 (#1138) and C7 (#1139).
3. **Docs, in parallel, after this lands** — archive (#1126), merge chains
   (#1127), fold in place (#1128), split 0034/0092 (#1129), single-home (#1130),
   then the doc-link and banner guard (#1131). #1130 re-homes facts this ADR
   writes, so it goes last.
4. **Held** — C8 (`ServerIdentity`) stays HOLD, feature freezing/deletion waits
   on the ROI table (#1113), and the map's open "Turn state machine's target
   shape" stays fog: no ticket here claims it, and it needs its own decision
   when work next touches `bridge/turn`. #1132 rides the C9/C6 split of the
   `async_trait` impls.
