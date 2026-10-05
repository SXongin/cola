//! The Chain Record (ADR-0069): one module owns the durable per-Session facts
//! about a Card Chain, in one sidecar (`chain_records.json`).
//!
//! Two sections with two lifetimes share the file:
//!
//! - **records** — the live card record (ADR-0063): which card a Session is
//!   currently streaming into and the Turn it answers, plus the Session's
//!   directory and the per-process-life reconciliation marks. Written when a
//!   card becomes the Session's live card, re-pointed when the chain
//!   continues, and removed when that card reaches a terminal or is
//!   collected.
//! - **announcements** — the Wake Watermark (ADR-0061): the newest Wake whose
//!   completion a card announced, advanced only after the carrying card write
//!   lands. Monotonic per Session and never removed, so a restart cannot
//!   re-announce a Wake a previous life already showed.
//!
//! The load fails open (a missing or corrupt file reads as empty) and the
//! write is best-effort and atomic (the shared [`sidecar`] mechanics). The
//! one deviation from the sidecar convention: the file is **kept even when
//! both sections are empty**, because its presence is the one-time migration
//! marker from the pre-ADR-0069 sidecars (`live_cards.json`,
//! `wake_watermarks.json`). When it is absent, [`ChainRecords::load`] folds
//! those legacy files into the two sections once and they are never written
//! again; removing an emptied file would fall back to them and resurrect
//! state that was deliberately removed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::backend::{MessageId, TurnAnchor};
use crate::bridge::sidecar;

/// The keep rule the fresh-Turn takeover's collect applied to the card it
/// replaced (ADR-0068), in memory only: which predecessor card the collect
/// targeted and whether the restart carry moved its running `⏳` panels onto
/// the successor. The #443 restart stamp's post-PATCH repair reads it so a
/// stamp landing over that takeover reproduces the collect's strip instead of
/// restoring the tail it removed; every other takeover (the Wake and external
/// arms) records none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PredecessorKeep {
    pub(crate) card_message_id: String,
    pub(crate) strip_running_panels: bool,
}

/// One Session's durable live-card facts. `card_message_id` is the Feishu
/// message the reap PATCHes; `message_id` is the Turn's own message (the
/// `msg_cola_` user message a cola Turn submitted, or the external message an
/// adopted run answers) and `created_ms` its server time, once a read carried
/// it — the two together are the [Turn anchor](TurnAnchor) the settle decision
/// is scoped with. `created_ms` stays `None` while the submitted message has
/// never been observed: the reap then probes the transcript with `message_id`,
/// and a read that still carries no such user message ends the card Unreceived
/// (ADR-0062), never ✅.
///
/// `directory` is the Session's working directory at track time: on a
/// generation whose reads route per directory (V1), the reap must ask the
/// instance the card belongs to, never the process's cwd — and an unmapped
/// Session has no mapping to fall back to (finding #438).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChainRecord {
    pub(crate) card_message_id: String,
    pub(crate) message_id: MessageId,
    #[serde(default)]
    pub(crate) created_ms: Option<i64>,
    #[serde(default)]
    pub(crate) directory: Option<String>,
    /// In-memory only: the reap already PATCHed this card into its waiting
    /// ending. A record that is rewritten (a new card becomes live) starts
    /// unmarked; the flag only suppresses PATCHing the same waiting card on
    /// every Session Sync tick, and never survives a restart.
    #[serde(skip)]
    pub(crate) waiting_reaped: bool,
    /// In-memory only: the reap already stamped this card's still-live
    /// restart orphan (#443) with the restart status. A record that is
    /// rewritten (a new card becomes live) starts unmarked; the flag only
    /// suppresses re-stamping the same orphan on every Session Sync tick, and
    /// never survives a restart — a fresh life stamps once again, which is
    /// idempotent (the stamp changes only the header).
    #[serde(skip)]
    pub(crate) restarted_reaped: bool,
    /// In-memory only: a restart-stamp attempt for this card is in flight
    /// (#443) — its view read or PATCH is running in a detached task. The
    /// claim keeps the reap from starting a second attempt, and from deciding
    /// anything (a settle, a successor collect) that the never-cancelled
    /// write could still land after; it is released when the attempt resolves
    /// and never survives a restart.
    #[serde(skip)]
    pub(crate) restart_stamping: bool,
    /// In-memory only: the keep rule the fresh-Turn takeover's collect applied
    /// to the card this record replaced (ADR-0068). The #443 restart stamp's
    /// post-PATCH repair reads it so a stamp that lands over that takeover
    /// reproduces the collect's strip — never restoring the tail the collect
    /// removed. Every other takeover leaves it `None`, and the repair then
    /// keeps today's body. A record that is rewritten (a later handover)
    /// starts `None`; it never persists.
    #[serde(skip)]
    pub(crate) predecessor_keep: Option<PredecessorKeep>,
}

