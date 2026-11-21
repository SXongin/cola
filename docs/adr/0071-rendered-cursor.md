# The Rendered Cursor: a restart projects only the delta after the confirmed render frontier

> **Amended by ADR-0072**: card write order now has an owner — the keyed
> per-card submission queue (chain generation + intent) — so the projection's
> ordering assumption below (the per-card delivery lock alone) reads under
> ADR-0072.
>
> **Amended 2026-10-15 by ADR-0082**: `release` hands the record's confirmed
> frontier to a Session-scoped delivered position, so "no long-lived state
> accumulates" holds except for that one fact.

## Context

A cola restart mid-answer had no single rule for what the user sees, so every
restart symptom grew its own mechanism: a still-live run whose card was orphaned
sat frozen or got the #443 stamp while its output after the last delivered PATCH
was never rendered (omission); the Fresh path deliberately does not replay, so a
run that finished while cola was down could only be settled by transcript truth
or announced by a Wake-scoped continuation; the message takeover carried the
orphan's running tool panels (ADR-0068) but not its text tail; and #505's
deferred faithful restore had no durable answer for "how far did this chain
render?". Duplication and omission were structural: whole-turn replay duplicates
everything the old card showed, a whole-body rebuild onto the existing card is
silent and drops Interaction Receipts, and "continue from now" omits everything
produced between the last delivered flush and the resumption. The Chain Record
(ADR-0069) knew the card, the Turn anchor and the directory, but not the render
boundary; six mechanisms (#443, #444, #424's Fresh gate, #454, #195, #426) each
policed one pairwise interleaving. This ADR records the finished mechanism (spec
#561, tickets #562–#566).

## Decision

**A Card Chain persists a Rendered Cursor on its Chain Record — the transcript
position whose content is confirmed delivered — and one reconcile pass projects
the chain's delta after it onto a successor card.**

### The fact: a frontier plus a live set, never content

The frontier names the newest delivered-final item of any content kind: a
text/reasoning part — its message identity, its ordinal in the message's typed
parts, its kind, its server start time and the Unicode character extent confirmed
delivered — or a settled tool panel, by position, with the extent then still
belonging to the newest text/reasoning part at or before it (so that part's
growth stays renderable). A still-`running` tool is never the frontier: it rides
the **live set** — the tool call ids whose newest delivered state was `running` —
for the display-only carry and settle-once resolution. The cursor stores position
and identity only: #505's rejected payload journal (built card JSON) stays
rejected — the content lives on the server, and a boundary is all the recovery
needs.

`None` on a record is valid — cursorless — and means an older release's record or
a chain whose first confirmed write has not landed. The frontier's identity is
the message, the ordinal, the kind AND the part's server start time: a read
carrying a different part in that slot resolves nothing and the projection falls
back rather than skip a replacement's prefix. The delivered extent counts only
what the card could DISPLAY: a reasoning entry is clipped to the card's reasoning
cap (800 characters plus the builder's ellipsis), so the extent clamps to it and
the digest covers exactly that clamped prefix; the characters beyond the cap
render as a continuation after a restart instead of being skipped. Because V2
decodes text parts without `time.start`, identity alone cannot tell growth from a
same-slot replacement there: the frontier also stores a small **digest of the
delivered prefix** — derived metadata, never content — and resolution requires
the read's prefix of that length to hash to it. A text/reasoning frontier whose
start time or digest no longer matches the read is not dropped — that would lose
its tail — but resolved as **cut 0**: that part renders in full while everything
before it stays delivered, so a rewrite duplicates nothing (the old content is
gone from the read) and a growth still renders only its tail.

The same rewrite seen live, from the read that carries it, replaces the part's
timeline entries instead of accumulating the old content under the new snapshot —
the replacement is the part's whole content now — and stamps its prefix digest at
the new delivered extent (the replacement's length clamped to the card's display
cap), so the persisted cursor describes exactly what the card shows rather than
the double-counted sum (issue #1025). A cursor written by an older release
carries no digest and therefore resolves nothing (the cursorless fallback by
another name); the first confirmed write after the upgrade gives it one.

### Advance only on a confirmed write, through the existing delivery choke point

The accumulator stages the cursor of the body it is about to write, exactly like
the Wake Watermark's staged mark; the flush — or the projection's successor
create — drains it only once the write is confirmed: a PATCH `Ok`, a create `Ok`,
and a payload the Pending Card Update drain delivers later (a recoverable failure
ties the stage to that payload's delivery sequence, and Session Sync's post-drain
reconcile confirms it once the payload lands). A permanent refusal or a failed
create drops the stage, so a write that reached no card can never hide content
from a later recovery. `advance_cursor` writes only the record naming the card the
write landed on, so a stale flush cannot touch a successor that already took the
chain over.

Every confirmation also names the exact stage its write carried — the stage
generation in the accumulator, and for the drain the outbox sequence whose
delivery was verified — so a body staged since (a fresh flush whose PATCH is still
pending) is left for its own confirmation and can never be advanced by an earlier
write; the Wake Watermark's drain carries the same stage identity. Stages are
therefore RETAINED until their own write resolves them, not kept in a single
pending slot: a delivered-but-unconfirmed stage stays confirmable after a newer
body is staged — the reconcile walks every retained stage and confirms each by its
own sequence, drops one whose entry settled without delivering it, and keeps a
still-owed one. The outbox answers only for a sequence's OWN outcome: the
reconcile confirms a stage by the exact sequence it was tied to, and a failure
note asks about the sequence ITS payload failed at — remembered alongside that
payload — so a newer write's success, a cached repaint that may omit the older
body's delta, can never confirm an older stage and let a restart skip content no
card received. The delivery layer keeps a **bounded set of the sequences proven
delivered** per card: a successful write adds its own sequence and the drain adds
the retried payload's, so a payload a drain delivered stays confirmable even
after a newer write FAILS and replaces the owed one, while a repaint still
answers only for its own sequence.

The confirmation's cursor advance and the gap's advance/clear share ONE record
write: two updates could leave a crash window with the cursor ahead of the gap,
and the recovery would re-render gap content a card already showed. A stage older
than the one the base already reflects is dropped unapplied, and the stage
comparison and the durable record write happen in ONE cards critical section, so
a concurrent confirmation can never interleave between them and an older
confirmation arriving after a newer one was applied is discarded — the cursor only
ever moves forward. The Wake Watermark's stages are retained the same way, and
`ChainRecords::advance` keeps the durable mark monotonic by `created_ms` as the
second guard. A failure note whose payload the drain delivered before the note
looked up its sequence confirms the stage then and there (the settled delivery
verdict), instead of discarding a write that reached the card. The per-write cost
rides the network PATCH it follows; this reopens ADR-0061's rejected
render-frontier watermark, and the affordability answer is recorded in that ADR's
amendment.

### The cursor is chain-level; release hands over one fact (ADR-0082)

A re-point within the chain — a split's continuation, a new Turn on the same
chain, a takeover — carries the cursor; a genuinely new chain starts cursorless;
`release` drops it with the record. `release`/`release_if_card` also hand the
record's CONFIRMED frontier to a **Session-scoped delivered position**
(`ChainRecords::delivered`: one `session_id → message` entry) in the same atomic
write that drops the record. It exists for exactly one reader — the
session-scoped compaction walk, whose exactly-once must hold ACROSS Turns while
the cursor that carried the bound dies with every terminal card. A cursorless
record hands over nothing, and what is handed over is the confirmed frontier,
never a stage a write may still be owed or a refusal discarded: a compaction no
card actually delivered still renders on the next one.

### One projection for both restart cases

The reconcile's decision ladder maps a cursor-carrying record whose run this
process does not own to one of two outcomes sharing one seed, one successor
renderer and one delivery rule. An ended-while-down (or run-lost) chain
**projects and settles**: the missed tail plus the transcript's true ending
(✅ / ❌ / ⏳) onto a successor. A still-live chain **projects and follows**: the
successor is armed seeded at the cursor, its create is the restart notification,
and the existing external-render arm streams the run's later content and settles
it by transcript truth, never an invented interruption. Only the live case adds
the follow phase. An ended run with no deliverable target claims nothing — it
keeps its record for a later pass rather than taking an in-place settle — and a
live one is not adopted.

- **The reply target is durable.** The successor replies to `reply_to` — the
  Feishu message the chain's cards reply under (the accumulator's reply target at
  track time), carried across re-points like the cursor and absent for an older
  record — then the recorded card, then the chain's top-level Chat. A DEFINITE
  non-delivery advances to the next rung instead of retrying the same target
  (issue #580: an OpenCode message id in Feishu's `/reply` path 400s forever);
  when every rung refuses, the record stays retryable for the next pass. The
  successor's accumulator adopts the rung its create actually landed on, so later
  splits thread there too, and a record with no deliverable rung claims nothing —
  never a settle PATCH to an empty card id, and never a write aimed at an
  OpenCode id.
- **The transition is atomic.** The re-point and the create's cursor confirmation
  are ONE write, inside the same takeover critical section: the record must never
  be observable naming the successor card while still carrying the predecessor's
  frontier, because a fresh Turn that snapshots it in that window seeds the
  already-delivered tail onto its own card while the collected successor keeps
  its body — the same text twice. The old card's collect PATCH and the Wake
  Watermark drain follow that atomic transition, and no awaited call sits between
  the transition and the confirmation.
- **The chain's future decides an empty delta.** A `Waiting` settle on a
  cursor-carrying record with a deliverable target sends its armed successor even
  when the delta is empty — a waiting card carrying the Background Task Ledger,
  nothing replayed — and collects the recorded card as taken over; its create is
  the restart notification, and the completion Wake's `ResumeInPlace` decision
  (ADR-0066) then resumes that successor in place, so one card carries the chain
  from the restart through the resumed run to its true end (issue #583). Each
  restart during the same wait sends one successor — the same per-restart card
  the live adoption already costs — and collects the previous one; no durable
  "already projected" mark is added for it. A stranded task (#454, no completion
  Wake) leaves the empty waiting successor as the newest card with the content
  card collected, and the pinned ledger on the successor is where its cleanup
  surfaces. The ended cases are unchanged: a terminal settle with an empty delta
  drops the successor and settles in place; a terminal settle with a missed tail
  still projects — content delivery, not future. A live adoption is always sent:
  the run may produce next, and the follow is what keeps it live.
- **The collect keeps what it cannot repair.** Both outcomes collect the recorded
  card as taken over — dropping the Background Task Ledger and, per call, each
  running `⏳` marker the successor actually resolved (ADR-0068's rule,
  generalized; a marker whose call the read did not carry stays as a frozen
  witness).
- **The delta splits like any card body.** An oversized delta goes through the
  SAME splitter as every card body, as a bounded chain of creates (≤ the existing
  chain bound), one slice per card, in order, each slice's cursor confirmed with
  the same critical section that attaches its card, only after its own create
  landed: a failed or bound-stopped chain leaves the record at the last confirmed
  slice, re-posts nothing in that life and lets the next life's projection resume
  from the cursor. For a LIVE adoption that stop is not the end of the run: the
  last landed slice's card is still handed to the follow, whose own
  flush/continuation machinery carries the remaining delta and everything the run
  produces next — the single-shot mark blocks further projections, never a
  follow. An ended projection's stopped chain keeps today's semantics (the
  successor already carries the transcript's ending, and the next life resumes
  from the cursor); a stop with NO landed card (an ambiguous first create, a lost
  window) follows nothing for either case.
- **A projection's create is single-shot per record per process life.** Feishu
  offers no idempotency key, so a create whose outcome is not a definite
  non-landing — any transport or API error — may have landed; retrying it would
  post a duplicate successor. The attempt is marked in memory, like the other
  reconcile marks, and no later pass this life re-posts: the record stays, the
  old card is state-repaired in place by transcript truth, and the ambiguous card
  is left as it fell. The single-shot fact is also written AHEAD durably: before
  every successor create — the first and each chain continuation — the record
  carries a projection intent (the same sidecar, the same atomic write), so a
  process that dies between the create landing and the takeover's re-point leaves
  it behind; the next life treats the create as ambiguous and never re-posts, and
  the old card takes the state-repair path. The intent is consumed by the
  takeover's re-point (a fresh record carries none; a fresh Turn's takeover
  CARRIES it, like the cursor and the gap, and the projection's lost-race
  late-card collect clears it on the winning chain, so an unresolved mark is
  never erased — and never outlives the create it covered) and cleared on a
  definite non-delivery, so a retry stays safe and a later life may resume a chain
  stopped by a definite slice failure from the last tracked slice.
- **Definite non-delivery is not single-shot.** A card-content rejection or an
  explicit 4xx refusal proves the platform created no message, so the record
  stays retryable: the next pass re-attempts, and a content rejection is
  re-rendered once through the flush's fenced fallback so the tail can still land
  (a content rejection that survives the fenced retry is suspended: single-shot,
  the payload can never land).
- **A lost race collects the late successor without its body.** A successor
  create that lands AFTER a fresh Turn already won the chain is collected
  **without its body** — the winner's message-first seed re-rendered the same
  tail, so preserving it would make the reader read the text twice. That is the
  one deliberate departure from ADR-0063's body-preserving collect. A chain that
  stopped mid-way keeps its record instead: the tail past the last confirmed slice
  is still owed, so a terminal card's usual record discard is suppressed for it
  and the next life resumes from the cursor.

### One timeline entry per transcript part

Every text/reasoning part keeps its own timeline entry and `PartSource`, even when
two parts of one message share a server time: the frontier's identity is the
part, so an entry merged from two parts would carry the first part's digest over
both extents, resolve to a cut at 0, and re-render content a card already showed.
Chunks of the SAME part still merge; a chunk continuing a part whose equal-key run
another part's entry follows is inserted beside its own entries, so the card's
content and order are unchanged.

### Render seeding

The seed resolves the cursor against the read: everything at or before the
frontier counts as delivered — marked per MESSAGE, not by content alone, so the
seeded suppression and the Wake step's content diff can never replay the orphan's
own parts while a NEW Turn's part with identical text still renders — and the
frontier part renders only its undelivered suffix, with a markdown lead (a
reopened fence, a repeated table header/delimiter) when the cut lands inside a
construct. That suffix-only cut holds only while the read still carries what the
successor delivered: the accumulated run's newest prefix digest must hash the
read's prefix of the delivered extent, and a live REWRITE — a read whose prefix no
longer does — replaces the part's run with the rewritten content in full, exactly
like the ordinary render's rewrite rule. Cutting a rewritten part at the old
extent would push a stray suffix and let the cursor claim the replacement
delivered. The frontier's identity rule covers tools too: a settled-tool frontier
whose recorded start time no longer matches the slot — with or without a text
extent before it — is a REPLACEMENT, and it renders (cut 0) rather than being
skipped, so its result is never omitted.

The live set resolves by call identity against the whole read on every render
read: a still-running call rides the successor's live tail display-only and a call
that settled while cola was down joins its timeline exactly once, at the server
start key it was born with. Only calls the read actually carries resolve, and the
collect strips per call: exactly the running `⏳` panels whose call the seed
resolved leave the old card (identified through the panel's call-named element id,
`tool_{call_id}`), while a call outside the read — a V2 transcript truncated at
its page cap — keeps its panel on the collected old card as a frozen witness
rather than being stripped from a card the successor cannot repair.

A truncated read is a prefix, and no projection settles on it. The V2 transcript
read pages to its cap (`MAX_MESSAGE_PAGES`) and returns what it has; the neutral
`SessionTranscript` says so with `truncated` (V1 reads are unbounded and always
false). A truncated read can place a cursor inside its returned prefix while
newer content sits unseen beyond the cap, so an ended projection claims nothing
while the read is truncated — no settle-as-complete, no released record — and a
LIVE record claims nothing either (the truncation is the OLDEST prefix, so
following it would re-read that same prefix forever). A later complete read
adopts or settles normally, and the message-first seed does not resolve against a
truncated read (its gap is never marked complete, so the unseen tail still lands
once a complete read shows the gap's end).

### The message-first race and the durable orphan gap

When a user message wins the race against the adoption, the fresh Turn's takeover
seeds its card from the same cursor, scoped at the orphaned Turn's window: the
orphan's final undelivered tail renders once, as a continuation, before the new
Turn's content, and the live set resolves by identity. A record with no cursor
keeps the retired carry's semantics — the orphaned Turn's still-live calls,
nothing replayed.

A cursor-bearing orphan's tail is never dropped: a seed the takeover's read could
not resolve — a failed or timed-out read, or one that cannot place the cursor —
becomes a **durable orphan gap** on the Chain Record (its `cursor` is the
orphaned chain's confirmed frontier, its `anchor` the orphaned Turn), while the
chain's cursor keeps advancing with the new Turn's own confirmed writes. One
frontier cannot express "gap undelivered, later content delivered", so the gap is
a fact of its own rather than a pin at the gap's frontier. A takeover that seeds
from a cursor records the gap durably whether or not its read can place the
cursor — in the SAME write that re-points the record, before that read even
starts: the old card is collected right there, so a crash before the new Turn's
first confirmed write would leave the record with the old cursor and the new
Turn's anchor — a recovery that cannot see the orphaned Turn's window and would
omit its tail (and, with the new message never submitted, would settle Unreceived
and release the record instead). An Unreceived ending on a record that still owes
a gap therefore claims nothing either: only a later Turn's takeover, which
carries the gap, lands the tail. The in-process render (the resolved seed's scope
walk, which tags its entries as gap content and marks the accumulator) never
clears it: only a confirmed write covering the gap's END does, so an unconfirmed
render is simply re-rendered by recovery.

Rendering the gap is a walk of the gap's own Turn window, cut at the gap's cursor
and bounded by the Turn the chain moved to — the gap records that message,
falling back to the read's next user message, because `turn_for_user` carries no
upper end — whose pushed entries carry **no source**: a successor built from them
can never offer a frontier older than the record's, and the delivered content the
cursor already covers — the answer of the Turn that followed the gap — is never
re-rendered. The gap lands exactly once and is then consumed: a projection takes
it with the re-point that rewrites the record (a gap the read could not place
rides onto the new record), a fresh Turn's takeover that finds one re-homes it
onto the new record and renders it on its own card, and every confirmation
advances or consumes the durable fact exactly as far as ITS body reached: the
stage carries the body's **gap coverage** — the gap's cursor at the newest gap
content that body includes, and whether it reached the gap's last entry — so a
partial body (a gap split across cards) only advances the gap's cursor, and a
restart, or the next slice, resumes after what a card already showed instead of
repeating it; the fact clears only when the body carrying the gap's END is
confirmed. A write built before the gap rendered carries no coverage and can
neither clear nor advance it. The re-point carries the gap like the cursor — a
continuation or a fresh Turn cannot drop an owed tail — and the chain cursor never
takes a gap entry as its frontier: the gap's content sits EARLIER in the read than
the confirmed frontier, so the accumulator tags the messages its gap walk rendered
and the frontier skips them. An ended record whose gap this read cannot place
claims nothing (`Keep`: a settle would drop the fact forever), and the record is
not released while an unrendered gap is owed.

No interlock is introduced: the projection relies on the existing per-card
delivery lock for ordering (#527's no-hold property), and since ADR-0072 the
keyed queue owns the order (below).

### The Wake Watermark still closes the announcement

The projection's confirmed create drains the staged Wake Watermark through the
same choke point the flush uses, so a Wake completion entry the successor rendered
is durable and a later, by then recordless Fresh post cannot re-announce it
(#424). The mark also flows the other way at arm time: every accumulator that
starts rendering a session — the fresh Turn, the projection and external arms,
the Wake continuation — seeds a **Wake floor** from the chain's durable mark, and
the wake-entry render skips any Wake at or below it: its completion entry was
already announced by an earlier card, so it is neither re-inserted nor staged for
a new advance. The floor is time-based, not an id set, so older Wakes below the
newest announced id are covered too, and a Wake strictly newer than the mark
still renders and announces.

### The cursorless fallback is today's behavior

A record carrying no cursor keeps the existing arm unchanged: the #443 one-time
restart stamp for a still-live orphan, the in-place settle for a
transcript-decided ending, the collect arms, and the Fresh path's Wake Watermark
gate for recordless posts. A cursor-carrying record is never stamped, however
unplaceable its frontier: an unresolved cursor, a missing read or no scope claims
nothing and leaves the record for a later pass the projection can seed. The ENDED
case is the same rule: a cursor this read cannot place keeps the record —
settling the card in place would release the tail the cursor still guards — and
only a cursorless record keeps the one-release in-place fallback, while an
Unreceived ending is its own terminal (ADR-0062: no anchor to continue onto, so
nothing can be projected). The fallback window closes by itself — the first
confirmed write gives the record a cursor.

### Ordering, and one refused slice (ADR-0072, #886)

The delivery lock serializes the writes themselves; their ORDER comes from the
keyed per-card queue (ADR-0072). The projection's card writes run through it —
the recorded card's collect, the message-first race's late-card collect and the
settle all submit under the chain generation they read — and the projection's
re-point bumps that generation, so an older writer's submission (the #443 stamp's,
a stale collect) is dropped by generation instead of racing the lock. The
successor's create stays what it is, a create with no idempotency key. The
no-hold property stands: no lock is held across a read, the queue is async and
bounded, and the projection still acquires no lock of its own.

When Feishu refuses the SAME payload twice (plain and fenced), the flush
**abandons the slice**, not the card: it skips the refused timeline range, leaves
one visible `⚠️ 部分内容未能显示` marker on the continuation, and continues the
chain on a NEW card; that continuation's confirmed create advances the record's
cursor past content no card received (#886). This is the ONE deliberate exception
to "the frontier is content confirmed delivered": a cursor is a single frontier
and cannot express "later content delivered, earlier content dropped", so the
drop is recorded where the user can see it — the marker on the card — and at WARN
with the payload's size, component count and slice. The alternative (never
advancing the cursor) would re-render the refused content forever, which is the
failure #886 names. Two guards keep the exception narrow: **only refused content
is dropped** (the build fingerprints its slice's content — a hash of the entries'
values, not their lengths; if a streaming text/reasoning part, an updated
compaction row, or a settled tool panel's asynchronous file delivery changed
while the refused write was in flight, the slice is NOT skipped: the boundary is
kept and the changed slice is attempted on the next flush), and it is **bounded**
(at most two consecutive abandonments per Turn without a delivered write; a
delivered write clears the budget; a card whose SHELL is refused has nothing to
skip, keeps the conservative refusal, and retries on the next flush). The
`Forbid` ledger refresh (ADR-0060) keeps the old latch — it must never post a new
card, so it cannot abandon a slice — while a yielded card's SETTLEMENT is a true
ending rather than a ledger update, so it uses the normal split policy and may
continue on a new card.

### One rule for both generations

The reads the projection needs (directory-routed session status, the typed
transcript, the external follow) are generation-neutral, and V1 naturally has no
Wakes; no second code path decides what the user sees.

## Considered options

Rejected: keeping the pairwise mechanisms (the mechanism count is itself the cost,
and a missed mirror stays invisible until it bites a live session); a payload
journal (#505: cola would become a second store of what OpenCode owns); a
per-Turn or per-card cursor (the chain is the unit the user reads, and a split
re-points the record anyway); a separate sidecar (one home, one atomic write and
lifetime); whole-turn replay, whole-body rebuild or continue-from-now (the three
structural failure modes this decision exists to end); reconstructing the
delivered state by content comparison (no durable boundary — the same content may
move or compact away); stamping an unplaceable cursor (it would freeze the run the
projection is supposed to follow); pinning the chain's cursor at the gap (it would
re-render delivered content that follows the gap); retrying a lost create (no
idempotency key, so a duplicate successor); holding a lock across the projection
(the delivery lock orders writes, and ADR-0072's queue is the order — no
interlock is added).

## Consequences

- A restart mid-answer keeps one live thread: a still-running run is followed
  onto a successor showing only the undelivered delta, a run that finished while
  cola was down shows its missed tail together with its true ending, a tool
  crossing the restart keeps its live panel or lands its result exactly once, and
  no collected card is left looking busy. The successor's send is the restart
  notification the #443 stamp never was.
- Duplication and omission are one rule: the successor renders strictly after the
  confirmed frontier, character-exactly, and the old card is collected as taken
  over rather than rebuilt (its body minus the live markers it handed over).
- Retirements: #443's stamp narrows to the cursorless fallback (one release);
  ADR-0068's carry retires into the seed; the Fresh gate narrows to recordless
  posts, where the Wake Watermark keeps its exactly-once meaning; #505's deferral
  lifts. #528 and #529 are closed by the adoption.
- Boundaries hold: Interaction Receipts stay in ADR-0038's request-flow domain
  (still-pending requests re-host onto the successor through the existing sweeps;
  resolved receipts are not rebuilt), and the Pending Card Update outbox and the
  runtime-retirement overlay keep their owners.
- Tests pin the rule where the symptoms live: the restart matrix seeds a cursor,
  restarts over the sidecar, runs the real Session Sync pass and asserts the
  successor's content against the old card's delivered state character-level (no
  duplication, no omission) for the ended and live cases, the message-first race,
  the cursorless fallback, the cursorless stamp's untouched suites, and the
  confirmed-write advance (a transport failure, a content rejection, and the
  drain's late delivery). The store seam pins the cursor's round-trip, `track`'s
  carry, `release`'s drop and the legacy cursorless read.

Origins: #424, #426, #443, #444, #454, #505, #522, #527, #528, #529, #561 (spec),
#562–#566, #569, #580, #583, #721, #886, #1025; ADR-0038, ADR-0060, ADR-0061,
ADR-0063, ADR-0066, ADR-0068, ADR-0069, ADR-0072, ADR-0082.
