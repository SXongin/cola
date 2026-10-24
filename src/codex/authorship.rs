//! The record of turns **cola started**, on a client that assigns its own ids
//! (ADR-0090's authorship consequence, ADR-0092).
//!
//! ADR-0026 tells cola's own messages apart by the `msg_cola_` id cola chose at
//! send time. Codex assigns every turn and item id itself, so that signal does
//! not exist there: the adapter instead records the turns it started and
//! answers the External Message question from that record
//! ([`crate::backend::Backend::is_cola_authored`]).
//!
//! The record has to survive a restart — otherwise the process that comes back
//! would read its own earlier turn as somebody else's and notify the user about
//! their own message. It is therefore a durable sidecar beside `sessions.json`,
//! client-scoped and versioned exactly like every other state file (ADR-0091):
//!
//! ```json
//! {
//!   "version": 1,
//!   "turns":   { "<client id>": { "<thread id>": ["turn-1"] } },
//!   "pending": { "<client id>": { "<thread id>": [
//!       { "id": "msg_cola_…", "at_ms": 1700000000000 }
//!   ] } }
//! }
//! ```
//!
//! **Provenance ordering.** A turn id only arrives in the `turn/start` response,
//! but the server can emit the turn's user item to the subscription before that
//! response is processed, and the response itself can be lost (the connection
//! closes after the server accepted the request). A prompt therefore records
//! its **chosen submission id** (`clientUserMessageId`, a `msg_cola_…` value)
//! durably *before* the request goes out. Until the turn is bound, the answer
//! for that submission's user item is "cola's":
//!
//! - the item the server persisted carries the chosen id as `userMessage.clientId`,
//!   so an item whose client id is a pending submission is cola's; and
//! - an item with no client id while a submission is in flight is cola's when
//!   it belongs to the thread's live turn.
//!
//! The next history read binds the pending id to the turn it landed in
//! ([`Self::bind_submission`]), which is what makes an accepted-but-unanswered
//! prompt survive a restart. A **definitely refused** request (the server
//! answered with a JSON-RPC error) clears its marker; an ambiguous one (a
//! closed connection, a lost response) keeps it, because the turn may exist.
//!
//! **The record is a precondition, not a best effort.** A prompt whose chosen
//! id cannot be made durable is **refused** ([`Self::begin`] returns an error
//! and the adapter sends nothing): a send the record did not learn about is
//! exactly the turn a restart would report as external. An unresolved
//! submission is never silently evicted to make room: at the per-thread cap the
//! next prompt is refused instead, because dropping an older unresolved id is
//! the same loss. Only a write that already succeeded is ever discarded, by the
//! tail caps below and by an explicit expiry on the thread-wide fallback: a
//! submission whose response never came and whose item never appeared stops
//! vouching for a later external turn after [`PENDING_TTL_MS`].
//!
//! **Degrading safely.** This file is the only evidence of who wrote a turn, so
//! it fails **closed**: a corrupt, unreadable or unwritable record marks the
//! whole record untrusted, and every message is then answered as cola-authored
//! (with an error logged) until a human inspects the file. A prompt on an
//! untrusted record is refused rather than sent unrecorded. Silently answering
//! "external" would notify the user about cola's own turns.
//!
//! What is *not* this file's problem: a message from another client that was
//! folded into a turn cola started belongs to that turn, and is therefore
//! reported as cola-authored too. Codex's `turn/steer` and the fold of a
//! second `turn/start` both put a user message inside the active turn, and the
//! protocol carries no per-message author; the turn is the finest granularity
//! either id space offers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::bridge::sidecar::{self, VersionedRead};
use crate::state::ClientScoped;

/// How many cola-started turns are remembered per thread. The External Message
/// question is asked about the **newest** user message, so a bounded tail is
/// plenty; the cap keeps a long-lived thread's file from growing forever.
const MAX_TURNS_PER_THREAD: usize = 128;

/// How many unresolved submissions one thread may carry. Reaching it **refuses**
/// the next prompt instead of evicting the oldest: an unresolved id is a
/// provenance record that may still be owed, and dropping it would make the
/// turn it names look external after a restart.
const MAX_PENDING_PER_THREAD: usize = 32;

