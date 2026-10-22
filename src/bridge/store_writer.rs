//! The durable writer behind one [`crate::bridge::session::SessionStore`].
//!
//! `sessions.json` is the one sidecar whose loss is not recoverable, so its
//! writer must not log-and-continue — but a synchronous write inside a card
//! callback can outlive Feishu's ack budget (`CODING_STANDARDS.md`: a card
//! callback runs on a deadline). Both facts hold if the store keeps serializing
//! snapshots under its own lock (the order of mutations) and ONE dedicated
//! thread performs the writes in mutation order:
//!
//! - a caller that needs durability before returning asks for a **blocking**
//!   write — the same contract as before this module existed;
//! - a card callback asks for a **deferred** write and bounds its own wait;
//!   the write still lands, in order, after the callback has answered.
//!
//! Handing a snapshot over never blocks, which is what keeps a callback
//! bounded even when the disk stalls: the hand-off is ONE slot holding the
//! newest snapshot (**a later snapshot supersedes an earlier one** — every
//! snapshot is a complete state, so writing the newest is always correct) plus
//! one small receipt per waiting caller. A newer snapshot never waits behind an
//! older one, and the queue's memory cannot grow with a stalled disk.
//!
//! Serializing under the store lock is what makes superseding safe: every
//! snapshot is taken after its mutation, so a later snapshot is a superset of
//! an earlier one and can never resurrect a stale state.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::oneshot;

use crate::bridge::session::PendingEntry;
use crate::config::SessionEntry;

/// The state one write persists, handed over by reference count: taking it is
/// two refcount bumps under the store lock, and the JSON is rendered here, on
/// the writer thread, so no card callback ever pays for it (ADR-0089).
#[derive(Clone)]
pub(crate) struct StoreSnapshot {
    entries: Arc<Vec<SessionEntry>>,
    pending: Arc<Vec<PendingEntry>>,
}

impl StoreSnapshot {
    pub(crate) fn new(entries: Arc<Vec<SessionEntry>>, pending: Arc<Vec<PendingEntry>>) -> Self {
        Self { entries, pending }
    }

    /// Render the file's bytes. A serializer failure is reported like a write
    /// failure — to the caller's receipt.
    pub(crate) fn to_bytes(&self) -> io::Result<Vec<u8>> {
        #[derive(Serialize)]
        struct StoreFile<'a> {
            entries: &'a [SessionEntry],
            pending: &'a [PendingEntry],
        }
        let data = serde_json::to_string_pretty(&StoreFile {
            entries: &self.entries,
            pending: &self.pending,
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(data.into_bytes())
    }
}

/// One store's writer thread. Dropping it lets the thread finish the snapshot
/// in hand and exit.
pub(crate) struct StoreWriter {
    queue: Arc<Queue>,
    /// Test-only gate a stalled write is held at (see [`WritePause`]).
    #[cfg(test)]
    pub(crate) pause: WritePause,
    /// Test-only count of writes that actually reached the disk.
    #[cfg(test)]
    pub(crate) writes: Arc<std::sync::atomic::AtomicUsize>,
}

/// The hand-off between the store and its writer thread.
struct Queue {
    slots: Mutex<Slots>,
    work: Condvar,
}

#[derive(Default)]
struct Slots {
    /// The snapshot to write next — at most one, always the newest.
    latest: Option<StoreSnapshot>,
    /// Callers waiting for the next write to settle (the pre-existing
    /// contract: they wait for the write before returning).
    blocking: Vec<SyncSender<io::Result<()>>>,
    /// Callers that bound their own wait; the write happens either way.
    deferred: Vec<oneshot::Sender<io::Result<()>>>,
    /// The store was dropped: write what is queued, then stop.
    closed: bool,
    /// The thread is gone; further writes settle with an error instead of
    /// waiting for a thread that will never run.
    dead: bool,
}

/// The receipt of a deferred write. Dropping it does not cancel the write; it
/// only means its outcome is no longer reported to that caller.
pub(crate) struct DeferredWrite {
    wait: oneshot::Receiver<io::Result<()>>,
}

impl StoreWriter {
    /// Spawn the writer for `path`. The thread is independent of the async
    /// runtime, so a store built in a plain `#[test]` gets one too.
    pub(crate) fn start(path: PathBuf) -> io::Result<Self> {
        let queue = Arc::new(Queue {
            slots: Mutex::new(Slots::default()),
            work: Condvar::new(),
        });
        #[cfg(test)]
        let pause = WritePause::default();
        #[cfg(test)]
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thread_queue = Arc::clone(&queue);
        #[cfg(test)]
        let thread_pause = pause.clone();
        #[cfg(test)]
        let thread_writes = Arc::clone(&writes);
        std::thread::Builder::new()
            .name("cola-session-store".into())
            .spawn(move || {
                run(
                    path,
                    thread_queue,
                    #[cfg(test)]
                    thread_pause,
                    #[cfg(test)]
                    thread_writes,
                )
            })?;
        Ok(Self {
            queue,
            #[cfg(test)]
            pause,
            #[cfg(test)]
            writes,
        })
    }

