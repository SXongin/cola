//! Server-initiated approvals and questions (spec #860, ticket #882).
//!
//! app-server asks the *client* for decisions: a command/file-change approval,
//! an additional-permission grant, a tool's user input or an MCP elicitation.
//! They are JSON-RPC requests with an id the **server** owns, so answering one
//! is a response frame on this connection — never a client call
//! ([`RpcConnection::respond`]).
//!
//! The spike (ADR-0090's amendment, question 1) established the peer property
//! this module preserves: an approval is delivered to **every subscriber** of
//! the thread, and an answer from the non-initiating client resolves it on all
//! of them. The adapter therefore never filters by "who started the turn":
//! every connection records every approval it receives, and the first answer
//! (from either side) is the one the server acts on. `serverRequest/resolved`
//! is the server's own word that a request is gone, so the record drops it and
//! the bridge's existing stale-card sweep withdraws the Feishu card — a card
//! that stayed clickable after another client answered would be the regression
//! the ticket names.
//!
//! # The neutral projection
//!
//! The bridge never sees a Codex wire name. Each request is projected onto the
//! neutral [`PermissionRequest`] / [`QuestionRequest`] the poll-shaped
//! `Backend` already serves, exactly as #879 projected the read model:
//!
//! - `item/commandExecution/requestApproval`, `execCommandApproval` and
//!   `item/permissions/requestApproval` → a permission ask;
//! - `item/fileChange/requestApproval`, `applyPatchApproval` → a permission ask;
//! - `item/tool/requestUserInput` and a form-mode `mcpServer/elicitation/request`
//!   → a question form.
//!
//! The wire reply is where the variants live. The card offers only what cola
//! can express — `once` → `accept`, `always` → `acceptForSession` **only where
//! the method's response schema advertises it**, `reject` → `decline` — and a
//! request cola cannot express is declined explicitly rather than approximated.
//! An unknown server request is refused with a JSON-RPC error naming the
//! method, so the server never waits on a client that will not answer.
//!
//! # Which decisions are offered, and how confidently
//!
//! The session-scoped control is a declared per-method capability
//! ([`advertises_session_decision`]), read from the **response schemas** of the
//! pinned `0.163.0` surface: the two modern approvals and the legacy pair
//! advertise a session decision, while `item/permissions/requestApproval`
//! advertises a grant profile plus a `turn`/`session` scope and no
//! `acceptForSession` variant. Construction verifies **method names**, not
//! payload variants (ADR-0090's method-level note), so a server on another
//! version keeps this declared set rather than being re-probed: the typed
//! decoders below are what turn a changed payload into a named decode error.
//! A method outside the table never shows the session control, and a stale
//! `always` reply against it is declined rather than approximated.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::backend::types::{
    FormAnswer, FormFieldKind, FormValue, PermissionRequest, QuestionInfo, QuestionOption, QuestionRequest,
};
use crate::bridge::request::kind::PERMISSION_PATTERN_MAX_CHARS;
use crate::error::{BridgeError, Result};

use super::backend::Projections;
use super::error::CodexError;
use super::transport::{Inbound, RpcConnection};

/// The notification the server sends when a request is answered (by any
/// client) or otherwise leaves the pending set. It carries the same
/// `requestId` the request frame did.
const RESOLVED: &str = "serverRequest/resolved";

/// The JSON-RPC error code for a method this client does not implement.
const METHOD_NOT_FOUND: i64 = -32601;

/// One approval/question request a server is waiting on, keyed by the canonical
/// spelling of its id.
struct Pending {
    /// The exact JSON-RPC id the server chose; the response frame must echo it
    /// verbatim (a string id stays a string, a number stays a number).
    id: Value,
    method: String,
    /// The thread's working directory when the adapter knew it. The bridge
    /// lists pending requests per known directory; an unknown directory is
    /// surfaced under the first one that asks, never dropped.
    directory: Option<String>,
    /// The raw params, for the reply variants that echo them (a requested
    /// permission profile, an elicitation's answer shape).
    params: Value,
    permission: Option<PermissionRequest>,
    question: Option<QuestionRequest>,
}

impl Pending {
    fn is_permission(&self) -> bool {
        self.permission.is_some()
    }

    fn is_question(&self) -> bool {
        self.question.is_some()
    }
}

/// The approvals/questions one connection has received and not yet resolved.
///
/// Cloning is not needed: [`super::client::CodexClient`] holds one `Arc` and
/// its clones share it, exactly like the read-model cache.
#[derive(Default)]
pub(crate) struct Approvals {
    pending: Mutex<HashMap<String, Pending>>,
    /// The watcher task. Aborted when the last client holding this record is
    /// dropped, so a replaced attachment stops recording into a map nothing
    /// reads.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for Approvals {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().expect("approval tasks").drain(..) {
            task.abort();
        }
    }
}

