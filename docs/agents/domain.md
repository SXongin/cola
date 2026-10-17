# Domain Docs

How the engineering skills consume this repo's domain documentation when
exploring the codebase. Upstream's `/setup-matt-pocock-skills` writes this file
and the engineering skills read it by this path; the layout it records is the
**single-context** option.

## Read before exploring

- **`GLOSSARY.md`** at the repo root — the project's domain glossary (formerly
  `CONTEXT.md`: upstream renamed the convention in 2026-08, so older ADRs still
  use the old name).
- **`docs/adr/`** — the ADRs touching the area you are about to work in.

Both are created lazily, by `/domain-modeling` (reached through
`/grill-with-docs` and `/improve-codebase-architecture`), when the first term or
decision is resolved. If they don't exist yet, proceed silently.

## Use its vocabulary, and flag conflicts

The rules are `CODING_STANDARDS.md` "Code conventions": name a domain concept
with the term `GLOSSARY.md` defines rather than a synonym it avoids, and surface
a contradiction with an existing ADR instead of silently overriding it. An
unresolved term is a signal either way — you may be inventing language the
project doesn't use, or there is a real gap to note for `/domain-modeling`.
