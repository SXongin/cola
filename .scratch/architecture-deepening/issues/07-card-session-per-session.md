# 07 — CardSession per session

**What to build:** Make "one live card per session" a single persistent module
that owns the streaming accumulator and the card identity chain, replacing the
two per-session maps kept in lockstep. The prompt path resets the card's content
per turn but keeps its identity; the mid-prompt session-recreate becomes one
remap operation instead of a three-map re-key; card updates (including
continuation cards) are app-independent. This builds on the dead-fold removal.

**Blocked by:** 01 — Delete the dead SSE event fold; 04 — Flows take SharedCore.

**Status:** ready-for-agent

- [ ] "One live card per session" is one module; no parallel identity/content
      maps kept in lockstep by hand.
- [ ] The session-recreate path performs a single remap operation.
- [ ] State transitions (loading → streaming/reasoning → done/error, inline
      sections) unit-tested; card flushing tested against the recording platform.
- [ ] Full verification loop green.