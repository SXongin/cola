# Worktree: cola creates the checkout with git and starts by Pending Session
**Status:** Superseded.
**Decision:** creation is a `--worktree` flag on `/topic` and `/new`, lazy at materialisation; cola runs git, the root is its own setting, and it creates but never removes.
Superseded by [ADR-0095](0095-worktree-creation-is-a-new-conversation-flag.md).
Originating issue: #449.
