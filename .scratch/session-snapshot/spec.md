# Session Snapshot — smooth session takeover on Feishu

Design source: `docs/adr/0028-session-snapshot-card.md`. Confirmed via grilling
2026-09-09.

## What

Taking over a session — first `/switch` to a foreign session, `/attach`,
`/topic --adopt`, or a re-`/switch` back to a mapped session — makes it the
thread's Active Session but tells the operator nothing about it: what happened,
whether the last turn finished, and whether something is blocked on them. Today
an activation ends in bare text (「已接管会话 `title`（目录 `dir`）。」) or a
switch-card patch. The snapshot replaces all of that with ONE read-only card
built purely from server reads — no prompt, no tokens, no session write, never
queued against a running turn.

## Decision summary (ADR-0028)

- **Session Snapshot (会话快照)**: a read-only card emitted whenever a Chat/Topic
  activates a session, replacing every adopt/switch confirmation.
- **One card per activation**: lobby text adopt, in-topic `/switch`/`/attach`
  adopt (the snapshot becomes the Topic Anchor), `/topic --adopt` (cover root
  stays, snapshot is the topic's first bot message + anchor), switch-card
  接管/切换 (the list card is patched in place to the snapshot).
- **Content**: header 已接管/已切换 + status line (等待你的确认 > 运行中 > 需要重试
  > 空闲, from `GET /session/status`), adopt-time pending Permission/Question
  blocks (actionable, claimed so the request poller does not duplicate them),
  and a 最近对话 tail (last 4 text-bearing messages, verbatim text parts,
  collapsible).
- **Suppression**: only when re-activating a session already mapped to this
  thread that is idle, has no pending items, and whose newest user message is
  a Cola-Authored Message — then the existing one-line text ack stays and no
  card is sent. First-time adoption always snapshots.
- **One-shot, with one live exception**: an adopted-**busy** session's in-flight
  external turn is followed into the card (reusing the external-reply renderer)
  until completion. All other snapshots are static; later events keep flowing
  through the existing external-sync, request poller, and turn streaming.
- **No ownership concepts**: each Chat/Topic stays a single-operator view; no
  "whose session" watermark. No AI-generated summary, ever.

## Key terms (CONTEXT.md)

Session Snapshot (会话快照).

## Scope

In: `GET /session/status` client support; snapshot data gathering + suppression
predicate; snapshot card renderer; wiring into every adoption entry point
(anchor handling, text-ack fallback); switch-card patch-in-place; adopt-time
pending claim; busy-adopt follow-to-completion. Update docs/help if they quote
the old adopt confirmations.

Out (deferred): child-session (sub-task) pending permissions embedded in a
parent's snapshot (they keep today's standalone delivery via the parent-chain
walk); persisted per-thread last-seen deltas; multi-user identity / ownership;
AI-generated summaries; live refresh beyond the busy-adopt case.

## Issues

| # | Title | Blocked by |
|---|-------|-----------|
| 01 | Session-status client + snapshot data & suppression predicate | — |
| 02 | Snapshot card renderer | 01 |
| 03 | Wire snapshot into adoption entry points | 01, 02 |
| 04 | Switch-card 接管/切换 patches in place to the snapshot | 03 |
| 05 | Adopt-time pending claim (no duplicate; resolve patches snapshot) | 02, 03 |
| 06 | Busy adoption follows the in-flight external turn | 03, 05 |

Triage state: `ready-for-agent` — tickets written from the ADR after grilling.