impl Approvals {
    /// Start consuming this connection's inbound requests and resolutions. The
    /// task holds a weak reference, so the record's own `Drop` is the
    /// lifetime edge (the same shape the projections' subscribers use).
    pub(crate) fn watch(self: &Arc<Self>, connection: &RpcConnection, projections: &Arc<Projections>) {
        let mut inbound = connection.inbound();
        let approvals = Arc::downgrade(self);
        let connection = connection.clone();
        let projections = Arc::clone(projections);
        let task = tokio::spawn(async move {
            loop {
                match inbound.recv().await {
                    Ok(Inbound::Request { id, method, params }) => {
                        let Some(approvals) = approvals.upgrade() else {
                            return;
                        };
                        approvals
                            .ingest(&connection, &projections, id, method, params)
                            .await;
                    }
                    Ok(Inbound::Notification { method, params, .. }) if method == RESOLVED => {
                        let Some(approvals) = approvals.upgrade() else {
                            return;
                        };
                        approvals.resolve(&params);
                    }
                    // The socket is gone: no answer can reach the server, so
                    // every pending request must stop being listed. The
                    // watcher holds a connection clone, so the broadcast's
                    // own channel would otherwise outlive the socket.
                    Ok(Inbound::Closed) => {
                        if let Some(approvals) = approvals.upgrade() {
                            approvals.clear();
                        }
                        return;
                    }
                    // Not this consumer's traffic; the thread dispatcher and
                    // the status watcher read the same broadcast.
                    Ok(_) => continue,
                    // A lagged broadcast may have dropped a request the server
                    // is waiting on. Nothing can answer it now, so report the
                    // hole rather than pretending the set is complete.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            "codex approvals fell behind by {skipped} event(s); a server request may be unanswered"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        if let Some(approvals) = approvals.upgrade() {
                            approvals.clear();
                        }
                        return;
                    }
                }
            }
        });
        self.tasks.lock().expect("approval tasks").push(task);
    }

    /// Forget every pending request: the connection they arrived on is gone,
    /// so no answer can reach the server and the cards must not stay
    /// clickable.
    fn clear(&self) {
        self.pending.lock().expect("pending approvals").clear();
    }

    /// Drop a request the server resolved (another client answered it, or the
    /// turn ended). The bridge's next sweep sees it absent and withdraws the
    /// card through the existing stale-card path.
    fn resolve(&self, params: &Value) {
        let Some(id) = params.get("requestId") else {
            return;
        };
        let Some(key) = request_key(id) else {
            return;
        };
        if self
            .pending
            .lock()
            .expect("pending approvals")
            .remove(&key)
            .is_some()
        {
            tracing::info!("codex server request {key} resolved elsewhere");
        }
    }

    async fn ingest(
        &self,
        connection: &RpcConnection,
        projections: &Arc<Projections>,
        id: Value,
        method: String,
        params: Value,
    ) {
        let Some(key) = request_key(&id) else {
            tracing::warn!("codex server request {method} carries no routable id");
            return;
        };

        let mut decoded = match decode_request(&method, &params, projections) {
            Ok(request) => request,
            Err(Decode::Unsupported) => {
                tracing::warn!(
                    "codex server request `{method}` is not one cola answers; refusing it explicitly"
                );
                if let Err(error) = connection
                    .respond_error(&id, METHOD_NOT_FOUND, &format!("cola does not answer `{method}`"))
                    .await
                {
                    tracing::warn!("codex server request `{method}` refusal failed: {error}");
                }
                return;
            }
            Err(Decode::CannotExpress(why)) => {
                tracing::warn!("codex server request `{method}` cannot be expressed: {why}; declining it");
                if let Err(error) = decline(&method, connection, &id).await {
                    tracing::warn!("codex server request `{method}` explicit decline failed: {error}");
                }
                return;
            }
            Err(Decode::Malformed(detail)) => {
                tracing::warn!("codex server request `{method}` is malformed: {detail}; refusing it");
                if let Err(error) = connection
                    .respond_error(&id, -32602, &format!("cola cannot decode `{method}`: {detail}"))
                    .await
                {
                    tracing::warn!("codex server request `{method}` refusal failed: {error}");
                }
                return;
            }
        };

        // The neutral id is the canonical key: it is what the card carries
        // back, and what `serverRequest/resolved` is matched against.
        if let Some(permission) = &mut decoded.permission {
            permission.request_id = key.clone();
        }
        if let Some(question) = &mut decoded.question {
            question.id = key.clone();
        }

        let pending = Pending {
            id,
            method,
            directory: decoded.directory,
            params,
            permission: decoded.permission,
            question: decoded.question,
        };
        if let Some(previous) = self
            .pending
            .lock()
            .expect("pending approvals")
            .insert(key.clone(), pending)
        {
            tracing::warn!(
                "codex server request {key} replaced an unanswered {} request",
                previous.method
            );
        }
    }

    /// The pending permission asks visible under `directory`. A request whose
    /// thread directory is known belongs to exactly that directory; an unknown
    /// one is visible everywhere (the sweep surfaces it under the first
    /// directory that asks, and `seen` keeps it single).
    pub(crate) fn permissions(&self, directory: Option<&str>) -> Vec<PermissionRequest> {
        self.pending
            .lock()
            .expect("pending approvals")
            .values()
            .filter(|pending| {
                pending.is_permission() && directory_matches(pending.directory.as_deref(), directory)
            })
            .filter_map(|pending| pending.permission.clone())
            .collect()
    }

    /// The pending question forms visible under `directory`, with the same
    /// directory rule as [`Self::permissions`].
    pub(crate) fn questions(&self, directory: Option<&str>) -> Vec<QuestionRequest> {
        self.pending
            .lock()
            .expect("pending approvals")
            .values()
            .filter(|pending| {
                pending.is_question() && directory_matches(pending.directory.as_deref(), directory)
            })
            .filter_map(|pending| pending.question.clone())
            .collect()
    }

    /// Answer one permission ask with the card's neutral reply. The request is
    /// removed before the frame is written, so a losing second click (or a
    /// racing sweep) can never produce a second JSON-RPC reply; the flow's own
    /// claim guard is the first line and this is the last.
    pub(crate) async fn reply_permission(
        &self,
        connection: &RpcConnection,
        request_id: &str,
        reply: &str,
    ) -> Result<()> {
        let pending = self.take_permission(request_id)?;
        let result = permission_result(&pending, reply)?;
        connection
            .respond(&pending.id, result)
            .await
            .map_err(bridge_error)
    }

    /// Answer one question form with its keyed answers.
    pub(crate) async fn reply_question(
        &self,
        connection: &RpcConnection,
        request_id: &str,
        answers: &[FormAnswer],
    ) -> Result<()> {
        let pending = self.take_question(request_id)?;
        let result = question_result(&pending, answers)?;
        connection
            .respond(&pending.id, result)
            .await
            .map_err(bridge_error)
    }

    /// Cancel a question form. A tool's user-input request has no cancel
    /// variant, so it is answered with no answers (the tool continues);
    /// an elicitation carries the protocol's own `decline`.
    pub(crate) async fn reject_question(&self, connection: &RpcConnection, request_id: &str) -> Result<()> {
        let pending = self.take_question(request_id)?;
        let result = match pending.method.as_str() {
            "mcpServer/elicitation/request" => json!({ "action": "decline" }),
            _ => json!({ "answers": {} }),
        };
        connection
            .respond(&pending.id, result)
            .await
            .map_err(bridge_error)
    }

    fn take_permission(&self, request_id: &str) -> Result<Pending> {
        let mut pending = self.pending.lock().expect("pending approvals");
        let Some(request) = pending.get(request_id) else {
            return Err(BridgeError::NotFound(format!(
                "codex approval {request_id} is no longer pending"
            )));
        };
        if !request.is_permission() {
            return Err(BridgeError::Client {
                client: "codex",
                detail: format!("{request_id} is a question request, not a permission"),
            });
        }
        Ok(pending.remove(request_id).expect("pending request"))
    }

    fn take_question(&self, request_id: &str) -> Result<Pending> {
        let mut pending = self.pending.lock().expect("pending approvals");
        let Some(request) = pending.get(request_id) else {
            return Err(BridgeError::NotFound(format!(
                "codex question {request_id} is no longer pending"
            )));
        };
        if !request.is_question() {
            return Err(BridgeError::Client {
                client: "codex",
                detail: format!("{request_id} is a permission request, not a question"),
            });
        }
        Ok(pending.remove(request_id).expect("pending request"))
    }
}

