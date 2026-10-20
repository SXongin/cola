//! The **Terminal Run Record** (ADR-0086 decision 5): one small durable file
//! per **Chat**/**Topic**, the only durable thing a **Terminal Command** has.
//!
//! Its job is narrow and that is the point. It is not a ledger: it has no
//! liveness model, no takeover and no re-adoption of a lost run. It exists so a
//! later cola process can (a) find the **Terminal Card** a run left behind and
//! tell its readers it can no longer be tracked, and (b) let that card's
//! 「↻ 重跑」 and 「📄 完整输出」 still know their data. It carries the run id, the
//! card's message id and chat, the **Terminal Log** path, the command, the cwd,
//! the timeout, the process ids and the state (`running` / `settled` / `lost`).
//!
//! Two rules come from the ADR and shape every caller:
//!
//! - **The record is written before the child is spawned** — the order is
//!   `card → record → spawn → record completed` — so no child ever runs without
//!   a record naming its card. The pre-spawn write is therefore *checked*: a
//!   writer that cannot land refuses the run rather than spawning an
//!   unnameable child.
//! - **A persisted process id is never signalled.** The field exists so a lost
//!   run is *reported*; a pid may have been reused since, so the sweep reads it
//!   and does nothing else with it.
//!
//! It is kept until the next run in that chat replaces it, which is what makes
//! a superseded card's buttons stale clicks and keeps the newest card's usable
//! across a restart.
//!
//! The load fails open and the write is atomic (the shared [`sidecar`]
//! mechanics); only the pre-spawn write is checked, because only that one is a
//! guarantee rather than progress.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bridge::sidecar;
use crate::config::ThreadKey;

/// What a run's record says about the run itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TerminalRunState {
    /// Written before the child was spawned, and not yet reported over. A
    /// record still in this state at startup is a run cola can no longer track:
    /// the sweep reports it and marks it [`Self::Lost`].
    Running,
    /// The run ended — an exit code, a 终止, or a spawn that never happened.
    /// Kept, so the card's buttons stay usable until the next run replaces it.
    Settled,
    /// The sweep found it `running` after a restart. The command may still be
    /// alive as an orphan (hand-launched cola) or may have been taken with
    /// cola (a supervised one); cola does not guess and never signals it.
    Lost,
}

/// One run's durable facts (ADR-0086 decision 5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TerminalRunRecord {
    /// The id the **Terminal Card**'s buttons carry; the click that names any
    /// other id is stale.
    pub(crate) run_id: String,
    pub(crate) chat_id: String,
    pub(crate) thread_id: String,
    /// The card's message id — the address a startup sweep patches. The card is
    /// sent before this file is written, so a record in the wild carries it.
    #[serde(default)]
    pub(crate) card_id: String,
    /// The **Terminal Log** the run writes and 「📄 完整输出」 delivers.
    pub(crate) log_path: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
    /// The run's cap in ms: 「↻ 重跑」 replays exactly this.
    pub(crate) timeout_ms: u64,
    /// The ids the run started. `process_group(0)` makes the child its own
    /// group leader, so its pid is the group id. **Never signalled** — a pid
    /// read back after a restart may belong to something else entirely.
    #[serde(default)]
    pub(crate) pids: Vec<u32>,
    /// Whether the chat is a group. A button payload carries no chat type, and
    /// a rerun owes the same per-chat-type Completion Notice the original run
    /// did (ADR-0086 decision 12).
    #[serde(default)]
    pub(crate) is_group: bool,
    pub(crate) state: TerminalRunState,
}

impl TerminalRunRecord {
    /// The Chat/Topic this record belongs to: its file is keyed by it.
    pub(crate) fn key(&self) -> ThreadKey {
        ThreadKey::new(self.chat_id.clone(), self.thread_id.clone())
    }

    /// The cap a rerun replays.
    pub(crate) fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

/// The record directory: one file per **Chat**/**Topic**, beside the session
/// mapping (`~/.cola/terminal-runs` in production, `<dir>` of
/// `[bridge] session_file`). One file per conversation is deliberate — a write
/// touches only its own chat, so two chats' runs never rewrite each other.
pub(crate) struct TerminalRunRecords {
    dir: PathBuf,
}

impl TerminalRunRecords {
    /// Records beside the session mapping, the same placement the reminder's
    /// pin set uses.
    pub(crate) fn beside_session_file(session_file: &Path) -> Self {
        Self::new(session_file.with_file_name("terminal-runs"))
    }

    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The directory the records live in. A test asserts a sweep's effect on the
    /// files; production never needs the path itself.
    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file one Chat/Topic's record lives in.
    pub(crate) fn path(&self, key: &ThreadKey) -> PathBuf {
        self.dir.join(file_name(key))
    }

