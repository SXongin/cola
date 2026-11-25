use std::ops::Deref;
use std::sync::Arc;

use tracing::Instrument;

use crate::backend::types::{PromptFile, SessionStatus};
use crate::backend::{PromptSkill, SkillInfo, TaskRetirementEnding};
use crate::bridge::access::{Access, Decision, DenyReason};
use crate::bridge::command;
use crate::bridge::core::SharedCore;
use crate::bridge::session::PendingEntry;
use crate::bridge::span;
use crate::bridge::turn::PromptContext;
use crate::bridge::{OmitReason, OmittedAttachment};
use crate::config::{Config, ConversationKind, ThreadKey};
use crate::feishu;
use crate::feishu::card::session::BrowseView;

/// Shown once when the first top-level message in a group auto-creates the
/// group's lobby session.
const GROUP_LOBBY_GUIDANCE: &str = "\
**已创建群会话** —— 这里就是本群的默认会话，可直接对话。
想为不同项目/任务分别开会话，请用：
- 在群里回复开启**话题**：每个话题是一个独立会话
- `/new [名字]` 开一个显式会话
- `/dir <路径>` 切换项目目录
- `/help` 查看全部命令";

// Re-exec cola itself with the ORIGINAL startup args, inheriting stdio so a
// shell log redirect (`cola ... > test.log 2>&1`) carries into the new process.
// The current process then calls `std::process::exit(0)` right after.

/// The server's per-attachment cap (`MAX_ATTACHMENT_BYTES`, the decoded payload
/// size past which the server fails the WHOLE prompt). cola pre-checks the same
/// line so an oversize attachment is reported instead of sent.
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// How long a card callback waits for a `sessions.json` write before it answers
/// anyway (ADR-0089). Feishu drops a callback it has not heard from within 3 s
/// and cola answers at `CARD_ACK_BUDGET`; the margin left here is for the
/// card's other work and the response itself, which must not be raced.
const CARD_STORE_BUDGET: std::time::Duration =
    crate::feishu::ws::CARD_ACK_BUDGET.saturating_sub(std::time::Duration::from_millis(600));

/// The toast for a card callback that found its store lock still held at
/// `CARD_STORE_BUDGET`: nothing was mutated, so no pending was declared.
pub(crate) const CARD_STORE_TIMEOUT: &str = "⚠️ 处理超时，请重新点选（未做任何更改）";

/// One inbound attachment cola could not attach, with the reason to show
/// (ADR-0076's inbound amendment: nothing is dropped silently).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Omission {
    /// The file's display name; `None` for an image (it has no name).
    pub name: Option<String>,
    pub reason: OmitReason,
}

/// Classify downloaded inbound attachments into prompt file parts (ADR-0076's
/// inbound amendment). An image keeps one of the four server-readable image
/// mimes; a file becomes `application/pdf`, one of the four image mimes, or
/// `text/plain`; anything else is omitted as unsupported. An attachment past
/// the server's 20MB cap is omitted too — for both images and files, since the
/// image path previously had no check and an oversize image failed the whole
/// run. Returns the attachable parts (images then files) and the omissions.
pub(crate) fn prompt_files(
    images: &[crate::feishu::client::ImageAttachment],
    files: &[crate::feishu::client::FileAttachment],
) -> (Vec<PromptFile>, Vec<Omission>) {
    let mut parts = Vec::new();
    let mut omissions = Vec::new();
    for image in images {
        // An image has no name, so its mime alone decides; the four-mime check
        // drops a `bmp`/`tiff`/`heic` the server would silently discard on V2.
        match classify_attachment(&image.mime, None, &image.data) {
            Ok(part) => parts.push(part),
            Err(omission) => omissions.push(omission),
        }
    }
    for file in files {
        match classify_attachment(&file.mime, Some(file.name.clone()), &file.data) {
            Ok(part) => parts.push(part),
            Err(omission) => omissions.push(omission),
        }
    }
    (parts, omissions)
}

/// One downloaded attachment's payload as a prompt file part, or the omission
/// it must be reported as. `name` is `None` for an image (it has no name).
/// Type is decided before size, so an unsupported oversize file reads
/// `类型不支持` (it would never be sent anyway).
fn classify_attachment(mime: &str, name: Option<String>, data: &[u8]) -> Result<PromptFile, Omission> {
    use base64::Engine;
    match crate::feishu::file::inbound_mime(mime, name.as_deref().unwrap_or("")) {
        Some(mime) if data.len() <= MAX_ATTACHMENT_BYTES => Ok(PromptFile {
            mime,
            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
            name,
        }),
        Some(_) => Err(Omission {
            name,
            reason: OmitReason::OverSize,
        }),
        None => Err(Omission {
            name,
            reason: OmitReason::Unsupported,
        }),
    }
}

/// The prompt-body fragment one omitted attachment contributes: `[文件: <name> ·
/// 未附带：<reason>]`, or `[图片 · 未附带：<reason>]` for an image (no name).
fn omission_note(name: Option<&str>, reason: OmitReason) -> String {
    match name {
        Some(name) => format!("[文件: {name} · 未附带：{}]", reason.label()),
        None => format!("[图片 · 未附带：{}]", reason.label()),
    }
}

/// Join the omission notes onto a message body. When the body IS the message's
/// own placeholder (`[文件: <name>]` for a file message, `[图片]` for an
/// image), the note annotates it in place; otherwise it is appended on its own
/// new line so the body's own text stands (ADR-0076's "有正文则另起一行追加").
pub(crate) fn join_omissions(text: &mut String, omissions: &[Omission]) {
    for omission in omissions {
        let base = match omission.name.as_deref() {
            Some(name) => format!("[文件: {name}]"),
            None => "[图片]".to_string(),
        };
        let note = omission_note(omission.name.as_deref(), omission.reason);
        if text.trim() == base {
            *text = note;
        } else {
            text.push('\n');
            text.push_str(&note);
        }
    }
}

