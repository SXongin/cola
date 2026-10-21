# `sessions.json` has one writer thread, and a caller may defer its wait

`SessionStore` persisted synchronously: every mutation serialized the whole
mapping and wrote it before returning. That is the right contract for the one
sidecar whose loss is not recoverable, and the wrong one for a **card
callback**, which runs on Feishu's 2 s ack budget (`CODING_STANDARDS.md`) — a
stalled filesystem could commit the mutation after the ack, so the user never
saw the confirmation of a change that had already taken effect. #449's pick hit
exactly that wall.

## Context

- **The write cannot be cancelled safely.** `set_pending` mutates memory, then
  persists. Cancelling between the two leaves the in-memory store ahead of the
  file; cancelling the write itself can truncate it. So bounding the callback
  by cancelling the write is not an option.
- **Wrapping the statement in a timeout changes nothing.** The write is a
  synchronous `std::fs` call inside the future, so the future cannot be
  interrupted at that point; the timeout fires only after the write returns.
- **Moving the write to `spawn_blocking` reorders it.** Two snapshots taken at
  different times can complete out of order, and the older one then overwrites
  the newer — a lost update, on the one file that must not lose updates.
- **Deferral is only safe if order is fixed by the snapshot.** Serializing
  under the store lock, immediately after each mutation, makes enqueue order
  equal mutation order; any later snapshot is a superset of the earlier one, so
  writing them in that order can never resurrect a stale state.

## Decision

- **One writer thread per store.** `StoreWriter::start(path)` spawns a named
  thread with a bounded channel; it performs `sidecar::write_atomic` in
  enqueue order. The thread is independent of the async runtime, so a store
  created in a plain `#[test]` gets one too. If the thread cannot be started,
  the store logs and keeps writing synchronously rather than losing durability.
- **Snapshots are serialized under the store lock**, right after the mutation.
  This is the ordering rule the rest depends on.
- **Two receipts, one writer.** `write_blocking` keeps the pre-existing
  contract — the caller waits for the write and gets its error. `write_deferred`
  returns a `DeferredWrite` whose `settle`/`settle_until` reports the outcome;
  the write happens either way. A deferred caller that stops waiting (a card
  callback past its deadline) forfeits the error, never the write.
- **The mutation is in memory before any receipt is awaited.** A caller that
  returns early still leaves the new state visible to the next reader — the
  next message routes to the newly declared directory even if the file lags.
- **Failures are always returned, never logged and swallowed.** The blocking
  path surfaces them as before; the deferred path settles them into the receipt
  so a caller that still waits can say what failed.

## Considered options

- **Keep the synchronous write and accept the ack overrun.** Rejected: the
  standard exists because the overrun is user-visible — the change happens and
  the confirmation does not.
- **Cancel the write at the deadline.** Rejected: it can leave memory and file
  disagreeing, or truncate the sidecar.
- **`spawn_blocking` per write without a queue.** Rejected: completion order is
  not commit order, so an older snapshot can overwrite a newer one.
- **Move the write out of the callback entirely (fire-and-forget).** Rejected
  as the default: a failed write would be invisible, and the mapping's
  durability would silently weaken for every caller. Deferral with a receipt is
  the narrow version of the same idea — the caller chooses its bound.

## Consequences

- The store file may trail memory by one write. A crash in that window loses
  the last mutation; the alternative was losing the user's confirmation, or
  risking a corrupt file.
- Every store instance spawns one thread (bounded channel, exits when the store
  drops). Tests therefore create threads too — cheap, and they die with the
  store.
- `write_to_disk` stays the blocking API, so existing call sites and their
  tests are unchanged; only callers that must bound their own wait use
  `write_to_disk_deferred` / `set_pending_deferred`.
- A deferred caller that abandons its receipt abandons the error with it. That
  is the price of the bound, and it is the caller's choice, not the store's.
