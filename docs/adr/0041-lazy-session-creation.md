# Lazy Session Creation: `/new`, `/dir` and `/topic` declare a Pending Session

Every session-selection command today creates a real backend Session immediately, so a mistyped directory, a `/new` meant as `/topic`, or a 新建 click meant as a takeover leaves a never-prompted Session in the Shared Store — visible to every client, never cleaned up by the server. This ADR moves creation to the first prompt: `/new`, `/dir`, `/topic` and their card forms write a **Pending Session**, and the conversation's first prompt materialises it.

## Context

- All session creation is eager today: `/new` (`src/bridge/command.rs:575-597`), `/dir` (`command.rs:526-568`), the `/dir` card's `pick` (`src/bridge/handler.rs:1075-1113`), the switch card's `新建` (`handler.rs:788-815`), and `/topic` Fresh (`src/bridge/topic.rs:73-95`). The only lazy paths are the first-message auto-create (`handler.rs:615-628`) and the 404-recreate, both of which create because a prompt is already there.
- The junk is store-wide and permanent. A never-prompted session appears in `GET /session` and `GET /experimental/session` with the default title `New session - <iso>`; the server has no cleanup job. OpenChamber's session list does not filter them; cola's own `/switch` list (top 15 by `time.updated` — the sort at `command.rs:1135-1139`, the cap at `command.rs:1555`) and `/dir` Recent Directories (deduped by directory, `command.rs:1183-1203`) show them too — a wrong `/dir` also pollutes the directory picker.
- The mistakes that produce them are noticed **before** the first prompt (grilling): a wrong directory, a `/new` where a topic was intended, a new session where an adoption was intended. A corrected mistake therefore only needs the eager creation not to have happened.
- Other clients already cover cleanup of what does exist: OpenChamber's session retention (`useSessionAutoCleanup`: delete or archive by age, keeping the recent 5 and the current session) and `opencode session delete`.
- Server facts: session ids are server-generated — `Session.CreateInput` has no `id` field, so a client cannot pre-name a session; a prompt to a missing session 404s; there is no create-and-prompt endpoint; archiving is `PATCH` with `time.archived`, deletion `DELETE /session/{id}`.
- ADR-0013 already made the Owned Server lazy (spawned at the moment a prompt needs it). This ADR applies the same principle one level up.
- **Amends seven existing ADRs** (each now carries an `Amended by ADR-0041` banner): ADR-0006 (`/topic` no longer creates the session at command time), ADR-0007 (`/new <name>`'s title PATCH happens at materialisation), ADR-0012 (project derivation is pending-first), ADR-0016 (plain `/topic` opens around a pending; `--adopt` unchanged), ADR-0017 (`/new` no longer promotes a session to active), ADR-0022 (switch-card scope falls back to the pending directory; 会话 stays reserved for real Sessions), ADR-0025 (建话题 writes a pending). Everything else in those ADRs stands.

## Decision

### The Pending Session

- A conversation records **at most one Pending Session**: directory, optional title, the per-session overrides (agent/model/variant/auto_accept), and — for topics — `topic_root`/`topic_anchor`. It is **not** a Session: no backend identity, invisible to every client's session list, never rendered as a turn.
- It is stored beside the mapping in the SessionStore and persisted in `~/.cola/sessions.json`. The file format becomes `{"entries":[…],"pending":[…]}`; the loader **must** also accept the legacy bare array (today's `from_str(...).unwrap_or_default()` would silently empty the mapping on a format change).
- Read rule: a thread with a Pending Session has **no Active Session** — `get_active` returns `None`. The previous session stays mapped and switchable (lobby semantics), but external-message sync stops following it and drops its Sync Watermark, which is exactly today's eager-`/new` behaviour (`src/bridge/external.rs:101-108`).
- `current_project_directory`, the switch card's current/active display, and the `/dir` card's 当前 all read the Pending Session first.

### Materialisation

- Trigger: the conversation's first **prompt** (a non-command message). cola creates the Session in the pending's directory, PATCHes the title when one was set (creation title policy, ADR-0007), applies the overrides, activates the new entry and clears the pending in one store write.
- A failed create keeps the pending intact: the error surfaces on that message and the next message retries — the existing first-message path already behaves this way.
- A first message that is a command (`/help`, `/switch`, …) does not materialise.
- The no-pending auto-create is unchanged, and the group-lobby guidance fires only there: an explicit `/new` or `/topic` already told the user what happened.

### Command matrix on a Pending Session

- `/name`, `/agent`, `/model`, `/think`, `/autoaccept` write the pending (card forms included); `/name` also patches the topic cover card.
- `/compact` and `/stop` keep today's "还没有会话" replies.
- `/switch`, `/dir`, `/new`, `/topic` replace the pending; `/switch forget` clears it together with the mapping.
- The topic gate treats a pending as **unbound**, so selection commands stay allowed in a topic whose only session is pending. This is what lets a mistaken `/topic` be corrected in place with `/switch <id>` — today the eagerly created empty session triggers `TOPIC_SELECTION_REJECTION` (`command.rs:91-94`).

### Topic cover card

- A fresh `/topic` cover shows the directory and 「下一条消息创建」 in place of the session line — the 会话 noun stays reserved for a real Session (ADR-0022). After materialisation the existing cover-sync path patches it to the full brief (title, session id, model). Fallback anchoring and the quote-injection guard (ADR-0023) are unchanged.

### Non-goals

- Sessions that were **prompted** by mistake are never touched: no auto-delete, and no delete/archive command is added by this ADR.
- Legacy never-prompted sessions are neither migrated nor swept; OpenChamber's retention and `opencode session delete` remain the cleanup path.

## Why

- The junk class disappears at the source instead of being chased: `/new`, `/dir` and `/topic` stop writing to the Shared Store, so a corrected mistake leaves no trace in Feishu, OpenChamber, or cola's pickers.
- Since mistakes are noticed before the first prompt, the pending never materialises for them; the Session appears exactly when the conversation speaks — which is also when the server generates its own title.
- The commands no longer need a live server: `/new`/`/dir`/`/topic` become local intent writes, and the first prompt is the single moment a server is demanded, composing with Lazy Start (ADR-0013).
- A pending topic can be re-pointed at a real session in place, closing the "wanted a takeover, got a topic" dead end.

## Alternatives considered

- **Eager creation + recycle-on-replace** (delete a conversation's replaced zero-message sessions at activation): only cleans what gets replaced — abandoned orphans stay forever — leaves the junk visible until the correction, and duplicates OpenChamber's age-based retention. Rejected.
- **Eager creation + a draft marker + background sweeper**: keeps the transient junk, needs a deletion policy and a background deleter with real deletion risk, and adds a second cleanup authority beside OpenChamber's. Rejected.
- **Accept and hide empty sessions in cola's lists only**: the Shared Store and OpenChamber stay dirty, and hiding needs its own emptiness heuristic. Rejected as the primary fix.
- **Deferring everything except `/topic`** (to keep the session id on the cover card): keeps the "mistaken topic" junk class alive — one of the three motivating mistakes. Rejected.
- **Keeping the previous session followed while a pending exists**: contradicts the meaning of `/new` (you moved on) and diverges from today's semantics. Rejected — parity chosen deliberately.
- **Migrating legacy zero-message active sessions into pendings on upgrade**: silently deletes user-visible server state; too surprising. Rejected.

## Risks / open questions

- The `sessions.json` format change must be loaded defensively; the current loader (`src/bridge/session.rs:12-23`) silently falls back to empty on any parse error.
- Every consumer of "the current session" must read through the pending-aware accessors (message path, `current_project_directory`, switch/dir card current, external sync's active computation, `/name` target, `/autoaccept` status). A missed consumer is a bug where the pending is invisible to that path.
- The cover card's 「下一条消息创建」 state is a visible, if brief, degradation: the session id appears only after the first prompt.
- `/new` followed by the first prompt still issues create-then-prompt; the total work is unchanged from today's first message, but both calls now land at prompt time — with Lazy Start, that prompt may also wait for the server spawn (see #99).
- Whether `/switch list` should show the pending as a row is a presentation question, not required for correctness.

## Compatibility

Forward compatibility is guaranteed: the dual loader (see the Decision) still
accepts the legacy bare array, and an unreadable file warns and starts empty.
Backward is not: an older binary parses the new object with its old bare-array
loader, the parse fails, and its thread→session mapping comes up empty. The
release that ships this format change must carry a note about it in its
release notes.

## Domain note

Glossary: **Pending Session** and **Lazy Session Creation** (the session-side counterpart of **Lazy Start**); **Active Session** is now at-most-one and absent while a Pending Session exists, and the **Project** derivation checks the Pending Session first. The UI avoids the noun 会话 for the pending state and uses verb phrases (「下一条消息创建」).
