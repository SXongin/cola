# An agent client is a peer attachment; Codex's is app-server

## Context

cola's whole value proposition rests on one architectural accident: OpenCode's
runtime is a server with a shared store. The TUI, the desktop app and the Web UI
resolve that server, and cola attaches to it as **one more client** — which is
why a Turn started in a terminal streams onto a Feishu card, and why a
Permission raised in one place can be answered in the other. ADR-0007 (session
identity lives on the backend), ADR-0013 (one server per Shared Store, cola
yields), ADR-0055 (per-generation attachment) and ADR-0085 (the Shared Service
is canonical) are all consequences of that property, not independent features.

Spec #860 asks for a second agent client, Codex first, with the same property.
The work is not "another adapter": an adapter can only mirror what the runtime
is willing to share, and the candidate Codex surfaces differ on exactly that.

- **`codex exec`** is a one-shot process. No live thread, no mid-turn approval,
  no steering. It cannot be a peer of anything.
- **The Codex CLI TUI** owns its thread in-process. It *can* be a peer — through
  `codex --remote <ws|unix>` against a `codex app-server` listener — but a TUI
  running against its own embedded server shares nothing.
- **`codex app-server`** (`codex-rs/app-server`) is the surface the richer
  clients use: the VS Code extension, the desktop and mobile apps (through
  `codex-rs/app-server-daemon`), and a remote TUI. It owns a thread store, and
  its subscription model is explicitly multi-client: `thread/unsubscribe`
  unloads a thread only when the connection was the **last** subscriber, after a
  grace period. It defines `thread/list`, `thread/read`, `thread/turns/list`,
  `thread/items/list`, `thread/resume`, `thread/fork`, `thread/status/changed`,
  `turn/start`, `turn/steer`, `turn/interrupt`, `thread/compact/start`,
  `model/list`, `skills/list`, plus server-initiated approvals
  (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`),
  `tool/requestUserInput` and `mcpServer/elicitation/request`. That is the
  OpenCode server shape, and its feature list maps onto cola's existing surface
  almost line for line.
- **ChatGPT / Codex cloud** runs elsewhere: no local thread store, no local
  checkout, a task-submission API rather than a session. Different product.
- **ACP** standardises client↔agent, but over **stdio with the agent as the
  client's subprocess** (the Streamable-HTTP transport is a draft). Its
  `session/load` can hand a session between client instances, but by replay and
  takeover — one writer at a time. Codex is on ACP's agent list only through
  `codex-acp`, whose own README says it "starts the Codex App Server and
  translates ACP requests into Codex operations". So ACP can reach many agents,
  but it is not a peer surface, and on Codex it adds a hop between cola and the
  server that actually holds the thread.

Two inherited facts constrain the state side. Every durable record keys a
Session by its bare server id (`SessionEntry.session_id`; the `records`,
`announcements`, `endings` and `delivered` sections of `chain_records.json`;
the surfaces and terminal records), and every sidecar path is derived from
`[bridge] session_file` by `with_file_name`, while `cola.lock` and the default
log path are hardcoded to `~/.cola`. No durable file carries a format version.
Config has the mirror problem in milder form: `[opencode]` is a table name
standing in for a backend identity, though only four non-test call sites read
it.

## Decision

**1. A supported agent client is one cola can attach to as a peer.** The bar is
the property cola already sells for OpenCode, stated here as the **Peer
Attachment** rule: one runtime-owned session, two client faces, and both faces
see the same live Turn and can answer the same pending approval. A backend that
can only hand over history between runs is a different mode (**Owned Runtime**:
cola starts the agent, cola is the only client, the user picks the session up
later), and it must never be described or configured as peer sync.

**2. Codex's peer surface is `app-server`, and that is the target.** Not `exec`,
not ChatGPT. cola becomes an app-server client: `initialize` + `initialized`,
`thread/list` and `thread/read`|`thread/turns/list` for discovery,
`thread/resume` to subscribe, the item/turn notification stream for rendering,
`turn/start`/`turn/steer`/`turn/interrupt` for prompts and `/stop`,
`thread/compact/start` for `/compact`, `model/list` and `skills/list` for the
pickers. Approval requests raise Permission cards; `item/tool/requestUserInput`
and elicitation raise Question cards. `thread/status/changed` feeds Session Sync.

**3. The user's client must be pointed at that server, and cola diagnoses when
it is not.** A daemon-attached TUI (`codex app-server daemon start`, then
`codex --remote …`, or the daemon's implicit attachment) is the supported
configuration; a TUI on an embedded server is unsupported and `cola doctor` must
say so, because the alternative is a bot that silently mirrors nothing. This is
ADR-0085's "one server is correctness" applied to Codex: cola discovers the
daemon socket under the shared `CODEX_HOME`, may start the daemon under
`start_server`, and never owns or reaps it.

**4. ACP is a later, cola-owned mode — not the way to reach the peer story.**
The ACP adapter, when it lands, is a second `kind` that spawns an agent process
and owns it (breadth: Gemini CLI, Copilot, Cursor, Kimi, Qwen, Claude, OpenCode
itself). Codex over ACP is explicitly **not** the peer path: it spawns
`codex-acp`, which spawns its own app-server. ACP v2 is still a draft, so the
mode waits until v1's stable set is the right trade for breadth.

**5. One client is active per cola process; the config is shaped for many.**
`[clients.<id>]` with a `kind` selects an adapter and carries that adapter's
options; `[opencode]` remains valid forever as an implicit
`clients.default`/`kind = "opencode"` (additive only, never renamed, never
rewritten on read), and `[bridge] default_client` picks the active one. Switching
clients is a config change plus a restart in this iteration. Per-chat routing
(one Chat/Topic on OpenCode, another on Codex, both live) is deferred: it needs
a client registry and a neutral DTO layer, neither of which can be designed
honestly before a second adapter exists.

**6. Durable state names its client before a second client exists.** Each
durable file gains an explicit format `version`; every record keyed by a Session
gains the client id; a legacy file reads as version 0 under the default client
and is rewritten once, in the new form. `StateLayout` becomes the single owner
of the `~/.cola` layout so nothing else derives paths with `with_file_name` or
joins `.cola` by hand. Renaming an `[clients.*]` id later is a state migration,
not a config edit, and the docs say so. The point is timing: this is cheap while
there is one writer and one client, and a data-recovery problem after a second
backend has written interleaved keys.

**7. The neutral `Backend` DTOs are earned from the second adapter, not
designed ahead.** The first Codex cut implements the existing trait and records
every mismatch (`Session`, `PermissionRequest` vs approval, `AgentInfo` vs
collaboration mode, `ProviderModels`, status, diff, transcript parts). The
generalisation follows that evidence, exactly as ADR-0053 generalised the read
model only after both generations existed.

## Why

The peer property is not a rendering feature; it is who owns the session. A
gateway that spawns an agent can be a perfectly good product, but it cannot
deliver "I see it in my terminal and in Feishu", and shipping it under that
promise would make cola's one differentiator a lie. Codex happens to expose a
server whose primitive set is close enough to OpenCode's that the bridge's
domain model — Session, Turn, Permission, Question, Compaction, Background Task
— survives intact; that is a strong signal the seam is right and the choice of
surface is right.

Writing the client id into durable state now is the same argument in the data
layer. The failure mode is not a crash; it is two backends' Session ids
colliding in `chain_records.json` or a watermarked card being re-announced after
an upgrade. cola already has one precedent for this class of bug (ADR-0069's
legacy-sidecar fold, whose "file presence is the migration marker" rule is
exactly what breaks once a second writer can appear), and one precedent for the
config half (`ThreadKey`'s `root_id` alias: add, tolerate, never rename).

## Considered options

- **ACP first, Codex through `codex-acp`.** Rejected as the first step. It
  multiplies agents but caps them all at Owned Runtime, adds a translation layer
  between cola and the thread, and builds on a protocol whose v2 is still a
  draft. It remains the right second step for breadth.
- **Use `codex exec` and poll the rollout JSONL.** Rejected. Reading the file is
  a mirror of a dead run, not a peer of a live one: no in-flight items, no
  approvals, no steering, and writing into the file is a second writer on
  Codex's format.
- **Treat the Codex TUI as the adapter target.** Rejected. There is no protocol
  into a TUI that owns its thread; `--remote` exists precisely to move the thread
  into a server.
- **Build per-chat multi-client routing now.** Rejected for this iteration. It
  forces a neutral DTO design and a registry before either has been exercised by
  a real second adapter, and `SharedCore`'s single `Arc<dyn Backend>` plus its
  single poller set would be rewritten twice.
- **Split state per client directory.** Rejected. A Chat/Topic switches clients;
  its card chain, watermark and task endings must survive the switch, and
  `/status`/`doctor` must see one layout. Split the key space, not the directory.
- **Make `[opencode]` a `[clients.opencode]` alias by rewriting users'
  files.** Rejected. Config files are hand-maintained and may be shared across
  versions; read both, write neither.

## Consequences

- The first Codex release is **one active client at a time**. Users switch by
  config + restart. Multi-client routing, `/client`, and per-card client labels
  are a later ADR.
- `cola status` and `cola doctor` gain client-scoped rows: which daemon was
  found, whether the user's TUI is attached to it, and which capabilities the
  connection advertised.
- Codex's protocol is versioned by binary and partly experimental. The adapter
  needs a probe-and-degrade seam like ADR-0055's generation strategy — read the
  `initialize` result, pin the schema the adapter was written against, and
  refuse or degrade loudly instead of guessing.
- **cola's poll-shaped Backend meets a push-shaped runtime.** `Backend` answers
  `transcript`/`session_status` reads on demand (ADR-0011); app-server delivers
  item deltas on a subscription. The Codex adapter must own that subscription
  and serve cola's reads from it — an adapter-internal cache, not a Bridge
  redesign.
- **Authorship needs a new signal.** ADR-0026 distinguishes cola's own messages
  by a `msg_cola_…` id it chooses at send time. Codex assigns turn and item ids
  itself, so the adapter must record the turns cola started and answer the
  External Message question from that record. The sync watermark itself still
  applies; only its authorship input changes.
- The state versioning work (decision 6) is a prerequisite for the second
  backend, not a follow-up. A separate ADR will record the concrete file
  formats when that migration is implemented.
- Glossary additions: **Agent Client** (a configured backend instance and its
  adapter `kind`) and **Peer Attachment** / **Owned Runtime** (the two sync
  modes). The existing **Backend** entry keeps its meaning; an Agent Client is
  the configured instance of one.

## Open questions (gate the adapter, not this ADR)

**Answered in part** by the 2026-10-23 amendment at the end of this file, on
`codex-cli 0.163.0`: questions 1 and 2 fully, question 3 at the protocol level
(the TUI half is #884's), question 4 on Linux only (all three platforms are
addressed by the ticket #954 amendment below). Kept for history: the questions
below are what the spike was run against.

1. With two connections subscribed to one thread, does a server-initiated
   approval go to every subscriber, only the turn's initiator, or the first to
   answer? If only the initiator, a TUI-started Turn cannot be approved from
   Feishu, and the Peer Attachment claim narrows to read-plus-start.
2. Does `thread/resume` on a thread another client has loaded subscribe without
   disturbing it, and is it idempotent across reconnects?
3. Does a `turn/steer` from the second client render in the TUI, and does a
   `turn/start` from cola while the TUI is mid-turn succeed, queue or fail?
4. Does the daemon's implicit attachment work on every supported platform
   (Windows AF_UNIX limits are documented), and how does discovery behave when
   `CODEX_HOME` is relocated?

## Amendment (2026-10-23): the gating spike's answers, on `codex-cli 0.163.0`

Run on WSL2/Linux with the standalone install: `codex app-server --listen
ws://127.0.0.1:4501`, two raw JSON-RPC WebSocket connections (A = the turn's
initiator, B = a second client that joined with `thread/resume`), and model
traffic through a custom `[model_providers.*]` entry with
`wire_api = "responses"`. No ChatGPT sign-in was involved — a provider-keyed
setup, which is the configuration decision 3 has to support.

