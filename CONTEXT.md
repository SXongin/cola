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
An `opencode serve` process that cola itself spawned (pid recorded in `~/.cola/self-opencode.pid`). Only an Owned Server may be killed, restarted, or reaped by cola; everything else is someone else's process.
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
A single conversation thread with an AI backend, identified by the server's session id and `title` (the server is the single source of truth for identity, ADR-0007). A session has a directory (project) and an optional agent selection. One session maps to at most one Feishu thread at a time.
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
A single user→assistant exchange inside a Session (one prompt plus its streamed card response). cola's internal vocabulary is the English word "turn" (Turn Footer, ADR-0019); there is deliberately NO user-facing Chinese noun for it — the UI never labels individual turns. If one is ever needed, use 轮次/本轮.
_Avoid_: 对话 as a user-facing term for this (overloads "conversation"); 消息 (a single message, not a full exchange).

**Supplement** (补充消息):
A user message sent while its **Session** has a **Turn** in flight. cola does not start a competing Turn: it submits the message to the Backend's running loop, which merges it into that Turn when the loop is still alive, or starts a new Turn when it has already exited. Either way the message lands below the live card, so it splits the **Card Chain** — the continuation card is its reply and carries a receipt line; there is no separate text acknowledgement. A command reply is NOT a Supplement: cola deliberately leaves it as the newest message — unless the user runs `/card`, which explicitly pulls the live card back down.
_Avoid_: Follow-up, addition, queued message

**Thread** (legacy name for Topic):
A Feishu topic, identified by `thread_id` (`omt_...`). Retained in code as `ThreadKey { chat_id, thread_id }`; the glossary now calls the Feishu side Chat/Topic, and 话题 for topics in the UI. See Topic.

