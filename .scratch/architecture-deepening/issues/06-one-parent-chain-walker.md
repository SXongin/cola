# 06 — One parent-chain walker

**What to build:** Replace the four parent-chain walks (card-target resolution,
inline-host resolution, auto-accept flag walk, descendant check) with one walker
taking a stop predicate. The 8-hop cap and "stop at self-parent" policy live in
exactly one place.

**Blocked by:** 05 — Fuse Permission and Question into one RequestFlow.

**Status:** resolved

- [x] All four walk sites call the one helper with their own predicate.
- [x] The hop-cap policy appears exactly once in the codebase.
- [x] Walker covered by unit tests over scripted session info (parent chains,
      self-parents, cap exhaustion).
- [x] Full verification loop green.

## Answer

`walk_parent_chain(core, start, directory, predicate)` now lives in
`pollers.rs` — the single owner of the 8-hop cap and the "stop at self-parent"
policy. It calls `predicate` on each visited session id (start first) and hops
up via `session_info` (through the `DirectoryBackend` handle, ADR-0010) until
the predicate returns `Some`, the chain self-loops, or 8 hops are exhausted.
`directory: None` restricts the walk to the starting session (no instance
handle to walk through).

The four call sites are now thin predicates:
- `resolve_card_target` — first session with a live accumulator's reply-to, or
  a store-mapped chat/topic anchor.
- `inline_host_session` — first session with a live accumulator.
- `should_auto_accept` — first store entry (its `auto_accept` flag).
- `session_descends_from` — first session equal to `root`.

Five new unit tests cover the walker over scripted `MockBackend` parent chains:
start-match without a hop, one-hop match, self-parent stop, 8-hop cap, and
no-directory restriction. 256 tests total pass.

Verification: fmt clean, clippy `-D warnings` clean, 256 tests pass, release
build succeeds.