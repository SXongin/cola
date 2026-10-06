# One card ownership verdict

## Context

"Who owns this Session's Card Chain right now, and may I touch its card?" is
asked in four places, and each one answers it by reading raw state itself:

- the prompt router decides Supplement vs new Turn from the in-flight guard or
  a render-owned card chain (ADR-0062's routing key);
- the Wake step asks the same pair before rendering a continuation, and asks a
  third question — the yielded-card write admission (a Waiting card, live, with
  no handoff owed) — before resuming a card in place (ADR-0066);
- the same admission gates the Background Task Ledger refresh and the runtime
  reconciliation (ADR-0060);
- the out-of-turn settle loop carries its own ownership ticket — the
  accumulator's Turn anchor, or the card's chain identity — and re-checks it
  atomically with the ending stamp (#539);
- the durable reap derives its own claim from the in-flight guard plus a
  pending-inbound message, and carries an essay explaining why it cannot use
  the router's ownership and how the two nevertheless agree.

Every ownership rule already has a single *definition* (the render-owned
predicate on `CardState`, the admission on the card session), but the
*question* is re-derived at each site from different raw state, so the
agreement between sites is an argument in prose, not a property of the code. A
changed rule — or a new loop kind — must be mirrored across modules, and a
missed mirror is invisible until it bites in a live session: a Supplement
stranding with no card to continue (the #428 incident), a Wake double-rendering
onto a card a renderer owns, or a reap PATCHing a card this process still
holds. Answering "may I touch this card" means reading four modules together.

The architecture review (2026-10-05, candidate 3) named the deepening; the
design round settled the shape. This ADR records it. This step of the refactor
is behavior-preserving: it lands the verdict whole and moves the routing rule
onto it; the remaining rules are consumed by the follow-up tickets of spec
#545.

## Decision

**A `CardOwnership` verdict, computed by one read in a new `ownership` module
in the Turn layer, answers the ownership question for every site.**

- **One read, no new lock.** `CardOwnership::read` reads the waits state
  first — the in-flight `inflight` guard, then the pending-inbound claim
  through `inbound_pending` (an entry past `INBOUND_CLAIM_TTL` reads as absent
  and is dropped; the expiry stays invisible and, being inside the read, is
  now uniform across every site that reads a verdict) — and then the card map.
  The two are taken sequentially, never nested, and the verdict introduces no
  lock of its own. No server read, no platform call, no I/O.
- **The verdict is a product, not a mutually exclusive state.**

  | half | values |
  | --- | --- |
  | in-process **claim** | none / inbound (a message being routed, #424) / guard (a Turn or its inherited follow) |
  | current **card class** | absent / render-owned / yielded `{ live, handoff_owed }` / restart-stamped / ended |

  The combination is deliberate: guard + yielded and inbound + render-owned
  both occur, and the reap's race needs "no card but claimed" to be
  expressible. `yielded` carries the card's write-readiness — still the live
  card (`live`) and owing no split (`handoff_owed`) — which is exactly the
  admission ADR-0060/ADR-0066 grant.
- **The identities travel with the verdict.** The card's message id, the
  accumulator's Turn anchor and the card's chain identity are read once, in
  the same card-map pass, so the rules that compare identities (the reap's
  record matching, the settle ticket's `covers`, the Wake loop's guard) consume
  one value instead of reopening the map.
- **The rules are named methods, and their differences are declared.**

  | site | rule | deliberate difference |
  | --- | --- | --- |
  | prompt router / Wake gate | owned when a guard is held or the card is render-owned; the label is `guard` / `card-chain` (ADR-0062) | the pending-inbound claim is **ignored**: a message still being routed owns no card to split (the #428 strand) |
  | yielded write admission (ledger refresh, runtime observation, in-place Wake resume) | yielded ∧ live ∧ no handoff owed (ADR-0060, ADR-0066) | render-owned is a card *class*; the admission is that class's write-readiness |
  | durable reap claim (ADR-0063) | guard ∨ **inbound** | the reap must count the pending message or it races the admission; it keeps its record-relative probe (record match, terminality, pending ending write, successor anchor) in the Chain Record module and consumes the claim |
  | settle ticket `covers` (ADR-0059, #539) | the ticket's identity variant (a landed Turn's anchor / a Wake continuation's chain / the unlanded watch's chain) | which loop owns the card is the ticket's own kind |
  | `/stop` disposition (#394) | render-owned → the owner stamps; yielded → the quiet-end ack; else nothing | the ack exists only where a stamp cannot land promptly |

- **The routing rule's label cannot drift from its predicate.** The rule's
  return value carries the label the router logs (`ownership=guard` /
  `ownership=card-chain`), so the INFO line reads the same value that produced
  the decision.

## Considered options

- **Keep the per-site reads and pin their agreement with tests.** Rejected:
  the #428 class of bug is a missed mirror, and prose plus per-site tests
  cannot make two derivations agree by construction.
- **Make the `CardState` predicates the shared interface.** Rejected: the
  predicates are rendering/recovery vocabulary with other readers (the header,
  the recovery actions, the reap's word), and the ownership question also
  needs the guard and the pending claim, which no card state carries.
- **Make the verdict mutually exclusive (one state).** Rejected: ownership is
  genuinely a product — a guard can sit beside a yielded card, a pending
  message beside a render-owned one — and collapsing it would force a priority
  into the classification that the sites disagree about on purpose.
- **Move the card map's raw state behind the verdict.** Rejected for this
  step: card addressing (continuation replies), Turn-anchor arming/capture and
  the stalled-poll comparison are not ownership questions; they keep their
  accessors (spec #545's migration scope). The ownership-domain accessors
  (`card_is_owned`, `card_is_waiting`, the reap's claim) migrate as their
  tickets land.

## Consequences

- The routing verdict type (`ChainOwnership`) and `Turn::chain_ownership`
  retire; the prompt router and the Wake gate read the one verdict's routing
  rule. Behavior and the `ownership=guard|card-chain` INFO line are unchanged.
- The classification lands whole in this step; only the routing rule is
  consumed here. The remaining rules (admission, reap claim, `covers`, `/stop`
  disposition) land as methods in the follow-up tickets, each consuming the
  same verdict instead of re-deriving it.
- The reap's divergence essay retires when the reap's claim migrates; until
  then it points at this verdict as the intended home.
- GLOSSARY gains **Card Ownership**, disambiguated from ADR-0007's owner (the
  Chat/Topic a Session is mapped to) and from the Backend's Execution.
- The verdict seam gains a module table test: every `CardState` × claim
  combination's classification, the yielded write-readiness, the routing rule
  with its labels, the identities, and the invisible inbound-claim expiry.
  Routing and Wake behavior stays pinned by the existing suites.

Related: #545, #546, #428, #539, #424, ADR-0007, ADR-0043, ADR-0059,
ADR-0060, ADR-0062, ADR-0063, ADR-0066, ADR-0069.
