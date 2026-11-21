# Collapsible panel fold state rides a stable element_id

**Status: Superseded.** A panel's `element_id` derives from a stable identity —
the entry's `seq` for a reasoning panel, the call id for a tool — never from a
body position, and that identity is never reused.

Superseded by [ADR-0104](0104-the-timeline-module.md), which records the
per-call tool id (spec #561, [ADR-0071](0071-rendered-cursor.md)). From PR #209; no issue.
