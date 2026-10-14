# The CLI gains lifecycle/introspection commands — status, doctor, restart, logs, config, completions

cola's command-line surface had grown to exactly four responsibilities —
`autostart` (ADR-0013), `update` (ADR-0015), `stop` (ADR-0039), and `--version`
(ADR-0027) — while the Feishu side had accumulated a large session-control
surface (`/dir`, `/switch`, `/new`, `/topic`, `/agent`, `/model`, `/think`,
`/autoaccept`, …). The asymmetry is partly deliberate: those commands act on a
chat's session, and a terminal has no chat context, so they belong where the
session lives (and OpenCode's own CLI/TUI already covers session-level work).

But **introspection** was a real hole. Answering "is cola running, and on which
OpenCode server and generation?", "is my config valid?", "are my Feishu
credentials good?", or "where is the log?" meant reading `~/.cola/cola.lock`,
scanning processes, and tailing a file by hand. `cola stop` existed with no
`cola status`; `cola update` could restart a daemon with no `cola restart`; and a
support request had no single command to paste. We decided to extend the CLI with
lifecycle and introspection commands, and to keep session control on the Feishu
side.

## Decision

Six additions, all **config-light and lock-free** — they must work while cola is
running and against a broken install, so they are handled before the
config/log/lock machinery exactly like `autostart`, `update` and `stop`.

1. **`cola status [--json]`** — a one-shot snapshot: run state (the Singleton
   Lock holder's PID), autostart registration/state and its binary path, claim
   state and Host, the resolved config path and whether it parsed, the log path,
   and the shared OpenCode server (URL, PID, generation V1/V2, and the
   `[opencode] generation` override). `--json` emits the same facts under stable
   keys for scripts. It is local and makes no outbound call; the one network act
   is the attach probe for the server's generation, a **bounded (5 s) loopback**
   request to the server discovery already found, made through
   `Transport::new_loopback` so an inherited `HTTP_PROXY` can never carry the
   server's Basic credentials off the machine — it never reaches the internet,
   and a wedged server degrades to "世代未知" rather than hanging indefinitely.
2. **`cola doctor`** — the checks a support request needs, each printed with a
   ✅/⚠️/❌ mark and a summary: the config parses, the Feishu credentials
   authenticate (a tenant token **and** `bot/v3/info`), `opencode` is on `PATH`,
   the shared server probes to a generation, the Singleton Lock is not stale, the
   log file/dir is present, and the autostart registration's binary path matches
   the running binary. Exits non-zero when any check **fails** (warnings do not
   fail the run).
3. **`cola restart [-y]`** — restart the running instance. The Supervisor path
   (`systemctl --user restart` / `launchctl kickstart -k`) is used **only when
   the Supervisor owns the running process** (detected from the lock holder's
   environment: systemd's `INVOCATION_ID` + `SYSTEMD_EXEC_PID`, launchd's
   `XPC_SERVICE_NAME`): restarting the unit while an unrelated manual instance
   holds the lock would start a process that cannot acquire it, which
   `Restart=on-failure` would then crash-loop. Otherwise — and whenever the
   Supervisor command fails — a **detached** replacement is spawned from the
   instance's OWN command line, executable and working directory (so a relative
   `--config`/`./cola.toml` still resolves) plus `--replace`. Both paths verify,
   within a bounded wait, that a **different** PID took the lock before reporting
   success.
4. **`cola logs [-f] [-n N] [--date YYYY-MM-DD]`** — the tail of today's log, or
   a rotated day's file; `-f` follows appended lines and reopens the fresh base
   file across a midnight rotation.
5. **`cola config path|show|validate`** — the resolved config path, the effective
   config with the app secret **masked** (`***（N 个字符）`), or a parse check
   through the same parser the daemon uses.
6. **`cola version`** (a subcommand alias of `--version`) and
   **`cola completions <shell>`** (a generated script on stdout, via
   `clap_complete`).

`status`, `doctor`, `logs`, `config` and `completions` never load the log writer
or touch the lock; `restart` shares `stop`'s Supervisor-first / PID-fallback
structure.

## Considered options

- **Mirror the Feishu session commands in the CLI** (`cola switch`, `cola model`,
  …): rejected. They act on a chat's session through cola's mapping store; a
  terminal has no chat context, and OpenCode's CLI/TUI already does session-level
  work. Duplicating them would blur cola's "bridge, not client" identity.
- **A CLI `/claim`**: rejected. A Claim records the **Feishu** Host `open_id`,
  which the terminal does not have; the claim code alone is not enough.
- **Make `status`/`doctor` a single command**: rejected. `status` must stay fast
  and offline (it is safe to run anywhere and is a snapshot for scripts); the
  Feishu-credential and PATH probes are network/OS work that belongs behind an
  explicit `doctor` invocation.
- **Put `restart` on the Supervisor only**: rejected. Without a registration
  (a manually-started instance) there is still a lock holder to replace, so the
  CLI reads its command line and re-execs it detached — the same takeover the
  lock already protects with `--replace`.
- **`--config`-only for `config`**: the command honors the global `--config` and
  otherwise resolves the same fallback chain as boot (`./cola.toml` →
  `~/.cola/cola.toml`).

## Consequences

- The CLI surface is now "lifecycle + introspection"; a session-control command
  added to Feishu is not automatically a CLI command, and vice versa. This is the
  boundary future work should keep.
- `cola doctor` performs a real Feishu API call (token + bot info). It is
  read-only and never prints the secret, but it does require network and valid
  credentials — that is exactly what it is checking.
- `cola restart` on an unsupervised, manually-started instance re-execs it
  **detached with null stdio** (logs already go to the log file), so the restarted
  daemon no longer inherits the original terminal. Under a Supervisor this does
  not apply.
- A new dependency, `clap_complete`, is added for `cola completions`; it is
  MIT/Apache-2.0 and part of the clap project, so `cargo deny` stays green.
