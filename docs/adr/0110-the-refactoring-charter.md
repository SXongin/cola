# Growth is budgeted, tests are behaviours: the refactoring charter

The four contracts a refactor here is settled by — code (#1109), tests (#1111),
docs (#1112) and flow (#1110) — assembled into one decision, with the segment
that applies them first (#1115). Evidence lives in #1105–#1108; this ADR cites
it rather than re-deriving it.

## Context

`src` doubled in six weeks, 163.6k → 362k lines, while `#[test]` functions stayed
flat (~2,011 → 1,990), so coverage per line roughly halved; files over 2,000
lines went 27 → 64, and C1/C2's extraction re-bloated `render.rs` 6.1k → 14.2k
and `handler.rs` 2.5k → 7.6k (#1108). The gate is not the strictness problem: 206
of 257 blocking findings were real defects, 0 defensive, 0 pre-existing, and
rounds track shared mutable ordering/lifecycle state rather than diff size — a
+9,329-line PR passed in one round, a +157-line PR took forty-seven (#1105). CI
is execution- and hotspot-bound: 208 tests (4.7%) own 77% of the 693 s serial
wall time, and three `feishu::card::compaction` tests are 45% of the top-50
(#1106). The docs are about half load-bearing: ~20% of the 109 ADRs' text folds
away with no live decision lost, and the largest modules' comments are ~38% pure
restatement (#1107).

## Decision

### 1. Growth is ratcheted, not capped

A production function over cognitive complexity 25 or 100 lines, a production
file over 2,000 lines, a file that grows a baselined size by more than 10%, a new
`#[allow(clippy::too_many_arguments)]` without a reason and a tracking issue, a
new `#[cfg(test)]` production site, and a `["tag"]` card probe outside
`src/feishu/card` all fail `check-structure` unless they are already in
`xtask/structure-baseline.json`. It ratchets down only: `--bless` lowers and
refuses an upward value, and an entry is identified by its function's body, so a
rename or a move is not new growth and a same-score replacement still is.
`clippy.toml` pins the two thresholds so a toolchain bump cannot move the bar.
Touching a function that can fit is the moment to cut it; an existing offender
is not a separate ticket. Card JSON belongs to `feishu/card`, the `CardElement`
type layer is the segment that will own the schema (#1138), and run lifecycle
has one owner, not four homes (#1140, #1141).

### 2. One observable behaviour per test

A test pins one behaviour; many assertions may assert that one behaviour.
Ordering/race, golden, and wire-shape tests (a fake server) are the enumerated
exceptions — their one behaviour is the order or the shape. An absence
assertion must name its counterpart, or a rename turns it vacuous. Deletion
goes cheapest-first: repeated negatives, then test-only twins, then duplicate
helpers, then same-shape merges, and a deletion needs a surviving test that
fails when the behaviour breaks. A new test-only production twin is banned; the
existing twins are C9's deletion target (#1118). CI targets
hotspots, not the count — the Test job's budget is 6 minutes — and nightly
sampled mutation replaces coverage as the value signal. No `nextest`, no Linux
Test job, no coverage ratio.

### 3. Docs have one home, and an ADR is read by its banners

A banner is the default way a later decision reaches a standing ADR; a fold is
the one exception to append-only and needs its own docs-only change, and it
lands only when the surviving body is a minority of the file or a whole section
has died. An ADR carrying 3+ banners is a mandatory read before editing, not an
automatic fold. A superseded or retired ADR folds, whole, to a 5-line pointer —
title, status, the replacing banner, one line of why. A new ADR is soft-capped
at 150 lines: longer is two decisions. A comment carries an invariant, a
wire/why trap, or an API contract the signature cannot express; a bare pointer
is fine, a retelling is not. History lives in the issue, not in the document. A
live fact gets one home, and the docs batches below (#1126–#1131) move the
existing restatements there.

### 4. The gate is not the cost; the round cycle is

The branch loop, the local pair, deferral and commit granularity stay: wall time
tracks rounds, and a 36-commit PR has passed in one (#1110). The gate keeps its
3-round budget and its 3-full-finding cap until #1114 measures them. A
`ready-for-agent` ticket's every acceptance criterion names what decides it — a
predicate, a symbol, or a shape in words — enforced at triage, not by a
template. The review prompt is unchanged except to skip what tooling now
decides.

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
| Conventional Commits subject | `check-commit-msg` |
| dependencies and advisories | `cargo deny` |
| doc links, ADR banner count | `check-doc-links` (#1131) |

Review-only, because a predicate cannot decide them: the one-behaviour test
rule and its exceptions (the `#[cfg(test)]` counter stops the twins refilling,
not their shape); the comment rule; the 150-line ADR cap; the fold conditions;
the over-defense rule against guards for impossible inputs (a finding, never a
block); the gate budget; the no-diff-cap rule; measure-don't-assert; a feature
cut waiting on user evidence; boy-scout lowering; the triage criterion rule.

A review-only rule becomes a guard only when a measurement ticket shows it is
being missed — then the guard lands with its baseline, never a blanket
threshold.

## Unchanged, with evidence

- **No diff-size cap, no split rule** — #1015 (+9,329) passed in one round; #1047
  (+157) took forty-seven (#1105, #1110).
- **One Test job, no Linux leg** — the cost is execution-bound, and a job split
  pays the same tests twice (#1106, #1111).
- **No PR template, no out-of-scope section** — the criterion rule lives at
  triage (#1110).
- **The 3-round gate budget** — the cost is the round, and the budget is what
  makes the local pair spend the second one (#1110).

## The measure

Three numbers decide whether this worked: median blocking gate rounds for a
≤300-line change ≤1; CI wall time ≤6 minutes; no new module over 2,000 lines and
the `fix` commit share falling. #1114 turns them into one read-only command and
records the baseline before the first segment; a rule does not change on an
assertion.

## The backlog

Tracked in the issues; the order is the leverage order of #1108 and #1115.

1. **Measure first** — #1114's baseline, so the effort is comparable.
2. **Segment 1: C9 then C6** — delete the named test-only twins and gates
   (#1118), then the CardAction dispatch (#1119) and its five verb groups
   (#1120–#1124). `feat` work is frozen from #1118's branch until the segment's
   four observable criteria hold — no per-verb body in the dispatch,
   characterization-pinned payloads and `CardActionResult`, no `Option<&Gate>`
   in production, lowered `check-structure` entries — with no calendar cap and
   verb-at-a-time rollback, the dispatch staying alive (#1115). The segment
   tickets are the authority on those bounds; this ADR records only that the
   window is allowed, not how long it lasts.
3. **Then the missing seams, in order** — #1140 (design the run-lifecycle seam)
   → #1141 (land it) → #1135 (tool-view) → #1136 (`CardsHandle` accessor) →
   #1137 (test harness) → #1138 (`CardElement`) → #1139 (delivery observer).
4. **Docs, in parallel, after this lands** — archive (#1126), merge chains
   (#1127), fold in place (#1128), split 0034/0092 (#1129), single-home (#1130),
   then the doc-link and banner guard (#1131). #1130 re-homes facts this ADR
   writes, so it goes last.
5. **Held** — C8 (`ServerIdentity`) stays HOLD, and feature freezing/deletion
   waits on the ROI table (#1113). #1132 rides the C9/C6 split of the
   `async_trait` impls.