Status: 1 and 2 are answered; 3 is answered at the protocol level, with the TUI
half left to #884; 4 is answered for Linux, with Windows still open at that
point (addressed by the ticket #954 amendment below). Each answer says which it
is.

### The four questions

**1. An approval reaches every subscriber, and any of them can answer it.**
_Answered._ A's turn raised `item/commandExecution/requestApproval`; both A
and B received it (A: 1, B: 1). B — the non-initiating client — answered
(A: 1, B: 1). B — the non-initiating client — answered
`{"decision":"accept"}`, both connections then received
`serverRequest/resolved`, and the turn completed normally on both streams.
Peer Attachment for Codex is therefore **read-plus-write**, not
read-plus-start: the ADR's stronger claim holds.

**2. `thread/resume` is the subscription, and it requires a persisted
rollout.** _Answered._ On a thread that has not yet run a turn, resume fails
with `-32600 no rollout found for thread id …`, so a freshly
`thread/start`-ed thread cannot be adopted until it has history — cola must
present that as a named state, not a transport error. A second client's resume
does not disturb the first subscriber: A kept receiving the full `turn/*` and
`item/*` stream while B received it too. Across reconnects the subscription is
clean and idempotent: B dropped its socket, a new connection resumed the same
thread successfully and received the next turn's item stream, a second resume on
that same connection also succeeded, and A was never disturbed. The thread stays
loaded while any subscriber holds it. `thread/unsubscribe` answers
`notSubscribed` when the connection never subscribed, and does not unload a
thread another subscriber holds (the server keeps it for the 30-minute
inactivity grace).

**3. A second client can act on an in-flight turn.** _Answered at the protocol
level; the TUI half is open._ A `turn/start` from B while A's turn was running
was accepted and folded into the active turn — it returned A's `turnId`, and no
second turn was created. `turn/steer` from B was accepted immediately, its text
appeared as a user-message item in **A's** stream, and the model answered the
steer on both connections. There is no dedicated steer notification: a steer is
a user message inside the turn, so the adapter renders it through the ordinary
item path. Whether the **TUI** paints that steer — and another client's approval
— was not observed here: the spike used raw WebSocket clients, so it is
inference from the shared item stream until #884 checks it on a real TUI.

**4. `CODEX_HOME` relocation works, and the control socket is a symlink.**
_Answered for Linux; Windows open._ The
socket follows `$CODEX_HOME/app-server-control/app-server-control.sock`, and
that path is a symlink into `/tmp/codex-daemon-0/<hash>`; discovery must follow
it rather than require a regular socket file. A `CODEX_HOME` under `/tmp` makes
Codex warn that it declines to install helper binaries there (warning only; the
server still serves). Windows remains untested at that time (exercised by the
ticket #954 amendment below).

### Facts the adapter is written against

- `thread/list` and `thread/loaded/list` are **server-global**, not
  per-connection.
- `thread/started`, `thread/status/changed`, `remoteControl/status/changed` and
  the `thread/goal/*` family are **broadcast to every connection**, subscribed
  or not; `turn/*`, `item/*` and `error` go **only to subscribers**. Session
  Sync can read status without subscribing; rendering must `thread/resume`.
- A turn started with no usable credential is **accepted first** and then fails
  as an `error` notification (`codexErrorInfo.responseStreamDisconnected.httpStatusCode
  = 401`, `willRetry: true`, 5 retries). An unreachable or uncredentialed
  backend therefore looks like a live turn that never completes. Decision 3's
  diagnosis gains a second, stronger check: **`cola doctor` must establish that
  the configured provider can complete a turn.** A ChatGPT sign-in is not a
  support requirement — a provider-keyed `[model_providers.*]` entry is a fully
  supported configuration.
- Pinned surface for `0.166.0` (re-pinned by ticket #953; the 2026-10-23 spike
  first recorded `0.163.0`): **106 client→server methods, 85 notifications,
  10 server→client requests**. The three lists are checked in at
  `docs/reference/codex-app-server-0.166.0.md`, generated from the binary with
  `codex app-server generate-json-schema --out <dir>`, so a CLI bump is a diff
  against that file rather than a re-derivation — and that output, generated
  from the pinned version, is what the adapter's contract is checked against.
  The live tier-1 suite regenerates this surface from the pinned binary and
  compares it with the checked-in file, so the reference is evidence rather
  than a guess.

### Corrections to this ADR

- `tool/requestUserInput` is spelled **`item/tool/requestUserInput`** in
  0.163.0 (corrected inline above).
- Server→client requests are **10**, not the four listed in the decision: add
  `item/permissions/requestApproval`, `item/tool/call`,
  `account/chatgptAuthTokens/refresh` and `attestation/generate`, alongside the
  legacy `applyPatchApproval` / `execCommandApproval` pair.
- The read-side consequence stands as written: the adapter owns the
  subscription and serves cola's poll-shaped reads from it. Nothing in the
  spike argues for a Bridge redesign.

### Left unverified

- Whether a real **TUI** renders another client's steer and approvals. The
  spike used raw WebSocket clients; the item stream is shared, so it should,
  but that is inference until #884 observes it.
- The Windows half of the daemon/socket matrix (AF_UNIX limits). The transport
  half is exercised by the ticket #954 amendment below; the managed daemon on
  Windows remains unmeasured.
- Whether cola's `thread/resume` can race a TUI-side unload inside the
  server's 30-minute grace window.

Spec #860's ticket map is cut from these answers.

## Amendment (2026-10-23, later): the running daemon is `0.164.0`

The pin above is `0.163.0`, but the managed daemon on the spike machine is a
**different binary**: `codex app-server daemon version` reports
`cliVersion: 0.163.0`, `appServerVersion: 0.164.0`, and
`managedCodexPath: ~/.codex/packages/app-server-daemon/current/bin/codex`. The
adapter must therefore probe the *running* binary, not the CLI on `PATH`.

Regenerating the schema from both binaries and diffing (ticket #876):

- `0.163.0` (the local `codex` CLI) reproduces the checked-in reference exactly:
  **105 client→server methods, 85 notifications, 10 server→client requests**.
- `0.164.0` (the daemon's managed binary) has **106 client→server methods**, one
  added — `feedback/save`; notifications and server→client requests are
  unchanged at **85** and **10**.
- Every other schema file diff is additive on the response side (v2
  image-generation attachment items, `ListMcpServerStatusResponse` and
  `PluginReadResponse` fields); no required field was removed and no method the
  adapter uses changed shape.

The adapter's declared contract stays the `0.163.0` surface at this point
(ticket #953 later re-pins it to `0.166.0`). Construction reads
`initialize`'s `userAgent` (`codex-tui/0.164.0 …`), and a version other than the
pin is checked against a schema regenerated from the binary the server is
*actually running*; a binary whose `--version` does not match the server's
reported version is refused, so a local `0.163.0` CLI can never vouch for a
`0.164.0` daemon. The required method subset is then checked; a missing method
fails construction with its name, and an older server is refused outright. The
live daemon at `~/.codex/app-server-control/app-server-control.sock` (a symlink
into `/tmp/codex-daemon-0/<hash>`) passes both connections under that rule.

The construction check is **method-name level by design**: the generated method
lists are the stable part of the surface, while payload shapes are consumed by
the adapter's typed decoders in the read-model and approval tickets (#879,
#882), where a changed field fails as a named decode error on the read that
uses it — never silently. The measured shape diff above (additive only across
the pin-to-running delta) is what makes probing at that level honest rather
than a guess.

One capability is finer than a method name and is read from the response
schemas: whether an approval method advertises a session-scoped decision
(`acceptForSession` in the modern pair, `approved_for_session` in the legacy
pair). A fixed table would offer the card's "always" control to a daemon that
kept the method but dropped the variant, and the reply would then be a decision
the server never advertised (#882's re-review). The read is fail-safe — a
response file the generated directory does not carry advertises nothing — so a
partial schema generator can only remove a control, never add one.

## Amendment (ticket #883): the doctor turn probe is default-on

Decision 3's diagnosis needs the provider-turn evidence, and the amendment above
records why: a turn with no usable credential is accepted and then fails as a
retryable `error`, so a login-shaped check would pass while every turn died. A
plain `cola doctor` therefore runs the real-turn probe for the active Codex
client by default. The user invoked the diagnostic explicitly, so the probe's
write is within its contract: it reserves an exclusive scratch directory, starts
a named thread and a minimal turn on the shared daemon, interrupts the turn
while it is live, and deletes the thread. The turn is bounded by a **60-second**
budget (`TURN_PROBE_TIMEOUT`) — a minimal turn still crosses the configured
provider, so the bound exists to fail a wedged one with a named timeout rather
than to hurry a slow one. That is still a write on a daemon cola does not own
and may spend the user's provider credit, so the check text names the side
effect and `--no-turn-check` is the explicit opt-out. `--check-turn` remains
accepted as a hidden no-op alias for scripts written before the default flipped;
the single safety setting set for both the report and the probe lives in one
helper (`attach_for_diagnostics`), so a future change cannot make the diagnostic
start a daemon.

## Amendment (ticket #953): the contract pin is `0.166.0`

The adapter's declared contract moves from `0.163.0` to `codex-cli 0.166.0`.
The checked-in lists are `docs/reference/codex-app-server-0.166.0.md`,
regenerated from the pinned binary with `codex app-server generate-json-schema
--out <dir>`; the `0.163.0` reference is deleted. `0.163.0` is therefore
**older than the pin** and is refused through the existing rule (the named
`OlderServer { pinned: "0.166.0", found: … }` error), never guessed at. The rule
is ordering-based, so `0.164.0` and `0.165.0` are refused by the same path.
tier-1 regenerates this surface from the live binary and compares it with the
checked-in file, so the reference is evidence rather than a document checked
against itself.

Diff against the `0.163.0` reference (measured on the 0.166.0 binary):

- client→server methods: **106**, one added — `feedback/save`;
- notifications: **85**, unchanged;
- server→client requests: **10**, unchanged;
- every `REQUIRED_CLIENT_METHODS`, `REQUIRED_NOTIFICATIONS` and
  `REQUIRED_SERVER_REQUESTS` entry is still served, and no required payload
  shape was removed;
- the four approval response schemas still advertise their session decision
  (`acceptForSession` / `approved_for_session`).

The pin's evidence is the two live tiers, both `#[ignore]`-gated and advisory
while they stabilise. **tier-1** runs on Ubuntu, macOS and Windows: the real
pinned binary starts `app-server --listen unix://<short path>` in an isolated
`CODEX_HOME` whose control path fits the Windows 108-byte limit, a scripted
Responses provider answers the model protocol, and cola attaches through
`codex app-server proxy`. The suite asserts a `thread/start`, a completed turn
carrying the scripted text, an approval request answered by a second cola
client subscribed with `thread/resume`, and a `turn/interrupt` that ends the
turn as `interrupted`. **tier-2** runs on Ubuntu and macOS against the real
managed daemon: `daemon start`, discovery through the control socket's
symlink, the `daemon version` report (`cliVersion` and `appServerVersion`
both 0.166.0), `start_server = "never"` refusing a start, `auto` starting one,
and `restart_server` answering `RestartOutcome::NotOwned`.

Both jobs mirror Live V1/V2: only the steps are gated by `classify` (and the
`release/*` skip), so the job name always reports, and neither may exceed the
Live V1/V2 20-minute bound.

## Amendment (ticket #954): open question 4 by platform

Open question 4 asked whether the daemon's implicit attachment works on every
supported platform, and how discovery behaves when `CODEX_HOME` is relocated.
Measured against `codex-cli 0.166.0` and the landed tickets #949–#953, the
answer splits by platform and by what was actually measured:

- **Linux and macOS:** attachment, discovery and the managed daemon are
  exercised by the advisory live suites, and each platform's prover can promote
  a client to attached;
- **Windows:** the transport and a standalone `codex app-server` attachment are
  exercised by the advisory live tier. The **managed daemon's Windows
  attachment is unmeasured**, and no prover can promote a Windows client.

The live suites are advisory, not required checks. A docs-only change classifies
`heavy=false` and skips their steps, so a green advisory check can be a skipped
job rather than an observed run. Promotion to a required check is tracked by
#953 under spec #948.

- **one transport on every platform.** cola spawns
  `codex app-server proxy --sock <control socket>` and runs the existing
  WebSocket handshake over the child's stdin/stdout. The direct
  `UnixStream` transport and `CodexError::UnsupportedPlatform` are gone (#949).
  The proxy child is cola's, so it is killed and reaped when the last
  `RpcConnection` drops; the daemon is not cola's, so its exit is the
  connection's `Closed` and nothing about it is reaped;
- **relocation is the control-socket path, not a second store.** The control
  socket is `$CODEX_HOME/app-server-control/app-server-control.sock` on every
  platform. On Unix it is normally a symlink into
  `/tmp/codex-daemon-<uid>/<hash>`; on Windows the published path **is** the
  AF_UNIX socket and must fit `sun_path`'s 108 bytes including the terminating
  NUL. Discovery hands the path verbatim to the proxy (the OS follows the link
  at connect time) and identifies the object from file metadata — `(device,
  inode)` on Unix, following the symlink; `(volume serial number, file index)`
  on Windows. A control socket that moves once between discovery and the dial
  is re-discovered and then re-attached (`ATTACH_ATTEMPTS = 2`); only a path
  that moves on both attempts ends the pass as `Attempt::Raced`, never an
  attachment to a daemon the path no longer names (#950);
- **Linux: attachment and proof.** The Linux prover pairs the client's open
  socket inodes (`/proc/<pid>/fd`) with the daemon's via `ss -x -e`. A missing
  `/proc`, `ss` or descriptor table leaves the client **unverified**. The
  advisory tier-2 suite exercises the real managed daemon (`daemon start`,
  discovery through the symlink, `daemon version`, the `start_server` policy
  and the restart refusal); it is not a required check yet;
- **macOS: attachment and proof.** The macOS prover pairs a client socket's
  `->0x<addr>` against a daemon socket bound to the control-socket path
  (`0x<addr>`) through `lsof -U -a -p <pid>`; the bind spelling is canonicalised
  when the verbatim and published spellings differ. A missing, denied, failing
  or timed-out `lsof`, or no pairing, leaves the client **unverified** — the
  same shape as a Linux host without `ss`. The advisory tier-1 and tier-2
  suites are wired for macOS; neither is a required check yet;
- **Windows: transport and standalone app-server, not the managed daemon.** The
  advisory tier-1 suite starts the real pinned binary with
  `app-server --listen unix://<short path>` — a standalone server, not the
  managed daemon — attaches cola through the proxy, and completes a turn, an
  approval round-trip answered by a second client subscribed with
  `thread/resume`, and an interrupt. The managed daemon cannot start on a
  hosted Windows runner because its token is elevated, so the managed daemon's
  Windows attachment has no measurement and tier-2 excludes Windows. cola has
  no descriptor or peer-address prover there: a client's process identity (pid,
  binary, `--remote` argv) is visible, but argv alone is never promoted. The
  unproven state is `UserClient.state == UserClientState::Unverified` with the
  reason in `UserClient.detail`; `ClientReport.state` is cola's own attachment
  and is `Attached` when the proxy dial succeeds. That split is what the user
  guide's Windows attachment row means by **not hard-proven**.

On Windows, a daemon start has three doctor checks. A fourth constraint applies
when Codex runs inside WSL. Most failures are silent at the TUI level because
the TUI falls back to an embedded server; in `cola doctor` an elevated token is
a ❌ and an unreadable elevation a ⚠️.

1. a **non-elevated process token**, since shared clients must not inherit
   administrator privileges; start cola from a non-elevated terminal;
2. a launcher that permits a **breakaway** child; `cargo run` and an outer job
   object do not;
3. a **short `CODEX_HOME`**, so the control-socket path fits the 108-byte
   AF_UNIX budget including the terminating NUL before anything is spawned;
4. **A `CODEX_HOME` on a Windows-mounted (DrvFS, `/mnt/c/...`) filesystem is
   refused when Codex runs inside WSL** (`9p`, mount type `0x01021997`) — this
   affects Windows users who run Codex under WSL, and is not a Windows-host
   precondition.

That fourth item corrects #948's shorthand "a WSL DrvFS home is refused":
upstream's `uses_wsl_drvfs` is `#[cfg(target_os = "linux")]` and tests the
mount's `f_type == 0x01021997`, so it is a WSL-side (Linux) check, not a
Windows-host check.

`cola doctor` reports the first three as rows with one actionable instruction
each, and refuses a start before the launcher runs (#952, #950); the fourth is
Codex's own refusal.

**Proxy is not ownership of the agent's stdio.** The Peer Attachment rule
(decision 1) is that cola is one client of a runtime-owned session; owning
the agent's stdin/stdout would make cola its sole client instead.
`codex app-server proxy` does not do that: cola owns the **proxy child's**
stdin/stdout — a pipe into a relay — while the app-server is a separate process
(the managed daemon, or a listener cola did not start) that cola neither spawned
nor reaps. That is the Peer Attachment shape: cola is one more connection to a
runtime someone else owns. Spelling the app-server's own stdin/stdout as the
transport (Codex's `stdio://`, or ACP's model of spawning the agent as the
client's subprocess) is the **Owned Runtime** shape and remains deliberately
unsupported. `ws://` describes neither: it stays an `Endpoint` that only tests
construct, because the daemon serves no WebSocket or TCP listener and offers no
config key to create one.