/// The canonical, wire-type-preserving key for a JSON-RPC request id. A string
/// id and a number id are distinct request ids (the protocol's `RequestId` is
/// either), so the encoding keeps the type; `serverRequest/resolved` uses the
/// same function on the same value.
fn request_key(id: &Value) -> Option<String> {
    match id {
        Value::String(text) => Some(format!("s:{text}")),
        Value::Number(number) => Some(format!("n:{number}")),
        _ => None,
    }
}

fn directory_matches(owner: Option<&str>, asked: Option<&str>) -> bool {
    match (owner, asked) {
        (Some(owner), Some(asked)) => owner == asked,
        (Some(_), None) | (None, _) => true,
    }
}

fn bridge_error(error: CodexError) -> BridgeError {
    BridgeError::Client {
        client: "codex",
        detail: error.to_string(),
    }
}

/// Why a server request did not become a card.
#[derive(Debug)]
enum Decode {
    /// This adapter has no handler for the method.
    Unsupported,
    /// The request is one cola knows, but this instance's answer cannot be
    /// expressed in cola's neutral model (a URL elicitation, a schema with a
    /// nested object). It is declined explicitly.
    CannotExpress(&'static str),
    /// The payload does not match the pinned schema.
    Malformed(String),
}

struct Decoded {
    directory: Option<String>,
    permission: Option<PermissionRequest>,
    question: Option<QuestionRequest>,
}

fn decode_request(
    method: &str,
    params: &Value,
    projections: &Projections,
) -> std::result::Result<Decoded, Decode> {
    match method {
        "item/commandExecution/requestApproval" => {
            let approval: CommandApproval = decode(method, params)?;
            Ok(Decoded {
                directory: approval.cwd.clone(),
                permission: Some(permission_from_command(&approval)),
                question: None,
            })
        }
        "execCommandApproval" => {
            let approval: ExecCommandApproval = decode(method, params)?;
            Ok(Decoded {
                directory: Some(approval.cwd.clone()),
                permission: Some(permission_from_exec(&approval)),
                question: None,
            })
        }
        "item/fileChange/requestApproval" => {
            let approval: FileChangeApproval = decode(method, params)?;
            let directory = projections.cwd(&approval.thread_id);
            Ok(Decoded {
                directory,
                permission: Some(permission_from_file_change(&approval)),
                question: None,
            })
        }
        "applyPatchApproval" => {
            let approval: PatchApproval = decode(method, params)?;
            let directory = projections.cwd(&approval.conversation_id);
            Ok(Decoded {
                directory,
                permission: Some(permission_from_patch(&approval)),
                question: None,
            })
        }
        "item/permissions/requestApproval" => {
            let approval: PermissionsApproval = decode(method, params)?;
            // Approving echoes the profile; a field the card cannot show must
            // not be granted behind the user's back.
            expressible_permission_profile(&approval.permissions)?;
            Ok(Decoded {
                directory: Some(approval.cwd.clone()),
                permission: Some(permission_from_permissions(&approval)),
                question: None,
            })
        }
        "item/tool/requestUserInput" => {
            let input: ToolInput = decode(method, params)?;
            let directory = projections.cwd(&input.thread_id);
            Ok(Decoded {
                directory,
                permission: None,
                question: Some(question_from_tool_input(&input)?),
            })
        }
        "mcpServer/elicitation/request" => {
            let elicitation: Elicitation = decode(method, params)?;
            let question = question_from_elicitation(&elicitation)
                .ok_or(Decode::CannotExpress("only form-mode elicitations have a card"))?;
            let directory = projections.cwd(&elicitation.thread_id);
            Ok(Decoded {
                directory,
                permission: None,
                question: Some(question),
            })
        }
        _ => Err(Decode::Unsupported),
    }
}

fn decode<T: serde::de::DeserializeOwned>(method: &str, params: &Value) -> std::result::Result<T, Decode> {
    serde_json::from_value(params.clone()).map_err(|error| Decode::Malformed(format!("{method}: {error}")))
}

/// Answer a request cola cannot express with the protocol's own refusal for
/// its kind. There is no generic "no"; each request family has a shape that
/// means "nothing granted / not now".
async fn decline(method: &str, connection: &RpcConnection, id: &Value) -> super::error::Result<()> {
    let result = match method {
        "item/permissions/requestApproval" => json!({ "permissions": {} }),
        "mcpServer/elicitation/request" => json!({ "action": "decline" }),
        "item/tool/requestUserInput" => json!({ "answers": {} }),
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({ "decision": "decline" })
        }
        "execCommandApproval" | "applyPatchApproval" => {
            json!({ "decision": { "denied": { "rejection": "Rejected by user" } } })
        }
        _ => {
            return connection
                .respond_error(id, METHOD_NOT_FOUND, &format!("cola cannot answer `{method}`"))
                .await;
        }
    };
    connection.respond(id, result).await
}

