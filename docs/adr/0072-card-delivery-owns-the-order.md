# Card delivery owns the order: keyed per-card submissions serialize every writer

## Context

A cola restart could orphan a still-live Session's card (#443): the reap stamps
it 「⏳ 已重启，等待运行结束」 while the Session reads live, and a successor's
takeover collects it later. The promise "a takeover's collect always lands after
a restart stamp" belonged to no one. It was derived from three locks, one
detached `tokio::spawn` that must never be cancelled (a cancelled write could
still commit at Feishu and land over a successor's collect), an in-flight claim
that froze every other reap decision for the record while the write was owed, a
post-read ownership pre-check, a post-PATCH repair collect that reproduced a
strip rule recorded on the successor's record — the wrong object — and two
in-memory marks suppressing re-stamping and #522's deterministic refusal loop.
Every fix added the next mechanism, and each new race had to be reasoned about
pairwise: "who writes this card last" had four owners and no single one to
point at. The trigger was rare, but a card a new message took over could still
flip back to the interim 「已重启」 status, repaired by a second PATCH the user
never asked for; the whole behaviour rested on the repair not being wrong.

The architecture review (2026-10-05, candidate 5) named the deepening; the
design round (2026-10-07) settled the shape, and spec #571 landed it (tickets
#572–#575). This ADR records the finished mechanism.

## Decision

**The card-delivery decorator owns per-card write order.** A writer that needs
its write ordered against the card's other writers submits a **Keyed
Submission** (GLOSSARY): an already-composed payload with the ordering key it
decided under — `(chain generation, intent)`. The decorator's per-card state
machine (merged with the Pending Card Update outbox, ADR-0067) then serializes,
drops stale intents, collapses duplicates and retries failures.

- **The key.** The first half is the **Chain Generation** (GLOSSARY): one
  in-memory monotonic version per chain, bumped exactly when the tracked card
  identity changes — a track onto a different card, a takeover's re-point, a
  projection's re-point — and carried across a re-track of the same card. It
  rides the record snapshot as a non-serialized field, so a decision captures
  it together with the record read it decided from; a fresh process life (and
  so a record loaded from disk) starts at zero, and no durable format changes.
  The reap's stamp and endings submit the generation of the record snapshot
  their decision used, and the collects read the chain's **current** version
  at submission (`ChainRecords::generation`), so a takeover that landed since
  the caller's own snapshot outranks that snapshot's older intent instead of
  being refused by it. The one exception is the reap's **lagging handover**
  (spec #571 review): its successor collect carries the handover's own
  generation — the lagging record's + 1, exactly the generation the re-point
  assigns — decided at hand-off, because the record may still name the orphan
  (or be released instead of re-pointed) when the detached task runs, and a
  read at submission would then name the lagging generation, or none at all
  once released: the old chain's late stamp at that generation could be
  admitted beside the collect and land after the takeover notice. The second half is the intent vocabulary: `Stamp` (the
  #443 restart stamp), the three collect variants — `Collect` (the ordinary
  collect: the reap's successor collect, the Wake continuation's arm, the
  external arm, keeping the whole preserved body), `TakeoverCollect` (the
  fresh-Turn message takeover's collect, which strips the orphan's live tail,
  ADR-0068) and `LateProjectionCollect` (the late projection's collect, which
  reduces the late card to the bare marker) — `Yield` (the waiting ending
  「⏳ 等待后台任务」) and `Settle` (the terminal ending). The collect variants
  are distinct intents because their payload rules differ (spec #571 review): a
  reap collect and a takeover's strip collect may share a generation and must
  both land — the strip collect last — rather than one collapsing into the
  other as a same-key duplicate. `(generation, intent)` names one logical
  write.

- **The queue rules.** A submission whose generation is below the card's
  **floor** — the lowest generation the card still accepts, raised to every
  accepted submission's own generation and past G+1 by an accepted `Settle` at
  G (below) — is dropped without a Feishu call: a newer generation owns the
  card, or a settle closed this one. Same-generation submissions land in
  submission order, one in flight and one waiting; a newer submission replaces
  the waiting slot, so the queue holds at most two and a submission displaced
  before its write settles its caller as `Superseded` instead of hanging. A
  generation advance also invalidates older **waits** (spec #571 review): a
  waiting write below the new generation is dropped (`Superseded`) the moment
  the newer generation is admitted, mirroring the forgotten keys, and every
  promotion of the waiting slot re-validates the waiter against the current
  state — a newer generation since, a **generation the floor has closed** (an
  accepted `Settle` at G raises the floor to G+1, so a waiting non-settle write
  of G may never land after the true ending; a `Settle` waiter of G stays
  promotable, because accepting it closed its OWN generation), an accepted
  ending's shadow over a `Stamp`, or a settled key drops it — so a stale waiter
  can never be written after a newer state. Accepting a `Settle` also drops
  such a stale waiter right away, belt-and-braces. A
  duplicate `(generation, intent)` collapses to one write: a
  **delivered** key answers every later re-submission `Delivered` without a
  second write (per-tick re-decisions are free), a **permanently refused**
  key answers every later re-submission `Superseded` without a call, and a
  same-key submission already waiting behind the key's in-flight write is
  dropped (`Superseded`) the moment that write lands — one key, one PATCH
  (spec #571 review). The newest generation's delivered/refused key states are
  remembered per card — **order state**, protected from the cap within their
  window exactly like a raised floor or an ending shadow (spec #571 review), so
  the coverage answer that stops a refused write's re-read and re-refusal
  survives cap pressure; a newer
  generation forgets the older generation's — every older submission is
  dropped by the floor anyway. The queue also answers a read-only coverage
  query (`keyed_write_covered`): a writer that re-decides every tick asks
  whether this exact key is already settled, covered by an accepted ending, or
  being written, and skips its card-view read and submission when it is. The
  query only reports; it never orders, claims or drops anything.

- **The ending rules.** A terminal `Settle` is not just another same-generation
  write: accepting one at generation G **closes** G — the floor rises to G+1 —
  so any later submission at ≤ G (a late stamp whose view read outlived the
  ending, a stale collect) is dropped, while the settle itself settles the
  key. A `Yield` (the waiting ending, which is not terminal) accepted at G
  **shadows** G only: any later `Stamp` at ≤ G is dropped, but the generation
  stays open, so the true end's `Settle` at the same generation still lands
  after it and owns the card's last word. The shadow state is kept while a
  stale stamp could still arrive. Both endings remain the writers' composed
  payloads — a settle keeps the card's body best-effort — and a preserved
  ending a platform refuses as card content still degrades to the bare ending
  (ADR-0063), but as the submission's **fallback payload**: the driver tries it
  under the same held card lock and the same `(generation, intent)` key, so the
  degradation is one ordered write and can never land over a newer generation
  (spec #571 review).

- **The permanent-refusal outcome (#522's home).** A write the platform
  permanently refuses (a typed card-content rejection) settles its
  `(generation, intent)` key as refused: later same-key submissions are
  dropped without a Feishu call, so a card that cannot render the payload is
  never retried forever. A write carrying a fallback payload tries that first,
  still under the same key; only a refusal of the fallback (or a write with
  none) settles the key refused (spec #571 review). A recoverable failure
  instead stays owed with its key and its backoff, retried by the existing
  drain. That schedule belongs to the **logical key, not the payload**: a
  re-submission of the same `(generation, intent)` inherits the owed write's
  attempt count and next retry time, so re-deciding a failed key does not
  retry it early, while a submission of a **newer key** supersedes the owed one
  at once — convergence stays newest-wins under retry too (spec #571 review).
  This is the outbox's one permanent-refusal outcome and #522's give-up in one
  place, not a mark on any record.

- **The release gate waits for the keyed ending.** A terminal card's record is
  released only once its ending write is confirmed (ADR-0063's amendment).
  The gate (`has_pending_card_update`, via `release_spent`) now counts a keyed
  `Settle` the queue still owes or is writing, not only the keyless Pending
  Card Update; the reap's own settle releases the record it wrote for as soon
  as its ticket reports delivery. A **recoverable** failure keeps the record and
  the queue's owed key for a later retry (or a later transcript-truth settle),
  while a **permanently refused** ending is a confirmed non-delivery: the reap
  spends the record on it — delivery *or* refusal releases, exactly as the
  keyless outbox's permanent refusal always did (spec #571 review). The ending
  path also asks the queue whether its `(generation, intent)` already settled
  (`keyed_write_settled`: delivered, so the card holds it, or refused, so it can
  never land) *before* its preserved-ending card-view read, so a settled ending
  costs no read and no submission — the same discipline the stamp's covered-key
  query applies (spec #571 review).

- **One queue, two write classes.** Keyless writes (`update_message` — the
  flush, interaction repaints, command cards, the drain's retries) pass through
  unchanged: serialized on the same per-card delivery lock, never dropped for
  staleness. The per-card state machine, the entry cap, the delivered-sequence
  set and the backoff are the outbox's (ADR-0067); the keyed slots are added
  beside them. Across the classes, **a keyed state write supersedes an owed
  keyless payload** (spec #571 review): an accepted keyed submission (a collect,
  an ending) tombstones the card's pending payload — keeping the delivered
  sequences and the failure memory — so the drain can never resurrect a
  pre-collect payload over the newer keyed state and make a collected card look
  live again; a keyless write that fails *after* the keyed state is the newer
  writer and stays owed as today (newest wins within the keyless class). The
  key and intent types live at the delivery seam, so the
  Feishu layer keeps no dependency on the bridge. `Platform` gains
  `submit_ordered` with a default implementation that delegates to
  `update_message` — the unordered fallback for an unwrapped platform (tests,
  fakes) — and the decorator overrides it to enqueue.

- **No hold (#527's property preserved).** The queue is async and bounded: at
  most one submission in flight plus one waiting plus the newest generation's
  key states per card, and the outbox's entry cap bounds the map (a
  payload-less entry that still carries order state — a raised generation
  floor, an ending shadow, a settled key state — is never evicted while any
  other victim exists, and the entry a write is being admitted into is never
  evicted by its own
  admission; once no stale writer can still be composing — an order state older
  than a generous protection window, past every bounded card-view read — the
  oldest such state is the cap's last-resort victim). Above a **hard ceiling**
  — a small documented multiple of the cap
  (`MAX_PENDING × MAX_ENTRIES_FACTOR`) — protection yields (spec #571 review):
  state-only entries leave first, oldest first, then entries with owed payloads,
  and **never an entry whose write is driver-active** — that issued write can
  never be cancelled, and evicting its entry would lose the card's generation
  floor while the write is still in flight, letting a stale submission admit
  into a fresh entry and land over the newer state later (the permit pool caps
  how many such entries exist, well below the ceiling, so the hard bound stays
  enforceable). The trade is the same narrow stale-writer window
  expiry accepts, forced early. The **live driver tasks** are bounded by a
  global permit pool on the decorator (spec #571 review): a driver retains its
  payload while it awaits Feishu, so a spawn takes a permit for the driver's
  whole life and, with none free, the submission stays owed in its entry (no
  driver) for the next drain — repeated hung writes cannot grow tasks or
  retained payloads without bound either. Its own
  state lock is taken only to admit, snapshot or settle — never held across an
  await — and the card's delivery lock is held only across the Feishu write
  itself. A submission's write is owned by a spawned driver task, not by the
  submitting task, so cancelling a submitter cannot strand an admitted
  payload. The stamp's view read and composition still run on a small
  pre-submission task that the Session Sync pass never awaits — and so do the
  collects and the endings (spec #571 review): the pass hands the *whole
  pipeline* — the bounded preserved-card GET, the composition, the submission
  and the completion handling (the cache release, the INFO line, the terminal
  record release, the waiting mark, the warning) — to a detached task, so no
  Session Sync pass ever awaits a Feishu call at all, GET or PATCH. Only the
  queue's settled-key query stays inline (it issues no call).
  **An issued keyed write is never cancelled** (spec #571 review): a timed-out
  PATCH could still commit at Feishu and land over a newer generation, the very
  hazard the ordering exists to remove (ADR-0063's rule), so the driver awaits
  the platform call to completion and a hung write holds its card's delivery
  lock until it resolves. The drain re-arms a driver with a try-lock (a card
  another writer holds is skipped, never awaited) and **spawns** it instead of
  awaiting it, so the Session Sync pass never waits on a keyed write. What is
  bounded is the **waiting**, not the write: the driver's acquisition of the
  card lock issues no request, so its expiry reports the recoverable timeout,
  settles the submission's ticket and leaves the payload owed for the drain's
  next try; and every caller that needs the write's own timing — the collect
  releasing its cached card on delivery and warning on failure, the ending's
  confirmation, the stamp's pre-submission task — awaits the completion ticket
  with `KEYED_TICKET_AWAIT`, after which it proceeds without a verdict (the
  queue still owns the write, which may yet land). The **composition read**
  before such a write is bounded too (spec #571 review): a collect's or
  ending's preserved card-view GET is awaited with the preserved-view bound
  (`CardsHandle::preserved_view_timeout_ms`), and a read that times out or
  fails degrades to the bare ending — the ending never depends on the read,
  and the pass never waits on Feishu unboundedly (the #443 stamp's own view
  read takes the pass's configured read bound the same way). A collect that
  gives up on an indeterminate ticket releases its cache anyway — and so does a
  collect whose write failed, because the queue's retry may land later without
  the caller ever seeing it (spec #571 review): the release exists so a re-host
  cannot resurrect the collected presentation, and a
  collect that lands later would do exactly that. The keyless Pending Card
  Update retry keeps its own pre-existing bound (`DRAIN_RETRY_TIMEOUT`),
  outside this contract.

**The two amendments (2026-10-07).** The vocabulary gained the endings while
ticket #575 retired the claim, because each retirement exposed a window the
claim had indirectly gated:

- **The `Settle`.** With the claim's gate on the settle decision gone, a
  structural window appeared and was reproduced: a settle decided while the
  stamp's card-view read (bounded at 30 s; Session Sync ticks every 8 s) was
  still outstanding landed first, its delivery released the record, and the
  read then submitted the stamp at its snapshot generation — no rule dropped
  it, and 「已重启，等待运行结束」 was painted over the ✅ permanently. The
  terminal ending therefore joins the keyed vocabulary and closes its
  generation (above), which also retires the original boundary that left the
  collect×settle race unchanged: a stale settle no longer lands over a collect
  either.
- **The `Yield`.** The same retirement opened a stamp × waiting-ending window:
  a stamp whose view read outlived a 「⏳ 等待后台任务」 yield would paint
  「已重启」 over it for the whole wait, self-healing only at the true end. The
  waiting ending therefore joins with the weaker shadow rule (above), so the
  same-generation terminal settle still lands last.

**Accepted residual (documented, not fixed).** An attempt still composing its
card-view read is invisible to the coverage query, so during a hung read each
tick may spawn another attempt — up to ~4 concurrent card-view GETs before the
first submission. The PATCH count stays one (the first submission takes the
key, and every same-key attempt that arrives meanwhile collapses into it
without a Feishu call — spec #571 review), and the churn is bounded and
one-shot; resurrecting attempt-lifecycle state (the retired claim's fragment)
for it was rejected.

**Accepted residual: the hard ceiling evicts young order state.** Under a flood
of more protected admissions than `MAX_PENDING × MAX_ENTRIES_FACTOR`, the
ceiling's oldest-first eviction may drop a raised floor, an ending shadow or a
settled key state while its protection window is still open — a stale writer
from an older generation could then land over the newer state, and a refused
key's card would be read and refused once more (the state a fresh process
starts from). This is deliberately the *same* narrow
window the protection window already accepts at its expiry, forced early to
keep the map hard-bounded (issue #571: "an outage or a flood of writers cannot
grow memory without bound"); the window's own reasoning (a bounded card-view
read) still bounds how stale such a writer can be.

## Considered options

- **Keep deriving the order from locks, the detached never-cancelled write and
  the in-flight claim.** Rejected: the cluster is the reported cost — four
  owners of "who writes this card last", one wrong object (a predecessor rule
  kept on the successor's record) and a mechanism per interleaving, every one
  of them a way for a future writer to sit silently outside the ordering
  scheme.
- **Hold a lock across read–compose–PATCH.** Rejected: a hung Feishu call
  would freeze Session Sync, the very hold #527's no-hold property forbids; the
  view read stays unlocked, and the write is ordered by key instead.
- **Key only the stamp and the collects, not the endings.** Rejected by the
  two windows above: without a keyed terminal ending, a late stamp can still
  outlive a settle's record release, and without the yield's shadow it can
  outlive a waiting ending.
- **Move payload composition into delivery.** Rejected: the strip/keep rules
  are the writers' semantics (ADR-0068 — the collect's rule is its own payload
  composition), and the seam stays free of bridge types.
- **Persist the generation or the queue.** Rejected: ordering is process-local
  — a fresh life starts at zero, no stale key survives a restart, and in-flight
  payloads already do not survive one (ADR-0067). A durable format change
  would buy nothing.

## Consequences

- **Retired** (spec #571, ticket #575): the detached restart-stamp write task,
  the `restart_stamping` in-flight claim and every decision gate it fed, the
  post-read ownership pre-check, the post-PATCH repair collect, the
  predecessor keep rule recorded for it, and the `restarted_reaped` /
  `restart_stamp_rejected` marks. The stamp is one keyed submission dropped by
  generation or an ending shadow; #522's refusal is the queue's settled key.
- **Unchanged by contract**: fly/repaint/ending paths and the session write
  lock; keyless writes are never dropped for staleness; the accumulator's
  read-send-record sequences; V1 and V2 behave identically (an in-process
  refactor — no wire or durable format change).
- **Testability**: the queue rules (generation drop, same-generation order,
  `(generation, intent)` collapse, waiting-slot replacement, permanent-refusal
  settle and the silent drop of re-submissions, keyless passthrough) are pinned
  at the delivery seam; the ordering end-to-end (a stale stamp never lands over
  a takeover collect, no repair PATCH, the record released only after its keyed
  ending is confirmed, the #522 replacement path) at the bridge seam with the
  real decorator; the generation's bump points and in-memory life in the Chain
  Record store's tests. The retired guards' tests were rewritten as ordering
  assertions at those same seams, never a new one.

Related: #443, #522, #527, spec #571, ADR-0038, ADR-0063, ADR-0067, ADR-0068,
ADR-0069, ADR-0071.
