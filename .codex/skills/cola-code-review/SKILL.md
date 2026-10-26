---
name: cola-code-review
description: Run cola paired review at review points or when asked to review changes.
---

Read .opencode/skill/code-review/SKILL.md and docs/agents/flow.md. Pin the base and pathspec. Dispatch standards_reviewer, spec_reviewer, adversarial_reviewer in parallel from this orchestrator; keep all read-only and on the same scope. Include untracked paths. For same-scope re-review, resume threads when supported or start fresh with verbatim findings and the fix delta. Report separate Standards, Spec, and Adversarial sections; do not merge or rerank. Keep CODEX_REVIEW_VERDICT last. Never edit.
