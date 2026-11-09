# Location forms belong to a directory Chat/Topic

## Context

OpenCode raises an MCP elicitation as a **Location Form**: the MCP server belongs
to a directory, so the request carries no Session. Upstream marks this with the
`global` session sentinel — `GLOBAL_ELICITATION_SESSION_ID` in OpenCode's
`packages/core/src/mcp/index.ts`, whose comment says it exists because a
location server cannot attribute a request to a persisted session row. The
in-progress session-owned MCP work gives a session-owned server's elicitations
that server's session id; a location-owned server keeps the sentinel. A location
server is the default `[mcp]` configuration, so the session-less shape is
permanent, not a transition to wait out.

Sharing a connection does not change ownership. A **shared** connection — the
`pool: true` option requests it, and only an MCP 2026-07-28 connection is shared
(`protocol` defaults to `auto` when `pool` is set; an older or refused server
keeps a private connection) — serves several directories but has no roots: an
elicitation caused by a tool call appears in the **calling** directory, while one
with no caller is delivered to every holder as its own form, first answer wins
and the rest are cancelled. The caller case is still directory-owned; the
no-caller case can list one logical request in several directories. (OpenCode
v2.2.6; upstream PR anomalyco/opencode#61185.)

#489 made these forms terminal: one WARN, no session read, no surface. The MCP
call stays blocked and nobody sees why. A form has no TTL — it lives in the
server's in-memory form cache until answered, cancelled, its connection aborts,
or the server process exits — so silence can last indefinitely.

cola sweeps forms only for directories in the SessionStore, so every visible
Location Form already has at least one mapped Chat/Topic to deliver into. But
the form is owned by the directory, not by any one Session: it carries no
requester and no Turn, and whoever answers speaks for everyone using that
directory. That is the fact every decision below hangs off.

## Decision

**Ownership.** The neutral request model carries an owner of `Session(id)` or
`Location`; the OpenCode V2 adapter maps the literal `global` sentinel to
`Location`. Only a form whose metadata marks it `mcp-elicitation` takes this
path (OpenCode writes `metadata.kind = "mcp-elicitation"` together with the
asking server's name). An unmarked session-less form, or a permission with no
session, keeps #489's one-WARN-and-skip: cola has no evidence that surfacing it
would mean anything.

**Delivery.** A Location Form is pushed once, as a standalone card, into the
Chat/Topic of the directory's newest mapped Session in the SessionStore's
activation order — the newest mapped entry first, filtered by directory and
Agent Client; this is the store's local activation order, not `/switch`'s
server-time sort — using that Chat/Topic's topic anchor when it is a topic. If
the mapped Session is in a Topic and its anchor cannot be resolved, the push is
deferred — no card, no suppression — and retried on the next sweep; a Location
Form never falls back to posting in the Chat lobby.
The card names the asking MCP server and its message, and shows the directory,
so the answerer knows what is being asked and whom the answer commits; the V2
adapter carries `metadata.server` and `metadata.message` into the neutral
request (OpenCode's `packages/core/src/mcp/index.ts` writes those keys; today's
`RawForm` in `src/opencode/v2/wire.rs` drops metadata). The new neutral fields
stay optional, so session-backed forms keep the normal Question path. A send
that fails definitively is not recorded as pushed, so the next complete sweep
with a mapped Chat/Topic retries it; a timed-out send can duplicate the card
exactly as every standalone send can today, and this record does not claim to
fix that ambiguity. If the
mapping disappears between listing and delivery, nothing is sent and nothing is
suppressed. A form raised on a shared connection with no caller is created once
per holder with its own form id, so cola treats each listing as its own request
and that rare case can produce one card per holder; answering one cancels the
others upstream, and the losing cards go stale through the existing
resolved-elsewhere path. That path's copy names “another client”, which is
imprecise when cola's own card in another directory won: the wire carries no
fan-out marker, so the losing card cannot say that, and the generic copy is
accepted as a known limitation. The same copy also covers a no-answer ending —
a connection abort or a server exit — which a session-less form cannot
distinguish from another client's decision. No cross-location dedupe is
attempted. The card never rides an inline block, never enters
`pending_sessions`, and never drives the Instant Reminder: that surface needs a
Turn's generation to own and clear its pin, and a Location Form has none. It
does take a **Message Pin** under the same `instant_reminder` opt-in, because a
pin is keyed by message. The pin joins the same Message Pins registry as every
other wait (ADR-0043): resolving, cancelling, dismissing, or staling the card
removes it from the waiting set and releases the pin.

**Dismissal.** 「稍后」 withdraws the card — and releases its Message Pin —
without declining the request. The form stays pending and `/mcp` can reopen it
as a fresh card; reopening sends the fresh card first and commits the cleared
dismissal only after the send lands, so a failed send leaves the form dismissed
and `/mcp` can retry rather than leaving it unsuppressed with no card. The
reopen serializes with 「稍后」 on the per-form write lock, and the clear is
conditional on the dismissal generation read before the send, so a dismissal
that lands while the send is in flight survives. A landed reopen rewrites the
standalone-surface record, so the two records never disagree about what the chat
currently shows. A clear write that fails is rolled back in memory and the
fresh card is withdrawn again; the form stays dismissed and retryable. Only an
explicit 「取消」 declines it, and cola never auto-declines on a timer: turning a
hesitation into a failed tool call is worse than waiting.

**Persistence.** "Pushed" and "dismissed" are recorded in the client-scoped
surface file (ADR-0038, ADR-0091), so a restart does not re-push a form the Host
already deferred. The record is pruned only when a complete sweep can speak for
the request — the #130 rule: a failed list never reads as gone. A restart that
finds the card still live re-adopts it through ADR-0038 rather than posting a
second; one that finds it withdrawn keeps it withdrawn until `/mcp` reopens it.
That withdrawal-before-restart rule is ADR-0038's 2026-11-09 amendment, the one
explicit exception to its "every still-pending request keeps one live surface".
The pushed/dismissed record and the standalone surface are committed in one
mutation, so a restart cannot see one without the other.

**The dismissal is committed before the card becomes terminal.** A terminal
card (the 「稍后」 replacement has no controls) must never sit over a record that
still says `Pushed`: that state is the one that strands a still-pending form,
because `/mcp` reads the record and offers no reopen. 「稍后」 therefore writes
the dismissal first and **checks that the write is durable**, then PATCHes the
card. The two failure directions are then both recoverable:

- a failed **dismissal write** leaves the live, answerable card and the
  previously written pushed record exactly as they were; the click reports that
  it did not land, and a later sweep or `/mcp` still sees a form that was never
  withdrawn;
- a failed or timed-out **card PATCH** after the durable dismissal leaves a
  live card with a dismissed record. A definite error is answered from that
  live card; an ambiguous timeout may have landed, leaving a terminal card —
  either way the record says dismissed, so `/mcp` reopens it instead of the form
  being stranded.

The mirror remains best-effort (ADR-0038) for the **push** and for every other
surface: a failed push write can cost one duplicate card after a restart, and a
timed-out **send** may duplicate exactly as every standalone send can today.
Neither is claimed atomic, and neither changes the dismissal ordering above.
`/mcp` treats a dismissed record as reopenable, and the sweep treats it as
authoritative suppression until a reopen clears it.

**Partial answer state is persisted, and the recovered card is not repainted.**
A Location Form's card-action state — each question's **clicked option wire
values**, whether the slot is final, plus the open question — is written into
the same client-scoped record (`partial`, serde-default, no format-version bump)
whenever a card action changes it. Text the Host typed is deliberately not
persisted: an unsubmitted draft, a custom-answer entry and a numeric input all
stay client-side (the card-repaints rule). A slot that carried such a value is
persisted as a live selection of only its clicked options (or nothing), never as
a final answer, so the restore can never submit an empty list in place of text it
could not write. On restart `readopt_location_form` still only **reads** the
live card: a repaint would wipe an unanswered typed field (ADR-0038's
card-repaints rule, ADR-0091). Instead, `remember_question` restores the
persisted paint into `QuestionState`, so the card's visible clicked selections
and the answer its next confirm/submit sends agree — the invariant the restart
test pins. `/mcp`'s reopen after a restart builds the fresh card from the same
restored state, so a reselect is not needed for clicked options; a typed custom
answer may have to be re-entered. The write shares the surface mirror's
best-effort nature (ADR-0038): a failed partial write can lose the selection
across a restart, and the recovered live card then keeps markers the new process
cannot toggle. Consistency after a **successful** write is the guarantee;
surviving a lost mirror write is not claimed.

Two accepted residuals, recorded rather than solved:

- a definite PATCH failure after a durable dismissal leaves the old card live
  while `/mcp` can reopen the form. The reopen sends and records a fresh card,
  overwriting `sent_cards`, so the old live card is no longer tracked (it stays
  answerable until the form resolves). This is a duplicate/orphan card, not a
  stranded form, and the same shape a timed-out-but-landed PATCH leaves.
- `/mcp`'s reopen performs its send inside the card action. The WS loop detaches
  an action that exceeds the 2 s ack budget and drops the late result, so the
  `/mcp` card's repaint may not land; the fresh Location Form card and the
  cleared dismissal are already committed, and the next `/mcp` shows the new
  state. This is the bounded-detached behaviour every ack-budget-exceeding
  action has (ADR-0038's 2026-09-18 update).

**Pull surface.** `/mcp` is the read-only counterpart: the current directory's
MCP server statuses plus **only** Location Forms (session-backed forms already
own their Session cards). The statuses belong here because a form names a server
the Host may have to fix — `needs_auth` (no usable credential) in particular —
before the form can be answered. `/mcp` can reopen a dismissed form. The
capability is an adapter method. OpenCode V1 publishes a status read (`/mcp`,
replaced by `/api/mcp*` in V2; `docs/research/opencode-v2/r2-api-protocol-delta.md`)
but has no elicitation-form surface; Codex publishes `mcpServerStatus/list` but
has no location-owned forms. Neither offers the full surface — statuses plus
Location Forms — so both answer "unavailable" rather than shipping a
status-only variant.

**Answering.** Any **Principal** admitted by the Access List may answer in the
target Chat/Topic, matching every other Permission/Question card; the default
single-Host model means that is normally the Host. The card states that the
answer commits the whole directory, and may also commit other directories
sharing the MCP connection: the wire carries no caller marker, so cola cannot
tell a caller-less pooled form from a caller-attributed one and puts that caveat
on every Location Form card. Reply and cancel use the session route with the
literal `global` session id — `POST /api/session/global/form/{id}/reply` and
`DELETE /api/session/global/form/{id}` — plus `location[directory]`, where the
directory is the one whose form list produced the request.

The two **distinguished** failure statuses are not interchangeable; every other
failure is retryable too. A **409** (`FormAlreadySettledError`) is benign — the
form settled, and the card takes the handled path. A **404**
(`FormNotFoundError`) on a Location Form is **always a retryable refusal**,
because the wire carries no marker that distinguishes a wrong location from a
cancelled pooled loser: cola keeps the standalone surface and the
pushed/dismissed record, tells the user the answer did not land, and lets the
next complete sweep withdraw the card once the form is genuinely gone. A
wrong-location 404 is never read as a successful answer, and it never deletes
the only surface the waiting form has. Any other outcome — a 401, 408, 5xx, or a
transport timeout — is a retryable error and never a handled one. A losing copy
on a pooled no-caller request is already cancelled upstream, so a late reply to
it takes the same retryable 404 path and the next complete sweep withdraws the
card.

**Shape.** Both elicitation modes are delivered. An `external` field renders as
its link plus an acknowledgement. A secret field is marked uncollectable with
the `SecretInput` reason — cola's variant for a request it cannot collect on a
shared card — and can only be declined: the shared card is the wrong place for a
password or token. The decoding stays defensive: today's upstream form schema
has no `password` format, so this guard exists for the shape the MCP protocol
permits, not for a case in the current wire.

## Consequences

- A new suppression record in the surface file, scoped by Agent Client. The
  file's format and version are ADR-0091's; a file from an older build loads
  with an empty suppression, so no migration is needed. The record and the
  standalone surface are written in one mutation.
- The same record carries the form's partial answer paint (clicked option wire
  values, final slots and the open question; never text the Host typed) so a
  restart restores what the un-repainted live card shows. The field is
  serde-default on the record, so older files load without it and need no
  format-version bump; a card action that changes the state writes it through.
- A Location Form reply's 404 is a retryable refusal, not the generic
  already-handled path: the surface and record survive, and only a complete
  sweep withdraws a form that is genuinely gone. A 409 keeps the benign handled
  path. Every Location Form card carries the shared-directory caveat.
- The reply-error mapping splits by owner: a session-backed form keeps the
  existing 404/409-as-handled mapping, while a Location Form maps 409 to handled
  and 404 to retryable. The existing V2 form-reply tests are updated to pin the
  split.
- Both user guides gain the location-form behaviour and the `/mcp` command,
  including the shared-directory caveat and the retryable-404 rule.
- A new Backend capability method for the MCP status read. An adapter without
  the surface returns `None`, not an error — the Backend trait's rule that a
  default shape means "not applicable to this client", never a failed read
  (`src/backend/mod.rs`).
- The #489 WARN remains for every session-less request that is not a marked
  elicitation.
- A shared connection's caller-attributed elicitation stays in the calling
  directory; a no-caller one may produce one card per holder, and each losing
  card goes stale through the existing resolved-elsewhere path. No
  cross-location dedupe is attempted, and that path's “another client” copy is
  an accepted imprecision for a pooled loser and for a no-answer ending the
  session-less card cannot distinguish.
- A Session-owned elicitation — which the in-progress session-owned MCP work
  produces — takes the normal Question path unchanged; the Location path
  remains for location-owned servers.
- The issue's minimum was delivery and reply routing. `/mcp` and the durable
  dismissal are deliberate additions: one push with no durable pull surface
  would leave a dismissed form unreachable.
- Known limitation, recorded rather than solved: no requester means no
  requester-scoped authorization beyond the Access List, no requester-scoped
  reminder, and no way to attribute the answer to the Session whose tool call is
  actually blocked.

## Considered options

- **Wait for session ownership.** Rejected: location-owned servers keep the
  sentinel permanently, so waiting never ends.
- **Fan out to every mapped Chat/Topic, or require a configured admin chat.**
  Rejected: the answer commits the whole directory, so one deterministic target
  with no new configuration is enough; fan-out multiplies cards that all speak
  for the same connection.
- **Treat 「稍后」 as a decline, or auto-decline on a timeout.** Rejected: both
  turn "not now" into a failed MCP call.
- **Aggregate every pending Location Form into one card.** Rejected: it breaks
  the per-request surface and settlement machinery, and ADR-0093 allows only one
  open typed input per card anyway.

## Update (2026-11-09): the secret-input refusal applies to every elicitation form

The **Shape** decision's secret guard is intentionally owner-agnostic. A V2
form whose field names a secret format makes the whole request uncollectable
with `UncollectableReason::SecretInput` whether the form is Session-owned or
Location-owned: the Feishu card is a shared surface either way, and the value
would otherwise be visible in the card's answered-state markdown and its
Interaction Receipt. This matches the Codex path's own rule (`src/codex/approvals.rs`),
which applies the refusal inside its ordinary Question path rather than
treating it as a Location-only concern. The renderer enforces the same rule
independently of the adapter (`question_elements` refuses any `QuestionInfo`
whose `secret` marker is set), so a caller that forgets the request-level
marker still cannot render the field as an input.

This is the one explicit exception to “a Session-owned elicitation takes the
normal Question path unchanged”: a session-backed form keeps its ordinary
Question route, card and reminder — only one carrying a secret-format
(`password`, and defensively `secret`) field becomes decline-only. The rest of
the ownership split is unchanged; the exception narrows collection, never
routing.
