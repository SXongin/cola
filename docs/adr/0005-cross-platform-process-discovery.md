# 0005: Cross-platform process discovery with sysinfo

## Decision

Replace every `/proc`-based process inspection in `discovery.rs` and `main.rs`
with the `sysinfo` crate, mirroring how OpenCode resolves its data directory.
Processes are enumerated with a limited refresh
(`ProcessRefreshKind::nothing().with_cmd(Always).with_environ(Always)`) on every
call — no caching. `pid_alive` does real liveness checks on all platforms
(zombie = dead, missing = dead, alive-but-unverifiable = alive). The default
store is "the store OpenCode would use by default": `$XDG_DATA_HOME` if set,
else `~/.local/share`, on every platform.

## Why

- macOS and Windows have no `/proc`, so discovery silently returned nothing and
  cola could never attach to a shared server there.
- OpenCode itself resolves its data directory with the `xdg-basedir` npm package
  (`$XDG_DATA_HOME || ~/.local/share`) on **all** platforms — a known Windows
  issue (opencode #8235) that was auto-closed without a fix. cola must mirror
  that, so `dirs::data_dir()` (which returns `~/Library/Application Support` on
  macOS) is the wrong yardstick for "default store".
- `kill` does not exist on Windows; the graceful first stage is meaningless for
  a windowless CLI, so Windows terminates with `taskkill /F`.

## Consequences

- `dirs` remains for `home_dir()`; its `data_dir()` use in store detection is
  replaced by an explicit XDG calculation shared with the server it attaches to.
- Reading another process's `environ` is best-effort (macOS/Windows limit it to
  same-user processes); unreadable env degrades to "assume default store".
- Zombie/`sh`-spawn tests stay Linux-only via `#[cfg(target_os = "linux")]`;
  cross-platform behaviour is covered by new pure-logic tests.