# Switching a question card keeps the draft

## Context

ADR-0093 folds every question but one, so a multi-question ask never pushes its
input off the top of the screen. Folding solved the layout and left an
interaction gap the reporter named (issue #917, part 3): a folded question can be
expanded and answered by its options, but it has **no custom-answer box**, and it
cannot get one — Feishu rejects a `form` nested in a `collapsible_panel`
(ErrCode 200621), and the card's one input belongs to the open question. The only
way to type an answer to question 3 was to answer questions 1 and 2 first.

A client-driven accordion cannot exist either: `collapsible_panel`
expand/collapse happens in the client and reports nothing, and `element_id` lets
the *server* target a component — it never tells the server a fold moved.

Device probes (Android 14 / Feishu 8.4.9, 2026-11-01) established the mechanics a
switch could use:

- A form **submit** delivers every input in the form as `action.form_value` —
  including one nested inside a `collapsible_panel` — and does **not** deliver
  the button's `value`, so routing has to ride the button `name`.
- A non-submit button delivers its `value` and no `form_value`.
- A `form` may contain markdown, buttons, `overflow`, `hr` and a
  `collapsible_panel` (which may in turn contain inputs). What it may not contain
  is another form: the nesting that fails is a **form inside a panel**, so a form
  that wraps the panels is legal — and that is what makes a switch possible.

## Decision

The whole question block is ONE root-level form, and a switch between questions
is a form **submit**:

```
form                                   ← the card's only form, at the body's top level
  markdown   **1. 问题一** + the option bullets
  buttons    options …                  ← non-submit callbacks, `value` arrives
  input      custom_0  (default_value = draft[0])
  button     ✍️ 自定义   (submit, name = "submit|req|ses|0")
  hr
  panel ▸    2. 问题二                  ← folded; its body is the same block minus the box
    markdown  **2. 问题二** + the option bullets
    buttons   options …                 ← answering by option still works
    button    ▸ 作答这道题 (submit, name = "open|req|ses|1")
  button     🚫 无法回答
```

**1. The reader picks the open question.** `QuestionState.open` remembers the
switch target. `question::open_index` resolves it — the chosen one while it is
still unanswered, otherwise the first unanswered one — and is the ONE predicate
both the renderer and the typed-answer freeze read, so the card and the freeze
cannot disagree about which question is open.

**2. A draft rides the switch, keyed by its own name.** The submit carries the
open question's box, so the handler files the text under the input's name
(`custom_<i>`) rather than guessing from state, and the repaint seeds the newly
opened box with `default_value`. Feishu keeps an input's text in the client only —
every repaint rebuilds `body.elements` — so a switch is the one moment it can be
captured. A draft is dropped when its question is finalized, and cleared when a
submit consumes it (a typed custom answer, a multi-select add, a confirmed set).

**3. Option, confirm, skip and reject buttons stay non-submit.** An answer click
never submits a draft, and a switch never answers anything.

**4. A question with no typed control opens without a box** (boolean, external,
custom disabled): the card then holds no input at all, and background repaints
are free.

## Consequences

- Typing into a folded question costs two taps (expand, 作答这道题) and saves
  answering the earlier questions first.
- Every question block is a form now, even one with no input; the card holds
  exactly one, and a `collapsible_panel` never gets one (200621).
- The switch is a user-initiated repaint, and it saves the draft before painting.
  It writes through `refresh_question_block`, never the render pass, so ADR-0093's
  freeze — which stands down for no background paint while a box is open — cannot
  swallow it.
- Typing and then tapping a **multi-select option** still loses the typed text: a
  non-submit click carries no `form_value`, and its repaint rebuilds the box.
  That is unchanged from before this decision; only a **switch** preserves a
  draft.
- The question block's interaction is stateful per request: `QuestionState`
  (handler side) and `PendingQuestion` (render side) both carry `open` and
  `drafts`, and the snapshot state mirrors them — otherwise a repaint would show
  a different question than the click meant to act on.
- A switch is honoured only while it still carries the box of the question that
  is open: two taps racing on the same pre-switch card keep the **first to
  land** and drop the other, so the winner's draft is never overwritten by the
  loser's. With no box to carry (a boolean open question) there is nothing to
  protect, and arrival order decides. A dropped switch does not repaint the card
  — the reader may be typing into the box that is open now, and a rebuild from
  state would seed it from the server's draft, erasing what the client alone
  holds.
- A switch's paint is the callback ack, like every card interaction. Feishu
  discards an ack that misses its budget, and the freeze that protects the open
  box also stops a background repaint from repairing the card, so a dropped ack
  leaves the previous render standing until the next tap.
- None of this is observable in `cargo test` beyond element order and the
  callback payloads the unit tests pin. Whether the draft actually survives on
  the client is a device observation (Android 14 / Feishu 8.4.9), like every
  layout claim in ADR-0093.
