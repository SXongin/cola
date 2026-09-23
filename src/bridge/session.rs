use crate::config::{SessionEntry, ThreadKey};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    entries: Vec<SessionEntry>,
    /// At most one Pending Session per ThreadKey (ADR-0041).
    pending: Vec<PendingEntry>,
}

impl SessionStore {
    pub fn new(path: PathBuf) -> crate::error::Result<Self> {
        let (entries, pending) = if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            match serde_json::from_str::<LoadedStore>(&data) {
                Ok(loaded) => loaded.into_parts(),
                Err(err) => {
                    tracing::warn!(
                        "could not parse {} ({err}); starting with an empty mapping",
                        path.display()
                    );
                    (Vec::new(), Vec::new())
                }
            }
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            (Vec::new(), Vec::new())
        };
        Ok(Self {
            path,
            entries,
            pending,
        })
    }

    /// Get the active session for a thread (first match). `None` while the
    /// thread has a Pending Session: the pending supersedes it, though the old
    /// session stays mapped (`list_thread`) and switchable (ADR-0041).
    pub fn get_active(&self, key: &ThreadKey) -> Option<&SessionEntry> {
        if self.pending_for(key).is_some() {
            return None;
        }
        self.entries.iter().find(|e| &e.thread_key == key)
    }

    /// The conversation's Pending Session, if it declared one (ADR-0041).
    pub fn pending_for(&self, key: &ThreadKey) -> Option<&PendingEntry> {
        self.pending.iter().find(|p| &p.thread_key == key)
    }

    /// Declare (or replace) the conversation's Pending Session and persist.
    pub fn set_pending(&mut self, pending: PendingEntry) -> crate::error::Result<()> {
        self.pending.retain(|p| p.thread_key != pending.thread_key);
        self.pending.push(pending);
        self.write_to_disk()
    }

    /// Drop the conversation's Pending Session and persist.
    #[allow(dead_code)] // ADR-0041 storage; the command tickets (#211-#214) clear pendings
    pub fn clear_pending(&mut self, key: &ThreadKey) -> crate::error::Result<()> {
        self.pending.retain(|p| &p.thread_key != key);
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
    fn promote(&mut self, mut entry: SessionEntry) {
        self.pending.retain(|p| p.thread_key != entry.thread_key);
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
        if let Some(pos) = self.entries.iter().position(|e| e.session_id == entry.session_id) {
            let previous = self.entries.remove(pos);
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
        self.entries.insert(0, entry);
    }

    /// Promote `entry` as its thread's active session and persist the store.
    /// The durable half of every create/adopt/promote path: callers cannot
    /// forget the save. Activation also resolves the thread's Pending Session
    /// (a pending and an active session are mutually exclusive, ADR-0041) —
    /// materialisation is exactly this operation, in one write.
    pub fn activate(&mut self, entry: SessionEntry) -> crate::error::Result<()> {
        self.promote(entry);
        self.write_to_disk()
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
        let Some(entry) = self.entries.iter_mut().find(|e| e.session_id == session_id) else {
            return Ok(None);
        };
        f(entry);
        let updated = entry.clone();
        self.write_to_disk()?;
        Ok(Some(updated))
    }

    /// Mutate the conversation's Pending Session in place and persist,
    /// returning `false` when the thread has no pending (ADR-0041: `/name`
    /// and the per-session settings commands configure a pending). Mirrors
    /// [`SessionStore::update`] for entries.
    pub fn update_pending<F>(&mut self, key: &ThreadKey, f: F) -> crate::error::Result<bool>
    where
        F: FnOnce(&mut PendingEntry),
    {
        let Some(pending) = self.pending.iter_mut().find(|p| &p.thread_key == key) else {
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
                variant: p.variant.clone(),
                auto_accept: p.auto_accept,
            });
        }
        self.get_active(key).map(|e| SessionSettings {
            directory: e.directory.clone(),
            session_id: Some(e.session_id.clone()),
            agent: e.agent.clone(),
            model: e.model.clone(),
            variant: e.variant.clone(),
            auto_accept: e.auto_accept,
        })
    }

    /// Write a [`SessionSettings`] snapshot back to the target it came from and
    /// persist: by `session_id` for a real session, else the thread's Pending
    /// Session. A snapshot taken while pending whose session materialises
    /// before this write lands on the new active entry — the settings still
    /// configure the conversation (ADR-0041). `false` when the target is gone.
    pub fn set_settings(&mut self, key: &ThreadKey, settings: SessionSettings) -> crate::error::Result<bool> {
        let overrides = (
            settings.agent.clone(),
            settings.model.clone(),
            settings.variant.clone(),
            settings.auto_accept,
        );
        let applied = if let Some(id) = settings.session_id.as_deref() {
            match self.entries.iter_mut().find(|e| e.session_id == id) {
                Some(e) => {
                    (e.agent, e.model, e.variant, e.auto_accept) = overrides.clone();
                    true
                }
                None => false,
            }
        } else if let Some(p) = self.pending.iter_mut().find(|p| &p.thread_key == key) {
            (p.agent, p.model, p.variant, p.auto_accept) = overrides;
            true
        } else if let Some(e) = self.entries.iter_mut().find(|e| &e.thread_key == key) {
            // The pending materialised between the read and this write: the
            // overrides belong on the session it created.
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
        if let Some(pos) = self.entries.iter().position(|e| e.session_id == session_id) {
            Some(self.entries.remove(pos))
        } else {
            None
        }
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
        self.pending.retain(|p| &p.thread_key != key);
        let removed: Vec<SessionEntry> = self
            .entries
            .iter()
            .filter(|e| &e.thread_key == key)
            .cloned()
            .collect();
        self.entries.retain(|e| &e.thread_key != key);
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
            .find(|e| e.session_id == session_id)
            .map(|e| e.thread_key.clone())
    }

    /// Find the entry for a session ID (regardless of thread). Used to check
    /// per-session flags like `auto_accept` from the permission poller.
    pub fn entry_for_session(&self, session_id: &str) -> Option<&SessionEntry> {
        self.entries.iter().find(|e| e.session_id == session_id)
    }

    /// Directory for a session ID (used to route permission requests to the
    /// correct server instance).
    pub fn directory_for_session(&self, session_id: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|e| e.session_id == session_id)
            .map(|e| e.directory.clone())
    }

    /// Unique session directories, used by the permission poller to check every
    /// instance the bot has created sessions in.
    pub fn directories(&self) -> Vec<String> {
        let mut dirs: Vec<String> = Vec::new();
        for e in &self.entries {
            if !dirs.contains(&e.directory) {
                dirs.push(e.directory.clone());
            }
        }
        dirs
    }

    /// List all sessions in a thread.
    pub fn list_thread(&self, key: &ThreadKey) -> Vec<&SessionEntry> {
        self.entries.iter().filter(|e| &e.thread_key == key).collect()
    }

    /// All known sessions (used by the external-message poller).
    pub fn all_entries(&self) -> Vec<&SessionEntry> {
        self.entries.iter().collect()
    }

    fn write_to_disk(&self) -> crate::error::Result<()> {
        #[derive(Serialize)]
        struct StoreFileRef<'a> {
            entries: &'a [SessionEntry],
            pending: &'a [PendingEntry],
        }
        let data = serde_json::to_string_pretty(&StoreFileRef {
            entries: &self.entries,
            pending: &self.pending,
        })?;
        std::fs::write(&self.path, data)?;
        Ok(())
    }

    /// Test-only durable write; production reaches it only through
    /// activate/update/remove_persist/remove_thread_persist.
    #[cfg(test)]
    pub(crate) fn persist(&self) -> crate::error::Result<()> {
        self.write_to_disk()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_entry(chat_id: &str, root_id: &str, session_id: &str, dir: &str) -> SessionEntry {
        SessionEntry {
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
}