impl ChainRecord {
    pub(crate) fn new(
        card_message_id: impl Into<String>,
        message_id: MessageId,
        created_ms: Option<i64>,
    ) -> Self {
        Self {
            card_message_id: card_message_id.into(),
            message_id,
            created_ms,
            directory: None,
            waiting_reaped: false,
            restarted_reaped: false,
            restart_stamping: false,
            predecessor_keep: None,
        }
    }

    /// Carry the Session's directory so the reap's reads route to the right
    /// instance. An empty directory is unknown, exactly like `None`.
    pub(crate) fn with_directory(mut self, directory: Option<String>) -> Self {
        self.directory = directory.filter(|directory| !directory.is_empty());
        self
    }

    /// The Turn anchor this record scopes a settle decision with, when the
    /// message's server time was captured. `None` means the submitted message
    /// was never observed in a transcript — the anchorless Unreceived scope.
    /// The message id travels with its time as one fact, like every anchor.
    pub(crate) fn anchor(&self) -> Option<TurnAnchor> {
        Some(TurnAnchor {
            message_id: self.message_id.clone(),
            created_ms: self.created_ms?,
        })
    }
}

/// One session's newest announced Wake: its identity and server time travel
/// together, the same one-fact anchor rule as [`TurnAnchor`].
///
/// [`TurnAnchor`]: crate::backend::TurnAnchor
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WakeMark {
    pub(crate) wake_id: String,
    pub(crate) created_ms: i64,
}

/// The persisted Chain Record; a wrapper so the on-disk shape can grow.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ChainRecordFile {
    #[serde(default)]
    records: HashMap<String, ChainRecord>,
    #[serde(default)]
    announcements: HashMap<String, WakeMark>,
}

/// The pre-ADR-0069 shape, read once when `chain_records.json` is absent and
/// never written again. The explicit `bound` keeps serde's `#[serde(default)]`
/// inference from demanding `T: Default` (only the map is defaulted).
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct LegacySessions<T> {
    #[serde(default)]
    sessions: HashMap<String, T>,
}

impl<T> Default for LegacySessions<T> {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }
}

/// The in-memory mirror of the Chain Record, under one lock: every mutation
/// happens and persists under it, so the two sections can never be observed
/// half-written against each other.
struct State {
    records: HashMap<String, ChainRecord>,
    announcements: HashMap<String, WakeMark>,
}

/// The write-through mirror of the Chain Record. Locked independently of the
/// cards and the session store: every reader (the card paths that track a
/// live card, the reap, the Fresh gate) reaches it through `CardsHandle`, and
/// no lock ordering against `cards.cards` matters because every write is a
/// plain synchronous file write.
pub(crate) struct ChainRecords {
    path: PathBuf,
    state: Mutex<State>,
}

