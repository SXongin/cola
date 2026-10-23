# Durable state is versioned, and a Session record names its client

cola's durable state was written for exactly one agent client and had no format
version. Spec #860's second client turns that from a detail into a data-loss
risk: a Codex process and an OpenCode process would key their records by the
same bare Session id, and the first writer would silently replace the other's
facts. ADR-0090 decision 6 therefore reserves the client identity in the state
**before** the second backend exists. This ADR records the concrete formats,
the key shapes, the migration, and the one operational rule that follows:
renaming a client id is a state migration, not a config edit.

## Context

- **Three facts share a Session id.** The session mapping (`sessions.json`), the
  Chain Record's four session-keyed sections (`records`, `announcements`,
  `endings`, `delivered`), and the interactive surfaces all name a Session by
  the backend's bare id. So do the Terminal Run Records, by way of the Chat/
  Topic that owns the run.
- **One bare id is not unique across clients.** Nothing in the design forbids
  OpenCode and Codex from both creating a `ses_...`-shaped id, and after a
  client switch the same Chat/Topic legitimately has a Session on each. A
  shared file must be able to hold both.
- **The files fail open today.** A missing, unreadable or corrupt sidecar reads
  as the empty record; only the session mapping's write is checked. A migration
  must preserve that, or the bot stops booting on a broken sidecar.
- **A version-less file is the only thing an upgrade can see.** The release
  before this one wrote `sessions.json`, `chain_records.json`,
  `interactive_surfaces.json` and `terminal-runs/*.json` without a version and
  without a client. Those are version 0, and every one of them belonged to the
  implicit `[opencode]` client.
- **ADR-0069's fold is keyed on file presence.** `chain_records.json` is the
  marker that stops the pre-0069 `live_cards.json` / `wake_watermarks.json`
  sidecars from being folded in again. The migration marker must not be
  confused with the format version: a version-0 file is present, and therefore
  still the marker.
- **The Chat/Topic key is not client-scoped.** A Chat/Topic is a Feishu
  conversation; it does not move when the agent client changes. `access.json`
  names the Host and `pinned_chats.json` names Chat/Topics, so neither gains a
  client id — but, like every durable file, both gain a format version.
- **"Every durable file" means the state sidecars, not every file under
  `~/.cola`.** `restart-notify.json` is a one-shot announcement the successor
  consumes and deletes; the lock and pid files are coordination, not state.
  Neither is read as history, so neither carries a version.

## Decision

**1. Every durable state file carries `"version": 1`.** The version is the
file's format version, written by this build, and read before the body is
interpreted. A file with no `version` field is version 0. A file whose version
is **newer** than this build's is refused loudly (see §5), never read as
version 0: rewriting an unknown shape as our own would destroy a newer
process's state.

**2. A Session-keyed section is keyed by client first, then Session.** The
on-disk shape is a nested map, `{ "<client id>": { "<session id>": value } }`,
so all of a client's records stay readable and the other client's are preserved
verbatim on every rewrite. The concrete files:

`sessions.json` (version 0 was `{entries, pending}` or, older still, a bare
array):

```json
{
  "version": 1,
  "entries": [
    { "client_id": "default", "thread_key": {"chat_id": "…", "thread_id": "…"},
      "session_id": "ses_…", "directory": "…" }
  ],
  "pending": [ { "thread_key": { … }, "directory": "…" } ]
}
```

`SessionEntry` gains `client_id` (default `default`). `PendingEntry` does not:
it is a declared intent to create a Session, not a Session yet, so there is no
backend id for a client to own; the store stamps the active client on the entry
when the pending materialises.

`chain_records.json` (version 0 had flat maps and the migration-only `rendered`
section):

```json
{
  "version": 1,
  "records":       { "<client id>": { "<session id>": ChainRecord } },
  "announcements": { "<client id>": { "<session id>": WakeMark } },
  "endings":       { "<client id>": { "<session id>": [TaskRunRecord] } },
  "delivered":     { "<client id>": { "<session id>": "<message id>" } }
}
```

The `renamed` forwarding map is in-memory only, but it is client-scoped the same
way: a rename under one client must not redirect another client's writes.

`interactive_surfaces.json` (version 0 had flat maps):

```json
{
  "version": 1,
  "inline":     { "<client id>": { "<request id>": InlineSurface } },
  "cards":      { "<client id>": { "<message id>": CachedSurface } },
  "standalone": { "<client id>": { "<request id>": StandaloneSurface } }
}
```

Request and message ids are minted per client, so the maps are scoped too — two
clients could otherwise mint the same `per_…`/`om_…` key. `InlineSurface` and
`StandaloneSurface` additionally carry `client_id` beside their existing
`session_id`, so a record read out of its map still says whose it is.

