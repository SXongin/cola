//! Thread discovery, adoption and subscription over `codex app-server`
//! (spec #860, ticket #878).
//!
//! app-server owns a thread store. `thread/list` and `thread/read` are the
//! discovery reads (server-global, cursor-paged); adopting a thread is
//! [`Threads::adopt`], which sends `thread/resume` — that call **is** the
//! subscription, and after it the server pushes `turn/*` and `item/*`
//! notifications on this connection only (ADR-0090's amendment, question 2).
//! The reader loop in [`super::transport`] routes those frames here; this
//! module fans them to the subscriber of the thread they name.
//!
//! Two facts from the spike shape the API:
//!
//! - **`thread/sessionId` is the session tree root**, not something derived
//!   from the thread id. Every [`Thread`] carries it as read from the wire.
//! - **A thread created by `thread/start` cannot be resumed until it has run a
//!   turn.** `thread/resume` answers `-32600 no rollout found` until then, so
//!   [`Threads::adopt`] reports [`Adoption::NoHistoryYet`] — a named state, not
//!   a transport error.
//!
//! Releasing a subscription sends `thread/unsubscribe`; the server keeps the
//! thread loaded for another subscriber (and for its 30-minute grace), and this
//! module never unsubscribes a thread another local handle still holds.
//!
//! The poll-shaped `Backend` (#879) serves `transcript` from what these
//! subscriptions accumulate: the read model lives in [`super::read_model`] and
//! the subscription/cache wiring in [`super::backend`]. This module stays the
//! fan-out surface — it does not interpret item payloads itself.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::backend::types::{SessionListInfo, SessionTime};
use crate::config::{ApprovalPolicy, ReasoningSummary, SandboxMode};

use super::error::{CodexError, Result};
use super::transport::{Inbound, RpcConnection};

/// The source kinds a human-facing session list shows: a TUI (`cli`), VS Code
/// (`vscode`) and another app-server client. `exec` is a one-shot run and the
/// `subAgent*` kinds are a thread's children, neither of which belongs in
/// cola's session list.
pub(crate) const INTERACTIVE_SOURCE_KINDS: &[&str] = &["cli", "vscode", "appServer"];

/// How many thread events one subscriber may fall behind before the broadcast
/// drops the oldest (reported as [`CodexError::SubscriptionLagged`]).
const EVENT_CAPACITY: usize = 1024;

/// The `-32600` detail `thread/resume` returns for a thread with no rollout
/// (ADR-0090's amendment, question 2): `no rollout found for thread id <id>`.
/// Matched together with the code and this thread's id, never on the phrase
/// alone.
const NO_ROLLOUT: &str = "no rollout found for thread id ";

/// One app-server thread, as `thread/list`, `thread/read` and `thread/resume`
/// return it. Only the fields cola reads today are decoded; turns and status
/// are the read model's (#879).
// `session_id` is the tree root the attach path (#881) reads from the wire;
// `source` and `status` feed `cola doctor` (#883). They are decoded here so the
// contract is exercised, not guessed from the thread id.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Thread {
    /// The thread's identity, used for every thread-scoped call.
    pub(crate) id: String,
    /// **The session tree root** (ADR-0090, ticket #878). Read from the wire;
    /// coalescing threads by session must never hash the thread id for it.
    pub(crate) session_id: String,
    /// The user-facing title, when the thread has one.
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// The parent thread of a sub-agent thread; absent for a top-level one.
    /// Mapped onto [`SessionListInfo::parent_id`], so the neutral child filter
    /// works unchanged.
    #[serde(default)]
    pub(crate) parent_thread_id: Option<String>,
    /// Usually the first user message — the title fallback.
    #[serde(default)]
    pub(crate) preview: String,
    pub(crate) cwd: String,
    /// The model this thread runs with, as `thread/list` / `thread/read`
    /// report it (issue #904). Nullable on the wire, and the only model fact
    /// Codex offers: its items carry none.
    #[serde(default)]
    pub(crate) model: Option<String>,
    /// The provider beside it (`modelProvider`), nullable too. cola shows it as
    /// the real source of the model rather than claiming OpenAI.
    #[serde(default)]
    pub(crate) model_provider: Option<String>,
    #[serde(default)]
    pub(crate) created_at: i64,
    #[serde(default)]
    pub(crate) updated_at: i64,
    #[serde(default)]
    pub(crate) source: Value,
    #[serde(default)]
    pub(crate) status: Value,
    #[serde(default)]
    pub(crate) turns: Vec<Value>,
}

impl Thread {
    /// The title the session list shows: the server's `name`, or its preview
    /// fallback (the first user message) when the thread was never named.
    pub(crate) fn title(&self) -> &str {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.preview)
    }

    /// The neutral row the session list consumes (spec #860, #870). `id` is the
    /// **thread** id — the identity every thread-scoped call takes — while the
    /// session tree root stays on [`Thread::session_id`], where `/attach` reads
    /// it (ADR-0090, ticket #878).
    ///
    /// `SessionListInfo` has **no field for the session tree root**, and the
    /// adapter does not smuggle one into `agent`/`model`: a list row is a
    /// selection/display surface only. Before attaching, the root is the one
    /// [`Threads::read`] returns for the selected id (or the `Thread` inside
    /// the [`Adoption::Subscribed`] response), and that read — never the listed
    /// id — is authoritative. `model` and `agent` have no equivalent on this
    /// wire yet, so they stay empty rather than being guessed at; the read
    /// model (#879) owns that mapping when it needs it.
    pub(crate) fn list_info(&self) -> SessionListInfo {
        SessionListInfo {
            id: self.id.clone(),
            title: self.title().to_string(),
            directory: self.cwd.clone(),
            parent_id: self.parent_thread_id.clone(),
            agent: None,
            model: None,
            time: Some(SessionTime {
                created: self.created_at,
                updated: self.updated_at,
                archived: None,
            }),
        }
    }
}

/// One page of [`Threads::list`], in the neutral session shape the list
/// surface consumes. `next_cursor` is `None` on the last page.
#[derive(Debug, Clone)]
pub(crate) struct ThreadPage {
    pub(crate) sessions: Vec<SessionListInfo>,
    pub(crate) next_cursor: Option<String>,
}

/// How adopting a thread ended.
#[derive(Debug)]
pub(crate) enum Adoption {
    /// `thread/resume` succeeded: the server now pushes this thread's
    /// `turn/*` and `item/*` notifications here.
    Subscribed(Box<ThreadSubscription>),
    /// The thread exists but has never run a turn, so it has no rollout to
    /// resume (`-32600 no rollout found`). Named, not an error: the caller can
    /// tell the user to run one turn in their client first.
    NoHistoryYet,
}

/// `thread/unsubscribe`'s answer. `NotSubscribed` is the server's reply for a
/// connection that holds no subscription to the thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum UnsubscribeStatus {
    NotLoaded,
    NotSubscribed,
    Unsubscribed,
}

