//! `/skill <id>` — loading OpenCode skills into the prompt (spec #652, ticket
//! #654). The Bridge parses the command, resolves the skill ids in order, and
//! submits the whole original message text verbatim with the neutral
//! [`crate::backend::PromptSkill`] attachment. Which generation carries that
//! attachment — V2's structured `skills` array or V1's text instruction — is the
//! Generation Strategy's business (#653), so the Bridge is asserted here through
//! the mock Backend's recorded prompt calls: identical on either generation.

use crate::backend::PromptSkill;
use crate::bridge::test_support::*;

/// A skill id typed on the command line becomes a [`PromptSkill`] whose name is
/// the id itself (there is no skill-list read until #656 resolves the real
/// name).
fn typed(id: &str) -> PromptSkill {
    PromptSkill {
        id: id.to_string(),
        name: id.to_string(),
    }
}

/// `/skill <id> <text>`: the named skill is attached and the whole message text
/// — command token and trailing argument included — is submitted unchanged.
#[tokio::test]
async fn skill_command_attaches_the_skill_and_keeps_the_message_text() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_skills = backend.prompt_skills.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_skill".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/skill implement-spec 644".into(),
        None,
    ))
    .await;

    assert_eq!(
        *prompt_calls.lock().await,
        vec!["/skill implement-spec 644".to_string()],
        "the whole original message text is the prompt"
    );
    assert_eq!(*prompt_skills.lock().await, vec![vec![typed("implement-spec")]]);
}

/// Repeating the token loads every skill, ids collected in order; the trailing
/// text after the tokens still reaches the model.
#[tokio::test]
async fn repeated_skill_tokens_load_every_skill_in_order() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_skills = backend.prompt_skills.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    let text = "/skill implement-spec /skill foreman 644";
    app.handle_message(incoming(
        "msg_skill".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        text.into(),
        None,
    ))
    .await;

    assert_eq!(*prompt_calls.lock().await, vec![text.to_string()]);
    assert_eq!(
        *prompt_skills.lock().await,
        vec![vec![typed("implement-spec"), typed("foreman")]]
    );
}

/// A bare `/skill <id>` — no text after the token — is accepted and still loads
/// the skill.
#[tokio::test]
async fn bare_skill_id_with_no_text_is_accepted() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_skills = backend.prompt_skills.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_skill".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/skill foreman".into(),
        None,
    ))
    .await;

    assert_eq!(*prompt_calls.lock().await, vec!["/skill foreman".to_string()]);
    assert_eq!(*prompt_skills.lock().await, vec![vec![typed("foreman")]]);
}

/// A bare `/skill` (no id) is recognised as the command but names nothing to
/// load: it answers a usage line instead of reaching the model as a literal
/// `/skill` prompt. The picker card replaces this reply (ticket #656).
#[tokio::test]
async fn a_bare_skill_command_never_reaches_the_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_skill".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "/skill".into(),
        None,
    ))
    .await;

    assert!(
        prompt_calls.lock().await.is_empty(),
        "a bare /skill must not be forwarded to the model"
    );
    let texts = platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("用法") && t.contains("/skill")),
        "a syntax reply is expected, got: {texts:?}"
    );
}

/// An ordinary message still submits with no skills attached — the parser does
/// not treat a `/skill` token that is not the command as one.
#[tokio::test]
async fn an_ordinary_message_carries_no_skills() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let prompt_calls = backend.prompt_calls.clone();
    let prompt_skills = backend.prompt_skills.clone();
    let (app, _platform) = build_app(cfg, backend).await;

    app.handle_message(incoming(
        "msg_plain".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "解释一下 /skill 这个命令".into(),
        None,
    ))
    .await;

    assert_eq!(
        *prompt_calls.lock().await,
        vec!["解释一下 /skill 这个命令".to_string()]
    );
    assert_eq!(*prompt_skills.lock().await, vec![Vec::<PromptSkill>::new()]);
}
