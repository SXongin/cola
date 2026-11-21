# One card per request: one unbounded post-prompt path, honest continuations, and a delivered Completion Notice

> **Amended 2026-10-14 (#650)**: §3's promise is narrowed. The gate guarantees
> *send* order — the notice is sent only after the terminal write's carrier is
> accepted — never *client-visible* order: a card PATCH pushes nothing while the
> notice is a new message, and no Feishu API reports that a client has
> repainted. The notice also **waits out a repaint delay**, whose losses are
> ratified residuals.

## Context

On a long Turn a Feishu user saw the card lifecycle lie in three ways, sharing
one root (spec #602; the originating issues #493, #511, #512):

- **A second Card that claimed 「🔔 已恢复执行」 when nothing resumed.** The
  post-prompt drain hit a fixed 10-minute bound and handed the still-running
  Turn to an out-of-turn follow; the follow finalized on a *later*
  execution-status read than the snapshot it had rendered, orphaning a tail that
  was already durable — and the next Session Sync pass read it as a Wake-less
  content diff and split a brand-new continuation Card.
- **Two Cards for one restart.** A genuine resumption opened a continuation Card;
  while its content was still streaming, a later tail split *again*.
- **The Completion Notice could precede the answer.** 「✅ 已完成。」 was gated
  only on the Card being terminal, not on the terminal write reaching Feishu, so
  it could land before the terminal slice or before a failed ending PATCH was
  retried as a Pending Card Update.

Underneath sat a fourth artifact: a fixed post-prompt budget whose only remaining
job was to hand a still-running Turn from an inline drain to an out-of-turn
follow — a user-visible seam the follow did not need, since it already had no
total budget and already waited for true idle.

## Decision

**cola renders a submitted prompt to its true end on one unbounded path, and
every continuation and notice is honest about what actually happened**
(spec #602, tickets #603–#607).

### 1. One unbounded post-prompt path

The inline drain and the out-of-turn follow are one loop owned by the Turn's own
task (`Turn::finish` → `drain_after_prompt` → `Turn::drain`): submit → read
admission (submitted / rejected / unregistered) → loop until a settle
disposition. There is no hand-off, no total budget (`turn_drain_timeout_ms` is
gone) and no budget-derived per-read timeout.

- **One per-read timeout, one grace.** The per-read timeout is the follow's fixed
  read timeout (`turn_follow_read_timeout_ms`, 30 s default);
  `turn_follow_grace_ms` (10 min default) is the single grace, bounding only the
  failure modes where nobody can act — lost contact (no full read pair
  answered), an unreconcilable live panel on an idle Session (a crash orphan),
  and the delay before an unreceived submit shows its neutral waiting line. A
  readable, merely-long run is never clocked: a clock turns a correct long wait
  (a permission, a healthy subagent chain, a provider retry) into a wrong error,
  which ADR-0043 had already shown.
- **The inflight guard is held for the whole path.** With no ownership hand-off
  there is no gap by construction, so the server-yield's busy read never sees a
  mid-request Session as idle. The pre-stamp ownership recheck stays: a new Turn
  that replaces the accumulator must not have its Card stamped with a stale
  ending.
- **The follow remains for the paths with no Turn task of their own** — the
  recovery re-attach (a retry click on a still-running run, a resumed re-arm)
  and the Wake continuation. It is the shared out-of-turn settle loop under the
  card's identity, holding the guard for its whole window.

### 2. Exactly one Card per resumption

- **The same-snapshot ending rule.** `Done` is decided only on a snapshot that
  was both rendered *and* shows no new content — read from the drain's own
  rendered, quiescent read, never a later separate execution-status read that
  could disagree. The same-turn late tail can therefore no longer be orphaned
  into a second Card.
- **Only a genuine resumption may open a continuation Card.** The Wake step
  opens a new Card only for a resumption the Backend performed — a
  Shell/Subagent completion, a restart, an interruption
  (`WakeSource::is_genuine_resumption`). A Wake-less content diff never produces
  a 「🔔 已恢复执行」 receipt; a source this build cannot classify stays off that
  path too. The 承接 line stays source-neutral for the sources that do open a
  Card.
- **Exactly one per resumption.** While a continuation Card a genuine
  resumption opened is open, a later tail renders onto it in place instead of
  splitting again; the receipt is pushed once turned over.
- **A terminal Card is never rewritten.** Resuming any card in place, terminal
  cards included, stays rejected (ADR-0066's rejected reopening): a ✅ Card
  flipping back to 🔄 reads as broken. A genuine Wake on a terminal card
  continues by split, and a Wake-less tail lands on the neutral residual Card.
- **The residual floor.** A Wake-less part the Backend wrote after the run
  already reported idle, on a card past its wait, renders on one
  neutrally-labelled continuation Card (「📄 还有更新」), at most once per request
  (the chain's `StreamAccumulator::residual_card_posted` marker) and with a
  `WARN`. It is a defensive floor, not a normal path — the same-snapshot rule
  makes it near-unreachable — so the content is never dropped and the Card never
  claims a resumption that did not happen.
- **Size split stays.** A slice that outgrows one Card still finalizes it and
  continues on a new Card — the one remaining mid-request continuation besides a
  genuine resumption.

### 3. The Completion Notice is gated on delivery, and waits out a repaint delay

The notice is sent only once the terminal write's carrier is **accepted by the
delivery layer** (`NoticeGate`), not merely on the Card's terminal state — that
state says the ending was *decided*, not that the write *reached Feishu* — **and**
the repaint delay has elapsed:

- `Delivered` — the terminal slice already landed (the direct PATCH, or a size
  split whose continuation create carries no owed write): announce after the
  delay;
- `Armed` — the Card still owes a keyless Pending Card Update: the callback is
  stored and fires from the drain that delivers that retry, and the delay is
  clocked from that delivery, not from arming, so the notice trails the terminal
  content it announces;
- `Never` — the Card's newest keyless write settled without delivering (a
  permanent refusal): nothing will ever carry the terminal slice there, so the
  notice is suppressed.

One rule covers all three carriers — the direct PATCH, the size split, the
queued retry — and the quiet in-place settlement reads it identically.

**The delay: `COMPLETION_NOTICE_DELAY_MS` (3 000 ms)**, a constant with a
test-injectable atomic like `LONG_TASK_NOTICE_MS` (not a config key: a judgement
call no operator can measure). It is clocked from the carrier's acceptance and
read when the send is scheduled, so the notice never trails acceptance by *less*
than the delay and may trail it by more. The delay and the send run on a spawned
task (`spawn_notice`), so no caller waits them out: the Turn's `finish` releases
its busy guard at once, Session Sync's quiet settle never stalls a pass, and the
delivery layer's drain is not extended. Ownership is re-checked after the delay —
a card a newer Turn replaced or a collect took suppresses the notice — and the
recipient is resolved against the **captured** card, never a re-read. The
identity is the card's message id: a fresh Turn installs its card with no id and
takes the id of the loading card `reply_card` creates for *it*, so a newer
request can never answer for an older card's id; the one id reused on purpose (a
Wake resuming its own waiting card in place) is the same request continuing.

**Why the delay, and what it does not promise.** The API-level order was already
correct: the chat timeline showed each notice 0.56–0.72 s *after* its turn's
terminal card PATCH, with no same-turn card write after any notice. The residual
inversion is client-side and no gate can see it: the notice is a **new** message,
so Feishu **pushes** it; the card is a **PATCH**, which pushes nothing — the
client repaints it asynchronously, and a large card does not repaint in ~0.6 s.
No Feishu API reports a client repaint (read receipts are message-level; card
JSON 2.0's `config` has no notification field; the only non-notifying send,
`ephemeral/v1/send`, is group-only, online-only and unpatchable). So §3
promises: the notice is not sent before the terminal write's carrier is accepted,
and it trails that acceptance by at least the delay. It does **not** promise the
user sees the final answer first — that fact lives in the client. Story 3 of
spec #602 reads this way from here on.

**Ratified residuals** (a heuristic is only honest with its losses named):

1. **The delay may be too short.** A very large card on a slow client can still
   repaint after it; that is exactly what the restated promise covers, and
   tuning is one constant.
2. **A notice still in its delay dies with the process** — nothing about the
   notice is durable, and it rides no staged structure.
3. **An armed notice is still lost to stage eviction, supersession or a
   permanent refusal** — unchanged, and the delay sits *after* firing, so it
   adds no new ownership.
4. **Every notice now trails its turn's end by the delay** (groups included) —
   the deliberate cost of one rule.

## Consequences

- The removed drain total-budget knob disappears from the config surface; the
  follow grace and the follow read timeout remain the surface an operator tunes.
- ADR-0043, ADR-0056, ADR-0059 and ADR-0066 carry the narrowed readings: the
  Turn's own task owns the post-prompt path, the ceiling is gone, and a
  Wake-less late tail lands on the neutral Residual Card instead of splitting.
- GLOSSARY records the merged path (no post-prompt hand-off, no inline/follow
  seam), one Card per resumption, the neutral residual Card, and the Completion
  Notice's delivery gate and repaint delay.
- The bridge fixtures inject a zero delay except in the tests that pin it,
  because the production value is a wall-clock wait with nothing for a test to
  observe.

Origins: #493, #511, #512, #602 (spec), #603–#607, #650; ADR-0043, ADR-0050,
ADR-0056, ADR-0059, ADR-0060, ADR-0061, ADR-0066, ADR-0070, ADR-0072.
