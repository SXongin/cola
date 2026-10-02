# cola

A bridge bot that brings the OpenCode AI coding experience into Feishu, with clean platform and backend boundaries.

## Language

**Bridge**:
The core orchestrator that routes messages between platform adapters and AI backends.
_Avoid_: Proxy, middleware, gateway

**Platform**:
A messaging platform integration (e.g. Feishu). Handles message receive, card rendering, and platform-specific UX.
_Avoid_: Client, frontend, channel

**Backend**:
An AI code agent provider (e.g. OpenCode). Handles session management, prompt execution, and event streaming.
_Avoid_: Engine, model, provider

**Generation**:
An OpenCode protocol era cola can speak: **V1** is the 1.18.x unprefixed compatibility surface; **V2** is the 2.0.x `/api`-only surface. The numbers name OpenCode releases, not protocol versions (ADR-0055).
_Avoid_: version, API version, protocol v1/v2 (the numbers are OpenCode releases, not protocol versions)

**Attached Generation**:
The **Generation** of the OpenCode server cola is currently attached to. A property of the attachment, not of a **Session**: switching generations is a reattach (which server is up), never a per-session route (ADR-0055).
_Avoid_: server version, active generation

**Generation Strategy**:
The adapter-internal module behind the one `OpenCodeBackend` adapter that owns one **Generation**'s paths, payloads, decoders and semantics; deleting it is the retirement act (ADR-0055).
_Avoid_: adapter (the adapter is the one `OpenCodeBackend` around it), driver

**Generation Override**:
The `[opencode] generation` setting forcing the **Attached Generation** when the probe is absent or contradicted; a contradicting probe still logs its evidence (ADR-0055).
_Avoid_: force mode, protocol pin

**Host** (机主):
The person who runs cola on their own machine and owns what it operates on — the OpenCode server, the Shared Store, and the filesystem. In the personal-machine model the Host is the single admitted Principal and the only one permitted to act at all.
_Avoid_: Operator (ambiguous — code and docs use it for whoever drives a session), owner (in code that word means the Chat/Topic a Session is mapped to, ADR-0007)

**Principal**:
The identity cola resolves for an inbound action — a message or a card click — so authorization can be decided. A Feishu user, keyed by app-scoped `open_id` (the same field on message events and card callbacks). Distinct from ownership: a Principal acts, a Chat/Topic maps Sessions.
_Avoid_: Actor, account, role (a role is a capability set assigned to a Principal, not the identity)

**Access List** (访问名单):
cola's persisted, machine-local record of admitted Principals, consulted for every inbound message and card action before cola acts. In the personal-machine model it holds exactly one entry, the Host. Distinct from a Feishu chat's member list, which says who can read messages, not who may operate the machine.
_Avoid_: Allowlist, whitelist (both imply names without roles), ACL (implementation jargon)

**Claim** (认领):
The one-time act that names the Host on an unclaimed cola: the p2p sender of `/claim <code>` — where the code is printed at startup and never persisted — becomes the Host and is written to the Access List. Until claimed, every other action is refused; a successful Claim survives upgrades.
_Avoid_: Setup, login, pairing

**Shared Store**:
The default OpenCode data directory (`~/.local/share/opencode`; `$XDG_DATA_HOME` when set) that every client — cola, OpenChamber, the CLI — reads and writes. The single source of truth for sessions; cola's "one server" invariant is about who serves this store.
_Avoid_: Database, data dir, state directory

