# cola

A bridge bot that brings the OpenCode AI coding experience into Feishu.

## Where the rules live

This file is the index. It names the document that owns each rule, and carries
only what every session needs before opening one — the branch loop, the commit
trailer, the handoff note, and the pitfalls below. Where this file and an owner
disagree, the owner wins.

- **The change contract** — branch loop, commit conventions, the verification
  loop, PR rules, the Codex gate, coverage and security policy, releasing —
  `CONTRIBUTING.md`
- **Code conventions**, the source of the `/code-review` skill's Standards axis —
  `CODING_STANDARDS.md`
- **Which skill runs when** — the main flow, the bug/issue on-ramps, the local
  bindings — `docs/agents/flow.md`
- **Repo-local skills** — shadowing, where a skill lives, compose-vs-fork, the
  upgrade discipline — `docs/agents/skills.md`
- **The issue tracker** — `gh` conventions, specs and tickets, wayfinding —
  `docs/agents/issue-tracker.md`
- **Triage labels** — `docs/agents/triage-labels.md`
- **Domain docs** — `GLOSSARY.md`, `docs/adr/` — `docs/agents/domain.md`
- **Reference trees and upstream docs** (OpenCode, cc-connect, OpenChamber,
  Feishu, Lark) — `docs/agents/references.md`

Two rules outrank the rest. **Review is always the pair**: `/code-review` carries
both the two-axis review and the adversarial pass, and `docs/agents/flow.md` is
what binds *when* it runs. And a skill is invoked as **`#<id>`** in a Feishu
message — never `@skill` (Feishu turns `@` into a person mention); `/skill` opens
the picker and `/skill <id>` is its alias (ADR-0077).

## Branch loop

`main` is the only long-lived branch and always the base. Start on `main`, branch
off it (`<type>/<slug>`), work in small Conventional Commits, rebase onto the
latest `main`, open a PR — and **stop at a green PR: agents never merge a
feature/task PR.** The user verifies and merges; the release cut, `cargo xtask
release`, is the one exception (ADR-0033). Check `git branch --show-current`
before editing; the full contract is `CONTRIBUTING.md`.

