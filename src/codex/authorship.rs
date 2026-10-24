//! The record of turns **cola started**, on a client that assigns its own ids
//! (ADR-0090's authorship consequence).
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
//! { "version": 1, "turns": { "<client id>": { "<thread id>": ["turn-1"] } } }
//! ```
//!
//! **Provenance ordering.** A turn id only arrives in the `turn/start` response,
//! but the server can emit the turn's user item to the subscription before that
//! response is processed. A prompt therefore records a **pending** submission
//! for its thread *before* the request goes out, and the authorship answer for
//! a turn this process has not bound yet is "cola's" while that pending marker
//! exists. The window between request and response can then never classify
//! cola's own message as external.
//!
//! **Degrading safely.** This file is the only evidence of who wrote a turn, so
//! it fails **closed**: a corrupt, unreadable or unwritable record marks the
//! whole record untrusted, and every message is then answered as cola-authored
//! (with an error logged) until a human inspects the file. Silently answering
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

/// The version-1 (ADR-0091) file.
#[derive(Debug, Serialize, Deserialize)]
struct AuthorshipFile {
    /// No earlier release wrote this file, so a body without a version is
    /// hand-made: it still parses (the shape is the only one this file has
    /// ever had) and is read under the client scope it names, exactly as
    /// ADR-0091 treats a version-0 body.
    #[serde(default)]
    version: u32,
    #[serde(default)]
    turns: ClientScoped<Vec<String>>,
}

