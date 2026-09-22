# Release notes: LLM highlights prepended to the native changelog, generated after publish

`generate_release_notes` produced the release body as a flat list of merged PR
titles — internal docs, CI and dependabot entries included — which reads as a
developer changelog, not as something a Host skims. The `notes` job now turns
the PRs merged since the previous tag into an English highlights body with one
tool-free model call, and the `notes-apply` job prepends it to the native
list. Generation is deliberately post-release and non-blocking: a failed
gather or model call leaves the release exactly as published. ADR-0033 left
changelog automation as an additive step; this is that step.

## Decision

- **Publish first, notes after.** `publish` creates the release with the native
  body; `notes` generates the highlights artifact alongside the builds (no
  `needs`), and `notes-apply` — which waits for `publish` — edits the release.
  A hung or failing model call costs the release nothing.
- **Read-only generation, one writer.** The `notes` job has `contents: read`
  and `pull-requests: read`, the model runs with every tool permission denied
  (`opencode run` auto-rejects an ask rather than hanging), and nothing evals
  its output. `notes-apply` is the only job that writes the release body (the
  pre-existing `publish` job keeps its `contents: write` for assets); its only
  input is the artifact file.
- **Highlights + native list.** The body is English `Highlights` / `Fixes` /
  `Breaking changes` sections (empty ones omitted, at most 8 bullets) followed
  by the untouched native list. That list is the safety net: no merged PR
  disappears from the release body even if the model ignores it. The material
  handed to the model is byte-capped, which can thin the highlights but never
  the list.
- **Breaking changes need evidence.** The model may only list a compatibility
  change when a merged PR's body states one (a body citing an ADR counts, and
  the material is PR bodies only), with the source cited. The manual
  `gh release edit` note is no longer a pre-step; it stays the correction
  channel (ADR-0041's `sessions.json` note is the model case).
- **No `CHANGELOG.md`.** Notes are GitHub-side metadata. A repository file
  would have to be committed before the tag, making the notes a release gate
  and violating the tag-on-main contract (ADR-0033).
- **The dry run is `workflow_dispatch`.** Dispatching the Release workflow
  with a tag (or tag range) runs the same generation with the real secret,
  prints the result, and applies nothing; the other jobs are push-only.

## Considered options

- **Generate before publish and pass the body to the release action.** The
  create endpoint prepends a supplied body to the auto notes, so this would
  compose for free — but it makes every release wait on the model call, and a
  hung call delays publishing. Rejected: the release must not depend on notes.
- **Edit the release assuming the native notes survive.** They do not: the
  update endpoint has no `generate_release_notes` parameter, so an edit
  replaces the whole body. The apply step reads the published body back and
  composes highlights + native list itself; a marker in the generated body
  makes a re-run idempotent.
- **Draft the release for human review.** Drafts are invisible to
  `releases/latest`, which self-update reads (ADR-0015); it would break the
  update channel and sacrifice automatic publishing.
- **Write the notes in Chinese, or bilingually.** The repository, README and
  all ADRs are English, the appended PR list is English, and Feishu support is
  China-only (`open.feishu.cn` is hardcoded) — there is no second audience for
  a second language.
- **Deterministic generation (filter PR types, no model).** Titles are terse
  and the value asked for is a user-facing summary, which is exactly a
  language task.

## Consequences

- The first release after this lands is the production verification; the
  `workflow_dispatch` dry run rehearses both generation and the failure path.
- PR titles and bodies are untrusted input. Prompt injection can only change
  prose in the notes, and a misleading line is correctable with
  `gh release edit`.
- The prompt, model and pinned opencode version live in the Release workflow;
  changing them is an ordinary PR.
