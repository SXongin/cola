---
name: cola-adversarial-review
description: Run cola CI adversarial review on a diff when requested or called by cola-code-review.
---

Read .opencode/skill/adversarial-review/SKILL.md and .github/codex/prompts/review.md. Pin base and pathspec, include untracked paths, then dispatch one `reviewer` sub-agent with axis `adversarial`, giving it the refs and commands. Require every untracked path to be read. Use the rubric sections verbatim, and apply the rubric's own host-tree rule: where the reference trees named in docs/agents/references.md are present, check the claims against them; where they are absent, use the diff's tests and recorded fixtures. Keep findings separate from Standards and Spec, preserve CODEX_REVIEW_VERDICT last. Never edit.
