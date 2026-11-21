# OpenSSF Scorecard: fix what is cheap and real, record the rest as deliberate no-s

> **The review gate's policy is `CONTRIBUTING.md` "Reviewing"** — the blocking
> definition, the finding cap, the round policy and the dossier; the gate's
> provider, endpoint, model and secret are repository settings, and its action
> pin a workflow commit, both recorded in `.github/workflows/codex-review.yml`.
> This ADR owns the Scorecard posture only (#1129).

The repository runs OpenSSF Scorecard (weekly workflow, published results, README
badge) and scored 6/10. The aggregate is a **risk-weighted** average
(Critical 10, High 7.5, Medium 5, Low 2.5), not a plain mean, so the zeros are
not equally valuable: of the seven zero-scoring checks, `Maintained` heals on
its own (the repo is under 90 days old), `Code-Review` needs a second human,
`Contributors` needs a second organization, `SAST` is a deliberate no (below),
and only `Pinned-Dependencies`, `Signed-Releases` and `Fuzzing` are both
actionable and genuinely security-relevant.

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

   The gate's action pin is asserted by `cargo test -p xtask codex_gate`, so a
   Dependabot group bump moves that expectation deliberately instead of
   silently reverting it.
2. **Publish build provenance as release assets.** The release already creates
   `actions/attest-build-provenance` attestations, but Scorecard v5.5.0 only
   reads *release assets* named `*.intoto.jsonl`, not the GitHub attestation
   store. The publish job downloads each artifact's bundle and attaches it as
   `<artifact>.intoto.jsonl`. This matters beyond the score: the release assets
   are cola's self-update channel (ADR-0015, ADR-0030), and users can verify a
   download offline. The check looks at the last five releases, so the score
   climbs one fifth per cut.
3. **Scope write tokens to the job that needs them.** A workflow declares
   `contents: read` at the top, and only the job that needs more raises it: the
   release's build job takes `id-token: write` and `attestations: write` to sign
   provenance, and its publish job takes `contents: write` (required to publish
   a release) and `attestations: read`. A job-level write under a declared
   top-level read is not penalized, so it costs nothing.
4. **Add `require_last_push_approval` to the `main: review` ruleset.** It is
   free, it is the only real setting missing before the score stops at 8, and it
   protects a future non-admin collaborator. We deliberately do **not** set
   2 required reviewers, code-owner review, or apply-to-admins: on a solo
   repository those settings would be fiction (every merge is an admin bypass,
   and apply-to-admins would make merging impossible). 8 is the honest ceiling
   for one maintainer.
5. **One fuzz target for the hand-written binary parser.** `src/feishu/pbbp2.rs`
   parses untrusted WS bytes by hand, so panics/DoS there are real; everything
   else is serde JSON, whose upstream is already fuzzed far beyond what we
   would add. `fuzz/fuzz_targets/pbbp2.rs` decodes arbitrary bytes and checks
   the `decode(encode(frame))` round-trip. The target immediately justified
   itself — see the overflow fix it forced in `read_len_delimited`.

   It runs weekly in CI (issue #264), as `.github/workflows/fuzz.yml` — a
   scheduled run plus `workflow_dispatch` that fuzzes for 300 s on a nightly
   toolchain (cargo-fuzz requires nightly), restores and saves the corpus
   through the Actions cache (a per-run key plus a shared `restore-keys` prefix,
   since cache entries are immutable), and uploads crash reproducers as an
   artifact. The target itself is unchanged.

   ClusterFuzzLite and OSS-Fuzz stay rejected: continuous fuzzing is recurring
   infrastructure this one small parser does not need, and neither option buys
   it anything the plain job does not, at a much higher cost. The Scorecard
   argument runs the other way: the `Fuzzing` check detects the target itself
   (it matches `libfuzzer_sys` in any `*.rs` file, `RustCargoFuzzer`), so it
   flipped to 10 when the target landed (`a3003fa`, 2026-09-14) and reads the
   same whether or not anything ever runs it (the published 2026-09-22 result
   at `c83eedf` shows `Fuzzing` 10). Wiring the weekly job therefore changes no
   score; it exists for the bug-finding value only. Do not adopt ClusterFuzzLite
   to "fix" a check that the fuzz target already satisfies.
6. **AI review is the review gate until a human co-maintainer exists.** For a
   solo project an AI second pass is better than no gate at all, and GitHub
   itself now sanctions bot approvals. The gate is the Codex Action; its policy
   is `CONTRIBUTING.md` "Reviewing". This ADR owns the Scorecard consequence
   only: that approval is the one non-author approval, so it closes the last
   solvable `Code-Review` gap (fragile — see Consequences).

**Explicit no-s.** We are not pursuing the CII Best Practices badge (hours of
self-attestation for at most +0.2), multi-organization `Contributors` (not
something a project can honestly fix), human `Code-Review` before a second
maintainer exists, or paid review tools. We also do not chase 9/10 with
settings that misrepresent how the project is maintained. Expected trajectory:
6 → ~8 once the changes land and `Maintained` heals (the published result on
2026-09-14 was 7.5, with `Maintained` still 0 until the repository turns 90 days
old); `SAST` stays 0 by design, and 10 is unreachable for a single-person,
single-organization project without gaming the checks. The review-gate approval does count toward
`Code-Review` today, but only because that check's implementation does not
filter bot reviewers — see Consequences.

`SAST` is a deliberate no. CodeQL was retired (ADR-0064), so the check returns
to 0 (no SAST tool detected); do not re-add a scanner for this check alone.

## Consequences

- SHA pins move the update signal into Dependabot; the `github-actions` group
  now receives SHA bumps with their version comments. `dtolnay/rust-toolchain`
  and `taiki-e/install-action@v2` follow their README-documented pinning forms;
  the rust-toolchain pin needs a manual refresh when its wrapper changes.
- The provenance attachment depends on the attestation store read permission
  (`attestations: read`) and `gh attestation download` naming bundles by digest;
  the release workflow renames them per artifact.
- The Scorecard number will be lower than its theoretical maximum by design.
  Do not "fix" the remaining zeros without revisiting this ADR.
- `Code-Review` counts a bot approval because the check's implementation
  (`probes/codeApproved`) filters only *bot-authored changesets*, never bot
  reviewers, despite its documentation saying bot reviews do not count. The
  published v5.5.0 result already shows this: 3/11 approved changesets with the
  review gate as the only non-author approver in the repository's history. Treat
  that score as fragile: if the implementation ever matches the docs, the check
  returns to 0.
- Merged Dependabot changesets that never receive an approval count against
  `Code-Review` (approved bot changesets are skipped entirely), so skipping
  Dependabot PRs in the gate has a small score cost. Worth revisiting only if
  the score matters more than the automation.
