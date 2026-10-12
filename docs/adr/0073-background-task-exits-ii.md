# Background-task exits II: one shared reconcile, subagent evidence, manual cleanup, output tails

## Context

ADR-0065 landed the first exit cut for a **Background Task** whose completion
record is lost: the Session Sync pass corroborated the live list against the
server's runtime registries and retired a shell the runtime reported ended — or
no longer knew — with positive evidence only, marking an inactive subagent's
row `⚠️ 状态待确认` while the cleanup action stayed deliberately deferred ("a
button needs its own dismissal persistence and interaction design"). Three
gaps remained, and the ledger itself stayed opaque (spec #588):

- **The reconcile was one call site, and it skipped an inflight Session.** A
  task that died mid-turn kept its row until the turn ended; the settle then
  still saw it live, the card yielded 「⏳ 等待后台任务」, and only a later ~8 s
  Session Sync pass retired it and settled ✅ — one wrong yield and one extra
  PATCH per dead task (#462).
- **A subagent the runtime reported inactive could not retire.** The inactive
  verdict only added the marker; even when the child's own transcript already
  showed a terminal end, only a Wake retired it — and the missing Wake is
  exactly the failure mode this family exists for (#464).
- **The residue no machine can confirm had no exit.** With the runtime silent
  (process-local registries, restart windows, cross-process hosting) the card
  waited forever; `/stop` only promises the card will be marked stopped once
  the tasks end, and a restart re-derived the same wait. The user could not
  say "this one is stranded, close the wait" (#463).
- **A long-running shell showed no output.** The ledger row was label + clock;
  the strongest "it is actually alive" signal — and what whoever picks the work
  up wants at the ending — was invisible (#465).

After #583 (ADR-0071's amendment) a restart's wait lives on a projected
successor card carrying the ledger; the residue, cleanup and output experience
had to fit that home. Spec #588 landed the second exit cut (tickets #589–#593).

## Decision

**Complete the background-task exit story client-side, on the successor
shape:** one shared reconcile step, the child's own transcript as a subagent's
evidence, a manual cleanup for the residue, and output tails on the surfaces
the user already looks at.

- **One shared reconcile step.** The **Runtime Reconciliation** — until now a
  private step of the external flow, reachable only from the Session Sync pass
  and gated on a receivable waiting card — becomes one step taking the session
  id, its directory and the transcript just read. Its callers are Session Sync
  (placement and waiting-card admission unchanged), the Turn's drain tick and
  the follow/settle loop. Guard rails, all shared: zero requests when the read
  lists no live Background Task; one verdict per Session per 30 s —
  process-wide, attempt-stamped (every attempt counts, success or not, so a
  failing runtime is not hammered at the poll cadence) and injectable beside
  the other timing atomics; a failed or timed-out read yields no verdict and
  changes nothing — no retirement, no settle — so a flaky runtime can never
  end a wait. The unconfirmed markers stay that invariant too (review, PR
  #595): they are process-local, the same adapter overlay re-applies them to
  every later transcript read — only for a task still live in it — so a read
  the throttle did not spend a verdict on cannot drop a marker and the cleanup
  button it gates, and only a verdict resolves one (a Running child clears its
  marker, an Inactive one keeps it, and a read that does not answer for the
  child keeps the previous state). The step is gate-free on the READ side: each
  caller first decides whether it can render what it observes (Session Sync's
  waiting-card admission; the live loops only once the card carries the Turn
  anchor the entry can be placed by). The WRITE side is one uniform invariant
  (review, PR #595): the reconcile returns a pass, and **every caller commits
  that pass to the overlay only after the flush that carried the read's entries
  was accepted** — Session Sync's landed wake/ledger refresh, the drain tick's
  and the follow/settle loop's completed render, the cleanup click's landed
  refresh. **Accepted is the delivery layer's own write outcome (ADR-0067,
  ADR-0072): the carrying payload was delivered, or failed recoverably and is
  still owed its retry by the pending outbox.** A **permanent refusal** — a
  typed content rejection the fenced fallback was rejected with too, so the
  card is suspended, or any other non-recoverable error — is NOT accepted:
  nothing will ever be written for that payload, so the pass records nothing
  and the yielded refresh reports neither `Refreshed` nor `Settled`. A carrier
  that never lands records nothing either: the task stays live, and the next
  card that can render its entry claims it again (ADR-0065's designed
  behavior).

- **Live-path retirement entries render exactly once.** The live render path —
  until now only refreshing ledger rows — gains the entry machinery: entries
  are planned against the same read (the retirement's synthetic
  `runtime:<call_id>` announce key), their output reads are spent outside the
  cards lock, and the announcements commit under it. Exactly-once comes from
  the announce key (per observing chain) plus the process-local retirement
  overlay the adapter applies to every later transcript read; a cola restart
  may re-show one entry on the newest chain, which is honest — that card is
  where the user is looking (ADR-0065's rule, kept). A mid-turn retirement
  therefore drops the row and records 「🔔 shell 结束」/「🔔 shell 已失联」 on
  the same render, and the settle that follows sees the task ended: the turn
  settles ✅ directly, no intermediate 「⏳ 等待后台任务」 detour. A drain tick
  that retires the last task still renders the read before settling — the
  overlay means no later read carries the retirement, so skipping the render
  would drop its entry.

- **Subagent positive evidence.** A child session absent from the runtime's
  active map keeps its row unconfirmed; the same admitted cycle then
  spends at most one light read per suspect: ONE page of the child session's
  newest assistant message (`type=assistant`, newest first — the retry read's
  minimal shape). A terminal step finish **with** the message's completion
  stamp retires the task as ended with that clock, rendered as the runtime's
  own ending (`🔔 subagent 结束`); a child session the server no longer knows
  (404) retires it as lost (`🔔 subagent 已失联`, identity-only); an unreadable
  or non-terminal read — a failed page, a finish still missing its stamp, no
  assistant message — keeps the row `⚠️ 状态待确认`. Nothing is guessed, and
  V1 answers without a request.

- **A manual cleanup for the residue.** A waiting card carrying at least one
  `⚠️ 状态待确认` row grows one section-level 「清理待确认任务」 button below
  the ledger — never on a live or terminal card, and gone with the last
  unconfirmed row, because its presence derives from the same rows the ledger
  renders. The click (action `cleanup`, carrying the session id alone) follows
  the retry button's claim-then-spawn pattern and acks 「正在清理...」 inside
  Feishu's 3 s budget. The pipeline re-reads the transcript and the runtime
  once, re-deriving the unconfirmed set on its own read (a failed runtime read
  clears nothing), then clears ALL unconfirmed rows at once — never a
  positively running task — synthesizes one clean retirement per row at the
  click's own clock, and pushes one completion entry per cleared row:
  `🧹 shell 已清理：<命令>（人工）` / `🧹 subagent 已清理：<描述>（人工）`
  (bare: `🧹 <noun> 已清理（人工）`). EVERY overlay record the pass owes — the
  runtime/evidence retirements it applied, the cleared ids and the resolved
  marker set — commits only when its refresh lands (review, PR #595: the write
  admission is re-checked under the card lock and a card replaced while the
  reads were in flight refuses the write — recording any of it anyway would
  hide the tasks, or drop their marker and button, with no card ever rendering
  the change, which no later read can reconstruct). The ordinary yielded-card
  refresh renders rows dropped and entries placed, and the card settles by
  ADR-0060's rules —
  the last task gone is ✅ in place, one PATCH, no continuation, the Completion
  Notice per its existing rules. A late Wake afterwards behaves exactly as
  before: the cleanup's entry is synthetic and never advances the durable Wake
  Watermark (see Consequences). The dismissal is process-local and re-derived
  at restart, like the retirement overlay — no durable format change — and the
  claim is released after the write, so a later unconfirmed row is cleanable
  again.

- **Output tails where the user already looks.** While a shell runs, its ledger
  row carries a bounded output window under it: one tail read per live shell on
  the same admitted reconcile cycle (the server's own tail idiom — a probe
  cursor past the end learns the record's size, then one page reads the last
  window), decoded and clipped to the last 15 lines of the last 4 KiB, with the
  byte boundary kept char-safe. The live window is labelled 「截至于 HH:MM」, plus
  「仅最后 N 行 · 已截断」 when the record held more (N = the lines the window
  actually renders); a read that fails, vanishes or answers nothing omits the
  window entirely — never a placeholder — and the accumulator keeps the last
  established window between throttled cycles, so a row never flickers. When
  the shell ends, its completion entry (完成 / 取消 / 失败 / 结束) carries the
  same bounded tail (≤ 4 KB, its last 15 lines) in its fold body under the
  identity line, with only
  「仅最后 N 行 · 已截断」 when the record held more — no 「截至于 HH:MM」: the
  identity line already carries the run's own start and duration, and a
  completed capture cannot grow (review, PR #595) — or
  「输出已不可用」 when the record cannot be read; the 已失联 entry, a 🧹
  cleanup and a subagent carry identity only. The reads are display-only — they
  never prompt, retire or settle — and generation-aware: V1 spends no request.
  The row and entry size estimates charge the rendered tail, so a noisy shell
  cannot balloon the card.

- **The successor shape.** The projected successor (#583) renders through the
  same ledger/entry/card-builder path, so rows, counts, the cleanup button and
  the output windows appear there with no extra mechanism; its reconcile and
  tail costs are bounded like any waiting card.

- **Unchanged copy.** The unconfirmed marker `· ⚠️ 状态待确认`, the title
  counts `⏳ 后台任务（N · M 待确认）`, and the entry titles'
  `🔔 <noun> <ending>：<label>` form stay as shipped.

## Considered options

- **Retire an absent child on the active map alone.** Rejected: the registries
  are process-local, so absence is also what a server replacement looks like;
  only positive evidence — the child's own newest assistant message, a gone
  child session — may end a task (ADR-0065's rule, re-applied to the new path).
- **Age- or quiet-based auto-clean.** Rejected: there is no honest signal. A
  quiet long-running shell is exactly "still running", and a threshold would
  end waits the runtime would confirm minutes later; the user's own click is
  the honest ending for the residue.
- **Two-strike lost for subagents** (two consecutive inactive verdicts without
  terminal evidence). Rejected in the design round: a second read refines
  nothing when the first can already answer from the child's own transcript,
  and an inconclusive read must keep the row unconfirmed rather than
  accumulate strikes.
- **Separate entry points for output** (a `/output` command, or a button per
  row). Rejected: the tails ride the folded surfaces the user already reads —
  the running row and the completion entry — so no new command, no new
  control, and no second read to teach.
- **Durable dismissal persistence.** Deferred: in-memory first, re-derived at
  restart like the retirement overlay. A restart re-reads the same transcript
  and runtime, the wait reappears, and the user can click again. Revisit like
  the Wake Watermark if it grates.

## Consequences

- **ADR-0065's deferred items now landed**: the cleanup action exists with its
  own interaction design, and the "a retirement is observed only while a
  waiting card can receive it" gate is replaced by the shared step plus the
  announce-key/overlay pair — each caller observes only where it can render and
  commits the overlay only after the flush that carried the read's entries was
  accepted (delivered, or owed by the delivery layer's retry; a permanent
  refusal records nothing — review, PR #595), and exactly-once holds across
  paths.
- The GLOSSARY gains **Runtime Reconciliation** and **Cleanup**; **Background
  Task**, **Background Task Ledger** and **Waiting on Background Work** record
  the widened retirement rule, the cleanup exit and the output tails.
- Costs are bounded per admitted cycle: one runtime verdict per Session (30 s
  shared, process-wide), at most one child read per suspect, and one tail read
  per live shell; an empty ledger spends nothing and a failed read changes
  nothing (issue #462's operator requirement).
- Retirements — runtime, child evidence and cleanup alike — are process-local:
  a cola restart loses the overlay and the dismissal, and the next reconcile
  re-derives the same wait. Accepted: honest re-derivation, no durable format
  change. Their completion entries are synthetic: they keep exactly-once
  through the observing chain's in-memory announcement set, and — since the
  2026-10-11 amendment below — through the durable `rendered` marks for every
  earlier chain and life. They never advance the durable **Wake Watermark**,
  which ADR-0061 defines as the newest
  **Wake** whose completion a card announced. Staging a synthetic clock there
  would make a restart read a genuinely un-announced late Wake at or below it
  as already announced and suppress its continuation — #590's "a late Wake for
  a cleared task behaves as before; nothing is suppressed" (review, PR #595).
- **#454 closes with its upstream half explicitly left open upstream**: run the
  restart sweep for a plain `opencode serve` (or reconcile stale
  `job.background/*` markers at boot), clean up the stale markers, and expose a
  job list/get route (tombstones included). The client reconcile remains the
  second line, not the cure (ADR-0065's Consequences keep their force).
- Tests pin: the shared throttle across paths and the zero-request empty
  ledger; a mid-turn retirement dropping the row and ending the turn ✅
  directly, its entry rendered once; the child-evidence matrix (terminal /
  gone / unreadable / non-terminal) and V1's zero requests; the cleanup click
  (claim, 3 s ack, all rows at once, a running task keeps the wait, button
  presence only with unconfirmed rows on a waiting card, late Wake untouched,
  the successor carrying the button, a replaced card's refused refresh
  recording nothing — the runtime retirement its own reconcile found included
  — a permanently refused write recording nothing and rendering on the next
  card that can carry its entry, a recoverably failed (queued) one still
  recording, and the successor still rendering and clearing the tasks exactly
  once);
  the output windows (labels, budget, live
  omission, entry tails, 「输出已不可用」 on a failed or vanished record, no
  re-read on the settled successor); and the projected successor's affordances.

Related: #588, #454, #462, #463, #464, #465, ADR-0054, ADR-0059, ADR-0060,
ADR-0065, ADR-0066, ADR-0069, ADR-0071, ADR-0072.

## Amendment (2026-10-11): a rendered retirement entry is durable, and "one entry" means one

Issue #671 (ticket #672) tightens the paragraph above's "a cola restart may
re-show one entry on the newest chain". In production that re-show was not
one: a Wake-less Background Task (#454) never retires durably, so every
restart re-derived its retirement and each restart's new card re-posted the
same `🔔 … 结束` entry — measured 2026-10-11, seven copies of one 5h59m
subagent's entry across a day of cola restarts. The premise the sentence
rested on ("that card is where the user is looking") does not hold when the
restart itself creates the card and pushes it.

- The durable mark rides the **Rendered Cursor stage** of the body that carried
  the entry (spec #561, review #569): each built body records the SYNTHETIC
  announce keys of the ledger entries it actually rendered
  (`BuiltCard::rendered_keys`; a Wake's entry carries none — its exactly-once is
  the Wake Watermark), its stage carries exactly those, and the stage's
  confirmation — the machinery that already distinguishes a DELIVERED write
  from an owed or refused one, the queued payload's late delivery included —
  writes them into a new `rendered` section of the Chain Record. A body claims
  only what it rendered, so a size split's prefix never claims an entry its
  continuation shows. A superseded stage (a newer confirmation already won)
  hands back the base's own cursor and no gap, so its delivered keys still ride
  that one write rather than waiting on a second. A write that never lands
  records nothing (its confirmation is never reached), and the next card that
  can render the entry still claims it (ADR-0065's rule, kept for a retirement
  nothing has shown yet). `plan_runtime_entries` skips a key that mark carries,
  so an entry one delivered card has shown never renders on a later card,
  restart included. A write the platform ACCEPTS advances its Rendered Cursor,
  gap coverage and rendered keys in ONE chains write in the same synchronous
  step as the acceptance — so a crash cannot leave the mark apart from the
  cursor it was confirmed with and cancelling the flush before the staged-cursor
  confirmation cannot lose them. A recoverably-failed body's keys
  are mirrored on the Chain
  Records, keyed by the card its queued payload travels on
  (`ChainRecords::note_owed_rendered`) — the card's id survives a Session's
  replace, removal and re-key, a rename moves the Session the entry names, and a
  writer that captured the dead id before the rename resolves through it — and
  the drain reconcile records them, inserting them into whatever chain is
  live, when that payload delivers; a commit landing in the window between that
  delivery and the reconcile honors the delivered mirror too
  (`Turn::recorded_marks`), so it cannot re-render the entry the delivered card
  already showed. Neither the staged-cursor cap nor a replaced
  accumulator can then lose the mark (both found by the CI gate): the cap is a
  cursor-retention bound again, and an evicted stage loses only its cursor and
  gap, never the mark.
- The residual the marks cannot close: two chains that PLAN the same retirement
  concurrently — before either body's write is accepted — can each render it
  once, because the claim is made only when a write is accepted. Claiming at
  plan time would suppress an entry whose body is then refused, the loss side
  the family above deliberately trades away from.
- The `rendered` marks are deliberately a SECOND durable fact, never the Wake
  Watermark: a retirement is not a Wake, so recording it must not move the
  floor a late Wake is judged against (#590, PR #595). It outlives the record
  it was written on and follows a recreated session id, like the Watermark. It
  is bounded (`MAX_RENDERED_KEYS`, oldest evicted first): past that bound a very
  old entry may render once more after a restart — the duplicate-over-loss side
  this family always trades toward.
- What this does NOT change: a restart may still create a card (a Wake
  continuation, a new Turn) and the entry may still be re-derived; only its
  duplication is gone. Suppressing the card itself is a separate question. A
  projection takeover whose armed session a fresh Turn replaced before the
  re-point still marks what its landed create rendered — the create is what put
  the body on a card, and the mark is session-scoped, so it needs no record
  write.
- A chain's read of the marks and its own install are **one critical section**
  (found in review, twice). A fresh Turn used to seed before its `begin_turn`
  await and insert after, and an external arm seed before its
  `attach_work_context` await: a landed body writing a mark in that gap left the
  new chain holding the older, empty set, and its next read re-rendered the
  entry the successor card had just shown. Seeded under the install, the
  orderings are total — a mark landing first is already durable when the seed
  reads, one landing after finds this chain's session and reaches its announce
  gate. A projection arm reads its seed inside the critical section that proves
  the session still has no card and plans there: no writer exists while the map
  entry is absent.

Related: #671, #672, #454, #590.