    /// The chat's current record. Missing, unreadable or corrupt all read as
    /// "none": a record only feeds a restart's recovery and a stale-click
    /// check, so a broken one must never wedge the click or the startup.
    pub(crate) fn read(&self, key: &ThreadKey) -> Option<TerminalRunRecord> {
        let path = self.path(key);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("terminal record: could not read {}: {e}", path.display());
                return None;
            }
        };
        match serde_json::from_str(&raw) {
            Ok(record) => Some(record),
            Err(e) => {
                tracing::warn!("terminal record: could not parse {}: {e}", path.display());
                None
            }
        }
    }

    /// Whether `run_id` names the chat's current run — the rule every card
    /// button's staleness is decided by (ADR-0086 decision 4), and the only
    /// source of "current" that survives a restart. An in-memory run table is
    /// empty in a fresh process, so a card whose run the chat has replaced must
    /// be refused from the record instead of submitting or re-running it.
    pub(crate) fn is_current(&self, key: &ThreadKey, run_id: &str) -> bool {
        !run_id.is_empty() && self.read(key).is_some_and(|record| record.run_id == run_id)
    }

    /// Write `record`, **checked**: this is the one write a run's identity
    /// depends on, so the pre-spawn caller refuses to spawn when it fails.
    pub(crate) fn write(&self, record: &TerminalRunRecord) -> std::io::Result<()> {
        let data = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        sidecar::write_atomic(&self.path(&record.key()), &data)
    }

    /// Rewrite a record whose card is already durable (the completed write, the
    /// settling mark): a failure is logged and changes nothing else, because
    /// the guarantee was the pre-spawn write.
    pub(crate) fn update(&self, record: &TerminalRunRecord) {
        if let Err(e) = self.write(record) {
            tracing::warn!(
                "terminal record: could not write {} for chat {}: {e}",
                record.run_id,
                record.chat_id
            );
        }
    }

    /// Every record on disk — the startup sweep's input. A corrupt file is
    /// skipped with a warning, never an error.
    pub(crate) fn load_all(&self) -> Vec<TerminalRunRecord> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                tracing::warn!("terminal record: could not read {}", path.display());
                continue;
            };
            match serde_json::from_str::<TerminalRunRecord>(&raw) {
                Ok(record) => records.push(record),
                Err(e) => {
                    tracing::warn!("terminal record: could not parse {}: {e}", path.display())
                }
            }
        }
        records
    }
}

/// One record's file name: a readable slug of the Chat/Topic plus a stable hash
/// of the whole key. The slug keeps a `ls` of the directory meaningful; the hash
/// keeps two keys that sanitise to the same slug (a `:` and a `-`, say) apart,
/// and unlike a `DefaultHasher` it is fixed across toolchains, so a record
/// written by one build is found by the next.
fn file_name(key: &ThreadKey) -> String {
    let slug: String = key
        .chat_id
        .chars()
        .chain(std::iter::once('_'))
        .chain(key.thread_id.chars())
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    format!("{slug}-{:016x}.json", fnv1a(key))
}

