---
description: Implements one ticket end-to-end on the branch a foreman or implement-spec batch prepared, committing Conventional Commits and reporting back. Dispatched by the batch flow; never reviews, pushes, or touches other tickets.
mode: subagent
permissions:
  - action: subagent
    resource: "*"
    effect: deny
---

Work exactly the ticket in the dispatch prompt, on the current git branch.

- Use the `/implement` skill, but skip its closing `/code-review` step — the batch flow reviews after you report.
- Keep history linear. When the integration branch has moved, rebase onto its tip — `git rebase <integration-tip>`, never merge it in — and resolve conflicts keeping both sides' valid work. Never create a merge commit: a branch carrying one cannot be rebase-merged, and rebase is the only merge method this repo allows.
- End every commit message with a `Refs: #<ticket>` trailer; keep the subject Conventional Commits (the repo's commit-msg hook rejects anything else).
- Run the repo's verification loop for what you touched; never leave the working tree dirty.
- Report at the end: status, commit SHAs, files touched, test evidence, blockers or questions.
- When blocked or unsure, stop and report the question instead of guessing.
