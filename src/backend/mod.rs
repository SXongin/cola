//! The backend contract: the traits the Bridge calls and the neutral read model
//! it consumes (ADR-0010, ADR-0053).
//!
//! The seam's read side carries a [`SessionTranscript`] — typed messages and
//! typed parts — instead of backend wire JSON. Backend protocol field names
//! for that read live only in the adapter's private decoders, so a protocol
//! change is an adapter change, not a Bridge-and-card change. The public
//! session/permission/provider/agent DTOs in [`crate::backend::types`] are
//! the known out-of-scope exception (spec #332): they still spell the
//! list/read wire fields, and the session list's raw `model` payload is
//! normalized where it is displayed (`bridge::display::model_display`).
//!
//! Every caller imports these traits from here, never from the adapter; the
//! OpenCode HTTP adapter and the test mock implement them. The DTOs the trait
//! speaks live in [`types`], beside it (spec #860, #870).

// #801 (the turn card's button) and #803 (repository review) are this
// generator's consumers; until one lands, nothing in the binary names it and a
// bin crate's dead-code lint would flag the whole module.
#[allow(dead_code)]
pub mod diff_html;
pub mod file_content;
pub mod lifecycle;
pub mod parsing;
pub mod transcript;
pub mod turn_diff;
pub mod types;

pub(crate) use lifecycle::wait_for_server_ready;
pub use lifecycle::{AttachOutcome, EnsureAttached, RestartOutcome, ServerLifecycle};
pub use types::{PERMISSION_PATTERN_MAX_CHARS, permission_target_budget};

// The neutral views are re-exported at the contract root: consumers import
// them from here, never from the decoder's module path.
// No consumer lands in the generator's own ticket; the re-export is the
// contract root #801/#803 import from, not dead surface.
#[allow(unused_imports)]
pub use diff_html::*;
pub use file_content::*;
pub use transcript::*;
pub use turn_diff::*;

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;

use types::{
    AgentInfo, CreateSessionInput, FormAnswer, ModelInfo, ModelSurface, ModelSurfaceQuery, PermissionRequest,
    PromptFile, ProviderModels, QuestionRequest, Session, SessionInfo, SessionListInfo, SessionSelection,
    SessionStatus,
};

/// One skill a caller attaches to a prompt (spec #652): a small generation-neutral
/// `{ id, name }` pair. `id` is the skill's identity in the attached generation's
/// own space — V2's `Skill.ID`, the only field its structured prompt `skills`
/// request carries — while `name` is the human-facing name. V1 has neither the
/// field nor a skill prompt part, so its strategy folds `name` into the prompt
/// text; the loaded-skill fold renders `name` too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSkill {
    pub id: String,
    pub name: String,
}

/// One registered skill as the skill-list read exposes it (spec #652, tickets
/// #656/#655): a small generation-neutral `{ id, name, description?, content? }`.
/// `id` is the skill's identity in the attached generation's own space — V2's
/// `Skill.ID` (what the structured prompt `skills` request carries and what the
/// picker sends back), while on V1, whose `Skill.Info` has no id, `id` IS
/// `name` (V1 keys skills by name and its `skill` tool loads by name). `name`
/// is the human-facing name; `description` is the frontmatter description when
/// the skill declares one. `content` is the skill's own markdown as the list
/// route reports it — the body the dedicated loaded-skill card's fold shows
/// (raw, with no `<skill_content>` envelope to unwrap) — and `None` when the
/// route carried none. The read is deliberately unfiltered: a hidden
/// (`disable-model-invocation`) or description-less skill is listed too, since
/// the whole point of the picker is to reach exactly the skills the model never
/// advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub content: Option<String>,
}

/// A directory-scoped handle to the backend. Instance routing lives here: the
/// handle carries the directory, so a caller cannot silently omit `?directory=`
/// and scope a request to the wrong server instance (ADR-0010). Directory-scoped
/// calls (permissions, questions, session info) take no directory argument — the
/// handle owns it.
#[async_trait]
pub trait DirectoryBackend: Send + Sync {
    async fn list_permissions(&self) -> Result<Vec<PermissionRequest>>;

    async fn reply_permission(&self, session_id: &str, request_id: &str, reply: &str) -> Result<()>;

    async fn list_questions(&self) -> Result<Vec<QuestionRequest>>;

    async fn reply_question(&self, session_id: &str, request_id: &str, answers: &[FormAnswer]) -> Result<()>;

    async fn reject_question(&self, session_id: &str, request_id: &str) -> Result<()>;

    async fn session_info(&self, session_id: &str) -> Result<SessionInfo>;
}

/// The single concrete [`DirectoryBackend`]: wraps any [`Backend`] and forwards
/// the carried directory into its directory-scoped methods. Both the real
/// OpenCode adapter and test mocks produce this via [`Backend::for_directory`],
/// so the directory-scoped seam is implemented once.
pub struct BackendDirectory {
    backend: Arc<dyn Backend>,
    directory: String,
}

impl BackendDirectory {
    pub(crate) fn new(backend: Arc<dyn Backend>, directory: String) -> Self {
        Self { backend, directory }
    }
}

#[async_trait]
impl DirectoryBackend for BackendDirectory {
    async fn list_permissions(&self) -> Result<Vec<PermissionRequest>> {
        self.backend.list_permissions(Some(&self.directory)).await
    }

    async fn reply_permission(&self, session_id: &str, request_id: &str, reply: &str) -> Result<()> {
        self.backend
            .reply_permission(session_id, request_id, reply, Some(&self.directory))
            .await
    }

    async fn list_questions(&self) -> Result<Vec<QuestionRequest>> {
        self.backend.list_questions(Some(&self.directory)).await
    }

    async fn reply_question(&self, session_id: &str, request_id: &str, answers: &[FormAnswer]) -> Result<()> {
        self.backend
            .reply_question(session_id, request_id, answers, Some(&self.directory))
            .await
    }

    async fn reject_question(&self, session_id: &str, request_id: &str) -> Result<()> {
        self.backend
            .reject_question(session_id, request_id, Some(&self.directory))
            .await
    }

