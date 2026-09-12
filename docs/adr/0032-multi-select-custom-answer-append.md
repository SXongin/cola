# Multi-select custom answers append verbatim, one per submission

A multi-select **Question** card's custom-answer box is a one-shot compose
box: each submission appends the typed text as exactly one **Custom Answer**
(never split on newlines or punctuation, never trimmed), clears the box, and
renders the entry as a selected button the user can click to remove. The
alternatives — retaining the text as an editable "custom set" and overwriting
it, and splitting one submission into several answers — were rejected because
Feishu refreshes a card only by full replacement, and because an answer may
legitimately contain the characters we would split on.

## Context

- The question card is refreshed by returning a replacement card in the
  callback response — the only mechanism that reliably repaints the body
  (`RequestFlow::handle_action`). Every option click, confirm, and submission
  rebuilds the whole card. Input state lives in the rendered card, so an
  unsubmitted draft cannot survive any interaction.
- An "editor" model — retain the box content (Feishu inputs support
  `default_value`), submit overwrites the custom portion of the selection —
  promises a stateful box the platform cannot keep: the next option click
  silently discards the draft. Retain + append is equally incoherent: a
  repeated submission of unchanged text deduplicates to a silent no-op.
- Splitting one submission into several answers (newlines, 、, commas) looks
  convenient but is lossy: a single answer may legitimately contain commas,
  newlines, or spaces.

## Decision

1. One submission adds exactly one Custom Answer; the text is stored and sent
   to the backend verbatim (no splitting, no trimming; only a completely empty
   string is rejected).
2. The box clears after every submission — a consequence of full-card
   replacement, not an explicit reset — and is a compose box, not an editor.
   This is multi-select only; a single-select custom answer keeps its
   replace-and-finalize semantics.
3. Each Custom Answer renders as a selected "✅" button like a selected option;
   clicking it removes the entry (the existing toggle path). Long or multiline
   text is collapsed and truncated for the button label only; the stored
   answer is untouched.
4. Feedback names the four outcomes: added, removed, already selected, and
   empty input.

## Considered options

- **Editor box with `default_value` + overwrite on submit.** Rejected: full
  card replacement on any other interaction discards unsubmitted drafts; the
  box would advertise editability it cannot deliver.
- **Retain the typed text + append.** Rejected: resubmitting unchanged text is
  a silent dedupe no-op, and editing it makes "append" indistinguishable from
  "compose another".
- **Split on newlines/、/commas into several answers.** Rejected: mangled
  entries (paths, sentences, code) cannot be recovered; one submission = one
  answer keeps the typed text intact.

## Consequences

- Custom Answers are individually removable, closing the previous one-way door
  (`record_append_answer` only ever added).
- Removal is the only way to delete a Custom Answer; there is no edit-in-place
  (the compose box does not reload old entries).
- The custom-answer path and the option-toggle path now share removal
  semantics; a multi-select selection can freely mix options and Custom
  Answers.