/// The one `⚠️` reply naming every attachment the turn could not attach, so the
/// user can tell their file was not read (ADR-0076's inbound amendment).
fn omission_warning(omissions: &[Omission]) -> String {
    omissions
        .iter()
        .map(|omission| match omission.name.as_deref() {
            Some(name) => format!("⚠️ 文件 {name} 未能附带（{}）", omission.reason.label()),
            None => format!("⚠️ 图片 未能附带（{}）", omission.reason.label()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One bounded `session_status` read for the prompt path — the ADVISORY read
/// that only picks a new Turn's opening line (ADR-0062, ADR-0109: the prompt
/// router routes on the held guard alone) — and the retry matrix read through
/// it, so the two can never drift in bound or failure policy. `what` labels the
/// read in the bounded-call timeout log; both callers pass the same injected
/// per-read bound (`TurnConfig::follow_read_timeout_ms`).
///
/// `None` is UNKNOWN: a failed call, a timed-out call, or an unrecognised
/// status kind (each warned once here). A successful read always yields a
/// status — absence from the server's map means idle. The callers own the
/// direction: the advisory read treats unknown as NOT live (the ordinary
/// loading card; a read may never invent a live Execution), while the retry
/// matrix submits a fresh id (the click must always have an effect).
async fn read_session_status(
    what: &str,
    timeout_ms: u64,
    backend: &Arc<dyn crate::backend::Backend>,
    session_id: &str,
    directory: Option<&str>,
) -> Option<SessionStatus> {
    match crate::bridge::bounded_call(what, timeout_ms, backend.session_status(session_id, directory)).await {
        Some(Ok(Some(status))) => Some(status),
        Some(Ok(None)) => {
            tracing::warn!("{what}: unrecognised status kind; reading as unknown");
            None
        }
        Some(Err(e)) => {
            tracing::warn!("{what}: {e}");
            None
        }
        // `bounded_call` already logged the timeout.
        None => None,
    }
}

/// The prompt route (ADR-0062, ADR-0109, ADR-0113). A **held guard** — the
/// [`crate::bridge::turn::CardOwnership`] verdict's in-process claim — is the
/// routing key; the Backend's status read only picks the new card's opening
/// line. A card-only chain is a display state, never a routing key.
enum PromptRoute {
    /// An in-process renderer holds the guard (a Turn, or the out-of-turn
    /// follow that inherited it): the message merges into it as a Supplement,
    /// and the chain splits.
    Supplement,
    /// No held guard: the message starts a new Turn, whose card owns and
    /// settles it. A genuinely live run still merges it through the prompt's
    /// merge delivery.
    NewTurn {
        /// Whether the Backend's advisory read reported a live Execution —
        /// the opening line's source only, never the route.
        advisory_live: bool,
    },
}

/// One routed user message (ADR-0113): the Turn's context, owned from
/// classification until the route has actually taken it. The route is decided
/// first and submitted second, and a handshake that changes the route must
/// submit the SAME message — never a copy or a second id.
struct PromptMessage {
    /// The Turn's context, built once with the route-only axes unset: the
    /// opening line (`advisory_live`) is the route's to choose, and
    /// `cola_message_id` is a Turn's own mint (a fresh message never reuses an
    /// id across routes).
    context: PromptContext,
}

impl PromptMessage {
    /// The context, taken only once the route has claimed the message: the
    /// route supplies its one axis.
    fn into_context(self, advisory_live: bool) -> PromptContext {
        PromptContext {
            advisory_live,
            ..self.context
        }
    }
}

/// What a merge-only steer answered, with the shared logging (#1056, ADR-0109,
/// ADR-0113). Only [`Self::Merged`] submitted something; an adapter that breaks
/// the merge-only contract has already started a run, and an `Err` may still
/// have landed (ADR-0092) — both are unconfirmed and never retried.
enum SteerAnswer {
    Merged,
    /// The adapter refused before submitting: nothing reached the Backend, so
    /// the message may be routed again.
    Refused,
    Unconfirmed,
}

fn classify_steer(
    session_id: &str,
    outcome: crate::error::Result<crate::backend::SteerOutcome>,
) -> SteerAnswer {
    match outcome {
        Ok(crate::backend::SteerOutcome::Merged) => SteerAnswer::Merged,
        Ok(crate::backend::SteerOutcome::RefusedNoLiveRun) => SteerAnswer::Refused,
        Ok(crate::backend::SteerOutcome::StartedTurn) => {
            tracing::warn!(
                "supplement: adapter started a new turn for a merge-only steer on session {session_id}"
            );
            SteerAnswer::Unconfirmed
        }
        Err(e) => {
            tracing::warn!("supplement: prompt failed: {}", e);
            SteerAnswer::Unconfirmed
        }
    }
}

/// How long a Supplement's typed "no live run" refusal may wait before the
/// handshake logs the long wait (ADR-0113). This is a **diagnostic**, not a
/// deadline: the wait ends when the guard clears — a Turn ends, and an
/// out-of-turn renderer's lease is scoped to its own loop (the live #1086
/// report: the Wake settled 100 ms after the refusal). Past it the message
/// keeps waiting, because reporting it unsubmitted while a writer exists is
/// exactly the message loss #1086 fixes.
const SUPPLEMENT_WAIT_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

/// The handshake's re-read cadence while it waits for that guard to clear: fast
/// while a real renderer is expected to retire (the live #1086 report: 100 ms).
const SUPPLEMENT_HANDSHAKE_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// The re-read cadence after [`SUPPLEMENT_WAIT_WARN_AFTER`]: a wait that long is
/// a Turn running, not a renderer retiring, so the poll slows to a tick that
/// still notices the guard clearing promptly without spinning.
const SUPPLEMENT_HANDSHAKE_POLL_COARSE: std::time::Duration = std::time::Duration::from_millis(250);

/// How often the handshake re-tries the merge while a **writer** holds the guard
/// (a Turn whose run has not registered yet). Coarser than the poll: each
/// attempt is a Backend call, and the merge lands as soon as the run is visible.
const SUPPLEMENT_MERGE_RETRY: std::time::Duration = std::time::Duration::from_millis(500);

/// The notice for a merged Supplement whose chain cannot continue (the card
/// ended inside the split's admission): the message DID reach the Backend, so
/// the copy must never claim it was lost (ADR-0092).
const SUPPLEMENT_UNCONTINUABLE_NOTICE: &str = "⚠️ 补充消息已提交，但当前卡片无法继续，请留意后续回复。";

/// The notice for a failed steer with no proven writer (ADR-0109): the merge is
/// unconfirmed — an ambiguous response may have landed — so the copy says so and
/// never invites a resend (ADR-0092).
const SUPPLEMENT_UNCONFIRMED_NOTICE: &str = "⚠️ 补充消息未确认送达，请先确认当前回复是否继续。";

/// What the prompt pipeline did with a submission, for the one caller that must
/// report the outcome on a card of its own (the Terminal Card's 「问 AI」,
/// ADR-0086 decision 9). Deriving it from the pipeline's own decision — rather
/// than reading ownership a second time — is what keeps the report from ever
/// disagreeing with the route. It deliberately says nothing about the model
/// having answered: the new Turn's card, or the live chain's continuation,
/// carries the answer.
enum PromptSubmission {
    /// No chain was owned: the message opened a new Turn whose card answers it.
    NewTurn,
    /// cola owned the live chain: the message merged into that Turn as its
    /// Supplement.
    Supplement,
    /// Nothing reached the Backend: the Turn could not start, its submit was
    /// rejected, or the Supplement's send failed. The failure was already
    /// spoken; the caller must not report a submission.
    NotAccepted,
}

/// Which `action: "file"` transfer a card click asked for (#739, #778). Both
/// send the file's whole bytes as one File Message; they differ in the card the
/// click landed on. [`ViewerFileOp::Send`] is the viewer's own button, so its
/// outcome is PATCHed back onto that viewer card (in place). [`ViewerFileOp::Preview`]
/// is a Tool Panel's button, and the clicked message is the agent's **Turn
/// card**: a PATCH would overwrite the panel and the reply, so a preview sends
/// and reports nothing on the card (the File Message is the evidence, and a
/// failure is spoken as a reply instead).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewerFileOp {
    Send,
    Preview,
}

/// Everything a settled Terminal Card's buttons carry (ADR-0086 decisions 4, 7
/// and 9): the clicked card and the **Terminal Log** path, plus the settled
/// card's own render inputs so a click's outcome can be PATCHed back onto the
/// same card without a card-view read or a second state store.
pub(crate) struct TerminalCardRequest {
    pub(crate) message_id: String,
    chat_id: String,
    /// The run every settled button names (ADR-0086 decision 4). Carried so a
    /// click can be checked against the Chat/Topic's current run id — the stale
    /// rule the Terminal Run Record makes checkable (#827).
    pub(crate) run_id: String,
    thread_id: String,
    /// Whether the card lives in a group chat. A click carries no chat type of
    /// its own, and the 「问 AI」 submission needs it to rebuild the Chat/Topic
    /// route; absent reads as group (the skill picker's fallback).
    is_group: bool,
    path: String,
    cwd: String,
    command: String,
    /// Whether the sweep had marked this run lost: the rebuilt card must stay a
    /// lost card (a restart is not an exit code).
    lost: bool,
    exit_code: Option<i32>,
    elapsed: std::time::Duration,
    output_over_limit: bool,
}

impl TerminalCardRequest {
    /// Parse the button value; `None` when the click cannot be routed (no card
    /// id, no log path) or the payload is not one of ours.
    pub(crate) fn from_value(value: &serde_json::Value) -> Option<Self> {
        let message_id = value.get("open_message_id")?.as_str()?.to_string();
        let path = value.get("path")?.as_str()?.to_string();
        if message_id.is_empty() || path.is_empty() {
            return None;
        }
        let run = value.get("terminal_card")?;
        Some(Self {
            message_id,
            chat_id: value
                .get("chat_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            run_id: value
                .get("run_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            thread_id: value
                .get("thread_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            is_group: value.get("chat_type").and_then(serde_json::Value::as_str) != Some("p2p"),
            path,
            cwd: run.get("cwd")?.as_str()?.to_string(),
            command: run.get("command")?.as_str()?.to_string(),
            lost: run
                .get("lost")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            exit_code: run
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .map(|code| code as i32),
            elapsed: std::time::Duration::from_millis(
                run.get("elapsed_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            ),
            output_over_limit: run
                .get("output_over_limit")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Everything the Terminal Card's 「问 AI」 button carries (ADR-0086 decisions 4
/// and 9): the clicked card's facts plus the output window it was rendered
/// with. The submission therefore needs neither the live process nor the
/// **Terminal Log** on disk — a run the sweep marked lost keeps its button
/// working.
pub(crate) struct TerminalAskAiRequest {
    pub(crate) card: TerminalCardRequest,
    /// The window the card showed: already ANSI-stripped, line-capped and
    /// bounded by the display tail budget.
    output: String,
    /// Whether older output existed and was omitted from that window.
    output_truncated: bool,
    /// The click's own actor, so a group Completion Notice can name them.
    requester_open_id: Option<String>,
}

impl TerminalAskAiRequest {
    /// Parse the button value; `None` when the payload is not one of ours (a
    /// submission without a window has nothing to quote, and one without a run
    /// id cannot name the run it was rendered for).
    pub(crate) fn from_value(value: &serde_json::Value) -> Option<Self> {
        let card = TerminalCardRequest::from_value(value)?;
        if card.run_id.is_empty() {
            return None;
        }
        Some(Self {
            card,
            output: value.get("output")?.as_str()?.to_string(),
            output_truncated: value
                .get("output_truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            requester_open_id: value
                .get("operator_open_id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
        })
    }
}

/// The transfer key of one viewer File Message attempt: the Chat, the clicked
/// message, the **path** (a Tool Panel's Turn card can carry several file
/// panels, so the clicked message alone does not name one file), and the content
/// identity (a re-read rotates it, so changed bytes may be sent again).
type ViewerFileTransferKey = (String, String, String, u64);
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewerFileTransferStatus {
    Sent,
    OutcomeUnknown,
}

/// The process-local state of one Turn Diff delivery (#801), keyed by
/// `(session_id, from)` — the card's exact Turn identity. `Sending` is the
/// single-flight claim (a concurrent click is a no-op); `Sent`,
/// `OutcomeUnknown` and `Empty` are terminal and make every later click a
/// no-op. A definite failure removes the entry, because nothing was sent and a
/// retry is not a duplicate.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnDiffSendState {
    Sending,
    Sent,
    OutcomeUnknown,
    /// The Turn changed no file: the answer is terminal even though no file
    /// was sent, so repeated clicks do not re-read the diff or repeat the
    /// reply.
    Empty,
}

/// One click's read result: the diff and the Turn's server-time anchor (the
/// artifact header's time). The anchor is `None` when the transcript read failed
/// — the artifact stays usable and says 「时间未知」 rather than failing.
struct TurnDiffRead {
    diff: crate::backend::TurnDiff,
    anchor_ms: Option<i64>,
}

/// One Turn Diff callback's whole routing payload, owned by the spawned task.
pub(crate) struct TurnDiffClick {
    pub(crate) session_id: String,
    /// The Turn anchor's user-message id (empty is allowed for future entries;
    /// V2 then asks for the latest Turn, while V1 cannot aggregate without it).
    pub(crate) from: String,
    /// The per-child-session routing key the #477 button carries.
    pub(crate) directory: String,
    /// The clicked Turn card, which the File Message replies under
    /// (`open_message_id` — never part of the card's own JSON).
    pub(crate) message_id: String,
}

/// RAII claim for one in-flight Turn Diff delivery. A duplicate click is
/// acknowledged without spawning a second one; Drop releases the claim —
/// removing a `Sending` entry so a failed or cancelled attempt stays retryable.
pub(crate) struct TurnDiffClaim {
    sends: Arc<std::sync::Mutex<indexmap::IndexMap<(String, String), TurnDiffSendState>>>,
    key: (String, String),
}

impl Drop for TurnDiffClaim {
    fn drop(&mut self) {
        let mut sends = self
            .sends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sends.get(&self.key) == Some(&TurnDiffSendState::Sending) {
            sends.shift_remove(&self.key);
        }
    }
}

/// What a click answers when the authoritative read is empty: the index existed
/// at settle, so the Turn's net change has gone to zero since (usually an edit
/// reverted later). ADR-0087.
const TURN_DIFF_EMPTY_TEXT: &str = "本轮期间工作区没有留下净变化。";

/// What a failed Turn Diff READ answers (a transient error, a server restart or
/// a snapshot the server cannot answer). The claim is dropped, so a later click
/// retries. Distinct from a failed render/upload, which the artifact's own copy
/// covers.
const TURN_DIFF_READ_FAILURE_TEXT: &str = "暂时读不到终态改动，请稍后再试。";

/// What a failed render or upload answers. The click is never silent, and a
/// definite failure is not retried to the caller's timeout.
const TURN_DIFF_FAILURE_TEXT: &str = "生成完整 diff 失败，请稍后重试。";

/// The artifact's `title`: what the reader opens from the Turn card. It names
/// the measurement — the worktree's change during the Turn — because the diff
/// cannot say who wrote it (#844).
const TURN_DIFF_TITLE: &str = "本轮期间的工作区变化";

/// The artifact subtitle's closing note: the diff compares two worktree
/// snapshots, so concurrent writers in the same directory are included.
const TURN_DIFF_SCOPE_NOTE: &str = "快照区间差，可能含同目录其他写入者";

/// The artifact header's time when the transcript read failed: the diff is
/// still usable, so the header states the gap instead of failing the click.
const TURN_DIFF_UNKNOWN_TIME: &str = "时间未知";

/// The bridge coordinator. Owns the state shared by every flow ([`SharedCore`])
/// plus the per-flow modules that hold their own private state. `Deref`s to the
/// shared core so the coordinator's own code can reach the aggregate; flows
/// receive narrow per-concern handles instead (`SharedCore::turn_handles` is
/// the Turn's bundle, spec #298).
pub struct App {
    pub(crate) core: Arc<SharedCore>,
    /// The Host gate (ADR-0035): every inbound message and card action is
    /// authorized against the Access List before cola acts.
    pub(crate) access: tokio::sync::Mutex<Access>,
    /// File Viewer callbacks are acknowledged before work begins; hold one
    /// process-local claim per Chat + clicked card until its task and PATCH end.
    viewer_actions_in_flight: Arc<std::sync::Mutex<std::collections::HashSet<(String, String)>>>,
    /// Viewer File Message outcomes, so stale callbacks cannot redeliver after
    /// either a confirmed send or an ambiguous timeout.
    pub(crate) viewer_file_transfers_sent:
        Arc<std::sync::Mutex<indexmap::IndexMap<ViewerFileTransferKey, ViewerFileTransferStatus>>>,
    /// Turn Diff deliveries, keyed by `(session_id, from)` and bounded like the
    /// File Message guard (#801): one artifact per Turn per process, with the
    /// `Sending` claim making concurrent clicks a no-op.
    turn_diff_sends: Arc<std::sync::Mutex<indexmap::IndexMap<(String, String), TurnDiffSendState>>>,
    /// Weak self-reference, set once inside `run` (which holds the Arc). Lets
    /// the `EventSink` trait impl (which only has `&self`) recover a
    /// `&Arc<App>` to hand to the inherent methods. `Weak` so it never keeps
    /// the app alive — no reference cycle; `OnceLock` because it is written
    /// exactly once, before any event can arrive.
    self_weak: std::sync::OnceLock<std::sync::Weak<App>>,
    /// Test-only (#450): whether a prompt stats its working directory before
    /// dispatching. The harness seeds Sessions at abstract paths ("/work")
    /// that no disk carries, so this defaults off and the two ending tests turn
    /// it on; production has no switch and always stats.
    #[cfg(test)]
    pub(crate) check_missing_directories: std::sync::atomic::AtomicBool,
    /// Test seam that pauses a Pending Session after its final provider-fact
    /// normalization and before activation persists the new SessionEntry.
    #[cfg(test)]
    pub(crate) materialise_before_activation: Arc<tokio::sync::Mutex<Option<Arc<tokio::sync::Semaphore>>>>,
    #[cfg(test)]
    pub(crate) materialise_activation_entered: Arc<tokio::sync::Notify>,
}

impl Deref for App {
    type Target = SharedCore;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

#[async_trait::async_trait]
impl crate::bridge::EventSink for App {
    async fn admit(&self, msg: &crate::bridge::IncomingMessage) -> bool {
        // The Access gate, run BEFORE any attachment download (ADR-0035: the
        // Principal is resolved before anything else happens). `gate_message`
        // fully answers a refusal/Claim; `handle_message` runs it again
        // (idempotently for an admitted message) as its own entry-point guard.
        match self.self_arc() {
            Some(app) => !app.gate_message(msg).await,
            None => false,
        }
    }

    async fn handle_message(&self, msg: crate::bridge::IncomingMessage) {
        if let Some(app) = self.self_arc() {
            app.handle_message(msg).await;
        }
    }

    async fn handle_card_action(&self, value: serde_json::Value) -> Option<CardActionResult> {
        match self.self_arc() {
            Some(app) => app.handle_card_action(value).await,
            None => None,
        }
    }
}

/// What a card click produces: an optional replacement card (JSON 2.0, so it
/// stays update-compatible with the 2.0 interactive cards, see Feishu err
/// 200830) and an optional client Toast for instant feedback. `card: None` means
/// "keep the current card" — used when an interaction was answered inline inside
/// the streaming card, which re-renders itself on the next poll.
#[derive(Clone)]
pub struct CardActionResult {
    pub card: Option<serde_json::Value>,
    pub toast: Option<String>,
}

/// Everything a terminal card's recovery click needs once its claim is held:
/// the claimed fixture, the Chat/Topic it belongs to, the directory its reads
/// route under, and the narrow handle bundle the spawned pipeline runs with.
/// Built by [`App::claim_recovery_click`] and destructured by the verb module
/// that claimed it ([`crate::bridge::card_action::retry`]).
pub(crate) struct RecoveryClick {
    pub(crate) recovery: crate::bridge::turn::TurnRecovery,
    pub(crate) thread_key: ThreadKey,
    pub(crate) directory: Option<String>,
    pub(crate) handles: crate::bridge::handles::TurnHandles,
}

/// The thread a card callback routes to. Every cola card button carries its
/// `chat_id` + `thread_id` in the value payload so the ack can route the choice
/// back to the right conversation. Shared by the dispatch's span context and
/// every verb module behind the CardAction seam.
pub(crate) fn thread_key_from_value(value: &serde_json::Value) -> crate::config::ThreadKey {
    crate::config::ThreadKey::new(
        value
            .get("chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        value
            .get("thread_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    )
}

/// Nested-topic rejection (ADR-0006, ADR-0025): a topic can only be created
/// from a non-topic message. A card whose `thread_id != chat_id` lives inside
/// a topic — a never-bound topic may open the `/dir`/`/switch` cards to bind
/// its single session, but the topic-creating ops must not nest. The one
/// guard + message shared by both card ops keeps their copies from drifting
/// (the text forms carry the same rule with their own reply text).
const NESTED_TOPIC_REJECTION: &str = "话题里不能开话题，请回主对话操作。";

pub(crate) fn reject_nested_topic(thread_key: &ThreadKey) -> Option<CardActionResult> {
    (thread_key.thread_id != thread_key.chat_id).then(|| CardActionResult {
        card: None,
        toast: Some(NESTED_TOPIC_REJECTION.to_string()),
    })
}

/// The read model of one `/dir` browse card render (ADR-0078): the directory
/// actually browsed (after resolution and any ancestor fallback), its child
/// rows (directories and files, filtered + sorted), whether the filter control
/// shows, and any fallback explanation.
pub(crate) struct BrowseState {
    dir: String,
    children: Vec<crate::feishu::card::session::BrowseChild>,
    show_filter: bool,
    notice: Option<String>,
}

impl BrowseState {
    /// The state a failed scan reports: the card still renders, saying why.
    fn unavailable() -> Self {
        Self {
            dir: String::new(),
            children: Vec::new(),
            show_filter: false,
            notice: Some("目录不可用".to_string()),
        }
    }
}

/// How many directory entries one browse render will `stat`. A pathological
/// directory (tens of thousands of children on a slow filesystem) must not
/// outrun the card callback's budget — the WS layer drops a late card result
/// entirely — so the scan stops here and the card says it was truncated
/// (#670 review).
pub(crate) const BROWSE_SCAN_CAP: usize = 2000;
/// Read + shape the browse card's data for a requested directory (ADR-0078,
/// ADR-0083 §3): resolve the target (symlinks followed, see
/// [`resolve_browse_path`]), fall back to the nearest existing ancestor when the
/// directory is gone, then list its children — directories first (a repo, a
/// child holding a `.git` entry, floats to the very front) and **regular files**
/// after, each group case-insensitively alphabetical; dot-entries behind the
/// hidden toggle; token-AND filtered by `keyword` over directory AND file names.
/// A file row carries the RESOLVED path (ADR-0083 §3), because its click goes
/// straight to the viewer — the path it names is the path it shows; only a
/// symlink entry is canonicalized, a plain one is already canonical. All local
/// `stat`/`read_dir`: the card callback runs inside Feishu's 3 s, no-retry
/// budget.
fn browse_dir_state(view: BrowseView<'_>) -> BrowseState {
    let keyword = view.keyword;
    let show_hidden = view.show_hidden;
    let Some(mut dir) = resolve_browse_path(view.dir) else {
        return BrowseState::unavailable();
    };
    let mut notice = None;
    if !dir.is_dir() {
        match nearest_existing_dir(&dir) {
            Some(ancestor) => {
                notice = Some(format!("目录不存在，已回退到 `{}`", ancestor.display()));
                dir = ancestor;
            }
            None => {
                return BrowseState {
                    dir: dir.to_string_lossy().to_string(),
                    children: Vec::new(),
                    show_filter: false,
                    notice: Some("目录不存在".to_string()),
                };
            }
        }
    }
    let lower_keyword = keyword.to_lowercase();
    let mut children = Vec::new();
    let mut visible = 0usize;
    let mut scanned = 0usize;
    let mut truncated = false;
    let mut read_error = false;
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                // Count EVERY attempt, errors included: a persistently failing
                // iterator must not loop without advancing the bound.
                if scanned == BROWSE_SCAN_CAP {
                    truncated = true;
                    break;
                }
                scanned += 1;
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        read_error = true;
                        continue;
                    }
                };
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                // A dot-directory and a dot-file are hidden alike (ADR-0083 §3).
                if !show_hidden && name.starts_with('.') {
                    continue;
                }
                // `metadata()` follows a symlink, so a linked directory is a
                // legitimate browsing target and a linked file reports its
                // target's size; an entry that cannot be stat'ed at all (a
                // broken symlink) is no row at all.
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                // Only a directory and a REGULAR file are rows (#742 review): a
                // FIFO, socket or device node is not a file the viewer could
                // preview, so listing one as a `0 B` file would be a lie. It is
                // no row at all — and never counted into `visible`, which is
                // what decides whether the filter control shows.
                let is_file = meta.is_file();
                if !meta.is_dir() && !is_file {
                    continue;
                }
                visible += 1;
                if !lower_keyword.is_empty()
                    && !crate::bridge::command::token_and_match(&name.to_lowercase(), &lower_keyword)
                {
                    continue;
                }
                let row_path = row_path_for(&entry, path, is_file);
                children.push(crate::feishu::card::session::BrowseChild {
                    name,
                    path: row_path.to_string_lossy().to_string(),
                    is_repo: meta.is_dir() && row_path.join(".git").exists(),
                    // `Some` marks a file row: files are listed beside
                    // directories and open the File Viewer (ADR-0083 §3).
                    size: is_file.then_some(meta.len()),
                });
            }
        }
        Err(e) => notice = Some(format!("无法读取该目录：{e}")),
    }
    if notice.is_none() {
        // A hard read failure outranks a partial read, which outranks truncation.
        if read_error {
            notice = Some("部分目录项读取失败，列表可能不完整".to_string());
        } else if truncated {
            notice = Some(format!("目录条目过多，仅扫描了前 {BROWSE_SCAN_CAP} 项"));
        }
    }
    children.sort_by(|a, b| {
        b.is_repo
            .cmp(&a.is_repo)
            // Directories first (ADR-0083 §3): a file row sorts after every
            // directory row, each group case-insensitively alphabetical.
            .then_with(|| a.size.is_some().cmp(&b.size.is_some()))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    let show_filter = visible > crate::feishu::card::session::CARD_PAGE_SIZE || !keyword.is_empty();
    BrowseState {
        dir: dir.to_string_lossy().to_string(),
        children,
        show_filter,
        notice,
    }
}

/// The path a browse row stores (ADR-0083 §3).
///
/// A **file** row stores the path the viewer will NAME: "the path is the one
/// that was resolved, not the one that was typed". Its click bypasses
/// `resolve_browse_path` — the row routes straight to `action: "file"` — so the
/// link path `read_dir` handed us must not be stored: a symlinked file would
/// otherwise open a viewer naming the link. The browsed directory is already
/// canonical, so only a SYMLINK entry needs resolving — a plain entry's path is
/// canonical as it stands, and canonicalizing every row would spend a syscall
/// per file on a directory of up to `BROWSE_SCAN_CAP` entries, inside Feishu's
/// 3 s callback budget. For a link, the scan's `metadata()` followed it, so
/// canonicalize here is exactly the `stat`-followed target the row's size came
/// from.
///
/// A **directory** row always keeps the entry path: its click re-resolves
/// through the browse handler anyway.
///
/// `pub(crate)` so the gate itself is pinned by
/// `tests::dir::dir_browse_row_path_resolves_only_symlink_entries` (#742
/// review): a black-box browse of a canonical directory cannot tell a gated
/// canonicalize from an unconditional one.
pub(crate) fn row_path_for(
    entry: &std::fs::DirEntry,
    path: std::path::PathBuf,
    is_file: bool,
) -> std::path::PathBuf {
    if is_file && entry_is_symlink(entry, &path) {
        std::fs::canonicalize(&path).unwrap_or(path)
    } else {
        path
    }
}

/// Whether the `read_dir` entry at `path` is a symlink, read from the entry's
/// cached file type where the platform provides one — no extra syscall for the
/// common case of a plain regular file — and from an `lstat` only when that type
/// is unavailable (#742 review). Only such an entry's path needs canonicalizing.
pub(crate) fn entry_is_symlink(entry: &std::fs::DirEntry, path: &std::path::Path) -> bool {
    match entry.file_type() {
        Ok(file_type) => file_type.is_symlink(),
        Err(_) => std::fs::symlink_metadata(path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false),
    }
}

/// Read + build the `/dir` browse card (ADR-0078, ADR-0083 §2) for `view`:
/// resolve the target (symlinks followed, landing on the nearest existing
/// ancestor when it is gone), list its child directories and files, and render
/// the card with the conversation's current directory marked. Shared by the
/// App's own browse rebuilds and the `/cat <directory>` answer — a command card
/// has no `App`, only the
/// [`CommandHandles`](crate::bridge::handles::CommandHandles) its dispatch
/// carries — so both render from one source of truth.
pub(crate) async fn build_dir_browse_card_for(
    sessions: &crate::bridge::handles::SessionsHandle,
    thread_key: &ThreadKey,
    view: BrowseView<'_>,
) -> serde_json::Value {
    // The directory scan is synchronous filesystem work, and a synchronous scan
    // cannot be interrupted by a caller's timeout (CODING_STANDARDS.md: a card
    // callback runs on a deadline). Run it on the blocking pool so a click can
    // abandon the refresh at its deadline while the scan finishes harmlessly in
    // the background; the scan itself is capped by `BROWSE_SCAN_CAP` entries.
    let dir = view.dir.to_string();
    let keyword = view.keyword.to_string();
    let page = view.page;
    let show_hidden = view.show_hidden;
    let state = tokio::task::spawn_blocking(move || {
        browse_dir_state(BrowseView {
            dir: &dir,
            show_hidden,
            keyword: &keyword,
            page,
        })
    })
    .await
    .unwrap_or_else(|_| BrowseState::unavailable());
    let current_dir = sessions.current_directory(thread_key).await;
    // The card renders the RESOLVED directory (post-resolution/ancestor
    // fallback), not the requested one.
    let resolved = BrowseView {
        dir: &state.dir,
        ..view
    };
    crate::feishu::card::session::build_dir_browse_card(
        thread_key,
        &state.children,
        current_dir.as_deref(),
        resolved,
        state.show_filter,
        state.notice.as_deref(),
    )
}

/// Resolve a browse target to the real directory it denotes, following
/// symlinks: the longest existing prefix is canonicalized (the OS resolves
/// symlinks and `..` truly) and any remaining components are applied to that
/// symlink-free base lexically — the one path the browser both renders and
/// reads, so a display spelling can never disagree with the read. `None` only
/// when nothing on the path exists.
fn resolve_browse_path(input: &str) -> Option<std::path::PathBuf> {
    let expanded = crate::bridge::command::expand_directory(input);
    if let Ok(canonical) = std::fs::canonicalize(&expanded) {
        return Some(canonical);
    }
    let ancestor = nearest_existing_dir(&expanded)?;
    let resolved = std::fs::canonicalize(&ancestor).ok()?;
    let tail = expanded.strip_prefix(&ancestor).ok()?;
    Some(crate::bridge::command::lexical_normalize(&resolved.join(tail)))
}

/// Resolve a browse target into the canonical absolute directory a Pending
/// Session may be declared in (ADR-0078); `None` when it is not a directory.
pub(crate) fn resolve_browse_dir(target: &str) -> Option<String> {
    let dir = resolve_browse_path(target)?;
    dir.is_dir().then(|| dir.to_string_lossy().to_string())
}

/// Walk up from `dir` to the first ancestor that exists as a directory.
fn nearest_existing_dir(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = Some(dir);
    while let Some(path) = current {
        if path.is_dir() {
            return Some(path.to_path_buf());
        }
        current = path.parent();
    }
    None
}

/// The browse card's dot-toggle rides as a JSON bool on regular buttons but as
/// the string `"true"`/`"false"` on the shared pager (whose callback fields are
/// strings only) — read both, defaulting to hidden.
pub(crate) fn parse_show_hidden(value: &serde_json::Value) -> bool {
    match value.get("show_hidden") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s == "true",
        _ => false,
    }
}

/// The viewer pager (`op: "page"`, #740) and the 「发为文件」 transfer carry the
/// home listing as `return_*`: strings on the pager (the shared pager holds
/// string callback fields only), JSON-typed on the transfer button — read
/// both, defaulting like the row's own fields do.
pub(crate) fn return_hidden(value: &serde_json::Value) -> bool {
    match value.get("return_show_hidden") {
        Some(serde_json::Value::Bool(hidden)) => *hidden,
        Some(serde_json::Value::String(flag)) => flag == "true",
        _ => false,
    }
}

/// The home listing's page from `return_page`: a number on the transfer
/// button, a decimal string on the pager — either way at least 1.
pub(crate) fn return_page_num(value: &serde_json::Value) -> usize {
    let page = match value.get("return_page") {
        Some(serde_json::Value::Number(page)) => page.as_u64().unwrap_or(1),
        Some(serde_json::Value::String(page)) => page.parse::<u64>().unwrap_or(1),
        _ => 1,
    };
    page.max(1) as usize
}

/// The browse listing a clicked viewer returns to, owned for a spawned task
/// (ADR-0052's statelessness rides the button; the task carries the same
/// state owned): `dir: None` when the viewer was opened with no listing (a
/// tool panel's 打开, #743), exactly like the borrowed [`BrowseView`] the
/// in-place open renders — which is what makes that viewer carry no
/// 「← 返回目录」.
pub(crate) struct OwnedReturnTo {
    pub(crate) dir: Option<String>,
    pub(crate) keyword: String,
    pub(crate) show_hidden: bool,
    pub(crate) page: usize,
}

/// The stateless work captured by a File Viewer open click.
pub(crate) struct ViewerOpen {
    pub(crate) thread_key: ThreadKey,
    pub(crate) path: String,
    pub(crate) session_dir: Option<String>,
    /// Tool Panel opens create a new viewer message; browse-row opens replace
    /// the listing card they came from.
    pub(crate) standalone: bool,
    pub(crate) card_message_id: String,
    pub(crate) return_to: OwnedReturnTo,
    pub(crate) page: usize,
    pub(crate) core: Arc<SharedCore>,
    pub(crate) in_flight_guard: ViewerActionGuard,
}

/// The stateless work captured by one File Viewer re-read click.
pub(crate) struct ViewerReread {
    pub(crate) thread_key: ThreadKey,
    pub(crate) path: String,
    pub(crate) session_dir: Option<String>,
    pub(crate) card_message_id: String,
    pub(crate) rich_message_id: Option<String>,
    pub(crate) sent_file_identity: Option<u64>,
    pub(crate) ambiguous_file_identity: Option<u64>,
    pub(crate) embedded_image_key: Option<String>,
    pub(crate) return_to: OwnedReturnTo,
    pub(crate) page: usize,
    /// The markdown view to rebuild at (#781): a re-read keeps the view the
    /// reader chose, so an explicit 「查看源码」 survives it.
    pub(crate) view: Option<crate::feishu::card::viewer::ViewerView>,
    pub(crate) in_flight_guard: ViewerActionGuard,
}

/// The bounded filesystem rebuild captured by one viewer pager callback.
pub(crate) struct ViewerPage {
    pub(crate) thread_key: ThreadKey,
    pub(crate) path: String,
    pub(crate) session_dir: Option<String>,
    pub(crate) card_message_id: String,
    pub(crate) return_to: OwnedReturnTo,
    pub(crate) page: usize,
    /// The markdown view to rebuild at (#781): `op: "page"` carries the reader's
    /// current one so a flip cannot silently change the view, `op: "view"` the
    /// target the toggle named.
    pub(crate) view: Option<crate::feishu::card::viewer::ViewerView>,
    pub(crate) lane: Option<crate::feishu::card::viewer::ViewerFileLane>,
    /// The content identity the clicked card displayed when the payload was
    /// built. A lane that never recorded its own bytes (the Rich Text
    /// delivery) still belongs to this snapshot, so the rebuild compares it
    /// too — otherwise a same-MIME/same-size edit would keep the stale lane.
    pub(crate) payload_identity: Option<u64>,
    pub(crate) in_flight_guard: ViewerActionGuard,
}

/// One stat/classification snapshot for a viewer open, completed off the card
/// callback path before any content lane or card replacement is decided.
enum ViewerOpenRead {
    Directory(std::path::PathBuf),
    File {
        path: std::path::PathBuf,
        classified: crate::bridge::viewer::Classified,
    },
}

#[cfg(test)]
pub(crate) type ViewerResolveTestHook = Arc<dyn Fn() + Send + Sync>;

#[cfg(test)]
static VIEWER_RESOLVE_TEST_HOOK: std::sync::OnceLock<
    std::sync::Mutex<Option<(std::path::PathBuf, ViewerResolveTestHook)>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn install_viewer_resolve_test_hook(path: std::path::PathBuf, hook: ViewerResolveTestHook) {
    *VIEWER_RESOLVE_TEST_HOOK
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((path, hook));
}

#[cfg(test)]
pub(crate) fn clear_viewer_resolve_test_hook() {
    *VIEWER_RESOLVE_TEST_HOOK
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

#[cfg(test)]
pub(crate) fn park_viewer_resolve_for_test(path: &std::path::Path) {
    let hook = VIEWER_RESOLVE_TEST_HOOK
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|(target, _)| target == path)
        .map(|(_, hook)| Arc::clone(hook));
    if let Some(hook) = hook {
        hook();
    }
}

struct ViewerOpenFailure<'a> {
    thread_key: &'a ThreadKey,
    resolved: &'a std::path::Path,
    path: &'a str,
    card_message_id: &'a str,
    standalone: bool,
    return_to: &'a OwnedReturnTo,
    line: &'a str,
    timeout_ms: u64,
}

/// Remember a completed or ambiguous viewer File Message attempt within the
/// same bounded, oldest-first window as the bridge's other process-local guards.
fn remember_viewer_file_transfer(
    sent: &mut indexmap::IndexMap<ViewerFileTransferKey, ViewerFileTransferStatus>,
    key: ViewerFileTransferKey,
    status: ViewerFileTransferStatus,
    cap: usize,
) {
    if sent.contains_key(&key) {
        return;
    }
    while sent.len() >= cap && crate::bridge::handles::evict_oldest_idle(sent, |_| true) {}
    sent.insert(key, status);
}

/// Why a Tool Panel's 「🌐 预览网页」 File Message did not land (#778): the
/// prepared snapshot carried no deliverable bytes, and the re-classified size
/// names the one cause the reader can act on — a file past Feishu's upload cap,
/// or one that became empty/unreadable between the panel and the click.
fn viewer_preview_failure_reason(size: u64) -> &'static str {
    if size > crate::feishu::file::MAX_FILE_BYTES {
        "文件超过 30 MB 上限"
    } else {
        "文件为空或不可读"
    }
}

/// The Toast a Turn Diff delivery's stored state answers a repeated click with.
fn turn_diff_state_toast(state: TurnDiffSendState) -> &'static str {
    match state {
        TurnDiffSendState::Sending => "正在生成完整 diff…",
        TurnDiffSendState::Sent => "已发送",
        TurnDiffSendState::OutcomeUnknown => crate::bridge::viewer::FILE_SEND_UNKNOWN_TEXT,
        TurnDiffSendState::Empty => TURN_DIFF_EMPTY_TEXT,
    }
}

/// Build the artifact's document from the neutral Turn Diff. `anchor_ms` is the
/// Turn's server time (#798's header wants title, turn time, stats, in that
/// order); `None` states it as unknown.
fn turn_diff_document(
    diff: &crate::backend::TurnDiff,
    click: &TurnDiffClick,
    anchor_ms: Option<i64>,
) -> crate::backend::DiffDocument {
    let turn = if click.from.is_empty() {
        "最新".to_string()
    } else {
        click.from.clone()
    };
    let mut subtitle = format!(
        "Session {} · turn {} · {}",
        click.session_id,
        turn,
        turn_diff_time(anchor_ms)
    );
    if !click.directory.is_empty() {
        subtitle.push_str(" · ");
        subtitle.push_str(&click.directory);
    }
    subtitle.push_str(" · ");
    subtitle.push_str(TURN_DIFF_SCOPE_NOTE);
    crate::backend::DiffDocument::from_turn_diff(diff.clone(), TURN_DIFF_TITLE, subtitle)
}

/// The Turn's anchor from a transcript: the user message `from` names, or —
/// when the payload carries no `from` (V2's "latest turn") — the newest user
/// message. The anchor pairs identity with the server time the header shows.
fn turn_diff_anchor(
    transcript: &crate::backend::SessionTranscript,
    from: &str,
) -> Option<crate::backend::TurnAnchor> {
    if from.is_empty() {
        transcript
            .newest_user()
            .and_then(crate::backend::TranscriptMessage::anchor)
    } else {
        transcript.anchor_of_user(from)
    }
}

/// The Turn time the artifact header shows: the card header's `MM-DD` date
/// idiom and the panel header's `HH:MM` clock (both existing formatters, so the
/// artifact cannot drift from the card), or 「时间未知」 when the anchor is
/// missing. The timestamp is the Turn's **server** time, like the card's date.
fn turn_diff_time(anchor_ms: Option<i64>) -> String {
    anchor_ms
        .and_then(|at| {
            Some(format!(
                "{} {}",
                crate::feishu::card::fmt_local_date(at)?,
                crate::feishu::card::fmt_local_time(at)?
            ))
        })
        .unwrap_or_else(|| TURN_DIFF_UNKNOWN_TIME.to_string())
}

/// The artifact's filename: ASCII, short and reproducible from the Turn's
/// identity (`session_id`, `from`), so the same click names the same file and a
/// client's extension-based preview always routes to the HTML renderer. FNV-1a
/// is spelled out because the name must not depend on a process's hash seed.
fn turn_diff_file_name(session_id: &str, from: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in session_id.bytes().chain([0]).chain(from.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("diff-{:012x}.html", hash & 0x0000_ffff_ffff_ffff)
}

/// RAII claim for one in-flight File Viewer action. A duplicate callback gets
/// acknowledged without spawning another sender; Drop releases even if the
/// spawned task is cancelled or unwinds.
pub(crate) struct ViewerActionGuard {
    in_flight: Arc<std::sync::Mutex<std::collections::HashSet<(String, String)>>>,
    key: (String, String),
}

impl Drop for ViewerActionGuard {
    fn drop(&mut self) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        in_flight.remove(&self.key);
    }
}

/// Blocking filesystem reads cannot be cancelled by their async timeout. Keep
/// only this many outstanding viewer reads; a timed-out closure holds its slot
/// until the OS call returns, and later taps fail fast instead of queueing more.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ViewerFsReadError {
    Busy(String),
    Timeout(String),
    Worker(String),
}

/// Acquire a bounded slot before submitting work, and hold it inside the
/// blocking closure so timing out its JoinHandle cannot free capacity early.
fn spawn_bounded_viewer_fs_read<T, F>(
    slots: &Arc<tokio::sync::Semaphore>,
    work: F,
) -> Result<tokio::task::JoinHandle<T>, tokio::sync::TryAcquireError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = Arc::clone(slots).try_acquire_owned()?;
    Ok(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    }))
}

/// Run a viewer filesystem operation in the shared bounded blocking pool. A
/// timed-out closure keeps its permit until the OS call actually returns.
pub(crate) async fn bounded_viewer_fs_read<T, F>(
    slots: Arc<tokio::sync::Semaphore>,
    timeout_ms: u64,
    busy_line: &str,
    timeout_line: &str,
    work: F,
) -> Result<T, ViewerFsReadError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let read = spawn_bounded_viewer_fs_read(&slots, work)
        .map_err(|_| ViewerFsReadError::Busy(busy_line.to_string()))?;
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), read).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(ViewerFsReadError::Worker(error.to_string())),
        Err(_) => Err(ViewerFsReadError::Timeout(timeout_line.to_string())),
    }
}

/// The result of the `/cat` path's bounded resolution, stat and classification.
pub(crate) enum CatPathRead {
    Directory(std::path::PathBuf),
    File {
        path: std::path::PathBuf,
        classified: crate::bridge::viewer::Classified,
    },
    Failure(String),
}

pub(crate) async fn read_cat_path_bounded(
    input: String,
    session_dir: String,
    timeout_ms: u64,
    slots: Arc<tokio::sync::Semaphore>,
) -> Result<CatPathRead, ViewerFsReadError> {
    bounded_viewer_fs_read(
        slots,
        timeout_ms,
        "⚠️ 文件读取繁忙，请稍后再试。",
        "⚠️ 打开超时，请稍后再试。",
        move || {
            let resolved = crate::bridge::viewer::resolve_cat_path(&input, &session_dir);
            match crate::bridge::viewer::open_cat_path(&resolved) {
                crate::bridge::viewer::CatOpen::Directory(dir) => CatPathRead::Directory(dir),
                crate::bridge::viewer::CatOpen::File { path, size } => {
                    match crate::bridge::viewer::classify(&path, size) {
                        Ok(classified) => CatPathRead::File { path, classified },
                        Err(failure) => CatPathRead::Failure(failure.line(&path)),
                    }
                }
                crate::bridge::viewer::CatOpen::Failed(failure) => {
                    CatPathRead::Failure(failure.line(&resolved))
                }
            }
        },
    )
    .await
}

/// A viewer payload's viewer page (#740): the pager's TARGET page, or the page
/// the 「发为文件」 transfer's card PATCH rebuilds — a missing page reads as
/// the first window, and the builder clamps an out-of-range one.
pub(crate) fn viewer_page(value: &serde_json::Value) -> usize {
    let page = match value.get("page") {
        Some(serde_json::Value::Number(page)) => page.as_u64().unwrap_or(1),
        Some(serde_json::Value::String(page)) => page.parse::<u64>().unwrap_or(1),
        _ => 1,
    };
    page.max(1) as usize
}

/// What a pager rebuild keeps of the lane its payload carried.
struct ViewerPageLane {
    /// The carried lane, when it still describes the file the rebuild read.
    lane: Option<crate::feishu::card::viewer::ViewerFileLane>,
    /// The fresh read no longer matches the carried lane, so the lane was
    /// dropped and the card states the change instead of a delivery outcome.
    changed: bool,
}

/// A pager payload can outlive the file snapshot it described. Keep its lane
/// only while the fresh read still matches its MIME, size and content identity;
/// otherwise drop it rather than carrying stale status or an image key across a
/// kind change — and report that the file changed.
///
/// Dropping it (rather than rebuilding it as `Undelivered`) matters because a
/// flip is navigation, not a re-read (#740): this pass delivers nothing, so
/// `未发送` would read as a failed send. The card says the file changed and
/// points at the one control that runs the real lane decision.
fn reconcile_viewer_page_lane(
    lane: Option<crate::feishu::card::viewer::ViewerFileLane>,
    payload_identity: Option<u64>,
    classified: &crate::bridge::viewer::Classified,
    path: &std::path::Path,
) -> ViewerPageLane {
    let Some(lane) = lane else {
        return ViewerPageLane {
            lane: None,
            changed: false,
        };
    };
    let fresh_identity = classified
        .bytes
        .as_deref()
        .or_else(|| classified.text.as_deref().map(str::as_bytes))
        .map(|bytes| {
            crate::bridge::handles::content_identity_hash(
                &classified.mime,
                &crate::feishu::card::command::file_display_name(path),
                bytes,
            )
        });
    // A lane that recorded no identity of its own (the Rich Text delivery) was
    // still built from the card's snapshot, which the payload carries: compare
    // that, so a same-MIME/same-size edit cannot keep a stale lane.
    let carried_identity = lane.content_identity.or(payload_identity);
    let identity_matches = match carried_identity {
        Some(identity) => fresh_identity == Some(identity),
        // Nothing identifies the old snapshot. Only an unreadable file (no
        // fresh identity either) may keep its lane without proof.
        None => fresh_identity.is_none(),
    };
    if lane.mime == classified.mime && lane.size == classified.size && identity_matches {
        return ViewerPageLane {
            lane: Some(lane),
            changed: false,
        };
    }
    ViewerPageLane {
        lane: None,
        changed: true,
    }
}

/// Declare (or replace) the conversation's Pending Session at `directory` — the
/// shared tail of the Recent Directories and browse cards' pick/选择 (Lazy
/// Session Creation, ADR-0041). Bounded by `deadline`, the callback's own
/// budget (ADR-0089): the mutation lands in memory at once and only the file
/// write is waited for.
///
/// `Some(toast)` means a pending was declared — the save may still have failed,
/// which the toast says. `None` means the store lock was still held at
/// `deadline` (or the deadline passed before the mutation), so nothing changed
/// and the caller refuses the click instead of refreshing the card.
pub(crate) async fn declare_pending_dir(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    directory: &str,
    deadline: tokio::time::Instant,
) -> Option<String> {
    match core
        .sessions_handle()
        .declare_pending_until(thread_key, directory, None, deadline)
        .await
    {
        Ok(Some(_)) => Some(format!("下一条消息将在目录 `{directory}` 创建会话")),
        Ok(None) => None,
        Err(e) => {
            // The pending IS declared in memory; only the save failed. Say
            // both halves instead of claiming a clean success.
            tracing::warn!("dir card pick: persist failed: {}", e);
            Some(format!(
                "⚠️ 已切换到 `{directory}`，但保存失败（重启后会丢失）：{e}"
            ))
        }
    }
}

/// #450: the prompt-path ending when the conversation's working directory is
/// gone (a removed worktree is the common case). The server boots the
/// directory's instance on every request and `realPath` fails on a missing
/// path, so dispatching would surface its raw error; this names the situation
/// and the one route out. `/dir` re-roots the conversation (its Pending
/// Session creates the next Session in the new directory), `/switch` adopts a
/// Session that already exists elsewhere.
fn gone_directory_text(directory: &str) -> String {
    format!(
        "⚠️ 工作目录已不存在或不可用：`{directory}`\n\
         可能已被删除（例如 worktree 被移除）。\n\
         用 `/dir <目录>` 切换目录（下一条消息会在新目录创建会话），\
         或用 `/switch` 换一个已有会话。"
    )
}

/// Whether `directory` is gone from disk, checked before a prompt dispatches
/// (#450). Only a definitively absent path — or one that is not a directory
/// anymore — counts: a permission or transient I/O error is left to the
/// server, because claiming "removed" for an unmounted or unreadable
/// directory would be wrong. One stat per prompt — cheap, and it runs before
/// any quoted-context or attachment fetch. The check is valid because cola and
/// its server share a host (ADR-0013's attach).
///
/// Test builds default the CALL off (see [`App::check_missing_directories`]):
/// the harness seeds Sessions at abstract paths ("/work"), so a real stat
/// would refuse every fixture. The tests that exercise the ending turn it on.
async fn directory_missing(directory: &str) -> bool {
    match tokio::fs::metadata(directory).await {
        Ok(metadata) => !metadata.is_dir(),
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ),
    }
}

/// The card a click answers with, built under the SAME deadline as its store
/// write (ADR-0089): a refresh can hit the backend for the session list, and a
/// card that misses the deadline would hold the click past Feishu's ack budget
/// — the answer is dropped and the user never sees the confirmation. `None`
/// means the card could not be built in time; the toast still answers, and the
/// next render poll repaints.
pub(crate) async fn card_before(
    deadline: tokio::time::Instant,
    build: impl std::future::Future<Output = serde_json::Value>,
) -> Option<serde_json::Value> {
    tokio::time::timeout_at(deadline, build).await.ok()
}

/// The whole `⚠️ 未找到技能：…` line's clip. The line is passed into the picker
/// card and the dedicated loaded-skill card OUTSIDE the fold budget, so a
/// dispatch naming many or very long unknown ids must still be bounded well
/// under Feishu's 30 KB card limit (AGENTS.md pitfall `card-budgets`).
const UNKNOWN_IDS_LINE_CHARS: usize = 500;

/// Resolve one Skill Token id against the read skill list (spec #662). The
/// **exact** id always wins; a case-insensitive match is only a fallback and is
/// accepted solely when exactly ONE registered id matches, so two ids differing
/// only by case (`Foo` / `foo` — the server's `Skill.ID` is an unconstrained
/// string) never make a typed spelling silently attach one of them.
fn resolve_skill<'a>(skills: &'a [SkillInfo], id: &str) -> Option<&'a SkillInfo> {
    if let Some(exact) = skills.iter().find(|skill| skill.id == id) {
        return Some(exact);
    }
    let mut candidates = skills.iter().filter(|skill| skill.id.eq_ignore_ascii_case(id));
    match (candidates.next(), candidates.next()) {
        (Some(entry), None) => Some(entry),
        _ => None,
    }
}

