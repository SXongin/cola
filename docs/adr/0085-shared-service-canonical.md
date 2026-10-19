# The V2 Shared Service is the canonical server

ADR-0013 fixed the two-server flap: cola yields its Owned Server to a
Coexistent one. That model predates the V2 **Shared Service**. OpenCode V2's
default way for a machine to have a server is `opencode serve --service`: the
TUI, the desktop app and the Web UI all resolve it through the registration file
(`$XDG_STATE_HOME/opencode/service.json`), and its daemon is detached, so it
outlives any terminal. A service carries no `--port`; discovery reads its port
and private password from that registration (spec #364 §2).

Real-time sync in V2 is **process-local**. The SSE event feed is an in-process
PubSub, and a permission/question pending list lives in the process holding the
turn. Only the SQLite store is shared across processes, so content converges
with delay while events and permissions do not. One machine-wide server plus one
mapped Session is therefore the sufficient condition for terminal ↔ Feishu
sync — and which server cola picks stops being a preference and becomes
correctness. Under ADR-0013's rule a Coexistent server always outranked cola's
Owned one, and its port was the tiebreaker: OpenChamber's managed server takes
an OS-assigned port (`desiredPort = 0`), so it beat the service whenever that
port happened to be lower — a coin flip per launch that silently broke the
terminal and Web UI.

## Decision

This ADR extends ADR-0013: its Owned/Coexistent/Yield model, the Lazy Start
rule, and the "one server per Shared Store" invariant stand unchanged. What
follows adds the Service class ahead of both existing classes and the ownership
boundary around it.

- **Selection priority**: Shared Service > Coexistent > Owned. `ServerCandidate`
  gains a `ServerClass` set from argv (`--service`); Owned is resolved against
  cola's recorded pid, and a Service is never Owned. Within the winning class
  the configured `[opencode] url` port wins, else the lowest port, then pid —
  deterministic across sysinfo's randomized scan order, so the reconnect loop
  cannot flap. Eligibility is unchanged (`uses_default_store && port != 0`).
- **Boundary — cola may start and attach the service, but never owns or reaps
  it.** Its pid is never written to `self-opencode.pid`, the yield never
  terminates it, and `/restart-opencode` restarts it through its own CLI
  (`opencode service restart`) and then waits, bounded (30s), for the
  registration file to describe a live `serve --service` that accepts
  connections. cola does not kill it or re-spawn it with `serve --port`, and
  does not rewrite its registration. (Starting the service when none exists is
  #810; this ADR fixes the ownership boundary that makes it possible.)
- **Detaching the started service deviates from #810's literal `setsid`.** The
  child leaves cola's process group through `CommandExt::process_group(0)` with
  stdin closed and stdout/stderr redirected to the service log, rather than a
  `pre_exec`/`libc::setsid`: the crate forbids `unsafe` outside tests, so that
  route is unavailable. With no controlling-terminal access and no
  foreground-group membership the child receives no terminal hangup, so it still
  outlives cola — the property `setsid` was there to provide.
- **Restart outcomes**: the decision is a pure `restart_plan` over the canonical
  pick. Service → `opencode service restart` + bounded wait → `RestartedService`;
  Owned → the existing terminate / re-raise on the same port and password →
  `Restarted`; Coexistent → `NotOwned`; nothing → `NoServer`.
- **Several servers stay visible, never aggregated.** cola still attaches to
  exactly one. Each reconcile pass WARNs the canonical server (URL + pid) and
  every ignored one (URL + pid, service or not); `cola status` shows the
  canonical server and lists `other_servers` in the human report and `--json`;
  `cola doctor` WARNs when more than one default-store server runs.

## Considered options

- **Keep the service as just another Coexistent server**: rejected. It is the
  process the default clients connect to, so a port tiebreak against it is a
  coin flip that breaks sync; the class distinction is what removes it.
- **Aggregate or proxy two servers**: rejected. Two servers do not share
  sessions or in-flight state, only the store; a proxy could not merge their
  event feeds or permission lists, and cola would be inventing a protocol.
- **Have cola own the service** (record its pid, yield/reap it like an Owned
  Server): rejected. The service's lifecycle belongs to OpenCode — it self-heals
  its registration and rejects a replacement daemon — and cola's exit must not
  take a machine-wide rendezvous down with it.

## Consequences

- A running service is stable: cola attaches to it whatever another server's
  port is, so the measured flap between service and OpenChamber disappears.
- `/restart-opencode` no longer touches an Owned Server the canonical pick
  outranks (a Coexistent or a Service): cola reports `NotOwned` instead of
  restarting a process it is about to yield.
- Restarting the service depends on the `opencode` CLI being on `PATH` — the
  same dependency Lazy Start already carries.
- The service outliving cola is expected, not a leak; a stale registration
  (registered pid no longer alive) is still ignored rather than attached to,
  the ADR-0013 rule surviving unchanged.
- ADR-0013's Owned/Coexistent/Yield model is not revised; this ADR adds the
  Service class ahead of it and the ownership boundary above it.
