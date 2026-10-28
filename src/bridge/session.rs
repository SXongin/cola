use crate::config::{SessionEntry, ThreadKey};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use crate::bridge::sidecar;

/// A conversation's declared intent to create a session (ADR-0041). It is not
/// a Session: no backend identity, invisible to the server and every session
/// list. The conversation's first prompt materialises it; until then it
/// supersedes the mapped session (`get_active` returns `None`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEntry {
    pub thread_key: ThreadKey,
    pub directory: String,
    /// Title to PATCH after materialisation (`/new <name>`); `None` keeps the
    /// server-generated title.
    #[serde(default)]
    pub title: Option<String>,
    /// Per-session agent override (`/agent`), carried onto the materialised
    /// SessionEntry.
    #[serde(default)]
    pub agent: Option<String>,
    /// Per-session model override ("provider/model", `/model`).
    #[serde(default)]
    pub model: Option<String>,
    /// The thread-level provider this new thread should start on (issue #910).
    /// It has no per-turn form — `turn/start` has no provider field — so a
    /// Pending Session is the only place it can wait: the first message
    /// materialises the session with it as `thread/start.modelProvider`.
    #[serde(default)]
    pub provider: Option<String>,
    /// Per-session thinking level (`/think`).
    #[serde(default)]
    pub variant: Option<String>,
    /// `/autoaccept` state, carried onto the materialised SessionEntry.
    #[serde(default)]
    pub auto_accept: bool,
    /// For topic-backed pendings (`/topic`): the in-topic reply anchor
    /// (ADR-0022), carried onto the materialised SessionEntry.
    #[serde(default)]
    pub topic_anchor: Option<String>,
    /// For cola-created topics: the command message the topic was created
    /// around (ADR-0023), carried onto the materialised SessionEntry.
    #[serde(default)]
    pub topic_root: Option<String>,
}

impl PendingEntry {
    /// A pending with every optional field at its default.
    pub fn new(thread_key: ThreadKey, directory: impl Into<String>) -> Self {
        Self {
            thread_key,
            directory: directory.into(),
            title: None,
            agent: None,
            model: None,
            provider: None,
            variant: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
        }
    }

    /// The Session Mapping entry a materialised session gets: this pending's
    /// directory and per-session fields under the new `session_id` (ADR-0041).
    pub fn into_entry(self, session_id: impl Into<String>) -> SessionEntry {
        let mut entry = SessionEntry::new(self.thread_key, session_id, self.directory);
        entry.agent = self.agent;
        entry.model = self.model;
        entry.variant = self.variant;
        entry.auto_accept = self.auto_accept;
        entry.topic_anchor = self.topic_anchor;
        entry.topic_root = self.topic_root;
        entry
    }
}

/// The settings a conversation's next prompt will use (ADR-0041): the fields
/// shared by a real [`SessionEntry`] and a [`PendingEntry`] — directory,
/// per-session overrides, and (for a real session) its id. The settings
/// commands read this snapshot, mutate it and write it back through
/// [`SessionStore::set_settings`], so ONE code path configures either kind of
/// target. `session_id` is `None` exactly when the settings belong to a Pending
/// Session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSettings {
    /// The directory the session is (or will be) rooted at: the auto-accept
    /// approval target and the model ladder's fetch directory.
    pub directory: String,
    /// `Some` for a real SessionEntry, `None` for a Pending Session.
    pub session_id: Option<String>,
    /// Per-session agent override (`/agent`).
    pub agent: Option<String>,
    /// Per-session model override (`/model`, "provider/model").
    pub model: Option<String>,
    /// The thread-level provider a Pending Session will start on (issue #910).
    /// Only a creation can apply it, so this is `None` for a live session —
    /// there the thread's own `modelProvider` (the adapter's projection) is the
    /// fact, and its change goes through the confirmation path, not this
    /// mirror. [`SessionStore::set_settings`] honours that: an entry ignores
    /// this field.
    pub provider: Option<String>,
    /// Per-session thinking level (`/think`).
    pub variant: Option<String>,
    /// `/autoaccept` state.
    pub auto_accept: bool,
}

/// The current on-disk shape (ADR-0041). `pending` defaults so a file written
/// by an older build (`{"entries":[…]}`) still loads.
#[derive(Deserialize)]
struct StoreFile {
    #[serde(default)]
    entries: Vec<SessionEntry>,
    #[serde(default)]
    pending: Vec<PendingEntry>,
}

/// Pre-ADR-0041 files are a bare `[SessionEntry]` array. Kept loadable: the
/// old `from_str(...).unwrap_or_default()` would silently empty the whole
/// mapping on a format change.
#[derive(Deserialize)]
#[serde(untagged)]
enum LoadedStore {
    Legacy(Vec<SessionEntry>),
    Current(StoreFile),
}

impl LoadedStore {
    fn into_parts(self) -> (Vec<SessionEntry>, Vec<PendingEntry>) {
        match self {
            LoadedStore::Legacy(entries) => (entries, Vec::new()),
            LoadedStore::Current(file) => (file.entries, file.pending),
        }
    }
}

/// Manages the thread → session mapping, persisted to a JSON file.
/// Multiple sessions can exist per thread; the first matching entry is "active"
/// unless the thread has a Pending Session.
pub struct SessionStore {
    path: PathBuf,
    /// The Agent Client this process is attached to (ADR-0090 decision 5): the
    /// id stamped on every mapping it activates, and the half that scopes the
    /// store's lookups when one bare Session id exists under two clients
    /// (ADR-0091).
    client_id: String,
    /// The two halves are refcounted so a snapshot (what the writer serializes)
    /// is O(1) to take under the store lock — the callback never renders JSON
    /// (ADR-0089). A mutation copies the vector only when a snapshot is
    /// outstanding, and the writer drops its snapshot as soon as it has the
    /// bytes, so a stalled DISK never makes a mutation copy.
    entries: Arc<Vec<SessionEntry>>,
    /// At most one Pending Session per ThreadKey (ADR-0041).
    pending: Arc<Vec<PendingEntry>>,
    /// The one thread that writes `sessions.json` (ADR-0089); `None` only when
    /// the thread could not be started, in which case writes stay synchronous.
    writer: Option<crate::bridge::store_writer::StoreWriter>,
    /// The live materialisation claims, by thread key (issue #910): the claim's
    /// generation and the pending epoch it took. In memory only: a claim either
    /// finishes (activation resolves its own generation) or is released by the
    /// failure path, so a restart leaves an ordinary pending to retry.
    creating: std::collections::HashMap<ThreadKey, CreationClaim>,
    /// The identity of each thread's Pending Session, bumped whenever its
    /// declaration changes, so a creation or a pick that started on an older
    /// pending cannot resolve a replacement (`activate_created` and the pending
    /// mirror write compare the epochs).
    pending_epoch: std::collections::HashMap<ThreadKey, u64>,
    /// Monotonic, process-wide counter behind [`Self::pending_epoch`]: every
    /// declaration takes the next value, so an epoch is never reused even after
    /// its map entry is pruned (issue #910).
    next_pending_epoch: u64,
    /// Monotonic generation for materialisation claims.
    next_creation: u64,
}

/// One live materialisation claim (issue #910).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CreationClaim {
    generation: u64,
    /// The pending epoch the claim took; a replacement pending bumps it.
    epoch: u64,
}

/// What a model/provider pair write did (issue #910).
///
/// Only [`Self::Applied`] means the pair is durable. Every other arm means
/// **nothing was written**: the caller must say so, never claim the pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairWrite {
    /// Written. `cleared_variant` is the `/think` variant this write actually
    /// cleared (a CAS: a newer `/think` wins and nothing is cleared).
    Applied { cleared_variant: Option<String> },
    /// The thread no longer maps to that session (a `/switch` remap, a delete, a
    /// recreate, or a pending declared since): nothing written.
    Moved,
    /// The target's mirror no longer holds the state the caller read (a newer
    /// `/model` or `/think` landed while the caller awaited its catalog lookup):
    /// nothing written.
    Changed,
    /// The conversation's session is being created right now: `thread/start`
    /// already snapshotted the pending, so a pair written now would be claimed
    /// but never applied.
    Creating,
}

/// One mirror write's fields (issue #910). Every writer — a model pick, a
/// `/think` pick, a provider+model pair, the deferred record — goes through the
/// same compare-and-set with the session gate; this names what it touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MirrorWrite<'a> {
    /// A `/model` pick: the model, plus the `/think` variant to clear when the
    /// new model does not declare it (ADR-0020).
    Model {
        model: &'a str,
        clear_variant: Option<&'a str>,
    },
    /// A provider+model pair: the model, the pending's provider
    /// (`thread/start`'s `modelProvider`), and the same `/think` clear.
    Pair {
        model: &'a str,
        provider: &'a str,
        clear_variant: Option<&'a str>,
    },
    /// A durable generation's `/model` pick: the model and the session's own
    /// surviving variant (V2 keeps the selection server-side, so the mirror
    /// follows the session's variant, `None` included).
    ModelWithVariant {
        model: &'a str,
        variant: Option<&'a str>,
    },
    /// A `/think` pick: the variant, or `None` to clear it.
    Variant(Option<&'a str>),
}

/// Apply `write` to a mirror's model/variant slots, returning the variant after
/// the write and the one it actually cleared. The clear is a compare-and-set:
/// a `/think` that landed since the caller's snapshot wins.
fn apply_mirror_write(
    model: &mut Option<String>,
    variant: &mut Option<String>,
    write: MirrorWrite<'_>,
) -> Option<String> {
    match write {
        MirrorWrite::Model {
            model: new_model,
            clear_variant,
        }
        | MirrorWrite::Pair {
            model: new_model,
            clear_variant,
            ..
        } => {
            *model = Some(new_model.to_string());
            let mut cleared = None;
            if let Some(candidate) = clear_variant
                && variant.as_deref() == Some(candidate)
            {
                *variant = None;
                cleared = Some(candidate.to_string());
            }
            cleared
        }
        MirrorWrite::ModelWithVariant {
            model: new_model,
            variant: new_variant,
        } => {
            *model = Some(new_model.to_string());
            *variant = new_variant.map(str::to_string);
            None
        }
        MirrorWrite::Variant(new_variant) => {
            *variant = new_variant.map(str::to_string);
            None
        }
    }
}

