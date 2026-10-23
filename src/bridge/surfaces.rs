//! The persisted interactive-surface state (ADR-0038, restart re-adoption).
//!
//! The block registry, the per-message card-JSON cache and the standalone-card
//! records are process-local; after a `/restart` (or a crash) a still-pending
//! request used to have no surface this process knew about, so the first sweep
//! posted a second standalone card while the pre-restart card froze with its
//! controls still showing. This module mirrors that state to
//! `interactive_surfaces.json` (beside `sessions.json`, like
//! `pinned_chats.json`) so the next process **re-adopts** the card the request
//! already lives on: the first sweep that lists the request repaints the
//! persisted card instead of sending a new one, and a click on it resolves the
//! request exactly as before the restart.
//!
//! The file is a best-effort mirror, written through on every registry
//! mutation (block recorded/forgotten, card cached/released, standalone card
//! sent/forgotten) and read once at startup. A write failure logs and changes
//! nothing else — the in-memory state stays authoritative. A card's JSON is
//! written only when its live blocks change: a live block freezes the turn's
//! content, so the header timer's extra flushes carry no new interactive state
//! and must not rewrite the file every second.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::bridge::card_handles::BlockSpan;
use crate::bridge::sidecar;
use crate::bridge::snapshot_claims::ClaimKind;
use crate::state::ClientScoped;

/// One live interaction block and the card that renders it — the persisted
/// half of the card-handle registry's block entries (ADR-0038, rule 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InlineSurface {
    /// The Agent Client that minted the request/session (ADR-0091).
    #[serde(default)]
    pub client_id: String,
    pub message_id: String,
    pub kind: ClaimKind,
    pub session_id: String,
    pub directory: String,
    /// The block's compact receipt target, recorded at render time.
    pub target: String,
}

/// One card that renders at least one live interaction block: the JSON as last
/// sent to Feishu plus the element ranges of its live blocks — the persisted
/// half of the per-message card-JSON cache (ADR-0038, rule 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedSurface {
    pub card: serde_json::Value,
    pub spans: Vec<BlockSpan>,
}

/// One standalone request card cola sent — the persisted half of a flow's
/// `sent_cards` (ADR-0038, rule 6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StandaloneSurface {
    /// The Agent Client that minted the request/session (ADR-0091).
    #[serde(default)]
    pub client_id: String,
    pub kind: ClaimKind,
    pub message_id: String,
    pub summary: String,
    pub directory: String,
    /// The owning session, so a restarted process can classify the stale copy
    /// by the run's state. Defaults to empty for a record written by an older
    /// build: an unknown owner keeps the neutral copy.
    #[serde(default)]
    pub session_id: String,
}

/// One active client's view of the persisted record — what the startup
/// hydration reads. The file itself is [`SurfaceFile`], scoped by client.
#[derive(Debug, Default, Clone)]
pub struct SurfaceState {
    /// request_id → the card rendering its live block.
    pub inline: HashMap<String, InlineSurface>,
    /// message_id → the card JSON and the live blocks it renders.
    pub cards: HashMap<String, CachedSurface>,
    /// request_id → the standalone card sent for it.
    pub standalone: HashMap<String, StandaloneSurface>,
}

impl SurfaceState {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.inline.is_empty() && self.cards.is_empty() && self.standalone.is_empty()
    }
}

/// The version-1 file (ADR-0091): every map is scoped by Agent Client first,
/// because request and message ids are minted per client and two clients could
/// otherwise collide on one key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SurfaceFile {
    version: u32,
    #[serde(default)]
    inline: ClientScoped<InlineSurface>,
    #[serde(default)]
    cards: ClientScoped<CachedSurface>,
    #[serde(default)]
    standalone: ClientScoped<StandaloneSurface>,
}

impl SurfaceFile {
    fn is_empty(&self) -> bool {
        self.inline.is_empty() && self.cards.is_empty() && self.standalone.is_empty()
    }

    /// This client's maps, flattened for the startup hydration.
    fn view(&self, client: &str) -> SurfaceState {
        SurfaceState {
            inline: self
                .inline
                .iter_client(client)
                .map(|(id, surface)| (id.clone(), surface.clone()))
                .collect(),
            cards: self
                .cards
                .iter_client(client)
                .map(|(id, card)| (id.clone(), card.clone()))
                .collect(),
            standalone: self
                .standalone
                .iter_client(client)
                .map(|(id, surface)| (id.clone(), surface.clone()))
                .collect(),
        }
    }
}

