# OpenSSF Scorecard: fix what is cheap and real, record the rest as deliberate no-s

The repository runs OpenSSF Scorecard (weekly workflow, published results, README
badge) and scored 6/10. The aggregate is a **risk-weighted** average
(Critical 10, High 7.5, Medium 5, Low 2.5), not a plain mean, so the zeros are
not equally valuable: of the seven zero-scoring checks, `Maintained` and `SAST`
heal on their own (the repo is under 90 days old; CodeQL was added after the
sampled commits), `Code-Review` needs a second human and `Contributors` needs a
second organization, and only `Pinned-Dependencies`, `Signed-Releases` and
`Fuzzing` are both actionable and genuinely security-relevant.

We decided the target is **real supply-chain risk, with the score as a
byproduct**. Concretely:

1. **Pin every GitHub Action to a full commit SHA** (with a `# vX.Y.Z` comment
   so Dependabot keeps bumping both). Action tags are mutable and a re-pointed
   tag runs attacker code with the repo's tokens; this is the one check that is
   pure mechanical work with real protection. Two exceptions follow each
   action's own guidance:
   - `dtolnay/rust-toolchain` is pinned to a commit on its `master` branch with
     an explicit `toolchain: stable` input (the action's README says SHA pins
     must live in `master` history, and that supplying `toolchain` explicitly
     means pinning `master`);
   - `taiki-e/install-action` stays on a `v2` SHA (never the tool-name ref
     hashed) with `tool: cargo-llvm-cov` (its README calls hashing tool-name
     refs an impostor-commit hazard).
2. **Publish build provenance as release assets.** The release already creates
   `actions/attest-build-provenance` attestations, but Scorecard v5.5.0 only
   reads *release assets* named `*.intoto.jsonl`, not the GitHub attestation
   store. The publish job now downloads each artifact's bundle and attaches it
   as `<artifact>.intoto.jsonl`. This matters beyond the score: the release
   assets are cola's self-update channel (ADR-0015, ADR-0030), and users can
   verify a download offline. The check looks at the last five releases, so the
   score climbs one fifth per cut.
3. **Scope write tokens to the job that needs them.** `codeql.yml`'s
   `security-events: write` moves from the top level to the `analyze` job;
   `release.yml`'s job-level `contents: write` (required to publish a release)
   stays, and costs nothing because a job-level write under a declared
   top-level read is not penalized.
4. **Add `require_last_push_approval` to the `main: review` ruleset.** It is
   free, it is the only real setting missing before the score stops at 8, and
   it protects a future non-admin collaborator. We deliberately do **not** set
   2 required reviewers, code-owner review, or apply-to-admins: on a solo
   repository those settings would be fiction (every merge is an admin bypass,
   and apply-to-admins would make merging impossible). 8 is the honest ceiling
   for one maintainer.
5. **One fuzz target for the hand-written binary parser.** `src/feishu/pbbp2.rs`
   parses untrusted WS bytes by hand, so panics/DoS there are real; everything
   else is serde JSON, whose upstream is already fuzzed far beyond what we
   would add. `fuzz/fuzz_targets/pbbp2.rs` decodes arbitrary bytes and checks
   the `decode(encode(frame))` round-trip. It is not wired into CI: continuous
   fuzzing (ClusterFuzzLite, OSS-Fuzz) is recurring infrastructure this project
   does not need. The target immediately justified itself — see the overflow
   fix it forced in `read_len_delimited`.

   > **Amended 2026-09-22**: it now runs weekly in CI as a plain scheduled
   > job; see the amendment at the end of this ADR.
6. **AI review is the review gate until a human co-maintainer exists.**
   CodeRabbit (free for public repositories) reviews a PR on request; with
   `request_changes_workflow: true` it approves once its unresolved comments
   are resolved and the latest commit is reviewed. Public repositories under
   10 stars never receive automatic reviews (CodeRabbit's documented anti-spam
   threshold), so each PR is triggered with `@coderabbitai review` — one
   command, not a review by hand. Scorecard counts the approval, closing the
   last solvable gap. This is a deliberate trade: for a solo project an AI
   second pass is better than no gate at all, and GitHub itself now sanctions
   bot approvals. The admin bypass still exists, so the bot can never lock the
   maintainer out.

   > **Amended 2026-09-15**: the gate is now the OpenCode review workflow; see
   > the amendment at the end of this ADR.
   >
   > **Amended 2026-10-04**: the gate is now the Codex Action review; see the
   > amendment at the end of this ADR.

