# Printable version: embed Build Provenance at build time; self-update still compares the Release Version alone

cola could not print its own version: `Cli` declared no `--version`, there was no
Feishu `/version`, and the only place the version surfaced was the self-update
flow (`/update`, `cola update`, the silent startup check), all of which read
`CARGO_PKG_VERSION`. Worse, that single number cannot tell a local dev build
from a released binary: a `cargo build` on `main` embeds the same `0.7.0` as the
release built from the `0.7.0` tag, so "what build is running?" had no answer.
We decided to make every binary identify itself, and to make release vs dev a
property of the build's git provenance rather than of the human who built it.

## Decision

1. **`build.rs` stamps the binary's Build Provenance from the git tree it was
   built in.** It runs `git describe --tags --exact-match HEAD`,
   `git rev-parse --abbrev-ref HEAD`, `git rev-parse --short HEAD`, and
   `git status --porcelain`, and emits them as `cargo:rustc-env` vars. Best
   effort: any git failure (no git, a tarball checkout) leaves the vars unset.
   The hook-install gate (`CI`/no-`.git`) is split out so stamping still runs in
   CI and non-git trees.
2. **A build is a release build IFF HEAD is exactly at a release tag equal to
   the Cargo.toml version AND the tree is clean.** Release binaries only come
   from `release.yml`'s tag builds, which satisfy that by construction — no CI
   marker, no build-profile check, no extra release plumbing. Every other build
   (a branch, a dirty tree, no git at all) is a **dev build**.
3. **The display string splits Release Version from Build Provenance.**
   - Release: `cola 0.7.0`
   - Dev: `cola 0.7.0-dev <branch>@<shortsha> ⚠` (detached HEAD omits the
     branch; a tree with no git shows bare `cola 0.7.0-dev`). ⚠ uses the same
     Dirty definition as the Turn Footer (porcelain, untracked included).
   The CLI (`cola --version`/`-V`, handled before config/log/lock), the Feishu
   `/version` reply (the same canonical string plus a Chinese note on dev
   builds), and a startup log line all render this one string.
4. **Self-update comparisons keep using the Release Version alone (ADR-0015
   unchanged).** The dev marker is display-only and never enters the semver
   compare — otherwise a local build ahead of the last release would report
   "update available" forever and `/update` would *downgrade* the developer.
   When a dev build runs `/update` (or the startup check finds a newer release),
   cola says so before proceeding.

## Considered options

- **`cfg!(debug_assertions)` as the release/dev split.** A local
  `cargo build --release` would still masquerade as the shipped binary; profile
  says nothing about provenance. Rejected.
- **A CI-only marker (`release.yml` sets an env).** Correct but forces the
  release path to carry state; the git-exact-tag test is automatic and cannot be
  forgotten in a new workflow. Rejected.
- **git describe version as the single printed number (`0.7.0-2-gab12cd3`) and
  comparing on it.** Reads great, compares wrong: prerelease ordering makes any
  build after the last tag "older" than the tag, so update-check lies on exactly
  the dev builds this feature exists for. Rejected — comparison source and
  display string must stay separate (see item 4).
- **Unknown provenance renders as the bare version.** A git-export of `main`
  would present as a release. Rejected — unverifiable provenance is a dev build.

## Consequences

- Release identity stays honest by construction: the only binary that prints a
  bare `cola <version>` is one built at that exact clean tag.
- Dev identity's dirty ⚠ is a snapshot taken when `build.rs` ran; it refreshes
  when git state moves (HEAD/refs change, which re-runs the script) and may lag
  a pure source edit inside an incremental build. Cosmetic, accepted.
- `/version`, `cola --version` and the startup log share one canonical string,
  so a log line can be pasted straight into a bug report.
- The release.yml tag-vs-manifest guard is unchanged and remains the contract
  that keeps a tag build printing its own version.
