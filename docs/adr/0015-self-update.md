# Self-update: GitHub Releases as the single update channel

cola needs to update itself on machines the operator only reaches through
Feishu. We decided: the update channel is GitHub Releases only — the release
binaries already built by `release.yml` — and the update is applied by an
in-band `cola update` / `/update` that reuses the existing restart machinery.

> **Amended by ADR-0030**: cola is published on crates.io as `colark`.
> GitHub Releases remains the self-update channel for binaries cargo does not
> track; a cargo-tracked binary is updated with cargo instead of this flow.

## Decision

- **Channel**: GitHub Releases (`releases/latest`). The update checks
  `env!("CARGO_PKG_VERSION")` against the latest release tag (semver compare),
  downloads the asset matching the current platform triple, verifies it against
  the release's `SHA256SUMS`, extracts, and atomically replaces the running
  binary. Platforms with no prebuilt asset (e.g. Linux aarch64, macOS Intel)
  get a clear "no binary for this platform" message. The latest-tag lookup uses
  the **HTML `releases/latest` 302 redirect** (github.com, not `api.github.com`):
  GET with redirects disabled, the tag is read from the `Location` header, and
  the asset + `SHA256SUMS` URLs are constructed from the tag (`release.yml`
  names them deterministically as `cola-<tag>-<triple>.tar.gz`/`.zip`). This
  avoids the API endpoint's unauthenticated quota (60 req/hr/IP) entirely —
  downloads are CDN-served and never count against it.
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
  actually up; a dead bot gets a plain "start cola" note. A supervisor restart
  is only reported as successful **after verification**: the singleton lock
  must move to a NEW daemon process running the freshly-installed binary
  (guards against a manually-started instance still holding the lock — systemd
  starts without `--replace` — or an ExecStart pointing at a different install
  than the updated binary). The same ExecStart-vs-current-exe mismatch is
  warned about before the binary is replaced, so an operator fixes the path
  instead of discovering a silent stale daemon.
- **Restart**: reuse the existing re-exec (`restart_process()` + `--replace` +
  singleton takeover + `restart-notify.json`) on every platform except one:
  when a systemd unit owns the cola process itself (detected via
  `INVOCATION_ID` + `SYSTEMD_EXEC_PID` == cola's own PID — see ADR-0021, the
  detection refined after `INVOCATION_ID` alone leaked into unit-owned shells)
  cola does NOT spawn a child — systemd's default `KillMode=control-group`
  would kill a spawned child when the unit stops, so cola instead exits with a
  non-zero code and lets the unit's `Restart=on-failure` bring up the new
  binary. macOS (launchd `KeepAlive`) and Windows (no supervisor) keep the
  existing spawn-and-exit behavior; the singleton lock resolves any race with
  launchd's KeepAlive restart.
- **crates.io is a distribution channel, not a second self-update channel
  (ADR-0030)**: cola is published as `colark` (binary still `cola`), so
  `cargo install colark` / `cargo binstall colark` are supported installs.
  A binary tracked by cargo's install receipt is updated through cargo —
  `/update` detects this and reports the command (`cargo install colark`,
  `--force` when cargo answers "already installed") instead of replacing the
  binary; its availability check reads the crates.io sparse index. The GitHub
  Releases flow above applies to every binary cargo does not track.
- **Release invariant**: the binary's embedded version must equal the release
  tag, otherwise self-update reports "update available" forever. `release.yml`
  gains a step enforcing `Cargo.toml` `package.version == tag`.

## Consequences

- No admin/owner concept was added; `/update` follows `/restart`'s open model.
- v1 has no rollback: the `SHA256SUMS` gate prevents corrupt installs, and
  "latest only" means downgrade via the updater is not supported.
- Version bumps must keep `Cargo.toml` and the tag in lockstep (guarded by CI).

## Amendment (2026-10-13): one live binary path, and an update announced only when verified

Issue #551 (a `/update` that answered `✅ 已更新到 0.12.4。` while `/version`
still said `0.12.3`) rests on one wrong premise the decision above never
spelled out: that `std::env::current_exe()` is a **path to a file**.
It is a handle to the *running* binary — on Linux `/proc/self/exe`, which
renders as `"<path> (deleted)"` once the file behind it was replaced under the
process. That is the normal state of any daemon kept running across a cargo or
archive upgrade, so it is not exotic: with the marker unhandled, channel
detection failed to `canonicalize`, silently degraded to GitHub (defeating
ADR-0030), and handed that path to `fs::rename` — which, for a **nonexistent**
destination, succeeds and creates it. The in-band path was left as the one
update surface with no verification, and it reported a success nothing checked.

- **The live path is resolved once, by one primitive.** `live_exe_path` /
  `current_exe_path` (`src/update.rs`) are Linux-scoped, where the marker
  exists: the literal path wins while it still holds the process's own
  (device, inode) — so a binary legitimately named `"<name> (deleted)"` is not
  mis-read — and the marker-stripped path is the live binary otherwise,
  including when a pre-fix run's phantom sits at the literal path. Channel
  detection, the install target, the ExecStart comparison, the CLI's
  supervisor-restart verification, the silent startup check and
  `restart_process` all read that value — so they cannot disagree about which
  file to read and to replace. Elsewhere the marker does not exist and the path
  *is* the file (no stripping: a name ending in `" (deleted)"` is just a name).
  A binary that is not on disk at all fails the update with a diagnostic: a
  resolution failure is never a silent re-classification as some other
  channel's install.
- **`install()` fails closed.** The rename destination must be an existing
  regular file, and on Linux must not carry the procfs marker either (the
  phantom a pre-fix run leaves behind is a real file, so existence alone is not
  enough there). A rename that *creates* its destination is never an install of
  the binary we are running.
- **The reporting guarantee now binds the in-band path too.** The replacer
  reports the swap it actually performed (`二进制已替换为 X`); the **successor**
  verifies `restart-notify.json` against the one version it cannot be wrong
  about — its own (`current_version()`) — and announces `已更新到 X` only on a
  match, `⚠️ 更新未生效（当前 Y）` with an `orange` header otherwise. This is the
  in-band analogue of the CLI's `supervisor_restart_took_effect`, and the same
  rule applied to a different fact: **the consumer of the durable notify fact
  verifies it rather than trusting the writer**, whose claim about a version it
  will never run is not evidence. A versionless payload predates the field (both
  were added together) and keeps a versionless completion rather than inventing
  one.
- **The silent startup check reads the channel's own source** (crates.io for a
  cargo-tracked binary, GitHub Releases otherwise) and advertises only a release
  that channel can apply — a `NoAssetForPlatform` release is not one — so the
  version it logs is the one that binary's `/update` acts on, and the two update
  surfaces no longer disagree. An unresolvable binary propagates as a check
  failure there too, instead of the GitHub fallback this amendment removes.

Related: ADR-0030 (the receipt detection this amendment keeps reachable for a
replaced-under-the-daemon binary), ADR-0027 (a dev build's version), ADR-0021
(the restart hand-off the successor's card reports on).