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
use crate::bridge::request::kind::InteractionKind;
use crate::bridge::sidecar;
use crate::state::ClientScoped;

/// One live interaction block and the card that renders it — the persisted
/// half of the card-handle registry's block entries (ADR-0038, rule 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InlineSurface {
    /// The Agent Client that minted the request/session (ADR-0091).
    #[serde(default)]
    pub client_id: String,
    pub message_id: String,
    pub kind: InteractionKind,
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

/// The lane that carried an over-budget permission's full target (ADR-0083 §5,
/// ADR-0102): the Rich Text Message, or the File Message it falls back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompanionLane {
    /// One Rich Text Message (`msg_type: "post"` carrying the `md` tag).
    RichText,
    /// One File Message (an uploaded `.txt`) when the `post` body would exceed
    /// its 30 KB cap or the Rich Text send definitely failed.
    File,
}

impl CompanionLane {
    /// The user-facing name the summary and result cards render.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::RichText => "富文本消息（Rich Text Message）",
            Self::File => "文件消息（File Message）",
        }
    }

    /// The short name a bounded Interaction Receipt can carry.
    pub(crate) fn short_label(self) -> &'static str {
        match self {
            Self::RichText => "富文本消息",
            Self::File => "文件消息",
        }
    }
}

/// One companion message that carried a permission request's full content —
/// the persisted proof the click guard trusts (ADR-0102). The record is
/// client-scoped like every other surface and read back after a restart without
/// re-sending. A confirmed send carries its message id; an ambiguous send
/// persists a marker with an empty id that only prevents a duplicate send —
/// it is **not** a completeness proof.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompanionDelivery {
    /// The message the companion landed as. Empty when the send outcome was
    /// ambiguous (a timeout or a 5xx after the request went out): the message
    /// may exist, so it must not be retried into a duplicate, but no id came
    /// back. The marker never permits an approval.
    pub message_id: String,
    pub lane: CompanionLane,
    /// The digest of the exact content that was delivered. The click guard
    /// compares it with the pending request it re-reads, so a card can never
    /// approve content the companion did not carry.
    pub target_digest: u64,
}

impl CompanionDelivery {
    /// Whether the send may have landed but returned no message id. Such a
    /// record is only a "do not re-send" marker, not a delivery proof.
    pub(crate) fn is_uncertain(&self) -> bool {
        self.message_id.is_empty()
    }

    /// Whether the companion is a confirmed delivery the click guard may trust.
    pub(crate) fn is_confirmed(&self) -> bool {
        !self.is_uncertain()
    }
}

/// One standalone request card cola sent — the persisted half of a flow's
/// `sent_cards` (ADR-0038, rule 6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StandaloneSurface {
    /// The Agent Client that minted the request/session (ADR-0091).
    #[serde(default)]
    pub client_id: String,
    pub kind: InteractionKind,
    pub message_id: String,
    pub summary: String,
    pub directory: String,
    /// The owning session, so a restarted process can classify the stale copy
    /// by the run's state. Defaults to empty for a record written by an older
    /// build: an unknown owner keeps the neutral copy.
    #[serde(default)]
    pub session_id: String,
}

/// The durable push/dismissal decision for a directory-owned **Location Form**
/// (ADR-0103). A restart must remember both decisions so it neither re-pushes a
/// form the Host deferred nor duplicates a live card. This record and the
/// [`StandaloneSurface`] it describes are written in **one** mutation, so a
/// restart cannot see one without the other.
///
/// The section is `#[serde(default)]` and its fields are too: an
/// `interactive_surfaces.json` written before this record existed loads with
/// empty suppression and needs no format-version bump (ADR-0091).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocationFormRecord {
    /// The directory that owns the form. A sweep can speak for a request only
    /// while that directory is still mapped and listed, so an unmapped
    /// directory's record is retained instead of pruned.
    #[serde(default)]
    pub directory: String,
    /// A standalone card was delivered and has not been dismissed.
    #[serde(default)]
    pub pushed: bool,
    /// 「稍后」 withdrew the card. The form stays pending but must not be
    /// pushed again until a reopen clears this.
    #[serde(default)]
    pub dismissed: bool,
    /// Monotonic dismissal generation. A reopen reads it before sending and
    /// clears the dismissal only if it is unchanged afterwards, so a dismissal
    /// that lands during the send survives.
    #[serde(default)]
    pub dismissal_generation: u64,
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
    /// request_id → the durable push/dismissal decision for a Location Form.
    pub location_forms: HashMap<String, LocationFormRecord>,
}

