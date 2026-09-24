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
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::bridge::card_handles::BlockSpan;
use crate::bridge::snapshot_claims::ClaimKind;

/// One live interaction block and the card that renders it — the persisted
/// half of the card-handle registry's block entries (ADR-0038, rule 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InlineSurface {
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
    pub kind: ClaimKind,
    pub message_id: String,
    pub summary: String,
    pub directory: String,
}

/// The whole persisted record. Every map defaults, so a file written by an
/// older build (or a truncated one) still loads what it can.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SurfaceState {
    /// request_id → the card rendering its live block.
    #[serde(default)]
    pub inline: HashMap<String, InlineSurface>,
    /// message_id → the card JSON and the live blocks it renders.
    #[serde(default)]
    pub cards: HashMap<String, CachedSurface>,
    /// request_id → the standalone card sent for it.
    #[serde(default)]
    pub standalone: HashMap<String, StandaloneSurface>,
}

impl SurfaceState {
    fn is_empty(&self) -> bool {
        self.inline.is_empty() && self.cards.is_empty() && self.standalone.is_empty()
    }
}

/// The write-through mirror of the interactive-surface state, persisted beside
/// `sessions.json` (`interactive_surfaces.json`).
pub struct Surfaces {
    path: PathBuf,
    state: Mutex<SurfaceState>,
}

impl Surfaces {
    /// Load the persisted record, or an empty one when the file is missing or
    /// unreadable. A corrupt file is logged and replaced on the next write —
    /// never an error: the mirror only feeds the next restart's re-adoption.
    pub fn load(path: PathBuf) -> Self {
        let state = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<SurfaceState>(&raw) {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!(
                        "could not parse {} ({e}); starting with an empty surface record",
                        path.display()
                    );
                    SurfaceState::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SurfaceState::default(),
            Err(e) => {
                tracing::warn!(
                    "could not read {} ({e}); starting with an empty surface record",
                    path.display()
                );
                SurfaceState::default()
            }
        };
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    /// The persisted record, cloned for startup hydration.
    pub fn snapshot(&self) -> SurfaceState {
        self.lock().clone()
    }

    /// Record (or refresh) the card rendering `request_id`'s live block.
    pub fn set_inline(&self, request_id: &str, surface: InlineSurface) {
        self.mutate(|state| {
            let changed = state.inline.get(request_id) != Some(&surface);
            if changed {
                state.inline.insert(request_id.to_string(), surface);
            }
            changed
        });
    }

    /// Forget a block's registry entry — it resolved.
    pub fn remove_inline(&self, request_id: &str) {
        self.mutate(|state| state.inline.remove(request_id).is_some());
    }

    /// Cache a card's last-rendered JSON and its live blocks' element ranges.
    /// A re-record whose spans did not change is a header-timer flush of an
    /// otherwise frozen card: the already-persisted JSON stays (see the module
    /// doc).
    pub fn set_card(&self, message_id: &str, card: &serde_json::Value, spans: &[BlockSpan]) {
        self.mutate(|state| {
            if state
                .cards
                .get(message_id)
                .is_some_and(|cached| cached.spans == spans)
            {
                return false;
            }
            state.cards.insert(
                message_id.to_string(),
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
        self.mutate(|state| state.cards.remove(message_id).is_some());
    }

    /// Record the standalone card cola sent for `request_id`.
    pub fn set_standalone(&self, request_id: &str, surface: StandaloneSurface) {
        self.mutate(|state| {
            let changed = state.standalone.get(request_id) != Some(&surface);
            if changed {
                state.standalone.insert(request_id.to_string(), surface);
            }
            changed
        });
    }

    /// Forget a standalone card's record — it resolved or was marked stale.
    pub fn remove_standalone(&self, request_id: &str) {
        self.mutate(|state| state.standalone.remove(request_id).is_some());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SurfaceState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Apply `f` under the lock and persist only when it reports a change.
    fn mutate(&self, f: impl FnOnce(&mut SurfaceState) -> bool) {
        let mut state = self.lock();
        if !f(&mut state) {
            return;
        }
        write_file(&self.path, &state);
    }
}

/// Write the record atomically (temp file + rename), best-effort: the mirror
/// only feeds the next startup's re-adoption, so a failure logs and changes
/// nothing else. An empty record removes the file.
fn write_file(path: &Path, state: &SurfaceState) {
    if state.is_empty() {
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("surfaces: could not remove {}: {}", path.display(), e);
        }
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let data = match serde_json::to_string(state) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!("surfaces: could not serialize the record: {e}");
            return;
        }
    };
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, data) {
        tracing::warn!("surfaces: could not write {}: {}", tmp.display(), e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!("surfaces: could not replace {}: {}", path.display(), e);
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
                    kind: ClaimKind::Permission,
                    message_id: "om_2".into(),
                    summary: "ls".into(),
                    directory: "/work".into(),
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
}
