//! `/model`'s model/provider picks (issue #910, spec #922).
//!
//! A provider is **creation-only**: a Pending Session records the pair that
//! rides `thread/start`, while a live thread's provider pick is refused without
//! any request and points at `/new` / `/fork`. A pick whose provider is the
//! thread's own is an ordinary model pick — it rides `turn/start.model` and sends
//! no `thread/resume`.

use crate::bridge::session::PendingEntry;
use crate::bridge::test_support::*;

const PROVIDER: &str = "deepseek";
const MODEL: &str = "deepseek-v4-pro";

fn key() -> crate::config::ThreadKey {
    crate::config::ThreadKey::new("chat_1".into(), "chat_1".into())
}

fn entry() -> crate::config::SessionEntry {
    crate::config::SessionEntry::new(key(), "ses_test", "/work/cola")
}

/// The callback a model row sends: one `provider/model` value (spec #930's
/// card shape — there is no provider callback field any more).
fn model_pick(provider: &str, model: &str) -> serde_json::Value {
    serde_json::json!({
        "action": "model",
        "level": "model",
        "chat_id": "chat_1",
        "thread_id": "chat_1",
        "value": crate::backend::parsing::join_model(provider, model),
    })
}

fn prompt(app: &std::sync::Arc<crate::bridge::handler::App>, message_id: &str, text: &str) {
    let app = app.clone();
    let message_id = message_id.to_string();
    let text = text.to_string();
    tokio::spawn(async move {
        app.handle_message(incoming(
            message_id,
            "chat_1".into(),
            "p2p".into(),
            None,
            text,
            None,
        ))
        .await;
    });
}

async fn command_text(
    app: &std::sync::Arc<crate::bridge::handler::App>,
    platform: &std::sync::Arc<crate::bridge::test_support::RecordingPlatform>,
    command: &str,
    message_id: &str,
) -> String {
    send_command_in(
        app,
        command,
        key(),
        message_id,
        crate::config::ConversationKind::P2p,
    )
    .await;
    platform.texts().await.join("\n")
}

/// Spec #930 acceptance 1: a Pending Session records the **model only**, and
/// the first message carries it into `thread/start` — with no provider.
#[tokio::test]
async fn a_pending_sessions_model_pick_rides_thread_creation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // Only a client whose thread takes a creation model (Codex's shape) sends
    // it in `thread/start`; the mock defaults to OpenCode's per-prompt path.
    backend.with_codex_capabilities();
    let picks = backend.created_session_models.clone();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    let step = app
        .host_action(model_pick(PROVIDER, MODEL))
        .await
        .expect("model pick");
    assert!(step.card.is_none(), "a pending pick is toast-only");
    assert!(
        step.toast
            .as_deref()
            .unwrap_or_default()
            .contains("会话创建时生效"),
        "the toast says where it applies: {step:?}",
        step = step.toast
    );
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pending survives");
    assert_eq!(pending.model.as_deref(), Some("deepseek/deepseek-v4-pro"));

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert_eq!(
        picks.lock().await.as_slice(),
        &[Some(MODEL.to_string())],
        "one thread/start carries the model"
    );
    assert_eq!(
        prompt_models.lock().await.as_slice(),
        &[None],
        "the thread already carries the model; the first turn sends no per-prompt pick"
    );
}

/// Spec #930: a Pending Session's pick through the **neutral bucket**
/// (`codex/<id>`, the documented text form) is accepted and still names only
/// the model at creation.
#[tokio::test]
async fn a_neutral_pending_pick_creates_the_thread_without_a_provider() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    let picks = backend.created_session_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    send_command_in(
        &app,
        "/model codex/gpt-custom",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pending survives");
    assert_eq!(pending.model.as_deref(), Some("codex/gpt-custom"));

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert_eq!(
        picks.lock().await.as_slice(),
        &[Some("gpt-custom".to_string())],
        "thread/start takes the model only, never `modelProvider`"
    );
}

/// Spec #930: the **same** provider with another model is an ordinary pick on a
/// live thread — no thread-level request and no refusal. On a next-turn client
/// it records a one-shot pending and leaves the thread's current pair alone.
#[tokio::test]
async fn a_same_provider_pick_is_an_ordinary_model_pick() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_known_thread_provider(Some("openai"));
    let pending = backend.pending_selections.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(model_pick("openai", "gpt-5.1"))
        .await
        .expect("same-provider pick");
    assert!(step.card.is_none(), "no confirmation for a model pick");
    assert!(
        step.toast.as_deref().unwrap_or_default().contains("已记录"),
        "the pending answer is honest: {step:?}",
        step = step.toast
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the live thread's current model is untouched"
    );
    let pending = pending.lock().await;
    assert_eq!(
        pending.get("ses_test").and_then(|(model, _)| model.as_deref()),
        Some("openai/gpt-5.1"),
        "the pick is the next new turn's pending model"
    );
}

/// Issue #910 / ADR-0089: a card's model pick hands its mirror write to the
/// store's writer and settles the receipt on the click's budget, so a stalled
/// disk never holds the callback. The pair is in memory (the toast says it
/// applies), the file catches up once the writer is released — and the click
/// never claims a durable save.
#[tokio::test]
async fn a_stalled_writer_does_not_stall_a_model_pick_callback() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;
    let pause = app
        .sessions
        .lock()
        .await
        .pause_writes()
        .expect("the app's store has a writer thread");
    // Hold the writer BEFORE the click, so the mirror write is in flight for the
    // whole callback (the seeded entry is already on disk).
    pause.hold();

    let started = std::time::Instant::now();
    let step = app
        .host_action(model_pick("openai", "gpt-5.1"))
        .await
        .expect("the click still answers");
    let elapsed = started.elapsed();

    assert!(
        elapsed < crate::feishu::ws::CARD_ACK_BUDGET,
        "the callback answered inside the ack budget with the write stalled: {elapsed:?}"
    );
    assert!(
        step.toast.as_deref().unwrap_or_default().starts_with("Model:"),
        "the pick's own answer, not a save claim: {step:?}",
        step = step.toast
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1"),
        "the mutation is in memory before the file"
    );
    let saved = std::fs::read_to_string(&sessions).unwrap_or_default();
    assert!(
        !saved.contains("\"openai/gpt-5.1\""),
        "the stalled write has not landed yet: {saved}"
    );

    // Releasing the writer lands it, in order, after the callback answered.
    pause.release();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if std::fs::read_to_string(&sessions).is_ok_and(|text| text.contains("\"openai/gpt-5.1\"")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the deferred mirror write landed after the callback"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Spec #930 acceptance 2: a pick naming another provider is refused
/// **without any request**, with copy saying a provider is a config fact.
/// Nothing is recorded, and no prompt/turn or resume is sent.
#[tokio::test]
async fn a_live_cross_provider_pick_is_refused_as_a_config_fact() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_known_thread_provider(Some("openai"));
    let prompts = backend.prompt_calls.clone();
    let resumes = backend.resume_calls.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let step = app
        .host_action(model_pick(PROVIDER, MODEL))
        .await
        .expect("provider pick");
    assert!(step.card.is_none(), "a refusal is a toast, not a card");
    let toast = step.toast.expect("the named refusal");
    assert!(
        toast.contains("配置事实") && toast.contains("model_provider") && toast.contains("重启"),
        "the refusal says provider is a config fact: {toast}"
    );
    assert!(
        !toast.contains("/new") && !toast.contains("/fork"),
        "a config fact is not a `/new` remedy: {toast}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the mirror is untouched"
    );
    assert!(prompts.lock().await.is_empty(), "no turn was submitted");
    assert!(resumes.lock().await.is_empty(), "no resume was sent");
}

