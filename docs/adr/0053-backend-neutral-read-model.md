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

The read side of the seam is now a neutral `SessionTranscript` in
`src/backend/`: typed envelope fields (message identity, role, server time,
model identity, token usage), a `Part`/`ToolCall` view whose tool payloads stay
raw, and tolerant `Other`/`Unknown` arms. `SessionTranscript` owns the
projections the bridge used to re-implement (`newest_user`, `turn_for_user`,
`transcript_tail`); a Turn's anchor travels as message identity plus server
time. Protocol field names live only in `opencode::wire` decoders — one per API
generation, selected inside the adapter and invisible to the Bridge — and
`prompt`/`prompt_async` keep their semantics while their response parts decode
through the same seam. Once no caller remains, the wire modules are sealed
`pub(crate)`.

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
