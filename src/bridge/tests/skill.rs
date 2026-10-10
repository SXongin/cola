//! `/skill <id>` — loading OpenCode skills into the prompt (spec #652, ticket
//! #654). The Bridge parses the command, resolves the skill ids in order, and
//! submits the whole original message text verbatim with the neutral
//! [`PromptSkill`] attachment. Which generation carries that attachment — V2's
//! structured `skills` array or V1's text instruction — is the Generation
//! Strategy's business (#653), so the Bridge is asserted here through the mock
//! Backend's recorded prompt calls: identical on either generation.

use crate::backend::PromptSkill;
use crate::bridge::test_support::*;
use std::sync::Arc;

/// A skill id typed on the command line becomes a [`PromptSkill`] whose name is
/// the id itself (there is no skill-list read until #656 resolves the real
/// name).
fn typed(id: &str) -> PromptSkill {
    PromptSkill {
        id: id.to_string(),
        name: id.to_string(),
    }
}

/// The `/skill` tests' fixture: a MockBackend-backed app plus the two prompt
/// recorders a skill assertion reads. Holds the working-dir and config-dir
/// guards so both outlive the app.
struct SkillFixture {
    _work_dir: tempfile::TempDir,
    _config_dir: tempfile::TempDir,
    app: Arc<App>,
    platform: Arc<RecordingPlatform>,
    prompt_calls: Arc<tokio::sync::Mutex<Vec<String>>>,
    prompt_skills: Arc<tokio::sync::Mutex<Vec<Vec<PromptSkill>>>>,
}

impl SkillFixture {
    async fn build() -> Self {
        Self::with(MockBackend::new(realistic_parts())).await
    }

    async fn with(backend: MockBackend) -> Self {
        let work_dir = test_work_dir();
        let config_dir = tempfile::tempdir().unwrap();
        let cfg = test_config(&config_dir.path().join("sessions.json"));
        let prompt_calls = backend.prompt_calls.clone();
        let prompt_skills = backend.prompt_skills.clone();
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
        Self {
            _work_dir: work_dir,
            _config_dir: config_dir,
            app,
            platform,
            prompt_calls,
            prompt_skills,
        }
    }

    /// Feed one message through the coordinator's production entry point.
    async fn send(&self, text: &str) {
        self.app
            .handle_message(incoming(
                "msg_skill".into(),
                "chat_1".into(),
                "p2p".into(),
                None,
                text.into(),
                None,
            ))
            .await;
    }
}

/// Wait until `recorder` holds at least `n` entries (a retry submits off the
/// ack path), or fail loudly.
async fn wait_for_prompts<T>(recorder: &Arc<tokio::sync::Mutex<Vec<T>>>, n: usize) {
    for _ in 0..500 {
        if recorder.lock().await.len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the pipeline submitted fewer than {n} prompts");
}

/// `/skill <id> <text>`: the named skill is attached and the whole message text
/// — command token and trailing argument included — is submitted unchanged.
#[tokio::test]
async fn skill_command_attaches_the_skill_and_keeps_the_message_text() {
    let fx = SkillFixture::build().await;

    fx.send("/skill implement-spec 644").await;

    assert_eq!(
        *fx.prompt_calls.lock().await,
        vec!["/skill implement-spec 644".to_string()],
        "the whole original message text is the prompt"
    );
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![typed("implement-spec")]]
    );
}

/// Repeating the token loads every skill, ids collected in order; the trailing
/// text after the tokens still reaches the model.
#[tokio::test]
async fn repeated_skill_tokens_load_every_skill_in_order() {
    let fx = SkillFixture::build().await;

    let text = "/skill implement-spec /skill foreman 644";
    fx.send(text).await;

    assert_eq!(*fx.prompt_calls.lock().await, vec![text.to_string()]);
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![typed("implement-spec"), typed("foreman")]]
    );
}

/// A bare `/skill <id>` — no text after the token — is accepted and still loads
/// the skill.
#[tokio::test]
async fn bare_skill_id_with_no_text_is_accepted() {
    let fx = SkillFixture::build().await;

    fx.send("/skill foreman").await;

    assert_eq!(*fx.prompt_calls.lock().await, vec!["/skill foreman".to_string()]);
    assert_eq!(*fx.prompt_skills.lock().await, vec![vec![typed("foreman")]]);
}

/// A bare `/skill` (no id) is recognised as the command but names nothing to
/// load: it answers a usage line instead of reaching the model as a literal
/// `/skill` prompt. The picker card replaces this reply (ticket #656).
#[tokio::test]
async fn a_bare_skill_command_never_reaches_the_model() {
    let fx = SkillFixture::build().await;

    fx.send("/skill").await;

    assert!(
        fx.prompt_calls.lock().await.is_empty(),
        "a bare /skill must not be forwarded to the model"
    );
    let texts = fx.platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("用法") && t.contains("/skill")),
        "a syntax reply is expected, got: {texts:?}"
    );
}

/// An ordinary message still submits with no skills attached — the parser does
/// not treat a `/skill` token that is not the command as one.
#[tokio::test]
async fn an_ordinary_message_carries_no_skills() {
    let fx = SkillFixture::build().await;

    fx.send("解释一下 /skill 这个命令").await;

    assert_eq!(
        *fx.prompt_calls.lock().await,
        vec!["解释一下 /skill 这个命令".to_string()]
    );
    assert_eq!(*fx.prompt_skills.lock().await, vec![Vec::<PromptSkill>::new()]);
}

/// A failed `/skill` turn's retry re-submits the prompt WITH its skills (spec
/// #652, ticket #654): the accumulator keeps the resolved attachment alongside
/// the prompt text, and `TurnRecovery` carries it into the new attempt. Images
/// stay excluded (#391, re-upload cost); skills are already-resolved tokens.
#[tokio::test]
async fn a_retried_skill_turn_still_submits_its_skills() {
    let mut backend = MockBackend::new(realistic_parts());
    backend.fail_prompts(1, "Simulated provider failure");
    let fx = SkillFixture::with(backend).await;

    let text = "/skill implement-spec 644";
    fx.send(text).await;
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![typed("implement-spec")]],
        "the first attempt submits the skill"
    );

    let retry = fx
        .app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "the error card must offer a retry");

    wait_for_prompts(&fx.prompt_skills, 2).await;
    assert_eq!(
        *fx.prompt_calls.lock().await,
        vec![text.to_string(), text.to_string()]
    );
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![typed("implement-spec")], vec![typed("implement-spec")]],
        "the retry must re-attach the requested skill"
    );
}