/// Spec #930: a Pending Session's model pick is one store write, with no
/// provider half written anywhere.
#[tokio::test]
async fn a_pending_model_pick_is_saved_to_disk() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    app.host_action(model_pick(PROVIDER, MODEL))
        .await
        .expect("model pick");

    let reloaded =
        crate::bridge::session::SessionStore::for_client(sessions.clone(), crate::config::DEFAULT_CLIENT_ID)
            .expect("the store reloads");
    let pending = reloaded.pending_for(&key()).expect("the pending is on disk");
    assert_eq!(pending.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
}

/// Spec #930 acceptance 2 (Pending Session): the cached `config/read` provider
/// is the current channel, so a different prefix is refused **before any
/// request** — the config-fact copy, never a creation pair.
#[tokio::test]
async fn a_pending_text_pick_refuses_a_different_cached_provider() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_configured_provider(Some("openai"));
    let prompts = backend.prompt_calls.clone();
    let resumes = backend.resume_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    send_command_in(
        &app,
        "/model deepseek/deepseek-v4-pro",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform.texts().await.join("\n");
    assert!(
        reply.contains("配置事实") && reply.contains("openai") && reply.contains("deepseek"),
        "the refusal names the config channel: {reply}"
    );
    assert!(!reply.contains("Model:"), "nothing was recorded: {reply}");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .and_then(|pending| pending.model.clone()),
        None,
        "the refused pick leaves the pending untouched"
    );
    assert!(prompts.lock().await.is_empty(), "no turn was submitted");
    assert!(resumes.lock().await.is_empty(), "no request was sent");
}

/// Spec #930 acceptance 1 (Pending Session): a prefix that names the cached
/// config provider is accepted and records the model only.
#[tokio::test]
async fn a_pending_text_pick_accepts_the_cached_provider() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_configured_provider(Some("openai"));
    let (app, platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    send_command_in(
        &app,
        "/model openai/gpt-5.1",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform.texts().await.join("\n");
    assert!(reply.contains("Model: openai/gpt-5.1"), "{reply}");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .and_then(|pending| pending.model.clone())
            .as_deref(),
        Some("openai/gpt-5.1"),
        "the pick records the model only"
    );
}

/// Spec #930 acceptance 3/4: with no cached config provider the channel is
/// unknown, so the pick is **not blocked** — it records model-only and the
/// server's own model error is the fallback.
#[tokio::test]
async fn a_pending_text_pick_without_a_cached_provider_records_model_only() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    let (app, platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    send_command_in(
        &app,
        "/model deepseek/deepseek-v4-pro",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform.texts().await.join("\n");
    assert!(
        reply.contains("Model: deepseek/deepseek-v4-pro"),
        "an unknown channel does not block: {reply}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .pending_for(&key())
            .and_then(|pending| pending.model.clone())
            .as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the model-only pick is recorded"
    );
}

/// Finding (record-first): a session that materialises while the pending's
/// catalog lookup runs is a failure, never a silent claim.
#[tokio::test]
async fn a_pending_that_materialises_is_not_reported_saved() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_model_surface();
    let entered = backend.model_surface_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut live = entry();
    live.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, live).await;
    let mut pending = PendingEntry::new(key(), "/work/cola");
    pending.variant = Some("high".into());
    seed_pending(&app, pending).await;

    let app_for_pick = app.clone();
    let pick = tokio::spawn(async move { app_for_pick.host_action(model_pick(PROVIDER, MODEL)).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(tokio::time::Instant::now() < deadline, "the lookup never started");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    app.sessions
        .lock()
        .await
        .clear_pending(&key())
        .expect("the pending materialises");
    gate.add_permits(1);

    let step = pick.await.expect("the pick joins").expect("a pick result");
    let toast = step.toast.unwrap_or_default();
    assert!(
        toast.contains("状态已变化"),
        "a vanished pending is an honest failure: {toast}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the model never falls through to the live entry"
    );
}

/// Finding: the creation claim makes a model write during materialisation an
/// honest failure, never a model the creation cannot apply.
#[tokio::test]
async fn a_model_write_during_materialisation_fails_honestly() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_create_session();
    let entered = backend.create_session_gate_entered.clone();
    let picks = backend.created_session_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut live = entry();
    live.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, live).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the creation never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let step = app
        .host_action(model_pick(PROVIDER, MODEL))
        .await
        .expect("model pick during creation");
    let toast = step.toast.unwrap_or_default();
    assert!(
        toast.contains("正在创建") || toast.contains("状态已变化"),
        "the click reports the creation honestly: {toast}"
    );
    assert!(
        !toast.contains("新线程创建时生效"),
        "it must not claim the model will apply at creation: {toast}"
    );

    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        picks.lock().await.as_slice(),
        &[None],
        "the thread was created without the model the click could not save"
    );
}

/// Final round, item 4: a replacement pending declared while a creation is in
/// flight survives that creation's activation.
#[tokio::test]
async fn a_replacement_pending_survives_the_older_creation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_create_session();
    let entered = backend.create_session_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/first")).await;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the creation never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let mut replacement = PendingEntry::new(key(), "/work/second");
    replacement.model = Some("deepseek/deepseek-v4-pro".into());
    app.sessions
        .lock()
        .await
        .set_pending(replacement)
        .expect("the replacement is declared");

    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the older creation must not resolve the replacement");
    assert_eq!(pending.directory, "/work/second");
    assert_eq!(pending.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
}

/// Extra finding: a `/agent` pick updates only its own field — it must not
/// restore a stale model/variant snapshot over a recorded switch.
#[tokio::test]
async fn an_agent_pick_keeps_the_recorded_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("deepseek/deepseek-v4-pro".into());
    entry.variant = Some("low".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/agent build",
        key(),
        "msg_agent",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let stored = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(stored.agent.as_deref(), Some("build"));
    assert_eq!(
        stored.model.as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the recorded model is untouched"
    );
    assert_eq!(stored.variant.as_deref(), Some("low"));
}

