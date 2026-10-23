//! ADR-0091's durable-state migration: a version-0 fixture written by the
//! previous release loads with every section intact, is rewritten once in the
//! current form, and a second process reading it writes nothing. The failure
//! modes — corrupt/absent fails open, a future version is refused — stay as
//! they were.

use std::path::Path;

use crate::bridge::access::AccessList;
use crate::bridge::chain::ChainRecords;
use crate::bridge::session::SessionStore;
use crate::bridge::surfaces::Surfaces;
use crate::bridge::terminal_record::TerminalRunRecords;
use crate::config::{DEFAULT_CLIENT_ID, ThreadKey};
use crate::state::STATE_FORMAT_VERSION;

/// The version-0 session mapping the previous release wrote: no `version`, no
/// `client_id`, one mapped session and one pending.
const SESSIONS_V0: &str = r#"{
  "entries": [
    {
      "thread_key": { "chat_id": "chat_1", "thread_id": "chat_1" },
      "session_id": "ses_1",
      "directory": "/work/one"
    }
  ],
  "pending": [
    {
      "thread_key": { "chat_id": "chat_2", "thread_id": "chat_2" },
      "directory": "/work/two"
    }
  ]
}"#;

/// The version-0 Chain Record: every durable section non-empty.
const CHAIN_V0: &str = r#"{
  "records": {
    "ses_1": { "card_message_id": "om_1", "message_id": "msg_1", "directory": "/work/one" }
  },
  "announcements": {
    "ses_1": { "wake_id": "wake_1", "created_ms": 7 }
  },
  "endings": {
    "ses_1": [
      {
        "run": { "key": "call_1", "call_id": "call_1" },
        "endings": [
          { "class": "evidence", "state": "shown" }
        ]
      }
    ]
  },
  "delivered": { "ses_1": "msg_delivered" }
}"#;

/// The version-0 interactive surfaces: one live block, its cached card, and one
/// standalone card.
const SURFACES_V0: &str = r#"{
  "inline": {
    "per_1": {
      "message_id": "om_1",
      "kind": "Permission",
      "session_id": "ses_1",
      "directory": "/work/one",
      "target": "ls"
    }
  },
  "cards": {
    "om_1": { "card": { "v": 1 }, "spans": [] }
  },
  "standalone": {
    "per_2": {
      "kind": "Permission",
      "message_id": "om_2",
      "summary": "ls",
      "directory": "/work/one",
      "session_id": "ses_1"
    }
  }
}"#;

const TERMINAL_V0: &str = r#"{
  "run_id": "run_1",
  "chat_id": "chat_1",
  "thread_id": "chat_1",
  "card_id": "om_card",
  "log_path": "/tmp/terminal.log",
  "command": "ls",
  "cwd": "/work/one",
  "timeout_ms": 1000,
  "state": "running"
}"#;

fn key(chat: &str) -> ThreadKey {
    ThreadKey::new(chat.into(), chat.into())
}

