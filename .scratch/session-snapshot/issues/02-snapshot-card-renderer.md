# 02 - Snapshot card renderer

Status: ready-for-agent
Type: task
Blocked by: 01

## What to build

The card builder for the Session Snapshot (ADR-0028), fed by ticket 01's data.
Static and one-shot by default; built so a later ticket (05) can drop a resolved
pending block by patching the card in place.

## Scope

- **Builder** (in `src/feishu/card.rs` or a sibling `snapshot_card.rs`): turns
  the snapshot payload into one interactive Feishu card:
  - Header: 已接管/已切换 verb + the session title.
  - Status line/chip with the ADR precedence 等待你的确认 > 运行中 > 需要重试 >
    空闲 (pending beats busy beats retry beats idle). A busy line carries one
    hint phrase. No status data → no chip line.
  - Pending Permission/Question sections: interactive, reusing the existing
    builders/elements from `src/bridge/request.rs` (`PermissionKind`/`QuestionKind`
    — Allow/Deny/Always, question options, the auto-accept toggle semantics) so a
    block looks and behaves like today's inline cards. Render zero-to-many.
  - 最近对话 collapsible panel: last-4 tail entries, role-marked, each previewed
    short and expandable to full text.
- **Layout that can patch**: keep each section individually constructible so a
  resolved claim can be removed and the card re-patched (used by ticket 05) and
  so a busy-follow (ticket 06) can stream into the same card — prefer reusing
  the existing accumulative render path where the payload allows.
- **Budgets**: stay under `MAX_CARD_COMPONENTS` / `MAX_CARD_JSON_CHARS`; sections
  collapse rather than truncate. Consider the tail-panel component cost.
- Pass the verb (接管/切换) in from the caller.

## Acceptance criteria

- [ ] Builder covers the matrix: idle / busy / retry / unknown-status, zero and
      several pending blocks, empty and full tail.
- [ ] Pending sections reuse the request.rs interactive components (not
      re-implemented buttons).
- [ ] No pending block for a different session id renders.
- [ ] Card JSON fits the Feishu limits for a worst-case (4-message tail + 2
      pending blocks) input.
- [ ] `cargo test --workspace --locked` green, clippy/fmt clean.