/// Final round, item 1: a thread forgotten while its session is being created
/// must not be re-mapped by the finished creation.
#[tokio::test]
async fn a_forget_during_a_parked_creation_is_not_undone() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_create_session();
    let entered = backend.create_session_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the creation never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The user forgets the thread while `thread/start` is in flight.
    send_command(&app, "/switch forget", "msg_forget").await;
    assert!(
        app.sessions.lock().await.pending_for(&key()).is_none(),
        "the forget removed the pending"
    );

    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let store = app.sessions.lock().await;
    assert!(
        store.get_active(&key()).is_none(),
        "the finished creation must not re-map a forgotten thread"
    );
    assert!(
        store.entry_for_session("ses_test").is_none(),
        "and the created session stays unmapped"
    );
}

/// Final round, item 1: a newer selection during a parked creation wins; the
/// stale creation does not overwrite it.
#[tokio::test]
async fn a_newer_selection_during_a_parked_creation_wins() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let gate = backend.hold_create_session();
    let entered = backend.create_session_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the creation never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The user selects another session while `thread/start` is in flight.
    app.activate_session(crate::config::SessionEntry::new(
        key(),
        "ses_new",
        "/work/elsewhere",
    ))
    .await
    .expect("the newer selection activates");
    assert_eq!(
        app.sessions.lock().await.get_active(&key()).unwrap().session_id,
        "ses_new"
    );

    gate.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let store = app.sessions.lock().await;
    assert_eq!(
        store.get_active(&key()).unwrap().session_id,
        "ses_new",
        "the stale creation must not overwrite the newer selection"
    );
    assert!(store.entry_for_session("ses_test").is_none());
}

/// Spec #930 acceptance 2: the `/model <provider>/<model>` text form obeys the
/// same rule — a different provider on a live thread is refused **without
/// recording and without any request**, with copy saying it is a config fact.
#[tokio::test]
async fn the_model_text_form_refuses_a_live_provider_change() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_known_thread_provider(Some("openai"));
    let prompts = backend.prompt_calls.clone();
    let resumes = backend.resume_calls.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model deepseek/deepseek-v4-pro",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("配置事实") && text.contains("model_provider") && text.contains("重启"),
        "the refusal says provider is a config fact: {text}"
    );
    assert!(
        !text.contains("/new") && !text.contains("/fork"),
        "a config fact is not a `/new` remedy: {text}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the text form must not record the provider change"
    );
    assert!(
        !text.contains("Model:"),
        "no success reply for a refused provider change: {text}"
    );
    assert!(prompts.lock().await.is_empty(), "no turn was submitted");
    assert!(resumes.lock().await.is_empty(), "no resume was sent");
}

/// Spec #930: a client whose thread selection rides each prompt (OpenCode)
/// records the model on the Pending Session and sends it per prompt; its
/// creation body stays unchanged — no provider, no creation model.
#[tokio::test]
async fn a_pending_pick_stays_model_only_without_thread_creation() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    // OpenCode's shape: the model rides each prompt, not the thread.
    let picks = backend.created_session_models.clone();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    send_command_in(
        &app,
        "/model deepseek/deepseek-v4-pro",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pending survives");
    assert_eq!(pending.model.as_deref(), Some("deepseek/deepseek-v4-pro"));

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    let picks = picks.lock().await.clone();
    assert_eq!(picks.len(), 1, "one creation: {picks:?}");
    assert_eq!(
        picks[0], None,
        "no creation model reaches this client's create body: {picks:?}"
    );
    // The model never rode the creation body on this client (main's shape):
    // it is applied to the created session and the first turn carries it.
    assert_eq!(
        prompt_models.lock().await.as_slice(),
        &[Some("deepseek/deepseek-v4-pro".to_string())],
        "the pick still reaches the first turn"
    );
}

/// Issue #910, item 3 (standards): a durable generation's session switch is a
/// backend RPC, so it runs **outside** the state gate — a stalled server must
/// not wedge this session's settings writes. (The prompt is a deliberate
/// exception: it waits at the switch point, see
/// `a_prompt_waits_for_an_in_flight_durable_switch`.)
#[tokio::test]
async fn a_model_pick_switches_the_session_outside_the_gate() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection(
        "ses_test",
        crate::backend::types::SessionSelection {
            model: Some(crate::backend::types::ModelInfo {
                id: "gpt-5.1-codex".into(),
                provider_id: "openai".into(),
                variant: Some("high".into()),
            }),
            agent: None,
        },
    );
    let gate = backend.hold_session_switch();
    let entered = backend.switch_model_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    let pick = tokio::spawn({
        let app = app.clone();
        async move {
            send_command_in(
                &app,
                "/model openai/gpt-5.1",
                key(),
                "msg_model",
                crate::config::ConversationKind::P2p,
            )
            .await;
        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(tokio::time::Instant::now() < deadline, "the switch never started");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The pick is parked in its RPC; the state gate must still be free, so a
    // settings write on the same session completes instead of waiting for it.
    let applied = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.sessions_handle()
            .set_agent(&key(), Some("ses_test"), Some("build")),
    )
    .await
    .expect("the settings write is not blocked by the pick's RPC")
    .expect("the agent write runs");
    assert!(applied, "the agent write landed");
    assert!(
        !pick.is_finished(),
        "the pick was still parked when the settings write went through"
    );
    gate.add_permits(1);
    pick.await.expect("the pick joins");
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1"),
        "the pick lands afterwards"
    );
}

/// Spec #922: when the cache knows the thread's own provider, a
/// **same-provider** pick is an ordinary model pick — the cache-only hedge
/// refuses only what it cannot prove.
#[tokio::test]
async fn a_same_provider_pick_is_recorded_when_the_cache_knows_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_known_thread_provider(Some("openai"));
    let pending = backend.pending_selections.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model openai/gpt-5.1",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(reply.contains("Model: openai/gpt-5.1"), "{reply}");
    assert!(
        reply.contains("已记录：将在下一条新消息生效"),
        "the pick is a pending, not an applied switch: {reply}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the thread's current pair is untouched"
    );
    assert_eq!(
        pending
            .lock()
            .await
            .get("ses_test")
            .and_then(|(model, _)| model.as_deref()),
        Some("openai/gpt-5.1"),
        "the same-provider pick is the next turn's pending"
    );
}

