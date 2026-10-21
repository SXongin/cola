//! The durable writer behind one [`crate::bridge::session::SessionStore`].
//!
//! `sessions.json` is the one sidecar whose loss is not recoverable, so its
//! writer must not log-and-continue — but a synchronous write inside a card
//! callback can outlive Feishu's ack budget (`CODING_STANDARDS.md`: a card
//! callback runs on a deadline). Both facts hold if the store keeps serializing
//! snapshots under its own lock (the order of mutations) and ONE dedicated
//! thread performs the writes in enqueue order:
//!
//! - a caller that needs durability before returning asks for a **blocking**
//!   write — the same contract as before this module existed;
//! - a card callback asks for a **deferred** write and bounds its own wait;
//!   the write still lands, in order, after the callback has answered.
//!
//! Serializing under the store lock is what makes deferral safe: every snapshot
//! is taken after its mutation, so a later write can never be overwritten by an
//! earlier one, whichever caller enqueued it.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender};

use tokio::sync::oneshot;

/// One store's writer thread. Dropping it closes the channel and the thread
/// exits after the request in hand.
pub(crate) struct StoreWriter {
    tx: SyncSender<Request>,
}

struct Request {
    data: Vec<u8>,
    receipt: Receipt,
}

// TODO(#449 follow-up): drop the `allow` once the card callbacks use deferral.
#[allow(dead_code)]
enum Receipt {
    /// Wait for the write before returning (the pre-existing contract).
    Blocking(SyncSender<io::Result<()>>),
    /// The caller bounds its own wait; the write happens regardless.
    Deferred(oneshot::Sender<io::Result<()>>),
}

/// The receipt of a deferred write. Dropping it does not cancel the write; it
/// only means its outcome is no longer reported to that caller.
#[allow(dead_code)]
pub(crate) struct DeferredWrite {
    wait: oneshot::Receiver<io::Result<()>>,
}

impl StoreWriter {
    /// Spawn the writer for `path`. The thread is independent of the async
    /// runtime, so a store built in a plain `#[test]` gets one too.
    pub(crate) fn start(path: PathBuf) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Request>(64);
        std::thread::Builder::new()
            .name("cola-session-store".into())
            .spawn(move || {
                while let Ok(request) = rx.recv() {
                    let result = crate::bridge::sidecar::write_atomic(&path, &request.data);
                    match request.receipt {
                        Receipt::Blocking(done) => {
                            let _ = done.send(result);
                        }
                        Receipt::Deferred(done) => {
                            let _ = done.send(result);
                        }
                    }
                }
            })?;
        Ok(Self { tx })
    }

    /// Enqueue a snapshot and wait for it to land (the synchronous contract).
    pub(crate) fn write_blocking(&self, data: Vec<u8>) -> io::Result<()> {
        let (done, wait) = mpsc::sync_channel(1);
        self.tx
            .send(Request {
                data,
                receipt: Receipt::Blocking(done),
            })
            .map_err(|_| stopped())?;
        wait.recv().map_err(|_| stopped())?
    }

    /// Enqueue a snapshot and return its receipt; the write lands in order with
    /// every other write, whether or not the caller waits.
    #[allow(dead_code)]
    pub(crate) fn write_deferred(&self, data: Vec<u8>) -> io::Result<DeferredWrite> {
        let (done, wait) = oneshot::channel();
        self.tx
            .send(Request {
                data,
                receipt: Receipt::Deferred(done),
            })
            .map_err(|_| stopped())?;
        Ok(DeferredWrite { wait })
    }
}

#[allow(dead_code)]
impl DeferredWrite {
    /// A receipt for a write that already happened synchronously (the fallback
    /// when the writer thread could not start).
    pub(crate) fn settled(result: io::Result<()>) -> Self {
        let (done, wait) = oneshot::channel();
        let _ = done.send(result);
        Self { wait }
    }

    /// Wait for the write, however long it takes.
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

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "session store writer stopped")
}