/// How long an unresolved submission vouches for the thread-wide (id-less)
/// fallback. A lost response reconciles as soon as its item appears, normally
/// well inside this window; a request that never reached the server must stop
/// making a later external turn look cola's. The precise id check is not
/// bounded by this: no other client can mint cola's chosen id.
#[cfg(not(test))]
pub(crate) const PENDING_TTL_MS: i64 = 5 * 60 * 1000;
#[cfg(test)]
pub(crate) const PENDING_TTL_MS: i64 = 50;

/// The version-1 (ADR-0091) file.
#[derive(Debug, Serialize, Deserialize)]
struct AuthorshipFile {
    /// A version-0 body is the shape this file had before it was ever written
    /// by a release (there is none) — it still parses, and ADR-0091 §4's
    /// migrate-on-load rule rewrites it once.
    #[serde(default)]
    version: u32,
    #[serde(default)]
    turns: ClientScoped<Vec<String>>,
    #[serde(default)]
    pending: ClientScoped<Vec<PendingSubmission>>,
}

/// One unresolved submission: the id cola chose for a prompt, and when it was
/// recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingSubmission {
    id: String,
    /// Epoch milliseconds, cola's own clock — the server never sees this
    /// record. It bounds the thread-wide fallback ([`PENDING_TTL_MS`]).
    at_ms: i64,
}

#[derive(Default)]
struct State {
    turns: ClientScoped<Vec<String>>,
    /// Durable, unresolved submissions per client and thread: the id cola chose
    /// for a prompt whose `turn/start` may have been accepted without a usable
    /// answer. A chosen id (not a count): the user item the server persisted
    /// carries exactly this id, so a later read can bind the turn precisely.
    pending: ClientScoped<Vec<PendingSubmission>>,
    /// In-memory markers for a prompt with no chosen id (the bridge always
    /// passes one, so this is a defensive path): a count per thread, since an
    /// anonymous submission has nothing to match a later item against anyway.
    anonymous: HashMap<String, usize>,
    /// The record could not be read (or a write failed): the adapter can no
    /// longer tell who wrote a turn, so it answers every message as cola's.
    untrusted: bool,
}

/// One process's record of the turns it started, with the durable file it
/// mirrors when it has one. Cloned through an `Arc`.
pub(crate) struct Authorship {
    /// `None` for an in-memory record (a test client with no state directory).
    path: Option<PathBuf>,
    /// The Agent Client this process is attached to (ADR-0091).
    client_id: String,
    state: Mutex<State>,
}

impl std::fmt::Debug for Authorship {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock().expect("authorship record");
        formatter
            .debug_struct("Authorship")
            .field("path", &self.path)
            .field("client_id", &self.client_id)
            .field("pending", &state.pending)
            .field("anonymous", &state.anonymous)
            .field("untrusted", &state.untrusted)
            .finish_non_exhaustive()
    }
}