/// Spec #922: the live model card's 待应用 line comes from the adapter's
/// one-shot pending, not from the store mirror, so it can disappear as soon as
/// the next new turn consumes it.
#[tokio::test]
async fn the_model_card_shows_the_adapters_pending_for_a_live_thread() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_known_thread_provider(Some("openai"));
    let queries = backend.model_surface_queries.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model openai/gpt-5.1",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let handles = app.command_handles();
    crate::feishu::card::command::model_surface(&handles, &key(), None, None, None).await;
    let recorded = queries.lock().await;
    let last = recorded.last().expect("the model-surface read");
    assert_eq!(
        last.pending.as_deref(),
        Some("openai/gpt-5.1"),
        "the card is told about the adapter's pending, not the untouched store"
    );
}

/// Spec #922 gate review: the model card names an effort-only pending even
/// when no thread model fact can anchor a composed reference.
#[tokio::test]
async fn the_model_card_names_an_effort_only_pending_without_a_thread_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    backend
        .pending_selections
        .lock()
        .await
        .insert("ses_test".into(), (None, Some("high".into())));
    let queries = backend.model_surface_queries.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let handles = app.command_handles();
    crate::feishu::card::command::model_surface(&handles, &key(), None, None, None).await;
    let recorded = queries.lock().await;
    let last = recorded.last().expect("the model-surface read");
    assert_eq!(
        last.pending.as_deref(),
        Some("思考等级 high"),
        "the card names the level when no model can anchor it"
    );
}

/// Spec #922: a Pending Session's pick is a creation fact. The model card must
/// show no 待应用 state for it.
#[tokio::test]
async fn a_pending_session_has_no_next_turn_pending_on_the_model_card() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    let queries = backend.model_surface_queries.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    send_command_in(
        &app,
        "/model deepseek/deepseek-v4-pro",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("将在会话创建时生效"),
        "the pending session names creation, not the next turn: {text}"
    );

    let handles = app.command_handles();
    crate::feishu::card::command::model_surface(&handles, &key(), None, None, None).await;
    let recorded = queries.lock().await;
    let last = recorded.last().expect("the model-surface read");
    assert_eq!(
        last.pending, None,
        "a Pending Session's pair rides thread/start and is never 待应用"
    );
}

/// Spec #922 whole-branch review: a lifecycle with no attached client must not
/// let `/model` or `/think` claim a recorded pick. The seam error reaches the
/// command reply; once the pending layer accepts the write again, the pick is
/// recorded.
#[tokio::test]
async fn an_unattachable_pending_seam_fails_the_pick_instead_of_claiming_success() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_known_thread_provider(Some("openai"))
        .with_models(vec![crate::backend::types::ProviderModels {
            provider: "openai".into(),
            models: vec![crate::backend::types::ModelOption {
                id: "gpt-5.1".into(),
                variants: vec!["high".into()],
                ..Default::default()
            }],
        }]);
    let pending_error = backend.pending_error.clone();
    let pending = backend.pending_selections.clone();
    *pending_error.lock().await = Some("not attached to a codex app-server daemon".into());
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("low".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model openai/gpt-5.1",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("切换模型失败") && text.contains("not attached"),
        "the model error is named: {text}"
    );
    assert!(
        !text.contains("已记录"),
        "an unattached pending seam must not claim success: {text}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .variant
            .as_deref(),
        Some("low"),
        "a failed /model restores the mirror's pre-pick /think level"
    );

    send_command_in(
        &app,
        "/think high",
        key(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("切换思考等级失败") && text.contains("not attached"),
        "the think error is named: {text}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .variant
            .as_deref(),
        Some("low"),
        "a failed /think restores the mirror's pre-pick level"
    );
    assert!(
        pending.lock().await.is_empty(),
        "neither failed seam recorded a pending"
    );

    *pending_error.lock().await = None;
    send_command_in(
        &app,
        "/model openai/gpt-5.1",
        key(),
        "msg_model_retry",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let text = platform.texts().await.join("\n");
    assert!(
        text.contains("已记录：将在下一条新消息生效"),
        "an attached seam records the pick: {text}"
    );
    assert_eq!(
        pending
            .lock()
            .await
            .get("ses_test")
            .and_then(|(model, _)| model.as_deref()),
        Some("openai/gpt-5.1"),
        "the retry reaches the pending layer"
    );
}

/// Spec #922 whole-branch review (test gap): a materialised Pending Session
/// carries its creation pair in the store, and the adapter may still hold a
/// next-turn pending. `Turn::run` must pass **no** per-prompt pair on a
/// thread-provider client — the adapter's pending owns the next turn — or a
/// later prompt would re-send the stored creation pair.
#[tokio::test]
async fn a_materialised_store_model_is_not_resent_when_the_adapter_holds_a_pending() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    let prompt_models = backend.prompt_models.clone();
    let prompt_variants = backend.prompt_variants.clone();
    backend.pending_selections.lock().await.insert(
        "ses_test".into(),
        (Some("openai/gpt-next".into()), Some("high".into())),
    );
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-creation".into());
    entry.variant = Some("low".into());
    seed_entry(&app, entry).await;

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_models.lock().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the prompt never reached the backend"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        prompt_models.lock().await.as_slice(),
        &[None],
        "the stored creation model is not re-sent; the adapter pending is"
    );
    assert_eq!(
        prompt_variants.lock().await.as_slice(),
        &[None],
        "the stored /think level is not re-sent either"
    );
}

/// Spec #922 gate fix: an unaccepted effort must not become the current mirror,
/// and a turn that leaves the adapter pending (a fold/steer or newer pick) must
/// not move it either.
#[tokio::test]
async fn a_pending_effort_does_not_move_the_mirror_while_the_adapter_holds_it() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    let prompt_models = backend.prompt_models.clone();
    let pending = backend.pending_selections.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-accepted".into());
    entry.variant = Some("low".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/think high",
        key(),
        "msg_think_pending",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .and_then(|entry| entry.variant.clone())
            .as_deref(),
        Some("low"),
        "an unaccepted /think leaves the mirror at the last accepted level"
    );

    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_models.lock().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the prompt never reached the backend"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .and_then(|entry| entry.variant.clone())
            .as_deref(),
        Some("low"),
        "a turn that leaves the pending in place does not move the mirror"
    );
    assert_eq!(
        pending
            .lock()
            .await
            .get("ses_test")
            .and_then(|(_, variant)| variant.clone())
            .as_deref(),
        Some("high"),
        "the adapter still holds the pending effort"
    );
}

