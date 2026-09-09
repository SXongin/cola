# Session Snapshot: a read-only state card when a session is taken over

Taking over a session — first `/switch` to a foreign session, `/attach`, `/topic --adopt`, or a re-`/switch` back to a mapped session — makes it the thread's Active Session but tells the operator nothing about it. The takeover target may have been driven elsewhere (OpenChamber, CLI) or long ago: its recent turns, whether the last turn finished, and any blocked Permission/Question are all unknown. The confirmation messages that exist today are bare text, so the operator starts typing blind.

## Context

- Today an activation ends in one of several confirmations: a text line 「已接管会话 `title`（目录 `dir`）。」 for a lobby adopt (`adopt_session`, `src/bridge/command.rs:1920-1927`), a 「📎 已接管…」 anchor replied in-thread for an in-topic adopt (command.rs:1882-1896), the Topic Cover Card root for `/topic --adopt`, or a switch-card patch to 「已接管」. None carries the session's recent state.
- A foreign session's state is fully readable with no writes:
  - pending Permission/Question: server `GET /permission?directory=` and `GET /question?directory=` (`src/opencode/client.rs:411-471`), already polled every 3 s by the request poller (`src/bridge/request.rs:795,856-943`).
  - running state: the server tracks per-session `idle`/`busy`/`retry` (`/session/status`; opencode `session/status.ts`, route in `httpapi/groups/session.ts`). cola never calls this endpoint — its own `busy()` only knows cola-owned in-flight prompts (`src/bridge/pollers.rs:17-19`).
  - transcript tail and message times: `GET /session/{id}/message` (already used by `render.rs`); the newest user message's authorship is authoritative via the `msg_cola_` id prefix (ADR-0026).
- The external-message sync surfaces an external turn only while the session is the thread's **active** session: it notifies on a new external user message and then streams the reply into that notification card (`src/bridge/external.rs:94-180`, `start_reply_render`). External activity received while a session was inactive is silently re-baselined, not replayed (ADR-0017). An in-flight external run therefore produces no push once a session becomes active mid-turn — there is no new user message to trigger the poller.
- The naive fix — ask the AI to summarise the session — writes to the session, spends tokens, can trigger tools, and would queue against a running turn. Every non-disturbance constraint forbids it.

## Decision

Introduce the **Session Snapshot (会话快照)**: one read-only card emitted whenever a Chat/Topic activates a session, replacing every existing adopt/switch confirmation. It is built purely from server reads — no prompt, no token spend, no session write, never queued against a running turn, nothing sent to other clients.

### Emission and replacement (one card per activation)

- **Lobby text adopt** (global `/switch <kw>`, `/attach` in a non-topic): the snapshot replaces the 「已接管…」 text (command.rs:1920-1927).
- **In-topic `/switch`/`/attach` adopt**: the snapshot is the in-thread confirmation AND becomes the **Topic Anchor** (persisted `topic_anchor`, command.rs:1900-1910), replacing the 「📎 已接管…」 anchor. Permission/question fallback cards keep routing to it (ADR-0023).
- **`/topic --adopt`**: the Topic Cover Card remains the Topic Root in the main chat (its chat-list entry); the snapshot is the first bot message inside the new topic and its Topic Anchor.
- **Switch-card 接管/切换 button**: the interactive list card is patched **in place** to snapshot content — no second message.
- **Re-`/switch` to a session already mapped to this thread**: snapshot when there is content to report, otherwise only the existing one-line text ack (「已切换」) and no card.

### Suppression ("nothing to report")

A snapshot is omitted only when **all** hold: the session is already mapped to this thread (a re-activation, not a first adopt), server status is idle, there is no pending Permission/Question for it, and the newest user message is a Cola-Authored Message (its recent life is therefore already visible in this thread — a session maps to at most one thread, so cola-authored activity happened here). First-time adoption always snapshots: the snapshot **is** the confirmation.

Everything else is a blind region and snapshots: an external newest message (invisible while inactive, ADR-0017 — the tail is the one vehicle that surfaces it), busy/retry, or a pending item.

### Content

