# The release cut is one command: manifest-first, PR through CI, tag on the merged commit

Cutting a release used to be three manual steps — bump `Cargo.toml` in a PR,
merge, then tag the merge and push the tag — usually executed by an agent
following prose in `CONTRIBUTING.md`. It drifted: the `0.7.0` tag points at a
commit that is not on `main` (the branch's bump commit, whose rebase merge left
a content-identical copy on `main`), so the tag and `main` disagree about which
commit `0.7.0` is. The alternative habit — push a tag and let CI derive every
version-bearing value from it — is common where no registry reads a manifest
(GoReleaser, semantic-release, setuptools-scm), but cola publishes to
crates.io as `colark` and embeds `CARGO_PKG_VERSION` for self-update, and those
make the manifest the source of truth (ADR-0015, ADR-0027, ADR-0030).

The repository's rulesets make the PR structural, not ceremonial:

- `main: CI (everyone)` requires the Format / Check / Test (macOS/Windows) /
  Dependency audit checks and has **no bypass actors** — a direct push of a
  freshly created commit is rejected for any actor, admin included.
- `main: review (admin bypass)` requires a PR and one approval, with the admin
  role bypassing both.
- `tags: create (admin bypass)` restricts tag creation to the admin role.

We decided: the Release Version stays in `Cargo.toml`, and one command runs the
cut from the manifest bump to the pushed tag, keeping every existing gate.

## Decision

1. **Manifest-first stays.** Version-bearing values may come from the tag only
   where the tag already is the truth (asset names, release notes). The
   compiled-in version comes from `Cargo.toml`; CI-side injection is rejected
   because it would dirty the tree that ADR-0027 requires to be clean, reopen
   the CI marker ADR-0027 explicitly rejected, make `cargo publish` need
   `--allow-dirty` with `.cargo_vcs_info.json` recording `dirty: true`, and
   make `cargo install --git <repo> --tag <tag>` build a binary reporting a
   different version than the tag.
2. **`cargo xtask release <version>` is the only release cut.** It:
   - classifies the current state (fresh cut / release branch before merge /
     merged cut with the tag missing) and is safe to re-run;
   - prints the release smoke test and waits for confirmation (`--yes` skips
     it; agents pass it only after a human confirmed in chat);
   - bumps `Cargo.toml` + `Cargo.lock`, commits
     `chore(release): bump version to <version>` on `release-<version>` and
     pushes the branch;
   - opens the PR, watches the required checks, and rebase-merges with the
     admin bypass once they are green;
   - pulls `main`, verifies `Cargo.toml` on the merged commit equals the
     version, and only then creates and pushes the lightweight tag.
3. **Tag after merge, always.** The tag must name the commit `main` actually
   contains; a tag created before a rebase merge can be left behind by the
   rewrite — the `0.7.0` failure. The command never tags a branch commit,
   never force-pushes, and never moves an existing tag.
4. **`release.yml` is unchanged.** It still builds the three platforms,
   attaches the archives + `SHA256SUMS`, publishes the GitHub release first and
   crates.io after (ADR-0030), and keeps its tag-equals-manifest guard as the
   backstop for hand cuts.

## Considered options

- **Tag-first: CI writes the version from the tag.** The habit from
  manifest-less ecosystems. Rejected for the four reasons in decision 1; the
  only saving is one mechanical commit.
- **`release-plz` (bot release PR).** Keeps a PR and adds changelog/version
  inference. Rejected for now: tag creation is reserved to the admin role, so
  the bot needs a PAT, and its publish step duplicates `release.yml`'s
  GitHub-first ordering. Additive later if changelog automation is wanted.
- **Direct push with a loosened `main: CI` ruleset.** The rule is the only
  gate that puts fmt/clippy/tests/audit (including Windows and macOS) before
  `main`. Removing it to save a PR trades real coverage for ceremony.
- **Keep the manual steps, only document them better.** The `0.7.0` drift
  shows prose is not a mechanism.

## Consequences

- The release still costs one commit, but it is produced mechanically; the
  human (or agent) runs one command after the smoke test.
- A release PR runs the same CI as any change and blocks until green; the
  admin bypass only skips the review requirement, never the checks.
- The release branch is deleted by the repository's rebase-merge setting; the
  local branch may be deleted manually.
- Hand-cutting a release remains possible but undocumented; `release.yml`'s
  tag-equals-manifest guard still catches the version half of the invariant,
  and the tag-on-`main` half is checked by the command.
