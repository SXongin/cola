//! `/skill <id>` — loading OpenCode skills into the prompt (spec #652, tickets
//! #654/#656). The Bridge parses the command, resolves the skill ids against a
//! single generation-neutral list read, and submits the whole original message
//! text verbatim with the neutral [`PromptSkill`] attachment. Which generation
//! carries that attachment — V2's structured `skills` array or V1's text
//! instruction — is the Generation Strategy's business (#653), so the Bridge is
//! asserted here through the mock Backend's recorded prompt calls: identical on
//! either generation.
//!
//! A user who does not know an id is not stuck: a bare `/skill` (or a typed id
//! that resolves to nothing) shows the picker card source-listed from the same
//! read, and a tap on a row re-enters this same pipeline as `/skill <id>`.

use crate::backend::{PromptSkill, SkillInfo};
use crate::bridge::test_support::*;
use std::sync::Arc;

/// The registered skills the standard fixture serves: `implement-spec` with a
/// description, `foreman` without one, and a hidden user-invoked skill. The
/// canonical `name` is deliberately distinct from the `id`, so a test proves
/// the Bridge attaches the resolved entry, not the raw token.
fn fixture_skills() -> Vec<SkillInfo> {
    vec![
        SkillInfo {
            id: "implement-spec".into(),
            name: "Implement Spec".into(),
            description: Some("Drive a spec to shipped code.".into()),
            content: Some("# Implement Spec\n\nDrive it.".into()),
        },
        SkillInfo {
            id: "foreman".into(),
            name: "Foreman".into(),
            description: None,
            content: Some("# Foreman\n\nRun the batch.".into()),
        },
        SkillInfo {
            id: "hidden-tool".into(),
            name: "Hidden Tool".into(),
            description: Some("Never advertised to the model.".into()),
            content: None,
        },
    ]
}