/// The `⚠️ 未找到技能：<id>、…` line a `/skill` dispatch with unresolvable ids
/// shows — on the picker card when NOTHING resolved, and on the dedicated
/// loaded-skill card when some did (Q19). `None` when every typed id resolved
/// (the bare `/skill` case included). Bounded here (each id and the whole line)
/// because the card renderers do not budget this leading line; each id reuses
/// the picker's own row-label clip so the two cannot drift.
fn unknown_skill_error(unknown: &[String]) -> Option<String> {
    if unknown.is_empty() {
        return None;
    }
    use crate::feishu::card::{budget::SurfaceBudget, clip};
    let label = SurfaceBudget::chars(crate::feishu::card::picker::SKILL_ROW_LABEL_CHARS);
    let named = unknown
        .iter()
        .map(|id| format!("`{}`", clip::clip(id).cut(label)))
        .collect::<Vec<_>>()
        .join("、");
    Some(clip::clip(&format!("⚠️ 未找到技能：{named}")).cut(SurfaceBudget::chars(UNKNOWN_IDS_LINE_CHARS)))
}

/// The user-facing refusal for a denied action (ADR-0035), shared by the
/// message gate and the card-click gate so their copy cannot drift.
fn denial_text(reason: &DenyReason) -> &'static str {
    match reason {
        DenyReason::Unclaimed => {
            "此 cola 尚未认领。请让机主查看启动日志中的认领码，并在私聊中发送 /claim <认领码>。"
        }
        DenyReason::NotHost => "此 cola 已设为私有，仅限机主使用。",
    }
}

impl App {
    /// The test-suite constructor: the production daemon builds the layout once
    /// and calls [`Self::with_layout`].
    #[cfg(test)]
    pub fn new(
        cfg: Config,
        client: Arc<dyn crate::backend::Backend>,
        feishu: Arc<dyn feishu::Platform>,
    ) -> anyhow::Result<Self> {
        let layout = crate::state::StateLayout::from_config(&cfg);
        Self::with_layout(cfg, layout, client, feishu)
    }

