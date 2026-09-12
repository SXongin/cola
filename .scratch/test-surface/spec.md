# Spec: test surface — 退役死掉的活体 E2E、给真 adapter 做 wire 测试、一条规则一个测试

Status: ready-for-agent

## Problem Statement

cola 的测试套件表面上很厚（513 个测试、约 1.9 万测试行，与生产代码行数相当），但测试面的分布是歪的：

- **唯一接触真实网络的代码几乎没有测试。** `feishu::Client`（796 行、15 处硬编码 URL、承担 token/鉴权/卡片/消息/图片全部 HTTP 交互）没有一条直接测试；所有集成测试跨的是进程内的 `Platform` trait seam，即用 `RecordingPlatform` 模拟平台、`MockBackend` 模拟后端。AGENTS.md 里记录的历史坑（Basic auth 需要 username、camelCase 字段、全局 permission 端点）恰好全部住在这块无测试的线缆层。
- **补这块空档的活体 E2E（第二个飞书机器人 + 测试群）已经腐坏。** 它 2026-08-06 创建、08-08 后再未维护，此后仓库走了 205 个 commit（其中 42 个改过 `test_support.rs`）；CI 从不运行它（`#[ignore]` + 需要外部凭据）；`cola-test.toml` 36 天没动过；断言只能看到飞书 API 返回的卡片 fallback，读不到卡片本体。它给套件制造了「有真实 E2E 覆盖」的假象。
- **测试接口浅、断言绑文案。** `test_support.rs` 把 harness 与 176 个测试（约 1.08 万行）混在一个文件；测试默认伸手进 `Mutex<Vec<PlatformCall>>` 原始调用日志，并用 235 处中文字符串 substring 断言渲染结果；160 处 setup 直接戳 `app.sessions.lock()` / `app.core`。改一句 UI 文案会红一片测试，找一个特性的上下文要跨上万行。
- **一条规则被实现四次、测试四次。** ADR-0007 的「话题内命令限制」在 `handle_command` 有一份集中 match，`/topic` 与 `/topic --adopt` 又各抄一份守卫（其中两份文案逐字重复），对应 4 个测试各钉一份拷贝。

## Solution

四个候选，分三个 PR 落地，行为零变化：

1. **C1 — 退役活体 E2E harness。** 删除 `LiveHarness` / `live_setup` / 两个 `#[ignore]` 测试 / 仅测试用的 `Client::send_text` / `cola-test.toml.example` 与 `.gitignore` 条目；把真实飞书验收写成 CONTRIBUTING「Releasing」小节的 6 项人工 smoke 清单（不需要第二个机器人，也不需要测试群——发布者在真实聊天里肉眼验收）。顺手修掉 `list_messages` 上「used by the live end-to-end harness」的过期注释（它是 `resolve_topic_anchor` 的生产依赖）。
2. **C2 — wire 测试 seam。** 给 `feishu::Client` 一个 `base_url` 字段（默认仍是 `https://open.feishu.cn`）和一个 `with_base_url` 构造器；新增共享的 `cfg(test)` 本地 fake HTTP server（路由表 + 请求日志），让真实 client 对着 `127.0.0.1` 跑。覆盖每个 public 方法的 happy path 与错误路径，同时断言「发出去的请求」与「解析回来的结果」。写 ADR-0031 记录该 seam（不 mock reqwest，trait seam 保留）。`opencode::Client` 复用同一套基础设施，作为紧随其后的独立 ticket。
3. **C3 — 一条规则一个 gate 一个测试。** 规则收进 `Command::topic_rejection(has_session) -> Option<&'static str>`：`/topic` 系列在任意话题都拒绝（防嵌套），`/dir`/`/switch`/`/new` 仅在已绑定会话的话题里拒绝；`handle_command` 顶部唯一调用。4 个守卫测试换 1 个表驱动集成测试；`/topic --adopt` 两份逐字重复的文案合并。
4. **C4 — 让集成测试可导航、少绑文案。** 先纯机械地把 `integration_tests` 按特性拆成 12 个 `cfg(test)` 模块（断言一字不改，单独 commit，测试名与数量保持）；再小步给 `RecordingPlatform` 加最小查询 helper、把卡片状态文案提为生产常量供测试断言、把直戳 store 的 setup 换成 `seed_session`。

