# Interaction blocks: inline on the live card, repainted through a card handle

A pending Permission/Question is rendered inline on the card the operator is already reading (an **Interaction Block**). Its updates were ad-hoc: discovery and first render came from the 3 s request poller; a click mostly repainted "the accumulator's current card" through a PATCH; a request resolved by another client was stripped from memory but never repainted; and a card that stopped being the accumulator's current card had no repaint path at all. Observed: a block clicked on an older card only toasted; a remotely-resolved block lingered on a finished turn's card forever; permission/answer disappearance lagged the click by one PATCH; a block whose turn ended froze in place. This ADR fixes the lifecycle: blocks stay inline and follow the live card, and every card showing a live block carries a handle that lets cola repaint it — cached last-rendered JSON plus a `request_id → message_id` registry — so a click updates the clicked card atomically in the callback ack and a remote resolution repaints within one sweep.

## Context

- **Placement today.** A new request is inlined onto the owning session's live accumulator (or the nearest ancestor's, for sub-task children) and the current card is flushed; with no live accumulator it becomes a standalone card (`RequestFlow::surface`, `src/bridge/request/flow.rs:710-808`; host resolution `inline_host_session`, `src/bridge/pollers.rs:437`).
- **Only the newest card of a chain carries the tail.** Interaction blocks render under `include_tail` (`build_card_inner`, `src/bridge/turn/state.rs:1277`), which a split switches off for the finalized card: `build_card_with_info` finalizes the filled card without the tail and advances `render_from`, and `flush_card` sends the continuation which carries it (`src/bridge/turn/state.rs:1058-1091`, `Turn::flush_card`, `src/bridge/turn/mod.rs:878`; the flush machine is `src/bridge/turn/flush.rs`). A pending request blocks the backend run, so a block rarely lives through a split; the cases that matter are the ones where the card is no longer current at all.
- **Failure paths observed.**
  - Click an inline permission → the resolution seam strips the section and flushes only the accumulator's current card (`resolve_blocks`, `src/bridge/request/delivery.rs:170`; `Turn::resolve_interactions`, `src/bridge/turn/mod.rs:1272`); the ack carries no card for inline clicks (`PermissionKind::handle_action`, `src/bridge/request/kind.rs:354`). A click on any other card (older chain card, card from a previous turn) changes nothing.
  - Request resolved elsewhere (OpenChamber, CLI) → the sweep's vanished-block pass strips an inline block from the accumulator but no path repaints the hosting card (`resolve_vanished_inline`, `src/bridge/request/kind.rs:195`; `Turn::resolve_vanished_permissions`/`_questions`, `src/bridge/turn/mod.rs:1221-1247`). Standalone cards survive through `mark_stale_cards` (`src/bridge/pollers.rs:476`) and snapshot claims are rebuilt, but the inline case has no equivalent. While a turn runs, the header timer's ~1.5 s tick incidentally repaints the current card; after the turn ends nothing does — the block stays forever.
  - A new turn replaces the accumulator (`Turn::start`'s fresh accumulator, `src/bridge/turn/mod.rs:226-265`); any still-pending block on the old card is orphaned — a click replies (or 404s) but can neither repaint the old card nor find its section. `CardsHandle`'s card map keeps no per-message card JSON, so no code path can reconstruct it.
- **The mechanisms that already work.** The question path's partial-answer branch returns a rebuilt card in the callback ack (`QuestionKind::handle_action`, `src/bridge/request/kind.rs:931`; its final answer/submit/reject branches instead clear the block with a PATCH), and Feishu applies an ack card to the clicked card atomically — this is why multi-select 已选 markers appear instantly. The callback payload carries the clicked message id (`open_message_id`, `src/feishu/ws.rs:369-381`), so cola can target the exact card. Session Snapshot claims already track a block to its host message and rebuild it in place (`src/bridge/snapshot_claims.rs:129-171`). Standalone request cards already have `sent_cards` + `mark_stale_cards` (`src/bridge/pollers.rs:476`).
- **The operator's constraint.** A standalone request card was proposed and rejected: it fragments the message stream, and whichever card is newest, the other's live updates go out of view ("要么我最新看到的永远是这些独立卡……要么这个消息要拆得很碎").

## Decision

**Interaction blocks stay inline and follow the live card.** Seven rules:

1. **Follow the live card.** A split keeps the tail (and with it the block) on the newest card, as today. When a new turn replaces the accumulator, the sweep re-hosts every still-pending block of that session onto the new card and repaints the old card without it. A block never lives on a card that is not the session's current card for longer than the re-host/repaint takes.

2. **Every card showing a live block is repaintable, through its card handle.** The handle is a `request_id → (message_id, kind, session, directory)` registry plus, for each message that shows a live block, the card JSON as last rendered. Every update — click, remote resolution, re-host, sweep strip — rebuilds that card from the cache and PATCHes it. The cache entry is released when no live block references the message. The handle is what makes the observed failure paths fixable; it is the only way to keep controls honest on a card whose accumulator is gone (a replaced turn, a stopped turn).

3. **A click updates the clicked card in the ack.** The callback response carries the cached card with the block updated (partial answer state) or replaced by an Interaction Receipt (resolved) — atomic on the clicked card, no PATCH race and no dependence on "which card is current". This unifies permissions and final submits with the question path's existing mechanism; the split-probe clone fallback (`Turn::ack_card`, `src/bridge/turn/mod.rs:1302`) and the PATCH-only disappearance go away.

4. **Resolution leaves an Interaction Receipt in the transcript, keyed by the moment of resolution, and it names its target.** The receipt is a timeline entry whose key is the moment the block was resolved; the timeline itself is ordered by each part's own server-side start time (text/reasoning at `time.start`, tools under `state.time.start`), and a part rendered late — the poll delivers a panel when its content lands, which can be after later parts — is inserted at its key's position instead of appended. So the receipt sits below everything the Host could see when they clicked, including a command or reasoning the server had already written before the click but cola rendered afterwards, and above everything that follows. Live blocks stay in the card's tail; only the residue is a transcript entry. It carries its target, so it still identifies its request where the position alone cannot: a sub-task child's block has no command panel on the parent card, and a toggle resolves several blocks at once. One markdown line, no controls, so a late click can never be ambiguous: `✅ 已允许一次：⚡ 执行 Shell 命令 \`ls -la\``, `✅ 已回答：目录 /a、分支 main`, `🚫 已拒绝：…`, `⏱ 已由其他客户端处理：…`. The one receipt that does not name a target is the Auto-Accept toggle's: it reports the MODE change (`🔄 已开启自动授权：后续权限请求将自动批准`), because naming a command read as "only this command gets auto-approved". A mode change resolves however many blocks are pending, so it leaves ONE such line per card it repaints (a resolution can repaint more than one card) and the blocks it swallowed are dismissed without receipts of their own — the same line repeated per block would be noise, not a record. A receipt shows no clock: the decision happens in cola's world, where no server time exists, and only server times (a part's own start time on its panel, the turn's user message on the header date) may reach the card — a cola time in the same costume would mix two clocks on one card. Applies on the Session Snapshot's claimed blocks too.

5. **Remote resolution repaints within one sweep (≤3 s), after the turn as well.** The sweep repaints the block's card from the cache instead of only stripping memory. Clicks stay instant (rule 3).

6. **Standalone cards and snapshot claims keep their lifecycles.** Where no accumulator exists (restarts, external turns, pre-existing adopt-time pendings) the standalone card path with `sent_cards`/stale marking stays as it is — a resolved standalone card keeps its existing result-card rendering. A Session Snapshot's claimed blocks ARE Interaction Blocks and adopt Interaction Receipts; the claim registry itself is unchanged.

7. **An unfinished cola turn rejects what it left pending (#187).** When a cola turn ends without completing — `/stop`, an interrupt, a prompt error — the tool fibers behind its still-pending Permission/Question requests are dead, so answering them reaches nobody. The turn rejects every such request of its session or a sub-task descendant at the source and settles its block into a `🚫 已拒绝` receipt (not the neutral `⏱ 已由其他客户端处理` line: cola itself decided). This removes the ghost-block case at its source; a request that is genuinely still live when a card stops being current (external turns, a turn replaced mid-flight) remains rule 1's re-host.

## Why

- The block lives where the operator reads; a separate card splits the stream and moves live updates out of view (operator constraint).
- A single repaint path through the handle removes the whole class of "which code path repaints which card" bugs: every mutation of a block's presence or state goes through it, including the ones that used to repaint nothing.
- The ack card is the only mechanism Feishu guarantees for refreshing the clicked card; a PATCH around a callback leaves the clicked card on its pre-answer state (the observation already recorded in the question path).
- An Interaction Receipt makes resolved state readable and stale clicks unambiguous — the misunderstanding the operator reported.
- Parts render when their content lands, not in server order, so the timeline is keyed by the part's own start time: appending by discovery order moved a gated command and its reasoning below the receipt that resolved it.

## Consequences

- New core state: the block registry and the per-message card-JSON cache, released as blocks resolve. Memory is one card JSON per card with live blocks (bounded by concurrent pendings).
- `pending_permissions`/`pending_questions` remain the render source of truth for the tail; the registry/cache is the repaint source of truth. The two must be kept in step — a single mutation seam (add / state change / resolve / re-host) is required or they drift.
- The registry, the card-JSON cache and the standalone-card records are persisted beside `sessions.json` and re-adopted on startup (2026-09-24 update), so a restart no longer duplicates or freezes a pending request's surface.
- Cross-client state is eventual within one sweep, by design (ADR-0004 keeps polling).

## Alternatives considered

- **Standalone request cards** (one card per interaction; the streaming card only shows the waiting header): predictable per-message updates, but fragments the stream and moves the live interaction away from where the turn is read. Rejected by the operator.
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
