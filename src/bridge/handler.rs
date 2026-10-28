use std::ops::Deref;
use std::sync::Arc;

use tracing::Instrument;

use crate::backend::types::{PromptFile, SessionStatus};
use crate::backend::{PromptSkill, SkillInfo, TaskRetirementEnding};
use crate::bridge::access::{Access, Decision, DenyReason};
use crate::bridge::command;
use crate::bridge::core::SharedCore;
use crate::bridge::handles::{PickOutcome, PickReply, classify_pick};
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
const CARD_STORE_TIMEOUT: &str = "⚠️ 处理超时，请重新点选（未做任何更改）";

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
/// that picks a new Turn's opening line (ADR-0062; it no longer routes) — and
/// the retry matrix read through it, so the two can never drift in bound or
/// failure policy. `what` labels the read in the bounded-call timeout log;
/// both callers pass the same injected per-read bound
/// (`TurnConfig::follow_read_timeout_ms`).
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

/// The prompt route (ADR-0062). cola's own live ownership — the
/// [`crate::bridge::turn::CardOwnership`] verdict's routing rule — is the
/// routing key; the Backend's status read is advisory data that only picks the
/// new card's opening line.
enum PromptRoute {
    /// cola owns the Session's live card chain — a Turn's inflight guard (or
    /// the out-of-turn follow that inherited it), or a chain a live renderer
    /// owns: the message merges into it as a Supplement, and the chain splits.
    Supplement,
    /// cola owns no live chain: the message starts a new Turn, whose card owns
    /// and settles it. A genuinely live run still merges it through the
    /// prompt's merge delivery.
    NewTurn {
        /// Whether the Backend's advisory read reported a live Execution.
        /// `false` when it read idle, unreadable (failed / timed out /
        /// unrecognised) — never a route, only the opening line.
        advisory_live: bool,
    },
}

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
enum ViewerFileOp {
    Send,
    Preview,
}

