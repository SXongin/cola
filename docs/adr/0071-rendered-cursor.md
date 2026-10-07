# The Rendered Cursor: a restart projects only the delta after the confirmed render frontier

## Context

A cola restart mid-answer had no single rule for what the user sees, so every
restart symptom grew its own mechanism: a still-live run whose card was
orphaned sat frozen or got the #443 「已重启，等待运行结束」 stamp while its
output after the last delivered PATCH was never rendered (omission); the Fresh
path deliberately does not replay, so a run that finished while cola was down
could only be settled by transcript truth or announced by a Wake-scoped
continuation; the message takeover carried the orphan's running tool panels
(ADR-0068) but not its text tail; and #505's deferred faithful restore had no
durable answer for "how far did this chain render?". Duplication and omission
were structural: whole-turn replay duplicates everything the old card showed,
a whole-body rebuild onto the existing card is silent and drops Interaction
Receipts, and "continue from now" omits everything produced between the last
delivered flush and the resumption. The Chain Record (ADR-0069) knew the card,
the Turn anchor and the directory, but not the render boundary; six mechanisms
(#443, #444, #424's Fresh gate, #454, #195, #426) each policed one pairwise
interleaving.

The architecture review (2026-10-05, candidate 1) named the deepening; the
design round (2026-10-06) settled the shape, and spec #561 landed it (tickets
#562–#566). This ADR records the finished mechanism.

## Decision

**A Card Chain persists a Rendered Cursor on its Chain Record — the transcript
position whose content is confirmed delivered — and one reconcile pass
projects the chain's delta after it onto a successor card.**

- **The fact: a frontier plus a live set, never content.** The frontier names
  the newest delivered-final item of any content kind: a text/reasoning part —
  its message identity, its ordinal in the message's typed parts, its kind, its
  server start time and the Unicode character extent confirmed delivered — or a
  settled tool panel, by position, with the extent then still belonging to the
  newest text/reasoning part at or before it (so that part's growth stays
  renderable). A still-`running` tool is never the frontier: it rides the live
  set — the tool call ids whose newest delivered state was `running` — for the
  display-only carry and settle-once resolution. The cursor stores position and
  identity only: #505's rejected payload journal (persisting built card JSON)
  stays rejected — the content lives on the server, and a boundary is all the
  recovery needs. `None` on a record is valid — cursorless — and means an older
  release's record or a chain whose first confirmed write has not landed. A
  cursor written by an older release (text/reasoning kinds only) still loads:
  a settled tool that followed its frontier re-renders once on the first
  restart after the upgrade, then the new shape covers it. The frontier's
  identity is the message, the ordinal, the kind AND the part's server start
  time: a read carrying a different part in that slot resolves nothing and the
  projection falls back rather than skip a replacement's prefix (review #569).
  The delivered extent counts only what the card could DISPLAY: a reasoning
  entry is clipped to the card's reasoning cap (800 characters plus the
  builder's ellipsis), so the extent clamps to it — and the
  digest covers exactly that clamped prefix — and the characters beyond the cap
  render as a continuation after a restart instead of being skipped (review
  #569, round 25).
  Because V2 decodes text parts without `time.start`, identity alone cannot
  tell growth from a same-slot replacement there: the frontier also stores a
  small **digest of the delivered prefix** — derived metadata, never content —
  and resolution requires the read's prefix of that length to hash to it. A
  text/reasoning frontier whose start time or digest no longer matches the read
  is not dropped — that would lose its tail — but resolved as **cut 0**: that
  part renders in full while everything before it stays delivered, so a rewrite
  duplicates nothing (the old content is gone from the read) and a growth still
  renders only its tail. The same rewrite seen live, from the read that carries
  it, replaces the part's timeline entries instead of accumulating the old
  content under the new snapshot — the replacement is the part's whole content
  now — and stamps its prefix digest at the new full length, so the persisted
  cursor describes exactly what the card shows rather than the double-counted
  sum, and a restart resolves the part instead of rejecting it as the digestless
  legacy case (review #569, round 2). A cursor written by an older release
  carries no digest and therefore resolves nothing (a one-release migration
  seam, the cursorless fallback by another name); the first confirmed write
  after the upgrade gives it one.

- **Advance only on a confirmed write, through the existing delivery choke
  point.** The accumulator stages the cursor of the body it is about to write,
  exactly like the Wake Watermark's staged mark; the flush — or the
  projection's successor create — drains it only once the write is confirmed:
  a PATCH `Ok`, a create `Ok`, and a payload the Pending Card Update drain
  delivers later (a recoverable failure ties the stage to that payload's
  delivery sequence, and Session Sync's post-drain reconcile confirms it once
  the payload lands). A permanent refusal or a failed create drops the stage,
  so a write that reached no card can never hide content from a later
  recovery. `advance_cursor` writes only the record naming the card the write
  landed on, so a stale flush cannot touch a successor that already took the
  chain over. Every confirmation also names the exact stage its write carried —
  the stage generation in the accumulator, and for the drain the outbox
  sequence whose delivery was verified — so a body staged since (a fresh
  flush whose PATCH is still pending) is left for its own confirmation and can
  never be advanced by an earlier write; the Wake Watermark's drain carries
  the same stage identity. Stages are therefore RETAINED until their own write
  resolves them, not kept in a single pending slot (review #569, round 2): a
  delivered-but-unconfirmed stage stays confirmable after a newer body is
  staged — the reconcile walks every retained stage and confirms each by its
  own sequence, drops one whose entry settled without delivering it, and keeps
  a still-owed one — so a later delivery can never lose an earlier write's
  confirmation. The outbox answers only for a sequence's OWN outcome (review
  #569, round 6): the reconcile confirms a stage by the exact sequence it was
  tied to, and a failure note asks about the sequence ITS payload failed at —
  remembered alongside that payload — so a newer write's success, a cached
  repaint that may omit the older body's delta, can never confirm an older
  stage and let a restart skip content no card received. The delivery layer
  keeps a **bounded set of the sequences proven delivered** per card (review
  #569, round 24): a successful write adds its own sequence and the drain adds
  the retried payload's, so a payload a drain delivered stays confirmable even
  after a newer write FAILS and replaces the owed one — without it the evidence
  vanished with the entry, the cursor stayed behind, and a restart re-rendered
  content already visible on the card — while a repaint still answers only for
  its own sequence. The confirmation's cursor advance and the gap's
  advance/clear share ONE record write (review #569, round 24): two updates
  could leave a crash window with the cursor ahead of the gap, and the recovery
  would re-render gap content a card already showed. A stage
  older than the one the base already reflects is dropped unapplied, and the
  stage comparison and the durable record write happen in ONE cards critical
  section (review #569, round 5): a concurrent confirmation can never
  interleave between them, so an older confirmation arriving after a newer one
  was applied is discarded and never overwrites the durable cursor — it only
  ever moves forward. The Wake Watermark's stages are
  retained the same way — a delivered mark drains after a newer one is staged,
  its take and durable write share the same critical section, and
  `ChainRecords::advance` keeps the durable mark monotonic by `created_ms` as
  the second guard (an older drain can never move it backwards). A failure note
  whose payload the drain delivered
  before the note looked up its sequence confirms the stage then and there
  (the settled delivery verdict), instead of discarding a write that reached
  the card. The per-write cost rides the network PATCH it follows; this
  reopens ADR-0061's rejected render-frontier watermark, and the
  affordability answer is recorded in that ADR's amendment.

- **The cursor is chain-level and belongs to the record's lifetime.** A
  re-point within the chain — a split's continuation, a new Turn on the same
  chain, a takeover — carries the cursor; a genuinely new chain starts
  cursorless; `release` drops it with the record. No long-lived state
  accumulates, and "what does this chain remember" still has one home and one
  atomic write.

- **One projection for both restart cases.** The reconcile's decision ladder
  maps a cursor-carrying record whose run this process does not own to one of
  two outcomes sharing one seed, one successor renderer and one delivery rule.
  An ended-while-down (or run-lost) chain **projects and settles**: the missed
  tail plus the transcript's true ending (✅ / ❌ / ⏳) onto a successor. A
  still-live chain **projects and follows**: the successor is armed seeded at
  the cursor, its create is the restart notification, and the existing
  external-render arm streams the run's later content and settles it by
  transcript truth, never an invented interruption. Only the live case adds
  the follow phase. Both reply to the original Turn anchor — recorded, or
  re-derived from the same read when the previous life never captured its
  server time — falling back to the recorded card and then the chain's
  top-level Chat; with no deliverable target at all an ended run takes
  today's in-place settle and a live one is not adopted. Both collect the
  recorded card as taken over — dropping the Background Task Ledger and,
  per call, each running `⏳` marker the successor actually resolved (ADR-0068's
  rule, generalized; a marker whose call the read did not carry stays as a
  frozen witness, review #569) —
  and neither re-points the record's card until the create landed. The
  re-point and the create's cursor confirmation are ONE write, inside the same
  takeover critical section (review #569, round 3): the record must never be
  observable naming the successor card while still carrying the predecessor's
  frontier, because a fresh Turn that snapshots it in that window seeds the
  already-delivered tail onto its own card while the collected successor keeps
  its body — the same text twice. The old card's collect PATCH and the Wake
  Watermark drain follow that atomic transition, and no awaited call sits
  between the transition and the confirmation. A
  projection that renders nothing new (the cursor covered the whole read)
  drops its armed successor and keeps today's in-place ending; a live
  adoption is still sent — the run may produce next, and the follow is what
  keeps it live. An oversized delta goes through the SAME splitter as every
  card body, as a bounded chain of creates (≤ the existing chain bound), one
  slice per card, in order, each slice's cursor confirmed with the same
  critical section that attaches its card, only after its own create landed: a
  failed or bound-stopped chain leaves the record at the
  last confirmed slice, re-posts nothing in that life and lets the next
  life's projection resume from the cursor (review #569). For a LIVE adoption
  that stop is not the end of the run (review #569, round 2): the last landed
  slice's card is still handed to the follow, whose own flush/continuation
  machinery carries the remaining delta and everything the run produces next —
  the single-shot mark blocks further projections, never a follow. An ended
  projection's stopped chain keeps today's semantics (the successor already
  carries the transcript's ending, and the next life resumes from the cursor);
  a stop with NO landed card (an ambiguous first create, a lost window) follows
  nothing for either case.

- **A projection's create is single-shot per record per process life.** Feishu
  offers no idempotency key, so a create whose outcome is not a definite
  non-landing — any transport or API error — may have landed; retrying it would
  post a duplicate successor. The attempt is marked in memory, like the other
  reconcile marks, and no later pass this life re-posts: the record stays, the
  old card is state-repaired in place by transcript truth, and the ambiguous
  card is left as it fell. The trade: an ambiguous create can leave the
  successor unposted and the old card state-repaired, never duplicated. The
  single-shot fact is also written AHEAD durably (review #569): before every
  successor create — the first and each chain continuation — the record
  carries a projection intent (the same sidecar, the same atomic write), so a
  process that dies between the create landing and the takeover's re-point
  leaves it behind; the next life treats the create as ambiguous and never
  re-posts, and the old card takes the state-repair path. The intent is
  consumed by the takeover's re-point (a fresh record carries none) and cleared
  on a definite non-delivery, so a retry stays safe and a later life may resume
  a chain stopped by a definite slice failure from the last tracked slice. A
  **definite** non-delivery is not single-shot: a card-content rejection or an
  explicit 4xx refusal proves the platform created no message, so the record
  stays retryable — the next pass re-attempts, and a content rejection is
  re-rendered once through the flush's fenced fallback so the tail can still
  land (a content rejection that survives the fenced retry is suspended:
  single-shot, the payload can never land). A restart forgets the in-memory
  mark, exactly like every in-memory reconcile mark — the durable intent is
  what carries the single-shot fact across lives. A successor create that lands
  AFTER a fresh Turn already won the chain is collected **without its body** —
  the winner's message-first seed re-rendered the same tail, so preserving it
  would make the reader read the text twice — the one deliberate departure
  from ADR-0063's body-preserving collect. A chain that stopped mid-way
  (below) keeps its record instead: the tail past the last confirmed slice is
  still owed, so a terminal card's usual record discard is suppressed for it
  and the next life resumes from the cursor.

- **One timeline entry per transcript part.** Every text/reasoning part keeps
  its own timeline entry and `PartSource`, even when two parts of one message
  share a server time (review #569): the frontier's identity is the part, so an
  entry merged from two parts would carry the first part's digest over both
  extents, resolve to a cut at 0, and re-render content a card already showed.
  Chunks of the SAME part still merge; a chunk continuing a part whose
  equal-key run another part's entry follows is inserted beside its own
  entries, so the card's content and order are unchanged.

- **Render seeding.** The seed resolves the cursor against the read:
  everything at or before the frontier counts as delivered — marked per
  MESSAGE, not by content alone, so the seeded suppression and the Wake step's
  content diff can never replay the orphan's own parts while a NEW Turn's part
  with identical text still renders (review #569, round 4; a content-global
  mark swallowed the new answer in the message-first race) — and the frontier
  part renders only its undelivered
  suffix, with a markdown lead (a reopened fence, a repeated table
  header/delimiter) when the cut lands inside a construct. That suffix-only
  cut holds only while the read still carries what the successor delivered:
  the accumulated run's newest prefix digest must hash the read's prefix of
  the delivered extent, and a live REWRITE — a read whose prefix no longer
  does — replaces the part's run with the rewritten content in full, exactly
  like the ordinary render's rewrite rule (review #569, round 3). Cutting a
  rewritten part at the old extent would push a stray suffix and let the
  cursor claim the replacement delivered. The frontier's identity rule covers
  tools too: a settled-tool frontier whose recorded start time no longer
  matches the slot — with or without a text extent before it — is a
  REPLACEMENT, and it renders (cut 0) rather than being skipped, so its result
  is never omitted (review #569, round 4). The live set resolves by call
  identity against the whole read on every render read: a
  still-running call rides the successor's live tail display-only and a call
  that settled while cola was down joins its timeline exactly once, at the
  server start key it was born with. Only calls the read actually carries
  resolve, and the collect strips per call: exactly the running `⏳` panels
  whose call the seed resolved leave the old card (identified through the
  panel's call-named element id, `tool_{call_id}`), while a call outside the
  read — a V2 transcript truncated at its page cap — keeps its panel on the
  collected old card as a frozen witness rather than being stripped from a
  card the successor cannot repair (review #569). The read DOES report that
  truncation: the V2 adapter sets `SessionTranscript::truncated` when it stops
  at its page cap, and V1 always reports false. A truncated read therefore
  adopts nothing — the ended projection and the live adoption alike claim
  nothing, and the message-first seed does not resolve against it (its gap is
  never marked complete, so the unseen tail still lands once a complete read
  shows the gap's end) — and the frozen marker is what the user sees until a
  later complete read adopts normally (review #569, rounds 22/24/26).

- **The message-first race uses the same primitive.** When a user message wins
  the race against the adoption, the fresh Turn's takeover seeds its card from
  the same cursor, scoped at the orphaned Turn's window: the orphan's final
  undelivered tail renders once, as a continuation, before the new Turn's
  content, and the live set resolves by identity. A record with no cursor
  keeps the retired carry's semantics — the orphaned Turn's still-live calls,
  nothing replayed. A cursor-bearing orphan's tail is never dropped: a seed the
  takeover's read could not resolve — a failed or timed-out read, or one that
  cannot place the cursor — becomes a **durable orphan gap** on the Chain
  Record (its `cursor` is the orphaned chain's confirmed frontier, its `anchor`
  the orphaned Turn), while the chain's cursor keeps advancing with the new
  Turn's own confirmed writes. One frontier cannot express "gap undelivered,
  later content delivered", so the gap is a fact of its own rather than a pin
  at the gap's frontier (review #569). A takeover that seeds from a cursor
  records the gap durably whether or not its read can place the cursor (review
  #569, round 24): the old card is collected right there, so a crash before the
  new Turn's first confirmed write would leave the record with the old cursor
  and the new Turn's anchor — a recovery that cannot see the orphaned Turn's
  window and would omit its tail. The in-process render (the resolved seed's
  scope walk, which tags its entries as gap content and marks the accumulator)
  never clears it: only a confirmed write covering the gap's END does, so an
  unconfirmed render is simply re-rendered by recovery. Rendering it is a walk
  of the gap's own
  Turn window, cut at the gap's cursor and bounded by the Turn the chain moved
  to — the gap records that message, falling back to the read's next user
  message, because `turn_for_user` carries no upper end — whose pushed entries carry
  **no source**: a successor built from them can never offer a frontier older
  than the record's, and the delivered content the cursor already covers — the
  answer of the Turn that followed the gap — is never re-rendered. The gap
  lands exactly once and is then consumed: a projection takes it with the
  re-point that rewrites the record (a gap the read could not place rides onto
  the new record), a fresh Turn's takeover that finds one re-homes it onto the
  new record and renders it on its own card, and every confirmation advances
  or consumes the durable fact exactly as far as ITS body reached: the stage
  carries the body's **gap coverage** — the gap's cursor at the newest gap
  content that body includes, and whether it reached the gap's last entry — so
  a partial body (a gap split across cards) only advances the gap's cursor,
  and a restart, or the next slice, resumes after what a card already showed
  instead of repeating it; the fact clears only when the body carrying the
  gap's END is confirmed. A write built before the gap rendered carries no
  coverage and can neither clear nor advance it. The re-point carries the gap
  like the cursor — a continuation or a fresh Turn cannot drop an owed tail —
  and the chain cursor never takes a gap entry as its frontier: the gap's
  content sits EARLIER in the read than the confirmed frontier, so the
  accumulator tags the messages its gap walk rendered and the frontier skips
  them. An ended record whose gap this read cannot place claims
  nothing (`Keep`: a settle would drop the fact forever), and the record is
  not released while an unrendered gap is owed. No interlock is
  introduced: the projection relies on the
  existing per-card delivery lock for ordering (#527's no-hold property).

- **A truncated read is a prefix, and the ended projection never settles on
  it.** The V2 transcript read pages to its cap (`MAX_MESSAGE_PAGES`) and
  returns what it has; the neutral `SessionTranscript` says so with `truncated`
  (V1 reads are unbounded and always false). A truncated read can place a
  cursor inside its returned prefix while newer content sits unseen beyond the
  cap, so an ended projection claims nothing while the read is truncated — no
  settle-as-complete, no released record — exactly like an unplaceable cursor:
  the ending itself cannot be trusted. A LIVE record claims nothing either
  (review #569, round 24): the truncation is the OLDEST prefix, so following it
  would re-read that same prefix forever and never see newer output. A later
  complete read adopts or settles normally.

- **The Wake Watermark still closes the announcement.** The projection's
  confirmed create drains the staged Wake Watermark through the same choke
  point the flush uses, so a Wake completion entry the successor rendered is
  durable and a later, by then recordless Fresh post cannot re-announce it
  (#424). The mark also flows the other way at arm time (review #569): every
  accumulator that starts rendering a session — the fresh Turn, the projection
  and external arms, the Wake continuation — seeds a **Wake floor** from the
  chain's durable mark, and the wake-entry render skips any Wake at or below
  it: its completion entry was already announced by an earlier card, so it is
  neither re-inserted nor staged for a new advance. The floor is time-based,
  not an id set, so older Wakes below the newest announced id are covered too,
  and a Wake strictly newer than the mark still renders and announces.

- **The cursorless fallback is today's behavior.** A record carrying no cursor
  keeps the existing arm unchanged: the #443 one-time restart stamp for a
  still-live orphan, the in-place settle for a transcript-decided ending, the
  collect arms, and the Fresh path's Wake Watermark gate for recordless posts.
  A cursor-carrying record is never stamped, however unplaceable its frontier:
  an unresolved cursor, a missing read or no scope claims nothing and leaves
  the record for a later pass the projection can seed. The ENDED case is the
  same rule (review #569): a cursor this read cannot place keeps the record —
  settling the card in place would release the tail the cursor still guards —
  and only a cursorless record keeps the one-release in-place fallback, while
  an Unreceived ending is its own terminal (ADR-0062: no anchor to continue
  onto, so nothing can be projected). The fallback window
  closes by itself — the first confirmed write gives the record a cursor.

- **One rule for both generations.** The reads the projection needs
  (directory-routed session status, the typed transcript, the external follow)
  are generation-neutral, and V1 naturally has no Wakes; no second code path
  decides what the user sees.

## Considered options

- **Keep the pairwise mechanisms** (#443 stamp, ADR-0068 carry, the Fresh
  gate's no-replay for recorded chains). Rejected: the mechanism count is
  itself the cost — every interleaving between them has to be reasoned about
  pairwise, and a missed mirror stays invisible until it bites a live session.
- **Persist a payload journal** (the built card JSON). Rejected by #505 and
  still: it makes cola a second store of what OpenCode owns, and recovery
  needs a boundary, not a transcript copy.
- **A per-Turn or per-card cursor.** Rejected: a split re-points the record to
  the continuation and a new Turn must not clear what the chain already
  showed; the chain is the unit the user reads, so the chain is the unit that
  remembers.
- **A separate sidecar for the cursor.** Rejected: ADR-0069 is one home for a
  chain's durable facts; the cursor shares the record's atomic write and
  lifetime, and release drops both.
- **Whole-turn replay, whole-body rebuild, or continue-from-now.** Rejected:
  the three structural failure modes (duplication, silent reset, omission)
  this decision exists to end.
- **Reconstruct the delivered state by comparing content against a read.**
  Rejected: no durable boundary — the same content may have moved between
  parts or compacted away, and the comparison decides by guess.
- **Stamp a cursor-carrying record when its cursor cannot be placed.**
  Rejected: the stamp freezes the run the projection is supposed to follow;
  claiming nothing leaves the record for a later read that can seed.
- **Pin the chain's cursor at the gap while the seed is pending** (the
  pre-review design). Rejected (review #569): the pin keeps the delivered
  content that follows the gap out of the cursor, so a restart's recovery
  renders that already-delivered content again — the successor shows what the
  old card already showed. The owed tail has to be a durable fact of its own.
- **Retry a create whose response was lost.** Rejected: Feishu has no
  idempotency key (ADR-0067), so a retry can post a second successor card; the
  single-shot mark prefers an unposted successor with an in-place state repair
  over a duplicate.
- **Hold a lock across the projection.** Rejected: the projection uses the
  existing per-card delivery lock for ordering, like every other card writer
  (the review's candidate 5 is not a prerequisite).

## Consequences

- A restart mid-answer keeps one live thread: a still-running run is followed
  onto a successor showing only the undelivered delta, a run that finished
  while cola was down shows its missed tail together with its true ending, a
  tool crossing the restart keeps its live panel or lands its result exactly
  once, and no collected card is left looking busy. The successor's send is
  the restart notification the #443 stamp never was.
- Duplication and omission are one rule: the successor renders strictly after
  the confirmed frontier, character-exactly, and the old card is collected as
  taken over rather than rebuilt (its body minus the live markers it handed
  over).
- Retirements: #443's stamp narrows to the cursorless fallback (one release);
  ADR-0068's carry retires into the seed; the Fresh gate narrows to recordless
  posts, where the Wake Watermark keeps its exactly-once meaning; #505's
  deferral lifts. #528 and #529 are closed by the adoption.
- Boundaries hold: Interaction Receipts stay in ADR-0038's request-flow domain
  (still-pending requests re-host onto the successor through the existing
  sweeps; resolved receipts are not rebuilt), and the Pending Card Update
  outbox and the runtime-retirement overlay keep their owners.
- Tests pin the rule where the symptoms live: the restart matrix seeds a
  cursor, restarts over the sidecar, runs the real Session Sync pass and
  asserts the successor's content against the old card's delivered state
  character-level (no duplication, no omission) for the ended and live cases,
  the message-first race, the cursorless fallback, the cursorless stamp's
  untouched suites, and the confirmed-write advance (a transport failure, a
  content rejection, and the drain's late delivery). A settled tool behind the
  newest text is part of the frontier: a restart over that cursor re-renders
  neither the panel nor the delivered text, and later growth still renders only
  its tail. A create whose response was lost is never retried: later passes
  post nothing further and state-repair the old card in place. The store seam
  pins the cursor's round-trip, `track`'s carry, `release`'s drop and the
  legacy cursorless read.

Related: #505, #528, #529, #443, #444, #522, #527, ADR-0038, ADR-0061,
ADR-0063, ADR-0068, ADR-0069.