    /// Hand a snapshot over and wait for it to land (the synchronous contract).
    pub(crate) fn write_blocking(&self, data: StoreSnapshot) -> io::Result<()> {
        let (done, wait) = mpsc::sync_channel(1);
        self.enqueue(data, Some(done), None);
        wait.recv().map_err(|_| stopped())?
    }

    /// Hand a snapshot over and return its receipt; the write lands after every
    /// earlier mutation, whether or not the caller waits.
    pub(crate) fn write_deferred(&self, data: StoreSnapshot) -> DeferredWrite {
        let (done, wait) = oneshot::channel();
        if !self.enqueue(data, None, Some(done)) {
            return DeferredWrite::settled(Err(stopped()));
        }
        DeferredWrite { wait }
    }

    /// Put `data` in the one pending slot. Never blocks on the writer: a newer
    /// snapshot replaces an older one, and both callers settle on the write
    /// that lands. `false` when the writer thread is gone.
    fn enqueue(
        &self,
        data: StoreSnapshot,
        blocking: Option<SyncSender<io::Result<()>>>,
        deferred: Option<oneshot::Sender<io::Result<()>>>,
    ) -> bool {
        let mut slots = lock(&self.queue);
        if slots.dead {
            return false;
        }
        if let Some(done) = blocking {
            slots.blocking.push(done);
        }
        if let Some(done) = deferred {
            // Drop the receipts of callers that already gave up (their deadline
            // passed and they dropped the receiver): a stalled writer must not
            // accumulate one sender per abandoned click.
            slots.deferred.retain(|waiting| !waiting.is_closed());
            slots.deferred.push(done);
        }
        slots.latest = Some(data);
        drop(slots);
        self.queue.work.notify_one();
        true
    }
}

impl Drop for StoreWriter {
    fn drop(&mut self) {
        lock(&self.queue).closed = true;
        self.queue.work.notify_all();
    }
}

/// The writer loop: wait for a snapshot, write it, settle everyone who was
/// waiting on it.
fn run(
    path: PathBuf,
    queue: Arc<Queue>,
    #[cfg(test)] pause: WritePause,
    #[cfg(test)] writes: Arc<std::sync::atomic::AtomicUsize>,
) {
    // Whatever way this thread leaves — even a panic inside the write — the
    // callers waiting on it must be told, not left waiting forever.
    let _exit = ExitGuard {
        queue: Arc::clone(&queue),
    };
    loop {
        {
            let mut slots = lock(&queue);
            while slots.latest.is_none() && !slots.closed {
                slots = queue.work.wait(slots).unwrap_or_else(|error| error.into_inner());
            }
        }
        // The test gate sits BEFORE the take, so held snapshots accumulate in
        // the slot exactly as they would behind a slow render — a newer one
        // supersedes an older one and only the newest is written.
        #[cfg(test)]
        pause.wait_before_write();
        let (data, waiting) = {
            let mut slots = lock(&queue);
            match slots.latest.take() {
                Some(data) => {
                    // Everyone waiting here settles on THIS write: their
                    // snapshots are at most as new as `data`, and this one
                    // already carries their mutation.
                    let waiting = (
                        std::mem::take(&mut slots.blocking),
                        std::mem::take(&mut slots.deferred),
                    );
                    (data, waiting)
                }
                // Closed with nothing left to write.
                None => return,
            }
        };
        // Render FIRST, then drop the snapshot: while a snapshot is outstanding
        // a mutation has to copy the vectors, so its life is this CPU step —
        // never the (possibly stalled) disk write.
        let bytes = data.to_bytes();
        drop(data);
        let result = match bytes {
            Ok(bytes) => {
                #[cfg(test)]
                writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                crate::bridge::sidecar::write_atomic(&path, &bytes)
            }
            Err(error) => Err(error),
        };
        settle(waiting.0, waiting.1, &result);
    }
}

/// Mark the writer dead and fail anyone still waiting when the thread exits.
struct ExitGuard {
    queue: Arc<Queue>,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        let mut slots = lock(&self.queue);
        slots.dead = true;
        let blocking = std::mem::take(&mut slots.blocking);
        let deferred = std::mem::take(&mut slots.deferred);
        slots.latest = None;
        drop(slots);
        settle(blocking, deferred, &Err(stopped()));
        self.queue.work.notify_all();
    }
}