    async fn session_info(&self, session_id: &str) -> Result<SessionInfo> {
        self.backend.session_info(session_id, Some(&self.directory)).await
    }
}

/// The backend, abstracted so tests can drive the bridge with canned responses
/// instead of a live server. The real implementation is the OpenCode adapter;
/// mock implementations feed scripted transcripts/permissions and verify what
/// cola renders from them.
///
/// # The lifecycle seam
///
/// A client also owns how it is attached to its runtime — discovery, attach,
/// health and an optional start all live behind [`ServerLifecycle`], which
/// this trait extends. The bridge's poll loop has one call
/// ([`ServerLifecycle::ensure_attached`]) and one failure signal
/// ([`AttachOutcome`]); the OpenCode-specific yield-and-reap policy stays in
/// the adapter (spec #860, ticket #875, ADR-0090).
///
/// # The core, and the surfaces a client may lack
///
/// Some methods describe one runtime's own surface, not a fact every agent
/// client has. They carry a default body stating **this client has no such
/// surface**, in the vocabulary the trait already uses: `Ok(None)`, `false`,
/// the no-op `Ok(())`, `Ok(TaskRuntime::default())`,
/// `Ok(ChildEvidence::Unfinished)`. The thirteen are
/// [`Self::session_selection`], [`Self::session_last_run_model`],
/// [`Self::switch_session_model`], [`Self::switch_session_agent`],
/// [`Self::keeps_session_selection`], [`Self::reuse_continues_an_admitted_turn`],
/// [`Self::resume_supported`], [`Self::resume`], [`Self::session_diff`],
/// [`Self::task_runtime`], [`Self::background_tasks_supported`],
/// [`Self::shell_output`] and [`Self::child_evidence`].
///
/// One more default is structural rather than a missing surface:
/// [`Self::is_addressable_session_id`] answers the `ses…` id shape both
/// OpenCode generations mint, and a client whose ids have another shape (Codex
/// thread UUIDs) overrides it so its requests are not filtered out as
/// unaddressable.
///
/// A future adapter (spec #860) is expected to leave all thirteen defaulted when
/// the runtime it attaches to publishes no equivalent surface, and to override
/// the ones it does publish; the rest of the trait — session lifecycle,
/// [`Self::prompt`], [`Self::transcript`], permissions/questions,
/// [`Self::session_status`], the pickers, the tasks handle and the
/// directory handle — is the core it must always implement.
///
/// The distinction is the trait's throughout: a default shape means *not
/// applicable to this client*, never that a read failed. A client that has the
/// surface overrides the method, answers the default shape only where the read
/// or write genuinely has nothing to report, and reports a failure as an
/// `Err`. A default must never look like a failed read.
#[async_trait]
pub trait Backend: ServerLifecycle + Send + Sync {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput;

    /// Create a session. A client whose provider is a **creation fact**
    /// validates the pair here, before the create request: Codex runs one
    /// provider per process, so a thread on another provider would be refused
    /// at its first turn (spec #922).
    async fn create_session(&self, input: &CreateSessionInput) -> Result<Session>;

    /// List every session in the shared store (canonical `GET /session`).
    async fn list_sessions(&self) -> Result<Vec<SessionListInfo>>;

    /// Rename a session server-side (`PATCH /session/{id}` with a title).
    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<()>;

    /// Delete a session server-side. V2 cascades to the session's child
    /// sessions and answers 204; V1 deletes the one session (boolean body).
    /// Un-mapping a chat from a session is a separate, local act.
    ///
    /// No Bridge command deletes a session yet (`/switch forget` deliberately
    /// keeps the server session); the capability completes the session surface
    /// across generations (spec #364, S4a) and is pinned by the wire tests.
    /// This is the capability's only `#[allow(dead_code)]`: the allow keeps
    /// this trait method live, and the adapter and strategy methods are
    /// reachable through it.
    #[allow(dead_code)]
    async fn delete_session(&self, session_id: &str) -> Result<()>;

    /// The bridge no longer mirrors this session: the client may free whatever
    /// it retained to serve it — a push subscription, its fold of that stream.
    /// The server session itself **stays**: releasing is local mirror teardown,
    /// never a delete, and a released session can be adopted again later (the
    /// next read re-subscribes).
    ///
    /// The bridge calls this wherever a chat stops owning a session —
    /// `/switch forget`, a switch that displaces the previous one, a steal, a
    /// dead mapping being recreated — and treats every call as best-effort: a
    /// failure is logged, never surfaced to the user's command, because the
    /// local unmap already happened and the server state is untouched.
    ///
    /// The default is the no-op `Ok(())`: a client whose reads are stateless
    /// retains nothing to release, so the no-op is **not applicable to this
    /// client**. A client that does retain a subscription overrides this and
    /// answers `Err` when the release itself failed.
    async fn release_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    /// Submit one prompt: the message is persisted and a run is scheduled,
    /// then the call returns — it does NOT block until the turn finishes. How
    /// a generation sends a prompt does not change the contract: V1 uses the
    /// native fire-and-forget route, V2 its admit-then-return prompt.
    ///
    /// The Turn observes the submitted turn's completion from the transcript
    /// and the run state ([`Self::transcript`] + [`Self::session_status`]);
    /// a failure that cannot be observed there (a rejected submit, a transport
    /// error) surfaces as the call's `Err`.
    ///
    /// `model` is the per-session `/model` override (parsed "provider/model");
    /// None → the configured default applies, and if that's also unset the
    /// server uses its own default model.
    ///
    /// `variant` is the per-session `/think` override; None → the server's
    /// default variant (unset) for whatever model runs this turn.
    ///
    /// `agent` is the per-session `/agent` override; None → the server uses the
    /// session's own/default agent.
    ///
    /// `files` are the prompt's inbound attachments, each attached as a
    /// data-URL part; a non-text one requires a vision-capable model
    /// (unsupported models surface an error).
    ///
    /// `skills` asks the server to load those skills with this prompt. The
    /// attachment is generation-neutral ([`PromptSkill`]); each generation's
    /// strategy owns the wire: V2 emits the prompt body's structured `skills`
    /// array (the server injects each skill's body deterministically), while V1
    /// — which has no field for it — folds the request into the prompt text as
    /// an instruction naming the skill, so the model loads it with its `skill`
    /// tool. An empty slice leaves the prompt untouched on both generations.
    ///
    /// `message_id` is the id cola chose for the user message this prompt will
    /// create (ADR-0026: `msg_cola_` self-identifies cola-authored messages).
    /// A re-post is idempotent either way, but only on a generation where
    /// [`Self::reuse_continues_an_admitted_turn`] is true does it continue the
    /// admitted turn (V1); on V2 the admitted id makes the re-post a no-op.
    /// None falls back to a server-generated id.
    #[allow(clippy::too_many_arguments)] // prompt axes: session/text/files/skills + model/variant/agent/message-id
    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        files: &[PromptFile],
        skills: &[PromptSkill],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()>;