/// The canonical attachment a typed id resolves to.
fn resolved(id: &str) -> PromptSkill {
    let skill = fixture_skills()
        .into_iter()
        .find(|s| s.id == id)
        .expect("the fixture lists this id");
    PromptSkill {
        id: skill.id,
        name: skill.name,
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
    list_skills_calls: Arc<std::sync::atomic::AtomicUsize>,
    list_skills_directories: Arc<tokio::sync::Mutex<Vec<Option<String>>>>,
}

impl SkillFixture {
    /// The standard fixture: the registered skills above.
    async fn build() -> Self {
        Self::with_skills(fixture_skills()).await
    }

    async fn with_skills(skills: Vec<SkillInfo>) -> Self {
        let mut backend = MockBackend::new(realistic_parts());
        backend.with_skills(skills);
        Self::with(backend).await
    }

    async fn with(backend: MockBackend) -> Self {
        Self::with_policy(backend, crate::config::ServerStartPolicy::Auto).await
    }

    /// [`Self::with`] under an explicit `start_server` policy — the serverless
    /// (`never`) path's fixture.
    async fn with_policy(backend: MockBackend, policy: crate::config::ServerStartPolicy) -> Self {
        let work_dir = test_work_dir();
        let config_dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(&config_dir.path().join("sessions.json"));
        cfg.opencode.start_server = policy;
        let prompt_calls = backend.prompt_calls.clone();
        let prompt_skills = backend.prompt_skills.clone();
        let list_skills_calls = backend.list_skills_calls.clone();
        let list_skills_directories = backend.list_skills_directories.clone();
        let platform = Arc::new(RecordingPlatform::new());
        let app = Arc::new(App::new(cfg, Arc::new(backend), platform.clone()).unwrap());
        Self {
            _work_dir: work_dir,
            _config_dir: config_dir,
            app,
            platform,
            prompt_calls,
            prompt_skills,
            list_skills_calls,
            list_skills_directories,
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

    /// Every button `(label, value)` of the first replied card, in order.
    async fn picker_buttons(&self) -> Vec<(String, String)> {
        let cards = self.platform.replied_cards().await;
        assert_eq!(cards.len(), 1, "exactly one picker card expected");
        card_buttons(&cards[0])
    }

    /// The first replied card's intro markdown.
    async fn picker_intro(&self) -> String {
        let cards = self.platform.replied_cards().await;
        assert_eq!(cards.len(), 1, "exactly one picker card expected");
        cards[0]["body"]["elements"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

/// The `(label, value)` of every button in a card, in order.
fn card_buttons(card: &serde_json::Value) -> Vec<(String, String)> {
    card["body"]["elements"]
        .as_array()
        .expect("card body elements")
        .iter()
        .filter(|element| element["tag"] == "button")
        .map(|element| {
            (
                element["text"]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                element["value"]["value"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// The dedicated loaded-skill card among the replied cards (spec #652, ticket
/// #655): the one whose own title is `🧩 已加载技能`.
async fn loaded_skill_card(fx: &SkillFixture) -> serde_json::Value {
    fx.platform
        .replied_cards()
        .await
        .into_iter()
        .find(|card| card["header"]["title"]["content"] == "🧩 已加载技能")
        .expect("the loaded-skill card must reply under the user's message")
}

/// Every collapsible panel of a card, in order.
fn collapsible_panels(card: &serde_json::Value) -> Vec<&serde_json::Value> {
    card["body"]["elements"]
        .as_array()
        .expect("card body elements")
        .iter()
        .filter(|element| element["tag"] == "collapsible_panel")
        .collect()
}

/// Wait until `recorder` holds at least `n` entries (a submission off the ack
/// path runs on a spawned task), or fail loudly.
async fn wait_for<T>(recorder: &Arc<tokio::sync::Mutex<Vec<T>>>, n: usize) {
    for _ in 0..500 {
        if recorder.lock().await.len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the pipeline submitted fewer than {n} prompts");
}

/// `/skill <id> <text>`: the named skill is attached — as its CANONICAL
/// `{id, name}` list entry, not the raw token — and the whole message text,
/// command token and trailing argument included, is submitted unchanged.
#[tokio::test]
async fn skill_command_resolves_the_canonical_entry_and_keeps_the_message_text() {
    let fx = SkillFixture::build().await;

    fx.send("/skill implement-spec 644").await;

    assert_eq!(
        *fx.prompt_calls.lock().await,
        vec!["/skill implement-spec 644".to_string()],
        "the whole original message text is the prompt"
    );
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![resolved("implement-spec")]],
        "the attached name is the canonical list name, not the typed id"
    );
    assert_ne!(
        fx.prompt_skills.lock().await[0][0].name,
        "implement-spec",
        "the raw token is never the attached name"
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
        vec![vec![resolved("implement-spec"), resolved("foreman")]]
    );
}

/// A resolved `/skill <id>` replies the dedicated loaded-skill card under the
/// user's message (spec #652, ticket #655, acceptance reversal): a
/// `🧩 已加载技能：<name>` fold whose body is the skill-list read's `content`.
/// The card is NOT the live Turn card; it is its own small card.
#[tokio::test]
async fn a_resolved_skill_replies_the_loaded_skill_card_with_the_content_body() {
    let fx = SkillFixture::build().await;

    fx.send("/skill implement-spec 644").await;

    let card = loaded_skill_card(&fx).await;
    assert_eq!(
        card["header"]["title"]["content"], "🧩 已加载技能",
        "the dedicated card's own title: {card}"
    );
    let panel = collapsible_panels(&card)[0];
    assert_eq!(
        panel["header"]["title"]["content"], "🧩 已加载技能：Implement Spec",
        "the fold names the canonical skill: {card}"
    );
    let body = panel["elements"][0]["content"].as_str().unwrap();
    assert!(body.contains("Drive it."), "the body is the list content: {body}");
}

/// Repeated ids dedupe by id, first-occurrence order preserved: `/skill a
/// /skill a` attaches the skill once and the loaded-skill card shows ONE fold,
/// not two (spec #652, ticket #655).
#[tokio::test]
async fn repeated_tokens_dedupe_to_one_fold_and_one_attachment() {
    let fx = SkillFixture::build().await;

    fx.send("/skill implement-spec /skill implement-spec").await;

    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![resolved("implement-spec")]],
        "the server dedupes injection; cola must dedupe the attachment too"
    );
    let card = loaded_skill_card(&fx).await;
    assert_eq!(
        collapsible_panels(&card).len(),
        1,
        "one fold per distinct skill: {card}"
    );
}

/// A multi-skill dispatch folds every DISTINCT loaded skill, one panel each,
/// in first-occurrence order.
#[tokio::test]
async fn a_multi_skill_dispatch_folds_one_panel_per_distinct_skill() {
    let fx = SkillFixture::build().await;

    fx.send("/skill foreman /skill implement-spec /skill foreman")
        .await;

    let card = loaded_skill_card(&fx).await;
    let titles: Vec<&str> = collapsible_panels(&card)
        .iter()
        .map(|p| p["header"]["title"]["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        ["🧩 已加载技能：Foreman", "🧩 已加载技能：Implement Spec"],
        "distinct skills, first-occurrence order: {card}"
    );
}

/// A skill the list read carried no `content` for still renders its titled
/// fold (spec #652, ticket #655): the empty body degrades to a marker, never an
/// empty panel.
#[tokio::test]
async fn an_empty_content_skill_still_renders_a_titled_fold() {
    let fx = SkillFixture::build().await;

    fx.send("/skill hidden-tool").await;

    let card = loaded_skill_card(&fx).await;
    let panel = collapsible_panels(&card)[0];
    assert_eq!(panel["header"]["title"]["content"], "🧩 已加载技能：Hidden Tool");
    assert_eq!(panel["elements"][0]["content"], "（无说明）");
}

/// A bare `/skill <id>` — no text after the token — is accepted and still loads
/// the skill.
#[tokio::test]
async fn bare_skill_id_with_no_text_is_accepted() {
    let fx = SkillFixture::build().await;

    fx.send("/skill foreman").await;

    assert_eq!(*fx.prompt_calls.lock().await, vec!["/skill foreman".to_string()]);
    assert_eq!(*fx.prompt_skills.lock().await, vec![vec![resolved("foreman")]]);
}

/// A `/skill` dispatch reads the list from the CONVERSATION's project
/// directory — the location the eventual prompt runs in — so a session's own
/// project skills are the ones listed and resolved, not the server default's.
#[tokio::test]
async fn a_skill_read_is_scoped_to_the_conversations_directory() {
    let fx = SkillFixture::build().await;
    seed_session(&fx.app, "ses_test", "/work/custom").await;

    // A bare `/skill` lists that directory's skills.
    fx.send("/skill").await;
    assert_eq!(
        fx.picker_buttons().await.len(),
        3,
        "the picker lists the directory's skills"
    );
    // Resolving a typed id reads the same directory.
    fx.send("/skill implement-spec").await;
    assert_eq!(
        *fx.list_skills_directories.lock().await,
        vec![Some("/work/custom".to_string()), Some("/work/custom".to_string())],
        "every skill read must be scoped to the session's directory"
    );
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![resolved("implement-spec")]]
    );
}

/// A bare `/skill` (no id) names nothing to load: it shows the picker card
/// listing EVERY registered skill — hidden ones included — with a description
/// where present, and never reaches the model as a literal `/skill` prompt.
#[tokio::test]
async fn a_bare_skill_command_shows_the_picker_and_never_reaches_the_model() {
    let fx = SkillFixture::build().await;

    fx.send("/skill").await;

    assert!(
        fx.prompt_calls.lock().await.is_empty(),
        "a bare /skill must not be forwarded to the model"
    );
    let buttons = fx.picker_buttons().await;
    assert_eq!(
        buttons.iter().map(|(_, v)| v.as_str()).collect::<Vec<_>>(),
        ["implement-spec", "foreman", "hidden-tool"],
        "every registered skill — the hidden one included — is a row"
    );
    assert!(
        buttons
            .iter()
            .any(|(label, _)| label.contains("Implement Spec") && label.contains("Drive a spec")),
        "a row shows the name and its description: {buttons:?}"
    );
    assert!(
        buttons.iter().any(|(label, _)| label.contains("Hidden Tool")),
        "the hidden skill still appears: {buttons:?}"
    );
    assert_eq!(
        fx.list_skills_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a dispatch reads the skill list exactly once"
    );
}

/// An unknown id sends an error line plus the SAME list, and never reaches the
/// model as a prompt.
#[tokio::test]
async fn an_unknown_skill_id_shows_an_error_and_the_list_without_reaching_the_model() {
    let fx = SkillFixture::build().await;

    fx.send("/skill no-such-skill").await;

    assert!(
        fx.prompt_calls.lock().await.is_empty(),
        "an unknown id must not reach the model"
    );
    let intro = fx.picker_intro().await;
    assert!(
        intro.contains("未找到技能") && intro.contains("no-such-skill"),
        "the error names the unknown id: {intro:?}"
    );
    let buttons = fx.picker_buttons().await;
    assert_eq!(buttons.len(), 3, "the same list follows the error: {buttons:?}");
}

/// A rejected picker card must not leave the command dead: the send degrades
/// to a plain-text listing of the same skills (mirroring `send_help_card`'s
/// `help_text()` fallback), and still never reaches the model.
#[tokio::test]
async fn a_failed_picker_card_falls_back_to_a_text_list() {
    let fx = SkillFixture::build().await;
    fx.platform
        .fail_reply_card_count
        .store(1, std::sync::atomic::Ordering::SeqCst);

    fx.send("/skill").await;

    assert!(fx.prompt_calls.lock().await.is_empty());
    assert!(
        fx.platform.replied_cards().await.is_empty(),
        "the failed card must not have landed"
    );
    let texts = fx.platform.texts().await;
    assert!(
        texts
            .iter()
            .any(|t| t.contains("implement-spec") && t.contains("Hidden Tool") && t.contains("/skill <id>")),
        "a text listing is the fallback: {texts:?}"
    );
}

/// An empty skill list renders a clear "no skills" card — the picker's
/// degraded state — and still never reaches the model.
#[tokio::test]
async fn an_empty_skill_list_renders_a_no_skills_card() {
    let fx = SkillFixture::with_skills(Vec::new()).await;

    fx.send("/skill").await;

    assert!(fx.prompt_calls.lock().await.is_empty());
    let intro = fx.picker_intro().await;
    assert!(
        intro.contains("没有可用技能"),
        "a clear no-skills state is expected: {intro:?}"
    );
    assert!(fx.picker_buttons().await.is_empty(), "no rows without skills");
}

/// A tap on a picker row submits `/skill <id>` through the same pipeline a
/// typed command takes: feed the row's OWN callback payload back in (the exact
/// value Feishu echoes), and wait for the spawned submission off the ack.
#[tokio::test]
async fn a_picker_row_tap_submits_the_skill_command() {
    let fx = SkillFixture::build().await;
    fx.send("/skill").await;

    // The row's own callback value, exactly as the picker card carries it.
    let cards = fx.platform.replied_cards().await;
    let value = cards[0]["body"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["tag"] == "button")
        .expect("a picker row")["value"]
        .clone();

    let result = fx.app.host_action(value).await;
    let result = result.expect("the tap must ack");
    assert_eq!(
        result.toast.as_deref(),
        Some("正在加载技能 implement-spec…"),
        "the tap toasts"
    );

    wait_for(&fx.prompt_calls, 1).await;
    assert_eq!(
        *fx.prompt_calls.lock().await,
        vec!["/skill implement-spec".to_string()],
        "a row sends `/skill <id>` with no text"
    );
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![resolved("implement-spec")]]
    );
    // The tap path replies the same dedicated loaded-skill card a typed
    // command does (spec #652, ticket #655).
    let card = loaded_skill_card(&fx).await;
    assert_eq!(
        collapsible_panels(&card)[0]["header"]["title"]["content"],
        "🧩 已加载技能：Implement Spec"
    );
}

/// A dispatch that resolves at least one id keeps the ids that resolve and
/// submits — the spec's "keeps the ids that resolve" — so a typo alongside a
/// valid id does not block the load. The dedicated card leads with a
/// `⚠️ 未找到技能：…` line naming the unknown ids (Q19): nothing is dropped
/// silently. Only a dispatch that resolves NOTHING (a lone unknown id included)
/// shows the error-plus-picker card.
#[tokio::test]
async fn a_partly_unknown_dispatch_submits_the_resolved_skills() {
    let fx = SkillFixture::build().await;

    let text = "/skill implement-spec /skill nope 644";
    fx.send(text).await;

    assert_eq!(*fx.prompt_calls.lock().await, vec![text.to_string()]);
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![resolved("implement-spec")]],
        "the unknown id is dropped from the prompt, the resolved one is attached"
    );
    let cards = fx.platform.replied_cards().await;
    assert!(
        cards.iter().all(|card| !card.to_string().contains("选择技能")),
        "a partly-resolved dispatch submits, it does not show the picker"
    );
    // The unknown id is surfaced on the dedicated card, not dropped silently.
    let card = loaded_skill_card(&fx).await;
    let intro = card["body"]["elements"][0]["content"].as_str().unwrap();
    assert!(
        intro.contains("未找到技能") && intro.contains("nope"),
        "the card names the unknown id: {intro:?}"
    );
    assert_eq!(
        collapsible_panels(&card).len(),
        1,
        "only the resolved skill folds: {card}"
    );
}

/// A skill with an over-long name is clipped in its fold title (spec #652,
/// ticket #655): the body budget never bounds the title, so an unbounded name
/// could alone push the card past Feishu's 30 KB limit. The clip matches the
/// picker's row-label budget.
#[tokio::test]
async fn an_overlong_skill_name_is_clipped_in_the_fold_title() {
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_skills(vec![SkillInfo {
        id: "long".into(),
        name: "名".repeat(500),
        description: None,
        content: Some("body".into()),
    }]);
    let fx = SkillFixture::with(backend).await;

    fx.send("/skill long").await;

    let card = loaded_skill_card(&fx).await;
    let title = collapsible_panels(&card)[0]["header"]["title"]["content"]
        .as_str()
        .unwrap();
    assert!(
        title.chars().count() <= 80,
        "the fold title is bounded, not 500 chars: {} chars",
        title.chars().count()
    );
    assert!(
        title.ends_with('…'),
        "a clipped title carries the marker: {title}"
    );
    assert!(
        card.to_string().len() <= crate::feishu::card::FEISHU_CARD_LIMIT_BYTES,
        "the card stays under Feishu's byte ceiling"
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

/// A failed `/skill` turn's retry re-submits the prompt WITH its resolved
/// skills (spec #652, ticket #654): the accumulator keeps the resolved
/// attachment alongside the prompt text, and `TurnRecovery` carries it into the
/// new attempt. Images stay excluded (#391, re-upload cost); skills are
/// already-resolved tokens.
#[tokio::test]
async fn a_retried_skill_turn_still_submits_its_skills() {
    let mut backend = MockBackend::new(realistic_parts());
    backend.fail_prompts(1, "Simulated provider failure");
    backend.with_skills(fixture_skills());
    let fx = SkillFixture::with(backend).await;

    let text = "/skill implement-spec 644";
    fx.send(text).await;
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![resolved("implement-spec")]],
        "the first attempt submits the resolved skill"
    );

    let retry = fx
        .app
        .host_action(serde_json::json!({ "action": "retry", "session_id": "ses_test" }))
        .await;
    assert!(retry.is_some(), "the error card must offer a retry");

    wait_for(&fx.prompt_skills, 2).await;
    assert_eq!(
        *fx.prompt_calls.lock().await,
        vec![text.to_string(), text.to_string()]
    );
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![resolved("implement-spec")], vec![resolved("implement-spec")]],
        "the retry must re-attach the requested skill"
    );
}

/// A picker row whose id contains whitespace still loads (spec #652, ticket
/// #656): V1's id IS the skill name (`Implement Spec`), and the tap dispatches
/// the callback's id DIRECTLY — round-tripping `/skill Implement Spec` through
/// the command text parser would split it into two ids and resolve neither.
#[tokio::test]
async fn a_picker_row_with_a_whitespace_id_still_loads() {
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_skills(vec![SkillInfo {
        id: "Implement Spec".into(),
        name: "Implement Spec".into(),
        description: None,
        content: None,
    }]);
    let fx = SkillFixture::with(backend).await;
    fx.send("/skill").await;

    let cards = fx.platform.replied_cards().await;
    let value = cards[0]["body"]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["tag"] == "button")
        .expect("a picker row")["value"]
        .clone();

    let result = fx.app.host_action(value).await;
    assert!(result.is_some(), "the tap must ack");

    wait_for(&fx.prompt_calls, 1).await;
    assert_eq!(
        *fx.prompt_calls.lock().await,
        vec!["/skill Implement Spec".to_string()],
        "the whitespace id reaches the prompt intact"
    );
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![PromptSkill {
            id: "Implement Spec".into(),
            name: "Implement Spec".into()
        }]],
        "the row's exact id resolves, not the split tokens"
    );
}

