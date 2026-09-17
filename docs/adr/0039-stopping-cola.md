# Stopping cola

There was no CLI way to stop a running cola: the workarounds were `cola --replace`
then Ctrl-C, killing the PID from `~/.cola/cola.lock`, or — on Linux only —
`systemctl --user stop`. `cola autostart disable` was meant to turn cola off but
only removed the registration, and only on Linux/Windows (macOS's `launchctl
bootout` happened to stop the process too), so the off-story differed per
platform and was documented nowhere. We decided: a new `cola stop` subcommand
stops the running instance, and `autostart disable` means stop + unregister.

## Decision

- **`cola stop`** stops the single running cola instance — the Singleton Lock
  holder of `~/.cola/cola.lock`. Config-free (handled before config loading,
  like `autostart` and `update`). Idempotent: no instance → prints 未运行 and
  exits 0. On an interactive terminal it asks for confirmation first; a
  non-TTY run proceeds without asking (script-friendly), `--yes` skips the
  prompt, and declining prints 已取消 and exits 0.
- **Supervisor first, PID fallback**: when the Autostart artifact is installed,
  stop through the Supervisor (`systemctl --user stop cola.service` /
  `launchctl bootout gui/<uid>/com.cola.bot`); if the lock holder is still
  alive afterwards, terminate it by PID (SIGTERM, escalating to SIGKILL). The
  PID path reuses the existing guards — only a live, identity-verified cola
  process is killed. Windows' `Run` key has no Supervisor, so stop is
  PID-only there.
- **`autostart disable` = stop + unregister on every platform**: stop the
  running instance (same confirmation rule), then remove the unit/plist/Run
  value. If stopping fails, unregister anyway, warn, and exit non-zero; if the
  user declines the stop, unregister anyway and leave the instance running.
- **Scope is cola alone**: an Owned Server keeps running after `cola stop`,
  since other Shared Store clients may still be using it (ADR-0013).
- **No Feishu-side stop**: `/stop` stays prompt interruption. cola cannot be
  started from Feishu, so a remote off-switch is a one-way door.

## Considered options

- **PID-only stop**: simplest, but a macOS LaunchAgent is `KeepAlive=true` —
  launchd respawns a directly-killed process, so the stop would silently fail
  there. The Supervisor path is mandatory, not a convenience.
- **Always prompt / require `--yes`**: blocks scripts that inherit a TTY, and
  makes CI read EOF as "no". Prompting only when stdin is a terminal (the
  existing `confirm_replace` convention) keeps humans safe and scripts
  unblocked.
- **`cola autostart stop` instead of `cola stop`**: cannot stop an instance
  started manually (nohup/foreground), which is exactly when no Supervisor
  exists.
- **Feishu `/quit`**: a kill switch with no matching start; under Autostart it
  would only come back at next boot.

## Consequences

- `autostart disable` now kills the running daemon, so the README's
  relocation advice (disable → move → enable) restarts cola where it used to
  keep running; the docs must say so.
- `/restart`, `/update`, and `cola --replace` are unchanged; `--replace` stays
  the CLI way to restart (it takes over the Singleton Lock directly instead of
  going through the Supervisor).

## Known limitation

- A supervised instance that never holds the Singleton Lock (a crash loop — a
  launchd `KeepAlive` agent retrying a binary that exits immediately) is not
  "running" under this definition, so `cola stop` reports 未运行 without
  cleaning it up. systemd's start limit bounds the Linux case; on macOS a
  manual `launchctl bootout` bounds it.
