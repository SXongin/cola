# A session has one card, and `/card` is how you read it

> **Supersedes ADR-0028** (the Session Snapshot card). **Amends ADR-0043's
> 2026-09-22 `/card` amendment**: the pull gains a second state.

## Context

ADR-0028 gave every session activation a read-only Session Snapshot card. It was
the first surface to answer "what happened while I was not looking", and it was
built without a renderer: a `SnapshotData` read, a bespoke card builder, and
`SnapshotClaims` — a registry that hosted adopt-time Permission/Question blocks
on the card and rebuilt it when one resolved.

Two things changed the calculus.

**The claim machinery grew a renderer's problems without a renderer.** #945
records five, and four are the second state machine: a rebuild from
`SnapshotQuestionState::new()` wipes a draft typed on a block nobody clicked; the
standalone switch paints from a state read taken outside the card write lock; the
state read and the rebuild do not share one lock; an ack can land after the
request settled. Every one is impossible by construction on a card that has no
live blocks. The fifth (`overflow` inside a root form) exists only because the
card renders an operable question.

**The snapshot is the only separate card path left.** An External Message's card
is a Turn: `start_reply_render` calls `Turn::arm_external_render`, and the
external loop runs `render_and_flush` — the same pass a cola prompt uses
(ADR-0059). Even the snapshot's busy-follow renders through it, as the static
lead the follow sets on its prepared accumulator. So the same `SnapshotData` is
assembled into card JSON twice, once by `snapshot_card.rs` when idle and once as
a static lead when busy, and the two can drift.

The premise to reject is that a session needs a second card type to report its
state. It needs a *read*. The card is the session's card.

## Decision

A session has one card, the **Session Card**, with two driving modes:

- **A renderer owns it** — a Turn, an external follow, a Wake continuation: it
  streams, and `/card` keeps its ADR-0043 meaning of splitting the chain and
  continuing at the bottom.
- **Nothing owns it**: `/card` reads the session once (`gather_snapshot`) and
  renders that read as a one-shot Session Card — a real `CardSession` in a new
  **`CardState::Idle`** (terminal, so the card's own `CardSession::is_running` is
  false and the handle-level `Turn::is_running` never admits it), built directly
  by `build_idle_card` and inserted by `record_idle_card`; no external arm runs
  for it. That insert writes **no Chain Record** and collects no predecessor, so
  a chain an earlier Turn still owns is left alone. No Wake, reap or ownership
  verdict may treat it as one, and an existing record still names the chain's
  card: the idle card is not the session's chain.

The idle card — a Session Card in `CardState::Idle` — is **read-only**. It
carries the status line (等待你的确认 > 等待后台任务 > 运行中 > 需要重试 > 空闲),
a count-only pointer naming how many requests wait on the user, and the 最近对话
tail (four messages; a Compaction's marker sits at its server position and
consumes no slot). It renders no operable
block: a Permission/Question always lives on a live card (inline on the Turn it
blocks) or on its own standalone card (ADR-0074). "Idle" and "interactive" are
mutually exclusive — an unanswered request *is* a session state — so a card that
renders one is not an idle card. This is what dissolves #945: there is no live
block on this card to rebuild, claim, or protect from a repaint. Its header is a
static 「📄 会话」 (grey, no phase timer), because it is a read with no run to
describe; #1003 renders its body under that header.

**Taking a session over no longer posts a card.** A first adoption confirms in
one text line carrying the same status summary, and that line is the Topic
Anchor. Only `/card` produces a Session Card. The switch-list op is not a post: it
patches the clicked list card **in place** to the compact 已切换 state
(`build_switched_state_card`), and ADR-0052's 返回列表 button rides that patch —
a text adoption from any other surface has no list to return to.

A second `/card` with nothing in between stamps the previous idle card
`TakenOver` — 「⏳ 已由新卡片接管 · 已停止更新」 — and only while that card is
still the session's current card. A Turn that ran in between has already
superseded it, so stamping it would only add noise. The stamp also covers an
**actionable terminal** (`Error`/`Unreceived`): the read replaces it in the
map, so its Retry/重新发起 control would otherwise stay visible while
`claim_recovery` could no longer find the card — the stamp strips the control
and says the card is superseded. If the card's view cannot be read, the bare
`TakenOver` header is stamped instead (the dead control outranks body
preservation); if the PATCH itself fails, the old card is left current so its
recovery action still works and the posted read stays untracked. The check, the
send and the current-card update run under the session's card **write lock** —
the same lock every flush takes — so two near-simultaneous commands serialize
and the later one stamps the card the earlier one posted. A terminal card that
still carries a **live, unanswered Permission/Question** is not supersedable at
all: `LostContact`/`StuckPanel` can end a Turn in `Error` with the block still
inline, so replacing it would strip the only answerable controls. `/card` posts
the read, leaves that card current, and stamps the posted read taken over.