## User Stories

1. 作为维护者，我想在 CI 里确定性地验证 `feishu::Client` 的 token 获取、缓存与过期，这样 401/鉴权回归不用等线上才发现。
2. 作为维护者，我想验证卡片 reply/send/update 的请求 URL、query 与 body JSON，这样卡片发错端点的历史坑不会复发。
3. 作为维护者，我想验证 `list_messages` 的 query 参数与响应解析，这样话题 anchor 解析（生产路径）有回归保护。
4. 作为维护者，我想验证 `get_message` 的 `raw_card_content` 请求与解析，这样 Quoted Context 的降级行为有据可依。
5. 作为维护者，我想验证 `download_image` 的字节与 content-type 处理，这样图片附件的失败路径有测试。
6. 作为维护者，我想验证非 JSON 错误页、HTTP 错误状态、业务错误码三类失败路径的错误信息与错误类型，这样故障诊断不会退化。
7. 作为维护者，我想让 fake server 同时记录请求日志并提供脚本化响应，这样每条 wire 测试既断言「发了什么」也断言「拿回什么」。
8. 作为未来实现者，我想 `with_base_url` 是生产可用的普通构造器（不是 cfg(test) 后门），这样测试跑的路径与生产完全一致。
9. 作为维护者，我想本地 fake server 住在共享测试模块里，这样 `opencode::Client` 的 wire 测试可以复用同一套基础设施。
10. 作为未来评审者，我想有一条 ADR 说明为什么 wire 测试用本地 fake server 而不是 mock reqwest，这样这个决策不会被重新翻案。
11. 作为发布者，我想 CONTRIBUTING 里有一份 6 项人工 smoke 清单（消息与卡片原位更新 / 权限按钮 / 问题按钮 / 建话题 / 模型选择卡 / 非 git 目录），这样每次发版 5 分钟就能完成真实飞书验收。
12. 作为发布者，我想 smoke 清单不依赖第二个机器人或测试群，这样发布不掉进凭据维护的坑。
13. 作为维护者，我想删掉两个从不在 CI 运行的 ignored 测试与整套 LiveHarness，这样套件不再宣称自己不拥有的覆盖。
14. 作为维护者，我想删掉仅测试用的 `Client::send_text` 和 `cola-test.toml.example`，这样生产代码与仓库根不再有测试残留。
15. 作为维护者，我想 `list_messages` 的文档注释描述它真实的生产用途，这样读者不会被「live harness」误导。
16. 作为未来贡献者，我想 bound-topic 规则只写在一个方法里，这样新增命令自动继承 gate 与测试。
17. 作为未来贡献者，我想拒绝原因文案仍然分选择类 / `/topic` / `/topic --adopt` 三种，这样用户仍能拿到「在主会话发什么」的引导。
18. 作为未来贡献者，我想 `/topic --adopt` 的两份重复文案合并，这样文案只维护一处。
19. 作为维护者，我想 176 个集成测试按特性拆进 `src/bridge/tests/` 的 12 个模块，这样找某个特性的测试不用滚过上萬行。
20. 作为维护者，我想机械搬迁单独成一个 commit 且断言零改动，这样 review 能区分「搬家错误」与「断言改造」。
21. 作为维护者，我想 `RecordingPlatform` 提供最小查询 helper（replied_cards / sent_cards / updated_cards / texts / button_values），这样新测试默认断言结构化 payload。
22. 作为维护者，我想卡片状态文案（等待授权 / 运行中 / 需要重试 / 空闲等）成为生产常量且被测试引用，这样改文案只改一处。
23. 作为维护者，我想长教程类文案最多只有一条断言，这样文案润色不再触发测试雪崩。
24. 作为维护者，我想迁移到手的模块用 `seed_session` 造状态而不是直戳 store，这样 setup 不再依赖内部数据形状。
25. 作为维护者，我想测试数量在机械搬迁前后逐一对应，这样搬迁不悄悄丢测试。
26. 作为未来实现者，我想 `opencode::Client` 的 wire 测试作为独立 ticket 复用 fake server，这样 C2 的首轮范围保持可评审。