    /// Submit a **Supplement**: merge this message into the Turn already running
    /// on `session_id` instead of starting a second one, so the user can append
    /// context mid-run without `/stop`. The message is persisted and observed
    /// exactly like a prompt's ([`Self::prompt`]); only the wire differs.
    ///
    /// The default delegates to [`Self::prompt`]: on a generation whose server
    /// steers a prompt into the in-flight run (V1's merge, V2's admitted
    /// message), a supplement IS a prompt. A client with a dedicated steer verb
    /// overrides this — Codex's `turn/steer` — so the bridge never has to know
    /// which shape the attached client speaks.
    ///
    /// The remaining arguments carry the same per-turn axes as [`Self::prompt`].
    #[allow(clippy::too_many_arguments)] // the prompt axes, unchanged
    async fn steer(
        &self,
        session_id: &str,
        text: &str,
        files: &[PromptFile],
        skills: &[PromptSkill],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()> {
        self.prompt(session_id, text, files, skills, model, variant, agent, message_id)
            .await
    }

    /// Whether `message_id` names a message **cola itself** wrote into this
    /// Session (ADR-0026), and must therefore never be reported as an External
    /// Message or treated as a Turn boundary.
    ///
    /// The default answers from the id convention every OpenCode generation
    /// echoes back: a message cola submitted carries the `msg_cola_` id it chose
    /// at send time. A client that assigns its own ids — Codex — overrides this
    /// and answers from its own record of the turns it started (ADR-0090's
    /// authorship consequence), which is durable across a restart.
    ///
    /// A client whose record is unusable must fail **closed** (answer `true`):
    /// telling the user about cola's own message is the failure this seam
    /// exists to prevent.
    async fn is_cola_authored(&self, message_id: &str) -> bool {
        crate::backend::parsing::is_cola_message_id(message_id)
    }

    /// The session's durable model/agent selection, where the generation keeps
    /// one server-side (V2's session-scoped switches). `None` on a generation
    /// whose selection rides each prompt (V1: cola's own override is
    /// authoritative, and the server-recorded model is only a display
    /// fallback), and for a Pending Session (no server identity to read).
    ///
    /// This is what the next turn will actually run: shared session state that
    /// another client may have changed, not cola's local mirror.
    ///
    /// The default answers `Ok(None)`: a client that keeps no durable
    /// selection has no such surface, so `None` is **not applicable to this
    /// client**, never a failed read. A client that has the surface overrides
    /// this and still answers `Err` when the read itself fails.
    async fn session_selection(
        &self,
        _session_id: &str,
        _directory: Option<&str>,
    ) -> Result<Option<SessionSelection>> {
        Ok(None)
    }

    /// The model the session last actually ran with — the newest assistant
    /// message's model ref (variant inside it). A durable generation records a
    /// model on the session only after an explicit switch, so for a session
    /// started from the server's own default this message ref is the public
    /// record of what is running; the effective-model ladder reads it as the
    /// last rung before giving up. `None` on V1, whose ladder already reads
    /// the server-recorded session model through `session_info`, and for a
    /// session with no assistant message yet.
    ///
    /// The default answers `Ok(None)`: a client with no assistant-message
    /// model record has no such surface, so `None` is **not applicable to this
    /// client**, never a failed read. A client that has the surface overrides
    /// this and still answers `Err` when the read itself fails.
    async fn session_last_run_model(
        &self,
        _session_id: &str,
        _directory: Option<&str>,
    ) -> Result<Option<ModelInfo>> {
        Ok(None)
    }

    /// Make the session's model selection durable so subsequent turns use it
    /// with nothing re-sent (V2's `POST /api/session/{id}/model`); the variant
    /// rides inside `model`. A no-op on V1, where the selection rides each
    /// prompt.
    ///
    /// The default is a no-op (`Ok(())`): a client whose selection rides each
    /// prompt has no durable switch to make, so the no-op is **not applicable
    /// to this client**. A client that has the switch overrides this and
    /// answers `Err` when the write fails.
    async fn switch_session_model(&self, _session_id: &str, _model: &ModelInfo) -> Result<()> {
        Ok(())
    }

    /// Record a **one-shot next-turn** model for a live session without
    /// changing the session's current selection. Codex has no durable model
    /// switch on its public surface: `turn/start.model` (and `effort`) are
    /// documented to become the thread's defaults for later turns, so the pick
    /// rides the next genuinely **new** turn and is then consumed.
    ///
    /// `variant` is the `/think` level the pick keeps (after ADR-0020's clear
    /// rule); the pending owns it beside the model so one selection reaches the
    /// turn as one pair. Recording is in-memory only — a restart drops it and
    /// the thread's own model stays the fact — because a persisted pending
    /// cannot distinguish an accepted turn whose response was lost from one
    /// that never ran.
    ///
    /// The default is `Ok(())`: a client whose selection rides each prompt has
    /// no next-turn window to record, so recording is **not applicable to this
    /// client**. A client that has the window overrides this and answers `Err`
    /// when the write cannot reach its adapter (for example, no attached
    /// client).
    async fn record_pending_model(
        &self,
        _session_id: &str,
        _model: &str,
        _variant: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }

    /// Record a live model pick only if the provider fact generation used by
    /// validation is still current. A replaced fact makes the channel unknown,
    /// so Codex stores the neutral `<codex>/<model>` ref instead of carrying a
    /// stale provider into the next turn. The lifecycle adapter overrides this
    /// to serialize the generation check with attachment replacement.
    async fn record_pending_model_for_generation(
        &self,
        session_id: &str,
        model: &str,
        variant: Option<&str>,
        expected_generation: Option<u64>,
        current_provider: Option<&str>,
    ) -> Result<()> {
        let (mut model, stale) = crate::backend::parsing::model_spec_for_provider_generation(
            model,
            expected_generation,
            self.provider_fact_generation(),
            self.neutral_provider_id(),
        );
        if !stale
            && let Some(provider) = current_provider
            && let Some(parsed) = crate::backend::parsing::parse_model(&model)
            && self.is_neutral_provider(&parsed.provider_id)
        {
            model = crate::backend::parsing::join_model(provider, &parsed.id);
        }
        self.record_pending_model(session_id, &model, variant).await
    }

    /// Record a **one-shot next-turn** `/think` level, preserving any pending
    /// model. `None` clears the pending effort (the model's own default). A
    /// pending that ends up carrying neither a model nor an effort is dropped.
    ///
    /// The default is `Ok(())`: **not applicable to a client** with no
    /// next-turn window. A client that has one overrides this and answers `Err`
    /// when the write cannot reach its adapter.
    async fn record_pending_variant(&self, _session_id: &str, _variant: Option<&str>) -> Result<()> {
        Ok(())
    }

    /// The model a live session's next new turn will carry, as a
    /// `provider/model` reference, while a pending selection exists. This is
    /// cola state, not the thread's current fact: the model card shows it as
    /// 待应用 until the next new turn is accepted.
    ///
    /// The default answers `None`: **not applicable to a client** with no
    /// next-turn pending. A client that has one overrides this.
    async fn pending_model(&self, _session_id: &str) -> Option<String> {
        None
    }

    /// The `/think` level a live session's next new turn will carry while a
    /// pending selection exists. This is cola state (the pending's effort), not
    /// the session's mirror; a turn captures it at send time for its footer.
    ///
    /// The default answers `None`: **not applicable to a client** with no
    /// next-turn pending. A client that has one overrides this.
    async fn pending_variant(&self, _session_id: &str) -> Option<String> {
        None
    }

    /// Whether a next-turn pending selection exists at all, including an
    /// explicit `/think --reset` marker whose model and effort are both absent.
    /// The bridge uses this one seam before committing an applied effort, so a
    /// clear marker is not mistaken for "no pending".
    ///
    /// The default answers `false`: **not applicable to a client** with no
    /// next-turn pending. A client that has one overrides this.
    async fn has_pending_selection(&self, _session_id: &str) -> bool {
        false
    }

    /// Take the effort an accepted turn actually applied, waiting to be written
    /// to the session mirror. `None` means nothing is waiting; `Some(None)` is
    /// an explicit clear. The adapter records this at acceptance, so a bridge
    /// cancellation after that point leaves it for the next turn.
    ///
    /// The default answers `None`: **not applicable to a client** with no
    /// next-turn pending. A client that has one overrides this.
    async fn take_applied_effort(&self, _session_id: &str) -> Option<Option<String>> {
        None
    }

    /// Peek at the effort an accepted turn applied without removing it. The
    /// bridge reads this, writes the mirror, and only then takes the record —
    /// so a failed mirror write is retried by the next turn.
    ///
    /// The default answers `None`: **not applicable to a client** with no
    /// next-turn pending. A client that has one overrides this.
    async fn applied_effort(&self, _session_id: &str) -> Option<Option<String>> {
        None
    }

    /// Make the session's agent selection durable (V2's
    /// `POST /api/session/{id}/agent`). A no-op on V1, where the selection
    /// rides each prompt.
    ///
    /// The default is a no-op (`Ok(())`): a client whose selection rides each
    /// prompt has no durable switch to make, so the no-op is **not applicable
    /// to this client**. A client that has the switch overrides this and
    /// answers `Err` when the write fails.
    async fn switch_session_agent(&self, _session_id: &str, _agent: &str) -> Result<()> {
        Ok(())
    }

    /// Whether this client owns a **one-shot next-turn pending** for its model
    /// and `/think` level (Codex's `turn/start.model` + `effort`) instead of
    /// sending a per-prompt mirror (OpenCode's `prompt.model`). The bridge uses
    /// it to know which side records a live pick and where the 待应用 line
    /// comes from; it says nothing about whether the client knows a
    /// thread-level provider.
    ///
    /// The default answers `false` — a client whose selection rides each prompt
    /// has no next-turn window to record.
    fn owns_next_turn_pending(&self) -> bool {
        false
    }

    /// Whether this client tracks a **thread-level provider fact** (Codex's
    /// `modelProvider`): the adapter's projection knows which provider a live
    /// thread runs, which is the fact a `/model` pick validates against and a
    /// provider surface names. It does not mean a provider can ride
    /// `thread/start` or `thread/resume` — a provider is a config fact
    /// (spec #930). The default answers `false` — a client whose provider
    /// rides each prompt (OpenCode) has no such thread fact.
    fn tracks_thread_provider(&self) -> bool {
        false
    }

    /// The client's display name for the bridge's user-facing lifecycle
    /// messages ("start failed", "no server, `start_server = never`"). It never
    /// rides the wire; it exists so a message names the client the user
    /// configured instead of whichever adapter the sentence was written for.
    fn client_name(&self) -> &'static str {
        "Agent Client"
    }

