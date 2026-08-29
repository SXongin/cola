# 08 — Move the test harness out of the coordinator

**What to build:** Move the test harness (mock backend, recording platform,
app-builder, and the ~4,500 test lines living in the coordinator module) into a
`cfg(test)` support module, so the coordinator reads as a coordinator and its
production size returns to normal scale. The harness stays importable by sibling
modules so flow-level tests keep driving it.

**Blocked by:** 05 — Fuse Permission and Question into one RequestFlow.

**Status:** ready-for-agent

- [ ] Coordinator module contains no test harness and no wholesale test bodies;
      production-only scale.
- [ ] Harness is a `cfg(test)` support module importable by sibling modules.
- [ ] Full suite (moved tests included) green.
- [ ] Full verification loop green.