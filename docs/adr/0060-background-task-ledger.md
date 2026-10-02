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

## Amendment (2026-10-02): the type words are the tools' own names

The ledger's type words are the tool's own name everywhere it names one (#502,
spec #501): the ledger and the Tool Panel call one task the same thing.

- The live row reads `· shell：**npm run build** · 14:02 · 3m12s` /
  `· subagent：**review the diff** · 14:04 · 1m05s` *(the subagent row's
  total elapsed was later dropped — see the amendment below)* — the subagent no
  longer reads 「子代理」; the omissions, bold escaping and clip are unchanged.
- The completion entry's collapsed title reads `🔔 shell 完成：<label>` /
  `🔔 subagent 完成：<label>`, with the same ending phrases as before (`已取消` /
  `失败` / `结束` / `已失联`, only on their own evidence) and the same bare form
  when the Wake names no label; the fold body's identity line reads
  `shell sh_abc · 14:02 · 12m` / `subagent ses_child · 14:02 · 1m`.
- Unchanged: the live list's title `⏳ 后台任务（N）`, the unconfirmed marker
  `⚠️ 状态待确认`, the resumption header `🔄 后台任务完成，继续处理中…`, and the
  permission card's 「调用子代理」.

## Amendment (2026-10-02, the yielded row's child activity): live subagent liveness on the ledger row

A background `subagent`'s live row carries the same child-session liveness the
live task panel shows (spec #501, ticket #503): appended after the row's start
clock — `· subagent：**review the diff** · 14:04 · bash 5s`, or
`· 思考中 30s · 等待你的授权` for a phase and its wait — through the front's own
fragment vocabulary, so one child state reads the same wherever cola renders
it. The fragment sits after the start clock and before `· ⚠️ 状态待确认`; a row
the runtime reconciliation could not confirm renders no activity, and a `shell`
row is unchanged (no fragment). A subagent row renders no total elapsed at all
(see the amendment below): its clock is its start and its liveness is the
fragment.

- **One shared gather.** The front task panel's per-child read loop becomes a
  batch — a set of `(call_id, child)` pairs, one transcript light read per
  distinct child plus its one pending-wait query, keyed back by call id — and
  the yielded ledger refresh reuses it. A pass reads each distinct child at
  most once; a session with no live background subagent (shell-only, V1) names
  no child and spends no new request, and a pass whose card cannot receive the
  refresh gathers nothing.
- **This cut wires Session Sync.** The live render path gathers no liveness for
  the ledger *(superseded by the ticket #504 amendment below: the live render
  gathers too)*: the fragment first appears on the yielding card's ~8 s Session
  Sync reads (a waiting card's only clock). A path that does not gather keeps
  whatever fragment is already stored — its age keeps counting — so the
  handover, the in-place resume and a live re-render never drop it.
- **Failure keeps the last successful fragment.** The gather carries only
  successes; a call whose child read fails (or whose transcript carries no
  timestamp) keeps the fragment of the last read that succeeded, whose stored
  times are never refreshed, so the rendered age keeps growing truthfully and a
  failed read never ends the wait (ADR-0054's rule).
- **The ledger clock covers the fragment's age, and the flush reads the
  rendered row.** A shell row's clock now holds its elapsed, and every row's
  holds its activity age; the live path still compares whole minutes and the
  yielded refresh whole seconds, so a yielded card PATCHes when its visible
  second turns and never once per read, while the live card gains no per-second
  churn. The comparison is on
  what the row RENDERS — its type, its label folded and clipped, its local
  `HH:MM` start clock, the fragment's label and wait — never the raw values
  that render identically (text past the clip, an empty label, a start moving
  inside the shown minute and second) nor the gathered liveness's stored
  timestamps: a child part landing inside the second the card already shows
  owes nothing. The read is still stored whatever the decision, so the next
  age counts from the freshest timestamps rather than from a stale accepted
  read. An unconfirmed row's stored age never ticks (it renders none), and an
  untimed tool has no age to tick.

GLOSSARY's **Background Task Ledger** entry follows. The child retirement read
of #464 will reuse the same gather.

## Amendment (2026-10-02, the live render's own gather): the live path reads the same liveness

Ticket #504 supersedes the "This cut wires Session Sync" bullet above: the live
render path gathers the ledger's child liveness too, so a background `subagent`
row is as fresh as the card while the parent turn still runs. Each render poll
runs ONE shared batch over the union of the children both surfaces name — the
card's live task panels and the read's live background subagents — and hands it
to both: one transcript light read and one pending-wait query per distinct
child, so a child named by a panel and a ledger row is read once per render.
The fragment's age is measured at the card's build clock, so it stays true, and
a later read of the same row renders the child's new typed activity (a phase
gives way to a running tool, a wait joins, a failed gather heals).

- **The whole-minute gate stands.** The live path's clock comparison is
  unchanged: the activity age's seconds never owe a PATCH by themselves, only
  its whole minute turning (the same gate a shell row's elapsed has), while a
  typed liveness change — a tool switch, a wait entering or leaving, the gather
  establishing or losing the child — owes one immediately, inside the same
  minute.
- **The yield still has no gather of its own.** The live render has usually
  established the fragment before the card yields, and the ~8 s Session Sync
  reads keep it fresh while the card waits; the yield's own rendering keeps
  whatever fragment is stored. A handover, an in-place resume or any path that
  gathers nothing still never drops it.
- **The zero-read rule is unchanged.** A read that names no live background
  subagent — shell-only, or V1 with no Background Task facts — spends no child
  request; only a live subagent row's child is read.

Related: #501, #503, #504, ADR-0054, ADR-0066.

## Amendment (2026-10-02, the subagent row's clock): no total elapsed

A `subagent` row never renders the task's total runtime. Its start clock and its
child's activity fragment already say what the row needs to: the fragment
carries the live age (`shell 26s`), and the total beside it (`0m34s`) is
redundant and long. The rule is kind-scoped: a `shell` row keeps its elapsed —
the elapsed is its only liveness, since it has no fragment — while a subagent
row's clock is its start `HH:MM` and its liveness is the fragment.

- The row copy: `· subagent：**review the diff** · 14:04 · shell 26s` (was
  `· … · 14:04 · 1m05s · shell 26s`); a fragment-less subagent row stops at its
  clock, `· subagent：**review the diff** · 14:04`; a shell row is unchanged
  (`· shell：**npm run build** · 14:02 · 3m12s`). Completion entries and their
  identity lines (`subagent ses_child · 22:46 · 1m`) keep their timing — a
  completion is history, not liveness.
- The render clock follows the render: a subagent row's `elapsed` clock is
  always `None`, so its total runtime can never owe a flush; only its fragment's
  age ticks (whole seconds on the yielded path, whole minutes on the live one),
  and the shell row's elapsed keeps the two-tier gate exactly as before.
- The size reserve keeps its shell-sized per-row constant; a subagent row's
  missing elapsed is over-reserve, which is safe.

This supersedes the subagent total elapsed in the earlier amendments: the #501
amendment's example is updated above, and the #502 amendment's example is marked
superseded.

Related: #501, #503, #504, ADR-0054, ADR-0060.
