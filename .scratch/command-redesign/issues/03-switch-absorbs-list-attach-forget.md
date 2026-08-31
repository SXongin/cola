# 03 - `/switch` absorbs `/list` `/attach` `/forget`

Status: resolved
Type: task
Blocked by: none

## What to build

Converge the session-selection cluster onto one command with three text forms
(ADR-0012). `/switch` absorbs `/list`, `/attach`, `/forget`:

- `/switch <keyword>` — matching rules (already implemented, ADR-0008):
  1. current conversation's mapped sessions: unique → switch; many → candidates;
  2. global (children excluded): unique → adopt; many → candidates;
  3. none → hint + card (card comes in issue 04).
- `/switch list [keyword] [--all]` — the old `/list` (global listing with
  server-side cache, ADR-0008).
- `/switch forget` — the old `/forget` (remove the current thread's session
  mapping, server session untouched).
- `/switch <full-id>` — the old `/attach <id>` (adopt by exact id / unique
  prefix / unique title substring). Keep `--force` steal semantics:
  `/switch <id> --force`.

## Scope

- `parse_command` / `handle_switch` (src/bridge/command.rs): extend the arg
  parser; `switch` with a `list`/`forget`/bare-id first token routes to the
  existing `handle_list` / `handle_forget` / adopt logic.
- Remove `/list`, `/attach`, `/forget` as standalone commands (their `Command`
  enum variants either fold into `Switch` subcommands or are deleted).
- Update `/help` text and README Commands list.
- `/switch` with **no arguments** → the card (issue 04); until then, show the
  matching hint text.

## Acceptance criteria

- [ ] `/switch list`, `/switch list foo`, `/switch list --all` behave like the
      old `/list` (including the 30s-cache invalidation on cola-side changes).
- [ ] `/switch forget` unmaps the thread; the server session remains listed.
- [ ] `/switch <id>` adopts; `/switch <id> --force` steals from another thread.
- [ ] `/list`, `/attach`, `/forget` are gone from `/help` and README.
- [ ] `cargo test --workspace --locked` green.