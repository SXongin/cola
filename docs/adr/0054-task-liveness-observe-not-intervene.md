# Task liveness is observed on the running panel; child sessions are never messaged

A `task` tool call runs a **child session** — a real session parented to the
calling one, whose id sits in the call's metadata (`state.metadata.sessionId`).
While it runs, cola's card shows only the **Tool Panel** (description,
subagent_type, `⏳`); everything inside — tools, waits, progress — has no Feishu
surface, so a long task and a hung one look identical. #284 showed what the
misjudgment costs: the card was finalized while both review task calls still
ran, and their work never rendered. This ADR decides the minimum answer: one
liveness line on the running panel, read-only. The operator never writes into a
child session.

## Context

- **A child session is resumable, so panels and sessions are not 1:1.** A
  `task` call creates the child; a later `task` call carrying the prior
  `task_id` continues the same child — foreman hands review results back this
  way. One child session can be driven by several task calls, and several
  panels can name it.
- **The blocking waits are already routed.** A child's Permission/Question
  re-homes to the parent's card through the parent chain
  (`resolve_card_target`), so "it is waiting on me" is not the gap.
- **The read is cheap.** The backend seam already reads one session's
  transcript (ADR-0053), and cola keeps the call's metadata verbatim
  (`ToolCall.metadata`); no new API surface is needed. The wait comes from the
  request flows' existing pending records, so no per-poll pending listing is
  added either.

## Decision

- **Observation only.** cola never sends a message into a child session. The
  parent Session stays the only prompt seam; continuing a child is the parent
  agent's `task` call with the prior `task_id`. No free-text injection, and no
  per-child abort/continue control (a `/stop` on the parent already aborts its
  task call with well-defined semantics).
- **One liveness line on the running panel.** While a task Tool Panel is live
  it carries the child's newest running tool (by server start time), the age of
  its last observed activity, and — when the child has a pending
  Permission/Question — the wait state, in the header's own vocabulary
  (等待你的授权 / 等待你的回答 / 等待你的授权/回答).
- **Live only, direct child only.** The line renders only while the panel is
  live; a settled panel keeps its ordinary final form. It reads only the
  panel's direct child session, never nested descendants (subagents cannot
  dispatch `task` under their derived permissions, so nesting is rare).

## Considered Options

- **Free-text injection into the child** (a child-addressed Supplement).
  Rejected: it bypasses the orchestrator that owns the child's lifecycle — the
  parent reads each task call's result as the child's answer, so an out-of-band
  message written mid-run makes the parent's view wrong — and it cannot settle
  the two common causes of a long-looking task (a routed wait, or a long tool).
- **Expand the child's transcript on the card** (the nested view other clients
  show). Rejected: a much larger render surface (paging, live tail, card
  limits) to answer a question the liveness line already answers.
- **A command or standalone card for on-demand inspection.** Rejected: the
  question is asked while looking at the running panel, so the answer belongs
  there, with no second entry point to learn.
- **A new glossary term for the child session.** Rejected: `task` keeps meaning
  the tool call/panel; the **Session** entry records the child relationship and
  the N:1 resume shape (see Domain note).

## Consequences

- The task Tool Panel gains a display-only liveness section sourced from the
  call's `sessionId`: the child's transcript read plus the existing pending-wait
  records. It opens no new write path, and ADR-0045 already keeps a running
  panel on the live card.
- The wait reflects the request sweep's cadence (seconds), and the age is kept
  as the last observed activity TIME, not an age snapshot: a failed child read
  makes the age keep growing truthfully instead of freezing at "5s 前" or
  inventing a new one.
- A resumed task shows the same child's state on whichever panel is running;
  the operator reads it as the session continuing, not as a new sub-task.
- If direct child messaging is ever wanted, it needs its own decision: the
  rejected alternative and its rationale are recorded here.

## Domain note

The **Session** glossary entry records the child session (parented; never
mapped to a Chat/Topic; its Permissions/Questions re-home to the parent's card)
and that the `task` call is a Tool Panel — one child session can be driven by
several calls. The **Tool Panel** entry records the live task panel's liveness
line. No new canonical term.
