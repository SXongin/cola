# Backend contract owns a neutral read model; wire types stay adapter-private

The backend seam's interface carried OpenCode's wire contract: `Backend`
returned `SessionMessage` (from `opencode::types`, explicitly wire types) whose
`parts` were raw `serde_json::Value`, so six production modules outside the
adapter indexed protocol fields directly (`turn/render`, `turn/state`,
`external`, `snapshot`, `request/kind`, `feishu/card/tool_render`). With
OpenCode's `/api` generation mounted beside the legacy unprefixed routes and
changing message, part, and tool shapes, every protocol change was a
bridge-and-card change, and the mock adapter had grown into a second
implementation of the wire contract.

The decision is that the read side of the seam carries a neutral
`SessionTranscript` in `src/backend/` (implemented by #333–#339): typed envelope
fields (message identity, role, server time,
model identity, token usage), a `Part`/`ToolCall` view whose tool payloads stay
raw, and tolerant `Other`/`Unknown` arms. `SessionTranscript` owns the
projections the bridge used to re-implement (`newest_user`, `turn_for_user`,
`transcript_tail`); a Turn's anchor travels as message identity plus server
time. Protocol field names live only in `opencode::wire` decoders — one per API
generation, selected inside the adapter and invisible to the Bridge — and
`prompt`/`prompt_async` keep their semantics while their response parts decode
through the same seam. Once no caller remains, the wire modules are sealed
`pub(crate)`.

## Naming across generations

Route labels flipped upstream since ADR-0001/0007/0026 were written. Those ADRs
call `/api/*` legacy because it then hosted an older generation whose prompt
appended a fresh user message and emitted only `v2` events (ADR-0001; ADR-0026
verified the behaviour live), while the unprefixed routes were canonical.
Upstream has since adopted `/api/...` as the current protocol surface
(`@opencode-ai/protocol` and `sdk-next`, announced as V2 with the V2 label
itself now being normalized away) and kept the unprefixed routes as the V1
compatibility layer. In this ADR and the spec, **legacy** means the unprefixed
routes and **current** means `/api/...`; the older ADRs' "legacy `/api/*`"
wording describes the earlier generation — ADR-0001/0007/0026 each now carry an
`Amended by ADR-0053` banner, and their labeling stands only as history.

## Considered Options

- **Keep wire DTOs on the trait; normalize in each consumer.** Rejected: that is
  the current drift, and the `/api` generation would be translated six times.
- **An event-shaped read model** (parts normalized into a delta stream).
  Rejected: cola polls message snapshots, not an SSE subscription; an event
  model would be a redesign the polling architecture does not need.
- **Per-built-in-tool typed payloads inside the read model.** Rejected: tool
  payload interpretation is the Platform's deliberate specialization (ADR-0042);
  the read model keeps payloads raw.

## Consequences

- A protocol change — including the `/api` generation — is an adapter change: a
  decoder plus its contract tests, with no bridge or card edits.
- `MockBackend` scripts the neutral views; a transitional wire-fixture helper
  runs the production decoder, and is deleted after the fixtures migrate.
- ADR-0010's seam stands; this decision deepens what the trait carries, not
  whether the trait exists.