/// Everything a settled Terminal Card's buttons carry (ADR-0086 decisions 4, 7
/// and 9): the clicked card and the **Terminal Log** path, plus the settled
/// card's own render inputs so a click's outcome can be PATCHed back onto the
/// same card without a card-view read or a second state store.
struct TerminalCardRequest {
    message_id: String,
    chat_id: String,
    /// The run every settled button names (ADR-0086 decision 4). Carried so a
    /// click can be checked against the Chat/Topic's current run id — the stale
    /// rule the Terminal Run Record makes checkable (#827).
    run_id: String,
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
    fn from_value(value: &serde_json::Value) -> Option<Self> {
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
struct TerminalAskAiRequest {
    card: TerminalCardRequest,
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
    fn from_value(value: &serde_json::Value) -> Option<Self> {
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
enum ViewerFileTransferStatus {
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
struct TurnDiffClick {
    session_id: String,
    /// The Turn anchor's user-message id (empty is allowed for future entries;
    /// V2 then asks for the latest Turn, while V1 cannot aggregate without it).
    from: String,
    /// The per-child-session routing key the #477 button carries.
    directory: String,
    /// The clicked Turn card, which the File Message replies under
    /// (`open_message_id` — never part of the card's own JSON).
    message_id: String,
}

/// RAII claim for one in-flight Turn Diff delivery. A duplicate click is
/// acknowledged without spawning a second one; Drop releases the claim —
/// removing a `Sending` entry so a failed or cancelled attempt stays retryable.
struct TurnDiffClaim {
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
    viewer_file_transfers_sent:
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
/// Built by [`App::claim_recovery_click`].
struct RecoveryClick {
    recovery: crate::bridge::turn::TurnRecovery,
    thread_key: ThreadKey,
    directory: Option<String>,
    handles: crate::bridge::handles::TurnHandles,
}

/// The thread a card callback routes to. Every cola card button carries its
/// `chat_id` + `thread_id` in the value payload so the ack can route the choice
/// back to the right conversation.
fn thread_key_from_value(value: &serde_json::Value) -> crate::config::ThreadKey {
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

fn reject_nested_topic(thread_key: &ThreadKey) -> Option<CardActionResult> {
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
fn resolve_browse_dir(target: &str) -> Option<String> {
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
fn parse_show_hidden(value: &serde_json::Value) -> bool {
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
fn return_hidden(value: &serde_json::Value) -> bool {
    match value.get("return_show_hidden") {
        Some(serde_json::Value::Bool(hidden)) => *hidden,
        Some(serde_json::Value::String(flag)) => flag == "true",
        _ => false,
    }
}

/// The home listing's page from `return_page`: a number on the transfer
/// button, a decimal string on the pager — either way at least 1.
fn return_page_num(value: &serde_json::Value) -> usize {
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
struct OwnedReturnTo {
    dir: Option<String>,
    keyword: String,
    show_hidden: bool,
    page: usize,
}

/// The stateless work captured by a File Viewer open click.
struct ViewerOpen {
    thread_key: ThreadKey,
    path: String,
    session_dir: Option<String>,
    /// Tool Panel opens create a new viewer message; browse-row opens replace
    /// the listing card they came from.
    standalone: bool,
    card_message_id: String,
    return_to: OwnedReturnTo,
    page: usize,
    core: Arc<SharedCore>,
    in_flight_guard: ViewerActionGuard,
}

/// The stateless work captured by one File Viewer re-read click.
struct ViewerReread {
    thread_key: ThreadKey,
    path: String,
    session_dir: Option<String>,
    card_message_id: String,
    rich_message_id: Option<String>,
    sent_file_identity: Option<u64>,
    ambiguous_file_identity: Option<u64>,
    embedded_image_key: Option<String>,
    return_to: OwnedReturnTo,
    page: usize,
    /// The markdown view to rebuild at (#781): a re-read keeps the view the
    /// reader chose, so an explicit 「查看源码」 survives it.
    view: Option<crate::feishu::card::viewer::ViewerView>,
    in_flight_guard: ViewerActionGuard,
}

/// The bounded filesystem rebuild captured by one viewer pager callback.
struct ViewerPage {
    thread_key: ThreadKey,
    path: String,
    session_dir: Option<String>,
    card_message_id: String,
    return_to: OwnedReturnTo,
    page: usize,
    /// The markdown view to rebuild at (#781): `op: "page"` carries the reader's
    /// current one so a flip cannot silently change the view, `op: "view"` the
    /// target the toggle named.
    view: Option<crate::feishu::card::viewer::ViewerView>,
    lane: Option<crate::feishu::card::viewer::ViewerFileLane>,
    /// The content identity the clicked card displayed when the payload was
    /// built. A lane that never recorded its own bytes (the Rich Text
    /// delivery) still belongs to this snapshot, so the rebuild compares it
    /// too — otherwise a same-MIME/same-size edit would keep the stale lane.
    payload_identity: Option<u64>,
    in_flight_guard: ViewerActionGuard,
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
struct ViewerActionGuard {
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
fn viewer_page(value: &serde_json::Value) -> usize {
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
async fn declare_pending_dir(
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
async fn card_before(
    deadline: tokio::time::Instant,
    build: impl std::future::Future<Output = serde_json::Value>,
) -> Option<serde_json::Value> {
    tokio::time::timeout_at(deadline, build).await.ok()
}

/// Open a NEW topic for `directory`, anchored on the card's own message
/// (`open_message_id`) — the shared body of the `/dir` and browse cards' 建话题
/// (ADR-0016, ADR-0025). `Ok(display)` carries the directory basename for the
/// success toast; `Err(result)` is the exact refusal to return as-is.
async fn open_topic_from_card(
    core: &Arc<SharedCore>,
    thread_key: &ThreadKey,
    open_message_id: Option<&str>,
    directory: String,
) -> std::result::Result<String, CardActionResult> {
    if let Some(rejection) = reject_nested_topic(thread_key) {
        return Err(rejection);
    }
    let Some(open_message_id) = open_message_id else {
        return Err(CardActionResult {
            card: None,
            toast: Some("无法创建话题（缺少卡片消息引用）".to_string()),
        });
    };
    // Creation title policy (ADR-0007): unnamed, like `/topic <dir>`; the cover
    // shows the directory basename until the server auto-generates a title.
    let display = crate::bridge::display::dir_basename(&directory);
    match crate::bridge::topic::open_topic(
        &core.topic_handles(),
        &thread_key.chat_id,
        open_message_id,
        crate::bridge::topic::TopicOpening::Fresh {
            directory,
            name: None,
        },
    )
    .await
    {
        Ok(_opened) => Ok(display),
        Err(crate::bridge::topic::OpenTopicError::NoThreadId) => Err(CardActionResult {
            card: None,
            toast: Some(
                "当前会话不支持创建话题（未返回 thread_id）。请改用 `/dir <目录>` 或在飞书里手动创建话题。"
                    .to_string(),
            ),
        }),
        Err(crate::bridge::topic::OpenTopicError::Failed(e)) => {
            tracing::warn!("dir card topic: create topic failed: {}", e);
            Err(CardActionResult {
                card: None,
                toast: Some("创建话题失败".to_string()),
            })
        }
    }
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
    let clip = crate::feishu::card::picker::SKILL_ROW_LABEL_CHARS;
    let named = unknown
        .iter()
        .map(|id| format!("`{}`", crate::feishu::card::truncate_md(id, clip)))
        .collect::<Vec<_>>()
        .join("、");
    Some(crate::feishu::card::truncate_md(
        &format!("⚠️ 未找到技能：{named}"),
        UNKNOWN_IDS_LINE_CHARS,
    ))
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

/// A display label for the Chat/Topic that owns a session, for the force-confirm
/// card: the chat's display name (falling back to its id), with `（话题）` when
/// the owner is a Topic rather than the Chat lobby. Vocabulary per GLOSSARY.md —
/// the Feishu side is a Chat/Topic, never a "conversation".
async fn owner_label(core: &Arc<SharedCore>, owner_key: &ThreadKey) -> String {
    let chat_name = core
        .feishu
        .chat_name(&owner_key.chat_id)
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| owner_key.chat_id.clone());
    if owner_key.thread_id != owner_key.chat_id {
        format!("{chat_name}（话题）")
    } else {
        chat_name
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
        })
    }

    /// Claim one clicked viewer card until its spawned operation and final card
    /// update complete. The real card message id is supplied by Feishu callback
    /// context; chat scopes it defensively across targets.
    fn claim_viewer_action(&self, thread_key: &ThreadKey, message_id: &str) -> Option<ViewerActionGuard> {
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
            vec![serde_json::json!({ "tag": "markdown", "content": body })],
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
        tokio::try_join!(ws_task, perm_task, question_task, external_task, reconnect_task)?;
        Ok(())
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
    /// skill fold renders (the live Turn card, the Session Snapshot tail and
    /// the External Message preview are text-only; ADR-0077's acceptance
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
    async fn handle_skill_command(
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

    /// Route a prompt by cola's OWN live ownership (ADR-0062, ADR-0070): the
    /// one [`crate::bridge::turn::CardOwnership`] read over the waits state and
    /// the card map, whose routing rule — a guard held, or a card chain a live
    /// renderer owns — the Wake gate reads too, so the two can never disagree
    /// about whether a session is already being rendered. Only an owned chain
    /// takes the Supplement path — the split needs a live card, and with no
    /// owned chain it would find nothing to continue (the #428 strand, where
    /// the read said live and the message was silently absorbed). A
    /// pending-inbound claim owns no chain and routes a new Turn.
    ///
    /// With no owned chain, the Backend's status read is ADVISORY: it no
    /// longer routes, it only tells the new Turn whether to open with the
    /// merge line ([`crate::bridge::turn`]'s `MERGE_OPENING`). The new Turn is
    /// safe either way — its V2 prompt carries the merge delivery, so a
    /// genuinely live run merges the message exactly as a Supplement would,
    /// while a stale read gets a card that owns and settles the message.
    ///
    /// One INFO line records every decision (ADR-0048): the session, the
    /// ownership verdict and, when read, the advisory status — never chat
    /// content.
    async fn route_prompt(&self, session_id: &str) -> PromptRoute {
        let ownership =
            crate::bridge::turn::CardOwnership::read(&self.cards_handle(), &self.waits_handle(), session_id)
                .await;
        if let Some(label) = ownership.routing_label() {
            tracing::info!("prompt routing: session={session_id} ownership={label} route=supplement");
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
        tracing::info!("prompt routing: session={session_id} ownership=none advisory={advisory} route=turn");
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
                let _ = self
                    .feishu
                    .reply_text(
                        message_id,
                        "⚠️ 当前没有可用的 OpenCode server，且 `start_server = \"never\"`。\
                          \n请启动 OpenChamber（或手动 `opencode serve`），或把配置改为 `start_server = \"auto\"`。",
                    )
                    .await;
                false
            }
            Err(e) => {
                tracing::warn!("ensure server failed: {}", e);
                let _ = self
                    .feishu
                    .reply_text(message_id, &format!("⚠️ 启动 OpenCode server 失败：{e}"))
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

        // Prompt routing (ADR-0062): cola's own live ownership decides a
        // Supplement, never the Backend's read. With no owned chain the message
        // starts a new Turn — whose card owns and settles it — opening with the
        // merge line only when the advisory read said live; a genuinely live
        // run still merges it through the prompt's merge delivery (V2's steer).
        match self.route_prompt(&session_id).await {
            PromptRoute::NewTurn { advisory_live } => {
                let outcome = self
                    .run_prompt(PromptContext {
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
                        advisory_live,
                    })
                    .await?;
                Ok(Some(match outcome {
                    crate::bridge::turn::PromptOutcome::Accepted => PromptSubmission::NewTurn,
                    crate::bridge::turn::PromptOutcome::NotAccepted => PromptSubmission::NotAccepted,
                }))
            }
            // Supplement path (ADR-0043, owned routing per ADR-0062): cola
            // holds the Session's live Execution, so the message does not start
            // a competing run_prompt (it would overwrite the running turn's
            // accumulator and race on the same card). Instead submit the
            // message to the backend without starting a Turn — OpenCode
            // persists it and the running loop picks it up at the next tool
            // boundary, merging it into the current turn (original message
            // preserved; model sees full history). This is what lets the user
            // append context mid-turn without /stop and without interrupting a
            // tool call. The message landed below the live card, so the Card
            // Chain splits at it (ADR-0043) — the split is requested even when
            // the send failed, never for commands (they are dispatched above
            // this path). The out-of-turn follow inherits the inflight guard
            // for its whole window (ADR-0059), so a message during a run that
            // outlived the drain bound keeps merging into the still-live chain
            // instead of freezing its card.
            PromptRoute::Supplement => {
                // Each supplement is its own logical user message: fresh
                // cola-authored id (ADR-0026), never reused.
                let cola_msg_id = crate::backend::parsing::cola_message_id();
                let submitted = match self
                    .client
                    .steer(
                        &session_id,
                        &text,
                        &attachments,
                        &skills,
                        self.session_model_override(&session_id).await.as_ref(),
                        self.session_variant_override(&session_id).await.as_deref(),
                        self.session_agent_override(&session_id).await.as_deref(),
                        Some(&cola_msg_id),
                    )
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            "supplement: session {} in-flight, message queued to merge into current turn",
                            session_id
                        );
                        true
                    }
                    Err(e) => {
                        tracing::warn!("supplement: prompt failed: {}", e);
                        // The failure notice is sent first, then the split
                        // still happens below — the live card must remain the
                        // newest message, not the notice.
                        let _ = self
                            .feishu
                            .reply_text(&message_id, "⚠️ 补充消息发送失败，请稍后重试。")
                            .await;
                        false
                    }
                };
                // The supplement landed below the live card: split the Card
                // Chain at this message (ADR-0043). The previous card is
                // finalized with the standard split header and keeps everything
                // before the split; the continuation — a reply to the
                // supplement — carries the receipt (and only the content that
                // arrives after it) and becomes the tracked live card. There is
                // NO separate acknowledgement message.
                crate::bridge::turn::Turn::split_card_chain(
                    &self.cards_handle(),
                    &session_id,
                    &message_id,
                    crate::bridge::turn::SplitKind::Supplement,
                )
                .await;
                // The message merged into the live Execution: the claim's job
                // is done — once the prompt is visible in the transcript, the
                // Sync's own staleness rule takes over (#424).
                self.waits_handle().clear_inbound(&session_id).await;
                Ok(Some(if submitted {
                    PromptSubmission::Supplement
                } else {
                    PromptSubmission::NotAccepted
                }))
            }
        }
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
                return match self.materialise_pending(*pending, generation, message_id).await {
                    Ok(id) => Ok((id, false)),
                    Err(e) => {
                        // A create failure leaves the pending intact (and gives
                        // the claim back): surface the error here and let the
                        // next message retry.
                        self.sessions_handle()
                            .unclaim_pending_creation(thread_key, generation)
                            .await;
                        let _ = self
                            .feishu
                            .reply_text(
                                message_id,
                                &format!(
                                    "⚠️ 创建会话失败（目录 `{}`）：{}\n下一条消息会再试一次。",
                                    directory, e
                                ),
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
        let mut input = self.client.new_session_input(Some(&pending.directory));
        // A provider pick is a **creation** fact (issue #910): a Codex thread's
        // provider can only be chosen by `thread/start` (or by a confirmed
        // resume), and it pairs with the model chosen beside it — a profile
        // preset names both. `input.provider` stays `None` for a client with no
        // provider axis, so OpenCode's create body is byte-identical.
        if pending.provider.is_some() {
            input.provider = pending.provider.clone();
            input.model = pending
                .model
                .as_deref()
                .and_then(crate::backend::parsing::parse_model);
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
        let entry = pending.into_entry(session.id.clone());
        let thread_key = entry.thread_key.clone();
        // The core wrapper (not the raw store) so the session-list cache is
        // invalidated: `/switch` must see the just-created session. Scoped to
        // this creation's generation (issue #910): a thread that moved on while
        // we ran is not overwritten, and a replacement pending survives.
        if !self.activate_created(entry, generation).await? {
            return Err(crate::error::BridgeError::State(
                "the thread moved on while its session was being created".to_string(),
            ));
        }
        // ADR-0041 + ADR-0023: a pending topic's cover still says
        // 「下一条消息创建」. Move its record onto the new session and re-render
        // now — a real Session exists, so the brief (title/id) belongs on the
        // card. No-op for lobby pendings and fallback-rooted topics.
        crate::bridge::topic::claim_pending_cover(&self.topic_handles(), &thread_key, &session.id).await;
        Ok(session.id)
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
    /// client feedback. Dispatches to the flow that owns the action tag.
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
            // The Turn card's 「🔍 查看完整变化」 carries `op` and NO `action`
            // (#477/spec #797): route it before the action match, without
            // widening the button's exact four-key payload.
            if op == crate::feishu::card::turn_diff::TURN_DIFF_OP {
                return self.handle_turn_diff_action(&value).await;
            }
            match action {
                "perm" => {
                    self.permission
                        .handle_card_action(&self.flow_handles(), &value)
                        .await
                }
                "question" => {
                    self.question
                        .handle_card_action(&self.flow_handles(), &value)
                        .await
                }
                "retry" => self.handle_retry_action(&value).await,
                "cleanup" => self.handle_cleanup_action(&value).await,
                "resume" => self.handle_resume_action(&value).await,
                "switch" => self.handle_switch_card_action(&self.core, &value, deadline).await,
                "sub" => self.handle_sub_card_action(&self.core, &value).await,
                "dir" => self.handle_dir_card_action(&self.core, &value, deadline).await,
                // Different cards carry `action: "file"` (#739, #742, #778): the
                // viewer's own 「发为文件」/「🌐 预览网页」 button, whose payload
                // carries `op: "send"` and transfers the file's whole bytes
                // (PATCHing the clicked viewer card), a Tool Panel's 「🌐 预览
                // 网页」 button, whose `op: "preview"` sends the same File Message
                // but must NOT PATCH — the clicked message is the agent's Turn
                // card (#778) — and a browse card's file row, which carries no op
                // and opens the file in place. The op tells the three apart.
                // The Terminal Card's 「📄 完整输出」 (ADR-0086 decision 7): the
                // click acknowledges and a spawned, bounded transfer uploads the
                // Terminal Log as ONE File Message, then PATCHes the card with
                // the outcome. A 30 MB upload never runs inside the callback's
                // ack budget.
                "terminal" => match value.get("op").and_then(|v| v.as_str()) {
                    Some("full_output") => self.handle_terminal_full_output(&value).await,
                    // 「问 AI」 (ADR-0086 decision 9): the ONLY path by which
                    // terminal output reaches the model. Like the retrieval
                    // click it leaves the callback: the normal prompt pipeline
                    // runs off the ack, because it may ensure a server and wait
                    // on a whole Turn.
                    Some("ask_ai") => self.handle_terminal_ask_ai(&value).await,
                    // 「↻ 重跑」 (ADR-0086 decisions 4 and 5): the click names a
                    // run, the chat's record must still be that run, and the
                    // command/cwd/timeout come from the record — which is why
                    // the button works after a restart and why a redelivered
                    // click is refused as stale instead of running twice.
                    Some("rerun") => self.handle_terminal_rerun(&value).await,
                    // 「⏹ 终止」 / 「继续」 (ADR-0086 decision 6): the click
                    // only routes to the Chat/Topic's current run, whose id
                    // the button carries. A click for any other run id is
                    // refused as stale, and the run loop owns the group.
                    _ => self.handle_terminal_action(&value).await,
                },
                "file" => match value.get("op").and_then(|v| v.as_str()) {
                    Some("send") => self.handle_viewer_file_action(&value, ViewerFileOp::Send).await,
                    Some("preview") => {
                        self.handle_viewer_file_action(&value, ViewerFileOp::Preview)
                            .await
                    }
                    _ => self.handle_file_card_action(&self.core, &value).await,
                },
                "agent" => self.handle_agent_card_action(&self.core, &value, false).await,
                "agent_clear" => self.handle_agent_card_action(&self.core, &value, true).await,
                "model" => {
                    self.handle_model_card_action(&self.core, &value, callback_started)
                        .await
                }
                // A provider switch is a two-step click (issue #910): the
                // confirmation card's own tags land here. 取消 is a no-op — it
                // answers the ack and leaves the thread exactly as it was.
                "model_provider_confirm" => self.handle_provider_switch_confirm(&self.core, &value).await,
                "model_provider_cancel" => Some(CardActionResult {
                    card: None,
                    toast: None,
                }),
                "think" => self.handle_think_card_action(&self.core, &value, false).await,
                "think_clear" => self.handle_think_card_action(&self.core, &value, true).await,
                "autoaccept" => self.handle_autoaccept_card_action(&self.core, &value).await,
                "skill" => self.handle_skill_card_action(&value).await,
                _ => None,
            }
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

    /// Handle a `/switch` card button (ADR-0012, issue 04): adopt a session,
    /// create a new one, re-search, or flip the page. The 接管/切换 ops return
    /// the Session Snapshot (or the compact suppressed-切换 state, ADR-0028) as
    /// the card so the ack patches the switch card in place — one message per
    /// activation; page/scope/search/new/topic_adopt return a refreshed list
    /// card carrying the active keyword/scope/page (ADR-0052), except that a
    /// search or scope toggle resets to page 1. Plus a Toast for instant
    /// feedback.
    async fn handle_switch_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        deadline: tokio::time::Instant,
    ) -> Option<CardActionResult> {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        let session_id = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let scope = crate::feishu::card::session::SwitchScope::parse(
            value.get("scope").and_then(|v| v.as_str()).unwrap_or(""),
        );
        // The card's stateless filter (ADR-0052): keyword, scope and page ride
        // every button. A missing/garbage page reads as 1; the builder clamps an
        // out-of-range page to the last page.
        let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;

        match op {
            "page" => Some(CardActionResult {
                card: Some(
                    self.build_switch_card_for(core, &thread_key, &keyword, scope, page)
                        .await,
                ),
                toast: None,
            }),
            "scope" => {
                // The 全部/本目录 toggle (ADR-0022): the button carries the
                // TARGET scope ("all"/"dir"); the outer `scope` above already
                // parsed it. Rebuild the card in that scope at page 1, keeping
                // the keyword so a scoped search survives the toggle (ADR-0052).
                Some(CardActionResult {
                    card: Some(
                        self.build_switch_card_for(core, &thread_key, &keyword, scope, 1)
                            .await,
                    ),
                    toast: None,
                })
            }
            "adopt" | "force_adopt" => {
                // Adopt/switch the target session into this thread. The plain
                // `adopt` op carries no `--force`: an occupied session patches
                // the card to the force-confirm card, whose `force_adopt`
                // button re-enters here. `force_adopt` skips the owner check and
                // lets the adoption's `set_active` steal the mapping. Either
                // op refuses a child the chat has not mapped: the card's rows
                // are roots-only, but a stale or forged callback can still name
                // one, and the general surfaces never adopt a child (spec
                // #344; the sanctioned path is `/sub attach`).
                let force = op == "force_adopt";
                let sessions = core.cached_session_list().await.ok()?;
                let target = sessions.iter().find(|s| s.id == session_id)?.clone();
                let child_refusal = {
                    let store = core.sessions.lock().await;
                    command::child_adoption_refusal(&store, &thread_key, &target)
                };
                if let Some(refusal) = child_refusal {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(refusal),
                    });
                }
                let already_active = {
                    let store = core.sessions.lock().await;
                    store
                        .get_active(&thread_key)
                        .map(|e| e.session_id == target.id)
                        .unwrap_or(false)
                };
                if already_active {
                    return Some(CardActionResult {
                        card: Some(
                            self.build_switch_card_for(core, &thread_key, &keyword, scope, page)
                                .await,
                        ),
                        // The target session is already this conversation's
                        // active session — nothing changed. 会话 here names the
                        // OpenCode session (ADR-0022 keeps it), not the Feishu
                        // side.
                        toast: Some("已在当前会话".to_string()),
                    });
                }
                if !force {
                    let owner = {
                        let store = core.sessions.lock().await;
                        store.thread_for_session(&target.id)
                    };
                    if let Some(owner_key) = owner
                        && owner_key != thread_key
                    {
                        let owner_label = owner_label(core, &owner_key).await;
                        return Some(CardActionResult {
                            card: Some(crate::feishu::card::session::build_force_confirm_card(
                                &thread_key,
                                &target,
                                &owner_label,
                                "force_adopt",
                                "强制接管",
                                scope,
                                &keyword,
                                page,
                            )),
                            toast: Some(format!("该会话被 {} 占用，请确认是否强制接管", owner_label)),
                        });
                    }
                }
                // ADR-0028 one-card rule: the switch card's OWN message becomes
                // the confirmation — the ack patches the list card in place to
                // the snapshot. The list's filter rides along (ADR-0052): the
                // snapshot's 返回列表 button rebuilds exactly this window.
                let open_message_id = value
                    .get("open_message_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let back = crate::feishu::card::session::BackToList {
                    thread_key: thread_key.clone(),
                    keyword: keyword.clone(),
                    scope,
                    page,
                };
                Some(
                    self.apply_card_adoption(core, &thread_key, &target, open_message_id, Some(&back))
                        .await,
                )
            }
            "back" => Some(CardActionResult {
                card: Some(
                    self.build_switch_card_for(core, &thread_key, &keyword, scope, page)
                        .await,
                ),
                toast: None,
            }),
            "new" => {
                // Lazy Session Creation (ADR-0041): the card form of `/new` —
                // declare a Pending Session; the first message materialises it.
                // The active filter survives (ADR-0052), so the refreshed list
                // stays on the same keyword/scope/page.
                //
                // The declaration and its write ride the click's ONE deadline
                // (ADR-0089): a click must never commit a pending after Feishu
                // has stopped listening for its answer.
                let toast = match core
                    .command_handles()
                    .declare_pending_in_current_project_until(&thread_key, None, deadline)
                    .await
                {
                    Ok(Some(_)) => "下一条消息创建会话".to_string(),
                    // The store lock was still held: nothing changed, so there
                    // is no refreshed card to show.
                    Ok(None) => {
                        return Some(CardActionResult {
                            card: None,
                            toast: Some(CARD_STORE_TIMEOUT.to_string()),
                        });
                    }
                    // The pending IS declared in memory; only the save failed.
                    Err(e) => {
                        tracing::warn!("switch card new: persist failed: {}", e);
                        format!("⚠️ 已新建待创建会话，但保存失败（重启后会丢失）：{e}")
                    }
                };
                Some(CardActionResult {
                    card: card_before(
                        deadline,
                        self.build_switch_card_for(core, &thread_key, &keyword, scope, page),
                    )
                    .await,
                    toast: Some(toast),
                })
            }
            "search" => {
                // A search is an explicit refresh request: drop the session-list
                // cache so the re-filter sees newly created/adopted sessions.
                // It always lands on page 1; the keyword is the typed one.
                core.invalidate_session_list_cache().await;
                Some(CardActionResult {
                    card: Some(
                        self.build_switch_card_for(core, &thread_key, &keyword, scope, 1)
                            .await,
                    ),
                    toast: None,
                })
            }
            "topic_adopt" | "force_topic_adopt" => {
                // "建话题接管" (ADR-0016): open a NEW Feishu topic around an
                // existing session, in one gesture from the session card. The
                // topic anchors on the card's own message (`open_message_id`,
                // threaded through by `extract_card_action_value`), so fallback
                // cards can reply inside it. The plain `topic_adopt` op patches
                // an occupied session to the force-confirm card (强制建话题接管),
                // whose `force_topic_adopt` button re-enters here and skips the
                // owner check.
                // A topic can only be created from a non-topic message
                // (ADR-0006, ADR-0025): the session card may legally open in a
                // never-bound topic, but its topic op must not nest there —
                // same guard as the `/dir` card's "建话题" op and the text
                // `/topic --adopt` forms.
                let force = op == "force_topic_adopt";
                if let Some(rejection) = reject_nested_topic(&thread_key) {
                    return Some(rejection);
                }
                let Some(open_message_id) = value.get("open_message_id").and_then(|v| v.as_str()) else {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some("无法创建话题（缺少卡片消息引用）".to_string()),
                    });
                };
                let open_message_id = open_message_id.to_string();
                let sessions = core.cached_session_list().await.ok()?;
                let target = sessions.iter().find(|s| s.id == session_id)?.clone();
                if target.is_child() {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some("子任务会话不支持接管".to_string()),
                    });
                }
                if !force {
                    let owner = {
                        let store = core.sessions.lock().await;
                        store.thread_for_session(&target.id)
                    };
                    if let Some(owner_key) = owner
                        && owner_key != thread_key
                    {
                        let owner_label = owner_label(core, &owner_key).await;
                        return Some(CardActionResult {
                            card: Some(crate::feishu::card::session::build_force_confirm_card(
                                &thread_key,
                                &target,
                                &owner_label,
                                "force_topic_adopt",
                                "强制建话题接管",
                                scope,
                                &keyword,
                                page,
                            )),
                            toast: Some(format!("会话被 {} 占用，请确认是否强制接管", owner_label)),
                        });
                    }
                }
                Some(
                    self.topic_adopt_target(
                        core,
                        &thread_key,
                        &target,
                        &open_message_id,
                        &keyword,
                        scope,
                        page,
                    )
                    .await,
                )
            }
            _ => None,
        }
    }

    /// Rebuild the `/switch` card for a thread with the given search keyword,
    /// list scope and 1-based page (ADR-0022, ADR-0052). The builder clamps an
    /// out-of-range page to the last page.
    async fn build_switch_card_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        keyword: &str,
        scope: crate::feishu::card::session::SwitchScope,
        page: usize,
    ) -> serde_json::Value {
        let (shown, active_id, mapped_ids, scope, current_dir) =
            crate::feishu::card::command::switch_card_data(
                &core.command_handles(),
                thread_key,
                keyword,
                scope,
            )
            .await;
        crate::feishu::card::session::build_switch_card(
            thread_key,
            &shown,
            keyword,
            scope,
            page,
            current_dir.as_deref(),
            active_id.as_deref(),
            &mapped_ids,
        )
    }

    /// The adoption tail shared by the card's 接管/切换 and 强制接管 ops: build
    /// the Session Snapshot (or the compact suppressed-切换 state card,
    /// ADR-0028), persist the new active entry, and settle the snapshot's
    /// claimed pendings. The returned card patches the clicked card in place
    /// (ADR-0028 one-card rule), so no second message is sent. `back` is the
    /// switch card's list state the adoption came from (ADR-0052): both
    /// snapshot forms carry its 返回列表 button, and the claim registry keeps it
    /// across rebuilds.
    ///
    /// The steal is `set_active`'s: it removes any existing entry with this
    /// session id, so a `force_adopt` from another thread leaves that owner
    /// sessionless there. No pre-remove — if the snapshot work failed, the old
    /// mapping would otherwise be lost for nothing.
    async fn apply_card_adoption(
        &self,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        target: &crate::backend::types::SessionListInfo,
        open_message_id: Option<String>,
        back: Option<&crate::feishu::card::session::BackToList>,
    ) -> CardActionResult {
        let mapped_to_this_thread = {
            let store = core.sessions.lock().await;
            store.thread_for_session(&target.id).as_ref() == Some(thread_key)
        };
        let verb = if mapped_to_this_thread { "切换" } else { "接管" };
        // `claim_data` is the snapshot's claimable pendings: claimed against
        // the patched card once the ack result is assembled, so the poll loop
        // never duplicates the embedded blocks.
        let (card, claim_data) = if mapped_to_this_thread {
            // A re-activation: the session is already mapped to this thread, so
            // its snapshot runs inside the Session's `snapshot` span with the
            // thread key known here (ADR-0048).
            match crate::bridge::snapshot::re_switch_snapshot(
                &core.snapshot_handles(),
                thread_key,
                &target.id,
                &target.directory,
                &target.title,
                back,
            )
            .await
            {
                crate::bridge::snapshot::ReSwitchSnapshot::Full { card, data } => (card, Some(data)),
                crate::bridge::snapshot::ReSwitchSnapshot::Suppressed => (
                    crate::feishu::snapshot_card::build_switched_state_card(
                        &target.title,
                        &target.id,
                        &target.directory,
                        back,
                    ),
                    None,
                ),
            }
        } else {
            let (card, data) =
                crate::bridge::snapshot::snapshot_card_for(&core.snapshot_handles(), "接管", target, back)
                    .await;
            (card, Some(data))
        };
        // In a topic the patched card lives INSIDE it, so persist its own
        // message id as the fallback-card anchor (same anchor semantics as the
        // text in-topic adopt, ADR-0028): later permission/question cards reply
        // to it and stay in the topic. When the topic's session was still
        // pending (ADR-0041), carry its cover root too so the quote-injection
        // guard keeps excluding the topic's own root and the cover record can
        // be claimed for the adopted session.
        let pending_topic_root = {
            let store = core.sessions.lock().await;
            store.pending_for(thread_key).and_then(|p| p.topic_root.clone())
        };
        let topic_anchor = if thread_key.thread_id != thread_key.chat_id {
            open_message_id.clone()
        } else {
            None
        };
        let mut entry =
            crate::config::SessionEntry::new(thread_key.clone(), target.id.clone(), target.directory.clone());
        entry.agent = target.agent.clone();
        entry.topic_anchor = topic_anchor;
        entry.topic_root = pending_topic_root;
        if let Err(e) = core.activate_session(entry).await {
            tracing::warn!("switch card adopt: persist failed: {}", e);
        }
        crate::bridge::topic::claim_pending_cover(&core.topic_handles(), thread_key, &target.id).await;
        if let (Some(message_id), Some(data)) = (&open_message_id, &claim_data) {
            crate::bridge::external::settle_snapshot_after_send(
                &core.external,
                &core.flow_handles(),
                message_id,
                verb,
                &target.title,
                data,
                back,
            )
            .await;
        }
        CardActionResult {
            card: Some(card),
            toast: Some(format!(
                "已{verb}「{}」",
                crate::bridge::display::title_or_id_tail(target)
            )),
        }
    }

    /// The topic-creation tail shared by the card's 建话题接管 and
    /// 强制建话题接管 ops: open a topic anchored on the card message and map
    /// the adopted session to the new topic's `ThreadKey` (shared with the text
    /// `/topic --adopt` form, ADR-0016), then refresh the list card in place at
    /// the same keyword/scope/page (ADR-0052).
    #[allow(clippy::too_many_arguments)] // the filter triple rides the rebuild: keyword + scope + page
    async fn topic_adopt_target(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        target: &crate::backend::types::SessionListInfo,
        open_message_id: &str,
        keyword: &str,
        scope: crate::feishu::card::session::SwitchScope,
        page: usize,
    ) -> CardActionResult {
        let new_thread_id = match crate::bridge::topic::open_topic(
            &core.topic_handles(),
            &thread_key.chat_id,
            open_message_id,
            crate::bridge::topic::TopicOpening::Adopt { info: target.clone() },
        )
        .await
        {
            Ok(opened) => opened.thread_id,
            Err(crate::bridge::topic::OpenTopicError::NoThreadId) => {
                return CardActionResult {
                    card: None,
                    toast: Some("创建话题失败（未返回 thread_id）".to_string()),
                };
            }
            Err(crate::bridge::topic::OpenTopicError::Failed(e)) => {
                tracing::warn!("switch card topic_adopt: create topic failed: {}", e);
                return CardActionResult {
                    card: None,
                    toast: Some("创建话题失败".to_string()),
                };
            }
        };
        tracing::info!(
            "switch card topic_adopt: created topic {} for session {} in chat {}",
            new_thread_id,
            target.id,
            thread_key.chat_id
        );
        CardActionResult {
            card: Some(
                self.build_switch_card_for(core, thread_key, keyword, scope, page)
                    .await,
            ),
            toast: Some("已建话题接管".to_string()),
        }
    }

    /// Handle a `/sub` child-session card button (spec #344): `op: "search"`
    /// rebuilds the card with the typed keyword at page 1 (and drops the
    /// session-list cache so a just-created child shows up); `op: "page"` flips
    /// to the payload's page. Both carry the active keyword/page, so the
    /// filtered window round-trips (ADR-0052). Read-only: these are the only
    /// two ops the card has.
    async fn handle_sub_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // The card's stateless filter (ADR-0052): a missing/garbage page reads
        // as 1; the builder clamps an out-of-range page to the last page.
        let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        match op {
            "search" => {
                core.invalidate_session_list_cache().await;
                Some(CardActionResult {
                    card: Some(self.build_child_card_for(core, &thread_key, &keyword, 1).await),
                    toast: None,
                })
            }
            "page" => Some(CardActionResult {
                card: Some(self.build_child_card_for(core, &thread_key, &keyword, page).await),
                toast: None,
            }),
            _ => None,
        }
    }

    /// Rebuild the `/sub` child-session card for a thread, narrowed by
    /// `keyword` and paged to `page` (ADR-0052). One status read per rendered
    /// row, gathered by [`crate::feishu::card::command::child_card_data`].
    async fn build_child_card_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        keyword: &str,
        page: usize,
    ) -> serde_json::Value {
        let (children, statuses) =
            crate::feishu::card::command::child_card_data(&core.command_handles(), thread_key, keyword, page)
                .await;
        crate::feishu::card::session::build_child_card(
            thread_key,
            &children,
            &statuses,
            keyword,
            page,
            chrono::Utc::now().timestamp_millis(),
        )
    }

    /// Rebuild the `/dir` Recent Directories card for a thread, narrowed by
    /// `keyword` and paged to `page` (ADR-0051, ADR-0052).
    async fn build_dir_card_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        keyword: &str,
        page: usize,
    ) -> serde_json::Value {
        let (dirs, current_dir) =
            crate::feishu::card::command::dir_card_data(&core.command_handles(), thread_key, keyword).await;
        crate::feishu::card::session::build_dir_card(thread_key, &dirs, current_dir.as_deref(), keyword, page)
    }

    /// Handle a `/dir` Recent Directories card button (ADR-0025): `op:
    /// "pick"` re-roots the thread into the picked directory by declaring a
    /// Pending Session there (the card form of `/dir <path>`, ADR-0041 — the
    /// first message materialises it); `op: "topic"` wraps a NEW session in a
    /// brand-new Feishu topic instead — the card equivalent of `/topic <dir>`.
    /// Clicking the current directory's `pick` is a no-op that just toasts.
    /// `op: "search"` rebuilds the card narrowed by the typed keyword at page
    /// 1 (ADR-0051); `op: "page"` flips to the payload's page. Every rebuild
    /// keeps the active keyword and page (ADR-0052) — a row action no longer
    /// resets the filter. Refreshes the card in place so the pending's
    /// directory shows as `当前`.
    async fn handle_dir_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        deadline: tokio::time::Instant,
    ) -> Option<CardActionResult> {
        // ADR-0078: the browse card multiplexes through the same `dir` action,
        // distinguished by `view: "browse"`, so one dispatch arm serves both.
        if value.get("view").and_then(|v| v.as_str()) == Some("browse") {
            return self.handle_dir_browse_action(core, value, deadline).await;
        }
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        // The card's stateless filter (ADR-0052): the active keyword and page
        // ride every button. A missing/garbage page reads as 1; the builder
        // clamps an out-of-range page to the last page.
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        // The search submit and a page flip carry no directory, so they are
        // handled before the directory guard below. A search always lands on
        // page 1; a flip rebuilds the payload's page.
        if op == "search" {
            return Some(CardActionResult {
                card: Some(self.build_dir_card_for(core, &thread_key, &keyword, 1).await),
                toast: None,
            });
        }
        if op == "page" {
            return Some(CardActionResult {
                card: Some(self.build_dir_card_for(core, &thread_key, &keyword, page).await),
                toast: None,
            });
        }
        // ADR-0078: the Recent Directories card's 浏览目录 button opens the
        // filesystem browse card in place.
        if op == "browse" {
            return Some(CardActionResult {
                card: Some(self.build_dir_browse_for_start(core, &thread_key).await),
                toast: None,
            });
        }
        // ADR-0078: ← 最近目录 on the browse card swaps back in place.
        if op == "recent" {
            return Some(CardActionResult {
                card: Some(self.build_dir_card_for(core, &thread_key, "", 1).await),
                toast: None,
            });
        }
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if directory.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        match op {
            "pick" => {
                // Pending-first (ADR-0041): 当前 is the pending's directory
                // when one exists, else the active session's. The equality rule
                // is the browse card's own ✅ 当前 rule (`command::same_dir`), so
                // the two pick surfaces agree on what "current" means.
                let current_dir = tokio::time::timeout_at(deadline, core.sessions.lock())
                    .await
                    .ok()
                    .and_then(|store| store.current_directory(&thread_key));
                if current_dir.is_some_and(|current| crate::bridge::command::same_dir(&current, &directory)) {
                    return Some(CardActionResult {
                        card: card_before(
                            deadline,
                            self.build_dir_card_for(core, &thread_key, &keyword, page),
                        )
                        .await,
                        toast: Some("已在当前目录".to_string()),
                    });
                }
                Some(
                    match declare_pending_dir(core, &thread_key, &directory, deadline).await {
                        Some(toast) => CardActionResult {
                            card: card_before(
                                deadline,
                                self.build_dir_card_for(core, &thread_key, &keyword, page),
                            )
                            .await,
                            toast: Some(toast),
                        },
                        None => CardActionResult {
                            card: None,
                            toast: Some(CARD_STORE_TIMEOUT.to_string()),
                        },
                    },
                )
            }
            "topic" => {
                // A topic can only be created from a non-topic message
                // (ADR-0006, ADR-0025): a `/dir` card opened inside a
                // never-bound topic — which may legally bind its single
                // session via `pick` — must not nest another topic. The guard
                // is location-based, so it also covers a topic that bound
                // itself via the row's left button on this same card. The
                // topic anchors on the card's own message (`open_message_id`,
                // threaded by `extract_card_action_value`), so fallback cards
                // can reply inside it (ADR-0016).
                let open_message_id = value.get("open_message_id").and_then(|v| v.as_str());
                match open_topic_from_card(core, &thread_key, open_message_id, directory).await {
                    Ok(display) => Some(CardActionResult {
                        card: Some(self.build_dir_card_for(core, &thread_key, &keyword, page).await),
                        toast: Some(format!("已建话题（{display}）——下一条消息创建会话")),
                    }),
                    Err(rejection) => Some(rejection),
                }
            }
            _ => Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            }),
        }
    }

    /// Open the `/dir` browse card (ADR-0078) at the conversation's current
    /// project directory.
    async fn build_dir_browse_for_start(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
    ) -> serde_json::Value {
        let start = core.command_handles().current_project_directory(thread_key).await;
        self.build_dir_browse_for(
            core,
            thread_key,
            BrowseView {
                dir: &start,
                show_hidden: false,
                keyword: "",
                page: 1,
            },
        )
        .await
    }

    /// Rebuild the `/dir` browse card (ADR-0078) for `view.dir`, clamping a
    /// vanished path to its nearest existing ancestor and marking the
    /// conversation's current directory.
    async fn build_dir_browse_for(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        view: BrowseView<'_>,
    ) -> serde_json::Value {
        let sessions = core.command_handles().flow.sessions;
        build_dir_browse_card_for(&sessions, thread_key, view).await
    }

    /// Handle a `/dir` browse-card click (ADR-0078). `go` drills down (a child
    /// row, a breadcrumb hop, or the form's typed path); `pick`/`topic` act on
    /// the directory you stand in (or the typed path), mirroring the Recent
    /// Directories card's two ops; `filter`/`page`/`show_hidden` rebuild the
    /// view; `restore` rebuilds it untouched (a viewer's 「← 返回目录」);
    /// `recent` swaps back to the Recent Directories card. A typed path
    /// wins over the payload's `dir`, and the current project is the fallback
    /// when a name-only form submit lost the dir.
    async fn handle_dir_browse_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        deadline: tokio::time::Instant,
    ) -> Option<CardActionResult> {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let thread_key = thread_key_from_value(value);
        let show_hidden = parse_show_hidden(value);
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        let dir = value
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let path = value
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // The typed path wins over the payload's dir on 前往/选择; a name-only
        // submit of 过滤 lost both, so it falls back to the current project.
        let target = if !path.is_empty() {
            path
        } else if !dir.is_empty() {
            dir
        } else {
            core.command_handles()
                .current_project_directory(&thread_key)
                .await
        };
        let base = BrowseView {
            dir: &target,
            show_hidden,
            keyword: &keyword,
            page,
        };
        match op {
            "go" => Some(CardActionResult {
                card: Some(
                    self.build_dir_browse_for(
                        core,
                        &thread_key,
                        BrowseView {
                            keyword: "",
                            page: 1,
                            ..base
                        },
                    )
                    .await,
                ),
                toast: None,
            }),
            "filter" => Some(CardActionResult {
                card: Some(
                    self.build_dir_browse_for(core, &thread_key, BrowseView { page: 1, ..base })
                        .await,
                ),
                toast: None,
            }),
            // ADR-0083 §3: a File Viewer's 「← 返回目录」 restores the listing
            // it was opened from — a pure rebuild of the payload's own state
            // (directory, filter, hidden toggle, page), so `base` is used
            // verbatim. `go` cannot serve it: a drill-down deliberately clears
            // the filter and returns to page 1.
            "restore" => Some(CardActionResult {
                card: Some(self.build_dir_browse_for(core, &thread_key, base).await),
                toast: None,
            }),
            "page" => Some(CardActionResult {
                card: Some(self.build_dir_browse_for(core, &thread_key, base).await),
                toast: None,
            }),
            "show_hidden" => Some(CardActionResult {
                card: Some(
                    self.build_dir_browse_for(
                        core,
                        &thread_key,
                        BrowseView {
                            show_hidden: !base.show_hidden,
                            page: 1,
                            ..base
                        },
                    )
                    .await,
                ),
                toast: None,
            }),
            "pick" => self.browse_pick(core, &thread_key, base, deadline).await,
            "topic" => {
                let Some(directory) = resolve_browse_dir(&target) else {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(format!("目录不存在：{target}")),
                    });
                };
                let open_message_id = value.get("open_message_id").and_then(|v| v.as_str());
                match open_topic_from_card(core, &thread_key, open_message_id, directory.clone()).await {
                    Ok(display) => Some(CardActionResult {
                        card: Some(
                            self.build_dir_browse_for(
                                core,
                                &thread_key,
                                BrowseView {
                                    dir: &directory,
                                    ..base
                                },
                            )
                            .await,
                        ),
                        toast: Some(format!("已建话题（{display}）——下一条消息创建会话")),
                    }),
                    Err(rejection) => Some(rejection),
                }
            }
            _ => Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            }),
        }
    }

    /// Handle a browse card's **file row** — and a Tool Panel's **「📂 打开」**
    /// (ADR-0083 §3, §8): read the file and answer its **File Viewer** card,
    /// which replaces the clicked card *in place* — the ack patches the
    /// clicked message, the same move as the browse card replacing the Recent
    /// Directories card, so the listing is never left as a second message.
    ///
    /// The row's payload carries the listing's own state (`dir`/`keyword`/
    /// `page`/`show_hidden`), so the viewer's 「← 返回目录」 restores it exactly.
    /// The action tag is `file`, never the browse card's `dir`: a file row
    /// offers no 选择/建话题, because declaring a Session requires a directory
    /// (the pick op refuses a file path on its own too).
    ///
    /// A tool panel's 打开 carries the Session instead of the Chat/Topic (a
    /// live Turn card knows no Chat — see
    /// [`crate::feishu::card::session::tool_file_value`]): the click resolves
    /// the Session's Chat/Topic from the SessionStore — the same read the
    /// card-action span context makes, and the rule a live card's own
    /// section-level buttons follow (`ledger::cleanup_button`) — so the viewer
    /// opens with working controls. Its structured path resolves on the Path
    /// Scope (ADR-0083 §4): `~` expands, a relative path joins the Session's
    /// directory, an absolute path passes through. A Session the store no
    /// longer knows answers an honest Toast, never a viewer whose controls
    /// route nowhere.
    async fn handle_file_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let session_id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty());
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let mut thread_key = thread_key_from_value(value);
        let session_dir = {
            let sessions = self.sessions.lock().await;
            let mut session_dir = None;
            if let Some(sid) = session_id {
                if thread_key.chat_id.is_empty()
                    && let Some(resolved) = sessions.thread_for_session(sid)
                {
                    thread_key = resolved;
                }
                session_dir = sessions.directory_for_session(sid);
            }
            value
                .get("session_dir")
                .and_then(|v| v.as_str())
                .filter(|dir| !dir.is_empty())
                .map(str::to_string)
                .or(session_dir)
                .or_else(|| sessions.current_directory(&thread_key))
        };
        // The row's `path` comes straight from the browse scan, so it names a
        // filesystem entry, not free text: it is read **verbatim**. Trimming it
        // would open a different file (or none) for a name that ends in — or is
        // surrounded by — whitespace, which Unix allows (the `/cat <path>` lane
        // may still trim, because there the path IS typed by hand).
        let path = value
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        // A tool panel's 打开 without a Session behind it — and no Chat routing
        // on the payload to fall back to — cannot route the viewer's own
        // controls: say so, and leave the card standing.
        if session_id.is_some() && thread_key.chat_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!("⚠️ 打不开 `{path}`：会话已不在。")),
            });
        }
        // A tool panel's structured path resolves on the Path Scope (an
        // absolute scan path resolves to itself, so browse rows never take
        // this lane — only a payload carrying a Session does).
        let resolved = match (session_id, session_dir.as_deref()) {
            (Some(_), Some(dir)) => crate::bridge::command::lexical_normalize(
                &crate::bridge::command::expand_against(&path, std::path::Path::new(dir)),
            ),
            _ => std::path::PathBuf::from(&path),
        };
        // The listing to return to, when the row came from one
        // (`browse_file_value`). Absent — a viewer opened from a tool panel
        // (#743) — leaves the viewer without a 返回目录 control. The viewer's
        // own pager (`op: "page"`, #740) carries the same listing as `return_*`
        // — strings only, because the shared pager holds string callback
        // fields — and the TARGET viewer page as `page` (a row's `page` is the
        // LISTING's, so the op is what tells the two shapes apart).
        let (home_dir, home_keyword, home_hidden, home_page, page) =
            if matches!(op, "page" | "reread" | "view") {
                (
                    value
                        .get("return_dir")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    value
                        .get("return_keyword")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    return_hidden(value),
                    return_page_num(value),
                    viewer_page(value),
                )
            } else {
                (
                    value
                        .get("dir")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    value
                        .get("keyword")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    parse_show_hidden(value),
                    value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize,
                    1,
                )
            };
        // The pager flip and the markdown view toggle share ONE bounded rebuild
        // (#740, #781): both re-read the file and rebuild the card at the
        // payload's `(page, view)`, so the toggle needs no second machinery. The
        // view rides the payload like the page does (ADR-0052), and an absent
        // one reads as the default.
        if matches!(op, "page" | "view") {
            let toggling = op == "view";
            let busy = if toggling {
                "正在切换视图…"
            } else {
                "正在翻页…"
            };
            let Some(open_message_id) = value
                .get("open_message_id")
                .and_then(|v| v.as_str())
                .filter(|id| !id.is_empty())
            else {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("无法翻页：找不到文件查看卡。".to_string()),
                });
            };
            let Some(in_flight_guard) = self.claim_viewer_action(&thread_key, open_message_id) else {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(busy.to_string()),
                });
            };
            let app = Arc::clone(self);
            let page = ViewerPage {
                thread_key,
                path: resolved.to_string_lossy().to_string(),
                session_dir: session_dir.clone(),
                card_message_id: open_message_id.to_string(),
                return_to: OwnedReturnTo {
                    dir: (!home_dir.is_empty()).then(|| home_dir.clone()),
                    keyword: home_keyword.clone(),
                    show_hidden: home_hidden,
                    page: home_page,
                },
                page,
                view: crate::feishu::card::viewer::ViewerView::from_payload(value),
                lane: crate::feishu::card::viewer::lane_from_page_payload(value),
                payload_identity: value
                    .get("viewer_file_identity")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|identity| identity.parse().ok()),
                in_flight_guard,
            };
            tokio::spawn(async move { app.run_viewer_page(page).await });
            return Some(CardActionResult {
                card: None,
                toast: Some(busy.to_string()),
            });
        }
        if op == "reread" {
            // The viewer's own Feishu message id cannot be serialized when its
            // card is built (the send returns that id afterward). ws injects
            // it from callback context; it is the target for the in-place PATCH.
            let Some(open_message_id) = value
                .get("open_message_id")
                .and_then(|v| v.as_str())
                .filter(|id| !id.is_empty())
            else {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("无法重新读取：找不到文件查看卡。".to_string()),
                });
            };
            let Some(in_flight_guard) = self.claim_viewer_action(&thread_key, open_message_id) else {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("正在重新读取…".to_string()),
                });
            };
            let app = Arc::clone(self);
            let card_message_id = open_message_id.to_string();
            let path = resolved.to_string_lossy().to_string();
            let rich_message_id = value
                .get("rich_message_id")
                .and_then(|v| v.as_str())
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            let sent_file_identity = value
                .get("viewer_sent_file_identity")
                .and_then(serde_json::Value::as_str)
                .and_then(|identity| identity.parse().ok());
            let ambiguous_file_identity = value
                .get("viewer_ambiguous_file_identity")
                .and_then(serde_json::Value::as_str)
                .and_then(|identity| identity.parse().ok());
            let embedded_image_key = value
                .get("viewer_embedded_image_key")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let return_to = OwnedReturnTo {
                dir: (!home_dir.is_empty()).then(|| home_dir.clone()),
                keyword: home_keyword.clone(),
                show_hidden: home_hidden,
                page: home_page,
            };
            // A re-read keeps the view the reader chose (#781).
            let view = crate::feishu::card::viewer::ViewerView::from_payload(value);
            tokio::spawn(async move {
                app.run_viewer_reread(ViewerReread {
                    thread_key,
                    path,
                    session_dir: session_dir.clone(),
                    card_message_id,
                    rich_message_id,
                    sent_file_identity,
                    ambiguous_file_identity,
                    embedded_image_key,
                    return_to,
                    page,
                    view,
                    in_flight_guard,
                })
                .await;
            });
            return Some(CardActionResult {
                card: None,
                toast: Some("正在重新读取…".to_string()),
            });
        }
        // Both click paths take the same bounded background read, but their
        // payloads distinguish delivery: browse rows carry the listing state
        // (`dir`/`keyword`/`page`, or `return_*` on a viewer page), while a Tool
        // Panel carries `session_id` and no listing. The latter must reply with
        // a standalone viewer message, never PATCH the live Turn card.
        // Even a fitting text file must classify off this callback path because
        // a host read can block (e.g. a network mount).
        // Pager flips are handled above with their own bounded background path.
        if !op.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        let Some(open_message_id) = value
            .get("open_message_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
        else {
            return Some(CardActionResult {
                card: None,
                toast: Some("无法打开文件：找不到文件浏览卡。".to_string()),
            });
        };
        let Some(in_flight_guard) = self.claim_viewer_action(&thread_key, open_message_id) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("正在打开文件…".to_string()),
            });
        };
        let app = Arc::clone(self);
        let core = Arc::clone(core);
        let standalone = session_id.is_some();
        let message_id = open_message_id.to_string();
        let path = resolved.to_string_lossy().to_string();
        let return_to = OwnedReturnTo {
            dir: (!home_dir.is_empty()).then(|| home_dir.clone()),
            keyword: home_keyword.clone(),
            show_hidden: home_hidden,
            page: home_page,
        };
        tokio::spawn(async move {
            app.run_viewer_open(ViewerOpen {
                thread_key,
                path,
                session_dir,
                standalone,
                card_message_id: message_id,
                return_to,
                page,
                core,
                in_flight_guard,
            })
            .await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some("正在打开文件…".to_string()),
        })
    }

    /// Finish a pager flip or a markdown view toggle (#740, #781) after the
    /// callback has acknowledged. The carried lane is retained while the bounded
    /// read rebuilds the requested `(page, view)`.
    async fn run_viewer_page(self: &Arc<Self>, page: ViewerPage) {
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
    async fn run_viewer_reread(self: &Arc<Self>, reread: ViewerReread) {
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
    async fn run_viewer_open(self: &Arc<Self>, open: ViewerOpen) {
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

    /// The browse card's 选择: declare a Pending Session in `view.dir` (Lazy
    /// Session Creation, ADR-0041) — the card form of `/dir <path>`. A target
    /// that is not a directory is refused with a toast instead.
    async fn browse_pick(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        view: BrowseView<'_>,
        deadline: tokio::time::Instant,
    ) -> Option<CardActionResult> {
        // The whole click shares ONE deadline (ADR-0089), started by
        // `handle_card_action`: the topic guard's read, the current-directory
        // read, the pending write and the refresh below all ride it, so a click
        // can never commit a pending after Feishu has stopped listening.
        // ADR-0007: a bound topic owns exactly one session. `/dir` and the
        // `/dir` card are refused there at command entry
        // (`Command::topic_rejection`), but `/cat <directory>` legitimately
        // renders the same browse card inside a topic (a read surface, never
        // gated — spec #669 story 41). Its 选择/切换到这里 must therefore
        // refuse the same way here, or a click would supersede the topic's
        // session through the pending declaration below.
        if thread_key.thread_id != thread_key.chat_id {
            match tokio::time::timeout_at(deadline, core.sessions.lock()).await {
                Ok(store) if store.get_active(thread_key).is_some() => {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(crate::bridge::command::TOPIC_SELECTION_REJECTION.to_string()),
                    });
                }
                // A guard that cannot answer in time also refuses: the topic's
                // one-session rule must not be bypassed by a slow store.
                Err(_) => {
                    return Some(CardActionResult {
                        card: None,
                        toast: Some(CARD_STORE_TIMEOUT.to_string()),
                    });
                }
                Ok(_) => {}
            }
        }
        // Resolving the target is filesystem work (`canonicalize` + `is_dir`),
        // and a synchronous call blocks the executor thread where no deadline
        // can reach it — so it runs on the blocking pool, bounded by the click's
        // deadline. Past that, the click refuses like any other overrun.
        let target = view.dir.to_string();
        let resolution = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || resolve_browse_dir(&target)),
        )
        .await;
        let directory = match resolution {
            Ok(Ok(Some(directory))) => directory,
            // The scan itself failed (a join error): report the path as
            // unusable, exactly as an absent directory reads.
            Ok(Err(_)) | Ok(Ok(None)) => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(format!("目录不存在：{}", view.dir)),
                });
            }
            Err(_) => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(CARD_STORE_TIMEOUT.to_string()),
                });
            }
        };
        // Pending-first (ADR-0041): 当前 is the pending's directory when one
        // exists, else the active session's. The comparison is the SAME one the
        // card's ✅ 当前 mark uses (`command::same_dir`), so a directory the card
        // marks current is never re-declared by a click.
        let current_dir = tokio::time::timeout_at(deadline, core.sessions.lock())
            .await
            .ok()
            .and_then(|store| store.current_directory(thread_key));
        let settled = BrowseView {
            dir: &directory,
            ..view
        };
        if current_dir.is_some_and(|current| crate::bridge::command::same_dir(&current, &directory)) {
            return Some(CardActionResult {
                card: card_before(deadline, self.build_dir_browse_for(core, thread_key, settled)).await,
                toast: Some("已在当前目录".to_string()),
            });
        }
        Some(
            match declare_pending_dir(core, thread_key, &directory, deadline).await {
                Some(toast) => CardActionResult {
                    card: card_before(deadline, self.build_dir_browse_for(core, thread_key, settled)).await,
                    toast: Some(toast),
                },
                None => CardActionResult {
                    card: None,
                    toast: Some(CARD_STORE_TIMEOUT.to_string()),
                },
            },
        )
    }

    /// Handle an `/agent` picker-card button: record the per-session override
    /// (action `agent`) — and on a durable generation (V2) switch the session's
    /// agent — or clear it via the dedicated `agent_clear` button (which on V2
    /// switches to the server's default agent id). Clearing is carried by the
    /// ACTION tag, never a value word — an agent literally named like a clear
    /// verb stays selectable (ADR-0020), so `clear` is a dispatch flag, not a
    /// value predicate. Refreshes the card so the current agent updates.
    async fn handle_agent_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        clear: bool,
    ) -> Option<CardActionResult> {
        let picked = value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let thread_key = thread_key_from_value(value);
        let agent = if clear { None } else { Some(picked.clone()) };
        let outcome = core.pick_agent(&thread_key, agent).await;
        match classify_pick(
            outcome,
            "Agent",
            crate::bridge::display::feishu_side_label(&thread_key),
        ) {
            PickReply::Applied { .. } => {}
            PickReply::Failed(text) => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(text),
                });
            }
        }
        let (card, _error) =
            crate::feishu::card::command::agent_card(&core.command_handles(), &thread_key).await;
        Some(CardActionResult {
            card,
            toast: Some(if clear {
                "已清除 Agent（回到服务器默认）".to_string()
            } else {
                format!("Agent: {picked}（下一条消息开始生效）")
            }),
        })
    }

    /// Handle a `/model` picker-card button. Two levels (the provider → model
    /// flow): a `level: "provider"` button either opens a provider's model list
    /// or (`value == PICKER_BACK_TO_PROVIDERS`) returns to the full provider
    /// list; a `level: "model"` button records the per-session override.
    async fn handle_model_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        callback_started: tokio::time::Instant,
    ) -> Option<CardActionResult> {
        // The ack deadline is anchored at the callback's **true start**, not at
        // this handler: gate access, span setup and the store reads before the
        // dispatch already spent part of the budget. The current-selection
        // ladder and the backend read are charged against what is left.
        let deadline = callback_started + crate::feishu::ws::CARD_ACK_BUDGET;
        let thread_key = thread_key_from_value(value);
        let picked = value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // A scope states what its pick does (`PickKind`): navigation asks the
        // client for the next page; every other scope is a concrete pick. The
        // bridge never infers the action from a scope id (OpenCode's and
        // Codex's provider scopes are both named `provider` but do opposite
        // things — #910).
        let level = value.get("level").and_then(|v| v.as_str()).unwrap_or("");
        if level == crate::feishu::card::picker::PICKER_LEVEL_PROVIDER {
            let back = picked != crate::feishu::card::picker::PICKER_BACK_TO_PROVIDERS;
            let (scope, selected) = if back {
                (Some("provider"), Some(picked.as_str()))
            } else {
                (None, None)
            };
            let handles = core.command_handles();
            let surface = crate::feishu::card::command::model_surface(
                &handles,
                &thread_key,
                scope,
                selected,
                Some(deadline),
            )
            .await;
            let cards = crate::feishu::card::picker::build_model_surface_cards(&thread_key, &surface, back);
            return Some(Self::picker_pages_result(core, &thread_key, cards).await);
        }

        // A **paired** pick names a provider *and* the model it must run
        // (issue #910): a `[profiles.*]` preset, a probed provider model, or a
        // row of the provider scope. The path is decided by whether that
        // provider differs from the thread's, never by which scope the row came
        // from — a bare provider is not a valid pick.
        let paired = value
            .get("provider")
            .and_then(|v| v.as_str())
            .filter(|provider| !provider.is_empty());
        if let Some(provider) = paired {
            return Some(self.pick_paired_model(core, &thread_key, provider, &picked).await);
        }

        Some(self.pick_model_ref(core, &thread_key, &picked).await)
    }

    /// A concrete **model-only** pick: record the override (and on a durable
    /// generation switch the session's model), carrying or clearing the
    /// `/think` variant (ADR-0020), same as the `/model` text form. Nothing
    /// thread-level happens: this is the same-provider path of issue #910.
    async fn pick_model_ref(
        &self,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        picked: &str,
    ) -> CardActionResult {
        if crate::backend::parsing::parse_model(picked).is_none() {
            return CardActionResult {
                card: None,
                toast: Some("模型格式应为 <provider>/<model>".to_string()),
            };
        }
        let cleared_variant = match classify_pick(
            core.pick_model(thread_key, picked).await,
            "模型",
            crate::bridge::display::feishu_side_label(thread_key),
        ) {
            PickReply::Applied { cleared_variant } => cleared_variant,
            PickReply::Failed(text) => {
                return CardActionResult {
                    card: None,
                    toast: Some(text),
                };
            }
        };
        let extra = cleared_variant
            .map(|v| format!("（已清除思考等级 `{v}`：新模型不支持）"))
            .unwrap_or_default();
        CardActionResult {
            card: None,
            toast: Some(format!("Model: {picked}（下一条消息开始生效）{extra}")),
        }
    }

    /// A **provider + model pair** (issue #910). A provider is a thread-level
    /// fact with no per-turn field, so a pick that changes it must re-open the
    /// thread — while a pick whose provider is the thread's own is an ordinary
    /// model pick (no resume, no unsubscribe). A running turn defers the switch
    /// to the next prompt instead of failing now; an existing idle thread asks
    /// for confirmation first.
    async fn pick_paired_model(
        &self,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        provider: &str,
        model: &str,
    ) -> CardActionResult {
        if model.trim().is_empty() {
            // The pair is what makes the switch safe: the old model name on a
            // new provider is a 404.
            return CardActionResult {
                card: None,
                toast: Some(format!(
                    "provider `{provider}` 需要同时选择模型（可点同一行的 model，或在 `[profiles.*]` 里声明一对）"
                )),
            };
        }
        let spec = format!("{provider}/{model}");
        let Some(settings) = core.session_settings(thread_key).await else {
            return CardActionResult {
                card: None,
                toast: Some(format!(
                    "{}还没有会话",
                    crate::bridge::display::feishu_side_label(thread_key)
                )),
            };
        };
        // A Pending Session takes both halves at creation: `thread/start` carries
        // `model` + `modelProvider` in one call.
        if settings.session_id.is_none() {
            return self.save_pending_pair(core, thread_key, provider, &spec).await;
        }
        // The rule is shared with the `/model` text form: a model-only pick is
        // recorded; a provider change needs the gate and a confirmation; nothing
        // unclassifiable is recorded.
        use crate::bridge::handles::ModelPickDecision;
        match core
            .sessions_handle()
            .model_pick_decision(&core.client, thread_key, provider, model)
            .await
        {
            ModelPickDecision::Record => self.pick_model_ref(core, thread_key, &spec).await,
            ModelPickDecision::Confirm {
                session_id,
                provider,
                model,
            } => CardActionResult {
                // Idle or mid-turn makes no difference: the pair is **recorded**
                // by the confirmation, and the next prompt's pre-flight applies
                // it before submitting (the owner's 「下一轮生效」).
                card: Some(crate::feishu::card::picker::build_provider_confirm_card(
                    thread_key,
                    &session_id,
                    &provider,
                    &model,
                )),
                toast: None,
            },
            ModelPickDecision::Refuse(text) => CardActionResult {
                card: None,
                toast: Some(text),
            },
        }
    }

    /// Record a Pending Session's provider + model pair (issue #910). Both
    /// halves are stored — the model mirror names the pair, the pending carries
    /// the provider for `thread/start` — and a write that did **not** land is
    /// reported as a failure: a click that could not be saved must never answer
    /// "Applied".
    async fn save_pending_pair(
        &self,
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        provider: &str,
        spec: &str,
    ) -> CardActionResult {
        // One write for both halves: a partial pair would be applied at thread
        // creation, and the model is what makes the provider safe (issue #910).
        match core
            .sessions_handle()
            .pick_pending_pair(&core.client, thread_key, provider, spec)
            .await
        {
            Ok(PickOutcome::Applied { cleared_variant }) => CardActionResult {
                card: None,
                toast: Some(format!(
                    "Provider: {provider}（模型 {spec}，新线程创建时生效）{}",
                    cleared_variant
                        .map(|v| format!("（已清除思考等级 `{v}`）"))
                        .unwrap_or_default()
                )),
            },
            Ok(PickOutcome::StateChanged) => CardActionResult {
                card: None,
                toast: Some(
                    "⚠️ 保存失败：会话正在创建或状态已变化，这次选择没有生效（可稍后在会话里重选）"
                        .to_string(),
                ),
            },
            Ok(_) => CardActionResult {
                card: None,
                toast: Some("⚠️ 保存失败：会话状态已变化，这次选择没有生效".to_string()),
            },
            Err(error) => CardActionResult {
                card: None,
                toast: Some(format!("⚠️ 保存失败：{error}")),
            },
        }
    }

    /// The confirmation card's 确认 click (issue #910). The target is resolved
    /// **here**, so a card that outlived a `/switch`, a delete or a recreate
    /// does nothing; the gate is re-read, so a card sitting in Feishu cannot
    /// record a switch after `provider_switch_on_resume` was turned off.
    ///
    /// This only **records** the pair: the thread is not touched. The next
    /// prompt's pre-flight (`Turn::attempt`) applies it before submitting — the
    /// owner's 「下一轮生效」 — so the click answers synchronously and no work runs
    /// off the ack.
    async fn handle_provider_switch_confirm(
        &self,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let provider = value
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let card_session = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let thread_key = thread_key_from_value(value);
        if provider.is_empty() || model.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("这张确认卡缺少 provider/model，没有做任何改动".to_string()),
            });
        }
        let Some(settings) = core.session_settings(&thread_key).await else {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!(
                    "{}还没有会话",
                    crate::bridge::display::feishu_side_label(&thread_key)
                )),
            });
        };
        let Some(session_id) = settings.session_id.clone() else {
            return Some(CardActionResult {
                card: None,
                toast: Some("这张确认卡已过期：会话已切换或尚未创建，没有做任何改动".to_string()),
            });
        };
        if card_session.is_empty() || card_session != session_id {
            return Some(CardActionResult {
                card: None,
                toast: Some("这张确认卡已过期（会话已切换），没有做任何改动".to_string()),
            });
        }
        // Re-checked at confirmation: the config may have been turned off while
        // the card sat in the chat. The pre-flight enforces it again before the
        // thread is ever touched.
        if core.client.provider_change_scope() != crate::backend::types::ChangeScope::ResumeOnly {
            return Some(CardActionResult {
                card: None,
                toast: Some("provider 只能在新线程切换".to_string()),
            });
        }
        let pair = format!("{provider}/{model}");
        // Record the pair as the session's next-turn model. The same gated,
        // compare-and-set write a `/model` pick uses, so a concurrent pick (or a
        // remap, or a materialising session) is an honest refusal, never an
        // overwrite.
        // The card named a session: if `/switch` remapped the chat in between,
        // this is a no-op instead of a record on the new session (issue #910).
        let saved = core
            .pick_model_expecting(&thread_key, Some(&session_id), &pair)
            .await;
        match classify_pick(
            saved,
            "Provider",
            crate::bridge::display::feishu_side_label(&thread_key),
        ) {
            PickReply::Applied { cleared_variant } => {
                let extra = cleared_variant
                    .map(|v| format!("（已清除思考等级 `{v}`）"))
                    .unwrap_or_default();
                Some(CardActionResult {
                    card: Some(crate::feishu::card::picker::build_provider_recorded_card(
                        &provider, &model,
                    )),
                    toast: Some(format!("已记录：下一条消息发出前切换{extra}")),
                })
            }
            PickReply::Failed(text) => Some(CardActionResult {
                card: Some(crate::feishu::card::picker::build_provider_save_blocked_card(
                    &pair, &text,
                )),
                toast: Some(text),
            }),
        }
    }

    /// Handle a `/think` card button: record the chosen variant (action
    /// `think`), or clear it via the dedicated `think_clear` button. Clearing
    /// is carried by the ACTION tag, never a value word — a variant literally
    /// named like a clear verb stays selectable (ADR-0020), so `clear` is a
    /// dispatch flag, not a value predicate. Refreshes the card so the current
    /// selection updates. Best-effort validation: a pick is rejected only when
    /// the effective model is resolvable AND positively lacks the variant
    /// (ADR-0020); unknown models fall through to the server's
    /// `VariantUnavailableError`.
    async fn handle_think_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
        clear: bool,
    ) -> Option<CardActionResult> {
        let picked = value
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let thread_key = thread_key_from_value(value);
        let Some(settings) = core.session_settings(&thread_key).await else {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!(
                    "{}还没有会话",
                    crate::bridge::display::feishu_side_label(&thread_key)
                )),
            });
        };
        if !clear
            && let Some((provider, model)) = core.effective_model(&settings).await
            && let Some(variants) = core.model_variants(&provider, &model).await
            && !variants.iter().any(|v| v == &picked)
        {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!("当前模型 `{provider}/{model}` 不支持思考等级 `{picked}`")),
            });
        }
        match classify_pick(
            core.pick_think(&thread_key, if clear { None } else { Some(picked.clone()) })
                .await,
            "思考等级",
            crate::bridge::display::feishu_side_label(&thread_key),
        ) {
            PickReply::Applied { .. } => {}
            PickReply::Failed(text) => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(text),
                });
            }
        }
        let (card, _error) =
            crate::feishu::card::command::think_card(&core.command_handles(), &thread_key).await;
        Some(CardActionResult {
            card,
            toast: Some(if clear {
                "已清除思考等级（回到模型默认）".to_string()
            } else {
                format!("Thinking: {}（下一条消息开始生效）", picked)
            }),
        })
    }

    /// Turn a page of picker cards into a card-action result: the FIRST card
    /// replaces the clicked one in place (via the ack); the remaining pages are
    /// posted into the chat so the full picker stays visible.
    async fn picker_pages_result(
        core: &Arc<SharedCore>,
        thread_key: &ThreadKey,
        cards: Vec<serde_json::Value>,
    ) -> CardActionResult {
        let (first, rest) = cards.split_first().map_or((None, &[][..]), |(f, r)| (Some(f), r));
        for card in rest {
            let _ = core.feishu.send_card("chat_id", &thread_key.chat_id, card).await;
        }
        CardActionResult {
            card: first.cloned(),
            toast: None,
        }
    }

    /// Handle a `/skill` picker card callback (#664): `op: "search"` and
    /// `op: "page"` rebuild the card in place ([`Self::rebuild_skill_card`]);
    /// anything else is a ROW TAP (spec #652, ticket #656): a row carries the
    /// skill's exact id, and the tap dispatches it straight to
    /// [`Self::handle_skill_command`] — the same submit path a typed command
    /// takes, minus the text parse. The id is NOT round-tripped as
    /// `/skill <id>` text: V1's id IS the skill name, which can contain
    /// whitespace (`Implement Spec`), and `/skill Implement Spec` would parse as
    /// two ids and resolve neither. The click must ack within 3s while a turn
    /// runs for the length of a model reply, so the submission is spawned off
    /// the ack; the picker card stays put (the dedicated loaded-skill card
    /// replies under the ORIGINAL user message, spec #655), and the
    /// agent/autoaccept toasts already established that a picker does not
    /// replace itself on a pick.
    /// Route a Terminal Card control (ADR-0086 decision 6). The handler never
    /// touches the process group: it looks the run up by the button's run id,
    /// refuses a stale id, and enqueues the signal for the run loop. A control
    /// is refused by state, not hidden, so a redelivered click is a no-op.
    async fn handle_terminal_action(self: &Arc<Self>, value: &serde_json::Value) -> Option<CardActionResult> {
        let thread_key = thread_key_from_value(value);
        if thread_key.chat_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("这张卡片缺少会话信息，无法操作。".to_string()),
            });
        }
        let run_id = value.get("run_id").and_then(|v| v.as_str()).unwrap_or("");
        let op = value
            .get("op")
            .and_then(|v| v.as_str())
            .and_then(crate::bridge::terminal::TerminalOp::parse);
        let Some(op) = op else {
            return Some(CardActionResult {
                card: None,
                toast: Some("未知的终端操作。".to_string()),
            });
        };
        let outcome = {
            let runs = self
                .terminal_runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            runs.control(&thread_key, run_id, op)
        };
        let toast = match outcome {
            crate::bridge::terminal::TerminalControlOutcome::Accepted => match op {
                crate::bridge::terminal::TerminalOp::Kill => "⏹ 正在终止…",
                crate::bridge::terminal::TerminalOp::Continue => "⏱ 已延长时限，继续跟踪输出…",
            },
            crate::bridge::terminal::TerminalControlOutcome::Stale => "这张终端卡片的操作已过期。",
            crate::bridge::terminal::TerminalControlOutcome::NotPastCap => "命令尚未到达时限。",
        };
        Some(CardActionResult {
            card: None,
            toast: Some(toast.to_string()),
        })
    }

    /// Handle a 「↻ 重跑」 click (ADR-0086 decisions 4 and 5). The claim and the
    /// stale check happen together, synchronously, inside the callback — cheap,
    /// so the ack is never held — and the run itself is spawned off it. The
    /// command, cwd and timeout come from the **Terminal Run Record**, which is
    /// what makes the button work after a restart and what a multi-kilobyte
    /// command is kept out of the card's JSON for.
    async fn handle_terminal_rerun(self: &Arc<Self>, value: &serde_json::Value) -> Option<CardActionResult> {
        let thread_key = thread_key_from_value(value);
        if thread_key.chat_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("这张卡片缺少会话信息，无法操作。".to_string()),
            });
        }
        let run_id = value.get("run_id").and_then(|v| v.as_str()).unwrap_or("");
        // The clicked card is this run's new reply target: a rerun posts under
        // the card it was clicked on, which is where its reader is looking.
        let Some(message_id) = value.get("open_message_id").and_then(|v| v.as_str()) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("这条操作缺少消息位置，无法重跑。".to_string()),
            });
        };
        let message_id = message_id.to_string();
        let requester = value
            .get("operator_open_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let claim = crate::bridge::terminal::claim_rerun(&self.command_handles(), &thread_key, run_id);
        let toast = match claim {
            crate::bridge::terminal::RerunClaim::Stale => "这张终端卡片的操作已过期。",
            // The slot is held (the command this card names, or a successor,
            // is still running): the record is untouched, so the card's own id
            // stays current for a later click.
            crate::bridge::terminal::RerunClaim::Busy => {
                "这个会话已有终端命令在运行，请先点击运行卡片的「⏹ 终止」。"
            }
            crate::bridge::terminal::RerunClaim::Accepted(claimed) => {
                let app = Arc::clone(self);
                tokio::spawn(async move {
                    if let Err(e) = crate::bridge::terminal::rerun(
                        &app.command_handles(),
                        claimed,
                        crate::bridge::terminal::TerminalRequest {
                            message_id: &message_id,
                            requester_open_id: requester.as_deref(),
                            // Replaced from the record inside `rerun`: a
                            // button payload carries no chat type, and the
                            // record does.
                            is_group: false,
                        },
                    )
                    .await
                    {
                        tracing::error!("Terminal rerun: {}", e);
                    }
                });
                "↻ 正在重跑…"
            }
        };
        Some(CardActionResult {
            card: None,
            toast: Some(toast.to_string()),
        })
    }

    async fn handle_skill_card_action(
        self: &Arc<Self>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        if op == "search" || op == "page" {
            return Some(self.rebuild_skill_card(value).await);
        }
        let id = value.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            return None;
        }
        let app = Arc::clone(self);
        let value = value.clone();
        let toast = format!("正在加载技能 {id}…");
        let id = id.to_string();
        tokio::spawn(async move {
            // The Chat/Topic kind the picker was sent in: every button carries
            // it next to chat_id/thread_id, since a click has no chat_type of
            // its own (see `build_skill_card`).
            let callback_string =
                |key: &str| value.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let chat_id = callback_string("chat_id");
            // A non-topic key's `thread_id` IS its `chat_id` (ThreadKey), so
            // drop it: re-classifying must not mistake the top level for a
            // topic. A real topic's `thread_id` differs and is kept.
            let thread_id = value
                .get("thread_id")
                .and_then(|v| v.as_str())
                .filter(|id| *id != chat_id)
                .map(str::to_string);
            let chat_type = {
                let t = callback_string("chat_type");
                if t.is_empty() { "group".to_string() } else { t }
            };
            let requester = value
                .get("operator_open_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // The loaded-skill card (and the turn's card) must reply under the
            // ORIGINAL user message the picker was sent under, exactly like a
            // typed dispatch — not under the picker card. The picker's buttons
            // carry that id (`reply_message_id`, see `build_skill_card`); a card
            // built before that field existed falls back to the picker's own
            // message.
            let reply_to = {
                let carried = callback_string("reply_message_id");
                if carried.is_empty() {
                    callback_string("open_message_id")
                } else {
                    carried
                }
            };
            let kind = ConversationKind::classify(&chat_type, thread_id.as_deref());
            let thread_key = kind.thread_key(&chat_id, thread_id.as_deref());
            app.handle_skill_command(
                thread_key,
                crate::bridge::IncomingMessage {
                    message_id: reply_to,
                    chat_id,
                    chat_type,
                    thread_id,
                    parent_id: None,
                    text: format!("#{id}"),
                    images: vec![],
                    files: vec![],
                    omitted: vec![],
                    requester_open_id: requester,
                },
                crate::bridge::command::SkillInvocation {
                    ids: vec![id],
                    picker_on_empty: true,
                },
            )
            .await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some(toast),
        })
    }

    /// Rebuild the `/skill` picker card in place for an `op: "search"` /
    /// `op: "page"` callback (#664). The filter (keyword, page) rides the button
    /// payload (ADR-0052); the skill list is re-read from the server — unlike
    /// `/dir`, which reads the local store — scoped to the Chat/Topic's project
    /// directory. Best-effort: a failed read degrades to the no-skills state.
    /// The `chat_type` + `reply_message_id` the picker carries ride back onto the
    /// fresh rows, so a later tap still replies the loaded-skill card under the
    /// ORIGINAL user message (spec #655). No Lazy Start here: a rebuild never
    /// submits a prompt, and the ack must stay fast.
    async fn rebuild_skill_card(self: &Arc<Self>, value: &serde_json::Value) -> CardActionResult {
        let thread_key = thread_key_from_value(value);
        let keyword = value
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // A missing/garbage page reads as 1; the builder clamps an out-of-range
        // page to the last.
        let page = value.get("page").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        // The picker's own `chat_type`, absent only on the WS name-only
        // form-submit fallback. Defaulting to "group" is inert for routing:
        // `ConversationKind::thread_key` keys every non-topic kind on `chat_id`,
        // and a real topic is recognised by its distinct `thread_id`, not the
        // chat type — so the rebuilt rows route identically either way. Only the
        // tap's reply target degrades (below), the accepted fallback shape.
        let chat_type = value
            .get("chat_type")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .unwrap_or("group")
            .to_string();
        // The original user message the picker replied under; a card built
        // before this field existed (or the name-only form-submit fallback)
        // falls back to the picker card's own message.
        let reply_message_id = value
            .get("reply_message_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .or_else(|| value.get("open_message_id").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let card = self
            .build_skill_card_for(&thread_key, &keyword, page, &chat_type, &reply_message_id)
            .await;
        CardActionResult {
            card: Some(card),
            toast: None,
        }
    }

    /// Read the skills for `thread_key`'s project directory, narrow them by
    /// `keyword` (id / name / description), and window them to `page` (#664).
    async fn build_skill_card_for(
        self: &Arc<Self>,
        thread_key: &ThreadKey,
        keyword: &str,
        page: usize,
        chat_type: &str,
        message_id: &str,
    ) -> serde_json::Value {
        let directory = self.command_handles().current_project_directory(thread_key).await;
        let skills = self
            .client
            .list_skills((!directory.is_empty()).then_some(directory.as_str()))
            .await;
        let filtered = crate::feishu::card::picker::filter_skills(&skills, keyword);
        crate::feishu::card::picker::build_skill_card(
            thread_key, &filtered, keyword, page, chat_type, message_id, None,
        )
    }

    /// Handle an `/autoaccept` toggle-card button: switch the flag and refresh
    /// the card.
    async fn handle_autoaccept_card_action(
        self: &Arc<Self>,
        core: &Arc<SharedCore>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let on = value.get("value").and_then(|v| v.as_str()) == Some("on");
        let thread_key = thread_key_from_value(value);
        let settings = core.session_settings(&thread_key).await;
        // Only a real session has requests to approve; a Pending Session has
        // none yet (ADR-0041).
        let active = settings
            .as_ref()
            .and_then(|s| s.session_id.clone().map(|id| (id, s.directory.clone())));
        let mut approved = Vec::new();
        if on && let Some((id, directory)) = &active {
            approved = core.approve_pending_for_session(id, directory).await;
        }
        if let Some(mut s) = settings {
            s.auto_accept = on;
            if let Err(err) = core.set_session_settings(&thread_key, s).await {
                tracing::warn!("autoaccept card: persist failed: {}", err);
            }
        }
        // The same residue the permission card's toggle and `/autoaccept on`
        // leave: ONE mode receipt and the approved blocks dismissed. Without it
        // the sweep would resolve them as `⏱ 已由其他客户端处理` — a lie, cola
        // itself approved them.
        if let Some((id, _)) = &active
            && !approved.is_empty()
        {
            crate::bridge::request::delivery::resolve_blocks(
                &core.permission,
                &core.cards_handle(),
                &core.requests_handle(),
                &Some(id.clone()),
                id,
                crate::bridge::request::delivery::Origin::Command,
                &approved,
                crate::bridge::request::delivery::Residue::Single(
                    crate::bridge::request::kind::AUTOACCEPT_RECEIPT,
                ),
            )
            .await;
        }
        let current_on = core
            .session_settings(&thread_key)
            .await
            .map(|s| s.auto_accept)
            .unwrap_or(false);
        Some(CardActionResult {
            card: Some(crate::feishu::card::picker::build_autoaccept_card(
                &thread_key,
                current_on,
            )),
            toast: Some(if on {
                "已开启自动审批".to_string()
            } else {
                "已关闭自动审批".to_string()
            }),
        })
    }

    /// The claim-and-dispatch preamble the Error card's retry (spec #391) and
    /// the Unreceived card's 重新发起 (#437) share: claim `from`'s recovery
    /// action (the atomic double-click gate), resolve the thread and directory
    /// the spawned pipeline needs, and refuse a Session that already has a
    /// prompt in flight. Every refusal releases the claim, so a later click can
    /// try again. `label` names the action in the warn lines ("retry" /
    /// "resume"); the callers spawn their own pipelines and ack with their own
    /// toasts.
    async fn claim_recovery_click(
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

    /// Handle the Turn card's 「🔍 查看完整变化」 click (spec #797, ticket #801).
    ///
    /// Feishu's card callback has a 3 s, no-retry budget (ADR-0078) and a diff
    /// fetch + HTML upload cannot fit it, so the click claims the Turn, spawns
    /// the delivery and returns a Toast immediately. The in-process guard is
    /// keyed by `(session_id, from)` — the card's exact Turn identity — and is
    /// bounded like the File Message once-guard: a later click is a no-op
    /// (「已发送」, or the ambiguous-send wording) and never generates or uploads
    /// again. Distinct Turns are distinct keys.
    async fn handle_turn_diff_action(
        self: &Arc<Self>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let session_id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let from = value
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let directory = value
            .get("directory")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let message_id = value
            .get("open_message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if session_id.is_empty() || message_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        let key = (session_id.clone(), from.clone());
        {
            let sends = self
                .turn_diff_sends
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(state) = sends.get(&key) {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(turn_diff_state_toast(*state).to_string()),
                });
            }
        }
        let Some(claim) = self.claim_turn_diff(&key) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("正在生成完整 diff…".to_string()),
            });
        };
        let click = TurnDiffClick {
            session_id,
            from,
            directory,
            message_id,
        };
        let app = Arc::clone(self);
        tokio::spawn(async move {
            let _claim = claim;
            app.run_turn_diff_delivery(click).await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some("正在生成完整 diff…".to_string()),
        })
    }

    /// Claim one Turn Diff delivery. `None` means a delivery for the same
    /// Turn is already in flight (the guard's Drop releases a failed one).
    fn claim_turn_diff(&self, key: &(String, String)) -> Option<TurnDiffClaim> {
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
    async fn run_turn_diff_delivery(self: &Arc<Self>, click: TurnDiffClick) {
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

    /// Handle the **File Viewer** card's 「发为文件」 click (ADR-0083 §5) — and,
    /// since #778, a Tool Panel's 「🌐 预览网页」 click (`.html`/`.htm`, `op:
    /// "preview"`). The card callback must ack within Feishu's 3 s, no-retry
    /// budget (ADR-0078), and a 30 MB upload cannot fit it, so the click records
    /// the intent, spawns the transfer and returns a toast immediately; the
    /// transfer then re-reads the file locally and posts exactly ONE File Message
    /// under the clicked card.
    ///
    /// The two ops differ in what happens to that clicked card. `Send` PATCHes it
    /// with the outcome — `已发送为文件消息`, `未发送` for a file past the 30 MB cap
    /// or definite failure, or an honest unknown result after an ambiguous
    /// timeout — because the clicked card IS the viewer. `Preview` **never**
    /// PATCHes: the clicked message is a Tool Panel on the agent's Turn card, and
    /// a transfer card built for the file would overwrite the panel and the
    /// reply. So a preview reports nothing on the card when the File Message
    /// lands (it is its own evidence) and speaks a failure as a reply instead.
    ///
    /// A viewer payload carries its Chat/Topic; a Tool Panel payload carries only
    /// the Session (`tool_file_value`), so this resolves the Chat/Topic and the
    /// path scope from the SessionStore the way the open lane does
    /// (`handle_file_card_action`) — the panel's structured path may be relative
    /// to the Session's directory. A Session the store no longer knows cannot
    /// route the reply and answers an honest Toast.
    async fn handle_viewer_file_action(
        self: &Arc<Self>,
        value: &serde_json::Value,
        op: ViewerFileOp,
    ) -> Option<CardActionResult> {
        let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let message_id = value
            .get("open_message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session_id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty());
        if path.is_empty() || message_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        }
        // Route from the payload's Chat/Topic, or — a Tool Panel — from the
        // Session, exactly as the open lane does; a panel whose Session is gone
        // has nothing to reply under and nothing to resolve its path against.
        let (thread_key, session_dir) = {
            let sessions = self.sessions.lock().await;
            let mut thread_key = thread_key_from_value(value);
            let mut session_dir = None;
            if let Some(sid) = session_id {
                if thread_key.chat_id.is_empty()
                    && let Some(resolved) = sessions.thread_for_session(sid)
                {
                    thread_key = resolved;
                }
                session_dir = sessions.directory_for_session(sid);
            }
            (thread_key, session_dir)
        };
        if thread_key.chat_id.is_empty() {
            return Some(CardActionResult {
                card: None,
                toast: Some(format!("⚠️ 无法发送 `{path}`：会话已不在。")),
            });
        }
        // The structured path resolves on the Path Scope (ADR-0083 §4), the same
        // rule the open lane applies; a viewer's already-resolved absolute path
        // passes through unchanged.
        let resolved = match (session_id, session_dir.as_deref()) {
            (Some(_), Some(dir)) => crate::bridge::command::lexical_normalize(
                &crate::bridge::command::expand_against(path, std::path::Path::new(dir)),
            ),
            _ => std::path::PathBuf::from(path),
        };
        let resolved = resolved.to_string_lossy().to_string();
        let content_identity = value
            .get("viewer_file_identity")
            .and_then(serde_json::Value::as_str)
            .and_then(|identity| identity.parse().ok())
            .unwrap_or(0);
        let transfer_key = (
            thread_key.chat_id.clone(),
            message_id.to_string(),
            resolved.clone(),
            content_identity,
        );
        let prior_transfer = self
            .viewer_file_transfers_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&transfer_key)
            .copied();
        if let Some(status) = prior_transfer {
            return Some(CardActionResult {
                card: None,
                toast: Some(match status {
                    ViewerFileTransferStatus::Sent => "文件已发送".to_string(),
                    ViewerFileTransferStatus::OutcomeUnknown => "发送结果未知，为避免重复未重试".to_string(),
                }),
            });
        }
        let Some(in_flight_guard) = self.claim_viewer_action(&thread_key, message_id) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("正在发送文件…".to_string()),
            });
        };
        let message_id = message_id.to_string();
        let value = value.clone();
        let app = Arc::clone(self);
        tokio::spawn(async move {
            let _in_flight_guard = in_flight_guard;
            app.run_viewer_file_transfer(thread_key, resolved, message_id, content_identity, value, op)
                .await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some("正在发送文件…".to_string()),
        })
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
    async fn run_viewer_file_transfer(
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

    /// The Terminal Card's 「📄 完整输出」 click (ADR-0086 decision 7) — the
    /// Terminal Command's counterpart of the viewer's 「发为文件」. The card
    /// callback must ack inside Feishu's budget, and a 30 MB upload cannot fit
    /// it, so the click claims the card, spawns the transfer and answers a
    /// toast immediately. The spawned step reads a bounded copy of the
    /// **Terminal Log**, sends it as exactly ONE File Message under the clicked
    /// card, and PATCHes that card with the outcome — so a large upload neither
    /// dies with the callback nor leaves the clicker without a result.
    ///
    /// It reuses the viewer's path on purpose: the same [`send_file_message`]
    /// lane (content-hash upload cache, one bounded send), the same
    /// [`SharedCore::viewer_transfer_timeout_ms`] per-call bound and the same
    /// per-card in-flight claim. A second upload implementation is exactly what
    /// this must not grow.
    async fn handle_terminal_full_output(
        self: &Arc<Self>,
        value: &serde_json::Value,
    ) -> Option<CardActionResult> {
        let Some(request) = TerminalCardRequest::from_value(value) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        };
        let thread_key = thread_key_from_value(value);
        let Some(in_flight_guard) = self.claim_viewer_action(&thread_key, &request.message_id) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("正在发送完整输出…".to_string()),
            });
        };
        let app = Arc::clone(self);
        tokio::spawn(async move {
            let _in_flight_guard = in_flight_guard;
            app.run_terminal_full_output(request).await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some("正在发送完整输出…".to_string()),
        })
    }

    /// The 「📄 完整输出」 transfer itself, off the card callback. One bounded
    /// local read (the delivery copy and the display tail), one File Message
    /// through the viewer's lane, then one PATCH onto the clicked card carrying
    /// the outcome (ADR-0086 decision 7). Past the platform's 30 MB cap the copy
    /// is the log's first 30 MB plus a truncation line — the log on disk is
    /// never truncated, because the child's descriptor offset does not move
    /// back.
    async fn run_terminal_full_output(self: &Arc<Self>, request: TerminalCardRequest) {
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

    /// The Terminal Card's 「问 AI」 click (ADR-0086 decision 9) — the ONE
    /// explicit door by which a Terminal Command's output reaches the model.
    /// No Terminal Command ever submits its own output, so this click is the
    /// only path into the Session Transcript.
    ///
    /// The payload is self-describing: the command, the output window and the
    /// run id it was rendered for. The submission therefore needs neither a
    /// live process nor the **Terminal Log** on disk, so it works on a run the
    /// startup sweep marked lost (ADR-0086 decision 5).
    ///
    /// The click claims the run's one submission before anything is spawned, so
    /// a redelivered or double-clicked button cannot submit the same output
    /// twice, and a click naming a run a newer command replaced is refused as
    /// stale (ADR-0086 decision 4) — refused by state, never hidden.
    ///
    /// The callback must ack inside Feishu's budget, and the normal prompt
    /// pipeline may ensure a server and wait on a whole Turn, so the click
    /// acknowledges and spawns it. The card is PATCHed with what happened to
    /// the *submission* — a new Turn or a Supplement — never with a claim that
    /// the model answered; the answer belongs to the Turn's own card.
    async fn handle_terminal_ask_ai(self: &Arc<Self>, value: &serde_json::Value) -> Option<CardActionResult> {
        let Some(request) = TerminalAskAiRequest::from_value(value) else {
            return Some(CardActionResult {
                card: None,
                toast: Some("无效操作".to_string()),
            });
        };
        let thread_key = thread_key_from_value(value);
        let (guard, toast) = match crate::bridge::terminal::AskAiGuard::claim(
            &self.terminal_runs,
            &self.terminal_records,
            &thread_key,
            &request.card.run_id,
        ) {
            crate::bridge::terminal::AskAiClaim::Accepted(guard) => (guard, "🤖 正在把终端输出提交给 AI…"),
            crate::bridge::terminal::AskAiClaim::InFlight => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("正在提交给 AI…".to_string()),
                });
            }
            crate::bridge::terminal::AskAiClaim::AlreadySubmitted => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("这份输出已经提交给 AI 了。".to_string()),
                });
            }
            crate::bridge::terminal::AskAiClaim::Stale => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some("这张终端卡片的操作已过期。".to_string()),
                });
            }
            // The durable authority is unreadable and this process never ran
            // the command: nothing can prove the card current, so refuse rather
            // than hand old output to the model (#827 review, finding 1). The
            // toast says which failure it is, so the reader knows to re-run.
            crate::bridge::terminal::AskAiClaim::Unverifiable => {
                return Some(CardActionResult {
                    card: None,
                    toast: Some(
                        "⚠️ 找不到这次运行的记录，无法确认它是否仍是最新的，因此没有提交（避免把旧输出交给 AI）。"
                            .to_string(),
                    ),
                });
            }
        };
        let app = Arc::clone(self);
        tokio::spawn(async move {
            app.run_terminal_ask_ai(request, guard).await;
        });
        Some(CardActionResult {
            card: None,
            toast: Some(toast.to_string()),
        })
    }

    /// The 「问 AI」 submission itself, off the card callback: build the framed
    /// prompt from the button's payload, hand it to the one prompt pipeline,
    /// and PATCH the clicked card with the submission's outcome. `guard` holds
    /// the run's one submission slot; it is released as retryable unless the
    /// pipeline accepted the message.
    async fn run_terminal_ask_ai(
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

    /// Re-submit a failed prompt from the error-card "retry" button (spec
    /// #391). The card callback must ack within 3s, so the click claims the
    /// retry (the atomic double-click guard behind the Turn interface), spawns
    /// the decision pipeline and returns a "retrying" toast immediately; the
    /// pipeline then reads the session's status and transcript once, decides,
    /// and either re-attaches the card to a run that is still alive, or marks
    /// the failed card `Retried` and submits the prompt onto a NEW card below
    /// it.
    ///
    /// The claim is released when nothing is submitted and nothing is
    /// re-attached (a session/thread that vanished, or a busy run with no
    /// anchor to follow), so a later click can try again.
    async fn handle_retry_action(self: &Arc<Self>, value: &serde_json::Value) -> Option<CardActionResult> {
        let RecoveryClick {
            recovery,
            thread_key,
            directory,
            handles,
        } = self
            .claim_recovery_click(value, crate::feishu::card::CardState::Error, "retry")
            .await?;
        let app = Arc::clone(self);
        let span = span::turn(&recovery.session_id, &thread_key, None);
        tokio::spawn(
            async move {
                app.run_retry(handles, recovery, thread_key, directory).await;
            }
            .instrument(span),
        );
        Some(CardActionResult {
            card: None,
            toast: Some("正在重试...".to_string()),
        })
    }

    /// The waiting card's 「清理待确认任务」 click (spec #588, ticket #590): the
    /// user's exit for the ⚠️ 状态待确认 residue no machine can confirm. The
    /// card callback must ack within 3s, so the click claims the cleanup (the
    /// same atomic double-click guard the retry uses, on the waiting card),
    /// spawns the pipeline and returns a "cleaning" toast immediately; the
    /// pipeline then reads the session's transcript and runtime once, retires
    /// exactly the unconfirmed live tasks it finds, and re-renders the waiting
    /// card.
    async fn handle_cleanup_action(self: &Arc<Self>, value: &serde_json::Value) -> Option<CardActionResult> {
        let sid = value.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        if sid.is_empty() {
            return None;
        }
        let handles = self.turn_handles();
        if !crate::bridge::turn::Turn::claim_cleanup(&handles.cards, sid).await {
            tracing::warn!("cleanup: no waiting card with unconfirmed tasks for session {sid}");
            return None;
        }
        let (thread_key, directory) = {
            let sessions = self.sessions.lock().await;
            (
                sessions.thread_for_session(sid),
                sessions.directory_for_session(sid),
            )
        };
        let Some(thread_key) = thread_key else {
            crate::bridge::turn::Turn::release_cleanup_claim(&handles.cards, sid).await;
            tracing::warn!("cleanup: no thread mapped for session {sid}");
            return None;
        };
        let session_id = sid.to_string();
        let span = span::turn(&session_id, &thread_key, None);
        tokio::spawn(
            async move {
                Self::run_cleanup(handles, session_id, directory).await;
            }
            .instrument(span),
        );
        Some(CardActionResult {
            card: None,
            toast: Some("正在清理...".to_string()),
        })
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
    async fn run_cleanup(
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
        let cleared: Vec<String> = transcript
            .background_tasks
            .iter()
            .filter(|task| transcript.unconfirmed_tasks.contains(&task.tool.call_id))
            .map(|task| task.tool.call_id.clone())
            .collect();
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
        if !cleared.is_empty() {
            // The read's own cleanup rides the local transcript BEFORE the
            // refresh (review, PR #595): the dropped rows, the 🧹 entries and
            // the settle decision all come from this transcript, so it carries
            // them into the write.
            transcript.apply_cleanup(&cleared, now_ms);
        }
        // The click's own ending, recorded AT DERIVATION (ADR-0080): every
        // cleared run's Cleanup ending is durable the moment the click derives
        // it — OUTSIDE the refresh gate below, because the fact is about the
        // run, not about the card that will show its 🧹 entry. A refresh the
        // chain replaces still leaves the task ended and the entry owed to the
        // next card that can carry it; only the process-local overlay record
        // waits on the refresh. The process-local overlay record does NOT sit
        // here — it is gated on the refresh's outcome below.
        for retirement in transcript
            .task_retirements
            .iter()
            .filter(|retirement| matches!(retirement.ending, TaskRetirementEnding::Cleaned))
        {
            let input = transcript.tool_input(retirement.task.tool.call_id.as_str());
            handles
                .backend
                .tasks()
                .record_cleanup_ending(&session_id, &retirement.task, now_ms, input);
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
        // the cleaned retirements `apply_cleanup` appended above, and the
        // post-verdict marker set — so no part of the cache can outlive the
        // others.
        if update.accepted() {
            pass.commit(&handles.backend, &session_id, &transcript);
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
    async fn run_retry(
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

    /// Handle the Unreceived card's 重新发起 action (ADR-0062, #437): on V2 the
    /// click interrupts (a no-op when the Session is already idle) and resumes,
    /// promoting the queued steer at the new run's start; on V1, which has no
    /// resume endpoint, the action degrades to resubmitting the message as a
    /// new Turn. Either way the already-admitted message is reused, never
    /// retyped, and the transcript message is never duplicated.
    ///
    /// The card callback must ack within 3s, so the click claims the action
    /// (the same atomic double-click guard the Error card's retry uses), spawns
    /// the pipeline and returns a toast immediately. A claim that cannot
    /// proceed (no thread mapping, or a prompt already in flight) is released,
    /// so a later press can try again.
    async fn handle_resume_action(self: &Arc<Self>, value: &serde_json::Value) -> Option<CardActionResult> {
        let RecoveryClick {
            recovery,
            thread_key,
            directory,
            handles,
        } = self
            .claim_recovery_click(value, crate::feishu::card::CardState::Unreceived, "resume")
            .await?;
        let app = Arc::clone(self);
        let span = span::turn(&recovery.session_id, &thread_key, None);
        tokio::spawn(
            async move {
                app.run_resume(handles, recovery, thread_key, directory).await;
            }
            .instrument(span),
        );
        Some(CardActionResult {
            card: None,
            toast: Some("正在重新发起...".to_string()),
        })
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
    async fn run_resume(
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
