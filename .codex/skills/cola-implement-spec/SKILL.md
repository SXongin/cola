---
name: cola-implement-spec
description: Implement cola spec tickets on one integration branch, or one PR per ticket in the spec's per-ticket landing mode; use only when the user asks for the batch.
---

Read .opencode/skill/implement-spec/SKILL.md, docs/agents/flow.md, and CONTRIBUTING.md. Translate dispatches to the Codex agents `implementer`, `merger`, and `reviewer` (one dispatch per review axis). Use $cola-code-review for paired review. Keep ticket work isolated, land linearly, verify the final tree, and stop at a green PR for user merge — or hand over when a gate stop predicate holds (`CONTRIBUTING.md` "Reviewing"). In the spec's **per-ticket** landing mode, ship one PR per ticket and merge each of the tickets the spec's grant names under that grant before cutting the next branch; without a grant — or for a ticket it does not name — stop at each green PR.
