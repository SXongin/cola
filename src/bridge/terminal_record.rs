//! The **Terminal Run Record** (ADR-0086 decision 5): one small durable file
//! per **Chat**/**Topic**, the only durable thing a **Terminal Command** has.
//!
//! Its job is narrow and that is the point. It is not a ledger: it has no
//! liveness model, no takeover and no re-adoption of a lost run. It exists so a
//! later cola process can (a) find the **Terminal Card** a run left behind and
//! tell its readers it can no longer be tracked, and (b) let that card's
//! 「↻ 重跑」 and 「📄 完整输出」 still know their data. It carries the run id, the
//! card's message id and chat, the **Terminal Log** path, the command, the cwd,
//! the timeout, the process ids, the state (`running` / `settled` / `lost`), the
//! run's ending, the 「问 AI」 submission bit and the run's last update time. A
//! restart uses the state to report a run it can no longer track, the ending to
//! rebuild a settled card a button was clicked on, the bit to refuse a second
//! submission (whole-branch review, finding 4), and the update time for
//! retention (spec #821 user story 23).
//!
//! A settle whose **card has not been told** is a different kind of fact, and it
//! does NOT live here: the chat's next run replaces this file, so an obligation
//! kept here would be erased by that very write. It lives in
//! [`TerminalOwedSettlements`], one file per run id beside the records, and is
//! dropped only once its card PATCH landed (whole-branch review, finding 1).
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
//! Every write of one Chat/Topic's record takes that chat's lock, because the
//! record is replaced whole: a late read-modify-write (the 「问 AI」 marker) must
//! never rewrite a snapshot taken before the chat's next run existed
//! (whole-branch review, finding 3).
//!
//! It is kept until the next run in that chat replaces it **or until
//! `[bridge] log_days` expires it**, whichever comes first (spec #821 user story
//! 23: a debugging session must not leave a directory of secrets around
//! forever). The age sweep deletes an expired record and its log together, except
//! a `lost` record whose process group may still be writing; that is what makes
//! a superseded card's buttons stale clicks and keeps the newest card's usable
//! across a restart.
//!
//! The load fails open and the write is atomic (the shared [`sidecar`]
//! mechanics); only the pre-spawn write is checked, because only that one is a
//! guarantee rather than progress.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bridge::sidecar;
use crate::config::ThreadKey;

/// The most a record file may contain and still be parsed. A record is a few
/// hundred bytes of metadata, but its command is whatever the Host typed (a
/// Feishu message's own bound is far below this), and the file is read inside a
/// card callback, so the read is bounded rather than trusting the filesystem
/// (CODING_STANDARDS: bound filesystem work in callbacks). Past this size the
/// record is treated as unreadable — the guards then refuse, and the sweep
/// leaves it alone.
const TERMINAL_RECORD_MAX_BYTES: u64 = 128 * 1024;

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
    /// Whether its card has been told is **not** recorded here: that obligation
    /// lives in [`TerminalOwedSettlements`], which survives this file's
    /// replacement.
    Settled,
    /// The sweep found it `running` after a restart. The command may still be
    /// alive as an orphan (hand-launched cola) or may have been taken with
    /// cola (a supervised one); cola does not guess and never signals it.
    Lost,
}

/// A run's ending (whole-branch review, finding 1), persisted **before** the
/// card is told so a restart can repeat the write the previous process could
/// not land. Every field is the settled card's own render input, which is why
/// it lives in the record rather than only in the card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TerminalRunOutcome {
    /// The child's real exit code; `None` when a signal ended it or it never
    /// started.
    pub(crate) exit_code: Option<i32>,
    /// Why the launch failed, when it did (missing cwd, no `sh`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spawn_error: Option<String>,
    pub(crate) elapsed_ms: u64,
    /// 终止 ended the run.
    pub(crate) stopped: bool,
    /// A group member outlived `sh`.
    pub(crate) survivors: bool,
    /// The size probe stopped the run at the observed threshold.
    pub(crate) output_over_limit: bool,
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
    /// Whether 「问 AI」 already handed this run's output to the model. The
    /// in-memory guard is process-local, so without this a restart's click
    /// would submit the same output again (whole-branch review, finding 4).
    #[serde(default)]
    pub(crate) ask_ai_submitted: bool,
    /// The run's ending, once it has one: exit code, spawn error, elapsed time,
    /// whether 终止 ended it, whether group members survived, whether the probe
    /// stopped it. Persisted so the settled card's **buttons** can rebuild the
    /// same card after a click — 「📄 完整输出」 and 「问 AI」 PATCH the card they
    /// were clicked on, and a rebuild that forgot a failed launch or an
    /// unconfirmed termination would misreport the run (spec #825). The owed
    /// settlement stays the durable obligation for finishing a failed final
    /// write; this is the display fact that outlives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<TerminalRunOutcome>,
    /// When this run last did something real — the pre-spawn write, the child
    /// coming up, or the settle — in epoch seconds. Retention (`[bridge]
    /// log_days`) measures from here, not from the file's mtime: a **resolution**
    /// (the lost sweep rewriting the state, the 「问 AI」 marker) preserves this
    /// value, and a bookkeeping write must not make an old log look young
    /// (spec #821 user story 23). `0` means a record written before this field
    /// existed; the sweeper then falls back to the file's mtime.
    #[serde(default)]
    pub(crate) updated_at: u64,
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

    /// This run did something real now: the pre-spawn write, the child coming
    /// up, or its settle. Retention measures from the last such moment;
    /// resolutions (a lost sweep, the 「问 AI」 marker) deliberately do not call
    /// this.
    pub(crate) fn touch(&mut self) {
        self.updated_at = now_epoch_secs();
    }
}