/// Issue #910, item 3: two competing durable picks. The first is slow (parked
/// inside its `switch_session_model`), the second queues at the session's
/// switch point — so the newer one applies **after** the older one, and both
/// the server and the mirror end on it (never server = old, mirror = new).
#[tokio::test]
async fn two_competing_durable_picks_keep_the_server_and_mirror_in_step() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection(
        "ses_test",
        crate::backend::types::SessionSelection {
            model: Some(crate::backend::types::ModelInfo {
                id: "gpt-5.1-codex".into(),
                provider_id: "openai".into(),
                variant: None,
            }),
            agent: None,
        },
    );
    let gate = backend.hold_session_switch();
    let entered = backend.switch_model_gate_entered.clone();
    let selections = backend.session_selections.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let spawn_pick = |model: &'static str| {
        let app = app.clone();
        tokio::spawn(async move {
            send_command_in(
                &app,
                model,
                key(),
                "msg_model",
                crate::config::ConversationKind::P2p,
            )
            .await;
        })
    };
    let first = spawn_pick("/model openai/model-a");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first switch never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let second = spawn_pick("/model openai/model-b");
    let switch = app.session_gates.gate("ses_test");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while switch.switch_waiting() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second pick never queued at the switch point"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        entered.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second pick does not enter the server while the first is in flight"
    );

    gate.add_permits(1);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second switch never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    gate.add_permits(1);
    first.await.expect("the first joins");
    second.await.expect("the second joins");

    assert_eq!(
        selections
            .lock()
            .await
            .get("ses_test")
            .and_then(|selection| selection.model.as_ref())
            .map(|model| model.id.as_str()),
        Some("model-b"),
        "the newest pick is the server's selection"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/model-b"),
        "and the mirror agrees with the server"
    );
}

/// Issue #910, item 1: a pick classified against one Pending Session must not
/// land on its **replacement**. `/new` (`set_pending`) takes a new identity,
/// so a stale pair is refused even when the replacement's model/variant match
/// the snapshot exactly (the case the model/variant compare-and-set cannot see).
#[tokio::test]
async fn a_pending_replaced_during_the_pick_is_not_overwritten() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_models(vec![crate::backend::types::ProviderModels {
            provider: PROVIDER.into(),
            models: vec![model_option(MODEL, &["low"])],
        }]);
    // The `/think` clear of the pick's catalog lookup is the parking point.
    let gate = backend.hold_model_surface();
    let entered = backend.model_surface_gate_entered.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;
    {
        let mut store = app.sessions.lock().await;
        let pending = store.pending_for(&key()).cloned().expect("the pending is seeded");
        let mut pending = pending;
        pending.variant = Some("high".into());
        store.set_pending(pending).expect("re-declare with a variant");
    }
    let epoch_before = app.sessions.lock().await.pending_epoch_for(&key());

    let pick = tokio::spawn({
        let app = app.clone();
        async move {
            send_command_in(
                &app,
                "/model deepseek/deepseek-v4-pro",
                key(),
                "msg_model",
                crate::config::ConversationKind::P2p,
            )
            .await;
        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pick's catalog lookup never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // `/new` replaces the pending with one that carries the same model/variant:
    // only the identity tells them apart.
    let mut replacement = PendingEntry::new(key(), "/work/cola");
    replacement.variant = Some("high".into());
    seed_pending(&app, replacement).await;
    assert!(
        app.sessions.lock().await.pending_epoch_for(&key()) != epoch_before,
        "the replacement takes a new identity"
    );
    gate.add_permits(1);
    pick.await.expect("the pick joins");

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(reply.contains("状态已变化"), "the stale pick is refused: {reply}");
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the replacement survives");
    assert_eq!(pending.model, None, "the replacement keeps its model");
    assert_eq!(
        pending.variant.as_deref(),
        Some("high"),
        "and its thinking level (the stale clear did not run)"
    );
}

/// Issue #910, item 2: on a durable generation the pre-flight takes the
/// session's switch point before its read, so a prompt cannot submit while a
/// `switch_session_model` is still in flight — it waits, then runs the settled
/// selection.
#[tokio::test]
async fn a_prompt_waits_for_an_in_flight_durable_switch() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection(
        "ses_test",
        crate::backend::types::SessionSelection {
            model: Some(crate::backend::types::ModelInfo {
                id: "gpt-5.1-codex".into(),
                provider_id: "openai".into(),
                variant: None,
            }),
            agent: None,
        },
    );
    let gate = backend.hold_session_switch();
    let entered = backend.switch_model_gate_entered.clone();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let pick = tokio::spawn({
        let app = app.clone();
        async move {
            send_command_in(
                &app,
                "/model openai/model-b",
                key(),
                "msg_model",
                crate::config::ConversationKind::P2p,
            )
            .await;
        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the durable switch never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    prompt(&app, "msg_prompt", "hi");
    let switch = app.session_gates.gate("ses_test");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while switch.switch_waiting() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pre-flight never queued at the switch point"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        prompt_models.lock().await.is_empty(),
        "the prompt is not submitted while the switch is in flight"
    );

    gate.add_permits(1);
    pick.await.expect("the pick joins");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_models.lock().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the prompt was never submitted after the switch settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        prompt_models.lock().await.last().cloned().flatten(),
        Some("openai/model-b".to_string()),
        "the prompt runs the settled selection"
    );
}

/// Issue #910, item 4 (standards): `/autoaccept` writes only its own field. A
/// recorded pair lands while the command's approval sweep is parked, and the
/// whole-snapshot write must not restore the model it read before that.
#[tokio::test]
async fn a_delayed_autoaccept_keeps_a_recorded_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.ask_permissions(vec![crate::backend::types::PermissionRequest {
        request_id: "per_pending".into(),
        session_id: Some("ses_test".into()),
        permission: Some("bash".into()),
        patterns: vec!["ls -la".into()],
        metadata: None,
        always: Vec::new(),
        always_supported: true,
    }]);
    let gate = backend.hold_reply_permission();
    let entered = backend.reply_permission_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.directory = "/work/cola".into();
    seed_entry(&app, entry).await;

    let autoaccept = tokio::spawn({
        let app = app.clone();
        async move {
            send_command_in(
                &app,
                "/autoaccept on",
                key(),
                "msg_cmd",
                crate::config::ConversationKind::P2p,
            )
            .await;
        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the approval sweep never reached the backend"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // An ordinary `/model` pick lands while the command waits.
    let outcome = app
        .sessions_handle()
        .pick_model_expecting(
            &app.client,
            &key(),
            Some("ses_test"),
            "deepseek/deepseek-v4-pro",
            None,
        )
        .await
        .expect("the record runs");
    assert_eq!(
        outcome,
        crate::bridge::handles::PickOutcome::Applied {
            cleared_variant: None
        }
    );
    gate.add_permits(1);
    autoaccept.await.expect("the command joins");

    let active = app
        .sessions
        .lock()
        .await
        .get_active(&key())
        .cloned()
        .expect("the session stays mapped");
    assert_eq!(
        active.model.as_deref(),
        Some("deepseek/deepseek-v4-pro"),
        "the stale snapshot did not restore the old model"
    );
    assert!(active.auto_accept, "the command's own field was written");
}

/// Issue #910, item 1: a durable switch the server accepts and then never
/// answers must fail by name inside the bridge's total bound, releasing the
/// switch point — so the next prompt still goes through instead of hanging.
#[tokio::test]
async fn an_unanswered_durable_switch_fails_by_name_and_releases_the_point() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection(
        "ses_test",
        crate::backend::types::SessionSelection {
            model: Some(crate::backend::types::ModelInfo {
                id: "gpt-5.1-codex".into(),
                provider_id: "openai".into(),
                variant: None,
            }),
            agent: None,
        },
    );
    backend.hang_next_switch();
    let prompt_models = backend.prompt_models.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model openai/model-b",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reply.contains("没有得到服务器回答"),
        "the switch fails by name: {reply}"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/gpt-5.1-codex"),
        "the failed switch claims nothing"
    );

    // The point is released: the next prompt is submitted normally.
    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_models.lock().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the next prompt was wedged behind the dead switch"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        prompt_models.lock().await.last().cloned().flatten(),
        Some("openai/gpt-5.1-codex".to_string()),
        "it runs the selection that is still there"
    );
}

/// Issue #910: the switch point is released just before the submit, so a
/// durable switch that lands while the turn is being submitted is a user action
/// applied from the **next** turn — the submit still runs the snapshot it read.
#[tokio::test]
async fn a_switch_landing_during_a_submit_applies_from_the_next_turn() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection(
        "ses_test",
        crate::backend::types::SessionSelection {
            model: Some(crate::backend::types::ModelInfo {
                id: "gpt-5.1-codex".into(),
                provider_id: "openai".into(),
                variant: None,
            }),
            agent: None,
        },
    );
    let prompt_gate = backend.hold_prompts();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    // The first turn reaches the submit (the point is already released) and
    // parks there.
    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_models.lock().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the prompt never reached the submit"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // A switch lands while the turn is in flight: it applies from the next
    // turn, and it is not blocked by the submit.
    send_command_in(
        &app,
        "/model openai/model-b",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/model-b"),
        "the switch lands during the submit"
    );
    assert_eq!(
        prompt_models.lock().await.as_slice(),
        &[Some("openai/gpt-5.1-codex".to_string())],
        "the submitted turn runs the snapshot it read"
    );

    // Releasing the submit ends the first turn; the next one runs the switch.
    prompt_gate.add_permits(1);
    prompt(&app, "msg_prompt2", "hi again");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while prompt_models.lock().await.len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second turn was never submitted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        prompt_models.lock().await.last().cloned().flatten(),
        Some("openai/model-b".to_string()),
        "the next turn runs the switch"
    );
    prompt_gate.add_permits(1);
}

