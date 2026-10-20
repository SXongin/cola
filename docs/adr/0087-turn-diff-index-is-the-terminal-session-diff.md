# The Turn Diff index is the artifact's own terminal read

## Context

Spec #797 split the Turn's diff into two surfaces: a card-tail index
(`📝 本次改动 · N files · +A −D`) with a 「查看完整改动」 button, and a
self-contained HTML artifact. #477 built the index from #800's tool-metadata
aggregation on **both** generations because a card render may not hit the
network. V2's artifact, however, reads `session.diff` scoped by the Turn's
user-message anchor. The two surfaces therefore had two sources and two
moments:

- the aggregation keeps only the **last recorded patch per path**, which is
  not the Turn's net state: an edit reverted later in the same Turn still
  counts `+A −D`, while `session.diff` answers `data: []`;
- a file edited more than once is undercounted by the index;
- after a location switch a Turn's snapshot can be unanswerable even though
  the transcript recorded edits — observed live as a `200` with `data: []`
  for a linked-worktree Turn that did edit files, and `Turn range spans a
  location change` for an unscoped read (issue #830).

The visible result was a dead end: a non-zero index, and a click answering
「本 turn 没有文件改动」. The index asserted a change the artifact could not
honour.

## Decision

- **One source, one moment.** The index is resolved once, when the Turn
  settles, from the same `session.diff` read the click's artifact uses. No
  transcript-side estimate is rendered on any generation, and no Turn-level
  count is shown while the Turn runs. A Tool Panel's own per-call diff is a
  different surface and stays live.
- **All terminal cards.** The read happens at the shared terminal seams —
  `Turn::finish`, `Ticket::apply_ending_if_owned` and the external render's
  `apply_disposition_if_anchor` — so a Wake continuation, a followed external
  run and a normal Turn's card all get the same index. It is skipped for a
  non-terminal ending (`Waiting`), for a card with no Turn anchor, and for a
  generation with no endpoint. A card resuming work clears its settled index
  until the next terminal read.
- **Authority or nothing.** The settle read is
  `session.diff(from = <anchor>, context = 0)`, bounded at 5 s and performed
  outside the card lock. A non-empty answer is the index. An empty answer, a
  read failure or a timeout leaves zero stats, so the card renders neither
  the line nor the button. The tool aggregation is never substituted for an
  empty or failed V2 answer.
- **V1 loses the surface.** V1 has no `session.diff`; its tool aggregation is
  deleted together with the V1 artifact fallback and the
  「按工具记录聚合」 subtitle. V1 cards render no index and no button: the
  feature is V2-only.
- **The click re-reads the same source.** It requests `context = 3` for the
  artifact's hunks. A successful empty answer says
  「本轮期间工作区没有留下净变化。」; a failed read says
  「暂时读不到终态改动，请稍后再试。」 and stays retryable. A legacy V1
  button — from the unreleased feature's cards — lands on the same
  read-failure answer rather than a special case.

## Considered options

- **Keep the estimate, label it, and explain a mismatch on click**: rejected.
  The line still asserts a count the artifact may deny; that is #830's core
  complaint, and a label does not remove the contradiction.
- **Fall back to the tool aggregation when `session.diff` is empty or
  unavailable**: rejected. An edit reverted in the same Turn is exactly the
  "the tools did something, the net is zero" case; a labelled fallback would
  restore the false entry the issue reports.
- **Read only at click, with no counts (button-only)**: rejected. The card
  would still offer an entry that can open nothing, and the at-a-glance index
  is lost without gaining correctness.
- **Flush the terminal card first, then patch the index on a background
  read**: rejected. It writes every terminal card twice and shows the index
  only after the completion signal.

## Consequences

- The line and button appear only on a terminal card, together. A split's
  tail carries them; a finalized prefix does not.
- One bounded `session.diff` read per terminal V2 card, read-only Turns
  included. `context = 0` keeps the read to the counts' cost.
- A Turn whose server snapshot cannot answer — a location switch, or a
  net-zero Turn — has no entry. The linked-worktree case in #830 loses its
  entry rather than showing a number the artifact cannot honour.
- The counts describe the Turn's recorded snapshot, not the current working
  tree; the card can lag the workspace by design.
- A hung server can delay a terminal card by at most the 5 s bound; the
  ending is never blocked on the read.
- `#797` decisions 6 and 8 and `#477`'s live-index rule are amended by this
  ADR.

## Amendment — 2026-10-21 (#844): the copy names the measurement

`session.diff` diffs two **worktree** snapshots taken at the Turn's step
boundaries (the working copy when the Session is still active), so its answer
is "the worktree changed between T0 and T1". It is not an authorship claim:
another session in the same directory (a sibling chat, a `task` child, a
background shell), an editor, or a `git merge` inside the window lands in the
same diff, and neither the API nor the bridge records write provenance. The
overlap is undetectable in general — the writers can both be idle by settle
time — so no active-session heuristic can restore attribution.

The index and the artifact therefore keep the same single source (the decision
above is unchanged) but stop claiming the Turn wrote the files:

- the card line is `📝 本轮期间工作区变化 · N files · +A −D`;
- the button is `🔍 查看完整变化`;
- the artifact title is 「本轮期间的工作区变化」 and its subtitle ends with
  「快照区间差，可能含同目录其他写入者」;
- an empty answer says 「本轮期间工作区没有留下净变化。」

The counts, the terminal-card gate, the 5 s bound and the "authority or
nothing" rule are untouched. A future source with per-write provenance (an
upstream turn-attribution API) is what would let the copy claim authorship
again.