impl Authorship {
    /// A process-lifetime record with no file: a client constructed without a
    /// state layout still answers authorship correctly for the turns it starts
    /// itself. A restart loses it, which is exactly why the daemon wires the
    /// durable [`Self::open`] form.
    pub(crate) fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            path: None,
            client_id: crate::config::DEFAULT_CLIENT_ID.to_string(),
            state: Mutex::new(State::default()),
        })
    }

    /// Load the durable record at `path` for `client_id`.
    ///
    /// A missing file is a fresh install: an empty, trusted record. A file that
    /// cannot be read or parsed marks the record **untrusted** (every message
    /// answers as cola's) and leaves its bytes untouched for a human. A
    /// version-0 body parses and is rewritten once in the current form
    /// (ADR-0091 §4); a declared version this build does not write is refused
    /// (ADR-0091 §5), the same rule every startup store follows.
    pub(crate) fn open(path: PathBuf, client_id: &str) -> anyhow::Result<Arc<Self>> {
        let mut state = State::default();
        let mut migrate = false;
        match sidecar::read_versioned(&path, "authored-turn record") {
            VersionedRead::Missing => {}
            VersionedRead::Unsupported(version) => {
                anyhow::bail!(
                    "{} declares unsupported format version {version} (this build writes {}); \
                     refusing to read it as version 0",
                    path.display(),
                    crate::state::STATE_FORMAT_VERSION
                );
            }
            VersionedRead::Unreadable | VersionedRead::Corrupt => {
                untrusted(&path, "the file could not be read as JSON");
                state.untrusted = true;
            }
            VersionedRead::Legacy(raw) => {
                match serde_json::from_str::<AuthorshipFile>(&raw) {
                    Ok(file) => {
                        state.turns = file.turns;
                        state.pending = file.pending;
                        // ADR-0091 §4: a parsed version-0 body is rewritten
                        // once, at load.
                        migrate = true;
                    }
                    Err(error) => {
                        untrusted(&path, &format!("the record did not parse: {error}"));
                        state.untrusted = true;
                    }
                }
            }
            VersionedRead::Current(raw) => match serde_json::from_str::<AuthorshipFile>(&raw) {
                Ok(file) => {
                    state.turns = file.turns;
                    state.pending = file.pending;
                }
                Err(error) => {
                    untrusted(&path, &format!("the record did not parse: {error}"));
                    state.untrusted = true;
                }
            },
        }
        let record = Arc::new(Self {
            path: Some(path),
            client_id: client_id.to_string(),
            state: Mutex::new(state),
        });
        if migrate {
            let mut state = record.state.lock().expect("authorship record");
            record.persist_or_untrust(&mut state);
        }
        Ok(record)
    }

    /// Mark a cola prompt as in flight for `thread` **before** it is sent, and
    /// fail when that marker cannot be made durable. The adapter must not send
    /// a prompt the record did not learn about: a restart would then report the
    /// accepted turn as external.
    ///
    /// `submission` is the id cola chose (`clientUserMessageId`); `None` is the
    /// defensive anonymous path, which has nothing a later read could match and
    /// so lives only in memory.
    pub(crate) fn begin(&self, thread: &str, submission: Option<&str>) -> Result<(), String> {
        let mut state = self.state.lock().expect("authorship record");
        let Some(submission) = submission else {
            *state.anonymous.entry(thread.to_string()).or_default() += 1;
            return Ok(());
        };
        let now = now_ms();
        drop_expired(&mut state, &self.client_id, thread, now);
        if self.path.is_some() && state.untrusted {
            return Err(format!(
                "the authored-turn record is unusable; refusing to submit `{submission}` \
                 without a durable provenance record"
            ));
        }
        let at_capacity = {
            let pending = state.pending.entry_mut(&self.client_id, thread);
            if let Some(index) = pending.iter().position(|known| known.id == submission) {
                // A retry re-sends the same chosen id: refresh its window rather
                // than adding a second marker.
                pending[index].at_ms = now;
                false
            } else if pending.len() >= MAX_PENDING_PER_THREAD {
                true
            } else {
                pending.push(PendingSubmission {
                    id: submission.to_string(),
                    at_ms: now,
                });
                false
            }
        };
        if at_capacity {
            return Err(format!(
                "the authored-turn record already holds {MAX_PENDING_PER_THREAD} unresolved \
                 submissions for this thread; inspect `authored_turns.json` before submitting again"
            ));
        }
        if self.path.is_none() {
            return Ok(());
        }
        if let Err(error) = self.persist(&state) {
            // Roll the marker back: the prompt is refused, and a later read
            // must not treat it as in flight.
            if let Some(pending) = state.pending.get_mut(&self.client_id, thread) {
                pending.retain(|known| known.id != submission);
            }
            prune_pending(&mut state.pending, &self.client_id, thread);
            return Err(format!(
                "the authored-turn record could not be persisted: {error}"
            ));
        }
        Ok(())
    }

    /// A prompt the server **definitely refused** (it answered an error): drop
    /// that submission's marker. Another prompt's marker on the same thread is
    /// left alone — a retry racing its predecessor must not clear it.
    pub(crate) fn abandon(&self, thread: &str, submission: Option<&str>) {
        let mut state = self.state.lock().expect("authorship record");
        let Some(submission) = submission else {
            if let Some(count) = state.anonymous.get_mut(thread) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.anonymous.remove(thread);
                }
            }
            return;
        };
        let removed = state
            .pending
            .get_mut(&self.client_id, thread)
            .is_some_and(|pending| {
                let before = pending.len();
                pending.retain(|known| known.id != submission);
                pending.len() != before
            });
        prune_pending(&mut state.pending, &self.client_id, thread);
        if removed {
            self.persist_or_untrust(&mut state);
        }
    }

    /// Bind the turn the server returned for `thread`'s pending prompt and
    /// persist it **before the caller sees the prompt succeed**, so no answer
    /// can be classified before the record exists.
    pub(crate) fn bind(&self, thread: &str, turn: &str, submission: Option<&str>) {
        let mut state = self.state.lock().expect("authorship record");
        match submission {
            Some(submission) => {
                if let Some(pending) = state.pending.get_mut(&self.client_id, thread) {
                    pending.retain(|known| known.id != submission);
                }
                prune_pending(&mut state.pending, &self.client_id, thread);
            }
            None => {
                if let Some(count) = state.anonymous.get_mut(thread) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        state.anonymous.remove(thread);
                    }
                }
            }
        }
        record_turn(&mut state, &self.client_id, thread, turn);
        self.persist_or_untrust(&mut state);
    }

    /// Bind a submission to the turn its user item landed in. The history read
    /// that sees `userMessage.clientId == submission` is what resolves a lost
    /// `turn/start` response, across a restart included. The caller decides
    /// which submissions are eligible (a pending marker, or one carrying
    /// cola's own id convention); this records the turn and clears any marker.
    pub(crate) fn bind_submission(&self, thread: &str, submission: &str, turn: &str) {
        let mut state = self.state.lock().expect("authorship record");
        let mut changed = state
            .pending
            .get_mut(&self.client_id, thread)
            .is_some_and(|pending| {
                let before = pending.len();
                pending.retain(|known| known.id != submission);
                pending.len() != before
            });
        prune_pending(&mut state.pending, &self.client_id, thread);
        changed |= record_turn(&mut state, &self.client_id, thread, turn);
        if changed {
            self.persist_or_untrust(&mut state);
        }
    }

    /// Drop an expired submission from this thread's record, persisting the
    /// removal when one was due. Called by the history read, so a marker whose
    /// request never reached the server does not stay in the file forever.
    pub(crate) fn expire(&self, thread: &str) {
        let mut state = self.state.lock().expect("authorship record");
        if drop_expired(&mut state, &self.client_id, thread, now_ms()) {
            self.persist_or_untrust(&mut state);
        }
    }

    /// Forget a thread's record (the thread was deleted): nothing can ask about
    /// it again, and a recreated id must not inherit an old turn.
    pub(crate) fn forget(&self, thread: &str) {
        let mut state = self.state.lock().expect("authorship record");
        state.anonymous.remove(thread);
        state.pending.remove(&self.client_id, thread);
        state.turns.remove(&self.client_id, thread);
        self.persist_or_untrust(&mut state);
    }

    /// Whether the record is trusted. When it is not, [`Self::is_authored`]
    /// answers `true` for everything (fail closed).
    pub(crate) fn is_trusted(&self) -> bool {
        !self.state.lock().expect("authorship record").untrusted
    }

    /// Whether a **live** prompt is in flight for `thread`. The turn id is not
    /// known yet, so an item with no client id in the thread's live turn is the
    /// record's stand-in until the response (or a history read) binds it. The
    /// window is bounded by [`PENDING_TTL_MS`]: a submission whose request
    /// never reached the server must stop vouching for later turns.
    pub(crate) fn is_pending(&self, thread: &str) -> bool {
        let state = self.state.lock().expect("authorship record");
        let now = now_ms();
        state
            .pending
            .get(&self.client_id, thread)
            .is_some_and(|pending| pending.iter().any(|known| is_live(known, now)))
            || state.anonymous.get(thread).copied().unwrap_or(0) > 0
    }

    /// Whether `submission` is a **live** unresolved chosen id. This is the
    /// precise provenance answer: the item carries exactly the id cola sent, so
    /// no other client's message can match it.
    pub(crate) fn is_pending_submission(&self, thread: &str, submission: &str) -> bool {
        let now = now_ms();
        self.state
            .lock()
            .expect("authorship record")
            .pending
            .get(&self.client_id, thread)
            .is_some_and(|pending| {
                pending
                    .iter()
                    .any(|known| known.id == submission && is_live(known, now))
            })
    }

    /// Whether a message in `thread`'s turn `turn` is cola's own.
    ///
    /// `None` for `turn` asks the question before the turn id is known (the
    /// user item arrived before the `turn/start` response was processed): the
    /// pending marker answers it, so the provenance window is closed.
    pub(crate) fn is_authored(&self, thread: &str, turn: Option<&str>) -> bool {
        let state = self.state.lock().expect("authorship record");
        if state.untrusted {
            return true;
        }
        match turn {
            Some(turn) => state
                .turns
                .get(&self.client_id, thread)
                .is_some_and(|turns| turns.iter().any(|recorded| recorded == turn)),
            None => {
                let now = now_ms();
                state
                    .pending
                    .get(&self.client_id, thread)
                    .is_some_and(|pending| pending.iter().any(|known| is_live(known, now)))
                    || state.anonymous.get(thread).copied().unwrap_or(0) > 0
            }
        }
    }

    /// Write the record through the shared atomic writer, reporting a failure
    /// to the caller, which makes the record untrusted for this process: a
    /// restart would lose a turn, and answering "external" for it is the one
    /// outcome the record exists to prevent.
    fn persist(&self, state: &State) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let file = AuthorshipFile {
            version: crate::state::STATE_FORMAT_VERSION,
            turns: state.turns.clone(),
            pending: state.pending.clone(),
        };
        let data = serde_json::to_vec(&file).map_err(|error| error.to_string())?;
        sidecar::write_atomic(path, &data).map_err(|error| error.to_string())
    }

    /// Persist, or mark the record untrusted (fail closed) when the write
    /// cannot land.
    fn persist_or_untrust(&self, state: &mut State) {
        if state.untrusted {
            return;
        }
        if let Err(error) = self.persist(state) {
            state.untrusted = true;
            if let Some(path) = &self.path {
                untrusted(path, &error);
            }
        }
    }
}