Every agent commit ends with one trailer naming the model —
`Co-authored-by: opencode (<Model Display Name>) <noreply@opencode.ai>` —
resolved from the session, never hardcoded (`CONTRIBUTING.md` "Agent
attribution").

Run `CONTRIBUTING.md`'s verification loop before pushing. The git hooks
(lefthook, installed automatically by the first `cargo build`) run the cheap
subset; scope the workspace runs while iterating, since each costs a minute-plus.

## Handoff

A handoff document for continuing this project lives at
`/tmp/opencode/cola-handoff.md`. That path is host-local and outside the repo, so
it does not travel with a clone — read it when it exists, and regenerate it with
the handoff skill when it is missing or stale.

## Known pitfalls (learned the hard way)

1. **Two API generations are mounted: unprefixed = V1 compatibility, `/api` =
   current (announced as V2).** Older ADRs (0001/0007/0026) wrote `/api/*` for
   the *other* generation — the prefix's meaning flipped upstream, so read them
   with that in mind. Check a route's generation and payload shape before using it.
2. **OpenCode event JSON fields are camelCase** (`callID`, `sessionID`,
   `assistantMessageID`, `textID`, `reasoningID`). Serde structs need
   `#[serde(rename = "...")]` — a missing rename silently nulls the field (e.g.
   tool panels never rendered).
3. **Permissions live at the global endpoints**: `GET /permission` (list all
   pending) and `POST /permission/{requestID}/reply` with `{reply}`. The old
   `/api/session/{id}/permission` returns empty — permission cards never appear
   and the AI hangs on a tool permission.
4. **Model is optional — unset means "use the server default"**: cola pins a
   model only when `[opencode] model` is set in `cola.toml`, and it must then
   exist on the attached server (usually `opencode/...`, not OpenChamber-only
   providers like `opencode-go/...`). Unset is the safest option on a new machine.
   V1 has no model-switch endpoint, so `/model` is stored and sent per prompt; on
   V2 `/model`, `/think` and `/agent` are durable session switches
   (`POST /api/session/{id}/model|agent`, the variant inside the model ref) that
   cola mirrors locally for display.
5. **Feishu WS frames are binary protobuf (pbbp2)**, parsed by hand in
   `src/feishu/pbbp2.rs`. A card button callback must be acked
   (`{"code":200,"headers":null,"data":"<base64>"}`) or Feishu shows
   "目标回调服务超时".
6. **Feishu resends events at-least-once**: dedupe on `header.event_id` and drop
   events older than 5 minutes.
7. **Sessions are shared through the default store — one store, many clients**:
   cola attaches to whatever `opencode serve` runs on the default store
   (`~/.local/share/opencode`; `XDG_DATA_HOME` unset), whoever started it,
   reading port + password live from `/proc`. With none running it self-starts
   one there (`bridge/discovery.rs`). A private data dir silently breaks sharing;
   a mapped session that 404s is a leftover from another store and is recreated
   automatically. (On OpenChamber's server the global SSE connection is ended
   every few seconds — cosmetic; cola renders by polling.)
8. **Ack EVERY WS event, not just card callbacks**: the Lark SDK replies
   `{"code":200,"headers":null,"data":null}` to each `MessageTypeEvent`. An
   unacked event is re-delivered forever, and a client that never acks is
   eventually treated as dead and stops receiving new events entirely (symptom:
   bot silently unresponsive, TCP still ESTAB). Answer pbbp2 "ping" frames with
   "pong", and keep a read timeout + proactive ping so a half-dead connection
   triggers a reconnect (`handle_connection`, `src/feishu/ws.rs`).
9. **OpenCode part payloads have NO `id`**: a part's id is a database column that
   is not serialised into the part JSON
   (`{"type":"text","text":"...","time":{...}}`). Dedupe text/reasoning on
   **content**, never on an `id` — a poll-keyed dedupe never fires and the text
   doubles (81→162 chars) (`render_new_turn_parts`, `src/bridge/turn/render.rs`).
10. **Feishu @mentions arrive as opaque `@_user_N` tokens**:
    `message.mentions: [{key:"@_user_1", id:{open_id}, name}]`. Unparsed, they
    leak into the prompt and the AI cannot see who was referenced. Strip the
    bot's own mention (needs `bot_open_id` from `GET /open-apis/bot/v3/info`) and
    replace others with `@名字` (`strip_mentions`, `src/feishu/message.rs`).
    Feishu only pushes group messages that @ the bot — a server-side permission,
    not fixable in cola.
11. **Sub-task (task tool) permissions carry the CHILD session id**: the `task`
    tool gives each subagent its own child session, so its tool permissions and
    list_questions come back with a `sessionID` that is not in cola's
    SessionStore → no chat mapping → the card is silently dropped ("No reply
    target or chat"). Resolve the target by walking the parent chain via
    `GET /session/{id}` (`resolve_card_target`, `src/bridge/pollers.rs`), and
    carry the owning `directory` on the permission card so `handle_card_action`
    can route the reply without the SessionStore map.
12. **Server Basic auth needs BOTH username and password**: the username is
    `OPENCODE_SERVER_USERNAME` (default `opencode`), and `authorized()` in
    opencode's `auth.ts` compares it too, so a wrong or missing username 401s
    even with a correct password. cola's client sends an Authorization header
    only when both are set, so discovery must supply the username
    (`src/bridge/discovery.rs`, `username_from_env`). cola's own self-started
    server strips an inherited `OPENCODE_SERVER_USERNAME` to keep the default.
13. **Feishu does NOT truncate long markdown elements at 3000 chars**:
    `MAX_ELEMENT_TEXT_CHARS = 3000` (`src/feishu/card/mod.rs`) is cola's own
    card-size budget, not a platform per-element cap — measured 2026-09-18, an
    8000-char ASCII and a 3060-char CJK element both render fully through create
    and PATCH, against documented limits of 30KB total and 200 components. So a
    tool panel over 3000 chars (`tool_panel_element`, max ~3.4k) is not a bug to
    "fix" by splitting.
14. **V1 answers `GET /api/info` with `200 text/html`, not 404**: V1's web-UI
    catch-all serves `index.html` for every unknown path, so the status alone
    cannot tell the generations apart (measured 2026-09-27 on the running
    1.18.23 and the pinned 1.18.31). Attach detection
    (`src/opencode/generation.rs`) follows one rule: **200 + V2's JSON info
    envelope (a `version` string and a `urls` array) = V2; 200 `text/html` = V1
    (the catch-all); 404 = V1; a 200 without the envelope / 401 / 503 / a
    transport error = inconclusive** — cola stays serverless (never guesses,
    never spawns a second server against the same store). The
    `[opencode] generation` override is the escape hatch for proxies and odd
    builds.