When the read says the session is busy, or waiting on Background Tasks, **and
the newest user message is not cola-authored**, `/card` arms the existing external
renderer on the posted card (the busy-follow path) instead of freezing it, so the
run's continuation lands on the card the reader just asked for. Nothing new
renders it. A **cola-authored** newest message keeps the card static even while
the session reads busy: that run is cola's own, and its hand-off belongs to the
Chain Record and Session Sync (ADR-0063, ADR-0071), never to a Session Card that
would take the chain over. With cola's own renderer alive `/card` never reaches
this branch — it pulls — so the guard only covers a run cola lost its renderer
for. One recorded #1005 carve-out: an **untimed newest user message** has no
render anchor, so the follow cannot scope the run; `/card` posts the static read
rather than guess a window (the read is still always posted — never a dead end).
The gather reports the actual newest user message, timed or not, so an older
timed message is never substituted for the untimed one.

The post and the arm are **one commit**, ordered so a cancellation can never
publish a half-built card. Before taking the lock, the command reads the Session
and builds the follow's accumulator **off the card map**. Then, under the
session's card write lock, it filters the read's pendings with
`is_already_surfaced`, composes the static lead from that exact set (so the
count-only pointer names what will be attached), sends the card, inserts the
render-owned `CardSession` carrying the id the send returned, **spawns the
render loop** (a sync spawn, immediately after the insert and before any further
await), seeds the filtered requests and writes the chain record. Nothing
reaches `cards.cards` before that insert, so a command cancelled earlier leaves
no Idle card behind: the next sweep
surfaces the requests normally, and the sent card — if the send had already
returned — stays a static read, not a host. `RequestFlow::surface` takes the
same host lock (or, when no live host remains, the request Session's own) for
its decision: it resolves the standalone target and inline host before the lock,
then re-checks the host's liveness and sends under that Session's lock, so it
cannot observe the window either. Because the filter runs inside that one critical section, a
request the sweep had surfaced before `/card` began keeps its own card and is
simply not seeded, and a request that lands during the command cannot be counted
on a card that does not carry it. The loop waits on the same session write lock
until the command releases it, so it cannot render before the seed and chain
record are written; a cancellation at either of those later awaits drops the
guard but leaves the already-spawned loop alive, which settles the card normally.
An abort can therefore never leave a live `CardSession` with no render loop —
the insert and the spawn are indivisible to cancellation. The **send itself is
not wrapped in a request timeout**: Feishu's create carries no idempotency key,
so abandoning a create whose response is slow can orphan a card that did land,
and a retry would post a second one. The bounded reads and the bounded stamp
PATCH are the lock's long awaits; the create is one platform call, and a real
send error takes the existing failure path. Because the create holds the
Session's card write lock, a half-open Feishu response blocks that Session's
flush, sweep, install and next `/card` until it returns — accepted, because a
timeout could orphan a create that did land and the retry would duplicate it.

One composition function turns the read into the body — the status line, the
pending pointer and the tail. Both drivers call it: the idle card renders it, and
the busy follow hands it to its prepared accumulator's static lead (set under the
card write lock, where the seed count is final). A test renders the same
`SnapshotData` through both and asserts the body text is equal, because today the
two are separate compositions that can drift. Two deliberate driver facts are
outside that equality: the **pending pointer** — an idle read names
「见原卡片」 because it carries no blocks, while a follow that seeds the counted
blocks is hint-free — and an **oversized read tail** — the one-shot card must
evict its oldest entries to fit one card and names the omission, while the live
lead renders all four (its card can split). The equality contract is asserted on
a fitting tail with `on_card == 0`; the pointer difference is the documented
driver fact for carried blocks.

## Consequences

- `src/feishu/snapshot_card.rs`, `src/bridge/snapshot.rs`'s build half and
  `src/bridge/snapshot_claims.rs` go, along with `SnapshotData`'s embedding
  fields (`pending_overflow`, `pending_elsewhere`), `claimable_pendings`,
  `SnapshotQuestionState`, `ClaimedSnapshot`, the claim registry's
  `claim`/`rebuild`/`resolve`/`drop_vanished` surface, tombstones, and
  `is_already_surfaced`'s snapshot arm. `pending` survives as the read's count
  for the pointer and as the follow's seed — it is no longer embedded in the card
  by the builder or claimed. The read survives as
  **Session Snapshot**, and `display_title` plus the body composition
  (`session_card_body`: status line, pending pointer, tail) move to
  `feishu::card::session`. The busy follow's `static_text` becomes a call to that
  same function, so the idle card and the follow cannot render different bodies
  for the same read.
