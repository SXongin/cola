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
    pub fn set_active(&mut self, entry: SessionEntry) {
        // Remove any existing entry with the same session_id
        if let Some(pos) = self.entries.iter().position(|e| e.session_id == entry.session_id) {
            self.entries.remove(pos);
        }
        self.entries.insert(0, entry);
    }

    /// Remove a session entry by session ID.
    #[allow(dead_code)]
    pub fn remove(&mut self, session_id: &str) -> Option<SessionEntry> {
        if let Some(pos) = self.entries.iter().position(|e| e.session_id == session_id) {
            Some(self.entries.remove(pos))
        } else {
            None
        }
    }

    /// Find the ThreadKey for a given session ID.
    #[allow(dead_code)]
    pub fn thread_for_session(&self, session_id: &str) -> Option<ThreadKey> {
        self.entries
            .iter()
            .find(|e| e.session_id == session_id)
            .map(|e| e.thread_key.clone())
    }

    /// Directory for a session ID (used to route permission requests to the
    /// correct server instance).
    pub fn directory_for_session(&self, session_id: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|e| e.session_id == session_id)
            .map(|e| e.directory.clone())
    }

    /// Chat (thread) a session belongs to — fallback target for permission
    /// cards when no per-message reply target is known (e.g. after a restart).
    pub fn chat_for_session(&self, session_id: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|e| e.session_id == session_id)
            .map(|e| e.thread_key.chat_id.clone())
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

    /// Switch active session by name (partial match).
    pub fn switch(&mut self, key: &ThreadKey, name: &str) -> Option<&SessionEntry> {
        let pos = self
            .entries
            .iter()
            .position(|e| &e.thread_key == key && e.name.contains(name))?;
        let entry = self.entries.remove(pos);
        self.entries.insert(0, entry);
        Some(&self.entries[0])
    }

    pub fn persist(&self) -> crate::error::Result<()> {
        let data = serde_json::to_string_pretty(&self.entries)?;
        std::fs::write(&self.path, data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_entry(chat_id: &str, root_id: &str, session_id: &str, name: &str, dir: &str) -> SessionEntry {
        SessionEntry {
            thread_key: ThreadKey::new(chat_id.into(), root_id.into()),
            session_id: session_id.into(),
            name: name.into(),
            directory: dir.into(),
            agent: None,
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

        let entry = make_entry("chat1", "root1", "ses_abc", "main", "/tmp/proj");
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
        store.set_active(make_entry("chat1", "root1", "ses_1", "first", "/tmp/a"));
        store.set_active(make_entry("chat1", "root1", "ses_2", "second", "/tmp/b"));

        let list = store.list_thread(&key);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn switch_by_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();

        let key = ThreadKey::new("chat1".into(), "root1".into());
        store.set_active(make_entry("chat1", "root1", "ses_1", "frontend", "/tmp/front"));
        store.set_active(make_entry("chat1", "root1", "ses_2", "backend", "/tmp/back"));

        let active = store.switch(&key, "back");
        assert!(active.is_some());
        assert_eq!(active.unwrap().session_id, "ses_2");
        assert_eq!(store.get_active(&key).unwrap().session_id, "ses_2");
    }

    #[test]
    fn persists_and_reloads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        {
            let mut store = SessionStore::new(path.clone()).unwrap();
            store.set_active(make_entry("chat1", "root1", "ses_x", "foo", "/tmp/x"));
            store.persist().unwrap();
        }

        let store2 = SessionStore::new(path).unwrap();
        let found = store2.get_active(&ThreadKey::new("chat1".into(), "root1".into()));
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "foo");
    }

    #[test]
    fn remove_by_session_id() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::new(path).unwrap();

        let key = ThreadKey::new("chat1".into(), "root1".into());
        store.set_active(make_entry("chat1", "root1", "ses_rm", "remove_me", "/tmp/y"));

        let removed = store.remove("ses_rm");
        assert!(removed.is_some());
        assert!(store.get_active(&key).is_none());
    }
}