**Explicit no-s.** We are not pursuing the CII Best Practices badge (hours of
self-attestation for at most +0.2), multi-organization `Contributors` (not
something a project can honestly fix), human `Code-Review` before a second
maintainer exists, or paid review tools. We also do not chase 9/10 with
settings that misrepresent how the project is maintained. Expected trajectory:
6 → ~8 once the changes and the two auto-healing checks land (the published
result on 2026-09-14 was 7.5, with `Maintained` still 0 until the repository
turns 90 days old); 10 is unreachable for a single-person, single-organization
project without gaming the checks. The review-gate approval does count toward
`Code-Review` today, but only because that check's implementation does not
filter bot reviewers — see the 2026-09-15 amendment.

## Consequences

- SHA pins move the update signal into Dependabot; the `github-actions` group
  now receives SHA bumps with their version comments. `dtolnay/rust-toolchain`
  and `taiki-e/install-action@v2` follow their README-documented pinning forms;
  the rust-toolchain pin needs a manual refresh when its wrapper changes.
- The provenance attachment depends on the attestation store read permission
  (`attestations: read`) and `gh attestation download` naming bundles by digest;
  the release workflow renames them per artifact.
- CodeRabbit was removed on 2026-09-15 (see the amendments). Its repository
  read access and approval voice are replaced by the review gate — OpenCode
  until 2026-10-04, Codex Action since — which holds `pull-requests: write` and
  an API key shared with local development.
- The Scorecard number will be lower than its theoretical maximum by design.
  Do not "fix" the remaining zeros without revisiting this ADR.

## Amendment (2026-09-15): the OpenCode review workflow replaces CodeRabbit