/// The wall clock in epoch seconds — the unit [`TerminalRunRecord::updated_at`]
/// is stored in.
pub(crate) fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// The record directory: one file per **Chat**/**Topic**, beside the session
/// mapping (`~/.cola/terminal-runs` in production, `<dir>` of
/// `[bridge] session_file`). One file per conversation is deliberate — a write
/// touches only its own chat, so two chats' runs never rewrite each other.
pub(crate) struct TerminalRunRecords {
    dir: PathBuf,
    /// One lock per Chat/Topic, taken by every write of that chat's record. The
    /// record is replaced whole, so a read-modify-write (the 「问 AI」 marker)
    /// that interleaved with the chat's next run could install a stale snapshot
    /// over the new run's record (whole-branch review, finding 3).
    locks: Mutex<HashMap<ThreadKey, Arc<Mutex<()>>>>,
    /// Test-only: how many more record writes may land before every one fails
    /// (`usize::MAX` = no limit). Models a temporarily unwritable record while
    /// the directory stays readable — the state a failed post-spawn pid update
    /// leaves behind, which retention must treat as unprovable.
    #[cfg(test)]
    writes_allowed: std::sync::atomic::AtomicUsize,
    /// Test-only: a one-shot pause inside the 「问 AI」 merge, between its
    /// re-read and its write. It lets a test hold that merge open while the
    /// chat's next run writes its own record — the interleave the chat lock
    /// forbids — and assert the newer record survives.
    #[cfg(test)]
    merge_pause: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl TerminalRunRecords {
    /// Records beside the session mapping, the same placement the reminder's
    /// pin set uses.
    pub(crate) fn beside_session_file(session_file: &Path) -> Self {
        Self::new(session_file.with_file_name("terminal-runs"))
    }

    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            locks: Mutex::new(HashMap::new()),
            #[cfg(test)]
            writes_allowed: std::sync::atomic::AtomicUsize::new(usize::MAX),
            #[cfg(test)]
            merge_pause: Mutex::new(None),
        }
    }

    /// The directory the records live in. The log sweep asks whether it exists
    /// at all (a test also asserts a sweep's effect on the files): a MISSING
    /// record directory beside existing logs cannot vouch for any of them.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file one Chat/Topic's record lives in.
    pub(crate) fn path(&self, key: &ThreadKey) -> PathBuf {
        self.dir.join(file_name(key))
    }

    /// The chat's current record. Missing, unreadable or corrupt all read as
    /// "none": a record only feeds a restart's recovery and a stale-click
    /// check, so a broken one must never wedge the click or the startup. A
    /// caller that cannot tell "no record" from "the run is replaced" must fail
    /// closed (see [`AskAiGuard`](crate::bridge::terminal::AskAiGuard) and
    /// [`is_current`](Self::is_current)).
    ///
    /// The file must also be *this* chat's: the record's own chat/topic is
    /// checked against `key`, and a mismatch reads as unreadable. Names are a
    /// slug plus a hash, not a proof — a misplaced or crafted file written under
    /// another chat's name would otherwise supply a matching `run_id` and let a
    /// stale card pass after a restart.
    pub(crate) fn read(&self, key: &ThreadKey) -> Option<TerminalRunRecord> {
        let path = self.path(key);
        let record = Self::load_file(&path)?;
        if record.chat_id != key.chat_id || record.thread_id != key.thread_id {
            tracing::warn!(
                "terminal record: {} names chat {}/{} instead of the file's {}/{}; ignoring it",
                path.display(),
                record.chat_id,
                record.thread_id,
                key.chat_id,
                key.thread_id
            );
            return None;
        }
        Some(record)
    }

    /// Read and parse one record file — the one implementation both
    /// [`read`](Self::read) and [`load_all`](Self::load_all) use, so the bound,
    /// the fail-open rule and the warnings cannot drift apart.
    ///
    /// The read is **bounded** to [`TERMINAL_RECORD_MAX_BYTES`]: this runs
    /// synchronously in a card callback for the stale-click guards, so it must
    /// not grow with whatever sits at the path.
    fn load_file(path: &Path) -> Option<TerminalRunRecord> {
        let raw = read_bounded(path)?;
        match serde_json::from_slice(&raw) {
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

    /// The Chat/Topic's record-write lock. Every mutation of that chat's record
    /// takes it, so no two read-modify-write sequences can interleave.
    fn chat_lock(&self, key: &ThreadKey) -> Arc<Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.clone())
                .or_default(),
        )
    }

    /// Write `record`, **checked**: this is the one write a run's identity
    /// depends on, so the pre-spawn caller refuses to spawn when it fails.
    pub(crate) fn write(&self, record: &TerminalRunRecord) -> std::io::Result<()> {
        let lock = self.chat_lock(&record.key());
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.write_locked(record)
    }

    /// [`write`](Self::write) for a caller already holding the chat's lock.
    fn write_locked(&self, record: &TerminalRunRecord) -> std::io::Result<()> {
        #[cfg(test)]
        if self.simulated_write_failure() {
            return Err(std::io::Error::other(
                "simulated record write failure (test seam)",
            ));
        }
        let data = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        sidecar::write_atomic(&self.path(&record.key()), &data)
    }

    /// Test-only: allow `writes` more record writes, then fail every one. Used
    /// to leave the on-disk record `running` with no pid while the directory
    /// stays readable.
    #[cfg(test)]
    pub(crate) fn fail_writes_after(&self, writes: usize) {
        self.writes_allowed
            .store(writes, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn simulated_write_failure(&self) -> bool {
        use std::sync::atomic::Ordering;
        let mut allowed = self.writes_allowed.load(Ordering::SeqCst);
        loop {
            if allowed == usize::MAX {
                return false;
            }
            if allowed == 0 {
                return true;
            }
            match self.writes_allowed.compare_exchange(
                allowed,
                allowed - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return false,
                Err(actual) => allowed = actual,
            }
        }
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

    /// Mark `run_id`'s record settled, as a **targeted merge**: only when the
    /// chat's current record still names that run, and only its state changes.
    ///
    /// The restart's owed-settle repair calls this after its card PATCH lands,
    /// and so does the live settle path — a landed PATCH alone must never drop
    /// the obligation while the on-disk record can still say `running`, or the
    /// next start's lost sweep would relabel the card it just repaired.
    ///
    /// A blind replace would be wrong twice over: the chat may have moved on to
    /// a newer run (whose record must not be touched), and the current record
    /// may have learned something since the snapshot was taken — today the
    /// 「问 AI」 bit — which a stale snapshot would silently clear.
    ///
    /// `true` means the obligation is discharged: the current record is
    /// **provably** absent, names another run, is already settled, or was just
    /// written. `false` means the record exists but cannot be read, or the write
    /// failed — either way the caller must keep the obligation, because a
    /// `running` record behind an unreadable file is exactly what the next
    /// start's lost sweep would act on.
    pub(crate) fn mark_settled(
        &self,
        key: &ThreadKey,
        run_id: &str,
        outcome: Option<&TerminalRunOutcome>,
    ) -> bool {
        let lock = self.chat_lock(key);
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(mut record) = self.read(key) else {
            return self.record_provably_absent(key);
        };
        if record.run_id != run_id || record.state != TerminalRunState::Running {
            return true;
        }
        record.state = TerminalRunState::Settled;
        // The record's own settle write can have failed; the ending the buttons
        // rebuild from would then be missing too. Fill it in, never overwrite one
        // already there (this is a resolution, not a new ending).
        if record.outcome.is_none()
            && let Some(outcome) = outcome
        {
            record.outcome = Some(outcome.clone());
        }
        match self.write_locked(&record) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    "terminal record: could not mark run {run_id} settled after its owed card landed: {error}"
                );
                false
            }
        }
    }

    /// Whether this chat's record file is **provably** absent.
    ///
    /// A missing file under a path that is not a directory (the record directory
    /// replaced by a file, say), or an entry that exists but will not parse, is
    /// not absence: something may still be on disk saying `running`, so an
    /// obligation must not be dropped on the strength of "I could not read it".
    /// A deleted store whose parent still exists is genuinely empty — nothing
    /// there could be lost-swept.
    fn record_provably_absent(&self, key: &ThreadKey) -> bool {
        match std::fs::metadata(&self.dir) {
            Ok(meta) if meta.is_dir() => match std::fs::metadata(self.path(key)) {
                Ok(_) => false,
                Err(error) => error.kind() == std::io::ErrorKind::NotFound,
            },
            // The store path exists but is not a directory: nothing provable.
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A deleted store cannot hold a record; an unreadable parent
                // means this is not knowing rather than absence.
                self.dir.parent().is_some_and(Path::is_dir)
            }
            Err(_) => false,
        }
    }

    /// Record that `run_id`'s output was submitted to the model — the durable
    /// half of 「问 AI」's one-submission rule (ADR-0086 decision 9).
    ///
    /// A **locked merge**, not a snapshot rewrite (whole-branch review, finding
    /// 3): under the chat's lock it re-reads the record, verifies it still names
    /// this run, and changes only the bit. If the chat has moved on, nothing is
    /// written — the newer run's record survives with its own fields.
    ///
    /// Best effort by design (review finding 4): a crash between the prompt's
    /// acceptance and this write, or a write that fails, can duplicate a
    /// submission after a restart. That window is accepted; failing the click
    /// over a bookkeeping write would be worse.
    pub(crate) fn mark_ask_ai_submitted(&self, key: &ThreadKey, run_id: &str) {
        let lock = self.chat_lock(key);
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(record) = self.read(key) else {
            return;
        };
        let Some(merged) = merge_ask_ai_submitted(record, run_id) else {
            return;
        };
        #[cfg(test)]
        {
            let pause = self
                .merge_pause
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(pause) = pause {
                pause();
            }
        }
        match self.write_locked(&merged) {
            Ok(()) => {}
            Err(error) => tracing::warn!(
                "terminal record: could not record the 问 AI submission of {}: {error}",
                merged.run_id
            ),
        }
    }

    /// The owed-settlement store that lives **beside** these records — a sibling
    /// directory, deliberately not inside it: the record directory being
    /// unusable (a plain file where it belongs, say) must not also take away the
    /// only place an ending can be recorded, or the card could never be told the
    /// run ended at all.
    pub(crate) fn owed(&self) -> TerminalOwedSettlements {
        let name = self
            .dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "terminal-runs".to_string());
        TerminalOwedSettlements::new(self.dir.with_file_name(format!("{name}-owed")))
    }

    /// Test-only: park the next 「问 AI」 merge after its re-read and before its
    /// write. The chat lock is held across the park, so a concurrent write of
    /// the chat's next record has to wait behind it — which is exactly the
    /// ordering the lock exists to guarantee.
    #[cfg(test)]
    pub(crate) fn pause_merge_once(&self, pause: impl FnOnce() + Send + 'static) {
        *self.merge_pause.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(pause));
    }

    /// Remove the chat's current record when it still names `run_id` — retention's
    /// last act (spec #821 user story 23).
    ///
    /// The re-read under the chat's lock is the point: the caller decided from a
    /// scan that this chat's record was old, but a successor run could have
    /// written its own pre-spawn record in between. Only the run the caller
    /// decided about may be deleted, so a successor's identity is never erased by
    /// an old decision.
    pub(crate) fn remove(&self, key: &ThreadKey, run_id: &str) -> bool {
        let lock = self.chat_lock(key);
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(record) = self.read(key) else {
            return false;
        };
        if record.run_id != run_id {
            return false;
        }
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                tracing::warn!("terminal record: could not expire run {run_id}: {error}");
                false
            }
        }
    }

    /// Every record on disk — the startup sweep's input. A corrupt file is
    /// skipped with a warning, never an error; callers that must know whether
    /// they saw everything use [`scan`](Self::scan).
    pub(crate) fn load_all(&self) -> Vec<TerminalRunRecord> {
        self.scan().0
    }

    /// Every record on disk, and whether the set is **complete** — the
    /// directory was readable, every entry could be listed, and every `.json`
    /// file in it parsed.
    ///
    /// The flag exists for the **Terminal Log** age sweep, which deletes files:
    /// a record it could not read may name the very log it is about to unlink,
    /// so an incomplete view must not sweep at all (see
    /// [`sweep_logs`](crate::bridge::terminal::sweep_logs)). A missing directory
    /// reads as complete-and-empty here — the sidecar convention, and what the
    /// startup report needs — while an unreadable one is not knowing, and says
    /// so. A caller that DELETES on the strength of the set must also check
    /// [`dir`](Self::dir): missing records beside existing files are not proof
    /// that nothing is referenced. A file whose record names a different chat/topic
    /// than its own name encodes counts as unreadable too: [`read`](Self::read)
    /// refuses it as this chat's record, so no sweep may treat the set as
    /// complete while it is there.
    pub(crate) fn scan(&self) -> (Vec<TerminalRunRecord>, bool) {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), true),
            Err(e) => {
                tracing::warn!(
                    "terminal record: could not read the record directory {}: {e}",
                    self.dir.display()
                );
                return (Vec::new(), false);
            }
        };
        let mut records = Vec::new();
        let mut complete = true;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(
                        "terminal record: could not list an entry in {}: {e}",
                        self.dir.display()
                    );
                    complete = false;
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match Self::load_file(&path) {
                // A record only means something at the path its OWN key names:
                // a file under another chat's name is unreadable, whether it was
                // misplaced or crafted, so the set is not complete.
                Some(record) if self.path(&record.key()) == path => records.push(record),
                Some(_) | None => complete = false,
            }
        }
        (records, complete)
    }
}

