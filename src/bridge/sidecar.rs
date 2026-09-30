//! The file I/O every best-effort JSON sidecar shares (the session mapping's
//! companions: the Wake Watermark, the Live Card record, the interactive
//! surfaces, the Instant Reminder's pin set).
//!
//! One convention, one implementation (CODING_STANDARDS: extract shared logic
//! instead of duplicating it): a **load** that fails open — a missing,
//! unreadable or corrupt file reads as the empty record, because a sidecar
//! only feeds a restart's recovery and must never wedge the process reading
//! it — and a **store** that is best-effort and atomic (temp file + rename),
//! so a crash mid-write cannot leave a half-file, and an empty record removes
//! the file instead of persisting one.
//!
//! Each module keeps its own record type, its own semantics and its own
//! format tests; only the bytes-on-disk mechanics live here.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Load the record `T` from `path`, or `T::default()` (the empty record) when
/// the file is missing, unreadable or unparseable. `what` names the record in
/// the warning lines ("Wake watermark", "live-card record", ...). Never an
/// error: a broken sidecar fails open.
pub(crate) fn load<T>(path: &Path, what: &str) -> T
where
    T: DeserializeOwned + Default,
{
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<T>(&raw) {
            Ok(record) => record,
            Err(e) => {
                tracing::warn!(
                    "could not parse {} ({e}); starting with an empty {what}",
                    path.display()
                );
                T::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(e) => {
            tracing::warn!(
                "could not read {} ({e}); starting with an empty {what}",
                path.display()
            );
            T::default()
        }
    }
}

/// Write `record` to `path` atomically (temp file + rename), best-effort: a
/// failure logs under `what` and changes nothing else — the in-memory state is
/// authoritative. `empty` records remove the file instead of writing one.
pub(crate) fn store<T: Serialize>(path: &Path, what: &str, record: &T, empty: bool) {
    if empty {
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("{what}: could not remove {}: {}", path.display(), e);
        }
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let data = match serde_json::to_string(record) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!("{what}: could not serialize the record: {e}");
            return;
        }
    };
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, data) {
        tracing::warn!("{what}: could not write {}: {}", tmp.display(), e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        tracing::warn!("{what}: could not replace {}: {}", path.display(), e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Record {
        #[serde(default)]
        values: Vec<String>,
    }

    #[test]
    fn store_round_trips_and_an_empty_record_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        store(
            &path,
            "test record",
            &Record {
                values: vec!["a".into()],
            },
            false,
        );
        assert_eq!(
            load::<Record>(&path, "test record"),
            Record {
                values: vec!["a".into()]
            }
        );
        // No temp file survives the rename.
        assert!(!path.with_extension("tmp").exists());

        store(&path, "test record", &Record::default(), true);
        assert!(!path.exists(), "an empty record removes the file");
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_the_empty_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        assert_eq!(load::<Record>(&path, "test record"), Record::default());

        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(
            load::<Record>(&path, "test record"),
            Record::default(),
            "a corrupt file reads as the empty record"
        );
    }
}
