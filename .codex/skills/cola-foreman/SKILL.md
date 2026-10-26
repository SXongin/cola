---
name: cola-foreman
description: Run one cola spec as a serial ticket batch; use only when the user explicitly asks.
---

Read .opencode/skill/foreman/SKILL.md and docs/agents/flow.md in full. Use Codex agents implementer, merger, standards_reviewer, spec_reviewer, and adversarial_reviewer instead of OpenCode subagent dispatch. Invoke paired review with $cola-code-review. Keep blocker order, one ticket at a time, linear landings, verification, and stop at green PR for user merge. Do not start from a mere spec mention.