#[derive(Default)]
struct State {
    turns: ClientScoped<Vec<String>>,
    /// Threads with a cola prompt in flight, awaiting its turn id. A count, not
    /// a flag: two prompts on one thread (a retry racing its predecessor) must
    /// not have one completion clear the other's marker.
    pending: HashMap<String, usize>,
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
    /// declared version this build does not write is refused, per ADR-0091
    /// §5 — the same rule every startup store follows.
    pub(crate) fn open(path: PathBuf, client_id: &str) -> anyhow::Result<Arc<Self>> {
        let mut state = State::default();
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
            VersionedRead::Legacy(raw) | VersionedRead::Current(raw) => {
                match serde_json::from_str::<AuthorshipFile>(&raw) {
                    Ok(file) => state.turns = file.turns,
                    Err(error) => {
                        untrusted(&path, &format!("the record did not parse: {error}"));
                        state.untrusted = true;
                    }
                }
            }
        }
        Ok(Arc::new(Self {
            path: Some(path),
            client_id: client_id.to_string(),
            state: Mutex::new(state),
        }))
    }

    /// Mark a cola prompt as in flight for `thread` **before** it is sent. The
    /// turn id is bound by [`Self::bind`] when the response names it.
    pub(crate) fn begin(&self, thread: &str) {
        let mut state = self.state.lock().expect("authorship record");
        *state.pending.entry(thread.to_string()).or_default() += 1;
    }

    /// A prompt that never reached the server: drop its pending marker.
    pub(crate) fn abandon(&self, thread: &str) {
        let mut state = self.state.lock().expect("authorship record");
        if let Some(count) = state.pending.get_mut(thread) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.pending.remove(thread);
            }
        }
    }

    /// Bind the turn the server returned for `thread`'s pending prompt and
    /// persist it **before the caller sees the prompt succeed**, so no answer
    /// can be classified before the record exists.
    pub(crate) fn bind(&self, thread: &str, turn: &str) {
        let mut state = self.state.lock().expect("authorship record");
        if let Some(count) = state.pending.get_mut(thread) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.pending.remove(thread);
            }
        }
        {
            let turns = state.turns.entry_mut(&self.client_id, thread);
            if !turns.iter().any(|recorded| recorded == turn) {
                turns.push(turn.to_string());
                if turns.len() > MAX_TURNS_PER_THREAD {
                    let excess = turns.len() - MAX_TURNS_PER_THREAD;
                    turns.drain(..excess);
                }
            }
        }
        if !state.untrusted
            && let Err(error) = self.persist(&state)
        {
            state.untrusted = true;
            untrusted(self.path.as_deref().expect("a durable record"), &error);
        }
    }

    /// Forget a thread's record (the thread was deleted): nothing can ask about
    /// it again, and a recreated id must not inherit an old turn.
    pub(crate) fn forget(&self, thread: &str) {
        let mut state = self.state.lock().expect("authorship record");
        state.pending.remove(thread);
        state.turns.remove(&self.client_id, thread);
        if !state.untrusted
            && let Err(error) = self.persist(&state)
        {
            state.untrusted = true;
            untrusted(self.path.as_deref().expect("a durable record"), &error);
        }
    }

    /// Whether the record is trusted. When it is not, [`Self::is_authored`]
    /// answers `true` for everything (fail closed).
    pub(crate) fn is_trusted(&self) -> bool {
        !self.state.lock().expect("authorship record").untrusted
    }

    /// Whether a prompt is in flight for `thread`. The turn id is not known
    /// yet, so the item's live turn is the record's stand-in until the response
    /// binds it (ADR-0092 decision 2).
    pub(crate) fn is_pending(&self, thread: &str) -> bool {
        self.state
            .lock()
            .expect("authorship record")
            .pending
            .get(thread)
            .copied()
            .unwrap_or(0)
            > 0
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
            None => state.pending.get(thread).copied().unwrap_or(0) > 0,
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
        };
        let data = serde_json::to_vec(&file).map_err(|error| error.to_string())?;
        sidecar::write_atomic(path, &data).map_err(|error| error.to_string())
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
            record.begin(thread);
            record.bind(thread, turn);
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
        opencode.begin("thread-1");
        opencode.bind("thread-1", "turn-2");
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
        assert_eq!(file["turns"]["codex"]["thread-1"], serde_json::json!(["turn-1"]));
        assert_eq!(
            file["turns"]["default"]["thread-1"],
            serde_json::json!(["turn-2"])
        );
    }

    #[test]
    fn a_pending_prompt_answers_before_its_turn_id_is_known() {
        let record = Authorship::in_memory();
        assert!(!record.is_authored("thread-1", None), "nothing in flight");
        record.begin("thread-1");
        assert!(
            record.is_authored("thread-1", None),
            "the provenance window before the response is cola's"
        );
        assert!(
            !record.is_authored("thread-2", None),
            "another thread's prompt does not vouch for this one"
        );
        record.bind("thread-1", "turn-1");
        assert!(
            !record.is_authored("thread-1", None),
            "the bound turn id is the record now"
        );
        assert!(record.is_authored("thread-1", Some("turn-1")));
    }

    #[test]
    fn an_abandoned_prompt_leaves_no_marker() {
        let record = Authorship::in_memory();
        record.begin("thread-1");
        record.abandon("thread-1");
        assert!(!record.is_authored("thread-1", None));
    }

    /// Two prompts on one thread (a retry racing its predecessor) each hold a
    /// marker; one completion must not clear the other's.
    #[test]
    fn a_pending_marker_is_counted_per_prompt() {
        let record = Authorship::in_memory();
        record.begin("thread-1");
        record.begin("thread-1");
        record.bind("thread-1", "turn-1");
        assert!(record.is_authored("thread-1", Some("turn-1")));
        assert!(
            record.is_authored("thread-1", None),
            "the second prompt is still in flight"
        );
        record.bind("thread-1", "turn-2");
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
        record.begin("thread-1");
        record.bind("thread-1", "turn-2");
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

    #[test]
    fn a_thread_can_be_forgotten_and_the_file_follows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("authored_turns.json");
        let record = written(&path, "codex", "thread-1", &["turn-1"]);
        record.forget("thread-1");
        assert!(!record.is_authored("thread-1", Some("turn-1")));
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the file")).expect("json");
        assert!(
            file["turns"].get("codex").is_none(),
            "no empty section is written: {file}"
        );
    }

    #[test]
    fn the_per_thread_tail_is_bounded() {
        let record = Authorship::in_memory();
        for index in 0..(MAX_TURNS_PER_THREAD + 10) {
            record.begin("thread-1");
            record.bind("thread-1", &format!("turn-{index}"));
        }
        assert!(
            record.is_authored("thread-1", Some(&format!("turn-{}", MAX_TURNS_PER_THREAD + 9))),
            "the newest turn is recorded"
        );
        assert!(
            !record.is_authored("thread-1", Some("turn-0")),
            "the cap drops the oldest, not the newest"
        );
    }
}
