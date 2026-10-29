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
  thread that performs `sidecar::write_atomic`. The thread is independent of the
  async runtime, so a store created in a plain `#[test]` gets one too.
- **The hand-off is one slot, not a queue.** The writer holds the newest
  snapshot plus one small receipt per waiting caller, and handing a snapshot
  over never blocks. A newer snapshot **supersedes** an older one waiting for
  the same thread — correct because every snapshot is a complete state taken
  after its mutation, so the newer one already carries the older one's change —
  and every caller in that batch settles on the write that lands. A stalled
  disk therefore cannot grow the hand-off: memory stays at one snapshot, no
  caller ever waits for queue space (the bounded-channel version this replaced
  blocked inside `SyncSender::send` once 64 snapshots queued, which could
  outlive a click's budget and was found by the review gate), and a receipt
  whose caller gave up is pruned on the next hand-off instead of accumulating.
- **Snapshots are taken under the store lock, as two reference-counted
  handles.** Taking one is O(1), so the callback never renders JSON; the writer
  thread renders and writes. Correctness still rests on the snapshot being a
  complete state taken right after its mutation, and on
  `Arc::make_mut` copy-on-write keeping an outstanding snapshot immutable.
  A mutation that lands while a snapshot is being *rendered* copies the vectors
  once; the writer drops its snapshot as soon as it holds the bytes, so a
  stalled DISK never makes a mutation copy.
- **Two receipts, one writer.** `write_blocking` keeps the pre-existing
  contract — the caller waits for the write and gets its error. `write_deferred`
  returns a `DeferredWrite` whose `settle`/`settle_until` reports the outcome;
  the write happens either way. A deferred caller that stops waiting (a card
  callback past its deadline) forfeits the error, never the write.
- **No writer thread means no deferred save.** If the thread cannot be started,
  the store logs, keeps blocking writes synchronous (durability unchanged), and
  refuses `write_to_disk_deferred` with an error instead of doing the disk write
  inline. Inline would be exactly the unbounded callback work this ADR exists to
  remove, and a silent success would hide that the bound was not kept. The
  mutation stays in memory, and the next blocking write persists it.
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
- **A bounded channel with a blocking send.** Rejected after review: a stalled
  disk fills it, and the send then blocks the caller — inside the card callback,
  and while it holds the store lock — so the very budget this ADR protects is
  missed again. An unbounded channel was rejected too: its queue grows with the
  stall, where one slot + supersession keeps memory flat.
- **Move the write out of the callback entirely (fire-and-forget).** Rejected
  as the default: a failed write would be invisible, and the mapping's
  durability would silently weaken for every caller. Deferral with a receipt is
  the narrow version of the same idea — the caller chooses its bound.

## Consequences

- The store file may trail memory by one write. A crash in that window loses
  the mutation(s) the pending write carried; the alternative was losing the
  user's confirmation, or risking a corrupt file.
- Every store instance spawns one thread (exits when the store drops after its
  last write). Tests therefore create threads too — cheap, and they die with the
  store.
- `write_to_disk` stays the blocking API, so existing call sites and their
  tests are unchanged; only callers that must bound their own wait use
  `write_to_disk_deferred` / `set_pending_deferred`.
- The first consumers are the three pre-existing card callbacks that declare a
  pending — `/dir`'s pick, the browse card's 选择 and `/switch`'s 新建 — through
  `SessionsHandle::declare_pending_until`. Each callback takes ONE deadline
  (`handler::CARD_STORE_BUDGET`, derived from the ack budget) that bounds the
  reads, the mutation, the wait for the file and the card it refreshes — and it
  starts when the click's callback is entered, before the action span's own
  store read, so no pre-dispatch work can buy later steps a fresh budget. The
  browse card's own filesystem work — resolving the target and scanning the
  directory (capped by `BROWSE_SCAN_CAP` entries) — runs on the blocking pool,
  because a synchronous `read_dir`/`canonicalize` cannot be interrupted by the
  deadline that abandons the rest of the refresh. And when a write fails after
  its caller stopped waiting, the error is logged: the receipt is forfeited, the
  failure is not silent. Running out of time while the
  lock is still held declares nothing and refuses the click; running out of time
  while the write is in flight still declares the pending and only forfeits the
  error, because the mutation is already in memory. If applying the mutation
  itself runs past the deadline (its scans are memory-only but proportional to
  the store), it is rolled back and the click refuses — the callback never
  commits a pending whose confirmation is gone.
- A deferred caller that abandons its receipt abandons the error with it. That
  is the price of the bound, and it is the caller's choice, not the store's.

## Amendment (2026-10-30, #910): the model-mirror writes defer too

The three pending-declaration callbacks were not the only card paths writing the
store: a `/model` pick and the provider-switch confirmation also persist the
per-thread model mirror, and they waited on the writer with no bound either.
`SessionStore::set_mirror_live` / `set_mirror_pending` now take the same
optional deadline (`write_to_disk_deferred`'s receipt), so:

- **a card click defers.** The pick handlers (`pick_model_expecting`,
  `pick_pending_pair`, `record_pair_expecting`, and the durable pick's
  forget-on-failure) pass the click's store deadline — the same
  `handler::CARD_STORE_BUDGET` that starts when the callback is entered — and
  settle the receipt inside it.
- **the turn pre-flight blocks.** The AC6 restore and the `/think` settle run
  with no deadline: a prompt can afford the wait, and it must observe the write
  before it submits.
- **the compare-and-set is unchanged.** A refused write (moved, changed,
  creating) returns before any save, exactly as before.
- **a missed settle is a warning, not a claim.** The mutation is in memory, so
  the pick answers normally; the file lands when the writer drains it, and the
  click never says the save was durable. A write that *failed* settles as the
  caller's save failure, as the blocking path did.

The verification: a paused writer leaves the pick and the confirm inside
`CARD_ACK_BUDGET` with the pair visible in memory and the file catching up on
release (`a_stalled_writer_does_not_stall_a_model_pick_callback`,
`a_stalled_writer_does_not_stall_a_provider_switch_confirm`), and a mirror write
without a deadline does not return until the file has it
(`a_mirror_write_without_a_deadline_blocks_until_the_file_has_it`).
