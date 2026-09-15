# binstall installs from the release archives; a new install channel must bring an update path

`cargo binstall colark` was documented as a supported install (ADR-0015,
ADR-0030) but never worked: binstall's default `pkg-url` derives the asset name
from the crate name (`colark-<version>-<target>`), while `release.yml` ships
`cola-<version>-<target>`, so the download 404ed and binstall fell through to
QuickInstall (third-party builds) or to a source compile. This ADR fills that
gap with `[package.metadata.binstall]` and records which distribution channels
cola deliberately does not add.

## Decision

1. **`[package.metadata.binstall]` maps binstall to the existing release
   assets.** `pkg-url = "{ repo }/releases/download/{ version }/cola-{ version }-{ target }{ archive-suffix }"`
   (the binary name is literal: `{ bin }` renders only in `bin-dir`, not in
   `pkg-url`), `bin-dir = "{ bin }{ binary-ext }"` (the archives are flat),
   `pkg-fmt = "tgz"`, and a `cfg(target_os = "windows")` override with
   `pkg-fmt = "zip"`. No asset rename, no cola code change: the metadata is read
   from the published `.crate`, so it takes effect with the next crates.io
   publish.
2. **`disabled-strategies = ["quick-install"]`.** QuickInstall serves a third
   party's binaries — built and hosted without cola's provenance or version
   control — and must not be what a user silently gets (ADR-0034's posture).
   `compile` stays enabled: on a target with no asset (Linux aarch64, Intel
   macOS) binstall falls back to `cargo install` from crates.io source — exactly
   what the user would run by hand, with crates.io's checksum behind the source.
3. **binstall stays one cargo channel.** A binstall install writes cargo's
   install receipt like `cargo install` does, so ADR-0030's detection already
   routes its updates through cargo (`/update` replaces nothing and reports the
   command). The receipt cannot tell the two apart, and does not need to: the
   message names both commands, and only the binstall one skips a compile.
4. **The Windows zip carries `cola.exe` at its root.** binstall's
   `bin-dir = "{ bin }{ binary-ext }"` resolves to `cola.exe`, but the zip
   packaged the versioned `cola-<version>-<target>.exe`; `release.yml` now
   stores it as `cola.exe` (the asset file name is unchanged). This also makes
   the README's "extract `cola.exe`" literally true. cola's self-updater finds
   the exe by content, so it is unaffected.
5. **A new install channel must bring an update path.** Homebrew, Scoop and
   winget keep their own install bookkeeping; a binary they installed that
   self-updates goes stale against it — the same class of problem ADR-0030
   solved for cargo — so each needs a detection and update-routing story before
   it is offered. On that basis: **Scoop and winget are not added now**;
   **Homebrew is parked** behind a research issue (Cellar detection, Homebrew's
   stance on self-updating tools, formula bump automation); **cargo-dist is not
   adopted** — its outputs (installers, formulae, binstall metadata) are either
   already covered by hand or not wanted yet, and its conventions would fight
   the asset naming and provenance contracts (ADR-0015/0027/0030). Revisit all
   three if demand appears.
6. **No China-specific publishing.** GitHub Releases stays the archive channel
   and crates.io the cargo one; no mirror is maintained — a mirror cannot carry
   the release's build-provenance attestations, and keeping it fresh would be
   on us. Slow-network users solve it on their side.

## Considered options

- **Delete the binstall mention instead.** Honest, but gives up the no-compile
  install for Rust users; the metadata is six lines and reuses assets already
  published.
- **Allow QuickInstall.** Makes binstall work even without metadata, at the
  cost of installing binaries built and hosted by a third party. Rejected for
  supply-chain reasons.
- **Homebrew tap now.** A one-line install and no quarantine `xattr`, but a
  third package-manager receipt to keep in sync. Deferred, not rejected.
- **cargo-dist.** Generates installers, formulae and binstall metadata, but
  cola's self-updater, `SHA256SUMS` gate, provenance attestations and asset
  naming are contractual; adoption would re-cut those for channels not being
  opened yet.

## Consequences

- `cargo binstall colark` downloads the CI binary for the three release targets
  (Linux x86_64, macOS aarch64, Windows x86_64) with no compile; other targets
  compile from source.
- The README/user-guide binstall claim becomes true with the next crates.io
  publish. Issue #157 stays open until the first post-metadata release verifies
  binstall on all three platforms. Before that release,
  `cargo binstall --manifest-path ./Cargo.toml colark` tests the metadata
  against the current release's assets.
- `--no-track` / `--install-path` binstall installs still look like GitHub
  installs and are self-updated (ADR-0030's documented cost; unchanged).
- The Homebrew/Scoop/winget/cargo-dist decisions are dated by this ADR; the
  brew research issue is their on-ramp.