/// What a subscription delivers. The method and params are kept as the wire
/// sent them; the read model (#879) decodes them into transcript parts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ThreadEvent {
    /// A notification the server pushed for this thread, with the connection's
    /// receipt sequence (the order the reader loop fanned it out).
    Notification { seq: u64, method: String, params: Value },
    /// The dispatcher fell behind the connection's push stream and dropped
    /// `skipped` events. A silent gap would render a transcript with holes, so
    /// this is delivered to every live subscriber, which can resync through
    /// `thread/read` (or end its subscription) instead of assuming the stream
    /// is complete.
    Lagged { skipped: u64 },
}

/// A live subscription to one thread.
///
/// Dropping it releases the local handle (the server-side subscription stays
/// until the connection closes); [`ThreadSubscription::release`] is the
/// explicit, server-acknowledged path the adapter's release uses.
pub(crate) struct ThreadSubscription {
    thread: Thread,
    events: broadcast::Receiver<ThreadEvent>,
    // Held for their `Drop`/release semantics: the guard releases this local
    // holder, and the connection sends the acknowledged unsubscribe.
    guard: ReleaseGuard,
    connection: RpcConnection,
}

impl std::fmt::Debug for ThreadSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ThreadSubscription")
            .field("thread", &self.thread.id)
            .finish_non_exhaustive()
    }
}

impl ThreadSubscription {
    /// The thread that was resumed.
    pub(crate) fn thread(&self) -> &Thread {
        &self.thread
    }

    /// The next event for this thread: a notification, or an explicit `Lagged`
    /// marker when the dispatcher dropped events before this one.
    pub(crate) async fn next_event(&mut self) -> Result<ThreadEvent> {
        match self.events.recv().await {
            Ok(event) => Ok(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => Err(CodexError::SubscriptionLagged {
                thread_id: self.thread.id.clone(),
                skipped,
            }),
            Err(broadcast::error::RecvError::Closed) => Err(CodexError::NoResponse(format!(
                "thread subscription {}",
                self.thread.id
            ))),
        }
    }

    /// Try the next already-queued event without awaiting. `Ok(None)` means the
    /// queue is empty right now; a lag is the same named error as
    /// [`Self::next_event`]'s.
    pub(crate) fn try_next_event(&mut self) -> Result<Option<ThreadEvent>> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => Ok(None),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                Err(CodexError::SubscriptionLagged {
                    thread_id: self.thread.id.clone(),
                    skipped,
                })
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => Err(CodexError::NoResponse(format!(
                "thread subscription {}",
                self.thread.id
            ))),
        }
    }

    /// Leave the subscription. Returns the server's answer when this was the
    /// last local handle for the thread, or `None` when another handle still
    /// holds it (no `thread/unsubscribe` is sent: the server must keep
    /// streaming to that handle). This is the **only** unsubscribe path, so a
    /// caller cannot stop a stream another handle is reading.
    pub(crate) async fn release(mut self) -> Result<Option<UnsubscribeStatus>> {
        // Claim the local release atomically: two handles releasing at once
        // cannot both decide they are the last (and neither can both skip).
        if !self.guard.take() {
            return Ok(None);
        }
        let result = self
            .connection
            .request("thread/unsubscribe", json!({ "threadId": self.thread.id }))
            .await?;
        let response: UnsubscribeResponse = decode("thread/unsubscribe", result)?;
        Ok(Some(response.status))
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct UnsubscribeResponse {
    status: UnsubscribeStatus,
}

/// The per-thread settings cola asks the daemon to apply (issues #898, #900)./// The sandbox/approval pair is optional: an unset one sends nothing, so the
/// daemon's own default stays in force — the behaviour before this existed. The
/// reasoning summary is optional in the same sense, but the client's config
/// always names one (`auto` by default), because the daemon's own default
/// leaves the model's reasoning invisible on the card.
///
/// These are the **only** levers in the released schema: named permission
/// profiles (`[permissions.<name>]`, defined in the daemon's own config — the
/// middle ground where `.git`/`.codex` are writable while the rest stays
/// sandboxed) are not client-schema fields, so cola cannot select one. Mutating
/// the daemon's config instead is not cola's to do: the daemon is shared, may be
/// started by the user's own client, and carries their provider tokens.
///
/// Every field is in the **pinned** 0.163 `ThreadStartParams` and
/// `ThreadResumeParams` (checked with `codex-cli 0.163.0 app-server
/// generate-json-schema`), with the same value sets, so no version gate guards
/// them. The summary is a config override rather than `TurnStartParams.summary`:
/// the live 0.163 daemon ignores the turn-level field (a turn asked for
/// `detailed` still recorded `none`), while the same value under
/// `thread/start`'s `config` takes effect and streams
/// `item/reasoning/summaryTextDelta`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ThreadSettings {
    /// `thread/start`'s `sandbox`.
    pub(crate) sandbox: Option<SandboxMode>,
    /// `thread/start`'s `approvalPolicy`.
    pub(crate) approval_policy: Option<ApprovalPolicy>,
    /// Whether `thread/resume` carries them too (`apply_on_adopt`). Off by
    /// default: an adopted thread may be shared with the user's own
    /// TUI/VSCode, and a silent override would change their session as well.
    pub(crate) apply_on_adopt: bool,
    /// Whether a `/model` provider pick may change an **existing** thread's
    /// provider through `thread/resume` (issue #910), behind a confirmation
    /// card. Off by default, for the same reason as `apply_on_adopt`: the
    /// provider is a thread-level fact every attached client sees.
    pub(crate) provider_switch_on_resume: bool,
    /// `thread/start`'s `config.model_reasoning_summary` (issue #900).
    pub(crate) reasoning_summary: Option<ReasoningSummary>,
}

impl ThreadSettings {
    /// Merge the overrides into a request's params. The caller decides whether
    /// the request may carry them: `thread/start` always may, `thread/resume`
    /// only for a client that set `apply_on_adopt`.
    fn add_overrides(self, params: &mut Value) {
        if let Some(sandbox) = self.sandbox {
            params["sandbox"] = json!(sandbox.as_str());
        }
        if let Some(policy) = self.approval_policy {
            params["approvalPolicy"] = json!(policy.as_str());
        }
        if let Some(summary) = self.reasoning_summary {
            params["config"] = json!({ "model_reasoning_summary": summary.as_str() });
        }
    }
}

impl From<&crate::config::CodexConfig> for ThreadSettings {
    /// The active client's options are the source: the same keys the file
    /// spells are the ones the thread requests carry.
    fn from(config: &crate::config::CodexConfig) -> Self {
        Self {
            sandbox: config.sandbox,
            approval_policy: config.approval_policy,
            apply_on_adopt: config.apply_on_adopt,
            provider_switch_on_resume: config.provider_switch_on_resume,
            reasoning_summary: Some(config.reasoning_summary),
        }
    }
}

/// The model/provider a thread is created or resumed with (issue #910).
///
/// `modelProvider` is a **thread-level** fact: `thread/start` and
/// `thread/resume` accept it, while `turn/start` has no such field — so a
/// provider never rides a turn, and cola must never invent one there. The model
/// is the turn's own field *and* a creation field; when it pairs with a provider
/// (a `[profiles.*]` preset) the two go in one creation call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ThreadModel<'a> {
    /// `thread/start`'s `model`: the model id the new thread runs.
    pub(crate) model: Option<&'a str>,
    /// `thread/start` / `thread/resume`'s `modelProvider`: the configured
    /// `[model_providers.<id>]` channel.
    pub(crate) provider: Option<&'a str>,
}

