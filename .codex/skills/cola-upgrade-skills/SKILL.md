---
name: cola-upgrade-skills
description: Audit and update cola skill adapters when explicitly asked.
---

Read .opencode/skill/upgrade-skills/SKILL.md and docs/agents/skills.md. Treat .opencode/skill as canonical OpenCode source and .codex/skills/cola-* as the Codex adapters (the Codex-only root, so OpenCode's `#<id>` list gains no near-duplicates). Compare upstream snapshots as source directs; update only valid changes by hand. Never overwrite wholesale or refresh an unfurled snapshot. Report movement, folded changes, and deferred changes.
