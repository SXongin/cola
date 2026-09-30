# Adopt the V2 interaction model: idle-bounded Executions, synthesized Wakes, and the waiting card

> **Amended by ADR-0060**: the waiting card is no longer strictly frozen — the
> **Background Task Ledger** updates it in place, and a last task retiring with
> nothing to render settles it ✅ there (the "the card stops updating" half of
> this ADR is narrowed; everything else stands).
>
> **Amended by ADR-0061**: "card state stays in memory" is narrowed for the
> restart path (#424): the newest Wake a card has already announced persists as
> a per-session **Wake Watermark**, and the no-chain Fresh continuation posts
> only for a Wake strictly newer than it. Live cards still die with the
> process, and the continuation still scopes itself at the Wake.
>
> **Amended by ADR-0062**: the routing key is cola's own live ownership, not
> the Backend's status read — `/api/session/active` can report live with no
> runner left to promote the message (#428). A message whose Session only
> *reads* live while cola owns no card chain starts a new Turn, and a submit
> the Session never promoted ends 「未被接收」, never ✅. The Follow's guard
> and every #409 case stand.
>
> **Amended by ADR-0063**: the live card's identity and anchor are durable; a
> restart reaps the card it orphaned instead of leaving it frozen. "Card state
> stays in memory" is narrowed accordingly, as with ADR-0061.

## Context

OpenCode 2 changed what a "turn" is. V1 had no formal turn boundary — a
per-session runner absorbed mid-run messages by re-reading the transcript and
checking parent ids — and every out-of-band continuation it generated was a
synthetic `role:"user"` message, which cola's external-message sync read as an
External Message (visible, if noisily). V2 records a durable `idle` message per
busy period and defines the turn as everything since the previous marker,
including prompts steered in while busy; continuations are first-class
`synthetic` messages that can wake a session with no user message; and
still-running backgrounded work stays readable on the assistant tool part
(`state.metadata.status=running` on a completed `shell`/`subagent` call,
retired by its synthetic completion) — the exact predicate the official app
(v2.0.18) uses to show "N tasks running". The tool input's request-time
`background=true` flag is not part of that predicate: a foreground run the user
moves to the background (`POST /api/session/:sessionID/background`) returns the
same handle without one.

cola kept the V1 shape: a Turn is anchored on a user message, `idle` and
`synthetic` are dropped by the read model (`synthetic` decodes to
`MessageRole::Other` and every projection ignores it), and background-work
metadata is never read. Issue #403 is the consequence, not the bug: a
background-triggered continuation is invisible. When the timing aligns it is
worse — the wake's assistant content is absorbed into the previous Turn and the
card is stamped `✅ 完成` early, because completion is read from any terminal
step since the anchor.

## Decision

**cola adopts the V2 interaction model wholesale instead of patching a watcher
onto the V1-shaped one.**

- **Read model.** The neutral transcript read (`src/backend/transcript.rs`)
  types three V2 facts: an **Execution** boundary and outcome from the durable
  `idle` message (a shutdown writes none — absence is tolerated); a **Wake**
  from `synthetic`, carrying its source (`shell`, `subagent`, `restart`,
  `interrupt`) and correlation keys (`shellID`/`jobID`/`childID`); and
  **Background Tasks** derived from assistant tool parts (`state.status=completed`
  with `state.metadata.status=running`, the official app's derivation; the
  request-time `background=true` input flag is not required, since a run moved
  to the background mid-flight carries none) and retired by their Wake. V1 has
  none of these types: the new fields are empty there and behavior degrades to
  today's.

- **A Turn spans Executions and is not complete while Background Tasks are
  live.** At an Execution's idle with background work still pending, the Turn's
  card yields with 「⏳ 等待后台任务」 — a distinct disposition, not one of the
  four terminals and never `✅`. The next Wake continues the chain on a new
  continuation card, replied to the user's message (the same Card Chain split
  ADR-0043 defined for Supplements); that new message is the notification. The
  Turn's `✅` and the existing opt-in Completion Notice fire only at the true
  end: the session idle with no live Background Task. Waiting has no time
  bound (a `gh run watch` may take an hour); a long-lived task holds the Turn
  until either the next user message supersedes it — the waiting card collects
  as 「⏳ 部分完成 · 已由新消息接管」 — or the Session stops being the thread's
  Active Session — it collects as 「⏳ 已切换会话 · 后台任务仍在运行」. While the
  Session is not active, no Wake is rendered for it (ADR-0017's no-interleaving
  rule is unchanged): nothing is posted during the absence. When it becomes the
  thread's Active Session again, Session Sync's Wake pass renders the missed
  Wake on the newest card chain as a continuation card — one continuation for
  the missed work, exactly once (later passes must not re-post it). The Session
  Snapshot keeps its state/navigation role and may be suppressed by ADR-0028
  when the recent life is already in the thread; the continuation card, not the
  snapshot, carries the missed content.

  One settle decision (`SessionTranscript::settle`) serves every ending — the
  drain, the out-of-turn follow and the Wake continuation. It reads the read's
  own Executions, Wakes and Background Tasks, never a terminal step: a Turn is
  complete only at an idle read whose newest Execution boundary answers every
  placeable Wake — a Wake opens an Execution, so a Wake's content landing in a
  finalization window cannot declare the Turn complete — with no live
  Background Task, and the Turn's settled failure dominates the waiting
  disposition (a failed or stopped Turn never yields waiting). A generation
  without these facts (V1) decides as today.

- **Routing key: live Execution.** A user message arriving while an Execution
  is live is a **Supplement** — submitted without starting a competing Turn,
  the chain splitting below it. When the session is idle, including the
  background-pending window, it starts a new **Turn**. The Backend's behavior
  is identical either way (every cola V2 prompt carries `delivery:"steer"`: V2
  merges it into a live execution or starts a new one), so this is cola's card
  bookkeeping, chosen to match V2's own boundary. The inflight guard extends to
  cover the follow window, so a message during a follow merges into the
  rendering chain instead of starting a second Turn — and `busy()` no longer
  reads a followed session as idle (previously the server-yield path could reap
  the Owned Server out from under a run cola was still rendering). **This
  supersedes ADR-0043's 2026-09-24 guard-release rule** — its "the inflight
  guard is released, so the next message is a normal new Turn" and the
  rejected alternative "not releasing the guard while busy": that rejection
  assumed the 10-minute bound owned the ending, while with no total budget the
  follow owns it. ADR-0043 carries the amendment banner; its split and receipt
  machinery stands unchanged and is reused below.

- **One Session Sync pass.** The external-message flow becomes Session Sync:
  the existing per-thread pass over the Active Session keeps notifying External
  Messages and gains Wake rendering (a continuation card whose render picks up
  the new content) plus a content-diff fallback for content the card missed.
  Wake rendering applies only while the Session's newest user message is a
  Cola-Authored Message — durable authorship, so it survives a cola restart —
  and the Session is the thread's Active Session; everything else stays with
  the external-message and snapshot flows. A shell or subagent Wake whose work
  renders into a card that is ALREADY LIVE (a running Turn's drain, its
  follow, or a continuation card a later Wake resumes) gets one mechanical
  completion receipt at the Wake's own server time — `🔔 后台任务完成：<command>`
  / `🔔 子代理完成：<description>`, the label taken from the Wake text's own tag
  attribute and clipped to a short line, bare when it names none (a Wake with
  no server time gets no receipt at all: it cannot be ordered) — so the
  completion is visible without depending on the model narrating it. A Wake
  that opened a card itself is already announced by that card's 承接 line, and
  restart or interruption Wakes keep today's behavior: exactly one mark per
  Wake id, never double-marked.

- **Card state stays in memory.** A cola restart loses live cards, as today: a
  waiting card goes stale, but the Wake path is card-independent — a Wake on a
  mapped Session still posts a continuation card below the user's message.

## Considered options

- **Finalize every Execution (`✅`, with a note when background work remains).**
  Keeps `✅` on an unfinished task and splits one authorized task across
  several `✅`s. Rejected.
- **Re-open the same card on a Wake.** A card PATCH neither notifies nor bumps
  the conversation, so a wake cannot reach a user who is not staring at the
  chat — the invisibility bug would only be half fixed. Rejected.
- **A bounded grace window, or status-only detection.** Wake timing is
  unbounded (in #403's case the decisive merge arrived 13 minutes after the
  turn finalized), and `/api/session/active` is process-local and lossy across
  restarts; the durable transcript is the only authoritative source. Rejected.
- **Chain-open routing (the waiting window counts as "in flight").** One Turn's
  bookkeeping would stay open for hours and the continuation would re-carry the
  accumulated card content. Rejected in favor of V2's own idle boundary.

## Consequences

- ADR-0017's scope is unchanged; its flow grows into Session Sync. ADR-0026's
  watermark stays user-message-scoped — a Wake never moves it.
- ADR-0043's supplement split and receipt machinery is reused for Wake
  continuations, anchored on the user message when no Supplement exists; its
  guard-release rule is superseded, and ADR-0043 carries the amendment banner.
- ADR-0058's retry matrix is unchanged in mechanism: its settled/unfinished
  read stays the transcript projection of whether the submission was answered
  (`turn_for_user(..).complete`), and this ADR does not alter that projection.
  The waiting disposition is not an Error card and never offers Retry, and a
  settled failure or a sticky stop dominates the ending — a failed or stopped
  Turn never yields waiting even with live Background Tasks; a later Wake then
  continues on a new card.
- ADR-0050's Turn gains the waiting disposition; the follow holds the guard.
- The read model's time-window turn membership (`turn_for_user`) now has the
  Backend's own boundary to lean on; a Turn's `✅` is decided from "idle with no
  live Background Task", not from "any terminal step since the anchor".
- V1 parity is deliberately not invested in (V1 is the retirement surface).
- `?type=idle` is not a legal message filter on V2's `/message` route; an idle
  boundary is only visible in an unfiltered read.

Related: #403 (the bug this subsumes), ADR-0017, ADR-0026, ADR-0028, ADR-0043,
ADR-0050, ADR-0053, ADR-0055, ADR-0058.