fn write_fixture(dir: &Path) {
    std::fs::write(dir.join("sessions.json"), SESSIONS_V0).unwrap();
    std::fs::write(dir.join("chain_records.json"), CHAIN_V0).unwrap();
    std::fs::write(dir.join("interactive_surfaces.json"), SURFACES_V0).unwrap();
    std::fs::write(dir.join("access.json"), r#"{ "host": "ou_host" }"#).unwrap();
    let runs = dir.join("terminal-runs");
    std::fs::create_dir_all(&runs).unwrap();
    // The record's file name is the Chat/Topic key; ask the store for the path
    // rather than reproducing the slug rule.
    let naming = TerminalRunRecords::new(runs.clone());
    std::fs::write(naming.path(&key("chat_1")), TERMINAL_V0).unwrap();
}

/// Every file's bytes, so "the second process did not rewrite" is exact.
fn file_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn a_release_written_fixture_loads_every_section_and_upgrades_once() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    // Load every store the daemon does at startup.
    let sessions = SessionStore::for_client(dir.path().join("sessions.json"), DEFAULT_CLIENT_ID).unwrap();
    assert_eq!(sessions.get_active(&key("chat_1")).unwrap().session_id, "ses_1");
    assert_eq!(
        sessions.pending_for(&key("chat_2")).unwrap().directory,
        "/work/two"
    );
    assert_eq!(
        sessions.get_active(&key("chat_1")).unwrap().client_id,
        DEFAULT_CLIENT_ID
    );

    let chains = ChainRecords::load_for(dir.path().join("chain_records.json"), DEFAULT_CLIENT_ID).unwrap();
    assert_eq!(
        chains.entries(),
        vec![(
            "ses_1".to_string(),
            chains.get("ses_1").expect("the migrated record")
        )]
    );
    assert_eq!(chains.announced("ses_1").unwrap().wake_id, "wake_1");
    assert_eq!(chains.endings("ses_1")[0].run.key, "call_1");
    assert_eq!(
        chains.delivered("ses_1").map(|id| id.as_str().to_string()),
        Some("msg_delivered".to_string())
    );

    let surfaces = Surfaces::open(dir.path().join("interactive_surfaces.json"), DEFAULT_CLIENT_ID).unwrap();
    let surface = surfaces.snapshot();
    assert_eq!(surface.inline.get("per_1").unwrap().message_id, "om_1");
    assert_eq!(surface.cards.get("om_1").unwrap().card["v"], 1);
    assert_eq!(surface.standalone.get("per_2").unwrap().message_id, "om_2");

    let terminals = TerminalRunRecords::open(dir.path().join("terminal-runs"), DEFAULT_CLIENT_ID).unwrap();
    let records = terminals.load_all();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].run_id, "run_1");

    let mut access = AccessList::load(&dir.path().join("access.json"));
    assert_eq!(access.host(), Some("ou_host"));
    access.migrate();

    // Every file now carries the current version and the default client id.
    for name in [
        "sessions.json",
        "chain_records.json",
        "interactive_surfaces.json",
        "access.json",
    ] {
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(name)).unwrap()).unwrap();
        assert_eq!(raw["version"], STATE_FORMAT_VERSION, "{name} is versioned");
    }
    for name in ["sessions.json", "chain_records.json", "interactive_surfaces.json"] {
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(name)).unwrap()).unwrap();
        assert!(
            raw.to_string().contains(DEFAULT_CLIENT_ID),
            "{name} records the default client"
        );
    }
    let terminal_raw = std::fs::read_to_string(
        dir.path()
            .join("terminal-runs")
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    let terminal: serde_json::Value = serde_json::from_str(&terminal_raw).unwrap();
    assert_eq!(terminal["version"], STATE_FORMAT_VERSION);
    assert_eq!(terminal["client_id"], DEFAULT_CLIENT_ID);

    // Exactly once: a second process reads version 1 and writes nothing.
    let before = file_bytes(dir.path());
    let sessions = SessionStore::for_client(dir.path().join("sessions.json"), DEFAULT_CLIENT_ID).unwrap();
    assert_eq!(sessions.writes(), 0, "a version-1 mapping is not rewritten");
    let chains = ChainRecords::load_for(dir.path().join("chain_records.json"), DEFAULT_CLIENT_ID).unwrap();
    assert_eq!(chains.writes(), 0, "a version-1 Chain Record is not rewritten");
    let _ = Surfaces::open(dir.path().join("interactive_surfaces.json"), DEFAULT_CLIENT_ID).unwrap();
    let _ = TerminalRunRecords::open(dir.path().join("terminal-runs"), DEFAULT_CLIENT_ID).unwrap();
    let _ = AccessList::load(&dir.path().join("access.json"));
    assert_eq!(
        before,
        file_bytes(dir.path()),
        "the second process writes nothing"
    );
}

