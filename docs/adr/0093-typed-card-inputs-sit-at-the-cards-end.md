# A typed card input sits at the card's end

> **Retired by ADR-0100** (the Session Snapshot's folded receipt log): the
> snapshot card and its claim/rebuild path are gone, so the receipt-log rule has
> no surface. The layout rule itself lives on in the Session Card's folded tail
> and in the list cards' folded forms.

## Context

Feishu cards are the only place cola collects free text: `input` is the only
free-text component, and it must live inside a `form`. On Android, taking focus
in one makes the client scroll the **message's bottom** to the keyboard's top —
not the focused input into view. Anything more than about half a screen of card
below the input therefore ends up above the top edge: the box is still on
screen, but the caret and the typed text are not, so the Host cannot see what
they type or delete (issue #917, 现象 2).

It is not a repaint race. Static cards sent straight through the Open API and
never patched reproduce it, and the same box keeps failing on every tap while
its neighbours behave — the verdict is a property of the rendered layout.

Measured on Android 14 / Feishu 8.4.9 (2026-11-01) with throwaway cards, tapping
each input from the worst scroll position (the box at the screen's bottom):

| content below the input | result |
| --- | --- |
| 6 card lines | fine |
| 10, 14, 20 or 40 card lines | jumps |
| 40 lines **above** the input | fine |
| 40 lines in a **folded** collapsible panel | fine |
| the same 40 lines in an **expanded** panel | jumps |
| 2 more answer forms (≈2 × 96 px) | fine |
| 4 more answer forms | jumps |
| nothing below it | fine |

Two consequences follow. The threshold is a rendered **height** (about half a
screen), not an element count; and a folded panel contributes only its header
line, which makes folding a real lever rather than a cosmetic one. Folding is
not immunity: enough folded headers would eventually add up.

The affected surfaces were all confirmed on the phone: a question card with
several questions (only the input near the card's bottom survived), a
`/switch`-shaped list card (search box above six rows and a pager), and a
snapshot card whose pending question sat above 25 receipts.

## Decision

Every card that renders a typed input keeps it within a couple of elements of
the card's bottom:

- **Question card** (`question_elements`): one question open at a time. The
  open question renders its controls — including its custom-answer or numeric
  input — and every other unanswered question folds into a collapsible panel
  whose header carries the question text. A single-question ask, the common
  case, is unchanged. Which question is open is the reader's choice, and the
  whole block is one root-level form so a switch can carry the open question's
  typed draft (ADR-0094) — the one form wraps the panels, because Feishu rejects
  a `form` nested **inside** a `collapsible_panel` (ErrCode 200621, "type of
  element is not supported tag: form"). The open question owns the card's only
  input; a folded question keeps its option controls and adds the switch that
  opens it. `PendingQuestion::awaits_typed_answer` reads the same resolved open
  question (`question::open_index`), so the freeze never guards an input the card
  does not render.
- **List cards** (`/switch`, `/dir`, `/sub`, the `/skill` picker): the search
  form renders **after** the rows and the pager. `/switch` keeps only its
  ＋新建会话 footer below.
- **Session Card** (ADR-0100): it carries no typed input, so the rule reaches it
  only through its folded 最近对话 tail; the snapshot's receipt log and its
  rebuild path no longer exist.

## Consequences

- New card surfaces that collect text inherit the rule: an input is the last
  thing on its card, or within a couple of elements of it. A future layout that
  puts a list, a tail or a second block below an input reintroduces #917 on
  Android.
- Multi-question asks change shape: the card shows one open question at a time
  and the rest as folded headers. Answering the open one opens the next, which
  is one tap per question but no longer a card taller than the phone. The reader
  may also pick the open question directly, which is the switch ADR-0094 adds.
- A Session Card's 最近对话 tail folds; there is no receipt log left on the card
  to fold.
- Nothing here is observable in `cargo test` beyond element order. The layout
  assertions pin the structure; whether the keyboard jump is gone is a
  device observation, recorded on the issue.
- The residual: a card that legitimately needs many folded panels can still
  approach the threshold. The old snapshot card's several-pending-block case is
  retired with ADR-0100. The fix is to keep the distance small, not to assume
  any single element is safe.
- A device tap-through of a real four-question card (Android 14 / Feishu 8.4.9,
  2026-11-01, after the fix shipped) showed **no jump in normal use**. One case
  still jumps: the reader deliberately drags the box to the screen's very
  bottom, leaving only a few pixels visible but still tappable — the rule
  protects a box the reader can see and put a caret in, not one they have just
  scrolled half off the screen. The reporter agreed that is out of scope. Do not
  treat it as a regression of this decision.