## Implementation Decisions

### C1 — 退役活体 E2E

- 删除：`LiveHarness`、`live_setup`、`send_and_process`、`wait_for_card`、`live_e2e_real_bot_renders_expected_cards`、`live_e2e_question_card_is_delivered`、`Client::send_text`（`#[cfg(test)]`）、`cola-test.toml.example`、`.gitignore` 的 `cola-test.toml` 条目。保留 `SURVEY.md` / `test.log` 的 gitignore 条目（它们服务运行中的 bot 调试，不属 harness）。
- 保留：`list_messages` / `ChatMessage`（`resolve_topic_anchor` 生产依赖）、`get_message` / `FeishuMessage`（Quoted Context 生产依赖）。修正 `list_messages` 上过期的「live harness」注释。
- CONTRIBUTING「Releasing」小节增加 6 项 smoke 清单：① 真实聊天发消息看到 Done 卡片且卡片原位更新；② 权限按钮往返；③ 问题按钮往返；④ `/topic` 建话题且话题在群列表可见；⑤ `/model` 弹选择卡；⑥ 非 git 目录不报错。不记录 `/update`、`/restart`（发布流程另行验证）。
- 本机 `cola-test.toml` 是 gitignored 的本地文件，随实现删除。

### C2 — wire 测试 seam（feishu）

- `Client` 增加私有 `base_url` 字段，`new` 默认为 `https://open.feishu.cn`；新增 `pub fn with_base_url(cfg, url)`。所有现有 HTTP 调用改用该字段拼接；WS endpoint 获取同样走它（不额外测试）。
- 新增共享 `cfg(test)` 模块（`src/test_http.rs`）：极简 HTTP/1.1 JSON responder，随机会话绑本地端口；提供路由表（method + path 前缀 → 状态码 + body）与请求日志（method/path/query/headers/body）。不引入新的 dev-dependency。
- wire 测试内联在 `feishu::Client` 的 `cfg(test)` 模块，覆盖全部 public 方法：
  - `get_access_token`：成功、缓存命中（token 端点只被请求一次）、过期后重取、业务错误码。
  - `reply_card` / `send_card` / `update_message` / `reply_text` / `reply_in_thread` / `reply_card_in_thread` / `reply_completion_notice`：请求 URL/query/Bearer/body JSON 与解析结果。
  - `list_messages`：query 参数（`container_id_type` / `container_id` / `sort_type` / `page_size`）与 items 解析；**生产实现没有分页**（单页 50、无 page_token），不写不存在的分页测试。
  - `get_message`：`card_msg_content_type=raw_card_content` 与 mentions/content 解析。
  - `download_image`：字节与 content-type 透传、非 2xx 报错。
  - `user_name` / `chat_name` / `bot_open_id` / `get_ws_endpoint`：happy path 与错误降级。
  - 错误路径：非 JSON 错误页（HTML）、HTTP 5xx、业务错误码、token 解析失败。
- 断言双面：请求日志（发出去什么）+ 返回值解析（拿回什么）；不把卡片渲染内容复制成 golden（那已由 card.rs 与集成测试覆盖）。
- ADR-0031：wire 测试用本地 fake server + base_url 注入；不引入可 mock 的 reqwest 层；`Backend` / `Platform` trait seam 保持不变（与 ADR-0010 一致）。

### C3 — topic 规则收拢

- `Command::topic_rejection(&self, has_session: bool) -> Option<&'static str>`：
  - `Topic` / `TopicAdopt` / `TopicAdoptCard`：任何话题都返回原因（与 `has_session` 无关）。
  - `Dir` / `DirCard` / `Switch` / `New`：仅 `has_session == true` 时返回原因。
  - 其余命令返回 `None`。