/// What claiming a thread's Pending Session for materialisation found.
pub(crate) enum PendingClaim {
    /// Claimed, with the snapshot the creation will use and the claim's
    /// generation (issue #910): no pair write can land on the pending until the
    /// claim is resolved, and `activate_created` resolves only this generation.
    Claimed(Box<PendingEntry>, u64),
    /// Another message is materialising this pending right now.
    Creating,
    /// This thread has no Pending Session.
    None,
}

impl SessionStore {
    /// The test/default construction: the implicit `default` client.
    #[cfg(test)]
    pub fn new(path: PathBuf) -> crate::error::Result<Self> {
        Self::for_client(path, crate::config::DEFAULT_CLIENT_ID)
    }

    /// The production construction: `client_id` is the active Agent Client's id
    /// (ADR-0090 decision 5). A version-0 file reads as the **default** client's
    /// (ADR-0091), then is rewritten once in the current form; a newer format
    /// version is refused rather than silently read as version 0.
    pub fn for_client(path: PathBuf, client_id: &str) -> crate::error::Result<Self> {
        let (entries, pending, legacy) = match sidecar::read_versioned(&path, "session mapping") {
            sidecar::VersionedRead::Missing => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                (Vec::new(), Vec::new(), false)
            }
            sidecar::VersionedRead::Unreadable | sidecar::VersionedRead::Corrupt => {
                (Vec::new(), Vec::new(), false)
            }
            sidecar::VersionedRead::Unsupported(version) => {
                return Err(crate::error::BridgeError::State(format!(
                    "{} declares unsupported format version {version} (this build writes {}); \
                     refusing to read it as version 0",
                    path.display(),
                    crate::state::STATE_FORMAT_VERSION
                )));
            }
            sidecar::VersionedRead::Legacy(raw) => match parse_store(&raw, &path) {
                Some((mut entries, pending)) => {
                    // ADR-0091: a version-0 file belongs to the default client,
                    // whatever client this process happens to run.
                    for entry in &mut entries {
                        entry.client_id = crate::config::DEFAULT_CLIENT_ID.to_string();
                    }
                    (entries, pending, true)
                }
                // A version-0 body this build cannot parse fails open and is
                // not rewritten: the bytes stay for a human to inspect.
                None => (Vec::new(), Vec::new(), false),
            },
            sidecar::VersionedRead::Current(raw) => match parse_store(&raw, &path) {
                Some((entries, pending)) => (entries, pending, false),
                None => (Vec::new(), Vec::new(), false),
            },
        };
        let writer = match crate::bridge::store_writer::StoreWriter::start(path.clone()) {
            Ok(writer) => Some(writer),
            Err(error) => {
                tracing::warn!("could not start the session-store writer ({error}); writes stay synchronous");
                None
            }
        };
        let store = Self {
            path,
            client_id: client_id.to_string(),
            entries: Arc::new(entries),
            pending: Arc::new(pending),
            writer,
            creating: std::collections::HashMap::new(),
            pending_epoch: std::collections::HashMap::new(),
            next_pending_epoch: 0,
            next_creation: 0,
        };
        if legacy {
            // ADR-0091: the one eager rewrite. A later process reads version 1
            // and therefore does not write again at load.
            store.write_to_disk()?;
        }
        Ok(store)
    }

    /// The two halves as mutable vectors. `Arc::make_mut` copies only when a
    /// writer snapshot is outstanding, and the writer drops its snapshot as soon
    /// as it holds the bytes — so a stalled DISK never makes a mutation copy
    /// (ADR-0089); only a mutation racing an in-flight serialization does.
    fn entries_mut(&mut self) -> &mut Vec<SessionEntry> {
        Arc::make_mut(&mut self.entries)
    }

    /// [`Self::entries_mut`] for the Pending Sessions.
    fn pending_mut(&mut self) -> &mut Vec<PendingEntry> {
        Arc::make_mut(&mut self.pending)
    }

    /// Get the active session for a thread (first match). `None` while the
    /// thread has a Pending Session: the pending supersedes it, though the old
    /// session stays mapped (`list_thread`) and switchable (ADR-0041).
    pub fn get_active(&self, key: &ThreadKey) -> Option<&SessionEntry> {
        if self.pending_for(key).is_some() {
            return None;
        }
        self.entries
            .iter()
            .find(|e| e.client_id == self.client_id && &e.thread_key == key)
    }

    /// The conversation's Pending Session, if it declared one (ADR-0041).
    pub fn pending_for(&self, key: &ThreadKey) -> Option<&PendingEntry> {
        self.pending.iter().find(|p| &p.thread_key == key)
    }

    /// Declare (or replace) the conversation's Pending Session and persist.
    pub fn set_pending(&mut self, pending: PendingEntry) -> crate::error::Result<()> {
        // A replacement pending takes a new identity (issue #910): the
        // creation or pick that started on the old pending must not resolve
        // this one, and its later compare sees the mismatch. The claim itself
        // is kept until then, so no second creation starts concurrently.
        let key = pending.thread_key.clone();
        self.bump_pending_epoch(&key);
        self.pending_mut().retain(|p| p.thread_key != pending.thread_key);
        self.pending_mut().push(pending);
        self.write_to_disk()
    }

    /// Drop the conversation's Pending Session and persist.
    #[allow(dead_code)] // ADR-0041 storage; the command tickets (#211-#214) clear pendings
    pub fn clear_pending(&mut self, key: &ThreadKey) -> crate::error::Result<()> {
        self.creating.remove(key);
        self.bump_pending_epoch(key);
        self.pending_mut().retain(|p| &p.thread_key != key);
        self.prune_pending_epoch(key);
        self.write_to_disk()
    }

    /// The conversation's current directory: the Pending Session's when one
    /// exists, else the active session's. `None` when neither exists.
    pub fn current_directory(&self, key: &ThreadKey) -> Option<String> {
        self.pending_for(key)
            .map(|p| p.directory.clone())
            .or_else(|| self.get_active(key).map(|e| e.directory.clone()))
    }

    /// Add or promote a session entry as the active one for its thread.
    /// The entry is moved to the front so `get_active` returns it, and any
    /// Pending Session of the thread is resolved in the same in-memory step:
    /// a thread cannot have both (ADR-0041). Centralised here so no activation
    /// path can leave a pending behind to supersede the session it activated.
    fn promote(&mut self, entry: SessionEntry) {
        // Activation resolves the thread's Pending Session (a pending and an
        // active session are mutually exclusive, ADR-0041).
        self.pending_mut().retain(|p| p.thread_key != entry.thread_key);
        self.promote_entry(entry);
    }

    /// Promote the entry without resolving the thread's Pending Session: a
    /// materialisation whose replacement pending must survive (issue #910).
    fn promote_entry(&mut self, mut entry: SessionEntry) {
        // The store owns which client the mapping belongs to: a caller builds
        // an entry without knowing the process's active client.
        entry.client_id = self.client_id.clone();
        // Replace any existing mapping of this session. ADR-0041 keeps the
        // per-session overrides on the session: an adoption flow rebuilds the
        // mapping fields but never sets model/variant/auto_accept, so
        // re-adopting (switch card, `/switch <id>`, `/attach`, `/topic
        // --adopt`, force adopt) must not reset the settings the session
        // already had. An override the new entry set explicitly wins — only
        // fields left at their default are carried; `auto_accept: false` reads
        // as "unspecified" here, because a bool has no unset state and OFF is
        // written through the settings path (`update`), never activation.
        // Everything the flow owns — thread key, directory, agent, topic
        // anchors — stays as the new entry set it.
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.client_id == entry.client_id && e.session_id == entry.session_id)
        {
            let previous = self.entries_mut().remove(pos);
            if entry.model.is_none() {
                entry.model = previous.model;
            }
            if entry.variant.is_none() {
                entry.variant = previous.variant;
            }
            if !entry.auto_accept {
                entry.auto_accept = previous.auto_accept;
            }
        }
        self.entries_mut().insert(0, entry);
    }

    /// Promote `entry` as its thread's active session and persist the store.
    /// The durable half of every create/adopt/promote path: callers cannot
    /// forget the save. Activation also resolves the thread's Pending Session
    /// (a pending and an active session are mutually exclusive, ADR-0041) —
    /// materialisation is exactly this operation, in one write.
    pub fn activate(&mut self, entry: SessionEntry) -> crate::error::Result<()> {
        let key = entry.thread_key.clone();
        // An adoption/creation that is not a materialisation of the thread's
        // pending (a `/switch`, a recreate) resolves any claim: the thread's
        // active session is this one now. A materialisation must use
        // [`Self::activate_created`], which resolves only its own generation.
        self.creating.remove(&entry.thread_key);
        self.promote(entry);
        // With the claim resolved and the pending consumed, nothing can compare
        // against the thread's epoch any more (issue #910, the same rule as
        // `remove_thread`).
        self.prune_pending_epoch(&key);
        self.write_to_disk()
    }

    /// Promote the session a **materialisation** created (issue #910), scoped to
    /// its claim's generation. Only the creation that still owns the claim may
    /// promote (and resolve the pending): after a `/switch forget`, a `/new` or a
    /// newer claim, the session exists on the server but the thread moved on —
    /// promoting it would undo the forget or overwrite the newer selection, so
    /// nothing is written and the caller is told (`false`).
    ///
    /// A **replacement pending** (a bumped epoch, claim still ours) is the one
    /// case that promotes: the new session is mapped, but the replacement
    /// pending stays for its own creation.
    pub(crate) fn activate_created(
        &mut self,
        entry: SessionEntry,
        generation: u64,
    ) -> crate::error::Result<bool> {
        let key = entry.thread_key.clone();
        let Some(claim) = self.creating.get(&key).copied() else {
            tracing::warn!(
                thread = %key.thread_id,
                "a session creation finished after its thread moved on (forgotten or re-declared); \
                 not promoting it"
            );
            return Ok(false);
        };
        if claim.generation != generation {
            tracing::warn!(
                thread = %key.thread_id,
                "a session creation finished after a newer creation claimed the thread; \
                 not promoting the stale one"
            );
            return Ok(false);
        }
        let replaced = self.pending_epoch.get(&key).copied().unwrap_or_default() != claim.epoch;
        self.creating.remove(&key);
        if replaced {
            self.promote_entry(entry);
        } else {
            self.promote(entry);
        }
        self.prune_pending_epoch(&key);
        self.write_to_disk()?;
        Ok(true)
    }

    /// Whether this generation still owns its thread's materialisation claim
    /// (issue #910): checked right after `thread/start` so a thread that was
    /// forgotten or re-declared while it ran gets no title, picks or mapping.
    pub(crate) fn creation_is_current(&self, key: &ThreadKey, generation: u64) -> bool {
        self.creating
            .get(key)
            .is_some_and(|claim| claim.generation == generation)
    }

    /// The thread's current Pending Session identity (issue #910): `0` when it
    /// has none and none was ever declared since load.
    pub(crate) fn pending_epoch_for(&self, key: &ThreadKey) -> u64 {
        self.pending_epoch.get(key).copied().unwrap_or_default()
    }

    /// Take the next identity for `key`'s Pending Session. Never reuses a
    /// value, so a stale pick or creation can never match a later pending.
    fn bump_pending_epoch(&mut self, key: &ThreadKey) -> u64 {
        self.next_pending_epoch += 1;
        let epoch = self.next_pending_epoch;
        self.pending_epoch.insert(key.clone(), epoch);
        epoch
    }

    /// The conversation's settings plus the identity of the Pending Session
    /// they describe (issue #910), read under one lock so a replacement cannot
    /// slip between the two. The epoch is `None` for a live session.
    pub(crate) fn settings_with_pending_epoch(
        &self,
        key: &ThreadKey,
    ) -> Option<(SessionSettings, Option<u64>)> {
        let settings = self.settings(key)?;
        let epoch = settings.session_id.is_none().then(|| self.pending_epoch_for(key));
        Some((settings, epoch))
    }

    /// Drop a thread's pending-epoch entry once no claim and no pending need it
    /// (issue #910): the map must not grow with every `/new` the process ever
    /// saw. Removed only when nothing can compare against it — a live claim keeps
    /// its epoch, and a pending keeps the epoch its next claim will take.
    fn prune_pending_epoch(&mut self, key: &ThreadKey) {
        if !self.creating.contains_key(key) && self.pending_for(key).is_none() {
            self.pending_epoch.remove(key);
        }
    }

    /// How many pending-epoch entries the store retains (test-only: proves the
    /// map does not grow with every `/new`).
    #[cfg(test)]
    pub(crate) fn pending_epochs(&self) -> usize {
        self.pending_epoch.len()
    }

    /// Test-only in-memory promote (seed a store without writing a file); the
    /// production write path cannot regress to it.
    #[cfg(test)]
    pub(crate) fn set_active(&mut self, entry: SessionEntry) {
        self.promote(entry)
    }

    /// Mutate the mapped session in place and persist, returning the updated
    /// entry (`None` when `session_id` is not mapped). Used for the
    /// per-session overrides (`/agent`, `/model`, `/think`, `/autoaccept`).
    pub fn update<F>(&mut self, session_id: &str, f: F) -> crate::error::Result<Option<SessionEntry>>
    where
        F: FnOnce(&mut SessionEntry),
    {
        let client_id = self.client_id.clone();
        let Some(entry) = self
            .entries_mut()
            .iter_mut()
            .find(|e| e.client_id == client_id && e.session_id == session_id)
        else {
            return Ok(None);
        };
        f(entry);
        let updated = entry.clone();
        self.write_to_disk()?;
        Ok(Some(updated))
    }

    /// Follow the mapped Sessions' server-reported locations (#433): a
    /// Session can move itself into another directory mid-life (the git
    /// worktree flow), and the directory-routed reads must follow.
    /// `locations` is the server's session list as `(session id, directory)`
    /// pairs; a mapped session whose directory differs follows it in place —
    /// every other field (the thread key, the topic anchors, the per-session
    /// overrides) stays untouched — and the store is persisted once, only
    /// when at least one mapping moved. Returns the pre-follow directory of
    /// every moved session, so the live-card reap keeps the directory the
    /// card was tracked under as its move baseline (#439). Two spellings of
    /// one directory (a trailing slash, repeated separators) are not a move.
    pub fn follow_locations(
        &mut self,
        locations: &[(String, String)],
    ) -> crate::error::Result<Vec<(String, String)>> {
        let listed: std::collections::HashMap<&str, &str> = locations
            .iter()
            .map(|(session_id, directory)| (session_id.as_str(), directory.as_str()))
            .collect();
        let client_id = self.client_id.clone();
        let mut moved = Vec::new();
        for entry in self.entries_mut().iter_mut() {
            if entry.client_id != client_id {
                continue;
            }
            let Some(directory) = listed.get(entry.session_id.as_str()) else {
                continue;
            };
            if same_directory(&entry.directory, directory) {
                continue;
            }
            let previous = std::mem::replace(&mut entry.directory, (*directory).to_string());
            moved.push((entry.session_id.clone(), previous));
        }
        if !moved.is_empty() {
            self.write_to_disk()?;
        }
        Ok(moved)
    }

    /// Mutate the conversation's Pending Session in place and persist,
    /// returning `false` when the thread has no pending (ADR-0041: `/name`
    /// and the per-session settings commands configure a pending). Mirrors
    /// [`SessionStore::update`] for entries.
    pub fn update_pending<F>(&mut self, key: &ThreadKey, f: F) -> crate::error::Result<bool>
    where
        F: FnOnce(&mut PendingEntry),
    {
        let Some(pending) = self.pending_mut().iter_mut().find(|p| &p.thread_key == key) else {
            return Ok(false);
        };
        f(pending);
        self.write_to_disk()?;
        Ok(true)
    }

    /// Snapshot the settings the conversation's next prompt will use (ADR-0041):
    /// the Pending Session's fields when one exists, else the active
    /// [`SessionEntry`]'s. `None` when the thread has neither.
    pub fn settings(&self, key: &ThreadKey) -> Option<SessionSettings> {
        if let Some(p) = self.pending_for(key) {
            return Some(SessionSettings {
                directory: p.directory.clone(),
                session_id: None,
                agent: p.agent.clone(),
                model: p.model.clone(),
                provider: p.provider.clone(),
                variant: p.variant.clone(),
                auto_accept: p.auto_accept,
            });
        }
        self.get_active(key).map(|e| SessionSettings {
            directory: e.directory.clone(),
            session_id: Some(e.session_id.clone()),
            agent: e.agent.clone(),
            model: e.model.clone(),
            // A live session's provider is the thread's own fact, never a local
            // choice: only a creation can take one (issue #910).
            provider: None,
            variant: e.variant.clone(),
            auto_accept: e.auto_accept,
        })
    }

    /// Write a provider+model **pair** onto the thread's Pending Session — and
    /// only there (issue #910). The entry is looked up under the lock the write
    /// takes, so a pending that materialised in between is
    /// [`PendingPairWrite::NoPending`]: the pair must never fall through to the
    /// live entry, whose provider a mirror write cannot change (the thread was
    /// already created with the old one). `clear_variant` clears the `/think`
    /// variant only if the entry still holds it — a concurrent `/think` wins —
    /// and the cleared name is returned for the toast.
    pub(crate) fn set_mirror_pending(
        &mut self,
        key: &ThreadKey,
        expected_epoch: u64,
        expected_model: Option<&str>,
        expected_variant: Option<&str>,
        write: MirrorWrite<'_>,
    ) -> crate::error::Result<PairWrite> {
        // A claimed (materialising) pending cannot take a write: the creation
        // already snapshotted it, so claiming success here would lie.
        if self.creating.contains_key(key) {
            return Ok(PairWrite::Creating);
        }
        let epoch_now = self.pending_epoch_for(key);
        let Some(pending) = self.pending_mut().iter_mut().find(|p| &p.thread_key == key) else {
            // Materialised (or cleared) while the caller awaited its lookup:
            // never fall through to the live entry, whose thread was created
            // with the state the write could not reach.
            return Ok(PairWrite::Moved);
        };
        // The pick classified a **specific** pending: `/new` (or another
        // declaration) has a different identity, and a stale pair must never
        // land on it — even when its model/variant happen to match, which they
        // commonly do on a fresh pending (issue #910).
        if epoch_now != expected_epoch {
            return Ok(PairWrite::Changed);
        }
        if pending.model.as_deref() != expected_model || pending.variant.as_deref() != expected_variant {
            return Ok(PairWrite::Changed);
        }
        let cleared_variant = apply_mirror_write(&mut pending.model, &mut pending.variant, write);
        if let MirrorWrite::Pair { provider, .. } = write {
            pending.provider = Some(provider.to_string());
        }
        self.write_to_disk()?;
        Ok(PairWrite::Applied { cleared_variant })
    }

    /// Write a model/provider mirror change onto a **live** entry (issue #910),
    /// in one lock that also re-verifies the mapping: the entry is found by
    /// `session_id`, and only when it is still the thread's target. A `/switch`
    /// remap, a delete/recreate or a newly declared pending is
    /// [`PairWrite::Moved`] — the write never lands on the new mapping's entry.
    /// The write is a compare-and-set on the state the caller read, so a newer
    /// `/model` or `/think` is [`PairWrite::Changed`], never overwritten.
    pub(crate) fn set_mirror_live(
        &mut self,
        key: &ThreadKey,
        session_id: &str,
        expected_model: Option<&str>,
        expected_variant: Option<&str>,
        write: MirrorWrite<'_>,
    ) -> crate::error::Result<PairWrite> {
        let client_id = self.client_id.clone();
        if self.pending_for(key).is_some() {
            return Ok(PairWrite::Moved);
        }
        let Some(entry) = self
            .entries_mut()
            .iter_mut()
            .find(|e| e.client_id == client_id && e.session_id == session_id)
        else {
            return Ok(PairWrite::Moved);
        };
        if &entry.thread_key != key {
            return Ok(PairWrite::Moved);
        }
        if entry.model.as_deref() != expected_model || entry.variant.as_deref() != expected_variant {
            return Ok(PairWrite::Changed);
        }
        let cleared_variant = apply_mirror_write(&mut entry.model, &mut entry.variant, write);
        self.write_to_disk()?;
        Ok(PairWrite::Applied { cleared_variant })
    }

    /// Claim the thread's Pending Session for materialisation, taking the
    /// snapshot the creation will use **under the same lock** (issue #910). A
    /// pair write arriving in the window before `thread/start` then sees
    /// [`PairWrite::Creating`] instead of claiming a pair the creation cannot
    /// apply. Released by [`Self::activate`] (success) or
    /// [`Self::unclaim_pending_creation`] (failure, so the next message retries).
    pub(crate) fn claim_pending_for_creation(&mut self, key: &ThreadKey) -> PendingClaim {
        if self.creating.contains_key(key) {
            return PendingClaim::Creating;
        }
        let Some(pending) = self.pending_for(key).cloned() else {
            return PendingClaim::None;
        };
        self.next_creation += 1;
        let claim = CreationClaim {
            generation: self.next_creation,
            epoch: self.pending_epoch.get(key).copied().unwrap_or_default(),
        };
        self.creating.insert(key.clone(), claim);
        PendingClaim::Claimed(Box::new(pending), claim.generation)
    }

    /// Release **this generation's** materialisation claim without creating the
    /// session (the pending stays, so the next message retries). A newer claim on
    /// the same thread is left alone (issue #910).
    pub(crate) fn unclaim_pending_creation(&mut self, key: &ThreadKey, generation: u64) {
        if self
            .creating
            .get(key)
            .is_some_and(|claim| claim.generation == generation)
        {
            self.creating.remove(key);
        }
    }

    /// Write one conversation's `/agent` override, touching **only** that field
    /// (issue #910): a whole [`SessionSettings`] snapshot write could restore a
    /// stale model/variant over a switch recorded meanwhile. Targets the pending
    /// when one exists, else the live entry by session id (mapping re-verified).
    pub(crate) fn set_agent(
        &mut self,
        key: &ThreadKey,
        session_id: Option<&str>,
        agent: Option<&str>,
    ) -> crate::error::Result<bool> {
        let agent = agent.map(str::to_string);
        let applied = match session_id {
            None => {
                let Some(pending) = self.pending_mut().iter_mut().find(|p| &p.thread_key == key) else {
                    return Ok(false);
                };
                pending.agent = agent;
                true
            }
            Some(session_id) => {
                let client_id = self.client_id.clone();
                let Some(entry) = self
                    .entries_mut()
                    .iter_mut()
                    .find(|e| e.client_id == client_id && e.session_id == session_id)
                else {
                    return Ok(false);
                };
                if &entry.thread_key != key {
                    return Ok(false);
                }
                entry.agent = agent;
                true
            }
        };
        if applied {
            self.write_to_disk()?;
        }
        Ok(applied)
    }

    /// Write one conversation's `/autoaccept` flag, touching only that field
    /// (issue #910): a whole settings snapshot could restore a stale
    /// model/variant over a recorded switch while the approval sweep it waits
    /// on runs.
    ///
    /// `expected_epoch` identifies a Pending Session target: a `/new` that
    /// replaced it is a refusal (`false`), never a flag written onto the
    /// replacement (`session_id` is `None` exactly when the target is the
    /// pending).
    pub(crate) fn set_auto_accept(
        &mut self,
        key: &ThreadKey,
        session_id: Option<&str>,
        expected_epoch: u64,
        on: bool,
    ) -> crate::error::Result<bool> {
        let applied = match session_id {
            None => {
                if self.pending_epoch_for(key) != expected_epoch {
                    return Ok(false);
                }
                let Some(pending) = self.pending_mut().iter_mut().find(|p| &p.thread_key == key) else {
                    return Ok(false);
                };
                pending.auto_accept = on;
                true
            }
            Some(session_id) => {
                let client_id = self.client_id.clone();
                let Some(entry) = self
                    .entries_mut()
                    .iter_mut()
                    .find(|e| e.client_id == client_id && e.session_id == session_id)
                else {
                    return Ok(false);
                };
                if &entry.thread_key != key {
                    return Ok(false);
                }
                entry.auto_accept = on;
                true
            }
        };
        if applied {
            self.write_to_disk()?;
        }
        Ok(applied)
    }

    /// Write a [`SessionSettings`] snapshot back to the target it came from and
    /// persist: by `session_id` for a real session, else the thread's Pending
    /// Session. A snapshot taken while pending whose session materialises
    /// before this write lands on the new active entry — the settings still
    /// configure the conversation (ADR-0041). `false` when the target is gone.
    ///
    /// **Test-only** since issue #910: every production write is field-scoped
    /// ([`Self::set_agent`], [`Self::set_auto_accept`], the model mirror), so a
    /// stale snapshot can never restore a recorded pair. It stays as the seeding
    /// helper the round-trip tests describe.
    #[cfg(test)]
    pub fn set_settings(&mut self, key: &ThreadKey, settings: SessionSettings) -> crate::error::Result<bool> {
        let client_id = self.client_id.clone();
        let overrides = (
            settings.agent.clone(),
            settings.model.clone(),
            settings.variant.clone(),
            settings.auto_accept,
        );
        // The provider has no entry mirror (issue #910): it is a creation-time
        // pick, so only a Pending Session stores it. A live thread's provider
        // is read from the server and changed by the confirmed resume path.
        let provider = settings.provider.clone();
        let applied = if let Some(id) = settings.session_id.as_deref() {
            match self
                .entries_mut()
                .iter_mut()
                .find(|e| e.client_id == client_id && e.session_id == id)
            {
                Some(e) => {
                    (e.agent, e.model, e.variant, e.auto_accept) = overrides.clone();
                    true
                }
                None => false,
            }
        } else if let Some(p) = self.pending_mut().iter_mut().find(|p| &p.thread_key == key) {
            (p.agent, p.model, p.variant, p.auto_accept) = overrides;
            p.provider = provider;
            true
        } else if let Some(e) = self
            .entries_mut()
            .iter_mut()
            .find(|e| e.client_id == client_id && &e.thread_key == key)
        {
            // The pending materialised between the read and this write: the
            // overrides belong on the session it created. The provider does
            // not — a live session has no such mirror.
            (e.agent, e.model, e.variant, e.auto_accept) = overrides;
            true
        } else {
            false
        };
        if applied {
            self.write_to_disk()?;
        }
        Ok(applied)
    }

    /// Remove a session entry by session ID.
    fn remove(&mut self, session_id: &str) -> Option<SessionEntry> {
        let pos = self
            .entries
            .iter()
            .position(|e| e.client_id == self.client_id && e.session_id == session_id)?;
        Some(self.entries_mut().remove(pos))
    }

    /// Remove a mapping and persist the store.
    pub fn remove_persist(&mut self, session_id: &str) -> crate::error::Result<Option<SessionEntry>> {
        let removed = self.remove(session_id);
        self.write_to_disk()?;
        Ok(removed)
    }

    /// Remove every session entry mapped to a thread, together with any
    /// Pending Session (ADR-0041: forget clears both in the same write).
    fn remove_thread(&mut self, key: &ThreadKey) -> Vec<SessionEntry> {
        let client_id = self.client_id.clone();
        self.creating.remove(key);
        // A removed thread keeps no epoch entry (issue #910): the bump
        // invalidates any in-flight creation or pick, and with neither a claim
        // nor a pending left there is nothing to compare against.
        self.bump_pending_epoch(key);
        self.pending_mut().retain(|p| &p.thread_key != key);
        self.prune_pending_epoch(key);
        let removed: Vec<SessionEntry> = self
            .entries
            .iter()
            .filter(|e| e.client_id == client_id && &e.thread_key == key)
            .cloned()
            .collect();
        self.entries_mut()
            .retain(|e| e.client_id != client_id || &e.thread_key != key);
        removed
    }

    /// Remove every mapping of a thread and persist the store (used by
    /// `/switch forget`).
    pub fn remove_thread_persist(&mut self, key: &ThreadKey) -> crate::error::Result<Vec<SessionEntry>> {
        let removed = self.remove_thread(key);
        self.write_to_disk()?;
        Ok(removed)
    }

    /// Find the ThreadKey for a given session ID.
    pub fn thread_for_session(&self, session_id: &str) -> Option<ThreadKey> {
        self.entries
            .iter()
            .find(|e| e.client_id == self.client_id && e.session_id == session_id)
            .map(|e| e.thread_key.clone())
    }

    /// Find the entry for a session ID (regardless of thread). Used to check
    /// per-session flags like `auto_accept` from the permission poller.
    pub fn entry_for_session(&self, session_id: &str) -> Option<&SessionEntry> {
        self.entries
            .iter()
            .find(|e| e.client_id == self.client_id && e.session_id == session_id)
    }

    /// Directory for a session ID (used to route permission requests to the
    /// correct server instance).
    pub fn directory_for_session(&self, session_id: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|e| e.client_id == self.client_id && e.session_id == session_id)
            .map(|e| e.directory.clone())
    }

    /// Unique session directories, used by the permission poller to check every
    /// instance the bot has created sessions in.
    pub fn directories(&self) -> Vec<String> {
        let mut dirs: Vec<String> = Vec::new();
        for e in self.entries.iter().filter(|e| e.client_id == self.client_id) {
            if !dirs.contains(&e.directory) {
                dirs.push(e.directory.clone());
            }
        }
        dirs
    }

    /// List all sessions in a thread.
    pub fn list_thread(&self, key: &ThreadKey) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|e| e.client_id == self.client_id && &e.thread_key == key)
            .collect()
    }

    /// All known sessions (used by the external-message poller).
    pub fn all_entries(&self) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|e| e.client_id == self.client_id)
            .collect()
    }

    fn write_to_disk(&self) -> crate::error::Result<()> {
        // Atomic, and the failure is returned: this is the one sidecar whose
        // loss is not recoverable, so its writer must not log-and-continue.
        // The blocking form keeps that contract; a caller that must not wait
        // (a card callback) uses `write_to_disk_deferred` (ADR-0089).
        match &self.writer {
            Some(writer) => writer.write_blocking(self.snapshot())?,
            None => {
                let bytes = self.snapshot().to_bytes()?;
                sidecar::write_atomic(&self.path, &bytes)?
            }
        }
        Ok(())
    }

    /// The state to persist, handed over by reference count (ADR-0089): O(1)
    /// under the store lock, so the *callback* never renders JSON — the writer
    /// thread does, right after taking the snapshot, before it touches the disk.
    fn snapshot(&self) -> crate::bridge::store_writer::StoreSnapshot {
        crate::bridge::store_writer::StoreSnapshot::new(Arc::clone(&self.entries), Arc::clone(&self.pending))
    }

    /// Persist this snapshot without waiting for the write: the caller bounds
    /// its own wait on the returned receipt. The write itself still happens, in
    /// order, on the store's single writer thread.
    ///
    /// `Err` means nothing was written and nothing will be: the writer thread
    /// could not be started, so there is no path that would keep the caller
    /// bounded AND durable. The mutation stays in memory and the next blocking
    /// write persists it, so the caller should answer "saved failed, kept for
    /// this process" rather than claim the file has it.
    pub(crate) fn write_to_disk_deferred(
        &self,
    ) -> crate::error::Result<crate::bridge::store_writer::DeferredWrite> {
        match &self.writer {
            Some(writer) => Ok(writer.write_deferred(self.snapshot())),
            None => Err(crate::bridge::store_writer::writer_unavailable().into()),
        }
    }

    /// [`Self::set_pending`] whose durability is reported later (ADR-0089):
    /// the mutation is in memory now, the receipt settles when the file does.
    ///
    /// `Ok(None)` means the scans that apply the mutation ran past `deadline`:
    /// they are memory-only but proportional to the store, so the change is
    /// ROLLED BACK and the click must refuse — a callback never commits a
    /// pending it can no longer confirm. `Err` means the mutation stuck and
    /// only the save failed.
    pub(crate) fn set_pending_deferred(
        &mut self,
        pending: PendingEntry,
        deadline: tokio::time::Instant,
    ) -> crate::error::Result<Option<crate::bridge::store_writer::DeferredWrite>> {
        // Replace in place and remember what was displaced, so an overrun can
        // be undone without holding a second handle to the vector up front —
        // that would force the copy-on-write below to copy every time.
        let key = pending.thread_key.clone();
        let index = self
            .pending
            .iter()
            .position(|p| p.thread_key == pending.thread_key);
        let displaced = index.map(|index| (index, self.pending_mut().remove(index)));
        self.pending_mut().push(pending);
        if tokio::time::Instant::now() >= deadline {
            self.pending_mut().pop();
            if let Some((index, displaced)) = displaced {
                self.pending_mut().insert(index, displaced);
            }
            return Ok(None);
        }
        // The replacement is visible in memory from here on (a failed save still
        // leaves it), so it takes a new identity: a pick or creation classified
        // against the displaced pending must refuse rather than land on it.
        self.bump_pending_epoch(&key);
        self.write_to_disk_deferred().map(Some)
    }

    /// Test-only durable write; production reaches it only through
    /// activate/update/remove_persist/remove_thread_persist.
    #[cfg(test)]
    pub(crate) fn persist(&self) -> crate::error::Result<()> {
        self.write_to_disk()
    }

    /// Test-only: the gate the writer thread stops at before each write, so a
    /// test can keep a write in flight (`None` without a writer thread).
    #[cfg(test)]
    pub(crate) fn pause_writes(&self) -> Option<crate::bridge::store_writer::WritePause> {
        self.writer.as_ref().map(|writer| writer.pause.clone())
    }

    /// Test-only: how many writes actually reached the disk, so a test can pin
    /// that a superseded snapshot is never written (ADR-0089).
    #[cfg(test)]
    pub(crate) fn writes(&self) -> usize {
        self.writer
            .as_ref()
            .map(|writer| writer.writes.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Test-only: this store without its writer thread — the degraded state
    /// `new` logs about, where a deferred save cannot stay both bounded and
    /// durable.
    #[cfg(test)]
    pub(crate) fn without_writer(mut self) -> Self {
        self.writer = None;
        self
    }
}

/// Parse either historical shape: the pre-ADR-0041 bare array, or the object
/// with `entries`/`pending`. A corrupt body fails open with a warning, exactly
/// as the store did before ADR-0091.
fn parse_store(raw: &str, path: &std::path::Path) -> Option<(Vec<SessionEntry>, Vec<PendingEntry>)> {
    match serde_json::from_str::<LoadedStore>(raw) {
        Ok(loaded) => Some(loaded.into_parts()),
        Err(err) => {
            tracing::warn!(
                "could not parse {} ({err}); starting with an empty mapping",
                path.display()
            );
            None
        }
    }
}

/// Whether two directory spellings name the same path, without touching the
/// filesystem: an exact match, or the same path components (a trailing slash,
/// doubled separators). A spelling-only difference must not report a move —
/// that would rewrite the store, log a move and stamp 「会话已迁移」 on the
/// card every pass for a Session that never moved (#433).
fn same_directory(a: &str, b: &str) -> bool {
    a == b || std::path::Path::new(a) == std::path::Path::new(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A deadline far enough away that a test's mutation is inside it.
    fn far() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(30)
    }

    fn make_entry(chat_id: &str, root_id: &str, session_id: &str, dir: &str) -> SessionEntry {
        SessionEntry {
            client_id: crate::config::DEFAULT_CLIENT_ID.into(),
            thread_key: ThreadKey::new(chat_id.into(), root_id.into()),
            session_id: session_id.into(),
            directory: dir.into(),
            agent: None,
            model: None,
            auto_accept: false,
            topic_anchor: None,
            topic_root: None,
            variant: None,
        }
    }

    fn make_pending(chat_id: &str, root_id: &str, dir: &str) -> PendingEntry {
        PendingEntry::new(ThreadKey::new(chat_id.into(), root_id.into()), dir)
    }

    #[test]
    fn new_store_creates_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let store = SessionStore::new(path.clone()).unwrap();
        assert!(store.entries.is_empty());
    }

    #[test]
    fn set_and_get_active() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();

        let entry = make_entry("chat1", "root1", "ses_abc", "/tmp/proj");
        store.set_active(entry);

        let found = store.get_active(&ThreadKey::new("chat1".into(), "root1".into()));
        assert!(found.is_some());
        assert_eq!(found.unwrap().session_id, "ses_abc");
    }

    #[test]
    fn multiple_sessions_per_thread() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();

        let key = ThreadKey::new("chat1".into(), "root1".into());
        store.set_active(make_entry("chat1", "root1", "ses_1", "/tmp/a"));
        store.set_active(make_entry("chat1", "root1", "ses_2", "/tmp/b"));

        let list = store.list_thread(&key);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn persists_and_reloads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        {
            let mut store = SessionStore::new(path.clone()).unwrap();
            store.set_active(make_entry("chat1", "root1", "ses_x", "/tmp/x"));
            store.persist().unwrap();
        }

        let store2 = SessionStore::new(path).unwrap();
        let found = store2.get_active(&ThreadKey::new("chat1".into(), "root1".into()));
        assert!(found.is_some());
        assert_eq!(found.unwrap().directory, "/tmp/x");
    }

    /// Following the server's locations updates only the moved sessions and
    /// reports their pre-follow directory; a spelling-only difference and an
    /// unmapped session change nothing (#433).
    #[test]
    fn follow_locations_moves_only_real_changes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();
        store.set_active(make_entry("chat1", "root1", "ses_a", "/work/proj"));
        store.set_active(make_entry("chat2", "root2", "ses_b", "/work/other"));

        let locations = |a: &str, b: &str| {
            vec![
                ("ses_a".to_string(), a.to_string()),
                ("ses_b".to_string(), b.to_string()),
                ("ses_unmapped".to_string(), "/work/new".to_string()),
            ]
        };

        // The trailing slash is the same directory: no move, no write.
        let moved = store
            .follow_locations(&locations("/work/proj/", "/work/other"))
            .unwrap();
        assert!(moved.is_empty(), "a spelling difference is not a move: {moved:?}");

        // A different directory is: the previous one is reported and the
        // mapping follows.
        let moved = store
            .follow_locations(&locations("/work/other", "/work/.worktrees/x"))
            .unwrap();
        assert_eq!(moved.len(), 2, "both real moves reported: {moved:?}");
        assert_eq!(
            store.directory_for_session("ses_a").as_deref(),
            Some("/work/other")
        );
        assert_eq!(
            store.directory_for_session("ses_b").as_deref(),
            Some("/work/.worktrees/x")
        );
        let reloaded = SessionStore::new(dir.path().join("sessions.json")).unwrap();
        assert_eq!(
            reloaded.directory_for_session("ses_b").as_deref(),
            Some("/work/.worktrees/x"),
            "the move is persisted"
        );
    }

    #[test]
    fn remove_by_session_id() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();

        let key = ThreadKey::new("chat1".into(), "root1".into());
        store.set_active(make_entry("chat1", "root1", "ses_rm", "/tmp/y"));

        let removed = store.remove("ses_rm");
        assert!(removed.is_some());
        assert!(store.get_active(&key).is_none());
    }

    #[test]
    fn remove_thread_removes_only_that_thread() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();

        let key_a = ThreadKey::new("chat1".into(), "chat1".into());
        let key_b = ThreadKey::new("chat2".into(), "chat2".into());
        store.set_active(make_entry("chat1", "chat1", "ses_1", "/tmp/a"));
        store.set_active(make_entry("chat2", "chat2", "ses_2", "/tmp/b"));

        let removed = store.remove_thread(&key_a);
        assert_eq!(removed.len(), 1);
        assert!(store.get_active(&key_a).is_none());
        assert!(store.get_active(&key_b).is_some());
    }

    #[test]
    fn legacy_json_with_name_field_loads() {
        // Pre-ADR-0007 sessions.json entries carried a `name`; serde ignores the
        // unknown field, so the mapping and directory survive the migration.
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        std::fs::write(
            &path,
            r#"[{
                "thread_key": { "chat_id": "oc_1", "thread_id": "oc_1" },
                "session_id": "ses_legacy",
                "name": "old-name",
                "directory": "/tmp/legacy",
                "auto_accept": false
            }]"#,
        )
        .unwrap();
        let store = SessionStore::new(path).unwrap();
        let key = ThreadKey::new("oc_1".into(), "oc_1".into());
        let entry = store.get_active(&key).expect("legacy entry loads");
        assert_eq!(entry.session_id, "ses_legacy");
        assert_eq!(entry.directory, "/tmp/legacy");
    }

    #[test]
    fn activate_promotes_and_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();

        store
            .activate(make_entry("chat1", "root1", "ses_1", "/tmp/a"))
            .unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_2", "/tmp/b"))
            .unwrap();
        assert_eq!(
            store
                .get_active(&ThreadKey::new("chat1".into(), "root1".into()))
                .unwrap()
                .session_id,
            "ses_2"
        );
        // Re-activating an existing session promotes it without duplicating.
        store
            .activate(make_entry("chat1", "root1", "ses_1", "/tmp/a"))
            .unwrap();
        assert_eq!(
            store
                .list_thread(&ThreadKey::new("chat1".into(), "root1".into()))
                .len(),
            2
        );
        assert_eq!(
            store
                .get_active(&ThreadKey::new("chat1".into(), "root1".into()))
                .unwrap()
                .session_id,
            "ses_1"
        );
        // The promotion is durable.
        let reloaded = SessionStore::new(path).unwrap();
        assert_eq!(
            reloaded
                .get_active(&ThreadKey::new("chat1".into(), "root1".into()))
                .unwrap()
                .session_id,
            "ses_1"
        );
    }

    #[test]
    fn update_mutates_and_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_upd", "/tmp/a"))
            .unwrap();

        let updated = store
            .update("ses_upd", |e| {
                e.model = Some("opencode-go/deepseek".into());
                e.auto_accept = true;
            })
            .unwrap()
            .expect("mapped session");
        assert_eq!(updated.model.as_deref(), Some("opencode-go/deepseek"));
        assert!(updated.auto_accept);

        let reloaded = SessionStore::new(path).unwrap();
        let entry = reloaded
            .get_active(&ThreadKey::new("chat1".into(), "root1".into()))
            .unwrap();
        assert_eq!(entry.model.as_deref(), Some("opencode-go/deepseek"));
        assert!(entry.auto_accept);
    }

    #[test]
    fn update_unknown_session_returns_none() {
        let dir = tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().join("sessions.json")).unwrap();
        assert!(
            store
                .update("ses_missing", |e| e.auto_accept = true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn remove_persist_removes_and_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_rm", "/tmp/y"))
            .unwrap();

        assert!(store.remove_persist("ses_rm").unwrap().is_some());
        assert!(store.remove_persist("ses_rm").unwrap().is_none());
        let reloaded = SessionStore::new(path).unwrap();
        assert!(
            reloaded
                .get_active(&ThreadKey::new("chat1".into(), "root1".into()))
                .is_none()
        );
    }

    #[test]
    fn remove_thread_persist_removes_only_that_thread() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();
        let key_a = ThreadKey::new("chat1".into(), "chat1".into());
        let key_b = ThreadKey::new("chat2".into(), "chat2".into());
        store
            .activate(make_entry("chat1", "chat1", "ses_1", "/tmp/a"))
            .unwrap();
        store
            .activate(make_entry("chat2", "chat2", "ses_2", "/tmp/b"))
            .unwrap();

        let removed = store.remove_thread_persist(&key_a).unwrap();
        assert_eq!(removed.len(), 1);

        let reloaded = SessionStore::new(path).unwrap();
        assert!(reloaded.get_active(&key_a).is_none());
        assert!(reloaded.get_active(&key_b).is_some());
    }

    #[test]
    fn persist_failure_is_returned() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();
        // Occupy the store path with a directory so the write fails.
        std::fs::create_dir(&path).unwrap();

        let err = store
            .activate(make_entry("chat1", "root1", "ses_fail", "/tmp/f"))
            .unwrap_err();
        assert!(matches!(err, crate::error::BridgeError::Io(_)));
    }

    /// The mapping is written atomically: a failed write leaves the previous
    /// file intact rather than a half-written mapping.
    #[test]
    fn a_failed_mapping_write_leaves_the_previous_file_intact() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_old", "/tmp/old"))
            .unwrap();

        // Occupy the temp path so the atomic replace fails before its rename.
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        let err = store
            .activate(make_entry("chat1", "root1", "ses_new", "/tmp/new"))
            .unwrap_err();
        assert!(matches!(err, crate::error::BridgeError::Io(_)));

        let reloaded = SessionStore::new(path).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        assert_eq!(
            reloaded.get_active(&key).unwrap().session_id,
            "ses_old",
            "the previous mapping survives the failed write"
        );
    }

    #[test]
    fn legacy_bare_array_loads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        std::fs::write(
            &path,
            r#"[{
                "thread_key": { "chat_id": "chat1", "thread_id": "root1" },
                "session_id": "ses_old",
                "directory": "/tmp/old",
                "auto_accept": false
            }]"#,
        )
        .unwrap();

        let store = SessionStore::new(path).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        assert!(store.pending.is_empty());
        assert_eq!(store.get_active(&key).unwrap().session_id, "ses_old");
    }

    #[test]
    fn new_format_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let key = ThreadKey::new("chat1".into(), "root1".into());
        let mut store = SessionStore::new(path.clone()).unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_old", "/tmp/old"))
            .unwrap();
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/new"))
            .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(value.get("entries").and_then(|v| v.as_array()).is_some());
        assert!(value.get("pending").and_then(|v| v.as_array()).is_some());

        let reloaded = SessionStore::new(path).unwrap();
        assert_eq!(reloaded.pending_for(&key).unwrap().directory, "/tmp/new");
        assert_eq!(reloaded.list_thread(&key).len(), 1);
    }

    #[test]
    fn pending_survives_reload_and_supersedes_active() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let key = ThreadKey::new("chat1".into(), "root1".into());
        {
            let mut store = SessionStore::new(path.clone()).unwrap();
            store
                .activate(make_entry("chat1", "root1", "ses_old", "/tmp/old"))
                .unwrap();
            store
                .set_pending(make_pending("chat1", "root1", "/tmp/new"))
                .unwrap();

            assert!(
                store.get_active(&key).is_none(),
                "a pending supersedes the active session"
            );
            assert_eq!(
                store.list_thread(&key).len(),
                1,
                "the old session stays mapped and switchable"
            );
            assert_eq!(store.current_directory(&key).as_deref(), Some("/tmp/new"));
        }

        let reloaded = SessionStore::new(path).unwrap();
        let pending = reloaded.pending_for(&key).expect("pending survives reload");
        assert_eq!(pending.directory, "/tmp/new");
        assert!(reloaded.get_active(&key).is_none());
        assert_eq!(reloaded.list_thread(&key).len(), 1);
        assert_eq!(reloaded.current_directory(&key).as_deref(), Some("/tmp/new"));
    }

    #[test]
    fn set_pending_replaces_the_threads_pending() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let key = ThreadKey::new("chat1".into(), "root1".into());
        let mut store = SessionStore::new(path).unwrap();

        store
            .set_pending(make_pending("chat1", "root1", "/tmp/a"))
            .unwrap();
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/b"))
            .unwrap();

        assert_eq!(store.pending.len(), 1, "at most one pending per thread");
        assert_eq!(store.pending_for(&key).unwrap().directory, "/tmp/b");
    }

    #[test]
    fn clear_pending_restores_active_and_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let key = ThreadKey::new("chat1".into(), "root1".into());
        let mut store = SessionStore::new(path.clone()).unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_old", "/tmp/old"))
            .unwrap();
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/new"))
            .unwrap();

        store.clear_pending(&key).unwrap();
        assert!(store.pending_for(&key).is_none());
        assert_eq!(store.get_active(&key).unwrap().session_id, "ses_old");

        let reloaded = SessionStore::new(path).unwrap();
        assert!(reloaded.pending_for(&key).is_none());
        assert_eq!(reloaded.get_active(&key).unwrap().session_id, "ses_old");
    }

    #[test]
    fn activation_resolves_the_pending_in_one_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let key = ThreadKey::new("chat1".into(), "root1".into());
        let mut store = SessionStore::new(path.clone()).unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_old", "/tmp/old"))
            .unwrap();
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/new"))
            .unwrap();

        // Materialisation and adoption both reach this operation.
        store
            .activate(make_entry("chat1", "root1", "ses_new", "/tmp/new"))
            .unwrap();

        assert!(store.pending_for(&key).is_none());
        assert_eq!(store.get_active(&key).unwrap().session_id, "ses_new");
        assert_eq!(
            store.list_thread(&key).len(),
            2,
            "the replaced session stays mapped"
        );

        let reloaded = SessionStore::new(path).unwrap();
        assert!(reloaded.pending_for(&key).is_none());
        assert_eq!(reloaded.get_active(&key).unwrap().session_id, "ses_new");
        assert_eq!(reloaded.list_thread(&key).len(), 2);
    }

    #[test]
    fn remove_thread_persist_clears_pending() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let key = ThreadKey::new("chat1".into(), "root1".into());
        let mut store = SessionStore::new(path.clone()).unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_old", "/tmp/old"))
            .unwrap();
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/new"))
            .unwrap();

        store.remove_thread_persist(&key).unwrap();
        assert!(store.pending_for(&key).is_none());
        assert!(store.get_active(&key).is_none());

        let reloaded = SessionStore::new(path).unwrap();
        assert!(reloaded.pending_for(&key).is_none());
        assert!(reloaded.list_thread(&key).is_empty());
    }

    #[test]
    fn current_directory_prefers_pending_only_for_its_thread() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_1", "/tmp/one"))
            .unwrap();
        store
            .activate(make_entry("chat2", "root2", "ses_2", "/tmp/two"))
            .unwrap();
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/pending"))
            .unwrap();

        let key = ThreadKey::new("chat1".into(), "root1".into());
        let other = ThreadKey::new("chat2".into(), "root2".into());
        assert_eq!(store.current_directory(&key).as_deref(), Some("/tmp/pending"));
        assert_eq!(store.current_directory(&other).as_deref(), Some("/tmp/two"));
        assert_eq!(
            store.current_directory(&ThreadKey::new("chat3".into(), "root3".into())),
            None
        );
    }

    /// The settings snapshot is pending-first, and a write goes back to the
    /// target the snapshot came from — the pending, not the superseded entry
    /// (ADR-0041).
    #[test]
    fn settings_round_trip_prefers_the_pending() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        let mut entry = make_entry("chat1", "root1", "ses_1", "/tmp/one");
        entry.model = Some("old/model".into());
        store.activate(entry).unwrap();
        let mut pending = make_pending("chat1", "root1", "/tmp/pending");
        pending.agent = Some("build".into());
        store.set_pending(pending).unwrap();

        let taken = store.settings(&key).expect("the pending is the target");
        assert_eq!(taken.directory, "/tmp/pending");
        assert_eq!(taken.session_id, None, "a pending has no session id");
        assert_eq!(taken.agent.as_deref(), Some("build"));

        let mut taken = taken;
        taken.model = Some("new/model".into());
        taken.auto_accept = true;
        assert!(store.set_settings(&key, taken).unwrap());
        assert_eq!(
            store.pending_for(&key).and_then(|p| p.model.as_deref()),
            Some("new/model"),
            "the write landed on the pending"
        );
        assert_eq!(
            store.entry_for_session("ses_1").and_then(|e| e.model.as_deref()),
            Some("old/model"),
            "the superseded entry is untouched"
        );
    }

    /// Issue #910: the pending mirror write is a pending-only compare-and-set. It
    /// never falls through to the live entry once the session materialised, a
    /// claimed (materialising) pending refuses with `Creating`, and a newer pick
    /// on the pending is `Changed` rather than overwritten. A replacement
    /// pending declared during a creation is versioned by the epoch, so the
    /// creation's `activate_created` leaves it alone.
    #[test]
    fn set_pending_mirror_never_falls_through_and_cas_guards_the_pending() {
        let dir = tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().join("sessions.json")).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        let mut entry = make_entry("chat1", "root1", "ses_1", "/tmp/one");
        entry.model = Some("old/model".into());
        store.activate(entry).unwrap();
        let mut pending = make_pending("chat1", "root1", "/tmp/pending");
        pending.variant = Some("high".into());
        store.set_pending(pending).unwrap();

        let epoch = store.pending_epoch_for(&key);
        // A newer pick landed since the caller's snapshot: nothing is written.
        assert_eq!(
            store
                .set_mirror_pending(
                    &key,
                    epoch,
                    Some("stale"),
                    None,
                    MirrorWrite::Pair {
                        model: "deepseek/pro",
                        provider: "deepseek",
                        clear_variant: None
                    },
                )
                .unwrap(),
            PairWrite::Changed
        );
        // While the pending exists, the pair lands and the CAS clear applies.
        let written = store
            .set_mirror_pending(
                &key,
                epoch,
                None,
                Some("high"),
                MirrorWrite::Pair {
                    model: "deepseek/pro",
                    provider: "deepseek",
                    clear_variant: Some("high"),
                },
            )
            .unwrap();
        assert_eq!(
            written,
            PairWrite::Applied {
                cleared_variant: Some("high".into())
            }
        );
        let pending = store.pending_for(&key).unwrap();
        assert_eq!(pending.model.as_deref(), Some("deepseek/pro"));
        assert_eq!(pending.provider.as_deref(), Some("deepseek"));

        // A claimed (materialising) pending refuses: the creation snapshotted it.
        match store.claim_pending_for_creation(&key) {
            PendingClaim::Claimed(pending, _) => {
                assert_eq!(pending.model.as_deref(), Some("deepseek/pro"))
            }
            _ => panic!("the pending must be claimable"),
        }
        assert_eq!(
            store
                .set_mirror_pending(
                    &key,
                    epoch,
                    Some("deepseek/pro"),
                    None,
                    MirrorWrite::Model {
                        model: "openai/x",
                        clear_variant: None
                    },
                )
                .unwrap(),
            PairWrite::Creating
        );
        let claim = store
            .creating
            .get(&key)
            .map(|claim| claim.generation)
            .expect("claimed");
        store.unclaim_pending_creation(&key, claim);

        // Materialised (pending cleared, live entry active): no fallthrough.
        store.clear_pending(&key).unwrap();
        assert_eq!(
            store
                .set_mirror_pending(
                    &key,
                    epoch,
                    None,
                    None,
                    MirrorWrite::Model {
                        model: "deepseek/pro",
                        clear_variant: None
                    },
                )
                .unwrap(),
            PairWrite::Moved
        );
        assert_eq!(
            store.entry_for_session("ses_1").and_then(|e| e.model.as_deref()),
            Some("old/model"),
            "the live entry keeps the model its thread was created with"
        );
    }

    /// Issue #910 (item 4): a materialisation claim is generation- and
    /// epoch-scoped. A replacement pending declared while the first creation runs
    /// is **not** resolved by that creation's activation, and the pair saved on
    /// it survives.
    #[test]
    fn a_replacement_pending_survives_the_older_creations_activation() {
        let dir = tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().join("sessions.json")).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        let first = make_pending("chat1", "root1", "/tmp/first");
        store.set_pending(first).unwrap();
        let PendingClaim::Claimed(claimed, generation) = store.claim_pending_for_creation(&key) else {
            panic!("the pending must be claimable");
        };
        assert_eq!(claimed.directory, "/tmp/first");

        // A replacement pending is declared while the creation is in flight.
        let mut replacement = make_pending("chat1", "root1", "/tmp/second");
        replacement.model = Some("deepseek/pro".into());
        replacement.provider = Some("deepseek".into());
        store.set_pending(replacement).unwrap();

        // The older creation finishes and activates: the replacement survives.
        let mut entry = make_entry("chat1", "root1", "ses_created", "/tmp/first");
        entry.client_id = store.client_id.clone();
        store.activate_created(entry, generation).unwrap();
        let replacement = store
            .pending_for(&key)
            .expect("the replacement pending is not resolved by the older creation");
        assert_eq!(replacement.directory, "/tmp/second");
        assert_eq!(
            replacement.model.as_deref(),
            Some("deepseek/pro"),
            "the pair saved on the replacement is intact"
        );
        // And the replacement can be claimed afresh (the older claim is gone).
        assert!(matches!(
            store.claim_pending_for_creation(&key),
            PendingClaim::Claimed(_, _)
        ));
    }

    /// Issue #910 (extra finding): releasing a materialisation claim is scoped to
    /// its generation — an older creation's failure must not release a newer
    /// claim on the same thread.
    #[test]
    fn unclaim_pending_creation_is_generation_scoped() {
        let dir = tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().join("sessions.json")).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/first"))
            .unwrap();
        let PendingClaim::Claimed(_, first) = store.claim_pending_for_creation(&key) else {
            panic!("claimable");
        };
        // The pending is cleared (removing the claim) and replaced, then claimed
        // again: a new generation.
        store.clear_pending(&key).unwrap();
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/second"))
            .unwrap();
        let PendingClaim::Claimed(_, second) = store.claim_pending_for_creation(&key) else {
            panic!("claimable again");
        };
        assert_ne!(first, second);

        // The older creation's failure releases nothing.
        store.unclaim_pending_creation(&key, first);
        assert!(
            matches!(store.claim_pending_for_creation(&key), PendingClaim::Creating),
            "the newer claim survives the older failure"
        );
        store.unclaim_pending_creation(&key, second);
        assert!(matches!(
            store.claim_pending_for_creation(&key),
            PendingClaim::Claimed(_, _)
        ));
    }

    /// Issue #910 (extra finding): the pending-epoch map is pruned once a thread
    /// has neither a pending nor a claim, so it does not grow with every /new.
    #[test]
    fn pending_epochs_are_pruned() {
        let dir = tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().join("sessions.json")).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/first"))
            .unwrap();
        assert_eq!(store.pending_epochs(), 1);
        let PendingClaim::Claimed(_, generation) = store.claim_pending_for_creation(&key) else {
            panic!("claimable");
        };
        let mut entry = make_entry("chat1", "root1", "ses_created", "/tmp/first");
        entry.client_id = store.client_id.clone();
        store.activate_created(entry, generation).unwrap();
        assert_eq!(store.pending_epochs(), 0, "no pending and no claim: pruned");

        // A replacement declared mid-creation keeps its epoch until it resolves.
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/second"))
            .unwrap();
        assert_eq!(store.pending_epochs(), 1);
        store.clear_pending(&key).unwrap();
        assert_eq!(store.pending_epochs(), 0);

        // Removing the thread (`/switch forget`) prunes it too, and the next
        // declaration takes a fresh identity — never the removed thread's.
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/third"))
            .unwrap();
        let removed_epoch = store.pending_epoch_for(&key);
        store.remove_thread_persist(&key).unwrap();
        assert_eq!(store.pending_epochs(), 0, "a removed thread keeps no epoch entry");
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/fourth"))
            .unwrap();
        assert!(
            store.pending_epoch_for(&key) > removed_epoch,
            "the identity is never reused"
        );
        assert_eq!(store.pending_epochs(), 1);

        // `activate` (an adoption or a `/switch`) prunes it too once the pending
        // is consumed and no claim is left.
        let mut adopted = make_entry("chat1", "root1", "ses_adopted", "/tmp/fourth");
        adopted.client_id = store.client_id.clone();
        store.activate(adopted).unwrap();
        assert_eq!(
            store.pending_epochs(),
            0,
            "an activated thread keeps no epoch entry"
        );
        assert_eq!(store.pending_epoch_for(&key), 0);
        assert!(store.pending_for(&key).is_none(), "the pending was consumed");
    }

    /// Issue #910 (extra finding): a `/agent` write touches only its own field —
    /// never the model/variant a recorded switch put there.
    #[test]
    fn set_agent_touches_only_its_field() {
        let dir = tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().join("sessions.json")).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        let mut entry = make_entry("chat1", "root1", "ses_1", "/tmp/one");
        entry.model = Some("deepseek/pro".into());
        entry.variant = Some("low".into());
        entry.auto_accept = true;
        store.activate(entry).unwrap();

        assert!(store.set_agent(&key, Some("ses_1"), Some("build")).unwrap());
        let entry = store.entry_for_session("ses_1").unwrap();
        assert_eq!(entry.agent.as_deref(), Some("build"));
        assert_eq!(entry.model.as_deref(), Some("deepseek/pro"));
        assert_eq!(entry.variant.as_deref(), Some("low"));
        assert!(entry.auto_accept);

        // The pending target works the same way (it has no session id).
        store.clear_pending(&key).unwrap();
        let mut pending = make_pending("chat1", "root1", "/tmp/pending");
        pending.model = Some("deepseek/pro".into());
        store.set_pending(pending).unwrap();
        assert!(store.set_agent(&key, None, Some("plan")).unwrap());
        let pending = store.pending_for(&key).unwrap();
        assert_eq!(pending.agent.as_deref(), Some("plan"));
        assert_eq!(pending.model.as_deref(), Some("deepseek/pro"));
    }

    /// Without a pending, the snapshot comes from the active entry and writes
    /// back by session id.
    #[test]
    fn settings_round_trip_uses_the_active_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        store
            .activate(make_entry("chat1", "root1", "ses_1", "/tmp/one"))
            .unwrap();

        let mut taken = store.settings(&key).expect("the active entry is the target");
        assert_eq!(taken.session_id.as_deref(), Some("ses_1"));
        taken.variant = Some("high".into());
        assert!(store.set_settings(&key, taken).unwrap());
        assert_eq!(
            store
                .entry_for_session("ses_1")
                .and_then(|e| e.variant.as_deref()),
            Some("high")
        );

        assert!(
            store
                .settings(&ThreadKey::new("chat9".into(), "root9".into()))
                .is_none()
        );
    }

    /// Re-adopting an already-mapped session keeps its per-session overrides:
    /// they belong to the session, not to one mapping. The mapping fields the
    /// adoption flow owns (thread, directory, agent, anchors) are overwritten.
    #[test]
    fn re_activation_keeps_per_session_overrides() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();

        let mut first = make_entry("chat1", "root1", "ses_abc", "/tmp/old");
        first.model = Some("provider/model-a".into());
        first.variant = Some("high".into());
        first.auto_accept = true;
        store.set_active(first);

        // A fresh entry for the same session under another mapping: the flow
        // only sets the mapping fields.
        let mut second = make_entry("chat2", "root2", "ses_abc", "/tmp/new");
        second.agent = Some("build".into());
        store.set_active(second);

        let found = store
            .get_active(&ThreadKey::new("chat2".into(), "root2".into()))
            .expect("re-activated entry exists");
        assert_eq!(found.directory, "/tmp/new", "flow-owned directory updates");
        assert_eq!(found.agent.as_deref(), Some("build"), "flow-owned agent updates");
        assert_eq!(found.model.as_deref(), Some("provider/model-a"));
        assert_eq!(found.variant.as_deref(), Some("high"));
        assert!(found.auto_accept, "auto-accept survives the re-adoption");
        assert!(
            store
                .get_active(&ThreadKey::new("chat1".into(), "root1".into()))
                .is_none(),
            "the old mapping is replaced, not stacked"
        );
    }

    /// An override the activating entry sets explicitly wins over the carried
    /// one; only fields left at their defaults are inherited from the session.
    #[test]
    fn explicit_overrides_win_over_carried_ones() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();

        let mut first = make_entry("chat1", "root1", "ses_abc", "/tmp/old");
        first.model = Some("provider/model-a".into());
        first.variant = Some("high".into());
        store.set_active(first);

        let mut second = make_entry("chat2", "root2", "ses_abc", "/tmp/new");
        second.model = Some("provider/model-b".into());
        store.set_active(second);

        let found = store.entry_for_session("ses_abc").unwrap();
        assert_eq!(
            found.model.as_deref(),
            Some("provider/model-b"),
            "the explicitly set model wins"
        );
        assert_eq!(
            found.variant.as_deref(),
            Some("high"),
            "the unset variant is carried"
        );
    }

    /// A session never seen before still starts from defaults.
    #[test]
    fn first_activation_starts_from_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();
        store
            .activate(make_entry("chat1", "root1", "ses_new", "/tmp/proj"))
            .unwrap();

        let found = store.entry_for_session("ses_new").expect("entry exists");
        assert!(!found.auto_accept);
        assert!(found.model.is_none());
        assert!(found.variant.is_none());
        assert!(found.agent.is_none());
    }

    /// ADR-0089: a deferred write applies the mutation to memory at once and
    /// settles once the file lands.
    #[tokio::test]
    async fn deferred_pending_write_settles_and_persists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());

        let receipt = store
            .set_pending_deferred(make_pending("chat1", "root1", "/tmp/wt"), far())
            .unwrap()
            .expect("the mutation is inside the deadline");
        assert_eq!(
            store.pending_for(&key).unwrap().directory,
            "/tmp/wt",
            "the mutation is visible before the write lands"
        );
        receipt.settle().await.unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("/tmp/wt"), "the file carries the pending: {text}");
    }

    /// Two deferred writes land in commit order on the one writer thread, so
    /// the later snapshot is never overwritten by the earlier one.
    #[tokio::test]
    async fn deferred_writes_land_in_commit_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();

        let first = store
            .set_pending_deferred(make_pending("chat1", "root1", "/tmp/first"), far())
            .unwrap()
            .expect("first within the deadline");
        let second = store
            .set_pending_deferred(make_pending("chat1", "root1", "/tmp/second"), far())
            .unwrap()
            .expect("second within the deadline");
        first.settle().await.unwrap();
        second.settle().await.unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("/tmp/second"), "{text}");
        assert!(!text.contains("/tmp/first"), "the stale snapshot lost: {text}");
    }

    /// A failed deferred write settles with the error instead of disappearing:
    /// the store's loss is not recoverable, so the caller can say so.
    #[tokio::test]
    async fn a_failed_deferred_write_settles_with_the_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();
        // Make the target unwritable: a directory cannot be replaced by the
        // atomic writer's rename.
        std::fs::create_dir(&path).unwrap();

        let receipt = store
            .set_pending_deferred(make_pending("chat1", "root1", "/tmp/wt"), far())
            .unwrap()
            .expect("the mutation is inside the deadline");
        assert!(receipt.settle().await.is_err());
    }

    /// Handing a snapshot over never blocks, so a newer one replaces an older
    /// one still waiting for the writer — and BOTH callers settle on the write
    /// that lands (their mutation is in it). This is what keeps a click bounded
    /// when the disk stalls.
    #[tokio::test]
    async fn a_superseded_snapshot_settles_on_the_newer_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();
        let pause = store.pause_writes().unwrap();
        pause.hold();

        let first = store
            .set_pending_deferred(make_pending("chat1", "root1", "/tmp/first"), far())
            .unwrap()
            .expect("first within the deadline");
        let second = store
            .set_pending_deferred(make_pending("chat1", "root1", "/tmp/second"), far())
            .unwrap()
            .expect("second within the deadline");
        // Neither can have landed: the writer is held before its first write.
        assert!(!path.exists(), "no write while the writer is held");
        pause.release();

        first.settle().await.unwrap();
        second.settle().await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("/tmp/second"), "{text}");
        assert!(!text.contains("/tmp/first"), "the stale snapshot lost: {text}");
        assert_eq!(
            store.writes(),
            1,
            "the superseded snapshot was replaced, not queued behind it"
        );
    }

    /// ADR-0089: if applying the mutation itself runs past the deadline, it is
    /// rolled back — a click never commits a pending whose answer is gone.
    #[tokio::test]
    async fn an_overrun_declaration_rolls_the_mutation_back() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap();
        let key = ThreadKey::new("chat1".into(), "root1".into());
        store
            .set_pending(make_pending("chat1", "root1", "/tmp/before"))
            .unwrap();

        let past = tokio::time::Instant::now() + std::time::Duration::from_millis(1);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let declared = store
            .set_pending_deferred(make_pending("chat1", "root1", "/tmp/after"), past)
            .unwrap();
        assert!(declared.is_none(), "an overrun declaration enqueues nothing");
        assert_eq!(
            store.pending_for(&key).unwrap().directory,
            "/tmp/before",
            "the pending is the one from before the click"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("/tmp/after"), "nothing was saved: {text}");
    }

    /// Without a writer thread a deferred save cannot be both bounded and
    /// durable, so it reports the failure instead of writing synchronously
    /// (which is exactly what a card callback cannot afford). The mutation
    /// stays in memory for the next blocking write.
    #[tokio::test]
    async fn a_deferred_save_without_a_writer_reports_the_failure() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path.clone()).unwrap().without_writer();
        let key = ThreadKey::new("chat1".into(), "root1".into());

        assert!(
            store
                .set_pending_deferred(make_pending("chat1", "root1", "/tmp/wt"), far())
                .is_err(),
            "no writer means the save is refused, not performed inline"
        );
        assert_eq!(
            store.pending_for(&key).unwrap().directory,
            "/tmp/wt",
            "the mutation is applied in memory"
        );
        assert!(!path.exists(), "nothing was written");
    }
}
