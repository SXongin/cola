# Triage Labels

The skills speak in terms of five canonical triage roles, and in this tracker each
role's label is the role name, unchanged:

`needs-triage` · `needs-info` · `ready-for-agent` · `ready-for-human` · `wontfix`

- `needs-triage` — the maintainer evaluates it.
- `needs-info` — waiting on the reporter.
- `ready-for-agent` — fully specified, ready for an AFK agent.
- `ready-for-human` — requires human implementation.
- `wontfix` — will not be actioned.

Repo-local, not a skill role — skills neither apply nor consume it:

- `deferred` — evaluated, intentionally postponed, and may be revisited. Stays
  open, unlike `wontfix`.

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the
label of that name. Archive files under `.scratch/` still carry the equivalent
`Status:` string, but those are inert.