/// Report one write's outcome to every caller that was waiting on it. Each gets
/// its own error value (`io::Error` is not `Clone`).
fn settle(
    blocking: Vec<SyncSender<io::Result<()>>>,
    deferred: Vec<oneshot::Sender<io::Result<()>>>,
    result: &io::Result<()>,
) {
    for done in blocking {
        if done.send(clone_result(result)).is_err() {
            log_unreported(result);
        }
    }
    for done in deferred {
        if done.send(clone_result(result)).is_err() {
            log_unreported(result);
        }
    }
}

/// A caller that stopped waiting abandons its error with its receipt — but the
/// failure still happened, and the store's loss is not recoverable, so it must
/// not vanish silently (ADR-0089).
fn log_unreported(result: &io::Result<()>) {
    if let Err(error) = result {
        tracing::warn!("session store write failed after its caller stopped waiting: {error}");
    }
}

fn clone_result(result: &io::Result<()>) -> io::Result<()> {
    result
        .as_ref()
        .copied()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
}

fn lock(queue: &Queue) -> MutexGuard<'_, Slots> {
    // A panic elsewhere must not turn a store bug into a hang.
    queue
        .slots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl DeferredWrite {
    /// A receipt for a write that already happened (or was refused) without
    /// reaching the writer thread.
    pub(crate) fn settled(result: io::Result<()>) -> Self {
        let (done, wait) = oneshot::channel();
        let _ = done.send(result);
        Self { wait }
    }

    /// Wait for the write, however long it takes. Test-only: a production
    /// caller always bounds its wait with [`Self::settle_until`], because a
    /// card callback runs on a deadline (CODING_STANDARDS.md).
    #[cfg(test)]
    pub(crate) async fn settle(self) -> io::Result<()> {
        self.wait.await.map_err(|_| stopped())?
    }

    /// Wait until `deadline`. `None` means the caller's bound passed first; the
    /// write continues and is still ordered.
    pub(crate) async fn settle_until(self, deadline: tokio::time::Instant) -> Option<io::Result<()>> {
        match tokio::time::timeout_at(deadline, self.wait).await {
            Ok(Ok(result)) => Some(result),
            Ok(Err(_)) => Some(Err(stopped())),
            Err(_) => None,
        }
    }
}

/// A test-only gate the writer thread stops at before each write, so a test can
/// keep a write in flight and prove that a card callback still answers inside
/// its own bound (and that the write lands once released).
///
/// Per store, never global: tests run in parallel and a global gate would stall
/// every other writer.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct WritePause {
    inner: Arc<(Mutex<bool>, Condvar)>,
}

#[cfg(test)]
impl WritePause {
    /// Hold every write at the gate from now on.
    pub(crate) fn hold(&self) {
        *lock_pause(self) = true;
    }

    /// Let the held write (and later ones) proceed.
    pub(crate) fn release(&self) {
        *lock_pause(self) = false;
        self.inner.1.notify_all();
    }

    fn wait_before_write(&self) {
        let mut held = lock_pause(self);
        while *held {
            held = self.inner.1.wait(held).unwrap_or_else(|error| error.into_inner());
        }
    }
}

#[cfg(test)]
fn lock_pause(pause: &WritePause) -> MutexGuard<'_, bool> {
    pause
        .inner
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "session store writer stopped")
}

/// The error a deferred write reports when there is no writer thread to defer
/// to: the caller's bound is not achievable, so it must say the save failed
/// instead of blocking on the disk.
pub(crate) fn writer_unavailable() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "session store writer unavailable")
}