    /// How to get this client's backend running by hand, for the
    /// `start_server = "never"` message. The adapter owns the remedy it can
    /// actually recommend.
    fn start_hint(&self) -> &'static str {
        "启动客户端的后端服务，或把 `start_server` 改为 `auto`。"
    }

    /// Whether `provider` is this client's **neutral bucket** — a selection
    /// that names no thread-level channel (Codex's `codex`), so a pick using it
    /// is a model-only pick no matter what the thread currently runs. The
    /// bridge must not compare such a pick to the thread's real provider
    /// (issue #910).
    fn is_neutral_provider(&self, _provider: &str) -> bool {
        false
    }

    /// The thread-level provider this client's **own cache** holds for the
    /// session, when it knows one (Codex's ordered thread fact). Cache-only: no
    /// RPC, so a card callback can decide whether a provider comparison is
    /// relevant without a request. The cache decides whether a live pick names
    /// the thread's own provider, and a different provider is refused without
    /// any request. `None` means unknown — which must **not** be treated as a
    /// mismatch (spec #930's accepted residual limit).
    async fn known_thread_provider(&self, _session_id: &str) -> Option<String> {
        None
    }

    /// The live thread provider and the attachment generation that supplied
    /// it. A provider comparison is valid only while this generation remains
    /// current at the eventual model write.
    async fn known_thread_provider_fact(
        &self,
        session_id: &str,
    ) -> crate::backend::types::ProviderFactSnapshot {
        crate::backend::types::ProviderFactSnapshot {
            provider_id: self.known_thread_provider(session_id).await,
            generation: self.provider_fact_generation(),
        }
    }

    /// The provider this client's configuration names (`config/read`'s
    /// top-level `model_provider`) **for the exact `directory`** a successful
    /// read was scoped to. Cache-only: no RPC on the pick path, because a
    /// `/model` pick must be decided without a request. `None` means unknown
    /// (no read for that directory, a failed read, or the fact was invalidated
    /// by a new attachment), and an unknown channel never blocks a pick.
    fn configured_provider(&self, _directory: &str) -> Option<String> {
        None
    }

    /// The configured provider fact and its attachment generation, read
    /// atomically by clients that cache it. Unknown facts still carry their
    /// generation so a late model write can notice that the cache was replaced.
    fn configured_provider_fact(&self, directory: &str) -> crate::backend::types::ProviderFactSnapshot {
        crate::backend::types::ProviderFactSnapshot {
            provider_id: self.configured_provider(directory),
            generation: self.provider_fact_generation(),
        }
    }

    /// The current generation of provider facts. The bridge captures it during
    /// pick validation and re-checks it immediately before writing.
    fn provider_fact_generation(&self) -> u64 {
        0
    }

    /// The neutral provider bucket used when a pick's fact generation expires.
    /// A client without one leaves its picks unchanged.
    fn neutral_provider_id(&self) -> Option<&'static str> {
        None
    }

    /// Whether this generation keeps model/agent choices as durable session
    /// state (V2's session switches) rather than sending them with each prompt
    /// (V1). A caller uses this to know a server-side switch is REQUIRED for a
    /// pick to take effect; it must never infer it from a failed selection
    /// read (which only says "unknown").
    ///
    /// The default answers `false`: a client with no durable selection keeps
    /// none, so `false` is **not applicable to this client** — not a failed
    /// read (a failed selection read is that read's own `Err`, and must never
    /// be folded into this answer).
    fn keeps_session_selection(&self) -> bool {
        false
    }

    /// Whether a same-id re-post of a user message this server has ALREADY
    /// admitted can still continue that turn. True on V1, where re-posting an
    /// admitted, unfinished (failed, no terminal finish) `msg_cola_` id upserts
    /// the one user message and runs a new step; false on V2, where the id is
    /// an admission key: once admitted, a same-id re-post returns without
    /// running (only a never-admitted id runs). The retry matrix reads this to
    /// choose id reuse vs a fresh id; it states the server contract, never a
    /// policy.
    ///
    /// The default answers `false`: a client with no continuation-by-re-post
    /// surface cannot promise the turn continues, so `false` is **not
    /// applicable to this client** and the retry matrix posts a fresh id (the
    /// conservative cell). There is no read here, so there is no `Err`; a
    /// client that would answer `true` must override this explicitly.
    fn reuse_continues_an_admitted_turn(&self) -> bool {
        false
    }

    /// Whether this generation can resume a Session through [`Self::resume`]
    /// (`POST /api/session/{id}/resume`, V2): a message queued in the session
    /// inbox (cola's steered submit) is promoted at the new run's start. V1 has
    /// no resume endpoint, so the Unreceived card's 重新发起 action degrades to
    /// resubmitting the message as a new Turn there (#437). States the server
    /// contract, never a policy.
    ///
    /// The default answers `false`: a client with no resume endpoint has no
    /// such surface, so `false` is **not applicable to this client** and the
    /// action degrades to a resubmit. There is no read here, so there is no
    /// `Err`; a client that answers `true` must override [`Self::resume`] too.
    fn resume_supported(&self) -> bool {
        false
    }

    /// Resume a Session: promote the queued steer at a new run's start
    /// (`POST /api/session/{id}/resume`, 204). The 重新发起 action pairs it
    /// with an [`Self::interrupt`] (a no-op when the Session is idle): the
    /// interrupt stops whatever stale run still holds the Session, and the
    /// resume promotes cola's already-admitted message into a fresh run —
    /// pressing the button never duplicates the transcript message. Only
    /// called where [`Self::resume_supported`] is true.
    ///
    /// The default is a no-op (`Ok(())`): a client with no resume endpoint has
    /// nothing to promote, so the no-op is **not applicable to this client**.
    /// It is reachable only where [`Self::resume_supported`] is true, so a
    /// defaulted pair is never called; a client that answers `true` overrides
    /// this, and a resume that was attempted and failed is its `Err`.
    async fn resume(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    /// Reply to a pending permission with a decision (`once` / `always` /
    /// `reject`). `session_id` is the requesting session (the child session for
    /// a sub-task ask): V2's reply is session-scoped and needs it; V1 routes by
    /// `directory` and ignores it.
    async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> Result<()>;

    /// List the pending permissions for one directory/instance. V1 reads the
    /// global `?directory=`-scoped list; V2 the location-scoped one — either
    /// way one call per known directory, never per session.
    async fn list_permissions(&self, directory: Option<&str>) -> Result<Vec<PermissionRequest>>;

    /// List the pending forms/questions for one directory/instance (the same
    /// per-directory read as [`Self::list_permissions`]).
    async fn list_questions(&self, directory: Option<&str>) -> Result<Vec<QuestionRequest>>;

    /// Whether `id` can name a Session on this client at all. Every OpenCode
    /// generation mints `ses…` ids, so the default preserves the structural
    /// filter that keeps a session-less request (`global`, an empty optional
    /// id) from paying a doomed read every tick. Codex mints UUID thread ids,
    /// which are just as addressable — the prefix rule must not silently drop
    /// every Codex approval.
    fn is_addressable_session_id(&self, id: &str) -> bool {
        id.starts_with("ses")
    }

    /// Answer a pending form/question with keyed answers (V2's `Form.Answer`);
    /// V1 flattens the values into its positional `answers` arrays.
    /// `session_id` is the requesting session (V2's reply is session-scoped).
    async fn reply_question(
        &self,
        session_id: &str,
        request_id: &str,
        answers: &[FormAnswer],
        directory: Option<&str>,
    ) -> Result<()>;

    /// Cancel a pending form (V2: DELETE) / reject a question (V1).
    async fn reject_question(
        &self,
        session_id: &str,
        request_id: &str,
        directory: Option<&str>,
    ) -> Result<()>;

    /// Read one Session as a neutral [`SessionTranscript`] — the read model the
    /// Bridge consumes (ADR-0053). The adapter's wire envelope never reaches
    /// the caller.
    async fn transcript(&self, session_id: &str) -> Result<SessionTranscript>;

    /// One Turn's complete file diff ([`TurnDiff`]), where the generation
    /// serves a session diff endpoint (V2's `GET /api/session/{id}/diff`).
    /// `from` is the Turn anchor's user-message id — V2 scopes the diff to
    /// that turn's snapshot, so later edits cannot drift into it; `to` and
    /// `context` are the endpoint's other axes and are omitted from the query
    /// when `None`. `Ok(None)` is strictly "this generation has no session diff
    /// endpoint" (V1), never an empty diff: V1 has no Turn Diff at all — no
    /// card index and no artifact (ADR-0087).
    ///
    /// The Turn card's settle index and its 「查看完整变化」 callback (#801)
    /// read this one source.
    ///
    /// The default answers `Ok(None)`: a client with no session diff endpoint
    /// has no Turn Diff at all, so `None` is **not applicable to this client**
    /// — never an empty diff and never a failed read. A client that has the
    /// endpoint overrides this and still answers `Err` when the read fails.
    async fn session_diff(
        &self,
        _session_id: &str,
        _from: Option<&str>,
        _to: Option<&str>,
        _context: Option<u32>,
    ) -> Result<Option<TurnDiff>> {
        Ok(None)
    }

    /// The server's live run state for one session (`GET /session/status`).
    /// `directory` selects the instance (ADR-0010). A successful read always
    /// yields a status (absent = idle); `Ok(None)` is an unrecognised status
    /// type (never guessed), an `Err` a failed read.
    async fn session_status(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionStatus>>;

    /// The runtime verdict for the named Background Tasks of one session
    /// (issue #454; V2 reads `GET /api/shell` + `GET /api/shell/{id}` + `GET
    /// /api/session/active`). `shells` and `children` are the task identities
    /// the caller cares about; every id the read can place gets a verdict, and
    /// an id left out is no evidence — the caller must not guess from absence.
    ///
    /// The reads are process-local to the attached server: a restarted server
    /// answers `Missing` for shells it never hosted. V1 carries no Background
    /// Tasks, so its callers never ask (it still answers an empty read without
    /// a request).
    ///
    /// The default answers `Ok(TaskRuntime::default())` — an empty verdict for
    /// every id asked about, which is no evidence at all: a client with no live
    /// runtime registry has no such surface, so the empty verdict is **not
    /// applicable to this client**, never a failed read. A client that has the
    /// surface overrides this and still answers `Err` when the read fails.
    async fn task_runtime(
        &self,
        _session_id: &str,
        _directory: Option<&str>,
        _shells: &[String],
        _children: &[String],
    ) -> Result<TaskRuntime> {
        Ok(TaskRuntime::default())
    }

    /// Whether this client publishes a **Background Task surface** at all: the
    /// live shells, subagent children and shell output records the Background
    /// Task Ledger renders, and the runtime verdicts that retire them. It is a
    /// capability, not a read: it says whether asking the questions is
    /// meaningful on this client.
    ///
    /// The default answers `false`: a client with no such surface renders **no
    /// ledger** — not an empty section, and never a `⚠️ 状态待确认` row — and the
    /// Bridge spends no runtime verdict, child-evidence or shell-output read on
    /// it. `false` is **not applicable to this client**, never a failed read; a
    /// client that has the surface overrides this, and each of its reads still
    /// reports its own failure as that read's `Err`.
    fn background_tasks_supported(&self) -> bool {
        false
    }

    /// The Background Task State module's handle (ADR-0079, tickets #681/#682)
    /// — the one instance the adapter was constructed with and the Bridge
    /// records through. It owns the volatile liveness tier (issue #454, review
    /// #588): the adapter applies it to every [`Self::transcript`] read
    /// ([`crate::bridge::tasks::TasksHandle::apply`]), so a task the runtime
    /// confirmed ended leaves the live list and a task it could not confirm as
    /// running carries its `⚠️ 状态待确认` marker on every read path — the reap's
    /// settle, a drain tick, the follow, the Snapshot status line and the
    /// cleanup click's own read alike.
    ///
    /// The module owns the tier's bytes AND its instance
    /// ([`crate::bridge::tasks::TasksHandle`]): `src/main.rs` creates the
    /// handle and threads it into the adapter at construction, and both the
    /// adapter and the Bridge hold that same handle — the Bridge records
    /// through exactly the handle every later read goes through, and a later
    /// caller using the handle as the lifecycle owner sees the same
    /// retirements.
    ///
    /// In-memory on purpose: it is a cola-life overlay, not durable state. A
    /// restart loses it and the next read re-derives the same retirement, the
    /// next verdict the same markers.
    fn tasks(&self) -> &crate::bridge::tasks::TasksHandle;

    /// Read one shell's captured output window (spec #588, ticket #592): the
    /// record's last [`SHELL_OUTPUT_WINDOW_BYTES`] bytes, decoded and clipped
    /// to its last [`SHELL_OUTPUT_WINDOW_LINES`] lines. V2 pages the capture
    /// by absolute byte cursor — the server's own tail idiom asks for the size
    /// first, then the last window. `Ok(None)` is strictly "no record to
    /// read": the runtime no longer keeps it (404). A record that exists and
    /// captured nothing answers `Some` with an empty window — a readable-empty
    /// result the row and the entry omit, never 「输出已不可用」 — while `Err`
    /// is a failed read. V1 carries no shell records, so it answers `Ok(None)`
    /// without a request.
    ///
    /// Display-only: the read never prompts, never changes a task's liveness
    /// and never settles anything.
    ///
    /// The default answers `Ok(None)`: a client with no shell records has no
    /// record to read, so `None` is **not applicable to this client**, never a
    /// failed read. A client that has the surface overrides this and still
    /// answers `Err` when the read fails (and `Ok(None)` only for a record the
    /// runtime no longer keeps).
    async fn shell_output(
        &self,
        _shell_id: &str,
        _directory: Option<&str>,
    ) -> Result<Option<ShellOutputWindow>> {
        Ok(None)
    }
    /// The evidence one child session's newest assistant message carries about
    /// its Background Task's run (#591, issue #464): one page of projected
    /// messages, newest assistant first (V2 reads
    /// `GET /api/session/{id}/message?type=assistant&order=desc&limit=1`).
    /// [`ChildEvidence::Terminal`] is a terminal step finish with the message's
    /// completion stamp — positive evidence the run ended; [`ChildEvidence::Gone`]
    /// is a 404 — positive evidence the session no longer exists; an
    /// `Ok(ChildEvidence::Unfinished)` is no evidence at all (the caller must
    /// not retire from it), and an `Err` is an unreadable child (same). V1
    /// carries no Background Tasks; it answers `Unfinished` without a request.
    ///
    /// The default answers `Ok(ChildEvidence::Unfinished)`: a client with no
    /// child sessions has no evidence to read, so no-evidence is **not
    /// applicable to this client**, never a failed read. A client that has the
    /// surface overrides this and still answers `Err` for an unreadable child.
    async fn child_evidence(&self, _session_id: &str) -> Result<ChildEvidence> {
        Ok(ChildEvidence::Unfinished)
    }

    /// The model's context-window size (tokens), from `GET /provider`. Used to
    /// compute the context-usage ratio for the card footer. Best-effort: None
    /// when the provider/model can't be resolved.
    async fn model_context_window(&self, provider: &str, model: &str) -> Result<Option<i64>>;

    /// The model configured as cola's default (`[opencode] model`), if any —
    /// the second rung of the `/think` effective-model resolution (session
    /// override → configured default → server-recorded session model).
    fn configured_default_model(&self) -> Option<ModelInfo>;

    /// Available agents (`GET /agent`), for the `/agent` card picker. Empty on
    /// failure (the card degrades to a text prompt).
    async fn list_agents(&self) -> Vec<AgentInfo>;

    /// The registered skills, for the `/skill` picker and the `/skill <id>`
    /// resolution (spec #652, ticket #656). V2 reads `GET /api/skill`
    /// (`Skill.Info[]`); V1 reads its own `GET /skill`, whose identity IS the
    /// name (V1's `Skill.Info` has no id). `directory` selects the location the
    /// read is scoped to — project skills live under `<directory>/.opencode`,
    /// so a session's own directory is what the eventual prompt runs in; `None`
    /// keeps the server's default location. Best-effort: an unreadable failure
    /// returns an empty list, so the picker degrades to its no-skills state.
    async fn list_skills(&self, directory: Option<&str>) -> Vec<SkillInfo>;

    /// The `/model` card's one data source: what this client can offer, on the
    /// page the caller asked for, plus the running selection.
    ///
    /// The surface is declared by the client, not assumed to be OpenCode's
    /// provider → model tree: OpenCode's two-page flow is a root page whose
    /// one `provider` scope lists providers, and a drilled page
    /// (`query.scope = Some("provider")`, `query.selected = Some(id)`) whose
    /// `model` scope lists that provider's models; Codex's flat catalog is one
    /// `model` scope with no pseudo provider level. `session_id`/`directory`
    /// are the session the card is for (a session fact, like
    /// [`Self::session_info`]); `current` is the selection the caller already
    /// resolved for the next turn, so the card has one source for its current
    /// line. Best-effort: an unreadable catalog yields an empty scope, never a
    /// failed read.
    ///
    /// `query.ack_budget` means **this read answers a card callback** and must
    /// fit that budget; `None` is a read that is not bound by an ack — a
    /// command's page, or a callback's validation read whose completeness
    /// matters more than its toast. A client that does network work here
    /// decides how to spend it (Codex narrows its config and catalog reads and
    /// serves probe answers from cache only); OpenCode ignores it.
    async fn model_surface(&self, query: ModelSurfaceQuery<'_>) -> ModelSurface;

    /// Available models grouped by provider (`GET /provider`), for the `/model`
    /// card picker. Empty on failure (the card degrades to a text prompt).
    /// Kept for a client's own surface mapping and the `/think` variant read;
    /// the `/model` card goes through [`Self::model_surface`].
    async fn list_models(&self) -> Vec<ProviderModels>;

    /// Fetch a session's info (exposes the parent chain for sub-task sessions).
    async fn session_info(&self, session_id: &str, directory: Option<&str>) -> Result<SessionInfo>;

    async fn interrupt(&self, session_id: &str) -> Result<()>;

    /// Compact a session's context. V1 takes no request body and answers 204;
    /// V2 takes an empty payload and answers `{data}` — the strategy owns the
    /// generation's shape.
    async fn compact(&self, session_id: &str) -> Result<()>;

    /// The base URL this backend currently targets (used by the reconnect loop
    /// to detect a changed server). Empty when serverless (Lazy Start hasn't
    /// attached or spawned yet).
    fn base_url(&self) -> String;

    /// A directory-scoped handle for instance-routed calls (permissions,
    /// questions, session info). The returned handle owns the directory, so no
    /// call site can silently omit it and hit the server cwd instance
    /// (ADR-0010).
    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn DirectoryBackend>;
}

/// The second adapter's shape, pinned: a client that implements the core and
/// leaves every generation-specific surface defaulted (spec #860, ticket #873).
///
/// This module is the executable half of the trait-level doc comment — if a
/// future change removes a default, or a default starts answering something
/// other than "not applicable to this client", this stops compiling or fails.
/// The body of [`CoreClient`] is deliberately trivial: the point is which
/// methods it does NOT have to write.
#[cfg(test)]
mod core_only_client {
    use super::*;
    use crate::backend::types::CreateSessionInput;

    /// A client with the neutral core and no OpenCode-generation surface.
    #[derive(Default)]
    struct CoreClient {
        tasks: crate::bridge::tasks::TasksHandle,
    }

    /// No runtime to discover: the explicit "attached, nothing to do" body
    /// every no-lifecycle client owes the required method.
    #[async_trait]
    impl ServerLifecycle for CoreClient {
        async fn ensure_attached(&self, _request: EnsureAttached<'_>) -> Result<AttachOutcome> {
            Ok(AttachOutcome::Attached)
        }
    }

    #[async_trait]
    impl Backend for CoreClient {
        fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
            CreateSessionInput {
                id: None,
                agent: None,
                model: None,
                location: directory.map(|directory| crate::backend::types::Location {
                    directory: directory.to_string(),
                }),
            }
        }

        async fn create_session(&self, _input: &CreateSessionInput) -> Result<Session> {
            Ok(Session {
                id: "ses_core".into(),
                project_id: None,
                agent: None,
                title: None,
                location: None,
                cost: None,
                time: None,
            })
        }

        async fn list_sessions(&self) -> Result<Vec<SessionListInfo>> {
            Ok(Vec::new())
        }

        async fn update_session_title(&self, _session_id: &str, _title: &str) -> Result<()> {
            Ok(())
        }

        async fn delete_session(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }

        async fn prompt(
            &self,
            _session_id: &str,
            _text: &str,
            _files: &[PromptFile],
            _skills: &[PromptSkill],
            _model: Option<&ModelInfo>,
            _variant: Option<&str>,
            _agent: Option<&str>,
            _message_id: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }

        async fn reply_permission(
            &self,
            _session_id: &str,
            _request_id: &str,
            _reply: &str,
            _directory: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }

        async fn list_permissions(&self, _directory: Option<&str>) -> Result<Vec<PermissionRequest>> {
            Ok(Vec::new())
        }

        async fn list_questions(&self, _directory: Option<&str>) -> Result<Vec<QuestionRequest>> {
            Ok(Vec::new())
        }

        async fn reply_question(
            &self,
            _session_id: &str,
            _request_id: &str,
            _answers: &[FormAnswer],
            _directory: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }

        async fn reject_question(
            &self,
            _session_id: &str,
            _request_id: &str,
            _directory: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }

        async fn transcript(&self, _session_id: &str) -> Result<SessionTranscript> {
            Ok(SessionTranscript::new(Vec::new()))
        }

        async fn session_status(
            &self,
            _session_id: &str,
            _directory: Option<&str>,
        ) -> Result<Option<SessionStatus>> {
            Ok(Some(SessionStatus::Idle))
        }

        fn tasks(&self) -> &crate::bridge::tasks::TasksHandle {
            &self.tasks
        }

        async fn model_context_window(&self, _provider: &str, _model: &str) -> Result<Option<i64>> {
            Ok(None)
        }

        fn configured_default_model(&self) -> Option<ModelInfo> {
            None
        }

        async fn list_agents(&self) -> Vec<AgentInfo> {
            Vec::new()
        }

        async fn list_skills(&self, _directory: Option<&str>) -> Vec<SkillInfo> {
            Vec::new()
        }

        async fn list_models(&self) -> Vec<ProviderModels> {
            Vec::new()
        }

        async fn model_surface(&self, _query: ModelSurfaceQuery<'_>) -> ModelSurface {
            ModelSurface::default()
        }

        async fn session_info(&self, session_id: &str, _directory: Option<&str>) -> Result<SessionInfo> {
            Ok(SessionInfo {
                id: session_id.to_string(),
                parent_id: None,
                title: None,
                model: None,
            })
        }

        async fn interrupt(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }

        async fn compact(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }

        fn base_url(&self) -> String {
            String::new()
        }

        fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn DirectoryBackend> {
            Arc::new(BackendDirectory::new(self, directory.to_string()))
        }
    }

    /// Every defaulted method answers "this client has no such surface" — an
    /// inert value, never an `Err` that would read as a failed read.
    #[tokio::test]
    async fn the_defaulted_surfaces_answer_not_applicable() {
        use crate::backend::types::ModelInfo;

        let client = CoreClient::default();
        let model = ModelInfo {
            id: "m".into(),
            provider_id: "p".into(),
            variant: None,
        };

        assert!(
            client
                .session_selection("ses_core", None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            client
                .session_last_run_model("ses_core", None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            client
                .session_diff("ses_core", None, None, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!client.keeps_session_selection());
        assert!(!client.reuse_continues_an_admitted_turn());
        assert!(!client.resume_supported());
        assert!(!client.owns_next_turn_pending());
        assert!(!client.tracks_thread_provider());
        assert_eq!(client.configured_provider("/work/cola"), None);
        client.resume("ses_core").await.unwrap();
        client.switch_session_model("ses_core", &model).await.unwrap();
        client.switch_session_agent("ses_core", "build").await.unwrap();
        assert!(client.shell_output("sh_1", None).await.unwrap().is_none());
        assert!(
            !client.background_tasks_supported(),
            "a client with no Background Task surface says so, and renders no ledger"
        );

        let runtime = client
            .task_runtime("ses_core", None, &["sh_1".into()], &["ses_child".into()])
            .await
            .unwrap();
        assert!(runtime.shell("sh_1").is_none(), "an empty verdict is no evidence");
        assert!(runtime.child("ses_child").is_none());

        assert_eq!(
            client.child_evidence("ses_child").await.unwrap(),
            ChildEvidence::Unfinished
        );
    }
}