    /// The daemon's constructor: it builds the layout once and shares it with
    /// the boot-time server resolution. The test suite uses the layout-building
    /// `new` instead.
    pub fn with_layout(
        cfg: Config,
        layout: crate::state::StateLayout,
        client: Arc<dyn crate::backend::Backend>,
        feishu: Arc<dyn feishu::Platform>,
    ) -> anyhow::Result<Self> {
        let core = Arc::new(SharedCore::new(&cfg, layout, client, feishu)?);
        let access = tokio::sync::Mutex::new(Access::new(cfg.bridge.access_file.clone()));
        Ok(Self {
            self_weak: std::sync::OnceLock::new(),
            core,
            access,
            viewer_actions_in_flight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            viewer_file_transfers_sent: Arc::new(std::sync::Mutex::new(indexmap::IndexMap::new())),
            turn_diff_sends: Arc::new(std::sync::Mutex::new(indexmap::IndexMap::new())),
            #[cfg(test)]
            check_missing_directories: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            materialise_before_activation: Arc::new(tokio::sync::Mutex::new(None)),
            #[cfg(test)]
            materialise_activation_entered: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Claim one clicked viewer card until its spawned operation and final card
    /// update complete. The real card message id is supplied by Feishu callback
    /// context; chat scopes it defensively across targets.
    pub(crate) fn claim_viewer_action(
        &self,
        thread_key: &ThreadKey,
        message_id: &str,
    ) -> Option<ViewerActionGuard> {
        let key = (thread_key.chat_id.clone(), message_id.to_string());
        let mut in_flight = self
            .viewer_actions_in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !in_flight.insert(key.clone()) {
            return None;
        }
        Some(ViewerActionGuard {
            in_flight: Arc::clone(&self.viewer_actions_in_flight),
            key,
        })
    }

    /// Recover an `Arc<Self>` from the weak self-reference set by `run`. Used
    /// by the `EventSink` trait impl to hand a `&Arc<App>` to the inherent
    /// methods. None before `run` sets it (and after it returns).
    fn self_arc(&self) -> Option<Arc<Self>> {
        self.self_weak.get().and_then(|w| w.upgrade())
    }

    /// Announce a completed `/restart` or `/update` from the payload the dying
    /// process left at `command::restart_notify_path`. A command issued inside
    /// a Topic is announced INSIDE that topic by replying to its command
    /// message — the create API rejects `receive_id_type=thread_id`, and a
    /// reply to an in-topic message stays in the topic (ADR-0006). A lobby
    /// command gets the chat-level card. A failed topic reply falls back to
    /// the chat so the announcement is never silently lost.
    pub(crate) async fn announce_restart(&self, notify: &command::RestartNotify) {
        let chat_id = notify.chat_id.as_str();
        let (title, body, template) = match notify.kind {
            command::RestartKind::Update => {
                // Verify before repeating the claim (issue #551). The version in
                // the notify file is what the DYING process said it installed;
                // this process knows exactly what it is running — its own
                // compile-time version. An update that did not take effect is
                // reported as such instead of announced as a success: the
                // in-band analogue of the CLI's `supervisor_restart_took_effect`.
                let running = crate::update::current_version().to_string();
                match notify.version.as_deref() {
                    Some(announced) if announced == running => (
                        format!("✅ 已更新到 {announced}"),
                        format!("cola 已更新到 {announced} 并重启完成。"),
                        "green",
                    ),
                    Some(announced) => (
                        format!("⚠️ 更新未生效（当前 {running}）"),
                        format!(
                            "cola 期望更新到 {announced}，但重启后仍在运行 {running}。请检查安装路径后重试。"
                        ),
                        "orange",
                    ),
                    // A versionless payload predates the field and has nothing
                    // to check against: state the completion without a version
                    // rather than invent one.
                    None => (
                        "✅ 已更新完成".to_string(),
                        "cola 已更新并重启完成。".to_string(),
                        "green",
                    ),
                }
            }
            command::RestartKind::Restart => {
                ("♻️ 已重启".to_string(), "cola 已重启完成。".to_string(), "green")
            }
        };
        let card = crate::feishu::card::shell::card_shell(
            &title,
            template,
            vec![crate::feishu::card::element::CardElement::markdown(body).into_value()],
        );
        // A topic command carries its `thread_id`; the command message itself
        // lives inside the topic, so replying to it keeps the card there.
        let topic_reply_to = notify
            .thread_id
            .as_deref()
            .filter(|t| !t.is_empty() && *t != chat_id)
            .and(notify.message_id.as_deref());
        let sent = match topic_reply_to {
            Some(message_id) => match self.feishu.reply_card(message_id, &card).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    tracing::warn!("restart announce in topic failed: {e}; announcing in chat");
                    self.feishu.send_card("chat_id", chat_id, &card).await.map(|_| ())
                }
            },
            None => self.feishu.send_card("chat_id", chat_id, &card).await.map(|_| ()),
        };
        match sent {
            Ok(()) => tracing::info!("announced restart in chat {}", chat_id),
            Err(e) => tracing::warn!("restart announce failed: {}", e),
        }
    }

    /// Startup recovery for **Terminal Commands** (ADR-0086 decisions 5 and 10).
    ///
    /// A restart loses the run: the child may be an orphan (a hand-launched
    /// cola) or may have gone with the process (a supervised one), and cola
    /// cannot know which. So every record a previous process left `running` is
    /// reported — its card patched to 「已失去跟踪（可能仍在运行）」 and the
    /// record marked lost — and **no recorded process id is ever signalled**: a
    /// pid may have been reused since, so a lost run is reported, never adopted
    /// or killed. The record is kept, because the card's buttons must still find
    /// their run id current.
    ///
    /// The **Terminal Log** directory is swept by age in the same pass, never
    /// wholesale: a live orphan is still appending, so its file's mtime is
    /// fresh and it is left alone.
    ///
    /// Called once from [`Self::run`]. Tests call it directly on a fresh `App`
    /// over the same state directory, after installing their temp log root, so
    /// "restart" is one explicit step rather than a hidden constructor side
    /// effect — a sweep in `App::new` would run before a test could redirect
    /// the log root and would touch the real `~/.cola/terminal`.
    pub(crate) async fn sweep_terminal(&self) {
        let sweep = crate::bridge::terminal::sweep_records(&*self.feishu, &self.terminal_records).await;
        if sweep.lost > 0 {
            tracing::warn!(
                "terminal: marked {} run(s) lost after a restart (their process ids are never signalled)",
                sweep.lost
            );
        }
        if sweep.settled > 0 {
            tracing::info!(
                "terminal: told {} settled card(s) their run had ended (the previous process's last write was owed)",
                sweep.settled
            );
        }
        let Some(root) = self
            .terminal_log_root
            .get()
            .cloned()
            .or_else(|| self.layout.terminal_root())
        else {
            return;
        };
        // Retention (spec #821 user story 23), after the lost sweep has resolved
        // every `running` record: an inactive chat's latest run and its log
        // expire at `log_days` too, except a lost run whose group may still be
        // writing. The ordinary unreferenced-log sweep below stays as it is.
        let expiry =
            crate::bridge::terminal::expire_records(&root, self.terminal_log_days, &self.terminal_records);
        if expiry.expired > 0 {
            tracing::info!(
                "terminal: expired {} run record(s) and their logs older than {} day(s)",
                expiry.expired,
                self.terminal_log_days
            );
        }
        if expiry.kept_live > 0 {
            tracing::info!(
                "terminal: kept {} lost run(s) past {} day(s) — their process groups may still be alive",
                expiry.kept_live,
                self.terminal_log_days
            );
        }
        if expiry.kept_unprovable > 0 {
            tracing::info!(
                "terminal: kept {} lost run(s) past {} day(s) — no process id was recorded, so their \
                 groups cannot be proven gone",
                expiry.kept_unprovable,
                self.terminal_log_days
            );
        }
        let removed =
            crate::bridge::terminal::sweep_logs(&root, self.terminal_log_days, &self.terminal_records);
        if removed > 0 {
            tracing::info!(
                "terminal: swept {removed} log(s) older than {} day(s)",
                self.terminal_log_days
            );
        }
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        // Let the EventSink trait impl recover a &Arc<App> from &self.
        let _ = self.self_weak.set(Arc::downgrade(&self));
        // After a `/restart` or self-update, announce it where the command was
        // issued: inside the Topic when there was one, else in the chat.
        let notify_path = command::restart_notify_path(&self.layout);
        if let Ok(raw) = std::fs::read_to_string(&notify_path)
            && let Ok(notify) = serde_json::from_str::<command::RestartNotify>(&raw)
        {
            let _ = std::fs::remove_file(&notify_path);
            self.announce_restart(&notify).await;
        }

        // A restart loses the run, and the card says so (ADR-0086 decision 10):
        // report every record a previous process left running and sweep the
        // Terminal Log directory by age. Before the WS and poll loops, so the
        // reports land before any new click can re-claim a chat; best-effort
        // and bounded per card, so a dead network cannot wedge startup.
        self.sweep_terminal().await;

        // Instant Reminder startup orphan sweep (#249): clear the reminders a
        // previous process left at Feishu — the client gives the user no way
        // to cancel them. Runs before the WS and poll loops, so no new pin can
        // race it except from a turn that starts while it runs; a Chat/Topic
        // re-pinned meanwhile is left alone. Best-effort: entries whose clear
        // failed stay recorded for the next startup.
        self.core.reminder.clear_orphans(&self.feishu).await;

        // Silent startup self-update check (ADR-0015): log when a new version
        // exists; never sends a card. Fire-and-forget — a dead network or a
        // rate limit only costs a debug log line.
        {
            tokio::spawn(async move {
                // Resolve the check through the SAME install channel `/update`
                // would apply (issue #551) — a cargo install is compared
                // against crates.io, not GitHub Releases, so the logged
                // version is the one its `/update` acts on.
                match crate::update::available_for_running_channel().await {
                    Ok(Some(latest)) => {
                        if crate::version::is_dev_build() {
                            // A dev build is typically ahead of the last release
                            // (ADR-0027): warn instead of implying an upgrade.
                            tracing::warn!(
                                "本地 dev 构建（{}）发现新版本 {} —— 自更新会替换为发布版，可能比本地代码旧。",
                                crate::version::display_version(),
                                latest
                            );
                        } else {
                            tracing::info!(
                                "新版本 {} 可用（当前 {}）—— 发送 /update 更新。",
                                latest,
                                crate::update::current_version()
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!("update check failed: {e}"),
                }
            });
        }

        // Discover cola's own open_id so @mentions of the bot can be recognised
        // and stripped from prompt text (Feishu delivers them as `@_user_N`).
        // Owned by the ws module now (see `feishu::ws`).

        let ws = Arc::clone(&self);
        let ws_feishu = Arc::clone(&self.feishu);
        let ws_state = Arc::new(feishu::ws::WsState::new());
        // The poll loops each run on their own narrow bundle, built once here
        // (the coordinator is the construction point).
        let perm_handles = self.flow_handles();
        let question_handles = self.flow_handles();
        let external_handles = self.flow_handles();
        let poll_handles = self.flow_handles();
        let perm_app = Arc::clone(&self);
        let question_app = Arc::clone(&self);
        let external_app = Arc::clone(&self);
        let ws_task = tokio::spawn(async move {
            let sink: Arc<dyn crate::bridge::EventSink> = ws;
            if let Err(e) = feishu::ws::event_loop(&sink, &ws_feishu, &ws_state).await {
                tracing::error!("WS: {}", e);
            }
        });
        // Permissions are not delivered on the global SSE (typed PubSub only),
        // and a prompt can be blocked on an unanswered permission forever, so the
        // poller must run independently of any single prompt lifecycle.
        let perm_task = tokio::spawn(async move {
            if let Err(e) = perm_app.permission.poll_loop(&perm_handles).await {
                tracing::error!("Permission poller: {}", e);
            }
        });
        // Questions (the interactive `question` tool) work the same way: the AI
        // blocks until answered, the event never reaches the global SSE, so poll
        // and surface them as Feishu cards.
        let question_task = tokio::spawn(async move {
            if let Err(e) = question_app.question.poll_loop(&question_handles).await {
                tracing::error!("Question poller: {}", e);
            }
        });
        // Notify Feishu when someone posts a message from another shared-store
        // client (e.g. OpenChamber) while cola is idle on that session.
        let external_task = tokio::spawn(async move {
            if let Err(e) = external_app.external.poll_loop(&external_handles).await {
                tracing::error!("External message poller: {}", e);
            }
        });
        // The OpenCode server cola attaches to is managed by another tool that
        // can restart it (new pid/port/password). Re-detect a changed server so
        // cola reconnects instead of 502ing against the dead port forever.
        let reconnect_task = tokio::spawn(async move {
            if let Err(e) = crate::bridge::pollers::reconnect_poll_loop(&poll_handles).await {
                tracing::error!("Reconnect poller: {}", e);
            }
        });
        let shutdown_handles = self.flow_handles();
        tokio::select! {
            joined = async {
                tokio::try_join!(ws_task, perm_task, question_task, external_task, reconnect_task)
            } => {
                joined?;
            }
            // A stop request is a normal exit (issue #918): hand the runtime back
            // what this process holds before it ends. Bounded, so a wedged
            // runtime cannot hold `cola stop` or Ctrl-C.
            _ = Self::shutdown_signal() => {
                tracing::info!("stop requested; handing the runtime back before exit");
                shutdown_handles.detach_for_exit().await;
            }
        }
        Ok(())
    }

    /// Resolve on the process's first **stop request**: `SIGTERM` (systemd,
    /// `cola stop`) or `SIGINT` (Ctrl-C) on unix, Ctrl-C elsewhere. The default
    /// signal action would end the process with no code running, so this is the
    /// one hook a normal exit gets (issue #918). Registration failures degrade
    /// to "this signal cannot arrive", never to a panic on a thread the whole
    /// bridge depends on.
    async fn shutdown_signal() {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut terminate = signal(SignalKind::terminate()).ok();
            let mut interrupt = signal(SignalKind::interrupt()).ok();
            tokio::select! {
                _ = async {
                    match terminate.as_mut() {
                        Some(stream) => drop(stream.recv().await),
                        None => std::future::pending::<()>().await,
                    }
                } => {}
                _ = async {
                    match interrupt.as_mut() {
                        Some(stream) => drop(stream.recv().await),
                        None => std::future::pending::<()>().await,
                    }
                } => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }

    pub async fn handle_message(self: &Arc<Self>, msg: crate::bridge::IncomingMessage) {
        if self.gate_message(&msg).await {
            return;
        }
        let kind = ConversationKind::classify(&msg.chat_type, msg.thread_id.as_deref());
        let thread_key = kind.thread_key(&msg.chat_id, msg.thread_id.as_deref());
        // A message whose first character is `!` is a **Terminal Command**, not
        // prose and not a **Command** (ADR-0086 §1). This runs BEFORE
        // `parse_command`: that parser scans a plain message for `#<id>` Skill
        // tokens, so `!ls #caveman` must run the literal command rather than
        // load a Skill. The `!` form takes no flags and no timeout, so the run
        // uses the default cap. Every other shape (`!`, `! `, a mid-message
        // `!`) falls through to the unchanged prompt path.
        if let Some(command) = crate::bridge::terminal::parse_bang(&msg.text) {
            let spec = command::RunSpec {
                command,
                timeout: command::RUN_DEFAULT_TIMEOUT,
            };
            if let Err(e) = crate::bridge::terminal::execute(
                &self.command_handles(),
                &thread_key,
                crate::bridge::terminal::TerminalRequest {
                    message_id: &msg.message_id,
                    requester_open_id: msg.requester_open_id.as_deref(),
                    is_group: msg.chat_type != "p2p",
                },
                Some(spec),
            )
            .await
            {
                tracing::error!("Terminal: {}", e);
            }
            return;
        }
        if let Some(cmd) = command::parse_command(&msg.text) {
            // Unrecognized `/command`s are forwarded as prompts. Command
            // dispatch lives on the shared core, but the prompt pipeline is the
            // coordinator's — so Forward is routed here, before the command flow.
            if let command::Command::Forward(text) = cmd {
                // The forward is the user's own message, so its downloaded
                // attachments ride along (an unrecognized `/command` with a
                // file attached must not silently lose the file).
                let forward = crate::bridge::IncomingMessage {
                    message_id: msg.message_id,
                    chat_id: msg.chat_id,
                    chat_type: msg.chat_type,
                    thread_id: msg.thread_id,
                    parent_id: None,
                    text,
                    images: msg.images,
                    files: msg.files,
                    omitted: msg.omitted,
                    requester_open_id: None,
                };
                if let Err(e) = self.handle_prompt(thread_key, forward, kind, Vec::new()).await {
                    tracing::error!("Prompt: {}", e);
                }
                return;
            }
            // `/skill <id> …` and `#<id> …` load skills into the prompt (spec
            // #652, tickets #654/#656; `#` form spec #662). Like Forward, this is
            // routed here rather than through the command dispatcher: the skills
            // ride the prompt axis, and the whole original message text is the
            // prompt. Dispatch resolves each typed id against the skill list —
            // see [`Self::handle_skill_command`].
            if let command::Command::Skill(invocation) = cmd {
                self.handle_skill_command(thread_key, msg, invocation).await;
                return;
            }
            if let Err(e) = command::handle_command(
                &self.command_handles(),
                cmd,
                thread_key,
                &msg.message_id,
                kind,
                msg.requester_open_id.as_deref(),
                msg.chat_type != "p2p",
            )
            .await
            {
                tracing::error!("Cmd: {}", e);
            }
            return;
        }
        if let Err(e) = self.handle_prompt(thread_key, msg, kind, Vec::new()).await {
            tracing::error!("Prompt: {}", e);
        }
    }

    /// Dispatch a skill invocation (spec #652, tickets #654/#656; the `#<id>`
    /// form, spec #662): the Bridge reads the skill list ONCE — scoped to the
    /// Chat/Topic's current project directory, the same location the eventual
    /// prompt runs in — resolves each typed id to its canonical `{ id, name }`
    /// list entry (so the attached skill is not just the raw token — V1's text
    /// fallback then names the skill correctly), and submits the whole original
    /// message text verbatim with the resolved attachment. Ids are deduped by
    /// id, first-occurrence order preserved. The read is generation-neutral, so
    /// the invocation behaves identically on V1 and V2.
    ///
    /// A resolved dispatch also replies the dedicated loaded-skill card under
    /// the user's message ([`crate::feishu::card::command::send_loaded_skill_card`]),
    /// one `🧩 已加载技能：<name>` fold per distinct skill — the ONLY place a
    /// skill fold renders (the live Turn card, the Session Card tail and the
    /// External Message preview are text-only; ADR-0077's acceptance
    /// reversal). A dispatch that resolves SOME ids but not others still
    /// submits the resolved ones and leads the card with a `⚠️ 未找到技能：…`
    /// line naming the rest, so nothing is dropped silently (Q19).
    ///
    /// A dispatch that resolves NO id branches on the source
    /// ([`command::SkillInvocation::picker_on_empty`]): `/skill` — the list-entry
    /// command — answers the picker card from the list just read (a bare
    /// `/skill` carries no id; an unknown one leads with the error line). A
    /// `#<id>` message instead submits as a plain prompt with no card: its
    /// unresolvable tokens are ordinary prose (`#644`, a heading) and the message
    /// text is already the prompt. A dispatch that resolves at least one id keeps
    /// those and submits — the ids that resolve ride the prompt, exactly as the
    /// spec's "keeps the ids that resolve" states. Resolution is exact-first,
    /// with a case-insensitive fallback only when it is unambiguous (see
    /// [`resolve_skill`]).
    ///
    /// Unlike every other command, this one runs Lazy Start first
    /// ([`Self::ensure_server_for`]): the skill list comes from the server, so a
    /// serverless cola must attach or spawn before the read — otherwise every id
    /// reads as unknown and the picker is empty. See the comment in the body.
    pub(crate) async fn handle_skill_command(
        self: &Arc<Self>,
        thread_key: ThreadKey,
        msg: crate::bridge::IncomingMessage,
        invocation: command::SkillInvocation,
    ) {
        let command::SkillInvocation { ids, picker_on_empty } = invocation;
        // `/skill` is the prompt-like exception to "commands never trigger Lazy
        // Start": both forms depend on the server's skill list — the dispatch
        // resolves typed ids against it (then submits a prompt), and the bare
        // form's picker lists it — so ensure the server BEFORE the list read.
        // A serverless cola otherwise reads an empty list: a typed id reads as
        // unknown, the picker shows nothing, and the path never reaches the
        // prompt that would have started the server. The user gets the same
        // NoServer / start-failure message a prompt does.
        if !self.ensure_server_for(&msg.message_id).await {
            return;
        }
        let kind = ConversationKind::classify(&msg.chat_type, msg.thread_id.as_deref());
        let handles = self.command_handles();
        // Read from the Chat/Topic's own project directory (the location the
        // prompt will run in), so resolution and injection cannot disagree; an
        // empty/unknown directory keeps the server's default location.
        let directory = handles.current_project_directory(&thread_key).await;
        let skills = self
            .client
            .list_skills((!directory.is_empty()).then_some(directory.as_str()))
            .await;
        // The distinct resolved skills, in first-occurrence order: ONE
        // collection holds both the prompt attachment and the dedicated
        // loaded-skill card's folds (spec #652, ticket #655), so the two views
        // cannot get out of sync. The card folds need the neutral `SkillInfo`
        // (for the list read's own `content`); the prompt skills are derived
        // from it at submit time.
        let mut selected: Vec<SkillInfo> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        // Dedupe by id, preserving first-occurrence order: the server also
        // dedupes injection within one prompt, but cola must not attach (or
        // fold) the same skill twice for `/skill a /skill a`.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for id in &ids {
            // Exact id first, then an unambiguous case-insensitive fallback —
            // see [`resolve_skill`]. The matched list entry (never the typed
            // token) is what rides the prompt, so the server's own exact-match
            // lookup still sees the canonical id.
            match resolve_skill(&skills, id) {
                Some(entry) => {
                    if seen.insert(entry.id.clone()) {
                        selected.push(entry.clone());
                    }
                }
                None => unknown.push(id.clone()),
            }
        }
        // The unknown-id error line, shared by the two card paths: a bare
        // `/skill` supplies none, a dispatch with unresolvable ids leads with
        // one — on the picker when NOTHING resolved, and on the dedicated
        // loaded-skill card when SOME did (Q19: partial submit, the unknowns
        // surfaced, nothing dropped silently). Bounded; see
        // [`unknown_skill_error`].
        let unknown_error = unknown_skill_error(&unknown);
        // No id resolved.
        if selected.is_empty() {
            // `/skill` — the list-entry command. A bare `/skill` or ids that all
            // fail to resolve answer the picker card from the list just read,
            // leading with the error line when ids were given. No prompt.
            if picker_on_empty {
                let _ = crate::feishu::card::command::send_skill_card(
                    &handles,
                    &thread_key,
                    &msg.chat_type,
                    &msg.message_id,
                    &skills,
                    unknown_error.as_deref(),
                )
                .await;
                return;
            }
            // A `#<id>` dispatch whose ids all fail to resolve: the tokens are
            // ordinary prose (`#644`, a heading), so the message is just a plain
            // prompt — no card, no attachment. The whole text is submitted
            // verbatim, which is where those tokens already live.
            if let Err(e) = self.handle_prompt(thread_key, msg, kind, Vec::new()).await {
                tracing::error!("Prompt: {}", e);
            }
            return;
        }
        // A mixed dispatch submits the ids that resolved AND names the unknown
        // ones on the dedicated loaded-skill card (Q19). The card replies under
        // the user's message — the same feedback for a typed command and a
        // picker-row tap. A failed card is logged (and degrades to text inside
        // the helper); it never blocks the submit, which is the command's point.
        if let Err(e) = crate::feishu::card::command::send_loaded_skill_card(
            &handles,
            &msg.message_id,
            &selected,
            unknown_error.as_deref(),
        )
        .await
        {
            tracing::warn!("loaded-skill card: {e}");
        }
        // The prompt attachment derived from the one selected collection.
        let resolved: Vec<PromptSkill> = selected
            .iter()
            .map(|skill| PromptSkill {
                id: skill.id.clone(),
                name: skill.name.clone(),
            })
            .collect();
        if let Err(e) = self.handle_prompt(thread_key, msg, kind, resolved).await {
            tracing::error!("Prompt: {}", e);
        }
    }

    /// The Access gate (ADR-0035): authorize the message's Principal before
    /// any command, session, or prompt handling. Returns true when the message
    /// was fully handled here — a Claim, an acknowledgement, or a refusal.
    async fn gate_message(&self, msg: &crate::bridge::IncomingMessage) -> bool {
        let decision = {
            let access = self.access.lock().await;
            access.decide(
                msg.requester_open_id.as_deref(),
                msg.chat_type == "p2p",
                &msg.text,
            )
        };
        match decision {
            Decision::Allow => false,
            Decision::Claim => {
                // `decide` returns Claim only for a resolved Principal.
                let principal = msg.requester_open_id.clone().unwrap_or_default();
                let result = self.access.lock().await.claim(&principal);
                let text = match result {
                    Ok(true) => {
                        tracing::info!("claimed by {principal}");
                        "✅ 认领成功，此 cola 现在仅你可用。"
                    }
                    Ok(false) => "此 cola 已被认领。",
                    Err(e) => {
                        tracing::error!("claim failed: {e}");
                        "认领失败，请查看日志后重试。"
                    }
                };
                let _ = self.feishu.reply_text(&msg.message_id, text).await;
                true
            }
            Decision::AlreadyClaimed => {
                let _ = self
                    .feishu
                    .reply_text(&msg.message_id, "此 cola 已被认领。")
                    .await;
                true
            }
            Decision::Deny(reason) => {
                tracing::info!(
                    "refused {} message from {:?} ({reason:?})",
                    msg.chat_type,
                    msg.requester_open_id
                );
                let _ = self
                    .feishu
                    .reply_text(&msg.message_id, denial_text(&reason))
                    .await;
                true
            }
        }
    }

    /// The Access gate for card clicks (ADR-0035): only the Host may act on a
    /// card button. Returns the refusal result when the click must not act;
    /// `None` means it may proceed. Unlike a message, a click carries no
    /// command text and no chat, so it can never Claim — for an unclaimed cola
    /// the outcome is the same refusal a message would get.
    async fn gate_card_action(&self, value: &serde_json::Value) -> Option<CardActionResult> {
        let principal = value.get("operator_open_id").and_then(|v| v.as_str());
        let decision = {
            let access = self.access.lock().await;
            access.decide(principal, false, "")
        };
        let reason = match decision {
            Decision::Allow => return None,
            Decision::Deny(reason) => reason,
            // Unreachable by construction (no text → no Claim), but a click
            // must fail closed if the gate ever returns a claim outcome.
            Decision::Claim | Decision::AlreadyClaimed => {
                tracing::warn!("card action decided a claim — refusing");
                DenyReason::NotHost
            }
        };
        tracing::info!("refused card action from {principal:?} ({reason:?})");
        Some(CardActionResult {
            card: None,
            toast: Some(denial_text(&reason).to_string()),
        })
    }

    /// Route a prompt by cola's OWN live ownership (ADR-0062, ADR-0070, ADR-0109):
    /// the one [`crate::bridge::turn::CardOwnership`] read over the waits state
    /// and the card map. Only a **held guard** takes the Supplement path: it is
    /// the only claim that PROVES an in-process renderer (a Turn, or the follow
    /// that inherited it), and the split needs a live card that a writer will
    /// continue. A card-only chain is a display state, not a writer — a failed
    /// Supplement can leave a non-terminal card with no run behind it (#1056) —
    /// so the message starts a new Turn, whose card owns and settles it. A
    /// pending-inbound claim owns no chain and routes a new Turn too.
    ///
    /// With no held guard, the Backend's status read only picks the new Turn's
    /// opening line ([`crate::bridge::turn`]'s `MERGE_OPENING`). The new Turn is
    /// safe either way — its V2 prompt carries the merge delivery, so a
    /// genuinely live run merges the message exactly as a Supplement would,
    /// while a stale read gets a card that owns and settles the message.
    ///
    /// This function DECIDES; it does not admit (ADR-0113). The status read and
    /// the orphan collect both await, so a renderer can lease the guard after
    /// any read here — which is why the caller claims the Turn's guard with
    /// [`crate::bridge::turn::Turn::try_admit`] and re-routes a refused claim as
    /// a Supplement. Re-reading here would only move the unguarded window, not
    /// close it.
    ///
    /// One INFO line records every decision (ADR-0048): the session, the
    /// ownership verdict and the advisory status — never chat content.
    async fn route_prompt(&self, session_id: &str) -> PromptRoute {
        let ownership =
            crate::bridge::turn::CardOwnership::read(&self.cards_handle(), &self.waits_handle(), session_id)
                .await;
        // A held guard is the ONLY proof of an in-process renderer: the message
        // merges into it, and the Backend's read could only add a stale second
        // opinion. A card-only claim is a display state, not a writer — the
        // failed Supplement of #1056 left exactly such a card non-terminal with
        // no run behind it — so it is never a Supplement routing key (ADR-0109).
        if ownership.holds_guard() {
            tracing::info!("prompt routing: session={session_id} ownership=guard route=supplement");
            return PromptRoute::Supplement;
        }
        let directory = self.sessions.lock().await.directory_for_session(session_id);
        let status = read_session_status(
            "prompt advisory status",
            self.turn_config().follow_read_timeout_ms(),
            &self.client,
            session_id,
            directory.as_deref(),
        )
        .await;
        // One match yields both the verdict's name and the boolean the card's
        // opening line reads: the same `is_live` predicate cannot be computed
        // twice.
        let (advisory_live, advisory) = match status {
            Some(status) if status.is_live() => (true, "live"),
            Some(_) => (false, "idle"),
            None => (false, "unreadable"),
        };
        let label = ownership.routing_label().unwrap_or("none");
        // Reap the orphaned card BEFORE this message is classified (#1056): a
        // card-only chain is render-owned on screen but no in-process renderer
        // drives it (every renderer leases the guard), so the recorded card is
        // exactly the orphan a failed Supplement leaves. Collecting it here
        // keeps the stale card from lingering under the new Turn's card. A
        // renderer that leases the guard while the collect awaits does not
        // change what is collected (the card was the orphan at classification
        // time); the caller's admission then re-routes the message as a
        // Supplement, so the message is not lost to the Turn's busy refusal.
        if ownership.card_class() == crate::bridge::turn::CardClass::RenderOwned
            && let Some(card_id) = ownership.card_message_id().map(str::to_string)
            && self.cards_handle().chains.has_record(session_id)
        {
            crate::bridge::chain::collect_orphan(&self.cards_handle(), session_id, &card_id).await;
        }
        // The card-only chain (a render-owned card holding no guard: the
        // external snapshot follow, a Wake continuation) starts a new Turn like
        // any other unowned session. Its prompt still carries the merge
        // delivery, so a genuinely live run merges the message exactly as a
        // Supplement would — and the new Turn, not a card state, owns and
        // settles the message (ADR-0062's safe shape, narrowed by ADR-0109).
        tracing::info!(
            "prompt routing: session={session_id} ownership={label} advisory={advisory} route=turn"
        );
        PromptRoute::NewTurn { advisory_live }
    }

    /// Lazy Start (ADR-0013) for a path about to need the server: attach to an
    /// existing default-store server, or spawn an Owned Server when none exists
    /// (unless `start_server = "never"`). On failure the user is told — the same
    /// NoServer / start-failure message every prompt shows — and the caller must
    /// stop. Returns whether a server is ready. Shared by [`Self::handle_prompt`]
    /// and [`Self::handle_skill_command`], so `/skill` reports the serverless
    /// state exactly like a prompt.
    async fn ensure_server_for(&self, message_id: &str) -> bool {
        match crate::bridge::pollers::ensure_server(&self.flow_handles()).await {
            Ok(true) => true,
            Ok(false) => {
                let backend = &self.flow_handles().backend;
                let _ = self
                    .feishu
                    .reply_text(
                        message_id,
                        &format!(
                            "⚠️ 当前没有可用的 {} server，且 `start_server = \"never\"`。\n{}",
                            backend.client_name(),
                            backend.start_hint()
                        ),
                    )
                    .await;
                false
            }
            Err(e) => {
                tracing::warn!("ensure server failed: {}", e);
                let backend = &self.flow_handles().backend;
                let _ = self
                    .feishu
                    .reply_text(
                        message_id,
                        &format!("⚠️ 启动 {} 失败：{e}", backend.client_name()),
                    )
                    .await;
                false
            }
        }
    }

    pub(crate) async fn handle_prompt(
        self: &Arc<Self>,
        thread_key: ThreadKey,
        msg: crate::bridge::IncomingMessage,
        kind: ConversationKind,
        skills: Vec<PromptSkill>,
    ) -> crate::error::Result<()> {
        self.submit_prompt(thread_key, msg, kind, skills)
            .await
            .map(|_| ())
    }

    /// The pipeline itself: every prompt — a typed message, a Skill dispatch, a
    /// Forward, and the Terminal Card's 「问 AI」 — goes through here, so none
    /// of them can grow a second routing rule. It returns what it did with the
    /// message for the one caller that reports the outcome on a card of its own;
    /// `None` means nothing was submitted (no server), and the refusal was
    /// already spoken.
    async fn submit_prompt(
        self: &Arc<Self>,
        thread_key: ThreadKey,
        msg: crate::bridge::IncomingMessage,
        kind: ConversationKind,
        skills: Vec<PromptSkill>,
    ) -> crate::error::Result<Option<PromptSubmission>> {
        let crate::bridge::IncomingMessage {
            message_id,
            chat_type,
            parent_id,
            text,
            images,
            files,
            omitted,
            requester_open_id,
            ..
        } = msg;
        // Lazy Start (ADR-0013): a prompt is the moment a server is needed.
        // Attach to an existing default-store server, or spawn an Owned Server
        // when none exists (unless `start_server = "never"`). Serverless means
        // the bot has no OpenCode to answer with — tell the user instead of
        // failing silently inside the prompt flow. Commands never trigger this
        // (so `/restart-opencode` still reports NoServer/NotOwned properly),
        // with `/skill` the one prompt-like exception (see
        // [`Self::handle_skill_command`]).
        if !self.ensure_server_for(&message_id).await {
            return Ok(None);
        }
        // #450: the conversation's next Session would start in this directory
        // — a Pending's when one is declared, else the Active Session's. A
        // directory deleted underneath it (a removed worktree) can never run,
        // and the server would answer with its own error, so end here before
        // the attachment work and the dispatch. Only an existing Pending or
        // mapping is checked: a fresh conversation has no directory of its own
        // yet, and its create failure stays the server's to report. Nothing is
        // claimed or created; the mapping and any Pending stay intact so a
        // `/dir` re-root loses nothing.
        let target_directory = {
            let store = self.sessions.lock().await;
            store.current_directory(&thread_key)
        };
        // Test builds default the check off: the harness seeds Sessions at
        // abstract paths no disk carries, so the two ending tests opt in.
        #[cfg(test)]
        let check_missing = self
            .check_missing_directories
            .load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(test))]
        let check_missing = true;
        if check_missing
            && let Some(target_directory) = target_directory
            && directory_missing(&target_directory).await
        {
            self.feishu
                .reply_text(&message_id, &gone_directory_text(&target_directory))
                .await?;
            return Ok(None);
        }
        let is_group = chat_type == "group";
        let mut text = text;
        // The quoted parent's attachments, filled by the two-phase fetch below
        // and prepended to the message's own (ADR-0076's inbound amendment).
        let mut quoted_images = Vec::new();
        let mut quoted_files = Vec::new();
        let mut quoted_omitted = Vec::new();

        // Quoted Context: when this message replies to another, fetch the
        // parent and prepend it so the model sees what the reply answers —
        // including parents missing from session history (lobby-session switch,
        // compaction). Depth-1 with a short timeout; any failure degrades to
        // text-only (the pre-change behavior).
        //
        // ADR-0023: never inject the topic's own creation messages — the
        // thread root (the user's `/topic` command, `topic_root`) and the seed
        // card (`topic_anchor`). Feishu reports a plain topic reply's parent_id
        // pointing at the root, so without this guard every prompt in a
        // cola-created topic would carry boilerplate. A topic whose session is
        // still pending (`get_active` is `None`, ADR-0041) carries the same
        // anchors on its PendingEntry, so check there too. Manually-created
        // topics leave both `None`, so their user-typed root still injects.
        let parent_is_topic_creation = match parent_id.as_deref() {
            Some(pid) => {
                let store = self.sessions.lock().await;
                match store.pending_for(&thread_key) {
                    Some(p) => p.topic_root.as_deref() == Some(pid) || p.topic_anchor.as_deref() == Some(pid),
                    None => store.get_active(&thread_key).is_some_and(|e| {
                        e.topic_root.as_deref() == Some(pid) || e.topic_anchor.as_deref() == Some(pid)
                    }),
                }
            }
            None => false,
        };
        if let Some(pid) = parent_id.as_deref().filter(|_| !parent_is_topic_creation) {
            // Phase 1, on its own 1.5s budget: the parent's TEXT (and the
            // attachment refs). The text is injected the moment it arrives.
            let fetch = feishu::message::quoted_source(&self.feishu, pid);
            let fetch = tokio::time::timeout(std::time::Duration::from_millis(1500), fetch);
            match fetch.await {
                Ok(Ok(source)) => {
                    if !source.text.is_empty() {
                        let capped: String = source.text.chars().take(2000).collect();
                        text = format!("[引用消息]:\n{}\n\n{}", capped, text);
                    }
                    // Phase 2, best-effort and outside the 1.5s budget: the
                    // parent's attachments, each within its own platform timeout
                    // (10s image / 30s file). A slow download now loses only the
                    // attachment, never the quoted text.
                    let quoted = feishu::message::download_attachments(
                        &self.feishu,
                        pid,
                        &source.image_keys,
                        &source.file_refs,
                    )
                    .await;
                    quoted_images = quoted.images;
                    quoted_files = quoted.files;
                    quoted_omitted = quoted.omitted;
                }
                Ok(Err(e)) => tracing::debug!("quoted context fetch failed for {}: {}", pid, e),
                Err(_) => tracing::debug!("quoted context fetch timed out for {}", pid),
            }
        }

        // Prompt Attachments (ADR-0076's inbound amendment). The quoted
        // parent's come first, then this message's own. Every attachment that
        // cannot be attached is reported — the quoted parent's too, since "a
        // file was not read" is exactly what the user must be able to tell.
        let (quoted_parts, quoted_omissions) = prompt_files(&quoted_images, &quoted_files);
        let (own_parts, own_omissions) = prompt_files(&images, &files);
        let mut attachments = quoted_parts;
        attachments.extend(own_parts);
        // A ref the platform could not resolve (a folder, a failed download)
        // joins the classification omissions, so every one is reported. The
        // message's own come before the quoted parent's, so an own file
        // message's placeholder is still annotated in place.
        let mut omissions: Vec<Omission> = omitted
            .into_iter()
            .map(|OmittedAttachment { name, reason }| Omission { name, reason })
            .collect();
        omissions.extend(own_omissions);
        omissions.extend(
            quoted_omitted
                .into_iter()
                .map(|OmittedAttachment { name, reason }| Omission { name, reason }),
        );
        omissions.extend(quoted_omissions);
        join_omissions(&mut text, &omissions);
        if !omissions.is_empty() {
            let _ = self
                .feishu
                .reply_text(&message_id, &omission_warning(&omissions))
                .await;
        }

        let (session_id, created) = self
            .get_or_create_session(&thread_key, &text, &message_id)
            .await?;
        // #424: from here until this message is admitted (`Turn::start`) or
        // merged (the Supplement path below), a Session Sync Fresh
        // continuation must not post: the message is not in the transcript
        // yet, so no read can see it.
        self.waits_handle().note_inbound(&session_id).await;

        // First message on a group's top level created a lobby session: reply
        // once with guidance so the user knows each topic isolates a session.
        if created && kind == ConversationKind::GroupLobby {
            self.feishu.reply_text(&message_id, GROUP_LOBBY_GUIDANCE).await?;
        }

        let subtitle = crate::bridge::turn::Turn::session_subtitle(
            &self.sessions_handle(),
            &self.client,
            &thread_key,
            &text,
        )
        .await;

        // Prompt routing (ADR-0062, ADR-0109): cola's own live ownership decides
        // a Supplement, never the Backend's read. The message's facts are owned
        // from here, so a route that changes during admission (ADR-0113)
        // submits the SAME message — never a copy, never twice.
        let message = PromptMessage {
            context: PromptContext {
                session_id,
                thread_key,
                text,
                message_id,
                subtitle,
                is_retry: false,
                requester_open_id,
                is_group,
                cola_message_id: None,
                files: attachments,
                skills,
                advisory_live: false,
            },
        };
        match self.route_prompt(&message.context.session_id).await {
            PromptRoute::NewTurn { advisory_live } => self.admit_turn(message, advisory_live).await,
            PromptRoute::Supplement => self.submit_supplement(message).await,
        }
    }

    /// Route the message as a Turn: claim the guard first — the claim IS the
    /// admission (#1086, ADR-0113), taken before the context is consumed — and
    /// run it. A refused claim re-routes the SAME message as the Supplement it
    /// is, instead of letting `Turn::start` refuse it busy after the router
    /// chose a Turn for it; a renderer can lease the guard after the route's
    /// awaits (the advisory status read, the orphan collect), so this is the
    /// only place the two can agree.
    async fn admit_turn(
        self: &Arc<Self>,
        message: PromptMessage,
        advisory_live: bool,
    ) -> crate::error::Result<Option<PromptSubmission>> {
        let Some(admission) =
            crate::bridge::turn::Turn::try_admit(&self.waits_handle(), &message.context.session_id).await
        else {
            tracing::info!(
                "prompt routing: session={} ownership=guard route=supplement (admission handshake)",
                message.context.session_id
            );
            return self.submit_supplement(message).await;
        };
        let outcome = self
            .run_prompt_admitted(message.into_context(advisory_live), admission)
            .await?;
        Ok(Some(match outcome {
            crate::bridge::turn::PromptOutcome::Accepted => PromptSubmission::NewTurn,
            crate::bridge::turn::PromptOutcome::NotAccepted => PromptSubmission::NotAccepted,
        }))
    }

    /// Submit `message` as a Supplement (ADR-0043, owned routing per ADR-0062):
    /// cola holds the Session's live Execution, so the message does not start a
    /// competing Turn (it would overwrite the running turn's accumulator and
    /// race on the same card). The message goes to the Backend without starting
    /// a Turn — OpenCode persists it and the running loop picks it up at the
    /// next tool boundary, merging it into the current turn (original message
    /// preserved; model sees full history). This is what lets the user append
    /// context mid-turn without `/stop` and without interrupting a tool call.
    /// The message landed below the live card, so the Card Chain splits at it
    /// (ADR-0043) — the split is requested even when the send failed, never for
    /// commands (they are dispatched above this path).
    ///
    /// This is also one half of the route/admission handshake (ADR-0113): a
    /// merge-only steer may answer `RefusedNoLiveRun`, which proves **nothing
    /// was submitted**. The message is then not lost to a notice — the guard is
    /// held by a renderer that cannot drive a run (a retiring one, or a
    /// restart-armed Wake continuation), so this waits for its window to close
    /// and becomes the Turn it could not merge into (see
    /// [`Self::settle_refused_supplement`]). Every path submits exactly once.
    async fn submit_supplement(
        self: &Arc<Self>,
        message: PromptMessage,
    ) -> crate::error::Result<Option<PromptSubmission>> {
        let PromptContext {
            session_id,
            message_id,
            ..
        } = &message.context;
        // Each supplement is its own logical user message: fresh cola-authored
        // id (ADR-0026), never reused. The id is minted ONCE and rides every
        // attempt of this handshake, so the last-chance retry is still the same
        // logical message — and if an adapter ever broke the typed-refusal
        // contract, the server's id dedupe would still cover the re-send.
        let cola_msg_id = crate::backend::parsing::cola_message_id();
        let answer = classify_steer(
            session_id,
            self.steer_supplement(&message.context, &cola_msg_id).await,
        );
        let submitted = match answer {
            SteerAnswer::Merged => {
                tracing::info!(
                    "supplement: session {} in-flight, message queued to merge into current turn",
                    session_id
                );
                true
            }
            // Typed refusal: the adapter observed no run to steer into and
            // refused BEFORE submitting, so the message may be re-routed
            // (ADR-0113). Nothing was submitted, so the handshake's resolution
            // is still this message's first submission.
            SteerAnswer::Refused => return self.settle_refused_supplement(message, &cola_msg_id).await,
            SteerAnswer::Unconfirmed => false,
        };
        // The supplement landed below the live card: split the Card Chain at
        // this message (ADR-0043). The previous card is finalized with the
        // standard split header and keeps everything before the split; the
        // continuation — a reply to the supplement — carries the receipt (and
        // only the content that arrives after it) and becomes the tracked live
        // card. There is NO separate acknowledgement message.
        Ok(Some(
            self.finish_supplement(session_id, message_id, submitted).await,
        ))
    }

    /// One merge-only submission of `message` (ADR-0109, ADR-0113): the steer
    /// carries the caller's model/variant/agent axes and forbids a start. A
    /// helper rather than an inline call because the typed-refusal handshake may
    /// retry the merge with the **same** `cola_msg_id` — the retry must be the
    /// same logical message, and the refusal proved the earlier attempt sent
    /// nothing.
    async fn steer_supplement(
        &self,
        ctx: &PromptContext,
        cola_msg_id: &str,
    ) -> crate::error::Result<crate::backend::SteerOutcome> {
        // A thread-provider client (Codex) owns the next-turn pick in its
        // adapter (spec #922): a steer carries no model field, and its fallback
        // `turn/start` reads the adapter's pending. Sending the stored mirror
        // here would re-send a consumed pick.
        let next_turn_pending = self.client.owns_next_turn_pending();
        let model = if next_turn_pending {
            None
        } else {
            self.session_model_override(&ctx.session_id).await
        };
        let variant = if next_turn_pending {
            None
        } else {
            self.session_variant_override(&ctx.session_id).await
        };
        self.client
            .steer(
                &ctx.session_id,
                &ctx.text,
                &ctx.files,
                &ctx.skills,
                model.as_ref(),
                variant.as_deref(),
                self.session_agent_override(&ctx.session_id).await.as_deref(),
                Some(cola_msg_id),
                // A Supplement must merge: a run this process does not render
                // must never be started here (ADR-0109).
                false,
            )
            .await
    }

    /// Resolve a message whose merge-only steer refused (ADR-0113): nothing was
    /// submitted, so this waits for the guard and admits the Turn it could not
    /// merge into — never reporting the message unsubmitted while any writer
    /// exists. While a writer holds the guard, the merge is retried so the
    /// message joins that run as soon as the adapter can see it; the retry and
    /// the Turn are the message's first submission. The wait has no user-facing
    /// deadline (ADR-0113 §3): [`SUPPLEMENT_WAIT_WARN_AFTER`] only logs, and
    /// coarsens the poll, so a wedged holder is visible.
    async fn settle_refused_supplement(
        self: &Arc<Self>,
        message: PromptMessage,
        cola_msg_id: &str,
    ) -> crate::error::Result<Option<PromptSubmission>> {
        let session_id = message.context.session_id.clone();
        let message_id = message.context.message_id.clone();
        let waits = self.waits_handle();
        let mut pending = Some(message);
        let mut last_merge: Option<std::time::Instant> = None;
        let started = std::time::Instant::now();
        let mut warned = false;
        loop {
            // The guard is free: claim it for the Turn that renders the message.
            if let Some(admission) = crate::bridge::turn::Turn::try_admit(&waits, &session_id).await {
                tracing::info!(
                    "supplement: session {session_id} refused the merge; the guard cleared and the message starts a Turn"
                );
                let message = pending.take().expect("the refused message is still pending");
                let outcome = self
                    .run_prompt_admitted(message.into_context(false), admission)
                    .await?;
                return Ok(Some(match outcome {
                    crate::bridge::turn::PromptOutcome::Accepted => PromptSubmission::NewTurn,
                    crate::bridge::turn::PromptOutcome::NotAccepted => PromptSubmission::NotAccepted,
                }));
            }
            // A held guard is a writer when no out-of-turn renderer OWNS a
            // lease (a lease alone is not proof of a run, ADR-0113 §3a): the
            // message can merge the moment that writer's run is visible.
            let writer = !waits.owns_render_lease(&session_id).await;
            let due = last_merge.is_none_or(|at| at.elapsed() >= SUPPLEMENT_MERGE_RETRY);
            if writer && due {
                last_merge = Some(std::time::Instant::now());
                let context = &pending
                    .as_ref()
                    .expect("the refused message is still pending")
                    .context;
                match classify_steer(&session_id, self.steer_supplement(context, cola_msg_id).await) {
                    SteerAnswer::Merged => {
                        tracing::info!(
                            "supplement: session {session_id} merged into the writer that held the guard"
                        );
                        return Ok(Some(self.finish_supplement(&session_id, &message_id, true).await));
                    }
                    SteerAnswer::Refused => {}
                    SteerAnswer::Unconfirmed => {
                        return Ok(Some(
                            self.finish_supplement(&session_id, &message_id, false).await,
                        ));
                    }
                }
            }
            if !warned && started.elapsed() >= SUPPLEMENT_WAIT_WARN_AFTER {
                warned = true;
                tracing::warn!(
                    "supplement: session {session_id} refused the merge and the guard is still held; waiting for it to clear"
                );
            }
            let poll = if warned {
                SUPPLEMENT_HANDSHAKE_POLL_COARSE
            } else {
                SUPPLEMENT_HANDSHAKE_POLL
            };
            tokio::time::sleep(poll).await;
        }
    }

    /// Close out a Supplement that has no fallback left (#1056, ADR-0109): split
    /// the chain when a writer can continue it, else report the one visible
    /// failure. A FAILED steer did not CONFIRM a merge: the continuation may
    /// only be opened while a held guard proves a live in-process writer will
    /// render and settle it. Without one, the chain's owner is only a card-state
    /// claim, and the split would hand a live card to nobody — every later
    /// message would then be absorbed as a supplement of a run that does not
    /// exist. Neither message claims the steer was lost: an ambiguous response
    /// may still have landed (ADR-0092 keeps its marker), so the copy says
    /// unconfirmed and never tells the user to resend.
    async fn finish_supplement(
        &self,
        session_id: &str,
        message_id: &str,
        submitted: bool,
    ) -> PromptSubmission {
        let owned_by_writer = !submitted && {
            let ownership = crate::bridge::turn::CardOwnership::read(
                &self.cards_handle(),
                &self.waits_handle(),
                session_id,
            )
            .await;
            // A renderer's LEASE is not proof of a writer (#1086, ADR-0113): it
            // says a renderer is in its window, not that a run exists behind it.
            // The restart-armed continuation rehydrates a card, holds the guard
            // and renders nothing new, so a continuation handed to it would be
            // the false receipt the live report showed. Only a Turn's (or its
            // follow's) own guard proves a live chain to continue.
            ownership.holds_guard()
                && ownership.card_class() == crate::bridge::turn::CardClass::RenderOwned
                && !self.waits_handle().owns_render_lease(session_id).await
        };
        let admitted = if submitted {
            crate::bridge::turn::Turn::split_card_chain(
                &self.cards_handle(),
                session_id,
                message_id,
                crate::bridge::turn::SplitKind::Supplement,
            )
            .await
        } else {
            // The split's admission re-checks the card under the same lock an
            // ending is applied with, so a Turn that settles in this window
            // refuses it: the chain keeps its card and the plain notice is the
            // one visible failure.
            owned_by_writer
                && crate::bridge::turn::Turn::split_card_chain(
                    &self.cards_handle(),
                    session_id,
                    message_id,
                    crate::bridge::turn::SplitKind::SupplementFailed,
                )
                .await
        };
        if !admitted {
            // A merged message with no card to continue on (the card ended in
            // the window) and a failed steer without a writer both end here;
            // the copy must not claim a delivery that did not happen
            // (ADR-0092).
            let copy = if submitted {
                SUPPLEMENT_UNCONTINUABLE_NOTICE
            } else {
                SUPPLEMENT_UNCONFIRMED_NOTICE
            };
            tracing::info!(
                "supplement: no continuation for session {session_id} (submitted={submitted}); reporting it"
            );
            let _ = self.feishu.reply_text(message_id, copy).await;
        }
        // The message merged into the live Execution: the claim's job is done —
        // once the prompt is visible in the transcript, the Sync's own staleness
        // rule takes over (#424).
        self.waits_handle().clear_inbound(session_id).await;
        if submitted {
            PromptSubmission::Supplement
        } else {
            PromptSubmission::NotAccepted
        }
    }

    /// [`Self::run_prompt`] for a Turn whose guard the caller already claimed
    /// with [`crate::bridge::turn::Turn::try_admit`] (ADR-0113): the claim and
    /// the admission are one instant, so the busy refusal cannot fire here.
    async fn run_prompt_admitted(
        self: &Arc<Self>,
        ctx: PromptContext,
        admission: crate::bridge::turn::Admission,
    ) -> crate::error::Result<crate::bridge::turn::PromptOutcome> {
        let handles = self.turn_handles();
        crate::bridge::turn::Turn::run_admitted(&handles, ctx, admission).await
    }

    /// Run one prompt end-to-end: show a Loading card (either a fresh reply or a
    /// reset of an existing card when retrying), stream parts via the poll loop,
    /// then render the final Done/Error card. Shared by fresh messages and the
    /// error-card "retry" action.
    ///
    /// The coordinator builds the Turn's narrow handles here (spec #298, A1) —
    /// the Turn itself never sees the aggregate.
    async fn run_prompt(
        self: &Arc<Self>,
        ctx: PromptContext,
    ) -> crate::error::Result<crate::bridge::turn::PromptOutcome> {
        let handles = self.turn_handles();
        crate::bridge::turn::Turn::run(&handles, ctx).await
    }

    async fn get_or_create_session(
        &self,
        thread_key: &ThreadKey,
        text: &str,
        message_id: &str,
    ) -> crate::error::Result<(String, bool)> {
        if let Some(id) = self.get_session_id(thread_key).await {
            return Ok((id, false));
        }
        // Lazy Session Creation (ADR-0041): a declared Pending Session
        // materialises here, on the conversation's first non-command message.
        // `created` stays false — an explicit `/new` already told the user this
        // would happen, so the group-lobby guidance (which belongs to the
        // silent auto-create) must not fire.
        match self
            .sessions_handle()
            .claim_pending_for_creation(thread_key)
            .await
        {
            crate::bridge::session::PendingClaim::Claimed(pending, generation) => {
                let directory = pending.directory.clone();
                let worktree_target = pending.worktree.as_ref().map(|intent| intent.target.clone());
                // The first message pays for the whole-tree checkout (and the
                // directory instance boot), which can take seconds on a large
                // repository. Acknowledge it before the Git call so the wait
                // never looks like a wedged cola (ADR-0095).
                if let Some(intent) = pending
                    .worktree
                    .as_ref()
                    .filter(|intent| !intent.checkout_exists())
                {
                    let _ = self
                        .feishu
                        .reply_text(
                            message_id,
                            &format!(
                                "⏳ 正在创建 worktree `{}`（首次需要检出文件，可能多等几秒）…",
                                intent.target
                            ),
                        )
                        .await;
                }
                return match self.materialise_pending(*pending, generation, message_id).await {
                    Ok(id) => Ok((id, false)),
                    Err(e) => {
                        // A create failure leaves the pending intact (and gives
                        // the claim back): surface the error here and let the
                        // next message retry.
                        self.sessions_handle()
                            .unclaim_pending_creation(thread_key, generation)
                            .await;
                        let place = match worktree_target {
                            Some(target) => format!("worktree `{target}`"),
                            None => format!("目录 `{directory}`"),
                        };
                        let _ = self
                            .feishu
                            .reply_text(
                                message_id,
                                &format!("⚠️ 创建会话失败（{place}）：{}\n下一条消息会再试一次。", e),
                            )
                            .await;
                        Err(e)
                    }
                };
            }
            // Another message is creating this pending right now: a second
            // `thread/start` would be a second session, so refuse this message
            // and let the user resend once the first one lands.
            crate::bridge::session::PendingClaim::Creating => {
                let _ = self
                    .feishu
                    .reply_text(message_id, "⏳ 这个会话正在创建中，请等它完成后再发消息。")
                    .await;
                return Err(crate::error::BridgeError::State(
                    "the session is being materialised by another message".to_string(),
                ));
            }
            crate::bridge::session::PendingClaim::None => {}
        }
        let directory = self.default_session_directory();
        let id = self
            .create_fresh_session(thread_key, text, directory, None, None)
            .await?;
        Ok((id, true))
    }

    /// Create the real session a Pending Session declared, apply its creation
    /// title and per-session overrides, then activate it and clear the pending
    /// in ONE store write (ADR-0041). A failed create returns the error with
    /// the pending intact, so the next message retries.
    async fn materialise_pending(
        &self,
        mut pending: PendingEntry,
        generation: u64,
        message_id: &str,
    ) -> crate::error::Result<String> {
        // ADR-0095: the pending may carry a checkout intent. Create it now —
        // the first message is the moment the conversation actually starts —
        // and point the pending at it. A failure leaves the pending (and the
        // intent) intact for the next message. The created path is recorded
        // before the session exists, so a retry after a session-create failure
        // reuses the same checkout instead of making a `-2` sibling.
        if let Some(intent) = pending.worktree.clone() {
            let source = pending.directory.clone();
            let path = match intent.path.clone().filter(|_| intent.checkout_exists()) {
                Some(path) => path,
                None => {
                    let branch = intent.branch_name();
                    let created = crate::git::create_worktree(
                        &source,
                        &self.worktree_root,
                        &intent.slug,
                        Some(&branch),
                        Some(&intent.base_sha),
                    )
                    .await
                    .map_err(|error| {
                        crate::error::BridgeError::State(format!("创建 worktree 失败：{error}"))
                    })?;
                    // Record the path only while this exact pending is still
                    // the one we claimed (ADR-0095). A replacement bumps the
                    // epoch; stamping our path onto it would make its first
                    // message run in the wrong checkout.
                    if !self
                        .sessions_handle()
                        .record_worktree_path(&pending.thread_key, generation, &created.path, &created.branch)
                        .await?
                    {
                        tracing::warn!(
                            "worktree {} was created for a pending that was replaced; leaving it in place",
                            created.path
                        );
                        return Err(crate::error::BridgeError::State(
                            "the thread moved on while its checkout was being created".to_string(),
                        ));
                    }
                    // Name what actually exists, including a `-2` suffix from a
                    // collision (ADR-0095 decision 3). Best-effort: the
                    // checkout is already made, and a failed notice must not
                    // fail materialisation.
                    let collision = crate::git::plain_path(std::path::PathBuf::from(&created.path))
                        != crate::git::plain_path(std::path::PathBuf::from(&intent.target));
                    let lead = if collision {
                        "目标目录已被占用，"
                    } else {
                        ""
                    };
                    let _ = self
                        .feishu
                        .reply_text(
                            message_id,
                            &format!(
                                "🌲 {lead}worktree 建在 `{}`（分支 `{}`，起点 `{} @ {}`）。",
                                created.path,
                                created.branch,
                                intent.base,
                                intent.short_sha()
                            ),
                        )
                        .await;
                    created.path
                }
            };
            pending.directory = path;
        }
        let mut input = self.client.new_session_input(Some(&pending.directory));
        // A Codex thread takes the model at creation; the provider is a config
        // fact and never a creation field (spec #930). A client whose thread
        // selection is per-prompt (OpenCode) keeps applying the pending model
        // after creation, so its create body stays byte-identical.
        if self.client.tracks_thread_provider() {
            self.normalize_pending_model_provider(&mut pending);
            input.model = pending
                .model
                .as_deref()
                .and_then(crate::backend::parsing::parse_model)
                .map(|model| crate::backend::types::ModelInfo {
                    id: model.id,
                    provider_id: model.provider_id,
                    variant: None,
                });
        }
        let session = self.client.create_session(&input).await?;
        // A thread forgotten or re-declared while `thread/start` ran is stale:
        // apply nothing to the created session and never map it (issue #910).
        if !self
            .sessions_handle()
            .creation_is_current(&pending.thread_key, generation)
            .await
        {
            return Err(crate::error::BridgeError::State(
                "the thread moved on while its session was being created".to_string(),
            ));
        }
        // `thread/start` is async and the daemon may have been replaced while
        // it ran. Revalidate before applying picks and again before copying the
        // ref into the durable SessionEntry.
        if self.client.tracks_thread_provider() {
            self.normalize_pending_model_provider(&mut pending);
        }
        // ADR-0041 + V2's durable selection: the pending's picks become
        // session switches now that a session exists (and on V1 the same
        // helper's switch is a no-op, leaving the first prompt to carry them).
        // It also applies the shared ADR-0020 clear rule, so a pending pick
        // that no longer matches the model cannot poison the first turn.
        // Best-effort: a failed switch warns (and tells the user), but the
        // created session is never orphaned.
        self.apply_pending_picks(&session.id, &mut pending, message_id)
            .await;
        // Creation title policy (ADR-0007): `/new <name>` PATCHes the title.
        // A failed PATCH must not orphan the created session (a retry would
        // create a second one), so it warns and keeps the server-generated
        // title.
        if let Some(title) = &pending.title
            && let Err(e) = self.client.update_session_title(&session.id, title).await
        {
            tracing::warn!(
                "materialise: title patch failed for {} ({e}); keeping the server title",
                session.id
            );
            let _ = self
                .feishu
                .reply_text(
                    message_id,
                    &format!(
                        "⚠️ 会话已创建，但标题 `{}` 设置失败：{}\n可用 `/name {}` 重试。",
                        title, e, title
                    ),
                )
                .await;
        }
        if self.client.tracks_thread_provider() {
            self.normalize_pending_model_provider(&mut pending);
        }
        let model_generation_at_activation = pending.validated_provider_fact_generation;
        let model_before_activation = pending.model.clone();
        let variant_before_activation = pending.variant.clone();
        let directory_for_generation = pending.directory.clone();
        let entry = pending.into_entry(session.id.clone());
        let thread_key = entry.thread_key.clone();
        // The core wrapper (not the raw store) so the session-list cache is
        // invalidated: `/switch` must see the just-created session. Scoped to
        // this creation's generation (issue #910): a thread that moved on while
        // we ran is not overwritten, and a replacement pending survives.
        #[cfg(test)]
        if let Some(gate) = self.materialise_before_activation.lock().await.clone() {
            self.materialise_activation_entered.notify_one();
            let permit = gate.acquire().await.expect("activation test gate closed");
            permit.forget();
        }
        if !self.activate_created(entry, generation).await? {
            return Err(crate::error::BridgeError::State(
                "the thread moved on while its session was being created".to_string(),
            ));
        }
        if self.client.tracks_thread_provider()
            && let Some(original_model) = model_before_activation.as_deref()
        {
            let expected_generation = Some(model_generation_at_activation.unwrap_or_default());
            let mut stored_model = original_model.to_string();
            // Activation persists the entry under the SessionStore lock. A
            // provider generation can change while it waits there, so correct
            // the now-live mirror afterwards with a model/variant CAS. Retry a
            // few fact swaps without making a config read; churn ultimately
            // converges to the neutral model ref.
            for _ in 0..3 {
                let fact = self.client.configured_provider_fact(&directory_for_generation);
                let generation_before = self.client.provider_fact_generation();
                let current_provider = (fact.generation == generation_before)
                    .then_some(fact.provider_id.as_deref())
                    .flatten();
                let (neutral_model, stale) = crate::backend::parsing::model_spec_for_provider_generation(
                    original_model,
                    expected_generation,
                    generation_before,
                    self.client.neutral_provider_id(),
                    current_provider,
                );
                if stale && neutral_model != stored_model {
                    match self
                        .sessions_handle()
                        .correct_created_model_if_unchanged(
                            &thread_key,
                            &session.id,
                            &stored_model,
                            variant_before_activation.as_deref(),
                            &neutral_model,
                        )
                        .await
                    {
                        Ok(true) => stored_model = neutral_model,
                        Ok(false) => break,
                        Err(error) => {
                            tracing::warn!(
                                session = %session.id,
                                "materialise: could not correct a stale provider model ref: {error}"
                            );
                            break;
                        }
                    }
                }
                if self.client.provider_fact_generation() == generation_before {
                    break;
                }
            }
        }
        // ADR-0041 + ADR-0023: a pending topic's cover still says
        // 「下一条消息创建」. Move its record onto the new session and re-render
        // now — a real Session exists, so the brief (title/id) belongs on the
        // card. No-op for lobby pendings and fallback-rooted topics.
        crate::bridge::topic::claim_pending_cover(&self.topic_handles(), &thread_key, &session.id).await;
        Ok(session.id)
    }

    /// A Pending Session's model ref is only provider-attributed while the
    /// cache generation that validated it remains current. Persisted pendings
    /// have no generation, so they intentionally fall back to the neutral ref.
    fn normalize_pending_model_provider(&self, pending: &mut PendingEntry) {
        let Some(model) = pending.model.as_deref() else {
            return;
        };
        let fact = self.client.configured_provider_fact(&pending.directory);
        let generation = self.client.provider_fact_generation();
        let current_provider = (fact.generation == generation)
            .then_some(fact.provider_id.as_deref())
            .flatten();
        let (model, stale) = crate::backend::parsing::model_spec_for_provider_generation(
            model,
            Some(pending.validated_provider_fact_generation.unwrap_or_default()),
            generation,
            self.client.neutral_provider_id(),
            current_provider,
        );
        if stale {
            pending.model = Some(model);
            pending.validated_provider_fact_generation = None;
        }
    }

    /// Apply a Pending Session's picks to the session it just created, through
    /// the same rules as a live pick: [`SessionsHandle::apply_model_selection`]
    /// clears a variant the model does not declare (ADR-0020) and switches the
    /// ref, and the agent switches by id. The model switch needs a full ref, so
    /// a variant-only pending (no `/model` pick) resolves the model the create
    /// recorded — and reports a failed read rather than guessing. V1's switches
    /// are no-ops; only the clear rule and the local mirror apply there.
    /// Failures are reported once, without orphaning the created session.
    async fn apply_pending_picks(&self, session_id: &str, pending: &mut PendingEntry, message_id: &str) {
        let mut failures: Vec<String> = Vec::new();
        let model_spec = match pending.model.clone() {
            Some(spec) => Some(spec),
            None if pending.variant.is_some() => {
                match self
                    .client
                    .session_info(session_id, Some(&pending.directory))
                    .await
                {
                    Ok(info) => info
                        .model
                        .map(|model| format!("{}/{}", model.provider_id, model.id)),
                    Err(e) => {
                        tracing::warn!("materialise: session model read failed for {session_id}: {e}");
                        failures.push(format!("读取会话模型失败（{e}）"));
                        None
                    }
                }
            }
            None => None,
        };
        if let Some(model_spec) = model_spec
            && let Err(e) = self
                .sessions_handle()
                .apply_model_selection(&self.client, session_id, &model_spec, &mut pending.variant)
                .await
        {
            tracing::warn!("materialise: model switch failed for {session_id}: {e}");
            failures.push(format!("模型/思考等级未生效（{e}）"));
        }
        // A thread-level provider client's `thread/start` carries the model and
        // provider only — there is no creation field for `/think`. The pending
        // Session's level therefore becomes the new thread's first **new** turn
        // pending (spec #922): it rides that `turn/start.effort` once, never
        // every later prompt. No model pending is recorded: the thread already
        // started on the creation model, so the card shows no 待应用.
        if self.client.owns_next_turn_pending()
            && pending.variant.is_some()
            && let Err(e) = self
                .client
                .record_pending_variant(session_id, pending.variant.as_deref())
                .await
        {
            tracing::warn!("materialise: pending think level failed for {session_id}: {e}");
            failures.push(format!("思考等级未生效（{e}）"));
        }
        if let Some(agent) = &pending.agent
            && let Err(e) = self.client.switch_session_agent(session_id, agent).await
        {
            tracing::warn!("materialise: agent switch failed for {session_id}: {e}");
            failures.push(format!("Agent 未生效（{e}）"));
        }
        if !failures.is_empty() {
            let _ = self
                .feishu
                .reply_text(
                    message_id,
                    &format!(
                        "⚠️ 会话已创建，但{}；可用 `/model`、`/think`、`/agent` 重试。",
                        failures.join("；")
                    ),
                )
                .await;
        }
    }

    /// Create a brand-new session on the current server and make it the active
    /// one for the thread. Used when a mapped session no longer exists (404).
    /// The per-session overrides (agent/model/variant/auto_accept) reset to
    /// defaults, but the topic's creation messages (`topic_anchor`/`topic_root`,
    /// ADR-0023) are Feishu message ids — not session state — and survive so the
    /// quote-injection guard keeps working after the recreate.
    pub(crate) async fn create_fresh_session(
        &self,
        thread_key: &ThreadKey,
        _text: &str,
        directory: String,
        topic_anchor: Option<String>,
        topic_root: Option<String>,
    ) -> crate::error::Result<String> {
        self.flow_handles()
            .create_fresh_session(thread_key, directory, topic_anchor, topic_root)
            .await
    }

    /// Handle a card action (permission Allow/Deny, question answer/reject,
    /// error-card retry). Returns the updated card showing the decision, so the
    /// caller can send it back in the ack, plus an optional Toast for instant
    /// client feedback. What every click shares lives here — the Host gate, the
    /// payload parse, the capability gates, the one ADR-0089 deadline and the
    /// span — and the matched verb's body lives behind the CardAction seam
    /// ([`crate::bridge::card_action`]), which the dispatch looks up by
    /// `action`/`op`.
    ///
    /// The whole dispatch runs inside the click's [`span::action`] (ADR-0048):
    /// every line the action and its awaits produce is then retrievable by the
    /// Session it worked on.
    pub async fn handle_card_action(self: &Arc<Self>, value: serde_json::Value) -> Option<CardActionResult> {
        // ONE deadline for the click, started HERE: the ack budget starts when
        // Feishu's callback reaches us, so a slow pre-dispatch store read must
        // spend the same budget the later work is bounded by — a fresh deadline
        // after it would let a pending commit after the ack was already lost
        // (ADR-0089).
        let callback_started = tokio::time::Instant::now();
        let deadline = callback_started + CARD_STORE_BUDGET;
        if let Some(refusal) = self.gate_card_action(&value).await {
            return Some(refusal);
        }
        let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        // The span context may read the store; a click whose context cannot be
        // resolved in time is still dispatched (the span then carries none).
        let (session_id, thread_key) =
            tokio::time::timeout_at(deadline, self.card_action_span_context(&value))
                .await
                .unwrap_or((None, None));
        let action_span = span::action(session_id.as_deref(), thread_key.as_ref());
        let dispatch = async {
            // A verb behind the CardAction seam (#1156) is one lookup: an
            // op-only entry (the Turn card's action-less diff click) wins, then
            // the action table. The capability gate is the entry's, applied
            // here so a verb never reads capabilities itself (ADR-0106).
            if let Some(verb) = crate::bridge::card_action::lookup(action, op) {
                if !verb.gate.is_open(&self.core) {
                    return None;
                }
                return (verb.run)(self, &value, deadline, callback_started).await;
            }
            // Every verb lives behind the seam (#1122): an action the registry
            // does not know is an unanswered click.
            None
        };
        dispatch.instrument(action_span).await
    }

    /// The Session a card click works on, for its span (ADR-0048), plus the
    /// Chat/Topic it lives in. The payload's `session_id` names the target
    /// (permission, question, retry, a switch adoption); the picker cards name
    /// only their Chat/Topic and read the thread's current Session (the active
    /// one, or the Pending Session the first message will materialise) — the
    /// same accessor the handlers themselves use. The other side is completed
    /// from the SessionStore: a permission card carries the session but not the
    /// chat, the picker cards carry the chat but not yet a session.
    ///
    /// Either side stays `None` when unknown — a Pending Session has no id
    /// yet, a brand-new Chat/Topic has no Session — and its field is then
    /// omitted from the fmt prefix rather than printed empty.
    async fn card_action_span_context(
        &self,
        value: &serde_json::Value,
    ) -> (Option<String>, Option<ThreadKey>) {
        let payload_key = thread_key_from_value(value);
        let payload_key = (!payload_key.chat_id.is_empty()).then_some(payload_key);
        let session_id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        match (session_id, payload_key) {
            (Some(session_id), Some(thread_key)) => (Some(session_id), Some(thread_key)),
            (Some(session_id), None) => {
                let thread_key = self
                    .sessions
                    .lock()
                    .await
                    .entry_for_session(&session_id)
                    .map(|e| e.thread_key.clone());
                (Some(session_id), thread_key)
            }
            (None, Some(thread_key)) => {
                let session_id = self
                    .session_settings(&thread_key)
                    .await
                    .and_then(|settings| settings.session_id);
                (session_id, Some(thread_key))
            }
            (None, None) => (None, None),
        }
    }

    /// Finish a pager flip or a markdown view toggle (#740, #781) after the
    /// callback has acknowledged. The carried lane is retained while the bounded
    /// read rebuilds the requested `(page, view)`.
    pub(crate) async fn run_viewer_page(self: &Arc<Self>, page: ViewerPage) {
        let ViewerPage {
            thread_key,
            path,
            session_dir,
            card_message_id,
            return_to,
            page,
            view,
            lane,
            payload_identity,
            in_flight_guard,
        } = page;
        let _in_flight_guard = in_flight_guard;
        let timeout_ms = self
            .viewer_transfer_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let resolved = std::path::PathBuf::from(&path);
        let read_path = resolved.clone();
        let read = bounded_viewer_fs_read(
            self.cards_handle().viewer_fs_read_slots.clone(),
            timeout_ms,
            "⚠️ 文件读取繁忙，请稍后再试。",
            "⚠️ 打开超时，请稍后再试。",
            move || match std::fs::metadata(&read_path) {
                Ok(metadata) => crate::bridge::viewer::classify(&read_path, metadata.len()),
                Err(error) => Err(crate::bridge::viewer::stat_failure(&error)),
            },
        )
        .await;
        let classified = match read {
            Ok(Ok(classified)) => classified,
            Ok(Err(failure)) => {
                self.answer_viewer_reread_failure(&card_message_id, &failure.line(&resolved), timeout_ms)
                    .await;
                return;
            }
            Err(ViewerFsReadError::Busy(line) | ViewerFsReadError::Timeout(line)) => {
                self.answer_viewer_reread_failure(&card_message_id, &line, timeout_ms)
                    .await;
                return;
            }
            Err(ViewerFsReadError::Worker(error)) => {
                let line = format!("⚠️ 无法读取 `{}`：{error}", resolved.display());
                self.answer_viewer_reread_failure(&card_message_id, &line, timeout_ms)
                    .await;
                return;
            }
        };
        let browse_view = return_to.dir.as_deref().map(|dir| BrowseView {
            dir,
            show_hidden: return_to.show_hidden,
            keyword: &return_to.keyword,
            page: return_to.page,
        });
        let ViewerPageLane { lane, changed } =
            reconcile_viewer_page_lane(lane, payload_identity, &classified, &resolved);
        let card = crate::feishu::card::command::build_viewer_for_classified(
            &thread_key,
            &resolved,
            &classified,
            lane,
            browse_view,
            crate::feishu::card::command::ViewerBuildOptions {
                page,
                overflowed: false,
                session_dir: session_dir.as_deref(),
                view,
                notice: changed.then_some(crate::feishu::card::viewer::PAGE_CONTENT_CHANGED_NOTE),
            },
        );
        if !matches!(
            crate::bridge::bounded_call(
                "viewer page card update",
                timeout_ms,
                self.feishu.update_message(&card_message_id, &card),
            )
            .await,
            Some(Ok(()))
        ) {
            tracing::warn!("viewer page card update for {card_message_id} did not land");
        }
    }

    /// Finish a re-read after the callback has acknowledged. All platform I/O
    /// is bounded and runs off the callback path; the local snapshot was read
    /// under the viewer's cap before dispatch.
    pub(crate) async fn run_viewer_reread(self: &Arc<Self>, reread: ViewerReread) {
        let ViewerReread {
            thread_key,
            path,
            session_dir,
            card_message_id,
            rich_message_id,
            sent_file_identity,
            ambiguous_file_identity,
            embedded_image_key,
            return_to,
            page,
            view,
            in_flight_guard,
        } = reread;
        let _in_flight_guard = in_flight_guard;
        let timeout_ms = self
            .viewer_transfer_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let browse_view = return_to.dir.as_deref().map(|dir| BrowseView {
            dir,
            show_hidden: return_to.show_hidden,
            keyword: &return_to.keyword,
            page: return_to.page,
        });
        let resolved = std::path::PathBuf::from(&path);
        // `metadata` and classify may block on the host filesystem (e.g. a
        // network mount). Run that bounded read off the callback/runtime worker:
        // the card click has already been acknowledged before this task starts.
        let read_path = resolved.clone();
        let read = bounded_viewer_fs_read(
            self.cards_handle().viewer_fs_read_slots.clone(),
            timeout_ms,
            "⚠️ 重新读取任务繁忙，请稍后再试。",
            &format!("⚠️ 无法读取 `{}`：重新读取超时。", resolved.display()),
            move || match std::fs::metadata(&read_path) {
                Ok(metadata) => crate::bridge::viewer::classify(&read_path, metadata.len()),
                Err(error) => Err(crate::bridge::viewer::stat_failure(&error)),
            },
        )
        .await;
        let mut classified = match read {
            Ok(Ok(classified)) => classified,
            Ok(Err(failure)) => {
                self.answer_viewer_reread_failure(&card_message_id, &failure.line(&resolved), timeout_ms)
                    .await;
                return;
            }
            Err(ViewerFsReadError::Busy(line) | ViewerFsReadError::Timeout(line)) => {
                self.answer_viewer_reread_failure(&card_message_id, &line, timeout_ms)
                    .await;
                return;
            }
            Err(ViewerFsReadError::Worker(error)) => {
                let line = format!("⚠️ 无法读取 `{}`：{error}", resolved.display());
                self.answer_viewer_reread_failure(&card_message_id, &line, timeout_ms)
                    .await;
                return;
            }
        };
        let name = crate::feishu::card::command::file_display_name(&resolved);
        let ext = resolved.extension().and_then(|extension| extension.to_str());
        let (lane, overflowed) = crate::bridge::viewer::open_viewer_lanes(
            &self.feishu,
            &self.cards_handle(),
            &card_message_id,
            &name,
            ext,
            &mut classified,
            crate::bridge::viewer::ViewerLaneOptions {
                timeout_ms,
                existing_rich_message_id: rich_message_id.as_deref(),
                existing_sent_file_identity: sent_file_identity,
                existing_ambiguous_file_identity: ambiguous_file_identity,
                existing_embedded_image_key: embedded_image_key.as_deref(),
            },
        )
        .await;
        let card = crate::feishu::card::command::build_viewer_for_classified(
            &thread_key,
            &resolved,
            &classified,
            lane,
            browse_view,
            crate::feishu::card::command::ViewerBuildOptions {
                page,
                overflowed,
                session_dir: session_dir.as_deref(),
                view,
                notice: None,
            },
        );
        if !matches!(
            crate::bridge::bounded_call(
                "viewer re-read card update",
                timeout_ms,
                self.feishu.update_message(&card_message_id, &card),
            )
            .await,
            Some(Ok(()))
        ) {
            tracing::warn!("viewer re-read card update for {card_message_id} did not land");
        }
    }

    /// Report an off-thread file read failure under the viewer card without
    /// replacing its last good contents.
    async fn answer_viewer_reread_failure(&self, card_message_id: &str, line: &str, timeout_ms: u64) {
        if !matches!(
            crate::bridge::bounded_call(
                "viewer re-read failure reply",
                timeout_ms,
                self.feishu.reply_text(card_message_id, line),
            )
            .await,
            Some(Ok(_))
        ) {
            tracing::warn!("viewer re-read failure reply for {card_message_id} did not land");
        }
    }

    /// Finish a viewer open after its callback ack. One bounded filesystem
    /// snapshot selects the path kind and content lanes; after any needed
    /// delivery settles, browse rows replace their listing card while Tool
    /// Panel opens reply with a standalone viewer beside the live Turn.
    pub(crate) async fn run_viewer_open(self: &Arc<Self>, open: ViewerOpen) {
        let ViewerOpen {
            thread_key,
            path,
            session_dir,
            standalone,
            card_message_id,
            return_to,
            page,
            core,
            in_flight_guard,
        } = open;
        let _in_flight_guard = in_flight_guard;
        let timeout_ms = self
            .viewer_transfer_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let resolved = std::path::PathBuf::from(&path);
        let read_path = resolved.clone();
        let read_session_dir = session_dir.clone();
        let read = bounded_viewer_fs_read(
            self.cards_handle().viewer_fs_read_slots.clone(),
            timeout_ms,
            "⚠️ 打开任务繁忙，请稍后再试。",
            &format!("⚠️ 无法读取 `{}`：打开超时。", resolved.display()),
            move || {
                // Canonicalization can touch a stalled/network filesystem too:
                // keep it in the same bounded worker as stat and classification,
                // after the card callback has already acknowledged.
                let resolved = match read_session_dir.as_deref() {
                    Some(dir) => crate::bridge::viewer::resolve_cat_path(&read_path.to_string_lossy(), dir),
                    None => read_path,
                };
                match crate::bridge::viewer::open_cat_path(&resolved) {
                    crate::bridge::viewer::CatOpen::Directory(dir) => Ok(ViewerOpenRead::Directory(dir)),
                    crate::bridge::viewer::CatOpen::File { path, size } => {
                        crate::bridge::viewer::classify(&path, size)
                            .map(|classified| ViewerOpenRead::File { path, classified })
                    }
                    crate::bridge::viewer::CatOpen::Failed(failure) => Err(failure),
                }
            },
        )
        .await;
        let read = match read {
            Ok(Ok(read)) => read,
            Ok(Err(failure)) => {
                self.patch_viewer_open_failure(ViewerOpenFailure {
                    thread_key: &thread_key,
                    resolved: &resolved,
                    path: &path,
                    card_message_id: &card_message_id,
                    standalone,
                    return_to: &return_to,
                    line: &failure.line(&resolved),
                    timeout_ms,
                })
                .await;
                return;
            }
            Err(ViewerFsReadError::Worker(error)) => {
                let line = format!("⚠️ 无法读取 `{}`：{error}", resolved.display());
                self.patch_viewer_open_failure(ViewerOpenFailure {
                    thread_key: &thread_key,
                    resolved: &resolved,
                    path: &path,
                    card_message_id: &card_message_id,
                    standalone,
                    return_to: &return_to,
                    line: &line,
                    timeout_ms,
                })
                .await;
                return;
            }
            Err(ViewerFsReadError::Busy(line) | ViewerFsReadError::Timeout(line)) => {
                self.patch_viewer_open_failure(ViewerOpenFailure {
                    thread_key: &thread_key,
                    resolved: &resolved,
                    path: &path,
                    card_message_id: &card_message_id,
                    standalone,
                    return_to: &return_to,
                    line: &line,
                    timeout_ms,
                })
                .await;
                return;
            }
        };
        let browse_view = return_to.dir.as_deref().map(|dir| BrowseView {
            dir,
            show_hidden: return_to.show_hidden,
            keyword: &return_to.keyword,
            page: return_to.page,
        });
        let card = match read {
            ViewerOpenRead::Directory(dir) => {
                let dir = dir.to_string_lossy().to_string();
                self.build_dir_browse_for(
                    &core,
                    &thread_key,
                    BrowseView {
                        dir: &dir,
                        show_hidden: false,
                        keyword: "",
                        page: 1,
                    },
                )
                .await
            }
            ViewerOpenRead::File { path, mut classified } => {
                let name = crate::feishu::card::command::file_display_name(&path);
                let ext = path.extension().and_then(|extension| extension.to_str());
                let (lane, overflowed) = crate::bridge::viewer::open_viewer_lanes(
                    &self.feishu,
                    &self.cards_handle(),
                    &card_message_id,
                    &name,
                    ext,
                    &mut classified,
                    crate::bridge::viewer::ViewerLaneOptions {
                        timeout_ms,
                        existing_rich_message_id: None,
                        existing_sent_file_identity: None,
                        existing_ambiguous_file_identity: None,
                        existing_embedded_image_key: None,
                    },
                )
                .await;
                crate::feishu::card::command::build_viewer_for_classified(
                    &thread_key,
                    &path,
                    &classified,
                    lane,
                    browse_view,
                    crate::feishu::card::command::ViewerBuildOptions {
                        page,
                        overflowed,
                        session_dir: session_dir.as_deref(),
                        // A fresh open leads with the default view (#781): the
                        // rendered markdown for a `.md` file that fits, source
                        // otherwise.
                        view: None,
                        notice: None,
                    },
                )
            }
        };
        if standalone {
            if !matches!(
                crate::bridge::bounded_call(
                    "standalone viewer open card reply",
                    timeout_ms,
                    self.feishu.reply_card(&card_message_id, &card),
                )
                .await,
                Some(Ok(_))
            ) {
                tracing::warn!("standalone viewer open card reply to {card_message_id} did not land");
            }
        } else if !matches!(
            crate::bridge::bounded_call(
                "browse viewer open card update",
                timeout_ms,
                self.feishu.update_message(&card_message_id, &card),
            )
            .await,
            Some(Ok(()))
        ) {
            tracing::warn!("browse viewer open card update for {card_message_id} did not land");
        }
    }

    async fn patch_viewer_open_failure(&self, failure: ViewerOpenFailure<'_>) {
        let ViewerOpenFailure {
            thread_key,
            resolved,
            path,
            card_message_id,
            standalone,
            return_to,
            line,
            timeout_ms,
        } = failure;
        let return_to = return_to.dir.as_deref().map(|dir| BrowseView {
            dir,
            show_hidden: return_to.show_hidden,
            keyword: &return_to.keyword,
            page: return_to.page,
        });
        let card = crate::feishu::card::viewer::build_viewer_failure_card(thread_key, path, line, return_to);
        if standalone {
            if !matches!(
                crate::bridge::bounded_call(
                    "standalone viewer open failure card reply",
                    timeout_ms,
                    self.feishu.reply_card(card_message_id, &card),
                )
                .await,
                Some(Ok(_))
            ) {
                tracing::warn!(
                    "standalone viewer open failure card reply for {} did not land",
                    resolved.display()
                );
            }
        } else if !matches!(
            crate::bridge::bounded_call(
                "browse viewer open failure card update",
                timeout_ms,
                self.feishu.update_message(card_message_id, &card),
            )
            .await,
            Some(Ok(()))
        ) {
            tracing::warn!(
                "browse viewer open failure card update for {} did not land",
                resolved.display()
            );
        }
    }

    /// The claim-and-dispatch preamble the Error card's retry (spec #391) and
    /// the Unreceived card's 重新发起 (#437) share: claim `from`'s recovery
    /// action (the atomic double-click gate), resolve the thread and directory
    /// the spawned pipeline needs, and refuse a Session that already has a
    /// prompt in flight. Every refusal releases the claim, so a later click can
    /// try again. `label` names the action in the warn lines ("retry" /
    /// "resume"); the callers spawn their own pipelines and ack with their own
    /// toasts.
    pub(crate) async fn claim_recovery_click(
        self: &Arc<Self>,
        value: &serde_json::Value,
        from: crate::feishu::card::CardState,
        label: &str,
    ) -> Option<RecoveryClick> {
        let sid = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if sid.is_empty() {
            return None;
        }
        let handles = self.turn_handles();
        let Some(recovery) =
            crate::bridge::turn::Turn::claim_recovery(&handles.cards, &sid, from.clone()).await
        else {
            tracing::warn!("{label}: no {from:?} card for session {sid}");
            return None;
        };
        let inflight = { self.inflight.lock().await.contains(&sid) };
        let (thread_key, directory) = {
            let sessions = self.sessions.lock().await;
            (
                sessions.thread_for_session(&sid),
                sessions.directory_for_session(&sid),
            )
        };
        let Some(thread_key) = thread_key else {
            crate::bridge::turn::Turn::release_recovery_claim(&handles.cards, &sid).await;
            tracing::warn!("{label}: no thread mapped for session {sid}");
            return None;
        };
        if inflight {
            crate::bridge::turn::Turn::release_recovery_claim(&handles.cards, &sid).await;
            tracing::warn!("{label}: session {sid} already has a prompt in flight");
            return None;
        }
        Some(RecoveryClick {
            recovery,
            thread_key,
            directory,
            handles,
        })
    }

    /// Claim one Turn Diff delivery. `None` means a delivery for the same
    /// Turn is already in flight (the guard's Drop releases a failed one).
    pub(crate) fn claim_turn_diff(&self, key: &(String, String)) -> Option<TurnDiffClaim> {
        let mut sends = self
            .turn_diff_sends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sends.contains_key(key) {
            return None;
        }
        while sends.len() >= crate::bridge::handles::FILE_MESSAGE_SENDS_CAP
            && crate::bridge::handles::evict_oldest_idle(&mut sends, |state| {
                *state != TurnDiffSendState::Sending
            })
        {}
        sends.insert(key.clone(), TurnDiffSendState::Sending);
        Some(TurnDiffClaim {
            sends: Arc::clone(&self.turn_diff_sends),
            key: key.clone(),
        })
    }

    /// The Toast a repeated Turn Diff click answers from its stored state,
    /// `None` when the Turn has no delivery recorded (#801).
    pub(crate) fn turn_diff_state_toast(&self, key: &(String, String)) -> Option<&'static str> {
        self.turn_diff_sends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .copied()
            .map(turn_diff_state_toast)
    }

    /// Mark a delivery's terminal state. The RAII claim leaves terminal states
    /// in place (only `Sending` is removed on drop).
    fn set_turn_diff_state(&self, key: &(String, String), state: TurnDiffSendState) {
        let mut sends = self
            .turn_diff_sends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = sends.get_mut(key) {
            *entry = state;
        }
    }

    /// The off-callback Turn Diff delivery (#801): read the Turn's diff (V2's
    /// `session.diff`; V1 has none, ADR-0087), render the
    /// self-contained HTML artifact, upload it once and post exactly ONE File
    /// Message under the clicked Turn card. Every network step is bounded by
    /// the viewer transfer's bound; every failure is spoken, never dropped.
    pub(crate) async fn run_turn_diff_delivery(self: &Arc<Self>, click: TurnDiffClick) {
        let key = (click.session_id.clone(), click.from.clone());
        let timeout_ms = self
            .viewer_transfer_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let turn = match self.read_turn_diff(&click, timeout_ms).await {
            Ok(turn) => turn,
            Err(()) => {
                self.reply_turn_diff_line(&click.message_id, TURN_DIFF_READ_FAILURE_TEXT, timeout_ms)
                    .await;
                return;
            }
        };
        if turn.diff.is_empty() {
            // The empty answer is terminal for this Turn: mark it before the
            // reply, so a repeated click is a no-op instead of a second read.
            self.set_turn_diff_state(&key, TurnDiffSendState::Empty);
            self.reply_turn_diff_line(&click.message_id, TURN_DIFF_EMPTY_TEXT, timeout_ms)
                .await;
            return;
        }
        let document = turn_diff_document(&turn.diff, &click, turn.anchor_ms);
        let html = crate::backend::render_diff_html(&document);
        let name = turn_diff_file_name(&click.session_id, &click.from);
        let delivery = crate::bridge::viewer::send_html_file(
            &self.feishu,
            &self.cards_handle(),
            &click.message_id,
            &name,
            html,
            timeout_ms,
        )
        .await;
        match delivery {
            crate::feishu::card::tool_render::FileDelivery::SentAsFile => {
                self.set_turn_diff_state(&key, TurnDiffSendState::Sent);
            }
            crate::feishu::card::tool_render::FileDelivery::FileSendOutcomeUnknown => {
                self.set_turn_diff_state(&key, TurnDiffSendState::OutcomeUnknown);
                self.reply_turn_diff_line(
                    &click.message_id,
                    crate::bridge::viewer::FILE_SEND_UNKNOWN_TEXT,
                    timeout_ms,
                )
                .await;
            }
            _ => {
                // A definite non-send: the RAII claim removes the `Sending`
                // entry, so a later click may retry — nothing escaped.
                self.reply_turn_diff_line(&click.message_id, TURN_DIFF_FAILURE_TEXT, timeout_ms)
                    .await;
            }
        }
    }

    /// Read one Turn Diff. V2's session diff (scoped to the Turn's snapshot by
    /// `from`) is the only source (ADR-0087): a generation without the endpoint
    /// (`Ok(None)`) is a failed read, never a tool-metadata fallback. The
    /// transcript is read once for the Turn's server-time anchor (the header's
    /// turn time); a failed transcript only makes the time unknown, never fails
    /// the artifact. `Err` is a failed read the caller speaks as a read failure.
    async fn read_turn_diff(
        self: &Arc<Self>,
        click: &TurnDiffClick,
        timeout_ms: u64,
    ) -> std::result::Result<TurnDiffRead, ()> {
        let transcript = self.read_turn_diff_transcript(click, timeout_ms).await;
        let anchor = transcript
            .as_ref()
            .and_then(|transcript| turn_diff_anchor(transcript, &click.from));
        let anchor_ms = anchor.as_ref().map(|anchor| anchor.created_ms);
        let from = (!click.from.is_empty()).then_some(click.from.as_str());
        match crate::bridge::bounded_call(
            "turn diff read",
            timeout_ms,
            self.client.session_diff(&click.session_id, from, None, Some(3)),
        )
        .await
        {
            Some(Ok(Some(diff))) => Ok(TurnDiffRead { diff, anchor_ms }),
            // V1 answers `Ok(None)` without a request: this card could only be
            // a legacy button from the unreleased feature, and there is no
            // artifact source to fall back to.
            Some(Ok(None)) => {
                tracing::warn!(
                    "turn diff: {} has no session diff endpoint (V1)",
                    click.session_id
                );
                Err(())
            }
            Some(Err(error)) => {
                tracing::warn!("turn diff: session diff read failed: {error}");
                Err(())
            }
            None => Err(()),
        }
    }

    /// The transcript read the click needs for the Turn's time. A failed read is
    /// logged and reported as `None`, never propagated: the artifact is still
    /// produced with an unknown time.
    async fn read_turn_diff_transcript(
        self: &Arc<Self>,
        click: &TurnDiffClick,
        timeout_ms: u64,
    ) -> Option<crate::backend::SessionTranscript> {
        match crate::bridge::bounded_call(
            "turn diff transcript",
            timeout_ms,
            self.client.transcript(&click.session_id),
        )
        .await
        {
            Some(Ok(transcript)) => Some(transcript),
            Some(Err(error)) => {
                tracing::warn!("turn diff: transcript read failed: {error}");
                None
            }
            None => None,
        }
    }

    /// Speak a Turn Diff outcome under the clicked card, bounded like every
    /// other delivery network step.
    async fn reply_turn_diff_line(self: &Arc<Self>, message_id: &str, line: &str, timeout_ms: u64) {
        if !matches!(
            crate::bridge::bounded_call(
                "turn diff reply",
                timeout_ms,
                self.feishu.reply_text(message_id, line),
            )
            .await,
            Some(Ok(_))
        ) {
            tracing::warn!("turn diff: reply for {message_id} did not land");
        }
    }

    /// The 「发为文件」/「🌐 预览网页」 transfer, off the card callback (ADR-0083
    /// §5, #778): re-read the file locally, upload its bytes once and post
    /// exactly ONE File Message under the clicked card. Every network call is
    /// bounded by [`SharedCore::viewer_transfer_timeout_ms`], so a failure or a
    /// timeout is stated rather than dropped.
    ///
    /// What the outcome does depends on `op`. `Send` — the viewer's own button —
    /// PATCHes the clicked viewer card with the outcome: `未发送` for a file past
    /// the 30 MB cap or definite failure, or an honest unknown after an ambiguous
    /// timeout; a path that vanished between the viewer and the click has no lane
    /// to report, and its own failure line IS the PATCHed card's body, so the
    /// click's outcome is stated either way (finding #739 §4). `Preview` — a Tool
    /// Panel's button — PATCHes nothing (the clicked message is the agent's Turn
    /// card): a landed File Message is its own evidence, and a failure is spoken
    /// as a reply (see [`Self::finish_viewer_preview`]). All local filesystem work
    /// is done in the bounded blocking-read pool below.
    pub(crate) async fn run_viewer_file_transfer(
        self: &Arc<Self>,
        thread_key: ThreadKey,
        path: String,
        message_id: String,
        content_identity: u64,
        value: serde_json::Value,
        op: ViewerFileOp,
    ) {
        let resolved = std::path::PathBuf::from(&path);
        let timeout_ms = self
            .viewer_transfer_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        // The browse listing the viewer was opened from, when it was — carried
        // on the transfer button's payload so the PATCHed card keeps
        // 「← 返回目录」 (ADR-0083 §3, ADR-0052).
        let return_dir = value
            .get("return_dir")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let return_keyword = value
            .get("return_keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let return_to = return_dir.as_deref().map(|dir| BrowseView {
            dir,
            show_hidden: return_hidden(&value),
            keyword: &return_keyword,
            page: return_page_num(&value),
        });
        // Stat, classification, and the transfer's fresh text snapshot may all
        // block on a host/network filesystem. Keep the entire filesystem phase
        // in the bounded blocking pool and time out its JoinHandle before doing
        // any network delivery; a timed-out closure keeps its semaphore slot
        // until the OS read actually returns, while this task can PATCH failure
        // and drop its in-flight guard.
        let read_path = resolved.clone();
        let read_name = crate::feishu::card::command::file_display_name(&resolved);
        let read = bounded_viewer_fs_read(
            self.cards_handle().viewer_fs_read_slots.clone(),
            timeout_ms,
            "⚠️ 文件读取繁忙，请稍后再试。",
            &format!("⚠️ 无法读取 `{}`：文件读取超时。", resolved.display()),
            move || {
                let meta = std::fs::metadata(&read_path)
                    .map_err(|error| crate::bridge::viewer::stat_failure(&error))?;
                let mut classified = crate::bridge::viewer::classify(&read_path, meta.len())?;
                let prepared =
                    crate::bridge::viewer::prepare_file_transfer(&read_path, &read_name, &mut classified);
                Ok::<_, crate::bridge::viewer::CatFailure>((classified, prepared))
            },
        )
        .await;
        let read = match read {
            Ok(Ok(snapshot)) => Ok(snapshot),
            Ok(Err(failure)) => Err(failure.line(&resolved)),
            Err(ViewerFsReadError::Busy(line) | ViewerFsReadError::Timeout(line)) => Err(line),
            Err(ViewerFsReadError::Worker(error)) => {
                Err(format!("⚠️ 无法读取 `{}`：{error}", resolved.display()))
            }
        };
        if op == ViewerFileOp::Preview {
            self.finish_viewer_preview(
                &thread_key,
                &resolved,
                &message_id,
                content_identity,
                read,
                timeout_ms,
            )
            .await;
            return;
        }
        let card = match read {
            Ok((classified, prepared)) => {
                let mut lane = crate::bridge::viewer::send_prepared_file_transfer(
                    &self.feishu,
                    &self.cards_handle(),
                    &message_id,
                    prepared,
                    timeout_ms,
                )
                .await;
                lane.embedded_image_key = value
                    .get("viewer_embedded_image_key")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                // The transferred bytes are an ADDITION to what the viewer
                // already delivered: an existing Rich Text Message survives
                // (its stale preview is refreshed on the next re-read), so the
                // rebuilt card must keep tracking it — otherwise that re-read
                // posts a second copy and orphans the first (#744, Codex gate
                // round 7).
                lane.rich_message_id = value
                    .get("viewer_lane_rich_message_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                lane.refresh_failed = value
                    .get("viewer_lane_refresh_failed")
                    .and_then(serde_json::Value::as_str)
                    == Some("true");
                // The viewer's own lane reports the same delivered/ambiguous
                // outcome through the shared memory the panel preview uses, so the
                // two cannot drift on what counts as sent.
                self.remember_viewer_file_delivery(
                    &thread_key,
                    &message_id,
                    &resolved,
                    content_identity,
                    &lane,
                );
                let session_dir = if let Some(dir) = value
                    .get("session_dir")
                    .and_then(|v| v.as_str())
                    .filter(|dir| !dir.is_empty())
                {
                    Some(dir.to_string())
                } else {
                    self.sessions.lock().await.current_directory(&thread_key)
                };
                crate::feishu::card::command::build_viewer_for_classified(
                    &thread_key,
                    &resolved,
                    &classified,
                    Some(lane),
                    return_to,
                    crate::feishu::card::command::ViewerBuildOptions {
                        page: viewer_page(&value),
                        overflowed: false,
                        session_dir: session_dir.as_deref(),
                        // The transfer's PATCH rebuilds the card it was clicked
                        // on: keep the markdown view the reader chose (#781).
                        view: crate::feishu::card::viewer::ViewerView::from_payload(&value),
                        notice: None,
                    },
                )
            }
            Err(line) => {
                crate::feishu::card::viewer::build_viewer_failure_card(&thread_key, &path, &line, return_to)
            }
        };
        if !matches!(
            crate::bridge::bounded_call(
                "viewer transfer card update",
                timeout_ms,
                self.feishu.update_message(&message_id, &card),
            )
            .await,
            Some(Ok(()))
        ) {
            tracing::warn!("viewer transfer: card update for {message_id} did not land");
        }
    }

    /// The tail of a Tool Panel's 「🌐 预览网页」 transfer (#778): post the file as
    /// one File Message and report ONLY off the card — the clicked message is the
    /// agent's Turn card, so no PATCH may touch it.
    ///
    /// A landed (or ambiguous) send is remembered on the same bounded window as
    /// every other viewer File Message ([`ViewerFileTransferKey`], keyed by the
    /// path too because one Turn card can carry several file panels), so a second
    /// click on the same panel does not post a duplicate — the button stays on the
    /// card, which no PATCH rewrites. With no card of cola's own to write `未发送`
    /// or `发送结果未知` on, both a definite failure and an ambiguous timeout are
    /// spoken as a reply, so the click's outcome is never silent; a definite
    /// failure is deduped on nothing, leaving the button live for a retry.
    async fn finish_viewer_preview(
        self: &Arc<Self>,
        thread_key: &ThreadKey,
        resolved: &std::path::Path,
        message_id: &str,
        content_identity: u64,
        read: Result<
            (
                crate::bridge::viewer::Classified,
                crate::bridge::viewer::PreparedFileTransfer,
            ),
            String,
        >,
        timeout_ms: u64,
    ) {
        let (classified, prepared) = match read {
            Ok(snapshot) => snapshot,
            Err(line) => {
                self.reply_viewer_preview_outcome(message_id, &line, timeout_ms)
                    .await;
                return;
            }
        };
        // A snapshot with no bytes can only be a definite non-send (past the cap,
        // empty, unreadable); every other `Undelivered` after this is a failed
        // upload or File Message send, which the reason must not misname.
        let snapshot_undeliverable = !prepared.carries_bytes();
        let lane = crate::bridge::viewer::send_prepared_file_transfer(
            &self.feishu,
            &self.cards_handle(),
            message_id,
            prepared,
            timeout_ms,
        )
        .await;
        // A delivered — or possibly-delivered — File Message is remembered, and an
        // ambiguous one is spoken; a definite non-send falls through to its reply.
        if self.remember_viewer_file_delivery(thread_key, message_id, resolved, content_identity, &lane) {
            if matches!(
                lane.delivery,
                crate::feishu::card::tool_render::FileDelivery::FileSendOutcomeUnknown
            ) {
                // There is no card of cola's own to state an ambiguous outcome
                // on, and staying silent would read as a silent failure. The
                // shared line carries this surface's warning prefix.
                let line = format!("⚠️ {}", crate::bridge::viewer::FILE_SEND_UNKNOWN_TEXT);
                self.reply_viewer_preview_outcome(message_id, &line, timeout_ms)
                    .await;
            }
            return;
        }
        let line = format!(
            "⚠️ 无法发送 `{}`：{}。",
            resolved.display(),
            if snapshot_undeliverable {
                viewer_preview_failure_reason(classified.size)
            } else {
                "发送失败"
            }
        );
        self.reply_viewer_preview_outcome(message_id, &line, timeout_ms)
            .await;
    }

    /// Remember a viewer File Message that LANDED — or may have (an ambiguous
    /// timeout) — in the same bounded deliver-once window every viewer transfer
    /// shares (#739, #778), under BOTH the identity the click asked for and the
    /// identity the bytes actually carried, so a stale callback for either cannot
    /// post a duplicate. Returns whether anything was remembered: a definite
    /// non-send (`Undelivered`) remembers nothing, leaving the button live for a
    /// retry. One rule for the viewer's own lane and a Tool Panel's preview, so the
    /// two cannot drift on what counts as delivered.
    fn remember_viewer_file_delivery(
        &self,
        thread_key: &ThreadKey,
        message_id: &str,
        path: &std::path::Path,
        content_identity: u64,
        lane: &crate::feishu::card::viewer::ViewerFileLane,
    ) -> bool {
        use crate::feishu::card::tool_render::FileDelivery;
        let status = match &lane.delivery {
            FileDelivery::SentAsFile => ViewerFileTransferStatus::Sent,
            FileDelivery::FileSendOutcomeUnknown => ViewerFileTransferStatus::OutcomeUnknown,
            _ => return false,
        };
        let mut sent = self
            .viewer_file_transfers_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key_path = path.to_string_lossy().to_string();
        let key = (thread_key.chat_id.clone(), message_id.to_string(), key_path);
        remember_viewer_file_transfer(
            &mut sent,
            (key.0.clone(), key.1.clone(), key.2.clone(), content_identity),
            status,
            crate::bridge::handles::FILE_MESSAGE_SENDS_CAP,
        );
        if let Some(delivered_identity) = lane.content_identity {
            remember_viewer_file_transfer(
                &mut sent,
                (key.0, key.1, key.2, delivered_identity),
                status,
                crate::bridge::handles::FILE_MESSAGE_SENDS_CAP,
            );
        }
        true
    }

    /// Speak a Tool Panel preview's outcome under the clicked card (#778) — a
    /// definite failure, or an ambiguous send. With no card of cola's own to write
    /// `未发送`/`发送结果未知` on, the reply IS the outcome — bounded like every
    /// other transfer network step, so a wedged call cannot outlive its timeout.
    async fn reply_viewer_preview_outcome(self: &Arc<Self>, message_id: &str, line: &str, timeout_ms: u64) {
        if !matches!(
            crate::bridge::bounded_call(
                "viewer preview failure reply",
                timeout_ms,
                self.feishu.reply_text(message_id, line),
            )
            .await,
            Some(Ok(_))
        ) {
            tracing::warn!("viewer preview: failure reply for {message_id} did not land");
        }
    }

    /// The 「📄 完整输出」 transfer itself, off the card callback. One bounded
    /// local read (the delivery copy and the display tail), one File Message
    /// through the viewer's lane, then one PATCH onto the clicked card carrying
    /// the outcome (ADR-0086 decision 7). Past the platform's 30 MB cap the copy
    /// is the log's first 30 MB plus a truncation line — the log on disk is
    /// never truncated, because the child's descriptor offset does not move
    /// back.
    pub(crate) async fn run_terminal_full_output(self: &Arc<Self>, request: TerminalCardRequest) {
        let timeout_ms = self
            .viewer_transfer_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let read_path = std::path::PathBuf::from(&request.path);
        let read = bounded_viewer_fs_read(
            self.cards_handle().viewer_fs_read_slots.clone(),
            timeout_ms,
            "⚠️ 终端日志读取繁忙，请稍后再试。",
            "⚠️ 读取终端日志超时。",
            move || -> std::io::Result<(crate::bridge::terminal::FullOutput, String, bool)> {
                let copy = crate::bridge::terminal::read_full_output(&read_path)?;
                let (tail, truncated) = crate::bridge::terminal::read_tail(
                    &read_path,
                    crate::feishu::card::terminal::TERMINAL_TAIL_CHARS,
                )?;
                Ok((copy, tail, truncated))
            },
        )
        .await;
        let read = match read {
            Ok(Ok(read)) => read,
            Ok(Err(error)) => {
                // A failed read has no outcome to PATCH onto the card: leaving
                // the card untouched keeps the tail the reader already sees
                // instead of rebuilding it as "无输出". The reply IS the
                // click's result.
                self.reply_terminal_outcome(
                    &request.message_id,
                    &format!(
                        "⚠️ 无法读取终端日志（{error}）。文件应在 `{}`（可能已被清理）。",
                        request.path
                    ),
                    timeout_ms,
                )
                .await;
                return;
            }
            Err(ViewerFsReadError::Busy(line) | ViewerFsReadError::Timeout(line)) => {
                self.reply_terminal_outcome(&request.message_id, &line, timeout_ms)
                    .await;
                return;
            }
            Err(ViewerFsReadError::Worker(error)) => {
                self.reply_terminal_outcome(
                    &request.message_id,
                    &format!("⚠️ 无法读取终端日志（{error}）。文件在 `{}`。", request.path),
                    timeout_ms,
                )
                .await;
                return;
            }
        };
        let (copy, tail, tail_truncated) = read;
        let notice = {
            let crate::bridge::terminal::FullOutput {
                bytes,
                total_size,
                truncated,
            } = copy;
            // The File Message's name is the log's own: it already names
            // the run (`<run-id>.log`), so the delivered artifact and the
            // path the card names cannot drift.
            let name = std::path::Path::new(&request.path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "terminal.log".to_string());
            let content = crate::backend::FileContent::from_bytes("text/plain", &name, bytes);
            let hash = crate::bridge::handles::content_identity_hash(
                &content.mime,
                &content.name,
                content.data.as_deref().unwrap_or_default(),
            );
            let delivery = crate::bridge::viewer::send_file_message(
                &self.feishu,
                &self.cards_handle(),
                &request.message_id,
                &content,
                hash,
                timeout_ms,
            )
            .await;
            use crate::feishu::card::tool_render::FileDelivery;
            crate::feishu::card::terminal::full_output_notice(match delivery {
                FileDelivery::SentAsFile => crate::feishu::card::terminal::FullOutputOutcome::Sent {
                    truncated,
                    total_size,
                    path: &request.path,
                },
                FileDelivery::FileSendOutcomeUnknown => {
                    crate::feishu::card::terminal::FullOutputOutcome::Unknown { path: &request.path }
                }
                _ => crate::feishu::card::terminal::FullOutputOutcome::NotSent {
                    reason: "上传或发送失败",
                    path: &request.path,
                },
            })
        };
        self.update_terminal_card_outcome(
            &request,
            &tail,
            tail_truncated,
            &notice,
            "terminal full output card",
        )
        .await;
    }

    /// Rebuild the clicked settled Terminal Card from its own payload and PATCH
    /// one outcome line onto it (ADR-0086 decision 4: the card carries its own
    /// render inputs, so no card-view read is needed). The rebuilt buttons keep
    /// the run id the payload named — and a legacy payload that named none gets
    /// no 「问 AI」 at all, because adopting the chat's current id would let an
    /// old card submit after its record was replaced. A failed PATCH is logged,
    /// never retried: the outcome is information, not an act.
    async fn update_terminal_card_outcome(
        self: &Arc<Self>,
        request: &TerminalCardRequest,
        output: &str,
        output_truncated: bool,
        notice: &str,
        what: &'static str,
    ) {
        let timeout_ms = self
            .viewer_transfer_timeout_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        // #828: a payload minted before the settled buttons named their run
        // carries no run id. The record is read for the card's ending, never to
        // fill that id in — see below.
        let record = self.terminal_records.read(&ThreadKey::new(
            request.chat_id.clone(),
            request.thread_id.clone(),
        ));
        // The run id every rebuilt button names is the **payload's own**. A
        // legacy payload has none, and the chat's current record may name a
        // newer run: adopting that id here would let the old card's 「问 AI」
        // pass the current-run check and submit the old command's output
        // (spec #827: a click whose run id is not the current record id is
        // stale). So it is never invented.
        let run_id = request.run_id.clone();
        // The ending must belong to the clicked card. A payload's run id names
        // it; a legacy payload names no run, so only a record whose **own card
        // id** is this card can speak for it — never a successor's card.
        let record = record.filter(|record| {
            if run_id.is_empty() {
                record.card_id == request.message_id
            } else {
                record.run_id == run_id
            }
        });
        // The ending is the **record's**, not the payload's: a startup failure
        // and an unconfirmed termination live only there, and a rebuild that
        // invented a clean ending would misreport the run (spec #825). The
        // payload is not trusted to carry them; it is only a route.
        let outcome = (!request.lost)
            .then(|| record.as_ref().and_then(|record| record.outcome.as_ref()))
            .flatten();
        if !request.lost && outcome.is_none() {
            // No durable ending for this run (the record expired, a successor
            // replaced it, or it cannot be read). Rebuilding would have to clear
            // the startup failure or the survivor warning the clicked card
            // shows, so leave that card exactly as it is and speak the outcome
            // under it instead.
            self.reply_terminal_outcome(&request.message_id, notice, timeout_ms)
                .await;
            return;
        }
        let exit_code = outcome
            .map(|outcome| outcome.exit_code)
            .unwrap_or(request.exit_code);
        let elapsed = outcome
            .map(|outcome| std::time::Duration::from_millis(outcome.elapsed_ms))
            .unwrap_or(request.elapsed);
        let output_over_limit = outcome
            .map(|outcome| outcome.output_over_limit)
            .unwrap_or(request.output_over_limit);
        // The rebuilt card keeps every settled button and its run id, and a run
        // the sweep marked lost is rebuilt AS a lost card: the outcome PATCH
        // must not invent an ending for it (ADR-0086 decisions 4, 5 and 10).
        let full_output = Some(crate::feishu::card::terminal::TerminalFullOutput {
            log_path: &request.path,
            run_id: &run_id,
            // 重跑 replays the record, so it may only name a run the card
            // itself named: the fallback above can be a newer, different
            // command.
            rerun_run_id: (!request.run_id.is_empty()).then_some(request.run_id.as_str()),
            chat_id: &request.chat_id,
            thread_id: &request.thread_id,
            lost: request.lost,
            is_group: request.is_group,
            exit_code,
            elapsed,
            output_over_limit,
        });
        let state = match outcome {
            Some(outcome) => crate::feishu::card::terminal::TerminalState::Settled {
                exit_code: outcome.exit_code,
                spawn_error: outcome.spawn_error.as_deref(),
                elapsed: std::time::Duration::from_millis(outcome.elapsed_ms),
                stopped: outcome.stopped,
                survivors: outcome.survivors,
            },
            None => crate::feishu::card::terminal::TerminalState::Lost,
        };
        let card =
            crate::feishu::card::terminal::build_terminal_card(crate::feishu::card::terminal::TerminalCard {
                cwd: &request.cwd,
                command: &request.command,
                output,
                output_truncated,
                output_over_limit,
                full_output,
                notice: Some(notice),
                state,
                run_id: &run_id,
                chat_id: &request.chat_id,
                thread_id: &request.thread_id,
            });
        if !matches!(
            crate::bridge::bounded_call(
                what,
                timeout_ms,
                self.feishu.update_message(&request.message_id, &card),
            )
            .await,
            Some(Ok(()))
        ) {
            tracing::warn!("{what}: outcome for {} did not land", request.message_id);
        }
    }

    /// Speak an outcome under the clicked card when there is no new card to
    /// write — a log that could not be read, or a run whose ending is no longer
    /// on record, leaves the settled card's own state (a startup failure, a
    /// survivor warning) untouched instead of rebuilding it cleared. Bounded like
    /// every other delivery step, so the click is never silently dropped.
    async fn reply_terminal_outcome(&self, message_id: &str, line: &str, timeout_ms: u64) {
        if !matches!(
            crate::bridge::bounded_call(
                "terminal full output reply",
                timeout_ms,
                self.feishu.reply_text(message_id, line),
            )
            .await,
            Some(Ok(_))
        ) {
            tracing::warn!("terminal: full-output outcome reply for {message_id} did not land");
        }
    }

    /// The 「问 AI」 submission itself, off the card callback: build the framed
    /// prompt from the button's payload, hand it to the one prompt pipeline,
    /// and PATCH the clicked card with the submission's outcome. `guard` holds
    /// the run's one submission slot; it is released as retryable unless the
    /// pipeline accepted the message.
    pub(crate) async fn run_terminal_ask_ai(
        self: &Arc<Self>,
        request: TerminalAskAiRequest,
        guard: crate::bridge::terminal::AskAiGuard,
    ) {
        let card = &request.card;
        // Rebuild the Chat/Topic route from the payload. A click carries no
        // chat_type of its own, and a non-topic ThreadKey's `thread_id` IS its
        // `chat_id`, so drop it before classifying (the skill picker's rule).
        let thread_id =
            (!card.thread_id.is_empty() && card.thread_id != card.chat_id).then(|| card.thread_id.clone());
        let chat_type = if card.is_group { "group" } else { "p2p" };
        let kind = ConversationKind::classify(chat_type, thread_id.as_deref());
        let thread_key = kind.thread_key(&card.chat_id, thread_id.as_deref());
        let text = crate::bridge::terminal::ask_ai_prompt(
            &card.cwd,
            &card.command,
            card.exit_code,
            &request.output,
            request.output_truncated,
        );
        let msg = crate::bridge::IncomingMessage {
            // The Turn's card replies under the clicked Terminal Card, so the
            // answer lands next to the run it explains.
            message_id: card.message_id.clone(),
            chat_id: card.chat_id.clone(),
            chat_type: chat_type.to_string(),
            thread_id,
            // No Quoted Context: the card's content is the submission itself,
            // not a parent message to fetch.
            parent_id: None,
            text,
            images: Vec::new(),
            files: Vec::new(),
            omitted: Vec::new(),
            requester_open_id: request.requester_open_id.clone(),
        };
        let notice = match self.submit_prompt(thread_key, msg, kind, Vec::new()).await {
            Ok(Some(PromptSubmission::NewTurn)) => {
                guard.submitted();
                "🤖 已作为新的对话提交给 AI（回答会出现在新的卡片上）。"
            }
            Ok(Some(PromptSubmission::Supplement)) => {
                guard.submitted();
                "🤖 已作为补充消息提交给当前运行。"
            }
            // The pipeline rejected the message (or could not start its Turn):
            // the failure was already spoken, and the card must not claim a
            // submission. The guard drops, so a retry is allowed.
            Ok(Some(PromptSubmission::NotAccepted)) => "⚠️ 提交给 AI 失败，请稍后重试。",
            // Nothing was submitted at all (no server): the pipeline already
            // spoke its refusal, and the card must not pretend otherwise.
            Ok(None) => return,
            Err(error) => {
                tracing::warn!("terminal: 问 AI submission failed: {error}");
                "⚠️ 提交给 AI 失败，请稍后重试。"
            }
        };
        self.update_terminal_card_outcome(
            &request.card,
            &request.output,
            request.output_truncated,
            notice,
            "terminal 问 AI card",
        )
        .await;
    }

    /// The cleanup's read → reconcile → retire → re-render pipeline, off the
    /// ack path (spec #588, #590). The click's own read runs the shared,
    /// NON-throttled reconcile ([`crate::bridge::runtime::reconcile_now`]) —
    /// a click must never no-op behind a recent poll's verdict: the live tasks
    /// the runtime cannot confirm yet are then exactly the
    /// [`SessionTranscript::unconfirmed_tasks`], and only those are cleared.
    /// The pass applies its verdicts to the read and is committed to the
    /// ADR-0065 overlay only under the refresh gate below (review, PR #595):
    /// every caller's reconcile pass commits after the flush that carried its
    /// entries, never before. The same read carries the cleared tasks' synthetic
    /// Cleaned retirements, and the ordinary yielded-card refresh renders the
    /// result — rows dropped, one 🧹 entry per cleared task (and per
    /// runtime/evidence retirement the read observed), and the card settled by
    /// the same rules as a quiet true end (the last task gone is ✅ in place,
    /// ADR-0060).
    /// EVERY overlay record the click owes — the runtime/evidence retirements
    /// its pass applied, the cleared ids, and the resolved marker set — is
    /// committed only when that refresh lands (review, PR #595). But the
    /// ENDINGS themselves are durable at derivation (ADR-0080): the reconcile's
    /// evidence endings and this click's own Cleanup endings were recorded the
    /// moment they were derived, so a card that refuses the write — a new Turn
    /// superseded it mid-pipeline — still leaves the tasks ended, their rows
    /// gone from every later read and their entries owed to the successor's own
    /// admitted paths, instead of being hidden without their entries. Only the
    /// overlay cache and the unconfirmed marker wait on the accepted write.
    /// Every verdict that moved the read refreshes the card, a marker-resolving
    /// `Running` verdict included (review, PR #595): the stale ⚠️ 状态待确认 row
    /// and its button leave on the click's own pass. A read that fails, and a
    /// verdict that changed nothing at all, give the claim back so the button
    /// stays usable.
    pub(crate) async fn run_cleanup(
        handles: crate::bridge::handles::TurnHandles,
        session_id: String,
        directory: Option<String>,
    ) {
        let read_timeout_ms = handles.config.follow_read_timeout_ms();
        let transcript = crate::bridge::bounded_call(
            "cleanup transcript",
            read_timeout_ms,
            handles.backend.transcript(&session_id),
        )
        .await;
        let Some(Ok(mut transcript)) = transcript else {
            tracing::warn!("cleanup: could not read session {session_id}");
            crate::bridge::turn::Turn::release_cleanup_claim(&handles.cards, &session_id).await;
            return;
        };
        // The shared reconcile, directly and unthrottled: the same runtime
        // read, positive-evidence verdicts, child evidence and deferred overlay
        // record every poll path runs through `observe`, without the
        // process-wide throttle — a click is a user action, not a poll, and
        // must spend its own read. A failed or timed-out one re-derives
        // nothing and so clears nothing (ADR-0073): the carried markers stay
        // on the card and the claim goes back, keeping the button usable.
        // (`None` is also the zero-live-task case: there is nothing to clear.)
        // The waiting card's Turn is the run scope: a supplement is not a new
        // Turn (ADR-0043), so the newest user message cannot stand in for it.
        let anchor = handles
            .cards
            .card(&session_id)
            .await
            .and_then(|card| card.card_turn_anchor().cloned());
        let verdict = crate::bridge::runtime::reconcile_now(
            &handles.backend,
            &session_id,
            directory.as_deref(),
            &mut transcript,
            // The click's own read budget (spec #588, review PR #595): the
            // verdict and its child-evidence reads share one `read_timeout_ms`
            // window, exactly like a poll cycle's, so a stalled read cannot
            // stack per-suspect timeouts on the click either.
            crate::bridge::runtime::CycleBudget::within(read_timeout_ms),
            anchor.as_ref(),
        )
        .await;
        // The pass's evidence endings are already durable (recorded by
        // `reconcile_now` at derivation); only its process-local overlay record
        // waits, and only on a landed refresh (review, PR #595).
        let Some(pass) = verdict else {
            tracing::warn!("cleanup: no runtime verdict on session {session_id}; clearing nothing");
            crate::bridge::turn::Turn::release_cleanup_claim(&handles.cards, &session_id).await;
            return;
        };
        let changed = pass.changed();
        // Every id the read marks unconfirmed — a task's call id or a
        // child-only run's durable key — is a row the click may clear.
        let cleared: Vec<String> = transcript.unconfirmed_tasks.iter().cloned().collect();
        // The click owes the card a refresh whenever its own verdict moved the
        // read — not only when a row was cleared or retired (spec #588, review
        // PR #595): a verdict that reports a carried child RUNNING resolves the
        // ⚠️ 状态待确认 marker with nothing to clear, and the stale row and its
        // 「清理待确认任务」 button must leave on this click's own refresh, not
        // on a later Session Sync pass. A verdict that changed nothing at all —
        // no marker moved, no retirement — gives the claim back unwritten.
        if cleared.is_empty() && transcript.task_retirements.is_empty() && !changed {
            tracing::warn!("cleanup: no unconfirmed task left on session {session_id}");
            crate::bridge::turn::Turn::release_cleanup_claim(&handles.cards, &session_id).await;
            return;
        }
        // One clock for the click's whole write: the synthetic retirements'
        // own timestamps and the refresh's render clock are the same moment.
        let now_ms = chrono::Utc::now().timestamp_millis();
        // The read's own cleanup rides the local transcript BEFORE the refresh
        // (review, PR #595), and the click's own Cleanup ending is recorded AT
        // DERIVATION (ADR-0080) by the owner's `retire` — OUTSIDE the refresh
        // gate below, because the fact is about the run, not about the card
        // that will show its 🧹 entry. A refresh the chain replaces still
        // leaves the task ended and the entry owed to the next card that can
        // carry it; only the process-local overlay record waits on the refresh
        // (it does NOT sit here — it is gated on the refresh's outcome below).
        handles
            .backend
            .runs()
            .cleanup(&session_id, &mut transcript, &cleared, now_ms);
        // A cleaned run/task no longer needs its child subscription: a Codex
        // child adopted for liveness is released at the retirement — AFTER the
        // ending was recorded, so a stalled or cancelled release cannot lose it.
        // Every Cleared retirement the read carries is released, including one
        // `observe` synthesized from a durable ending an earlier click recorded.
        for retirement in transcript
            .task_retirements
            .iter()
            .filter(|retirement| matches!(retirement.ending, TaskRetirementEnding::Cleaned))
        {
            if let Some(child) = retirement.task.child_id.as_deref() {
                crate::bridge::runtime::release_child_session(&handles.backend, child).await;
            }
        }
        let stopped = handles.waits.is_stopped(&session_id).await;
        let update = crate::bridge::turn::Turn::refresh_yielded_ledger(
            &handles.cards,
            &handles.backend,
            &handles.requests,
            &session_id,
            &transcript,
            now_ms,
            stopped,
            read_timeout_ms,
        )
        .await;
        // EVERY process-local record the click owes is gated on the refresh's
        // outcome (review, PR #595): the durable ENDINGS were already recorded
        // at derivation above, but the `retired` cache and the resolved marker
        // set ride here. The write admission is re-checked under the card lock,
        // and a card replaced while this pipeline's reads were in flight (a new
        // Turn's supersede, a late Wake) refuses the write — `Unchanged`,
        // nothing submitted. A resolved marker recorded anyway would drop the
        // ⚠️ row and its button on a card that never took the resolution; no
        // later read can reconstruct that. An accepted write counts even when
        // the delivery layer drains it later (ADR-0067/0072), the same rule the
        // Wake Watermark's drain uses, while a permanently refused one never
        // writes (the card suspended; review, PR #595): `update.accepted()` is
        // the one gate over the cache. A refusal writes no cache entry — the
        // durable ending already took the task out of the ledger and left its
        // entry owed — and the marker stays as the read left it, so the
        // successor's still-usable button can clear it on the card that took the
        // chain. The commit is one call for the union — the pass's retirements,
        // the cleaned retirements `RunLifecycle::cleanup` recorded and appended
        // above, and the post-verdict marker set — so no part of the cache can
        // outlive the others.
        if update.accepted() {
            pass.commit(handles.backend.runs(), &session_id, &transcript);
        } else {
            tracing::warn!(
                "cleanup: the refresh was not accepted on session {session_id}; \
                 the durable ending stands, the cache and marker are left unwritten"
            );
        }
        tracing::info!(
            "cleanup: cleared {} task(s) on session {session_id} (refreshed: {})",
            cleared.len(),
            update.accepted()
        );
        // The claim's job ends with this write: a later unconfirmed row on the
        // same card (new work going inactive) must be cleanable again, and a
        // refresh that did not land (the card stopped admitting it between the
        // claim and the write) must leave the button usable too. Releasing is
        // harmless on a settled card — no state offers the claim there.
        crate::bridge::turn::Turn::release_cleanup_claim(&handles.cards, &session_id).await;
    }

    /// The retry's read → decide → act pipeline, off the ack path.
    ///
    /// One bounded read pair — session status + session transcript, each with
    /// the follow's per-read timeout — then the matrix (spec #391): a live run
    /// is re-attached and submits nothing; a settled turn takes a fresh
    /// `msg_cola_` id; a never-admitted id reuses the failed attempt's id on
    /// either generation, while an admitted-but-unfinished turn reuses it only
    /// where the generation's re-post still runs (V1 — on V2 the same re-post
    /// would be a silent no-op, so it takes a fresh id); a read that failed or
    /// timed out is unknown and submits a fresh id, so a transient hiccup
    /// never recreates the "click does nothing" bug. The submit branches hand
    /// off to `Turn::start`, which marks the failed card `Retried` once it
    /// holds the session's inflight guard and replies the attempt's new card.
    pub(crate) async fn run_retry(
        self: Arc<Self>,
        handles: crate::bridge::handles::TurnHandles,
        retry: crate::bridge::turn::TurnRecovery,
        thread_key: ThreadKey,
        directory: Option<String>,
    ) {
        let read_timeout_ms = handles.config.follow_read_timeout_ms();
        // Each read is bounded on its own: a hung status must not swallow the
        // transcript read, and vice versa. A failed, timed-out or unrecognised
        // status read is UNKNOWN (`None`) — the matrix then submits a fresh id.
        let status = read_session_status(
            "retry session status",
            read_timeout_ms,
            &handles.backend,
            &retry.session_id,
            directory.as_deref(),
        )
        .await;
        let transcript = match crate::bridge::bounded_call(
            "retry transcript",
            read_timeout_ms,
            handles.backend.transcript(&retry.session_id),
        )
        .await
        {
            Some(Ok(transcript)) => Some(transcript),
            Some(Err(e)) => {
                tracing::warn!("retry transcript: {}", e);
                None
            }
            None => None,
        };
        let decision = crate::bridge::turn::retry_decision(
            status,
            transcript.as_ref(),
            retry.cola_message_id.as_deref(),
            handles.backend.reuse_continues_an_admitted_turn(),
        );
        let cola_message_id = match decision {
            crate::bridge::turn::RetryDecision::Busy => {
                // The run is alive: no prompt. Re-attach the card to it — the
                // Error is cleared, a live header state is restored, and the
                // out-of-turn follow keeps rendering on the accumulator's own
                // anchor until the run really ends (spec #391, ticket #393).
                let attached = crate::bridge::turn::Turn::reattach_run(
                    &handles,
                    &retry.session_id,
                    &thread_key,
                    directory,
                )
                .await;
                if !attached {
                    // No live card, or no anchor to follow: give the claim
                    // back so the Error card's retry still works.
                    crate::bridge::turn::Turn::release_recovery_claim(&handles.cards, &retry.session_id)
                        .await;
                }
                tracing::info!(
                    "retry: session {} still busy; re-attached={}",
                    retry.session_id,
                    attached
                );
                return;
            }
            crate::bridge::turn::RetryDecision::NewId => None,
            crate::bridge::turn::RetryDecision::Reuse(id) => Some(id),
        };
        // `Turn::start` marks the failed card Retried once it holds the
        // inflight guard, so a retry that loses the guard never marks without
        // submitting.
        if let Err(e) = self
            .run_prompt(retry.into_context(thread_key, cola_message_id))
            .await
        {
            tracing::error!("retry prompt: {}", e);
        }
    }

    /// The resume pipeline, off the ack path.
    ///
    /// V2 (the generation has the resume write): interrupt first — a no-op
    /// when the Session is already idle, but exactly what frees a stale run
    /// still holding it — then resume, which promotes the message already
    /// queued in the session inbox at the new run's start. On success the SAME
    /// card is re-armed live and the unreceived watch renders the resumed run,
    /// capturing the Turn anchor the moment the promoted message lands
    /// (ADR-0062). The click never re-submits the message: the queued steer is
    /// already durable, so nothing is duplicated.
    ///
    /// V1 (no resume endpoint): the message is resubmitted as a new Turn under
    /// its own `msg_cola_` id. V1's prompt upserts by id
    /// (`Backend::reuse_continues_an_admitted_turn`), so the re-post stays
    /// idempotent even if the message landed between the Unreceived ending and
    /// the click — the action cannot duplicate a transcript message either way.
    ///
    /// A failed write releases the claim and leaves the card Unreceived: the
    /// button keeps working for a later press, never a dead end.
    pub(crate) async fn run_resume(
        self: Arc<Self>,
        handles: crate::bridge::handles::TurnHandles,
        recovery: crate::bridge::turn::TurnRecovery,
        thread_key: ThreadKey,
        directory: Option<String>,
    ) {
        let session_id = recovery.session_id.clone();
        if !handles.backend.resume_supported() {
            // V1 degradation (#437, ADR-0062): no resume endpoint, so
            // resubmit the already-admitted message as a new Turn. The id is
            // reused unconditionally — deliberately NOT `retry_decision`'s
            // matrix. The Unreceived ending means the message never landed (an
            // unadmitted id, the reuse cell on either generation); reusing it
            // also keeps the action idempotent if the message DID land between
            // the ending and the click, since V1's prompt upserts by id
            // (`Backend::reuse_continues_an_admitted_turn`). In every shape the
            // user's message is answered and never duplicated: unlanded → the
            // upsert creates and runs it; landed-unsettled → the upsert
            // continues the turn; landed-and-settled → it was already answered
            // (the stale card simply never showed it).
            tracing::info!("resume: session {session_id} has no resume endpoint; resubmitting as a new Turn");
            let cola_message_id = recovery.cola_message_id.clone();
            if let Err(e) = self
                .run_prompt(recovery.into_context(thread_key, cola_message_id))
                .await
            {
                tracing::error!("resume resubmit: {}", e);
            }
            return;
        }
        // The click IS the user's consent: interrupt the stale run (idle is a
        // success no-op), then resume so the queued message is promoted at the
        // new run's start.
        if let Err(e) = handles.backend.interrupt(&session_id).await {
            tracing::warn!("resume: interrupt failed for session {session_id}: {e}");
            crate::bridge::turn::Turn::release_recovery_claim(&handles.cards, &session_id).await;
            return;
        }
        if let Err(e) = handles.backend.resume(&session_id).await {
            tracing::warn!("resume: resume failed for session {session_id}: {e}");
            crate::bridge::turn::Turn::release_recovery_claim(&handles.cards, &session_id).await;
            return;
        }
        tracing::info!("resume: session {session_id} interrupted then resumed; watching the queued message");
        if !crate::bridge::turn::Turn::reattach_resumed(&handles, &session_id, &thread_key, directory).await {
            // The card was replaced between the claim and the writes (a new
            // Turn owns the session now): nothing to re-arm. The resumed
            // message still runs; whoever owns the card renders it.
            crate::bridge::turn::Turn::release_recovery_claim(&handles.cards, &session_id).await;
        }
    }
}

#[cfg(test)]
mod turn_diff_delivery_helpers {
    use super::{
        TurnDiffClick, TurnDiffSendState, turn_diff_document, turn_diff_file_name, turn_diff_state_toast,
    };
    use crate::backend::{TurnDiff, TurnDiffFile};

