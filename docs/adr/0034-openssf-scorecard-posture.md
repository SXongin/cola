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
   CodeRabbit (free for public repositories) reviews every PR; with
   `request_changes_workflow: true` it approves once its unresolved comments
   are resolved and the latest commit is reviewed. Scorecard counts the
   approval, closing the last solvable gap. This is a deliberate trade: for a
   solo project an AI second pass is better than no gate at all, and GitHub
   itself now sanctions bot approvals. The admin bypass still exists, so the
   bot can never lock the maintainer out.

**Explicit no-s.** We are not pursuing the CII Best Practices badge (hours of
self-attestation for at most +0.2), multi-organization `Contributors` (not
something a project can honestly fix), human `Code-Review` before a second
maintainer exists, or paid review tools. We also do not chase 9/10 with
settings that misrepresent how the project is maintained. Expected trajectory:
6 → ~8 once the changes and the two auto-healing checks land (9 if CodeRabbit
approvals and the fuzz target are counted); 10 is unreachable for a
single-person, single-organization project without gaming the checks.

## Consequences

- SHA pins move the update signal into Dependabot; the `github-actions` group
  now receives SHA bumps with their version comments. `dtolnay/rust-toolchain`
  and `taiki-e/install-action@v2` follow their README-documented pinning forms;
  the rust-toolchain pin needs a manual refresh when its wrapper changes.
- The provenance attachment depends on the attestation store read permission
  (`attestations: read`) and `gh attestation download` naming bundles by digest;
  the release workflow renames them per artifact.
- CodeRabbit is a new third-party app with repository read access and an
  approval voice; removing it later reverts this ADR's review decision but not
  the other five.
- The Scorecard number will be lower than its theoretical maximum by design.
  Do not "fix" the remaining zeros without revisiting this ADR.