**Active Session**:
The single session of a chat/topic that messages route to and that external-message sync follows. At most one per ThreadKey at a time (the SessionStore's first entry); `/switch` promotes a session to active, and so does a **Pending Session**'s materialisation. A conversation with a Pending Session has NO Active Session until that first prompt materialises it.
_Avoid_: Current session, latest session, selected session

**Session Mapping**:
Cola's record of which Sessions a Chat or Topic has activated, and which one is its Active Session. Established when a Session is opened or adopted for a conversation and remembered across restarts; distinct from the Session itself, whose identity lives on the Backend (ADR-0007), and from the server's session list, which is Backend state rather than cola's. A Pending Session is stored beside the mapping, not in it — it is not a Session.
_Avoid_: Session list, session store, mapping table

**Cola-Authored Message**:
A user message cola itself submitted to the Backend on behalf of a Feishu Chat/Topic, as opposed to an External Message. Self-identifying: cola chooses the message's id (`msg_cola_…`) at send time and the Backend persists that id, so authorship survives a server crash/replacement and even a cola restart without any cola-side ledger. Recognised by the `msg_cola_` id prefix.
_Avoid_: Outbound message, own prompt (a prompt is the send action, not the stored message)

**External Message**:
A user message in a Session that cola did NOT author — someone posted it from another Shared Store client (OpenChamber, the CLI). Surfaced to Feishu by the external-message sync, which follows only the Active Session (ADR-0017). The opposite of a Cola-Authored Message.
_Avoid_: Foreign message, out-of-band message

**Sync Watermark**:
The external-message poller's per-session record of the newest user message it has already accounted for: anything newer that is not a Cola-Authored Message is an External Message and triggers a notification. Advances past both cola-authored and external messages; cleared when a session stops being the Active Session so a later `/switch` back re-baselines silently. Owned by the poller alone — the prompt path no longer records it (formerly called the "baseline").
_Avoid_: Baseline (the old name; it implied the prompt path owned it)

**Project**:
A working directory on the filesystem where OpenCode operates. A property of a session, not of the bot. A conversation's current project is the directory of its Pending Session when it has one, otherwise of its active session (derived, never stored separately); `/new` and the bare `/topic` form inherit it and fall back to the default directory only when the conversation has neither. Sessions created outside a conversation still carry their own directory.
_Avoid_: Workspace, repo

**Recent Directories** (「最近目录」):
Directories of the most recently active sessions in the Shared Store, deduplicated by directory and sorted by last activity, unioned with the directories cola has mapped (most recently mapped first) and the conversation's current directory. The union is what keeps a directory on the card after the server drops its last session — deletion or archival — and after a Pending Session declares one no server session exists for yet (ADR-0046). A bare `/dir` (no argument) presents them as a picker card whose rows offer one-tap re-rooting into the current conversation or opening a new Topic for that directory (ADR-0025).
_Avoid_: Recently opened folders, folder history, recent projects

**Variant**:
A model-declared reasoning-effort setting (e.g. "low"/"high"/"minimal"), selectable per session via the `/think` command and sent to the backend as `PromptInput.variant`. Each model declares its own variant set — there is no universal scale across models — and "unset" means the server's default for that model. Selecting a model that doesn't declare the current session's variant clears it. The user-facing label on cards is "思考等级"/thinking; "variant" is the backend protocol term, never a command name.
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
The one read-only card a Chat/Topic receives when it activates a Session it was not already following — a first adoption, `/attach`, `/topic --adopt`, or the re-activation of a mapped Session — replacing the bare adoption confirmation. It answers what the operator cannot know at takeover: whether the last Turn ended (status line 等待你的确认 / 运行中 / 需要重试 / 空闲), what is blocked on the user (adopt-time Permissions and Questions, actionable inline), and what was recently said (最近对话 tail). Purely a read: it never prompts the Backend, writes to the Session, or disturbs a running Turn. Suppressed when re-activating a Session whose recent life is already fully visible in this Chat/Topic (the switch then confirms in one text line). An adopted-busy Session's in-flight external Turn is followed into the card until completion; every other snapshot is one-shot.
_Avoid_: Briefing, takeover summary, handoff card

**Card**:
A Feishu interactive message card. Evolves through states (loading → reasoning → running → streaming → done), uses collapsible panels for secondary content, and shows progress in its header (phase timer, silence, reasoning length) so a slow turn is distinguishable from a dead one — including a "等待你的授权"/"等待你的回答" state that names whichever pending request blocks the turn (both at once reads "等待你的授权/回答").
_Avoid_: Widget, component, bubble

**Card Chain**:
The one or more **Card**s a single **Turn** renders into when its content exceeds what one Feishu card may hold, when a **Supplement** lands below the live card and the chain must continue there to stay the newest message, or when the user explicitly pulls the live card down with `/card`: the filled card is finalized with a 部分完成，继续中 header and a continuation card takes over, replied to the user message it continues from. Only the newest card of the chain keeps receiving updates; an **Interaction Block** rides that newest card; a command reply does not split the chain by itself — `/card` is the user-invoked exception.
_Avoid_: Split card, multi-card turn, card pagination

**Instant Reminder** (即时提醒):
Feishu's `time_sensitive` capability (opt-in via `[bridge] instant_reminder`): temporarily pinning a conversation at the top of a **Principal**'s message list. cola enables it while a **Permission** or **Question** is pending, and it clears when the wait resolves. It is a **state**, not an event: it stays until the user acts, so it cannot be missed the way a message can; a long task's end is announced by a **Completion Notice** instead. A Chat/Topic has one reminder with one deterministic owner: the newest pending wins, and when the owning wait resolves the next pending takes the pin immediately. Every pin carries its **Turn**'s generation, so a stale clear can never unpin a newer turn's pin. Because the Feishu client offers the user no way to cancel an app's reminder, cola persists its live pins and clears the orphaned ones at startup, retrying any clear that failed; a pin lost to a permanently dead cola stays until the user marks the conversation 完成. The pin carries no reason text; the card title in the conversation preview does, and a pending wait's card also carries a **Message Pin** so opening the chat leads to it. Feishu addresses only a Chat (or the bot conversation), never a Topic: a Topic has no feed-card id, so the reminder lands on the Chat's row in the message list and only the **Message Pin** reaches into the Topic. Distinct from an app feed card, which is a separate list entry cola does not create (neither feed-card mode can address a specific message).
_UI label_: 即时提醒 — the ONLY Chinese term for the list-level pin; never 「会话置顶」 (会话 means **Session** in cola's UI), and not 「置顶聊天」 (that is the user's own Feishu client feature) or 「聊天置顶」 (ambiguous).
_Avoid_: App message stream, feed card, message-list notification

**Message Pin** (消息置顶):
Feishu's `im/v1/pins` capability, enabled or disabled with the same `[bridge] instant_reminder` opt-in as the **Instant Reminder**: putting the exact card a pending **Permission**/**Question** lives on into its Chat/Topic's pinned-message list, so the reminder's list-level nudge actually leads to the waiting message once the user opens the chat — the conversation preview alone only ever shows the newest message, which in a multi-topic chat may be anything. A card rendering both kinds is pinned once and unpinned by the last wait leaving it; a re-hosted card moves its pin. Cola pins each waiting card once per wait, so an unpin the user makes by hand is never re-asserted; failures are best-effort and retried. Distinct from **Instant Reminder**, which pins the conversation, not a message.
_UI label_: 消息置顶 — one message, one pin; several pinned messages simply coexist in the chat's 置顶消息列表. There is no separate 「多条置顶」 concept: pinning is per message and the list is the set.
_Avoid_: Pin module, 置顶 conversation, feed card, 「pin 多条」

**Completion Notice** (完成通知):
The reply cola sends to a **Turn**'s prompt message when the turn ends, so Feishu pushes a notification: the streaming card is patched in place, which neither notifies nor bumps the conversation. Groups notify on every turn (opt-in `[bridge] group_completion_notice`) and @-mention the requester; p2p notifies only when the turn ran past the long-task threshold (5 minutes, opt-in `[bridge] long_task_notice`) and sends a plain reply — the new message itself is the notification. It is an **event** (one message), not a state: a wait's persistence is the **Instant Reminder**'s job. Distinct from the **Interaction Receipt**, which records a resolution inside the card, and from the **Topic Cover Card**, which is a topic's root.
_Avoid_: Done ping, completion message, notification card

**Todo Panel** (待办面板):
A **Turn**'s live task checklist, rendered as a card-TAIL status section instead of a timeline row: each `todowrite` call replaces it in place, so it rides the newest card of the **Card Chain** and a split never strands the reader on a stale list. Folded by default; its header carries the plan size and the non-empty status counts (`共 N 项 · 🔄 1 · ⬜ 2`) plus the last write's clock, so progress is readable without unfolding. An update arrives as a whole new list, never as an edit of the previous panel.
_Avoid_: Todo card, task list card, progress panel

**Tool Panel** (工具面板):
The card element recording one tool call in a **Turn** — a folded collapsible panel carrying the tool's name, status icon and server start time, plus its rendered input and output. One call, one panel, and it is not interactive: dealing with a tool's **Permission** or **Question** lives in an **Interaction Block**. An unfinished call (`running`/`pending`) is live content: its panel rides the newest card of the **Card Chain** as a card-TAIL section, like the **Todo Panel**, so a split can never strand it; once the tool settles, the panel takes its ordered place in the card timeline. The `todowrite` status section is a **Todo Panel**, not a Tool Panel.
_Avoid_: tool card, call card, tool bubble

**Built-in Tool**:
A tool OpenCode ships itself, identified by a stable tool id (`bash`, `read`, `edit`, `task`, …). Cola may tailor a Built-in Tool's **Tool Panel** rendering because that id and its payload shapes are a contract cola can follow. Contrast MCP servers and injected plugins (OpenChamber), whose input/output shapes cola treats as opaque text and never models.
_Avoid_: native tool, core tool, internal tool

**Interaction Block** (交互块):
A pending **Permission** or **Question** rendered inline inside another card — a **Turn**'s card tail, or a **Session Snapshot**'s pending section — carrying its prompt, controls and answer state, as opposed to a standalone request card. It follows the newest card of its **Turn**'s **Card Chain** (including when a new **Turn** replaces the accumulator) and is updated in place on whichever card currently shows it.
_Avoid_: Pending section, inline card, request block

**Interaction Receipt** (回执):
The one-line record left in place of an **Interaction Block** once its **Permission**/**Question** is resolved — by this Chat/Topic, by another client, or by Auto-Accept — stating what was decided (allowed/denied/answered/handled elsewhere) and about what (its target: the command, file, or question). It renders as a transcript entry keyed at the moment of resolution — the card's transcript is ordered by each part's server-side start time, so a command or reasoning the server wrote before the click stays above the receipt even if cola rendered it afterwards. Contrast a block that simply vanishes, which leaves history unreadable and a stale click ambiguous. Distinct from a **Cargo Receipt** (cargo's install bookkeeping).
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
A typed protocol message from the Backend via SSE. Drives Card state transitions.
_Avoid_: Notification, message, signal

## Relationships

- A **Bot** contains one **Platform** and one or more **Backend** adapters
- Every inbound message or card action carries exactly one **Principal** (its sender or clicking user), authorized against the **Access List** before cola acts
- The first successful **Claim** writes the **Host** into the **Access List**; every other Principal is refused
- A **Chat** contains many **Topics**; a **Chat** may hold several **Sessions** directly (lobby), while a **Topic** holds exactly one **Session** (or one **Pending Session** until its first prompt)
- A **Chat** or **Topic** has one **Session Mapping**: the set of **Session**s it has activated, with at most one of them its **Active Session**, plus at most one **Pending Session** that materialises at the conversation's first prompt
- A **Topic** is created around its **Topic Root** and, when cola opens it, is anchored on its **Topic Anchor**; a cola-created **Topic Root** is a **Topic Cover Card**
- A **Session** contains many **Turns** and has one **Project** and one optional **Agent**
- A **Turn** renders into a **Card Chain**; a pending **Permission**/**Question** rides its newest card as an **Interaction Block**, and resolving one leaves an **Interaction Receipt**
- A **Supplement** splits its **Turn**'s **Card Chain** so the continuation card is the newest message; the render loop stays alive across a Supplement that starts a new **Turn**; a **Command** reply does not split the chain by itself — `/card` pulls the live card down on explicit request
- An **Instant Reminder** pins the conversation while a **Permission**/**Question** is pending, and clears when the wait resolves; a **Completion Notice** announces a long p2p **Turn**'s end (a new message, not a pin)
- A **Turn** renders one **Tool Panel** per tool call; an unfinished panel rides the newest card of its **Card Chain** as a tail section and joins the card timeline when the tool settles; only **Built-in Tool**s (and tools cola itself injects) may get tailored rendering — every other tool's payload stays opaque
- A **Session** receives many **Permissions** and **Questions**
- A **Session Snapshot** reports the state of one **Session** (its last **Turn**'s completion, pending **Permissions**/**Questions**, recent messages) to the **Chat**/**Topic** that activated it
- The **Bridge** receives **Events** from a **Backend** and renders them as **Card** updates on the **Platform**
- A prompt's **Quoted Context** and **Image Attachment**s enrich the **Session** the reply belongs to
- A **Command** is parsed by the **Bridge** from message text before routing to the **Backend**
- Every **Cola-Authored Message** carries a `msg_cola_` id chosen by the **Bridge**; external-message sync treats only user messages newer than the **Sync Watermark** that are NOT **Cola-Authored Message**s as **External Message**s
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