impl ChainRecords {
    /// Load the Chain Record — from `chain_records.json` when it exists, else
    /// by folding the legacy `live_cards.json` / `wake_watermarks.json` beside
    /// it (ADR-0069's one-time migration). A missing or corrupt file is an
    /// empty record, never an error: a lost record only means a restart reaps
    /// nothing and a Fresh path may re-announce once.
    pub(crate) fn load(path: PathBuf) -> Self {
        let folded = !path.exists();
        let state = if folded {
            let records = sidecar::load::<LegacySessions<ChainRecord>>(
                &path.with_file_name("live_cards.json"),
                "live-card record",
            )
            .sessions;
            let announcements = sidecar::load::<LegacySessions<WakeMark>>(
                &path.with_file_name("wake_watermarks.json"),
                "Wake watermark",
            )
            .sessions;
            State {
                records,
                announcements,
            }
        } else {
            let file = sidecar::load::<ChainRecordFile>(&path, "Chain Record");
            State {
                records: file.records,
                announcements: file.announcements,
            }
        };
        let store = Self {
            path,
            state: Mutex::new(state),
        };
        if folded {
            // Materialize the marker now: the folded state is what the legacy
            // files held, and the first load is what settles the migration, not
            // the first later mutation. The subsequent loads never look back.
            let state = store.lock();
            store.write(&state);
        }
        store
    }

    /// The session's record, when its card is (believed to be) live. The test
    /// seam: production reads the whole record through [`Self::entries`] (the
    /// reap's pass) or the previous value [`Self::replace`] returns.
    #[cfg(test)]
    pub(crate) fn get(&self, session_id: &str) -> Option<ChainRecord> {
        self.lock().records.get(session_id).cloned()
    }

    /// Every record, as one snapshot — Session Sync's reap iterates this so a
    /// record whose session is no longer mapped is still reconciled.
    pub(crate) fn entries(&self) -> Vec<(String, ChainRecord)> {
        self.lock()
            .records
            .iter()
            .map(|(session_id, card)| (session_id.clone(), card.clone()))
            .collect()
    }

    /// Make `card` the session's record, returning the previous one — the
    /// one-shot write every "a card became the session's live card" path uses.
    /// Best-effort: a failed write leaves the in-memory record correct (the
    /// next write retries the file), which at worst means this card is not
    /// reaped by the next restart.
    pub(crate) fn replace(&self, session_id: &str, card: ChainRecord) -> Option<ChainRecord> {
        let mut state = self.lock();
        let previous = state.records.insert(session_id.to_string(), card);
        self.write(&state);
        previous
    }

    /// Drop the session's record — the card reached a terminal or was
    /// collected, so nothing is owed a reap. Its announcement, if any, stays:
    /// the watermark outlives the record it was announced on.
    pub(crate) fn remove(&self, session_id: &str) {
        let mut state = self.lock();
        if state.records.remove(session_id).is_some() {
            self.write(&state);
        }
    }

    /// Re-key a record when the session it names is recreated under a fresh id
    /// (the 404 recreate): the card is still live, only its session id moved.
    /// The Session's announcement moves with it — the recreated id names the
    /// same logical Session, so a Wake the old id announced must not be
    /// re-announced under the new one.
    pub(crate) fn rename(&self, from: &str, to: &str) {
        let mut state = self.lock();
        let record = state.records.remove(from);
        let announcement = state.announcements.remove(from);
        if record.is_none() && announcement.is_none() {
            return;
        }
        if let Some(record) = record {
            state.records.insert(to.to_string(), record);
        }
        if let Some(announcement) = announcement {
            state.announcements.insert(to.to_string(), announcement);
        }
        self.write(&state);
    }

    /// Refresh the anchor of the record naming `card_message_id` — the render
    /// path's write once a read carries the submitted message's server time.
    /// A no-op when the record names another card (the chain moved on, a
    /// successor owns the session) or the anchor is already current, so the
    /// frequent render polls do not rewrite the file.
    pub(crate) fn set_anchor(&self, session_id: &str, card_message_id: &str, anchor: &TurnAnchor) {
        let mut state = self.lock();
        let Some(card) = state.records.get_mut(session_id) else {
            return;
        };
        if card.card_message_id != card_message_id
            || (card.message_id == anchor.message_id && card.created_ms == Some(anchor.created_ms))
        {
            return;
        }
        card.message_id = anchor.message_id.clone();
        card.created_ms = Some(anchor.created_ms);
        self.write(&state);
    }

