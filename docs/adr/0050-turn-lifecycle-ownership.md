# Turn lifecycle ownership: a deep module behind narrow handles

**Status: Superseded.** The Turn stays one deep module whose delivery operations
are `flush_card` / `split_card_chain` / `render_and_flush` /
`session_subtitle`; the Timeline is private to it.

Superseded by [ADR-0104](0104-the-timeline-module.md), from #301 (spec #298);
the narrow handles stay [ADR-0049](0049-narrow-handles-shared-core-aggregate-root.md).