/// FNV-1a over the key's two halves, separated so `("ab", "c")` and
/// `("a", "bc")` cannot collide.
fn fnv1a(key: &ThreadKey) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in key
        .chat_id
        .bytes()
        .chain(std::iter::once(0))
        .chain(key.thread_id.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(chat: &str, thread: &str) -> ThreadKey {
        ThreadKey::new(chat.to_string(), thread.to_string())
    }

    fn record(key: &ThreadKey, run_id: &str) -> TerminalRunRecord {
        TerminalRunRecord {
            run_id: run_id.to_string(),
            chat_id: key.chat_id.clone(),
            thread_id: key.thread_id.clone(),
            card_id: "om_card".to_string(),
            log_path: "/tmp/terminal/run.log".to_string(),
            command: "echo hi".to_string(),
            cwd: "/work".to_string(),
            timeout_ms: 42_000,
            pids: vec![1234],
            is_group: true,
            state: TerminalRunState::Running,
        }
    }

    /// The record is the whole durable fact set, so every field must survive
    /// the round trip — and the state must read as the ADR's own lowercase
    /// words, not a Rust variant name.
    #[test]
    fn a_record_round_trips_with_its_state_spelled_as_the_adr_does() {
        let dir = tempfile::tempdir().unwrap();
        let records = TerminalRunRecords::new(dir.path().to_path_buf());
        let key = key("oc_chat", "omt_topic");
        let written = record(&key, "run-1");
        records.write(&written).unwrap();

        assert_eq!(records.read(&key), Some(written.clone()));
        let raw = std::fs::read_to_string(records.path(&key)).unwrap();
        assert!(raw.contains("\"running\""), "the state is the ADR's word: {raw}");
        assert!(!raw.contains("Running"), "not the Rust variant: {raw}");
    }

    /// A chat's record is replaced by that chat's next run and untouched by
    /// another chat's: one file per conversation is what makes that true.
    #[test]
    fn one_file_per_chat_and_a_write_never_touches_another_chat() {
        let dir = tempfile::tempdir().unwrap();
        let records = TerminalRunRecords::new(dir.path().to_path_buf());
        let a = key("oc_a", "oc_a");
        let b = key("oc_a", "omt_topic_b");
        records.write(&record(&a, "run-a1")).unwrap();
        records.write(&record(&b, "run-b1")).unwrap();
        records.write(&record(&a, "run-a2")).unwrap();

        assert_eq!(records.read(&a).unwrap().run_id, "run-a2");
        assert_eq!(records.read(&b).unwrap().run_id, "run-b1");
        let mut loaded: Vec<String> = records
            .load_all()
            .into_iter()
            .map(|record| record.run_id)
            .collect();
        loaded.sort();
        assert_eq!(loaded, vec!["run-a2", "run-b1"]);
    }

    /// Two keys that sanitise to the same readable slug still get different
    /// files; and the same key always maps to the same file (a restart must
    /// find what the previous process wrote).
    #[test]
    fn the_file_name_is_stable_and_distinguishes_keys_that_look_alike() {
        let a = key("oc:one", "oc/two");
        let b = key("oc_one", "oc_two");
        assert_ne!(file_name(&a), file_name(&b), "a slug alone is not an identity");
        assert_eq!(file_name(&a), file_name(&key("oc:one", "oc/two")));
        assert!(
            file_name(&a).starts_with("oc_one_oc_two-"),
            "the slug stays readable: {}",
            file_name(&a)
        );
    }

    /// The record is what decides a button's staleness, and it is the ONLY
    /// source that survives a restart: the replaced run is stale, the current
    /// one is not — whether it settled or the sweep marked it lost.
    #[test]
    fn only_the_current_record_names_a_current_run() {
        let dir = tempfile::tempdir().unwrap();
        let records = TerminalRunRecords::new(dir.path().to_path_buf());
        let key = key("oc_chat", "oc_chat");
        assert!(
            !records.is_current(&key, "run-1"),
            "no record, nothing is current"
        );
        assert!(!records.is_current(&key, ""), "an empty id never matches");

        records.write(&record(&key, "run-1")).unwrap();
        assert!(records.is_current(&key, "run-1"));

        let mut lost = record(&key, "run-2");
        lost.state = TerminalRunState::Lost;
        records.write(&lost).unwrap();
        assert!(!records.is_current(&key, "run-1"), "the replaced run is stale");
        assert!(records.is_current(&key, "run-2"), "a lost run is still current");
    }

    /// A corrupt or missing record reads as "none" rather than an error: the
    /// click and the startup sweep must survive a half-written file.
    #[test]
    fn a_corrupt_or_missing_record_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let records = TerminalRunRecords::new(dir.path().to_path_buf());
        let key = key("oc_chat", "oc_chat");
        assert_eq!(records.read(&key), None, "no record is not an error");

        std::fs::write(records.path(&key), b"{not json").unwrap();
        assert_eq!(records.read(&key), None, "a corrupt record reads as none");
        assert!(records.load_all().is_empty(), "and it is skipped by the sweep");
    }
}