The gate in item 6 is now `.github/workflows/opencode-review.yml` (PR #164,
issue #163):

- It reviews every non-draft in-repo PR on `opened`, `synchronize`, `reopened`
  and `ready_for_review` with the OpenCode Go subscription and, on a clean
  review, approves through `github-actions[bot]` — same approval, no manual
  trigger.
- The approval is deterministic: the model ends its PR comment with a
  `PASS`/`FAIL` verdict line, and a separate step approves only on `PASS`, only
  for the reviewed commit. A failed, missing or malformed review never approves.
- The CLI is installed from a version-pinned release tarball and verified
  against its SHA-256 digest (the composite action installs `releases/latest`,
  so pinning the action would not pin the executed code), and the model runs
  sandboxed: edits denied, bash limited to read-only `git`/`gh` verbs, and
  `gh pr review` denied so the approval stays with the workflow's own step.
- Fork PRs are skipped (they never see secrets). Dependabot PRs are skipped
  too: `opencode github run` asserts the triggering actor is a collaborator,
  and a bot actor fails that check before the model runs.

Why CodeRabbit went away:

- Its automatic reviews skip public repositories under 10 stars, so every PR
  needed a manual `@coderabbitai review`.
- Its free tier rate limits reviews (observed: `Next included review available
  in 15 minutes` on PR #162), and a rate-limited attempt still consumes the
  PR's slot.
- Its `request_changes_workflow` veto blocked merges after the new gate had
  approved (PR #164: `CHANGES_REQUESTED` at 01:34 and 01:45 against an approval
  at 02:10, unblocked only when it re-reviewed at 02:18 — ~44 minutes of
  waiting), and with `require_last_push_approval` every push makes both bots
  re-review.

Two Scorecard facts recorded while doing this:

- `Code-Review` counts a bot approval because the check's implementation
  (`probes/codeApproved`) filters only *bot-authored changesets*, never bot
  reviewers, despite its documentation saying bot reviews do not count. The
  published v5.5.0 result already shows this: 3/11 approved changesets with
  CodeRabbit as the only non-author approver in the repository's history. Treat
  that score as fragile: if the implementation ever matches the docs, the check
  returns to 0.
- Merged Dependabot changesets that never receive an approval count against
  `Code-Review` (approved bot changesets are skipped entirely), so skipping
  Dependabot PRs in the gate has a small score cost. Worth revisiting only if
  the score matters more than the automation.

## Amendment (2026-09-20): release PRs skip the review

The gate also skips `release/*` PRs: the diff is a generated version-bump
commit, the code it bumps was already reviewed on `main`, and the release cut
merges with the admin bypass — so the missing approval changes nothing. Every
other non-draft in-repo PR still gets the review the 2026-09-15 amendment
describes.

## Amendment (2026-09-22): the pbbp2 fuzzer runs weekly as a plain scheduled job

Item 5 is amended: the `pbbp2` target is now wired into CI (issue #264), as
`.github/workflows/fuzz.yml` — a weekly scheduled run plus `workflow_dispatch`
that fuzzes for 300 s on a nightly toolchain (cargo-fuzz requires nightly),
restores and saves `fuzz/corpus` through the Actions cache (a per-run key plus
a shared `restore-keys` prefix, since cache entries are immutable), and uploads
crash reproducers as an artifact. The target itself is unchanged.

ClusterFuzzLite and OSS-Fuzz stay rejected: this is one small hand-written
parser, and neither option buys it anything the plain job does not, at a much
higher infrastructure cost. The Scorecard argument runs the other way, and is
recorded here because this ADR originally listed `Fuzzing` as an open,
actionable zero: the check detects the target itself — it matches
`libfuzzer_sys` in any `*.rs` file (`RustCargoFuzzer`) — so it flipped to 10
when item 5's target landed (`a3003fa`, 2026-09-14) and reads the same whether
or not anything ever runs it (the published 2026-09-22 result at `c83eedf`
shows `Fuzzing` 10). Wiring the weekly job therefore changes no score; it
exists for the bug-finding value only. Do not adopt ClusterFuzzLite to "fix" a
check that the fuzz target already satisfies.

## Amendment (2026-09-30): the `SAST` check is a deliberate no

CodeQL was retired (ADR-0064), so the `SAST` check — expected above to "heal on
its own" once CodeQL's runs covered the sampled commits — returns to 0 (no SAST
tool detected). That is now deliberate: the tool produced no true positive in
~640 runs, and the credential class it kept flagging is covered by secret
scanning with push protection. Add `SAST` to the "Explicit no-s" list; do not
re-add a scanner for this check alone.

## Amendment (2026-10-04): the verdict readback ignores the Codex trial's first-line marker

The advisory Codex review trial (`.github/workflows/codex-review-trial.yml`,
issue #516) posts under `github-actions[bot]` — the same identity the gate uses
— while the gate approves by reading the *last* bot comment. Polling from the
trial side cannot serialize against a gate already past its review post and
before its verdict readback, so the gate now skips bot comments whose **first
line** is `<!-- codex-review-trial -->`.

The anchor is the first line deliberately. A substring match was tried first
and failed in the trial's own reviews: the gate's free-form review quotes the
marker when reviewing the trial workflow, so `contains` filtered out the
gate's own comments too — no verdict was found and the gate stopped approving
(and had a stale PASS comment matched instead, it would have approved a commit
whose review said FAIL). First-line anchoring cannot be tripped by a quoted
marker, because the trial always posts the marker as line one.

The verdict contract is unchanged: the model ends its comment with a
line-final `OPENCODE_REVIEW_VERDICT: PASS|FAIL`, and the step approves only on
`PASS`, only for the reviewed commit. The filter only narrows which comments
the readback may see. The trial is advisory and never approves; whether Codex
replaces the OpenCode gate remains an open decision (issue #516).

## Amendment (2026-10-04, superseding the marker filter): the Codex Action review is the gate

The gate in item 6 is now `.github/workflows/codex-review.yml` (PR #518),
replacing `.github/workflows/opencode-review.yml`: `openai/codex-action`
(v1.12) runs Codex read-only on `gpt-6-luna` through the OpenCode Go
subscription's Responses-compatible inference endpoint, with the two axes plus
an adversarial pass rubric at `.github/codex/prompts/review.md`. The approval contract is
carried over: a line-final `CODEX_REVIEW_VERDICT: PASS|FAIL` in the review
comment, approval only on `PASS`, pinned to the reviewed commit. This
supersedes the 2026-09-15 amendment's OpenCode specifics (model, prompt
injection, permission allowlist); the skips (fork, Dependabot, `release/*`)
carry over. The local `/code-review` skill stays the author-side OpenCode
two-axis pass — the CI gate is deliberately a different agent and model
family, so the two are independent review signals.

The advisory trial (issue #516, PR #517) is the evidence: nine review rounds
at ~1.5 minutes per run, including a FAIL that caught a real defect in the
gate's own comment filter. The trial-only plumbing is deleted with it:
`.github/workflows/codex-review-trial.yml` and the `codex-trial` label.

The approval readback no longer scans comments at all. The workflow posts the
review via `gh api` and records the new comment's ID as a step output; the
approve step fetches exactly that comment and approves only when its final
line is `PASS`. That removes the last-comment scan whose interference the
previous amendment's first-line trial-marker filter guarded against: with
comment-ID anchoring no other comment can shadow or forge the verdict, so the
marker rule dies with the trial workflow.

The trial header recorded `pull_request_target` as the hardening to evaluate
if Codex became the gate; this replacement keeps the `pull_request` + fork-skip
trust model instead. Endpoint, key and read-only posture are unchanged from the
trial: the Console inference API round-trips `gpt-6-luna` with the
`OPENCODE_API_KEY` Go key, which stays inside the action's Responses proxy and
never reaches the Codex step's environment. As before, a FAIL is a comment
without an approval, never a `request-changes`.