#[test]
fn a_corrupt_or_absent_file_still_fails_open() {
    let dir = tempfile::tempdir().unwrap();

    // Absent: empty stores, no error.
    let sessions = SessionStore::for_client(dir.path().join("sessions.json"), DEFAULT_CLIENT_ID).unwrap();
    assert!(sessions.all_entries().is_empty());
    let chains = ChainRecords::load_for(dir.path().join("chain_records.json"), DEFAULT_CLIENT_ID).unwrap();
    assert!(chains.entries().is_empty());
    let surfaces = Surfaces::open(dir.path().join("interactive_surfaces.json"), DEFAULT_CLIENT_ID).unwrap();
    assert!(surfaces.snapshot().inline.is_empty());

    // Corrupt: the same empty stores, and the bytes are left for a human to
    // inspect instead of being replaced by an empty version-1 file.
    let corrupt = dir.path().join("corrupt");
    std::fs::create_dir_all(&corrupt).unwrap();
    std::fs::write(corrupt.join("sessions.json"), "{not json").unwrap();
    std::fs::write(corrupt.join("chain_records.json"), "{not json").unwrap();
    std::fs::write(corrupt.join("interactive_surfaces.json"), "{not json").unwrap();
    let sessions = SessionStore::for_client(corrupt.join("sessions.json"), DEFAULT_CLIENT_ID).unwrap();
    assert!(sessions.all_entries().is_empty());
    let chains = ChainRecords::load_for(corrupt.join("chain_records.json"), DEFAULT_CLIENT_ID).unwrap();
    assert!(chains.entries().is_empty());
    let surfaces = Surfaces::open(corrupt.join("interactive_surfaces.json"), DEFAULT_CLIENT_ID).unwrap();
    assert!(surfaces.snapshot().inline.is_empty());
    for name in ["sessions.json", "chain_records.json", "interactive_surfaces.json"] {
        assert_eq!(
            std::fs::read_to_string(corrupt.join(name)).unwrap(),
            "{not json",
            "{name} is untouched"
        );
    }

    let terminals = TerminalRunRecords::open(corrupt.join("terminal-runs"), DEFAULT_CLIENT_ID).unwrap();
    assert!(terminals.load_all().is_empty());
}

#[test]
fn a_future_version_refuses_to_load_instead_of_reading_version_zero() {
    let dir = tempfile::tempdir().unwrap();
    let future = format!("{{\"version\": {}, \"entries\": []}}", STATE_FORMAT_VERSION + 1);
    std::fs::write(dir.path().join("sessions.json"), &future).unwrap();
    assert!(
        SessionStore::for_client(dir.path().join("sessions.json"), DEFAULT_CLIENT_ID).is_err(),
        "a future sessions.json is refused"
    );

    let chain = format!("{{\"version\": {}, \"records\": {{}}}}", STATE_FORMAT_VERSION + 1);
    std::fs::write(dir.path().join("chain_records.json"), &chain).unwrap();
    assert!(
        ChainRecords::load_for(dir.path().join("chain_records.json"), DEFAULT_CLIENT_ID).is_err(),
        "a future chain_records.json is refused"
    );

    let surfaces = format!("{{\"version\": {}, \"inline\": {{}}}}", STATE_FORMAT_VERSION + 1);
    std::fs::write(dir.path().join("interactive_surfaces.json"), &surfaces).unwrap();
    assert!(
        Surfaces::open(dir.path().join("interactive_surfaces.json"), DEFAULT_CLIENT_ID).is_err(),
        "a future interactive_surfaces.json is refused"
    );

    let runs = dir.path().join("terminal-runs");
    std::fs::create_dir_all(&runs).unwrap();
    let mut record: serde_json::Value = serde_json::from_str(TERMINAL_V0).unwrap();
    record["version"] = serde_json::json!(STATE_FORMAT_VERSION + 1);
    std::fs::write(runs.join("chat.json"), record.to_string()).unwrap();
    assert!(
        TerminalRunRecords::open(runs, DEFAULT_CLIENT_ID).is_err(),
        "a future terminal record is refused"
    );

    let access = format!(
        "{{\"version\": {}, \"host\": \"ou_host\"}}",
        STATE_FORMAT_VERSION + 1
    );
    std::fs::write(dir.path().join("access.json"), &access).unwrap();
    let mut list = AccessList::load(&dir.path().join("access.json"));
    assert_eq!(list.host(), None, "a future Access List is not read as version 0");
    list.migrate();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("access.json")).unwrap(),
        access,
        "a refused file is never rewritten"
    );

    // The files are untouched — refusing must never rewrite what it cannot read.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sessions.json")).unwrap(),
        future
    );
}
