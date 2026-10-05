//! The durable Live Card record (ADR-0063): which card a Session is currently
//! streaming into, and the Turn it answers, so a restart can reap the card it
//! orphaned instead of leaving it frozen forever.
//!
//! Persisted beside the session mapping (`live_cards.json`) through the
//! shared best-effort sidecar I/O ([`crate::bridge::sidecar`]): a missing or
//! corrupt file is an empty record (fail open — the pre-#438 behavior freezes
//! at most the card the crash orphaned, then self-heals), writes are atomic
//! temp-file + rename, and an empty record removes the file.
//!
//! At most one record per Session: it is written when a card becomes the
//! Session's live card, re-pointed when the same chain continues on a new card
//! or a successor takes over, and removed when the card reaches a terminal or
//! is collected (ADR-0063). The record holds no chat content — the card's
//! message id, the Turn's own message id and anchor, and the directory its
//! reads route under — so an orphan can be settled from the transcript without
//! storing anything the transcript does not already have.

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
pub(crate) struct LiveCard {
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

impl LiveCard {
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

/// The persisted record; a wrapper so the on-disk shape can grow.
#[derive(Debug, Default, Serialize, Deserialize)]
struct LiveCardFile {
    #[serde(default)]
    sessions: HashMap<String, LiveCard>,
}

/// The write-through mirror of the Live Card record. Locked independently of
/// the cards and the session store: every reader (the card paths that track a
/// live card, Session Sync's reap) reaches it through `CardsHandle`, and no
/// lock ordering against `cards.cards` matters because every write is a plain
/// synchronous file write.
pub(crate) struct LiveCards {
    path: PathBuf,
    sessions: Mutex<HashMap<String, LiveCard>>,
}

impl LiveCards {
    /// Load the persisted record, or an empty one when the file is missing or
    /// unreadable. A corrupt file is logged and replaced on the next write —
    /// never an error: a lost record only means a restart reaps nothing.
    pub(crate) fn load(path: PathBuf) -> Self {
        let sessions = sidecar::load::<LiveCardFile>(&path, "live-card record").sessions;
        Self {
            path,
            sessions: Mutex::new(sessions),
        }
    }

    /// The session's record, when its card is (believed to be) live. The test
    /// seam: production reads the whole record through [`Self::entries`] (the
    /// reap's pass) or the previous value [`Self::replace`] returns.
    #[cfg(test)]
    pub(crate) fn get(&self, session_id: &str) -> Option<LiveCard> {
        self.lock().get(session_id).cloned()
    }

    /// Every record, as one snapshot — Session Sync's reap iterates this so a
    /// record whose session is no longer mapped is still reconciled.
    pub(crate) fn entries(&self) -> Vec<(String, LiveCard)> {
        self.lock()
            .iter()
            .map(|(session_id, card)| (session_id.clone(), card.clone()))
            .collect()
    }

    /// Make `card` the session's record, returning the previous one — the
    /// one-shot write every "a card became the session's live card" path uses.
    /// Best-effort: a failed write leaves the in-memory record correct (the
    /// next write retries the file), which at worst means this card is not
    /// reaped by the next restart.
    pub(crate) fn replace(&self, session_id: &str, card: LiveCard) -> Option<LiveCard> {
        let mut sessions = self.lock();
        let previous = sessions.insert(session_id.to_string(), card);
        write(&self.path, &sessions);
        previous
    }

    /// Drop the session's record — the card reached a terminal or was
    /// collected, so nothing is owed a reap.
    pub(crate) fn remove(&self, session_id: &str) {
        let mut sessions = self.lock();
        if sessions.remove(session_id).is_some() {
            write(&self.path, &sessions);
        }
    }

    /// Re-key a record when the session it names is recreated under a fresh id
    /// (the 404 recreate): the card is still live, only its session id moved.
    pub(crate) fn rename(&self, from: &str, to: &str) {
        let mut sessions = self.lock();
        let Some(card) = sessions.remove(from) else {
            return;
        };
        sessions.insert(to.to_string(), card);
        write(&self.path, &sessions);
    }

