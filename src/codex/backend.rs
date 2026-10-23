//! The Codex adapter's `Backend` implementation (spec #860, ticket #879).
//!
//! This is the seam ADR-0090 describes: cola's poll-shaped `Backend` on one
//! side, app-server's push-shaped notifications on the other. The subscription
//! is the adapter's, the cache is the adapter's, and every read call here is a
//! projection over that cache — the bridge learns nothing about item streams.
//!
//! Two types implement the contract:
//!
//! - [`CodexClient`] is one attached connection. It owns the [`Threads`]
//!   subscription surface and the read-model cache, so a test (and an eventual
//!   per-connection caller) can drive the whole read path.
//! - [`CodexLifecycle`] is the active client the composition root stores in
//!   `SharedCore.client`; it owns discovery/attach and delegates the `Backend`
//!   surface to whatever client the last attach pass installed. A read before
//!   an attach is a named error, never a silently empty transcript.
//!
//! Mid-turn reads never wait on the subscription: [`Projections`] is guarded by
//! a plain mutex held only for the projection fold, while the subscriber task is
//! the only thing that awaits the push stream. A dropped-event `Lagged` signal
//! first marks the projection incomplete (so the read states the gap) and then
//! resyncs from `thread/read` with persisted turns.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::backend::lifecycle::{AttachOutcome, EnsureAttached, ServerLifecycle};
use crate::backend::types::{
    AgentInfo, CreateSessionInput, FormAnswer, Location, ModelInfo, PermissionRequest, PromptFile,
    ProviderModels, QuestionRequest, Session, SessionInfo, SessionListInfo, SessionStatus,
};
use crate::backend::{Backend, BackendDirectory, PromptSkill, SessionTranscript, SkillInfo, TurnDiff};
use crate::bridge::tasks::TasksHandle;
use crate::error::{BridgeError, Result};

use super::client::CodexClient;
use super::error::{CodexError, Result as CodexResult};
use super::lifecycle::CodexLifecycle;
use super::read_model::ThreadProjection;
use super::threads::{Adoption, ThreadEvent, ThreadSubscription, Threads};
use super::transport::Endpoint;

/// One thread's projection and the state of its subscription.
#[derive(Default)]
pub(crate) struct Projections {
    threads: Mutex<HashMap<String, Arc<Mutex<ThreadProjection>>>>,
    states: Mutex<HashMap<String, SubscriptionState>>,
    /// One adoption lock per thread. Two concurrent reads of the same new
    /// thread must not both send `thread/resume`, but an unrelated thread (a
    /// repeatedly polled never-run thread included) must never block another's
    /// first adoption. Entries are weak, so a thread that is never adopted
    /// again costs no retained lock.
    adopting: Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    /// The subscriber tasks this cache owns. Aborted when the last client
    /// holding the cache is dropped, so a replaced attachment stops folding
    /// into a cache nothing reads.
    subscribers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for Projections {
    fn drop(&mut self) {
        for task in self
            .subscribers
            .get_mut()
            .expect("codex subscriber tasks")
            .drain(..)
        {
            task.abort();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionState {
    /// `thread/resume` succeeded; a subscriber task is folding events.
    Live,
}

impl Projections {
    fn adoption_lock(&self, thread_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.adopting.lock().expect("codex adoption locks");
        // Drop entries whose owner finished, so the map tracks live adoptions
        // rather than every thread ever polled.
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(thread_id).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(thread_id.to_string(), Arc::downgrade(&lock));
        lock
    }

    /// Whether this thread's run state has a live subscription. Retained
    /// history alone is NOT enough: a dead subscription leaves stale state, so
    /// status/transcript must re-adopt before answering. An unseeded projection
    /// must not answer `Idle` as if it had read the server.
    pub(crate) fn is_seeded(&self, thread_id: &str) -> bool {
        self.state(thread_id) == Some(SubscriptionState::Live)
    }

    fn projection(&self, thread_id: &str) -> Arc<Mutex<ThreadProjection>> {
        let mut threads = self.threads.lock().expect("codex projections");
        threads
            .entry(thread_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(ThreadProjection::new(thread_id))))
            .clone()
    }

    pub(crate) fn apply(&self, thread_id: &str, method: &str, params: &Value) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .apply(method, params);
    }

    pub(crate) fn seed(&self, thread_id: &str, turns: &[Value]) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .seed(turns);
    }

    pub(crate) fn resync(&self, thread_id: &str, turns: &[Value]) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .resync(turns);
    }

    pub(crate) fn mark_incomplete(&self, thread_id: &str) {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .mark_incomplete();
    }

    pub(crate) fn transcript(&self, thread_id: &str) -> SessionTranscript {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .transcript()
    }

    pub(crate) fn diff(&self, thread_id: &str, from: Option<&str>) -> Option<TurnDiff> {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .diff_for(from)
    }

    /// The context window of whichever projection last recorded one. The
    /// `Backend` call names provider/model, but Codex's usage notification is
    /// per thread; an attached thread's figure is the only one there is.
    pub(crate) fn any_context_window(&self) -> Option<i64> {
        self.threads
            .lock()
            .expect("codex projections")
            .values()
            .filter_map(|projection| {
                projection
                    .lock()
                    .expect("codex projection")
                    .model_context_window()
            })
            .next()
    }