    fn click() -> TurnDiffClick {
        TurnDiffClick {
            session_id: "ses_1".to_string(),
            from: "msg_user_1".to_string(),
            directory: "/repo/cola".to_string(),
            message_id: "msg_card".to_string(),
        }
    }

    fn diff() -> TurnDiff {
        TurnDiff(vec![TurnDiffFile {
            path: "src/main.rs".to_string(),
            patch: "@@ -1 +1 @@\n-a\n+b\n".to_string(),
            additions: 1,
            deletions: 1,
            status: "modified".to_string(),
        }])
    }

    /// #798's header wants the Turn's time between the title and the stats;
    /// the subtitle carries it, in the card's own date + clock idiom, plus the
    /// Turn's identity and the scope note: `session.diff` is a worktree range,
    /// not an authorship claim (#844).
    #[test]
    fn the_document_header_carries_the_turn_time_and_identity() {
        let anchor_ms = crate::feishu::card::test_local_ms(2026, 10, 19, 14, 30);
        let document = turn_diff_document(&diff(), &click(), Some(anchor_ms));
        assert_eq!(document.title, "本轮期间的工作区变化");
        assert_eq!(document.files.len(), 1);
        assert!(
            document.subtitle.contains("10-19 14:30"),
            "subtitle: {}",
            document.subtitle
        );
        assert!(document.subtitle.contains("ses_1"));
        assert!(document.subtitle.contains("msg_user_1"));
        assert!(document.subtitle.contains("/repo/cola"));
        assert!(
            document.subtitle.ends_with("快照区间差，可能含同目录其他写入者"),
            "the scope note closes the subtitle: {}",
            document.subtitle
        );
        let html = crate::backend::render_diff_html(&document);
        assert!(
            html.contains("10-19 14:30"),
            "the rendered artifact's subtitle carries the turn time"
        );
    }

