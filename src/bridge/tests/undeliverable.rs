//! #489: a pending request whose session id cannot name a session — the
//! `global` sentinel a session-less MCP elicitation carries, or a permission
//! with no session id at all — is structurally undeliverable: no parent chain,
//! no card target, no chat to remind. The sweep warns once, naming it, and
//! never reads the session. The pre-fix shape retried a doomed
//! `GET api/session/global` — and its WARN — on every 3 s sweep, forever.

use std::sync::Arc;

use crate::bridge::test_support::*;

/// A one-field form, the shape an MCP elicitation lists.
fn question(id: &str, session_id: &str) -> crate::opencode::types::QuestionRequest {
    crate::opencode::types::QuestionRequest {
        id: id.into(),
        session_id: session_id.into(),
        questions: vec![crate::opencode::types::QuestionInfo {
            question: "继续吗？".into(),
            header: "下一步".into(),
            options: vec![crate::opencode::types::QuestionOption {
                label: "继续".into(),
                description: String::new(),
                ..Default::default()
            }],
            kind: crate::opencode::types::FormFieldKind::String,
            custom: None,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// A permission whose session id is absent (V1 leaves it optional; the neutral
/// type maps absence to "").
fn permission_without_session(request_id: &str) -> crate::opencode::types::PermissionRequest {
    crate::opencode::types::PermissionRequest {
        request_id: request_id.into(),
        session_id: None,
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
    }
}

/// A session-less form is skipped with one WARN, never walked, never
/// surfaced, never pinned — while a session-backed form in the same sweep
/// still surfaces normally.
#[tokio::test]
async fn a_session_less_form_warns_once_and_is_never_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true; // the pre-fix loop's gate
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_questions(vec![
        question("frm_global", "global"),
        question("que_real", "ses_1"),
    ]);
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let mut seen = std::collections::HashSet::new();
    let (_, logs) = capture_logs(async {
        app.question.sweep(&app.flow_handles(), &mut seen).await;
        app.question.sweep(&app.flow_handles(), &mut seen).await;
    })
    .await;

    let calls = backend.session_info_calls.lock().await.clone();
    assert!(
        !calls.iter().any(|id| id == "global"),
        "a session-less request must never be walked: {calls:?}"
    );
    assert!(
        calls.iter().any(|id| id == "ses_1"),
        "the session-backed form still resolves through the parent chain: {calls:?}"
    );
    assert_eq!(
        level_count(&logs, "not a session id", "WARN"),
        1,
        "one WARN for the state change, not one per sweep:\n{logs}"
    );
    let line = assert_line_level(&logs, "frm_global", "WARN");
    assert!(line.contains("global"), "the WARN names the offending id: {line}");
    assert_eq!(
        platform.sent_cards().await.len(),
        1,
        "only the session-backed form is delivered"
    );
    assert!(
        platform.reminders().await.is_empty(),
        "a session-less request never pins"
    );
}

/// The same terminal judgement covers a permission with no session id: one
/// WARN, no session read, no card attempt, no reply.
#[tokio::test]
async fn a_permission_without_a_session_id_warns_once_and_is_never_read() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(&dir.path().join("sessions.json"));
    cfg.bridge.instant_reminder = true;
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permission(permission_without_session("per_1"));
    let backend = Arc::new(backend);
    let platform = Arc::new(RecordingPlatform::new());
    let app = Arc::new(App::new(cfg, backend.clone(), platform.clone()).unwrap());
    seed_session(&app, "ses_1", "/work").await;

    let mut seen = std::collections::HashSet::new();
    let (_, logs) = capture_logs(async {
        app.permission.sweep(&app.flow_handles(), &mut seen).await;
        app.permission.sweep(&app.flow_handles(), &mut seen).await;
    })
    .await;

    assert!(
        backend.session_info_calls.lock().await.is_empty(),
        "a session-less request must never be walked"
    );
    assert_eq!(
        level_count(&logs, "not a session id", "WARN"),
        1,
        "one WARN for the state change, not one per sweep:\n{logs}"
    );
    assert_line_level(&logs, "per_1", "WARN");
    assert!(
        platform.sent_cards().await.is_empty() && platform.replied_cards().await.is_empty(),
        "a session-less request is never delivered"
    );
    assert!(
        backend.reply_permission_calls.lock().await.is_empty(),
        "nothing is answered for it"
    );
    assert!(
        platform.reminders().await.is_empty(),
        "a session-less request never pins"
    );
}