    pub(crate) fn status(&self, thread_id: &str) -> Option<SessionStatus> {
        Some(
            if self
                .projection(thread_id)
                .lock()
                .expect("codex projection")
                .is_live()
            {
                SessionStatus::Busy
            } else {
                SessionStatus::Idle
            },
        )
    }

    pub(crate) fn live_turn(&self, thread_id: &str) -> Option<String> {
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .live_turn()
            .map(str::to_string)
    }

    /// How many adoption-lock slots are retained (test-only; proves a finished
    /// adoption's slot is pruned rather than leaking).
    #[cfg(test)]
    fn adoption_lock_count(&self) -> usize {
        self.adopting.lock().expect("codex adoption locks").len()
    }

    fn state(&self, thread_id: &str) -> Option<SubscriptionState> {
        self.states
            .lock()
            .expect("codex subscription states")
            .get(thread_id)
            .copied()
    }

    fn track(&self, task: tokio::task::JoinHandle<()>) {
        self.subscribers
            .lock()
            .expect("codex subscriber tasks")
            .push(task);
    }

    fn set_state(&self, thread_id: &str, state: SubscriptionState) {
        self.states
            .lock()
            .expect("codex subscription states")
            .insert(thread_id.to_string(), state);
    }

    /// Forget a dead subscription so the next read adopts again, and state the
    /// gap on the retained history: a later read must not present stale state
    /// as current.
    pub(crate) fn forget(&self, thread_id: &str) {
        self.states
            .lock()
            .expect("codex subscription states")
            .remove(thread_id);
        self.projection(thread_id)
            .lock()
            .expect("codex projection")
            .mark_incomplete();
    }
}

impl CodexClient {
    /// The endpoint this connection dialled, for the neutral base-URL read.
    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Subscribe the thread if this connection has not already: `thread/resume`
    /// **is** the subscription (ADR-0090's amendment, question 2). A never-run
    /// thread is a named state, not an error; its read is an empty transcript.
    pub(crate) async fn ensure_subscription(&self, thread_id: &str) -> CodexResult<()> {
        if self.projections.state(thread_id).is_some() {
            return Ok(());
        }
        let adoption = self.projections.adoption_lock(thread_id);
        let _adopting = adoption.lock().await;
        if self.projections.state(thread_id).is_some() {
            return Ok(());
        }
        match self.threads().adopt(thread_id).await? {
            Adoption::Subscribed(subscription) => {
                // `thread/resume` carries only a bounded history page. A first
                // read must not present that prefix as the whole thread, so
                // page every turn before the subscriber starts folding. The
                // receiver was registered before `thread/resume`, so events
                // that arrive during this paging wait in its queue.
                match self.threads().all_turns(thread_id).await {
                    Ok(turns) => self.projections.resync(thread_id, &turns),
                    Err(error) => {
                        tracing::warn!(
                            thread = %thread_id,
                            "could not page the codex thread history on adoption; \
                             the bounded resume page is explicitly incomplete: {error}"
                        );
                        self.projections.seed(thread_id, &subscription.thread().turns);
                        self.projections.mark_incomplete(thread_id);
                    }
                }
                self.projections.set_state(thread_id, SubscriptionState::Live);
                let task = tokio::spawn(consume(
                    *subscription,
                    self.threads().clone(),
                    Arc::downgrade(&self.projections),
                ));
                self.projections.track(task);
                Ok(())
            }
            Adoption::NoHistoryYet => {
                // A thread created by `thread/start` cannot be resumed until
                // it has run a turn. That turn may be started by the user's
                // own client, so the state is NOT remembered as terminal: the
                // next read retries, and the subscription starts the moment
                // the rollout exists (the peer-attachment promise).
                Ok(())
            }
        }
    }
}

/// Fold one subscription's events into the cache. Notifications are applied
/// inline; a lag or a lost connection is explicitly surfaced.
async fn consume(
    mut subscription: ThreadSubscription,
    threads: Threads,
    projections: std::sync::Weak<Projections>,
) {
    let thread_id = subscription.thread().id.clone();
    loop {
        // Hold no strong reference across the await: the cache's `Drop`
        // aborts this task when the last client is gone, so a replaced
        // attachment cannot keep folding into an unread cache.
        let event = subscription.next_event().await;
        match event {
            Ok(ThreadEvent::Notification { method, params }) => {
                let Some(projections) = projections.upgrade() else {
                    return;
                };
                projections.apply(&thread_id, &method, &params);
            }
            Ok(ThreadEvent::Lagged { skipped }) => {
                tracing::warn!(
                    thread = %thread_id,
                    skipped,
                    "the codex push stream dropped events; resyncing the projection from paged turns"
                );
                resync(&threads, &projections, &thread_id).await;
            }
            Err(CodexError::SubscriptionLagged { skipped, .. }) => {
                tracing::warn!(
                    thread = %thread_id,
                    skipped,
                    "the codex subscription fell behind; resyncing the projection from paged turns"
                );
                resync(&threads, &projections, &thread_id).await;
            }
            Err(error) => {
                tracing::warn!(
                    thread = %thread_id,
                    "the codex thread subscription ended: {error}"
                );
                if let Some(projections) = projections.upgrade() {
                    projections.forget(&thread_id);
                }
                return;
            }
        }
    }
}

