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

**Thread**:
A Feishu topic, identified by `thread_id` (`omt_...`; called "话题/topic" in the Feishu UI). A message is a topic message IFF it carries `thread_id`. A thread holds exactly one session; the boundary that isolates one session from another (group or p2p).

**Lobby**:
A chat's top-level conversation: messages sent directly in a group (or p2p top-level), carrying no `thread_id`. Unlike a thread, a lobby may hold several sessions (switched via `/switch`).
_Avoid_: Main channel, root session

**Active Session**:
The single session of a thread that lobby messages route to and that external-message sync follows. Exactly one per thread at a time (the SessionStore's first entry for the thread); `/switch` and `/new` promote a session to active, and cola derives the conversation's current project from it.
_Avoid_: Current session, latest session, selected session

**Project**:
A working directory on the filesystem where OpenCode operates. A property of a session, not of the bot. A conversation's current project is the directory of its active session (derived, never stored separately); `/new` and the bare `/topic` form inherit it and fall back to the default directory only when the conversation has no session. Sessions created outside a conversation still carry their own directory.
_Avoid_: Workspace, repo

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
The parent message's content (text + attached images) that a reply answers, fetched from the platform and prepended to the prompt. Makes the reply relationship explicit and covers parents missing from session history (lobby-switch, compaction). Distinct from the user's own message text, which is the prompt's primary content.
_Avoid_: Quote, reference, reply context

**Image Attachment**:
A platform image (a standalone image message, an image inside a rich-text message, or a quoted image) downloaded by the platform and attached to a prompt as a vision file part. Requires a vision-capable model; unsupported models surface an error.
_Avoid_: Picture, media, attachment file

**Turn Footer**:
The Card footer line summarizing what a turn ran on: working directory (project basename), git branch and dirty state, the answering model, and context-window usage. The directory/branch half is captured when the turn starts and shows from the first card; the model and context usage appear only when the turn completes.
_Avoid_: Tail, footer bar, status line

**Dirty**:
A git working tree that differs from HEAD — including untracked files — as measured by `git status --porcelain`. Shown as ⚠ on the Turn Footer. Captured at turn start, so it reflects the state the AI operated on, not the changes the AI itself made.
_Avoid_: Uncommitted, modified

## Relationships

- A **Bot** contains one **Platform** and one or more **Backend** adapters
- A **Thread** contains exactly one **Session** (topics only; a **Lobby** may hold several)
- A **Session** has one **Project** and one optional **Agent**
- A **Session** receives many **Permissions** and **Questions**
- The **Bridge** receives **Events** from a **Backend** and renders them as **Card** updates on the **Platform**
- A prompt's **Quoted Context** and **Image Attachment**s enrich the **Session** the reply belongs to

## Example dialogue

> **Dev:** "If a user sends a message in a new thread, does the Bridge create a new Session?"
> **Domain expert:** "Yes — the first message in a thread triggers session creation. If there's an existing thread, the message routes to that thread's session."
>
> **Dev:** "What happens when a Permission request arrives mid-stream?"
> **Domain expert:** "The Bridge pauses the Card stream, renders a Permission card with action buttons, and waits for the user to reply. Once resolved, streaming resumes."

**Command**:
A slash-prefixed instruction (e.g. `/new`, `/dir`, `/switch`, `/compact`). Every command supports two forms: a text-direct form (an argument that completes the action in one step) and a card form (no argument pops a card — an **Interactive Card** for strong-interaction commands like `/switch`, `/model`, `/agent`, `/autoaccept`, or a **Reference Card** for `/help`). cola parses its own commands locally and forwards unrecognized ones to the Backend as prompt text.
_Avoid_: Slash command, action, operation

**Reference Card**:
A read-only **Card** whose only job is to present information: no buttons, no state change. `/help` is the only one — a grouped command manual; detail for a single command stays text via `/help <command>`. Distinct from an **Interactive Card**, which drives state changes through buttons (Permission, Question, `/switch`, `/model`, `/agent`, `/autoaccept`).
_Avoid_: Manual card, static card

**Event**:
A typed protocol message from the Backend via SSE. Drives Card state transitions.
_Avoid_: Notification, message, signal

## Relationships

- A **Bot** contains one **Platform** and one or more **Backend** adapters
- A **Thread** contains exactly one **Session** (topics only; a **Lobby** may hold several)
- A **Session** has one **Project** and one optional **Agent**
- A **Session** receives many **Permissions** and **Questions**
- The **Bridge** receives **Events** from a **Backend** and renders them as **Card** updates on the **Platform**
- A **Command** is parsed by the **Bridge** from message text before routing to the **Backend**

## Example dialogue

> **Dev:** "If a user sends a message in a new thread, does the Bridge create a new Session?"
> **Domain expert:** "Yes — the first message in a thread triggers session creation. If there's an existing thread, the message routes to that thread's session."
>
> **Dev:** "What happens when a Permission request arrives mid-stream?"
> **Domain expert:** "The Bridge pauses the Card stream, renders a Permission card with action buttons, and waits for the user to reply. Once resolved, streaming resumes."
>
> **Dev:** "Does the Bridge forward `/compact` to the Backend?"
> **Domain expert:** "No — the Bridge recognizes `/compact` as a Command and calls the Backend's REST endpoint directly. Only unrecognized slash commands are forwarded as prompt text."

## Flagged ambiguities

- None yet.
