use crate::bridge::session::PendingEntry;
use crate::bridge::test_support::*;

fn key() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

/// ADR-0041: `current_project_directory` reads the Pending Session first, even
/// though `get_active` is `None` for the thread while the pending exists.
#[tokio::test]
async fn current_project_directory_reads_pending_first() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.work_dir = Some(dir.path().join("work"));
    let (app, _platform) = build_app(cfg, MockBackend::new(realistic_parts())).await;

    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/a"),
    )
    .await;
    seed_pending(&app, PendingEntry::new(key(), "/work/pending")).await;

    assert_eq!(app.core.current_project_directory(&key()).await, "/work/pending");
}

/// The `/dir` card's 当前 directory follows the pending, not the superseded
/// active session.
#[tokio::test]
async fn dir_card_current_reads_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![list_session("ses_a", "A", "/work/a", 100)];
    let (app, _platform) = build_app(cfg, backend).await;

    seed_pending(&app, PendingEntry::new(key(), "/work/pending")).await;

    let (dirs, current) = crate::bridge::command::dir_card_data(&app.core, &key()).await;
    assert_eq!(dirs, vec!["/work/a".to_string()]);
    assert_eq!(current.as_deref(), Some("/work/pending"));
}

/// The `/switch` card's current-directory scope follows the pending, and no
/// row is marked active — the pending is not a Session (ADR-0041); the
/// superseded session stays mapped and switchable.
#[tokio::test]
async fn switch_card_current_reads_pending_and_marks_no_active_row() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.session_list = vec![
        list_session("ses_a", "A", "/work/a", 100),
        list_session("ses_p", "P", "/work/pending", 200),
    ];
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(
        &app,
        crate::config::SessionEntry::new(key(), "ses_old", "/work/a"),
    )
    .await;
    seed_pending(&app, PendingEntry::new(key(), "/work/pending")).await;

    let (shown, active_id, mapped_ids, scope, current_dir) = crate::bridge::command::switch_card_data(
        &app.core,
        &key(),
        "",
        crate::bridge::command::SwitchScope::All,
    )
    .await;

    assert_eq!(shown.len(), 2);
    assert_eq!(current_dir.as_deref(), Some("/work/pending"));
    assert!(active_id.is_none(), "a pending means no active session");
    assert_eq!(mapped_ids, vec!["ses_old".to_string()]);
    assert_eq!(scope, crate::bridge::command::SwitchScope::All);
}
