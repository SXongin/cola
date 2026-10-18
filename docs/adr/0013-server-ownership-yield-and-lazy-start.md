# Server ownership: yield + lazy start + autostart

cola used to start its own `opencode serve` at boot whenever none was running,
and blindly attach to the first default-store server. OpenChamber never reuses
an existing server (it always spawns its own on a random port), so cola-first
startups produced two servers writing the same store, and the reconnect loop
could flap between them. We decided: the Shared Store has exactly one server,
cola yields its Owned Server when a Coexistent Server appears, and cola never
starts a server until a prompt actually needs one.

## Decision

- **Selection priority** (fixes the flap): among default-store candidates, a
  Coexistent Server wins over cola's Owned Server; the configured
  `[opencode] url` port is only a tiebreaker within the same class.
- **Yield**: when a Coexistent Server appears while cola is attached to its own
  Owned Server, cola terminates the Owned Server (verified via
  `self-opencode.pid` + live `serve`) and re-attaches — but only when no
  session is in flight, so a streaming generation is never truncated.
- **Lazy Start**: boot attaches only and never spawns. An Owned Server is
  spawned at the moment of first need (a prompt is about to be sent, discovery
  finds no server). `[opencode] start_server = "auto"` (default, lazy),
  `"never"` (attach-only, never spawn), or `"eager"` (the old boot-time spawn).
  The OpenCode client's endpoint becomes late-bound (`reconnect` already
  exists); a spawn-in-progress guard prevents double-spawning on concurrent
  first messages.
- **Autostart**: `cola autostart enable|disable|status` registers a launcher per
  platform — Linux systemd user unit (with a `loginctl enable-linger` hint and
  the installer's PATH snapshot so the headless unit can find `opencode`),
  macOS LaunchAgent, Windows `HKCU\...\Run` value. `ExecStart` is the `cola`
  binary itself; no separate `serve` subcommand.

## Consequences

- The tested path (OpenChamber first, cola attaches) is preserved and now also
  holds when OpenChamber arrives after cola's Owned Server (yield).
- `[opencode] url` degrades from a port preference to a tiebreaker, matching
  its existing documentation as "preferred/fallback port".
- `/restart-opencode` keeps its semantics: only an Owned Server may be
  restarted; a Coexistent Server reports `NotOwned`; no server reports
  `NoServer`.

## Known limitations

- **Busy detection covers only cola's own prompts.** The yield is deferred
  while any session is in cola's `inflight` set, which tracks sessions cola
  itself prompted. A turn started by another shared-store client on cola's
  Owned Server and merely rendered externally (the external-message flow) does
  not mark the session busy, so a yield could still truncate such a
  generation. Deliberately not extended to external renders: adding the
  render loop to `inflight` risks a session stuck "busy" on an early exit
  path. Only reachable when a non-cola client (e.g. a manual CLI) prompts on
  cola's Owned Server while a Coexistent Server appears — rare, accepted.