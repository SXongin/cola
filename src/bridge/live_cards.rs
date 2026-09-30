//! The durable Live Card record (ADR-0063): which card a Session is currently
//! streaming into, and the Turn it answers, so a restart can reap the card it
//! orphaned instead of leaving it frozen forever.
//!
//! Persisted beside the session mapping (`live_cards.json`) — the
//! wake_watermarks and `interactive_surfaces.json` pattern: a missing or
//! corrupt file is an empty record (fail open — the pre-#438 behavior freezes
//! at most the card the crash orphaned, then self-heals), writes are
//! best-effort temp-file + rename, and an empty record removes the file.
//!
//! At most one record per Session: it is written when a card becomes the
//! Session's live card, re-pointed when the same chain continues on a new card
//! or a successor takes over, and removed when the card reaches a terminal or
//! is collected (ADR-0063). The record holds no chat content — only the two
//! message identities and the anchor's server time — so an orphan can be
//! settled from the transcript without storing anything the transcript does
//! not already have.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::backend::{MessageId, TurnAnchor};

/// One Session's durable live-card facts. `card_message_id` is the Feishu
/// message the reap PATCHes; `message_id` is the Turn's own message (the
/// `msg_cola_` user message a cola Turn submitted, or the external message an
/// adopted run answers) and `created_ms` its server time, once a read carried
/// it — the two together are the [Turn anchor](TurnAnchor) the settle decision
/// is scoped with. `created_ms` stays `None` while the submitted message has
/// never been observed: the reap then probes the transcript with `message_id`,
/// and a read that still carries no such user message ends the card Unreceived
/// (ADR-0062), never ✅.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LiveCard {
    pub(crate) card_message_id: String,
    pub(crate) message_id: String,
    #[serde(default)]
    pub(crate) created_ms: Option<i64>,
    /// In-memory only: the reap already PATCHed this card into its waiting
    /// ending. A record that is rewritten (a new card becomes live) starts
    /// unmarked; the flag only suppresses PATCHing the same waiting card on
    /// every Session Sync tick, and never survives a restart.
    #[serde(skip)]
    pub(crate) waiting_reaped: bool,
}

impl LiveCard {
    pub(crate) fn new(
        card_message_id: impl Into<String>,
        message_id: impl Into<String>,
        created_ms: Option<i64>,
    ) -> Self {
        Self {
            card_message_id: card_message_id.into(),
            message_id: message_id.into(),
            created_ms,
            waiting_reaped: false,
        }
    }

