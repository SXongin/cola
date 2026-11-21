# The background-task ledger: one live list on the newest card, entries where they lived

> **Amended by ADR-0106**: the ledger lists a Session's **live runs** — a
> Background Task or a Turn-scoped child agent — gates on `reports_live_runs()`,
> and each row's type word is the run's tool token (`shell`/`subagent`), not the
> vendor id.
>
> **Amended by ADR-0107**: the liveness vocabulary is `waiting`/`reasoning`/
> `responding`, and the tool token is `shell` (not `bash`).
>
> **Amended by ADR-0066**: the freeze's carve-out widens — a yielded card also
> receives its Wake's resumed content (ADR-0066 resumes shell/subagent completion
> Wakes in place), so the live list and the completion entries stay on the card
> the tasks lived on, and the handover to a newer card runs only for the splits
> that remain (size, supplement, terminal-card continuation, restart Fresh).
>
> **Amended 2026-10-31 (#917)**: a card rendering an unanswered typed answer
> field stops refreshing its ledger, so the repaint cannot wipe the text.

## Context

ADR-0059 made a Turn's Background Tasks gate its card (`⏳ 等待后台任务`) and
gave their completions a Wake continuation, but never showed the tasks as
tasks. The tool part that launches one settles at launch — the call returns the
handle, so the panel reads ✅ — and while any task is live the header is the only
trace: no list, no per-task state. Completion rendered ad hoc: a Wake
continuation card, or one merged receipt line (`🔔 后台任务完成：<命令>`) when the
Wake's work landed in an already-live card. #412 asked for a pinned live list,
fixed completed entries, and a decision between Policy W (ADR-0059: not complete
while tasks are live) and Policy C (`✅` = the turn's answer is done).

The mechanics it builds on: Session Sync keeps polling a yielded session every
8 s (the inflight guard is released at the yield) but PATCHes only when its
content diff owes content; the request sweeps can already PATCH a waiting card in
place; Wake continuations are content-gated; the receipt is a plain timeline line
with no folding; and the read model carries a task's identity and timing but no
label — the originating tool part's `input` (same `call_id`) does.

## Decision

**Policy W stands** (ADR-0059's terminal semantics are unchanged): ✅ and the
Completion Notice fire only at a Turn's true end — idle with no live run. Policy
C was rejected: every completion already opens a continuation, so W's chain reads
⏳ → … → ✅ for one request, while C would stamp ✅ on each interim card and force
the glossary to re-narrow what ✅ promises. The ledger supplies the visibility C
was meant to buy, with one meaning for ✅.

**One ledger of live runs, following the newest card.** Every live run of the
Session — across Turns — renders in a card-tail section like the Todo Panel and
live Tool Panels (ADR-0045): a folded-by-default collapsible panel titled
`⏳ 后台任务（N）` — the title is the count, so the folded panel still answers how
many are running, and a stable `element_id` (`task_ledger`) keeps the reader's
fold state across re-renders — whose body is one row per run. A run is a
Background Task or a Turn-scoped child agent; the section gates on
`reports_live_runs()` (ADR-0106), so an Agent Client with no live-run facts
renders nothing rather than an empty list (the ADR-0059 degradation), and a
live-run set that is momentarily empty renders nothing at all — no empty panel.

The row copy: the type word is the run's tool token — `shell` / `subagent`
(ADR-0107: a token names a concept, never a vendor id), plain — then the label
bolded and clipped (`TASK_LABEL_CHARS` plus `…`; its own `&`, `*` and `_` become
numeric entities before the bold wrap, `&` first, so a command cannot close the
span or bleed formatting), then the run's server `started_at` as local `HH:MM`
and, for a shell row, the bare elapsed (`3m12s` / `1h05m`). A subagent row
renders no total elapsed: its start is its clock and its child-activity fragment
is its liveness (below). The label is joined from the tool part's input by
`call_id` (shell: `command`/`description`; subagent has its own rendering arm),
and elapsed is second-granular and driven by the existing reads (the 8 s Session
Sync pass) — no new polling cadence. No label or no start omits its part whole.
Identity and icons stay out of the live row: Feishu cards have no hover
tooltips, and future task types have no recognizable glyphs. Exactly one card
carries the live list: when a Wake continues the chain or a new Turn takes over,
the handover removes it from the old card.

**A subagent row carries its child's liveness.** The row appends the same child
activity the live task panel shows — `· subagent：**review the diff** · 14:04 ·
shell 5s`, or `· waiting 30s · 等待你的授权` for a phase and its wait — through the
front's own fragment vocabulary, so one child state reads the same wherever cola
renders it. It sits after the start clock and before `· ⚠️ 状态待确认`; a row the
runtime reconciliation could not confirm renders no activity, and a shell row is
unchanged. Both surfaces gather through **one shared batch**: the union of the
children the card's live panels and the read's live runs name — one transcript
light read and one pending-wait query per distinct child, keyed back by call id —
so a child named by both is read once per pass, and a session with no live
subagent spends no child request. The live render and the yielded refresh both
gather. Failure keeps the last successful fragment: a failed child read (or a
timestampless transcript) never refreshes the stored times, so the rendered age
keeps counting truthfully and a failed read never ends the wait (ADR-0054's
rule).

**The render gate compares what the row renders.** The live path owes a PATCH
when a whole minute of the elapsed (or the fragment's age) turns, or when a typed
liveness change arrives — a tool switch, a wait entering or leaving, the gather
establishing or losing the child; the yielded path owes one when a whole
**second** of a visible clock turns (≈0.125 QPS per waiting card at the 8 s
cadence, far below Feishu's per-message cap). Repeated reads inside one rendered
second owe nothing. The comparison is on the rendered row — its type, its folded
and clipped label, its local `HH:MM`, the fragment's label and wait — never raw
values that render identically (text past the clip, a start moving inside the
shown minute) nor the gathered liveness's stored timestamps: a child part landing
inside the second the card already shows owes nothing. The read is still stored
whatever the decision, so the next age counts from the freshest timestamps; an
unconfirmed row's stored age never ticks (it renders none), and an untimed tool
has no age to tick.

**The freeze's one carve-out, widened.** ADR-0059's "the card stops updating" is
narrowed to allow the host card to be re-rendered in place for the ledger:
membership and elapsed stay fresh while the card is otherwise yielded. The
carve-out covers the periodic ledger refresh itself, the handover removal, the
quiet true end and the in-place Wake resume (ADR-0066: a yielded card also
receives its resumed content), and nothing else. It serves **any active
Session's yielded card chain cola holds** — an
External Message follow's waiting card included (#568) — because Session Sync
runs its Wake and ledger steps for that chain too; the write admission is
unchanged (only a yielded card admits the ledger write), so widening the caller
adds no new write path. A chain cola did not start owes no Completion Notice: its
有新消息 send was the notification (ADR-0059).

**A card holding an unanswered typed answer does not refresh.** The yielded read
exists to prove the wait is alive; it stops when the card renders a free-text
answer field nobody has submitted yet (`StreamAccumulator::awaits_typed_answer`),
because Feishu rebuilds `body.elements` on every update and the text lives only
in the client. The freeze covers the **whole ledger read** — rows, a completion
entry, the clock — and the read is **not consumed while frozen**:
`refresh_yielded_ledger` returns before planning its entries, before its child
reads and before `apply_ledger_read`, so nothing is marked shown and nothing
advances, and the first pass after the answer finds the same delta and lands it.
The check is made twice — early, to spend no read at all, and again under the
card-write lock before the apply, so a question surfaced while those reads ran
still keeps its form out of this pass. The settlement that ends the wait is not
ledger progress and still lands (`YieldedUpdate::Settled`).

**A completion leaves a fixed entry where the task lived.** The run leaves the
live list and becomes a collapsible entry on the card that hosted it — the
handover PATCH carries both — with the mechanical receipt line as its collapsed
title (`🔔 shell 完成：<label>` / `🔔 subagent 完成：<label>`, the ending phrases
`已取消` / `失败` / `结束` / `已失联` only on their own evidence, bare when the
Wake names no label) and identity and timing inside (`shell sh_abc · 14:02 ·
12m`). The receipt line and the entry are one mechanism, not two renderings; the
completion still never depends on the model narrating it. A continuation card
carries only its 承接 line and the remaining live list — the entry does not
migrate, so a continuation never accumulates history (ADR-0043's 承接 rule: a
continuation carries only new work).

**The launch panel stops reading as done.** The settled `shell`/`subagent` call
whose metadata says the run is still going keeps its place in the timeline as the
record of the request, but renders 🌙 in its status slot instead of ✅ — the rest
of the title is a normal panel's (`🌙 shell · <label>`); it never claims
completion, persists after the run retires, and the ledger, not the panel, owns
the run's liveness.

**A quiet true end settles the waiting card in place.** When the last run retires
and the Wake's resumed run renders nothing, the read that keeps the ledger fresh
is also the true end: the host waiting card settles in place — ✅, or ❌ on a
settled failure, per its existing rules — and the Completion Notice fires per its
existing rules (groups per `[bridge] group_completion_notice`, p2p per the
long-task threshold and ADR-0074's delivery gate and delay). This supersedes the
drain's expectation that a retirement after the yield leaves the waiting card
alone.

## Consequences

- GLOSSARY gains **Background Task Ledger**; **Wake**, **Card**, **Tool Panel**
  and **Waiting on Background Work** record the entry copy, the launch panel's 🌙
  marker, and the carve-out.
- The ledger must fit the card budget like the existing tail sections — many live
  runs are still one section, rows clipped like the receipt — and the size reserve
  keeps its shell-sized per-row constant, so a subagent row's missing elapsed is
  over-reserve (safe).
- Unchanged copy stays: the live list's `⏳ 后台任务（N）` title,
  `⚠️ 状态待确认`, the resumption header `🔄 后台任务完成，继续处理中…` and the
  permission card's 「调用子代理」.
- A cola restart still loses live cards; the next Wake pass rebuilds the ledger
  on a new card exactly as it rebuilds any continuation, and a restart's
  projection seeds it (ADR-0071).
- Rejected: Policy C; no ledger (the opaque header says something runs without
  saying what); the completed entry following the newest card (history would
  accumulate on every continuation); a separate receipt line plus an entry (one
  fact, one rendering); refreshing only at handovers (the ledger would snapshot
  exactly when a user checks it); keeping the waiting card frozen until
  superseded.

Origins: #403, #405, #412, #415, #416, #419, #423, #501, #502, #503, #504, #568,
#917; ADR-0054, ADR-0059, ADR-0066, ADR-0071, ADR-0106, ADR-0107.