    /// A failed transcript read must not lose the artifact: the header states
    /// the time as unknown instead.
    #[test]
    fn a_missing_turn_time_says_unknown_without_failing() {
        let document = turn_diff_document(&diff(), &click(), None);
        assert!(
            document.subtitle.contains(super::TURN_DIFF_UNKNOWN_TIME),
            "subtitle: {}",
            document.subtitle
        );
        let html = crate::backend::render_diff_html(&document);
        assert!(html.contains(super::TURN_DIFF_UNKNOWN_TIME));
        assert!(html.contains("src/main.rs"));
    }

    /// The filename is reproducible from the Turn's identity, ASCII, short and
    /// ends in `.html` (the client preview routes by extension).
    #[test]
    fn the_file_name_is_ascii_short_reproducible_html() {
        let name = turn_diff_file_name("ses_1", "msg_user_1");
        assert_eq!(name, turn_diff_file_name("ses_1", "msg_user_1"));
        assert_ne!(name, turn_diff_file_name("ses_1", "msg_user_2"));
        assert!(name.is_ascii(), "ASCII only: {name}");
        assert!(name.starts_with("diff-"), "diff- prefix: {name}");
        assert!(name.ends_with(".html"), "html extension: {name}");
        assert!(name.len() <= 40, "short: {name}");
    }

