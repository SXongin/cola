//! The Wake Watermark (ADR-0061): the durable per-session record of the
//! newest Wake whose completion a card has already announced.
//!
//! A cola restart loses live cards, so the accumulator's in-memory
//! `announced_wakes` set — what the card chain has shown — dies with the
//! process, and Session Sync's no-chain Fresh path would re-announce a Wake a
//! previous life already rendered (#424). The watermark is the one card fact
//! that survives: it advances only when the card write that carried the
//! announcement actually lands (a split's continuation send, a PATCH carrying
//! a merged completion entry, the Fresh card's own send), and the Fresh path
//! continues only a Wake strictly newer than it.
//!
//! Persisted beside the session mapping (`wake_watermarks.json`), the
//! `interactive_surfaces.json` pattern: a missing or corrupt file is an empty
//! record (fail open — the pre-#424 behavior posts at most one announcement
//! per session after the upgrade, then self-heals), writes are best-effort
//! temp-file + rename, and an empty record removes the file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::bridge::sidecar;

/// One session's newest announced Wake: its identity and server time travel
/// together, the same one-fact anchor rule as [`TurnAnchor`].
///
/// [`TurnAnchor`]: crate::backend::TurnAnchor
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WakeMark {
    pub(crate) wake_id: String,
    pub(crate) created_ms: i64,
}

/// The persisted record; a wrapper so the on-disk shape can grow.
#[derive(Debug, Default, Serialize, Deserialize)]
struct WakeMarkFile {
    #[serde(default)]
    sessions: HashMap<String, WakeMark>,
}

/// The write-through mirror of the Wake Watermark. Locked independently of
/// the cards and the session store: every reader (the Fresh decision, the
/// flush's drain) reaches it through `CardsHandle`, and no lock ordering
/// against `cards.cards` matters because the write never awaits.
pub(crate) struct WakeWatermarks {
    path: PathBuf,
    sessions: Mutex<HashMap<String, WakeMark>>,
}

impl WakeWatermarks {
    /// Load the persisted record, or an empty one when the file is missing or
    /// unreadable. A corrupt file is logged and replaced on the next write —
    /// never an error: the record only gates the next restart's continuation.
    pub(crate) fn load(path: PathBuf) -> Self {
        let sessions = sidecar::load::<WakeMarkFile>(&path, "Wake watermark").sessions;
        Self {
            path,
            sessions: Mutex::new(sessions),
        }
    }

    /// The session's mark, when one exists — the Fresh path's gate: a Wake at
    /// or below it was already announced.
    pub(crate) fn announced(&self, session_id: &str) -> Option<WakeMark> {
        self.lock().get(session_id).cloned()
    }

    /// Advance the session's mark to `(wake_id, created_ms)` and persist, when
    /// strictly newer than the current one. Best-effort: a failed write leaves
    /// the in-memory mark advanced (the next advance retries the file), which
    /// suppresses at most the announcement whose write raced the crash — the
    /// same duplicate-over-loss trade the drain itself makes.
    pub(crate) fn advance(&self, session_id: &str, wake_id: &str, created_ms: i64) {
        let mut sessions = self.lock();
        if sessions
            .get(session_id)
            .is_some_and(|mark| mark.created_ms >= created_ms)
        {
            return;
        }
        sessions.insert(
            session_id.to_string(),
            WakeMark {
                wake_id: wake_id.to_string(),
                created_ms,
            },
        );
        sidecar::store(
            &self.path,
            "Wake watermark",
            &WakeMarkFile {
                sessions: sessions.clone(),
            },
            sessions.is_empty(),
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, WakeMark>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_persists_and_reloads_the_newest_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wake_watermarks.json");
        let watermarks = WakeWatermarks::load(path.clone());

        watermarks.advance("ses_a", "msg_wake_1", 1_000);
        // Older or equal sends do not move the mark.
        watermarks.advance("ses_a", "msg_wake_0", 900);
        watermarks.advance("ses_a", "msg_wake_1b", 1_000);
        watermarks.advance("ses_b", "msg_wake_2", 2_000);

        let reloaded = WakeWatermarks::load(path);
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

    #[test]
    fn a_corrupt_or_missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wake_watermarks.json");
        assert_eq!(WakeWatermarks::load(path.clone()).announced("ses_a"), None);

        std::fs::write(&path, "not json").unwrap();
        let watermarks = WakeWatermarks::load(path.clone());
        assert_eq!(watermarks.announced("ses_a"), None);
        // The next advance replaces the corrupt file with a valid one.
        watermarks.advance("ses_a", "msg_wake_1", 1_000);
        assert_eq!(
            WakeWatermarks::load(path).announced("ses_a"),
            Some(WakeMark {
                wake_id: "msg_wake_1".into(),
                created_ms: 1_000,
            })
        );
    }
}
