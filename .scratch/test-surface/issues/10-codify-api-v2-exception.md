# 10: 在 CODING_STANDARDS 里写明 `/api/session` 的 v2 例外

**What to build:** 精确化 `CODING_STANDARDS.md` 的「OpenCode's API paths have no `/api` prefix」规则，让 `POST /api/session`（v2 创建会话）不再被 code review 误判为违规，同时保留对 prompt / permission / question 等 canonical 路径的要求。

**Source:** ticket 07 合并后 `/code-review` 的 Standards 轴 finding（2026-09-12，标为 hard violation）。

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

依据（2026-09-12 已核实）：

- `src/opencode/client.rs` 的 `create_session` 调 `POST /api/session`，解析 `{data: Session}` envelope——这是 OpenCode 的 v2 API。
- 走 v2 的原因：cola 的 `CreateSessionInput.location.directory` 只在 v2 的 `SessionsCreateInput` 里；canonical v1 `POST /session` 的 `Session.CreateInput`（`/root/workspace/dev/opencode/packages/opencode/src/session/session.ts`）没有 `location` 字段，无法把会话建在映射目录。
- `/api` 仅用于 session create；prompt / permission / question 等仍走无前缀 canonical 路径。
- ticket 07 的 wire test 已把该请求钉住（`src/opencode/client.rs::wire_tests::create_session_posts_the_input_and_parses_the_data_envelope`）。

- [ ] 更新 `CODING_STANDARDS.md`：明确「无 `/api` 前缀」针对哪些端点，并列出 `POST /api/session`（携带 `location`）的 v2 例外及原因。
- [ ] 如维护者认为该选择难以逆转或易被重新翻案，可加一条 ADR；否则标准里的说明即可。
- [ ] 验证循环全绿。

## Comments

- 2026-09-12：由 ticket 07 会话整理成 ticket（用户确认）。
