# Pitfalls are cited by slug, not by position

## Context

`AGENTS.md` "Known pitfalls" was a numbered list, and 49 references across
twenty-five files cite those numbers: four ADRs, six `docs/research/` notes, ten
`src/` files whose comments point at a pitfall by number, and five files inside
the frozen `.scratch/` archive — which the tracker policy forbids editing, so
those references can never be repaired in place.

A number is a position, not an identity. Consolidating two pitfalls, deleting
one, or reordering the list silently repoints every citation at a different
rule — and one research note cites the whole range (`#1–#13`), which any edit
invalidates. The churn is live, not hypothetical: a pitfall 15 was added and
then deleted under the old scheme, and every reference to it had to be found and
removed by hand.

## Decision

Each pitfall carries a **slug** — a short, stable, kebab-case identifier — and
every other document cites it by *pitfall* + that slug (e.g. *pitfall attach*),
never by a position. The list keeps an order for reading, and that order is free
to change; so is the number of items.

The eight slugs:

| slug | the trap |
| --- | --- |
| `generations` | the route, the envelope and the model-switch semantics differ per **Generation** |
| `wire-shapes` | camelCase fields need `#[serde(rename)]`; part payloads carry no `id` |
| `attach` | one store many clients, and Basic auth needs both the username and the password |
| `ws-lifecycle` | pbbp2 frames, ack every event, ping/pong, at-least-once delivery |
| `mentions` | `@_user_N` tokens must be resolved before a prompt sees them |
| `child-session-permissions` | a subagent's permission carries the child session id, so the card needs the parent walk |
| `card-budgets` | the 3000-char element budget is cola's, not Feishu's |
| `info-probe` | V1's catch-all answers 200 `text/html`, so the status alone cannot classify a server |

Renaming a slug is the one breaking change, and it is deliberate: a rename means
the rule was re-scoped, so its citations deserve a look.

## The map, for records that cite the old numbers

| was | is |
| --- | --- |
| #1 two generations, #3 permissions, #4 model switch | `generations` |
| #2 camelCase, #9 part payloads have no `id` | `wire-shapes` |
| #7 shared store, #12 Basic auth | `attach` |
| #5 pbbp2 and the callback ack, #6 at-least-once, #8 ack every event | `ws-lifecycle` |
| #10 @mention tokens | `mentions` |
| #11 child-session permissions | `child-session-permissions` |
| #13 the 3000-char element budget | `card-budgets` |
| #14 the `/api/info` probe | `info-probe` |

`.scratch/` keeps its pre-slug numbers and stays frozen; this table is why they
still mean something. `docs/agents/issue-tracker.md` points here from its archive
section.

## Consequences

- Consolidating, splitting or reordering the list no longer touches a citation.
- A citation is now self-describing: *pitfall attach* says which trap without
  opening `AGENTS.md`.
- The list drops from fourteen items to eight: `generations` absorbs three,
  `wire-shapes` and `attach` two each, and `ws-lifecycle` three.
- Three factual errors go with the rewrite, all of them repointing rather than
  changing behaviour: permissions were described as global (true of V1, not of
  V2, whose list is location-scoped and whose reply is session-scoped), the
  server's credentials were said to be read from `/proc` (discovery uses
  `sysinfo`), and V1 was said to have no model switch at all (`switch_session_model`
  exists and is deliberately a no-op).
