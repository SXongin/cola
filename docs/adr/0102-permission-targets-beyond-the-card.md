# Permission targets beyond the card

## Context

A permission request whose target cannot be shown in full on a card used to be
declined **before any surface**: `reject` settled it
server-side, so no client could approve it afterwards. #1009 asked for a way to
approve the text anyway; #1010 is the parent spec, and #1012 is the interactive
half. #892 / ADR-0101 already removed the refusal for Auto-Accept sessions.

The budget is the card's wire cost, not a readability preference: every button
used to echo the body. #1011 moved the body out of the callback and made the
completeness proof a single predicate, `PermissionOverflow` — the summed
`PERMISSION_TARGET_MAX_CHARS` display budget or the rendered-body
`PERMISSION_BODY_MAX_CHARS` render invariant. This decision supersedes
ADR-0101's remaining clause that an interactive surface still declines an
over-budget target; its Auto-Accept decision is unchanged.

**Why 3,000.** The platform documents no per-element text cap; cola measured
that one markdown element of **8,000 ASCII / 3,060 CJK characters** renders
fully (`docs/research/feishu-rich-surfaces/r1-display-surface-inventory.md`).
The summed target budget takes the CJK figure as the widest script and rounds it
down to **3,000** — one display number, shared by every target in the one
element. `PERMISSION_TARGET_MAX_CHARS = 3000` is therefore the only **display**
budget, a compile-time constant a developer tunes, not a user knob. The fixed
markdown scaffolding around a bare target (header, labels and footer) is
charged inside the same measured render point — 3,000 target characters plus
that chrome are not promised to fit. `PERMISSION_BODY_MAX_CHARS = 3060` is the
**render invariant**, not a second display budget: it bounds the whole rendered
body, targets plus scaffolding, and any body that passes it — reason and
metadata chrome included — is `PermissionOverflow::Body` and takes the companion
lane, never the card, unless it renders a diff: a diff carries its own
escaped-byte cap and is shown truncated on the card instead.

The proof counts **sanitized** characters, and a sanitized character is at most
six JSON bytes (`\u0000`); a 3,060-character body is therefore ≈18 KB, inside
`MAX_CARD_JSON_CHARS` (25 KB). The JSON bound is a consequence of the render
invariant, not a second byte budget.

