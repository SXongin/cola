# In-place Wake resumption: one card per request

## Context

ADR-0059 made a Turn's card yield 「⏳ 等待后台任务」 when its Execution ended
with Background Tasks still live, and gave every Wake a continuation card
split below the user's message. The 2026-09-29 acceptance test read that as
verbose: one user request produced one card per completion, each opening with
the same 承接 line 「🔔 已恢复执行，继续处理…」, while the folded completion entry
stayed on the old card and the resumed work moved to a new one. Worse, a
continuation that has no reachable reply target can never continue at all: in a
lobby (and after a restart, whose continuation card is posted top-level) the
next Wake's split finds no reply target, `render_wake_continuation` warns
"has a chain but no reachable reply target" on every pass, and the resumed
work never renders (live 2026-09-30, recorded on #426).

The design round (2026-09-29, confirmed with the maintainer) asked for **one
card per user request**: a Wake that resumes a yielded card continues ON that
card, in place — the completion entry, the resumed content and the eventual ✅
all live on the same card. The boundaries below were settled in the ADR round
(2026-10-01). This ADR records that model.

## Decision

**A shell/subagent completion Wake resumes a yielded (⏳) card in place; every
other continuation keeps today's chain behavior.**

Precisely:

- **Trigger.** The Wake step (`Turn::wake_continuation`) picks the in-place
  handoff iff the chain's card is `Waiting` and the newest placeable Wake —
  the completion whose work the continuation would render — is a shell or
  subagent Wake the card has not yet announced. Restart and interrupt Wakes,
  the Wake-less content-diff fallback, and a card already at a terminal
  (✅/❌/⏹) keep the ADR-0059 split. Terminal cards are the third new-card
  exception the design round's shorthand left implicit: a Wake arriving after
  an ending is a race or a late tail, and re-opening a ✅ card would misread
  the ending it recorded.
- **Resumption.** Under the session's card-write lock the same accumulator
  takes one ledger read — the retiring Wake's fixed completion entry lands at
  its own moment and the remaining live list stays — then the card takes
  「🔄 后台任务完成，继续处理中…」 (a new `CardState::Resuming`: render-owned, so
  a mid-resume user message is a Supplement, and non-terminal) and the shared
  out-of-turn settle loop renders the resumed work into that same card. No 承接
  line is written: only the post-restart Fresh card carries one (it genuinely
  has no context). The completion entry is the timeline marker — the record
  stays where the task lived.
- **Back to waiting, or the true end.** A resumed run that ends with Background
  Tasks still live yields the card back to 「⏳ 等待后台任务」; one that reaches
  the true end settles it there as ✅/❌/⏹.
- **One notice.** An in-place update never notifies. The Completion Notice
  fires at the true end under its existing rules (groups per
  `[bridge] group_completion_notice`, p2p past the long-task threshold; the
  clock is the request's original Turn start, exactly as the quiet true end
  already reads it) and exactly once: the quiet true end and the resumed run's
  ending are mutually exclusive by card state. The restart Fresh card stays
  notice-free — its own send was the notification, and that remains its one
  notification.
- **Failure keeps Retry.** The in-place card is the request's card and still
  carries its prompt, so a failed resumed run ends ❌ with the ordinary #391
  重试 — which also keeps the ❌ notice's 「可点击卡片上的『重试』」 copy true.
  The rule that a Wake continuation offers no Retry (ADR-0059) now covers only
  the cards armed without a user prompt: the Fresh card and the remaining split
  continuations.
- **The ledger stays.** The live list no longer leaves the card for a
  completion — the resumed run renders on it — so ADR-0060's handover now
  serves only the splits that remain (size, supplement, terminal-card
  continuation, Fresh).
- **Watermark.** The in-place PATCH that carries the completion entry is the
  announcement; it drains the staged Wake Watermark (ADR-0061) exactly as the
  承接 line's send did, so a restart cannot re-post the Wake.
- **Mid-resume user message.** The card is live again, so the existing
  Supplement rule applies unchanged (ADR-0043): the message steers into the
  run and the chain splits below it. A message on a merely-waiting card still
  starts a new Turn and collects the card (ADR-0059).

## Considered options

- **Keep the new card per completion (ADR-0059's shape).** Rejected — the
  acceptance test's verbosity, and the lobby/restart gap that cannot continue
  at all, are the bug this changes.
- **Resume any same-life card in place, terminal cards included.** Rejected —
  a ✅ card flipping back to 🔄 reads as broken; the terminal case is a race
  and a fresh continuation card is honest about it.
- **Resume every Wake source in place.** Rejected — restart/interrupt
  continuations are not task completions; keeping their chain behavior holds
  the change to #426's scope.
- **Bring the Wake-less content-diff fallback in place too.** Rejected for now
  — there is no completion to name, so the 「后台任务完成」 header would lie or
  need a second copy; the rare late-tail case keeps the split and can become
  its own follow-up.
- **Compose the resuming card out of an existing state (Streaming plus a
  flag).** Rejected — `CardState` is the one vocabulary every predicate reads
  (render-owned, awaiting override, phase timer, reap); a second flag would
  drift from it.

## Consequences

- ADR-0059's continuation model is narrowed for shell/subagent completion
  Wakes: the 承接 card is no longer their default; the waiting card wakes in
  place. ADR-0059 carries the amendment banner.
- ADR-0060's freeze carve-out widens: a yielded card may receive resumed
  content, not only its ledger and a quiet true end; its handover only serves
  the remaining splits. ADR-0060 carries the amendment banner.
- CONTEXT.md's Card, Wake, Waiting on Background Work, Turn and Background
  Task Ledger entries record the in-place resumption and the resuming state.
- The background-completion continuation tests (#417–#420's batch) are
  rewritten to the new contract rather than deleted: one card carries the
  entry and the resumed work, the header cycles waiting → resuming →
  waiting/✅, a content-ful Wake posts no card, a mid-resume message still
  supplements, a size overflow still splits, and the true end notifies once.
  The lobby case from #426 gains its regression test: a top-level (restart)
  continuation receives a later Wake in place with no reply target available.
- #424's durable Wake Watermark machinery is unchanged and still required: the
  in-place path stages and drains it like the split path did.

Related: #426, #424, #412, #403, ADR-0059, ADR-0060, ADR-0061, ADR-0062,
ADR-0043, ADR-0038, ADR-0058.