    /// A repeated click's toast is the stored state, not a fresh send.
    #[test]
    fn the_stored_states_speak_distinct_toasts() {
        assert_eq!(
            turn_diff_state_toast(TurnDiffSendState::Sending),
            "正在生成完整 diff…"
        );
        assert_eq!(turn_diff_state_toast(TurnDiffSendState::Sent), "已发送");
        assert_eq!(
            turn_diff_state_toast(TurnDiffSendState::OutcomeUnknown),
            crate::bridge::viewer::FILE_SEND_UNKNOWN_TEXT
        );
        assert_eq!(
            turn_diff_state_toast(TurnDiffSendState::Empty),
            super::TURN_DIFF_EMPTY_TEXT
        );
    }
}

#[cfg(test)]
mod viewer_reread_slot_tests {
    use super::{ViewerFileTransferStatus, remember_viewer_file_transfer, spawn_bounded_viewer_fs_read};

    #[test]
    fn viewer_transfer_once_guard_evicts_the_oldest_entry_at_its_cap() {
        let mut sent = indexmap::IndexMap::new();
        remember_viewer_file_transfer(
            &mut sent,
            ("chat".into(), "card".into(), "/a.rs".into(), 1),
            ViewerFileTransferStatus::Sent,
            2,
        );
        remember_viewer_file_transfer(
            &mut sent,
            ("chat".into(), "card".into(), "/a.rs".into(), 2),
            ViewerFileTransferStatus::Sent,
            2,
        );
        remember_viewer_file_transfer(
            &mut sent,
            ("chat".into(), "other".into(), "/b.rs".into(), 3),
            ViewerFileTransferStatus::Sent,
            2,
        );

        assert_eq!(sent.len(), 2);
        assert!(!sent.contains_key(&("chat".into(), "card".into(), "/a.rs".into(), 1)));
        assert!(sent.contains_key(&("chat".into(), "card".into(), "/a.rs".into(), 2)));
        assert!(sent.contains_key(&("chat".into(), "other".into(), "/b.rs".into(), 3)));
    }

