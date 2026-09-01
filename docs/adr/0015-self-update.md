# Self-update: GitHub Releases as the single update channel

cola needs to update itself on machines the operator only reaches through
Feishu. We decided: the update channel is GitHub Releases only — the release
binaries already built by `release.yml` — and the update is applied by an
in-band `cola update` / `/update` that reuses the existing restart machinery.

## Decision

- **Channel**: GitHub Releases (`releases/latest`). The update checks
  `env!("CARGO_PKG_VERSION")` against the latest release tag (semver compare),
  downloads the asset matching the current platform triple, verifies it against
  the release's `SHA256SUMS`, extracts, and atomically replaces the running
  binary. Platforms with no prebuilt asset (e.g. Linux aarch64, macOS Intel)
  get a clear "no binary for this platform" message.
- **Trigger**: manual only — a Feishu `/update` command (open to anyone, like
  `/restart`) and a `cola update [--check]` CLI subcommand. Startup does a
  silent check that logs when an update exists. No auto-apply, no periodic
  checks, no `[update]` config section in v1. The in-band `/update` replaces
  the binary **and restarts itself** (correct because it runs inside the
  daemon, where the systemd/launchd context is known). The `cola update` CLI
  replaces the binary and then restarts a RUNNING daemon through its OS
  supervisor when one is registered (`systemctl --user restart cola` /
  `launchctl kickstart`) — supervisor-mediated, so the daemon stays supervised
  and does not die with the CLI's terminal. When no supervisor restarts it, the
  CLI prints a hint that depends on whether a daemon is running (it checks the
  singleton lock): `/restart` in Feishu is only offered while a daemon is
  actually up; a dead bot gets a plain "start cola" note.
- **Restart**: reuse the existing re-exec (`restart_process()` + `--replace` +
  singleton takeover + `restart-notify.json`) on every platform except one:
  under a systemd unit (detected via `INVOCATION_ID`) cola does NOT spawn a
  child — systemd's default `KillMode=control-group` would kill a spawned
  child when the unit stops, so cola instead exits with a non-zero code and lets
  the unit's `Restart=on-failure` bring up the new binary. macOS (launchd
  `KeepAlive`) and Windows (no supervisor) keep the existing spawn-and-exit
  behavior; the singleton lock resolves any race with launchd's KeepAlive
  restart.
- **No crates.io for now**: the `cola` crate name is taken on crates.io (a text
  CRDT library). Publishing is deferred until a name is chosen; self-update
  does not depend on it. `cargo-binstall`/`cargo install` remain future
  conveniences.
- **Release invariant**: the binary's embedded version must equal the release
  tag, otherwise self-update reports "update available" forever. `release.yml`
  gains a step enforcing `Cargo.toml` `package.version == tag`.

## Consequences

- No admin/owner concept was added; `/update` follows `/restart`'s open model.
- v1 has no rollback: the `SHA256SUMS` gate prevents corrupt installs, and
  "latest only" means downgrade via the updater is not supported.
- Version bumps must keep `Cargo.toml` and the tag in lockstep (guarded by CI).