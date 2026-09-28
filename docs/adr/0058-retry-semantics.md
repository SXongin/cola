# Retry semantics: the state → action matrix, live-run re-attach, and a narrowed id reuse

## Context

An Error Card's 重试 used to re-submit the failed prompt under the same
`msg_cola_` id (ADR-0026). On the current generation that re-post is a no-op
once the id has been admitted: the server reconciles it onto the existing
admission and runs nothing, cola re-reads the old failure, and the card lands
back on Error — the observed "retry retries nothing" bug (spec #391). The reuse
scope was too broad in both directions: reuse is only safe where a same-id
re-post still runs, and a click whose read fails must still have an effect. Two
neighboring fixes bound the problem: the render baseline that stops a retry
from replaying the failed attempt (#387, ADR-0040's 2026-09-28 amendment), and
the follow whose lost-contact fallback can leave an Error card over a run that
is still alive (#386).

## Decision

**A retry re-runs the same question on the same Session, at most once, and
never asks it again while the first run is alive.** At the click, cola takes
one bounded read pair — the session's run status and its transcript — then
evaluates the matrix top-down.

### The state → action matrix

| # | Observed state (read pair + id + capability) | Action |
|---|----------------------------------------------|--------|
| 1 | status `Busy`/`Retry`: a run is alive | **re-attach** the card to the run; no submission |
| 2 | status unreadable (read failed, timed out, or an unknown status kind) | **submit under a fresh id** |
| 3 | idle, but no stored `msg_cola_` id, or no transcript read | **submit under a fresh id** |
| 4 | idle, id present, transcript readable, but no anchor for the id (nothing of the submission was stored, or it cannot be ordered) | **submit reusing the id** |
| 5 | idle, anchor present, the turn reads settled (an assistant message started within it carries a terminal finish) | **submit under a fresh id** |
| 6 | idle, anchor present, the turn reads unfinished (a failed step, or a run the server died in) | **submit reusing the id iff the generation's re-post continues an admitted turn** — V1 yes, V2 no (fresh id) |

Each row presupposes the rows above it did not match. The status cell
dominates: an unreadable status submits a fresh id without consulting the
transcript, because the click must always have an effect and a same-id re-post
against an admitted turn can be a no-op. The settled/unfinished split is read
through the same neutral turn projection the finalization uses
(`turn_for_user(..).complete`), so the retry and the card agree on what "the
turn ended" means.

### Re-attach: a live run is never re-asked

`Busy`/`Retry` means a run is alive — possibly one cola cannot currently read,
which is exactly the state the follow's lost-contact fallback can leave an
Error card over. The retry submits nothing. The card leaves Error (its content
kept), a live header state is restored, the unused claim goes back so a later
failure can be retried, and the out-of-turn follow keeps rendering the same
card until the run truly ends (Done, Error, or Stopped). Re-attach adds no new
loop: it hands the card to the follow that already owns Busy→non-busy
finalization, `/stop`, the failure graces and the silent exit when a new Turn
replaces the accumulator (ADR-0043, ADR-0050). When the card is no longer in
Error, has no anchor to follow, or has vanished, nothing is submitted either:
the claim is released and the Error card keeps a working retry.

### One card per attempt; the failed card is marked, not reset

A submit-branch retry replies a NEW card and streams the new attempt there; it
never resets the failed card in place. The failed card becomes terminal
`Retried` — header 「↩️ 已重试」, grey, the retry button suppressed, a marker
line appended, every byte of the failed content preserved — so the failure
stays readable and a stale click cannot retry a newer turn. The marking happens
only under the session's inflight guard (a retry that loses the guard marks
nothing and releases its claim), and the atomic claim that gates the click is
taken only while the card is an unclaimed Error, so a double-click submits at
most once. The attempt's fresh accumulator carries the failed attempt's render
baseline across: the old attempt's already-observed messages stay suppressed,
and the new card shows only the new attempt (ADR-0040's 2026-09-28 amendment).

### Why the id policy is generation-aware

The id is sometimes reused and sometimes fresh because the two generations'
servers give `msg_cola_` ids different meanings (ADR-0055):

- **Fresh id — nothing else runs.** A same-id re-post of an admitted turn is a
  silent no-op on V2: the id is an admission key, and the server reconciles the
  re-post onto the existing admission. For a settled turn there is additionally
  nothing left to continue, and a fresh attempt is the honest history: the
  question appears in the session as asked again. Unknown reads take the
  fresh-id branch for the same reason — reuse is only safe in the
  never-admitted territory, and an unknown read cannot establish it. The
  accepted cost is one extra user-message row after a read hiccup, which beats
  a click that does nothing.
- **Reuse — idempotent, never-admitted.** When no anchor exists for the id,
  nothing of the submission was stored: re-posting it is the idempotent submit
  both generations still create and run, so a submission that may have landed
  is never duplicated.
- **Reuse iff the generation continues an admitted turn.** An admitted id with
  an unfinished turn is continuable only where the server's re-post upserts and
  runs (V1). On V2 the re-post is an admission no-op for settled, aborted and
  orphaned turns alike, so the retry must take a fresh id to have any effect.

The generation capability is the *contract*
(`Backend::reuse_continues_an_admitted_turn`), not a heuristic: the retry
matrix reads it instead of sniffing generations.

### `/stop` ends `Stopped`, never a retry

The #386 spec's 「`/stop` — prompt Done」 is superseded: a deliberate stop is
the operator's decision, not a failure, and finalizes the card `Stopped`
(「⏹ 已停止」, #394; ADR-0043's 2026-09-28 stop-terminal amendment). `Stopped`
is terminal for every running probe and renders no retry button, so a stopped
card never enters this matrix; the abort text the server recorded is never
written to the card, and the completion notice says 已停止.

## Evidence

- **V1's admitted-unfinished continuation** is pinned live by
  `live_v1_scripted_failure_and_retry_chain`: a same-id re-post after a failed
  (no terminal finish) turn upserts the one user message and runs a new step,
  which recovers to a clean completion (ADR-0057).
- **V2's admission no-op** is pinned live by `live_v2_scripted_retry_id_chain`
  against server 2.0.18: a same-id re-post on a settled turn, an aborted turn
  (`finish=error`/`aborted` — a terminal finish, so it reads settled), and an
  orphaned turn (no finish, tool still `running` after the server was killed —
  unfinished) all answer `200` echoing the admitted id, add no assistant
  message, make no model call, leave every row byte-identical, and append only
  an idle event. A fresh id on the same session runs a second turn for real.
- The bridge seam pins every matrix cell and the card behavior (mark, new card,
  re-attach, double-click) without a live server; the live chains above pin the
  server contract the policy rests on (ADR-0057).

## Consequences

- A retry always has an observable effect and never duplicates a running turn;
  the failed card is a permanent, readable record and the retried question is
  either the same message (reuse) or a genuinely new one (fresh id).
- A read hiccup can leave one extra prompt row; accepted over a silent no-op.
- Images are not re-sent by a retry (deferred by #391).
- External aborts (another client's stop) still read as failures and remain
  retryable; classifying them as stops is out of scope.
- ADR-0026's "a retry reuses the same id" is narrowed by a dated correction
  note there: reuse covers only the never-admitted case plus V1's
  admitted-unfinished continuation.

Source: spec #391 (retry semantics; build tickets #392–#396). Related: #387
(the replay flood), PR #390 (the render baseline), #386 (the follow whose
fallback can fail a live run), #394 (the stopped terminal); ADR-0026 (the id),
ADR-0040's 2026-09-28 amendment, ADR-0043's 2026-09-28 amendment, ADR-0050
(Turn interface operations), ADR-0057 (the live contract suite).
