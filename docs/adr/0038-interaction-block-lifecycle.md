# Interaction blocks: inline on the live card, repainted through a card handle

> **Retired by ADR-0100** (rule 6's snapshot claim): the claim registry goes
> with the Session Snapshot card, so every **Interaction Block** lives on a live
> card or its own standalone card, and `SnapshotClaims` no longer exists.
>
> **Amended by ADR-0104 when it lands** (the keying sentence and the equal-key rule): synthetic
> entries may be keyed at their own server time or at a key the caller already
> holds — a click's reserved receipt key, the Wake 承接 line's server time — not
> only at `next_order()`; and a tail-only **Tool Panel** whose settle floor is at or above its key takes a
> fresh allocation order above the equal-key entries of earlier messages (but
> below its own message's following entries), because its allocation predates
> content that became visible after it was pinned. The receipt's server-time cut
> is unchanged.

A pending Permission/Question is rendered inline on the card the Host is already reading (an **Interaction Block**). Its updates were ad-hoc: discovery and first render came from the 3 s request poller; a click mostly repainted "the accumulator's current card" through a PATCH; a request resolved by another client was stripped from memory but never repainted; and a card that stopped being the accumulator's current card had no repaint path at all. Observed: a block clicked on an older card only toasted; a remotely-resolved block lingered on a finished turn's card forever; permission/answer disappearance lagged the click by one PATCH; a block whose turn ended froze in place. This ADR fixes the lifecycle: blocks stay inline and follow the live card, and every card showing a live block carries a handle that lets cola repaint it — cached last-rendered JSON plus a `request_id → message_id` registry — so a click updates the clicked card atomically in the callback ack and a remote resolution repaints within one sweep.

## Context

- **Placement today.** A new request is inlined onto the owning session's live accumulator (or the nearest ancestor's, for sub-task children) and the current card is flushed; with no live accumulator it becomes a standalone card (`RequestFlow::surface`, `src/bridge/request/flow.rs:710-808`; host resolution `inline_host_session`, `src/bridge/pollers.rs:437`).
- **Only the newest card of a chain carries the tail.** Interaction blocks render under `include_tail` (`build_card_inner`, `src/bridge/turn/state.rs:1277`), which a split switches off for the finalized card: `build_card_with_info` finalizes the filled card without the tail and advances `render_from`, and `flush_card` sends the continuation which carries it (`src/bridge/turn/state.rs:1058-1091`, `Turn::flush_card`, `src/bridge/turn/mod.rs:878`; the flush machine is `src/bridge/turn/flush.rs`). A pending request blocks the backend run, so a block rarely lives through a split; the cases that matter are the ones where the card is no longer current at all.
- **Failure paths observed.**
  - Click an inline permission → the resolution seam strips the section and flushes only the accumulator's current card (`resolve_blocks`, `src/bridge/request/delivery.rs:170`; `Turn::resolve_interactions`, `src/bridge/turn/mod.rs:1272`); the ack carries no card for inline clicks (`PermissionKind::handle_action`, `src/bridge/request/kind.rs:354`). A click on any other card (older chain card, card from a previous turn) changes nothing.
  - Request resolved elsewhere (OpenChamber, CLI) → the sweep's vanished-block pass strips an inline block from the accumulator but no path repaints the hosting card (`resolve_vanished_inline`, `src/bridge/request/kind.rs:195`; `Turn::resolve_vanished_permissions`/`_questions`, `src/bridge/turn/mod.rs:1221-1247`). Standalone cards survive through `mark_stale_cards` (`src/bridge/pollers.rs:476`) and snapshot claims are rebuilt, but the inline case has no equivalent. While a turn runs, the header timer's ~1.5 s tick incidentally repaints the current card; after the turn ends nothing does — the block stays forever.
  - A new turn replaces the accumulator (`Turn::start`'s fresh accumulator, `src/bridge/turn/mod.rs:226-265`); any still-pending block on the old card is orphaned — a click replies (or 404s) but can neither repaint the old card nor find its section. `CardsHandle`'s card map keeps no per-message card JSON, so no code path can reconstruct it.
- **The mechanisms that already work.** The question path's partial-answer branch returns a rebuilt card in the callback ack (`QuestionKind::handle_action`, `src/bridge/request/kind.rs:931`; its final answer/submit/reject branches instead clear the block with a PATCH), and Feishu applies an ack card to the clicked card atomically — this is why multi-select 已选 markers appear instantly. The callback payload carries the clicked message id (`open_message_id`, `src/feishu/ws.rs:369-381`), so cola can target the exact card. The **retired** Session Snapshot claims used to track a block to its host message and rebuild it in place (`src/bridge/snapshot_claims.rs:129-171`; retired by ADR-0100, no such module remains). Standalone request cards already have `sent_cards` + `mark_stale_cards` (`src/bridge/pollers.rs:476`).
- **The Host's constraint.** A standalone request card was proposed and rejected: it fragments the message stream, and whichever card is newest, the other's live updates go out of view ("要么我最新看到的永远是这些独立卡……要么这个消息要拆得很碎").

## Decision

**Interaction blocks stay inline and follow the live card.** Seven rules:

1. **Follow the live card.** A split keeps the tail (and with it the block) on the newest card, as today. When a new turn replaces the accumulator, the sweep re-hosts every still-pending block of that session onto the new card and repaints the old card without it. A block never lives on a card that is not the session's current card for longer than the re-host/repaint takes.

2. **Every card showing a live block is repaintable, through its card handle.** The handle is a `request_id → (message_id, kind, session, directory)` registry plus, for each message that shows a live block, the card JSON as last rendered. Every update — click, remote resolution, re-host, sweep strip — rebuilds that card from the cache and PATCHes it. The cache entry is released when no live block references the message. The handle is what makes the observed failure paths fixable; it is the only way to keep controls honest on a card whose accumulator is gone (a replaced turn, a stopped turn).

3. **A click updates the clicked card in the ack.** The callback response carries the cached card with the block updated (partial answer state) or replaced by an Interaction Receipt (resolved) — atomic on the clicked card, no PATCH race and no dependence on "which card is current". This unifies permissions and final submits with the question path's existing mechanism; the split-probe clone fallback (`Turn::ack_card`, `src/bridge/turn/mod.rs:1302`) and the PATCH-only disappearance go away.

4. **Resolution leaves an Interaction Receipt in the transcript, keyed by the moment of resolution, and it names its target.** The receipt is a timeline entry whose key is the moment the block was resolved; the timeline itself is ordered by each part's own server-side start time (text/reasoning at `time.start`, tools under `state.time.start`), and a part rendered late — the poll delivers a panel when its content lands, which can be after later parts — is inserted at its key's position instead of appended. So the receipt sits below everything the Host could see when they clicked, including a command or reasoning the server had already written before the click but cola rendered afterwards, and above everything that follows. Live blocks stay in the card's tail; only the residue is a transcript entry. It carries its target, so it still identifies its request where the position alone cannot: a sub-task child's block has no command panel on the parent card, and a toggle resolves several blocks at once. One markdown line, no controls, so a late click can never be ambiguous: `✅ 已允许一次：⚡ 执行 Shell 命令 \`ls -la\``, `✅ 已回答：目录 /a、分支 main`, `🚫 已拒绝：…`, `⏱ 已由其他客户端处理：…`. The one receipt that does not name a target is the Auto-Accept toggle's: it reports the MODE change (`🔄 已开启自动授权：后续权限请求将自动批准`), because naming a command read as "only this command gets auto-approved". A mode change resolves however many blocks are pending, so it leaves ONE such line per card it repaints (a resolution can repaint more than one card) and the blocks it swallowed are dismissed without receipts of their own — the same line repeated per block would be noise, not a record. A receipt shows no clock: the decision happens in cola's world, where no server time exists, and only server times (a part's own start time on its panel, the turn's user message on the header date) may reach the card — a cola time in the same costume would mix two clocks on one card. Applies on the Session Snapshot's claimed blocks too. The 2026-11-09 amendment below narrows the "each part's own server-side start time" premise: a V2 text part carries no time, so it sorts by the transcript's own part order instead.

5. **Remote resolution repaints within one sweep (≤3 s), after the turn as well.** The sweep repaints the block's card from the cache instead of only stripping memory. Clicks stay instant (rule 3).

6. **Standalone cards and snapshot claims keep their lifecycles.** Where no accumulator exists (restarts, external turns, pre-existing adopt-time pendings) the standalone card path with `sent_cards`/stale marking stays as it is — a resolved standalone card keeps its existing result-card rendering. A Session Snapshot's claimed blocks ARE Interaction Blocks and adopt Interaction Receipts; the claim registry itself is unchanged.

7. **An unfinished cola turn rejects what it left pending (#187).** When a cola turn ends without completing — `/stop`, an interrupt, a prompt error — the tool fibers behind its still-pending Permission/Question requests are dead, so answering them reaches nobody. The turn rejects every such request of its session or a sub-task descendant at the source and settles its block into a `🚫 已拒绝` receipt (not the neutral `⏱ 已由其他客户端处理` line: cola itself decided). This removes the ghost-block case at its source; a request that is genuinely still live when a card stops being current (external turns, a turn replaced mid-flight) remains rule 1's re-host.

## Why

- The block lives where the Host reads; a separate card splits the stream and moves live updates out of view (Host constraint).
- A single repaint path through the handle removes the whole class of "which code path repaints which card" bugs: every mutation of a block's presence or state goes through it, including the ones that used to repaint nothing.
- The ack card is the only mechanism Feishu guarantees for refreshing the clicked card; a PATCH around a callback leaves the clicked card on its pre-answer state (the observation already recorded in the question path).
- An Interaction Receipt makes resolved state readable and stale clicks unambiguous — the misunderstanding the Host reported.
- Parts render when their content lands, not in server order, so the timeline is keyed by the part's own start time: appending by discovery order moved a gated command and its reasoning below the receipt that resolved it.

## Consequences

- New core state: the block registry and the per-message card-JSON cache, released as blocks resolve. Memory is one card JSON per card with live blocks (bounded by concurrent pendings).
- `pending_permissions`/`pending_questions` remain the render source of truth for the tail; the registry/cache is the repaint source of truth. The two must be kept in step — a single mutation seam (add / state change / resolve / re-host) is required or they drift.
- The registry, the card-JSON cache and the standalone-card records are persisted beside `sessions.json` and re-adopted on startup (2026-09-24 update), so a restart no longer duplicates or freezes a pending request's surface.
- Cross-client state is eventual within one sweep, by design (ADR-0004 keeps polling).

## Alternatives considered

- **Standalone request cards** (one card per interaction; the streaming card only shows the waiting header): predictable per-message updates, but fragments the stream and moves the live interaction away from where the turn is read. Rejected by the Host.
- **Pin blocks to the card where they first appeared**, repainting through the same handle: no re-hosting, but a still-pending request can sit above the newest card — the "scroll up to find the live thing" problem. Rejected.
- **Retain the replaced accumulator and rebuild old cards from it**: heavier than caching the rendered JSON, and the split state (`render_from`) makes reconstruction error-prone. Rejected.
- **No cache; toast-only clicks on stale cards**: leaves the observed "clicked, nothing changes" behavior in place. Rejected.
- **Replace a stale whole card with a result card**: destroys the card's streamed content. Rejected.

## Out of scope

- **Notifications when an inline block first appears**: a card PATCH does not notify, and a new-message ping was considered and deferred — not re-litigated here.
- **SSE or any push channel for cross-client resolution**: the ≤3 s sweep latency is accepted (ADR-0004/0011 keep polling).
- **Reworking the Session Snapshot beyond Interaction Receipts**: its claim model, gathering and busy-follow stay as ADR-0028 left them.
- **Post-restart guarantees**: the interactive surface state is persisted and re-adopted (2026-09-24 update) — a still-pending request keeps its pre-restart card, repainted, and never gets a second one. A card deleted while cola was down, or one whose persisted JSON is unusable, falls back to the standalone path; streaming content with no live block is still not persisted (a pre-restart card that carries no block may freeze).

## Risks / open questions

- The two sources of truth (accumulator sections + registry/cache) need one mutation seam; without it they will drift. This is the main implementation risk.
- The cached card is the same JSON Feishu already accepted, so a surgical edit stays under the card size cap.
- The Interaction Receipt line adds a line to every card that hosted an interaction; if it proves noisy it can be collapsed or shortened later — presentation-level.
- A click on a pre-restart card is a first-class surface now (the handle is re-adopted); a click on a card the registry does not know (persistence off, or the card was never recorded) stays best-effort: the request is still replied to, but that card may not repaint.
- The re-host on a new turn is rare (the busy guard prevents a new turn while a request is pending; it happens when a request outlives a cancelled/aborted turn) and needs a regression test.
- The persisted record is written through on every registry mutation, so a crash between a card write and its mirror leaves the record one surface stale; the sweep's reconciliation (stale marking, vanished-block receipts) heals it on the next start.

## Domain note

The concepts enter the glossary as **Card Chain**, **Interaction Block** (交互块) and **Interaction Receipt** (回执).

## Update (2026-09-17)

Rule 2's single mutation seam also needs **serialization** — the risk the
original Risks section named. Every card write is a read-send-record sequence
(`flush_card`'s build → Feishu PATCH → handle record; `resolve_blocks`' mutate →
ack/PATCH), and its callers are concurrent: the render poll, the two request
pollers, and click acks. Two interleavings were observed live:

- a flush that snapshotted before a resolution recorded the resolved block back
  (its stale body overwrote the receipt), and the sweep then reported cola's own
  auto-accept approval as `⏱ 已由其他客户端处理` — the Host saw exactly that
  line before the mode receipt;
- a flush whose build had already advanced a split PATCHed the continuation's
  slice onto the card it had just finalized: two identical messages, only one
  tracked and repaintable, the other frozen with live controls (the duplicate
  question cards the Host answered twice).

**Rule 8: one card writer per session at a time.** `flush_card` and
`resolve_blocks` hold the session's card-write lock
(`SharedCore::card_write_lock`) across their whole sequence, so a resolution can
never be overwritten by a stale flush and a split advances its card id before
another flush reads it. The lock deliberately covers exactly these two writers:
they are the ones that can name the same message (the flush's PATCH and a
resolution's ack/cache edit). The sweep's and re-host's own patches target
standalone or superseded cards that no flush PATCHes; where a racing flush does
revive a block the sweep already resolved, the next sweep re-resolves it into
the same neutral line, so those paths stay lock-free rather than serializing the
poll loop behind a click. The sweep's vanished passes instead skip every request
cola itself is answering or has answered (`answered_requests` ∪
`settling_requests` — the latter claimed before an auto-accept approval's reply
lands and released when `resolve_blocks` settles; a claim left behind by a
cancelled settlement expires after a short TTL), because the neutral `⏱` receipt
is only true when another client did the resolving. The auto-accept toggle card
now settles its approved blocks with the mode receipt exactly as the command and
the permission card's own button do, instead of leaving them to the sweep.

## Update (2026-09-18)

Rule 3's ack now has a deadline. Feishu fails a card callback it does not hear
back from within 3 s (`200341`), so the WS loop answers within
`CARD_ACK_BUDGET` (2 s, `src/feishu/ws.rs`) even when the handler is still
running: it acks with a `处理中…` toast and drops the late result. The next
render poll repaints the session's current accumulator card; a dropped result
on a standalone card or a non-current handle card (rules 2 and 6) is not
repainted, and its controls stay live until a second click re-serves the
settled result. A handler that beats the budget still updates the clicked
card in the ack exactly as rule 3 says — the budget changes nothing on the
normal path, it only bounds the stalled one. The request-reply calls that gate the slow paths
(permission/question) carry their own total timeout
(`REPLY_TIMEOUT`, `src/opencode/client.rs`), so a stalled backend surfaces as
the existing retryable failure card inside the budget instead of riding the ack
deadline.

## Update (2026-09-24)

The restart behavior the original Consequences and Out-of-scope accepted is
replaced: the interactive surface state is **persisted and re-adopted** (#309),
so a still-pending request keeps exactly one live surface across a `/restart`
or a crash. The block registry, the per-message card-JSON cache and the
standalone-card records are mirrored to `interactive_surfaces.json` beside
`sessions.json` (best-effort, atomic temp-file replace, an empty record
removes the file — the reminder's `pinned_chats.json` pattern) and written
through on every registry mutation (`Surfaces`, `src/bridge/surfaces.rs`).
The card JSON is written only when a card's live blocks change: a live block
freezes the turn's content, so the header timer's re-flushes carry no new
interactive state and must not rewrite the file every second.

On startup `CardHandles` hydrates from the record and each flow seeds a
one-shot `recovered` set from its kind's entries. The first sweep that lists a
still-pending recovered request **re-adopts** its card instead of posting a
second one (`RequestFlow::readopt_surface`): the kind remembers what a click
needs (a question's full request), then the persisted card is PATCHed — for a
question, with its block re-rendered from the state this process holds, so
partial answers that did not survive the restart are not shown as selected.
A recovered standalone card is already the live surface and is left as it is.
A card that cannot be repainted (message deleted, unusable cached JSON) is
forgotten and the request is surfaced as a standalone card in the same sweep.
The kind's `prepare` deliberately does not run on the re-adoption path: a
request the previous process left pending was not auto-accepted then, and
re-adopting its card keeps the decision with the Host; auto-accept still
applies to newly-listed requests.

A click on a re-adopted card resolves exactly as before the restart: the
handle registry names the clicked card, so the resolution edits its cached
JSON and the ack carries it (rules 2+3). The flow's `inline` ack choice now
also counts a card handle on the clicked card, so a question's partial answer
refreshes it in place rather than replacing it with a standalone rebuild, and
the directory fallback resolves through the persisted handle when neither the
callback payload nor the session store names one.

The sweep's reconciliation is unchanged and now runs on hydrated state too: a
request resolved while cola was down has its block stamped
`⏱ 已由其他客户端处理` from the cached JSON, and a standalone card is marked
stale; either way the persisted record is removed as its surface resolves. A
Session Snapshot adoption no longer embeds a request whose block has a card
handle — a re-adopted card is already surfaced (ADR-0028's
`is_already_surfaced`).

## Amendment (2026-10-02): a vanished request is classified by its session's run state

Rule 4's receipt vocabulary gains one line: `⏱ 已随会话中断：…`. When a
Permission/Question leaves the pending list and cola did not answer it, the
sweep reads the owning session's run state (ADR-0059's live predicate, one
bounded read per distinct session with a vanished block) before choosing the
line:

- the run is still live (`Busy`/`Retry`) → the neutral
  `⏱ 已由其他客户端处理` stands: another client answered while the run
  continued;
- the run reads `Idle` → `⏱ 已随会话中断`: the execution that owned the request
  is gone (a stop, a crash, a server restart, or the server's own inactivity
  eviction — the 60-minute recycle that interrupts a session parked on an
  unanswered request), so nobody consumed it and claiming another client did
  would be a lie;
- a failed or unrecognised read is UNKNOWN and keeps the neutral line —
  unknown is never read as an interrupted run, the same rule a failed list
  gets (#130).

The classifier only runs when a block actually vanished, so a sweep with
nothing resolved makes no status call. It covers all three sweep surfaces: the
inline receipt (accumulator and card-handle paths) and the standalone card,
whose stale copy becomes `⏱ 已随会话中断` / 「会话已中断，该…请求未处理。」
(`build_interrupted_card`) and whose persisted surface record now carries the
owning session id, so a restarted process can classify it too. An empty owner
(a record written before the id was persisted) is UNKNOWN and keeps the
neutral copy. Snapshot claims (ADR-0028) and a late click's 404 card keep
their existing line.

## Update (2026-10-20)

Rule 4's receipts yield to the Session Snapshot's one-shot card budget (#726).
A snapshot has no splitter, so when its 最近对话 tail plus its receipts cannot
fit one Feishu card, the OLDEST receipts are omitted first — their count named
on the card (`…更早的 N 条处理回执已省略（超出单卡容量）`) — and then the oldest
tail entries, the same way.

The rule's requirement is about a receipt's CONTENT, not its permanence on a
one-shot surface: every receipt the snapshot renders still names its target in
one markdown line, the omitted ones keep their home in the session transcript,
and an unrendered receipt is absent rather than mis-targeted — the ambiguity
rule 4 exists to prevent cannot arise. Receipts are evicted before the
conversation because they are residue for blocks another client already
resolved, while the tail is what an adopting reader came for.

The snapshot's claimed pending blocks are the ONE non-evictable section: a
claim whose block the card does not render leaves a request the user cannot
answer there. The rule enforcing that invariant is PARTIAL rather than
all-or-nothing (#842, refining #841): the card embeds the longest PREFIX of the
session's own pendings that fits one card — capped at five blocks, so the block
share stays small enough for the 最近对话 section the reader adopted the session
for — and leaves the rest UNCLAIMED, named on the card
(`…另有 N 个待处理请求超出单卡容量，将稍后显示`). The cap bounds the blocks'
share; it does not reserve the tail, so a tail that is over budget on its own is
still trimmed by the ladder below. The copy promises neither a
standalone card nor another card: an unclaimed request is surfaced INLINE on the
session's live card when one exists, and only falls back to a card of its own
otherwise.

The split is decided ONCE, at send. `SnapshotData.pending` is trimmed to the
embedded prefix, so a rebuild re-renders the stored decision and never re-derives
it from the post-resolution claim set — otherwise the ladder would subtract the
leftover count a second time and drop a still-claimed block. Rebuilds also stop
naming the COUNT (`…另有待处理请求…`): the unclaimed requests are outside the
claim registry, so how many survive cannot be observed from the snapshot.

## Update (2026-10-31): a live form freezes the render pass (#917)

Feishu rebuilds `body.elements` on every card update, and a form input's
unsubmitted text lives only in the client — so **any** repaint of a card that
renders a free-text answer field wipes what the Host typed. A card with an
unanswered typed answer (`StreamAccumulator::awaits_typed_answer`: a live
question field with a custom-answer box or a numeric form) is therefore not
written by the **render pass** at all: the live header's tick (ADR-0014), the
ledger's rows and its completion entries, a task panel's activity, a file
delivery and even a freshly rendered part stand down. The pass's own content is
not special: a pass can render a part, pause for its reads and resume after a
question landed, and the Host may already be typing (the gate's own view of
"content" is the delta it found before those reads). This is the "a live block
freezes the turn's content" rule reaching the write itself, not just the
persisted mirror.

Nothing is lost, because nothing is consumed: the pass plans no completion
entry, applies no ledger, spends no liveness read, and does not build the card —
the build is what advances `render_from`, so the first pass after the answer
renders the whole backlog at once. (A read applied to a body nothing writes
would make every later read see "no difference", and its entry would never
reach a card or the durable record.)

The form's own writes still land, because they are not the render pass: the
block's arrival and a click's ack both write the card the Host is looking at
(已选/✅ markers, a receipt), and the answer's resolution flushes; the settled
wait lands too, since a true end is not progress. The form of a mid-pass
arrival is handled where it can be: the pass's apply (entries, footer, ledger,
panel activity), its form check and its build share ONE card-write-lock
acquisition — the lock a question's block insertion also takes
(`Turn::add_question` / `add_permission`) — so a question cannot land between
the check and the build, and "insert, then flush" is one ordered step against
every card write: the form's flush either lands first (the pass stands down,
having applied nothing) or after it. A question answered only by clicking
renders no input and keeps everything; a permission block never has one.

One more write owns the render gate's header cache: a click's callback ack is
the card's body, so a resolution that restamps the header advances the cache
with it (`Turn::mark_header_shown`) — otherwise the next tick re-sent the
identical ack body, which is the same wipe one click later.

## Amendment (2026-11-07): the sweep takes the session card-write lock for its surfacing decision

Rule 8 named `flush_card` and `resolve_blocks` as the only holders. ADR-0100's
`/card` busy follow adds a third: `RequestFlow::surface` takes a card write lock
for its **whole surfacing decision** — the inline-host check, the block
insert/flush, and the standalone send with its `sent_cards` record — so the
follow arm and the sweep cannot both deliver one request. A sweep that read
"no live card" just before the arm must not post a standalone card for the
request the follow seeds.

The lock remains per-session, and a sweep **never holds two Sessions' locks**
(ADR-0100's 2026-11-07 amendment). It resolves the inline host first, bounded
and before any lock because the parent-chain walk reads the server. When a host
exists, the inline decision — re-checking the host's liveness, inserting the
block, flushing — takes **the host's** card write lock, the same lock `/card`'s
post-and-arm and a busy follow take. If the host is no longer live (a Turn that
ended between the walk and the lock, or a card that vanished), that lock is
released and the standalone path takes **the request Session's own** write lock
for its decision, send and `sent_cards` record. The two locks are never nested:
a sub-task child's lock is not held while its ancestor host's is, so the old
child → ancestor acquisition and its liveness check under the wrong lock are
gone. The standalone send is one Feishu create and is deliberately not wrapped
in a request timeout: a create has at-least-once semantics, so abandoning a
slow response could orphan a card that did land and a retry would post a second
(ADR-0100). The sweep patches that target standalone or superseded cards — the
ones Rule 8 already calls lock-free — stay exactly that.

## Amendment (2026-11-09): V2's timeless text sorts by the transcript's own part order

Rule 4 orders the timeline by each part's own server-side start time. V2 breaks
that premise for text: `AssistantText` carries **no time at all** (only
`AssistantReasoning` and `AssistantTool` do, and reasoning's is optional), so
cola decoded every V2 text part with no start time and keyed its timeline entry
by `next_order()` — cola's wall clock. Two clocks then shared one ordering key.
A text part first read together with the tool call it precedes got a "now" key
*after* the tool's server `time.created`, so the tool panel sorted above the text
it followed (issue #1022; the question tool made it visible: the Host was asked
before the model's explanation of why, and the explanation stayed below the
question panel after it settled).

The order key is now **the transcript's**, not a clock, for the parts that were
mis-ordered:

- **Before the message's first tool call**, a part with no server time takes the
  start time of the previous timed part in the same message, or the message's own
  `time.created` when it has none. V2's `content[]` is in output order, so this
  is the position the server wrote; for a timed part the rule is a no-op. That
  prefix is a **floor**: a timed part whose own start is behind it takes the
  floor instead, so no part of the prefix sorts before content already placed
  from it.
- A part with no time that **follows a tool call** keeps the pre-#1022 render
  order (`next_order()`): it was produced after that call's result, and moving it
  onto the transcript's position would carry it above the Interaction Receipt the
  answer wrote at the click (Rule 4). This boundary is the tool part, live or
  settled; the receipt and late-part rules are otherwise untouched.
- Equal keys order by **allocation**, not settle order: a live panel allocates
  its position where its part stands in the read and joins the timeline only
  when it settles, so the equal-key run keeps the panel at that position instead
  of appending it after a part already pushed at the same key. The sort uses an
  internal `order`: an ordinary insert's `order` equals its `seq`, a chunked
  continuation shares its part's `order`, and the render-boundary clamp lowers
  `order` alone — never the identity `seq` (ADR-0040).
- A part already on the timeline **keeps the key it was placed at** — a rewrite
  captures the old key before it drops the run — because a later read's floor can
  differ, and re-keying would split a growing part into two entries.
- **Across messages, read order is monotonic.** Each walk floors a message's
  parts at the highest key a part from an EARLIER transcript message already
  holds (`read_floor_before`/`set_read_floor`). A post-tool part that took
  `next_order` therefore cannot be overtaken by a later message whose own
  server time is older, on any read pass — including the pass that dedupes the
  earlier message. Inside one message the transcript part order wins: the floor
  is the earlier messages' high-water mark, never the current message's own
  parts, so a timed part keeps its server position and a late pre-tool part
  still lands before the tool it precedes.

`shown_at` — what a panel header displays and what the Rendered Cursor frontier
records as the part's start — is unchanged: it still carries only the time the
server reported, so a borrowed key never becomes a fake clock or perturbs a
cursor. Interaction Receipts keep Rule 4's resolution key (`next_order`, or the
key a click reserved before replying): a receipt is an event in cola's world,
and its key orders only; the server-time cut above is what floors later
messages against it. The rule applies to
every walk that places transcript parts (the Turn window, the seed scope and the
pending gap); synthetic content (receipts, wake markers) keeps `next_order()`,
except the advisory-live opening line, which is keyed at the card head (`0`) so
an older server position can never sort above it.

**The receipt's server-time cut.** The borrow no longer stands alone: the
surfacing work (#1023) carries the asking tool part's server time on the block,
and a resolution records it as the receipt's cut. A transcript MESSAGE the
server created after that ask is post-click content, so every part of it —
including an untimed V2 text that borrowed its `time.created` — is floored at
the receipt's key. Under any clock skew the continuation therefore stays below
the receipt, and so does one delivered to a card that showed no timed part
before the click (there is no server high-water mark to compare, but the cut is
the request's own ask). Already-placed entries keep their position; the remote
answer window is described in the 2026-11-10 amendment below.

## Amendment (2026-11-09): a dismissed Location Form is a deliberately surface-less pending request

Rule 6 assumes a still-pending request has exactly one live surface, and re-posts
one whose card is gone so the Host is never left without a way to answer. ADR-0103
adds the one exception: a **Location Form** the Host deferred with 「稍后」 is still
pending but intentionally has no live card. Its persisted dismissed record is
authoritative: the sweep must not re-post it, and restart re-adoption applies only
to a card that was not dismissed; `/mcp` is the way back. The exception does not
apply to a session-backed request, or to a Location Form that was never dismissed.

## Amendment (2026-11-10): the sweep drains the session before it surfaces a block, and a receipt carries a server-time cut

The 2026-10-31 update made a live form freeze the render pass, which is right —
but it has a consequence at **surfacing**: the request sweep lists a pending
question independently of the render loop, and the assistant's explanation of
*why* it is asking sits in the same transcript, not yet rendered. If the sweep
inserts the block first, the card now carries an unanswered input, so the render
pass stands down and the explanation is held until after the answer (issue
#1023): the Host is asked to choose blind.

`RequestFlow::surface` therefore **drains the session once before the inline
block lands**: one bounded transcript read, one bounded **render-only** pass,
then the block insert and its flush — so the text the Host needs is on the card
when the question appears. The drain runs only for the request's **own** session
with a live card; an ancestor host renders no part of a child's transcript, and
a standalone card has no card to drain. It is best-effort by construction: a
failed or timed-out read changes nothing, the request surfaces exactly as before
without the anchor, and it costs at most one read per first surfacing.

The render-only split is deliberate. The **read** is bounded — ONE deadline for
the transcript read and the render pass together (5 s, or the flow's list
timeout when shorter) — but the **write** it owes is not: the pass renders into
the accumulator and the flow flushes afterwards, outside that bound, because a
card create carries no idempotency key and abandoning one could duplicate it
(ADR-0100). The pass also skips the side effects a render normally owns outside
the flush — the auto-rename's title write and the File Message deliveries — since
both write to Feishu and neither is guarded by the card write lock; the next
ordinary render performs them (a pending file delivery stays pending). The
progress mark tells the two apart — a pass the timeout abandoned after applying
content still owes its flush, a pass that found nothing new owes no write at
all. The flush re-checks the live-form freeze under the write lock, so a question
that landed since the apply still refuses the repaint (#917) and the applied
content rides the form's own resolution flush.

The same read yields the **asking tool part's server time** — in the ordinary
case the `question` / permission tool call is the newest part the server wrote.
The anchor is the read's **high-water mark**, not one part id: the read happens
before the click, and while a request is pending its turn is blocked, so nothing
the Host could have seen — a parallel tool call or a queued message included —
exceeds it, and nothing is produced between the read and the click either. A
newer sibling part only raises the mark; a lower anchor is the unsafe choice.
That time travels on the block
(`PendingQuestion::asked_at` / `PendingPermission::asked_at`) and, when the block
resolves, becomes the receipt's **server-time cut** (`ReceiptCut`): the receipt
itself keeps its `next_order` key — it must follow everything the Host could see
— and a later transcript **MESSAGE** the server created after the ask is floored
at that key. The unit is the message, not the part: a part that STARTED before
the click but grew after it belongs to a pre-click message and keeps its place;
a parallel tool's late-delivered part sits in the asking message too. A new
message is only created once the turn resumes, i.e. after the click, so
`message.created > ask` is the sound test. Both halves are on the server's
clock, so cola's wall clock cannot decide the comparison.

Every anchored receipt keeps its cut rather than only the newest one: requests
can be asked in one order and answered in another, and content written between
the asks must clear the receipt of the *last* click too. The floor is the newest
receipt key among the asks that precede the content's server time — a staircase,
not a single "latest" cut.

Content can be PLACED before its receipt lands: the backend reply returns before
the receipt is written, so a render poll may insert the post-click continuation
in that gap. A **resolving click reserves the receipt's key** before it replies
(`reserve_receipt`), which floors future inserts at `key + 1`; the receipt then
takes the reserved key, so the gap content follows it. Nothing already placed
moves: everything on the card before the click was something the Host could see.
An Auto-Accept toggle reserves before its reply too, with the LATEST ask among
the live blocks its one receipt covers; the receipt takes that reserved key
(`take_reserved_receipt`), and the blocks it resolves through the shared receipt
drop their own reservations (`consume_reservation`). A reservation that never
becomes a receipt drops its floor with it (`forget_reservation`, by token, so a
stale click's late cleanup cannot delete a retry's fresh reservation). The reservation consumes its
floor key from the synthetic key sequence, so no unrelated receipt can take the
same key and be cut by a later cleanup.

When a QUESTION is resolved with no reservation (a remote client's answer,
noticed by the next sweep), the ask cut floors future messages only — entries
already placed keep their server position. There is no click time to cut
against, and moving placed content (child/background or external content can
reach the card while a question waits) would violate rule 4 for what the remote
Host already saw. That window (one sweep interval) is a deliberate limit. A
permission's receipt records the same message-scoped cut: parallel tools write
their parts into the ASKING message (created before the ask), so they are never
floored, while a new message after the answer follows the receipt. Every merge lookup uses the
floored (and split-clamped) key, so a part cannot split into two entries across
reads, and a part that spans the cut keeps one entry above the receipt.

A request whose asking part lives in a CHILD session (hosted on an ancestor's
card) is anchored by one bounded read of the child's transcript, without
rendering it: the ancestor card renders no child parts, but the anchor is still
a property of the request. The busy-follow seed does the same: its plan carries
the anchor from one bounded read, so a block attached by the follow has the cut
the sweep would have captured. The reservation takes the card write lock under a
short budget (a click runs on Feishu's ack deadline, and a concurrent flush holds
that lock across a network PATCH): under contention the reservation is skipped
and the receipt keeps its default position. A block that reached the card without a surfacing read
(a restart re-adoption) carries no anchor and keeps the previous receipt
behaviour; a **re-hosted** block keeps the anchor it was first
surfaced with — the anchor belongs to the request, not to the card it sits on, so
it travels in the block handle the registry writes at every render (the old
accumulator is already gone when a re-host runs). A chunked entry's continuation
chunks take the same floor, so the timeline stays key-sorted under the receipt.
A mode change (Auto-Accept) that resolves several blocks leaves one receipt and
cuts at the latest ask among them.