/// Rebuild a projection from persisted turns. A failed read leaves the
/// projection marked incomplete — the gap stays visible rather than vanishing.
///
/// `thread/read` carries only a bounded page, so the resync pages
/// `thread/turns/list` to exhaustion. The cache handle is re-upgraded around
/// each step rather than held across the network await: a replaced attachment
/// can still drop the cache (and abort this task) while a request is in flight.
async fn resync(threads: &Threads, projections: &std::sync::Weak<Projections>, thread_id: &str) {
    if let Some(projections) = projections.upgrade() {
        projections.mark_incomplete(thread_id);
    }
    match threads.all_turns(thread_id).await {
        Ok(turns) => {
            if let Some(projections) = projections.upgrade() {
                projections.resync(thread_id, &turns);
            }
        }
        Err(error) => tracing::warn!(
            thread = %thread_id,
            "could not resync the codex projection from thread/turns/list: {error}"
        ),
    }
}

fn client_error(error: CodexError) -> BridgeError {
    BridgeError::Client {
        client: "codex",
        detail: error.to_string(),
    }
}

fn not_attached() -> BridgeError {
    BridgeError::Client {
        client: "codex",
        detail: "not attached to a codex app-server daemon".to_string(),
    }
}

fn neutral_session_input(directory: Option<&str>) -> CreateSessionInput {
    CreateSessionInput {
        id: None,
        agent: None,
        model: None,
        location: directory.map(|directory| Location {
            directory: directory.to_string(),
        }),
    }
}

#[async_trait]
impl ServerLifecycle for CodexClient {
    /// One attached connection is already attached: there is no discovery to
    /// run here. The active client's lifecycle is [`CodexLifecycle`].
    async fn ensure_attached(&self, _request: EnsureAttached<'_>) -> Result<AttachOutcome> {
        Ok(AttachOutcome::Attached)
    }
}

