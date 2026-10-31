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

Each pitfall has a **slug**, and the rest of the docs cite it as *pitfall* + that
slug (e.g. *pitfall attach*), never by position; ADR-0084 records the convention
and the map from the old numbers.

- **`generations`** — Two OpenCode API generations are mounted: the unprefixed
  routes are **V1** (`src/opencode/v1/`), `/api/...` is **V2**
  (`src/opencode/v2/`). Older ADRs (0001/0007/0026) wrote `/api/*` for the
  *other* generation — the prefix's meaning flipped upstream, so read them with
  that in mind. A per-generation **strategy** owns its generation's routes, so a
  call is generation-specific even where the path is shared, and the split is not
  cosmetic: permissions are the bare global `GET /permission` +
  `POST /permission/{id}/reply` on V1 (whose old per-session list returned empty,
  which is why the list is global there) but location-scoped
  `GET /api/permission/request?location[directory]=…` + session-scoped
  `POST /api/session/{id}/permission/{requestID}/reply` on V2; and V1's
  `switch_session_model` is deliberately a **no-op** — its model rides each
  prompt — where V2's is durable. Check the generation before adding a route call.
- **`wire-shapes`** — OpenCode event JSON is camelCase (`callID`, `sessionID`,
  `assistantMessageID`, `textID`, `reasoningID`), so serde structs need
  `#[serde(rename = "...")]` — a missing rename silently nulls the field. Part
  payloads carry **no `id`** (it is a database column, not serialised), so dedupe
  text and reasoning on **content**: a poll-keyed dedupe never fires and the text
  doubles.
- **`codex-request-ids`** — Codex's app-server numbers its **server-initiated**
  requests per connection and restarts the counter on every reconnect, so the bare
  wire id (`0`, `1`, …) does not identify a request across connections. The bridge
  keys long-lived process state by the neutral request id — the double-click
  guard, the poller's `seen` set, snapshot claims, persisted card surfaces — so a
  reused id aliased a request an *earlier* connection had already answered: the
  card's click was re-served as "already answered", and for an inline question
  that replay was a completely empty ack (no card, no toast), so the option
  button did nothing at all (#896; the sibling wedge of #893). The adapter mints
  the neutral id with its connection's random tag (`n:0@3f9a1c2b`,
  `Approvals::request_key`, `src/codex/approvals.rs`); never mint or hardcode a
  bare id, and never make an "already handled" replay carry neither feedback.
- **`attach`** — **One store, many clients**: cola attaches to whatever
  `opencode serve` is on the default store, whoever started it, and starts one of
  its own only when `start_server` allows it (`auto` by default, `never` for
  attach-only, ADR-0013). Never use a private data dir — it silently breaks that
  sharing — and a mapped session that 404s is a leftover from another store,
  recreated automatically. Discovery resolves each server process through
  `sysinfo`: a classic `serve --port P` keeps its port in argv and its credentials
  in the environment (`OPENCODE_SERVER_PASSWORD`, and `OPENCODE_SERVER_USERNAME`
  on V1 only), while a V2 `serve --service` has no `--port` and is reached through
  its registration file (`$XDG_STATE_HOME/opencode/service.json` — the server's
  own state home, not cola's), always with the username `opencode`. The attach
  forces `opencode` on V2 once the generation is classified, but the *probe* still
  authenticates with the environment value, so a foreground V2 server with it set
  can probe inconclusive (#763). Either way Basic auth needs **both** halves — the
  server compares the username, so a missing or wrong one 401s even with a correct
  password. (On OpenChamber's server the global SSE connection is ended every few
  seconds — cosmetic; cola renders by polling.)
- **`ws-lifecycle`** — Feishu WS frames are binary protobuf (pbbp2), parsed by
  hand in `src/feishu/pbbp2.rs`; answer "ping" frames with "pong" and keep a read
  timeout, so a half-dead connection reconnects (`handle_connection`,
  `src/feishu/ws.rs`). **Ack every event**: a card callback needs its ack frame or
  Feishu shows "目标回调服务超时", and an unacked event is re-delivered forever
  until the client is treated as dead and stops receiving events at all (symptom:
  the bot goes silent while its TCP connection still reads ESTAB). Message
  delivery is at-least-once, so `im.message.receive_v1` is deduped on
  `header.event_id` and dropped when its `create_time` is older than 5 minutes —
  both optional fields, so a payload carrying neither passes through.
- **`mentions`** — Feishu @mentions arrive as opaque `@_user_N` tokens, so a
  prompt leaks `@_user_1` and the AI cannot see who was referenced unless they are
  resolved: strip the bot's own mention (it needs `bot_open_id` from
  `GET /open-apis/bot/v3/info`) and replace the rest with `@名字`
  (`strip_mentions`, `src/feishu/message.rs`). Feishu also only pushes group
  messages that @ the bot — a server-side permission, not fixable in cola.
- **`child-session-permissions`** — the `task` tool gives each subagent its own
  child session, so its tool permissions and questions come back with a
  `sessionID` that is not in cola's SessionStore → no chat mapping → the card is
  silently dropped ("No reply target or chat"). Resolve the target by walking the
  parent chain (`resolve_card_target`, `src/bridge/pollers.rs`), and carry the
  owning `directory` on the card so the reply routes without the map
  (`handle_card_action`, `src/bridge/request/flow.rs`).
- **`card-budgets`** — `MAX_ELEMENT_TEXT_CHARS` and its neighbours
  (`src/feishu/card/mod.rs`) are **cola's own** budgets, not Feishu limits, and
  the doc comment there holds the evidence. A tool panel deliberately exceeds
  3000 chars, so its size is not a bug and splitting it buys nothing.
- **`card-repaints`** — every card PATCH rebuilds `body.elements`, and a form
  input's unsubmitted text lives only in the client, so **any** repaint wipes
  what the operator is typing (issue #917). Two rules follow. A card that
  renders an unanswered free-text answer field (`awaits_typed_answer`) is never
  repainted for **background progress** — the live header's second, the
  ledger's rows, its clock and its completion entries, a task panel's activity,
  the yielded read as a whole. The read must not be **consumed** either: plan
  no entry, apply no ledger, spend no liveness read while the form waits, or
  every later read sees "no difference" and the fact never reaches a card. The
  turn's own content and the settlement still flush. And every card write must
  advance the render gate's signature cache: the flush does it
  (`advance_sent_signatures`, `src/bridge/turn/flush.rs`) and so must an
  out-of-band write that restamps the header (`Turn::mark_header_shown` for a
  click's ack, once the eager patches landed). The cache records what the card
  last SHOWED, so a stale one makes the next render tick re-PATCH an identical
  body — the "it refreshed by itself and ate my text" report.
- **`info-probe`** — the generation is probed with `GET /api/info`, and the
  status alone cannot classify it: V1's web-UI catch-all serves `index.html` for
  every unknown path, so V1 answers 200 `text/html`. The rule — 200 + V2's JSON
  info envelope = V2; 200 `text/html` or 404 = V1; anything else, including 401,
  503 and a transport error, = inconclusive — is `src/opencode/generation.rs`'s,
  evidence included; `[opencode] generation` overrides it.