/// Issue #910, item 1: a stalled catalog under the switch point is bounded, so
/// the prompt queued behind it is not wedged — it waits at most the catalog's
/// bound and then runs.
#[tokio::test]
async fn a_stalled_catalog_under_the_point_does_not_wedge_later_waiters() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_session_selection(
            "ses_test",
            crate::backend::types::SessionSelection {
                model: Some(crate::backend::types::ModelInfo {
                    id: "gpt-5.1-codex".into(),
                    provider_id: "openai".into(),
                    variant: Some("high".into()),
                }),
                agent: None,
            },
        )
        .with_models(vec![crate::backend::types::ProviderModels {
            provider: "openai".into(),
            models: vec![model_option("model-b", &["low"])],
        }]);
    // The catalog never answers; the pick's own bound must end the hold.
    let _catalog = backend.hold_model_surface();
    let entered = backend.model_surface_gate_entered.clone();
    let prompt_models = backend.prompt_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    let pick = tokio::spawn({
        let app = app.clone();
        async move {
            send_command_in(
                &app,
                "/model openai/model-b",
                key(),
                "msg_model",
                crate::config::ConversationKind::P2p,
            )
            .await;
        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pick's catalog read never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // A prompt waits behind the point; the catalog bound releases it.
    prompt(&app, "msg_prompt", "hi");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while prompt_models.lock().await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the prompt was wedged behind the stalled catalog"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    pick.await.expect("the pick joins");
    let switch = app.session_gates.gate("ses_test");
    assert_eq!(
        switch.waiting(),
        0,
        "no phantom waiter is left behind by a bounded acquire"
    );
    assert_eq!(switch.switch_waiting(), 0);
}

/// Issue #910, item 3: `/autoaccept` with a Pending Session target carries the
/// epoch it read, so a `/new` that replaces the pending is a refusal instead of
/// a flag on the replacement.
#[tokio::test]
async fn autoaccept_on_a_replaced_pending_is_refused() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    let (_, epoch) = app
        .sessions_handle()
        .settings_and_pending_epoch(&key())
        .await
        .expect("the pending is the target");
    let epoch = epoch.expect("a pending carries its epoch");

    // `/new` replaces the pending while the command's write is on its way.
    seed_pending(&app, PendingEntry::new(key(), "/work/other")).await;
    let applied = app
        .sessions_handle()
        .set_auto_accept(&key(), None, epoch, true)
        .await
        .expect("the write runs");
    assert!(!applied, "the displaced pending is refused");
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the replacement survives");
    assert!(!pending.auto_accept, "the replacement is untouched");

    // The replacement's own identity applies.
    let (_, epoch) = app
        .sessions_handle()
        .settings_and_pending_epoch(&key())
        .await
        .expect("the replacement is the target");
    assert!(
        app.sessions_handle()
            .set_auto_accept(&key(), None, epoch.expect("an epoch"), true)
            .await
            .expect("the write runs")
    );
    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the pending survives");
    assert!(pending.auto_accept);
}

/// Issue #910 (adversarial): a V2 switch the server **applied** before its
/// answer was lost is ambiguous, not a failure. The bounded reconciliation read
/// finds the attempted selection and records the pick as applied — server and
/// mirror agree.
#[tokio::test]
async fn an_applied_switch_whose_response_is_lost_is_reconciled() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection(
        "ses_test",
        crate::backend::types::SessionSelection {
            model: Some(crate::backend::types::ModelInfo {
                id: "gpt-5.1-codex".into(),
                provider_id: "openai".into(),
                variant: None,
            }),
            agent: None,
        },
    );
    backend.hang_after_apply();
    let selections = backend.session_selections.clone();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model openai/model-b",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reply.contains("Model: openai/model-b"),
        "the reconciled pick is reported as applied: {reply}"
    );
    assert_eq!(
        selections
            .lock()
            .await
            .get("ses_test")
            .and_then(|selection| selection.model.as_ref())
            .map(|model| model.id.as_str()),
        Some("model-b"),
        "the server holds the change"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .model
            .as_deref(),
        Some("openai/model-b"),
        "the mirror was reconciled to the server"
    );
}

