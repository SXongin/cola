# One card per request: one unbounded post-prompt path, honest continuations, and a delivered Completion Notice

> **Amended 2026-10-14 (#650)**: §3's promise is narrowed. The gate guarantees
> *send* order — the notice is sent only after the terminal write's carrier is
> accepted — never *client-visible* order: a card PATCH pushes nothing while the
> notice is a new message, and no Feishu API reports that a client has repainted.
> The notice now also **waits out a repaint delay** before it is sent, and the
> delay's losses are ratified as residuals. See the amendment at the end.

## Context

On a long Turn a Feishu user saw the card lifecycle lie in three ways, and all
three shared one root (spec #602; the originating issues #493, #511, #512):

- **A second Card that claimed 「🔔 已恢复执行」 when nothing resumed.** The
  post-prompt drain hit a fixed 10-minute bound and handed the still-running
  Turn to an out-of-turn follow; the follow finalized on a *later*
  execution-status read than the transcript snapshot it had rendered, so a tail
  that was already durable was orphaned and the next Session Sync pass read it
  as a Wake-less content diff and split a brand-new continuation Card (live
  2026-10-08, `ses_ee3f4531fffeBcOYDY3vHUQDPw`).
- **Two Cards for one restart.** A genuine resumption (a server restart, a
  background-task completion) legitimately opens a continuation Card; while its
  content was still streaming, a later tail split *again* (live 2026-10-08,
  `ses_ee3f19fc4ffefrgIFcTk6zV8yH`).
- **The Completion Notice could precede the answer.** 「✅ 已完成。」 was gated
  only on the Card being terminal, not on the terminal write reaching Feishu,
  so it could land before the terminal slice (when the flush size-split onto a
  new Card) or before a failed ending PATCH was retried as a Pending Card
  Update.

Underneath sat a fourth artifact: a fixed post-prompt budget whose only
remaining job was to hand a still-running Turn from an inline drain to an
out-of-turn follow — a seam that was user-visible (「⏳ 部分完成，继续中…」 then a
new Card) and that the follow did not need, since the follow already had no
total budget and already waited for true idle.

## Decision

**cola renders a submitted prompt to its true end on one unbounded path, and
every continuation and notice is honest about what actually happened.** Three
invariants (spec #602, tickets #603–#607).

### 1. One unbounded post-prompt path

The inline drain and the out-of-turn follow become one loop owned by the
Turn's own task (`Turn::finish` → `drain_after_prompt` → `Turn::drain`): submit
→ read admission (submitted / rejected / unregistered) → loop until a settle
disposition. The hand-off, the 10-minute total budget (`turn_drain_timeout_ms`)
and its budget-derived per-read timeout are removed, as is the guard-release
branch that keyed off the hand-off. The drain's unique startup states survive,
folded into the loop: submit rejection, a racing Supplement, the
unregistered-run confirmation window (a *count* window, already non-clock) and
the unreceived "wait for the message to land" semantics.

- **One per-read timeout, one grace.** The per-read timeout unifies on the
  follow's fixed read timeout (`turn_follow_read_timeout_ms`, 30 s default);
  `turn_follow_grace_ms` (10 min default) is the single grace, bounding only
  the failure modes where nobody can act — lost contact (no full read pair
  answered), an unreconcilable live panel on an idle Session (a crash orphan),
  and the delay before an unreceived submit shows its neutral waiting line.
- **The inflight guard is held for the whole path.** With no ownership
  hand-off there is no gap by construction, so the server-yield's busy read
  never sees a mid-request Session as idle. The pre-stamp ownership recheck
  stays: a new Turn that replaces the accumulator must not have its Card
  stamped with a stale ending.
- **The follow remains for the paths with no Turn task of their own** — the
  recovery re-attach (a retry click on a still-running run, a resumed re-arm)
  and the Wake continuation. It is the shared out-of-turn settle loop under the
  card's identity, holding the guard for its whole window.

### 2. Exactly one Card per resumption

- **The same-snapshot ending rule.** `Done` may be decided only on a snapshot
  that was both rendered *and* shows no new content — the ending is read from
  the drain's own rendered, quiescent read, never a later separate
  execution-status read that could disagree. The same-turn late tail can
  therefore no longer be orphaned into a second Card.
- **Only a genuine resumption may open a continuation Card.** The Wake step
  opens a new Card only for a resumption the Backend performed — a
  Shell/Subagent completion, a restart, an interruption
  (`WakeSource::is_genuine_resumption`). A Wake-less content diff never
  produces a 「🔔 已恢复执行」 receipt; a source this build cannot classify stays
  off that path too. The 承接 line stays source-neutral for the sources that do
  open a Card.
- **Exactly one per resumption.** While a continuation Card the hand-off
  opened is open, a later tail renders onto it in place instead of splitting
  again; the receipt is pushed once turned over.
- **The residual floor.** A Wake-less part the Backend wrote after the run
  already reported idle, on a card past its wait, renders on one
  neutrally-labelled continuation Card (「📄 还有更新」), at most once per request
  (the chain's `StreamAccumulator::residual_card_posted` marker) and with a
  `WARN`. It is a defensive floor, not a normal path — the same-snapshot rule
  makes it near-unreachable — so the content is never dropped and the Card
  never claims a resumption that did not happen.
- **Size split stays.** A slice that outgrows one Card still finalizes it and
  continues on a new Card — the one remaining mid-request continuation besides
  a genuine resumption.

### 3. The Completion Notice is gated on delivery

The notice is sent only once the terminal write's carrier is **accepted by the
delivery layer** (`NoticeGate`), not merely on the Card's terminal state:

- `Delivered` — the terminal slice already landed (the direct PATCH, or a size
  split whose continuation create carries no owed write): announce now;
- `Armed` — the Card still owes a keyless Pending Card Update: the callback is
  stored and fires from the drain that delivers that retry, so the notice
  trails the terminal content it announces;
- `Never` — the Card's newest keyless write settled without delivering (a
  permanent refusal): nothing will ever carry the terminal slice there, so the
  notice is suppressed.

One rule covers all three carriers — the direct PATCH, the size split, the
queued retry — and the quiet in-place settlement reads it identically. A
notice fired from a drain runs on its own task and therefore trails the drain
call it followed.

## Considered options

- **Keep a bounded drain with a hand-off.** Rejected — the hand-off is the seam
  that created the orphan and the guard-free window; the follow already had no
  total budget and already waited for true idle, so the bound's only
  non-derivable role was to give the running state an exit point the hand-off
  itself created.
- **Keep a fixed total budget.** Rejected — a clock turns a correct long wait
  (a permission, a healthy subagent chain, a provider retry) into a wrong
  error, which ADR-0043's 2026-09-28 amendment had already shown; the two
  graces bound the states where nobody can act without clocking a healthy run.
- **Open a Card per Wake-less diff.** Rejected — there is no resumption to
  name, so the receipt would lie, and a late tail could then open a second and
  third Card; the residual floor renders the content honestly once.
- **Drop the Wake-less late tail entirely.** Rejected — the honest floor is
  cheap and the content a user is owed must never be dropped; one neutral Card
  is the floor.
- **Gate the notice on the Card's terminal state (the shipped rule).**
  Rejected — terminal state says the ending was *decided*, not that the write
  *reached Feishu*; the delivery layer's own acceptance verdict is the only
  fact that also covers the size-split and queued-retry carriers.
- **Resume any card in place, terminal cards included (ADR-0066's rejected
  reopening).** Stays rejected — a ✅ Card flipping back to 🔄 reads as broken;
  a genuine Wake on a terminal card continues by split, and a Wake-less tail
  lands on the neutral residual Card. The terminal Card's recorded ending is
  never rewritten.

## Consequences

- **ADR-0043** carries a new amendment banner and a dated amendment: its
  2026-09-24 "the drain bound hands the card to an out-of-turn follow" and its
  2026-09-28 "no total budget — the bound is a lost-contact / stuck-panel
  grace" describe a follow that no longer continues the post-prompt path; the
  no-total-budget conclusion stands, but the Turn's own task owns the whole
  path.
- **ADR-0059** carries a new amendment banner: its "with no total budget the
  follow owns it" is superseded — the merged path owns it, and the follow
  serves only the paths with no Turn task.
- **ADR-0056** carries a dated amendment to its drain-budget asides ("burn the
  drain budget plus the follow ceiling", "retried to the drain bound"): the
  ceiling is gone, and a failing read is bounded by the one per-read timeout,
  not a drain bound.
- **ADR-0066** carries a new amendment banner and a dated amendment: its
  "Restart/interrupt Wakes and the Wake-less late tail keep the split" is
  narrowed — the Wake-less late tail no longer splits into a 「已恢复执行」 Card
  but lands on the neutral residual Card (once per request); and its
  no-reopen-terminal rule is reconciled — a terminal Card is never rewritten,
  but the honest floor for a Wake-less tail past it is the neutral continuation
  Card, not silence and not a false receipt.
- **GLOSSARY** records the merged path (no post-prompt hand-off, no
  inline/follow seam), one Card per resumption, the neutral residual Card, and
  the Completion Notice's delivery gate; **Turn**, **Wake**, **Card Chain**,
  **Session Sync**, **Waiting on Background Work**, **Disposition** and
  **Completion Notice** are updated.
- Tests pin the whole contract at the bridge lifecycle seam: a long healthy run
  finishes on one Card with no hand-off artifact
  (`a_long_healthy_run_finishes_on_one_card`); a Wake-less tail on a waiting
  card opens no Card (`a_wake_less_tail_on_a_waiting_card_opens_no_card`); a
  restart tail stays on its one continuation Card
  (`a_restart_tail_stays_on_its_continuation_card`); the residual floor fires
  at most once (`a_second_late_tail_does_not_post_a_second_neutral_card`); the
  notice trails its carrier
  (`a_direct_terminal_patch_notifies_at_once`,
  `a_queued_ending_patch_notifies_only_after_it_drains`,
  `a_size_split_terminal_write_notifies_after_the_new_card`,
  `a_quiet_settle_defers_the_notice_until_the_retry_drains`). The two tests
  that asserted a Wake-less tail still opens a Card are rewritten to the new
  rule.
- The removed drain total-budget knob disappears from the config surface; the
  follow grace and the follow read timeout remain the surface an operator
  tunes.

## Amendment (2026-10-14): the notice waits out a repaint delay — the gate's promise is send order, not client-visible order

Issue #650, live 2026-10-10 (p2p, `long_task_notice = true`): on turns longer
than five minutes the plain-text 「✅ 已完成。」 still surfaced ahead of the final
answer on the Card — the very symptom #512 set out to remove — on a build
carrying every #602/#607 commit. The API-level order was already correct: the
chat's own timeline (`im/v1/messages`, `update_time`) showed each notice
0.56–0.72 s *after* its turn's terminal card PATCH, and no same-turn card write
after any notice. §3's gate was doing its job; the residual inversion is
client-side and no gate can see it:

- the notice is a **new** message, so Feishu **pushes** it (notification, newest
  line, unread badge);
- the card is a **PATCH** to an existing message, which pushes nothing — the
  client repaints it asynchronously, and a large card (the terminal PATCH
  re-renders the live slice — up to `MAX_CARD_TEXT_CHARS` of body plus its
  panels) does not repaint in ~0.6 s.

No Feishu API reports a client repaint, so no rule at this seam can close the
gap: `im/v1/messages/{id}/read_users` and `im.message.message_read_v1` are
message-level read receipts (the bot's own sends, a 7-day window) that say
nothing about an update; the card PATCH has no notify option; card JSON 2.0's
`config` has no notification field; and every bot message into p2p or a group
pushes — the only non-notifying send, `ephemeral/v1/send`, is group-only,
online-only and explicitly unpatchable.

**The decision.** A Completion Notice is sent once the terminal write's carrier
is accepted **and** the repaint delay has elapsed:

- **`COMPLETION_NOTICE_DELAY_MS` (3 000 ms)** — a constant with a test-injectable
  atomic, like `LONG_TASK_NOTICE_MS` (a constant, not a config key: the value is
  a judgement call no operator can measure either).
- **One rule for every carrier.** The wait is clocked from the carrier's
  acceptance — the direct PATCH's return, the size-split continuation create, or
  the drain that delivers the owed retry — never from arming: an ending that is
  owed waits the delay *after* its drain fires, not from the flush that armed
  it. The clock is read when the send is *scheduled*, which is at or after
  acceptance (the ending path does its topic-cover update in between), so the
  guarantee is the one the promise states: the notice never trails acceptance by
  *less* than the delay, and may trail it by more.
- **The delay and the send run on a spawned task**, so no caller ever waits
  them out: the Turn's `finish` releases its busy guard at once, Session Sync's
  quiet settle never stalls a pass, and the delivery layer's drain is not
  extended. The three send sites collapse into one entry point (`spawn_notice`),
  which replaces `deliver_deferred_notice`.
- **Ownership is re-checked after the delay.** A card a newer Turn replaced (or
  a collect took) while the notice waited suppresses it — the same rule the
  announce already applied to a card replaced *before* it — and the recipient
  is still resolved against the **captured** card, never a re-read. The identity
  is the card's message id, the name cola gives a card everywhere, and it is
  sufficient here: a fresh Turn installs its card with no id at all
  (`install_fresh_session`) and takes the id of the loading card `reply_card`
  creates for *it* — a retry and a takeover included — so a newer request can
  never answer for an older card's id; and the one id reused on purpose (a Wake
  resuming its own waiting card in place) is the same request continuing, which
  is what the notice announces anyway. The delay also never outlives a process,
  so the window a restart could have created does not exist (residual 2 below).

**§3's promise, restated.** The gate guarantees that the notice is not sent
before the terminal write's carrier is accepted, and that it trails that
acceptance by at least the delay. It does **not** guarantee that the user sees
the final answer first — that fact lives in the client, and nothing on the wire
exposes it. Story 3 of spec #602 reads this way from here on.

**Ratified residuals** (a heuristic is only honest with its losses named):

1. **The delay may be too short.** A very large card on a slow client can still
   repaint after it and the notice then lands first. That is exactly what the
   restated promise covers; tuning is one constant.
2. **A notice still in its delay dies with the process.** Nothing about the
   notice is durable — it never was, and it rides no staged structure.
3. **An armed notice is still lost to stage eviction, supersession or a
   permanent refusal** — unchanged, and the delay sits *after* firing, so it adds
   no new ownership (pinned by `an_evicted_owed_write_suppresses_the_notice`).
4. **Every notice now trails its turn's end by the delay** (groups included) —
   the deliberate cost of one rule.

**Tests.** The four acceptance tests named below are rewritten to the delayed
rule — `a_direct_terminal_patch_notifies_at_once` becomes
`a_delivered_terminal_write_notifies_only_after_the_delay` (the gate still never
defers a delivered write; the send just waits its delay, and the turn's guard is
released meanwhile), and
`a_queued_ending_patch_notifies_only_after_it_drains`,
`a_size_split_terminal_write_notifies_after_the_new_card` /
`a_size_split_terminal_flush_notifies_after_the_new_card` and
`a_quiet_settle_defers_the_notice_until_the_retry_drains` assert the notice
*arrives* rather than that it was already sent — the queued one also pinning
that the delay's clock starts at delivery, not at arming. New:
`the_notice_delay_never_blocks_the_turn` (a 60 s delay cannot hold
`Turn::run`, so the send is spawned) and
`a_card_replaced_during_the_notice_delay_suppresses_the_notice`. Two #602
review-ordering tests change shape with it:
`a_replace_between_captures_never_redirects_a_delivered_notice` now pins
suppression across the capture→send window (the window the delay creates), and
`a_delivered_notice_uses_the_original_recipient` still pins the captured
recipient. The bridge fixtures inject a zero delay except in the tests that pin
it (`scripted_app_with`, `build_app`), because the production value is a
wall-clock wait with nothing for a test to observe.

Related: #602, #603, #604, #605, #606, #607, #493, #511, #512, #650, ADR-0043,
ADR-0050, ADR-0056, ADR-0059, ADR-0060, ADR-0061, ADR-0066, ADR-0070,
ADR-0072.