/// The **owed settlements**: one file per run whose ending the previous process
/// could not tell its card (whole-branch review, finding 1).
///
/// It lives **beside** the per-chat current records (`<records>-owed/`), never
/// inside one, for two reasons: the chat's next run replaces its current record,
/// and an obligation stored there would be erased by that write — leaving the
/// earlier card permanently live-looking; and the record directory being
/// unusable must not also take away the only place an ending can be recorded.
/// A file is removed only once its card PATCH landed, so the obligation is
/// retried by every start until then.
///
/// The startup sweep reads this set **independently of the current records**: an
/// unreadable or already-replaced record is no reason to abandon a card whose
/// ending is still owed.
pub(crate) struct TerminalOwedSettlements {
    dir: PathBuf,
}

impl TerminalOwedSettlements {
    fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The directory the obligations live in. Its absence means "nothing owed";
    /// an unreadable one is not knowing. A test uses it to plant a corrupt
    /// entry; production only asks `load_all`.
    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, run_id: &str) -> PathBuf {
        self.dir.join(owed_file_name(run_id))
    }

    /// Persist one obligation — the settled record snapshot, ending included —
    /// **checked**: the card may only be told a run ended once this landed,
    /// otherwise no restart could repeat the write.
    pub(crate) fn add(&self, record: &TerminalRunRecord) -> std::io::Result<()> {
        let data = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        sidecar::write_atomic(&self.path(&record.run_id), &data)
    }

    /// Drop an obligation whose card has been told. Best effort: a failure only
    /// means the next start repeats an idempotent PATCH.
    pub(crate) fn remove(&self, run_id: &str) {
        match std::fs::remove_file(self.path(run_id)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!("terminal owed: could not remove the obligation for {run_id}: {error}")
            }
        }
    }

    /// Every obligation on disk, and whether the set is **complete**.
    ///
    /// An incomplete set must not be read as "nothing owed": that is what would
    /// let a `running` sweep relabel a settled card 「已失去跟踪」. A missing
    /// directory is complete-and-empty (the sidecar convention); an unreadable
    /// one, or a file that will not parse or is not named for its own run, is
    /// not knowing.
    pub(crate) fn load_all(&self) -> (Vec<TerminalRunRecord>, bool) {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), true),
            Err(e) => {
                tracing::warn!("terminal owed: could not read {}: {e}", self.dir.display());
                return (Vec::new(), false);
            }
        };
        let mut owed = Vec::new();
        let mut complete = true;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(
                        "terminal owed: could not list an entry in {}: {e}",
                        self.dir.display()
                    );
                    complete = false;
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(raw) = read_bounded(&path) else {
                complete = false;
                continue;
            };
            match serde_json::from_slice::<TerminalRunRecord>(&raw) {
                // An obligation only means something at the path its OWN run id
                // names: anything else is unreadable, whether misplaced or
                // crafted.
                Ok(record) if self.path(&record.run_id) == path => owed.push(record),
                Ok(_) => complete = false,
                Err(e) => {
                    tracing::warn!("terminal owed: could not parse {}: {e}", path.display());
                    complete = false;
                }
            }
        }
        (owed, complete)
    }
}

