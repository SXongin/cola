## What
<!-- One paragraph: what does this change do? -->

## Why
<!-- The problem it solves. Reference the issue — ALWAYS: the Codex gate collects references from this body and from `Refs: #N` commit trailers, and fails before the model runs when neither carries one, so a change with no issue yet creates one first (CONTRIBUTING.md "Pull request rules" 3) — `Closes #N` completes an issue, `Refs #N` merely relates to one; a dependency batch references the standing deps spec (#733) instead of a per-batch ticket. Leave the examples as `#N`: a literal number here is collected as this PR's own reference. -->

## How tested
<!-- Commands run, e.g. the verification loop from CONTRIBUTING.md:
     cargo fmt --all -- --check
     cargo clippy --workspace --all-targets -- -D warnings
     cargo test --workspace --locked
     cargo build --release --locked
     Add manual steps for Feishu/bridge behavior changes. -->
