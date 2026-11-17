---
name: cola-implement-spec
description: Implement cola spec tickets on one integration branch; use only when the user asks for the batch.
---

Read .opencode/skill/implement-spec/SKILL.md, docs/agents/flow.md, and CONTRIBUTING.md. Translate dispatches to the Codex agents `implementer`, `merger`, and `reviewer` (one dispatch per review axis). Use $cola-code-review for paired review. Keep ticket work isolated, land linearly, verify the final tree, and stop at a green PR for user merge — or hand over a spent gate budget (`CONTRIBUTING.md` "Reviewing").

Rehearse the CI gate before every push: run the full `.github/codex/prompts/review.md` prompt read-only with the spec and ticket bodies (`codex exec`, or the equivalent Codex dispatch), on the gate's own lane and the batch's own trusted text; fix every blocking finding, answer and defer the non-blocking ones, and push only after it PASSes (or after recording a lane skip when that lane cannot be reproduced locally). The rehearsal is a pre-check, never the CI approval.
