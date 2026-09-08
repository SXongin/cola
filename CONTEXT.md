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

**Session**:
A single conversation thread with an AI backend, identified by the server's session id and `title` (the server is the single source of truth for identity, ADR-0007). A session has a directory (project) and an optional agent selection. One session maps to at most one Feishu thread at a time.
_Avoid_: Chat, conversation, room
_UI label_: 会话 — the ONLY meaning of 「会话」 in cola's UI; the Feishu side is never called 会话 (see Chat/Topic UI labels). Resolves the old "why are so many sessions 本会话" ambiguity: cola marks only the Active Session and uses 聊天/话题 for the Feishu side.

**Chat**:
The Feishu top-level conversation (a group or p2p), identified by `chat_id` — the lobby of ADR-0007. A chat contains many Topics and may hold several Sessions directly (the lobby). Distinct from a Topic: a chat has no `thread_id`.
_Avoid_: Conversation, room, group
_UI label_: 聊天 (the Feishu-side container; a Feishu user's own term). Feishu's own UI calls the top-level thing a 会话, which is exactly the collision cola avoids — cola never uses 会话 for this.

**Topic**:
A Feishu thread inside a Chat, identified by `thread_id` (`omt_...`; called "话题" in the Feishu UI). A message is a topic message IFF it carries `thread_id`. A topic holds exactly one Session; the boundary that isolates one session from another. A topic is created around a message (its Topic Root) and is usually opened by cola with a seed card (Topic Anchor).
_UI label_: 话题.

**Topic Root**:
The message a Feishu topic is created around. For a cola-created topic (`/topic`, `/topic --adopt`) this is the user's own command message — it stays in the main Chat while also anchoring the thread; for a manually-created topic it is the user's message the topic was made on. Persisted as `topic_root`. Distinct from Topic Anchor: the Root is the thread's first message (user-typed, outside/at the boundary of the topic), the Anchor is the first reply inside it (bot-typed).
_Avoid_: Seed, anchor, topic message

**Topic Anchor**:
The bot's seed card inside a cola-created topic — the `/topic` confirmation message, the first reply inside the thread. Doubles as the reply target that keeps permission/question/external cards inside the topic (the create API rejects a `thread_id` as target). Persisted as `topic_anchor`. Distinct from Topic Root: the Anchor is inside the topic, the Root is the message the thread is built around. Never injected as prompt context (ADR-0023).
_Avoid_: Root, seed message, confirmation card

**Turn**:
A single user→assistant exchange inside a Session (one prompt plus its streamed card response). cola's internal vocabulary is the English word "turn" (Turn Footer, ADR-0019); there is deliberately NO user-facing Chinese noun for it — the UI never labels individual turns. If one is ever needed, use 轮次/本轮.
_Avoid_: 对话 as a user-facing term for this (overloads "conversation"); 消息 (a single message, not a full exchange).

**Thread** (legacy name for Topic):
A Feishu topic, identified by `thread_id` (`omt_...`). Retained in code as `ThreadKey { chat_id, thread_id }`; the glossary now calls the Feishu side Chat/Topic, and 话题 for topics in the UI. See Topic.

**Active Session**:
The single session of a chat/topic that messages route to and that external-message sync follows. Exactly one per ThreadKey at a time (the SessionStore's first entry); `/switch` and `/new` promote a session to active, and cola derives the conversation's current project from it.
_Avoid_: Current session, latest session, selected session

**Project**:
A working directory on the filesystem where OpenCode operates. A property of a session, not of the bot. A conversation's current project is the directory of its active session (derived, never stored separately); `/new` and the bare `/topic` form inherit it and fall back to the default directory only when the conversation has no session. Sessions created outside a conversation still carry their own directory.
_Avoid_: Workspace, repo

**Recent Directories** (「最近目录」):
Directories of the most recently active sessions in the Shared Store, deduplicated by directory and sorted by last activity. A bare `/dir` (no argument) presents them as a picker card for one-tap re-rooting. The server's session list is the only source — cola records no folder history of its own.
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
A structured multi-choice prompt from the AI backend, distinct from permissions. User selects options to reply.
_Avoid_: Poll, survey, prompt

**Card**:
A Feishu interactive message card. Evolves through states (loading → reasoning → running → streaming → done), uses collapsible panels for secondary content, and shows progress in its header (phase timer, silence, reasoning length) so a slow turn is distinguishable from a dead one — including a "等待你的授权/回答" state while a permission or question is pending.
_Avoid_: Widget, component, bubble

**Quoted Context**:
The parent message's content (text + attached images) that a reply answers, fetched from the platform and prepended to the prompt. Makes the reply relationship explicit and covers parents missing from session history (lobby-switch, compaction). Distinct from the user's own message text, which is the prompt's primary content. Never applied to the topic's own Topic Root or Topic Anchor — they are creation boilerplate, not genuine quotes (ADR-0023).
_Avoid_: Quote, reference, reply context

**Image Attachment**:
A platform image (a standalone image message, an image inside a rich-text message, or a quoted image) downloaded by the platform and attached to a prompt as a vision file part. Requires a vision-capable model; unsupported models surface an error.
_Avoid_: Picture, media, attachment file

**Turn Footer**:
The Card footer line summarizing what a turn ran on: working directory (project basename), git branch and dirty state, the answering model, and context-window usage. The model line renders the full identity `provider/model@variant` (provider is part of model identity, not decoration). The directory/branch half is captured when the turn starts and shows from the first card; the model and context usage appear only when the turn completes.
_Avoid_: Tail, footer bar, status line

**Dirty**:
A git working tree that differs from HEAD — including untracked files — as measured by `git status --porcelain`. Shown as ⚠ on the Turn Footer. Captured at turn start, so it reflects the state the AI operated on, not the changes the AI itself made.
_Avoid_: Uncommitted, modified

**Singleton Lock**:
The guarantee that at most one cola instance processes platform events at a time. A `/restart` hands it to its replacement before the old instance exits; a replacement may take it over from an owner that is no longer **Functionally Alive** — dead, a zombie, or mid-`exit()`.
_Avoid_: pid file, lock file

**Functionally Alive**:
A cola instance that can still process events: running with its identity still readable. A process whose memory map the kernel has already torn down (mid-`exit()`) is NOT Functionally Alive even though the OS reports a live status — a `/restart` replacement reclaims its lock instead of waiting for it.
_Avoid_: alive, running, process alive (all ambiguous — they include the mid-`exit()` state)

## Relationships

- A **Bot** contains one **Platform** and one or more **Backend** adapters
- A **Chat** contains many **Topics**; a **Chat** may hold several **Sessions** directly (lobby), while a **Topic** holds exactly one **Session**
- A **Topic** is created around its **Topic Root** and, when cola opens it, is anchored on its **Topic Anchor**
- A **Session** contains many **Turns** and has one **Project** and one optional **Agent**
- A **Session** receives many **Permissions** and **Questions**
- The **Bridge** receives **Events** from a **Backend** and renders them as **Card** updates on the **Platform**
- A prompt's **Quoted Context** and **Image Attachment**s enrich the **Session** the reply belongs to

## Example dialogue

> **Dev:** "If a user sends a message in a new topic, does the Bridge create a new Session?"
> **Domain expert:** "Yes — the first message in a topic triggers session creation. If there's an existing topic, the message routes to that topic's session."
>
> **Dev:** "What happens when a Permission request arrives mid-stream?"
> **Domain expert:** "The Bridge pauses the Card stream, renders a Permission card with action buttons, and waits for the user to reply. Once resolved, streaming resumes."

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
- A **Chat** contains many **Topics**; a **Chat** may hold several **Sessions** directly (lobby), while a **Topic** holds exactly one **Session**
- A **Topic** is created around its **Topic Root** and, when cola opens it, is anchored on its **Topic Anchor**
- A **Session** contains many **Turns** and has one **Project** and one optional **Agent**
- A **Session** receives many **Permissions** and **Questions**
- The **Bridge** receives **Events** from a **Backend** and renders them as **Card** updates on the **Platform**
- A **Command** is parsed by the **Bridge** from message text before routing to the **Backend**

## Example dialogue

> **Dev:** "If a user sends a message in a new topic, does the Bridge create a new Session?"
> **Domain expert:** "Yes — the first message in a topic triggers session creation. If there's an existing topic, the message routes to that topic's session."
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
