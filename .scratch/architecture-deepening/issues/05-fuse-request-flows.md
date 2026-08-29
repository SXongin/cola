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

**Status:** ready-for-agent

- [ ] Exactly one poll-loop skeleton, one delivery match, one double-click guard
      and one error-result block exist in the codebase.
- [ ] Auto-accept and multi-slot answers still behave identically (the existing
      coordinator-level tests are the regression net).
- [ ] Poll loops are driven with small injected intervals in tests; the stale
      sweep and inline-vs-separate branches are exercised.
- [ ] Full verification loop green.