# 06 - Busy adoption follows the in-flight external turn

Status: ready-for-agent
Type: task
Blocked by: 03, 05

## What to build

The one live case of the Session Snapshot (ADR-0028): when the adopted session
is **busy** — an external (OpenChamber/CLI) turn is mid-flight — the snapshot
follows that turn to completion instead of freezing a "运行中" static line. An
in-flight external run never self-surfaces: the external-message sync only fires
on NEW external user messages (`src/bridge/external.rs:94-180`), so the snapshot
is the only vehicle for its conclusion.

## Scope

- When snapshot gathering (ticket 01) finds server status `busy`, build the
  snapshot as a **host card** for the external-reply renderer rather than a
  static card:
  - epoch = the created time of the newest user message (the one being answered);
    session id = the adopted session; the snapshot message/card is the host
    (`CardSession` in `core.cards`, mirrors `start_reply_render`,
    external.rs:192-280).
  - The external turn's parts stream into the card via the existing
    `render_and_flush` path and finalize Done on completion
    (`external_turn_completed`, external.rs:381) or the hard render timeout.
  - Compose with ticket 05: adopt-time pending blocks stay claimed inline on the
    host card, so an external run blocked on a permission — approved from the
    snapshot — resumes and streams the rest into the SAME card.
  - Existing guards hold unchanged: a cola prompt or a newer external message
    replaces the accumulator and the renderer exits (external.rs:308-340) — no
    double render.
- **Representation choice**: whether the snapshot's tail/header ride as static
  acc text (like the 👤 preview in `start_reply_render`) or the host card is a
  fresh acc whose content grows purely from parts — decide against
  `StreamAccumulator` semantics and keep the header verb + 已接管 visible.
- **Race**: status busy at gather but idle by send time → emit the static
  snapshot and rely on normal channels; never arm a renderer for a turn that
  already finished.

## Acceptance criteria

- [ ] Adopting a busy foreign session shows the running turn's reasoning/text
      streaming into the snapshot and finalizes Done when it completes (or times
      out with content).
- [ ] A blocked external run whose permission is approved from the snapshot
      resumes and completes inside the same card.
- [ ] Adopting an idle session stays a static one-shot snapshot (no renderer
      armed).
- [ ] If the user prompts during the follow, the renderer exits and the user's
      own turn renders normally — no duplicate/overwritten cards.
- [ ] `cargo test --workspace --locked` green, clippy/fmt clean.