/// The 「问 AI」 marker's whole decision, as one function: verify the record still
/// names the run being marked, then change **only** that bit. `None` means the
/// chat has moved on (or the bit is already set), and the newer record must not
/// be replaced by this snapshot.
fn merge_ask_ai_submitted(mut record: TerminalRunRecord, run_id: &str) -> Option<TerminalRunRecord> {
    if record.run_id != run_id || record.ask_ai_submitted {
        return None;
    }
    record.ask_ai_submitted = true;
    Some(record)
}

/// Read one sidecar file's bytes, bounded. `None` means missing, unreadable or
/// past [`TERMINAL_RECORD_MAX_BYTES`] — the record and the owed entry share the
/// bound because both can carry the Host's command.
fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("terminal record: could not read {}: {e}", path.display());
            return None;
        }
    };
    let mut raw = Vec::new();
    if let Err(e) = file.take(TERMINAL_RECORD_MAX_BYTES + 1).read_to_end(&mut raw) {
        tracing::warn!("terminal record: could not read {}: {e}", path.display());
        return None;
    }
    if raw.len() as u64 > TERMINAL_RECORD_MAX_BYTES {
        tracing::warn!(
            "terminal record: {} is larger than the {TERMINAL_RECORD_MAX_BYTES}-byte bound; ignoring it",
            path.display()
        );
        return None;
    }
    Some(raw)
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
    fnv1a_bytes(
        key.chat_id
            .bytes()
            .chain(std::iter::once(0))
            .chain(key.thread_id.bytes()),
    )
}

