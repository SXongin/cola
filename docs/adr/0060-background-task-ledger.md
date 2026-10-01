# The background-task ledger: one live list on the newest card, entries where they lived

> **Amended by ADR-0066**: the freeze's carve-out widens — a yielded card also
> receives its Wake's resumed content (ADR-0066 resumes shell/subagent
> completion Wakes in place), so the live list and the completion entries stay
> on the card the tasks lived on, and the handover to a newer card now only
> runs for the splits that remain (size, supplement, terminal-card
> continuation, restart Fresh).

## Context

ADR-0059 made a Turn's Background Tasks gate its card (`⏳ 等待后台任务`) and
gave their completions a Wake continuation, but never showed the tasks as
tasks. The tool part that launches one settles at launch — the call returns the
handle, so the panel reads ✅ — and while any task is live the header is the
only trace: no list, no per-task state, no readable set of "these are the
things still running". Completion renders ad hoc: a Wake continuation card, or
one merged receipt line (`🔔 后台任务完成：<命令>`) when the Wake's work landed
in an already-live card. Issue #412 (raised during #405's acceptance) asked for
a pinned live list, fixed completed entries, and a decision between Policy W
(ADR-0059: not complete while tasks are live) and Policy C (`✅` = the turn's
answer is done).

The design round (2026-09-29) established the mechanics this builds on: Session
Sync keeps polling a yielded session every 8 s (the inflight guard is released
at the yield) but PATCHes only when its content diff owes content; the request
sweeps (3 s) can already PATCH a waiting card in place; Wake continuations are
content-gated (`wake_continuation` returns `None` when the resumed run renders
nothing, so a quiet completion posts nothing today); the receipt is a plain
timeline line with no folding; and the read model's `BackgroundTask` carries
identity and timing but no label — the originating tool part's `input` (same
`call_id`) does.

## Decision

**Policy W stands** (ADR-0059's terminal semantics are unchanged): ✅ and the
Completion Notice still fire only at a Turn's true end — idle with no live
Background Task. Policy C was rejected: every completion already opens a Wake
continuation, so W's chain reads ⏳ → … → ✅ for one request, while C would
stamp ✅ on each interim card (✅ → ✅ → ✅) and force CONTEXT.md to re-narrow
what ✅ promises. The ledger supplies the visibility C was meant to buy, with
one meaning for ✅.

**One ledger, following the newest card.** Every live Background Task of the
Session — across Turns — renders in a card-tail section like the Todo Panel and
live Tool Panels (ADR-0045): a folded-by-default collapsible panel titled
`⏳ 后台任务（N）` — the title is the count, so the folded panel still answers
how many tasks are running, and a stable `element_id` keeps the reader's fold
state across re-renders — whose body is one row per task, the pinned copy
`· shell：**npm run build** · 14:02 · 3m12s` / `· 子代理：**review the diff** ·
14:04 · 1m05s`: the type word plain, the label bolded and clipped, then the
task's server `started_at` as local `HH:MM` and the bare elapsed. The label is
joined from the tool part's input by `call_id` (shell: `command`/`description`;
`subagent` needs its own rendering arm), clipped like the receipt; elapsed is
rendered second-granular and driven by the existing reads (the 8 s Session Sync
pass) — no new polling cadence. Exactly one card carries the live list: when a
Wake continues the chain or a new Turn takes over, the handover removes it from
the old card.

