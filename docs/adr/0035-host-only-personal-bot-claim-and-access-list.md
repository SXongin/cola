# Host-only personal bot: a claim bootstrap and a one-entry Access List

A Feishu bot is reachable by every member of its tenant — any of them can DM it, and group messages arrive from anyone who @-mentions it — yet cola treated every sender as the operator: they could `/restart` the bot, root sessions in any directory, list and steal sessions from the whole Shared Store, and answer permission cards that run tools on the host's machine. We make the personal-machine bot private: it must be *claimed* once by the machine's owner, and only that Host may act.

## Context

- The identity is already in every payload: `sender.sender_id.open_id` on message events, `event.operator.open_id` on card-action callbacks. cola persists and enforces neither; the callback's `operator` block is dropped entirely (`extract_card_action_value`, `src/feishu/ws.rs`).
- `open_id` is app-scoped: stable for a user within cola's app, different in other apps; rebuilding the Feishu app changes it. It is the only identity field carried by both messages and card callbacks (card `operator` has `open_id`/`user_id`, no `union_id`).
- cola cannot draw a real safety boundary around another user on a personal machine (ADR-0036). The honest boundary is therefore *whether a Principal may act at all*.

## Decision

1. **Principal**: every inbound message or card action resolves to exactly one Principal — the acting Feishu user, keyed by `open_id` — before anything else happens. Parsing `event.operator.open_id` from card callbacks is a prerequisite.
2. **Claim**: cola starts unclaimed. While unclaimed, every action is refused; the refusal points the viewer at the startup log, where cola prints a one-time claim code (rotated each start, never persisted). The p2p sender of `/claim <code>` becomes the Host; group messages cannot claim. A successful Claim writes the Host's `open_id` into the Access List and survives upgrades.
3. **Access List**: a persisted, machine-local record consulted for every inbound action. In this version it holds exactly one Principal, the Host.
4. **Total enforcement**: non-Host Principals are refused — messages get a short denial; card clicks are rejected on the operator check, not merely hidden (Feishu cannot hide buttons per user).
5. **Universal claim**: fresh and upgraded installations alike start unclaimed. `sessions.json` history is not a trust signal; there is no grandfathering.

## Why

- It closes the exposure found in the grilling: today anyone who can message the bot is the operator.
- It keeps identity first-class (paid for now, not retrofitted) without pretending to sandbox Guests on a personal machine.
- The claim code proves machine access — only someone who can read the start log can claim — with zero manual id copying.

## Alternatives considered

- **Config allowlist + `/whoami`**: works, but makes the Host transcribe an opaque app-scoped id by hand; rejected.
- **First-DM auto-claim**: zero-friction, but first-come — a guest could claim the machine; rejected.
- **Grandfathering existing installs** (keep today's behavior until claimed): leaves the exposure in place indefinitely; the user base is small, so the one-time claim is the better trade; rejected.
- **App-admin detection** (Feishu admin API): an extra permission and an organizational concept for a personal bot; rejected.

## Consequences

- Upgrading installs need one `/claim` (read the code from the log; restart to rotate it).
- Rebuilding the Feishu app invalidates the stored `open_id` → re-claim.
- Guest/sharing/collaboration surfaces do not exist in this version; the Access List shape should stay compatible with the server-mode blueprint (ADR-0036).
