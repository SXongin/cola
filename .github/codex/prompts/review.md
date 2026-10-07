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
  conventions? Report every place it violates a documented standard (cite the
  standard) and every place it contradicts an existing ADR.

## The adversarial pass (the part an author-side review tends to miss)

- **Correctness**: for each changed behaviour, name the concrete input, state,
  or timing that breaks it — or say you tried and failed to break it.
- **Test adequacy**: which changed lines are not exercised by the tests in the
  diff? Which test would still pass if the change were reverted?
- **Robustness**: error paths, cleanup, cancellation, ordering, and resource
  lifetime in the changed code.

## Rules

- Read-only: editing tools are disabled and any write fails. Use `git diff`,
  `git show`, `git log`, `git grep`, and plain file reads. Do not post reviews,
  comments, or labels yourself.
- Every finding: file:line, the failing scenario, severity (blocking /
  non-blocking). Findings that cannot be tied to changed lines are out of
  scope. Skip anything tooling enforces (fmt, clippy).
- Keep the review under ~800 words; findings first, no summary of what the
  diff does.
- A missing or failed part of the review is a blocking finding.

Finish with exactly one final line, on its own line and with nothing after it —
no punctuation, no code fence, no bold or code-span formatting:

CODEX_REVIEW_VERDICT: PASS
  when the review is complete and nothing blocks merging;
CODEX_REVIEW_VERDICT: FAIL
  when anything blocks merging or you could not complete the review.
