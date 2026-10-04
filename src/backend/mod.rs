//! The backend contract: the traits the Bridge calls and the neutral read model
//! it consumes (ADR-0010, ADR-0053).
//!
//! The seam's read side carries a [`SessionTranscript`] — typed messages and
//! typed parts — instead of backend wire JSON. Backend protocol field names
//! for that read live only in the adapter's private decoders, so a protocol
//! change is an adapter change, not a Bridge-and-card change. The public
//! session/permission/provider/agent DTOs in [`crate::opencode::types`] are
//! the known out-of-scope exception (spec #332): they still spell the
//! list/read wire fields, and the session list's raw `model` payload is
//! normalized where it is displayed (`bridge::display::model_display`).
//!
//! Every caller imports these traits from here, never from the adapter; the
//! HTTP adapter ([`crate::opencode::client::OpenCodeBackend`]) and the test mock
//! implement them.

pub mod transcript;

// The neutral views are re-exported at the contract root: consumers import
// them from here, never from the decoder's module path.
pub use transcript::*;

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::opencode::types::{
    AgentInfo, CreateSessionInput, FormAnswer, ImageInput, ModelInfo, PermissionRequest, ProviderModels,
    QuestionRequest, Session, SessionInfo, SessionListInfo, SessionSelection, SessionStatus,
};

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
/// [`OpenCodeBackend`](crate::opencode::client::OpenCodeBackend) and test mocks produce this via
/// [`Backend::for_directory`], so the directory-scoped seam is implemented once.
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
/// instead of a live server. The real implementation is the adapter's
/// [`OpenCodeBackend`](crate::opencode::client::OpenCodeBackend); mock implementations feed
/// scripted transcripts/permissions and verify what cola renders from them.
#[async_trait]
pub trait Backend: Send + Sync {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput;

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
    /// `images` are attached as data-URL `file` parts; requires a vision-capable
    /// model (unsupported models surface an error).
    ///
    /// `message_id` is the id cola chose for the user message this prompt will
    /// create (ADR-0026: `msg_cola_` self-identifies cola-authored messages).
    /// A re-post is idempotent either way, but only on a generation where
    /// [`Self::reuse_continues_an_admitted_turn`] is true does it continue the
    /// admitted turn (V1); on V2 the admitted id makes the re-post a no-op.
    /// None falls back to a server-generated id.
    #[allow(clippy::too_many_arguments)] // prompt axes: session/text/images + model/variant/agent/message-id
    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageInput],
        model: Option<&ModelInfo>,
        variant: Option<&str>,
        agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()>;

    /// The session's durable model/agent selection, where the generation keeps
    /// one server-side (V2's session-scoped switches). `None` on a generation
    /// whose selection rides each prompt (V1: cola's own override is
    /// authoritative, and the server-recorded model is only a display
    /// fallback), and for a Pending Session (no server identity to read).
    ///
    /// This is what the next turn will actually run: shared session state that
    /// another client may have changed, not cola's local mirror.
    async fn session_selection(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionSelection>>;

    /// The model the session last actually ran with — the newest assistant
    /// message's model ref (variant inside it). A durable generation records a
    /// model on the session only after an explicit switch, so for a session
    /// started from the server's own default this message ref is the public
    /// record of what is running; the effective-model ladder reads it as the
    /// last rung before giving up. `None` on V1, whose ladder already reads
    /// the server-recorded session model through `session_info`, and for a
    /// session with no assistant message yet.
    async fn session_last_run_model(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<ModelInfo>>;

    /// Make the session's model selection durable so subsequent turns use it
    /// with nothing re-sent (V2's `POST /api/session/{id}/model`); the variant
    /// rides inside `model`. A no-op on V1, where the selection rides each
    /// prompt.
    async fn switch_session_model(&self, session_id: &str, model: &ModelInfo) -> Result<()>;

    /// Make the session's agent selection durable (V2's
    /// `POST /api/session/{id}/agent`). A no-op on V1, where the selection
    /// rides each prompt.
    async fn switch_session_agent(&self, session_id: &str, agent: &str) -> Result<()>;

    /// Whether this generation keeps model/agent choices as durable session
    /// state (V2's session switches) rather than sending them with each prompt
    /// (V1). A caller uses this to know a server-side switch is REQUIRED for a
    /// pick to take effect; it must never infer it from a failed selection
    /// read (which only says "unknown").
    fn keeps_session_selection(&self) -> bool;

    /// Whether a same-id re-post of a user message this server has ALREADY
    /// admitted can still continue that turn. True on V1, where re-posting an
    /// admitted, unfinished (failed, no terminal finish) `msg_cola_` id upserts
    /// the one user message and runs a new step; false on V2, where the id is
    /// an admission key: once admitted, a same-id re-post returns without
    /// running (only a never-admitted id runs). The retry matrix reads this to
    /// choose id reuse vs a fresh id; it states the server contract, never a
    /// policy.
    fn reuse_continues_an_admitted_turn(&self) -> bool;

    /// Whether this generation can resume a Session through [`Self::resume`]
    /// (`POST /api/session/{id}/resume`, V2): a message queued in the session
    /// inbox (cola's steered submit) is promoted at the new run's start. V1 has
    /// no resume endpoint, so the Unreceived card's 重新发起 action degrades to
    /// resubmitting the message as a new Turn there (#437). States the server
    /// contract, never a policy.
    fn resume_supported(&self) -> bool;

    /// Resume a Session: promote the queued steer at a new run's start
    /// (`POST /api/session/{id}/resume`, 204). The 重新发起 action pairs it
    /// with an [`Self::interrupt`] (a no-op when the Session is idle): the
    /// interrupt stops whatever stale run still holds the Session, and the
    /// resume promotes cola's already-admitted message into a fresh run —
    /// pressing the button never duplicates the transcript message. Only
    /// called where [`Self::resume_supported`] is true.
    async fn resume(&self, session_id: &str) -> Result<()>;

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

    /// Read the newest end of one Session's transcript as a bounded,
    /// newest-first tail scan that stops once a page's oldest message predates
    /// `boundary_ms` (ADR-0068). The common case is one request; a small page
    /// cap bounds the worst case and reports it via
    /// [`TranscriptTail::complete`], so the caller carries nothing from a
    /// partial scan. A generation with no wire pagination reads its one
    /// standard message array and reports it complete; every generation's read
    /// is bounded by the caller's own timeout.
    async fn transcript_tail(&self, session_id: &str, boundary_ms: i64) -> Result<TranscriptTail>;

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
    async fn task_runtime(
        &self,
        session_id: &str,
        directory: Option<&str>,
        shells: &[String],
        children: &[String],
    ) -> Result<TaskRuntime>;

    /// Record the Background Tasks a runtime reconciliation retired for one
    /// session (issue #454), so every later [`Self::transcript`] read drops
    /// them from its live list. The launch record in the transcript never
    /// flips, so without this overlay the next read would resurrect the task
    /// on a live card and the next Turn would yield waiting on it again.
    ///
    /// In-memory on purpose: it is a cola-life overlay, not durable state. A
    /// restart loses it and the next read re-derives the same retirement.
    fn retire_background_tasks(&self, session_id: &str, call_ids: &[String]);

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

    /// Available models grouped by provider (`GET /provider`), for the `/model`
    /// card picker. Empty on failure (the card degrades to a text prompt).
    async fn list_models(&self) -> Vec<ProviderModels>;

    /// Fetch a session's info (exposes the parent chain for sub-task sessions).
    async fn session_info(&self, session_id: &str, directory: Option<&str>) -> Result<SessionInfo>;

    async fn interrupt(&self, session_id: &str) -> Result<()>;

    /// Compact a session's context. V1 takes no request body and answers 204;
    /// V2 takes an empty payload and answers `{data}` — the strategy owns the
    /// generation's shape.
    async fn compact(&self, session_id: &str) -> Result<()>;

    /// Re-point the backend at a (re)discovered attachment, selecting that
    /// attachment's generation strategy (the server was restarted/replaced at
    /// runtime, or Lazy Start raised one). `None` goes serverless. The caller
    /// has already resolved the generation (attach detection, spec #364 §2) —
    /// this never guesses and never probes. No-op for mocks.
    async fn reconnect(&self, server: Option<&crate::bridge::discovery::ResolvedServer>) -> Result<()>;

    /// The base URL this backend currently targets (used by the reconnect loop
    /// to detect a changed server). Empty when serverless (Lazy Start hasn't
    /// attached or spawned yet).
    fn base_url(&self) -> String;

    /// The pid of the server this backend is attached to, when the attach path
    /// knew it. A neutral identity token: the reconnect loop compares it with
    /// the discovered candidate's pid so a replacement on the SAME port (a new
    /// generation or password) is noticed and re-probed (spec #364 §2). `None`
    /// for mocks and for attachments whose identity was not carried.
    fn attached_server_pid(&self) -> Option<i32> {
        None
    }

    /// Whether this backend can lazily start its own OpenCode server when none
    /// is running. The real adapter can; test mocks cannot (there is no
    /// process to spawn), so the Lazy Start hook is a no-op in tests.
    fn can_self_start_server(&self) -> bool {
        false
    }

    /// A directory-scoped handle for instance-routed calls (permissions,
    /// questions, session info). The returned handle owns the directory, so no
    /// call site can silently omit it and hit the server cwd instance
    /// (ADR-0010).
    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn DirectoryBackend>;
}