    /// Mark that `card_message_id`'s waiting ending was already PATCHed, so
    /// the reap does not repeat it every tick. In-memory only: a restart
    /// re-PATCHes once, which is idempotent. Returns whether the record still
    /// names that card (a stale mark for a replaced card is dropped).
    pub(crate) fn mark_waiting_reaped(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_reap_flag(session_id, card_message_id, |card| card.waiting_reaped = true)
    }

    /// Mark that `card_message_id`'s restart stamp was already PATCHed, so the
    /// reap stamps a still-live restart orphan once per process life (#443).
    /// In-memory only: a restart re-stamps once, which is idempotent. Returns
    /// whether the record still names that card (a stale mark for a replaced
    /// card is dropped).
    pub(crate) fn mark_restarted_reaped(&self, session_id: &str, card_message_id: &str) -> bool {
        self.set_reap_flag(session_id, card_message_id, |card| card.restarted_reaped = true)
    }

    /// Record the keep rule the fresh-Turn takeover's collect is about to
    /// apply to `card_message_id` (ADR-0068), so the #443 stamp's post-PATCH
    /// repair can reproduce it. Recorded before the collect's PATCH so a stamp
    /// that lands after it finds the rule. In-memory only; a later handover
    /// that rewrites the record drops it.
    pub(crate) fn note_predecessor_keep(
        &self,
        session_id: &str,
        card_message_id: &str,
        strip_running_panels: bool,
    ) {
        let mut state = self.lock();
        if let Some(card) = state.records.get_mut(session_id) {
            card.predecessor_keep = Some(PredecessorKeep {
                card_message_id: card_message_id.to_string(),
                strip_running_panels,
            });
        }
    }

    /// The live-tail strip the takeover collect recorded for the predecessor
    /// card `card_message_id`, when this record still remembers it:
    /// `Some(strip_running_panels)` after a fresh-Turn takeover's collect,
    /// `None` for every other takeover (the #443 repair then keeps today's
    /// body) and for a record that moved on.
    pub(crate) fn predecessor_keep_strip(&self, session_id: &str, card_message_id: &str) -> Option<bool> {
        self.lock().records.get(session_id).and_then(|card| {
            card.predecessor_keep
                .as_ref()
                .filter(|keep| keep.card_message_id == card_message_id)
                .map(|keep| keep.strip_running_panels)
        })
    }