/// The version-0 shape (ADR-0091): one flat map per section, read as the
/// default client's.
#[derive(Debug, Default, Deserialize)]
struct SurfaceFileV0 {
    #[serde(default)]
    inline: HashMap<String, InlineSurface>,
    #[serde(default)]
    cards: HashMap<String, CachedSurface>,
    #[serde(default)]
    standalone: HashMap<String, StandaloneSurface>,
}

/// The write-through mirror of the interactive-surface state, persisted beside
/// `sessions.json` (`interactive_surfaces.json`).
pub struct Surfaces {
    path: PathBuf,
    /// The Agent Client this process is attached to (ADR-0091).
    client_id: String,
    state: Mutex<SurfaceFile>,
}

impl Surfaces {
    /// The test convenience: the implicit `default` client.
    #[cfg(test)]
    pub fn load(path: PathBuf) -> Self {
        Self::open(path, crate::config::DEFAULT_CLIENT_ID).unwrap_or_else(|error| panic!("{error}"))
    }

    /// Load the persisted record, or an empty one when the file is missing or
    /// unreadable. A corrupt file is logged and replaced on the next write —
    /// never an error: the mirror only feeds the next restart's re-adoption.
    ///
    /// ADR-0091: a version-0 file is read as the **default** client's and
    /// rewritten once in the current form; a newer version is refused.
    pub(crate) fn open(path: PathBuf, client_id: &str) -> anyhow::Result<Self> {
        let mut rewrite = false;
        // A current-version file may carry an empty embedded client id (written
        // by an intermediate build). Repair it once and persist the repair, so
        // the next load does not repeat it.
        let mut repaired = false;
        let state = match sidecar::read_versioned(&path, "surface record") {
            sidecar::VersionedRead::Missing
            | sidecar::VersionedRead::Unreadable
            | sidecar::VersionedRead::Corrupt => SurfaceFile {
                version: crate::state::STATE_FORMAT_VERSION,
                ..SurfaceFile::default()
            },
            sidecar::VersionedRead::Unsupported(version) => {
                anyhow::bail!(
                    "{} declares unsupported format version {version} (this build writes {}); \
                     refusing to read it as version 0",
                    path.display(),
                    crate::state::STATE_FORMAT_VERSION
                );
            }
            sidecar::VersionedRead::Legacy(raw) => match serde_json::from_str::<SurfaceFileV0>(&raw) {
                Ok(file) => {
                    let mut inline = file.inline;
                    for surface in inline.values_mut() {
                        surface.client_id = crate::config::DEFAULT_CLIENT_ID.to_string();
                    }
                    let mut standalone = file.standalone;
                    for surface in standalone.values_mut() {
                        surface.client_id = crate::config::DEFAULT_CLIENT_ID.to_string();
                    }
                    rewrite = true;
                    SurfaceFile {
                        version: crate::state::STATE_FORMAT_VERSION,
                        inline: ClientScoped::from_flat(crate::config::DEFAULT_CLIENT_ID, inline),
                        cards: ClientScoped::from_flat(crate::config::DEFAULT_CLIENT_ID, file.cards),
                        standalone: ClientScoped::from_flat(crate::config::DEFAULT_CLIENT_ID, standalone),
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        "could not parse {} ({error}); starting with an empty surface record",
                        path.display()
                    );
                    SurfaceFile {
                        version: crate::state::STATE_FORMAT_VERSION,
                        ..SurfaceFile::default()
                    }
                }
            },
            sidecar::VersionedRead::Current(raw) => match serde_json::from_str::<SurfaceFile>(&raw) {
                Ok(mut file) => {
                    // A record written by an intermediate build may carry the
                    // client in the scope but not in the surface itself; keep
                    // the two spellings agreeing.
                    for (client, surfaces) in file.inline.clients_mut() {
                        for surface in surfaces.values_mut() {
                            if surface.client_id.is_empty() {
                                surface.client_id = client.clone();
                                repaired = true;
                            }
                        }
                    }
                    for (client, surfaces) in file.standalone.clients_mut() {
                        for surface in surfaces.values_mut() {
                            if surface.client_id.is_empty() {
                                surface.client_id = client.clone();
                                repaired = true;
                            }
                        }
                    }
                    file
                }
                Err(error) => {
                    tracing::warn!(
                        "could not parse {} ({error}); starting with an empty surface record",
                        path.display()
                    );
                    SurfaceFile {
                        version: crate::state::STATE_FORMAT_VERSION,
                        ..SurfaceFile::default()
                    }
                }
            },
        };
        let surfaces = Self {
            path,
            client_id: client_id.to_string(),
            state: Mutex::new(state),
        };
        if rewrite || repaired {
            let state = surfaces.lock();
            // A migration must land the version even when the record is empty;
            // the normal empty-record removal applies only to later mutations.
            sidecar::store(&surfaces.path, "surface record", &*state, false);
        }
        Ok(surfaces)
    }

