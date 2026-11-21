# Collapsible panel fold state rides a stable element_id
**Status:** Superseded.
**Decision:** a panel's id comes from a stable identity (`reason_{seq}`, `tool_{call_id}`), never a body position, and is never reused.
Superseded by [ADR-0104](0104-the-timeline-module.md).
Originating issue: none — the decision came from PR #209's review discussion (per-call tool id: #561).
