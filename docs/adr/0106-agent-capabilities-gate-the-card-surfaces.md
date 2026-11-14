# An Agent Client declares its capabilities; surfaces degrade, never guess

## Context

Every surface cola can show is optional, and the **Agent Client** is the only one
who knows which are available. Today that knowledge is spread across
per-subsystem methods and implicit defaults:

- `background_tasks_supported()` answers `false` for both Codex adapters and for
  OpenCode V1 (`src/codex/backend.rs:3191`, `src/opencode/strategy.rs:260`) and
  `true` for V2 (`src/opencode/v2/mod.rs:766`). The ledger simply never appears
  on the false branch — correct, but expressed nowhere the card can read.
- `session_diff` returns `Option`; compaction, questions, models, agents and
  MCP each have their own shape of "may be absent".
- The capability differs *within* one Agent Client: Codex reports child-agent
  **activity** but has no background launches, so its runs cannot appear in a
  ledger and its activity items become standalone panels.

The absence is usually treated correctly and always accidentally. The card's
"when does which surface show" rules exist, but spread over ADR-0038 (inline vs
standalone request blocks), ADR-0043 (splits and supplements), ADR-0059/0066
(yield and in-place resume), ADR-0063 (restart stamps), ADR-0074 (one card per
request), ADR-0086 (Host terminal), ADR-0100 (the one-shot Session Card) and
the child-session permission re-home. No one document says what a new Agent
Client must answer, so a surface's absence is discovered when someone notices it.

ACP's shape is the opposite and worth copying: capabilities are negotiated once
at `initialize`, and each side degrades for what the other did not advertise.
The Agent Client states what it supports; nobody probes.

## Decision

**1. One capability read per attachment.** The **set of axes** is this ADR's;
the struct's layout is spec #1057's. The axes are: background runs, child-agent
mode, plan, compaction, usage, questions, modes, session/turn diffs, and MCP
server statuses. `reports_live_runs()` is derived — the ledger lists **runs**,
not only background work, so a turn-scoped child counts:

```text
reports_live_runs = background_runs || child_agents != None
```

`Backend::capabilities()` returns it. It is a property of the **attachment**, not
of a Session: a reattach (a Codex daemon restart, an OpenCode generation switch)
may change it, and a value read from a stale connection is not used to render a
new one.

**The permission always-scope is not a capability.** Whether 「始终允许」 exists
is a property of the **request**: Codex advertises `acceptForSession` per
request method (`src/codex/approvals.rs:776`) and the neutral
`PermissionRequest::always_supported` carries it per request
(`src/backend/types.rs:827`), exactly ADR-0018's per-type rule. An
attachment-wide flag would show the button where the backend declines it or hide
it where it is valid, so the card reads the request's own field.

**2. Every optional surface gates on it.**

| Surface | Gate | When absent |
|---|---|---|
| Live-run ledger | `reports_live_runs()` | no section |
| Panel liveness line | `child_agents != None` **and** a run with activity | plain panel |
| Plan section | `plan != None` | no section |
| Compaction row | `compaction` | the part is not a row |
| Turn footer usage | `usage != None` | no footer figure |
| 「始终允许」 on a permission | the **request's** `always_supported` (ADR-0018) | one-time options only |
| Question card / inline block | `questions != None` | no question surface |
| Location Form | `questions` includes a location scope | no directory-owned form |
| Agent picker | `modes` | `/agent` reports it as unsupported |
| Turn-diff index | `diffs` | no index line, no button |
| `/mcp` card | `mcp` | the command reports it as unsupported |

An absent surface is **omitted**, never rendered empty and never guessed from a
failed read. Absence is recorded once at attach (one log line naming the Agent
Client and the missing surfaces), and each Agent Client has a test that asserts
each absent surface stays absent.

**3. The event→surface table is part of this decision.** It is the reference a
new surface is checked against:

| Event | Surface |
|---|---|
| A prompt starts a Turn | streaming Turn card, a reply to the message |
| Content fills the card | continuation card in the same Card Chain |
| A message arrives mid-Turn | Supplement card (steer) |
| The Turn ends with live runs | the same card yields (`Waiting`) |
| A run's completion resumes work | the same card (`Resuming`) |
| A Permission/Question on a live mapped Turn | inline block |
| … with no live card | standalone request card, re-adoptable |
| … from a child session | re-homed to the parent's card |
| … already resolved elsewhere | resolved-elsewhere card |
| `/card` on a Session nobody renders | one-shot Session Card, no controls |
| Another Agent Client's user message | notify card |
| A Host command | Terminal Card, outside every Chain |
| A Host file/directory | Viewer/browse card, outside every Chain |
| A compaction | row on the Turn card at its server position |
| A restart finds a live card | `Restarted` stamp, not a new card |

**4. A call that addresses an existing run is a receipt, not a panel.** Codex's
`sendInput`/`wait`/`closeAgent` carry no new work: they belong to an existing
`AgentRun` (ADR-0105) and render as a run receipt. A `wait` that names no target
addresses the Turn's live runs as a set, so its receipt is turn-level
(ADR-0105's addressing rule). A call that **launches** a run keeps its panel as
the record of the request. The raw call stays in the transcript either way. This
is a surface rule, not a read-model one: the call is still a `ToolCall`, and
`kind`/`detail` still describe it.

**5. Existing per-subsystem methods fold into the read.**
`background_tasks_supported` and its siblings are not a second source of truth.
They become capability reads (thin delegations during the migration, removed
when no caller remains). Landing this ADR changes no rendering by itself.

## Why

- **One question instead of nine.** A new surface asks `capabilities()`; a new
  Agent Client answers one struct.
- **Degradation becomes testable.** "This Agent Client has no ledger" is now an
  assertion, not a consequence observed in production.
- **It keeps the card free of Agent Client identity.** The card never asks "is
  this Codex?"; it asks whether a surface exists.
- **It matches what an Agent Client already is.** Codex's own `initialize`
  result advertises its capabilities; cola's adapter translates that into this
  struct instead of re-deriving it per subsystem.

## Considered options

- **Keep per-subsystem methods and document the gaps.** Rejected: that is the
  current drift, and every new surface re-invents "does this Agent Client
  support X?".
- **Probe at runtime (call it and see if it fails).** Rejected: expensive,
  timing-dependent, and it turns a design fact into a failed request.
- **Put capabilities on the Session.** Rejected: they are the attachment's, and
  a Session can outlive a reattach.
- **Always render the section and let it say "unsupported".** Rejected: it
  reports an empty surface as a fact about the Session rather than about the
  Agent Client, and it costs a card element for every absence.
- **Fold the permission always-scope into the capability read.** Rejected: it is
  per request (ADR-0018), and an attachment-wide flag cannot represent a
  protocol that advertises the scope per method.

## Consequences

- **A capability lie is visible.** An Agent Client that claims
  `background_runs` but never reports a launch renders no ledger rows; that is a
  clean, diagnosable failure, not a card that silently invents one.
- **The ledger's concept widens, and its gate says so.** Once runs are one fact
  (ADR-0105), the ledger lists **live runs**, not only "background tasks", so it
  gates on `reports_live_runs()`. Codex and OpenCode V1 can gain the section even
  though neither has a background concept, because their child agents are runs.
  Its card title and copy are UI copy, so their wording is ADR-0107's, not this
  one's.
- **A reattach can remove a surface mid-chain.** The card already on screen is a
  snapshot and is not rewritten; the next render under the new attachment simply
  omits the section.
- **New surfaces follow one shape:** add the axis, gate the render, add the
  absent-surface test.
- **Multi-client routing still needs its own decision.** Per-chat capabilities
  belong to a later ADR (ADR-0090 defers multi-client routing).

## Open

- Whether a capability read should include a `reason` string for `/status`
  ("Codex V2: no background runs") rather than only a boolean. Deferred until
  `/status` grows a per-Agent-Client row; the boolean is what the card needs
  today.