/// Issue #910 (adversarial): when the reconcile read shows neither the attempt
/// nor the previous selection (a peer moved it, or the read failed), the mirror
/// is **forgotten** and the failure is named — never a stale mirror claimed as
/// truth, never a success.
#[tokio::test]
async fn an_unknown_switch_outcome_forgets_the_mirror() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection(
        "ses_test",
        crate::backend::types::SessionSelection {
            model: Some(crate::backend::types::ModelInfo {
                id: "gpt-5.1-codex".into(),
                provider_id: "openai".into(),
                variant: None,
            }),
            agent: None,
        },
    );
    backend.hang_after_apply();
    backend.then_the_selection_becomes(crate::backend::types::ModelInfo {
        id: "third-model".into(),
        provider_id: "other".into(),
        variant: None,
    });
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model openai/model-b",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reply.contains("切换结果未知"),
        "the failure names the unknown outcome: {reply}"
    );
    let active = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(
        active.model, None,
        "the mirror claims nothing after an unconfirmed switch"
    );
    assert_eq!(
        active.variant, None,
        "nor a variant that belonged to the old model"
    );
}

/// Issue #910 (adversarial), the `/think` side of the same ambiguity: the
/// server applied the new level before the answer was lost, and the reconcile
/// records it instead of reporting a failure.
#[tokio::test]
async fn an_applied_think_whose_response_is_lost_is_reconciled() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_session_selection(
        "ses_test",
        crate::backend::types::SessionSelection {
            model: Some(crate::backend::types::ModelInfo {
                id: "gpt-5.1-codex".into(),
                provider_id: "openai".into(),
                variant: None,
            }),
            agent: None,
        },
    );
    let selections = backend.session_selections.clone();
    // The mock applies the switch (the new level) and then goes silent.
    backend.hang_after_apply();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/think high",
        key(),
        "msg_think",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reply.contains("Thinking: high"),
        "the reconciled level is reported as applied: {reply}"
    );
    assert_eq!(
        selections
            .lock()
            .await
            .get("ses_test")
            .and_then(|selection| selection.model.as_ref())
            .and_then(|model| model.variant.as_deref()),
        Some("high"),
        "the server holds the level"
    );
    assert_eq!(
        app.sessions
            .lock()
            .await
            .get_active(&key())
            .cloned()
            .unwrap()
            .variant
            .as_deref(),
        Some("high"),
        "the mirror was reconciled"
    );
}

/// Issue #910 (adversarial, precise): the initial selection read times out, so
/// the previous state was **never observed**. The switch then times out, and the
/// reconciliation read answers "no model" — which must be `Unknown`, never
/// "unchanged" (a lost read is not the same fact as a model-less selection).
/// The mirror is forgotten, and the outcome is named unknown.
#[tokio::test]
async fn an_unobserved_previous_and_a_model_less_read_are_unknown() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    // The server has a selection, but no model: the reconciliation answer.
    backend.with_session_selection("ses_test", crate::backend::types::SessionSelection::default());
    // The initial read never answers …
    backend.hang_next_session_selection();
    // … and the switch never answers either; neither state is known.
    backend.hang_next_switch();
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-5.1-codex".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    send_command_in(
        &app,
        "/model openai/model-b",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let reply = platform
        .calls
        .lock()
        .await
        .iter()
        .filter_map(|call| match call {
            PlatformCall::ReplyText { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        reply.contains("切换结果未知"),
        "an unobserved previous + a model-less answer is unknown: {reply}"
    );
    let active = app.sessions.lock().await.get_active(&key()).cloned().unwrap();
    assert_eq!(
        active.model, None,
        "the mirror is forgotten, never left claiming the old model"
    );
    assert_eq!(active.variant, None);
}

/// Issue #908 / spec #930: a provider id may itself contain `/` (a quoted TOML
/// key). The text form's quoted provider (`"foo/bar"/gpt`) round-trips to
/// (`foo/bar`, `gpt`) — never `foo` + `bar/gpt` — and a Pending Session caches
/// that provider so the same prefix is accepted while another is refused.
#[tokio::test]
async fn a_slashed_provider_id_round_trips_through_the_text_form() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_configured_provider(Some("foo/bar"));
    let picks = backend.created_session_models.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    seed_pending(&app, PendingEntry::new(key(), "/work/cola")).await;

    send_command_in(
        &app,
        "/model \"foo/bar\"/gpt",
        key(),
        "msg_model",
        crate::config::ConversationKind::P2p,
    )
    .await;

    let pending = app
        .sessions
        .lock()
        .await
        .pending_for(&key())
        .cloned()
        .expect("the model is recorded");
    let reference = pending.model.clone().expect("the model ref");
    let parsed = crate::backend::parsing::parse_model(&reference).expect("the ref parses");
    assert_eq!(parsed.provider_id, "foo/bar", "the whole provider id survives");
    assert_eq!(parsed.id, "gpt", "and the model is not `bar/gpt`");

    app.handle_message(incoming(
        "msg_prompt".into(),
        "chat_1".into(),
        "p2p".into(),
        None,
        "hi".into(),
        None,
    ))
    .await;
    assert_eq!(
        picks.lock().await.as_slice(),
        &[Some("gpt".to_string())],
        "the creation carries the model only"
    );
}

/// Spec #922 decision 2: `/status` shows the live thread's current model and
/// the adapter's next-turn pending when one exists.
#[tokio::test]
async fn status_shows_the_live_current_and_pending_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    backend.pending_selections.lock().await.insert(
        "ses_test".into(),
        (Some("openai/gpt-new".into()), Some("high".into())),
    );
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-current".into());
    seed_entry(&app, entry).await;

    let text = command_text(&app, &platform, "/status", "msg_status").await;
    assert!(text.contains("当前：openai/gpt-current"), "{text}");
    assert!(
        text.contains("待应用：openai/gpt-new@high（下一条新消息生效）"),
        "{text}"
    );
    assert!(text.contains("会话：`ses_test`"), "{text}");
}