**Owned Server**:
An `opencode serve` process that cola itself spawned (pid recorded in `~/.cola/self-opencode.pid`). Only an Owned Server may be killed, restarted, or reaped by cola; everything else is someone else's process. Its protocol generation is probed like any other attachment's — the `opencode` binary on PATH may be either lineage (spec #364 §2).
_Avoid_: Managed server, our server, private server

**Coexistent Server**:
A default-store `opencode serve` started by someone else — OpenChamber's managed server or a manual launch. cola attaches to it and serves through it, but never kills or restarts it.
_Avoid_: Foreign server, external server, shared server

**Yield**:
The act of terminating an Owned Server and re-attaching to a Coexistent Server, so the Shared Store returns to exactly one server. Deferred while a session is in flight (a mid-stream generation must not be truncated).
_Avoid_: Give way, step down, hand over

**Lazy Start**:
Spawning an Owned Server only at the moment a server is actually needed (a prompt is about to be sent and no server exists), never proactively at boot. Boot-time behavior is attach-only unless `start_server = "eager"`.
_Avoid_: On-demand start, deferred start

**Lazy Session Creation**:
Creating a Session only when a prompt is actually sent — never when the conversation's intent is declared. `/new`, `/dir` and `/topic` write a **Pending Session**; the first prompt materialises it. The session-side counterpart of **Lazy Start**.
_Avoid_: Cold start (means the opposite), deferred creation, eager creation

**Pending Session**:
A conversation's recorded intent for its next Session — directory, optional title, and per-session overrides — with no Backend identity yet. NOT a Session: it has no server id, and while it exists the conversation has no **Active Session**. The first prompt materialises it into a Session and the Pending Session ceases to exist; a later selection command replaces it. Written by `/new`, `/dir`, `/topic` and their card forms.
_Avoid_: Draft session, session intent, tentative session

**Session**:
A single conversation thread with an AI backend, identified by the server's session id and `title` (the server is the single source of truth for identity, ADR-0007). A session has a directory (project) and an optional agent selection. One session maps to at most one Feishu thread at a time. A session created by a `task`/`subagent` tool call is a child session: parented to the session the call ran in, it stays absent from the Session Mapping (and has no Active Session) until it is explicitly taken over with `/sub attach`, after which it is mapped to its Chat/Topic like any other Session — the parent stays mapped alongside it. Until then its Permissions/Questions re-home to the parent's card, `/sub` is its read-only discovery surface, and no message is ever injected into it. The `task`/`subagent` call is a Tool Panel, not the session — one child session can be driven by several such calls over time (a resume passes the prior child session id).
_Avoid_: Chat, conversation, room
_UI label_: 会话 — the ONLY meaning of 「会话」 in cola's UI; the Feishu side is never called 会话 (see Chat/Topic UI labels). Resolves the old "why are so many sessions 本会话" ambiguity: cola marks only the Active Session and uses 聊天/话题 for the Feishu side.

**Chat**:
The Feishu top-level conversation (a group or p2p), identified by `chat_id` — the lobby of ADR-0007. A chat contains many Topics and may hold several Sessions directly (the lobby). Distinct from a Topic: a chat has no `thread_id`.
_Avoid_: Conversation, room, group
_UI label_: 聊天 (the Feishu-side container; a Feishu user's own term). Feishu's own UI calls the top-level thing a 会话, which is exactly the collision cola avoids — cola never uses 会话 for this.

**Topic**:
A Feishu thread inside a Chat, identified by `thread_id` (`omt_...`; called "话题" in the Feishu UI). A message is a topic message IFF it carries `thread_id`. A topic holds exactly one Session (or one Pending Session before that Session exists); the boundary that isolates one session from another. A topic is created around a message (its Topic Root) and is usually opened by cola with a seed card (Topic Anchor).
_UI label_: 话题.

**Topic Root**:
The message a Feishu topic is created around. For a cola-created topic (`/topic`, `/topic --adopt`) this is the bot's Topic Cover Card, sent to the main Chat at creation and then replied-in-thread; for a manually-created topic it is the user's message the topic was made on. Persisted as `topic_root`. Distinct from Topic Anchor: the Root is the thread's first message (outside/at the boundary of the topic), the Anchor is the first reply inside it (bot-typed). Never injected as prompt context (ADR-0023).
_Avoid_: Seed, anchor, topic message

**Topic Cover Card**:
The bot's card that is the Topic Root of a cola-created topic. Sent to the main Chat at creation so the chat-list topic entry shows the session brief (title, session id, project, branch, directory, agent, model) permanently, then replied-in-thread to open the topic. Being an interactive card, cola patches it in place when the session title changes — auto-generated after the first exchange (mid-turn, with a post-turn retry ladder) or set by `/name` (ADR-0023). When the card cannot be sent, the topic falls back to anchoring on the user's `/topic` command message instead.
_Avoid_: Cover message, topic stub, seed card

**Topic Anchor**:
The bot's confirmation card inside a topic, placed by `/topic` / `/topic --adopt` (the seed card) or by `/switch` / `/attach` inside an existing topic. Doubles as the reply target that keeps permission/question/external cards inside the topic (the create API rejects a `thread_id` as target). Persisted as `topic_anchor`. Distinct from Topic Root: the Anchor is inside the topic, the Root is the message the thread is built around. Never injected as prompt context (ADR-0023).
_Avoid_: Root, seed message, confirmation card

**Turn**:
A single user→assistant exchange inside a Session (one prompt plus its streamed card response) — spanning every **Execution** that belongs to it, including ones a **Wake** opened after the Backend went idle. cola's internal vocabulary is the English word "turn" (Turn Footer, ADR-0019); there is deliberately NO user-facing Chinese noun for it — the UI never labels individual turns. If one is ever needed, use 轮次/本轮. A Turn is not complete while the **Background Tasks** it left running are still live: its card yields 等待后台任务 and the next shell/subagent completion **Wake** resumes it in place (ADR-0066) — a Wake after an ending, a restart/interrupt Wake or a Wake-less late tail still continues the chain on a new card (ADR-0059). The Session's live tasks stay visible as the **Background Task Ledger** (ADR-0060).
_Avoid_: 对话 as a user-facing term for this (overloads "conversation"); 消息 (a single message, not a full exchange).

**Execution** (执行):
The Backend's own unit of running work: one busy period of a Session, opened by an admitted input after idle and closed by the Backend's next idle boundary (ADR-0059). cola reads Executions; it never owns them — a **Turn** may span several, and a **Wake** opens one with no user message.
_Avoid_: run (cola's render/follow loops are also "runs"), turn (the card-level unit)

**Retry** (重试):
The action an Error **Card** offers for a failed **Turn**: run that turn's question again on the same **Session**. A Retry never oversubmits — if the run is still alive (cola may merely have lost sight of it), the card is re-attached to it instead of asking again — and it never overwrites the failed attempt: the failed card is marked 「↩️ 已重试」 and the new attempt renders on a new card below it, so the failure stays readable and a stale click cannot retry a newer turn (ADR-0058). A deliberate stop is not a failure and offers no Retry.
_Avoid_: Resend, re-ask (both imply an unconditional second submission)

**Supplement** (补充消息):
A user message sent while cola itself owns its **Session**'s live **Execution** — the routing key is cola's own live ownership of the Session's card chain, not the Backend's run status, which can report live with no runner left to pick the message up (ADR-0062). cola does not start a competing Turn: it submits the message to the Backend, which merges it into the live Execution when one is still running, or starts a new Execution when it has already ended. Either way the message lands below the live card, so it splits the **Card Chain** — the continuation card is its reply and carries a receipt line; there is no separate text acknowledgement. A message whose Session only *reads* live while cola owns no card chain is not a Supplement either: it starts a new Turn, so the message always has a card that owns and settles it (the Backend still merges it when the run is genuinely live). A message arriving while the session is idle — including a **Turn** waiting on **Background Tasks** — is not a Supplement: it starts a new Turn (ADR-0059). A command reply is NOT a Supplement either: cola deliberately leaves it as the newest message — unless the user runs `/card`, which explicitly pulls the live card back down.
_Avoid_: Follow-up, addition, queued message

**Unreceived Message** (未被接收的消息):
A user message a **Turn** submitted but the Session never promoted into the Session Transcript — the Backend accepted it durably, yet no runner merged it, so nobody will answer it. The Turn's card ends 「⚠️ 这条消息未被接收」 and offers 「重新发起」 (interrupt, then resume, so the queued message is promoted when the new run starts); it never settles ✅ (ADR-0062). A card a cola restart reaped into this ending (ADR-0063) shows the same copy without the action — the click's fixture died with the process — so the user re-sends instead.
_Avoid_: Failed message, lost message (the submit succeeded — no run ever received it)

**Background Task** (后台任务):
Work the agent left running in the background — a backgrounded shell command, a subagent — that will **Wake** the Session when it finishes. Durable and pollable: the tool part records it as a completed call whose run is still running, and its Wake retires it (ADR-0059). The launch record never flips, so on the reads that already drive the ledger Session Sync also corroborates each task against the server's runtime registries — `GET /api/shell` (a retained terminal record or a 404 ends the task) and `GET /api/session/active` (an inactive subagent child marks its row unconfirmed, nothing more) — because a completion record the dying process never wrote would otherwise leave the task live forever (#454, ADR-0065). It belongs to the Session, not to the Turn that started it: while one is live its Turn is not complete, and its completion resumes the yielded card in place — the fixed entry and the resumed work both stay on the card the task lived on (the **Background Task Ledger**, ADR-0066) — unless the user moved on or the card already ended, when the newest chain continues with a continuation card (ADR-0059).
_Avoid_: Job, task (the task tool call is the call, not the work), pending work

**Background Task Ledger** (后台任务账本):
The card-tail section that lists a **Session**'s live **Background Tasks** — task type, bolded label (the command or description), start clock, and each row's liveness: a shell row's elapsed, a subagent's child activity — riding the newest card of its **Card Chain** like the **Todo Panel** and live **Tool Panel**s (ADR-0045/0060). It renders as one folded-by-default collapsible panel whose title is the pinned count (`⏳ 后台任务（N）`, so the folded panel still says how many are running; a task the runtime reconciliation could not confirm adds its own count, `⏳ 后台任务（2 · 1 待确认）`) and whose body is one row per task (`· shell：**npm run build** · 14:02 · 3m12s` / `· subagent：**review the diff** · 14:04 · shell 26s`): the type word (the tool's own name) plain, the label bolded — its own `&`/`*`/`_` neutered (`&` first) so they cannot break the bold span — the task's server `started_at` as local `HH:MM`, then — for a `shell` row — the bare elapsed (its only liveness); a background `subagent` row renders its child's live activity in the live **Tool Panel**'s own vocabulary and format instead (`· shell 26s`, `· 思考中 30s · 等待你的授权`; ADR-0054) and never a total elapsed, which the fragment's age would only restate — a fragment a row the reconciliation could not confirm does not show — then `· ⚠️ 状态待确认` when a reconciliation read could not confirm the row as running; no label or no start omits its part whole (`· shell · 14:02 · 0m05s`, `· shell：**npm run build**`, `· shell`). The subagent's activity is gathered on the reads that already drive the ledger — one transcript light read per distinct child plus its pending-wait query, the same batch the live task panel uses — and a child read that fails keeps the last successful fragment, its age still counting from the stored start rather than freezing or inventing one. Identity stays out of the live row (the completion entry's fold body keeps it), and a stable panel id keeps the reader's fold state across re-renders. The live list exists on exactly one card: a shell/subagent completion **Wake** resumes the yielded card in place, so the list and the entries stay on the card the tasks lived on (ADR-0066); the handover to a newer card runs only when the chain actually moves — a new **Turn** taking over, a size overflow, a **Supplement**, or the continuation a restart/interrupt Wake or a late tail opens. A completed task leaves the list and becomes a fixed collapsible entry on the card it lived on — the mechanical completion line is its collapsed title, naming what the task's ending actually was: `🔔 shell 完成` / `🔔 subagent 完成` for a Wake that reported completion, `已取消` / `失败` for the states the Wake itself reported, `结束` for a runtime-confirmed end whose Wake never came, `已失联` for a record the runtime no longer knows (#454, ADR-0065), all with `：<命令>` / `：<描述>` when named and bare otherwise — and identity and timing are the fold's body (no clock for a lost record) — so the ledger reads as history rather than noise, and a Wake continuation card carries only its 承接 line and the remaining live list. Made possible by the one carve-out of a yielded card's freeze: the ledger (and a quiet true end, see **Waiting on Background Work**) is the only content a card that stopped updating may still receive (ADR-0060) — its rows' rendered numbers (a shell row's elapsed, a subagent fragment's age) advancing at second granularity on the same existing reads, while a live card's ledger refreshes on whole minutes. Distinct from the **Todo Panel**, which is the plan's checklist, and from a **Tool Panel**, which records the call.
_Avoid_: Task list, job queue, progress panel

**Wake** (唤醒):
A Backend-generated message (V2 `synthetic`) that resumes a Session with no user message: a **Background Task** finishing, a subagent completing, the server continuing after an interruption or restart. cola renders a shell/subagent completion Wake by resuming the yielded card in place (ADR-0066) — the completion entry, the resumed work and the eventual ✅ on the same card, no new card and no 承接 line — while a Wake with no yielded card to resume (a restart/interrupt Wake, one arriving after an ending, or a Wake-less late tail) keeps the **Card Chain** continuation: a new card below the user's message, which is also the notification. Either way it renders only while the Session is the thread's **Active Session** and its newest user message is a **Cola-Authored Message** (ADR-0059). A Wake that lands while the Session is not active renders nothing during the absence; when the Session becomes active again, Session Sync's Wake pass renders it on the newest chain as a continuation card, exactly once (later passes must not re-post it). A shell or subagent completion leaves one mechanical **Background Task Ledger** entry at the Wake's own time (`🔔 shell 完成：<命令>` / `🔔 subagent 完成：<描述>` as its collapsed title, clipped to a short line and bare when the Wake names no label), so the completion never depends on the model narrating it; a Wake that opened a continuation card is announced by its 承接 line instead, and each Wake is marked once.
_Avoid_: Notification, callback (both mean other things here), resume

**Thread** (legacy name for Topic):
A Feishu topic, identified by `thread_id` (`omt_...`). Retained in code as `ThreadKey { chat_id, thread_id }`; the glossary now calls the Feishu side Chat/Topic, and 话题 for topics in the UI. See Topic.

**Active Session**:
The single session of a chat/topic that messages route to and that Session Sync follows. At most one per ThreadKey at a time (the SessionStore's first entry); `/switch` promotes a session to active, and so does a **Pending Session**'s materialisation. A conversation with a Pending Session has NO Active Session until that first prompt materialises it.
_Avoid_: Current session, latest session, selected session

**Session Mapping**:
Cola's record of which Sessions a Chat or Topic has activated, and which one is its Active Session. Established when a Session is opened or adopted for a conversation and remembered across restarts; distinct from the Session itself, whose identity lives on the Backend (ADR-0007), and from the server's session list, which is Backend state rather than cola's. A Pending Session is stored beside the mapping, not in it — it is not a Session.
_Avoid_: Session list, session store, mapping table

**Session Transcript**:
The normalized read of one Session's messages and parts, produced by the Backend and consumed by the Bridge and the Platform: message identity, role, server time, model identity, token usage and a message's recorded failure are typed, while tool payloads stay opaque content. One Transcript serves rendering, Session Sync and the Session Snapshot, and a message's identity and its server time travel together, so a Turn's anchor is one fact rather than two independently derived ones. An Execution's idle boundary and a Wake are typed facts here too, so the Bridge never re-derives them from timestamps (ADR-0059). A Turn's failure is read from its assistant messages here (the Backend records it on the message), so completion observation is one read.
_Avoid_: Message list, history, message log

**Cola-Authored Message**:
A user message cola itself submitted to the Backend on behalf of a Feishu Chat/Topic, as opposed to an External Message. Self-identifying: cola chooses the message's id (`msg_cola_…`) at send time and the Backend persists that id, so authorship survives a server crash/replacement and even a cola restart without any cola-side ledger. Recognised by the `msg_cola_` id prefix.
_Avoid_: Outbound message, own prompt (a prompt is the send action, not the stored message)

**External Message**:
A user message in a Session that cola did NOT author — someone posted it from another Shared Store client (OpenChamber, the CLI). Surfaced to Feishu by **Session Sync**, which follows only the Active Session (ADR-0017). The opposite of a Cola-Authored Message.
_Avoid_: Foreign message, out-of-band message

**Sync Watermark**:
Session Sync's per-session record of the newest user message it has already accounted for: anything newer that is not a Cola-Authored Message is an External Message and triggers a notification. Advances past both cola-authored and external messages; cleared when a session stops being the Active Session so a later `/switch` back re-baselines silently. Owned by Session Sync alone — the prompt path no longer records it (formerly called the "baseline"). A **Wake** never moves it: it accounts user messages only.
_Avoid_: Baseline (the old name; it implied the prompt path owned it)

**Wake Watermark**:
The durable per-session record of the newest **Wake** whose completion a card
has already announced — its identity and server time — advanced only after the
card write that carried the announcement succeeds. **Session Sync**'s
post-restart continuation reads it: a Wake at or below the watermark is never
re-announced, only a strictly newer one continues (with the content probe still
required). Distinct from the **Sync Watermark**, which accounts user messages,
lives in memory, and is never moved by a Wake (ADR-0061).
_Avoid_: Sync Watermark (the user-message-scoped one), announced set (the
in-memory per-chain predecessor)

**Session Sync** (会话同步):
The Bridge flow that keeps a thread's **Active Session**'s card chain current without a user message: it notifies **External Messages**, renders **Wakes** as continuation cards, and catches content its card missed (ADR-0059). The successor of the external-message sync; still scoped to the thread's active Session (ADR-0017).
_Avoid_: External poller (the old name), background watcher

**Project**:
A working directory on the filesystem where OpenCode operates. A property of a session, not of the bot. A conversation's current project is the directory of its Pending Session when it has one, otherwise of its active session (derived, never stored separately); `/new` and the bare `/topic` form inherit it and fall back to the default directory only when the conversation has neither. Sessions created outside a conversation still carry their own directory.
_Avoid_: Workspace, repo

**Recent Directories** (「最近目录」):
Directories of the most recently active sessions in the Shared Store, deduplicated by directory and sorted by last activity, unioned with the directories cola has mapped (most recently mapped first) and the conversation's current directory. The union is what keeps a directory on the card after the server drops its last session — deletion or archival — and after a Pending Session declares one no server session exists for yet (ADR-0046). A bare `/dir` (no argument) presents them as a picker card whose rows offer one-tap re-rooting into the current conversation or opening a new Topic for that directory (ADR-0025); when there are more directories than one page holds, a keyword search over the paths narrows them, and the rows paginate six per page with the active keyword and page surviving every rebuild (ADR-0051, ADR-0052).
_Avoid_: Recently opened folders, folder history, recent projects

**Variant**:
A model-declared reasoning-effort setting (e.g. "low"/"high"/"minimal"), selectable per session via the `/think` command. Each model declares its own variant set — there is no universal scale across models — and "unset" means the server's default for that model. Selecting a model that doesn't declare the current session's variant clears it. How it reaches the backend is generation-specific: V1 sends `PromptInput.variant` per prompt, while V2 carries it inside the session's durable model ref (a session switch, not a prompt field). The user-facing label on cards is "思考等级"/thinking; "variant" is the backend protocol term, never a command name.
_Avoid_: Thinking level as the domain term (it's the presentation label); calling the command `/variant`

**Permission**:
A request from the AI backend to perform an action on a resource. Presented to the user as an interactive card with Allow/Deny/Always options.
_Avoid_: Approval, authorization, consent

**Auto-Accept**:
cola's per-session blanket flag (`auto_accept` in the SessionStore, `/autoaccept`). When on, cola answers EVERY pending permission for that session with "once" automatically — no permission card is surfaced at all. Lives in cola's store and persists. Distinct from the backend's per-type "Always" rule: Auto-Accept is session-wide (all permission types) and cola-side, while "Always" is scoped to one permission type, lives on the backend instance, and makes the backend skip the ask entirely.
_Avoid_: Auto-approve mode, always-allow (that is the backend's per-type rule, not this)

**Permission Toggle**:
The "开启自动授权" control on a permission card (inline or standalone). One click turns on the session's Auto-Accept and simultaneously approves the current pending permission, without producing a new message. Turning it off is still done via `/autoaccept off`.
_Avoid_: Auto-authorize button, approve-all switch

**Question**:
A structured multi-choice prompt from the AI backend, distinct from
permissions. User selects options to reply.
_Avoid_: Poll, survey, prompt

**Custom Answer** (自定义答案):
A user-typed entry in a multi-select **Question**'s selection, standing
alongside the backend's options: kept verbatim (never split or trimmed) and
individually removable. Distinct from an option, which the backend supplied.
_Avoid_: Free-text answer, custom option, note

**Session Snapshot** (会话快照):
The one read-only card a Chat/Topic receives when it activates a Session it was not already following — a first adoption, `/attach`, `/topic --adopt`, or the re-activation of a mapped Session — replacing the bare adoption confirmation. It answers what the operator cannot know at takeover: whether the last Turn ended (status line 等待你的确认 / 运行中 / 需要重试 / 空闲), what is blocked on the user (adopt-time Permissions and Questions, actionable inline unless already surfaced on another card — then the status line names the wait and points at that card), and what was recently said (最近对话 tail). Purely a read: it never prompts the Backend, writes to the Session, or disturbs a running Turn. Suppressed when re-activating a Session whose recent life is already fully visible in this Chat/Topic (the switch then confirms in one text line). An adopted-busy Session's in-flight external Turn is followed into the card until completion; every other snapshot is one-shot.
_Avoid_: Briefing, takeover summary, handoff card

**Card**:
A Feishu interactive message card. It evolves through live states (loading → reasoning → streaming, including a running-tool phase) and ends in one of five terminals — 「✅ 完成」, 「❌ 出错」, 「⏹ 已停止」 for a deliberate stop, 「↩️ 已重试」 once its retry was submitted, or 「⚠️ 这条消息未被接收」 when its Turn's message never reached the Session Transcript (ADR-0062) — or, while its Turn's **Background Tasks** are still live, yields with 「⏳ 等待后台任务」 (not a terminal: the next shell/subagent completion **Wake** resumes it in place as 「🔄 后台任务完成，继续处理中…」, ADR-0066); such a waiting card is later **collected** as a terminal of its own — 「⏳ 部分完成 · 已由新消息接管」 when a new Turn supersedes it, 「⏳ 已切换会话 · 后台任务仍在运行」 when its Session stops being the thread's Active Session — and a card a cola restart orphaned is **reaped** by Session Sync into its true ending (✅ / ❌ / 等待后台任务 / 未被接收) or collected as 「⏳ 已由新卡片接管 · 已停止更新」 when a continuation takes the chain over — while its Session still reads live, that orphan is stamped once per cola life with 「⏳ 已重启，等待运行结束」 until the same settle decides it (ADR-0063); a filled card hands over to its chain's continuation with a 「⏳ 部分完成，继续中…」 pause. A yielded card still accepts exactly two updates while it waits: its **Background Task Ledger**, and a quiet true-end settlement (ADR-0060); the resumption a completion Wake brings is the third, and only once the wait is over (ADR-0066). It uses collapsible panels for secondary content and shows progress in its header (phase timer, silence, reasoning length) so a slow turn is distinguishable from a dead one — including a "等待你的授权"/"等待你的回答" state that names whichever pending request blocks the turn (both at once reads "等待你的授权/回答").
_Avoid_: Widget, component, bubble

**Card Chain**:
The one or more **Card**s a single **Turn** renders into when its content exceeds what one Feishu card may hold, when a **Supplement** lands below the live card and the chain must continue there to stay the newest message, when a **Wake** cannot resume the Turn in place (a restart/interrupt Wake, a Wake-less late tail, or a card already at an ending — a shell/subagent completion resumes the yielded card in place instead, ADR-0066), or when the user explicitly pulls the live card down with `/card`: the filled card is finalized with a 部分完成，继续中 header and a continuation card takes over, replied to the user message it continues from. Only the newest card of the chain keeps receiving updates; an **Interaction Block** rides that newest card; a command reply does not split the chain by itself — `/card` is the user-invoked exception.
_Avoid_: Split card, multi-card turn, card pagination

**Pending Card Update**:
A **Card** state cola has produced but not yet delivered to the platform. At most one per card — the newest; an older pending update is always superseded, never replayed ("newest wins"). cola retries it until it lands, is replaced by a newer state, or is refused permanently: a recoverable failure never abandons it. A pending update does not survive a cola restart — a card orphaned that way is reconciled by **Session Sync**'s reap instead.
_Avoid_: Dirty (this glossary's Dirty is a git working tree), outbox (implies replaying queued states in order), retry queue

**Waiting on Background Work** (等待后台任务):
The disposition a **Card** takes when its **Execution** ended but the **Turn**'s **Background Tasks** are still live: the card stops updating while it waits (its **Background Task Ledger** and a quiet true end are the only updates it accepts, ADR-0060), is neither a terminal nor ✅, and the next shell/subagent completion **Wake** resumes it in place as 「🔄 后台任务完成，继续处理中…」 — back to waiting while tasks remain live, or to the true end — while a restart/interrupt Wake, a late tail or a card already at an ending still continues the chain on a new card (ADR-0059, ADR-0066). Distinct from the live 「等待你的授权/回答」 state: the turn is not blocked on the user, and it resumes without one. A waiting card that is superseded collects as 「⏳ 部分完成 · 已由新消息接管」; one whose Session stops being the Active Session collects as 「⏳ 已切换会话 · 后台任务仍在运行」. A waiting card whose last Background Task retires — by its **Wake**, or by the runtime reconciliation that finds the task ended or gone while no Wake will ever come (#454, ADR-0065) — with no continuation to render settles in place as ✅, the true end, with the **Completion Notice** per its existing rules, instead of staying frozen (ADR-0060).
_Avoid_: Paused, suspended, 等待中

**Instant Reminder** (即时提醒):
Feishu's `time_sensitive` capability (opt-in via `[bridge] instant_reminder`): temporarily pinning a conversation at the top of a **Principal**'s message list. cola enables it while a **Permission** or **Question** is pending, and it clears when the wait resolves. It is a **state**, not an event: it stays until the user acts, so it cannot be missed the way a message can; a long task's end is announced by a **Completion Notice** instead. A Chat/Topic has one reminder with one deterministic owner: the newest pending wins, and when the owning wait resolves the next pending takes the pin immediately. Every pin carries its **Turn**'s generation, so a stale clear can never unpin a newer turn's pin. Because the Feishu client offers the user no way to cancel an app's reminder, cola persists its live pins and clears the orphaned ones at startup, retrying any clear that failed; a pin lost to a permanently dead cola stays until the user marks the conversation 完成. The pin carries no reason text; the card title in the conversation preview does, and a pending wait's card also carries a **Message Pin** so opening the chat leads to it. Feishu addresses only a Chat (or the bot conversation), never a Topic: a Topic has no feed-card id, so the reminder lands on the Chat's row in the message list and only the **Message Pin** reaches into the Topic. Distinct from an app feed card, which is a separate list entry cola does not create (neither feed-card mode can address a specific message).
_UI label_: 即时提醒 — the ONLY Chinese term for the list-level pin; never 「会话置顶」 (会话 means **Session** in cola's UI), and not 「置顶聊天」 (that is the user's own Feishu client feature) or 「聊天置顶」 (ambiguous).
_Avoid_: App message stream, feed card, message-list notification

**Message Pin** (消息置顶):
Feishu's `im/v1/pins` capability, enabled or disabled with the same `[bridge] instant_reminder` opt-in as the **Instant Reminder**: putting the exact card a pending **Permission**/**Question** lives on into its Chat/Topic's pinned-message list, so the reminder's list-level nudge actually leads to the waiting message once the user opens the chat — the conversation preview alone only ever shows the newest message, which in a multi-topic chat may be anything. A card rendering both kinds is pinned once and unpinned by the last wait leaving it; a re-hosted card moves its pin. Cola pins each waiting card once per wait, so an unpin the user makes by hand is never re-asserted; failures are best-effort and retried. Distinct from **Instant Reminder**, which pins the conversation, not a message.
_UI label_: 消息置顶 — one message, one pin; several pinned messages simply coexist in the chat's 置顶消息列表. There is no separate 「多条置顶」 concept: pinning is per message and the list is the set.
_Avoid_: Pin module, 置顶 conversation, feed card, 「pin 多条」

**Completion Notice** (完成通知):
The reply cola sends to a **Turn**'s prompt message when the turn ends — its true end, after any 等待后台任务 yields (ADR-0059) — so Feishu pushes a notification: the streaming card is patched in place, which neither notifies nor bumps the conversation. Groups notify on every turn (opt-in `[bridge] group_completion_notice`) and @-mention the requester; p2p notifies only when the turn ran past the long-task threshold (5 minutes, opt-in `[bridge] long_task_notice`) and sends a plain reply — the new message itself is the notification. It is an **event** (one message), not a state: a wait's persistence is the **Instant Reminder**'s job. Distinct from the **Interaction Receipt**, which records a resolution inside the card, and from the **Topic Cover Card**, which is a topic's root.
_Avoid_: Done ping, completion message, notification card

**Todo Panel** (待办面板):
A **Turn**'s live task checklist, rendered as a card-TAIL status section instead of a timeline row: each `todowrite` call replaces it in place, so it rides the newest card of the **Card Chain** and a split never strands the reader on a stale list. Folded by default; its header carries the plan size and the non-empty status counts (`共 N 项 · 🔄 1 · ⬜ 2`) plus the last write's clock, so progress is readable without unfolding. An update arrives as a whole new list, never as an edit of the previous panel.
_Avoid_: Todo card, task list card, progress panel

**Tool Panel** (工具面板):
The card element recording one tool call in a **Turn** — a folded collapsible panel carrying the tool's name, status icon and server start time, plus its rendered input and output. A live `task`/`subagent` call's panel also carries its child session's liveness in the title — the child's current activity and how long it has run (its newest tool and that call's elapsed time, or its phase and the time since its newest activity), plus its wait state, read-only (ADR-0054). One call, one panel, and it is not interactive: dealing with a tool's **Permission** or **Question** lives in an **Interaction Block**. An unfinished call (`running`/`pending`) is live content: its panel rides the newest card of the **Card Chain** as a card-TAIL section, like the **Todo Panel**, so a split can never strand it; once the tool settles, the panel takes its ordered place in the card timeline. A `shell`/`subagent` call that moved its run to the background keeps that place as the record of the request, rendered 🌙 in its status slot rather than ✅ — the **Background Task Ledger** owns the run's liveness (ADR-0060). A settled call whose own metadata reports a failed run (a shell's non-zero exit or timeout, a Code Mode program's error) renders the failure icon; a Code Mode `execute` call lists the nested calls it made. The `todowrite` status section is a **Todo Panel**, not a Tool Panel.
_Avoid_: tool card, call card, tool bubble

**Built-in Tool**:
A tool OpenCode ships itself, identified by a stable tool id (`read`, `edit`, `shell`/`bash`, `subagent`/`task`, …). Cola may tailor a Built-in Tool's **Tool Panel** rendering because that id and its payload shapes are a contract cola can follow. Contrast MCP servers and injected plugins (OpenChamber), whose input/output shapes cola treats as opaque text and never models.
_Avoid_: native tool, core tool, internal tool

**Interaction Block** (交互块):
A pending **Permission** or **Question** rendered inline inside another card — a **Turn**'s card tail, or a **Session Snapshot**'s pending section — carrying its prompt, controls and answer state, as opposed to a standalone request card. It follows the newest card of its **Turn**'s **Card Chain** (including when a new **Turn** replaces the accumulator) and is updated in place on whichever card currently shows it.
_Avoid_: Pending section, inline card, request block

**Interaction Receipt** (回执):
The one-line record left in place of an **Interaction Block** once its **Permission**/**Question** is resolved — by this Chat/Topic, by another client, by Auto-Accept, or by the run ending before anyone answered — stating what was decided (allowed/denied/answered/handled elsewhere) or that the request died with its session's run (interrupted), and about what (its target: the command, file, or question). It renders as a transcript entry keyed at the moment of resolution — the card's transcript is ordered by each part's server-side start time, so a command or reasoning the server wrote before the click stays above the receipt even if cola rendered it afterwards. Contrast a block that simply vanishes, which leaves history unreadable and a stale click ambiguous. Distinct from a **Cargo Receipt** (cargo's install bookkeeping).
_Avoid_: Result line, status card, stale marker

**Quoted Context**:
The parent message's content (text + attached images) that a reply answers, fetched from the platform and prepended to the prompt. Makes the reply relationship explicit and covers parents missing from session history (lobby-switch, compaction). Distinct from the user's own message text, which is the prompt's primary content. Never applied to the topic's own Topic Root or Topic Anchor — they are creation boilerplate, not genuine quotes (ADR-0023).
_Avoid_: Quote, reference, reply context

**Image Attachment**:
A platform image (a standalone image message, an image inside a rich-text message, or a quoted image) downloaded by the platform and attached to a prompt as a vision file part. Requires a vision-capable model; unsupported models surface an error.
_Avoid_: Picture, media, attachment file

**Turn Footer**:
The Card footer line summarizing what a turn ran on: working directory (project basename), git branch and dirty state, the answering model, and context-window usage. The model line renders the full identity `provider/model@variant` (provider is part of model identity, not decoration). The directory/branch half is captured when the turn starts (so a wrong-branch run is visible from the first card) and refreshed when the turn ends (so the completed card shows where the turn landed); the model line and the context-window segment appear on every card — including a split "部分完成" one — as soon as their data exists. Context usage advances with the turn (each completed step), rendered as `used/window (percent)`, or as the used tokens alone when the window size is unknown. Only a streaming-card chain carries the Turn Footer; standalone Permission/Question cards and session snapshot cards never do (ADR-0019, ADR-0044).
_Avoid_: Tail, footer bar, status line

**Dirty**:
A git working tree that differs from HEAD — including untracked files — as measured by `git status --porcelain`. Shown as ⚠ on the Turn Footer. Captured at turn start (the state the AI operated on) and refreshed at turn end (whether the AI left uncommitted work behind).
_Avoid_: Uncommitted, modified

**Release Version**:
The semver in `Cargo.toml` that the running cola binary was built from — the ONLY value an update check ever compares: against the GitHub release tag (ADR-0015) or, on the cargo channel, the crates.io version (ADR-0030). It must equal the tag a release is cut on; dev/provenance detail never enters the comparison, or a dev build ahead of the last release would report "update available" forever.
_Avoid_: Version, build number (unqualified — they blur the comparison source with the display string)

**Build Provenance**:
The identity baked into a cola binary at build time describing where it came from. An exact, clean release-tag checkout shows the bare **Release Version**; a crates.io package build (no `.git`, but `.cargo_vcs_info.json`) is also a release identity, marked `(crates.io)` (ADR-0030); every other build — a branch, a dirty tree, or a source tree with no git — is a dev build and shows `-dev` plus the branch/short-sha it was built from, with ⚠ when that tree was Dirty. Determines whether a binary is a release or a dev build (ADR-0027).
_Avoid_: Version, build info, source marker

**Distribution Channel** (分发渠道):
A publishing-side channel through which cola builds are made available: GitHub Releases (release archives with `SHA256SUMS` and build-provenance attestations) and crates.io (source packages for `cargo install` / `cargo binstall`). Distinct from the **Install Channel**, which records where a given machine's binary actually came from; only the Install Channel decides the **Update Channel** (ADR-0030).
_Avoid_: Publishing channel, release channel

**Install Channel**:
How a cola binary got onto the machine: a GitHub Release archive, crates.io (`cargo install`/`cargo binstall`), or a source build. Decides the **Update Channel** (ADR-0030).
_Avoid_: Install method, distribution channel (that is the named **Distribution Channel**, the publishing side)

**Update Channel**:
Where a running binary takes its updates from: GitHub Releases self-update for binaries cargo does not track; crates.io (via the cargo commands) for a binary with a **Cargo Receipt**. Set by the **Install Channel**, never mixed (ADR-0030).
_Avoid_: Channel (unqualified), update source

**Cargo Receipt**:
cargo's install bookkeeping in the install root — `.crates2.json`, mapping each installed package to the binary names it placed in `<root>/bin`. Its presence for the running binary is what makes cola route updates through cargo instead of self-update; a `--no-track` install leaves none and is treated as a GitHub-channel install (ADR-0030).
_Avoid_: Lock file, metadata file (both ambiguous)

**Singleton Lock**:
The guarantee that at most one cola instance processes platform events at a time. A `/restart` hands it to its replacement before the old instance exits; a replacement may take it over from an owner that is no longer **Functionally Alive** — dead, a zombie, or mid-`exit()`.
_Avoid_: pid file, lock file

**Functionally Alive**:
A cola instance that can still process events: running with its identity still readable. A process whose memory map the kernel has already torn down (mid-`exit()`) is NOT Functionally Alive even though the OS reports a live status — a `/restart` replacement reclaims its lock instead of waiting for it.
_Avoid_: alive, running, process alive (all ambiguous — they include the mid-`exit()` state)

**Autostart** (自动启动):
cola's boot/login registration — the OS artifact that starts the cola binary at boot (a systemd user unit, a LaunchAgent, or an `HKCU\...\Run` value), managed by `cola autostart enable|disable|status`. `disable` unregisters and stops the instance, including a supervised one that never took the **Singleton Lock** (still starting up, or crash-looping); `cola stop` stops the lock holder without unregistering.
_Avoid_: service, launcher (both ambiguous between the registration and the facility)

**Supervisor**:
The OS facility that honours cola's **Autostart** registration and can also stop or restart the instance — a systemd user unit or a launchd agent. cola stops a supervised instance through it (a LaunchAgent with `KeepAlive` would respawn a directly-killed process); Windows' `Run` key only launches, so stop falls back to terminating the lock-holding PID.
_Avoid_: launcher, systemd/launchd (platform names for one platform's Supervisor)

**Command**:
A slash-prefixed instruction (e.g. `/new`, `/dir`, `/switch`, `/compact`). Every command supports two forms: a text-direct form (an argument that completes the action in one step) and a card form (no argument pops a card — an **Interactive Card** for strong-interaction commands like `/switch`, `/dir`, `/model`, `/agent`, `/autoaccept`, or a **Reference Card** for `/help`). cola parses its own commands locally and forwards unrecognized ones to the Backend as prompt text.
_Avoid_: Slash command, action, operation

**Reference Card**:
A read-only **Card** whose only job is to present information: no buttons, no state change. `/help` is the only one — a grouped command manual; detail for a single command stays text via `/help <command>`. Distinct from an **Interactive Card**, which drives state changes through buttons (Permission, Question, `/switch`, `/model`, `/agent`, `/autoaccept`).
_Avoid_: Manual card, static card

**Event**:
A typed protocol message on the Backend's server-sent event stream. Not a domain input: cola subscribes to no stream and renders **Cards** by polling the **Session Transcript** instead (ADR-0011).
_Avoid_: Notification, message, signal

## Relationships

- A **Bot** contains one **Platform** and one or more **Backend** adapters
- A **Backend** attachment speaks exactly one **Generation**; its **Generation Strategy** is the adapter-internal module that owns it, and the **Attached Generation** is a property of the attachment, never of a **Session** (ADR-0055)
- Every inbound message or card action carries exactly one **Principal** (its sender or clicking user), authorized against the **Access List** before cola acts
- The first successful **Claim** writes the **Host** into the **Access List**; every other Principal is refused
- A **Chat** contains many **Topics**; a **Chat** may hold several **Sessions** directly (lobby), while a **Topic** holds exactly one **Session** (or one **Pending Session** until its first prompt)
- A **Chat** or **Topic** has one **Session Mapping**: the set of **Session**s it has activated, with at most one of them its **Active Session**, plus at most one **Pending Session** that materialises at the conversation's first prompt
- A **Topic** is created around its **Topic Root** and, when cola opens it, is anchored on its **Topic Anchor**; a cola-created **Topic Root** is a **Topic Cover Card**
- A **Session** contains many **Turns** and has one **Project** and one optional **Agent**; a **Turn** spans one or more **Executions**, and an **Execution** ends at the Backend's idle boundary, which a **Wake** may follow with another Execution (ADR-0059)
- A **Session** is read through one **Session Transcript**, whose projections serve rendering, **Session Sync** and the **Session Snapshot**
- A **Wake Watermark** persists which **Wake**s a card chain has already announced, so a cola restart never re-posts one; the **Sync Watermark** stays user-message-scoped (ADR-0061)
- A **Turn** renders into a **Card Chain**; a pending **Permission**/**Question** rides its newest card as an **Interaction Block**, and resolving one leaves an **Interaction Receipt**
- A **Supplement** splits its **Turn**'s **Card Chain** so the continuation card is the newest message; the render loop stays alive across a Supplement that starts a new **Turn**; a **Command** reply does not split the chain by itself — `/card` pulls the live card down on explicit request
- A **Turn** with live **Background Tasks** yields its card as 等待后台任务; the next shell/subagent completion **Wake** resumes it in place, and a **Turn** superseded while waiting collects as 已由新消息接管 (ADR-0059, ADR-0066); the Session's live tasks ride the newest card as one **Background Task Ledger** — a completion no longer moves it, since the yielded card resumes in place — a task leaving — its Wake, or a runtime-confirmed end / lost record (ADR-0065) — leaves a fixed entry on the card it lived on, and a last task retiring with nothing to render settles the waiting card as ✅ (ADR-0060)
- An **Instant Reminder** pins the conversation while a **Permission**/**Question** is pending, and clears when the wait resolves; a **Completion Notice** announces a long p2p **Turn**'s end — after any 等待后台任务 yields, including a waiting card's settle-in-place and an in-place resumed run's true end (a new message, not a pin; ADR-0059, ADR-0060, ADR-0066)
- A **Turn** renders one **Tool Panel** per tool call; an unfinished panel rides the newest card of its **Card Chain** as a tail section and joins the card timeline when the tool settles; only **Built-in Tool**s (and tools cola itself injects) may get tailored rendering — every other tool's payload stays opaque
- A **Session** receives many **Permissions** and **Questions**
- A **Session Snapshot** reports the state of one **Session** (its last **Turn**'s completion, pending **Permissions**/**Questions**, recent messages) to the **Chat**/**Topic** that activated it
- The **Bridge** reads the **Session Transcript** from a **Backend** and renders **Card** updates on the **Platform**
- A prompt's **Quoted Context** and **Image Attachment**s enrich the **Session** the reply belongs to
- A **Command** is parsed by the **Bridge** from message text before routing to the **Backend**
- Every **Cola-Authored Message** carries a `msg_cola_` id chosen by the **Bridge**; Session Sync treats only user messages newer than the **Sync Watermark** that are NOT **Cola-Authored Message**s as **External Message**s
- A **Distribution Channel** publishes builds; a machine's **Install Channel** records which one it got them from
- A **Cargo Receipt** for the running binary flips its **Update Channel** from GitHub Releases to crates.io; the **Install Channel** decides, never the other way around (ADR-0030)

## Example dialogue

> **Dev:** "If a user sends a message in a new topic, does the Bridge create a new Session?"
> **Domain expert:** "Yes — the first message in a topic triggers session creation. If there's an existing topic, the message routes to that topic's session."
>
> **Dev:** "I ran `/new` — which session did that create?"
> **Domain expert:** "None. `/new` records a Pending Session; the Session itself appears when the conversation's first prompt arrives."
>
> **Dev:** "What happens when a Permission request arrives mid-stream?"
> **Domain expert:** "The Bridge pauses the Card stream, renders a Permission card with action buttons, and waits for the user to reply. Once resolved, streaming resumes."
>
> **Dev:** "Does the Bridge forward `/compact` to the Backend?"
> **Domain expert:** "No — the Bridge recognizes `/compact` as a Command and calls the Backend's REST endpoint directly. Only unrecognized slash commands are forwarded as prompt text."

## Flagged ambiguities

- None. Resolved: the 「会话」 overload (Feishu conversation vs OpenCode
  session) and the "why are so many sessions 本会话" confusion were settled in
  ADR-0022 — 会话 is the OpenCode Session only, the Feishu side is 聊天/话题,
  a single exchange is a Turn (internal), and only the Active Session is marked.