The parent spec directs this ticket to reuse **ADR-0083 §5**'s existing
Rich Text Message → File Message ladder as the one overflow mechanism. That
lane's own charter says "by what the content *is*, not how big it is"
(`GLOSSARY.md` calls the Rich Text Message "deliberately not the overflow lane
for size alone"). Using it here is a deliberate bend, not a new lane: the
mechanism, its 30 KB body cap and its File Message fallback are unchanged; only
the TRIGGER differs (a permission target past the card budget, which #1010
explicitly assigns to this ladder). The GLOSSARY and user-guide wording were
updated with #1014.

Surface persistence is ADR-0038's: a restart re-adopts the card a previous
process surfaced, and the persisted state is client-scoped (ADR-0091).

## Decision

The completeness proof's authority is the **surface**: an over-budget approval
is allowed only when the full content was delivered for that request.

- **No refusal on the way in.** A fresh over-budget permission is not declined;
  the sweep surfaces it with a summary and delivers its full content first. The
  flow's `decline_permission` path remains for a request that resolves no card
  target at all; a failed or uncertain companion takes the fallback card below.
  A recovered card without a usable companion is withdrawn and takes the fresh
  path, not a refusal.
- **One proof, two renderings.** The summary card and the inline block show the
  action, the summed target length, the first 120 characters of the target, and
  where the full content lives. The Interaction Receipt names the action, the
  summed length and the companion lane with a short (60-character) target line,
  clipped to the receipt budget. The full description stays for in-budget
  requests.
- **ADR-0083 §5's ladder.** The full content goes out as one Rich Text Message
  (`post` with an `md` tag); a body past the 30 KB `post` cap, or a send that
  definitely did not land, falls to one File Message (an uploaded `.txt` whose
  name and following caption name the request; the caption is sent after the
  file lands). An ambiguous send is terminal:
  it is never retried into a duplicate, but it is **not** a delivery. It leaves
  a non-proof marker and the request takes the fallback card; the copy names
  the unknown outcome instead of claiming either delivery or a definite failure.
- **Delivered before interactive.** The companion is sent from the sweep's
  surfacing path before the inline block or standalone card is created, so there
  is no window in which a button approves text the user never saw. It is sent
  once per request, whichever surface hosts it. Auto-Accept returns before this
  step: it renders no card and sends no companion (ADR-0101 unchanged).
- **Delivery is persisted; uncertainty is only a marker.** A client-scoped
  `request_id → CompanionDelivery { message_id, lane, target digest }` record
  rides beside the existing surface records (ADR-0038). A **confirmed** send
  writes the record; an ambiguous send writes the same shape with an empty
  message id, which only prevents a second send. The record is removed when the
  request resolves and hydrated at startup. Readoption trusts a confirmed record
  whose digest matches and never re-sends; a marker never proves anything.
- **The click guard checks it.** An `once`/`always` click re-reads the pending
  request and accepts an over-budget approval only when a **confirmed** companion
  record exists whose digest matches that request. A stale record is replaced at
  surfacing time, never trusted; an uncertain marker is refused like a missing
  record, except through the fallback card's explicit risk confirmation below.
  This covers **any** `PermissionOverflow` — the summed target budget and the
  rendered-body ceiling alike.
- **A recovered card without proof is withdrawn, never decided in place.**
  Readoption does not trust a persisted over-budget card that has no matching
  confirmed companion record (including one whose only record is an uncertain
  marker): the card is withdrawn neutrally and the recovered entry is forgotten.
  The request then continues through the **normal fresh path in the same sweep**
  — `prepare` may approve it unseen on an Auto-Accept session (with its usual
  INFO line), the companion lane may deliver and surface the summary, or a
  failed companion takes the fallback card. The old decision is never silently
  taken over (ADR-0101), and a card from an older build cannot approve text a
  newer build never delivered.
- **Fallback names the risk and confirms twice.** If neither lane delivers — a
  definite failure, or an uncertain marker that is not a proof — the surfacing
  path renders a **standalone** fallback card instead of replying `reject`: the
  target summary, a content line that follows the cause — "完整内容：未能送达。"
  for a definite failure, "完整内容：发送结果未知，可能已送达。" for an uncertain
  one — the risk paragraph ("完整目标未能送达；批准即批准未能在卡片上完整显示的
  完整命令。建议先在原始客户端查看详情", with a neutral "完整目标未能确认送达…"
  variant for the unknown case), and the four controls 允许一次 /
  始终允许 / 拒绝 / 开启自动授权. A block already inserted for the request is
  withdrawn first, and the card is standalone so its click ack replaces it
  without destroying a streaming card. The allow buttons are **first-step
  only**: their click acks a confirmation card (the same risk and summary,
  确认批准 / 取消), and neither replies nor claims the request. 确认批准 carries
  `risk_confirmed`, the original decision and a digest over the request's
  **patterns** (a lowercase hex string, so a 64-bit value survives the callback's
  JSON round-trip); the click guard allows an over-budget approval without a
  companion only when that digest matches the request it re-reads. A summary
  card's buttons carry the same patterns digest plus a `summary` marker, and the
  guard compares it **regardless of the request's current overflow** — a target
  that shrank out of the budget after the card was shown cannot borrow the old
  surface's approval. A request that fits the card when it is first surfaced
  drops any stale companion record, so an ordinary allow is not mistaken for a
  summary surface. Deny and Auto-Accept never run this proof: they do not
  approve unseen text, so a changed target cannot bounce them. A mismatch
  sends **no reply at all** and acks a rebuilt fallback card — the changed
  target's summary, a fresh digest, all four controls, and a note that the
  target changed and must be confirmed again — so the pending request never
  loses its actionable surface. When the click landed on an inline summary
  block, the replacement standalone card is posted **before** the old block is
  withdrawn: the "withdraw first" rule protects a fresh fallback from
  duplicating a block, but a recovery must not leave the request surface-less
  if the post fails. A failed withdrawal leaves the new card pending and the old
  block logs/retries.
  Deny and Auto-Accept stay one click; Auto-Accept still renders no companion.
  This two-step confirmation is the **one deliberate exception** to the delivery
  proof: it is the only way an over-budget target is approved from an
  interactive surface without a confirmed companion, and it requires the user
  to read the risk statement and confirm again against the request the click
  re-reads.
  A risk-confirmed approval logs at WARN with the request id, the target length
  and the fact that the user confirmed unseen text. The risk digest differs
  from the companion digest on a rendered-body overflow on purpose: the
  companion proof covers exactly the rendered content it carried, while the
  user is confirming the **target** (the commands), not the render chrome.

## Consequences

- A huge command is readable in the chat, stays inside the card's JSON budget,
  and can be approved where it was seen. The transcript keeps the full content.
- Auto-Accept keeps ADR-0101's decision unchanged: it answers every pending
  permission unseen, renders no card, sends no companion, and logs the request
  id, the crossed bound, the target length and a bounded first line. The
  completeness proof never applies there because no approval surface exists to
  guard.
- The proof depends on persisted state, which is why the record is client-scoped
  and versioned with the other surfaces (ADR-0091). A lost or corrupt record
  fails closed: the click is refused, never approved unseen.
- The digest is over the exact text the companion carries — for a target
  overflow, the raw targets; for a rendered-body overflow, the full rendered
  description including its non-diff chrome. A request whose text changed while
  its record stayed behind cannot borrow the old delivery.
- An ambiguous send is recorded only as a no-duplicate marker (empty message
  id) and never retried. It is not a completeness proof, so the request takes
  the fallback card, whose content line names the unknown outcome rather than
  claiming either delivery or a definite failure.
- A standalone fallback card has no inline Interaction Block, so its **result
  card is the decision record**: the allow result carries the risk form the
  inline receipt would have carried (`✅ 已允许一次（目标未完整展示）：…`), and a
  block that was withdrawn for the fallback leaves the non-decision move
  receipt (`🔄 已改为单独的风险确认卡片`) on the streaming card. There is no
  later inline decision receipt to update, because the fallback card replaced
  the only interactive surface.
- A summary surface's recovery copy says the companion delivered the **previous**
  target ("之前的目标已完整送达；当前目标已变化，未能完整展示") rather than
  claiming an unknown send that was in fact confirmed.
- A summary card written before the `summary` marker existed, whose companion
  record is **also** gone, can still approve an in-budget target on one click.
  The window requires both a legacy card and a lost record; every card this
  build writes carries the marker and digest, and a surviving record still
  forces the proof.
- The refusal copy, the reason card and the retry behaviour stay in use only for
  a request that resolves no card target at all; a failed or uncertain companion
  takes the fallback card instead.
- A target with only a chat reply target (no message to reply under) cannot take
  either companion lane, so it takes the standalone fallback card sent into the
  chat. Only a request that resolves no card target at all follows the refusal
  path.
