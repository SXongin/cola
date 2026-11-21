# Collapsible panel fold state rides a stable element_id

**Status: Superseded.** A panel's `element_id` derives from its timeline entry's
`seq`, never from a body position, and `seq` is never reused or renumbered — so
fold state survives a settle.

Superseded by [ADR-0104](0104-the-timeline-module.md), from PR #209; the
decision came out of a review discussion and has no originating issue.
