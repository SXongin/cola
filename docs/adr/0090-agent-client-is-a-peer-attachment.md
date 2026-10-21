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
pickers. Approval requests raise Permission cards; `tool/requestUserInput` and
elicitation raise Question cards. `thread/status/changed` feeds Session Sync.

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

These are answered by a live spike before the adapter merges, and each one can
change the shape of a card path:

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
