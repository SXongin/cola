# Location forms belong to a directory conversation

## Context

OpenCode raises an MCP elicitation as a **Location Form**: the MCP server belongs
to a directory, so the request carries no Session. Upstream marks this with the
`global` session sentinel and calls it temporary, but the branch that adds
session-owned MCP servers keeps the sentinel for location-owned servers
(`entry.owner.type === "session" ? entry.owner.id : GLOBAL_ELICITATION_SESSION_ID`).
A location server is the default `[mcp]` configuration, so the session-less
shape is permanent, not a transition to wait out.

#489 made these forms terminal: one WARN, no session read, no surface. The MCP
call stays blocked and nobody sees why. A form has no TTL — it lives in the
server's in-memory form cache until answered, cancelled, its connection aborts,
or the server process exits — so silence can last indefinitely.

cola sweeps forms only for directories in the SessionStore, so every visible
Location Form already has at least one mapped conversation to deliver into. But
the form is owned by the directory, not by any one Session: it carries no
requester and no Turn, and whoever answers speaks for everyone using that
directory. That is the fact every decision below hangs off.

## Decision

**Ownership.** The neutral request model carries an owner of `Session(id)` or
`Location`; the OpenCode V2 adapter maps the literal `global` sentinel to
`Location`. Only a form marked `metadata.kind == "mcp-elicitation"` takes this
path — an unmarked session-less form, or a permission with no session, keeps
#489's one-WARN-and-skip, because cola has no evidence that surfacing it would
mean anything.

**Delivery.** A Location Form is pushed once, as a standalone card, into the
conversation of the directory's **most recently active mapped Session** (the
topic anchor when that conversation is a topic). It never rides an inline block,
never enters `pending_sessions`, and never drives the Instant Reminder — it has
no Turn to own those. It does take a **Message Pin** under the same
`instant_reminder` opt-in, because a pin is keyed by message.

**Dismissal.** 「稍后」 withdraws the card without declining the request. The
form stays pending and `/mcp` can reopen it as a fresh card. Only an explicit
「取消」 declines it, and cola never auto-declines on a timer: turning a
hesitation into a failed tool call is worse than waiting.

**Persistence.** "Pushed" and "dismissed" are recorded in the client-scoped
surface file, so a cola restart does not re-push a form the Host already
deferred; the record is cleared when the request settles. A restart that finds
the card still live re-adopts it through ADR-0038 rather than posting a second.

**Pull surface.** `/mcp` is the read-only counterpart: the current directory's
MCP server statuses plus **only** Location Forms (session-backed forms already
own their Session cards). It can reopen a dismissed form. The capability is an
adapter method; OpenCode V1 and Codex answer "unavailable", because neither has
a location-scoped form surface (Codex elicitations are thread-owned).

**Answering.** Anyone in the target Chat/Topic may answer, matching every other
Permission/Question card; the card states that the answer applies to the whole
directory. Reply and cancel use the `global` route with
`location[directory]`, where the directory is the one that listed the form — a
reply sent to the wrong location is refused upstream while the form keeps
waiting.

**Shape.** Both elicitation modes are delivered. An `external` field renders as
its link plus an acknowledgement. A secret field is `Uncollectable` and only
declined — the shared card is the wrong place for a password or token. The
decoding stays defensive: today's upstream form schema has no `password`
format, so this guard exists for the shape the MCP protocol permits, not for a
case in the current wire.

## Consequences

- A new suppression record in the surfaces file, scoped by Agent Client, pruned
  when its request leaves the pending list.
- A new Backend capability method for the MCP status read; adapters without the
  surface return `None` rather than an error, per ADR-0090's default rule.
- The #489 WARN remains for every session-less request that is not a marked
  elicitation.
- When upstream gives session-owned elicitations a real session id, they take
  the normal Question path unchanged; the Location path remains for
  location-owned servers.
- Known limitation, recorded rather than solved: no requester means no per-user
  authorization, no requester-scoped reminder, and no way to attribute the
  answer to the Session whose tool call is actually blocked.

## Considered options

- **Wait for session ownership.** Rejected: location-owned servers keep the
  sentinel permanently, so waiting never ends.
- **Fan out to every mapped conversation, or require a configured admin chat.**
  Rejected: the answer commits the whole directory, so one deterministic target
  with no new configuration is enough; fan-out multiplies cards that all speak
  for the same connection.
- **Treat 「稍后」 as a decline, or auto-decline on a timeout.** Rejected: both
  turn "not now" into a failed MCP call.
- **Aggregate every pending Location Form into one card.** Rejected: it breaks
  the per-request surface and settlement machinery, and
  [ADR-0093](0093-typed-card-inputs-sit-at-the-cards-end.md) allows only one
  open typed input per card anyway.