**The yielded ledger ticks at the read cadence.** A Waiting card has no render
loop of its own, so the existing 8 s Session Sync reads are its only clock: the
in-place refresh owes a PATCH whenever the ledger it would render differs from
the one the card last rendered — membership, or any row's rendered elapsed at
**second** granularity (≈0.125 QPS per waiting card at the 8 s cadence, far
below Feishu's per-message cap). Repeated reads inside one rendered second owe
nothing. The live render keeps whole-minute gating: its flushes are
content-driven, and a per-render second clock would be clock churn.

**A completion leaves a fixed entry where the task lived.** The task leaves the
live list and becomes a collapsible entry on the card that hosted it — the
handover PATCH carries both — with the mechanical receipt line as its collapsed
title (`🔔 后台任务完成：<命令>` / `🔔 子代理完成：<描述>`, bare when the Wake
names no label) and identity and timing inside (`shell sh_abc · 14:02 · 12m`).
The receipt line and the entry are one mechanism, not two renderings; the
completion still never depends on the model narrating it. A continuation card
carries only its 承接 line and the remaining live list — the entry does not
migrate.

**The ledger is the freeze's one carve-out.** ADR-0059's "the card stops
updating" (Waiting on Background Work) is narrowed to allow the host card to be
re-rendered in place for the ledger: membership and elapsed stay fresh while
the card is otherwise yielded. The carve-out covers the handover removal above
and the case below, nothing else.

**A quiet true end settles the waiting card in place.** When the last
Background Task retires and the Wake's resumed run renders nothing, nothing
posts today — the waiting card would be left saying ⏳ forever. With the ledger
updating it anyway, that read is also the true end: the host waiting card
settles ✅ in place and the Completion Notice fires per its existing rules
(groups per `[bridge] group_completion_notice`, p2p per the long-task
threshold). This supersedes drain.rs's
`a_retirement_after_the_yield_leaves_the_waiting_card_alone` expectation.

**The launch panel stops reading as done.** The settled `shell`/`subagent` call
whose metadata says the run is still going keeps its place in the timeline as
the record of the request, but renders 🌙 in its status slot instead of ✅ — the
rest of the title is a normal panel's (`🌙 shell · <label>`); the ledger, not
the panel, owns the run's liveness.

V1 carries none of these facts: there the section is empty and behavior is
today's (the ADR-0059 degradation).

## Considered options

- **Policy C (`✅` = the turn's answer is done).** Rejected — with every
  completion already continuing the chain, C makes one user request produce a
  row of ✅s and requires re-narrowing ✅ in the glossary; W plus the ledger
  keeps ✅ single-meaning and still shows the running set.
- **No ledger; keep the opaque header.** Rejected — the header says something
  is running without saying what or for how long, which is #412's whole point.
- **The completed entry follows the newest card** (the spec's first wording).
  Rejected — an entry that migrates accumulates history onto every continuation
  card, contradicting the 承接 rule that a continuation carries only new work
  (ADR-0043); the entry is the audit residue of the card the task lived on.
- **A separate receipt line plus a ledger entry.** Rejected — one fact, one
  rendering.
- **Only refresh the ledger at card handovers.** Rejected — the ledger would
  read as a snapshot the moment the card yields, which is exactly when a user
  checks it.
- **Keep the waiting card frozen until superseded (today's quiet-end
  behavior).** Rejected — the card would show a completed entry under a header
  claiming ⏳, and the Turn's ✅ would never render for a quiet completion.

## Consequences

- CONTEXT.md gains **Background Task Ledger**; **Wake**, **Card**, **Tool
  Panel** and **Waiting on Background Work** record the entry copy, the launch
  panel's 🌙 marker, and the carve-out.
- New rendering work: a third card-tail section; a fold element for the live
  list and the entries; a `subagent` arm in the input formatter; the `call_id`
  label join.
- The ledger must fit the card budget like the existing tail sections — many
  live tasks are still one section, rows clipped like the receipt.
- Tests pin: the exact copy, one and several live tasks, out-of-order
  completions, a completion while the card is live vs waiting, the list
  surviving a split/continuation, the handover removal, the second-granular
  yielded refresh (and the live path's whole-minute gating), and the quiet true
  end (✅ + notice, no new card).
- A cola restart still loses live cards; the next Wake pass rebuilds the
  ledger on a new card exactly as it rebuilds any continuation.

Related: #412, #405, #403, ADR-0059, ADR-0045, ADR-0043, ADR-0040, ADR-0054.

## Amendment (2026-09-29): the launch marker, the folded live list, and the yielded second

The #412 batch's acceptance review (#423) changed three presentation decisions
recorded above; the mechanics — one ledger, following the newest card, entries
where the task lived, Policy W, the freeze's one carve-out, the quiet true end —
are unchanged.

- The launch marker is the 🌙 icon in the panel's status slot, not the text
  「已转后台」 (#416 superseded): the title reads `🌙 shell · <label>`, the
  semantics are the same — it never claims completion, it persists after the
  run retires, and the ledger owns the run's liveness.
- The live list is a folded-by-default `collapsible_panel` titled
  `⏳ 后台任务（N）` — the title is the count, the rows are its markdown body,
  and a stable `element_id` (`task_ledger`) keeps the reader's fold state
  across re-renders (#415's flat markdown section superseded). An empty ledger
  still renders nothing.
- The yielded refresh compares the ledger it would render against the one the
  card last rendered at **second** granularity, superseding #419's
  whole-minute rule on that path: a rendered second moving owes the PATCH,
  repeated reads inside it owe nothing. The live render and its Wake handover
  keep whole-minute gating.

## Amendment (2026-09-29, the live row's copy): the start clock and the bold label

The live row gains the task's own start clock and a bolded label (the same
review round as the amendment above):

- `· shell：**npm run build** · 14:02 · 3m12s`, `· shell · 14:02 · 0m05s`,
  `· shell：**npm run build**`, `· shell`. The type word stays plain, the label
  is bolded, the start clock is the task's server `started_at` in local
  `HH:MM` (the completion entry body's clock), and the elapsed format is
  unchanged (`3m12s` / `1h05m`). No label or no start omits its part whole.
- A label's own `&`, `*` and `_` become their numeric entities before the
  `**…**` wrap (`&` first, so a label carrying entity text of its own stays
  literal), so a command cannot close the span or bleed formatting into the
  next row (Feishu decodes the entities back to the literal characters — the
  same mechanism as the `<` escape). The clip is unchanged: `TASK_LABEL_CHARS`
  characters plus the `…` marker, inside the bold.
- Identity and icons stay out of the live row: Feishu cards have no hover
  tooltips, and future task types have no recognizable glyphs. The completion
  entry's fold body keeps the identity.
