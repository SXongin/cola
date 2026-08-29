# 04 — Flows take SharedCore

**What to build:** Narrow the dependency of the flows, pollers and render
pipeline to the shared state they actually use, instead of the whole bridge
coordinator, so each flow is constructible and testable with the shared state +
the two adapter mocks alone. The one cross-flow map poke (writing another flow's
user-message baseline from the prompt path) becomes a method on the owning flow.
Break the command↔coordinator module cycle by giving command dispatch the same
narrowed dependency.

**Blocked by:** 02 — DirectoryBackend seam.

**Status:** ready-for-agent

- [ ] Every flow constructs with the shared state + adapter mocks alone — no
      coordinator needed.
- [ ] No module pokes another flow's private state; that coupling is a method
      call on the owning flow.
- [ ] The module dependency graph is acyclic (no command↔coordinator cycle).
- [ ] Full verification loop green.