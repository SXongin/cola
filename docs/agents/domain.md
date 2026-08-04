# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root — the project's domain glossary
- **`docs/adr/`** — read ADRs that touch the area you're about to work in
- **`AGENTS.md`** — agent skill configuration (this file is a sibling, not part of the domain docs)

If any of these files don't exist, proceed silently. The `/domain-modeling` skill (reached via `/grill-with-docs` and `/improve-codebase-architecture`) creates them lazily when terms or decisions get resolved.

## File structure

Single-context repo:

```
/
├── AGENTS.md
├── CONTEXT.md
├── docs/
│   ├── adr/
│   │   ├── 0001-rust-bridge-architecture.md
│   │   └── 0002-card-state-machine.md
│   └── agents/
└── src/
```

## Use the glossary's vocabulary

When your output names a domain concept, use the term as defined in `CONTEXT.md`. Don't drift to synonyms the glossary explicitly avoids (e.g. don't say "workspace" when the glossary says "project").

If the concept you need isn't in the glossary yet, that's a signal — either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0002 (inline errors in card state machine) — but worth reopening because…_
