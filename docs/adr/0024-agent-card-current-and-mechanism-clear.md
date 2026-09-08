# `/agent`: card shows the current agent; clearing is a mechanism, never a value word

`/agent` (card + text) records a per-session agent override sent as `PromptInput.agent` on the next message (the server has no agent-switch endpoint). The card previously listed every available agent but never said which agent is currently answering, and there was no way to drop the override back to the server's default once set (ADR-0020's decoupling for `/think` made the gap visible). This ADR makes `/agent` show the current agent and gives clearing a dedicated mechanism — extending ADR-0020's "clearing is a mechanism, never a value word" rule to the agent picker.

## Decision

- **The card shows the current agent.** The intro renders the per-session override when set, else the server's default agent name annotated `（默认）`. The default agent is derived from the existing `GET /agent` response — no `/config` round trip: opencode's `agent.list()` already sorts the configured default (or `build`) first (Agent.Service, `sortBy`), and the server's own `defaultInfo` fallback is "the first primary (non-subagent), non-hidden agent in that order". cola mirrors exactly that: `server_default_agent` returns the first agent whose `mode != "subagent" && hidden != true`. This is the agent that actually runs when no override is set, because a prompt without `input.agent` resolves to the server's default agent — not the session's last-recorded agent.
- **Clearing is a mechanism.** The card's "默认（清除）" button carries its OWN `agent_clear` action tag (distinct from the option buttons' `agent`), and the text form clears via `/agent --reset`. An agent literally named `default`/`off`/`reset` (or `--reset`) is a normal, selectable pick — the value namespace holds only real agent names. No reserved word is overloaded and nothing depends on upstream conventions.

## Considered options

- **Reuse the reserved word `default` as the clear verb** (like the pre-refactor `/think`): rejected — agent names are user-authored files, so `default` is a plausible real name; overloading it would make such an agent unselectable. The same reasoning that drove ADR-0020's refactor applies a fortiori to agents.
- **Fetch `GET /config` to read `default_agent`**: feasible (`ConfigV1.Info` carries it) but unnecessary — the sorted `GET /agent` list plus the `defaultInfo` fallback rule yields the same name with data cola already fetches for the card.

## Consequences

- `build_agent_card` gains `override_agent`/`default_agent` parameters and a leading `agent_clear` button via `picker_card`'s shared clear seam (ADR-0020). The empty-agent degrade carries no clear button.
- `send_agent_card` now routes through a shared `agent_card` renderer (like `think_card`), so the text send path and the card-ack refresh show the same current agent; a thread with no active session gets a text nudge instead of a useless card.
- `Command::Agent` and `handle_agent_card_action` treat `--reset`/`agent_clear` as a clear; `is_reset_flag` (formerly `is_think_reset`) is now the one flag definition shared by `/think` and `/agent`.
- Adoption (`/switch`, `/topic --adopt`) still copies the server-recorded session agent into the entry's override, which now simply reads as the current override on the card.