/// FNV-1a over a byte stream, in one place so every sidecar name agrees on the
/// hash and it stays fixed across toolchains.
fn fnv1a_bytes(bytes: impl Iterator<Item = u8>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// One obligation's file name: the run id sanitised for `ls`, plus its stable
/// hash. Run ids are normally `[0-9-]`, but the name is derived defensively so a
/// crafted id can never escape the directory, and the hash keeps two ids that
/// sanitise alike apart.
fn owed_file_name(run_id: &str) -> String {
    let slug: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    format!("{slug}-{:016x}.json", fnv1a_bytes(run_id.bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(chat: &str, thread: &str) -> ThreadKey {
        ThreadKey::new(chat.to_string(), thread.to_string())
    }

    /// A record store whose directories live INSIDE the tempdir: the owed
    /// sidecar is a sibling of the records directory, so a store rooted at the
    /// tempdir itself would leave one behind.
    fn store(dir: &tempfile::TempDir) -> TerminalRunRecords {
        let path = dir.path().join("terminal-runs");
        std::fs::create_dir_all(&path).unwrap();
        TerminalRunRecords::new(path)
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
            ask_ai_submitted: false,
            outcome: None,
            updated_at: 0,
        }
    }

    /// The record is the whole durable fact set, so every field must survive
    /// the round trip — and the state must read as the ADR's own lowercase
    /// words, not a Rust variant name.
    #[test]
    fn a_record_round_trips_with_its_state_spelled_as_the_adr_does() {
        let dir = tempfile::tempdir().unwrap();
        let records = store(&dir);
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
        let records = store(&dir);
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
        let records = store(&dir);
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

    /// #827 re-review (finding 2): a parseable file under a chat's name is not
    /// that chat's record. The record's own chat/topic must match the file's
    /// key, or a misplaced or crafted record could pass the restart staleness
    /// checks merely by carrying a matching run id.
    #[test]
    fn a_record_written_under_another_chats_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let records = store(&dir);
        let owner = key("oc_owner", "oc_owner");
        let other = key("oc_other", "oc_other");
        let owner_record = record(&owner, "run-owner");
        let raw = serde_json::to_vec(&owner_record).unwrap();
        // A perfectly valid record, installed under another chat's file name.
        std::fs::write(records.path(&other), &raw).unwrap();

        assert_eq!(
            records.read(&other),
            None,
            "another chat's record is unreadable at this key"
        );
        assert!(
            !records.is_current(&other, "run-owner"),
            "and it proves nothing about this chat's current run"
        );

        // The same bytes at the owner's own path are the owner's record.
        std::fs::write(records.path(&owner), &raw).unwrap();
        assert_eq!(records.read(&owner), Some(owner_record));
    }

    /// The read is bounded (a card callback reads it synchronously): a file
    /// past the bound is treated as unreadable, never loaded whole.
    #[test]
    fn an_oversized_record_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let records = store(&dir);
        let key = key("oc_chat", "oc_chat");
        // A valid record padded past the bound: parsable JSON that is simply too
        // large to belong in a click's callback.
        let mut record = record(&key, "run-1");
        record.command = "x".repeat(TERMINAL_RECORD_MAX_BYTES as usize);
        let raw = serde_json::to_vec(&record).unwrap();
        assert!(raw.len() as u64 > TERMINAL_RECORD_MAX_BYTES);
        std::fs::write(records.path(&key), &raw).unwrap();

        assert_eq!(records.read(&key), None, "past the bound is unreadable");
        assert!(!records.is_current(&key, "run-1"), "and it proves nothing");
        assert!(records.load_all().is_empty(), "the sweep skips it too");
    }

    /// A corrupt or missing record reads as "none" rather than an error: the
    /// click and the startup sweep must survive a half-written file.
    #[test]
    fn a_corrupt_or_missing_record_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let records = store(&dir);
        let key = key("oc_chat", "oc_chat");
        assert_eq!(records.read(&key), None, "no record is not an error");

        std::fs::write(records.path(&key), b"{not json").unwrap();
        assert_eq!(records.read(&key), None, "a corrupt record reads as none");
        assert!(records.load_all().is_empty(), "and it is skipped by the sweep");
    }

    /// Whole-branch review (finding 4): 「问 AI」's one-submission rule survives a
    /// restart because the record carries it — and only for the run the record
    /// still names.
    #[test]
    fn mark_ask_ai_submitted_only_marks_the_current_run() {
        let dir = tempfile::tempdir().unwrap();
        let records = store(&dir);
        let key = key("oc_chat", "oc_chat");
        records.write(&record(&key, "run-1")).unwrap();
        assert!(!records.read(&key).unwrap().ask_ai_submitted);

        records.mark_ask_ai_submitted(&key, "run-2");
        assert!(
            !records.read(&key).unwrap().ask_ai_submitted,
            "a superseded run id marks nothing"
        );

        records.mark_ask_ai_submitted(&key, "run-1");
        assert!(
            records.read(&key).unwrap().ask_ai_submitted,
            "the current run records its submission"
        );
    }

    /// Whole-branch review (finding 1): the ending a restart needs to finish the
    /// card's last write lives in the **owed sidecar**, not in the per-chat
    /// record — the chat's next run replaces that file, and the obligation must
    /// outlive it. The snapshot is a whole record, so the ending rides in its
    /// `outcome`.
    #[test]
    fn an_owed_settle_round_trips_beside_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let records = store(&dir);
        let key = key("oc_chat", "oc_chat");
        let mut settled = record(&key, "run-1");
        settled.state = TerminalRunState::Settled;
        settled.outcome = Some(TerminalRunOutcome {
            exit_code: Some(3),
            spawn_error: None,
            elapsed_ms: 1_500,
            stopped: false,
            survivors: true,
            output_over_limit: false,
        });
        records.owed().add(&settled).unwrap();
        assert_eq!(records.owed().load_all(), (vec![settled.clone()], true));

        // Replacing the chat's current record — what the next run does — leaves
        // the obligation untouched.
        records.write(&record(&key, "run-2")).unwrap();
        assert_eq!(
            records.owed().load_all(),
            (vec![settled.clone()], true),
            "the obligation is not the per-chat record"
        );

        records.owed().remove("run-1");
        assert_eq!(records.owed().load_all(), (Vec::new(), true));
    }

    /// An unreadable obligation is "not knowing", never "nothing owed": a sweep
    /// that read it as empty could relabel a settled card lost.
    #[test]
    fn an_unreadable_owed_entry_makes_the_set_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let records = store(&dir);
        std::fs::create_dir_all(records.owed().dir()).unwrap();
        std::fs::write(records.owed().dir().join("broken.json"), b"{ not json").unwrap();

        let (owed, complete) = records.owed().load_all();
        assert!(owed.is_empty());
        assert!(!complete, "an unparseable obligation is not an empty set");

        std::fs::remove_file(records.owed().dir().join("broken.json")).unwrap();
        assert_eq!(
            records.owed().load_all(),
            (Vec::new(), true),
            "and with it gone the set is complete again"
        );
    }

    /// Whole-branch review (finding 3): the marker only ever changes its own
    /// bit, and only while the record still names that run. Everything else in
    /// the newer run's record — command, cwd, state, timeout, card — survives.
    #[test]
    fn the_ask_ai_marker_never_rewrites_a_successor_runs_record() {
        let dir = tempfile::tempdir().unwrap();
        let records = store(&dir);
        let key = key("oc_chat", "oc_chat");
        let mut successor = record(&key, "run-b");
        successor.command = "cargo test".into();
        successor.cwd = "/successor".into();
        successor.timeout_ms = 90_000;
        successor.card_id = "om_b".into();
        successor.state = TerminalRunState::Settled;
        records.write(&successor).unwrap();

        // The marker for a run the chat has moved on from writes nothing.
        assert_eq!(
            merge_ask_ai_submitted(successor.clone(), "run-a"),
            None,
            "a stale snapshot is not a permission to rewrite the new record"
        );
        assert_eq!(
            merge_ask_ai_submitted(successor.clone(), "run-b").map(|merged| merged.ask_ai_submitted),
            Some(true)
        );

        records.mark_ask_ai_submitted(&key, "run-a");
        assert_eq!(
            records.read(&key),
            Some(successor),
            "the successor's record survives intact"
        );
    }

    /// Whole-branch review (finding 3): the merge is a locked read-modify-write.
    /// A merge already in flight (parked after its re-read) must block the
    /// chat's next run's write; the successor's record is what lands last.
    #[test]
    fn an_in_flight_marker_cannot_clobber_the_next_runs_record() {
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let records = Arc::new(store(&dir));
        let key = key("oc_chat", "oc_chat");
        records.write(&record(&key, "run-a")).unwrap();

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        records.pause_merge_once(move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        });

        let marker = {
            let records = Arc::clone(&records);
            let key = key.clone();
            std::thread::spawn(move || records.mark_ask_ai_submitted(&key, "run-a"))
        };
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the merge reached its pause");

        let mut successor = record(&key, "run-b");
        successor.command = "sleep 1".into();
        successor.cwd = "/successor".into();
        let (started_tx, started_rx) = mpsc::channel();
        let (written_tx, written_rx) = mpsc::channel();
        let writer = {
            let records = Arc::clone(&records);
            let successor = successor.clone();
            std::thread::spawn(move || {
                let _ = started_tx.send(());
                let result = records.write(&successor);
                let _ = written_tx.send(());
                result
            })
        };
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the successor's write reached the lock");
        // Give the writer every chance to slip in while the merge is paused; the
        // chat lock is what keeps it out.
        std::thread::sleep(Duration::from_millis(50));
        let _ = release_tx.send(());
        marker.join().unwrap();
        assert!(written_rx.recv_timeout(Duration::from_secs(5)).is_ok());
        writer.join().unwrap().unwrap();

        assert_eq!(
            records.read(&key),
            Some(successor),
            "the next run's record wins, with its own fields"
        );
    }

    /// Whole-branch review (round 3): `mark_settled` discharges an obligation
    /// only on **proof**. An absent record under a real directory is proof; a
    /// record that exists but will not parse, or a record directory that is no
    /// longer a directory, is not — it may still say `running`, which is exactly
    /// what the next lost sweep would act on.
    #[test]
    fn mark_settled_distinguishes_absence_from_an_unreadable_record() {
        let dir = tempfile::tempdir().unwrap();
        let records = store(&dir);
        let key = key("oc_chat", "oc_chat");

        assert!(
            records.mark_settled(&key, "run-1", None),
            "no record at all is nothing to protect"
        );

        std::fs::write(records.path(&key), b"{ not json").unwrap();
        assert!(
            !records.mark_settled(&key, "run-1", None),
            "an unreadable record may still be running"
        );

        std::fs::remove_file(records.path(&key)).unwrap();
        let records_dir = records.dir().to_path_buf();
        let backup = records_dir.with_file_name("terminal-runs.bak");
        std::fs::rename(&records_dir, &backup).unwrap();
        std::fs::write(&records_dir, b"not a directory").unwrap();
        assert!(
            !records.mark_settled(&key, "run-1", None),
            "a store that is not a directory cannot prove absence"
        );

        std::fs::remove_file(&records_dir).unwrap();
        std::fs::rename(&backup, &records_dir).unwrap();
        records.write(&record(&key, "run-1")).unwrap();
        assert!(
            records.mark_settled(&key, "run-1", None),
            "a running record is merged"
        );
        assert_eq!(
            records.read(&key).unwrap().state,
            TerminalRunState::Settled,
            "the merge is the targeted state change"
        );
    }
}