#[async_trait]
impl Backend for CodexClient {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
        neutral_session_input(directory)
    }

    async fn create_session(&self, input: &CreateSessionInput) -> Result<Session> {
        if input.id.is_some() {
            // Codex assigns the thread id itself; a caller-supplied one cannot
            // be honoured without inventing an identity the server did not
            // issue.
            return Err(BridgeError::Client {
                client: "codex",
                detail: "thread/start assigns the thread id; a caller-supplied session id is not \
                         supported by this adapter"
                    .to_string(),
            });
        }
        let cwd = input
            .location
            .as_ref()
            .map(|location| location.directory.as_str())
            .filter(|directory| !directory.is_empty());
        let thread = self.threads().start(cwd).await.map_err(client_error)?;
        Ok(Session {
            id: thread.id.clone(),
            project_id: None,
            agent: None,
            title: (!thread.title().is_empty()).then(|| thread.title().to_string()),
            location: Some(json!({ "directory": thread.cwd })),
            cost: None,
            time: Some(crate::backend::types::SessionTime {
                created: thread.created_at,
                updated: thread.updated_at,
                archived: None,
            }),
        })
    }

    async fn list_sessions(&self) -> Result<Vec<SessionListInfo>> {
        self.threads().list_all_any().await.map_err(client_error)
    }

    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.threads()
            .rename(session_id, title)
            .await
            .map_err(client_error)
    }

    async fn delete_session(&self, session_id: &str) -> Result<()> {
        self.connection()
            .request("thread/delete", json!({ "threadId": session_id }))
            .await
            .map_err(client_error)?;
        self.projections.forget(session_id);
        Ok(())
    }

    async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        files: &[PromptFile],
        skills: &[PromptSkill],
        model: Option<&ModelInfo>,
        _variant: Option<&str>,
        _agent: Option<&str>,
        message_id: Option<&str>,
    ) -> Result<()> {
        if !files.is_empty() {
            return Err(BridgeError::Client {
                client: "codex",
                detail: "prompt attachments are not mapped onto this adapter yet".to_string(),
            });
        }
        if !skills.is_empty() {
            return Err(BridgeError::Client {
                client: "codex",
                detail: "skill attachments are not mapped onto this adapter yet".to_string(),
            });
        }
        let mut params = json!({
            "threadId": session_id,
            "input": [{ "type": "text", "text": text, "textElements": [] }],
        });
        if let Some(message_id) = message_id {
            params["clientUserMessageId"] = json!(message_id);
        }
        if let Some(model) = model {
            params["model"] = json!(model.id);
        }
        // `turn/start` is the write half of this ticket's read model; a failed
        // submit is its own error, while the Turn's outcome is observed from
        // the transcript and session status (ADR-0011).
        self.connection()
            .request("turn/start", params)
            .await
            .map_err(client_error)?;
        Ok(())
    }

    async fn reply_permission(
        &self,
        _session_id: &str,
        _request_id: &str,
        _reply: &str,
        _directory: Option<&str>,
    ) -> Result<()> {
        // The approval surface (and its neutral DTOs, decision 7) is #882.
        // Nothing lists a request on this adapter yet, so there is nothing to
        // reply to.
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

    async fn transcript(&self, session_id: &str) -> Result<SessionTranscript> {
        self.ensure_subscription(session_id).await.map_err(client_error)?;
        Ok(self.projections.transcript(session_id))
    }

    async fn session_diff(
        &self,
        session_id: &str,
        from: Option<&str>,
        _to: Option<&str>,
        _context: Option<u32>,
    ) -> Result<Option<TurnDiff>> {
        Ok(self.projections.diff(session_id, from))
    }

    async fn session_status(
        &self,
        session_id: &str,
        _directory: Option<&str>,
    ) -> Result<Option<SessionStatus>> {
        // The cache is not the server's status service: an unseeded projection
        // answering `Idle` would present "never observed" as an authoritative
        // idle run. Adopt the thread first (which seeds its live turn from the
        // resume history); a never-run thread then genuinely has no live run.
        if !self.projections.is_seeded(session_id) {
            self.ensure_subscription(session_id).await.map_err(client_error)?;
        }
        Ok(self.projections.status(session_id))
    }

    fn tasks(&self) -> &TasksHandle {
        &self.tasks
    }

    async fn model_context_window(&self, _provider: &str, _model: &str) -> Result<Option<i64>> {
        // The Codex usage notification states the window for the running
        // model; a read with no attached turn has no figure to report.
        Ok(self.projections.any_context_window())
    }

    fn configured_default_model(&self) -> Option<ModelInfo> {
        // The Codex model picker is a later ticket; no config model exists to
        // report, and a guess would be worse than the server default.
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

    async fn session_info(&self, session_id: &str, _directory: Option<&str>) -> Result<SessionInfo> {
        let thread = self
            .threads()
            .read(session_id, false)
            .await
            .map_err(client_error)?;
        let title = (!thread.title().is_empty()).then(|| thread.title().to_string());
        Ok(SessionInfo {
            id: thread.id,
            parent_id: thread.parent_thread_id,
            title,
            model: None,
        })
    }

    async fn interrupt(&self, session_id: &str) -> Result<()> {
        let Some(turn_id) = self.projections.live_turn(session_id) else {
            // No turn has been observed: nothing to interrupt. `/stop` on an
            // idle session is a no-op, exactly as the bridge expects.
            return Ok(());
        };
        self.connection()
            .request(
                "turn/interrupt",
                json!({ "threadId": session_id, "turnId": turn_id }),
            )
            .await
            .map_err(client_error)?;
        Ok(())
    }

    async fn compact(&self, session_id: &str) -> Result<()> {
        self.connection()
            .request("thread/compact/start", json!({ "threadId": session_id }))
            .await
            .map_err(client_error)?;
        Ok(())
    }

    fn base_url(&self) -> String {
        self.endpoint().display_url()
    }

    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn crate::backend::DirectoryBackend> {
        Arc::new(BackendDirectory::new(self, directory.to_string()))
    }
}

/// The active client the composition root installs in `SharedCore.client`:
/// the lifecycle owns discovery/attach, and the `Backend` surface delegates to
/// the client the last attach pass installed.
#[async_trait]
impl Backend for CodexLifecycle {
    fn new_session_input(&self, directory: Option<&str>) -> CreateSessionInput {
        neutral_session_input(directory)
    }

    async fn create_session(&self, input: &CreateSessionInput) -> Result<Session> {
        Backend::create_session(&self.current().await?, input).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionListInfo>> {
        Backend::list_sessions(&self.current().await?).await
    }

    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        Backend::update_session_title(&self.current().await?, session_id, title).await
    }

    async fn delete_session(&self, session_id: &str) -> Result<()> {
        Backend::delete_session(&self.current().await?, session_id).await
    }

    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<()> {
        Backend::prompt(
            &self.current().await?,
            session_id,
            text,
            files,
            skills,
            model,
            variant,
            agent,
            message_id,
        )
        .await
    }

    async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        reply: &str,
        directory: Option<&str>,
    ) -> Result<()> {
        Backend::reply_permission(&self.current().await?, session_id, request_id, reply, directory).await
    }

    async fn list_permissions(&self, directory: Option<&str>) -> Result<Vec<PermissionRequest>> {
        Backend::list_permissions(&self.current().await?, directory).await
    }

    async fn list_questions(&self, directory: Option<&str>) -> Result<Vec<QuestionRequest>> {
        Backend::list_questions(&self.current().await?, directory).await
    }

    async fn reply_question(
        &self,
        session_id: &str,
        request_id: &str,
        answers: &[FormAnswer],
        directory: Option<&str>,
    ) -> Result<()> {
        Backend::reply_question(&self.current().await?, session_id, request_id, answers, directory).await
    }

    async fn reject_question(
        &self,
        session_id: &str,
        request_id: &str,
        directory: Option<&str>,
    ) -> Result<()> {
        Backend::reject_question(&self.current().await?, session_id, request_id, directory).await
    }

    async fn transcript(&self, session_id: &str) -> Result<SessionTranscript> {
        Backend::transcript(&self.current().await?, session_id).await
    }

    async fn session_diff(
        &self,
        session_id: &str,
        from: Option<&str>,
        to: Option<&str>,
        context: Option<u32>,
    ) -> Result<Option<TurnDiff>> {
        Backend::session_diff(&self.current().await?, session_id, from, to, context).await
    }

    async fn session_status(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Option<SessionStatus>> {
        Backend::session_status(&self.current().await?, session_id, directory).await
    }

    fn tasks(&self) -> &TasksHandle {
        &self.tasks
    }

    async fn model_context_window(&self, provider: &str, model: &str) -> Result<Option<i64>> {
        Backend::model_context_window(&self.current().await?, provider, model).await
    }

    fn configured_default_model(&self) -> Option<ModelInfo> {
        None
    }

    async fn list_agents(&self) -> Vec<AgentInfo> {
        Vec::new()
    }

    async fn list_skills(&self, directory: Option<&str>) -> Vec<SkillInfo> {
        match self.attached_client().await {
            Some(client) => Backend::list_skills(&client, directory).await,
            None => Vec::new(),
        }
    }

    async fn list_models(&self) -> Vec<ProviderModels> {
        Vec::new()
    }

    async fn session_info(&self, session_id: &str, directory: Option<&str>) -> Result<SessionInfo> {
        Backend::session_info(&self.current().await?, session_id, directory).await
    }

    async fn interrupt(&self, session_id: &str) -> Result<()> {
        Backend::interrupt(&self.current().await?, session_id).await
    }

    async fn compact(&self, session_id: &str) -> Result<()> {
        Backend::compact(&self.current().await?, session_id).await
    }

    fn base_url(&self) -> String {
        // The control socket is the stable target even while serverless: a
        // replacement daemon keeps the same path, and the lifecycle's liveness
        // pass (not this string) decides whether it is the same server.
        format!("unix://{}", self.environment_socket().display())
    }

    fn for_directory(self: Arc<Self>, directory: &str) -> Arc<dyn crate::backend::DirectoryBackend> {
        Arc::new(BackendDirectory::new(self, directory.to_string()))
    }
}

impl CodexLifecycle {
    async fn current(&self) -> Result<CodexClient> {
        self.attached_client().await.ok_or_else(not_attached)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::codex::client::CodexOptions;
    use crate::codex::schema::PinnedProbe;
    use crate::codex::test_server::{FakeServer, ScriptedThread};
    use crate::codex::transport::Endpoint;

    const RECORDING: &str = include_str!("fixtures/turn-recording.json");

    async fn connect(url: &str) -> CodexClient {
        let endpoint = Endpoint::parse(url).expect("endpoint");
        CodexClient::connect(&CodexOptions::new(endpoint), &PinnedProbe)
            .await
            .expect("attach to the scripted app-server")
    }

    fn recording() -> Vec<Value> {
        serde_json::from_str(RECORDING).expect("the recording is a JSON array")
    }

    /// One persisted turn in the fake server's history: one agent-message
    /// item, with the turn status the projection reads.
    fn scripted_turn(id: &str, status: &str, started_at: i64, item_id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "status": status,
            "startedAt": started_at,
            "completedAt": (status != "inProgress").then_some(started_at + 1),
            "error": null,
            "items": [{ "type": "agentMessage", "id": item_id, "text": text }],
        })
    }

    fn tokens_total(transcript: &SessionTranscript) -> Option<i64> {
        transcript
            .messages
            .iter()
            .filter_map(|message| message.tokens.map(|tokens| tokens.total))
            .next()
    }

    /// Wait for one token-usage total. The fixture's usage notification is its
    /// last data event, so a matching total is the replay's terminal sentinel:
    /// nothing earlier in that pass remains in flight.
    async fn wait_for_tokens(client: &CodexClient, thread_id: &str, total: i64) -> SessionTranscript {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let transcript = client.transcript(thread_id).await.expect("transcript");
            if tokens_total(&transcript) == Some(total) {
                return transcript;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for token usage {total}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_text(client: &CodexClient, thread_id: &str, needle: &str) -> SessionTranscript {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let transcript = client.transcript(thread_id).await.expect("transcript");
            if transcript
                .messages
                .iter()
                .any(|message| message.text().contains(needle))
            {
                return transcript;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for `{needle}`"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Ticket #879: the recorded notification fixture replays through the real
    /// connection, and two reads deliver the same transcript with no duplicate
    /// items.
    #[tokio::test]
    async fn a_recorded_fixture_replays_over_the_subscription_without_duplicates() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;

        // The first read subscribes the thread; no history yet, so it is empty.
        assert!(
            client
                .transcript("thread-1")
                .await
                .expect("the empty read")
                .messages
                .is_empty()
        );
        server.wait_for_subscribers("thread-1", 1).await;

        for event in recording() {
            let method = event["method"].as_str().expect("recorded method");
            let pushed = server.push_to_thread("thread-1", method, event["params"].clone());
            assert_eq!(pushed, 1, "the subscriber receives `{method}`");
        }

        // Synchronise on the fixture's terminal usage marker, then on the
        // completed turn: agent text alone arrives several events too early and
        // would compare a mid-replay snapshot.
        let first = wait_for_tokens(&client, "thread-1", 12_345).await;
        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("session status"),
            Some(SessionStatus::Idle)
        );
        assert!(!first.truncated, "a complete recording is not a prefix");

        // Replay the whole recording over the live subscription, then push a
        // sentinel usage update: WebSocket order means seeing the sentinel
        // proves the second pass was fully folded, not merely that a stale
        // read happened to match.
        for event in recording() {
            let method = event["method"].as_str().unwrap();
            server.push_to_thread("thread-1", method, event["params"].clone());
        }
        server.push_to_thread(
            "thread-1",
            "thread/tokenUsage/updated",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "tokenUsage": {
                    "total": {
                        "totalTokens": 24690,
                        "inputTokens": 16000,
                        "cachedInputTokens": 8000,
                        "cacheWriteInputTokens": 0,
                        "outputTokens": 690,
                        "reasoningOutputTokens": 100
                    },
                    "last": {
                        "totalTokens": 99999,
                        "inputTokens": 1,
                        "cachedInputTokens": 0,
                        "cacheWriteInputTokens": 0,
                        "outputTokens": 1,
                        "reasoningOutputTokens": 0
                    },
                    "modelContextWindow": 128000
                }
            }),
        );
        let second = wait_for_tokens(&client, "thread-1", 99_999).await;
        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("session status"),
            Some(SessionStatus::Idle)
        );

        // The only expected difference is the sentinel usage; the second
        // replay must not add a message or double any streamed text.
        let mut first_messages = first.messages.clone();
        let mut second_messages = second.messages.clone();
        for message in first_messages.iter_mut().chain(second_messages.iter_mut()) {
            message.tokens = None;
        }
        assert_eq!(
            first_messages, second_messages,
            "a replayed recording produces the same transcript"
        );
        let ids: std::collections::HashSet<&str> = second
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(
            second.messages.len(),
            ids.len(),
            "a replayed read must not duplicate an item"
        );
        assert_eq!(
            second
                .messages
                .iter()
                .find(|message| message.id.as_str() == "item-agent")
                .unwrap()
                .text(),
            "The fix",
            "the replay must not double the streamed text"
        );

        // The turn boundary reached the status read: completed is idle.
        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("session status"),
            Some(SessionStatus::Idle)
        );
    }

    /// Contract finding: `session_status` is the server's live run state, so
    /// the first call on a busy thread must adopt and seed before answering —
    /// never present an unseeded cache as `Idle`.
    #[tokio::test]
    async fn a_status_first_read_on_a_busy_thread_is_busy() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a real thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "inProgress",
            1_700_000_000,
            "item-live",
            "working",
        )];
        server.seed_thread(thread);
        let client = connect(&url).await;

        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("status read"),
            Some(SessionStatus::Busy),
            "the status read seeded the live turn instead of guessing idle"
        );
        assert_eq!(server.resume_calls(), 1);
        server.wait_for_subscribers("thread-1", 1).await;
    }

    /// A bounded `thread/resume` page is never presented as the whole history:
    /// adoption pages `thread/turns/list` before the first transcript read.
    #[tokio::test]
    async fn adoption_pages_the_whole_history_without_a_lag() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a long thread");
        thread.turns = vec![
            scripted_turn("turn-1", "completed", 1_700_000_000, "item-1", "one"),
            scripted_turn("turn-2", "completed", 1_700_000_010, "item-2", "two"),
            scripted_turn("turn-3", "completed", 1_700_000_020, "item-3", "three"),
        ];
        // The bounded resume page carries only the first turn.
        thread.read_turn_limit = Some(1);
        server.seed_thread(thread);
        let client = connect(&url).await;

        let first = client.transcript("thread-1").await.expect("first read");
        assert_eq!(
            first.messages.len(),
            3,
            "the first read must not present the bounded resume page as complete"
        );
        assert!(!first.truncated);
        assert_eq!(
            first
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["item-1", "item-2", "item-3"]
        );
        assert!(
            server.turns_calls() >= 2,
            "three turns at two per page need paging"
        );
        let params = server.turns_params();
        assert_eq!(params[0]["itemsView"], "full");
        assert_eq!(params[0]["sortDirection"], "asc");
    }

    /// The `ThreadEvent::Lagged` path through the subscriber pages the history
    /// again, rather than resyncing from the bounded `thread/read` page.
    #[tokio::test]
    async fn a_lagged_subscription_resyncs_every_paged_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a long thread");
        thread.turns = vec![
            scripted_turn("turn-1", "completed", 1_700_000_000, "item-1", "one"),
            scripted_turn("turn-2", "completed", 1_700_000_010, "item-2", "two"),
            scripted_turn("turn-3", "completed", 1_700_000_020, "item-3", "three"),
        ];
        thread.read_turn_limit = Some(1);
        server.seed_thread(thread);
        let client = connect(&url).await;

        assert_eq!(
            client.transcript("thread-1").await.expect("adopt").messages.len(),
            3
        );
        server.wait_for_subscribers("thread-1", 1).await;
        let pages_before = server.turns_calls();
        client.threads().simulate_lag(3);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let resynced = loop {
            let transcript = client.transcript("thread-1").await.expect("resynced read");
            if transcript.messages.len() == 3 && server.turns_calls() > pages_before {
                break transcript;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the lag resync did not page the history again"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(!resynced.truncated, "a full paged resync is complete");
        assert_eq!(
            resynced
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["item-1", "item-2", "item-3"]
        );
    }

    /// A malformed `thread/turns/list` page is a decode error, not an empty
    /// successful history: the bounded resume page is kept and the projection
    /// stays explicitly truncated.
    #[tokio::test]
    async fn a_malformed_turns_page_leaves_the_first_read_truncated() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a long thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        thread.read_turn_limit = Some(1);
        thread.malformed_turns_page = true;
        server.seed_thread(thread);
        let client = connect(&url).await;

        let first = client.transcript("thread-1").await.expect("first read");
        assert_eq!(first.messages.len(), 1, "the bounded page is still shown");
        assert!(
            first.truncated,
            "a failed page must keep the projection incomplete"
        );
        assert_eq!(server.turns_calls(), 1);
    }

    /// A server that repeats its cursor is refused rather than looped, and the
    /// projection stays explicitly incomplete.
    #[tokio::test]
    async fn a_repeated_turns_cursor_leaves_the_first_read_truncated() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a long thread");
        thread.turns = vec![
            scripted_turn("turn-1", "completed", 1_700_000_000, "item-1", "one"),
            scripted_turn("turn-2", "completed", 1_700_000_010, "item-2", "two"),
            scripted_turn("turn-3", "completed", 1_700_000_020, "item-3", "three"),
        ];
        thread.read_turn_limit = Some(1);
        thread.repeat_turns_cursor = true;
        server.seed_thread(thread);
        let client = connect(&url).await;

        let first = client.transcript("thread-1").await.expect("first read");
        assert_eq!(first.messages.len(), 1, "the bounded page is still shown");
        assert!(
            first.truncated,
            "a repeated cursor must keep the projection incomplete"
        );
        assert!(
            server.turns_calls() >= 2,
            "the repeat is only detected after a second page"
        );
    }

    /// A dead subscription must not leave retained history looking seeded: the
    /// next status read re-adopts rather than answering stale state.
    #[tokio::test]
    async fn a_dead_subscription_reopens_on_the_next_status_read() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "a thread");
        thread.turns = vec![scripted_turn(
            "turn-1",
            "completed",
            1_700_000_000,
            "item-1",
            "one",
        )];
        server.seed_thread(thread);
        let client = connect(&url).await;

        assert_eq!(
            client
                .transcript("thread-1")
                .await
                .expect("subscribe")
                .messages
                .len(),
            1
        );
        assert_eq!(server.resume_calls(), 1);

        client.threads().simulate_close();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = client
                .session_status("thread-1", None)
                .await
                .expect("status read");
            if server.resume_calls() >= 2 {
                assert_eq!(status, Some(SessionStatus::Idle));
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "a dead subscription must be re-adopted by the next status read"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn finished_adoption_locks_are_pruned() {
        let projections = Projections::default();
        {
            let _first = projections.adoption_lock("thread-a");
            let _second = projections.adoption_lock("thread-b");
            assert_eq!(projections.adoption_lock_count(), 2);
        }
        // Both locks have no strong owner now; the next call prunes them.
        let _next = projections.adoption_lock("thread-c");
        assert_eq!(
            projections.adoption_lock_count(),
            1,
            "finished threads must not retain adoption-lock slots"
        );
    }

    /// Contract finding: adoption locks are per thread. A hung `thread/resume`
    /// for one never-run thread must not block an unrelated thread's first
    /// read.
    #[tokio::test]
    async fn a_hung_adoption_does_not_block_an_unrelated_thread() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut other =
            ScriptedThread::adopted("thread-other", "session-other", "/work/cola", "another thread");
        other.turns = vec![scripted_turn(
            "turn-other",
            "completed",
            1_700_000_000,
            "item-other",
            "ready",
        )];
        server.seed_thread(other);
        server.seed_thread(ScriptedThread::never_run(
            "thread-new",
            "session-new",
            "/work/cola",
        ));
        let client = connect(&url).await;

        server.silence_next_resume();
        let hung = tokio::spawn({
            let client = client.clone();
            async move { client.transcript("thread-new").await }
        });
        server.wait_for_resume_calls(1).await;

        let other = tokio::time::timeout(Duration::from_secs(1), client.transcript("thread-other"))
            .await
            .expect("an unrelated adoption must not wait on the hung one")
            .expect("the unrelated thread reads");
        assert_eq!(other.messages.len(), 1);

        hung.abort();
    }

    /// A mid-turn read must not block on the push stream, and it keeps the
    /// deltas already folded into the cache. The read is bounded by a timeout
    /// that a subscription-awaiting implementation would miss.
    #[tokio::test]
    async fn a_mid_turn_read_is_prompt_and_keeps_applied_deltas() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;

        // Stop the recording at the first agent delta: the turn is live.
        let events = recording();
        for event in &events {
            let method = event["method"].as_str().unwrap();
            server.push_to_thread("thread-1", method, event["params"].clone());
            if method == "item/agentMessage/delta" {
                break;
            }
        }
        let mid = tokio::time::timeout(Duration::from_secs(1), wait_for_text(&client, "thread-1", "The"))
            .await
            .expect("a mid-turn read must not block");
        assert_eq!(
            mid.messages
                .iter()
                .find(|message| message.id.as_str() == "item-agent")
                .expect("the streamed message")
                .text(),
            "The"
        );
        assert_eq!(
            client
                .session_status("thread-1", None)
                .await
                .expect("session status"),
            Some(SessionStatus::Busy),
            "the neutral status says the projection is live"
        );

        // The rest of the turn settles the same cache; nothing is dropped.
        for event in events
            .iter()
            .skip_while(|event| event["method"] != "item/agentMessage/delta")
            .skip(1)
        {
            let method = event["method"].as_str().unwrap();
            server.push_to_thread("thread-1", method, event["params"].clone());
        }
        let final_read = wait_for_text(&client, "thread-1", "The fix").await;
        assert_eq!(
            final_read
                .messages
                .iter()
                .find(|message| message.id.as_str() == "item-agent")
                .unwrap()
                .text(),
            "The fix"
        );
    }

    /// A never-run thread's read is empty, and the next read retries the
    /// subscription. That is the peer story: the user's own client runs the
    /// first turn, and cola starts mirroring the moment the rollout exists.
    #[tokio::test]
    async fn a_never_run_thread_retries_the_subscription_after_its_first_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::never_run(
            "thread-new",
            "session-root",
            "/work/cola",
        ));
        let client = connect(&url).await;

        assert!(
            client
                .transcript("thread-new")
                .await
                .expect("the empty read")
                .messages
                .is_empty()
        );
        assert_eq!(server.resume_calls(), 1, "the first read tried to resume");

        server.mark_history("thread-new");
        assert!(
            client
                .transcript("thread-new")
                .await
                .expect("the retried read")
                .messages
                .is_empty()
        );
        assert_eq!(
            server.resume_calls(),
            2,
            "a read after the first turn retries `thread/resume`"
        );
        server.wait_for_subscribers("thread-new", 1).await;
    }

    /// Turn Diff and token usage reach the neutral seam for a turn that changed
    /// a file.
    #[tokio::test]
    async fn turn_diff_and_token_usage_are_served_for_a_changed_file_turn() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        client.transcript("thread-1").await.expect("subscribe");
        server.wait_for_subscribers("thread-1", 1).await;
        for event in recording() {
            let method = event["method"].as_str().unwrap();
            server.push_to_thread("thread-1", method, event["params"].clone());
        }
        let _transcript = wait_for_text(&client, "thread-1", "The fix").await;
        let transcript = {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                let read = client.transcript("thread-1").await.expect("transcript");
                if read.messages.iter().any(|message| message.tokens.is_some()) {
                    break read;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for token usage"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        let diff = client
            .session_diff("thread-1", Some("item-user"), None, None)
            .await
            .expect("diff read")
            .expect("the turn's diff");
        assert_eq!(diff.0.len(), 1);
        assert_eq!(diff.0[0].path, "src/lib.rs");
        assert_eq!(diff.0[0].additions, 1);
        assert_eq!(diff.0[0].deletions, 1);

        let usage = transcript
            .messages
            .iter()
            .find_map(|message| message.tokens)
            .expect("token usage on the turn's newest assistant message");
        assert_eq!(usage.total, 12_345);
        assert_eq!(
            client
                .model_context_window("openai", "gpt-5")
                .await
                .expect("context window"),
            Some(128_000)
        );
    }

    /// The adapter is constructed by kind, and an unattached lifecycle is a
    /// named "not attached" read rather than an empty transcript that could be
    /// mistaken for a real one.
    #[tokio::test]
    async fn an_unattached_lifecycle_refuses_a_transcript_read_by_name() {
        let directory = tempfile::tempdir().expect("tempdir");
        let settings = crate::config::CodexConfig::default();
        let environment = crate::codex::discovery::Environment {
            home: directory.path().to_path_buf(),
            cli: directory.path().join("no-codex"),
            version_timeout: Duration::from_millis(50),
            start_timeout: Duration::from_millis(50),
            socket_ready_timeout: Duration::from_millis(50),
        };
        let lifecycle = CodexLifecycle::new(settings, environment, Box::new(crate::codex::ManagedProbe));
        let error = lifecycle
            .transcript("thread-1")
            .await
            .expect_err("an unattached client has no transcript");
        assert!(error.to_string().contains("not attached"), "{error}");
        let status_error = lifecycle
            .session_status("thread-1", None)
            .await
            .expect_err("no client to read");
        assert!(
            status_error.to_string().contains("not attached"),
            "{status_error}"
        );
    }
}