/// Cola's clock in epoch milliseconds (this record is never ordered against a
/// server time).
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Whether a submission is still inside the thread-wide fallback's window.
fn is_live(submission: &PendingSubmission, now: i64) -> bool {
    now.saturating_sub(submission.at_ms) <= PENDING_TTL_MS
}

/// Drop expired submissions for one thread, reporting whether anything was
/// removed (the caller persists once).
fn drop_expired(state: &mut State, client: &str, thread: &str, now: i64) -> bool {
    let changed = state.pending.get_mut(client, thread).is_some_and(|pending| {
        let before = pending.len();
        pending.retain(|submission| is_live(submission, now));
        pending.len() != before
    });
    prune_pending(&mut state.pending, client, thread);
    changed
}

/// Record a bound turn, keeping the per-thread tail bounded. Returns whether
/// the record changed.
fn record_turn(state: &mut State, client: &str, thread: &str, turn: &str) -> bool {
    let turns = state.turns.entry_mut(client, thread);
    if turns.iter().any(|recorded| recorded == turn) {
        return false;
    }
    turns.push(turn.to_string());
    if turns.len() > MAX_TURNS_PER_THREAD {
        let excess = turns.len() - MAX_TURNS_PER_THREAD;
        turns.drain(..excess);
    }
    true
}