- **Header**: 已接管/已切换 + the session title.
- **Status line**, precedence 等待你的确认 > 运行中 > 需要重试 > 空闲: `GET /session/status` (idle/busy/retry) read at build time, plus any pending items found; busy carries one hint line. A failed status read omits the chip rather than guessing. The line reflects the moment of activation.
- **Pending Permission/Question blocks** (actionable), inline, reusing the existing interactive builders (Allow/Deny/Always; question options). Only requests whose `sessionID` is the adopted session. **Snapshot claim**: adopt-time pending items are registered as hosted by the snapshot card (the request poller's inline-host bookkeeping, `src/bridge/request.rs`) so the 3 s poller does not also pop a standalone card for the same request; resolving one patches the snapshot in place. Pending items that first appear **after** the snapshot was sent are not claimed and keep today's standalone-card flow.
- **最近对话 tail**: the last four text-bearing user/assistant messages, verbatim `text` parts, in a collapsible panel; each entry previews briefly and expands to full text. Reasoning and tool parts are inner monologue, not conversation, and are excluded.
- Sections collapse to respect the Feishu card component/size budgets.

### Busy adoption follows the run to completion

If the session is **busy at adoption**, the snapshot reuses the existing external-reply renderer (`start_reply_render`, `external.rs:192`): the snapshot card becomes the host, armed with the epoch of the newest user message (the one being answered), and streams that external turn's parts into the card until completion (`external_turn_completed` or the hard timeout), then finalises the card. This is the **only** live behaviour; every other activation is one-shot. Existing guards apply unchanged: a cola prompt or a newer external message replaces the accumulator and the renderer exits (`external.rs:308-340`). This composes with an adopt-time pending block — an external run blocked on a permission, approved from the snapshot, resumes and streams the rest into the same card.

### Non-disturbance invariants

The snapshot never: injects a prompt or any write into the session, spends tokens, queues against a running turn, notifies another client, or produces more than one new card per activation (in `/topic --adopt` the cover root is the permanent pre-existing card, not a new confirmation).

## Why

- Takeover is exactly when the operator is blind; a read-only snapshot restores state with zero disturbance and answers the two questions asked at takeover: did the last turn end (status line), and is something waiting on me (pending blocks).
- The one-card rule keeps a Feishu thread unambiguous: every activation ends with exactly one bot message that both confirms the switch and carries the state.
- Bounded by reuse: the only new client capability is calling `GET /session/status`; pending-action and busy-follow reuse the request poller's inline-host machinery and the external-reply renderer.
- Suppression keeps re-activation between stacked lobby sessions quiet when there is genuinely nothing new — the complaint that motivated the ADR-0022 marker cleanup, without losing the reassurance of a switch.

## Alternatives considered

- **AI-generated takeover briefing** (a summary prompt injected into the session): writes history, spends tokens, risks disturbing a live turn, notifies other clients. Violates every non-disturbance constraint; rejected.
- **Live-refreshing snapshot for all activations**: extra poll machinery for little gain; subsequent events already flow through the external-message sync, the request poller, and turn streaming.
- **Embedding later-appearing pending items into a stale snapshot** instead of standalone cards: fights the existing poller's ownership of requests and adds card-patch complexity. Rejected — only adopt-time items are claimed.
- **Not claiming adopt-time pending items**: the adopt just added the session's directory to the SessionStore, so the 3 s request poller would also surface the same request as a standalone card — a duplicate. Rejected.
- **Keeping the bare 「已接管…」 text and adding a separate status card**: two messages per activation, against the one-card rule. Rejected.

## Risks / open questions

- **Suppression is a heuristic**: "newest user message is cola-authored ⟹ recent life visible here" (a session maps to at most one thread). A `--force`-stolen session whose last cola-authored messages came from the previous thread could be wrongly suppressed; rare, and the user can re-invoke `/switch`. Losing the tail there is acceptable.
- `GET /session/status` reflects the attached server instance only — consistent with the one-server Shared Store invariant (ADR-0005, ADR-0013). After a server restart an in-flight run is lost anyway, so `idle` is the correct post-restart answer.
- Patching the switch-card into a snapshot discards the session list; accepted — the action is complete.
- The busy-follow's accumulator composition with claimed pending blocks needs implementation care in `request.rs`/`render.rs` (a blocked external run approved from the snapshot then streams the rest); verify against `retain_inline` and the `submit_epoch_ms` replacement guard.
- **Child-session (sub-task) pending permissions keep today's standalone delivery** via the parent-chain walk (pitfall #11); the snapshot embeds only same-session requests. Surfacing a descendant's blockers on the parent's snapshot is possible future work.
- The tail window (four messages) and suppression heuristic may need tuning from real use; both are presentation-level constants, not protocol.

## Domain note

The concept enters the glossary as **Session Snapshot (会话快照)**; the UI verbs stay 已接管/已切换. The status chip is cola's first consumer of the server's per-session status endpoint — the same one a future "is anyone running this session" feature would build on.