    /// The persisted record for this process's client, cloned for startup
    /// hydration.
    pub fn snapshot(&self) -> SurfaceState {
        self.lock().view(&self.client_id)
    }

    /// Record (or refresh) the card rendering `request_id`'s live block.
    pub fn set_inline(&self, request_id: &str, mut surface: InlineSurface) {
        surface.client_id = self.client_id.clone();
        self.mutate(|state| {
            let changed = state.inline.get(&self.client_id, request_id) != Some(&surface);
            if changed {
                state.inline.insert(&self.client_id, request_id, surface);
            }
            changed
        });
    }

    /// Forget a block's registry entry — it resolved.
    pub fn remove_inline(&self, request_id: &str) {
        self.mutate(|state| state.inline.remove(&self.client_id, request_id).is_some());
    }

    /// Cache a card's last-rendered JSON and its live blocks' element ranges.
    /// A re-record whose spans did not change is a header-timer flush of an
    /// otherwise frozen card: the already-persisted JSON stays (see the module
    /// doc).
    pub fn set_card(&self, message_id: &str, card: &serde_json::Value, spans: &[BlockSpan]) {
        self.mutate(|state| {
            if state
                .cards
                .get(&self.client_id, message_id)
                .is_some_and(|cached| cached.spans == spans)
            {
                return false;
            }
            state.cards.insert(
                &self.client_id,
                message_id,
                CachedSurface {
                    card: card.clone(),
                    spans: spans.to_vec(),
                },
            );
            true
        });
    }

    /// Release a card's cache — it no longer renders a live block.
    pub fn remove_card(&self, message_id: &str) {
        self.mutate(|state| state.cards.remove(&self.client_id, message_id).is_some());
    }

    /// Record the standalone card cola sent for `request_id`.
    pub fn set_standalone(&self, request_id: &str, mut surface: StandaloneSurface) {
        surface.client_id = self.client_id.clone();
        self.mutate(|state| {
            let changed = state.standalone.get(&self.client_id, request_id) != Some(&surface);
            if changed {
                state.standalone.insert(&self.client_id, request_id, surface);
            }
            changed
        });
    }