`terminal-runs/<chat-or-topic>.json` is one record per file and therefore one
version per file; the version and the client are siblings of the record's own
fields:

```json
{ "version": 1, "client_id": "default", "run_id": "…", "chat_id": "…", … }
```

The owed-settlement journal beside it (`terminal-runs-owed/`) uses the same
envelope, so an obligation written before this ADR reads as version 0 too.

`access.json` and `pinned_chats.json` carry `"version": 1` and no client id:
the Host is not a Session, and a pin is a Chat/Topic fact. The Chat/Topic key
itself is unchanged everywhere.

**3. A version-0 file reads as the default client's.** The release before this
one had one client, the implicit `[opencode]` client, whose durable id is
`default` (ADR-0090 decision 5). Every legacy key and every legacy `SessionEntry`
is therefore read under `default`, even when the process that upgrades runs a
different client; the file's facts are the default client's, and inventing an
active-client owner for them would be wrong.

**4. A legacy file is rewritten once, at load.** For the stores the daemon
loads at startup, the version-0 file is rewritten in the version-1 form during
the load that read it — the same moment ADR-0069's fold materialises its marker,
and for the same reason: the migration is settled at the first load, not left
for a mutation that may never come. A second process reads `"version": 1` and
writes nothing. The stale legacy sections (`rendered`, the pre-0041 bare array)
are dropped by that rewrite. `TerminalRunRecords::open` performs the same
one-time rewrite for old run files; `access.json` is rewritten by the daemon's
startup path (`Access::new`, never by a read-only `cola status`);
`pinned_chats.json` is rewritten by its next write (the startup orphan sweep).

**5. The failure modes are split deliberately.**

- A **missing**, **unreadable** or **corrupt** file still fails open exactly as
  today: an empty record and a warning. The bytes are left untouched, so a human
  can inspect them.
- An **unknown future version** is refused: the startup stores return an error
  naming the path, the found version and the version this build writes; the
  bot does not start rather than rewrite state it cannot understand. A store
  loaded outside startup (the Access List) logs the refusal and behaves
  unclaimed/fails closed.
- A version-0 file whose body still will not parse fails open and is **not**
  rewritten; only a body that parsed as version 0 is migrated.

**6. The presence marker stays presence, not version.** `ChainRecords::load`
folds `live_cards.json` / `wake_watermarks.json` only when
`chain_records.json` is **absent**. A version-0 file is present, so it is the
marker and the legacy files are not read; the migration then rewrites that same
file as version 1, which remains the marker. There is no window in which the
fold can fire twice.

**7. Renaming an `[clients.<id>]` id is a state migration.** The config edit
alone is not enough: every Session-keyed record under the old id becomes
invisible to the renamed client, and the two ids now coexist as separate
records. A rename must re-key the old client's sections — `sessions.json`'s
`client_id` values, the two map levels of `chain_records.json` and
`interactive_surfaces.json`, and the `client_id` of every Terminal Run Record —
in the same migration that changes the config. cola does not perform this
automatically: the id is durable state, and a silent rewrite of the user's
history is worse than a documented rule. The user guide says so beside the
state-file table.

## Considered options

- **A flat composite key (`"<client>/<session>"`) in the existing maps.** It
  reads well until a client id or Session id contains the separator, and then
  the key cannot be parsed back. Nested maps are unambiguous and keep each
  client's section contiguous; the code pays one map level.
- **One state directory per client.** It would avoid key changes, but a
  Chat/Topic switches clients and must keep its chain, watermark and endings
  (spec #860, "out of scope"), so the records must coexist in one place.
- **No eager rewrite; upgrade on the next mutation.** Rejected: a user who only
  reads state would keep a version-0 file indefinitely, and the acceptance
  contract is "loaded once, rewritten once".
- **Read a future version as version 0 and fail open.** Rejected: it is the only
  failure mode that can silently destroy newer state, and a write after the
  read would do exactly that.
- **A separate version file beside each state file.** Rejected: two writes, two
  failure modes, and a torn pair is worse than a header in the same object.

## Consequences

- One bare Session id can now hold two clients' facts, and switching clients no
  longer makes the other side's records indistinguishable.
- A non-default client that upgrades an existing install sees the legacy
  records under `default`, not under itself. That is the truthful reading of the
  file; the records are not lost, and switching back or migrating the id
  reaches them.
- Every state file is one object larger (`version`), and the nested sections are
  one map level deeper. The load writes once per file on the first start after
  the upgrade; a second start writes nothing.
- A binary that predates this ADR cannot read the new files. That is the point
  of a version, and the reason an unknown future version is refused rather than
  guessed.
- The rule that a client-id rename re-keys state is now written down in the ADR
  and the user guide; it is a manual migration until a command earns its place.