impl ThreadModel<'_> {
    /// Merge the pair into a request's params. An unset half sends nothing, so
    /// the daemon's own default stays in force.
    fn add_params(self, params: &mut Value) {
        if let Some(model) = self.model {
            params["model"] = json!(model);
        }
        if let Some(provider) = self.provider {
            params["modelProvider"] = json!(provider);
        }
    }
}

/// The thread surface over one attached connection.
#[derive(Clone)]
pub(crate) struct Threads {
    connection: RpcConnection,
    /// Outlives every subscription it handed out: the dispatcher is owned here,
    /// not by [`Threads`], so a subscription taken from a client that is later
    /// dropped still receives its stream.
    registry: Arc<Registry>,
    /// The overrides this client's threads are asked to run under (issues #898,
    /// #900); `Default` sends nothing.
    settings: ThreadSettings,
}

impl std::fmt::Debug for Threads {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Threads").finish_non_exhaustive()
    }
}

impl Threads {
    /// Start the dispatcher over an attached connection. One per
    /// [`super::CodexClient`]; the client owns it for its lifetime.
    pub(crate) fn new(connection: RpcConnection, settings: ThreadSettings) -> Self {
        let registry = Registry::start(connection.clone());
        Self {
            connection,
            registry,
            settings,
        }
    }

    /// The connection's newest inbound notification sequence — the page
    /// boundary a history rebuild records its derived status against.
    pub(crate) fn inbound_seq(&self) -> u64 {
        self.connection.inbound_seq()
    }

    /// List threads whose session cwd is exactly `cwd`, newest-page first as
    /// the server orders them. Pass the returned cursor back for the next
    /// page; [`Self::list_all`] does that until the end.
    // The directory-scoped reads are used by #881's discovery, not the neutral
    // `list_sessions` (which lists every thread).
    #[allow(dead_code)]
    pub(crate) async fn list(
        &self,
        cwd: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<ThreadPage> {
        self.list_page(Some(cwd), cursor, limit).await
    }

    /// One page of the server's thread list. `cwd` is the discovery scopes'
    /// filter; `None` asks for every thread (the neutral `list_sessions` read,
    /// which has no directory axis).
    async fn list_page(
        &self,
        cwd: Option<&str>,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<ThreadPage> {
        let mut params = json!({
            "sourceKinds": INTERACTIVE_SOURCE_KINDS,
        });
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        if let Some(cursor) = cursor {
            params["cursor"] = json!(cursor);
        }
        if let Some(limit) = limit {
            params["limit"] = json!(limit);
        }

        let result = self.connection.request("thread/list", params).await?;
        let response: ListResponse = decode("thread/list", result)?;
        Ok(ThreadPage {
            sessions: response.data.iter().map(Thread::list_info).collect(),
            next_cursor: response.next_cursor,
        })
    }

    /// Every thread for `cwd`, following `nextCursor` until the end. A server
    /// that repeats a cursor would otherwise loop forever, so that is a named
    /// protocol failure instead.
    #[allow(dead_code)]
    pub(crate) async fn list_all(&self, cwd: &str) -> Result<Vec<SessionListInfo>> {
        self.list_all_page(Some(cwd)).await
    }

    /// Every thread the server lists, with no cwd filter. The neutral
    /// `Backend::list_sessions` read has no directory argument; a Codex thread
    /// carries its cwd on the row for the caller to filter.
    pub(crate) async fn list_all_any(&self) -> Result<Vec<SessionListInfo>> {
        self.list_all_page(None).await
    }

    async fn list_all_page(&self, cwd: Option<&str>) -> Result<Vec<SessionListInfo>> {
        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        loop {
            let page = self.list_page(cwd, cursor.as_deref(), None).await?;
            sessions.extend(page.sessions);
            match page.next_cursor {
                None => return Ok(sessions),
                Some(next) => {
                    if !seen.insert(next.clone()) {
                        return Err(CodexError::Protocol {
                            method: "thread/list".to_string(),
                            detail: format!("the server repeated pagination cursor `{next}`"),
                        });
                    }
                    cursor = Some(next);
                }
            }
        }
    }

    /// Read one thread's metadata. `include_turns` asks the server for its
    /// persisted turns, but that page is **bounded** — the read model (#879)
    /// must page [`Self::all_turns`] when it needs the whole history, or it
    /// would mistake a prefix for the thread.
    pub(crate) async fn read(&self, thread_id: &str, include_turns: bool) -> Result<Thread> {
        let result = self
            .connection
            .request(
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": include_turns }),
            )
            .await?;
        let response: ReadResponse = decode("thread/read", result)?;
        Ok(response.thread)
    }

    /// Every persisted turn of `thread_id`, oldest first, following
    /// `nextCursor` until exhaustion. `thread/read` returns a bounded page, so
    /// a resync that needs the complete history reads turns through this
    /// method — never through the `thread/read` prefix.
    pub(crate) async fn all_turns(&self, thread_id: &str) -> Result<Vec<Value>> {
        let mut turns = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        loop {
            let mut params = json!({
                "threadId": thread_id,
                "sortDirection": "asc",
                "itemsView": "full",
            });
            if let Some(cursor) = cursor.as_deref() {
                params["cursor"] = json!(cursor);
            }
            let result = self.connection.request("thread/turns/list", params).await?;
            let page: TurnsResponse = decode("thread/turns/list", result)?;
            turns.extend(page.data);
            match page.next_cursor {
                None => return Ok(turns),
                Some(next) => {
                    if !seen.insert(next.clone()) {
                        return Err(CodexError::Protocol {
                            method: "thread/turns/list".to_string(),
                            detail: format!("the server repeated pagination cursor `{next}`"),
                        });
                    }
                    cursor = Some(next);
                }
            }
        }
    }

    /// Start a new thread (`thread/start`). The server assigns the thread id;
    /// `cwd` scopes it to the session's directory when the caller has one.
    /// The response is the same [`Thread`] `thread/read` returns.
    ///
    /// The configured sandbox/approval pair and reasoning summary ride along:
    /// cola creates this thread, so it owns the choice (issues #898, #900).
    /// `model` is the Pending Session's chosen model/provider pair (issue #910):
    /// a thread's provider can only be chosen when it is created, or by
    /// resuming it. Unset halves send nothing.
    pub(crate) async fn start(&self, cwd: Option<&str>, model: ThreadModel<'_>) -> Result<Thread> {
        let mut params = json!({});
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd);
        }
        model.add_params(&mut params);
        self.settings.add_overrides(&mut params);
        let result = self.connection.request("thread/start", params).await?;
        let response: ReadResponse = decode("thread/start", result)?;
        Ok(response.thread)
    }

