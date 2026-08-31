# 06 - Daily log rotation

Status: resolved
Type: task
Blocked by: none

## What to build

Rotate the append log daily instead of growing one file forever (ADR-0012).
Log files live in `~/.cola/` (or the `--log-file` parent):

- Active log: `cola.log` (current day), matching today's behavior of always
  appending to a file.
- On the first write of a new day, rename `cola.log` →
  `cola-YYYY-MM-DD.log` and start a fresh `cola.log`. Simpler alternative:
  write directly to `cola-YYYY-MM-DD.log` and keep `cola.log` as a symlink or
  drop it; decide in implementation — the key requirement is one file per day
  and a stable "current" file for tailing.
- Retention: keep the most recent N days (default 14), delete older
  `cola-*.log` files. N configurable — `[bridge] log_days` (default 14) or a
  `--log-days` CLI flag; pick one (config is the consistent home since it
  already holds log-adjacent settings).
- Cross-day session queries are NOT handled by the log layer: log lines already
  carry `session_id`, operators grep across `cola-*.log`.

## Scope

- `src/main.rs` logging setup (lines ~312-336): replace the single
  `tracing_subscriber::fmt::layer()` file target with a small daily-rotating
  writer (implement in-place or via a tiny helper; the codebase avoids adding
  dependencies — check `Cargo.toml` before pulling in a rotation crate).
- Retention sweep on startup (and optionally at each rotation).
- `[bridge] log_days` config field + README Logs section update.

## Acceptance criteria

- [ ] Two days apart, two files exist (`cola-YYYY-MM-DD.log` per day); the
      current day's writes land in the active file.
- [ ] Files older than `log_days` are deleted on startup.
- [ ] A restart never truncates (append, not overwrite) — same guarantee as
      today.
- [ ] stdout mirror behavior unchanged (terminal → ANSI mirror; redirected →
      file only).
- [ ] `cargo test --workspace --locked` green.