    /// Set `flag` on the record naming `card_message_id` — the shared body of
    /// the record-scoped flag mutations (the reap's one-per-life marks and the
    /// restart-stamp release). Returns whether the record still names that
    /// card (a stale mutation for a replaced card is dropped).
    fn set_reap_flag(
        &self,
        session_id: &str,
        card_message_id: &str,
        flag: impl FnOnce(&mut ChainRecord),
    ) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                flag(card);
                true
            }
            _ => false,
        }
    }

    /// Claim the one in-flight restart-stamp attempt for `card_message_id`
    /// (#443): true when the caller now owns the attempt, false when the
    /// record moved on, the stamp already landed, or another attempt holds
    /// the claim. In-memory only.
    pub(crate) fn begin_restart_stamp(&self, session_id: &str, card_message_id: &str) -> bool {
        let mut state = self.lock();
        match state.records.get_mut(session_id) {
            Some(card)
                if card.card_message_id == card_message_id
                    && !card.restarted_reaped
                    && !card.restart_stamping =>
            {
                card.restart_stamping = true;
                true
            }
            _ => false,
        }
    }

    /// Release `card_message_id`'s in-flight restart-stamp claim once its
    /// attempt resolved — landed (the record carries the mark) or failed, so
    /// the next pass may retry. A record that no longer names the card is
    /// left untouched.
    pub(crate) fn finish_restart_stamp(&self, session_id: &str, card_message_id: &str) {
        self.set_reap_flag(session_id, card_message_id, |card| {
            card.restart_stamping = false;
        });
    }

    /// The session's mark, when one exists — the Fresh path's gate: a Wake at
    /// or below it was already announced.
    pub(crate) fn announced(&self, session_id: &str) -> Option<WakeMark> {
        self.lock().announcements.get(session_id).cloned()
    }

    /// Advance the session's mark to `(wake_id, created_ms)` and persist, when
    /// strictly newer than the current one. Best-effort: a failed write leaves
    /// the in-memory mark advanced (the next advance retries the file), which
    /// suppresses at most the announcement whose write raced the crash — the
    /// same duplicate-over-loss trade the drain itself makes.
    pub(crate) fn advance(&self, session_id: &str, wake_id: &str, created_ms: i64) {
        let mut state = self.lock();
        if state
            .announcements
            .get(session_id)
            .is_some_and(|mark| mark.created_ms >= created_ms)
        {
            return;
        }
        state.announcements.insert(
            session_id.to_string(),
            WakeMark {
                wake_id: wake_id.to_string(),
                created_ms,
            },
        );
        self.write(&state);
    }

    /// Persist both sections through the shared sidecar writer. The file is
    /// kept even when both sections are empty: its presence is the one-time
    /// migration marker that stops [`Self::load`] folding the legacy files
    /// again (ADR-0069).
    fn write(&self, state: &State) {
        sidecar::store(
            &self.path,
            "Chain Record",
            &ChainRecordFile {
                records: state.records.clone(),
                announcements: state.announcements.clone(),
            },
            false,
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "chain_records.json";

    #[test]
    fn replace_persists_and_reloads_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        assert_eq!(
            chains.replace(
                "ses_a",
                ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000))
            ),
            None
        );
        chains.replace(
            "ses_b",
            ChainRecord::new("om_card_2", MessageId::new("msg_cola_2"), None),
        );

        let reloaded = ChainRecords::load(path.clone());
        assert_eq!(
            reloaded.get("ses_a"),
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
        assert_eq!(
            reloaded.get("ses_b"),
            Some(ChainRecord::new("om_card_2", MessageId::new("msg_cola_2"), None))
        );
        assert_eq!(reloaded.get("ses_c"), None);

        // Replacing returns the previous record and keeps one per session.
        let previous = chains.replace(
            "ses_a",
            ChainRecord::new("om_card_3", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        assert_eq!(
            previous,
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
        assert_eq!(
            ChainRecords::load(path.clone()).get("ses_a"),
            Some(ChainRecord::new(
                "om_card_3",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );

        // Removing the last record keeps the file (the migration marker) but
        // drops the records themselves.
        chains.remove("ses_a");
        chains.remove("ses_b");
        assert!(path.exists(), "the Chain Record file is kept when empty");
        assert_eq!(chains.entries().len(), 0);
        assert_eq!(ChainRecords::load(path).entries().len(), 0);
    }

    #[test]
    fn rename_rekeys_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.replace(
            "ses_old",
            ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000)),
        );

        chains.rename("ses_old", "ses_fresh");
        assert_eq!(chains.get("ses_old"), None);
        assert_eq!(
            ChainRecords::load(path).get("ses_fresh"),
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
    }

    /// A 404 recreate names the same logical Session: its announcement moves
    /// with the record, so the fresh id cannot re-announce a Wake the old id
    /// already showed.
    #[test]
    fn rename_rekeys_the_announcement_with_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.replace(
            "ses_old",
            ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        chains.advance("ses_old", "msg_wake_1", 1_000);

        chains.rename("ses_old", "ses_fresh");
        let reloaded = ChainRecords::load(path);
        assert_eq!(
            reloaded.announced("ses_fresh"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            })
        );
        assert_eq!(reloaded.announced("ses_old"), None);
    }

    #[test]
    fn a_corrupt_or_missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        assert_eq!(ChainRecords::load(path.clone()).get("ses_a"), None);

        std::fs::write(&path, "not json").unwrap();
        let chains = ChainRecords::load(path.clone());
        assert_eq!(chains.get("ses_a"), None, "a corrupt file reads as empty");
        // The next write replaces the corrupt file with a valid one.
        chains.replace(
            "ses_a",
            ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        assert_eq!(
            ChainRecords::load(path).get("ses_a"),
            Some(ChainRecord::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
    }

    /// The waiting mark is in-memory only: it survives the record's own life
    /// but never persists, and it only ever marks the card the record names.
    #[test]
    fn the_waiting_mark_is_in_memory_and_card_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.replace(
            "ses_a",
            ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000)),
        );

        assert!(!chains.mark_waiting_reaped("ses_a", "om_other"));
        assert!(!chains.get("ses_a").unwrap().waiting_reaped);
        assert!(chains.mark_waiting_reaped("ses_a", "om_card_1"));
        assert!(chains.get("ses_a").unwrap().waiting_reaped);

        // A reload (a restart) forgets it; a rewrite (a new live card) resets it.
        assert!(
            !ChainRecords::load(path.clone())
                .get("ses_a")
                .unwrap()
                .waiting_reaped
        );
        chains.replace(
            "ses_a",
            ChainRecord::new("om_card_2", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        assert!(!chains.get("ses_a").unwrap().waiting_reaped);
    }

    /// ADR-0068's takeover keep rule is in-memory only and scoped to the
    /// predecessor card it was recorded for: the #443 stamp's repair asks for
    /// the orphan's id and gets `None` for any other card, a restart forgets
    /// it, and a rewrite (a later handover) resets it.
    #[test]
    fn the_takeover_keep_rule_is_in_memory_and_card_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.replace(
            "ses_a",
            ChainRecord::new("om_successor", MessageId::new("msg_cola_1"), Some(1_000)),
        );

        assert_eq!(chains.predecessor_keep_strip("ses_a", "om_old"), None);
        chains.note_predecessor_keep("ses_a", "om_old", true);
        assert_eq!(chains.predecessor_keep_strip("ses_a", "om_old"), Some(true));
        assert_eq!(
            chains.predecessor_keep_strip("ses_a", "om_other"),
            None,
            "the rule only answers for the card it was recorded for"
        );
        chains.note_predecessor_keep("ses_a", "om_old", false);
        assert_eq!(
            chains.predecessor_keep_strip("ses_a", "om_old"),
            Some(false),
            "a second takeover collect replaces the recorded rule"
        );

        // A reload (a restart) forgets it; a rewrite (a later handover) resets it.
        assert_eq!(
            ChainRecords::load(path.clone()).predecessor_keep_strip("ses_a", "om_old"),
            None
        );
        chains.replace(
            "ses_a",
            ChainRecord::new("om_next", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        assert_eq!(chains.predecessor_keep_strip("ses_a", "om_old"), None);
    }

    /// The stored directory round-trips (the reap's V1 route); an empty string
    /// is unknown, exactly like `None`.
    #[test]
    fn the_directory_round_trips_and_an_empty_one_reads_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.replace(
            "ses_a",
            ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000))
                .with_directory(Some("/work".into())),
        );
        chains.replace(
            "ses_b",
            ChainRecord::new("om_card_2", MessageId::new("msg_cola_2"), None)
                .with_directory(Some(String::new())),
        );

        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a").unwrap().directory.as_deref(), Some("/work"));
        assert_eq!(reloaded.get("ses_b").unwrap().directory, None);
    }

    #[test]
    fn the_anchor_travels_only_once_its_server_time_is_known() {
        let anchorless = ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), None);
        assert_eq!(anchorless.anchor(), None);
        let anchored = ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000));
        assert_eq!(
            anchored.anchor(),
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 1_000,
            })
        );
    }

    #[test]
    fn advance_persists_and_reloads_the_newest_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());

        chains.advance("ses_a", "msg_wake_1", 1_000);
        // Older or equal sends do not move the mark.
        chains.advance("ses_a", "msg_wake_0", 900);
        chains.advance("ses_a", "msg_wake_1b", 1_000);
        chains.advance("ses_b", "msg_wake_2", 2_000);

        let reloaded = ChainRecords::load(path);
        assert_eq!(
            reloaded.announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            })
        );
        assert_eq!(
            reloaded.announced("ses_b"),
            Some(WakeMark {
                wake_id: "msg_wake_2".into(),
                created_ms: 2_000,
            })
        );
        assert_eq!(reloaded.announced("ses_c"), None);
    }

    /// The two sections have independent lifetimes: removing a Session's
    /// record leaves its announcement in place.
    #[test]
    fn the_announcements_outlive_the_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.replace(
            "ses_a",
            ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        chains.advance("ses_a", "msg_wake_1", 1_000);

        chains.remove("ses_a");
        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a"), None);
        assert_eq!(
            reloaded.announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            }),
            "the announcement is not removed with the record"
        );
    }

    /// ADR-0069's migration: without `chain_records.json`, the legacy sidecars
    /// are folded in once; afterwards they are never read again, so a record
    /// removed in the new store cannot be resurrected from the stale files.
    #[test]
    fn folds_the_legacy_sidecars_once_and_never_refolds_a_cleared_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(
            dir.path().join("live_cards.json"),
            r#"{"sessions":{"ses_a":{"card_message_id":"om_card_1","message_id":"msg_cola_1","created_ms":1000,"directory":"/work"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("wake_watermarks.json"),
            r#"{"sessions":{"ses_a":{"wake_id":"msg_wake_1","created_ms":1000}}}"#,
        )
        .unwrap();

        let chains = ChainRecords::load(path.clone());
        assert!(
            path.exists(),
            "the fold materializes the Chain Record file immediately"
        );
        assert_eq!(
            chains.get("ses_a"),
            Some(
                ChainRecord::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000))
                    .with_directory(Some("/work".into()))
            ),
            "the legacy record is folded in"
        );
        assert_eq!(
            chains.announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            }),
            "the legacy watermark is folded in"
        );

        // Clearing the folded record persists the new file; the stale legacy
        // record must not come back on the next load.
        chains.remove("ses_a");
        assert!(path.exists());
        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a"), None);
    }

    /// The fold settles the migration at the first load: the marker file is
    /// materialized then, so the legacy files can vanish before any later
    /// mutation without losing the folded facts.
    #[test]
    fn the_fold_materializes_the_marker_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let legacy = dir.path().join("live_cards.json");
        std::fs::write(
            &legacy,
            r#"{"sessions":{"ses_a":{"card_message_id":"om_card_1","message_id":"msg_cola_1"}}}"#,
        )
        .unwrap();

        let chains = ChainRecords::load(path.clone());
        assert!(path.exists(), "the first load writes the merged file");
        assert_eq!(chains.get("ses_a").unwrap().card_message_id, "om_card_1");

        // The legacy file is no longer needed; a restart reads only the new
        // file.
        std::fs::remove_file(&legacy).unwrap();
        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_a").unwrap().card_message_id, "om_card_1");
    }

    /// Once `chain_records.json` exists, it wins: the legacy files beside it
    /// are never folded again.
    #[test]
    fn the_chain_file_wins_over_the_legacy_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let chains = ChainRecords::load(path.clone());
        chains.replace(
            "ses_new",
            ChainRecord::new("om_new", MessageId::new("msg_cola_new"), None),
        );
        std::fs::write(
            dir.path().join("live_cards.json"),
            r#"{"sessions":{"ses_old":{"card_message_id":"om_old","message_id":"msg_cola_old"}}}"#,
        )
        .unwrap();

        let reloaded = ChainRecords::load(path);
        assert_eq!(reloaded.get("ses_new").unwrap().card_message_id, "om_new");
        assert_eq!(reloaded.get("ses_old"), None);
    }
}
