# Interaction blocks: inline on the live card, repainted through a card handle

> **Retired by ADR-0100** (rule 6 — the snapshot claim): the Session Snapshot's
> claim registry goes with its card, so every **Interaction Block** lives on a
> live card or on a standalone card of its own, and `SnapshotClaims` no longer
> exists.
>
> **Amended by ADR-0104**: where a synthetic entry lands — how it is keyed, the
> equal-key tie-break and the read floor — is the Timeline module's rule, and
> #1040 carries the exact rules and their tests.
>
> **Amended by ADR-0114** (issue #1103): the typed-answer freeze below has one
> bounded exception — a live block whose carrying write was refused is not on
> the card, so it guards nothing and the next pass may write it.

A pending Permission/Question is rendered inline on the card the Host is already
reading (an **Interaction Block**). It stays inline and follows the live card,
and every card showing a live block carries a handle that lets cola repaint it —
cached last-rendered JSON plus a `request_id → message_id` registry — so a click
updates the clicked card atomically in the callback ack and a remote resolution
repaints within one sweep.

## Context

- **Placement.** A new request is inlined onto the owning Session's live card
  (or the nearest ancestor's, for sub-task children) and the current card is
  flushed; with no live card it becomes a standalone card
  (`RequestFlow::surface`, `src/bridge/request/flow.rs`; host resolution
  `inline_host_session`, `src/bridge/pollers.rs`).
- **Only the newest card of a chain carries the tail.** Interaction blocks
  render under `include_tail` (`build_card_inner`, `src/bridge/turn/state.rs`),
  which a split switches off for the finalized card: `build_card_with_info`
  finalizes the filled card without the tail and advances `render_from`, and
  `Turn::flush_card` sends the continuation which carries it (the flush machine
  is `src/bridge/turn/flush.rs`). A pending request blocks the backend run, so a
  block rarely lives through a split; the cases that matter are the ones where
  the card is no longer current at all.
- **What went wrong.** A click on a card that was not the accumulator's current
  card only toasted, and a request resolved by another client (OpenChamber, the
  CLI) was stripped from memory but no path repainted its card — while a turn
  ran, the header timer's ~1.5 s tick incidentally repainted the current card;
  after the turn ended nothing did, so the block stayed forever. A new turn
  replaced the accumulator and orphaned any still-pending block on the old card:
  a click replied (or 404'd) but could neither repaint the old card nor find its
  section, because the card map keeps no per-message card JSON.
- **The mechanisms the decision builds on.** Feishu applies an ack card to the
  clicked card atomically — the reason the question path's partial answer
  already shows its 已选 markers instantly — and the callback payload carries the
  clicked message id (`open_message_id`), so cola can target the exact card.
  Standalone request cards already have `sent_cards` + `mark_stale_cards`.
- **The Host's constraint.** A standalone card per interaction was proposed and
  rejected: it fragments the message stream, and whichever card is newest, the
  other's live updates go out of view — "要么我最新看到的永远是这些独立卡……要么这个消息要拆得很碎".

## Decision

**Interaction blocks stay inline and follow the live card, and every card that
shows a live block can be repainted.** The rules keep the numbers the code and
the other ADRs cite; **rule 6 is retired** by ADR-0100, so the numbering skips
from 5 to 7, and its surviving half is the standalone/persistence section after
the rules.

### 1. Follow the live card

A split keeps the tail (and with it the block) on the newest card. When a new
turn replaces the accumulator, the sweep re-hosts every still-pending block of
that session onto the new card and repaints the old card without it. A block
never lives on a card that is not the session's current card for longer than the
re-host/repaint takes.

### 2. Every card showing a live block is repaintable, through its card handle

The handle is a `request_id → (message_id, kind, session, directory)` registry
plus, for each message that shows a live block, the card JSON as last rendered.
Every update — click, remote resolution, re-host, sweep strip — rebuilds that
card from the cache and PATCHes it, and the cache entry is released when no live
block references the message. It is the only way to keep controls honest on a
card whose accumulator is gone (a replaced turn, a stopped turn). Every card
write is a read-send-record sequence (build → Feishu PATCH → the handle's
record) and its callers are concurrent — the render poll, the two request
pollers and click acks — so rule 8 serializes them.

**A live form freezes the render pass (#917).** Feishu rebuilds `body.elements`
on every card update and a form input's unsubmitted text lives only in the
client, so **any** repaint of a card that renders a free-text answer field wipes
what the Host typed. A card with an unanswered typed answer
(`StreamAccumulator::awaits_typed_answer`: a live question field with a
custom-answer box or a numeric form) is therefore not written by the **render
pass** at all: the header's tick, the ledger's rows and completion entries, a
task panel's activity, a file delivery and even a freshly rendered part stand
down — the pass's own "content" is not special, because a pass can render a
part, pause for its reads and resume after the question landed. Nothing is
lost, because nothing is consumed: the pass plans no completion entry, applies
no ledger, spends no liveness read and does not build the card, and the build is
what advances `render_from`, so the first pass after the answer renders the
whole backlog at once. The pass's apply, its form check and its build share ONE
card-write-lock acquisition, and block insertion (`Turn::add_question` /
`add_permission`) takes that lock too, so "insert, then flush" is one ordered
step against every card write. The form's own writes still land, because they
are not the render pass: the block's arrival, a click's ack, the answer's
resolution flush and the settled wait. An ack that restamps the header advances
the render gate's signature cache with it (`Turn::mark_header_shown`), or the
next tick re-PATCHes the identical body — the same wipe one click later.

### 3. A click updates the clicked card in the ack

The callback response carries the cached card with the block updated (partial
answer state) or replaced by an Interaction Receipt (resolved) — atomic on the
clicked card, no PATCH race and no dependence on "which card is current". This
unifies permissions and final submits with the question path's mechanism; the
split-probe clone fallback and the PATCH-only disappearance go away.

**The ack has a deadline.** Feishu fails a card callback it does not hear back
from within 3 s (`200341`), so the WS loop answers within `CARD_ACK_BUDGET`
(2 s, `src/feishu/ws.rs`) even when the handler is still running: it acks with a
`处理中…` toast and drops the late result. The next render poll repaints the
session's current card; a dropped result on a standalone card or a non-current
handle card (rule 2) is not repainted, and its controls stay live until a second
click re-serves the settled result. A handler that beats the budget still
updates the clicked card in the ack exactly as above — the budget bounds the
stalled path only. The permission/question reply calls carry their own total
timeout (`REPLY_TIMEOUT`, `src/opencode/transport.rs`), so a stalled backend
surfaces as the existing retryable failure card inside the budget instead of
riding the ack deadline.

### 4. Resolution leaves an Interaction Receipt at the moment of resolution, and it names its target

The receipt is a timeline entry for the moment the block was resolved, so it
sits below everything the Host could see when they clicked and above everything
that follows; live blocks stay in the card's tail, and only the residue is a
transcript entry. It carries its target, so it still identifies its request
where the position alone cannot: a sub-task child's block has no command panel
on the parent card, and a toggle resolves several blocks at once. One markdown
line, no controls, so a late click can never be ambiguous:
`✅ 已允许一次：⚡ 执行 Shell 命令 \`ls -la\``, `✅ 已回答：目录 /a、分支 main`,
`🚫 已拒绝：…`, `⏱ 已由其他客户端处理：…`, `⏱ 已随会话中断：…`. The one receipt that
does not name a target is the Auto-Accept toggle's: it reports the MODE change
(`🔄 已开启自动授权：后续权限请求将自动批准`), because naming a command read as "only
this command gets auto-approved". A mode change resolves however many blocks are
pending, so it leaves ONE such line per card it repaints (a resolution can
repaint more than one card) and the blocks it swallowed are dismissed without
receipts of their own — the same line repeated per block would be noise, not a
record. A receipt shows no clock: the decision happens in cola's world, where no
server time exists, and only server times (a part's own start time on its panel,
the turn's user message on the header date) may reach the card — a cola time in
the same costume would mix two clocks on one card.

**Where it lands is ADR-0104's**: the Timeline keys the synthetic entry, sorts
it by its equal-key class rule and owns the read floor (#1040 carries the rules
and their tests). This ADR keeps the receipt's content and the cut it carries.

**A vanished request is classified by its session's run state.** When a block
leaves the pending list and cola did not answer it, the sweep reads the owning
session's run state (ADR-0059's live predicate, one bounded read per distinct
session with a vanished block, none when nothing vanished) before choosing the
line: a live run (`Busy`/`Retry`) keeps the neutral `⏱ 已由其他客户端处理` —
another client answered while the run continued; an `Idle` run gets
`⏱ 已随会话中断` — the execution that owned the request is gone (a stop, a crash,
a server restart, or the server's own 60-minute inactivity eviction), so nobody
consumed it and claiming another client did would be a lie; a failed or
unrecognised read is UNKNOWN and keeps the neutral line — unknown is never read
as an interrupted run, the same rule a failed list gets (#130). It covers the
inline receipt and the standalone card, whose copy becomes `⏱ 已随会话中断` /
「会话已中断，该…请求未处理。」 (`build_interrupted_card`) and whose persisted record
carries the owning session id so a restarted process can classify it too; an
empty owner (a record written before the id was persisted) is UNKNOWN and keeps
the neutral copy. A late click's 404 card keeps its existing line.

**The receipt's server-time cut.** A resolution records the asking tool part's
server time as the receipt's cut (`ReceiptCut`) — in the ordinary case the asking
tool call is the newest part the server wrote. The surfacing read's
**high-water mark** travels on the block (`PendingQuestion::asked_at` /
`PendingPermission::asked_at`), not one part id: the read happens before the
click, and while a request is pending its turn is blocked, so nothing the Host
could have seen — a parallel tool call or a queued message included — exceeds
it, and nothing is produced between the read and the click either; a newer
sibling part only raises the mark, and a lower anchor is the unsafe choice. The receipt
itself keeps the key the click gave it — it must follow everything the Host
could see — and a later transcript **MESSAGE** the server created after the ask
is floored at that key. The unit is the message, not the part: a part that
STARTED before the click but grew after it belongs to a pre-click message and
keeps its place, and a parallel tool's late-delivered part sits in the asking
message too. A new message is only created once the turn resumes, i.e. after the
click, so `message.created > ask` is the sound test, and both halves are on the
server's clock, so cola's wall clock cannot decide the comparison.

Every anchored receipt keeps its cut rather than only the newest one: requests
can be asked in one order and answered in another, and content written between
the asks must clear the receipt of the *last* click too. The floor is the newest
receipt key among the asks that precede the content's server time — a
staircase, not a single "latest" cut — and a chunked entry's continuation
chunks take it too, so the timeline stays key-sorted under the receipt.

**A resolving click reserves the receipt's key** before it replies
(`reserve_receipt`): the backend reply returns before the receipt is written, so
a render poll may insert the post-click continuation in that gap, and the
reservation floors future inserts at `key + 1`. The receipt then takes the
reserved key, so the gap content follows it, and nothing already placed moves —
everything on the card before the click was something the Host could see. An
Auto-Accept toggle reserves before its reply too, with the LATEST ask among the
live blocks its one receipt covers (`take_reserved_receipt`), and the blocks it
resolves through the shared receipt drop their own reservations
(`consume_reservation`). A reservation that never becomes a receipt drops its
floor with it (`forget_reservation`, by token, so a stale click's late cleanup
cannot delete a retry's fresh reservation), and it consumes its key from the
synthetic sequence, so no unrelated receipt can take the same key. The
reservation takes the card write lock under a short budget — a click runs on
Feishu's ack deadline and a concurrent flush holds that lock across a network
PATCH — so under contention the reservation is skipped and the receipt keeps its
default position.

**A remote resolution cuts differently, by design.** When a question is resolved
with no reservation (a remote client's answer, noticed by the next sweep), the
ask cut floors future messages only — there is no click time to cut against, and
moving placed content (a child, background or external content can reach the
card while a question waits) would violate this rule for what the remote Host
already saw; that one-sweep window is a deliberate limit. A permission's receipt
records the same message-scoped cut: parallel tools write into the ASKING
message (created before the ask), so they are never floored, while a new message
after the answer follows the receipt. Every merge lookup uses the floored key,
so a part that spans the cut keeps one entry above the receipt.

**The anchor travels with the request.** A request whose asking part lives in a
CHILD session (hosted on an ancestor's card) is anchored by one bounded read of
the child's transcript, without rendering it (`session_anchor`); the busy-follow
seed does the same, so a block attached by the follow has the cut the sweep
would have captured. A block that reached the card without a surfacing read (a
restart re-adoption) carries no anchor; a **re-hosted** block keeps the anchor
it was first surfaced with — the anchor belongs to the request, not to the card
it sits on, so it travels in the block handle. A mode change that resolves
several blocks leaves one receipt and cuts at the latest ask among them.

### 5. The sweep drains before it surfaces, and a remote resolution repaints within one sweep (≤3 s)

The sweep lists a pending question independently of the render loop, and the
assistant's explanation of *why* it is asking sits in the same transcript, not
yet rendered; if the block landed first the card would carry an unanswered
input, the freeze above would hold the explanation until after the answer, and
the Host would be asked to choose blind (#1023). `RequestFlow::surface` therefore
drains once before the inline block lands: one bounded transcript read, one
bounded **render-only** pass, then the block insert and its flush
(`RequestFlow::drain_and_anchor`). It runs only for the request's **own** session
with a live card — an ancestor host renders no part of a child's transcript, and
a standalone card has no card to drain — and it is best-effort: a failed or
timed-out read changes nothing, the request surfaces exactly as before without
the anchor, at most one read per first surfacing.

ONE deadline covers the transcript read and the render pass together
(`SURFACING_DRAIN_TIMEOUT_MS`, 5 s, or the flow's list timeout when shorter), but
the write it owes is not bounded by it: the pass renders into the accumulator
and the flow flushes afterwards, because a card create carries no idempotency
key and abandoning one could duplicate it (ADR-0100). The pass also skips the
side effects a render owns outside the flush — the auto-rename's title write and
the File Message deliveries, neither guarded by the card write lock; the next
ordinary render performs them (a pending file delivery stays pending). The
progress mark tells the two apart: a pass the timeout abandoned after applying
content still owes its flush, a pass that found nothing new owes no write. The
flush re-checks the live-form freeze under the write lock, so a question that
landed since the apply still refuses the repaint, and the applied content rides
the form's own resolution flush.

The sweep's other half is the repaint: a block resolved elsewhere is repainted
from the handle's cache instead of only being stripped from memory, so the card
catches up within one sweep — after the turn as well. Clicks stay instant
(rule 3).

### 7. An unfinished cola turn rejects what it left pending (#187)

When a cola turn ends without completing — `/stop`, an interrupt, a prompt
error — the tool fibers behind its still-pending Permission/Question requests are
dead, so answering them reaches nobody. The turn rejects every such request of
its session or a sub-task descendant at the source and settles its block into a
`🚫 已拒绝` receipt (not the neutral `⏱ 已由其他客户端处理` line: cola itself
decided). This removes the ghost-block case at its source; a request that is
genuinely still live when a card stops being current (external turns, a turn
replaced mid-flight) remains rule 1's re-host.

### 8. One card writer per session at a time

`flush_card` and `resolve_blocks` hold the session's card-write lock
(`CardsHandle::write_lock`, `src/bridge/handles.rs`) across their whole
read-send-record sequence, so a resolution can never be overwritten by a stale
flush and a split advances its card id before another flush reads it. Two
interleavings were observed live before the lock: a flush that snapshotted
before a resolution recorded the resolved block back (its stale body overwrote
the receipt, and the sweep then reported cola's own auto-accept approval as
`⏱ 已由其他客户端处理`), and a flush whose build had already advanced a split
PATCHed the continuation's slice onto the card it had just finalized (two
identical messages, only one tracked and repaintable, the other frozen with live
controls — the duplicate question cards the Host answered twice).

`RequestFlow::surface` is the third holder: it takes a card write lock for its
**whole surfacing decision** — the inline-host check, the block insert/flush and
the standalone send with its `sent_cards` record — so `/card`'s busy follow and
the sweep cannot both deliver one request. The lock stays per-session, and a
sweep **never holds two Sessions' locks**: it resolves the inline host first,
bounded and before any lock because the parent-chain walk reads the server; with
a host it takes **the host's** lock (the one `/card`'s post-and-arm and a busy
follow take), and if the host is no longer live it releases that lock and takes
**the request Session's own** for the standalone path. The two are never nested:
a sub-task child's lock is not held while its ancestor host's is, so no liveness
check runs under a lock it does not own. The standalone send is one Feishu
create and is deliberately not wrapped in a request timeout: a create has at-least-once semantics, so
abandoning a slow response could orphan a card that did land and a retry would
post a second (ADR-0100).

The lock covers exactly the two writers that can name the same message (the
flush's PATCH and a resolution's ack/cache edit). The sweep's and re-host's own
patches target standalone or superseded cards that no flush PATCHes; where a
racing flush does revive a block the sweep already resolved, the next sweep
re-resolves it into the same neutral line, so those paths stay lock-free rather
than serializing the poll loop behind a click. The sweep's vanished passes
instead skip every request cola itself is answering or has answered
(`answered_requests` ∪ `settling_requests` — the latter claimed before an
auto-accept approval's reply lands and released when `resolve_blocks` settles,
with a short TTL on a claim left by a cancelled settlement), because the neutral
`⏱` receipt is only true when another client did the resolving. An auto-accept
toggle settles the blocks it approves with the mode receipt, exactly as the
command and the permission card's own button do, rather than leaving them to the
sweep.

## Standalone cards, and the persisted surface

Where no accumulator exists (restarts, external turns, pre-existing adopt-time
pendings) the standalone card path with `sent_cards`/stale marking stays as it
is, and a resolved standalone card keeps its result-card rendering.

**The interactive surface state is persisted and re-adopted (#309), so a
still-pending request keeps exactly one live surface across a `/restart` or a
crash.** The block registry, the per-message card-JSON cache and the
standalone-card records are mirrored to `interactive_surfaces.json` beside
`sessions.json` (best-effort, atomic temp-file replace, an empty record removes
the file — the reminder's `pinned_chats.json` pattern) and written through on
every registry mutation (`Surfaces`, `src/bridge/surfaces.rs`). The card JSON is
written only when a card's live blocks change: a live block freezes the turn's
content, so the header timer's re-flushes carry no new interactive state and must
not rewrite the file every second.

On startup `CardHandles` hydrates from the record, and each flow seeds a one-shot
`recovered` set from its kind's entries. The first sweep that lists a
still-pending recovered request **re-adopts** its card instead of posting a
second one (`RequestFlow::readopt_surface`): the kind remembers what a click
needs (a question's full request), then the persisted card is PATCHed — for a
question, with its block re-rendered from the state this process holds, so
partial answers that did not survive the restart are not shown as selected. A
recovered standalone card is already the live surface and is left as it is. A
card that cannot be repainted (message deleted, unusable cached JSON) is
forgotten and the request is surfaced as a standalone card in the same sweep.
The kind's `prepare` deliberately does not run on the re-adoption path: a request
the previous process left pending was not auto-accepted then, and re-adopting
its card keeps the decision with the Host; auto-accept still applies to
newly-listed requests.

A click on a re-adopted card resolves exactly as before the restart: the handle
registry names the clicked card, so the resolution edits its cached JSON and the
ack carries it (rules 2+3). The flow's `inline` ack choice also counts a card
handle on the clicked card, so a question's partial answer refreshes it in place
rather than replacing it with a standalone rebuild, and the directory fallback
resolves through the persisted handle when neither the callback payload nor the
session store names one.

The sweep's reconciliation runs on hydrated state too: a request resolved while
cola was down has its block stamped `⏱ 已由其他客户端处理` from the cached JSON,
and a standalone card is marked stale; either way the persisted record is removed
as its surface resolves.

**A dismissed Location Form is a deliberately surface-less pending request**
(ADR-0103 owns it): a Location Form the Host deferred with 「稍后」 is still
pending but intentionally has no live card. Its persisted dismissed record is
authoritative — the sweep must not re-post it, and restart re-adoption applies
only to a card that was not dismissed — and `/mcp` is the way back. The exception
does not apply to a session-backed request, or to a Location Form that was never
dismissed.

Streaming content with no live block is still not persisted: a pre-restart card
that carries no block may freeze, and a click on a card the registry does not
know (persistence off, or the card was never recorded) is best-effort — the
request is still replied to, but that card may not repaint.

## Why

- The block lives where the Host reads; a separate card splits the stream and
  moves live updates out of view (the Host's constraint).
- A single repaint path through the handle removes the whole class of "which code
  path repaints which card" bugs: every mutation of a block's presence or state
  goes through it, including the ones that used to repaint nothing.
- The ack card is the only mechanism Feishu guarantees for refreshing the clicked
  card; a PATCH around a callback leaves the clicked card on its pre-answer state.
- An Interaction Receipt makes resolved state readable and stale clicks
  unambiguous — the misunderstanding the Host reported — and the cut puts it below
  what they saw, the *how* being ADR-0104's.
- Persisting and re-adopting the surface keeps "one live surface per pending
  request" true across a restart; without it the accepted cost was a duplicate
  card and a frozen one.

## Consequences

- New core state: the block registry and the per-message card-JSON cache,
  released as blocks resolve. Memory is one card JSON per card with live blocks
  (bounded by concurrent pendings).
- The accumulator's `interactions` section remains the render source of truth
  for the tail; the registry/cache is the repaint source of truth. The two must
  be kept in step — the same mutation seam (add / state change / resolve /
  re-host) edits both, or they drift.
- The registry, the card-JSON cache and the standalone-card records are persisted
  beside `sessions.json` and re-adopted on startup ("Standalone cards, and the
  persisted surface"), so a restart no longer duplicates or freezes a pending
  request's surface.
- Cross-client state is eventual within one sweep, by design (ADR-0004 keeps
  polling), and a stalled click's late result is dropped at the ack budget: a
  standalone or non-current handle card waits for a second click or the next
  sweep.
- A render pass on a card showing an unanswered typed answer writes nothing at
  all, so its content and its ledger arrive on the flush after the answer.

## Alternatives considered

- **Standalone request cards** (one card per interaction; the streaming card
  only shows the waiting header): predictable per-message updates, but fragments
  the stream and moves the live interaction away from where the turn is read.
  Rejected by the Host.
- **Pin blocks to the card where they first appeared**, repainting through the
  same handle: no re-hosting, but a still-pending request can sit above the
  newest card — the "scroll up to find the live thing" problem. Rejected.
- **Retain the replaced accumulator and rebuild old cards from it**: heavier than
  caching the rendered JSON, and the split state (`render_from`) makes
  reconstruction error-prone. Rejected.
- **No cache; toast-only clicks on stale cards**: leaves the observed "clicked,
  nothing changes" behavior in place. Rejected.
- **Replace a stale whole card with a result card**: destroys the card's streamed
  content. Rejected.

## Out of scope

- **Notifications when an inline block first appears**: a card PATCH does not
  notify, and a new-message ping was considered and deferred — not re-litigated
  here.
- **SSE or any push channel for cross-client resolution**: the ≤3 s sweep latency
  is accepted (ADR-0004/0011 keep polling).

## Risks / open questions

- The two sources of truth (the accumulator's `interactions` section + the
  registry/cache) drift unless every mutation goes through one seam: the section
  moves only through `add_interaction`, `resolve_interaction`,
  `dismiss_interaction` and `update_question_state`, the handle only through
  `record`, `resolve_on`, `refresh_on` and `drop_vanished`, and a resolution
  edits both. A new path that touches a card directly is the main
  implementation risk.
- The cached card is the same JSON Feishu already accepted, so a surgical edit
  stays under the card size cap.
- The Interaction Receipt line adds a line to every card that hosted an
  interaction; if it proves noisy it can be collapsed or shortened later —
  presentation-level.
- The re-host on a new turn is rare (the busy guard prevents a new turn while a
  request is pending; it happens when a request outlives a cancelled/aborted
  turn) and needs a regression test.
- The persisted record is written through on every registry mutation, so a crash
  between a card write and its mirror leaves the record one surface stale; the
  sweep's reconciliation heals it on the next start.

## Amendment (ADR-0114, issue #1103)

The typed-answer freeze above gains **one bounded exception**, and it reads the
freeze's own premise: a card that does not **render** the box guards nothing.
A live block is written by exactly one write — the flow's arrival flush — and
every later pass stands down while the block renders a typed answer, so a
refused arrival used to strand the block forever (live 2026-11-20: the header
said 「等待你的回答」 and the card carried the question's prose with no
controls). A question block's insert therefore arms
`BLOCK_ARRIVAL_RETRIES` attempts (`owe_block_arrival`), `awaits_typed_answer()`
is false while one is owed, each refused attempt spends one, and a delivered
write settles the obligation (`settle_block_arrival`). A card already guarding
a sibling question's box arms nothing. The freeze's rule is otherwise unchanged:
a card that shows a box is never written by the render pass.

## Domain note

The concepts enter the glossary as **Card Chain**, **Interaction Block** (交互块)
and **Interaction Receipt** (回执).

Origins: #173, #180, #187, #309, #917, #1023, #1040 (spec), #945; ADR-0004,
ADR-0014, ADR-0043, ADR-0049, ADR-0059, ADR-0071, ADR-0091, ADR-0100, ADR-0102,
ADR-0103, ADR-0104.
