# Collapsible panel fold state rides a stable element_id

**Status: Superseded by** [ADR-0104](0104-the-timeline-module.md) — a panel's id
comes from a stable identity (`reason_{seq}`, `tool_{call_id}`), never a body
position, and is never reused. From PR #209, no issue (#561).
