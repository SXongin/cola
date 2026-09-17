# Interaction blocks: inline on the live card, repainted through a card handle

A pending Permission/Question is rendered inline on the card the operator is already reading (an **Interaction Block**). Its updates were ad-hoc: discovery and first render came from the 3 s request poller; a click mostly repainted "the accumulator's current card" through a PATCH; a request resolved by another client was stripped from memory but never repainted; and a card that stopped being the accumulator's current card had no repaint path at all. Observed: a block clicked on an older card only toasted; a remotely-resolved block lingered on a finished turn's card forever; permission/answer disappearance lagged the click by one PATCH; a block whose turn ended froze in place. This ADR fixes the lifecycle: blocks stay inline and follow the live card, and every card showing a live block carries a handle that lets cola repaint it — cached last-rendered JSON plus a `request_id → message_id` registry — so a click updates the clicked card atomically in the callback ack and a remote resolution repaints within one sweep.

## Context

- **Placement today.** A new request is inlined onto the owning session's live accumulator (or the nearest ancestor's, for sub-task children) and the current card is flushed; with no live accumulator it becomes a standalone card (`src/bridge/request.rs:1197-1252`, `src/bridge/pollers.rs:413-430`).
- **Only the newest card of a chain carries the tail.** Interaction blocks render under `include_tail` (`src/bridge/streaming.rs:472-498`), which a split switches off for the finalized card: `build_card_with_split` finalizes the filled card without the tail and advances `render_from`, and `flush_card` sends the continuation which carries it (`src/bridge/streaming.rs:337-349`, `src/bridge/render.rs:377-436`). A pending request blocks the backend run, so a block rarely lives through a split; the cases that matter are the ones where the card is no longer current at all.
- **Failure paths observed.**
  - Click an inline permission → `drop_surfaced` strips the section and flushes only the accumulator's current card (`src/bridge/request.rs:1508-1527`); the ack carries no card for inline clicks (`src/bridge/request.rs:350-353`). A click on any other card (older chain card, card from a previous turn) changes nothing.
  - Request resolved elsewhere (OpenChamber, CLI) → the sweep's `retain_inline` strips an inline block from the accumulator's sections but no path repaints the hosting card (`src/bridge/request.rs:1264-1273`). Standalone cards survive through `mark_stale_cards` and snapshot claims are rebuilt, but the inline case has no equivalent. While a turn runs, the header timer's ~1.5 s tick incidentally repaints the current card; after the turn ends nothing does — the block stays forever.
  - A new turn replaces the accumulator (`src/bridge/turn.rs:137-165`); any still-pending block on the old card is orphaned — a click replies (or 404s) but can neither repaint the old card nor find its section. `core.cards` keeps no per-message card JSON, so no code path can reconstruct it.
- **The mechanisms that already work.** The question path's partial-answer branch returns a rebuilt card in the callback ack (`src/bridge/request.rs:660-695`; its final answer/submit/reject branches instead clear the block with a PATCH), and Feishu applies an ack card to the clicked card atomically — this is why multi-select 已选 markers appear instantly. The callback payload carries the clicked message id (`open_message_id`, `src/feishu/ws.rs:369-381`), so cola can target the exact card. Session Snapshot claims already track a block to its host message and rebuild it in place (`src/bridge/snapshot_claims.rs:118-160`). Standalone request cards already have `sent_cards` + `mark_stale_cards` (`src/bridge/pollers.rs:452-479`).
- **The operator's constraint.** A standalone request card was proposed and rejected: it fragments the message stream, and whichever card is newest, the other's live updates go out of view ("要么我最新看到的永远是这些独立卡……要么这个消息要拆得很碎").

## Decision

**Interaction blocks stay inline and follow the live card.** Seven rules:

1. **Follow the live card.** A split keeps the tail (and with it the block) on the newest card, as today. When a new turn replaces the accumulator, the sweep re-hosts every still-pending block of that session onto the new card and repaints the old card without it. A block never lives on a card that is not the session's current card for longer than the re-host/repaint takes.

2. **Every card showing a live block is repaintable, through its card handle.** The handle is a `request_id → (message_id, kind, session, directory)` registry plus, for each message that shows a live block, the card JSON as last rendered. Every update — click, remote resolution, re-host, sweep strip — rebuilds that card from the cache and PATCHes it. The cache entry is released when no live block references the message. The handle is what makes the observed failure paths fixable; it is the only way to keep controls honest on a card whose accumulator is gone (a replaced turn, a stopped turn).

3. **A click updates the clicked card in the ack.** The callback response carries the cached card with the block updated (partial answer state) or replaced by an Interaction Receipt (resolved) — atomic on the clicked card, no PATCH race and no dependence on "which card is current". This unifies permissions and final submits with the question path's existing mechanism; the split-probe clone fallback (`src/bridge/request.rs:681-695`) and the PATCH-only disappearance go away.

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
- After a cola restart the cache is empty: pre-restart cards freeze (accepted), and a still-pending request is re-surfaced as a standalone card, interactive there.
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
- **Post-restart guarantees**: after a restart the caches are empty — pre-restart cards freeze (see Consequences) and a still-pending request is re-surfaced as a standalone card.

## Risks / open questions

- The two sources of truth (accumulator sections + registry/cache) need one mutation seam; without it they will drift. This is the main implementation risk.
- The cached card is the same JSON Feishu already accepted, so a surgical edit stays under the card size cap.
- The Interaction Receipt line adds a line to every card that hosted an interaction; if it proves noisy it can be collapsed or shortened later — presentation-level.
- A click on a pre-restart card (the registry is empty after a restart) is best-effort: the request is still replied to, but that card may not repaint — the re-surfaced standalone card is the live surface for it.
- The re-host on a new turn is rare (the busy guard prevents a new turn while a request is pending; it happens when a request outlives a cancelled/aborted turn) and needs a regression test.

## Domain note

The concepts enter the glossary as **Card Chain**, **Interaction Block** (交互块) and **Interaction Receipt** (回执).
