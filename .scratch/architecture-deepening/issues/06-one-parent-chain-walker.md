# 06 — One parent-chain walker

**What to build:** Replace the four parent-chain walks (card-target resolution,
inline-host resolution, auto-accept flag walk, descendant check) with one walker
taking a stop predicate. The 8-hop cap and "stop at self-parent" policy live in
exactly one place.

**Blocked by:** 05 — Fuse Permission and Question into one RequestFlow.

**Status:** ready-for-agent

- [ ] All four walk sites call the one helper with their own predicate.
- [ ] The hop-cap policy appears exactly once in the codebase.
- [ ] Walker covered by unit tests over scripted session info (parent chains,
      self-parents, cap exhaustion).
- [ ] Full verification loop green.