impl SurfaceState {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.inline.is_empty()
            && self.cards.is_empty()
            && self.standalone.is_empty()
            && self.location_forms.is_empty()
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
    #[serde(default)]
    companion: ClientScoped<CompanionDelivery>,
    #[serde(default)]
    location_forms: ClientScoped<LocationFormRecord>,
}

impl SurfaceFile {
    fn is_empty(&self) -> bool {
        self.inline.is_empty()
            && self.cards.is_empty()
            && self.standalone.is_empty()
            && self.companion.is_empty()
            && self.location_forms.is_empty()
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
            location_forms: self
                .location_forms
                .iter_client(client)
                .map(|(id, record)| (id.clone(), record.clone()))
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
                        companion: ClientScoped::default(),
                        location_forms: ClientScoped::default(),
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

    /// This client's durable push/dismissal decision for `request_id`.
    pub fn location_form(&self, request_id: &str) -> Option<LocationFormRecord> {
        self.lock()
            .location_forms
            .get(&self.client_id, request_id)
            .cloned()
    }

    /// Record that a Location Form's card was pushed: the standalone surface
    /// and the `pushed` decision land in **one** mutation (ADR-0103), so a
    /// restart never sees one without the other.
    pub fn set_location_form_pushed(
        &self,
        request_id: &str,
        mut surface: StandaloneSurface,
        directory: &str,
    ) {
        surface.client_id = self.client_id.clone();
        self.mutate(|state| {
            let mut record = state
                .location_forms
                .get(&self.client_id, request_id)
                .cloned()
                .unwrap_or_default();
            record.directory = directory.to_string();
            record.pushed = true;
            record.dismissed = false;
            state.location_forms.insert(&self.client_id, request_id, record);
            state.standalone.insert(&self.client_id, request_id, surface);
            true
        });
    }

    /// Record a 「稍后」 dismissal and forget the withdrawn standalone card in
    /// one mutation. The form stays pending; the persisted dismissal is what
    /// stops a later sweep (or a restart) from pushing it again.
    pub fn dismiss_location_form(&self, request_id: &str, directory: &str) {
        self.mutate(|state| {
            let mut record = state
                .location_forms
                .get(&self.client_id, request_id)
                .cloned()
                .unwrap_or_default();
            if !directory.is_empty() {
                record.directory = directory.to_string();
            }
            record.pushed = true;
            record.dismissed = true;
            record.dismissal_generation = record.dismissal_generation.wrapping_add(1);
            state.location_forms.insert(&self.client_id, request_id, record);
            state.standalone.remove(&self.client_id, request_id);
            true
        });
    }

    /// Clear the dismissal a reopen read before sending, and record the fresh
    /// standalone card in the same mutation. Returns whether it cleared: a
    /// dismissal recorded after `generation` was read (a 「稍后」 that landed
    /// during the send) leaves the record dismissed and returns `false`.
    pub fn clear_location_form_dismissal(
        &self,
        request_id: &str,
        generation: u64,
        mut surface: StandaloneSurface,
    ) -> bool {
        surface.client_id = self.client_id.clone();
        let mut cleared = false;
        self.mutate(|state| {
            let Some(mut record) = state.location_forms.get(&self.client_id, request_id).cloned() else {
                return false;
            };
            if !record.dismissed || record.dismissal_generation != generation {
                return false;
            }
            record.dismissed = false;
            record.pushed = true;
            state.location_forms.insert(&self.client_id, request_id, record);
            state.standalone.insert(&self.client_id, request_id, surface);
            cleared = true;
            true
        });
        cleared
    }

    /// Forget every Location Form record `keep` does not name. The sweep calls
    /// this only when every mapped directory listed successfully, so "not
    /// pending" really means the request resolved (#130).
    pub fn retain_location_forms(&self, keep: impl Fn(&str, &LocationFormRecord) -> bool) {
        self.mutate(|state| {
            let stale: Vec<String> = state
                .location_forms
                .iter_client(&self.client_id)
                .filter(|(id, record)| !keep(id, record))
                .map(|(id, _)| id.clone())
                .collect();
            for id in &stale {
                state.location_forms.remove(&self.client_id, id);
            }
            !stale.is_empty()
        });
    }

    /// Record the companion message that delivered `request_id`'s full target.
    /// Written only after the send landed, so the record is never a promise
    /// (ADR-0102).
    pub fn set_companion(&self, request_id: &str, delivery: CompanionDelivery) {
        self.mutate(|state| {
            let changed = state.companion.get(&self.client_id, request_id) != Some(&delivery);
            if changed {
                state.companion.insert(&self.client_id, request_id, delivery);
            }
            changed
        });
    }

    /// This client's companion record for `request_id`, when one was delivered.
    pub fn companion(&self, request_id: &str) -> Option<CompanionDelivery> {
        self.lock().companion.get(&self.client_id, request_id).cloned()
    }

    /// The record for `request_id` only when it proves the exact content
    /// identified by `target_digest`. This is the one predicate the click
    /// guard, readoption and delivery replacement share (ADR-0102), so a stale
    /// record can never validate a different request.
    pub fn matching_companion(&self, request_id: &str, target_digest: u64) -> Option<CompanionDelivery> {
        self.companion(request_id)
            .filter(|delivery| delivery.target_digest == target_digest)
    }

    /// The record for `request_id` only when it is a **confirmed** delivery
    /// (a message id came back) and its digest matches. An uncertain marker is
    /// never a completeness proof (ADR-0102).
    pub fn delivered_companion(&self, request_id: &str, target_digest: u64) -> Option<CompanionDelivery> {
        self.matching_companion(request_id, target_digest)
            .filter(CompanionDelivery::is_confirmed)
    }

    /// Forget a companion record — its request resolved (or was refused).
    pub fn remove_companion(&self, request_id: &str) {
        self.mutate(|state| state.companion.remove(&self.client_id, request_id).is_some());
    }

    /// Forget every companion record `keep` does not name. The sweep calls this
    /// only when every directory listed successfully, so "not pending" really
    /// means the request resolved (#130).
    pub fn retain_companions(&self, keep: impl Fn(&str) -> bool) {
        let stale: Vec<String> = {
            let state = self.lock();
            state
                .companion
                .iter_client(&self.client_id)
                .map(|(id, _)| id.clone())
                .filter(|id| !keep(id))
                .collect()
        };
        for id in stale {
            self.remove_companion(&id);
        }
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
            kind: InteractionKind::Permission,
            session_id: "ses_1".into(),
            directory: "/work".into(),
            target: "⚡ 执行 Shell 命令 `ls`".into(),
        }
    }

