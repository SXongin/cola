use crate::config::{SessionEntry, ThreadKey};
use std::path::PathBuf;

/// Manages the thread → session mapping, persisted to a JSON file.
/// Multiple sessions can exist per thread; the first matching entry is "active".
pub struct SessionStore {
    path: PathBuf,
    entries: Vec<SessionEntry>,
}

impl SessionStore {
    pub fn new(path: PathBuf) -> crate::error::Result<Self> {
        let entries = if path.exists() {
            let data = std::fs::read_to_string(&path)?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Vec::new()
        };
        Ok(Self { path, entries })
    }

    /// Get the active session for a thread (first match).
    pub fn get_active(&self, key: &ThreadKey) -> Option<&SessionEntry> {
        self.entries.iter().find(|e| &e.thread_key == key)
    }

    /// Add or promote a session entry as the active one for its thread.
    /// The entry is moved to the front so `get_active` returns it.
    fn promote(&mut self, entry: SessionEntry) {
        // Remove any existing entry with the same session_id
        if let Some(pos) = self.entries.iter().position(|e| e.session_id == entry.session_id) {
            self.entries.remove(pos);
        }
        self.entries.insert(0, entry);
    }

    /// Promote `entry` as its thread's active session and persist the store.
    /// The durable half of every create/adopt/promote path: callers cannot
    /// forget the save.
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

    /// Remove every session entry mapped to a thread (used by `/forget`).
    fn remove_thread(&mut self, key: &ThreadKey) -> Vec<SessionEntry> {
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
        let data = serde_json::to_string_pretty(&self.entries)?;
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
}
