Review this pull request on the two axes from CONTRIBUTING.md's "Reviewing"
section, then add one adversarial pass.

The repository is checked out at the PR head; the base commit is in history.
Read `CONTRIBUTING.md` ("Reviewing"), `CODING_STANDARDS.md`, and any ADR under
`docs/adr/` the diff touches before writing findings. The PR metadata and every
issue the PR references — the body's closing/ref keywords and the commits'
`Refs:` trailers, deduplicated, body references first — are appended below.

## The two axes

- **Spec**: does the diff implement what the originating issue or spec asked
  for? Report requirements that are missing, partial, or implemented wrongly,
  and any scope creep. When several issues are linked, the body's first
  reference is the spec by convention; the rest are its tickets. Quote the
  issue line for each finding.
- **Standards**: does the diff follow `CODING_STANDARDS.md` and the repo's
  conventions? Report the places it violates a documented standard (cite the
  standard) and the places it contradicts an existing ADR.

## The adversarial pass (the part an author-side review tends to miss)

- **Correctness**: for each changed behaviour, name the concrete input, state,
  or timing that breaks it — or say you tried and failed to break it.
- **Test adequacy**: which changed lines are not exercised by the tests in the
  diff? Which test would still pass if the change were reverted?
- **Robustness**: error paths, cleanup, cancellation, ordering, and resource
  lifetime in the changed code.

## What blocks the merge

A finding is **blocking** when it names a concrete input, state, or timing the
diff newly fails; an acceptance criterion of a linked issue this PR closes
(`Closes`, `Fixes` or `Resolves`, not `Refs`) that is unmet; a regression this PR
introduces; or a silent contradiction of an accepted ADR (an ADR the PR adds or
explicitly amends is not a contradiction). Nothing else blocks.

If you cannot establish whether the diff introduced a candidate blocking
finding, it blocks: say what you could not determine in the finding rather than
moving it to the backlog.

Everything else is non-blocking and is listed only under a
`Backlog (non-blocking)` heading:

- a pre-existing defect that the diff neither creates nor makes newly reachable;
- out-of-scope suggestions, naming, and style;
- speculative edge cases, and "could add a guard" for an input that cannot
  occur;
- tests that would only cover an impossible input, and a missing test unless the
  untested behaviour is itself a concrete failure the diff introduces.

If a change adds a guard, branch, or test for an input that cannot occur, that
over-engineering is itself a non-blocking finding: report it, never ask for more
guards.

Report at most 3 blocking and 5 non-blocking findings in full per review, most
consequential first. When you hold further blocking findings back, list them as
one-line stubs — `file:line` and one clause each — so none is lost; the stubs do
not count against the cap. A security or data-loss finding the diff introduces
or makes newly reachable is exempt from both limits and is marked `critical`; it
blocks. A pre-existing security finding is non-blocking like any other
pre-existing defect.

## Rules

- Read-only: editing tools are disabled and any write fails. Use `git diff`,
  `git show`, `git log`, `git grep`, and plain file reads. Do not post reviews,
  comments, or labels yourself.
- Every finding: file:line, the failing scenario, severity (blocking /
  non-blocking / critical). A finding must be tied to a changed line; a newly
  reachable defect ties to the changed line that creates the state, not to the
  failing one. If it has no such line, leave it out. Skip anything tooling
  enforces (fmt, clippy).
- Blocking findings first, then `Backlog (non-blocking)`; no summary of what the
  diff does. Keep the review under ~800 words.
- A missing or failed part of the review is a blocking finding. The review's
  parts are reading the diff and the PR metadata and checking each claimed
  acceptance criterion of the issues whose bodies are appended below — an issue
  named only by number is not one you can check; if a part cannot be completed,
  the review is incomplete. An absent tool that the review does not require is
  not a missing part: verify against the diff's tests, fixtures and the quoted
  issue text instead, and if that leaves a candidate blocking finding
  unattributed, the rule above makes it block.
- The host-only reference trees named in `docs/agents/references.md` (e.g.
  `/root/workspace/dev/opencode`) are absent in CI. Their absence is not a
  missing part of this review: verify wire-contract claims against the diff's
  tests and recorded fixtures, and any upstream shapes quoted in the PR body or
  the linked issues.

Finish with exactly one final line, on its own line and with nothing after it —
no punctuation, no code fence, no bold or code-span formatting:

CODEX_REVIEW_VERDICT: PASS
  when the review is complete and nothing blocks merging;
CODEX_REVIEW_VERDICT: FAIL
  when anything blocks merging or the review is incomplete as defined above.