/// A `/skill` message received while a turn is already live takes the Supplement
/// (steer) route, and its skills ride that submit too (spec #652, ticket #654).
/// The live turn is seeded exactly as the ownership read sees it — the inflight
/// guard held plus a live card — so the second message merges instead of
/// starting a Turn.
#[tokio::test]
async fn a_skill_command_during_a_live_turn_carries_its_skills_to_the_supplement() {
    let fx = SkillFixture::build().await;
    seed_session(&fx.app, "ses_test", "/work").await;
    fx.app.inflight.lock().await.insert("ses_test".to_string());
    Turn::seed_card(&fx.app.cards_handle(), "ses_test", None).await;

    fx.send("/skill implement-spec 644").await;

    assert_eq!(
        *fx.prompt_calls.lock().await,
        vec!["/skill implement-spec 644".to_string()]
    );
    assert_eq!(
        *fx.prompt_skills.lock().await,
        vec![vec![resolved("implement-spec")]],
        "the supplement submit must carry the requested skill"
    );
}

/// `/skill <id>` on a serverless cola runs Lazy Start BEFORE the skill-list read
/// (spec #652): with no server and `start_server = "never"`, the user gets the
/// SAME NoServer message a prompt does, and the list is never read — so a typed
/// id is never falsely reported as unknown against an empty serverless list
/// (the bug this guards). The backend must be able to self-start for the check
/// to be reached at all (the default attached mock makes `ensure_server` a
/// no-op).
#[tokio::test]
async fn a_skill_command_on_a_serverless_cola_ensures_the_server_first() {
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_skills(fixture_skills());
    backend.serverless = true;
    let fx = SkillFixture::with_policy(backend, crate::config::ServerStartPolicy::Never).await;

    fx.send("/skill implement-spec").await;

    assert!(
        fx.prompt_calls.lock().await.is_empty(),
        "a serverless /skill must not submit a prompt"
    );
    assert_eq!(
        fx.list_skills_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the list read must not run before Lazy Start: a serverless read is empty, \
         so a typed id would read as unknown and never reach the prompt"
    );
    let texts = fx.platform.texts().await;
    assert!(
        texts.iter().any(|t| t.contains("没有可用的 OpenCode server")),
        "the same NoServer message a prompt shows: {texts:?}"
    );
    assert!(
        fx.platform.replied_cards().await.is_empty(),
        "no picker on a serverless cola: {texts:?}"
    );
}