/// Spec #922 gate review: an older pick whose catalog lookup finishes after a
/// newer pick started must not overwrite it.
#[tokio::test]
async fn a_later_model_pick_wins_over_a_slower_earlier_lookup() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_models(vec![crate::backend::types::ProviderModels {
            provider: "openai".into(),
            models: vec![
                model_option("model-a", &["low"]),
                model_option("model-b", &["low"]),
            ],
        }]);
    let gate = backend.hold_next_model_surface();
    let entered = backend.model_surface_gate_entered.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/model-old".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    let handles = app.turn_handles();
    let sessions = handles.sessions.clone();
    let backend = handles.backend.clone();
    let earlier = tokio::spawn({
        let sessions = sessions.clone();
        let backend = backend.clone();
        async move {
            sessions
                .pick_model_expecting(&backend, &key(), Some("ses_test"), "openai/model-a", None)
                .await
        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while entered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the earlier catalog lookup never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The later pick records while the earlier lookup is still parked.
    let later = sessions
        .pick_model_expecting(&backend, &key(), Some("ses_test"), "openai/model-b", None)
        .await
        .expect("the later pick");
    assert!(
        matches!(later, crate::bridge::handles::PickOutcome::Applied { .. }),
        "the later pick applies: {later:?}"
    );
    assert_eq!(
        backend.pending_model("ses_test").await.as_deref(),
        Some("openai/model-b@high")
    );

    gate.add_permits(1);
    let earlier = earlier
        .await
        .expect("the earlier pick joins")
        .expect("the earlier pick result");
    assert_eq!(
        earlier,
        crate::bridge::handles::PickOutcome::StateChanged,
        "the stale earlier pick records nothing"
    );
    assert_eq!(
        backend.pending_model("ses_test").await.as_deref(),
        Some("openai/model-b@high"),
        "the newer pick still owns the pending"
    );
}

/// Spec #922 gate review: after a pending model is accepted the thread fact is
/// the model the session runs, so `/status` reports it rather than the mirror
/// that still names the previous pick.
#[tokio::test]
async fn status_after_an_accepted_pick_reports_the_thread_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend
        .with_codex_capabilities()
        .with_session_model(crate::backend::types::SessionModel {
            provider_id: "openai".into(),
            id: "gpt-accepted".into(),
        });
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-stale".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    let text = command_text(&app, &platform, "/status", "msg_status").await;
    assert!(
        text.contains("当前：openai/gpt-accepted@high"),
        "the accepted thread model is current: {text}"
    );
    assert!(!text.contains("gpt-stale"), "{text}");
    assert!(!text.contains("待应用"), "{text}");
}

/// Spec #922 gate review: an effort-only pending with no thread model fact has
/// no composed reference, but `/status` still names the level; a reset-only
/// marker names the clear.
#[tokio::test]
async fn status_shows_an_effort_only_pending_without_a_thread_model() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    backend
        .pending_selections
        .lock()
        .await
        .insert("ses_test".into(), (None, Some("high".into())));
    let pending = backend.pending_selections.clone();
    let (app, platform) = build_app(cfg, backend).await;
    seed_entry(&app, entry()).await;

    let text = command_text(&app, &platform, "/status", "msg_status_effort").await;
    assert!(
        text.contains("当前：未知"),
        "there is no thread model fact: {text}"
    );
    assert!(
        text.contains("待应用：思考等级 high（下一条新消息生效）"),
        "an effort-only pending names its level: {text}"
    );

    pending.lock().await.insert("ses_test".into(), (None, None));
    let text = command_text(&app, &platform, "/status", "msg_status_clear").await;
    assert!(
        text.contains("待应用：清除思考等级（下一条新消息生效）"),
        "a reset-only pending names the clear: {text}"
    );
}

/// A Pending Session's `/status` shows its creation model and never 待应用.
#[tokio::test]
async fn status_shows_a_pending_sessions_creation_model_only() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    let (app, platform) = build_app(cfg, backend).await;
    let mut pending = PendingEntry::new(key(), "/work/cola");
    pending.model = Some("openai/gpt-creation".into());
    pending.variant = Some("high".into());
    seed_pending(&app, pending).await;

    let text = command_text(&app, &platform, "/status", "msg_status").await;
    assert!(
        text.contains("当前：openai/gpt-creation@high（将在会话创建时生效）"),
        "{text}"
    );
    assert!(!text.contains("待应用"), "{text}");
    assert!(text.contains("会话：待创建"), "{text}");
}

/// No target session: the existing no-session phrasing.
#[tokio::test]
async fn status_without_a_session_says_so() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, platform) = build_app(cfg, backend).await;

    let text = command_text(&app, &platform, "/status", "msg_status").await;
    assert!(text.contains("还没有会话"), "{text}");
}

/// OpenCode has a current line and no pending line, even with a store variant.
#[tokio::test]
async fn status_on_opencode_shows_current_only() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("sessions.json"));
    let backend = MockBackend::new(realistic_parts());
    let (app, platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.model = Some("openai/gpt-current".into());
    entry.variant = Some("high".into());
    seed_entry(&app, entry).await;

    let text = command_text(&app, &platform, "/status", "msg_status").await;
    assert!(text.contains("当前：openai/gpt-current@high"), "{text}");
    assert!(!text.contains("待应用"), "{text}");
}

/// Spec #922 post-review: an applied effort is removed only after the mirror
/// write succeeds. A failed write leaves the record for the next commit.
#[tokio::test]
async fn a_failed_mirror_write_keeps_the_applied_effort_for_a_retry() {
    let _wd = test_work_dir();
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions.json");
    let cfg = test_config(&sessions);
    let mut backend = MockBackend::new(realistic_parts());
    backend.with_codex_capabilities();
    backend
        .applied_efforts
        .lock()
        .await
        .insert("ses_test".into(), Some("high".into()));
    let applied = backend.applied_efforts.clone();
    let (app, _platform) = build_app(cfg, backend).await;
    let mut entry = entry();
    entry.variant = Some("low".into());
    seed_entry(&app, entry).await;
    let handles = app.turn_handles();
    let key = key();

    // Make the store path unwritable: a directory cannot be replaced by the
    // writer's atomic rename.
    std::fs::remove_file(&sessions).unwrap();
    std::fs::create_dir(&sessions).unwrap();
    crate::bridge::turn::Turn::commit_applied_effort(&handles, &key, "ses_test").await;
    assert_eq!(
        applied.lock().await.get("ses_test").cloned().flatten(),
        Some("high".into()),
        "a failed mirror write keeps the applied record"
    );

    std::fs::remove_dir(&sessions).unwrap();
    crate::bridge::turn::Turn::commit_applied_effort(&handles, &key, "ses_test").await;
    assert!(
        applied.lock().await.get("ses_test").is_none(),
        "a successful retry removes the applied record"
    );
    let reloaded =
        crate::bridge::session::SessionStore::for_client(sessions, crate::config::DEFAULT_CLIENT_ID)
            .expect("the store reloads");
    assert_eq!(
        reloaded
            .get_active(&key)
            .and_then(|entry| entry.variant.clone())
            .as_deref(),
        Some("high"),
        "the retry persisted the applied level"
    );
}