- `handle_command` 顶部唯一调用点：命中则 reply 原因并返回；删除 `command.rs` 中其余三处手写守卫。
- 文案：选择类一条「回主对话操作」；`/topic` 一条；`/topic --adopt` 与 `/topic --adopt` 卡片两条合并为一条。
- 测试：删除 `test_support.rs` 的 4 个专用拒绝测试，换 1 个表驱动集成测试，遍历被禁命令集合（含已绑定与未绑定两种话题状态）与允许命令集合，断言每个被禁命令恰好得到一条拒绝回复。

### C4 — 测试面整理

- 布局：`src/bridge/tests/` 下的 12 个 `cfg(test)` 特性模块（prompt-render / permission / external / subtask / topic / dir / session-routing / question / switch / snapshot-follow / config-commands / misc），harness（MockBackend / RecordingPlatform / build_app / incoming / seed_session 等）留在 `test_support.rs`；删除嵌套的 `integration_tests` 模块。
- 第一步机械搬迁：断言零改动，测试名与数量在搬迁前后一一对应；独立 commit。
- 第二步最小断言层：`RecordingPlatform` 增加 `replied_cards()` / `sent_cards()` / `updated_cards()` / `texts()` / `button_values()`；卡片状态文案提为 `card.rs` 的 `pub(crate) const`，测试断同一常量；教程类长文案在测试中最多一条断言。逐步迁移，不搞大爆炸重写。
- setup：迁移到手的模块把 `app.sessions.lock()` 直戳替换为 `seed_session`。

### 交付次序

- PR 1：C1 + C2（删与补是同一件事）。
- PR 2：C3。
- PR 3：C4（机械搬迁 + 断言改造在同一 PR，分 commit）。
- 后续 ticket：`opencode::Client` 的 wire 测试，复用 `test_http`。

## Testing Decisions

- **只测外部行为**：wire 测试的接口是 HTTP——断言请求（method/path/query/Bearer/body）与解析结果，不触碰 client 私有字段或内部函数。
- **合成 seam 的选择**：优先使用已有 seam（`Platform` / `Backend` trait）；C2 新增的是「HTTP 端点」这条真实 seam（生产真远端 + 测试本地 fake 两个 adapter），不是 mock 框架层。
- **被测试模块**：`feishu::Client`（新增 wire 测试）；`Command::topic_rejection`（表驱动集成测试）；C4 是测试组织重构，不改变被测行为。
- **测试先例**：`ws.rs` 的 frame 编解码 round-trip、`card.rs` 的卡片 JSON 结构断言、`test_support.rs` 的 RecordingPlatform 集成测试。
- **机械搬迁的验证**：搬迁前后 `cargo test -- --list` 的测试名集合一致（扣除 C1 删除项）；每步维持 fmt / clippy / test / release build 全绿。
- **人工验收**：CONTRIBUTING 的 6 项 smoke 清单在每个 release tag 前执行；它是唯一验证「真实飞书接受卡片 schema」的手段，无法用 fake 替代。

## Out of Scope

- `opencode::Client` 的 wire 测试（独立后续 ticket，复用同一 fake server）。
- 真实飞书 schema 的自动化验收（保留为发版人工 smoke）。
- 可 mock 的 reqwest 层（ADR-0010 明确拒绝）。
- 任何新命令、新卡片、新行为；本 spec 是纯测试面与规则归属重构，行为零变化。
- `SURVEY.md` / `test.log` 的 gitignore 条目与运行期调试流程。

## Further Notes

- 两个已核实、写进 spec 的事实校正：`list_messages` 是生产路径（`resolve_topic_anchor`），不是 live harness 专用；它没有分页逻辑。
- 运行一次 `cargo-mutants` 作为一次性审计实验（不常驻 CI）可以在清理后客观回答「哪些测试从没抓到过 bug」，本 spec 不含该工作。
- 若未来需要再次引入「真机器人 + 群」的自动化，应先写 spec 说明它相对本地 fake server + smoke 清单的增量价值，而不是复活 ignored 测试。
