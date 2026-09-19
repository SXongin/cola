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
- CodeRabbit was removed on 2026-09-15 (see the amendment). Its repository read
  access and approval voice are replaced by the OpenCode review workflow, which
  holds `pull-requests: write` and an API key shared with local development.
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
