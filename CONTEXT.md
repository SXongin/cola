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

**Session**:
A single conversation thread with an AI backend. In Feishu, one thread maps to one session. A session has a directory (project) and an optional agent selection.
_Avoid_: Chat, conversation, room

**Thread**:
A Feishu message thread (root message + replies). The boundary that isolates one session from another.
_Avoid_: Topic, channel

**Project**:
A working directory on the filesystem where OpenCode operates. A property of a session, not of the bot. Switched via `/dir`.
_Avoid_: Workspace, repo

**Permission**:
A request from the AI backend to perform an action on a resource. Presented to the user as an interactive card with Allow/Deny/Always options.
_Avoid_: Approval, authorization, consent

**Question**:
A structured multi-choice prompt from the AI backend, distinct from permissions. User selects options to reply.
_Avoid_: Poll, survey, prompt

**Card**:
A Feishu interactive message card. Evolves through states (loading → reasoning → running → streaming → done) and uses collapsible panels for secondary content.
_Avoid_: Widget, component, bubble

## Relationships

- A **Bot** contains one **Platform** and one or more **Backend** adapters
- A **Thread** contains exactly one **Session**
- A **Session** has one **Project** and one optional **Agent**
- A **Session** receives many **Permissions** and **Questions**
- The **Bridge** receives **Events** from a **Backend** and renders them as **Card** updates on the **Platform**

## Example dialogue

> **Dev:** "If a user sends a message in a new thread, does the Bridge create a new Session?"
> **Domain expert:** "Yes — the first message in a thread triggers session creation. If there's an existing thread, the message routes to that thread's session."
>
> **Dev:** "What happens when a Permission request arrives mid-stream?"
> **Domain expert:** "The Bridge pauses the Card stream, renders a Permission card with action buttons, and waits for the user to reply. Once resolved, streaming resumes."

**Command**:
A slash-prefixed instruction (e.g. `/dir`, `/switch`, `/compact`). cola parses its own commands locally and forwards unrecognized ones to the Backend as prompt text.
_Avoid_: Slash command, action, operation

**Event**:
A typed protocol message from the Backend via SSE. Drives Card state transitions.
_Avoid_: Notification, message, signal

## Relationships

- A **Bot** contains one **Platform** and one or more **Backend** adapters
- A **Thread** contains exactly one **Session**
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
