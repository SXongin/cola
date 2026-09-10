# Force takeover from the session card

When a `/switch` card button targets a session owned by **another** chat, the
handler refuses the steal and tells the user to type a command. That command
required a session id the card never shows, so the escape hatch was unusable.

## Context

- The `/switch` session card (ADR-0012, ADR-0022) renders each row with two
  buttons: `adopt` (接管/切换) and `topic_adopt` (建话题接管). By design the
  buttons carry **no** `--force` (ADR-0016): an occupied session was rejected
  with a Toast pointing at the text form —
  `/switch <ID> --force` or `/topic --adopt <ID> --force`.
- The ID in that Toast was unobtainable. The card shows `id_tail` — the first
  7 characters of the session id **after stripping `ses_`** — and nothing else.
  `resolve_session`, which backs the `--force` forms, matched only prefixes of
  the **full** id (`ses_…`). So the visible hash (`1a2b3c4`) never matched, and
  prepending `ses_` was a trick the user had to discover. The plain
  `/switch <kw>` path did substring-match the id, but it also refuses occupied
  sessions and points back at `--force` — a dead loop.
- Net effect: "force take over" was only reachable by guessing an argument the
  UI deliberately hides. The card affordance the user already clicked was a
  dead end.

## Decision

1. **Confirm the steal on the card.** When `adopt` or `topic_adopt` targets a
   session owned by another chat, patch the card in place to a force-confirm
   card that names the owner Chat/Topic and warns that the old owner loses the
   session. Its danger button re-enters the handler as `force_adopt` /
   `force_topic_adopt` carrying the **full** session id; a 返回列表 button
   rebuilds the session list. The force op skips the owner check and runs the
   normal adoption or topic-creation path, whose `set_active` drops the old
   owner's mapping (the same steal as the text `--force`). Because the steal is
   the mapping write itself, a failed topic creation leaves the old owner
   mapped rather than stranding the session.
2. **Make the displayed hash resolvable.** `resolve_session` accepts, in
   precedence order, the exact id, a prefix of the full id, a prefix of the
   `ses_`-stripped id (the card hash), or a suffix of the id's tail. A tier that
   matches decides before the next is tried, so a prefix hit is never made
   ambiguous by a coincidental suffix hit. So the text forms
   (`/switch <hash> --force`, `/attach`, `/topic --adopt`) accept the card hash
   too — the button and the command agree on what identifies a session.

## Why

- The user is already on a card; the honest affordance is a second click, not a
  remembered command with an id the card withholds.
- The steal is destructive to another Chat/Topic, so it stays an explicit
  confirmation (owner named, warning shown) rather than firing on the first
  click. The two-click cost is the safety.
- Fixing the resolution as well keeps the text forms first-class for power
  users and removes the hidden `ses_`-prefix knowledge from every adopt surface.

## Alternatives considered

- **Keep the Toast, print the full id**: full ids are 29 characters and clutter
  the compact rows; the user still has to type a command from a card.
- **Force immediately on the first click**: no confirmation that the steal
  deprives another chat of its session. Rejected.
- **Only fix `resolve_session`, keep the Toast**: removes the dead loop but
  still requires command typing and arg memorisation — the actual complaint.
- **Show a suffix-only hash and match suffixes only**: the displayed prefix is
  time-derived; matching both prefix and suffix covers every shape a user might
  quote without changing what the card renders.

## Risks / open questions

- **Stealing an actively-driven session** is still allowed (as with the text
  `--force`, ADR-0008/ADR-0016); the confirm card names the owner but does not
  detect a live run. Accepted, matching the existing text-form risk.
- The 7-character display hash is time-derived, so two sessions created within
  the same ~256 ms share it; the ambiguous case lists candidates instead of
  picking one. Rare in practice.
- The force-confirm card replaces the list in place; 返回列表 rebuilds it
  (keyword/scope preserved from the click that opened it — the row buttons carry
  scope, not keyword, so a search keyword is not restored). Accepted.

## Domain note

Supersedes the card half of ADR-0016 ("the card form does not honor `--force`");
the text `--force` semantics are unchanged.