    fn standalone(message_id: &str) -> StandaloneSurface {
        StandaloneSurface {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            kind: InteractionKind::Question,
            message_id: message_id.into(),
            summary: "Context7 登录".into(),
            directory: "/work".into(),
            session_id: String::new(),
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
                    kind: InteractionKind::Permission,
                    message_id: "om_2".into(),
                    summary: "ls".into(),
                    directory: "/work".into(),
                    session_id: "ses_1".into(),
                },
            );
            surfaces.set_companion(
                "per_3",
                CompanionDelivery {
                    message_id: "om_post".into(),
                    lane: CompanionLane::RichText,
                    target_digest: 42,
                },
            );
        }

        let surfaces = Surfaces::load(path);
        let loaded = surfaces.snapshot();
        assert_eq!(loaded.inline.get("per_1"), Some(&inline("om_1")));
        assert_eq!(loaded.cards.get("om_1").unwrap().spans, vec![span("per_1", 0, 2)]);
        assert_eq!(loaded.standalone.get("per_2").unwrap().message_id, "om_2");
        assert_eq!(
            surfaces.companion("per_3"),
            Some(CompanionDelivery {
                message_id: "om_post".into(),
                lane: CompanionLane::RichText,
                target_digest: 42,
            })
        );
    }

    /// A companion record is removed with its request, and a record for another
    /// request is retained. The sweep relies on the retain helper's predicate
    /// being applied to every stored id.
    #[test]
    fn companion_records_are_removed_and_retained_by_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        let surfaces = Surfaces::load(path.clone());
        for (id, digest) in [("per_1", 1), ("per_2", 2)] {
            surfaces.set_companion(
                id,
                CompanionDelivery {
                    message_id: format!("om_{id}"),
                    lane: CompanionLane::File,
                    target_digest: digest,
                },
            );
        }
        surfaces.retain_companions(|id| id == "per_1");
        assert_eq!(surfaces.companion("per_1").unwrap().target_digest, 1);
        assert!(surfaces.companion("per_2").is_none());

        surfaces.remove_companion("per_1");
        assert!(surfaces.companion("per_1").is_none());
        assert!(!path.exists(), "the last record leaves no file behind");
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

    /// ADR-0103: the push decision and its standalone surface are one record;
    /// a dismissal withdraws the surface but keeps the decision, and the two
    /// survive a reload.
    #[test]
    fn location_form_push_and_dismissal_round_trip_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        {
            let surfaces = Surfaces::load(path.clone());
            surfaces.set_location_form_pushed("frm_1", standalone("om_card"), "/work");
            let record = surfaces.location_form("frm_1").expect("a pushed record");
            assert!(record.pushed && !record.dismissed);
            assert_eq!(
                surfaces.snapshot().standalone.get("frm_1").unwrap().message_id,
                "om_card"
            );
            surfaces.dismiss_location_form("frm_1", "/work");
        }

        let surfaces = Surfaces::load(path);
        let record = surfaces.location_form("frm_1").expect("the dismissal persisted");
        assert!(record.pushed && record.dismissed);
        assert_eq!(record.dismissal_generation, 1);
        assert!(
            !surfaces.snapshot().standalone.contains_key("frm_1"),
            "the withdrawn surface is forgotten in the same mutation"
        );
    }

    /// A reopen clears only the dismissal generation it read: a later dismissal
    /// leaves the record dismissed and the fresh surface unwritten.
    #[test]
    fn a_reopen_clears_only_the_generation_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        let surfaces = Surfaces::load(path);
        surfaces.dismiss_location_form("frm_1", "/work");
        let generation = surfaces.location_form("frm_1").unwrap().dismissal_generation;

        // A dismissal lands between the read and the commit.
        surfaces.dismiss_location_form("frm_1", "/work");
        assert!(
            !surfaces.clear_location_form_dismissal("frm_1", generation, standalone("om_new")),
            "a newer dismissal survives the reopen"
        );
        assert!(surfaces.location_form("frm_1").unwrap().dismissed);
        assert!(
            !surfaces.snapshot().standalone.contains_key("frm_1"),
            "no surface is recorded for a reopen that lost the race"
        );

        let current = surfaces.location_form("frm_1").unwrap().dismissal_generation;
        assert!(surfaces.clear_location_form_dismissal("frm_1", current, standalone("om_new")));
        assert!(!surfaces.location_form("frm_1").unwrap().dismissed);
    }

    /// An older file has no `location_forms` section: it loads with empty
    /// suppression and no format-version bump is needed.
    #[test]
    fn an_older_file_without_the_location_form_section_loads_empty_suppression() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interactive_surfaces.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":{},"inline":{{}},"cards":{{}},"standalone":{{}}}}"#,
                crate::state::STATE_FORMAT_VERSION
            ),
        )
        .unwrap();

        let surfaces = Surfaces::open(path, crate::config::DEFAULT_CLIENT_ID).unwrap();
        assert!(surfaces.snapshot().location_forms.is_empty());
        assert!(surfaces.location_form("frm_1").is_none());
    }
}