// ---------------------------------------------------------------------------
// Wire params → neutral DTOs
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandApproval {
    thread_id: String,
    item_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    approval_id: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    started_at_ms: Option<i64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileChangeApproval {
    thread_id: String,
    item_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    grant_root: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    started_at_ms: Option<i64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionsApproval {
    thread_id: String,
    item_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    cwd: String,
    permissions: Value,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    started_at_ms: Option<i64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecCommandApproval {
    conversation_id: String,
    call_id: String,
    #[serde(default)]
    approval_id: Option<String>,
    command: Vec<String>,
    cwd: String,
    #[serde(default)]
    reason: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchApproval {
    conversation_id: String,
    call_id: String,
    #[serde(default)]
    file_changes: HashMap<String, Value>,
    #[serde(default)]
    grant_root: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolInput {
    thread_id: String,
    item_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    is_blocking: bool,
    questions: Vec<ToolQuestion>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolQuestion {
    id: String,
    #[serde(default)]
    header: String,
    #[serde(default)]
    question: String,
    #[serde(default)]
    options: Option<Vec<ToolOption>>,
    #[serde(default)]
    is_other: bool,
    #[serde(default)]
    is_secret: bool,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ToolOption {
    label: String,
    #[serde(default)]
    description: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Elicitation {
    server_name: String,
    thread_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    message: String,
    mode: String,
    #[serde(default)]
    requested_schema: Option<Value>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    elicitation_id: Option<String>,
}

fn metadata(fields: &[(&str, Option<String>)]) -> Option<Value> {
    let mut object = Map::new();
    for (key, value) in fields {
        if let Some(value) = value.as_ref().filter(|value| !value.trim().is_empty()) {
            object.insert((*key).to_string(), Value::String(value.clone()));
        }
    }
    (!object.is_empty()).then_some(Value::Object(object))
}

fn permission(
    id: &str,
    thread_id: &str,
    action: &str,
    patterns: Vec<String>,
    metadata: Option<Value>,
    always_supported: bool,
) -> PermissionRequest {
    PermissionRequest {
        request_id: id.to_string(),
        session_id: Some(thread_id.to_string()),
        permission: Some(action.to_string()),
        patterns,
        metadata,
        always: Vec::new(),
        always_supported,
    }
}

/// Which approval methods advertise a session-scoped decision in their
/// response schema. See the module doc: this is a declared, method-level set
/// (the same granularity the construction probe checks), never inferred from
/// the request payload. `item/permissions/requestApproval` is absent: its
/// response is a granted profile with a `turn`/`session` scope, not an
/// `acceptForSession` decision.
fn advertises_session_decision(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "execCommandApproval"
            | "applyPatchApproval"
    )
}

fn permission_from_command(approval: &CommandApproval) -> PermissionRequest {
    // `codex` sends the command as one string; an empty one is still a request
    // (an approval for a different environment), so the card must render.
    let patterns = approval
        .command
        .iter()
        .filter(|command| !command.trim().is_empty())
        .cloned()
        .collect();
    permission(
        "",
        &approval.thread_id,
        "command",
        patterns,
        metadata(&[
            ("reason", approval.reason.clone()),
            ("cwd", approval.cwd.clone()),
            ("item_id", Some(approval.item_id.clone())),
            ("turn_id", approval.turn_id.clone()),
            ("approval_id", approval.approval_id.clone()),
        ]),
        advertises_session_decision("item/commandExecution/requestApproval"),
    )
}

fn permission_from_file_change(approval: &FileChangeApproval) -> PermissionRequest {
    let patterns = approval.grant_root.iter().cloned().collect();
    permission(
        "",
        &approval.thread_id,
        "edit",
        patterns,
        metadata(&[
            ("reason", approval.reason.clone()),
            ("item_id", Some(approval.item_id.clone())),
            ("turn_id", approval.turn_id.clone()),
            ("grant_root", approval.grant_root.clone()),
        ]),
        advertises_session_decision("item/fileChange/requestApproval"),
    )
}

fn permission_from_permissions(approval: &PermissionsApproval) -> PermissionRequest {
    permission(
        "",
        &approval.thread_id,
        "permissions",
        permission_patterns(&approval.permissions),
        metadata(&[
            ("reason", approval.reason.clone()),
            ("cwd", Some(approval.cwd.clone())),
            ("item_id", Some(approval.item_id.clone())),
            ("turn_id", approval.turn_id.clone()),
        ]),
        // The response shape is a grant (`scope: turn|session`), not the
        // session decision the command/file-change schemas advertise (see
        // `advertises_session_decision`). A session-wide arbitrary profile is
        // the one grant cola cannot express faithfully, so the card offers
        // once/deny only.
        advertises_session_decision("item/permissions/requestApproval"),
    )
}

fn permission_from_exec(approval: &ExecCommandApproval) -> PermissionRequest {
    let command = approval.command.join(" ");
    permission(
        "",
        &approval.conversation_id,
        "command",
        (!command.trim().is_empty())
            .then_some(command)
            .into_iter()
            .collect(),
        metadata(&[
            ("reason", approval.reason.clone()),
            ("cwd", Some(approval.cwd.clone())),
            ("item_id", Some(approval.call_id.clone())),
            ("approval_id", approval.approval_id.clone()),
        ]),
        advertises_session_decision("execCommandApproval"),
    )
}

fn permission_from_patch(approval: &PatchApproval) -> PermissionRequest {
    let mut files: Vec<String> = approval.file_changes.keys().cloned().collect();
    files.sort();
    let filepath = files.first().cloned();
    permission(
        "",
        &approval.conversation_id,
        "patch",
        files,
        metadata(&[
            ("reason", approval.reason.clone()),
            ("filepath", filepath),
            ("grant_root", approval.grant_root.clone()),
            ("item_id", Some(approval.call_id.clone())),
        ]),
        advertises_session_decision("applyPatchApproval"),
    )
}

/// The one reason a requested grant is refused: the card can only show the
/// filesystem arrays/entries and the network switch, so any other field would
/// be granted without being visible. The names are deliberately about the card,
/// not the client's schema.
const PROFILE_UNEXPRESSIBLE: &str = "the requested permission profile has a grant the card cannot show";

/// A requested path/glob whose rendered form would be truncated by the card.
const PROFILE_TOO_LONG: &str = "a requested permission path is longer than the card can show";

/// Whether the card renders this pattern in full (the renderer clips at
/// [`PERMISSION_PATTERN_MAX_CHARS`] characters).
fn pattern_fits(rendered: &str) -> bool {
    rendered.chars().count() <= PERMISSION_PATTERN_MAX_CHARS
}

/// Whether the card can render every part of a requested permission profile.
/// `permission_patterns` is the renderer; this is its completeness proof, so an
/// `once` grant can echo the profile without hiding a field.
fn expressible_permission_profile(profile: &Value) -> std::result::Result<(), Decode> {
    let Some(object) = profile.as_object() else {
        return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
    };
    for (key, value) in object {
        match key.as_str() {
            "fileSystem" => expressible_file_system(value)?,
            "network" => expressible_network(value)?,
            _ if value.is_null() => {}
            _ => return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE)),
        }
    }
    Ok(())
}

fn expressible_file_system(value: &Value) -> std::result::Result<(), Decode> {
    if value.is_null() {
        return Ok(());
    }
    let Some(file_system) = value.as_object() else {
        return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
    };
    for (key, value) in file_system {
        match key.as_str() {
            "read" => expressible_path_array(value, "read")?,
            "write" => expressible_path_array(value, "write")?,
            "entries" => expressible_entries(value)?,
            _ if value.is_null() => {}
            _ => return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE)),
        }
    }
    Ok(())
}

fn expressible_path_array(value: &Value, prefix: &str) -> std::result::Result<(), Decode> {
    if value.is_null() {
        return Ok(());
    }
    let Some(paths) = value.as_array() else {
        return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
    };
    for path in paths {
        let Some(path) = path.as_str() else {
            return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
        };
        if !pattern_fits(&format!("{prefix}: {path}")) {
            return Err(Decode::CannotExpress(PROFILE_TOO_LONG));
        }
    }
    Ok(())
}

fn expressible_entries(value: &Value) -> std::result::Result<(), Decode> {
    if value.is_null() {
        return Ok(());
    }
    let Some(entries) = value.as_array() else {
        return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
    };
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
        };
        if entry.keys().any(|key| !matches!(key.as_str(), "access" | "path")) {
            return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
        }
        let Some(access) = entry.get("access").and_then(Value::as_str) else {
            return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
        };
        let Some(path) = entry.get("path").and_then(Value::as_object) else {
            return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
        };
        // `path` and `glob_pattern` are the two shapes the card names in full;
        // a `special` root (`project_roots` + subpath, `slash_tmp`, …) has
        // semantics the one-line pattern loses.
        let (allowed, value_key): (&[&str], &str) = match path.get("type").and_then(Value::as_str) {
            Some("path") => (&["type", "path"], "path"),
            Some("glob_pattern") => (&["type", "pattern"], "pattern"),
            _ => return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE)),
        };
        if path.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
        }
        let Some(path_value) = path.get(value_key).and_then(Value::as_str) else {
            return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
        };
        if !pattern_fits(&format!("{access}: {path_value}")) {
            return Err(Decode::CannotExpress(PROFILE_TOO_LONG));
        }
    }
    Ok(())
}

