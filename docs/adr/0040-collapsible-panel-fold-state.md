# Collapsible panel fold state rides a stable element_id

**Status: Superseded.** A panel's `element_id` derives from its timeline entry's
`seq`, never from a body position, and `seq` is never reused or renumbered — so
fold state survives a settle.

Superseded by [ADR-0104](0104-the-timeline-module.md); the snapshot example
retired by [ADR-0100](0100-a-session-has-one-card.md). Originated in PR #209,
out of a review discussion — no originating issue.