- Every reader of the in-memory card map that means *the session's live card*
  asks `Turn::is_running` instead of "the map has an entry":
  `inline_host_session` and `resolve_card_target`'s in-flight arm
  (`src/bridge/pollers.rs`), and the click's inline-host decision
  (`RequestFlow::handle_card_action`, `src/bridge/request/flow.rs`).
  `ExternalFlow::wake_reply_target` asks `Turn::is_running` too: a Wake
  continues the Session's **Card Chain** on a **Fresh** card, and only a live
  card supplies its reply target. A card past its ending (Done/Stopped) or an
  Idle read therefore falls through to the topic anchor or top level, with the
  late content still carried on the Fresh continuation. `Turn::is_chain_card`
  stays for the flush/split/residual guards. `is_running` is
  `!is_terminal()`, so `Waiting` still counts (it is the chain's newest card) and
  `Restarted` still counts (deliberately not terminal). Without the live
  predicate, an idle card in the map would take a stale permission click down the
  inline path — the ack returns no replacement card and the idle card never
  repaints the block, the empty-ack class of pitfall `codex-request-ids`.
- An adopted session's pending requests surface through the ordinary sweep
  (`RequestFlow::sweep` lists every known directory's requests): inline on a live
  card, else their own. A **busy follow** seeds the read's pendings onto its own
  card instead — from the same read, filtered and attached inside the
  post-and-arm critical section above — so the acceptance is that they are on the
  follow card, not that a later sweep lands them. No registry claims anything;
  only the `pending_overflow` notice goes, because overflow is no longer a
  decision the card makes.
- ADR-0028's suppression heuristic dies with the card: `/card` always answers.
- `build_switched_state_card` moves to `feishu::card::session`; it carries
  ADR-0052's 返回列表 button, which must survive on the list card the switch-list
  op patches in place.
- The **External Message**'s card is out of scope. `build_external_message_card`
  and its `Turn` arm are already the shared path; this decision neither changes
  them nor folds them into the idle state.

## Considered options

- **Keep the snapshot card and give it the turn's repaint gate** (#945's
  proposed direction). It fixes today's five symptoms one at a time and leaves
  two state machines, one of which exists only because a card took on live state
  without a renderer.
- **A thin builder without a `CardSession`.** Cheaper, but keeps a second card
  builder and a separate record of the last idle card, and buys none of the
  shared flush, signature cache or typed-input freeze.
- **A persistent per-session card, patched forever.** Feishu cannot move a
  message; a card buried above the user's own replies stays buried, which is the
  problem `/card` exists to solve.

## Risks

- `/card` becomes the only way to learn an inactive session's state. An adoption
  used to answer it unprompted; now the Host must remember the command. The
  one-line confirmation carries the status summary to soften that.
- The idle card does not refresh itself. If the session moves on elsewhere, the
  card is stale until the next `/card` — which then follows a live run when there
  is one.
- The pending pointer is text, not a link: cola has no message-deep-link
  capability, so it can only say 「见原卡片」. After `/switch forget` and a
  re-adoption in another chat, that card may live in another chat — the same risk
  ADR-0028's `pending_elsewhere` already carried.
- An idle card joins no Chain Record, so a restart does not reap it: it stays in
  the chat as a static message. That is the intended meaning of a read.
- A **Wake-less** residual tail after `/card` has no anchor to scope it: it
  falls through to the Fresh gate with no Wake and decides nothing, so it is not
  rendered immediately. **While a Chain Record survives** — an owed ending, or a
  run that ended while cola was down — its Rendered Cursor seeds the next Turn's
  `seed_orphan_delta`, which renders the undelivered delta on that Turn's card.
  `release_spent` releases the record once a terminal ending's write is
  confirmed, so after a normal Done turn there is no record to seed the next
  Turn: the common case defers the tail to a later `/card` read. The read is
  never repainted and no per-tick warning is emitted while the tail waits.

## Reconciled references

Each ADR this decision touches carries its own banner or amendment; this is the
index, so a reader finds the change from either end. One term per status:
**superseded** retires an ADR, **retired** retires a rule, **kept** leaves a rule
standing.

- ADR-0038 rule 6 — the Session Snapshot's claimed blocks. **Retired**: the
  claim registry goes, so every Interaction Block lives on a live card or its own
  standalone card.
- ADR-0038 rule 8 — the card writer set. **Amended** (2026-11-07): `/card`'s
  follow makes `RequestFlow::surface` hold a card write lock across its whole
  surfacing decision too, including the standalone send, so the follow arm and
  the sweep cannot double-surface one request. The sweep takes exactly one
  Session's lock: the resolved HOST's for an inline block, or the request
  Session's own for the standalone fallback — never both, and never nested. The
  lock stays per-session, and the sweep's non-live-card patches stay lock-free.
- ADR-0094 — several pending blocks on one snapshot, and its no-repaint
  carve-out. **Retired** with the claimed block.
- ADR-0040 — the Session Snapshot tail's fold ids. **Retired** with the rebuild
  path.
- ADR-0093 — the Session Snapshot's folded receipt log. **Retired** with the
  snapshot card; the Session Card's folded tail and list forms keep that
  layout rule.
- ADR-0082 §3 — session-scoped exactly-once. **Narrowed** (2026-11-07): the
  live timeline still renders a compaction once, but the busy follow's static
  lead keeps the read tail's markers for #1002's equal-body contract, so a
  followed card may show the same marker twice — in the lead and in the
  timeline.
- ADR-0044 — "session snapshot cards never carry the Turn Footer." **Kept**: the
  Session Card's idle state carries no footer model either.
- ADR-0043 — the claimed-snapshot pin arm. **Retired**; a request's Message Pin
  follows the live card or the standalone card.

Source: #945, #999.