    /// Refresh the anchor of the record naming `card_message_id` — the render
    /// path's write once a read carries the submitted message's server time.
    /// A no-op when the record names another card (the chain moved on, a
    /// successor owns the session) or the anchor is already current, so the
    /// frequent render polls do not rewrite the file.
    pub(crate) fn set_anchor(&self, session_id: &str, card_message_id: &str, anchor: &TurnAnchor) {
        let mut sessions = self.lock();
        let Some(card) = sessions.get_mut(session_id) else {
            return;
        };
        if card.card_message_id != card_message_id
            || (card.message_id == anchor.message_id && card.created_ms == Some(anchor.created_ms))
        {
            return;
        }
        card.message_id = anchor.message_id.clone();
        card.created_ms = Some(anchor.created_ms);
        write(&self.path, &sessions);
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
        let mut sessions = self.lock();
        if let Some(card) = sessions.get_mut(session_id) {
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
        self.lock().get(session_id).and_then(|card| {
            card.predecessor_keep
                .as_ref()
                .filter(|keep| keep.card_message_id == card_message_id)
                .map(|keep| keep.strip_running_panels)
        })
    }

    /// Set `flag` on the record naming `card_message_id` — the shared body of
    /// the reap's one-per-life marks. Returns whether the record still names
    /// that card (a stale mark for a replaced card is dropped).
    fn set_reap_flag(
        &self,
        session_id: &str,
        card_message_id: &str,
        flag: impl FnOnce(&mut LiveCard),
    ) -> bool {
        let mut sessions = self.lock();
        match sessions.get_mut(session_id) {
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
        let mut sessions = self.lock();
        match sessions.get_mut(session_id) {
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
        let mut sessions = self.lock();
        if let Some(card) = sessions.get_mut(session_id)
            && card.card_message_id == card_message_id
        {
            card.restart_stamping = false;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveCard>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Persist the record through the shared sidecar writer; an empty record
/// removes the file.
fn write(path: &std::path::Path, sessions: &HashMap<String, LiveCard>) {
    sidecar::store(
        path,
        "live-card record",
        &LiveCardFile {
            sessions: sessions.clone(),
        },
        sessions.is_empty(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_persists_and_reloads_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live_cards.json");
        let cards = LiveCards::load(path.clone());

        assert_eq!(
            cards.replace(
                "ses_a",
                LiveCard::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000))
            ),
            None
        );
        cards.replace(
            "ses_b",
            LiveCard::new("om_card_2", MessageId::new("msg_cola_2"), None),
        );

        let reloaded = LiveCards::load(path.clone());
        assert_eq!(
            reloaded.get("ses_a"),
            Some(LiveCard::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
        assert_eq!(
            reloaded.get("ses_b"),
            Some(LiveCard::new("om_card_2", MessageId::new("msg_cola_2"), None))
        );
        assert_eq!(reloaded.get("ses_c"), None);

        // Replacing returns the previous record and keeps one per session.
        let previous = cards.replace(
            "ses_a",
            LiveCard::new("om_card_3", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        assert_eq!(
            previous,
            Some(LiveCard::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
        assert_eq!(
            LiveCards::load(path.clone()).get("ses_a"),
            Some(LiveCard::new(
                "om_card_3",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );

        // Removing the last record removes the file entirely.
        cards.remove("ses_a");
        cards.remove("ses_b");
        assert!(!path.exists(), "an empty record removes the sidecar");
        assert_eq!(cards.entries().len(), 0);
    }

    #[test]
    fn rename_rekeys_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live_cards.json");
        let cards = LiveCards::load(path.clone());
        cards.replace(
            "ses_old",
            LiveCard::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000)),
        );

        cards.rename("ses_old", "ses_fresh");
        assert_eq!(cards.get("ses_old"), None);
        assert_eq!(
            LiveCards::load(path).get("ses_fresh"),
            Some(LiveCard::new(
                "om_card_1",
                MessageId::new("msg_cola_1"),
                Some(1_000)
            ))
        );
    }

    #[test]
    fn a_corrupt_or_missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live_cards.json");
        assert_eq!(LiveCards::load(path.clone()).get("ses_a"), None);

        std::fs::write(&path, "not json").unwrap();
        let cards = LiveCards::load(path.clone());
        assert_eq!(cards.get("ses_a"), None, "a corrupt file reads as empty");
        // The next write replaces the corrupt file with a valid one.
        cards.replace(
            "ses_a",
            LiveCard::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        assert_eq!(
            LiveCards::load(path).get("ses_a"),
            Some(LiveCard::new(
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
        let path = dir.path().join("live_cards.json");
        let cards = LiveCards::load(path.clone());
        cards.replace(
            "ses_a",
            LiveCard::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000)),
        );

        assert!(!cards.mark_waiting_reaped("ses_a", "om_other"));
        assert!(!cards.get("ses_a").unwrap().waiting_reaped);
        assert!(cards.mark_waiting_reaped("ses_a", "om_card_1"));
        assert!(cards.get("ses_a").unwrap().waiting_reaped);

        // A reload (a restart) forgets it; a rewrite (a new live card) resets it.
        assert!(!LiveCards::load(path.clone()).get("ses_a").unwrap().waiting_reaped);
        cards.replace(
            "ses_a",
            LiveCard::new("om_card_2", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        assert!(!cards.get("ses_a").unwrap().waiting_reaped);
    }

    /// ADR-0068's takeover keep rule is in-memory only and scoped to the
    /// predecessor card it was recorded for: the #443 stamp's repair asks for
    /// the orphan's id and gets `None` for any other card, a restart forgets
    /// it, and a rewrite (a later handover) resets it.
    #[test]
    fn the_takeover_keep_rule_is_in_memory_and_card_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live_cards.json");
        let cards = LiveCards::load(path.clone());
        cards.replace(
            "ses_a",
            LiveCard::new("om_successor", MessageId::new("msg_cola_1"), Some(1_000)),
        );

        assert_eq!(cards.predecessor_keep_strip("ses_a", "om_old"), None);
        cards.note_predecessor_keep("ses_a", "om_old", true);
        assert_eq!(cards.predecessor_keep_strip("ses_a", "om_old"), Some(true));
        assert_eq!(
            cards.predecessor_keep_strip("ses_a", "om_other"),
            None,
            "the rule only answers for the card it was recorded for"
        );
        cards.note_predecessor_keep("ses_a", "om_old", false);
        assert_eq!(
            cards.predecessor_keep_strip("ses_a", "om_old"),
            Some(false),
            "a second takeover collect replaces the recorded rule"
        );

        // A reload (a restart) forgets it; a rewrite (a later handover) resets it.
        assert_eq!(
            LiveCards::load(path.clone()).predecessor_keep_strip("ses_a", "om_old"),
            None
        );
        cards.replace(
            "ses_a",
            LiveCard::new("om_next", MessageId::new("msg_cola_1"), Some(1_000)),
        );
        assert_eq!(cards.predecessor_keep_strip("ses_a", "om_old"), None);
    }

    /// The stored directory round-trips (the reap's V1 route); an empty string
    /// is unknown, exactly like `None`.
    #[test]
    fn the_directory_round_trips_and_an_empty_one_reads_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live_cards.json");
        let cards = LiveCards::load(path.clone());
        cards.replace(
            "ses_a",
            LiveCard::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000))
                .with_directory(Some("/work".into())),
        );
        cards.replace(
            "ses_b",
            LiveCard::new("om_card_2", MessageId::new("msg_cola_2"), None)
                .with_directory(Some(String::new())),
        );

        let reloaded = LiveCards::load(path);
        assert_eq!(reloaded.get("ses_a").unwrap().directory.as_deref(), Some("/work"));
        assert_eq!(reloaded.get("ses_b").unwrap().directory, None);
    }

    #[test]
    fn the_anchor_travels_only_once_its_server_time_is_known() {
        let anchorless = LiveCard::new("om_card_1", MessageId::new("msg_cola_1"), None);
        assert_eq!(anchorless.anchor(), None);
        let anchored = LiveCard::new("om_card_1", MessageId::new("msg_cola_1"), Some(1_000));
        assert_eq!(
            anchored.anchor(),
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 1_000,
            })
        );
    }
}
