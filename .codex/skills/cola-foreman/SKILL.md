---
name: cola-foreman
description: Run one cola spec as a serial ticket batch; use only when the user explicitly asks.
---

Read .opencode/skill/foreman/SKILL.md and docs/agents/flow.md in full. Use the Codex agents `implementer`, `merger`, and `reviewer` (one dispatch per review axis) instead of OpenCode subagent dispatch. Invoke paired review with $cola-code-review. Keep blocker order, one ticket at a time, linear landings, verification, and stop at green PR for user merge. Do not start from a mere spec mention.