    /// Adopt a thread: `thread/resume` is the subscription (ADR-0090's
    /// amendment, question 2). The receiver is registered **before** the call,
    /// so a turn that starts while the response is in flight is not lost.
    ///
    /// The overrides ride along only when the client opted in
    /// (`apply_on_adopt`): a resumed thread may be shared with the user's own
    /// client, so adopting alone must not change their session (issues #898,
    /// #900).
    pub(crate) async fn adopt(&self, thread_id: &str) -> Result<Adoption> {
        // The holder is counted before the await and released by `guard`'s
        // `Drop` on every exit that does not hand it to a returned
        // subscription — including a caller cancelling this future while
        // `thread/resume` is in flight. A holder that survived a cancellation
        // would make the next real handle look non-last and skip
        // `thread/unsubscribe` forever.
        let guard = ReleaseGuard {
            thread_id: thread_id.to_string(),
            registry: Arc::clone(&self.registry),
            released: false,
        };
        let events = self.registry.subscribe(thread_id);
        let mut params = json!({ "threadId": thread_id });
        // Opt-in: adopting a thread the user's own client may share must not
        // silently reconfigure it (issue #898).
        if self.settings.apply_on_adopt {
            self.settings.add_overrides(&mut params);
        }
        let result = self.connection.request("thread/resume", params).await;

        match result {
            Ok(result) => {
                let response: ResumeResponse = decode("thread/resume", result)?;
                // `guard` moves into the subscription: its holder now lives as
                // long as the subscription and is released by `release` or by
                // dropping it.
                Ok(Adoption::Subscribed(Box::new(ThreadSubscription {
                    thread: response.thread,
                    events,
                    guard,
                    connection: self.connection.clone(),
                })))
            }
            // A never-run thread has no rollout; that is a state of the
            // thread, not a transport failure (the spike's exact answer).
            Err(CodexError::Rpc { code, detail, .. }) if no_history(code, thread_id, &detail) => {
                Ok(Adoption::NoHistoryYet)
            }
            Err(error) => Err(error),
        }
    }

    /// Override a thread's provider by resuming it (issue #910). This is the
    /// **only** way an existing thread's channel changes — `turn/start` has no
    /// provider field — and it is deliberately *not* [`Self::adopt`]: the caller
    /// already holds this thread's subscription, so re-resuming must not add a
    /// second local holder (which would leave the server subscribed for good).
    ///
    /// No silent fallback: a server that answers with a **different** provider
    /// did not apply the switch, and that is a named error. A response that
    /// omits the field is accepted (it is nullable on the wire).
    pub(crate) async fn resume_with_provider(&self, thread_id: &str, provider: &str) -> Result<Thread> {
        let mut params = json!({ "threadId": thread_id });
        ThreadModel {
            model: None,
            provider: Some(provider),
        }
        .add_params(&mut params);
        let result = self.connection.request("thread/resume", params).await?;
        let response: ResumeResponse = decode("thread/resume", result)?;
        if let Some(observed) = response.thread.model_provider.as_deref()
            && observed != provider
        {
            return Err(CodexError::Protocol {
                method: "thread/resume".to_string(),
                detail: format!("the server kept provider `{observed}` instead of `{provider}`"),
            });
        }
        Ok(response.thread)
    }

    /// Whether this client's threads may have their provider changed by a
    /// resume (`[clients.<id>] provider_switch_on_resume`, issue #910).
    pub(crate) fn provider_switch_on_resume(&self) -> bool {
        self.settings.provider_switch_on_resume
    }

    /// End every live subscription's stream (test-only), as a broken
    /// connection would.
    #[cfg(test)]
    pub(crate) fn simulate_close(&self) {
        self.registry.close_all();
    }

    /// Deliver a lag marker to every live subscription (test-only): the same
    /// path the connection dispatcher takes when it falls behind, so the
    /// subscriber's resync is exercised rather than simulated separately.
    #[cfg(test)]
    pub(crate) fn simulate_lag(&self, skipped: u64) {
        self.registry.lag_all(skipped);
    }

    /// The local handle count for a thread (test-only): proves a cancelled
    /// adopt did not leak a holder.
    #[cfg(test)]
    pub(crate) fn local_holders(&self, thread_id: &str) -> usize {
        self.registry
            .threads
            .lock()
            .expect("thread registry")
            .get(thread_id)
            .map(|entry| entry.holders)
            .unwrap_or(0)
    }

    /// Rename a thread (`thread/name/set`); the next `thread/list` reads the
    /// stored name.
    pub(crate) async fn rename(&self, thread_id: &str, name: &str) -> Result<()> {
        self.connection
            .request("thread/name/set", json!({ "threadId": thread_id, "name": name }))
            .await?;
        Ok(())
    }
}

/// Whether `thread/resume`'s error is the documented no-rollout answer for
/// **this** thread: `-32600` and exactly `no rollout found for thread id
/// <id>`. A near-match (another code, another thread's id, or a message that
/// merely contains the phrase) is not this state and stays an error.
fn no_history(code: Option<i64>, thread_id: &str, detail: &str) -> bool {
    if code != Some(-32600) {
        return false;
    }
    let Some(rest) = detail.trim().strip_prefix(NO_ROLLOUT) else {
        return false;
    };
    rest.trim() == thread_id
}

