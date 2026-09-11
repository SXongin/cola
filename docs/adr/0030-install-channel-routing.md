# Two install channels, two update paths: crates.io installs update through cargo

cola ships through two installers: GitHub Release archives and crates.io
(`cargo install colark` / `cargo binstall colark`, binary still named `cola`).
Self-update (ADR-0015) replaced the running binary from GitHub Releases
regardless of how it got there, so a cargo-installed binary silently switched
provenance, left cargo's install bookkeeping (`.crates2.json`) lying about the
installed version, and kept receiving versions a crates.io yank no longer
offered. We decided the install channel decides the update path: a
cargo-tracked binary is updated with cargo, everything else keeps self-update.

## Decision

1. **Cargo-tracked ⇒ cargo updates it.** Before replacing a binary, `/update`
   and `cola update` look for cargo's install receipt: a binary in `<root>/bin`
   (`cola`, `cola.exe` on Windows) whose install root `<root>`
   (`$CARGO_HOME`, `$CARGO_INSTALL_ROOT`, or a `--root` directory) has a
   `.crates2.json` listing that binary name in some package's `bins`. A match switches the flow from "download and replace" to "report the
   available version and how to get it": `cargo install colark` — binstall
   installs may use `cargo binstall colark`; when cargo answers "already
   installed", add `--force`. cola does not run cargo itself for now (see
   options); the update is one command in the operator's shell.
2. **Cargo-channel availability comes from crates.io, not GitHub Releases.**
   For a cargo-tracked binary the check asks the crates.io sparse index
   (`https://index.crates.io/co/la/colark`), so "update available" is true for
   the channel the binary actually lives on. A GitHub release whose crates.io
   publish failed is not an update for a cargo install.
3. **No receipt ⇒ GitHub Releases self-update (ADR-0015), unchanged.** This
   covers release archives and source builds; dev builds keep the ADR-0027
   warning. Detection is best-effort: `--no-track` installs and binstall's
   `--install-path` (local, no global receipt) look like GitHub installs and
   will be self-updated — the documented cost of opting out of cargo's
   bookkeeping.
4. **`build.rs` learns the crates.io provenance.** A registry install is
   extracted without `.git` but with `.cargo_vcs_info.json`, so today's
   build.rs marks it a dev build and `/version` claims "本地 dev 构建" for a
   legitimate registry release. When that file is present and `.git` is not,
   build.rs stamps a crates.io channel and treats the build as a release
   identity (`cola 0.8.1 (crates.io)`); registry installs stop lying about
   being dev builds, and the compile-time channel is available as a fallback
   when no runtime receipt exists.
5. **Release pipeline order is unchanged** (GitHub Release first, crates.io
   after). crates.io publishing is not revocable, so publishing it first would
   create a crate whose release assets do not exist yet; a failed crates-io job
   is an operational incident to rerun. A yank must be paired with removing the
   GitHub Release, or GitHub-channel users keep receiving the yanked version.

## Considered options

- **Unconditional self-update (deno's model).** Simplest, but the cargo
  bookkeeping lie becomes user-visible: `cargo install colark` may answer
  "already installed" while a different binary is on disk, a crates.io yank no
  longer stops updates, and a source build silently becomes a prebuilt one.
  Rejected once crates.io became a real channel.
- **Prompt, but check GitHub Releases for availability.** Would tell a cargo
  user to update when crates.io has nothing newer — the advice would be wrong
  for the channel. Rejected for the channel-matched sparse-index check.
- **cola runs `cargo install` itself.** Preserves one-tap Feishu updates, but
  the daemon environment is not the installer's (different user, no
  `~/.cargo/bin`, no proxy/mirror config, possibly older rustc), the build is
  minutes of CPU on a host where `/update` is open to any chat member, and it
  executes the whole dependency tree's build scripts — a much larger trust
  surface than a checksummed prebuilt asset. Windows cannot replace a running
  exe at all. Rejected for v1; additive later behind the same detection.
- **Compile-time `no-self-update` feature (rustup's model).** Airtight for
  distro builds, but it conflates "cargo-installed" with "built from source"
  and removes self-update from source builds that use it today. The runtime
  receipt is the ground truth for install method; the compile-time channel
  stays as a fallback signal, not the gate.

## Consequences

- `/update` on a cargo install replaces nothing; it returns at most a version
  notice plus the exact cargo command.
- cargo's bookkeeping stays truthful: the binary on disk is the one cargo
  installed, so `cargo install --list` and third-party tools (cargo-update,
  cargo-binstall) see the real version.
- crates.io package builds are no longer misreported as dev builds.
- The channels can still skew when the crates-io job fails after a release;
  accepted, with the rerun/yank convention above.
- The README's claim that a `cargo install`-ed binary can move itself onto the
  GitHub channel via `/update` is now wrong and must be replaced by the cargo
  commands.
