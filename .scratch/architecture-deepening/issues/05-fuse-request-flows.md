# 05 — Fuse Permission and Question into one RequestFlow

**What to build:** Fuse the Permission and Question flows into one parameterized
flow behind a `RequestKind` trait: the poll loop (sleep → directories → list →
seen guard → inline-host → flush → else build card → resolve target → reply/send
→ stale sweep), the delivery match, the double-click guard and the error-result
block live once. Two thin impls keep the deltas — permission's auto-accept and
description rendering; question's partial-slot accumulation and
submit/skip/reject. Poll intervals become fields on the flow (defaults = today's
values) so every loop branch is testable without sleeping real seconds.

**Blocked by:** 02 — DirectoryBackend seam; 04 — Flows take SharedCore.

**Status:** resolved

- [x] Exactly one poll-loop skeleton, one delivery match, one double-click guard
      and one error-result block exist in the codebase.
- [x] Auto-accept and multi-slot answers still behave identically (the existing
      coordinator-level tests are the regression net).
- [x] Poll loops are driven with small injected intervals in tests; the stale
      sweep and inline-vs-separate branches are exercised.
- [x] Full verification loop green.

## Answer

`permission.rs` and `question.rs` are replaced by a single `request.rs`:

- `RequestFlow` owns the shared skeleton: the poll loop (sleep → directories →
  list → seen guard → inline-host → flush → else build card → resolve target →
  reply/send → stale sweep), the delivery match, the double-click guard
  primitives (`is_answered` / `mark_answered` over the shared `answered_requests`
  set), the shared error-result block (`failed_result_card`), and the
  `sent_cards` map.
- `RequestKind` is a trait with two impls. `PermissionKind` keeps auto-accept
  and the friendly description rendering; `QuestionKind` keeps the partial-slot
  accumulation (`question_partial`) and answer/submit/reject. The question
  kind's remembered requests and partial slots live on the `RequestFlow`
  (`question_requests` / `question_partial`) so tests can seed them directly.
- Poll intervals are an atomic `poll_interval_ms` field (default 3000). Tests
  store a 50 ms interval and sleep 200 ms instead of 3500 ms — the stale-sweep
  and inline-vs-separate branches run in every poll test without sleeping real
  seconds.
- `App` keeps `permission`/`question` as `RequestFlow` instances (one per kind),
  so the dispatch (`app.permission.poll_loop`, `app.permission.handle_card_action`,
  `app.question.*`) and the coordinator-level tests are unchanged.

Verification: fmt clean, clippy `-D warnings` clean, 251 tests pass, release
build succeeds.