    /// Forget a standalone card's record — it resolved or was marked stale.
    pub fn remove_standalone(&self, request_id: &str) {
        self.mutate(|state| state.standalone.remove(&self.client_id, request_id).is_some());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SurfaceFile> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Apply `f` under the lock and persist only when it reports a change.
    fn mutate(&self, f: impl FnOnce(&mut SurfaceFile) -> bool) {
        let mut state = self.lock();
        if !f(&mut state) {
            return;
        }
        self.persist(&state);
    }

    fn persist(&self, state: &SurfaceFile) {
        sidecar::store(&self.path, "surface record", state, state.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(request_id: &str, start: usize, end: usize) -> BlockSpan {
        BlockSpan {
            request_id: request_id.into(),
            start,
            end,
        }
    }

    fn inline(message_id: &str) -> InlineSurface {
        InlineSurface {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            message_id: message_id.into(),
            kind: ClaimKind::Permission,
            session_id: "ses_1".into(),
            directory: "/work".into(),
            target: "⚡ 执行 Shell 命令 `ls`".into(),
        }
    }

    #[test]
    fn record_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        {
            let surfaces = Surfaces::load(path.clone());
            surfaces.set_inline("per_1", inline("om_1"));
            surfaces.set_card(
                "om_1",
                &serde_json::json!({ "body": { "elements": [] } }),
                &[span("per_1", 0, 2)],
            );
            surfaces.set_standalone(
                "per_2",
                StandaloneSurface {
                    client_id: crate::config::DEFAULT_CLIENT_ID.into(),
                    kind: ClaimKind::Permission,
                    message_id: "om_2".into(),
                    summary: "ls".into(),
                    directory: "/work".into(),
                    session_id: "ses_1".into(),
                },
            );
        }

        let loaded = Surfaces::load(path).snapshot();
        assert_eq!(loaded.inline.get("per_1"), Some(&inline("om_1")));
        assert_eq!(loaded.cards.get("om_1").unwrap().spans, vec![span("per_1", 0, 2)]);
        assert_eq!(loaded.standalone.get("per_2").unwrap().message_id, "om_2");
    }

    #[test]
    fn empty_record_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        let surfaces = Surfaces::load(path.clone());
        surfaces.set_inline("per_1", inline("om_1"));
        assert!(path.exists());
        surfaces.remove_inline("per_1");
        assert!(!path.exists(), "an empty record leaves no file behind");
    }

    /// The header timer's re-records carry the same spans: the persisted JSON
    /// must not be rewritten every second (it stays the card as of its live
    /// blocks' last change).
    #[test]
    fn a_same_spans_re_record_keeps_the_cached_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        let surfaces = Surfaces::load(path);
        surfaces.set_card("om_1", &serde_json::json!({ "v": 1 }), &[span("per_1", 0, 2)]);
        surfaces.set_card("om_1", &serde_json::json!({ "v": 2 }), &[span("per_1", 0, 2)]);
        assert_eq!(
            surfaces.snapshot().cards.get("om_1").unwrap().card,
            serde_json::json!({ "v": 1 })
        );

        surfaces.set_card("om_1", &serde_json::json!({ "v": 3 }), &[span("per_1", 0, 3)]);
        assert_eq!(
            surfaces.snapshot().cards.get("om_1").unwrap().card,
            serde_json::json!({ "v": 3 }),
            "a span change rewrites the cached JSON"
        );
    }

    #[test]
    fn corrupt_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        std::fs::write(&path, "{not json").unwrap();
        let surfaces = Surfaces::load(path);
        assert!(surfaces.snapshot().is_empty());
    }

    /// A version-0 file is rewritten in the current form even when it is
    /// empty: the migration marker is the version, not a non-empty record.
    #[test]
    fn an_empty_version_0_file_is_rewritten_versioned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        std::fs::write(&path, "{}").unwrap();
        let _ = Surfaces::open(path.clone(), crate::config::DEFAULT_CLIENT_ID).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["version"], crate::state::STATE_FORMAT_VERSION);
    }

    /// ADR-0091: a current-version file whose embedded client id is empty is
    /// repaired once and written back, not repaired in memory on every load.
    #[test]
    fn an_empty_embedded_client_is_repaired_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":{},"inline":{{"default":{{"per_1":{{"client_id":"","message_id":"om_1","kind":"Permission","session_id":"ses_1","directory":"/work","target":"ls"}}}}}},"cards":{{}},"standalone":{{}}}}"#,
                crate::state::STATE_FORMAT_VERSION
            ),
        )
        .unwrap();

        let surfaces = Surfaces::open(path.clone(), crate::config::DEFAULT_CLIENT_ID).unwrap();
        assert_eq!(
            surfaces.snapshot().inline.get("per_1").unwrap().client_id,
            crate::config::DEFAULT_CLIENT_ID
        );
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            raw["inline"]["default"]["per_1"]["client_id"],
            crate::config::DEFAULT_CLIENT_ID,
            "the repair is on disk"
        );

        let before = std::fs::read(&path).unwrap();
        let _ = Surfaces::open(path.clone(), crate::config::DEFAULT_CLIENT_ID).unwrap();
        assert_eq!(before, std::fs::read(&path).unwrap(), "the repair lands once");
    }
}