/// Drop a thread's pending section once its last submission resolved, so a
/// written file never carries an empty list.
fn prune_pending(pending: &mut ClientScoped<Vec<PendingSubmission>>, client: &str, thread: &str) {
    if pending
        .get(client, thread)
        .is_some_and(|submissions| submissions.is_empty())
    {
        pending.remove(client, thread);
    }
}

/// One loud line for the one failure that must never be silent.
fn untrusted(path: &std::path::Path, reason: &str) {
    tracing::error!(
        "the authored-turn record {} is unusable ({reason}); every message is treated as \
         cola-authored until the file is inspected — External Message detection is disabled, \
         not pointed at cola's own turns",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(path: &std::path::Path, client: &str, thread: &str, turns: &[&str]) -> Arc<Authorship> {
        let record = Authorship::open(path.to_path_buf(), client).expect("the record loads");
        for turn in turns {
            record
                .begin(thread, Some(&format!("msg_cola_{turn}")))
                .expect("the marker persists");
            record.bind(thread, turn, Some(&format!("msg_cola_{turn}")));
        }
        record
    }

    #[test]
    fn a_bound_turn_is_durable_and_versioned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        let record = written(&path, "codex", "thread-1", &["turn-1", "turn-2"]);

        assert!(record.is_authored("thread-1", Some("turn-1")));
        assert!(record.is_authored("thread-1", Some("turn-2")));
        assert!(!record.is_authored("thread-1", Some("turn-9")));
        assert!(!record.is_authored("thread-2", Some("turn-1")));

        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
        assert_eq!(file["version"], crate::state::STATE_FORMAT_VERSION);
        assert_eq!(
            file["turns"]["codex"]["thread-1"],
            serde_json::json!(["turn-1", "turn-2"])
        );
        assert!(
            file["pending"].get("codex").is_none(),
            "a bound submission leaves no pending marker: {file}"
        );

        // A restarted process reads the same record: the turn is still cola's.
        let restarted = Authorship::open(path.clone(), "codex").expect("reload");
        assert!(restarted.is_authored("thread-1", Some("turn-2")));
        assert!(restarted.is_trusted());
    }

    #[test]
    fn a_record_is_client_scoped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        written(&path, "codex", "thread-1", &["turn-1"]);

        let opencode = Authorship::open(path.clone(), "default").expect("reload");
        assert!(
            !opencode.is_authored("thread-1", Some("turn-1")),
            "another client's turns are not this client's"
        );
        // Its own write preserves the other client's section verbatim.
        opencode
            .begin("thread-1", Some("msg_cola_own"))
            .expect("the marker persists");
        opencode.bind("thread-1", "turn-2", Some("msg_cola_own"));
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
        assert_eq!(file["turns"]["codex"]["thread-1"], serde_json::json!(["turn-1"]));
        assert_eq!(
            file["turns"]["default"]["thread-1"],
            serde_json::json!(["turn-2"])
        );
    }

    /// A pending submission is durable **before** its send: a process that
    /// dies before the response still knows the chosen id, and a later read can
    /// bind it to the turn the server persisted.
    #[test]
    fn a_pending_submission_survives_a_restart_and_binds_by_its_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        let first = Authorship::open(path.clone(), "codex").expect("the record opens");
        first
            .begin("thread-1", Some("msg_cola_lost"))
            .expect("the marker persists");
        assert!(first.is_pending("thread-1"));
        assert!(first.is_pending_submission("thread-1", "msg_cola_lost"));
        drop(first);

        // The restarted process still holds the submission, and the history
        // read that carries its user item binds the turn.
        let second = Authorship::open(path.clone(), "codex").expect("the record reloads");
        assert!(second.is_pending_submission("thread-1", "msg_cola_lost"));
        assert!(!second.is_authored("thread-1", Some("turn-1")));
        second.bind_submission("thread-1", "msg_cola_lost", "turn-1");
        assert!(second.is_authored("thread-1", Some("turn-1")));
        assert!(!second.is_pending("thread-1"));

        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
        assert_eq!(file["turns"]["codex"]["thread-1"], serde_json::json!(["turn-1"]));
        assert!(
            file["pending"].get("codex").is_none(),
            "the resolved submission leaves no section: {file}"
        );
    }

    /// Finding 1: a marker that cannot be made durable refuses the prompt —
    /// nothing may be sent the record did not learn about — and a failed write
    /// does not distrust the record's existing, valid contents.
    #[test]
    fn an_unwritable_marker_is_refused_and_costs_no_trust() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        let record = written(&path, "codex", "thread-1", &["turn-1"]);
        // A directory at the writer's temp path makes the atomic replace fail
        // (deterministically, as root too).
        std::fs::create_dir(path.with_extension("tmp")).expect("block the temp path");

        let error = record
            .begin("thread-1", Some("msg_cola_new"))
            .expect_err("the marker cannot be persisted");
        assert!(
            error.contains("could not be persisted"),
            "the refusal names the record: {error}"
        );
        assert!(
            !record.is_pending_submission("thread-1", "msg_cola_new"),
            "a refused marker is rolled back"
        );
        assert!(
            record.is_trusted(),
            "the valid record is still trustworthy for later reads"
        );
        assert!(
            record.is_authored("thread-1", Some("turn-1")),
            "the existing turns still answer"
        );
    }

    /// A record that could not be read cannot take a new marker either: the
    /// prompt is refused rather than sent unrecorded.
    #[test]
    fn a_corrupt_record_refuses_a_new_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        std::fs::write(&path, "{not json").expect("write");
        let record = Authorship::open(path, "codex").expect("a corrupt file loads");
        assert!(!record.is_trusted());
        let error = record
            .begin("thread-1", Some("msg_cola_1"))
            .expect_err("an unusable record refuses");
        assert!(error.contains("unusable"), "{error}");
        assert!(
            record.is_authored("thread-1", Some("any-turn")),
            "the record still fails closed for reads"
        );
    }

    /// Finding 2: at capacity an unresolved submission is never evicted; the
    /// new prompt is refused instead.
    #[test]
    fn the_pending_cap_refuses_rather_than_evicting() {
        let record = Authorship::in_memory();
        for index in 0..MAX_PENDING_PER_THREAD {
            record
                .begin("thread-1", Some(&format!("msg_cola_{index}")))
                .expect("below the cap");
        }
        let error = record
            .begin("thread-1", Some("msg_cola_over_capacity"))
            .expect_err("at the cap");
        assert!(error.contains("unresolved submissions"), "{error}");
        assert!(
            record.is_pending_submission("thread-1", "msg_cola_0"),
            "the oldest unresolved submission is still owed"
        );
        assert!(!record.is_pending_submission("thread-1", "msg_cola_over_capacity"));
    }

    /// Finding 3: a submission whose request never reached the server stops
    /// vouching for the thread-wide fallback once its window elapses. The item
    /// that does carry the chosen id remains precisely cola's (nothing else can
    /// mint it), but a foreign live turn with no client id does not.
    #[test]
    fn an_expired_marker_stops_vouching_for_a_live_turn() {
        let record = Authorship::in_memory();
        record
            .begin("thread-1", Some("msg_cola_never"))
            .expect("the marker is recorded");
        std::thread::sleep(std::time::Duration::from_millis((PENDING_TTL_MS as u64) + 30));
        assert!(!record.is_pending("thread-1"), "the fallback expired");
        assert!(
            !record.is_pending_submission("thread-1", "msg_cola_never"),
            "the precise window expired too"
        );
        assert!(!record.is_authored("thread-1", None));
    }

    #[test]
    fn a_definite_refusal_clears_only_its_own_marker() {
        let record = Authorship::in_memory();
        record
            .begin("thread-1", Some("msg_cola_refused"))
            .expect("marker");
        record.begin("thread-1", Some("msg_cola_retry")).expect("marker");
        record.abandon("thread-1", Some("msg_cola_refused"));
        assert!(!record.is_pending_submission("thread-1", "msg_cola_refused"));
        assert!(
            record.is_pending_submission("thread-1", "msg_cola_retry"),
            "a retry's marker is not cleared by its predecessor's failure"
        );
    }

    #[test]
    fn a_pending_prompt_answers_before_its_turn_id_is_known() {
        let record = Authorship::in_memory();
        assert!(!record.is_authored("thread-1", None), "nothing in flight");
        record.begin("thread-1", Some("msg_cola_1")).expect("marker");
        assert!(
            record.is_authored("thread-1", None),
            "the provenance window before the response is cola's"
        );
        assert!(
            !record.is_authored("thread-2", None),
            "another thread's prompt does not vouch for this one"
        );
        record.bind("thread-1", "turn-1", Some("msg_cola_1"));
        assert!(
            !record.is_authored("thread-1", None),
            "the bound turn id is the record now"
        );
        assert!(record.is_authored("thread-1", Some("turn-1")));
    }

    #[test]
    fn an_abandoned_prompt_leaves_no_marker() {
        let record = Authorship::in_memory();
        record.begin("thread-1", Some("msg_cola_1")).expect("marker");
        record.abandon("thread-1", Some("msg_cola_1"));
        assert!(!record.is_authored("thread-1", None));
    }

    /// Two prompts on one thread (a retry racing its predecessor) each hold a
    /// marker; one completion must not clear the other's.
    #[test]
    fn a_pending_marker_is_counted_per_prompt() {
        let record = Authorship::in_memory();
        record.begin("thread-1", None).expect("marker");
        record.begin("thread-1", None).expect("marker");
        record.bind("thread-1", "turn-1", None);
        assert!(record.is_authored("thread-1", Some("turn-1")));
        assert!(
            record.is_authored("thread-1", None),
            "the second prompt is still in flight"
        );
        record.bind("thread-1", "turn-2", None);
        assert!(!record.is_authored("thread-1", None));
    }

    #[test]
    fn a_corrupt_record_fails_closed_and_keeps_its_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        std::fs::write(&path, "{not json").expect("write");

        let record = Authorship::open(path.clone(), "codex").expect("a corrupt file still loads");
        assert!(!record.is_trusted());
        assert!(
            record.is_authored("thread-1", Some("turn-1")),
            "cola's own turn must never be reported external"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("the bytes"),
            "{not json",
            "an unusable record is left for a human, never rewritten"
        );
    }

    #[test]
    fn a_future_version_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        std::fs::write(&path, r#"{"version":999,"turns":{}}"#).expect("write");
        let error = Authorship::open(path, "codex").expect_err("a future version is refused");
        assert!(error.to_string().contains("999"), "{error}");
    }

    /// ADR-0091 §4: a parsed version-0 body is migrated once, at load.
    #[test]
    fn a_version_zero_record_is_rewritten_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"turns":{{"codex":{{"thread-1":["turn-1"]}}}},"pending":{{"codex":{{"thread-1":[{{"id":"msg_cola_1","at_ms":{}}}]}}}}}}"#,
                now_ms()
            ),
        )
        .expect("write");

        let record = Authorship::open(path.clone(), "codex").expect("the record loads");
        assert!(record.is_authored("thread-1", Some("turn-1")));
        assert!(record.is_pending_submission("thread-1", "msg_cola_1"));

        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
        assert_eq!(
            file["version"],
            crate::state::STATE_FORMAT_VERSION,
            "the load rewrites the version-0 body: {file}"
        );
        assert_eq!(file["turns"]["codex"]["thread-1"], serde_json::json!(["turn-1"]));
        assert_eq!(
            file["pending"]["codex"]["thread-1"][0]["id"],
            serde_json::json!("msg_cola_1")
        );
    }

    #[test]
    fn a_thread_can_be_forgotten_and_the_file_follows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        let record = written(&path, "codex", "thread-1", &["turn-1"]);
        record
            .begin("thread-1", Some("msg_cola_pending"))
            .expect("marker");
        record.forget("thread-1");
        assert!(!record.is_authored("thread-1", Some("turn-1")));
        assert!(!record.is_pending("thread-1"));
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
        assert!(
            file["turns"].get("codex").is_none(),
            "no empty section is written: {file}"
        );
        assert!(file["pending"].get("codex").is_none(), "{file}");
    }

    #[test]
    fn the_per_thread_tail_is_bounded() {
        let record = Authorship::in_memory();
        for index in 0..(MAX_TURNS_PER_THREAD + 10) {
            record
                .begin("thread-1", Some(&format!("msg_cola_{index}")))
                .expect("marker");
            record.bind(
                "thread-1",
                &format!("turn-{index}"),
                Some(&format!("msg_cola_{index}")),
            );
        }
        assert!(
            record.is_authored("thread-1", Some(&format!("turn-{}", MAX_TURNS_PER_THREAD + 9))),
            "the newest turn is recorded"
        );
        assert!(
            !record.is_authored("thread-1", Some("turn-0")),
            "the cap drops the oldest bound turn, not the newest"
        );
    }
}
