# 03 — Feishu WS: pure frame processor + shared parse helpers

**What to build:** Extract per-frame processing — dedupe by event id, the
5-minute age filter, type dispatch, a single typed parse, and the
what-to-ack / what-to-dispatch decision — into a pure function; keep only socket
I/O in the connection handler. Cap or time-expire the dedupe set. Share the
message text/image/mention parse helpers between the live receive path and the
quoted-context fetch (no unified message model — that stays out of scope).

**Blocked by:** None — can start immediately.

**Status:** ready-for-agent

- [ ] Dedupe replay, age-filtering and ack/outcome decisions covered by pure
      unit tests with no socket involved.
- [ ] The dedupe set is bounded (cap or expiry) — no unbounded growth.
- [ ] The live path and the quoted-context fetch call one shared parse helper
      set for text/image/mentions.
- [ ] Full verification loop green.