    /// The Turn anchor this record scopes a settle decision with, when the
    /// message's server time was captured. `None` means the submitted message
    /// was never observed in a transcript — the anchorless Unreceived scope.
    /// The message id travels with its time as one fact, like every anchor.
    pub(crate) fn anchor(&self) -> Option<TurnAnchor> {
        Some(TurnAnchor {
            message_id: MessageId::new(&self.message_id),
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
        let sessions = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<LiveCardFile>(&raw) {
                Ok(file) => file.sessions,
                Err(e) => {
                    tracing::warn!(
                        "could not parse {} ({e}); starting with an empty live-card record",
                        path.display()
                    );
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                tracing::warn!(
                    "could not read {} ({e}); starting with an empty live-card record",
                    path.display()
                );
                HashMap::new()
            }
        };
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
        write_file(&self.path, &sessions);
        previous
    }

    /// Drop the session's record — the card reached a terminal or was
    /// collected, so nothing is owed a reap.
    pub(crate) fn remove(&self, session_id: &str) {
        let mut sessions = self.lock();
        if sessions.remove(session_id).is_some() {
            write_file(&self.path, &sessions);
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
        write_file(&self.path, &sessions);
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
            || (card.message_id == anchor.message_id.as_str() && card.created_ms == Some(anchor.created_ms))
        {
            return;
        }
        card.message_id = anchor.message_id.to_string();
        card.created_ms = Some(anchor.created_ms);
        write_file(&self.path, &sessions);
    }

    /// Mark that `card_message_id`'s waiting ending was already PATCHed, so
    /// the reap does not repeat it every tick. In-memory only: a restart
    /// re-PATCHes once, which is idempotent. Returns whether the record still
    /// names that card (a stale mark for a replaced card is dropped).
    pub(crate) fn mark_waiting_reaped(&self, session_id: &str, card_message_id: &str) -> bool {
        let mut sessions = self.lock();
        match sessions.get_mut(session_id) {
            Some(card) if card.card_message_id == card_message_id => {
                card.waiting_reaped = true;
                true
            }
            _ => false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveCard>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Write the record atomically (temp file + rename), best-effort: the record
/// only feeds the next restart's reap, so a failure logs and changes nothing
/// else. An empty record removes the file.
fn write_file(path: &Path, sessions: &HashMap<String, LiveCard>) {
    if sessions.is_empty() {
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("live cards: could not remove {}: {}", path.display(), e);
        }
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let data = match serde_json::to_string(&LiveCardFile {
        sessions: sessions.clone(),
    }) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!("live cards: could not serialize the record: {e}");
            return;
        }
    };
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, data) {
        tracing::warn!("live cards: could not write {}: {}", tmp.display(), e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!("live cards: could not replace {}: {}", path.display(), e);
    }
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
            cards.replace("ses_a", LiveCard::new("om_card_1", "msg_cola_1", Some(1_000))),
            None
        );
        cards.replace("ses_b", LiveCard::new("om_card_2", "msg_cola_2", None));

        let reloaded = LiveCards::load(path.clone());
        assert_eq!(
            reloaded.get("ses_a"),
            Some(LiveCard::new("om_card_1", "msg_cola_1", Some(1_000)))
        );
        assert_eq!(
            reloaded.get("ses_b"),
            Some(LiveCard::new("om_card_2", "msg_cola_2", None))
        );
        assert_eq!(reloaded.get("ses_c"), None);

        // Replacing returns the previous record and keeps one per session.
        let previous = cards.replace("ses_a", LiveCard::new("om_card_3", "msg_cola_1", Some(1_000)));
        assert_eq!(
            previous,
            Some(LiveCard::new("om_card_1", "msg_cola_1", Some(1_000)))
        );
        assert_eq!(
            LiveCards::load(path.clone()).get("ses_a"),
            Some(LiveCard::new("om_card_3", "msg_cola_1", Some(1_000)))
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
        cards.replace("ses_old", LiveCard::new("om_card_1", "msg_cola_1", Some(1_000)));

        cards.rename("ses_old", "ses_fresh");
        assert_eq!(cards.get("ses_old"), None);
        assert_eq!(
            LiveCards::load(path).get("ses_fresh"),
            Some(LiveCard::new("om_card_1", "msg_cola_1", Some(1_000)))
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
        cards.replace("ses_a", LiveCard::new("om_card_1", "msg_cola_1", Some(1_000)));
        assert_eq!(
            LiveCards::load(path).get("ses_a"),
            Some(LiveCard::new("om_card_1", "msg_cola_1", Some(1_000)))
        );
    }

    /// The waiting mark is in-memory only: it survives the record's own life
    /// but never persists, and it only ever marks the card the record names.
    #[test]
    fn the_waiting_mark_is_in_memory_and_card_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live_cards.json");
        let cards = LiveCards::load(path.clone());
        cards.replace("ses_a", LiveCard::new("om_card_1", "msg_cola_1", Some(1_000)));

        assert!(!cards.mark_waiting_reaped("ses_a", "om_other"));
        assert!(!cards.get("ses_a").unwrap().waiting_reaped);
        assert!(cards.mark_waiting_reaped("ses_a", "om_card_1"));
        assert!(cards.get("ses_a").unwrap().waiting_reaped);

        // A reload (a restart) forgets it; a rewrite (a new live card) resets it.
        assert!(!LiveCards::load(path.clone()).get("ses_a").unwrap().waiting_reaped);
        cards.replace("ses_a", LiveCard::new("om_card_2", "msg_cola_1", Some(1_000)));
        assert!(!cards.get("ses_a").unwrap().waiting_reaped);
    }

    #[test]
    fn the_anchor_travels_only_once_its_server_time_is_known() {
        let anchorless = LiveCard::new("om_card_1", "msg_cola_1", None);
        assert_eq!(anchorless.anchor(), None);
        let anchored = LiveCard::new("om_card_1", "msg_cola_1", Some(1_000));
        assert_eq!(
            anchored.anchor(),
            Some(TurnAnchor {
                message_id: MessageId::new("msg_cola_1"),
                created_ms: 1_000,
            })
        );
    }
}
