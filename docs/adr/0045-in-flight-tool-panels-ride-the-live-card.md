# In-flight Tool Panels ride the live card; settled panels join the timeline

A **Tool Panel** rendered as a timeline row: it sat where its tool call started,
and a **Card Chain** split could finalize the card while the tool still ran. The
finalized card then froze the panel at `⏳` forever, and the delta continuation
(ADR-0043 amendment) re-rendered nothing before the split — so the tool's result
appeared on no card at all. Observed live with `sleep 30` straddling a Supplement
split (#243). This ADR moves an unfinished Tool Panel into the card TAIL — the
section only the live card carries — and lets it enter the timeline only once the
tool settles. It is the answer the card already gives twice: live state (Todo
Panel, Interaction Blocks) rides the tail, settled history is a timeline row.

## Context

- **The tail is the live section.** A split finalizes the previous card without
  the tail, so the tail's content always continues on the newest card. The Todo
  Panel was put there for exactly this reason ("a split never strands the reader
  on a stale list"); Interaction Blocks migrate the same way.
- **A running tool was the only timeline row that could still change.** The
  accumulator keeps updating its state (`running` → `completed`, streamed
  output), but the finalized card showing it was frozen. Timeline entries are
  keyed by server start time, and a closed slice is never re-rendered.
- **A clean abort settles.** OpenCode writes an aborted running tool as
  `status: "error"` ("Tool execution aborted"), so it takes the normal path. A
  hard crash leaves the part `running` with nothing to settle — out of scope
  here, exactly as on any card today.
- **A running tool is in practice the last content.** OpenCode runs a step's
  tools to completion before the model continues, so the tail's bottom is where
  the row already appeared; the move is invisible in the common case.

## Decision

- **Unfinished Tool Panels are tail content.** A panel whose tool is `running`
  or `pending` renders in the tail, in call order, after the Todo Panel and
  before the Interaction Blocks — so a running tool's Permission/Question stays
  below its own panel. It rides every split onto the new live card; a finalized
  card never carries it.
- **Settled panels join the timeline.** When the tool reaches any other status,
  its panel is inserted into the timeline at its server start key — clamped to
  the top of the live slice when the key is behind `render_from`, the ADR-0038
  late-part rule — and stops rendering in the tail. The permanent transcript is
  strictly ordered; only the live view is not.
- **A split with nothing but a live tail may finalize an empty card.** The old
  card keeps its 「部分完成，继续中」 header and footer, and the live tail
  continues on the continuation. Accepted: a card whose only content is a Todo
  Panel or an Interaction Block has done exactly this since ADR-0043, and
  nothing is lost — the panel is live on the card below.
- **The tail's size reserve covers it.** `estimate_split_index`'s reserve
  generalizes from the Todo Panel alone to the tail's large items (todo +
  running panels), so a card that fits without its tail but not with it
  finalizes without the tail and lets the continuation carry it.
- **Element identity survives the move.** The panel's `element_id` is allocated
  when it first appears (running, in the tail) and reused when it joins the
  timeline, so fold state (ADR-0040) does not reset at settle.

## Why

- The stranding becomes impossible by construction: the tail renders on the live
  card only, so no split seam has to move anything and no finalized card can
  hold content that still changes.
- The reader watches the tool finish where they are looking — the newest card —
  instead of scrolling up.
- One rule covers both split causes (size and Supplement) and both engines
  (cola's own turns and the external renderer).

## Alternatives considered

- **Patch the finalized card in place** — the panel stays where it started and
  the old card is repainted with the result when the tool settles. Rejected: the
  result lands above the reader's viewport with no notification, and it needs a
  per-session registry of finalized cards plus a repaint writer.
- **Carry stranded panels at each split seam** — keep panels as timeline rows
  while live, move any the boundary would finalize onto the continuation.
  Rejected: two seams need boundary bookkeeping, and the case where the running
  panel is the live slice's first row has no clean answer — the finalized slice
  is empty — so it degrades into duplication or a frozen copy.
- **Duplicate the panel onto the continuation, leave the old copy** — rejected:
  the old copy freezes at `⏳` forever (a lie), and the operator already refused
  duplicated content for Supplement splits (ADR-0043 amendment).
- **Mark the stranded panel interrupted** — rejected: the tool's output is
  usually the point of the call; this loses it.

## Consequences

- While a tool runs, its panel can render below timeline content keyed after its
  start (a receipt that landed mid-run); at settle it snaps into its ordered
  position. Accepted: live view vs. transcript, not history corruption.
- The tail gains a dynamic content type: its ordering, size reserve and the card
  build must treat running panels like the todo section.
- The header's running-tool selection is unchanged: only a `running` tool names
  the header (「⏳ bash」); a `pending` panel shows in the tail while the header
  keeps its plain streaming label.
- A split while a tool runs no longer leaves a running panel on the finalized
  card — the behavior this ADR exists to change. Only a crash-orphaned panel can
  still freeze, as before.

## Domain note

The **Tool Panel** glossary entry gains the live/settled rule, and the
Relationships list gains the same one-liner.