fn decode<T: DeserializeOwned>(method: &str, value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|error| CodexError::Protocol {
        method: method.to_string(),
        detail: error.to_string(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListResponse {
    #[serde(default)]
    data: Vec<Thread>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadResponse {
    thread: Thread,
}

#[derive(Debug, Deserialize)]
struct ResumeResponse {
    thread: Thread,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnsResponse {
    /// Required: a page without `data` is malformed, not an empty page. A
    /// default here would let a broken response clear the resync's incomplete
    /// marker with no turns at all.
    data: Vec<Value>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// The local subscription table: one broadcast channel per adopted thread.
///
/// `holders` counts local handles, not server subscribers: the server fans a
/// thread to every connection that resumed it, and this table only decides when
/// *this* connection's last handle may unsubscribe.
#[derive(Default)]
struct Registry {
    threads: Mutex<HashMap<String, Entry>>,
    /// The dispatcher task. It captures only a `Weak`, so this handle does not
    /// keep the registry alive; aborting it on the registry's drop stops the
    /// routing when the last subscription and the [`Threads`] are gone.
    dispatcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        if let Some(dispatcher) = self.dispatcher.get_mut().expect("dispatcher").take() {
            dispatcher.abort();
        }
    }
}

struct Entry {
    sender: broadcast::Sender<ThreadEvent>,
    holders: usize,
}

impl Registry {
    /// Build the registry and start its dispatcher over `connection`'s
    /// notification broadcast.
    fn start(connection: RpcConnection) -> Arc<Self> {
        let registry = Arc::new(Self::default());
        let dispatcher = tokio::spawn(dispatch(connection.inbound(), Arc::downgrade(&registry)));
        *registry.dispatcher.lock().expect("dispatcher") = Some(dispatcher);
        registry
    }

    fn subscribe(&self, thread_id: &str) -> broadcast::Receiver<ThreadEvent> {
        let mut threads = self.threads.lock().expect("thread registry");
        let entry = threads.entry(thread_id.to_string()).or_insert_with(|| {
            let (sender, _) = broadcast::channel(EVENT_CAPACITY);
            Entry { sender, holders: 0 }
        });
        entry.holders += 1;
        entry.sender.subscribe()
    }

    /// Drop one local handle. Returns whether it was the last one.
    fn release(&self, thread_id: &str) -> bool {
        let mut threads = self.threads.lock().expect("thread registry");
        let Some(entry) = threads.get_mut(thread_id) else {
            return true;
        };
        entry.holders = entry.holders.saturating_sub(1);
        if entry.holders == 0 {
            threads.remove(thread_id);
            true
        } else {
            false
        }
    }

    fn sender(&self, thread_id: &str) -> Option<broadcast::Sender<ThreadEvent>> {
        self.threads
            .lock()
            .expect("thread registry")
            .get(thread_id)
            .map(|entry| entry.sender.clone())
    }

    /// Deliver an explicit lag marker to every live subscription.
    fn lag_all(&self, skipped: u64) {
        let senders: Vec<_> = self
            .threads
            .lock()
            .expect("thread registry")
            .values()
            .map(|entry| entry.sender.clone())
            .collect();
        for sender in senders {
            let _ = sender.send(ThreadEvent::Lagged { skipped });
        }
    }

    /// Drop every per-thread sender, so each subscription's stream reports
    /// `Closed` when the connection ends instead of waiting forever.
    fn close_all(&self) {
        self.threads.lock().expect("thread registry").clear();
    }
}

/// The handle whose drop releases one local subscription. Kept separate so
/// `ThreadSubscription::release` can ask whether it is the last **before**
/// consuming itself.
struct ReleaseGuard {
    thread_id: String,
    registry: Arc<Registry>,
    released: bool,
}

impl ReleaseGuard {
    /// Drop this local handle and report whether it was the last one.
    fn take(&mut self) -> bool {
        if self.released {
            return false;
        }
        self.released = true;
        self.registry.release(&self.thread_id)
    }
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if !self.released {
            self.registry.release(&self.thread_id);
        }
    }
}

/// Route notifications by `params.threadId` to the subscriber of that thread.
/// Broadcasts without a thread (`thread/started`, `remoteControl/…`) are not a
/// subscription's traffic, and server requests ride their own lossless stream
/// (the approval surface, #882) instead of this broadcast.
async fn dispatch(mut inbound: broadcast::Receiver<Inbound>, registry: Weak<Registry>) {
    loop {
        let event = inbound.recv().await;
        // Upgrade per event, never across the await above: a strong handle held
        // by this task would keep the registry (and this task) alive forever.
        let Some(registry) = registry.upgrade() else {
            return;
        };
        match event {
            Ok(Inbound::Notification { seq, method, params }) => {
                let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(sender) = registry.sender(thread_id) {
                    // No receiver means every local handle already dropped;
                    // the server stops sending once the connection unsubscribes
                    // or closes, so a dropped event costs nothing.
                    let _ = sender.send(ThreadEvent::Notification { seq, method, params });
                }
            }
            Ok(Inbound::Closed) => {
                registry.close_all();
                return;
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // Losing events here would silently punch holes in a
                // transcript. Tell every live subscription how many were
                // dropped so its consumer can resync from `thread/read`.
                tracing::warn!(
                    "the codex connection's push stream outran the thread dispatcher by \
                     {skipped} event(s); marking every subscription lagged"
                );
                registry.lag_all(skipped);
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::codex::client::{CodexClient, CodexOptions};
    use crate::codex::schema::PinnedProbe;
    use crate::codex::test_server::{FakeServer, ScriptedThread};
    use crate::codex::transport::Endpoint;

    fn notification(event: ThreadEvent) -> (String, Value) {
        match event {
            ThreadEvent::Notification { method, params, .. } => (method, params),
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    async fn connect(url: &str) -> CodexClient {
        let endpoint = Endpoint::parse(url).expect("endpoint");
        CodexClient::connect(&CodexOptions::new(endpoint), &PinnedProbe)
            .await
            .expect("attach to the scripted app-server")
    }

    async fn connect_with(url: &str, settings: ThreadSettings) -> CodexClient {
        let endpoint = Endpoint::parse(url).expect("endpoint");
        CodexClient::connect(
            &CodexOptions::new(endpoint).with_thread_settings(settings),
            &PinnedProbe,
        )
        .await
        .expect("attach to the scripted app-server")
    }

    /// Issue #898: cola creates the thread, so the configured pair rides on
    /// `thread/start` — exactly the values the file spelled, not a default.
    #[tokio::test]
    async fn thread_start_carries_the_configured_sandbox_and_approval_policy() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let client = connect_with(
            &url,
            ThreadSettings {
                sandbox: Some(SandboxMode::DangerFullAccess),
                approval_policy: Some(ApprovalPolicy::OnRequest),
                apply_on_adopt: false,
                provider_switch_on_resume: false,
                reasoning_summary: None,
            },
        )
        .await;

        let thread = client
            .threads()
            .start(Some("/work/cola"), ThreadModel::default())
            .await
            .expect("start");

        assert_eq!(thread.cwd, "/work/cola");
        let params = server.start_params();
        assert_eq!(params.len(), 1, "one thread/start: {params:?}");
        assert_eq!(params[0]["cwd"], "/work/cola");
        assert_eq!(params[0]["sandbox"], "danger-full-access");
        assert_eq!(params[0]["approvalPolicy"], "on-request");
    }

    /// Issue #900: the reasoning summary is a `thread/start` config override.
    /// The daemon's own default on the pinned build is `none`, so a client that
    /// did not set one would keep every reasoning item empty; the adapter's
    /// config always names it, and `thread/resume` stays opt-in like the rest.
    #[tokio::test]
    async fn thread_start_carries_the_reasoning_summary_and_resume_only_when_opted_in() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a shared thread",
        ));
        let settings = ThreadSettings {
            reasoning_summary: Some(ReasoningSummary::Detailed),
            ..ThreadSettings::default()
        };
        let client = connect_with(&url, settings).await;

        client
            .threads()
            .start(Some("/work/cola"), ThreadModel::default())
            .await
            .expect("start");
        let params = server.start_params();
        assert_eq!(
            params[0]["config"],
            json!({ "model_reasoning_summary": "detailed" }),
            "the summary rides thread/start's config: {params:?}"
        );
        assert!(
            params[0].get("sandbox").is_none() && params[0].get("approvalPolicy").is_none(),
            "the summary alone must not invent the other overrides: {params:?}"
        );

        let Adoption::Subscribed(_first) = client.threads().adopt("thread-1").await.expect("adopt") else {
            panic!("must subscribe");
        };
        assert_eq!(
            server.resume_params(),
            vec![json!({ "threadId": "thread-1" })],
            "adopting alone must not reconfigure a shared thread"
        );

        let opted_in = ThreadSettings {
            reasoning_summary: Some(ReasoningSummary::Auto),
            apply_on_adopt: true,
            ..ThreadSettings::default()
        };
        let client = connect_with(&url, opted_in).await;
        let Adoption::Subscribed(_second) = client.threads().adopt("thread-1").await.expect("adopt") else {
            panic!("must subscribe");
        };
        let params = server.resume_params();
        assert_eq!(
            params[1]["config"],
            json!({ "model_reasoning_summary": "auto" }),
            "the opt-in extends the summary to an adopted thread: {params:?}"
        );
    }

    /// Unset means "send nothing": the daemon's own default stays in force, so
    /// a client that did not opt in behaves exactly as before #898.
    #[tokio::test]
    async fn thread_start_sends_no_overrides_when_they_are_unset() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let client = connect(&url).await;

        client
            .threads()
            .start(Some("/work/cola"), ThreadModel::default())
            .await
            .expect("start");

        let params = server.start_params();
        assert_eq!(params.len(), 1, "one thread/start: {params:?}");
        assert_eq!(params[0], json!({ "cwd": "/work/cola" }));
    }

    /// Adopting a thread may take it away from the user's own client, so the
    /// overrides are opt-in on `thread/resume` (issue #898): off means the
    /// request carries only the thread id.
    #[tokio::test]
    async fn thread_resume_carries_the_overrides_only_when_opted_in() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a shared thread",
        ));

        let overrides = ThreadSettings {
            sandbox: Some(SandboxMode::DangerFullAccess),
            approval_policy: Some(ApprovalPolicy::Never),
            apply_on_adopt: false,
            provider_switch_on_resume: false,
            reasoning_summary: None,
        };
        let client = connect_with(&url, overrides).await;
        let Adoption::Subscribed(_first) = client.threads().adopt("thread-1").await.expect("adopt") else {
            panic!("must subscribe");
        };
        assert_eq!(
            server.resume_params(),
            vec![json!({ "threadId": "thread-1" })],
            "adopting alone must not reconfigure a shared thread"
        );

        // A second connection with the opt-in sends the same pair.
        let opted_in = ThreadSettings {
            apply_on_adopt: true,
            ..overrides
        };
        let second = connect_with(&url, opted_in).await;
        let Adoption::Subscribed(_second) = second.threads().adopt("thread-1").await.expect("adopt") else {
            panic!("must subscribe");
        };
        let params = server.resume_params();
        assert_eq!(params.len(), 2, "two resumes: {params:?}");
        assert_eq!(params[1]["threadId"], "thread-1");
        assert_eq!(params[1]["sandbox"], "danger-full-access");
        assert_eq!(params[1]["approvalPolicy"], "never");
    }

    /// Issue #910: a new thread takes its provider at creation. The pair — a
    /// `[profiles.*]` preset's model + provider — goes in ONE `thread/start`,
    /// because a provider has no other creation path and `turn/start` has no
    /// provider field at all.
    #[tokio::test]
    async fn thread_start_carries_the_chosen_model_and_provider() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let client = connect(&url).await;

        let thread = client
            .threads()
            .start(
                Some("/work/cola"),
                ThreadModel {
                    model: Some("deepseek-v4-pro"),
                    provider: Some("deepseek"),
                },
            )
            .await
            .expect("start");

        let params = server.start_params();
        assert_eq!(params.len(), 1, "one thread/start: {params:?}");
        assert_eq!(params[0]["cwd"], "/work/cola");
        assert_eq!(params[0]["model"], "deepseek-v4-pro");
        assert_eq!(params[0]["modelProvider"], "deepseek");
        assert_eq!(
            thread.model_provider.as_deref(),
            Some("deepseek"),
            "the response is the thread the server now holds"
        );
    }

    /// Issue #910: an existing thread's provider changes through
    /// `thread/resume`, and that request carries **only** the provider — the
    /// model and the thread's other overrides are not re-sent, so a provider
    /// switch cannot change the model.
    #[tokio::test]
    async fn resume_with_provider_sends_only_the_provider() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a shared thread",
        ));
        server.set_thread_model("thread-1", Some("gpt-5.1-codex"), Some("openai"));
        let client = connect_with(
            &url,
            ThreadSettings {
                sandbox: Some(SandboxMode::DangerFullAccess),
                approval_policy: Some(ApprovalPolicy::OnRequest),
                apply_on_adopt: false,
                provider_switch_on_resume: true,
                reasoning_summary: None,
            },
        )
        .await;

        // The thread is already subscribed by this connection: the switch must
        // not add a second local holder (that would strand the server's
        // subscription forever).
        let Adoption::Subscribed(subscription) = client.threads().adopt("thread-1").await.expect("adopt")
        else {
            panic!("must subscribe");
        };
        assert_eq!(client.threads().local_holders("thread-1"), 1);

        let thread = client
            .threads()
            .resume_with_provider("thread-1", "deepseek")
            .await
            .expect("switch the provider");

        assert_eq!(
            thread.model_provider.as_deref(),
            Some("deepseek"),
            "the server's answer names the new provider"
        );
        assert_eq!(
            thread.model.as_deref(),
            Some("gpt-5.1-codex"),
            "a provider switch leaves the thread's model alone"
        );
        let params = server.resume_params();
        assert_eq!(params.len(), 2, "the adopt, then the switch: {params:?}");
        assert_eq!(
            params[1],
            json!({ "threadId": "thread-1", "modelProvider": "deepseek" })
        );
        assert_eq!(
            client.threads().local_holders("thread-1"),
            1,
            "a provider switch is not a second subscription"
        );
        drop(subscription);
    }

    /// Issue #910: no silent fallback. A server that accepted `thread/resume`
    /// but kept the old provider is a named protocol error, and a refused one
    /// is the server's own error — neither reports success.
    #[tokio::test]
    async fn resume_with_provider_refuses_a_server_that_kept_another_provider() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a shared thread",
        ));
        server.set_thread_model("thread-1", Some("gpt-5.1-codex"), Some("openai"));
        let client = connect(&url).await;

        server.keep_provider_on_resume();
        let error = client
            .threads()
            .resume_with_provider("thread-1", "deepseek")
            .await
            .expect_err("a server that kept `openai` did not switch");
        let message = error.to_string();
        assert!(
            message.contains("openai"),
            "names what the server kept: {message}"
        );
        assert!(message.contains("deepseek"), "names what was asked: {message}");

        server.reject_next_resume("unknown provider: nope");
        let error = client
            .threads()
            .resume_with_provider("thread-1", "nope")
            .await
            .expect_err("the server refused the provider");
        assert!(
            error.to_string().contains("unknown provider"),
            "the server's reason survives: {error}"
        );
    }

    /// Ticket #878 acceptance 1: the list is scoped to the directory, shows
    /// the stored titles, reads `sessionId` from the wire, and paging reaches
    /// the end (the scripted page size is 2, so five threads are three calls).
    #[tokio::test]
    async fn the_list_pages_to_the_end_for_the_directory() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        for index in 0..5 {
            server.seed_thread(ScriptedThread::adopted(
                &format!("thread-{index}"),
                &format!("session-root-{index}"),
                "/work/cola",
                &format!("title {index}"),
            ));
        }
        server.seed_thread(ScriptedThread::adopted(
            "thread-elsewhere",
            "session-elsewhere",
            "/work/other",
            "not this directory",
        ));
        // A sub-agent thread in the same directory keeps its parent, so the
        // neutral child filter (`parent_id`) sees it.
        let mut child = ScriptedThread::adopted(
            "thread-child",
            "session-root-0",
            "/work/cola",
            "a sub-agent thread",
        );
        child.parent_thread_id = Some("thread-0".to_string());
        server.seed_thread(child);
        let client = connect(&url).await;

        let sessions = client
            .threads()
            .list_all("/work/cola")
            .await
            .expect("the list pages to the end");

        assert_eq!(
            sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            [
                "thread-0",
                "thread-1",
                "thread-2",
                "thread-3",
                "thread-4",
                "thread-child"
            ]
        );
        assert!(sessions[5].is_child(), "a sub-agent thread keeps its parent");
        assert_eq!(sessions[0].title, "title 0");
        assert_eq!(sessions[3].directory, "/work/cola");
        assert_eq!(
            sessions[0].time.as_ref().map(|time| time.updated),
            Some(1_700_000_000)
        );
        assert_eq!(server.list_calls(), 3, "6 threads at 2 per page is 3 calls");

        let params = server.list_params();
        assert_eq!(params[0]["cwd"], "/work/cola");
        assert_eq!(
            params[0]["sourceKinds"],
            json!(INTERACTIVE_SOURCE_KINDS),
            "the list asks for the interactive source kinds"
        );
        assert!(params[0].get("cursor").is_none(), "the first page has no cursor");
        assert_eq!(params[1]["cursor"], "2");
        assert_eq!(params[2]["cursor"], "4");
    }

    /// `thread/read` returns one thread with the same identity fields; a title
    /// falls back to the preview when the thread was never named.
    #[tokio::test]
    async fn read_returns_one_thread_and_title_falls_back_to_the_preview() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::never_run(
            "thread-new",
            "session-root",
            "/work/cola",
        ));
        let client = connect(&url).await;

        let thread = client.threads().read("thread-new", false).await.expect("read");
        assert_eq!(thread.id, "thread-new");
        assert_eq!(thread.session_id, "session-root");
        assert_eq!(thread.title(), "", "a never-run thread has no preview yet");
        assert_eq!(thread.cwd, "/work/cola");
    }

    /// Ticket #878 acceptance 2: adopting a thread with history subscribes it,
    /// and an item event for it arrives on this connection.
    #[tokio::test]
    async fn adopting_a_thread_with_history_streams_its_items() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;

        let Adoption::Subscribed(mut subscription) = client
            .threads()
            .adopt("thread-1")
            .await
            .expect("adopt the thread")
        else {
            panic!("a thread with history must subscribe");
        };
        assert_eq!(subscription.thread().session_id, "session-root");
        assert_eq!(server.resume_calls(), 1);
        server.wait_for_subscribers("thread-1", 1).await;

        assert_eq!(server.push_item("thread-1", "item-1", "hello from the server"), 1);
        let event = tokio::time::timeout(Duration::from_secs(2), subscription.next_event())
            .await
            .expect("the item arrives on this connection")
            .expect("a thread event");
        let (method, params) = notification(event);
        assert_eq!(method, "item/completed");
        assert_eq!(params["item"]["text"], "hello from the server");
        assert_eq!(params["threadId"], "thread-1");
    }

    /// Ticket #878 acceptance 3: a never-run thread reports the documented
    /// state, not a transport error, and leaves no local subscription behind.
    #[tokio::test]
    async fn adopting_a_never_run_thread_reports_no_history_yet() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::never_run(
            "thread-new",
            "session-root",
            "/work/cola",
        ));
        let client = connect(&url).await;

        let adoption = client
            .threads()
            .adopt("thread-new")
            .await
            .expect("a never-run thread is a state, not a failure");

        assert!(matches!(adoption, Adoption::NoHistoryYet));
        assert_eq!(server.subscribers("thread-new"), 0);
        assert_eq!(
            server.unsubscribe_calls(),
            0,
            "this connection never subscribed, so it sends no unsubscribe"
        );
    }

    /// Ticket #878 acceptance 4: a rename reaches the server and the next list
    /// read shows it.
    #[tokio::test]
    async fn a_rename_round_trips_through_the_next_list() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        let mut thread = ScriptedThread::adopted("thread-1", "session-root", "/work/cola", "first message");
        thread.name = None;
        server.seed_thread(thread);
        let client = connect(&url).await;

        let listed = client.threads().list_all("/work/cola").await.expect("list");
        assert_eq!(listed[0].title, "first message");

        client
            .threads()
            .rename("thread-1", "renamed in Feishu")
            .await
            .expect("rename");
        assert_eq!(
            server.thread_name("thread-1").as_deref(),
            Some("renamed in Feishu")
        );

        let listed = client.threads().list_all("/work/cola").await.expect("re-list");
        assert_eq!(listed[0].title, "renamed in Feishu");
    }

    /// Ticket #878 acceptance 5: two connections subscribe to one thread;
    /// releasing one leaves the other's stream intact (the spike's guarantee
    /// that `thread/unsubscribe` does not unload a thread another subscriber
    /// holds).
    #[tokio::test]
    async fn releasing_one_subscription_leaves_another_connection_streaming() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "shared thread",
        ));
        let first = connect(&url).await;
        let second = connect(&url).await;

        let Adoption::Subscribed(mut first_subscription) =
            first.threads().adopt("thread-1").await.expect("first adopts")
        else {
            panic!("the first connection must subscribe");
        };
        let Adoption::Subscribed(mut second_subscription) =
            second.threads().adopt("thread-1").await.expect("second adopts")
        else {
            panic!("the second connection must subscribe");
        };
        server.wait_for_subscribers("thread-1", 2).await;

        assert_eq!(
            server.push_item("thread-1", "item-1", "both see this"),
            2,
            "a subscribed thread fans out to both connections"
        );
        for subscription in [&mut first_subscription, &mut second_subscription] {
            let event = tokio::time::timeout(Duration::from_secs(2), subscription.next_event())
                .await
                .expect("both streams receive the first item")
                .expect("a thread event");
            let (_, params) = notification(event);
            assert_eq!(params["item"]["text"], "both see this");
        }

        let status = first_subscription
            .release()
            .await
            .expect("release the first subscription");
        assert_eq!(status, Some(UnsubscribeStatus::Unsubscribed));
        assert_eq!(server.subscribers("thread-1"), 1, "only the first left");

        assert_eq!(
            server.push_item("thread-1", "item-2", "only the second sees this"),
            1,
            "the remaining connection still owns the subscription"
        );
        let event = tokio::time::timeout(Duration::from_secs(2), second_subscription.next_event())
            .await
            .expect("the remaining stream is intact")
            .expect("a thread event");
        let (_, params) = notification(event);
        assert_eq!(params["item"]["text"], "only the second sees this");

        assert_eq!(
            second_subscription
                .release()
                .await
                .expect("release the last subscription"),
            Some(UnsubscribeStatus::Unsubscribed)
        );
        assert_eq!(server.subscribers("thread-1"), 0);
    }

    /// Two local handles on one connection: releasing either one must not send
    /// `thread/unsubscribe` while the other still reads.
    #[tokio::test]
    async fn a_second_local_handle_keeps_the_server_subscription() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "shared thread",
        ));
        let client = connect(&url).await;

        let Adoption::Subscribed(first) = client.threads().adopt("thread-1").await.expect("adopt") else {
            panic!("must subscribe");
        };
        let Adoption::Subscribed(mut second) = client.threads().adopt("thread-1").await.expect("adopt")
        else {
            panic!("must subscribe");
        };
        assert_eq!(server.subscribers("thread-1"), 1, "one connection, two resumes");

        assert_eq!(
            first.release().await.expect("release one handle"),
            None,
            "another local handle still holds the thread"
        );
        assert_eq!(server.unsubscribe_calls(), 0);

        assert_eq!(server.push_item("thread-1", "item-1", "still streaming"), 1);
        let event = tokio::time::timeout(Duration::from_secs(2), second.next_event())
            .await
            .expect("the remaining handle still reads")
            .expect("a thread event");
        let (_, params) = notification(event);
        assert_eq!(params["item"]["text"], "still streaming");

        assert_eq!(
            second.release().await.expect("release the last handle"),
            Some(UnsubscribeStatus::Unsubscribed)
        );
        assert_eq!(server.unsubscribe_calls(), 1);
    }

    /// A subscription owns what it needs: it keeps streaming after the
    /// [`Threads`] surface (and the client) that handed it out is dropped,
    /// because the dispatcher lives with the registry, not with that surface.
    #[tokio::test]
    async fn a_subscription_outlives_the_surface_that_created_it() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let mut subscription = {
            let client = connect(&url).await;
            let Adoption::Subscribed(subscription) = client.threads().adopt("thread-1").await.expect("adopt")
            else {
                panic!("must subscribe");
            };
            subscription
        };
        server.wait_for_subscribers("thread-1", 1).await;

        assert_eq!(server.push_item("thread-1", "item-1", "still here"), 1);
        let event = tokio::time::timeout(Duration::from_secs(2), subscription.next_event())
            .await
            .expect("the stream survives the surface")
            .expect("a thread event");
        let (_, params) = notification(event);
        assert_eq!(params["item"]["text"], "still here");
    }

    /// A connection that dies ends the subscription's stream instead of
    /// leaving `next_event` waiting forever, and later reads fail rather than
    /// hanging on a response that cannot come.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_lost_connection_ends_the_subscription() {
        let directory = crate::codex::test_server::socket_tempdir();
        let socket = directory.path().join("app-server.sock");
        let server = FakeServer::unix(&socket, "codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let endpoint = Endpoint::Unix(socket);
        let client = CodexClient::connect(&CodexOptions::new(endpoint), &PinnedProbe)
            .await
            .expect("attach");
        let Adoption::Subscribed(mut subscription) = client.threads().adopt("thread-1").await.expect("adopt")
        else {
            panic!("must subscribe");
        };

        server.die().await;

        let ended = tokio::time::timeout(Duration::from_secs(2), subscription.next_event())
            .await
            .expect("the stream ends instead of hanging forever");
        assert!(ended.is_err(), "a closed connection is not an event: {ended:?}");
        assert!(
            client.threads().read("thread-1", false).await.is_err(),
            "a read after the socket died fails instead of waiting for a response"
        );
    }

    #[test]
    fn only_the_documented_no_rollout_answer_is_a_named_state() {
        let id = "019a";
        assert!(no_history(
            Some(-32600),
            id,
            "no rollout found for thread id 019a"
        ));
        // Near matches stay errors: another thread's id, the right phrase in
        // the wrong shape, another code, or no code at all.
        assert!(!no_history(
            Some(-32600),
            id,
            "no rollout found for thread id 019b"
        ));
        assert!(!no_history(
            Some(-32600),
            id,
            "prefix: no rollout found for thread id 019a"
        ));
        assert!(!no_history(
            Some(-32600),
            id,
            "no rollout found for thread id 019a and more"
        ));
        assert!(!no_history(Some(-32600), id, "no rollout found"));
        assert!(!no_history(
            Some(-32602),
            id,
            "no rollout found for thread id 019a"
        ));
        assert!(!no_history(None, id, "no rollout found for thread id 019a"));
    }

    /// `SessionListInfo` has no field for the session tree root, so a list row
    /// is selection/display only: `/attach` must take the root from
    /// `thread/read` (or the resume response), never from the listed id. This
    /// pins the decision the list API cannot express.
    #[tokio::test]
    async fn the_session_root_comes_from_read_not_the_list_row() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;

        let listed = client.threads().list_all("/work/cola").await.expect("list");
        assert_eq!(listed[0].id, "thread-1");

        let thread = client
            .threads()
            .read(&listed[0].id, false)
            .await
            .expect("the attach read");
        assert_eq!(thread.session_id, "session-root");
        assert_ne!(
            thread.session_id, thread.id,
            "the root is read from the wire, never derived from the thread id"
        );
    }

    /// A cancelled adopt releases its local holder: without RAII the phantom
    /// holder makes the next handle look non-last and the release below skips
    /// `thread/unsubscribe` (this test then fails on the call count).
    #[tokio::test]
    async fn cancelling_an_adopt_releases_the_local_holder() {
        let (server, url) = FakeServer::tcp("codex-tui/0.163.0 (test)").await;
        server.seed_thread(ScriptedThread::adopted(
            "thread-1",
            "session-root",
            "/work/cola",
            "a real thread",
        ));
        let client = connect(&url).await;
        server.silence_next_resume();

        let cancelled =
            tokio::time::timeout(Duration::from_millis(150), client.threads().adopt("thread-1")).await;
        assert!(cancelled.is_err(), "the resume is deliberately unanswered");
        server.wait_for_resume_calls(1).await;
        assert_eq!(
            client.threads().local_holders("thread-1"),
            0,
            "a cancelled adopt must not leave a phantom holder"
        );

        // A real adopt on the same thread must still be the last holder, so
        // its release reaches the server.
        let Adoption::Subscribed(subscription) = client.threads().adopt("thread-1").await.expect("adopt")
        else {
            panic!("must subscribe");
        };
        assert_eq!(
            subscription.release().await.expect("release"),
            Some(UnsubscribeStatus::Unsubscribed)
        );
        assert_eq!(server.unsubscribe_calls(), 1);
    }

    /// A dispatcher that falls behind the connection's push stream delivers an
    /// explicit lag marker instead of silently skipping events: every live
    /// subscriber learns how many were dropped and can resync.
    #[tokio::test]
    async fn a_lagging_dispatcher_tells_subscriptions_how_many_events_were_lost() {
        let registry = Arc::new(Registry::default());
        let mut events = registry.subscribe("thread-1");
        let (inbound, receiver) = broadcast::channel::<Inbound>(1);
        let dispatcher = tokio::spawn(dispatch(receiver, Arc::downgrade(&registry)));

        // Three sends with no await in between over a capacity-1 channel: the
        // dispatcher has to observe two losses before the last event.
        for text in ["dropped-1", "dropped-2", "delivered"] {
            inbound
                .send(Inbound::Notification {
                    seq: 1,
                    method: "item/completed".to_string(),
                    params: json!({
                        "threadId": "thread-1",
                        "item": { "type": "agentMessage", "id": "i", "text": text },
                    }),
                })
                .expect("the test receiver is alive");
        }

        let marker = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("the lag marker arrives")
            .expect("the subscription is open");
        assert_eq!(marker, ThreadEvent::Lagged { skipped: 2 });

        let delivered = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("the retained event arrives")
            .expect("the subscription is open");
        let (method, params) = notification(delivered);
        assert_eq!(method, "item/completed");
        assert_eq!(params["item"]["text"], "delivered");

        dispatcher.abort();
    }
}