fn expressible_network(value: &Value) -> std::result::Result<(), Decode> {
    if value.is_null() {
        return Ok(());
    }
    let Some(network) = value.as_object() else {
        return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE));
    };
    for (key, value) in network {
        match key.as_str() {
            "enabled" if value.is_null() || value.is_boolean() => {}
            _ if value.is_null() => {}
            _ => return Err(Decode::CannotExpress(PROFILE_UNEXPRESSIBLE)),
        }
    }
    Ok(())
}

/// The human-read patterns of a requested permission profile: write/read paths
/// (the current `entries` shape and the legacy arrays) plus network access.
fn permission_patterns(profile: &Value) -> Vec<String> {
    let mut patterns = Vec::new();
    let Some(file_system) = profile.get("fileSystem") else {
        if profile
            .get("network")
            .and_then(|network| network.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            patterns.push("网络访问".to_string());
        }
        return patterns;
    };
    for key in ["write", "read"] {
        for path in file_system
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            patterns.push(format!("{key}: {path}"));
        }
    }
    for entry in file_system
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let access = entry.get("access").and_then(Value::as_str).unwrap_or("?");
        let path = entry
            .get("path")
            .and_then(|path| path.get("path").or_else(|| path.get("pattern")))
            .and_then(Value::as_str)
            .or_else(|| {
                entry
                    .get("path")
                    .and_then(|path| path.get("value"))
                    .and_then(|value| value.get("kind"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("?");
        patterns.push(format!("{access}: {path}"));
    }
    if profile
        .get("network")
        .and_then(|network| network.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        patterns.push("网络访问".to_string());
    }
    patterns
}

fn question_from_tool_input(input: &ToolInput) -> std::result::Result<QuestionRequest, Decode> {
    // A secret answer would be visible on a shared card, in the answered-state
    // markdown and in the Interaction Receipt. Do not collect it: project the
    // request as an unanswerable refusal form, whose only control is the
    // ordinary reject button (and whose reply is the protocol's empty
    // `answers`, the decline).
    if input.questions.iter().any(|question| question.is_secret) {
        return Ok(QuestionRequest::uncollectable(&input.thread_id));
    }
    let questions = input
        .questions
        .iter()
        .map(|question| {
            let options: Vec<QuestionOption> = question
                .options
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|option| QuestionOption {
                    value: option.label.clone(),
                    label: option.label.clone(),
                    description: option.description.clone(),
                })
                .collect();
            QuestionInfo {
                key: question.id.clone(),
                question: question.question.clone(),
                header: if question.header.is_empty() {
                    question.id.clone()
                } else {
                    question.header.clone()
                },
                kind: FormFieldKind::String,
                options,
                // A question without options is free text; `isOther` is the
                // protocol's own "the user may type something else".
                custom: Some(question.is_other || question.options.as_deref().is_none_or(|o| o.is_empty())),
                required: false,
                url: None,
            }
        })
        .collect();
    Ok(QuestionRequest {
        id: String::new(),
        session_id: input.thread_id.clone(),
        // The tool carries no form title; the card's default header is right.
        title: String::new(),
        questions,
        uncollectable: None,
    })
}

/// A form-mode elicitation projected onto the neutral typed form. `None` when
/// the mode is `url` (a browser interaction cola cannot deliver) or a field's
/// schema cannot be expressed (a nested object): the caller declines rather
/// than inventing a control.
fn question_from_elicitation(elicitation: &Elicitation) -> Option<QuestionRequest> {
    if elicitation.mode == "url" {
        let _ = (&elicitation.url, &elicitation.elicitation_id);
        return None;
    }
    let schema = elicitation.requested_schema.as_ref()?;
    let properties = schema.get("properties")?.as_object()?;
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();

    let mut questions = Vec::with_capacity(properties.len());
    for (name, field) in properties {
        questions.push(elicitation_field(
            name,
            field,
            required.iter().any(|key| *key == name),
        )?);
    }
    Some(QuestionRequest {
        id: String::new(),
        session_id: elicitation.thread_id.clone(),
        // The elicitation's message is its human explanation; with sparse
        // field schemas it is the only text that says what is being asked.
        // The server name is the fallback when the message is empty.
        title: if elicitation.message.trim().is_empty() {
            elicitation.server_name.clone()
        } else {
            elicitation.message.clone()
        },
        questions,
        uncollectable: None,
    })
}

fn elicitation_field(name: &str, field: &Value, required: bool) -> Option<QuestionInfo> {
    let kind_name = field.get("type").and_then(Value::as_str)?;
    let description = field
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = field.get("title").and_then(Value::as_str).unwrap_or(name);
    let (kind, options, custom) = match kind_name {
        "boolean" => (FormFieldKind::Boolean, Vec::new(), false),
        "number" => (FormFieldKind::Number, Vec::new(), false),
        "integer" => (FormFieldKind::Integer, Vec::new(), false),
        "string" => {
            let options = enum_options(field);
            let custom = options.is_empty();
            (FormFieldKind::String, options, custom)
        }
        "array" => {
            let items = field.get("items")?;
            let options = enum_options(items);
            if options.is_empty() {
                return None;
            }
            (FormFieldKind::Multiselect, options, false)
        }
        _ => return None,
    };
    let question = if field.get("description").is_some() {
        description.to_string()
    } else {
        title.to_string()
    };
    Some(QuestionInfo {
        key: name.to_string(),
        question,
        header: title.to_string(),
        kind,
        options,
        custom: Some(custom),
        required,
        url: None,
    })
}

fn enum_options(field: &Value) -> Vec<QuestionOption> {
    if let Some(values) = field.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| QuestionOption {
                value: value.to_string(),
                label: value.to_string(),
                description: String::new(),
            })
            .collect();
    }
    field
        .get("oneOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| {
            let value = option.get("const").and_then(Value::as_str)?;
            Some(QuestionOption {
                value: value.to_string(),
                label: option
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(value)
                    .to_string(),
                description: String::new(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Neutral reply → wire result
// ---------------------------------------------------------------------------

fn permission_result(pending: &Pending, reply: &str) -> Result<Value> {
    let always_offered = pending
        .permission
        .as_ref()
        .is_some_and(|permission| permission.always_supported);
    match pending.method.as_str() {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let decision = match reply {
                "once" | "autoaccept" => "accept",
                "always" if always_offered => "acceptForSession",
                // The variant the button did not carry: decline, loudly. Never
                // an approximate `accept` — the user asked for something the
                // card did not offer.
                "always" => {
                    tracing::warn!(
                        "codex {} did not advertise acceptForSession; declining instead of approximating",
                        pending.method
                    );
                    "decline"
                }
                "reject" => "decline",
                other => {
                    return Err(BridgeError::Client {
                        client: "codex",
                        detail: format!("unknown permission reply `{other}`"),
                    });
                }
            };
            Ok(json!({ "decision": decision }))
        }
        "execCommandApproval" | "applyPatchApproval" => {
            let decision = match reply {
                "once" | "autoaccept" => json!("approved"),
                "always" if always_offered => json!("approved_for_session"),
                "always" => {
                    tracing::warn!(
                        "codex {} did not advertise a session approval; declining instead",
                        pending.method
                    );
                    json!({ "denied": { "rejection": "Rejected by user" } })
                }
                "reject" => json!({ "denied": { "rejection": "Rejected by user" } }),
                other => {
                    return Err(BridgeError::Client {
                        client: "codex",
                        detail: format!("unknown permission reply `{other}`"),
                    });
                }
            };
            Ok(json!({ "decision": decision }))
        }
        "item/permissions/requestApproval" => {
            // A grant echoes exactly the profile the server asked for; a
            // decline grants nothing. Never a silently empty accept.
            let requested = pending
                .params
                .get("permissions")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let (permissions, scope) = match reply {
                "once" | "autoaccept" => (requested, "turn"),
                "always" if always_offered => (requested, "session"),
                "always" => {
                    tracing::warn!(
                        "codex item/permissions/requestApproval did not advertise a session grant; declining instead"
                    );
                    (json!({}), "turn")
                }
                "reject" => (json!({}), "turn"),
                other => {
                    return Err(BridgeError::Client {
                        client: "codex",
                        detail: format!("unknown permission reply `{other}`"),
                    });
                }
            };
            Ok(json!({ "permissions": permissions, "scope": scope }))
        }
        other => Err(BridgeError::Client {
            client: "codex",
            detail: format!("`{other}` is not an approval this adapter answers"),
        }),
    }
}

fn question_result(pending: &Pending, answers: &[FormAnswer]) -> Result<Value> {
    match pending.method.as_str() {
        "item/tool/requestUserInput" => {
            let mut answer_map = Map::new();
            for answer in answers {
                let Some(value) = &answer.value else {
                    continue;
                };
                answer_map.insert(answer.key.clone(), json!({ "answers": value.to_strings() }));
            }
            Ok(json!({ "answers": answer_map }))
        }
        "mcpServer/elicitation/request" => {
            let mut content = Map::new();
            for answer in answers {
                let Some(value) = &answer.value else {
                    continue;
                };
                content.insert(answer.key.clone(), form_value_json(value));
            }
            Ok(json!({ "action": "accept", "content": content }))
        }
        other => Err(BridgeError::Client {
            client: "codex",
            detail: format!("`{other}` is not a question this adapter answers"),
        }),
    }
}

fn form_value_json(value: &FormValue) -> Value {
    match value {
        FormValue::Text(text) => Value::String(text.clone()),
        FormValue::Number(number) => serde_json::Number::from_f64(*number)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        FormValue::Integer(integer) => json!(integer),
        FormValue::Bool(flag) => json!(flag),
        FormValue::List(values) => json!(values),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(method: &str, permissions: PermissionRequest, params: Value) -> Pending {
        Pending {
            id: json!(1),
            method: method.to_string(),
            directory: None,
            params,
            permission: Some(permissions),
            question: None,
        }
    }

    fn command() -> PermissionRequest {
        permission(
            "n:1",
            "thread-1",
            "command",
            vec!["rm -rf build".into()],
            None,
            true,
        )
    }

    #[test]
    fn request_ids_keep_their_wire_type() {
        assert_eq!(request_key(&json!(7)), Some("n:7".into()));
        assert_eq!(request_key(&json!("7")), Some("s:7".into()));
        assert_ne!(request_key(&json!(7)), request_key(&json!("7")));
        assert_eq!(request_key(&Value::Null), None);
    }

    #[test]
    fn a_command_approval_maps_onto_the_neutral_permission() {
        let params = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "command": "rm -rf build",
            "cwd": "/work/cola",
            "reason": "needs network",
            "startedAtMs": 1_700_000_000_000_i64,
        });
        let Decoded {
            permission,
            directory,
            ..
        } = decode_request(
            "item/commandExecution/requestApproval",
            &params,
            &Projections::default(),
        )
        .expect("decodes");
        let permission = permission.expect("a permission");
        assert_eq!(permission.session_id.as_deref(), Some("thread-1"));
        assert_eq!(permission.permission.as_deref(), Some("command"));
        assert_eq!(permission.patterns, vec!["rm -rf build"]);
        assert!(permission.always_supported);
        assert_eq!(directory.as_deref(), Some("/work/cola"));
        assert_eq!(
            permission
                .metadata
                .as_ref()
                .and_then(|m| m.get("reason"))
                .and_then(Value::as_str),
            Some("needs network")
        );
    }

    #[test]
    fn a_permission_grant_is_not_a_session_decision() {
        let params = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "cwd": "/work/cola",
            "permissions": {
                "fileSystem": { "write": ["/work/cola"] },
                "network": { "enabled": true }
            }
        });
        let Decoded { permission, .. } = decode_request(
            "item/permissions/requestApproval",
            &params,
            &Projections::default(),
        )
        .expect("decodes");
        let permission = permission.expect("a permission");
        assert!(!permission.always_supported);
        assert!(
            permission
                .patterns
                .iter()
                .any(|pattern| pattern.contains("/work/cola"))
        );
        assert!(permission.patterns.iter().any(|pattern| pattern == "网络访问"));
    }

    #[test]
    fn accepts_map_to_the_advertised_decision_variants() {
        let request = command();
        let once = permission_result(
            &pending(
                "item/commandExecution/requestApproval",
                request.clone(),
                json!({}),
            ),
            "once",
        )
        .expect("once");
        assert_eq!(once, json!({ "decision": "accept" }));

        let always = permission_result(
            &pending("item/commandExecution/requestApproval", request, json!({})),
            "always",
        )
        .expect("always");
        assert_eq!(always, json!({ "decision": "acceptForSession" }));

        let decline = permission_result(
            &pending("item/fileChange/requestApproval", command(), json!({})),
            "reject",
        )
        .expect("reject");
        assert_eq!(decline, json!({ "decision": "decline" }));
    }

    #[test]
    fn a_session_variant_the_request_did_not_advertise_is_declined() {
        let mut request = command();
        request.always_supported = false;
        let result = permission_result(
            &pending("item/commandExecution/requestApproval", request, json!({})),
            "always",
        )
        .expect("a reply");
        assert_eq!(result, json!({ "decision": "decline" }));
    }

    #[test]
    fn the_legacy_decisions_use_their_own_vocabulary() {
        let legacy = permission("n:1", "thread-1", "command", vec![], None, true);
        let once = permission_result(&pending("execCommandApproval", legacy.clone(), json!({})), "once")
            .expect("once");
        assert_eq!(once, json!({ "decision": "approved" }));
        let always = permission_result(
            &pending("execCommandApproval", legacy.clone(), json!({})),
            "always",
        )
        .expect("always");
        assert_eq!(always, json!({ "decision": "approved_for_session" }));
        let reject =
            permission_result(&pending("applyPatchApproval", legacy, json!({})), "reject").expect("reject");
        assert_eq!(
            reject,
            json!({ "decision": { "denied": { "rejection": "Rejected by user" } } })
        );
    }

    #[test]
    fn a_permission_grant_echoes_the_requested_profile_or_grants_nothing() {
        let requested = json!({
            "fileSystem": { "write": ["/work/cola"] },
            "network": { "enabled": true }
        });
        let grant = permission("n:1", "thread-1", "permissions", vec![], None, false);
        let accepted = permission_result(
            &pending(
                "item/permissions/requestApproval",
                grant.clone(),
                json!({ "permissions": requested.clone() }),
            ),
            "once",
        )
        .expect("accept");
        assert_eq!(accepted, json!({ "permissions": requested, "scope": "turn" }));

        let denied = permission_result(
            &pending(
                "item/permissions/requestApproval",
                grant,
                json!({ "permissions": requested }),
            ),
            "reject",
        )
        .expect("decline");
        assert_eq!(denied, json!({ "permissions": {}, "scope": "turn" }));
    }

    #[test]
    fn a_user_input_answer_is_keyed_by_the_question_id() {
        let question = QuestionRequest {
            id: "n:1".into(),
            session_id: "thread-1".into(),
            title: String::new(),
            questions: vec![QuestionInfo {
                key: "q1".into(),
                ..Default::default()
            }],
            uncollectable: None,
        };
        let pending = Pending {
            id: json!(1),
            method: "item/tool/requestUserInput".into(),
            directory: None,
            params: json!({}),
            permission: None,
            question: Some(question),
        };
        let answers = [FormAnswer {
            key: "q1".into(),
            value: Some(FormValue::Text("Yes".into())),
        }];
        assert_eq!(
            question_result(&pending, &answers).expect("answer"),
            json!({ "answers": { "q1": { "answers": ["Yes"] } } })
        );
    }

    #[test]
    fn an_elicitation_form_is_projected_onto_typed_fields() {
        let params = json!({
            "serverName": "files",
            "threadId": "thread-1",
            "message": "Pick a folder",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "folder": { "type": "string", "enum": ["/a", "/b"] },
                    "confirm": { "type": "boolean", "title": "Confirm" },
                    "note": { "type": "string", "description": "anything" }
                },
                "required": ["folder"]
            }
        });
        let Decoded { question, .. } =
            decode_request("mcpServer/elicitation/request", &params, &Projections::default())
                .expect("decodes");
        let question = question.expect("a form");
        assert_eq!(question.title, "Pick a folder");
        assert_eq!(question.questions.len(), 3);
        let folder = question.questions.iter().find(|q| q.key == "folder").unwrap();
        assert_eq!(folder.kind, FormFieldKind::String);
        assert_eq!(folder.options.len(), 2);
        assert_eq!(folder.custom, Some(false));
        assert!(folder.required);
        let confirm = question.questions.iter().find(|q| q.key == "confirm").unwrap();
        assert_eq!(confirm.kind, FormFieldKind::Boolean);
    }

    #[test]
    fn a_url_elicitation_cannot_be_expressed() {
        let params = json!({
            "serverName": "files",
            "threadId": "thread-1",
            "message": "Open this",
            "mode": "url",
            "url": "https://example.test/consent",
            "elicitationId": "e-1"
        });
        assert!(matches!(
            decode_request("mcpServer/elicitation/request", &params, &Projections::default()),
            Err(Decode::CannotExpress(_))
        ));
    }

    #[test]
    fn a_secret_question_becomes_a_refusal_form_not_a_text_field() {
        let params = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "isBlocking": true,
            "questions": [{
                "id": "password",
                "header": "登录",
                "question": "密码？",
                "isSecret": true
            }]
        });
        let Decoded { question, .. } =
            decode_request("item/tool/requestUserInput", &params, &Projections::default()).expect("decodes");
        let question = question.expect("a refusal form");
        assert_eq!(
            question.uncollectable,
            Some(crate::backend::types::UncollectableReason::SecretInput),
            "the neutral shape carries the reason, not the card copy"
        );
        assert!(
            question.title.is_empty(),
            "the user-facing copy is not in the neutral DTO"
        );
        assert!(
            question.questions.is_empty(),
            "no secret field is rendered, not even a free-text one"
        );
    }

    #[test]
    fn a_profile_with_a_grant_the_card_cannot_show_is_refused() {
        let hidden = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "cwd": "/work/cola",
            "permissions": {
                "fileSystem": {
                    "write": ["/work/cola"],
                    "globScanMaxDepth": 8
                }
            }
        });
        assert!(matches!(
            decode_request(
                "item/permissions/requestApproval",
                &hidden,
                &Projections::default()
            ),
            Err(Decode::CannotExpress(_))
        ));

        let hidden_special = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "cwd": "/work/cola",
            "permissions": {
                "fileSystem": {
                    "entries": [{
                        "access": "read",
                        "path": { "type": "special", "value": { "kind": "project_roots" } }
                    }]
                }
            }
        });
        assert!(matches!(
            decode_request(
                "item/permissions/requestApproval",
                &hidden_special,
                &Projections::default()
            ),
            Err(Decode::CannotExpress(_))
        ));

        let visible = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "cwd": "/work/cola",
            "permissions": {
                "fileSystem": {
                    "write": ["/work/cola"],
                    "entries": [{
                        "access": "read",
                        "path": { "type": "glob_pattern", "pattern": "**/*.rs" }
                    }]
                },
                "network": { "enabled": false }
            }
        });
        assert!(
            decode_request(
                "item/permissions/requestApproval",
                &visible,
                &Projections::default()
            )
            .is_ok(),
            "every rendered field decodes"
        );
    }

    #[test]
    fn session_decisions_follow_the_method_table() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "execCommandApproval",
            "applyPatchApproval",
        ] {
            assert!(
                advertises_session_decision(method),
                "{method} advertises a session decision"
            );
        }
        for method in [
            "item/permissions/requestApproval",
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
        ] {
            assert!(
                !advertises_session_decision(method),
                "{method} does not advertise a session decision"
            );
        }
    }

    #[test]
    fn an_elicitation_uses_its_message_as_the_card_title() {
        let params = json!({
            "serverName": "files",
            "threadId": "thread-1",
            "message": "把文件写入工作区？",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": { "confirm": { "type": "boolean", "title": "确认" } },
                "required": ["confirm"]
            }
        });
        let Decoded { question, .. } =
            decode_request("mcpServer/elicitation/request", &params, &Projections::default())
                .expect("decodes");
        assert_eq!(
            question.expect("a form").title,
            "把文件写入工作区？",
            "the message explains the request even when the fields are sparse"
        );
    }

    /// A socket close ends the answers too: a pending request must not stay
    /// listed against a dead connection, or a click would fail and consume it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_closed_connection_clears_pending_requests() {
        use crate::backend::Backend;
        use crate::codex::client::{CodexClient, CodexOptions};
        use crate::codex::schema::PinnedProbe;
        use crate::codex::test_server::FakeServer;
        use crate::codex::transport::Endpoint;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("app-server.sock");
        let server = FakeServer::unix(&path, "codex-tui/0.163.0 (test)").await;
        let endpoint = Endpoint::parse(&format!("unix://{}", path.display())).expect("endpoint");
        let client = CodexClient::connect(&CodexOptions::new(endpoint), &PinnedProbe)
            .await
            .expect("attach to the scripted server");

        server.request_to_all(
            "item/commandExecution/requestApproval",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "command": "ls",
                "cwd": "/work",
            }),
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while client
            .list_permissions(Some("/work"))
            .await
            .expect("the pending list")
            .is_empty()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the approval was never recorded"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        server.die().await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !client
            .list_permissions(None)
            .await
            .expect("the pending list")
            .is_empty()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a pending approval outlived its connection"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[test]
    fn the_unanswered_request_kinds_are_unsupported() {
        for method in [
            "account/chatgptAuthTokens/refresh",
            "attestation/generate",
            "item/tool/call",
        ] {
            assert!(matches!(
                decode_request(method, &json!({}), &Projections::default()),
                Err(Decode::Unsupported)
            ));
        }
    }
}
