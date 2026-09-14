# Multi-user collaboration waits for a server-mode deployment

We designed how Guests could share and drive sessions on a personal machine, then concluded every cola-level boundary is organizational, not a security boundary: OpenCode's default agent auto-allows edit/read/bash, so a Guest driving the agent effectively holds the Host's OS-user powers. Real isolation needs OS/container separation that only a hosted deployment provides. The personal-machine bot ships Host-only (ADR-0035); the multi-user model and the findings that deferred it are recorded here as the server-mode blueprint.

## The deferred model (server-mode blueprint)

- **Roles**: Host, Guest; per-person `open_id` identity via a persisted Access List; admission by self-request + Host approval card (identity captured from the callback — nobody types ids). Per-Chat admission policy: person-by-person by default, configurable to "join = admitted".
- **Visibility vs operation**: visibility belongs to the Chat (Feishu membership decides who sees cards — cola cannot hide them per person); a Share grants *operation only*. A Share targets a Principal (operable anywhere) or a Chat (operable only inside that Chat). No view-only Share (observation needs no grant) and no "person within a Chat" grant.
- **Adoption**: a Session lives in one thread; adoption moves it. The Host may adopt anything anywhere (into a group: shared to that group by default); a Guest may only adopt Sessions they created whose directory is already shared to the target Chat (no expansion). `--force` is Host-only.
- **Defaults**: new Sessions created in a group are group-shared; p2p Sessions are private. Multiple Sessions per group are allowed, isolated by topics; Sessions belong to people, not to groups.
- **Directories**: the Host shares explicitly; Guests may request. The sandbox is the set of shared directories.
- **Approval**: the Session's owner approves its permissions; `Always` (backend-global) and `/autoaccept` are Host-only.
- **Attribution**: who prompted and who approved is recorded and displayed.
- **Cost**: Guests inherit the Host's model/agent/thinking; no per-Guest config initially.

## Findings that forced the deferral (verified against the source, 2026-09-14)

- OpenCode's build-agent defaults auto-allow `edit`/`write`/`read`/`bash`; cola never sees those tool calls, so it cannot intercept them. Only `external_directory` (paths outside the worktree) is asked by default.
- **Session-level rules** exist (`PATCH /session/{id}` with a `permission` ruleset): the patch *appends* (`Permission.merge` is array concatenation), evaluation is last-match-wins, the ruleset is persisted on the session row and shared by every client, and there is no API to clear it. A patch therefore binds a restriction to the Session for everyone — including the Host — irreversibly. It does propagate to subagent child sessions (only session-level `external_directory`/`deny` rules are inherited).
- **Agent-level rules** follow the actor (the turn's agent is resolved from the last user message) and can be defined in config, but subagents do not inherit agent-level rules (a `task` escape), agents are config-file-only (a missing agent hard-errors), and cola must force the agent on every Guest prompt.
- Path detection is partial in every in-process scheme: `external_directory` only fires for file tools and a fixed command set; `curl`, `git`, variable expansion and subshells bypass it.
- Net: on a personal machine a Guest is someone holding an OS account via the agent; the sandbox is a guardrail at best.

## Decision

Do not build Guest sharing, group collaboration, or sandboxing on the personal-machine bot. When a server-mode deployment exists (hosted OpenCode, per-Guest isolation via OS user/container, multi-tenant store), implement the blueprint there, with a real boundary. Re-verify OpenCode's permission semantics then — they may have changed.

## Alternatives considered

- **Session-level sandbox for Guest-created sessions only**: confuses co-driven Sessions (the Host loses full power in them), and patches are irreversible; rejected.
- **Agent-level sandbox**: subagent escape + config dependency + per-prompt discipline; rejected.
- **Actor-aware routing of `external_directory` asks** (the Host's asks surface; a Guest's are rejected by cola): cheap and per-actor, but still bypassable (`curl`/`git`) and moot once Guests were deferred; kept as the likely first hardening in server mode if a lightweight boundary is wanted before containers.
- **Trust-based sharing with no boundary**: rejected — opening a personal machine's OpenCode to others is too dangerous to ship as a feature.