    #[test]
    fn viewer_transfer_once_guard_rotates_with_content_identity() {
        let mut sent = indexmap::IndexMap::new();
        let old = ("chat".into(), "card".into(), "/a.rs".into(), 11);
        let refreshed = ("chat".into(), "card".into(), "/a.rs".into(), 22);
        remember_viewer_file_transfer(&mut sent, old.clone(), ViewerFileTransferStatus::Sent, 10);

        assert!(sent.contains_key(&old), "same snapshot remains deliver-once");
        assert!(
            !sent.contains_key(&refreshed),
            "a refreshed content identity can be delivered"
        );
        remember_viewer_file_transfer(&mut sent, refreshed.clone(), ViewerFileTransferStatus::Sent, 10);
        assert!(sent.contains_key(&refreshed));
    }

    /// #778: two Tool Panels on one Turn card share the clicked message, so the
    /// PATH is part of the key — a second file's preview is not mistaken for a
    /// duplicate of the first's.
    #[test]
    fn viewer_transfer_once_guard_separates_two_files_on_one_card() {
        let mut sent = indexmap::IndexMap::new();
        let first = ("chat".into(), "card".into(), "/a.html".into(), 0);
        let second = ("chat".into(), "card".into(), "/b.html".into(), 0);
        remember_viewer_file_transfer(&mut sent, first.clone(), ViewerFileTransferStatus::Sent, 10);
        assert!(sent.contains_key(&first));
        assert!(
            !sent.contains_key(&second),
            "a different path on the same card is a different file"
        );
    }

    /// A timed-out wait leaves the blocking job's slot held until its closure
    /// returns; subsequent work is refused rather than accumulated in the pool.
    #[tokio::test]
    async fn a_timed_out_reader_keeps_its_slot_until_the_os_work_returns() {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut first = spawn_bounded_viewer_fs_read(&slots, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
        .unwrap();
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut first)
                .await
                .is_err()
        );
        assert!(
            spawn_bounded_viewer_fs_read(&slots, || ()).is_err(),
            "the timed-out worker still holds the only slot"
        );

        release_tx.send(()).unwrap();
        first.await.unwrap();
        spawn_bounded_viewer_fs_read(&slots, || ())
            .unwrap()
            .await
            .unwrap();
    }
}
