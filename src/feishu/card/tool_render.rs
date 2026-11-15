use std::borrow::Cow;

use crate::backend::{
    ContentBlock, DelegationEffect, FileAction, FileContent, FileEdit, NestedCall, Phase, RunActivity,
    RunWaits, SearchScope, TodoList, TodoStatus, TodoStep, ToolCall, ToolDetail, ToolKind, ToolStatus,
    V1_DIRECTORY_TYPE_TAG, V2_DIRECTORY_HEADER_PREFIX, read_output_is_directory,
};

use super::lang::{PLAIN_TEXT, language_for_path};
use super::sanitize::{CardMarkdown, serialized_len};
use super::shell::{collapsible_panel, fmt_elapsed, panel_time_suffix};
use super::vocab::{self, INPUT_LABEL, OUTPUT_LABEL, phase_token};
use super::{
    AwaitingAction, MAX_ELEMENT_TEXT_CHARS, clip_md, exceeds_md_budget, fenced_block_bytes,
    fenced_block_overhead, fenced_code, fenced_overhead, truncate_md,
};

/// Cap for a tool's OUTPUT shown inside its collapsible panel. Higher than the
/// old 800: code-wrapped output renders without line wrapping, so a file or
/// command output can be meaningfully long.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 3000;

/// The line a `read` or `write` panel appends under a body its budget CLIPPED
/// (ADR-0083 §8 amendment, #770/#788): the panel is a
/// [`TOOL_OUTPUT_MAX_CHARS`]-character preview, so a clipped one says so where
/// the reader can act on it — the way-in button inside the fold (「📂 打开」, or
/// 「🌐 预览网页」 for an HTML page, #778).
const PREVIEW_TRUNCATION_NOTE: &str = "_（预览已截断，打开查看完整文件）_";

/// The line a file-editing panel appends under a diff its budget CLIPPED. The
/// note is a fenced body's ONLY clip signal: the `…` `truncate_md` appends
/// stops Feishu rendering the block as code at all (#788), so the body clips
/// bare ([`clip_md`]) and this line says why. Unlike
/// [`PREVIEW_TRUNCATION_NOTE`] it names no way-in: the 「📂 打开」 button opens
/// the file's CURRENT content, not the diff that changed it.
pub(crate) const DIFF_TRUNCATION_NOTE: &str = "_（改动过长，已截断）_";

/// The line a Codex search's raw-JSON fallback appends under a body its budget
/// CLIPPED. The opaque `results` value can exceed the panel budget; the fenced
/// body clips bare ([`clip_fenced_body`]) so the enclosing fence survives, and
/// this line is the reader's only signal that the payload was cut.
const SEARCH_TRUNCATION_NOTE: &str = "_（搜索结果过长，已截断）_";

/// The budget for a file-editing panel's diff BODY, measured in the bytes the
/// body occupies once the card serializes it (its JSON-escaped form) — NOT in
/// characters, and not the [`TOOL_OUTPUT_MAX_CHARS`] preview cap.
///
/// A real diff routinely exceeds 3,000 characters, and a panel cannot be split
/// (the whole card is bounded by [`super::MAX_CARD_JSON_CHARS`]), so a diff gets
/// its own cap at roughly half the card budget, leaving room for the panel's
/// input, header, button and note. Bytes are what Feishu limits and what the
/// splitter estimates; a character cap would let one CJK char (3 bytes) or one
/// control char (JSON's six-byte `\uXXXX` escape) count as one and blow the
/// budget by up to sixfold.
///
/// The budget covers the escaped body, the fence [`fenced_code`] wraps it in AND
/// the opening fence's language token ([`clip_fenced_body`]): a body carrying a
/// long backtick run would otherwise clip at ~12 KB and demand ~24 KB more in
/// fence, and a body that just fits without a tag can be pushed over by the tag
/// alone. Shared with the permission card, which renders the same diff under
/// the same cap (#794).
pub(crate) const DIFF_MAX_ESCAPED_BYTES: usize = 12_000;

/// A Tool Panel: the typed tool call as the card renders it (ADR-0053).
///
/// The panel owns the decoded [`ToolCall`] — identity, typed status, raw
/// input/metadata, typed output — and derives everything presentation-side
/// (status icon, liveness, the assembled output text) from it. Nothing is
/// copied out into a second name/status/input/output field, so the read model
/// stays the single description of the call. `PartialEq` is the accumulator's
/// rendered-tool revision: an update re-renders exactly when the typed view
/// differs.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolPanel {
    call: ToolCall,
    /// The live child-session liveness a running `task` call carries
    /// (ADR-0054). Display-only: the Bridge refreshes it while the call runs,
    /// rendering only shows it while the call is live. `None` for every
    /// non-task panel and until the child state has been read.
    liveness: Option<TaskLiveness>,
    /// The File Contents this call's output carries, with the card delivery the
    /// render path resolved for each (ADR-0076). Empty for the common case; a
    /// `read` of an image is the shape that fills it.
    files: Vec<ToolFile>,
}

/// One File Content a tool call carries, plus the card delivery the render path
/// resolved for it (ADR-0076). The card build renders the tracking line from
/// `content` and, for a [`FileDelivery::Embedded`] entry, the `img` element
/// after the panel.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolFile {
    pub content: FileContent,
    /// The resolved delivery. Starts [`FileDelivery::Unresolved`] and is set by
    /// the render path's pre-resolve step, so a `None`-free enum keeps "not
    /// considered yet" distinct from "considered, nothing to render".
    pub delivery: FileDelivery,
}

/// A File Content's card delivery (ADR-0076). Exactly one per File Content; the
/// card build renders the line and image from it instead of re-deriving
/// anything. A content that cannot be embedded is delivered as a File Message
/// (#649): `SentAsFile` when the message went out, `FileSendOutcomeUnknown` when
/// a timed-out request may have landed, or `Undelivered` when it could not be —
/// over Feishu's 30MB cap, or a failed upload/send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDelivery {
    /// The render path has not resolved this content yet. A card built before
    /// the pre-resolve runs (a unit fixture) shows just the tracking line.
    Unresolved,
    /// An image within Feishu's caps, uploaded once and embedded in the card
    /// immediately after its Tool Panel: the reusable `image_key` the `img`
    /// element references. The panel line reads `· 已内嵌`.
    Embedded { image_key: String },
    /// A File Content uploaded once and posted as exactly one File Message
    /// under the live card (#649). The reply OMITS `reply_in_thread`, so Feishu
    /// keeps it in the card's own container: the Session's topic when the card
    /// is already threaded, a plain reply that creates no topic otherwise
    /// (ADR-0076's amendment). The panel line reads `· 已发送为文件消息`.
    SentAsFile,
    /// A File Message send timed out after the request may have reached Feishu.
    /// Do not retry this identity: the card says the result is unknown because
    /// the message cannot be edited or retracted.
    FileSendOutcomeUnknown,
    /// A preview the card structurally cannot hold, posted as exactly one
    /// Rich Text Message (`msg_type: "post"` carrying the `md` tag) under the
    /// viewer (ADR-0083 §5, #741). The reply rides the same generic reply path
    /// as [`Self::SentAsFile`], so it lands in the card's own container the
    /// same way. The tracking line reads `· 已发送为富文本消息`.
    SentAsRichText,
    /// A File Content that could not be delivered: over Feishu's 30MB message
    /// cap, or a failed upload/send. The panel line reads `· 未发送`.
    Undelivered,
}

impl FileDelivery {
    /// The reusable `image_key` when this content is embedded in the card.
    fn as_embedded_key(&self) -> Option<&str> {
        match self {
            FileDelivery::Embedded { image_key } => Some(image_key),
            FileDelivery::Unresolved
            | FileDelivery::SentAsFile
            | FileDelivery::FileSendOutcomeUnknown
            | FileDelivery::SentAsRichText
            | FileDelivery::Undelivered => None,
        }
    }
}

/// The status marker a backgrounded call's panel shows instead of ✅
/// (ADR-0060, the pinned marker): the call settled by returning the background
/// handle, so the 🌙 icon in the status slot marks the launch — never a claim
/// that the run completed. The Background Task Ledger owns the run's liveness,
/// before and after retirement.
pub(crate) const BACKGROUNDED_MARKER: &str = "🌙";

/// The card's tail TODO section. It is deliberately separate from
/// [`ToolPanel`]: the list comes from OpenCode's `todowrite` call or the Codex
/// adapter's plan notifications, and no synthetic tool call crosses the
/// adapter/card boundary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TodoPanel {
    List {
        todo: TodoList,
        live: bool,
    },
    /// A malformed `todowrite` payload falls back to the existing raw tool
    /// renderer. This is the adapter's real built-in call, never a synthetic
    /// list carrier.
    Tool(Box<ToolPanel>),
}

impl TodoPanel {
    pub(crate) fn new(todo: TodoList, live: bool) -> Self {
        Self::List { todo, live }
    }

    pub(crate) fn from_tool(call: ToolCall) -> Self {
        let panel = ToolPanel::new(call);
        match panel.todo_list() {
            Some(todo) => Self::new(todo, panel.is_live()),
            None => Self::Tool(Box::new(panel)),
        }
    }

    pub(crate) fn is_live(&self) -> bool {
        match self {
            Self::List { live, .. } => *live,
            Self::Tool(panel) => panel.is_live(),
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        match self {
            Self::List { live, .. } => *live,
            Self::Tool(panel) => panel.is_running(),
        }
    }
}

impl ToolPanel {
    pub fn new(call: ToolCall) -> Self {
        // The call's file blocks become the panel's File Content carriers, so
        // the card build reads them without re-parsing any tool output.
        let files = call
            .output
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::File(content) => Some(ToolFile {
                    content: content.clone(),
                    delivery: FileDelivery::Unresolved,
                }),
                _ => None,
            })
            .collect();
        Self {
            call,
            liveness: None,
            files,
        }
    }

    /// The neutral plan fact an OpenCode `todowrite` call carries: a
    /// [`TodoList`] parsed from its input or output. The plan surface decides
    /// *whether* to ask ([`is_plan_call`], the closed `plan` token); this
    /// method only reports the fact, so no renderer repeats a vendor name.
    pub(crate) fn todo_list(&self) -> Option<TodoList> {
        call_todo_list(self.call())
    }

    /// The File Contents this panel carries (ADR-0076), in decoder order.
    pub(crate) fn files(&self) -> &[ToolFile] {
        &self.files
    }

    /// How many File Contents this panel embeds (ADR-0076) — the extra `img`
    /// elements its render pushes after the panel, so a card-size estimate can
    /// charge them. It is exactly the number [`file_image_elements`] emits, so
    /// the estimate charges precisely what renders and can never under-charge a
    /// panel's images.
    pub(crate) fn embedded_file_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.delivery.as_embedded_key().is_some())
            .count()
    }

    /// Attach the resolved delivery for the file at `index` (the render path's
    /// pre-resolve step). A no-op for an out-of-range index — the panel's files
    /// and its call's blocks can never disagree, but a caller cannot panic here.
    pub(crate) fn set_file_delivery(&mut self, index: usize, delivery: FileDelivery) {
        if let Some(file) = self.files.get_mut(index) {
            file.delivery = delivery;
        }
    }

    /// The typed call this panel renders. The accumulator compares it against
    /// the polled call to decide whether the panel is still that call's
    /// rendered revision — before building (and cloning) a panel for an
    /// unchanged poll.
    pub(crate) fn call(&self) -> &ToolCall {
        &self.call
    }

    /// Whether this panel is the plan surface's carrier ([`is_plan_call`]),
    /// derived once so the panel's title, input and output cannot disagree.
    pub(crate) fn is_plan(&self) -> bool {
        is_plan_call(self.call())
    }

    /// The tool's built-in id (`bash`, `read`, …). It is diagnostic and the
    /// opaque fallback title; the `Execute` family selects its renderer by
    /// `(kind, detail)` instead (ADR-0105).
    pub fn name(&self) -> &str {
        &self.call.identity.name
    }

    /// The call's typed lifecycle status.
    pub fn status(&self) -> &ToolStatus {
        &self.call.status
    }

    pub fn status_icon(&self) -> &'static str {
        // A backgrounded call never claims completion (ADR-0060): it settled
        // at launch while its run continues, so the panel reads the 🌙 marker
        // in its status slot — durably, before and after the task retires. The
        // ledger owns the run's liveness.
        if self.backgrounded() {
            return BACKGROUNDED_MARKER;
        }
        if self.detail_failure() {
            return "❌";
        }
        match self.status() {
            ToolStatus::Running | ToolStatus::Pending => "⏳",
            ToolStatus::Completed => "✅",
            // OpenCode marks failed tools as status "error" (the typed arm
            // above); a status this build does not model keeps its own name,
            // and one named "failed" still reads as a failure.
            ToolStatus::Error => "❌",
            ToolStatus::Other(status) if status == "failed" => "❌",
            // A missing status renders as its historical default (completed);
            // any other unrecognized status gets the generic wrench.
            ToolStatus::Unknown => "✅",
            ToolStatus::Other(_) => "🔧",
        }
    }

    /// Whether the call is still live (`running`/`pending`): its panel is
    /// TAIL content that rides the live card, and joins the timeline only once
    /// the tool settles (ADR-0045). Classification is the typed status's own,
    /// never a string comparison.
    pub fn is_live(&self) -> bool {
        self.status().is_live()
    }

    /// Whether the call is running right now (not merely pending): the header's
    /// "⏳ tool" hint names the Turn's running call (ADR-0014).
    pub fn is_running(&self) -> bool {
        matches!(self.status(), ToolStatus::Running)
    }

    /// Whether this call moved its run to the background (ADR-0060): the
    /// backend's own durable marker on the call — the settled `shell`/
    /// `subagent` launch whose metadata still says the run is going
    /// ([`ToolCall::background_launch`]). Such a panel keeps its timeline place
    /// as the record of the request and reads [`BACKGROUNDED_MARKER`] in its
    /// status slot instead of ✅ — never claiming completion, whatever the
    /// ledger currently lists.
    pub(crate) fn backgrounded(&self) -> bool {
        self.call.background_launch().is_some()
    }

    /// The raw structured tool input (what OpenCode recorded for the call), kept
    /// as JSON so the panel can render it human-friendly per tool type instead
    /// of dumping a raw JSON blob.
    pub fn input(&self) -> Option<&serde_json::Value> {
        self.call.input.as_ref()
    }

    /// The structured path a 「📂 打开」 button would open for this panel
    /// (ADR-0083 §8, #743): a FileEdits call's one file — a rename's
    /// destination, a delete nothing at all, several no single target — read
    /// from the adapter's typed detail, so the button, the Input line and the
    /// folded title cannot disagree about which file the call touched. A FILE
    /// `read` names one too (ADR-0083 §8 amendment, #770): its panel is a
    /// preview of the file clipped to [`TOOL_OUTPUT_MAX_CHARS`], so the whole
    /// file is a tap away rather than on the card. Two read shapes are
    /// excluded, both of which already reach the reader whole: a read carrying
    /// a File Content (an image or a PDF has its own delivery, ADR-0076) and a
    /// DIRECTORY read ([`read_output_is_directory`]), which names no file at
    /// all. `None` for every other tool, and for a call whose path is absent
    /// or unusable — or whose output has not arrived yet, a still-running
    /// `read` having no directory verdict to decide on. Those panels carry no
    /// button rather than a broken one. A path that merely carries surrounding
    /// whitespace is still a name Unix allows, so it stays verbatim — only a
    /// path with nothing printable is unusable. Natural-language paths in the
    /// agent's prose never reach here — only the adapter's typed detail is
    /// read.
    pub(crate) fn file_open_path(&self) -> Option<String> {
        if let Some(edits) = file_edits(self) {
            let [only] = edits.as_ref() else {
                return None;
            };
            if only.action == FileAction::Delete {
                return None;
            }
            let path = file_edit_display_path(only);
            return (!path.trim().is_empty()).then(|| path.to_string());
        }
        // A Codex `imageView` reads a file, but its panel is not a preview of
        // one (P1's read, out of the FileEdits ticket's scope): no button.
        if is_codex_image(self) {
            return None;
        }
        let read = read_detail(self)?;
        let path = read.path.filter(|path| !path.trim().is_empty())?;
        // A panel of a file the viewer can show WHOLE: the body is a preview,
        // so the way in is what the panel lacks. The two exclusions already
        // reach the reader whole (a File Content's own delivery; a directory,
        // which is no file at all), and a call with no output yet has no
        // verdict — `?` leaves a still-running read buttonless until it
        // renders one.
        if read.directory || !self.files.is_empty() || read_output_is_directory(&self.output()?) {
            return None;
        }
        Some(path.to_string())
    }

    /// How many card elements the 「📂 打开」 button adds (ADR-0083 §8, #743):
    /// one button inside a qualifying panel, none otherwise — so the component
    /// estimate charges precisely what renders. Decided by the button builder
    /// itself, never re-derived, so the count cannot drift from the render.
    pub(crate) fn open_button_count(&self, session_id: Option<&str>) -> usize {
        usize::from(tool_open_button_element(self, session_id).is_some())
    }

    /// The call's raw metadata (what OpenCode recorded beside the input). The
    /// card reads run facts its own shapes do not carry — a background launch's
    /// shell/session id, a search's result count — from here; a failure verdict
    /// is the adapter's (`ToolDetail`), never a raw re-parse.
    fn metadata(&self) -> Option<&serde_json::Value> {
        self.call.metadata.as_ref()
    }

    /// Whether the call's own recorded facts report a failed run even though the
    /// protocol's status settled `completed`: a `Command` whose exit is non-zero
    /// or which timed out, and a `Program` that reported an error — its own, or
    /// a nested call's. The official 2.0.x client derives its error rendering
    /// from exactly these facts; the typed [`ToolStatus`] stays the protocol's
    /// own and is never rewritten.
    fn detail_failure(&self) -> bool {
        if self.status() != &ToolStatus::Completed {
            return false;
        }
        match ExecuteShape::of(self) {
            Some(ExecuteShape::Command { exit, timeout, .. }) => {
                timeout || exit.is_some_and(|exit| exit != 0)
            }
            Some(ExecuteShape::Program { calls, error, .. }) => {
                error || calls.iter().any(|call| call.status.is_failure())
            }
            None => false,
        }
    }

    /// The output text the panel renders: the decoder's text blocks joined (in
    /// decoder order) with a failure's message appended on its own line. A
    /// file-editing call's hunks come from its typed [`ToolDetail`]
    /// ([`file_edits_output`]), not from this text.
    ///
    /// This is presentation assembly, not protocol decoding: the decoder
    /// already applied the sources' precedence into the typed output's blocks,
    /// so a payload carrying more than one text source can never render twice.
    pub fn output(&self) -> Option<String> {
        tool_output(&self.call)
    }

    /// The child-session liveness gathered for a live `task` call (ADR-0054).
    pub fn liveness(&self) -> Option<&TaskLiveness> {
        self.liveness.as_ref()
    }

    /// Attach or replace the gathered liveness (the accumulator's refresh
    /// path). Rendering only shows it while the call is live, so a settled
    /// panel's stale snapshot is never visible.
    pub(crate) fn set_liveness(&mut self, liveness: Option<TaskLiveness>) {
        self.liveness = liveness;
    }

    /// Whether this panel is the launch of a child run (a `Delegation` with at
    /// least one run). Such a panel is live while its child runs, but it is
    /// not the Turn's own orphaned work: the run owns the wait, so the drain
    /// must not grace it into `StuckPanel`.
    pub(crate) fn runs_a_child(&self) -> bool {
        matches!(
            &self.call.detail,
            ToolDetail::Delegation {
                effect: DelegationEffect::Launch,
                runs,
            } if runs.iter().any(|run| !run.run_id.is_empty())
        )
    }

    /// The child Session a launch runs — the run's live identity as the
    /// adapter wrote it into the call's [`ToolDetail::Delegation`] (the launch
    /// relation replaces the old tool-name check, spec #1057's P4).
    ///
    /// `None` unless the call names EXACTLY one run: a launch with several
    /// receivers ranks no child above another, and picking `first()` would make
    /// an arbitrary receiver the panel's liveness source. A multi-child
    /// launch's liveness comes from `SessionTranscript::run_children()` (the
    /// ledger/gather path) instead, and no single child exists to name here.
    pub(crate) fn child_session_id(&self) -> Option<&str> {
        let ToolDetail::Delegation {
            runs,
            effect: DelegationEffect::Launch,
        } = &self.call.detail
        else {
            return None;
        };
        let [run] = runs.as_slice() else {
            return None;
        };
        (!run.run_id.is_empty()).then_some(run.run_id.as_str())
    }
}

/// Whether a call only addresses a run — it renders as a one-line receipt,
/// never a Tool Panel (ADR-0106 §4, spec #1057's P4). Only an `Address` is a
/// receipt: a `Report` updates its run and leaves no timeline row, and a launch
/// keeps its panel as the record of the request.
pub(crate) fn is_receipt_call(call: &ToolCall) -> bool {
    matches!(
        &call.detail,
        ToolDetail::Delegation {
            effect: DelegationEffect::Address,
            ..
        }
    )
}

/// Whether a call is a report on an existing run. It updates the run
/// ([`crate::backend::AgentRun`]); it is neither a Tool Panel nor a receipt on
/// the card.
pub(crate) fn is_report_call(call: &ToolCall) -> bool {
    matches!(
        &call.detail,
        ToolDetail::Delegation {
            effect: DelegationEffect::Report,
            ..
        }
    )
}

/// The one-line receipt an addressing call leaves on its run. The target names
/// the runs it addressed — or says it addressed the Turn's runs as a set when
/// it named none. The turn-level words are [`vocab::RECEIPT_TURN_RUNS`].
///
/// An addressing verb has no neutral field yet: `ToolDetail` declares only
/// `{runs, effect}`, so the adapter's own `input.tool` (`wait`, `sendInput`,
/// `closeAgent`) is the only source, read as display data exactly as a tool
/// panel reads a vendor title. Inventing a verb would be worse; a neutral
/// addressing verb belongs to a read-model ticket, not this vocabulary pass.
pub(crate) fn receipt_line(call: &ToolCall) -> String {
    let effect = match &call.detail {
        ToolDetail::Delegation { effect, .. } => Some(*effect),
        _ => None,
    };
    let verb = match effect {
        Some(DelegationEffect::Address) => call
            .input
            .as_ref()
            .and_then(|input| input.get("tool"))
            .and_then(serde_json::Value::as_str)
            .filter(|verb| !verb.is_empty())
            .or_else(|| call.title.as_deref().filter(|title| !title.is_empty()))
            .unwrap_or(call.identity.name.as_str())
            .to_string(),
        _ => call.identity.name.clone(),
    };
    let targets: Vec<&str> = match &call.detail {
        ToolDetail::Delegation { runs, .. } => runs.iter().map(|run| run.run_id.as_str()).collect(),
        _ => Vec::new(),
    };
    if targets.is_empty() {
        format!("↪ {verb} · {}", vocab::RECEIPT_TURN_RUNS)
    } else {
        format!("↪ {verb} · {}", targets.join("、"))
    }
}

/// A receipt's dedupe identity: the call. An addressing call's verb does not
/// move with its status, so it stays one line per call. Reports are not
/// receipts (they update their run and render no timeline row), so no state
/// belongs in this key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReceiptKey {
    call_id: String,
}

pub(crate) fn receipt_key(call: &ToolCall) -> ReceiptKey {
    ReceiptKey {
        call_id: call.identity.call_id.clone(),
    }
}

/// Whether this call is the plan surface's carrier: the closed `plan` arm of
/// the **name-only** table ([`vocab::name_only_token`]) — `Other`, `None` and
/// the vendor id `todowrite`. It is deliberately not a rendered-token
/// comparison: the opaque fallback returns the identity's own name, so an
/// MCP/plugin call literally named `plan` must not disappear into the plan
/// surface. The plan's *content* is still the neutral `TodoList`
/// [`ToolPanel::todo_list`] parses.
pub(crate) fn is_plan_call(call: &ToolCall) -> bool {
    vocab::name_only_token(&call.identity, call.kind, &call.detail) == Some(vocab::TOOL_PLAN)
}

/// The neutral plan fact a call carries, parsed from its input first and its
/// output second (the same precedence the plan panel renders). No vendor id is
/// read: the shape is the fact.
fn call_todo_list(call: &ToolCall) -> Option<TodoList> {
    call.input.as_ref().and_then(parse_todo_list).or_else(|| {
        tool_output(call)?
            .parse::<serde_json::Value>()
            .ok()
            .as_ref()
            .and_then(parse_todo_list)
    })
}

/// The **Execute** family's two shapes (ADR-0105, spec #1057's P2): a shell-like
/// command and a Code Mode program. Selection is `(kind, detail)` — never the
/// vendor name — so `bash`, `shell` and `codex/command_execution` reach one
/// renderer and a third adapter inherits it by declaring the same shape.
///
/// One selection point, so the input line, the output body, the failure verdict
/// and the folded title's details cannot disagree about what the call is.
enum ExecuteShape<'a> {
    Command {
        command: &'a str,
        cwd: Option<&'a str>,
        description: Option<&'a str>,
        exit: Option<i64>,
        timeout: bool,
    },
    Program {
        source: &'a str,
        language: Option<&'a str>,
        calls: &'a [NestedCall],
        error: bool,
    },
}

impl<'a> ExecuteShape<'a> {
    /// The `Execute` family's shape `tool` carries, when its adapter declared one. A
    /// detail on a kind that is not `Execute` is not an Execute call (a lying
    /// detail gets the opaque path, ADR-0105's consequence).
    fn of(tool: &'a ToolPanel) -> Option<Self> {
        match (tool.call().kind, &tool.call().detail) {
            (
                ToolKind::Execute,
                ToolDetail::Command {
                    command,
                    cwd,
                    description,
                    exit,
                    timeout,
                },
            ) => Some(Self::Command {
                command,
                cwd: cwd.as_deref(),
                description: description.as_deref(),
                exit: *exit,
                timeout: *timeout,
            }),
            (
                ToolKind::Execute,
                ToolDetail::Program {
                    source,
                    language,
                    calls,
                    error,
                },
            ) => Some(Self::Program {
                source,
                language: language.as_deref(),
                calls,
                error: *error,
            }),
            _ => None,
        }
    }
}

/// Whether the call is the `Execute` family's `Command` shape.
fn is_command(tool: &ToolPanel) -> bool {
    matches!(ExecuteShape::of(tool), Some(ExecuteShape::Command { .. }))
}

/// Whether the call is the `Execute` family's `Program` shape: the panel that
/// spends the output budget on its input and puts its call count in the folded
/// title.
fn is_program(tool: &ToolPanel) -> bool {
    matches!(ExecuteShape::of(tool), Some(ExecuteShape::Program { .. }))
}

/// Whether the call declares a non-empty `FileEdits` detail on a kind that can
/// carry one, without cloning the merged list.
fn has_file_edits(tool: &ToolPanel) -> bool {
    matches!(
        (tool.call().kind, &tool.call().detail),
        (ToolKind::Edit | ToolKind::Delete | ToolKind::Move, ToolDetail::FileEdits(edits))
            if !edits.is_empty()
    )
}

/// The **FileEdits** family (ADR-0105, spec #1057's P3): a call that touched
/// files, each with its action and diff. Selection is `(kind, detail)` — never
/// the vendor name — so `edit`, `write`, `patch`, `apply_patch` and
/// `codex/file_change` reach one renderer in one SHAPE, and a third adapter
/// inherits it by declaring the same detail.
///
/// The kind must agree with the detail: a `FileEdits` on an unrelated kind is
/// not this family (a lying detail gets the opaque path, ADR-0105's
/// consequence).
fn file_edits(tool: &ToolPanel) -> Option<Cow<'_, [FileEdit]>> {
    match (tool.call().kind, &tool.call().detail) {
        (ToolKind::Edit | ToolKind::Delete | ToolKind::Move, ToolDetail::FileEdits(edits))
            if !edits.is_empty() =>
        {
            // A Move kind is admitted because ACP declares it; `FileAction`
            // has no Move, so the detail's own action (Rename for a move,
            // Update for an in-place edit) is the word shown.
            //
            // The adapters already merge duplicate paths; a third adapter may
            // not, so the card re-merges only when it actually sees a repeat
            // and otherwise borrows the detail (the common path costs no
            // clone).
            if has_duplicate_paths(edits) {
                Some(Cow::Owned(crate::backend::merge_file_edits(
                    edits.iter().cloned(),
                )))
            } else {
                Some(Cow::Borrowed(edits.as_slice()))
            }
        }
        _ => None,
    }
}

fn has_duplicate_paths(edits: &[FileEdit]) -> bool {
    edits
        .iter()
        .enumerate()
        .any(|(index, edit)| edits[..index].iter().any(|earlier| earlier.path == edit.path))
}

/// The `#947` action word for one file: add/update/delete/rename, never
/// inferred from a hunk (a pure rename or a binary change has none).
fn file_action_word(action: FileAction) -> &'static str {
    match action {
        FileAction::Add => "➕ 新增",
        FileAction::Update => "✏️ 编辑",
        FileAction::Delete => "🗑 删除",
        FileAction::Rename => "📦 重命名",
    }
}

/// The file a [`FileEdit`] names to the reader: a rename's destination (the
/// file that now exists), otherwise its own path.
fn file_edit_display_path(edit: &FileEdit) -> &str {
    match edit.action {
        FileAction::Rename => edit.rename_to.as_deref().unwrap_or(&edit.path),
        _ => &edit.path,
    }
}

/// The **Read** shape (ADR-0105 decision 5): a file or directory a call read.
/// Keyed on `(kind, detail)`, so `read` and a future client's read reach the
/// same renderer.
struct ReadShape<'a> {
    path: Option<&'a str>,
    directory: bool,
}

fn read_detail(tool: &ToolPanel) -> Option<ReadShape<'_>> {
    match (tool.call().kind, &tool.call().detail) {
        (ToolKind::Read, ToolDetail::Read { path, directory, .. }) => Some(ReadShape {
            path: path.as_deref(),
            directory: *directory,
        }),
        _ => None,
    }
}

/// The **Search** shape (ADR-0105 decision 5): a filename, content or web
/// search. The scope picks the word and the body parser; the vendor name is
/// never read.
struct SearchShape<'a> {
    query: Option<&'a String>,
    matches: Option<u64>,
    scope: SearchScope,
}

fn search_detail(tool: &ToolPanel) -> Option<SearchShape<'_>> {
    match (tool.call().kind, &tool.call().detail) {
        (
            ToolKind::Search,
            ToolDetail::Search {
                query,
                matches,
                scope,
            },
        ) => Some(SearchShape {
            query: query.as_ref(),
            matches: *matches,
            scope: *scope,
        }),
        _ => None,
    }
}

/// Whether the panel is Codex's `imageView` — a `Read` in the neutral model
/// (P1's table) whose panel keeps its own `🖼` word and no open button. The
/// name and input shape are only half the gate: the neutral detail must be
/// `Read` too, so an Opaque `imageGeneration` (which shares the identity and a
/// `path` input) never takes the image-view presentation.
fn is_codex_image(tool: &ToolPanel) -> bool {
    tool.name() == "codex/image"
        && matches!(tool.call().detail, ToolDetail::Read { .. })
        && image_shape(tool.input())
}

/// One file's action, with each path formatted by the caller: inline code on
/// the Input line, the folded short name on the title. A rename names both
/// ends.
fn file_edit_action_with(edit: &FileEdit, path: impl Fn(&str) -> String) -> String {
    let word = file_action_word(edit.action);
    match edit.action {
        FileAction::Rename => match edit.rename_to.as_deref().filter(|to| !to.is_empty()) {
            Some(to) => format!("{} {} → {}", word, path(&edit.path), path(to)),
            None => format!("{} {}", word, path(&edit.path)),
        },
        _ => format!("{} {}", word, path(&edit.path)),
    }
}

/// One file's action on the Input line (#947): the word, then the path (or a
/// rename's source → destination). The Input line is where the file is already
/// named, so this is where the action belongs for a list.
fn file_edit_action_line(edit: &FileEdit) -> String {
    file_edit_action_with(edit, inline_code)
}

/// The folded title's fragment for one file: the action word and the short
/// name (a rename names both ends). The title is one `plain_text` line, so a
/// deep path keeps its tail; several files keep the count/totals fragment
/// ([`multi_file_detail`]) instead.
fn file_edit_title(edit: &FileEdit) -> String {
    file_edit_action_with(edit, header_file_name)
}

/// The Input line a FileEdits panel renders: one action line per touched file
/// (#947), so a multi-file patch names every file's action rather than only
/// its first path.
fn file_edits_input(edits: &[FileEdit]) -> String {
    edits
        .iter()
        .map(file_edit_action_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The additions and deletions every file's hunks record: `+`/`-` lines, as a
/// unified diff writes them. One count for the totals the panel heads and the
/// folded title carries, so they cannot disagree.
fn file_edits_totals(edits: &[FileEdit]) -> (usize, usize) {
    let mut additions = 0;
    let mut deletions = 0;
    for diff in edits.iter().filter_map(|edit| edit.diff.as_deref()) {
        for line in diff.lines() {
            if line.starts_with('+') {
                additions += 1;
            } else if line.starts_with('-') {
                deletions += 1;
            }
        }
    }
    (additions, deletions)
}

/// One file's section in a FileEdits panel: its action line (markdown, outside
/// any fence) and its own hunks, so a multi-file body keeps every action next
/// to the change it names.
struct FileEditSection {
    action: Option<String>,
    hunks: String,
}

/// The pieces a FileEdits panel composes: per-file sections (action + hunks)
/// and the two trailing notes the ticket keeps apart — a `write`'s LSP note
/// rides after the fence (pre-P3 #788), an edit/patch note stays inside it.
struct FileEditsBody {
    header: Option<String>,
    sections: Vec<FileEditSection>,
    lang: &'static str,
    outside_tail: Option<String>,
    inside_tail: Option<String>,
}

/// The FileEdits panel's pieces, unclipped. `None` for a FAILED call (its error
/// text is the body) and for a list with no hunk and no action line to show.
fn file_edits_body(tool: &ToolPanel) -> Option<FileEditsBody> {
    if call_failed(tool) {
        return None;
    }
    let edits = file_edits(tool)?;
    let multi = edits.len() > 1;
    let mut sections: Vec<FileEditSection> = Vec::new();
    for edit in edits.iter() {
        let hunks = edit
            .diff
            .as_deref()
            .filter(|diff| !diff.trim().is_empty())
            .unwrap_or_default()
            .to_string();
        let action = (multi || hunks.is_empty()).then(|| file_edit_action_line(edit));
        if action.is_some() || !hunks.is_empty() {
            sections.push(FileEditSection { action, hunks });
        }
    }
    if sections.is_empty() {
        return None;
    }
    let lang = diff_language_for(edits.iter().map(file_edit_display_path));
    let header = match edits.as_ref() {
        [only] => only.diff.as_deref().map(|_| {
            let (additions, deletions) = file_edits_totals(std::slice::from_ref(only));
            format!("+{additions} −{deletions}")
        }),
        _ => None,
    };
    let tail = file_edits_tail(tool);
    // A `write`'s note (its own success summary precedes it) belonged outside
    // the fence before P3 (#788), under its own 500-char bound. Any other
    // producer's note stays inside, under the diff budget; an Add alone is not
    // enough — an `apply_patch` Add keeps its note inside.
    let (outside_tail, inside_tail) =
        if edits.len() == 1 && edits[0].action == FileAction::Add && is_write_output(tool) {
            (tail.map(|tail| truncate_md(&tail, WRITE_TAIL_MAX_CHARS)), None)
        } else {
            (None, tail)
        };
    Some(FileEditsBody {
        header,
        sections,
        lang,
        outside_tail,
        inside_tail,
    })
}

impl FileEditsBody {
    /// The markdown body the panel writes, with every file's action line
    /// immediately before its own fence. Each hunk block and the action list
    /// share [`DIFF_MAX_ESCAPED_BYTES`]: the action list stops growing when
    /// the budget is spent, a hunk block clips to the remainder, and the
    /// returned flag tells the caller its truncation note is owed.
    fn render(&self) -> (String, bool) {
        let mut body = String::new();
        let mut clipped = false;
        let mut inside_tail = self.inside_tail.as_deref();
        let last_section = self.sections.len().checked_sub(1);
        for (index, section) in self.sections.iter().enumerate() {
            let action = section.action.as_deref();
            if let Some(action) = action {
                let separator = if body.is_empty() { 0 } else { 2 };
                if json_escaped_len(&body) + separator + json_escaped_len(action) > DIFF_MAX_ESCAPED_BYTES {
                    clipped = true;
                    break;
                }
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str(action);
            }
            let mut hunks = section.hunks.clone();
            if Some(index) == last_section
                && let Some(tail) = inside_tail.take()
            {
                if !hunks.is_empty() {
                    hunks.push('\n');
                }
                hunks.push_str(tail);
            }
            if hunks.is_empty() {
                continue;
            }
            let separator = if body.is_empty() { 0 } else { 2 };
            let remaining = DIFF_MAX_ESCAPED_BYTES.saturating_sub(json_escaped_len(&body) + separator);
            let clipped_hunks = clip_fenced_body(&hunks, Some(self.lang), remaining);
            if clipped_hunks.is_empty() {
                clipped = true;
                break;
            }
            if clipped_hunks != hunks {
                clipped = true;
            }
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&fenced_code(&clipped_hunks, Some(self.lang)));
        }
        if let Some(tail) = &self.outside_tail {
            let separator = if body.is_empty() { 0 } else { 2 };
            let remaining = DIFF_MAX_ESCAPED_BYTES.saturating_sub(json_escaped_len(&body) + separator);
            if remaining > 0 {
                let original = tail.chars().count();
                let tail = truncate_md(tail, remaining);
                if tail.chars().count() < original {
                    clipped = true;
                }
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str(&tail);
            } else {
                clipped = true;
            }
        }
        (body, clipped)
    }

    fn clipped(&self) -> bool {
        self.render().1
    }
}

/// Whether the call's output carries a `write` tool's success summary (V1's
/// `Wrote file successfully.`, V2's `Wrote/Created file successfully: …`).
/// This is the producer's OUTPUT shape, not its id: an `apply_patch` Add must
/// keep its LSP note inside the diff fence.
fn is_write_output(tool: &ToolPanel) -> bool {
    let Some(text) = tool.output() else {
        return false;
    };
    let first = text.lines().next().unwrap_or("").trim_start();
    [
        "Wrote file successfully",
        "Created file successfully",
        "Successfully wrote",
    ]
    .iter()
    .any(|prefix| first.starts_with(prefix))
}

/// The FileEdits panel's Output: markdown action lines outside the fence, the
/// hunks inside it, already clipped under the diff byte budget. `BodyStyle`
/// is `Markdown` because the body is composed: the generic renderer must not
/// wrap the action lines in a second fence.
fn file_edits_output(tool: &ToolPanel) -> Option<(Option<String>, BodyStyle, String)> {
    let body = file_edits_body(tool)?;
    let (text, _) = body.render();
    if text.trim().is_empty() {
        return None;
    }
    Some((body.header, BodyStyle::Markdown, text))
}

/// The tool's own trailing note after its success summary (V1's LSP
/// diagnostics). `None` for a successful call with no note.
fn file_edits_tail(tool: &ToolPanel) -> Option<String> {
    if call_failed(tool) {
        return None;
    }
    tool.output()
        .as_deref()
        .and_then(summary_tail)
        .map(|tail| tail.trim_end().to_string())
        .filter(|tail| !tail.is_empty())
}

/// The cap on a `write` panel's LSP note, restored to pre-P3's placement
/// outside the fence (#788): the note is a preview, so it keeps its own bound
/// rather than riding the diff budget.
const WRITE_TAIL_MAX_CHARS: usize = 500;

/// A live `task`/`subagent` call's child-session liveness (ADR-0054): what the
/// child is doing right now, as the panel title shows it. Display-only data —
/// the Bridge gathers it, the Platform formats it — so the panel stays a view
/// over the call plus one read-only line. It is the card's view of the run's
/// neutral [`RunActivity`] (`from_run`), so the panel, the ledger and the
/// completion entry cannot disagree about one run.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLiveness {
    /// The child's current activity: its newest live tool, or the phase its
    /// newest part implies.
    pub activity: ChildActivity,
    /// Epoch ms of the child's newest observed activity: the timer a phase
    /// (no tool running) counts from. Stored as a time, not an age, so a
    /// failed read never freezes a made-up "5s". `None` when the run's
    /// activity carries no clock at all: the fragment then shows the phase
    /// word with no age rather than an age measured from the epoch.
    pub last_activity_ms: Option<i64>,
    /// The child's pending wait, in the header's own vocabulary (ADR-0054).
    pub wait: Option<AwaitingAction>,
}

/// What a child session is doing right now, as its transcript reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildActivity {
    /// The child's newest live tool. `started_at` is the server clock the
    /// duration counts from; a call with no clock yet shows its name alone.
    Tool { name: String, started_at: Option<i64> },
    /// Between steps: the phase its newest part implies
    /// (`waiting`/`reasoning`/`responding`).
    Phase(Phase),
}

impl TaskLiveness {
    /// The card's view of one run's neutral activity (ADR-0105, spec #1057's
    /// P4). The token comes from the call's `(kind, detail)` via
    /// [`vocab::tool_token`]; a phase with no live call reads
    /// [`phase_token`]. A wait set renders both waits through the header's own
    /// vocabulary.
    pub(crate) fn from_run(activity: &RunActivity) -> Self {
        let activity_kind = match &activity.call {
            Some(call) => ChildActivity::Tool {
                name: vocab::tool_token(&call.identity, call.kind, &call.detail).to_string(),
                // The call's own server start is the clock: `shell 28s` counts
                // from the call, not from the last activity.
                started_at: call.started_at,
            },
            None => ChildActivity::Phase(activity.phase.unwrap_or(Phase::Waiting)),
        };
        let wait = activity.awaiting.and_then(awaiting_action);
        Self {
            activity: activity_kind,
            last_activity_ms: activity.clock_ms(),
            wait,
        }
    }

    /// The fragment's parts, spelled once (ADR-0054, spec #501): the
    /// activity's display label (a tool's own name, or the word for a phase
    /// with no live tool), the clock its age counts from (`None` when the
    /// fragment renders no age), and the wait's words. [`Self::title_fragment`]
    /// assembles these for the card; the ledger's size reserve measures these,
    /// so the two cannot drift apart over how an activity is displayed.
    pub(crate) fn title_parts(&self) -> TitleParts<'_> {
        TitleParts {
            label: match &self.activity {
                ChildActivity::Tool { name, .. } => name,
                ChildActivity::Phase(phase) => phase_token(*phase),
            },
            age_clock_ms: match &self.activity {
                ChildActivity::Tool { started_at, .. } => *started_at,
                ChildActivity::Phase(_) => self.last_activity_ms,
            },
            wait: self.wait.and_then(|wait| wait.label()),
        }
    }

    /// The title fragment a live task panel appends, in the header's own
    /// shape — the activity first, then how long it has run: `bash 28s`,
    /// `reasoning 12s`, `bash · 等待你的授权`. `now_ms` is passed in so the
    /// elapsed time is measured at card build time.
    pub fn title_fragment(&self, now_ms: i64) -> String {
        let parts = self.title_parts();
        let mut segments = match parts.age_clock_ms {
            Some(at) => vec![format!("{} {}", parts.label, fmt_elapsed(secs_since(at, now_ms)))],
            None => vec![parts.label.to_string()],
        };
        if let Some(wait) = parts.wait {
            segments.push(wait.to_string());
        }
        segments.join(" · ")
    }

    /// The epoch-ms clock the fragment's rendered age counts from — the live
    /// tool's server start, or the phase's newest activity. `None` when the
    /// fragment shows no age at all (a tool with no server clock yet), so a
    /// reader keyed on this fragment (the ledger's render clock, ADR-0060)
    /// knows there is no number to tick.
    pub fn age_clock_ms(&self) -> Option<i64> {
        self.title_parts().age_clock_ms
    }
}

/// A liveness fragment's own parts, the shape [`TaskLiveness::title_fragment`]
/// renders and the ledger's `activity_estimate` reserves room for — one
/// description, so the display and its cost cannot disagree.
pub(crate) struct TitleParts<'a> {
    /// The activity's display label: a tool's own name, or the phase word.
    /// Raw here; the ledger escapes it where it reaches a markdown body.
    pub(crate) label: &'a str,
    /// The clock the rendered age counts from; `None` when the fragment shows
    /// no age at all.
    pub(crate) age_clock_ms: Option<i64>,
    /// The wait's words without the header's icon, when one is pending.
    pub(crate) wait: Option<&'a str>,
}

/// The liveness fragment's wait vocabulary, from the run's own set: both waits
/// can be live at once (spec #1057's `RunWaits`).
fn awaiting_action(waits: RunWaits) -> Option<AwaitingAction> {
    if !waits.any() {
        return None;
    }
    match (waits.permission, waits.question) {
        (false, false) => None,
        (true, false) => Some(AwaitingAction::Permission),
        (false, true) => Some(AwaitingAction::Question),
        (true, true) => Some(AwaitingAction::Both),
    }
}

/// Seconds between two epoch-ms clocks, never negative.
fn secs_since(at_ms: i64, now_ms: i64) -> u64 {
    ((now_ms - at_ms).max(0) / 1000) as u64
}

/// The text a tool panel renders for a call: the decoder's text blocks joined
/// (in decoder order), with a failure's message appended on its own line. An
/// explicit empty text block still counts as output (the historical
/// `metadata.output: ""` rendered as an empty body, not as no output); a call
/// with neither text nor an error has no output.
///
/// Non-text blocks and the raw payload stay out: the panel renders the plain
/// string the card has always shown.
fn tool_output(call: &ToolCall) -> Option<String> {
    let mut out = String::new();
    let mut has_output = false;
    for block in &call.output.blocks {
        if let ContentBlock::Text(text) = block {
            has_output = true;
            out.push_str(text);
        }
    }
    if call.status.is_failure()
        && let Some(error) = &call.output.error
    {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("❌ {}", error));
        has_output = true;
    }
    has_output.then_some(out)
}

/// The trailing note after a file-editing call's success summary: everything
/// following the output's first blank line, trimmed. `None` when the text IS
/// the summary (or nothing) — the summary is dropped, never shown beside the
/// hunks it summarizes.
fn summary_tail(output: &str) -> Option<&str> {
    output
        .split_once("\n\n")
        .map(|(_, tail)| tail.trim())
        .filter(|tail| !tail.is_empty())
}

/// The diff a raw metadata payload carries: V1 stores one plain string at
/// `metadata.diff`; V2 stores `metadata.files[]`, one `{file, patch, …}` per
/// touched file, whose patches join here in file order. Each patch keeps its
/// own `Index:` header — `apply_patch`'s multi-file parser reads those for
/// per-file attribution.
///
/// Shared with the permission card (#794): V2's `edit` permission carries the
/// same `{files: [FileDiff]}` metadata and no top-level `diff`, so reading only
/// `metadata.diff` would render no change on the card where the reader decides
/// whether to approve it.
pub(crate) fn diff_from_metadata(metadata: Option<&serde_json::Value>) -> Option<String> {
    let metadata = metadata?;
    if let Some(diff) = metadata
        .get("diff")
        .and_then(serde_json::Value::as_str)
        .filter(|diff| !diff.is_empty())
    {
        return Some(diff.to_string());
    }
    let patches = metadata
        .get("files")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(|file| file.get("patch").and_then(serde_json::Value::as_str))
        .filter(|patch| !patch.is_empty())
        .collect::<Vec<_>>();
    (!patches.is_empty()).then(|| patches.join("\n"))
}

/// A Tool Panel for tests, built from the typed pieces a formatter reads: the
/// tool name, typed status, raw input, and one text output block (the shape
/// most fixtures need). It does NOT infer a [`ToolDetail`] from the id: a
/// fixture that needs a shape uses [`Self::for_test_file_edits`]/
/// [`Self::for_test_read`] or builds the [`ToolCall`] with its detail, so a
/// bare `bash`/`task` id here exercises the opaque path on purpose. Production
/// code always wraps a decoded [`ToolCall`] whose adapter declared the detail.
#[cfg(test)]
impl ToolPanel {
    pub(crate) fn for_test(
        name: &str,
        status: ToolStatus,
        input: Option<serde_json::Value>,
        output: Option<&str>,
    ) -> Self {
        Self::new(ToolCall {
            identity: crate::backend::ToolIdentity {
                name: name.to_string(),
                call_id: format!("call_{name}"),
            },
            status,
            started_at: None,
            input,
            metadata: None,
            output: crate::backend::ToolOutput {
                raw: output.map(|text| serde_json::Value::String(text.to_string())),
                blocks: output
                    .map(|text| vec![ContentBlock::Text(text.to_string())])
                    .unwrap_or_default(),
                error: None,
            },
            ..ToolCall::default()
        })
    }

    /// A test panel whose typed detail IS the fixture: the adapter's shape,
    /// never a mapping from a raw payload (the client seam keeps the card away
    /// from the adapter even in tests).
    pub(crate) fn for_test_file_edits(
        name: &str,
        status: ToolStatus,
        input: Option<serde_json::Value>,
        output: Option<&str>,
        edits: Vec<FileEdit>,
    ) -> Self {
        Self::new(ToolCall {
            identity: crate::backend::ToolIdentity {
                name: name.to_string(),
                call_id: format!("call_{name}"),
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(edits),
            status,
            started_at: None,
            input,
            output: crate::backend::ToolOutput {
                raw: output.map(|text| serde_json::Value::String(text.to_string())),
                blocks: output
                    .map(|text| vec![ContentBlock::Text(text.to_string())])
                    .unwrap_or_default(),
                error: None,
            },
            ..ToolCall::default()
        })
    }

    /// A test read panel with the typed `Read` detail.
    pub(crate) fn for_test_read(status: ToolStatus, path: Option<&str>, output: Option<&str>) -> Self {
        Self::new(ToolCall {
            identity: crate::backend::ToolIdentity {
                name: vocab::TOOL_READ.to_string(),
                call_id: "call_read".to_string(),
            },
            kind: ToolKind::Read,
            detail: ToolDetail::Read {
                path: path.map(str::to_string),
                preview: None,
                directory: false,
            },
            status,
            started_at: None,
            input: path.map(|path| serde_json::json!({ "filePath": path })),
            output: crate::backend::ToolOutput {
                raw: output.map(|text| serde_json::Value::String(text.to_string())),
                blocks: output
                    .map(|text| vec![ContentBlock::Text(text.to_string())])
                    .unwrap_or_default(),
                error: None,
            },
            ..ToolCall::default()
        })
    }

    /// A panel carrying one unresolved File Content per `(name, bytes)` pair,
    /// each a PNG `data:` URI (ADR-0076) — the shape the split-estimate and the
    /// attach-guard tests build their panels from.
    pub(crate) fn for_test_with_files(name: &str, files: &[(&str, &[u8])]) -> Self {
        use base64::Engine as _;
        let mut panel = Self::for_test(name, ToolStatus::Completed, None, None);
        panel.files = files
            .iter()
            .map(|(file_name, bytes)| {
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                let content = crate::backend::FileContent::decode(
                    &format!("data:image/png;base64,{encoded}"),
                    Some("image/png"),
                    Some(file_name),
                )
                .expect("an inline payload is a File Content");
                ToolFile {
                    content,
                    delivery: FileDelivery::Unresolved,
                }
            })
            .collect();
        panel
    }
}

/// How a Tool Panel's output body renders. The renderer for a tool knows its
/// own body best, so the choice travels with `format_tool_output`'s result
/// instead of being re-derived from the tool name at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyStyle {
    /// Fence it when Feishu markdown would wrap a long line or fold lines into
    /// a Setext heading (the generic fallback for free text).
    Auto,
    /// Always a fenced code block: file content and diff hunks, where one
    /// visual line per source line matters.
    Code(Option<&'static str>),
    /// Markdown by construction (a generated list): never fenced, even when a
    /// line is long — fencing would show the literal list syntax.
    Markdown,
}

/// The tracking block a Tool Panel appends for its File Contents (ADR-0076):
/// one line per file — `content.record_line()` plus the resolved delivery's
/// state — so a file the panel read stops being invisible. `None` when the call
/// carries no File Content: `· 已内嵌` for an embedded image, `· 已发送为文件消息`
/// for one delivered as a File Message, `· 未发送` for one that could not be.
fn file_tracking_block(tool: &ToolPanel) -> Option<String> {
    if tool.files().is_empty() {
        return None;
    }
    Some(
        tool.files()
            .iter()
            .map(|file| match delivery_suffix(&file.delivery) {
                Some(suffix) => format!("{} · {suffix}", file.content.record_line()),
                None => file.content.record_line(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// The state suffix a File Content's tracking line carries (ADR-0076): the one
/// copy `· 已内嵌` / `· 已发送为文件消息` / `· 未发送`, shared by the Tool Panel
/// and the **File Viewer** card (ADR-0083 §5) so the two surfaces cannot drift.
/// `None` for a content nothing has decided yet (no suffix while unresolved).
pub(crate) fn delivery_suffix(delivery: &FileDelivery) -> Option<&'static str> {
    match delivery {
        FileDelivery::Embedded { .. } => Some("已内嵌"),
        FileDelivery::SentAsFile => Some("已发送为文件消息"),
        FileDelivery::FileSendOutcomeUnknown => Some("发送结果未知，可能已发送；为避免重复未重试"),
        FileDelivery::SentAsRichText => Some("已发送为富文本消息"),
        FileDelivery::Undelivered => Some("未发送"),
        FileDelivery::Unresolved => None,
    }
}

/// The character budget the folded panel HEADER spends on a single File
/// Content's name (ADR-0076 amendment): the header is a one-line status strip
/// that shows the file's BASENAME ([`header_file_name`]), so the budget only
/// bites a genuinely long FILE name — a basename longer than this is clipped to
/// its TAIL, a leading `…` plus the last characters, keeping the extension
/// visible — while the body's tracking line keeps the full name. The leading
/// `…` counts against the budget, so the rendered name is at most this many
/// characters.
const FILE_HEADER_NAME_MAX_CHARS: usize = 24;

/// The name a File Content's folded panel HEADER falls back to when folding
/// leaves nothing printable (ADR-0076 amendment): an all-line-breaking filename
/// would otherwise clip to a bare, nameless `📎`.
const DEFAULT_FILE_HEADER_NAME: &str = "文件";

/// Whether `c` must fold to a space to keep the folded panel HEADER's one-line
/// `plain_text` title on one line (ADR-0076 amendment): every `Cc` control
/// character (a newline, carriage return, tab, `U+0085` next line, …) plus the
/// two Unicode line-breaking separators `U+2028` LINE SEPARATOR and `U+2029`
/// PARAGRAPH SEPARATOR. The separators are `Zl`/`Zp`, NOT `Cc`, so
/// `char::is_control()` alone misses them while a renderer may still break a
/// line on them.
fn is_header_line_break(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

/// A File Content's name folded to a single printable line for the folded
/// panel HEADER (ADR-0076 amendment): every line-breaking character
/// ([`is_header_line_break`] — a newline, a carriage return, a tab, any other
/// `Cc`, and the `U+2028`/`U+2029` separators) becomes a space, so the one-line
/// `plain_text` title cannot wrap or break. `truncate_md` clips by character
/// count and would pass a newline straight through, and a valid filename may
/// carry one. One character in, one character out, so the clip's character
/// budget is unchanged. The folded name is then trimmed: a name that is ALL
/// line-breaking characters folds to a run of spaces and would spend the clip
/// budget before any printable character, leaving a nameless `📎`, so
/// leading/trailing spaces are dropped and an empty result falls back to
/// [`DEFAULT_FILE_HEADER_NAME`] — the header always names the file. Mirrors the
/// ledger's `folded_label`.
fn folded_header_name(name: &str) -> String {
    let folded: String = name
        .chars()
        .map(|c| if is_header_line_break(c) { ' ' } else { c })
        .collect();
    let trimmed = folded.trim();
    if trimmed.is_empty() {
        DEFAULT_FILE_HEADER_NAME.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Clip a folded File Content name for the folded panel HEADER keeping its TAIL
/// (ADR-0076 amendment): the header is a one-line status strip, and a FILE name
/// longer than [`FILE_HEADER_NAME_MAX_CHARS`] is clipped by dropping its head
/// and keeping its tail — a leading `…` plus the last characters, the `…`
/// counted against the budget so the whole rendered name fits it — so a long
/// name still shows its extension. Applied to the basename
/// ([`header_file_name`]), this only bites a genuinely long FILE name; a long
/// absolute PATH is reduced to its basename first, so its directories never
/// spend the budget. Character-counted like the body's [`truncate_md`], so a
/// multibyte (CJK) name clips on a character boundary and never splits one — the
/// tail-side mirror of the head clip it replaces. A name already within the
/// budget is returned unchanged, with no `…`.
fn clip_header_name_tail(name: &str, max_chars: usize) -> String {
    let count = name.chars().count();
    if count <= max_chars {
        return name.to_string();
    }
    // The leading `…` is part of the budget: it plus the last `max_chars - 1`
    // characters is exactly `max_chars` (or fewer when `max_chars` is tiny).
    let keep = max_chars.saturating_sub(1);
    let tail: String = name.chars().skip(count - keep).collect();
    format!("…{tail}")
}

/// The basename of a folded File Content name (ADR-0076 amendment): the
/// substring after its final `/` or `\`. `None` when the name ends in a
/// separator (the basename is then empty), so [`header_file_name`] falls back to
/// the whole folded name rather than render an empty marker. A name with no
/// separator at all is returned whole — a bare filename is already its own
/// basename.
fn header_name_basename(folded: &str) -> Option<&str> {
    let after = folded.rsplit(['/', '\\']).next().unwrap_or(folded);
    if after.is_empty() { None } else { Some(after) }
}

/// The short File Content name the folded panel HEADER shows (ADR-0076
/// amendment): the name folded to one printable line ([`folded_header_name`]),
/// reduced to its BASENAME ([`header_name_basename`]), then clipped to its tail
/// within [`FILE_HEADER_NAME_MAX_CHARS`] ([`clip_header_name_tail`]) — the clip
/// now only bites a genuinely long FILE name, not a long absolute path, since
/// the directories are dropped first. A name ending in a separator (or one with
/// nothing printable) falls back to the whole folded name, so the marker is
/// never empty. Shared by the header marker and the embedded image element's
/// `title`/`alt` ([`file_image_elements`]) so they never diverge: the image's
/// fields are the SAME short name the header shows when it shows one, not a
/// second, longer copy of the absolute path a `read` reports.
fn header_file_name(name: &str) -> String {
    let folded = folded_header_name(name);
    let chosen = header_name_basename(&folded).unwrap_or(&folded);
    clip_header_name_tail(chosen, FILE_HEADER_NAME_MAX_CHARS)
}

/// The file a file-touching panel is about, as its FOLDED header names it
/// (ADR-0083 §8 amendment, #766). The panel body carries the path in its Input
/// line, but the body starts collapsed, so a file-editing panel used to say
/// nothing about the file it touched until the reader unfolded it — and a text
/// `read` likewise.
///
/// A MULTI-file list is the exception that names no path (#793): its folded
/// header carries the file count and the change totals instead
/// (`📄 3 files · +42 −7`). One path could not describe several files, the
/// action lines that separate them are inside the fold, and the count is what
/// tells the reader whether expanding is worth it. The totals come from the
/// same typed [`FileEdit`] hunks the body renders, so they cannot disagree. A
/// single file names its action and its short name — a rename names both ends.
///
/// The name is the same short form the File Content marker/caption uses
/// ([`header_file_name`]): basename first, one printable line, tail-clipped to
/// [`FILE_HEADER_NAME_MAX_CHARS`]. A folded header is one `plain_text` line,
/// and on a phone a full absolute path would dominate it — the directories
/// belong to the unfolded body and the viewer.
///
/// A `read` whose output carries a File Content gets NO fragment: that file is
/// already named by the `📎` marker (`SentAsFile`/`Undelivered`, ADR-0076) or by
/// the embedded image's caption, and two names for one file on one line is
/// noise. `None` for every other tool and for a call with no usable path.
fn panel_file_detail(tool: &ToolPanel) -> Option<String> {
    if let Some(edits) = file_edits(tool) {
        // A failed call's body is the error text and its change is unknown, so
        // `+0 −0` there would claim nothing changed; the count still stands.
        if edits.len() > 1 {
            let totals = (!call_failed(tool)).then(|| file_edits_totals(&edits));
            return Some(multi_file_detail(edits.len(), totals));
        }
        return Some(format!(" · {}", file_edit_title(edits.first()?)));
    }
    if is_codex_image(tool) {
        return None;
    }
    let read = read_detail(tool)?;
    if !tool.files().is_empty() {
        return None;
    }
    let path = read.path.filter(|path| !path.trim().is_empty())?;
    // The book icon the read panel's Input line already uses — and the one
    // that still reads correctly for a directory read.
    // A directory read may name its path with a trailing separator. Dropping it
    // lets the short-name rule reach the directory's own name instead of
    // rendering the whole path; a path that is nothing BUT separators keeps its
    // form (`header_file_name` then falls back to it).
    let trimmed = path.trim_end_matches(['/', '\\']);
    let name = if trimmed.is_empty() { path } else { trimmed };
    Some(format!(" · 📖 {}", header_file_name(name)))
}

/// The folded-title fragment a multi-file patch carries: the file count and,
/// when they are known, the change totals. One formatter for both the Codex
/// and OpenCode producers, so the two cannot disagree.
fn multi_file_detail(files: usize, totals: Option<(usize, usize)>) -> String {
    match totals {
        Some((additions, deletions)) => format!(" · 📄 {files} files · +{additions} −{deletions}"),
        None => format!(" · 📄 {files} files"),
    }
}

/// The target agents a collaboration panel's folded title names (issue #962):
/// the first row's thread — the call's receiver when it has one — plus how many
/// other targets the same call covers. The label is the very eight-character id
/// the body rows use, so the folded and unfolded views cannot disagree. A child
/// agent activity names the child's `agentPath` instead, which is the readable
/// identity a sub-agent has on this wire.
fn panel_target_detail(tool: &ToolPanel) -> Option<String> {
    let obj = tool.input()?.as_object()?;
    let (label, count) = match tool.name() {
        "codex/collab_agent" if collab_agent_shape(tool.input()) => {
            let agents = obj.get("agents")?.as_array()?;
            let first = agents.first()?;
            (collab_agent_label(first)?, agents.len())
        }
        "codex/sub_agent_activity" if sub_agent_activity_shape(tool.input()) => {
            // A folded header is one `plain_text` line with its own budget: a
            // deep path keeps its tail (the agent's own name) and drops its
            // head, and any line-breaking character folds to a space — the same
            // rules a long file name takes, since both ride the title.
            let path = folded_header_fragment(obj.get("agentPath")?.as_str()?);
            (clip_header_name_tail(&path, FILE_HEADER_NAME_MAX_CHARS), 1)
        }
        _ => return None,
    };
    let mut detail = format!(" · 🧵 {label}");
    if count > 1 {
        detail.push_str(&format!(" +{}", count - 1));
    }
    Some(detail)
}

/// Fold every line-breaking character of a value that rides a one-line
/// `plain_text` header to a space — the file-name rule ([`folded_header_name`])
/// without that name's empty fallback, for a path the header shows as a
/// fragment. A valid Unix path may carry a newline or a `U+2028`, and an
/// unfolded one would break the folded title across lines.
fn folded_header_fragment(text: &str) -> String {
    text.chars()
        .map(|c| if is_header_line_break(c) { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The byte length the folded header's target detail [`panel_target_detail`]
/// contributes to the card, for a card-size estimate that must charge what the
/// header renders. `0` when the panel carries no detail; kept beside the render
/// so the two cannot drift.
pub(crate) fn panel_target_detail_estimate(tool: &ToolPanel) -> usize {
    header_fragment_estimate(panel_target_detail(tool))
}

/// The byte length the folded header's file detail [`panel_file_detail`]
/// contributes to the card (ADR-0083 §8 amendment, #766), for a card-size
/// estimate that must charge what the header renders. Like the File Content
/// marker's estimate, the title is a `plain_text` element whose bytes reach the
/// card JSON verbatim, so the detail is charged at its JSON-escaped length —
/// charging the raw length would under-count a quote or control character in a
/// filename. `0` when the panel carries no detail. Kept beside the render so
/// the two cannot drift.
pub(crate) fn panel_file_detail_estimate(tool: &ToolPanel) -> usize {
    header_fragment_estimate(panel_file_detail(tool))
}

/// The compact File Content marker the folded panel HEADER carries (ADR-0076
/// amendment). The detailed tracking line lives in the panel BODY, which is
/// collapsed by default, so the user never sees it — worst of all for `未发送`,
/// the ONLY signal that a file was read but could not be delivered. The marker
/// exists for the files the card does NOT show: an embedded image is visible
/// beside the panel with its own caption ([`file_image_elements`]), so naming it
/// again in the folded header would only duplicate it, and an `Unresolved` file
/// is not yet a card's concern. The marker therefore appears IFF the panel has at
/// least one file delivered [`FileDelivery::SentAsFile`] or
/// [`FileDelivery::Undelivered`], and names only those: ` · 📎 <name>` for a
/// single one (the name's BASENAME, folded to one printable line and clipped to
/// its tail within [`FILE_HEADER_NAME_MAX_CHARS`] only when the basename itself
/// is long), ` · 📎 <N> 个文件` for several, and ` · 未发送` appended when ANY of
/// them is [`FileDelivery::Undelivered`]. A panel whose files are ALL `Embedded`
/// (or unresolved) carries NO marker, so its header is byte-for-byte the plain
/// one. The body's tracking line is unchanged for every file. `None` when the
/// panel carries no such file.
fn file_header_marker(tool: &ToolPanel) -> Option<String> {
    let marked: Vec<&ToolFile> = tool
        .files()
        .iter()
        .filter(|file| {
            matches!(
                file.delivery,
                FileDelivery::SentAsFile | FileDelivery::Undelivered
            )
        })
        .collect();
    if marked.is_empty() {
        return None;
    }
    let mut marker = if marked.len() == 1 {
        format!(" · 📎 {}", header_file_name(&marked[0].content.name))
    } else {
        format!(" · 📎 {} 个文件", marked.len())
    };
    if marked
        .iter()
        .any(|file| file.delivery == FileDelivery::Undelivered)
    {
        marker.push_str(" · 未发送");
    }
    Some(marker)
}

/// The byte length the folded panel HEADER's File Content marker
/// [`file_header_marker`] contributes to the card (ADR-0076 amendment), for a
/// card-size estimate that must charge what the header renders. The header
/// title is a `plain_text` element — NOT markdown-sanitized like the panel body
/// — so its bytes reach the card's JSON verbatim and only the JSON escaping
/// (`"` → `\"`, `\` → `\\`, a control character to its escape — two bytes for
/// the short `\n`/`\r`/`\t`/`\b`/`\f` forms, six for the `\uXXXX` one) inflates
/// them; charging the raw length would under-count a quote-heavy filename and
/// let a card pass the split check while Feishu rejects the serialized card on
/// size. `0` when the panel carries no File Content. Kept beside the render so
/// the two cannot drift.
pub(crate) fn file_header_marker_estimate(tool: &ToolPanel) -> usize {
    header_fragment_estimate(file_header_marker(tool))
}

/// The byte length an optional folded-header fragment contributes to the card:
/// its JSON-escaped form, or `0` when the header carries none. Shared by the
/// File Content marker (ADR-0076) and the file detail (ADR-0083 §8 amendment,
/// #766) so the two estimates cannot drift apart. The header title is a
/// `plain_text` element — NOT markdown-sanitized like the panel body — so its
/// bytes reach the card's JSON verbatim and only the JSON escaping inflates
/// them; the escape accounting lives in [`json_escaped_len`].
fn header_fragment_estimate(fragment: Option<String>) -> usize {
    fragment.map(|text| json_escaped_len(&text)).unwrap_or(0)
}

/// The byte length of the tracking block [`file_tracking_block`] appends to a
/// panel body (ADR-0076), for a card-size estimate that must charge the same
/// bytes the renderer writes — the record line plus the `· 已内嵌` /
/// `· 已发送为文件消息` / `· 未发送` suffix the resolved delivery adds. `0` when
/// the panel carries no File Content. Kept beside the render so the two cannot
/// drift.
///
/// The block is charged at its **serialized** size, not its raw UTF-8 length:
/// it is markdown-sanitized by the card's own [`CardMarkdown::element`] (which
/// escapes `<` and downgrades images) and then embedded in the card's JSON
/// `content` string, where a `"` or `\` doubles to two bytes and a control
/// character to six. A raw-length estimate under-counts a filename full of
/// quotes (or `<`) by half, letting a panel pass the split check while the
/// serialized card still exceeds Feishu's size cap — which the fenced content
/// fallback cannot repair.
pub(crate) fn file_tracking_estimate(tool: &ToolPanel) -> usize {
    file_tracking_block(tool)
        .map(|block| json_escaped_len(&CardMarkdown::new().element(&block)))
        .unwrap_or(0)
}

/// The byte length `text` contributes to a JSON string: its JSON-escaped form
/// without the surrounding quotes. `serde_json::to_string` on a `&str` cannot
/// fail, so the raw-length fallback is unreachable; it only keeps the estimate
/// finite if it ever were.
pub(crate) fn json_escaped_len(text: &str) -> usize {
    serde_json::to_string(text)
        .map(|quoted| quoted.len().saturating_sub(2))
        .unwrap_or(text.len())
}

/// The bytes one character contributes to [`json_escaped_len`]: `"` and `\`
/// take a two-byte escape, a control character JSON's widest six-byte
/// `\uXXXX` form (with the short `\n`/`\r`/`\t`/`\b`/`\f` forms at two), and
/// everything else its own UTF-8 length — serde_json does not escape
/// non-ASCII. The clip walks this charge, so it never cuts a char in half and
/// never over-counts a body by its worst case.
fn escaped_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        c if c < '\u{20}' => 6,
        c => c.len_utf8(),
    }
}

/// The bytes the opening fence's language token adds to a fenced block: zero
/// for `None` or an empty token (the shapes [`fenced_code`] renders as a bare
/// fence). The tag sits on the opening fence line, so it is part of the block
/// the diff budget and `panel_output_estimate` must charge.
fn fence_lang_bytes(lang: Option<&str>) -> usize {
    lang.filter(|lang| !lang.is_empty()).map_or(0, str::len)
}

/// Whether `text` fits a fenced-body budget: its JSON-escaped length PLUS the
/// whole fence [`fenced_code`] will wrap it in (dynamic backtick run, minimum
/// fence and newlines) PLUS the opening fence's language token. The one budget
/// test [`clip_fenced_body`] and the diff note share, so a body is clipped
/// exactly when the note says so.
fn fenced_body_fits(text: &str, lang: Option<&str>, max_escaped_bytes: usize) -> bool {
    json_escaped_len(text) + fenced_block_overhead(text) + fence_lang_bytes(lang) <= max_escaped_bytes
}

/// Clip `text` to fit `max_escaped_bytes`, where the budget covers the body's
/// JSON-escaped bytes, the fence [`fenced_code`] adds around it AND the opening
/// fence's `lang` token, with NO truncation marker — the bare clip a fenced
/// body takes when a note below the fence says it was clipped (the `…`
/// immediately before a closing fence stops Feishu rendering the block as code
/// at all, #788).
///
/// The fence must be inside the budget, not beside it: `fenced_code` sizes its
/// fence to the body's longest backtick run plus one, so a body that clips at
/// the cap while carrying a run of backticks would demand up to twice the cap
/// in fence alone — ~36 KB from a 12 KB body, past both the card estimate and
/// Feishu's hard limit, and a lone panel cannot split away from it. The
/// language tag is part of the rendered block too, so it is charged as well: a
/// body that fits without a tag can otherwise be pushed over by the tag alone
/// (`rust` is four bytes). The walk therefore charges each char's escaped bytes
/// plus the fence growth a new backtick run causes against the tag-adjusted
/// budget, and stops before the one that would cross it (the charge only ever
/// grows, so the first over-budget prefix is the answer). Measured in escaped
/// bytes, not characters, so the cap matches the card's own byte budget
/// ([`DIFF_MAX_ESCAPED_BYTES`], #792); the cut lands on a character boundary.
/// Shared with the permission card, which passes its own selected language
/// (#794).
pub(crate) fn clip_fenced_body(text: &str, lang: Option<&str>, max_escaped_bytes: usize) -> String {
    if fenced_body_fits(text, lang, max_escaped_bytes) {
        return text.to_string();
    }
    let lang_bytes = fence_lang_bytes(lang);
    let mut used = 0usize;
    let mut run = 0usize;
    let mut longest = 0usize;
    let mut clipped = String::new();
    for c in text.chars() {
        let cost = escaped_char_len(c);
        let next_run = if c == '`' { run + 1 } else { 0 };
        let next_longest = longest.max(next_run);
        if used + cost + fenced_block_bytes(next_longest) + lang_bytes > max_escaped_bytes {
            break;
        }
        used += cost;
        run = next_run;
        longest = next_longest;
        clipped.push(c);
    }
    clipped
}

/// [`clip_fenced_body`] from the END: keep the NEWEST bytes of a body under the
/// same budget, dropping the oldest ones. Output panes need this — a command's
/// last line is its diagnostic, so a fence-sized clip must eat the prefix, not
/// the tail. The walk is the same charge as [`clip_fenced_body`] (escaped
/// bytes + the fence the accumulated suffix needs + `lang`), run backwards and
/// then reversed; the fence only grows as the walk extends leftward, so the
/// first over-budget character stops it. No truncation marker, for the same
/// reason as [`clip_fenced_body`].
pub(crate) fn clip_fenced_tail(text: &str, lang: Option<&str>, max_escaped_bytes: usize) -> String {
    if fenced_body_fits(text, lang, max_escaped_bytes) {
        return text.to_string();
    }
    let lang_bytes = fence_lang_bytes(lang);
    let mut used = 0usize;
    let mut run = 0usize;
    let mut longest = 0usize;
    let mut kept: Vec<char> = Vec::new();
    for c in text.chars().rev() {
        let cost = escaped_char_len(c);
        let next_run = if c == '`' { run + 1 } else { 0 };
        let next_longest = longest.max(next_run);
        if used + cost + fenced_block_bytes(next_longest) + lang_bytes > max_escaped_bytes {
            break;
        }
        used += cost;
        run = next_run;
        longest = next_longest;
        kept.push(c);
    }
    kept.into_iter().rev().collect()
}

/// How a shell-like command's output body is clipped to its budget. Each
/// variant names ONE existing clip rule rather than a new one — a panel preview
/// keeps [`truncate_md`]'s `…` marker, a fenced element clips bare (a `…` right
/// before the closing fence stops Feishu rendering the block as code at all,
/// #788) — so the two surfaces that share the block keep their measured
/// behaviour and share only the composition above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputClip {
    /// A markdown head clip, budgeted in CHARACTERS, ending on the shared `…`.
    Chars { max_chars: usize },
    /// A fenced element's head clip, budgeted in the JSON-ESCAPED BYTES the
    /// element actually carries, clipped bare.
    Bytes { max_bytes: usize },
    /// [`Self::Bytes`] from the END: the newest bytes survive, because a
    /// command's last line is its diagnostic.
    BytesTail { max_bytes: usize },
}

impl OutputClip {
    /// The clip one fenced element's body takes under `prefix`, keeping its
    /// head: the element's whole budget is [`MAX_ELEMENT_TEXT_CHARS`], so the
    /// prefix and its newline come out of the body's.
    pub(crate) fn under_element(prefix: &str) -> Self {
        Self::Bytes {
            max_bytes: element_body_budget(prefix),
        }
    }

    /// [`Self::under_element`] from the END: the newest bytes survive, because a
    /// command's last line is its diagnostic.
    pub(crate) fn tail_under_element(prefix: &str) -> Self {
        Self::BytesTail {
            max_bytes: element_body_budget(prefix),
        }
    }

    /// The clipped body: the budget applies and no fence is added, for a caller
    /// that needs the exact text the block shows (a button's payload).
    pub(crate) fn clipped(self, body: &str) -> String {
        match self {
            Self::Chars { max_chars } => truncate_md(body, max_chars),
            Self::Bytes { max_bytes } => clip_fenced_body(body, None, max_bytes),
            Self::BytesTail { max_bytes } => clip_fenced_tail(body, None, max_bytes),
        }
    }
}

/// The bytes one fenced element's body may spend under `prefix`: the element's
/// whole budget ([`MAX_ELEMENT_TEXT_CHARS`]) less the prefix and its newline.
fn element_body_budget(prefix: &str) -> usize {
    MAX_ELEMENT_TEXT_CHARS.saturating_sub(prefix.len() + 1)
}

/// The `Command` output block — one code path for a `Command` Tool Panel
/// (ADR-0105) and the Host **Terminal Card** (ADR-0086: the two share a render
/// block, never a lifecycle, a Chain or a record).
///
/// The body is clipped by `clip` and rendered as the markdown that goes under
/// the caller's own label: fenced outright when the body is `preformatted`
/// machine output (a terminal log always is) — or, for free text, only when
/// Feishu's markdown would wrap a long line or fold the lines above a Setext
/// underline into one heading. Returns the block and whether `clip` cut the
/// body, so a caller's own "it was truncated" line follows the same verdict.
pub(crate) fn command_output_block(body: &str, clip: OutputClip, preformatted: bool) -> (String, bool) {
    let clipped = clip.clipped(body);
    let cut = clipped != body;
    let block = if preformatted || needs_code_block(&clipped) {
        fenced_code(&clipped, None)
    } else {
        clipped
    };
    (block, cut)
}

/// The `img` elements this panel renders (ADR-0076): one per embedded File
/// Content, all pushed immediately AFTER the panel so the image is visible and
/// its adjacency to the panel gives the correspondence. `alt` and `title` share
/// one short name — the file's BASENAME, folded and clipped to its tail within
/// [`FILE_HEADER_NAME_MAX_CHARS`] via [`header_file_name`] only when the
/// basename itself is long — NOT the full absolute path a `read` reports.
/// `title` is the visible caption, `📎 <name>`; `alt` is the image's accessible
/// name and the text a client shows if the image fails to load, so it is the
/// **plain basename** (`shot.png`), no `📎` prefix and no path. Feishu's Card
/// JSON 2.0 image component marks `alt` required (field table:
/// `alt | Yes | Struct`), so the field is always present and now meaningfully
/// populated while staying short. `preview` is on so a click enlarges it. A
/// content with no embed renders nothing here. EVERY embedded file emits its
/// `img` — there is no per-panel cap: the delivery verdict is the content's
/// intrinsic embeddability, so the render is exactly what the panel's files
/// resolved to.
pub(super) fn file_image_elements(tool: &ToolPanel) -> Vec<serde_json::Value> {
    tool.files()
        .iter()
        .filter_map(|file| {
            file.delivery
                .as_embedded_key()
                .map(|image_key| file_image_element(image_key, &file.content.name))
        })
        .collect()
}

/// The `img` element one embedded File Content renders, from the reusable
/// `image_key` and the content's name. `alt` is the image's accessible name (the
/// text a client shows if the image fails to load) and `title` the visible
/// caption, both the file's short BASENAME (`header_file_name`: folded and
/// clipped to its tail when long). `preview` is on so a click enlarges it.
/// Shared by the Tool Panel (ADR-0076) and the **File Viewer** card (ADR-0083
/// §5), so the two embed surfaces cannot drift.
pub(crate) fn file_image_element(image_key: &str, name: &str) -> serde_json::Value {
    let name = header_file_name(name);
    serde_json::json!({
        "tag": "img",
        "img_key": image_key,
        "alt": { "tag": "plain_text", "content": name.clone() },
        "title": { "tag": "plain_text", "content": format!("📎 {name}") },
        "preview": true,
    })
}

/// The serialized byte length the `img` elements [`file_image_elements`] emits
/// add to a card body (ADR-0076), for a card-size estimate that must charge
/// exactly what renders. Each element carries the File Content's name once, in
/// its `title` and again in its `alt` (the same folded-and-clipped basename),
/// embedded in the card's JSON, so the name's JSON-escaped bytes — once per
/// field — plus the element's own `img_key` and scaffolding are charged. A
/// count-only charge (or a raw name-length one) under-counts a panel of
/// embedded images, letting a near-budget card pass the split check while
/// Feishu rejects the serialized card on size — which the fenced content
/// fallback cannot repair.
/// `0` when the panel embeds nothing. Kept beside the render so the two cannot
/// drift.
pub(crate) fn file_image_estimate(tool: &ToolPanel) -> usize {
    file_image_elements(tool)
        .iter()
        .filter_map(|element| serde_json::to_string(element).ok())
        .map(|serialized| serialized.len())
        .sum()
}

/// The way-in button a panel naming an open path carries (ADR-0083 §8, #743,
/// #778): a `button` element placed INSIDE the panel's body, first, so the
/// folded card shows no button row (ADR-0083 §8 amendment, #766) and a long
/// diff — or a clipped `read` body (ADR-0083 §8 amendment, #770) — cannot push
/// the way in out of view. Feishu's JSON 2.0 button supports nesting in a
/// collapsible panel.
///
/// An HTML page (`.html`/`.htm`, [`super::viewer::is_html_page_path`]) reads
/// 「🌐 预览网页」 and carries the [`super::session::tool_page_value`] payload
/// (`op: "preview"`): the panel's body already showed the markup, so the middle
/// hop through the viewer buys nothing, and the click sends the File Message the
/// client renders — WITHOUT rewriting the clicked Turn card (#778). Every other
/// file keeps 「📂 打开」 and the [`super::session::tool_file_value`] payload
/// (`action: "file"` with no `op`), opening the File Viewer rather than
/// transferring it (#739's `op: "send"`).
///
/// `None` when the panel names no open path ([`ToolPanel::file_open_path`]: a
/// `write`/`edit`, or a FILE `read`) or no Session is known to route the click
/// by — no button rather than a broken one.
pub(super) fn tool_open_button_element(
    tool: &ToolPanel,
    session_id: Option<&str>,
) -> Option<serde_json::Value> {
    let path = tool.file_open_path()?;
    let session_id = session_id.filter(|id| !id.is_empty())?;
    let (label, value) = if super::viewer::is_html_page_path(&path) {
        ("🌐 预览网页", super::session::tool_page_value(session_id, &path))
    } else {
        ("📂 打开", super::session::tool_file_value(session_id, &path))
    };
    Some(serde_json::json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": label },
        "type": "default",
        "value": value,
    }))
}

/// The serialized byte length [`tool_open_button_element`] adds to a card
/// (ADR-0083 §8, #743), for a card-size estimate that must charge exactly what
/// renders. `0` when the panel carries no button. Kept beside the render so
/// the two cannot drift.
pub(crate) fn tool_open_button_estimate(tool: &ToolPanel, session_id: Option<&str>) -> usize {
    tool_open_button_element(tool, session_id)
        .and_then(|element| serde_json::to_string(&element).ok())
        .map(|serialized| serialized.len())
        .unwrap_or(0)
}

/// The note a `read` or `write` panel appends under a body its budget CLIPPED
/// (ADR-0083 §8 amendment, #770/#788): the body is a
/// [`TOOL_OUTPUT_MAX_CHARS`]-character preview, so a clipped one says what it
/// is and names the way to the rest. It renders IFF that way renders — the
/// panel carries a way-in button ([`tool_open_button_element`]: a `write`, a
/// file read, and a Session to route the click by) — so the note never promises
/// a control an unopenable panel does not have, and the size estimate can charge
/// exactly the bytes it adds. `None` for every other panel and for a body that
/// fits.
fn preview_truncation_note(tool: &ToolPanel, session_id: Option<&str>) -> Option<&'static str> {
    // The button first: a panel without one (a directory read, an image read, a
    // routeless panel) leaves without assembling the body at all.
    tool_open_button_element(tool, session_id)?;
    preview_body_clipped(tool).then_some(PREVIEW_TRUNCATION_NOTE)
}

/// The Output triple a Codex `webSearch` panel renders. The app-server schema
/// calls `results` opaque JSON, so only one shape is read as a result list —
/// an array whose every entry has a non-empty `url` (a `title` is optional) —
/// and everything else is shown as raw JSON rather than guessed at or dropped
/// (ADR-0042: the payload is the adapter's, not a presentation contract).
///
/// A search that has produced no results yet (`null`, or an empty list while
/// still running) renders no Output at all: an empty Output beside the query
/// would claim the search returned nothing.
fn codex_web_search_output(tool: &ToolPanel) -> Option<(Option<String>, BodyStyle, String)> {
    let results = codex_search_results(tool)?;
    if let Some(entries) = codex_search_entries(results) {
        if entries.is_empty() && tool.is_live() {
            return None;
        }
        let header = Some(format!("🔎 {} 条结果", entries.len()));
        return Some((header, BodyStyle::Markdown, search_result_list(&entries)));
    }
    // The value is a string (a provider's own message) or a shape the schema
    // reserves the right to change: show it, don't interpret it. It clips BARE
    // before the fence, because the `…` `truncate_md` appends directly before a
    // closing fence stops Feishu rendering the block as code at all (#788).
    let body = codex_search_raw_body(results);
    Some((
        None,
        BodyStyle::Code(Some("json")),
        clip_fenced_body(&body, Some("json"), TOOL_OUTPUT_MAX_CHARS),
    ))
}

/// The opaque `results` a web-search payload carries: its metadata first, then
/// the raw output (the adapter writes both; the fallback covers a call from
/// before the field existed). The gate is the PAYLOAD SHAPE, not the id: a V2
/// `websearch`'s raw output is its content-block envelope
/// (`[{type: "text", …}]`) and falls through to [`web_output`], never to this
/// opaque JSON. `None` while the search has no results yet.
fn codex_search_results(tool: &ToolPanel) -> Option<&serde_json::Value> {
    if search_detail(tool)?.scope != SearchScope::Web {
        return None;
    }
    if let Some(results) = tool
        .metadata()
        .and_then(|metadata| metadata.get("results"))
        .filter(|value| !value.is_null())
    {
        // A provider's empty message is no result, not an empty Output.
        return (!results.as_str().is_some_and(|text| text.trim().is_empty())).then_some(results);
    }
    let raw = tool.call().output.raw.as_ref().filter(|value| !value.is_null())?;
    if is_content_block_envelope(raw) {
        return None;
    }
    // A text raw value may be OpenCode's own result envelope (V1's JSON or
    // V2's markdown): the neutral parser claims it. A raw string that parser
    // cannot read is a provider's message — Codex's opaque fallback.
    if let Some(text) = raw.as_str()
        && parse_websearch_results(text).is_some()
    {
        return None;
    }
    Some(raw)
}

/// Whether a value is a tool output's `content` block envelope (V2's
/// `[{type: "text", …}]`), as opposed to a vendor's opaque `results` payload.
fn is_content_block_envelope(value: &serde_json::Value) -> bool {
    value.as_array().is_some_and(|items| {
        !items.is_empty()
            && items
                .iter()
                .all(|item| item.get("type").and_then(serde_json::Value::as_str).is_some())
    })
}

/// The raw-JSON body for a `results` value the card cannot confirm: a JSON
/// string's own text, otherwise its pretty-printed JSON.
fn codex_search_raw_body(results: &serde_json::Value) -> String {
    match results {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// The note a file-editing panel appends under a diff its budget CLIPPED
/// ([`DIFF_TRUNCATION_NOTE`]): the clip's only signal, because the fenced body
/// clips bare (#788). Renders IFF the body the panel shows is a recognized diff
/// ([`file_edits_body`]) whose escaped body, fence and language tag exceed
/// [`DIFF_MAX_ESCAPED_BYTES`] ([`fenced_body_fits`]) — a FAILED call's error
/// text is not a diff and takes no note, since there is no change to name.
/// Unlike [`preview_truncation_note`] it needs no button: "已截断" is true even
/// when nothing can be opened, so the reader still knows the diff is partial.
fn diff_truncation_note(tool: &ToolPanel) -> Option<&'static str> {
    file_edits_body(tool)
        .filter(FileEditsBody::clipped)
        .map(|_| DIFF_TRUNCATION_NOTE)
}

/// The note a Codex search's raw-JSON fallback appends under a body its budget
/// CLIPPED ([`SEARCH_TRUNCATION_NOTE`]): the body clips bare, so without this
/// line a cut payload would look complete. `None` for a recognized result list
/// (its compact list always fits) and for a body that fits.
fn codex_search_truncation_note(tool: &ToolPanel) -> Option<&'static str> {
    let results = codex_search_results(tool)?;
    if codex_search_entries(results).is_some() {
        return None;
    }
    let body = codex_search_raw_body(results);
    (!fenced_body_fits(&body, Some("json"), TOOL_OUTPUT_MAX_CHARS)).then_some(SEARCH_TRUNCATION_NOTE)
}

/// The note a panel appends under a body its budget clipped: a `read` preview's
/// way-in note, a FileEdits diff's bare clip signal, or a Codex search's raw
/// payload note. One function, because the render and the size estimate must
/// agree on exactly one note (a panel never carries two: its body is one shape).
fn truncation_note(tool: &ToolPanel, session_id: Option<&str>) -> Option<&'static str> {
    preview_truncation_note(tool, session_id)
        .or_else(|| diff_truncation_note(tool))
        .or_else(|| codex_search_truncation_note(tool))
}

/// Whether a `read` panel's rendered body is clipped by its own budget — the
/// clamp [`tool_panel_element`] applies ([`truncate_md`] at
/// [`TOOL_OUTPUT_MAX_CHARS`]) — measured from the same body the render shows,
/// the way every size estimate here re-derives its render, so the note
/// [`preview_truncation_note`] renders is the one charged. A FileEdits diff is
/// [`diff_truncation_note`]'s, not this one's.
fn preview_body_clipped(tool: &ToolPanel) -> bool {
    if is_codex_image(tool) || read_detail(tool).is_none() {
        return false;
    }
    tool.output()
        .map(|raw| format_tool_output(tool, &raw).2)
        .is_some_and(|body| exceeds_md_budget(&body, TOOL_OUTPUT_MAX_CHARS))
}

/// The serialized byte length the truncation note [`truncation_note`] appends
/// to a clipped body (ADR-0083 §8 amendment, #770/#788), for a card-size
/// estimate that must charge exactly what the panel writes — a note the estimate
/// missed would let a clipped body pass the split check while the serialized
/// card still carries bytes nothing counted. `0` when the note does not render.
/// Kept beside the render so the two cannot drift.
pub(crate) fn preview_truncation_note_estimate(tool: &ToolPanel, session_id: Option<&str>) -> usize {
    truncation_note(tool, session_id)
        .map(|note| json_escaped_len(&CardMarkdown::new().element(note)))
        .unwrap_or(0)
}

/// The Output triple a panel renders: a Code Mode `Program`'s nested-call body,
/// a FileEdits panel's hunks, a Codex `web_search`'s results (assembled from
/// the adapter's metadata, not the text output), or the tool's output text
/// through the shape-keyed formatter. One function, because the render and the
/// size estimate must agree on which body a panel shows. A `Command` needs no
/// branch here: its body is the call's output text, rendered through the shared
/// command block ([`command_output_block`]) exactly where every panel's body
/// is.
fn panel_output(tool: &ToolPanel) -> Option<(Option<String>, BodyStyle, String)> {
    if is_program(tool) {
        program_output(tool)
    } else if let Some(edits) = file_edits_output(tool) {
        Some(edits)
    } else if let Some(search) = codex_web_search_output(tool) {
        Some(search)
    } else if let Some(collab) = codex_collab_output(tool) {
        Some(collab)
    } else if is_command(tool) {
        // A `Command`'s body is its own output text, never a name-keyed
        // formatter's: the shared command block (below, and in `terminal.rs`)
        // owns how it is clipped and fenced.
        tool.output().map(|raw| (None, BodyStyle::Auto, raw))
    } else {
        tool.output().map(|raw| format_tool_output(tool, &raw))
    }
}

/// Whether the call settled as a FAILURE, by the same rule its status icon
/// draws ([`ToolPanel::status_icon`]): [`ToolStatus::is_failure`], or a
/// recorded fact in the call's shape that reports one.
fn call_failed(tool: &ToolPanel) -> bool {
    tool.detail_failure() || tool.status().is_failure()
}

/// The card bytes one adapter-assembled Output body contributes: its header
/// through the card's sanitizer, then the body clipped at
/// [`TOOL_OUTPUT_MAX_CHARS`] and charged in the form the renderer writes it —
/// a generated list goes through the sanitizer (which expands `<`), a fenced
/// body carries its dynamic fence and language tag. Shared by the Codex search
/// results, a collaboration call's target rows and a Code Mode program's
/// nested-call body, so none of them can drift from what it renders.
fn assembled_body_estimate(header: Option<&str>, style: BodyStyle, body: &str) -> usize {
    let header = header
        .map(|header| json_escaped_len(&CardMarkdown::new().element(header)))
        .unwrap_or(0);
    let body = truncate_md(body, TOOL_OUTPUT_MAX_CHARS);
    let body = match style {
        BodyStyle::Markdown => serialized_len(&body),
        BodyStyle::Code(lang) => json_escaped_len(&body) + fenced_overhead(&body) + fence_lang_bytes(lang),
        BodyStyle::Auto => json_escaped_len(&body) + fenced_overhead(&body),
    };
    header + body
}

/// The serialized byte length the Output body a panel renders contributes to
/// its card size (#788). The branches walk the bodies in the same order
/// [`panel_output`] does, so the first body the renderer reaches is the one
/// charged: a `Program`'s assembled nested-call body; a FileEdits panel's
/// composed body ([`file_edits_body`], clipped at [`DIFF_MAX_ESCAPED_BYTES`]
/// as the render writes it, #792); a Codex search's or collaboration call's
/// assembled body; else the call's output text (a `Command`'s included,
/// through the same shared block). Every body is measured JSON-ESCAPED (the
/// form it takes inside the card's `content` string, each newline and quote
/// costing its extra byte) and with the dynamic fence [`fenced_overhead`]
/// charges for a body full of backticks. Kept beside the render so the
/// estimate cannot charge a body the panel never writes, and a diff is
/// charged under the SAME cap it renders under, never the old 3,000-character
/// preview (#792).
///
/// The boundary #792's acceptance line names: a diff that already fit the old
/// 3,000-character preview renders exactly as it did before and is charged at
/// its rendered size — the clip is a no-op, so nothing grows. Only a diff the
/// byte budget newly reveals (over 3,000 characters but inside
/// [`DIFF_MAX_ESCAPED_BYTES`]) is charged larger than the old preview charge,
/// because the card now carries those bytes. Ordinary `read` panels
/// keep their 3,000-character budget and their old charge untouched.
pub(crate) fn panel_output_estimate(tool: &ToolPanel) -> usize {
    // A Code Mode program's nested-call rows and result are assembled from the
    // shape, not from the call's text output: charge the body the panel writes.
    // Its progress header is the folded title's (`title_details`), not part of
    // this body charge.
    if is_program(tool) {
        return program_output(tool)
            .map(|(_, style, body)| assembled_body_estimate(None, style, &body))
            .unwrap_or(0);
    }
    // A FileEdits panel composes markdown: action lines outside the fence and
    // hunks inside it, already clipped at the diff byte budget. Charge the
    // header plus the body exactly as `CardMarkdown` serializes it.
    if let Some(body) = file_edits_body(tool) {
        let header = body
            .header
            .as_deref()
            .map(|header| json_escaped_len(&CardMarkdown::new().element(header)))
            .unwrap_or(0);
        return header + serialized_len(&body.render().0);
    }
    // A Codex search's or collaboration call's body is assembled from metadata,
    // not from the call's (empty) text output, so it is charged from the same
    // derivation the panel renders — header included, and the raw fallback's
    // `json` fence tag, which the generic branch's constant does not carry.
    if let Some((header, style, body)) = codex_web_search_output(tool).or_else(|| codex_collab_output(tool)) {
        return assembled_body_estimate(header.as_deref(), style, &body);
    }
    // Everything else — a `Command` included — is its output text, clipped at
    // the preview cap exactly as the render clips it.
    tool.output()
        .map(|body| {
            let body = truncate_md(&body, TOOL_OUTPUT_MAX_CHARS);
            json_escaped_len(&body) + fenced_overhead(&body)
        })
        .unwrap_or(0)
}

/// One tool panel as a folded collapsible element. `at_ms` is the part's
/// server `state.time.start`; when present, the panel header carries it as
/// `· HH:MM` so the start time is visible while collapsed (#183), and when
/// absent (a payload with no server time) no clock is rendered. A panel with a
/// File Content the card does NOT show — one sent as a File Message or left
/// undelivered — appends the compact file marker [`file_header_marker`] so the
/// file — and a `未发送` one above all — is visible while folded (ADR-0076
/// amendment); an embedded image, visible beside the panel, adds none. The
/// panel's own file also joins the header [`panel_file_detail`] (ADR-0083 §8
/// amendment, #766). `element_id` is the panel's stable identity on the card
/// (see [`super::shell::collapsible_panel_chunks`]). `session_id` routes the
/// panel's 「📂 打开」 button, which renders inside the fold — a `write`/`edit`
/// panel's way into the file it produced (ADR-0083 §8 amendment, #766), and a
/// file `read`'s way into the file it only previewed, whose clipped body says
/// so beside it (ADR-0083 §8 amendment, #770).
pub(super) fn tool_panel_element(
    tool: &ToolPanel,
    at_ms: Option<i64>,
    element_id: Option<&str>,
    session_id: Option<&str>,
    md: &mut CardMarkdown,
) -> serde_json::Value {
    // A `Program` assembles its body from its nested-call list, and a `write`
    // renders its own `content` input, so those have their own builders; every
    // other tool's body is its output text through the per-tool formatter (see
    // [`panel_output`]). The result-count header (glob/grep) is metadata-side,
    // so it is applied over either path.
    let output = panel_output(tool);
    let output = output.map(|(header, style, body)| (search_count_header(tool).or(header), style, body));
    // The plan panel is a status section, not a transcript: a parsed list is
    // the panel, so the generic Input line and Output marker would only frame
    // the checklist (a still-running call has no parsed output yet — the Input
    // line `📋 共 N 项任务` is all it can show), and its prefix names the
    // section rather than the call (a finished call would sit at a permanent ✅
    // while the counts right beside it still report open items).
    let todo_panel = tool.is_plan();
    let todowrite_list = todo_panel
        && output
            .as_ref()
            .is_some_and(|(_, style, _)| *style == BodyStyle::Markdown);
    let mut content = String::new();
    // An empty Input line renders no section, for every shape: the Execute
    // family's own line comes from its detail (which may name a command the raw
    // payload does not), and every other tool's from its input.
    if !todowrite_list {
        let formatted = format_tool_input(tool);
        if !formatted.is_empty() {
            // A program is the exception to the 400-char input budget: its
            // code is the panel, so it gets the output budget.
            let budget = if is_program(tool) {
                TOOL_OUTPUT_MAX_CHARS
            } else {
                400
            };
            // Trailing blank line so a multi-line input (edit diff, skill
            // metadata list) can't swallow the Output section as a markdown
            // lazy continuation of its last list line — Feishu would render
            // the output label glued to the last input line.
            content.push_str(&format!(
                "**{INPUT_LABEL}**\n{}\n\n",
                truncate_md(&formatted, budget)
            ));
        }
    }
    let mut title_details: Option<String> = None;
    if let Some((header, style, body)) = output {
        if todowrite_list {
            // The header is the panel's progress line, not a body intro; the
            // checklist is markdown by construction (status icons,
            // strikethrough), so it must not be fenced into literal `- ✅ …`
            // rows even when a task text is long.
            title_details = header;
            content.push_str(&truncate_md(&body, TOOL_OUTPUT_MAX_CHARS));
        } else {
            // The Output marker owns its own paragraph: without the blank line any
            // following line that Feishu reads as a block start that cannot
            // interrupt a paragraph (a Setext underline, an indented line) makes
            // the marker a lazy continuation — and a Setext underline even turns
            // the whole run above it, marker included, into one heading
            // (`## Output99- …`), glued together.
            content.push_str(&format!("**{OUTPUT_LABEL}**\n\n"));
            if is_program(tool) {
                // The call count is the folded panel's progress line; the body
                // leads with the nested call rows themselves.
                title_details = header;
            } else if let Some(h) = &header {
                content.push_str(&format!("{}\n\n", h));
            }
            // A file-editing diff is capped by its own BYTE budget (#792), and
            // clips bare: the `…` immediately before the closing fence stops
            // Feishu rendering the block as code at all (measured, #788), and
            // the note below already says it was clipped — the only signal a
            // clipped diff has. A `read` preview keeps its 3,000-CHAR budget
            // and `truncate_md`'s marker (or `clip_md` when it carries the
            // way-in note), exactly as before.
            let note = truncation_note(tool, session_id);
            if is_command(tool) {
                // A `Command`'s body is the shared command output block
                // (ADR-0105, ADR-0086's "one render block, never a lifecycle"):
                // the same clip and fence the Host Terminal Card puts its log
                // through. No diff note belongs to a command.
                let clip = OutputClip::Chars {
                    max_chars: TOOL_OUTPUT_MAX_CHARS,
                };
                content.push_str(&command_output_block(&body, clip, false).0);
            } else {
                let body = if has_file_edits(tool) && !call_failed(tool) {
                    // Already composed and clipped by `file_edits_body`.
                    body
                } else if note.is_some() && matches!(style, BodyStyle::Code(_)) {
                    clip_md(&body, TOOL_OUTPUT_MAX_CHARS)
                } else {
                    truncate_md(&body, TOOL_OUTPUT_MAX_CHARS)
                };
                // File content (read) and FileEdits hunks always render as a
                // fenced code block; a `Markdown` body (websearch results) never
                // does; anything else fences when Feishu markdown would wrap a long
                // line or fold lines above a Setext underline into one heading.
                match style {
                    BodyStyle::Code(lang) => content.push_str(&fenced_code(&body, lang)),
                    BodyStyle::Markdown => content.push_str(&body),
                    BodyStyle::Auto if needs_code_block(&body) => {
                        content.push_str(&fenced_code(&body, None));
                    }
                    BodyStyle::Auto => content.push_str(&body),
                }
                // A clipped `read` body says so, naming the button above it
                // (ADR-0083 §8 amendment, #770). Only a read gets it: it is the
                // panel whose clip hides a file the viewer can show whole, and
                // the note appears exactly when that button does.
                if let Some(note) = note {
                    content.push_str(&format!("\n\n{note}"));
                }
            }
        }
    }
    // The File Contents' tracking lines (ADR-0076) close the panel body: the
    // file's own line plus its delivery state, so a Tool Panel that read a file
    // names it instead of dropping it.
    if let Some(files) = file_tracking_block(tool) {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str(&files);
    }
    if content.is_empty() {
        content = "_(no details)_".to_string();
    }
    let content = md.element(&content);
    let icon = if todo_panel { "📋" } else { tool.status_icon() };
    let mut title = format!("{icon} {}{}", tool_display_name(tool), panel_time_suffix(at_ms));
    if let Some(details) = &title_details {
        title.push_str(&format!(" · {}", details));
    }
    // The file the call is about, visible while the panel is folded
    // (ADR-0083 §8 amendment, #766): its Input line is inside the fold, so a
    // collapsed `write`/`edit` — and a text `read` — would otherwise never name
    // the file it touched.
    if let Some(detail) = panel_file_detail(tool) {
        title.push_str(&detail);
    }
    // A collaboration panel's folded header names its target agent(s) — the
    // body that lists them is collapsed (issue #962).
    if let Some(detail) = panel_target_detail(tool) {
        title.push_str(&detail);
    }
    // A live task call carries its child session's liveness in the title, so
    // the line stays readable while the panel is folded (ADR-0054).
    if tool.is_live()
        && let Some(liveness) = tool.liveness()
    {
        title.push_str(&format!(
            " · {}",
            liveness.title_fragment(chrono::Utc::now().timestamp_millis())
        ));
    }
    // The File Contents' compact marker closes the header (ADR-0076 amendment):
    // the detailed tracking line is in the body, which is folded by default, so
    // the header names the file — and above all its `未发送` state — while the
    // panel stays collapsed.
    if let Some(marker) = file_header_marker(tool) {
        title.push_str(&marker);
    }
    let mut panel = collapsible_panel(&title, &content, element_id);
    // The way into the file lives INSIDE the fold (ADR-0083 §8 amendment,
    // #766): a full-width button after every `write`/`edit` panel crowded the
    // chat, above all on a phone. Opening the file now takes expanding the
    // panel first, accepted because the folded header already names it. It is
    // the body's FIRST element so a long diff — or a clipped `read` body
    // (ADR-0083 §8 amendment, #770) — cannot push it out of view.
    if let Some(button) = tool_open_button_element(tool, session_id)
        && let Some(elements) = panel
            .get_mut("elements")
            .and_then(serde_json::Value::as_array_mut)
    {
        elements.insert(0, button);
    }
    panel
}

/// The card-tail Todo Panel. Its neutral list is the whole presentation
/// contract, so this renderer never depends on a synthetic ToolCall.
pub(super) fn todo_panel_element(
    todo: &TodoPanel,
    at_ms: Option<i64>,
    element_id: Option<&str>,
    session_id: Option<&str>,
    md: &mut CardMarkdown,
) -> serde_json::Value {
    match todo {
        TodoPanel::List { todo, .. } => {
            let details = format!("共 {} 项 · {}", todo.steps.len(), todo_counts(&todo.steps));
            let body = truncate_md(&format_todo_list_with_explanation(todo), TOOL_OUTPUT_MAX_CHARS);
            let content = md.element(&body);
            let title = format!(
                "📋 {}{} · {}",
                vocab::TOOL_PLAN,
                panel_time_suffix(at_ms),
                details
            );
            collapsible_panel(&title, &content, element_id)
        }
        TodoPanel::Tool(panel) => tool_panel_element(panel, at_ms, element_id, session_id, md),
    }
}

/// The exact tail element's serialized size, used by the card splitter's
/// tail reserve. It passes the same clock, identity and Session the build
/// renders, and includes a malformed fallback panel's embedded images, so the
/// estimate and the built elements cannot drift.
pub(crate) fn todo_panel_estimate(
    todo: &TodoPanel,
    at_ms: Option<i64>,
    element_id: Option<&str>,
    session_id: Option<&str>,
) -> usize {
    let element = todo_panel_element(todo, at_ms, element_id, session_id, &mut CardMarkdown::new());
    let mut size = serialized_len(&element.to_string());
    if let TodoPanel::Tool(panel) = todo {
        for image in file_image_elements(panel) {
            size += serialized_len(&image.to_string());
        }
    }
    size
}

/// The meaningful parts of a unified file diff (a file-editing tool's recorded
/// diff, or an edit permission request's `diff`): the target path, the hunk
/// lines the producer wrote (`@@`, context, `-`, `+`) and the counted change.
/// The hunks keep their context lines byte-for-byte: upstream's
/// `createTwoFilesPatch` emits the whole file with at most four context lines
/// per side, so the context is already bounded — and it is what puts a change
/// in its place and says whether neighbouring changes are one run or two. Only
/// the header block before a hunk (`Index:`, `===`, `diff --git`, `index`,
/// `---`, `+++`) is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditDiff {
    pub path: Option<String>,
    /// Every file the diff names, in order (an `Index:` line, else the
    /// `---`/`+++` header). A V2 `edit` patch has no `Index:`, so the header is
    /// the only place its target is named; the list is what lets a multi-file
    /// patch agree on one language.
    pub paths: Vec<String>,
    pub additions: usize,
    pub deletions: usize,
    pub body: String,
    /// Non-diff text after the hunks: the tool's trailing note (its
    /// "LSP errors detected …" diagnostics), which the bridge appends after
    /// the diff. Kept out of `body` so a permission description still shows
    /// just the change, but shown after the fenced hunks on a Tool Panel.
    pub tail: String,
}

/// Parse a unified diff produced by OpenCode's `edit` tool into its hunks.
/// Returns `None` when `diff` isn't a recognizable edit diff (plain text
/// output, an error message), so callers fall back to showing it unchanged.
pub(crate) fn parse_edit_diff(diff: &str) -> Option<EditDiff> {
    parse_diff(diff, false)
}

/// Scan a unified diff into an [`EditDiff`]: the file headers name the target,
/// and each hunk's own lines — `@@`, ` ` context, `-`, `+` — are copied
/// **verbatim**. No `trim_end` runs, because a context line's leading space is
/// diff syntax and its trailing spaces are file content. A truly empty line
/// inside a hunk starts the tool's trailing note (a real blank context line is
/// the single space `" "`); a joined patch's blank line between files is a
/// boundary instead — see [`opens_file_block`].
fn parse_diff(diff: &str, keep_file_headers: bool) -> Option<EditDiff> {
    let mut path = None;
    let mut paths: Vec<String> = Vec::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    let mut body = String::new();
    let mut tail = String::new();
    let mut changed = false;
    // The `--- path` / `+++ path` file headers only appear BEFORE the first
    // `@@` hunk. Past that point any `-`/`+` line is real content — a removed
    // line whose text starts with `-- ` (Lua/YAML `--` comments) renders as
    // `--- …` and must stay a removal, not be mistaken for a header.
    let mut in_hunk = false;
    // Once the tool's trailing note starts (the bridge appends the LSP
    // diagnostics after a blank line), everything after it is that note — even
    // a line that happens to start like diff syntax (a diagnostic quoting
    // `+`/`-` code).
    let mut in_tail = false;
    let lines: Vec<&str> = diff.lines().collect();
    for (index, raw) in lines.iter().enumerate() {
        // Only a CRLF's carriage return is dropped: `lines()` already splits
        // CRLF, and this keeps a stray trailing `\r` out of the body. A
        // context line's own trailing spaces are content, so no `trim_end`.
        let l = raw.strip_suffix('\r').unwrap_or(raw);
        if in_tail {
            tail.push_str(l);
            tail.push('\n');
            continue;
        }
        if in_hunk && l.is_empty() && !opens_file_block(&lines, index + 1) {
            // A unified hunk cannot hold a truly empty line: a blank source
            // line is the context line `" "`. So a real empty line is the
            // note's boundary — unless it separates two files of a joined
            // patch (`diff_from_metadata` joins per-file strings that end in `\n`
            // with `\n`), where the next line opens the next file.
            in_tail = true;
            continue;
        }
        if let Some(p) = l.strip_prefix("Index: ") {
            path = Some(p.to_string());
            // A new `Index:` starts a new file's header block even after an
            // earlier file's hunks (a concatenated multi-file patch), so its
            // `---`/`+++` lines are headers again, not content.
            in_hunk = false;
            if !paths.iter().any(|seen| seen == p) {
                paths.push(p.to_string());
            }
            if keep_file_headers {
                body.push_str(l);
                body.push('\n');
            }
        } else if l.starts_with("===") {
            // A concatenated patch's file boundary. V2's `edit` metadata joins
            // per-file diffs that carry no `Index:` — the `====` separator is
            // the only thing that says the next `---`/`+++` pair is a header
            // again, not `+`/`-` hunk content.
            in_hunk = false;
        } else if l.starts_with("diff --git ") {
            // `a/old b/new` — the explicit Index/filepath carries the target.
            // A joined patch's next file starts here, so this ends the previous
            // hunk (a real hunk line always starts with `+`, `-`, or a space,
            // so this marker can never be content).
            in_hunk = false;
        } else if l.starts_with("index ") {
            // A Git patch's `index <old>..<new> <mode>` line sits between the
            // `diff --git` marker and the `---`/`+++` pair. It names no file
            // (the headers do) and can never be hunk content, so it is header
            // noise, and it also ends the previous hunk of a joined patch —
            // without this branch it starts the tail and swallows the file.
            in_hunk = false;
        } else if !in_hunk && (l.starts_with("--- ") || l.starts_with("+++ ")) {
            // The old/new file headers of a unified diff. A V2 `edit` patch has
            // no `Index:`, so the header is the only place its target is named.
            if let Some(p) = diff_header_path(l) {
                if path.is_none() {
                    path = Some(p.clone());
                }
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        } else if !in_hunk && (l == "---" || l == "+++") {
            // A bare header line with no path.
        } else if l.starts_with("@@") {
            in_hunk = true;
            changed = true;
            body.push_str(l);
            body.push('\n');
        } else if l.starts_with('+') {
            additions += 1;
            changed = true;
            body.push_str(l);
            body.push('\n');
        } else if l.starts_with('-') {
            deletions += 1;
            changed = true;
            body.push_str(l);
            body.push('\n');
        } else if in_hunk && l.starts_with(' ') {
            // An unchanged context line: copied byte-for-byte, leading space
            // included. It is what places the change in the file.
            body.push_str(l);
            body.push('\n');
        } else if !l.is_empty() && !l.starts_with(' ') && !l.starts_with('\\') {
            // Not diff syntax and not an (indented) context line: the start of
            // the trailing note. Blank lines and the `\ No newline…` marker are
            // not it. An indented note line reaches here only after the blank
            // boundary above already set `in_tail`.
            in_tail = true;
            tail.push_str(l);
            tail.push('\n');
        }
        // Anything else (a pre-hunk blank line, "\ No newline…") is dropped.
    }
    if !changed {
        return None;
    }
    Some(EditDiff {
        path,
        paths,
        additions,
        deletions,
        body,
        tail,
    })
}

/// Whether the next non-empty line at or after `from` opens another file's
/// header block in a concatenated patch. [`diff_from_metadata`] joins per-file
/// patch strings — each ending in `\n` — with `\n`, so a joined multi-file
/// patch carries a real blank line between files. That line is a file
/// boundary, not the trailing note's start; reading it as one would swallow
/// every file after the first into [`EditDiff::tail`].
///
/// The evidence is the opener **and** the header line after it: a real block
/// spells out `===` / `index` / `---` / `+++` next, where a note that merely
/// quotes a line starting with `Index: …` does not. Requiring both keeps such
/// a note in `tail`, as the spec's blank-line boundary demands.
///
/// One shape is accepted as ambiguous rather than guessed at: a note that opens
/// with a **complete** synthetic file block (`Index: …` followed by `---`/`+++`,
/// or a whole quoted Git diff) is textually identical to a real second file.
/// The bridge appends a tail after the whole diff and no tool note opens with a
/// full file-block shape today, so this check reads it as a file; separating
/// the two would need the producer's intent, which the text does not carry.
///
/// One rule for a joined multi-file patch — and for a tool's trailing note —
/// so the panel cannot read the same text two ways.
pub(crate) fn opens_file_block(lines: &[&str], from: usize) -> bool {
    let mut block = lines
        .get(from..)
        .unwrap_or_default()
        .iter()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.is_empty());
    let Some(opener) = block.next() else {
        return false;
    };
    if !(opener.starts_with("Index: ") || opener.starts_with("===") || opener.starts_with("diff --git ")) {
        return false;
    }
    block.next().is_some_and(|line| {
        line.starts_with("===")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
    })
}

/// The file a `---`/`+++` header names, without a git `a/`/`b/` prefix or a
/// trailing timestamp. `None` for `/dev/null` (an add/delete's empty side) and
/// a pathless header.
pub(crate) fn diff_header_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("--- ").or_else(|| line.strip_prefix("+++ "))?;
    let path = rest.split('\t').next().unwrap_or(rest).trim();
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    (!path.is_empty() && path != "/dev/null").then(|| path.to_string())
}

/// [`diff_language_for`] over an explicit path list: the one language every path
/// shares, [`PLAIN_TEXT`] when they disagree or when the list is empty.
///
/// This is the Tool Panel's rule for a patch that may touch several files. The
/// permission card does not use it: #794 gives that surface a single selected
/// path (metadata `filepath`, else the diff's path, else its first pattern), so
/// its fence follows exactly that file rather than an agreement across all of
/// them.
pub(crate) fn diff_language_for<'a>(paths: impl IntoIterator<Item = &'a str>) -> &'static str {
    let mut langs = paths.into_iter().map(language_for_path);
    let Some(first) = langs.next() else {
        return PLAIN_TEXT;
    };
    if langs.any(|lang| lang != first) {
        PLAIN_TEXT
    } else {
        first
    }
}

/// Parse a todo payload into the neutral whole-list shape. OpenCode's input is
/// `{"todos":[…]}` and its output is the same array JSON-encoded; Codex's
/// adapter passes the same shape with an optional explanation. Priority is
/// deliberately dropped: it rarely varies within one list and would mark almost
/// every row without adding signal.
fn parse_todo_list(value: &serde_json::Value) -> Option<TodoList> {
    let arr = match value {
        serde_json::Value::Array(arr) => arr,
        serde_json::Value::Object(obj) => obj.get("todos")?.as_array()?,
        _ => return None,
    };
    let mut steps = Vec::new();
    for v in arr {
        // A list with one malformed row falls back to the raw payload as a
        // whole: rendering the readable rows and silently dropping the rest
        // would lose content the model wrote.
        let content = v.get("content")?.as_str()?;
        if content.is_empty() {
            return None;
        }
        let status = match v.get("status").and_then(|s| s.as_str()).unwrap_or("pending") {
            "pending" => TodoStatus::Pending,
            "in_progress" | "inProgress" => TodoStatus::InProgress,
            "completed" => TodoStatus::Completed,
            "cancelled" => TodoStatus::Cancelled,
            other => TodoStatus::Other(other.to_string()),
        };
        steps.push(TodoStep {
            text: content.to_string(),
            status,
        });
    }
    Some(TodoList {
        explanation: value
            .get("explanation")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        steps,
    })
}

/// Parse a todo payload's non-empty item list. Anything malformed, empty or
/// placeholder-shaped returns `None`, and the caller falls back to raw output.
fn parse_todos(value: &serde_json::Value) -> Option<Vec<TodoStep>> {
    let list = parse_todo_list(value)?;
    (!list.steps.is_empty()).then_some(list.steps)
}

/// How one todo status renders — its icon and its row markup. The counts line
/// reads the table in this order (active first), so this is the one place the
/// status vocabulary lives. Unknown statuses read as pending.
#[derive(Clone, Copy)]
enum TodoMarkup {
    Plain,
    Bold,
    Strike,
}

const TODO_STATUSES: [(&str, &str, TodoMarkup); 4] = [
    ("in_progress", "🔄", TodoMarkup::Bold),
    ("pending", "⬜", TodoMarkup::Plain),
    ("completed", "✅", TodoMarkup::Strike),
    ("cancelled", "🚫", TodoMarkup::Strike),
];

fn todo_style(status: &TodoStatus) -> (&'static str, TodoMarkup) {
    // An unseen wire status keeps its row and renders as pending — never as a
    // known status whose spelling it happens to share (`cancelled` from a
    // backend that does not document it, for example).
    if matches!(status, TodoStatus::Other(_)) {
        return ("⬜", TodoMarkup::Plain);
    }
    let status = status.as_str();
    TODO_STATUSES
        .iter()
        .find(|(s, _, _)| *s == status)
        .map(|(_, icon, markup)| (*icon, *markup))
        .unwrap_or(("⬜", TodoMarkup::Plain))
}

/// The header of a todo panel: one `icon count` group per non-empty status, so
/// the folded panel still answers "how far along is this?" without unfolding.
fn todo_counts(todos: &[TodoStep]) -> String {
    TODO_STATUSES
        .iter()
        .filter_map(|(status, icon, _)| {
            let n = todos
                .iter()
                .filter(|t| {
                    // An unseen status counts under pending, matching the row
                    // icon `todo_style` picks.
                    match &t.status {
                        TodoStatus::Other(_) => *status == "pending",
                        known => known.as_str() == *status,
                    }
                })
                .count();
            (n > 0).then(|| format!("{icon} {n}"))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The checklist body of a todo panel: one `- <icon> content` line per item,
/// in the order the agent wrote them (that order carries the plan's sequence).
/// Finished and dropped items are struck through so the eye can skip them; the
/// in-progress item is bolded — it is the only actionable row.
fn format_todo_list(todos: &[TodoStep]) -> String {
    todos
        .iter()
        .map(|t| {
            let text = first_chunk(&t.text, 120);
            let (icon, markup) = todo_style(&t.status);
            match markup {
                TodoMarkup::Bold => format!("- {icon} **{text}**"),
                TodoMarkup::Strike => format!("- {icon} ~~{text}~~"),
                TodoMarkup::Plain => format!("- {icon} {text}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_todo_list_with_explanation(todo: &TodoList) -> String {
    let list = format_todo_list(&todo.steps);
    match todo.explanation.as_deref() {
        Some(explanation) if !explanation.is_empty() => format!("{explanation}\n\n{list}"),
        _ => list,
    }
}

/// Render a tool's raw output string where the neutral shape says it has one.
/// A **Read** output is OpenCode's envelope — V1's XML wrapper
/// (`<path>…</path>`, `<type>…</type>`, `<content>…</content>`) or V2's header
/// line over numbered lines — so the card shows the file's path and lines
/// rather than the wrapper. A **Search** with web scope is a result envelope —
/// render it as a title+url list. A launch keeps its own envelope
/// ([`delegation_output`]); a tool with no typed detail keeps its renderer (a
/// `skill` envelope); a plan call reads the neutral `TodoList` fact
/// ([`plan_output`]); everything else passes through unchanged.
///
/// Returns `(header, body style, body)`: the header is the short markdown line
/// above the body (file path, change count, result count); the style decides
/// how the body blocks (see [`BodyStyle`]).
fn format_tool_output(tool: &ToolPanel, output: &str) -> (Option<String>, BodyStyle, String) {
    if tool.is_plan() {
        return plan_output(output);
    }
    // A Codex `imageView` is a `Read` in the neutral model but keeps P1's
    // presentation (its panel is not a text preview): show its output raw.
    if is_codex_image(tool) {
        return (None, BodyStyle::Auto, output.to_string());
    }
    if read_detail(tool).is_some() {
        return read_output(output);
    }
    if search_detail(tool).is_some_and(|search| search.scope == SearchScope::Web) {
        return web_output(output);
    }
    if is_launch_call(tool) {
        return delegation_output(output);
    }
    format_named_output(tool.name(), output)
}

/// A **web search**'s raw output: V1 emits a JSON envelope (`parallel`) or
/// plain-text blocks (`exa`); V2 emits markdown result blocks. Each parser
/// shape-gates itself, so an unrecognized provider output still passes through
/// unchanged.
fn web_output(output: &str) -> (Option<String>, BodyStyle, String) {
    match parse_websearch_results(output) {
        Some((count, body)) => (Some(format!("🔎 {} 条结果", count)), BodyStyle::Markdown, body),
        None => (None, BodyStyle::Auto, output.to_string()),
    }
}

/// Render a tool's raw output for the ids whose ENVELOPE is vendor-specific
/// and has no shared shape: a `skill` envelope. A launch's envelope is
/// [`delegation_output`]'s, selected by the neutral detail.
fn format_named_output(name: &str, output: &str) -> (Option<String>, BodyStyle, String) {
    if name == "skill"
        && output.starts_with("<skill_content ")
        && let Some(body) = parse_skill_envelope(output)
    {
        return (None, BodyStyle::Auto, body);
    }
    (None, BodyStyle::Auto, output.to_string())
}

/// Whether a call launches a child run, selected by its neutral detail — never
/// its vendor name (ADR-0105 §5). A third adapter's launch under a new id takes
/// the same panel.
fn is_launch_call(tool: &ToolPanel) -> bool {
    matches!(
        &tool.call().detail,
        ToolDetail::Delegation {
            effect: DelegationEffect::Launch,
            ..
        }
    )
}

/// A launch's raw output: V1's `<task …>` envelope (whose `<summary>` can
/// become the header) or V2's `<subagent …>` envelope. The marker, not the
/// identity name, picks the parser.
fn delegation_output(output: &str) -> (Option<String>, BodyStyle, String) {
    if output.starts_with("<task ")
        && let Some((header, body)) = parse_task_envelope(output)
    {
        return (header, BodyStyle::Auto, body);
    }
    if output.starts_with("<subagent ")
        && let Some(body) = parse_subagent_envelope(output)
    {
        return (None, BodyStyle::Auto, body);
    }
    (None, BodyStyle::Auto, output.to_string())
}

/// A launch's Input line: its description and the child agent's type. V1 names
/// the type `subagent_type`, V2 `agent`; both are payload data, read the same
/// way whichever identity name the adapter chose.
fn delegation_input(input: &serde_json::Value) -> String {
    let get = |key: &str| input.get(key).and_then(serde_json::Value::as_str);
    let desc = get("description").unwrap_or("子任务");
    let sub = get("agent")
        .or_else(|| get("subagent_type"))
        .map(|s| format!("\n🤖 `{}`", s))
        .unwrap_or_default();
    format!("🔀 {}{}", desc, sub)
}

/// The plan surface's output: the neutral [`TodoList`] parsed from the call's
/// output, rendered as the iconed checklist with its count header. A payload
/// that does not parse passes through unchanged rather than being dropped.
fn plan_output(output: &str) -> (Option<String>, BodyStyle, String) {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(output)
        && let Some(todo) = parse_todo_list(&value)
        && !todo.is_empty()
    {
        return (
            Some(format!(
                "共 {} 项 · {}",
                todo.steps.len(),
                todo_counts(&todo.steps)
            )),
            BodyStyle::Markdown,
            format_todo_list_with_explanation(&todo),
        );
    }
    (None, BodyStyle::Auto, output.to_string())
}

/// A **Read** tool's raw output, whichever generation wrote it. V1's output
/// *starts* with the `<path>…</path>` wrapper; V2's carries no wrapper at all.
/// The test is on the **head**, not the whole output: a V2 read whose body
/// merely *mentions* the literal `<path>` (any file quoting it, e.g. a README
/// documenting `--config <path>`) must still take the V2 branch below, or its
/// path header, language token and first body line are all lost.
fn read_output(output: &str) -> (Option<String>, BodyStyle, String) {
    let v1_read_wrapper = output.trim_start().starts_with("<path>");
    if !v1_read_wrapper {
        // V2's read output carries no XML wrapper: a header line over numbered
        // lines or a directory listing. Anything else falls through to raw.
        if let Some(parsed) = parse_v2_read_output(output) {
            return parsed;
        }
        return (None, BodyStyle::Auto, output.to_string());
    }
    let mut path = String::new();
    let mut body = String::new();
    for line in output.lines() {
        let l = line.trim_end();
        if l.starts_with("<path>") && l.ends_with("</path>") {
            path = l
                .trim_start_matches("<path>")
                .trim_end_matches("</path>")
                .to_string();
        } else if l.trim() == "<content>"
            || l.trim() == "</content>"
            || l.trim() == "<type>file</type>"
            || l.trim() == V1_DIRECTORY_TYPE_TAG
        {
            // Skip the wrapper tags.
        } else if !body.is_empty() || !line.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(line);
        }
    }
    if body.trim().is_empty() {
        // Nothing usable parsed — show the raw output so info isn't lost.
        return (None, BodyStyle::Auto, output.to_string());
    }
    (
        Some(format!("📄 `{}`", path)),
        BodyStyle::Code(Some(language_for_path(&path))),
        body,
    )
}

/// The result-count header V2 records in a search call's metadata (`count` for
/// a filename search, `matches` for a content search, plus its `truncated`
/// flag). `None` when the shape is not one of those or the call carries no
/// count — a generation that doesn't record it, or a third-party tool that
/// merely claims an id — so the panel keeps its raw shape.
fn search_count_header(tool: &ToolPanel) -> Option<String> {
    let search = search_detail(tool)?;
    let (count, unit) = match search.scope {
        SearchScope::FileName => (search.matches?, "个文件"),
        SearchScope::Content => (search.matches?, "处匹配"),
        SearchScope::Web => return None,
    };
    let truncated = tool
        .metadata()
        .and_then(|metadata| metadata.get("truncated"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    Some(if truncated {
        format!("{count} {unit} · 已截断")
    } else {
        format!("{count} {unit}")
    })
}

/// The `Program` panel's output (the execute family's Code Mode shape): one
/// status row per nested tool call the program made — the shape records them
/// live, in start order, while the program runs — followed by the formatted
/// result. The header is the pinned title's progress line — the call count, the
/// nested failures and the program's OWN failure — which the panel promotes to
/// its folded title (`title_details`). `None` when there is neither a nested
/// call nor a result, so the panel falls back to its input alone.
fn program_output(tool: &ToolPanel) -> Option<(Option<String>, BodyStyle, String)> {
    let Some(ExecuteShape::Program { calls, error, .. }) = ExecuteShape::of(tool) else {
        return None;
    };
    let result = tool.output();
    if calls.is_empty() && result.is_none() {
        return None;
    }
    let failures = calls.iter().filter(|call| call.status.is_failure()).count();
    // Every fact the ❌ icon stands on is named beside it: a clean call count
    // under a failed program would read as a contradiction. The program's own
    // error is not a nested call's failure, so it gets its own segment.
    let mut segments = Vec::new();
    if !calls.is_empty() {
        segments.push(format!("{} 个工具调用", calls.len()));
        if failures > 0 {
            segments.push(format!("{failures} 失败"));
        }
    }
    if error {
        segments.push("程序出错".to_string());
    }
    let header = (!segments.is_empty()).then(|| segments.join(" · "));
    let mut body = calls
        .iter()
        .map(|call| format!("- {} `{}`", nested_status_icon(&call.status), call.name))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(result) = result.filter(|result| !result.is_empty()) {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        // The result is often JSON; fence it when the generic long-line rules
        // would wrap or fold it. Truncation happens before fencing so the
        // panel's own budget clamp cannot leave an unclosed fence.
        let result = truncate_md(&result, TOOL_OUTPUT_MAX_CHARS - 256);
        if needs_code_block(&result) {
            body.push_str(&fenced_code(&result, None));
        } else {
            body.push_str(&result);
        }
    }
    Some((header, BodyStyle::Markdown, body))
}

/// The icon for one nested Code Mode call's recorded status — the same
/// vocabulary the nested tool's own panel would show.
fn nested_status_icon(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending | ToolStatus::Running => "⏳",
        ToolStatus::Completed => "✅",
        status if status.is_failure() => "❌",
        _ => "🔧",
    }
}

/// The raw lines inside an XML-envelope output — the shape `task` and `skill`
/// produce: verifies the first line opens the envelope (`<tag …>`) and yields
/// every following line up to the matching close tag. `None` when the first
/// line doesn't open it, so the caller shows the output unchanged.
fn envelope_lines<'a>(
    output: &'a str,
    open_prefix: &str,
    close_tag: &str,
) -> Option<impl Iterator<Item = &'a str>> {
    let first = output.lines().next()?;
    if !first.starts_with(open_prefix) || !first.ends_with('>') {
        return None;
    }
    Some(
        output
            .lines()
            .skip(1)
            .take_while(move |line| line.trim() != close_tag),
    )
}

/// Strip a `task` tool's XML envelope: `<task id state>` around an optional
/// `<summary>` and the `<task_result>`/`<task_error>` body. Returns
/// `(summary as header, body text)`; `None` when the shape doesn't match (an
/// error text, a legacy payload), so the caller shows the output unchanged.
fn parse_task_envelope(output: &str) -> Option<(Option<String>, String)> {
    let mut summary = None;
    let mut body = String::new();
    let mut in_body = false;
    for line in envelope_lines(output, "<task ", "</task>")? {
        let t = line.trim();
        if t.starts_with("<summary>") && t.ends_with("</summary>") {
            summary = Some(
                t.trim_start_matches("<summary>")
                    .trim_end_matches("</summary>")
                    .to_string(),
            );
        } else if t == "<task_result>" || t == "<task_error>" {
            in_body = true;
        } else if t == "</task_result>" || t == "</task_error>" {
            in_body = false;
        } else if in_body {
            body.push_str(line);
            body.push('\n');
        }
    }
    // The envelope stays stripped even when it carried no text: an empty
    // output is better than the raw XML back on the card.
    Some((summary, body.trim_end().to_string()))
}

/// Strip V2's `subagent` envelope: `<subagent sessionID state>` around the
/// child's final report. Unlike V1's `<task>`, the body is the whole envelope
/// (no `<summary>`/`<task_result>` inner tags). `None` when the shape doesn't
/// match (a failure text, the background handle's plain sentence), so the
/// caller shows the output unchanged.
fn parse_subagent_envelope(output: &str) -> Option<String> {
    let body = envelope_lines(output, "<subagent ", "</subagent>")?
        .collect::<Vec<_>>()
        .join("\n");
    Some(body.trim().to_string())
}

/// Strip a `skill` tool's XML envelope: `<skill_content name=…>` around the
/// skill's own markdown, dropping the sampled `<skill_files>` inventory (a
/// file list the reader can't use). `None` when the wrapper isn't there, so the
/// caller shows the output unchanged. Shared with the loaded-skill fold (spec
/// #652, ticket #655), which unwraps the same envelope from a user message's
/// attached skill.
pub(super) fn parse_skill_envelope(output: &str) -> Option<String> {
    let mut body = String::new();
    let mut in_files = false;
    for line in envelope_lines(output, "<skill_content ", "</skill_content>")? {
        let t = line.trim();
        if t == "<skill_files>" {
            in_files = true;
        } else if t == "</skill_files>" {
            in_files = false;
        } else if !in_files {
            body.push_str(line);
            body.push('\n');
        }
    }
    Some(body.trim_end().to_string())
}

/// Parse V2's `read` output — no XML wrapper, just a header line over the
/// content (`Read file <path>, lines N-M` / `Read directory <path>, entries
/// N-M`, or the `0 lines` / `0 entries` empty forms). The header line is
/// promoted to the panel header (`📄` for a file, `📁` for a directory) and the
/// rest renders as a code block (the directory listing without a language).
/// `None` when the first line isn't that shape, so the caller shows the output
/// unchanged.
fn parse_v2_read_output(output: &str) -> Option<(Option<String>, BodyStyle, String)> {
    let (first, body) = match output.split_once('\n') {
        Some((first, body)) => (first, body),
        None => (output, ""),
    };
    // The path may itself contain ", ", so the counted tail decides where it
    // ends (`lines 1-2` / `0 lines` / `entries 3-4` / `0 entries`).
    let (head, tail) = first.rsplit_once(", ")?;
    if !(tail.starts_with("lines ")
        || tail == "0 lines"
        || tail.starts_with("entries ")
        || tail == "0 entries")
    {
        return None;
    }
    let (icon, path) = if let Some(path) = head.strip_prefix("Read file ") {
        ("📄", path)
    } else {
        let path = head.strip_prefix(V2_DIRECTORY_HEADER_PREFIX)?;
        ("📁", path)
    };
    let style = if icon == "📄" {
        BodyStyle::Code(Some(language_for_path(path)))
    } else {
        BodyStyle::Code(None)
    };
    Some((Some(format!("{icon} `{path}`")), style, body.to_string()))
}

/// Parse a `websearch` tool's JSON result envelope into an un-fenced markdown
/// list: one `[title](url)` per result, with its publish date when present.
/// Excerpts are deliberately dropped — they are the model's reading material,
/// not the card's. `None` when the payload isn't the expected envelope (the
/// provider's no-results text, a different provider's shape), so the caller
/// shows it unchanged.
fn parse_websearch_results(output: &str) -> Option<(usize, String)> {
    let results = parse_parallel_results(output)
        .or_else(|| parse_exa_results(output))
        .or_else(|| parse_websearch_markdown(output))?;
    Some((results.len(), search_result_list(&results)))
}

/// One `n. [title](url)` line per search result, with its publish date when
/// present. The single list formatter every web-search shape shares — the
/// provider envelopes and Codex's `webSearch` results — so their panels cannot
/// disagree on numbering, the title clip, or a title-less entry.
fn search_result_list(results: &[SearchEntry]) -> String {
    results
        .iter()
        .enumerate()
        .map(|(i, (title, url, date))| {
            let title = first_chunk(title, 100);
            let mut line = match (title.is_empty(), url.is_empty()) {
                (false, false) => format!("{}. [{}]({})", i + 1, title, url),
                (false, true) => format!("{}. {}", i + 1, title),
                _ => format!("{}. {}", i + 1, url),
            };
            if let Some(date) = date.as_deref().filter(|d| !d.is_empty()) {
                line.push_str(&format!(" · {}", date));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Codex `webSearch` results as the same entries the providers give, when the
/// payload is the one shape the card can confirm: an array whose every entry
/// carries a non-empty `url` (an optional `title` names it). Any other value —
/// a different key, a missing url in one entry, a wrapped object — returns
/// `None`, so the caller shows it raw rather than guessing (the schema calls
/// `results` opaque JSON).
fn codex_search_entries(results: &serde_json::Value) -> Option<Vec<SearchEntry>> {
    let array = results.as_array()?;
    let mut entries = Vec::with_capacity(array.len());
    for result in array {
        let url = result.get("url").and_then(serde_json::Value::as_str)?.trim();
        if url.is_empty() {
            return None;
        }
        let title = result
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        entries.push((title.to_string(), url.to_string(), None));
    }
    Some(entries)
}

/// `(title, url, publish date)` of one websearch result, shared by both
/// provider parsers and the Codex results renderer.
type SearchEntry = (String, String, Option<String>);

/// The `parallel` provider's JSON envelope:
/// `{search_id, results: [{url, title, publish_date, excerpts}]}`.
fn parse_parallel_results(output: &str) -> Option<Vec<SearchEntry>> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    let results = value.get("results")?.as_array()?;
    if results.is_empty() {
        return None;
    }
    Some(
        results
            .iter()
            .map(|r| {
                let get = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                (get("title"), get("url"), Some(get("publish_date")))
            })
            .collect(),
    )
}

/// The `exa` provider's plain-text blocks — `Title:`, `URL:`, `Published:`,
/// `Author:`, then `Highlights:` and the excerpt — separated by `---`. `N/A`
/// stands in for every missing field. A chunk without a `URL` (an excerpt that
/// itself contains `---`) is dropped.
fn parse_exa_results(output: &str) -> Option<Vec<SearchEntry>> {
    if !output.starts_with("Title: ") {
        return None;
    }
    let mut results = Vec::new();
    for chunk in output.split("\n\n---\n\n") {
        let mut title = String::new();
        let mut url = String::new();
        let mut date = None;
        for line in chunk.lines() {
            if let Some(v) = line.trim().strip_prefix("Title: ") {
                title = v.trim().to_string();
            } else if let Some(v) = line.trim().strip_prefix("URL: ") {
                url = v.trim().to_string();
            } else if let Some(v) = line.trim().strip_prefix("Published: ") {
                date = Some(v.trim().to_string());
            } else if line.trim_start().starts_with("Highlights:") {
                // The excerpt is the model's reading material — stop here so a
                // quoted `Title:` line inside it is never read as a result.
                break;
            }
        }
        if url.is_empty() {
            continue;
        }
        let title = if title == "N/A" { String::new() } else { title };
        let date = date.filter(|d| !d.is_empty() && d != "N/A");
        results.push((title, url, date));
    }
    (!results.is_empty()).then_some(results)
}

/// V2's `websearch` output is markdown: each result is a `## [title](url)`
/// heading, an optional `Published:` line, then the excerpt. Parse the headings
/// into the same `(title, url, date)` entries and drop the excerpts (the
/// model's reading material, as with the V1 envelopes). A heading only counts
/// at a block start (the first line or after a blank line), so an excerpt that
/// itself quotes a `## …` line is not read as another result; `None` when the
/// shape doesn't match (the provider's no-results text), so the caller shows
/// the output unchanged.
fn parse_websearch_markdown(output: &str) -> Option<Vec<SearchEntry>> {
    let mut results: Vec<SearchEntry> = Vec::new();
    let mut previous_blank = true;
    for line in output.lines() {
        if previous_blank
            && let Some(rest) = line.strip_prefix("## [")
            && let Some((title, rest)) = rest.split_once("](")
            && let Some(url) = rest.strip_suffix(')')
            && !url.is_empty()
        {
            results.push((title.to_string(), url.to_string(), None));
            previous_blank = false;
            continue;
        }
        if let Some(date) = line.trim().strip_prefix("Published: ")
            && let Some(entry) = results.last_mut()
            && entry.2.is_none()
        {
            entry.2 = Some(date.trim().to_string());
        }
        previous_blank = line.trim().is_empty();
    }
    (!results.is_empty()).then_some(results)
}

/// Whether `text` needs a fenced code block: a line long enough that Feishu's
/// markdown would wrap it, or a line Feishu reads as a Setext heading
/// underline. The underline makes the parser fold every line above it — with
/// their soft breaks gone — into one heading (`rg`'s `--` group separators did
/// exactly that to a bash panel), so the output is shown literally inside a
/// fence instead.
fn needs_code_block(text: &str) -> bool {
    text.lines()
        .any(|l| l.chars().count() > 100 || is_setext_underline(l))
}

/// A line Feishu's markdown parser reads as a Setext heading underline: up to
/// three leading spaces, then one or more `-` or `=`, then only trailing
/// whitespace. More indentation is an indented code block, and internal
/// whitespace (`- - -`) is a thematic break — which can interrupt a paragraph,
/// so neither folds the lines above it.
fn is_setext_underline(line: &str) -> bool {
    let t = line.trim_end();
    let body = t.trim_start_matches(' ');
    if t.len() - body.len() > 3 || body.is_empty() {
        return false;
    }
    body.bytes().all(|b| b == b'-') || body.bytes().all(|b| b == b'=')
}

/// A file-tool input's target path: V1 names it `filePath`, V2 names it `path`.
/// Every formatter that shows a file reads it through this one rule.
fn input_path(obj: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    ["filePath", "path"]
        .iter()
        .find_map(|key| obj.get(*key).and_then(|value| value.as_str()))
}

/// The name a Tool Panel's folded header shows: ADR-0107's token, derived from
/// `(kind, detail)` by [`vocab::tool_token`]. A vendor tool keeps its own full
/// name (ADR-0042's opaque path), so a third-party tool needs no token lookup;
/// the id a Codex item kind carries is not a title (`codex/command_execution`
/// reads `shell`), and neither is any translated noun.
///
/// This is a display word, not a renderer key: the renderers select by
/// `(kind, detail)` (ADR-0105).
pub(crate) fn tool_display_name(tool: &ToolPanel) -> &str {
    let call = tool.call();
    vocab::tool_token(&call.identity, call.kind, &call.detail)
}

/// `imageView`: `{path: string | null}`.
fn image_shape(input: Option<&serde_json::Value>) -> bool {
    input
        .and_then(serde_json::Value::as_object)
        .is_some_and(|obj| obj.contains_key("path"))
}

/// An `imageView`'s Input line: the file it looked at. Unlike a `read` there is
/// no preview body to fold, so the panel only names it — with the same backtick
/// flattening as a file change's path. Selected only when [`is_codex_image`]
/// holds (the neutral `Read` detail), never by name alone.
fn image_input(input: &serde_json::Value) -> String {
    let Some(obj) = input.as_object() else {
        return input.to_string();
    };
    match obj
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.trim().is_empty())
    {
        Some(path) => format!("🖼 {}", inline_code(path)),
        None => generic_tool_input(obj),
    }
}

/// `collabAgentToolCall`: the whole shape cola's adapter writes (issue #962) —
/// the tool and its status, the sender, the receiver list, the three optional
/// scalars as **present** keys (the adapter writes them even as `null`, keyed
/// like `webSearch`'s `action`), and one object per target state row.
fn collab_agent_shape(input: Option<&serde_json::Value>) -> bool {
    let Some(obj) = input.and_then(serde_json::Value::as_object) else {
        return false;
    };
    let optional_scalar = |key: &str| {
        obj.get(key)
            .is_some_and(|value| value.is_null() || value.is_string())
    };
    obj.get("tool").and_then(serde_json::Value::as_str).is_some()
        && optional_scalar("status")
        && obj
            .get("senderThreadId")
            .and_then(serde_json::Value::as_str)
            .is_some()
        && obj
            .get("receiverThreadIds")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ids| ids.iter().all(serde_json::Value::is_string))
        && optional_scalar("prompt")
        && optional_scalar("model")
        && optional_scalar("reasoningEffort")
        && obj
            .get("agents")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|agents| {
                agents.iter().all(|agent| {
                    agent
                        .get("threadId")
                        .and_then(serde_json::Value::as_str)
                        .is_some()
                        && agent.get("status").and_then(serde_json::Value::as_str).is_some()
                        && agent
                            .get("message")
                            .is_some_and(|message| message.is_null() || message.is_string())
                        && agent
                            .get("label")
                            .is_none_or(|label| label.is_null() || label.is_string())
                })
            })
}

/// `subAgentActivity`: `{kind, agentThreadId, agentPath, model, reasoningEffort}`
/// — the first three are always written, the last two as present keys.
fn sub_agent_activity_shape(input: Option<&serde_json::Value>) -> bool {
    let Some(obj) = input.and_then(serde_json::Value::as_object) else {
        return false;
    };
    let string = |key: &str| obj.get(key).and_then(serde_json::Value::as_str).is_some();
    let optional_scalar = |key: &str| {
        obj.get(key)
            .is_some_and(|value| value.is_null() || value.is_string())
    };
    string("kind")
        && string("agentThreadId")
        && string("agentPath")
        && optional_scalar("model")
        && optional_scalar("reasoningEffort")
}

/// Wrap `text` in an inline code span whose delimiter is longer than any
/// backtick run inside it, so a command or path containing backticks cannot
/// close the span (Markdown's own rule). Content that starts or ends with a
/// backtick takes one padding space on each side, or the delimiter merges with
/// the content.
fn inline_code(text: &str) -> String {
    let longest = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    if text.starts_with('`') || text.ends_with('`') {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// Render a tool call as the panel's Input line.
///
/// Every migrated family is selected by `(kind, detail)` — never by a name: one
/// renderer per shape covers `bash`/`shell`/`codex/command_execution` and Code
/// Mode's `execute` (Execute), `edit`/`write`/`patch`/`apply_patch`/
/// `codex/file_change` (FileEdits), the read, search and fetch shapes
/// (ADR-0105, spec #1057's P2/P3). The plan's own input is the neutral
/// `TodoList` fact; only the ids whose ENVELOPE is vendor-specific and has no
/// shared shape (`task`, `skill`, `question`, Codex's collaboration rows,
/// `codex/image`) still read their name.
///
/// A declared shape owns its Input line, the empty one included: an empty line
/// renders no section, and it never falls through to the raw payload's key table
/// (which would print `- command:` for a call the adapter already described).
fn format_tool_input(tool: &ToolPanel) -> String {
    if let Some(ExecuteShape::Command {
        command,
        cwd,
        description,
        ..
    }) = ExecuteShape::of(tool)
    {
        return command_input(command, cwd, description);
    }
    if let Some(ExecuteShape::Program { source, language, .. }) = ExecuteShape::of(tool) {
        return program_input(source, language);
    }
    if let Some(edits) = file_edits(tool) {
        return file_edits_input(&edits);
    }
    if tool.is_plan() {
        return tool.input().map(plan_input).unwrap_or_default();
    }
    if is_codex_image(tool) {
        return tool.input().map(image_input).unwrap_or_default();
    }
    if let Some(read) = read_detail(tool) {
        return read_input(tool, read);
    }
    if let Some(search) = search_detail(tool) {
        return search_input(tool, search);
    }
    if tool.call().kind == ToolKind::Fetch {
        return fetch_input(tool.input());
    }
    // A Codex collaboration call keeps its own vendor presentation (issue
    // #962); the shared `Delegation` launch panel below is for the generic
    // task/subagent shape.
    let codex_collab = tool.name() == "codex/collab_agent" && collab_agent_shape(tool.input());
    if is_launch_call(tool) && !codex_collab {
        return tool.input().map(delegation_input).unwrap_or_default();
    }
    tool.input()
        .map(|input| format_named_input(tool.name(), input))
        .unwrap_or_default()
}

/// The Input line a `Command` renders: the command, its working directory and
/// the client's one-line description. Every producer has already resolved its
/// own key into the shape (V1's `workdir` and Codex's `cwd` are one `cwd`
/// there), so there is no vendor split left to take. Empty when the shape names
/// nothing — the shape is the whole input, so there is nothing else to show.
fn command_input(command: &str, cwd: Option<&str>, description: Option<&str>) -> String {
    let mut line = String::new();
    if !command.is_empty() {
        line.push_str(&inline_code(command));
    }
    if let Some(dir) = cwd {
        line.push_str(&format!("\n📁 `{dir}`"));
    }
    if let Some(description) = description {
        line.push_str(&format!("\n📝 {description}"));
    }
    line
}

/// The Input line a Code Mode `Program` renders: its source, fenced with the
/// OUTPUT budget (`tool_panel_element` widens the input clamp for this shape),
/// the pre-clamp keeping the closing fence inside the panel's budget. Code
/// Mode's runtime is JavaScript and its wire declares no language, so that stays
/// the default; a client that declares one names the fence. Empty when the shape
/// carries no source, for the same reason as [`command_input`].
fn program_input(source: &str, language: Option<&str>) -> String {
    if source.is_empty() {
        return String::new();
    }
    let lang = language.filter(|lang| !lang.is_empty()).unwrap_or("javascript");
    fenced_code(&truncate_md(source, TOOL_OUTPUT_MAX_CHARS - 64), Some(lang))
}

/// The Input line a **Read** renders: the book icon and the path, plus the
/// page window when the raw input records one. The path comes from the typed
/// detail; `offset`/`limit` are the call's own paging arguments and stay raw
/// facts beside it.
fn read_input(tool: &ToolPanel, read: ReadShape<'_>) -> String {
    let Some(path) = read.path.filter(|path| !path.trim().is_empty()) else {
        // A path-less read keeps its raw payload (a bare string, an unknown
        // shape) rather than rendering an empty line.
        return tool
            .input()
            .map(|input| format_named_input(tool.name(), input))
            .unwrap_or_default();
    };
    let mut line = format!("📖 {}", inline_code(path));
    if let Some(obj) = tool.input().and_then(serde_json::Value::as_object) {
        if let Some(offset) = obj.get("offset").and_then(serde_json::Value::as_i64) {
            line.push_str(&format!("\n从第 {} 行起", offset));
        }
        if let Some(limit) = obj.get("limit").and_then(serde_json::Value::as_i64) {
            line.push_str(&format!("\n最多 {} 行", limit));
        }
    }
    line
}

/// The Input line a **Search** renders, keyed by its scope: a web search names
/// its query with the magnifier; a filename/content search names the pattern
/// (`匹配 …`), then the path it was limited to, its `include` filter and its
/// paging window when the raw input records them.
fn search_input(tool: &ToolPanel, search: SearchShape<'_>) -> String {
    let obj = tool.input().and_then(serde_json::Value::as_object);
    let query = search.query.map(String::as_str).filter(|q| !q.is_empty());
    if search.scope == SearchScope::Web {
        return match query {
            Some(query) => format!("🔎 {}", inline_code(query)),
            None => obj.map(generic_tool_input).unwrap_or_default(),
        };
    }
    let mut parts = Vec::new();
    if let Some(query) = query {
        parts.push(format!("匹配 {}", inline_code(query)));
    }
    if let Some(obj) = obj {
        if let Some(path) = input_path(obj) {
            parts.push(inline_code(path));
        }
        if let Some(include) = obj.get("include").and_then(serde_json::Value::as_str) {
            parts.push(format!("include {}", inline_code(include)));
        }
    }
    if parts.is_empty() {
        return obj.map(generic_tool_input).unwrap_or_default();
    }
    let mut line = format!("🔍 {}", parts.join(" · "));
    if let Some(obj) = obj {
        if let Some(offset) = obj.get("offset").and_then(serde_json::Value::as_i64) {
            line.push_str(&format!("\n从第 {} 行起", offset));
        }
        if let Some(limit) = obj.get("limit").and_then(serde_json::Value::as_i64) {
            // `limit` counts the tool's own unit: a filename search counts
            // files, a content search counts matching lines.
            let unit = if search.scope == SearchScope::FileName {
                "个文件"
            } else {
                "行"
            };
            line.push_str(&format!("\n最多 {} {}", limit, unit));
        }
    }
    line
}

/// The Input line a **Fetch** renders: the URL it fetched.
fn fetch_input(input: Option<&serde_json::Value>) -> String {
    let Some(obj) = input.and_then(serde_json::Value::as_object) else {
        return input
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_default();
    };
    match obj.get("url").and_then(serde_json::Value::as_str) {
        Some(url) => format!("🌐 {}", inline_code(url)),
        None => generic_tool_input(obj),
    }
}

/// Render a tool's input JSON as human-readable markdown, for the ids whose
/// ENVELOPE is vendor-specific and has no shared shape. Anything else falls
/// back to a generic key-value list. The migrated families (Execute,
/// FileEdits, Read, Search, Fetch) are selected by their shape
/// ([`format_tool_input`]), not here.
fn format_named_input(name: &str, input: &serde_json::Value) -> String {
    // Non-object inputs (a bare path string, a number) just display as-is.
    let obj = match input.as_object() {
        Some(o) => o,
        None => return input.as_str().map(|s| s.to_string()).unwrap_or_default(),
    };
    let get = |k: &str| obj.get(k).and_then(|v| v.as_str());
    match name {
        // One collaboration call's report (issue #962) and one child-agent
        // activity line. Both are adapter-written shapes, gated like every
        // other `codex/` arm.
        "codex/collab_agent" if collab_agent_shape(Some(input)) => collab_agent_input(input),
        "codex/sub_agent_activity" if sub_agent_activity_shape(Some(input)) => {
            sub_agent_activity_input(input)
        }
        "skill" => match get("name").or_else(|| get("id")) {
            Some(name) => format!("🧩 {}", name),
            None => input.to_string(),
        },
        "question" => match obj.get("questions").and_then(|v| v.as_array()) {
            Some(questions) if !questions.is_empty() => {
                let first = questions[0]
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let first = first_chunk(first, 120);
                format!("❓ {} 个问题 · {}", questions.len(), first)
            }
            _ => input.to_string(),
        },
        _ => generic_tool_input(obj),
    }
}

/// The plan surface's Input line: a still-running call has no parsed output
/// yet, so the plan's size is all it can show (`📋 共 N 项任务`). A payload with
/// no todo array keeps its raw form.
fn plan_input(input: &serde_json::Value) -> String {
    match parse_todos(input) {
        Some(todos) => format!("📋 共 {} 项任务", todos.len()),
        None => input.to_string(),
    }
}

/// The prompt clip on a collaboration call's body: the state rows are the
/// call's live information, so the prompt is summarized (its first line) to
/// leave them inside the panel's shared input budget.
const COLLAB_PROMPT_MAX_CHARS: usize = 200;
/// The clip on one target's reported message (an error's explanation).
const COLLAB_MESSAGE_MAX_CHARS: usize = 160;

/// A collaboration call's Input line (issue #962): the prompt, then the
/// model/effort the child was started with. The target rows are the call's
/// RESULT and ride the panel's Output body ([`codex_collab_output`]): a `wait`
/// may name many targets, more than the Input line's shared 400-character
/// budget could hold.
fn collab_agent_input(input: &serde_json::Value) -> String {
    let Some(obj) = input.as_object() else {
        return input.to_string();
    };
    let get = |key: &str| obj.get(key).and_then(serde_json::Value::as_str);
    let mut lines = Vec::new();
    if let Some(prompt) = get("prompt").filter(|prompt| !prompt.trim().is_empty()) {
        lines.push(format!("📝 {}", first_chunk(prompt, COLLAB_PROMPT_MAX_CHARS)));
    }
    if let Some(line) = collab_model_line(get("model"), get("reasoningEffort")) {
        lines.push(line);
    }
    // A status this build does not model keeps its own string on the card, so a
    // wire enum Codex extends is never reduced to a wrench icon alone.
    if let Some(status) = get("status").filter(|status| {
        !status.is_empty() && !matches!(*status, "inProgress" | "completed" | "failed" | "interrupted")
    }) {
        lines.push(format!("🔧 {status}"));
    }
    let has_rows = obj
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|agents| !agents.is_empty());
    if lines.is_empty() && !has_rows {
        // V2's `wait` carries no prompt, model or target state at all; say so
        // rather than render an empty body. A call with rows but no prompt is
        // not payload-less: its states ride the Output body.
        lines.push(collab_payload_less_line(get("status").unwrap_or_default()));
    }
    lines.join("\n")
}

/// The line a call with no payload shows: V2's `wait` carries no prompt, model
/// or target state, and its own result (`{"message":"Wait completed.",…}`) is a
/// tool-output response item the item stream never carries — so the card names
/// the call's own state and says why the target list is absent, instead of
/// rendering an empty `_(no details)_` body.
fn collab_payload_less_line(status: &str) -> String {
    if status.is_empty() {
        "未上报目标状态".to_string()
    } else {
        format!(
            "{} {} · 未上报目标状态",
            collab_state_icon(status),
            collab_state_label(status)
        )
    }
}

/// One markdown row per target agent (issue #962): its state icon, the thread's
/// short id, its own state word and — when the wire reported one — that state's
/// message. `None` when the call has no target rows (a start payload), so the
/// panel shows no empty Output section.
fn codex_collab_output(tool: &ToolPanel) -> Option<(Option<String>, BodyStyle, String)> {
    if tool.name() != "codex/collab_agent" || !collab_agent_shape(tool.input()) {
        return None;
    }
    let agents = tool.input()?.get("agents")?.as_array()?;
    if agents.is_empty() {
        return None;
    }
    Some((None, BodyStyle::Markdown, collab_agent_rows(agents)))
}

/// How many characters of target rows one panel shows: the output clamp minus
/// room for the closing remainder line, so the body always ends on a WHOLE row
/// (or that line) instead of a row clipped mid-message. A `wait` may name more
/// targets than this — the title counts them all, and the body names the rest
/// explicitly rather than hiding them behind a bare ellipsis.
const COLLAB_ROWS_MAX_CHARS: usize = TOOL_OUTPUT_MAX_CHARS - 40;

/// The target rows of a collaboration call, in the adapter's own order. Every
/// row is a whole one; targets that no longer fit are counted by the closing
/// `… 另有 N 个目标` line.
fn collab_agent_rows(agents: &[serde_json::Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut hidden = 0usize;
    for (index, agent) in agents.iter().enumerate() {
        let Some(row) = collab_agent_row(agent) else {
            continue;
        };
        // The newline after the row is part of its cost.
        let cost = row.chars().count() + 1;
        if used + cost > COLLAB_ROWS_MAX_CHARS {
            hidden = agents.len() - index;
            break;
        }
        used += cost;
        lines.push(row);
    }
    if hidden > 0 {
        lines.push(format!("… 另有 {hidden} 个目标"));
    }
    lines.join("\n")
}

/// One target's row: its state icon, the thread's short id, its own state word
/// and — when the wire reported one — that state's message.
fn collab_agent_row(agent: &serde_json::Value) -> Option<String> {
    let label = collab_agent_label(agent)?;
    let status = agent
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut row = format!(
        "{} {} · {}",
        collab_state_icon(status),
        inline_code(&label),
        collab_state_label(status)
    );
    if let Some(message) = agent
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|message| !message.trim().is_empty())
    {
        row.push_str(&format!(" · {}", first_chunk(message, COLLAB_MESSAGE_MAX_CHARS)));
    }
    Some(row)
}

/// One child-agent activity, on one line: what happened, to which thread, and
/// the model it was started with (issue #962). The agent's path is on the
/// folded title, the same place every other panel names its subject.
fn sub_agent_activity_input(input: &serde_json::Value) -> String {
    let Some(obj) = input.as_object() else {
        return input.to_string();
    };
    let get = |key: &str| obj.get(key).and_then(serde_json::Value::as_str);
    let mut line = format!("🌱 {}", activity_label(get("kind").unwrap_or_default()));
    if let Some(thread_id) = get("agentThreadId").filter(|id| !id.is_empty()) {
        line.push_str(&format!(" · 🧵 {}", inline_code(&thread_short_id(thread_id))));
    }
    if let Some(model) = collab_model_line(get("model"), get("reasoningEffort")) {
        line.push('\n');
        line.push_str(&model);
    }
    line
}

/// The model / reasoning-effort line a collab body shows, from whichever half
/// the wire carried — omitted entirely when it carried neither.
fn collab_model_line(model: Option<&str>, effort: Option<&str>) -> Option<String> {
    match (model.filter(|m| !m.is_empty()), effort.filter(|e| !e.is_empty())) {
        (Some(model), Some(effort)) => Some(format!("🤖 {} · {effort}", inline_code(model))),
        (Some(model), None) => Some(format!("🤖 {}", inline_code(model))),
        (None, Some(effort)) => Some(format!("🧠 {effort}")),
        (None, None) => None,
    }
}

/// A target's readable label: the child's own `agentPath` when the read
/// carried one (the `subAgentActivity` of the same child thread reports it),
/// otherwise the thread id's first eight characters. Folded to one line
/// because both the folded title (`plain_text`) and the body row are single
/// lines, and a valid path may carry a newline.
fn collab_agent_label(agent: &serde_json::Value) -> Option<String> {
    let thread_id = agent.get("threadId").and_then(serde_json::Value::as_str)?;
    let label = agent
        .get("label")
        .and_then(serde_json::Value::as_str)
        .filter(|label| !label.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| thread_short_id(thread_id));
    Some(folded_header_fragment(&label))
}

/// The 8-character id label the card falls back to when no readable agent
/// path is known.
fn thread_short_id(thread_id: &str) -> String {
    thread_id.chars().take(8).collect()
}

/// A target agent's state icon (the collab item's `agentsStates`). The two
/// failure states are the card's failure mark, like a settled call's.
fn collab_state_icon(status: &str) -> &'static str {
    match status {
        "pendingInit" | "running" => "⏳",
        "completed" => "✅",
        "errored" | "interrupted" => "❌",
        "shutdown" => "⏹",
        "notFound" => "❓",
        "" => "•",
        _ => "🔧",
    }
}

/// A target agent's state word, keeping an unknown state's own string.
fn collab_state_label(status: &str) -> &str {
    match status {
        "pendingInit" => "准备中",
        "running" => "运行中",
        "completed" => "完成",
        "errored" => "出错",
        "interrupted" => "已中断",
        "shutdown" => "已关闭",
        "notFound" => "未找到",
        "" => "尚无状态",
        other => other,
    }
}

/// A child-agent activity kind's word, keeping an unknown kind's own string.
fn activity_label(kind: &str) -> &str {
    match kind {
        "started" => "启动",
        "interacted" => "交互",
        "interrupted" => "中断",
        "completed" => "完成",
        other => other,
    }
}

/// The generic fallback for a tool without a tailored input arm: one
/// `- key: value` line per scalar field, nested values compacted and clipped.
fn generic_tool_input(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut lines = Vec::new();
    for (k, v) in obj {
        let val = match v {
            serde_json::Value::String(s) => s.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            other => other.to_string(),
        };
        lines.push(format!("- {}: {}", k, first_chunk(&val, 160)));
    }
    if lines.is_empty() {
        serde_json::Value::Object(obj.clone()).to_string()
    } else {
        lines.join("\n")
    }
}

/// The first line of `s`, clipped to `max_chars` (with a "…" marker). Used to
/// summarize multi-line tool inputs (edit diffs, task prompts) instead of
/// flooding the card.
fn first_chunk(s: &str, max_chars: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    let clipped: String = first.chars().take(max_chars).collect();
    if clipped.chars().count() < first.chars().count() {
        format!("{}…", clipped)
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A section label as the panel renders it. ADR-0107 §6: a copy rename
    /// edits `vocab.rs`, and the test reads the constant rather than repeating
    /// the word.
    fn input_md() -> String {
        format!("**{}**", crate::feishu::card::vocab::INPUT_LABEL)
    }

    fn output_md() -> String {
        format!("**{}**", crate::feishu::card::vocab::OUTPUT_LABEL)
    }
    use crate::backend::{ToolIdentity, ToolOutput};
    use crate::feishu::card::CardState;
    use crate::feishu::card::shell::CardBuilder;
    use serde_json::json;

    /// A `read` panel whose one output block is a File Content, with the given
    /// resolved delivery (ADR-0076).
    fn read_panel(name: &str, mime: &str, bytes: &[u8], delivery: FileDelivery) -> ToolPanel {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let content = FileContent::decode(&format!("data:{mime};base64,{encoded}"), Some(mime), Some(name))
            .expect("an inline payload is a File Content");
        let call = ToolCall {
            identity: ToolIdentity {
                name: "read".into(),
                call_id: "call_read".into(),
            },
            kind: ToolKind::Read,
            detail: ToolDetail::Read {
                path: Some(name.to_string()),
                preview: None,
                directory: false,
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: None,
            metadata: None,
            output: ToolOutput {
                raw: None,
                blocks: vec![ContentBlock::File(content)],
                error: None,
            },
            ..ToolCall::default()
        };
        let mut panel = ToolPanel::new(call);
        panel.set_file_delivery(0, delivery);
        panel
    }

    /// An embedded File Content (ADR-0076): the panel carries its tracking line
    /// with `· 已内嵌`, and the `img` element sits IMMEDIATELY after the panel
    /// (not inside it — the panel is collapsed by default), `title` the visible
    /// caption `📎 <name>`, `alt` the image's accessible name — the plain
    /// basename (Feishu's image schema marks `alt` required, so it is always
    /// present, now meaningfully populated) — and `preview` on.
    #[test]
    fn an_embedded_file_content_renders_its_tracking_line_and_image_after_the_panel() {
        let panel = read_panel(
            "shot.png",
            "image/png",
            b"ABC",
            FileDelivery::Embedded {
                image_key: "img_v2_x".into(),
            },
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .with_text("after")
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[0]["tag"], "collapsible_panel", "{card}");
        assert_eq!(elements[1]["tag"], "img", "the image follows its panel: {card}");
        assert_eq!(elements[1]["img_key"], "img_v2_x");
        assert_eq!(elements[1]["title"]["content"], "📎 shot.png");
        assert_eq!(
            elements[1]["alt"],
            json!({ "tag": "plain_text", "content": "shot.png" }),
            "the schema-required `alt` carries the plain basename: {card}"
        );
        assert_eq!(elements[1]["preview"], true, "the image is clickable to enlarge");
        assert_eq!(
            elements[2]["content"], "after",
            "later content still follows: {card}"
        );
        let body = elements[0]["elements"][0]["content"].as_str().unwrap();
        assert!(
            body.contains("📎 shot.png · image/png · 3 B · 已内嵌"),
            "the panel carries the tracking line: {body}"
        );
    }

    /// An embedded image's `alt` and `title` are the file's BASENAME (ADR-0076
    /// amendment): a `read` reports the file's absolute path, but `alt` is the
    /// image's accessible name (shown if the image fails to load) and `title` is
    /// the visible caption, where a 44-character path is noise. `title` is
    /// `📎 shot.png` and `alt` is the plain `shot.png` — no `/` and no `…` in
    /// either — and the full path appears nowhere in the element. The header
    /// carries no marker (the image is itself visible beside the panel), so the
    /// short name lives only on the image.
    #[test]
    fn an_embedded_image_alt_and_title_are_the_plain_basename() {
        let name = "/root/workspace/dev/cola/target/acc/shot.png";
        let panel = read_panel(
            name,
            "image/png",
            b"ABC",
            FileDelivery::Embedded {
                image_key: "img_v2_x".into(),
            },
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[1]["tag"], "img", "{card}");
        let title = elements[1]["title"]["content"].as_str().unwrap();
        let alt = elements[1]["alt"]["content"].as_str().unwrap();
        // The basename alone: the path prefix is dropped, no clip marker shows.
        assert_eq!(title, "📎 shot.png", "{card}");
        assert_eq!(alt, "shot.png", "the alt is the plain basename: {card}");
        for field in [title, alt] {
            assert!(
                !field.contains('/') && !field.contains('\\') && !field.contains('…'),
                "no path fragment and no clip marker in the image fields: {field}"
            );
        }
        // The whole element carries no long absolute path anywhere.
        let serialized = serde_json::to_string(&elements[1]).unwrap();
        assert!(
            !serialized.contains("/root/workspace"),
            "the image element must not repeat the absolute path: {serialized}"
        );
        // The image is visible in the card, so the folded header names nothing:
        // no marker, no `📎`, byte-for-byte the plain header.
        let header = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .unwrap();
        assert_eq!(
            header,
            format!("✅ {}", vocab::TOOL_READ),
            "an embedded image leaves the header plain: {header}"
        );
        assert!(
            !header.contains("📎"),
            "no marker for an embedded-only panel: {header}"
        );
    }

    /// A panel renders one `img` for EVERY embedded File Content — there is no
    /// per-panel image cap (ADR-0076 review round 4): a panel with more embedded
    /// files than the old 20-image bound emits an `img` for each, the panel body
    /// still lists every file with `已内嵌`, and the card-size estimate charges
    /// exactly the images that render ([`ToolPanel::embedded_file_count`] equals
    /// the emitted `img` count). The old bound degraded the extra files to their
    /// tracking lines; the maintainer removed it so the render is exactly what
    /// the panel's files resolved to.
    #[test]
    fn a_panel_embeds_every_embedded_file_past_the_old_bound() {
        use base64::Engine as _;
        let total: usize = 25; // past the old 20-image panel bound
        let blocks: Vec<ContentBlock> = (0..total)
            .map(|i| {
                let encoded = base64::engine::general_purpose::STANDARD.encode(b"ABC");
                let name = format!("shot_{i}.png");
                ContentBlock::File(
                    FileContent::decode(
                        &format!("data:image/png;base64,{encoded}"),
                        Some("image/png"),
                        Some(&name),
                    )
                    .expect("an inline payload is a File Content"),
                )
            })
            .collect();
        let call = ToolCall {
            identity: ToolIdentity {
                name: "read".into(),
                call_id: "call_read".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: None,
            metadata: None,
            output: ToolOutput {
                raw: None,
                blocks,
                error: None,
            },
            ..ToolCall::default()
        };
        let mut panel = ToolPanel::new(call);
        for i in 0..total {
            panel.set_file_delivery(
                i,
                FileDelivery::Embedded {
                    image_key: format!("img_{i}"),
                },
            );
        }
        // The estimate charges exactly what the render emits.
        assert_eq!(panel.embedded_file_count(), total);

        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let images = elements.iter().filter(|e| e["tag"] == "img").count();
        assert_eq!(images, total, "every embedded file emits an img: {card}");
        let body = elements[0]["elements"][0]["content"].as_str().unwrap();
        assert!(body.contains("shot_0.png"), "the first file is tracked: {body}");
        assert!(
            body.contains(&format!("shot_{}.png", total - 1)),
            "the last file is tracked: {body}"
        );
        assert_eq!(
            body.matches("已内嵌").count(),
            total,
            "every embedded file claims 已内嵌: {body}"
        );
        assert_eq!(
            body.matches("已发送为文件消息").count(),
            0,
            "nothing is sent as a file: {body}"
        );
    }

    /// A File Content with no embed — a non-image, or an image the upload could
    /// not embed — keeps the bare record line with the `· 未发送` state and
    /// emits NO `img` element.
    #[test]
    fn an_undelivered_file_content_renders_only_its_unsent_line() {
        let panel = read_panel("doc.pdf", "application/pdf", b"%PDF", FileDelivery::Undelivered);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 1, "no image element: {card}");
        let body = elements[0]["elements"][0]["content"].as_str().unwrap();
        assert!(
            body.contains("📎 doc.pdf · application/pdf · 4 B · 未发送"),
            "{body}"
        );
        assert!(!body.contains("已内嵌"), "{body}");
    }

    /// A File Content delivered as a File Message (#649) keeps the bare record
    /// line with the `· 已发送为文件消息` state and emits NO `img` element: the
    /// bytes live in a separate message, not on the card.
    #[test]
    fn a_file_message_content_renders_its_sent_line_and_no_image() {
        let panel = read_panel("doc.pdf", "application/pdf", b"%PDF", FileDelivery::SentAsFile);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 1, "no image element: {card}");
        let body = elements[0]["elements"][0]["content"].as_str().unwrap();
        assert!(
            body.contains("📎 doc.pdf · application/pdf · 4 B · 已发送为文件消息"),
            "{body}"
        );
        assert!(!body.contains("已内嵌"), "{body}");
    }

    /// An embedded image leaves the folded panel HEADER unmarked (ADR-0076
    /// amendment): the image is visible in the card right after the panel, with
    /// its own `📎 <name>` caption, so naming it again in the folded header would
    /// only duplicate it. The marker is for the files the card does NOT show, so
    /// an all-embedded panel's header is byte-for-byte the plain one — while the
    /// body still carries the full tracking line.
    #[test]
    fn an_embedded_file_leaves_the_collapsed_panel_header_unmarked() {
        let panel = read_panel(
            "shot.png",
            "image/png",
            b"ABC",
            FileDelivery::Embedded {
                image_key: "img_v2_x".into(),
            },
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(title, format!("✅ {}", vocab::TOOL_READ), "{card}");
        assert!(!title.contains("📎"), "no marker for an embedded file: {title}");
        // The body still records the file and its `已内嵌` state.
        let body = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert!(
            body.contains("📎 shot.png · image/png · 3 B · 已内嵌"),
            "the body keeps the tracking line: {body}"
        );
    }

    /// An undelivered File Content — the one state that must never be invisible
    /// — carries both its name and `未发送` in the folded header, so the user
    /// sees the failure without expanding the panel (ADR-0076 amendment).
    #[test]
    fn an_undelivered_file_marks_the_collapsed_panel_header_unsent() {
        let panel = read_panel("doc.pdf", "application/pdf", b"%PDF", FileDelivery::Undelivered);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(
            title,
            format!("✅ {} · 📎 doc.pdf · 未发送", vocab::TOOL_READ),
            "{card}"
        );
    }

    /// A panel with several File Contents the card does NOT show — each sent as a
    /// File Message — shows a count in the folded header, not one name per file
    /// (ADR-0076 amendment), and appends `未发送` when any of them is
    /// undelivered. The count is the marked files only.
    #[test]
    fn several_unembedded_files_show_a_count_in_the_collapsed_panel_header() {
        let panel = |deliveries: &[FileDelivery]| {
            let files: Vec<(&str, &[u8])> = vec![("a.png", &b"ABC"[..]), ("b.png", &b"ABC"[..])];
            let mut panel = ToolPanel::for_test_with_files("read", &files);
            for (i, delivery) in deliveries.iter().enumerate() {
                panel.set_file_delivery(i, delivery.clone());
            }
            panel
        };
        let card = |panel: ToolPanel| {
            CardBuilder::new()
                .with_state(CardState::Done)
                .with_tool(panel)
                .build()
        };

        let sent = card(panel(&[FileDelivery::SentAsFile, FileDelivery::SentAsFile]));
        let title = sent["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert!(title.contains("📎 2 个文件"), "{title}");
        assert!(!title.contains("a.png"), "a count, not a name: {title}");

        let unsent = card(panel(&[FileDelivery::SentAsFile, FileDelivery::Undelivered]));
        let title = unsent["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert!(title.contains("📎 2 个文件 · 未发送"), "{title}");
    }

    /// A single file sent as a File Message marks the folded header with its
    /// BASENAME (ADR-0076 amendment): the bytes live in a separate message, so
    /// the card shows nothing for it and the header is the only signal on the
    /// card itself.
    #[test]
    fn a_sent_as_file_content_marks_the_collapsed_panel_header_with_its_basename() {
        let name = "/root/workspace/dev/cola/target/acc/doc.pdf";
        let panel = read_panel(name, "application/pdf", b"%PDF", FileDelivery::SentAsFile);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(title, format!("✅ {} · 📎 doc.pdf", vocab::TOOL_READ), "{card}");
        assert!(!title.contains('/'), "the marker shows the basename: {title}");
    }

    /// A mixed panel — an embedded image plus an undelivered file — still marks
    /// the folded header (ADR-0076 amendment): the embedded image is visible in
    /// the card and needs no marker, but the undelivered file does, so the marker
    /// appears and names the file that the card does NOT show.
    #[test]
    fn a_mixed_panel_marks_the_header_for_its_unembedded_file() {
        let files: Vec<(&str, &[u8])> = vec![("shot.png", &b"ABC"[..]), ("doc.pdf", &b"%PDF"[..])];
        let mut panel = ToolPanel::for_test_with_files("read", &files);
        panel.set_file_delivery(
            0,
            FileDelivery::Embedded {
                image_key: "img_v2_x".into(),
            },
        );
        panel.set_file_delivery(1, FileDelivery::Undelivered);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(
            title,
            format!("✅ {} · 📎 doc.pdf · 未发送", vocab::TOOL_READ),
            "{card}"
        );
        // The embedded image still renders beside the panel; its own caption
        // names it, so the header need not.
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[1]["tag"], "img", "{card}");
        assert_eq!(elements[1]["title"]["content"], "📎 shot.png", "{card}");
    }

    /// A panel with no File Content leaves its header byte-for-byte what it was:
    /// no `📎` marker, and the existing pieces (status icon · name · time) stay
    /// intact (ADR-0076 amendment).
    #[test]
    fn a_panel_without_files_leaves_the_header_unmarked() {
        let panel = ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            Some(json!({"command": "cargo test"})),
            Some("ok"),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(title, "✅ bash", "{card}");
        assert!(!title.contains("📎"), "{title}");
    }

    /// A long absolute path shows only its BASENAME in the folded header
    /// (ADR-0076 amendment): a `read` reports the file's absolute path, but the
    /// header is a one-line status strip where path fragments are noise, so the
    /// directories are dropped and just `shot.png` renders — no `/` and no `…`,
    /// since the basename fits the budget. The body keeps the full name.
    #[test]
    fn a_long_absolute_path_shows_only_its_basename_in_the_panel_header() {
        let name = "/root/workspace/dev/cola/target/acc/shot.png";
        let panel = read_panel(name, "image/png", b"ABC", FileDelivery::SentAsFile);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(title, format!("✅ {} · 📎 shot.png", vocab::TOOL_READ), "{card}");
        assert!(
            !title.contains('/') && !title.contains('…'),
            "no path fragment and no clip marker reach the header: {title}"
        );
        let body = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert!(body.contains(name), "the body keeps the full name: {body}");
    }

    /// A short name is unchanged in the folded header (ADR-0076 amendment): a
    /// name within [`FILE_HEADER_NAME_MAX_CHARS`] renders verbatim, with no
    /// leading `…`.
    #[test]
    fn a_short_file_name_is_unchanged_in_the_panel_header() {
        let panel = read_panel("shot.png", "image/png", b"ABC", FileDelivery::SentAsFile);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(title, format!("✅ {} · 📎 shot.png", vocab::TOOL_READ), "{card}");
        assert!(!title.contains('…'), "a short name is not clipped: {title}");
    }

    /// The header marker is card JSON too (ADR-0076 amendment): the estimate
    /// charges its JSON-escaped bytes, so a quote-heavy filename in the folded
    /// header cannot push a card past Feishu's size cap unnoticed. A panel with
    /// no File Content charges nothing.
    #[test]
    fn file_header_marker_estimate_charges_the_header_bytes() {
        let plain = ToolPanel::for_test("bash", ToolStatus::Completed, None, Some("ok"));
        assert_eq!(file_header_marker_estimate(&plain), 0);

        let panel = read_panel("shot.png", "image/png", b"ABC", FileDelivery::SentAsFile);
        let marker = file_header_marker(&panel).expect("a file panel has a header marker");
        assert_eq!(
            file_header_marker_estimate(&panel),
            marker.len(),
            "the estimate is the marker's serialized length"
        );
        assert!(file_header_marker_estimate(&panel) > 0);

        // Each `"` in the header title costs its escaped `\"`: the JSON-escaped
        // charge is one byte more per quote than the raw marker length.
        let quotes = "\"".repeat(FILE_HEADER_NAME_MAX_CHARS);
        let quoted = read_panel(&quotes, "image/png", b"ABC", FileDelivery::Undelivered);
        let raw = file_header_marker(&quoted).expect("a file panel has a header marker");
        assert_eq!(
            file_header_marker_estimate(&quoted),
            raw.len() + quotes.len(),
            "each `\"` must cost its escaped `\\\"` — one extra byte"
        );
        assert!(
            file_header_marker_estimate(&quoted) > raw.len(),
            "the estimate must not be the raw UTF-8 length"
        );
    }

    /// A `write` or `edit` Tool Panel's 「📂 打开」 (ADR-0083 §8, #743) opens the
    /// file the call itself produced: the button's path is the call's
    /// STRUCTURED `filePath` (V1) — `path` on V2 — never the agent's prose, and
    /// the payload routes by `action: "file"` with no `op`, so the click opens
    /// the File Viewer rather than transferring it (#739's `op: "send"`).
    #[test]
    fn write_and_edit_panels_carry_an_open_button_from_the_structured_path() {
        for (name, input) in [
            ("write", json!({"filePath": "report.md", "content": "hi"})),
            ("write", json!({"path": "src/v2w.rs"})),
            ("edit", json!({"filePath": "src/main.rs"})),
            ("edit", json!({"path": "src/v2.rs"})),
        ] {
            let panel = match name {
                "write" => write_tool(ToolStatus::Completed, Some(input), Some("ok")),
                _ => edit_tool(ToolStatus::Completed, Some(input), Some("ok")),
            };
            let path = panel.file_open_path().expect("a write/edit panel names its path");
            let button = tool_open_button_element(&panel, Some("ses_1")).expect("the button");
            assert_eq!(button["tag"], "button");
            assert_eq!(button["text"]["content"], "📂 打开");
            let value = &button["value"];
            assert_eq!(value["action"], "file");
            assert_eq!(value["path"], path);
            assert_eq!(value["session_id"], "ses_1");
            assert!(value.get("op").is_none(), "no op: the click opens, never sends");
            assert!(value.get("dir").is_none(), "no listing rides along: {value}");
        }
    }

    /// A FILE `read` panel carries 「📂 打开」 (ADR-0083 §8 amendment, #770) from
    /// its STRUCTURED `filePath` (V1) — `path` on V2 — through the same payload
    /// #743/#766 gave `write`/`edit` (`action: "file"`, no `op`, the Session to
    /// route by), in the same in-fold position (#767): the body's FIRST
    /// element, so the panel stays folded and a long body cannot push it out of
    /// view. The body is a `TOOL_OUTPUT_MAX_CHARS`-character preview, which is
    /// exactly why a read needs the way in, and the folded header keeps naming
    /// the file while it is collapsed.
    #[test]
    fn a_file_read_panel_carries_an_open_button_from_the_structured_path() {
        for (input, output) in [
            (
                json!({"filePath": "/x/y.rs"}),
                "<path>/x/y.rs</path>\n<type>file</type>\n<content>\n1: fn main() {}\n</content>",
            ),
            (
                json!({"path": "/x/y.rs"}),
                "Read file /x/y.rs, lines 1-1\n1: fn main() {}",
            ),
        ] {
            let panel = read_tool(ToolStatus::Completed, Some(input), Some(output));
            let path = panel.file_open_path().expect("a file read names its open path");
            assert_eq!(path, "/x/y.rs");
            let button = tool_open_button_element(&panel, Some("ses_1")).expect("the button");
            assert_eq!(button["tag"], "button");
            assert_eq!(button["text"]["content"], "📂 打开");
            let value = &button["value"];
            assert_eq!(value["action"], "file");
            assert_eq!(value["path"], path);
            assert_eq!(value["session_id"], "ses_1");
            assert!(value.get("op").is_none(), "no op: the click opens, never sends");
            assert!(value.get("dir").is_none(), "no listing rides along: {value}");
            assert_eq!(panel.open_button_count(Some("ses_1")), 1, "one component");
            assert!(
                tool_open_button_estimate(&panel, Some("ses_1")) > 0,
                "and its bytes are charged"
            );
            assert!(
                panel_title(panel.clone()).ends_with(" · 📖 y.rs"),
                "the folded header still names the file"
            );
            let card = CardBuilder::new()
                .with_state(CardState::Done)
                .with_tool_at(panel, None, Some("tool_call_read"), Some("ses_1"))
                .build();
            let body = card["body"]["elements"][0]["elements"]
                .as_array()
                .expect("panel body");
            assert_eq!(body[0]["tag"], "button", "the button leads the body: {card}");
            assert_eq!(body[0]["value"]["path"], "/x/y.rs");
            assert_eq!(body[1]["tag"], "markdown", "the content follows: {card}");
        }
    }

    /// An HTML page's Tool Panel (#778) leads with 「🌐 预览网页」 and carries the
    /// `op: "preview"` payload: `write`, `edit` and a FILE `read` of an
    /// `.html`/`.htm` path (case-insensitively) all take it, on the SAME
    /// structured path the open payload used, so the click sends the File
    /// Message the client renders instead of detouring through the viewer whose
    /// source the panel already showed. A non-page path is untouched (its own
    /// tests above).
    #[test]
    fn an_html_panel_leads_with_a_page_preview_button() {
        // `write`/`edit` and a file `read`, across V1/V2 input keys and both
        // HTML extensions — the extension decides, never the tool.
        let cases: Vec<(&str, serde_json::Value, String)> = vec![
            (
                "write",
                json!({"filePath": "/site/index.html", "content": "<p>hi</p>"}),
                "ok".to_string(),
            ),
            ("edit", json!({"path": "/site/INDEX.HTM"}), "ok".to_string()),
            (
                "read",
                json!({"filePath": "/site/page.html"}),
                "Read file /site/page.html, lines 1-1\n1: <p>hi</p>".to_string(),
            ),
        ];
        for (name, input, output) in cases {
            let panel = match name {
                "write" => write_tool(ToolStatus::Completed, Some(input), Some(&output)),
                "edit" => edit_tool(ToolStatus::Completed, Some(input), Some(&output)),
                _ => read_tool(ToolStatus::Completed, Some(input), Some(&output)),
            };
            let path = panel.file_open_path().expect("the panel names its path");
            let button = tool_open_button_element(&panel, Some("ses_1")).expect("the button");
            assert_eq!(button["tag"], "button");
            assert_eq!(button["text"]["content"], "🌐 预览网页", "{name} {path}");
            let value = &button["value"];
            assert_eq!(value["action"], "file", "one action tag: {value}");
            assert_eq!(value["path"], path);
            assert_eq!(value["session_id"], "ses_1");
            assert_eq!(
                value["op"], "preview",
                "the op that sends without rewriting the clicked card: {value}"
            );
            assert!(value.get("dir").is_none(), "no listing rides along: {value}");
            // The estimate charges the very element that renders, whatever its
            // label and payload — one constructor, no re-derivation.
            assert_eq!(panel.open_button_count(Some("ses_1")), 1, "one component");
            assert!(
                tool_open_button_estimate(&panel, Some("ses_1")) > 0,
                "and its bytes are charged"
            );
        }
    }

    /// A DIRECTORY `read` carries no 「📂 打开」 (ADR-0083 §8 amendment, #770):
    /// there is no file to open — the viewer shows files, and a listing's
    /// overflow is what the folded panel's own body already is. The verdict is
    /// the OUTPUT's shape in both generations (V1's `<type>directory</type>`,
    /// V2's `Read directory …` header), and the folded header keeps its 📖 name
    /// either way (#766).
    #[test]
    fn a_directory_read_panel_carries_no_open_button() {
        for (input, output) in [
            (
                json!({"filePath": "/tmp/notes"}),
                "<path>/tmp/notes</path>\n<type>directory</type>\n<content>\n/tmp/notes/a.rs\n</content>",
            ),
            (
                json!({"path": "/tmp/notes"}),
                "Read directory /tmp/notes, entries 1-2\n/tmp/notes/a.rs\n/tmp/notes/b.rs",
            ),
            (
                json!({"path": "/tmp/notes"}),
                "Read directory /tmp/notes, 0 entries",
            ),
        ] {
            let panel = read_tool(ToolStatus::Completed, Some(input), Some(output));
            assert_eq!(
                panel.file_open_path(),
                None,
                "a directory is not an open path: {output}"
            );
            assert!(
                tool_open_button_element(&panel, Some("ses_1")).is_none(),
                "no button: {output}"
            );
            assert_eq!(tool_open_button_estimate(&panel, Some("ses_1")), 0);
            assert_eq!(panel.open_button_count(Some("ses_1")), 0);
            assert!(
                panel_title(panel).ends_with(" · 📖 notes"),
                "the folded header still names the directory: {output}"
            );
        }
    }

    /// A `read` that carries a File Content — an image or a PDF (ADR-0076) —
    /// gets no 「📂 打开」 (ADR-0083 §8 amendment, #770): that file has its own
    /// delivery channel (the embedded image, or the `📎` marker and its File
    /// Message), so a second way in would only duplicate it.
    #[test]
    fn a_read_with_a_file_content_carries_no_open_button() {
        let mut panel = ToolPanel::for_test_with_files("read", &[("shot.png", b"ABC")]);
        panel.call.input = Some(json!({"filePath": "/x/shot.png"}));
        panel.call.kind = ToolKind::Read;
        panel.call.detail = ToolDetail::Read {
            path: Some("/x/shot.png".into()),
            preview: None,
            directory: false,
        };
        assert_eq!(
            panel.file_open_path(),
            None,
            "the file content is already named and delivered"
        );
        assert!(tool_open_button_element(&panel, Some("ses_1")).is_none());
        assert_eq!(panel.open_button_count(Some("ses_1")), 0);
    }

    /// A still-running `read` has no output yet, so the directory verdict is
    /// unknown: no button (ADR-0083 §8 amendment, #770) — nothing renders,
    /// nothing is charged. The button appears once the call settles with a
    /// file's output.
    #[test]
    fn a_running_read_carries_no_open_button_until_it_renders_a_file() {
        let running = read_tool(ToolStatus::Running, Some(json!({"filePath": "/x/y.rs"})), None);
        assert_eq!(running.file_open_path(), None, "no output, no verdict");
        assert!(tool_open_button_element(&running, Some("ses_1")).is_none());

        let settled = read_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "/x/y.rs"})),
            Some("Read file /x/y.rs, lines 1-1\n1: fn main() {}"),
        );
        assert_eq!(settled.file_open_path().as_deref(), Some("/x/y.rs"));
        assert!(tool_open_button_element(&settled, Some("ses_1")).is_some());
    }

    /// The `read` body is a `TOOL_OUTPUT_MAX_CHARS`-character preview, so a
    /// body the clamp cut says so and points at the way to the rest — the
    /// 「📂 打开」 button inside the fold (ADR-0083 §8 amendment, #770). A body
    /// that fits carries no note; a clipped DIRECTORY listing carries none
    /// either (it has no button to name); and a panel with no Session to route
    /// the click by renders neither, so the note never promises a control the
    /// card does not carry — nor bytes the estimate does not charge.
    #[test]
    fn a_clipped_read_body_says_the_preview_was_clipped() {
        let clipped = read_tool(
            ToolStatus::Completed,
            Some(json!({"path": "/x/big.rs"})),
            Some(&format!(
                "Read file /x/big.rs, lines 1-300\n{}",
                long_read_lines()
            )),
        );
        assert!(clipped.file_open_path().is_some());
        assert!(preview_truncation_note(&clipped, Some("ses_1")).is_some());
        // The note is plain text — nothing in it is sanitizer-expanded or
        // JSON-escaped — so its charge is its own byte length.
        assert_eq!(
            preview_truncation_note_estimate(&clipped, Some("ses_1")),
            PREVIEW_TRUNCATION_NOTE.len(),
            "the note's bytes are charged"
        );
        let md = panel_md_with_session(clipped.clone(), "ses_1");
        assert!(
            md.ends_with(&format!("\n\n{PREVIEW_TRUNCATION_NOTE}")),
            "the note closes a clipped body: {md}"
        );
        assert!(
            !md.contains('…'),
            "a clipped fenced body carries NO `…`: the marker stops Feishu \
             rendering the block as code (#788), so the note is the signal: {md}"
        );
        assert!(
            md.contains(&format!("```\n\n{PREVIEW_TRUNCATION_NOTE}")),
            "the fence closes bare, right before the note: {md}"
        );

        // V1's envelope clips the same way: the note is the body's, not the
        // generation's.
        let v1 = format!(
            "<path>/x/big.rs</path>\n<type>file</type>\n<content>\n{}</content>",
            long_read_lines()
        );
        let panel = read_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "/x/big.rs"})),
            Some(&v1),
        );
        let md = panel_md_with_session(panel, "ses_1");
        assert!(
            md.ends_with(&format!("\n\n{PREVIEW_TRUNCATION_NOTE}")),
            "V1 clips too: {md}"
        );

        let small = read_tool(
            ToolStatus::Completed,
            Some(json!({"path": "/x/small.rs"})),
            Some("Read file /x/small.rs, lines 1-1\n1: fn main() {}"),
        );
        assert_eq!(preview_truncation_note_estimate(&small, Some("ses_1")), 0);
        let md = panel_md_with_session(small, "ses_1");
        assert!(!md.contains(PREVIEW_TRUNCATION_NOTE), "nothing was cut: {md}");

        let directory = read_tool(
            ToolStatus::Completed,
            Some(json!({"path": "/x/huge"})),
            Some(&format!(
                "Read directory /x/huge, entries 1-300\n{}",
                long_read_lines()
            )),
        );
        assert_eq!(preview_truncation_note_estimate(&directory, Some("ses_1")), 0);
        let md = panel_md_with_session(directory, "ses_1");
        assert!(
            !md.contains(PREVIEW_TRUNCATION_NOTE),
            "no button, no promise: {md}"
        );

        assert_eq!(preview_truncation_note_estimate(&clipped, None), 0);
        let md = panel_md(clipped);
        assert!(!md.contains(PREVIEW_TRUNCATION_NOTE), "no button, no note: {md}");
    }

    /// 300 numbered lines of well over [`TOOL_OUTPUT_MAX_CHARS`] characters — a
    /// `read` body the panel clips, the shape the truncation note exists for
    /// (ADR-0083 §8 amendment, #770).
    fn long_read_lines() -> String {
        (1..=300).map(|n| format!("{n}: {}\n", "x".repeat(40))).collect()
    }

    /// #1060: a `write` is one `Add` FileEdit — the same renderer the edit and
    /// patch families use — so its content reads as additions with the action
    /// word and the `+A −D` totals, fenced with the file's language.
    #[test]
    fn a_write_renders_as_one_add_file_edit() {
        for (path, lang) in [
            ("src/main.rs", "rust"),
            ("notes.md", "markdown"),
            ("data.unknownext", "plain_text"),
        ] {
            let panel = write_panel(
                path,
                "line one\nline two",
                ToolStatus::Completed,
                Some("Wrote file successfully."),
            );
            let md = panel_md_with_session(panel.clone(), "ses_1");
            assert!(md.contains("➕ 新增"), "the action word: {md}");
            assert!(md.contains("+line one"), "the content is an addition: {md}");
            assert!(md.contains("+line two"), "the content is an addition: {md}");
            assert!(md.contains("+2 −0"), "the totals: {md}");
            assert!(md.contains(&format!("```{lang}")), "the fence: {md}");
            assert!(
                !md.contains("Wrote file successfully."),
                "the summary is dropped: {md}"
            );
            assert_eq!(
                panel.file_open_path().as_deref(),
                Some(path),
                "the way in is unchanged"
            );
            assert!(
                tool_open_button_element(&panel, Some("ses_1")).is_some(),
                "the open button still renders"
            );
        }
    }

    /// A `write` body is a diff now: it clips under the diff byte budget and
    /// says so with the diff note, not the read-preview note.
    #[test]
    fn a_clipped_write_diff_says_the_change_was_clipped() {
        let content = "x".repeat(20_000);
        let clipped = write_panel(
            "/x/big.rs",
            &content,
            ToolStatus::Completed,
            Some("Wrote file successfully."),
        );
        assert!(diff_truncation_note(&clipped).is_some());
        assert_eq!(
            preview_truncation_note_estimate(&clipped, Some("ses_1")),
            DIFF_TRUNCATION_NOTE.len(),
            "the diff note's bytes are charged"
        );
        let md = panel_md_with_session(clipped, "ses_1");
        assert!(
            md.ends_with(&format!("\n\n{DIFF_TRUNCATION_NOTE}")),
            "the diff note closes the body: {md}"
        );
        assert!(!md.contains(PREVIEW_TRUNCATION_NOTE), "not a preview note: {md}");
        assert!(!md.contains("Wrote file successfully."));

        // A body that fits carries no note.
        let small = write_panel("/x/small.rs", "fn main() {}", ToolStatus::Completed, None);
        assert_eq!(preview_truncation_note_estimate(&small, Some("ses_1")), 0);
    }

    /// A failed write keeps its error text: the hunks it never wrote are not a
    /// body, and a failure with only a recorded error still reaches the panel.
    #[test]
    fn a_failed_write_keeps_its_error() {
        let failed = write_panel(
            "report.md",
            "secret",
            ToolStatus::Error,
            Some("Unable to write report.md"),
        );
        let md = panel_md_with_session(failed, "ses_1");
        assert!(md.contains("Unable to write report.md"), "the error: {md}");
        assert!(
            !md.contains("secret"),
            "the content cannot replace an error: {md}"
        );

        let error_only = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "write".into(),
                call_id: "call_write".into(),
            },
            status: ToolStatus::Other("failed".into()),
            started_at: None,
            input: Some(json!({"path": "src/main.rs", "content": "fn main() {}"})),
            output: ToolOutput {
                raw: None,
                blocks: Vec::new(),
                error: Some("permission denied".into()),
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(vec![FileEdit {
                path: "src/main.rs".into(),
                action: FileAction::Add,
                rename_to: None,
                diff: Some("@@ -0,0 +1,1 @@\n+fn main() {}".to_string()),
            }]),
            ..ToolCall::default()
        });
        let md = panel_md_with_session(error_only, "ses_1");
        assert!(
            md.contains("permission denied"),
            "the error reaches the panel: {md}"
        );
        assert!(
            !md.contains("fn main() {}"),
            "the content cannot replace the error: {md}"
        );
    }

    /// The FileEdits estimate charges the JSON-escaped diff body clipped under
    /// the diff budget, its fence and its language tag — the same body the
    /// panel renders, never the old 3,000-character preview charge.
    #[test]
    fn the_file_edits_estimate_charges_the_body_it_renders() {
        let content = "\n".repeat(1_000);
        let panel = write_panel(
            "src/main.rs",
            &content,
            ToolStatus::Completed,
            Some("Wrote file successfully."),
        );
        let md = panel_md(panel.clone());
        let (body, _) = md
            .split_once("```rust\n")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split_once("\n```"))
            .expect("the fenced body");
        assert!(
            json_escaped_len(body) > TOOL_OUTPUT_MAX_CHARS,
            "the fixture is past the old preview cap"
        );
        assert!(
            panel_output_estimate(&panel) >= json_escaped_len(body),
            "the rendered body is charged"
        );
    }

    /// The FileEdits estimate charges the DYNAMIC fence: a hunk carrying one
    /// long backtick run sizes a fence longer than its content, and a
    /// body-only charge would let a near-budget card evade the split (#788).
    #[test]
    fn the_file_edits_estimate_charges_the_dynamic_fence() {
        let content = "`".repeat(TOOL_OUTPUT_MAX_CHARS);
        let panel = write_panel("notes.md", &content, ToolStatus::Completed, None);
        let body = file_edits_body(&panel).expect("the Add body");
        let hunks = &body.sections[0].hunks;
        let clipped = clip_fenced_body(hunks, Some(body.lang), DIFF_MAX_ESCAPED_BYTES);
        assert!(
            fenced_block_overhead(&clipped) > TOOL_OUTPUT_MAX_CHARS,
            "the fence dwarfs the body it wraps"
        );
        let header = body
            .header
            .as_deref()
            .map(|header| json_escaped_len(&CardMarkdown::new().element(header)))
            .unwrap_or(0);
        assert_eq!(
            panel_output_estimate(&panel),
            header + serialized_len(&body.render().0),
            "the fence beyond the minimum is charged"
        );
    }

    /// A FAILED FileEdits call renders its error text, not a diff: the body
    /// takes the preview cap and the render and estimate agree (#788/#792).
    #[test]
    fn a_failed_file_edit_keeps_the_error_under_the_preview_cap() {
        let error = format!("write failed: {}", "e".repeat(TOOL_OUTPUT_MAX_CHARS * 3));
        let mut panel = write_panel("big.rs", "content", ToolStatus::Error, None);
        panel.call.output.blocks = Vec::new();
        panel.call.output.error = Some(error.clone());
        let md = panel_md_with_session(panel.clone(), "ses_1");
        assert!(md.contains("write failed:"), "the error is the body: {md}");
        assert!(
            !md.contains(DIFF_TRUNCATION_NOTE),
            "no diff note over an error: {md}"
        );
        assert!(md.contains('…'), "the preview clamp marks the cut: {md}");
        let body = format_tool_output(&panel, &panel.output().unwrap()).2;
        let body = truncate_md(&body, TOOL_OUTPUT_MAX_CHARS);
        assert_eq!(
            panel_output_estimate(&panel),
            json_escaped_len(&body) + fenced_overhead(&body),
            "the estimate charges the same preview body"
        );
    }

    /// A file updated twice is one file: the totals, the action list and the
    /// open button all see the merged edit (#793).
    #[test]
    fn a_repeated_file_keeps_the_bare_change_count() {
        let tool = panel_with_edits(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            vec![
                FileEdit {
                    path: "a.rs".into(),
                    action: FileAction::Update,
                    rename_to: None,
                    diff: Some("@@ -1 +1 @@\n-a\n+b".into()),
                },
                FileEdit {
                    path: "a.rs".into(),
                    action: FileAction::Update,
                    rename_to: None,
                    diff: Some("@@ -5 +5 @@\n-c\n+d".into()),
                },
            ],
            Some("Success. Updated the following files:\nM a.rs"),
        );
        assert_eq!(tool.file_open_path().as_deref(), Some("a.rs"));
        let title = panel_title(tool.clone());
        assert!(
            title.contains("· ✏️ 编辑 a.rs") && !title.contains("2 files"),
            "one file, no count fragment: {title}"
        );
        let md = panel_md(tool);
        assert_eq!(
            md.matches("✏️ 编辑 `a.rs`").count(),
            1,
            "one action line for the one file: {md}"
        );
        assert!(md.contains("+2 −2"), "both hunks' totals: {md}");
    }

    /// The action list is clipped under the same body budget as the hunks: a
    /// many-file patch cannot exceed the card's diff allowance through its
    /// action lines alone.
    #[test]
    fn the_action_list_is_clipped_under_the_body_budget() {
        let edits = (0..1_000)
            .map(|i| FileEdit {
                path: format!("src/file_{i}.rs"),
                action: FileAction::Update,
                rename_to: None,
                diff: None,
            })
            .collect();
        let tool = panel_with_edits(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            edits,
            None,
        );
        let body = file_edits_body(&tool).expect("the action list body");
        let (text, clipped) = body.render();
        assert!(clipped, "the action list is over budget");
        assert!(
            json_escaped_len(&text) <= DIFF_MAX_ESCAPED_BYTES,
            "the body stays inside the diff budget: {}",
            json_escaped_len(&text)
        );
        assert!(text.contains("src/file_0.rs"), "early actions kept: {text}");
        assert!(
            !text.contains("src/file_999.rs"),
            "over-budget actions stopped: {text}"
        );
        assert_eq!(
            preview_truncation_note_estimate(&tool, None),
            DIFF_TRUNCATION_NOTE.len(),
            "the clip note is charged"
        );
    }

    /// A multi-file body keeps the action lines OUTSIDE the fence (markdown,
    /// so the paths' inline code renders) and preserves a hunk's trailing
    /// spaces.
    #[test]
    fn multi_file_actions_sit_outside_the_fence_and_hunk_spaces_survive() {
        let tool = panel_with_edits(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            vec![
                FileEdit {
                    path: "a.rs".into(),
                    action: FileAction::Update,
                    rename_to: None,
                    diff: Some("@@ -1 +1 @@\n-old \n+new  ".into()),
                },
                FileEdit {
                    path: "b.rs".into(),
                    action: FileAction::Add,
                    rename_to: None,
                    diff: Some("@@ -0,0 +1 @@\n+added".into()),
                },
            ],
            None,
        );
        let md = panel_md(tool);
        assert!(
            md.contains("✏️ 编辑 `a.rs`\n\n```rust\n@@ -1 +1 @@"),
            "the first file's action precedes its own hunks: {md}"
        );
        assert!(
            md.contains("➕ 新增 `b.rs`\n\n```rust\n@@ -0,0 +1 @@"),
            "the second file's action precedes its own hunks: {md}"
        );
        assert!(
            !md.contains("```rust\n✏️ 编辑"),
            "no inline-code action inside the fence: {md}"
        );
        assert!(md.contains("-old \n+new  \n"), "trailing spaces kept: {md:?}");
    }

    /// A no-hunk edit still shows the tool's own tail: the note attaches to
    /// the last section even when there is no hunk to carry it.
    #[test]
    fn a_no_hunk_edit_keeps_its_output_tail() {
        let tool = panel_with_edits(
            "edit",
            ToolStatus::Completed,
            json!({"filePath": "src/main.rs"}),
            vec![FileEdit {
                path: "src/main.rs".into(),
                action: FileAction::Update,
                rename_to: None,
                diff: None,
            }],
            Some("Edit applied successfully.\n\nLSP errors detected in this file"),
        );
        let md = panel_md(tool);
        assert!(md.contains("✏️ 编辑 `src/main.rs`"), "the action: {md}");
        assert!(
            md.contains("LSP errors detected in this file"),
            "the tail survives without a hunk: {md}"
        );
    }

    /// A `write`'s LSP note rides AFTER the fence, under its own 500-char
    /// bound, as it did before P3 (#788).
    #[test]
    fn a_write_lsp_note_rides_outside_the_fence() {
        let panel = write_panel(
            "src/main.rs",
            "fn main() {}",
            ToolStatus::Completed,
            Some("Wrote file successfully.\n\nLSP errors detected, please fix:"),
        );
        let md = panel_md(panel);
        assert!(
            md.contains("```\n\nLSP errors detected"),
            "the note follows the closed fence: {md}"
        );
        assert!(!md.contains("LSP errors detected\n```"), "not inside: {md}");

        // An Add is not enough: an `apply_patch` Add keeps its note INSIDE the
        // fence, because its output is not a write summary.
        let patch = panel_with_edits(
            "apply_patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            vec![FileEdit {
                path: "a.rs".into(),
                action: FileAction::Add,
                rename_to: None,
                diff: Some("@@ -0,0 +1 @@\n+new".into()),
            }],
            Some("Success. Updated the following files:\nA a.rs\n\nLSP errors detected"),
        );
        let md = panel_md(patch);
        assert!(
            md.contains("LSP errors detected\n```"),
            "a patch Add's note stays inside: {md}"
        );
    }

    /// A call whose path is absent or unusable carries no button rather than a
    /// broken one (#743): no input at all, a non-object input, a missing key,
    /// a non-string path, or a blank path (empty or whitespace-only — a button
    /// for one could only ever answer 找不到) — and prose in the OUTPUT naming
    /// a path never becomes a control either.
    #[test]
    fn a_panel_without_a_usable_path_carries_no_open_button() {
        let cases: Vec<(&str, Option<serde_json::Value>, Option<&str>)> = vec![
            ("write", None, Some("the agent wrote /tmp/report.md, go look")),
            ("write", Some(json!("report.md")), None),
            ("write", Some(json!({"content": "hi"})), None),
            ("write", Some(json!({"filePath": 7})), None),
            ("write", Some(json!({"filePath": ""})), None),
            ("write", Some(json!({"filePath": "   "})), None),
            ("edit", Some(json!({"filePath": ""})), None),
            ("edit", Some(json!({"path": "  "})), None),
            (
                "bash",
                Some(json!({"command": "cat report.md"})),
                Some("report.md"),
            ),
        ];
        for (name, input, output) in cases {
            let panel = ToolPanel::for_test(name, ToolStatus::Completed, input, output);
            assert_eq!(panel.file_open_path(), None, "no usable path for {name}");
            assert!(
                tool_open_button_element(&panel, Some("ses_1")).is_none(),
                "no button for {name}"
            );
            assert_eq!(
                tool_open_button_estimate(&panel, Some("ses_1")),
                0,
                "no button, no estimate charge for {name}"
            );
            assert_eq!(
                panel.open_button_count(Some("ses_1")),
                0,
                "no button, no component charge for {name}"
            );
        }
        // No Session to route by — or an empty one, which routes nowhere — no
        // button either: the click could not know whose viewer to open.
        let panel = write_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "report.md"})),
            None,
        );
        assert!(tool_open_button_element(&panel, None).is_none());
        assert!(
            tool_open_button_element(&panel, Some("")).is_none(),
            "an empty Session id routes nowhere"
        );
    }

    /// The 「📂 打开」 rides INSIDE the folded panel (ADR-0083 §8 amendment,
    /// #766): the card body carries only the panel, the panel stays collapsed,
    /// and the button is the body's first element — unfolded, it is the first
    /// thing the reader sees, ahead of a diff.
    #[test]
    fn the_open_button_rides_inside_the_folded_panel() {
        let panel = write_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "report.md", "content": "hi"})),
            Some("written"),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool_at(panel, None, Some("tool_call_write"), Some("ses_1"))
            .build();
        let elements = card["body"]["elements"].as_array().expect("body elements");
        assert_eq!(elements.len(), 1, "the panel alone: {card}");
        let folded = &elements[0];
        assert_eq!(folded["tag"], "collapsible_panel");
        assert_eq!(folded["expanded"], false, "the panel stays folded");
        let body = folded["elements"].as_array().expect("panel body");
        let button = &body[0];
        assert_eq!(button["tag"], "button", "the button leads the body: {card}");
        assert_eq!(button["text"]["content"], "📂 打开");
        assert_eq!(button["value"]["action"], "file");
        assert_eq!(button["value"]["path"], "report.md");
        assert_eq!(button["value"]["session_id"], "ses_1");
        let markdown = &body[1];
        assert_eq!(markdown["tag"], "markdown", "the panel content follows");
        assert!(
            markdown["content"]
                .as_str()
                .unwrap()
                .contains(input_md().as_str()),
            "the body is still the rendered call: {markdown}"
        );
    }

    /// A `write`/`edit` panel's folded header names the file it touched
    /// (ADR-0083 §8 amendment, #766): the path's BASENAME, through the same
    /// short-name rule the `📎` marker uses, so an absolute path never reaches
    /// the one-line header.
    #[test]
    fn write_and_edit_headers_name_the_touched_file() {
        for (name, input, word, want) in [
            (
                "write",
                json!({"filePath": "report.md", "content": "hi"}),
                "➕ 新增",
                "report.md",
            ),
            ("write", json!({"path": "src/v2w.rs"}), "➕ 新增", "v2w.rs"),
            (
                "edit",
                json!({"filePath": "/root/workspace/dev/cola/src/feishu/card/tool_render.rs"}),
                "✏️ 编辑",
                "tool_render.rs",
            ),
            ("edit", json!({"path": "src/v2.rs"}), "✏️ 编辑", "v2.rs"),
        ] {
            let panel = match name {
                "write" => write_tool(ToolStatus::Completed, Some(input), Some("ok")),
                _ => edit_tool(ToolStatus::Completed, Some(input), Some("ok")),
            };
            let title = panel_title(panel);
            assert!(
                title.ends_with(&format!(" · {word} {want}")),
                "{name} header must name {want} with its action: {title}"
            );
            assert!(
                !title.contains("/root/") && !title.contains("src/"),
                "no directories in the folded header: {title}"
            );
        }
    }

    /// A text `read` names its file in the folded header too — its path lives
    /// in the Input line, inside the fold (ADR-0083 §8 amendment, #766) — while
    /// a `read` that carries a File Content adds NO fragment: the `📎` marker
    /// (or the embedded image's caption) already names that file, and the
    /// header must not say it twice.
    #[test]
    fn a_read_header_names_its_file_only_without_a_file_content() {
        let text = panel_title(read_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "/x/y.rs"})),
            Some("<path>/x/y.rs</path>"),
        ));
        assert!(text.ends_with(" · 📖 y.rs"), "text read header: {text}");

        // A directory read's path may keep its trailing separator: the
        // detail still reaches the directory's own name, never the whole path.
        let dir = panel_title(read_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "/tmp/notes/"})),
            Some("Read directory /tmp/notes/, 3 entries\n..."),
        ));
        assert!(dir.ends_with(" · 📖 notes"), "directory read header: {dir}");

        let pdf = read_panel("doc.pdf", "application/pdf", b"%PDF", FileDelivery::SentAsFile);
        let title = panel_title(pdf);
        assert!(title.contains("📎 doc.pdf"), "the marker names it: {title}");
        assert!(!title.contains("📖"), "no second name on one line: {title}");
    }

    /// A name longer than [`FILE_HEADER_NAME_MAX_CHARS`] is clipped to its TAIL
    /// (the extension survives), and a control character folds to a space — the
    /// header is one `plain_text` line, so neither may break it.
    #[test]
    fn a_long_or_control_laden_header_name_is_clipped_and_folded() {
        let long = format!("{}.md", "a".repeat(FILE_HEADER_NAME_MAX_CHARS));
        let clipped = panel_title(write_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": long})),
            Some("ok"),
        ));
        let shown = clipped.rsplit("➕ 新增 ").next().expect("the detail");
        assert!(
            shown.starts_with('…') && shown.ends_with(".md"),
            "a long name keeps its tail: {clipped}"
        );
        assert!(
            shown.chars().count() <= FILE_HEADER_NAME_MAX_CHARS,
            "clipped to the budget: {clipped}"
        );

        let folded = panel_title(edit_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "re\nport.md"})),
            Some("ok"),
        ));
        assert!(
            folded.contains("✏️ 编辑 re port.md"),
            "folded to one line: {folded}"
        );
    }

    /// The header detail is charged at its JSON-escaped length, exactly like
    /// the File Content marker, so card splitting sees the bytes the header
    /// renders (ADR-0083 §8 amendment, #766).
    #[test]
    fn the_header_detail_estimate_charges_the_rendered_bytes() {
        let named = write_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "a \"quoted\".md"})),
            Some("ok"),
        );
        let detail = panel_file_detail(&named).expect("the detail");
        assert_eq!(
            panel_file_detail_estimate(&named),
            serde_json::to_string(&detail).unwrap().len() - 2,
            "the estimate is the JSON-escaped length"
        );
        assert!(
            panel_file_detail_estimate(&named) > detail.len(),
            "an escaping quote must cost its extra byte"
        );
        let anonymous = write_tool(ToolStatus::Completed, None, Some("ok"));
        assert_eq!(panel_file_detail_estimate(&anonymous), 0);
    }

    /// A filename may carry a control character — a newline above all. The
    /// folded header is a one-line `plain_text` title, so the name is folded to
    /// spaces before it is clipped: no raw control character reaches the title,
    /// and the size estimate is exactly the JSON-escaped length of the marker
    /// that now renders (ADR-0076 amendment).
    #[test]
    fn a_control_character_in_a_file_name_folds_out_of_the_panel_header() {
        let panel = read_panel(
            "report\npart.pdf",
            "application/pdf",
            b"%PDF",
            FileDelivery::Undelivered,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel.clone())
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert!(
            !title.chars().any(char::is_control),
            "no raw control character in the header title: {title:?}"
        );
        assert_eq!(
            title,
            format!("✅ {} · 📎 report part.pdf · 未发送", vocab::TOOL_READ),
            "{card}"
        );

        let marker = file_header_marker(&panel).expect("a file panel has a header marker");
        assert!(
            !marker.chars().any(char::is_control),
            "the folded marker carries no control character: {marker:?}"
        );
        assert_eq!(
            file_header_marker_estimate(&panel),
            json_escaped_len(&marker),
            "the estimate is the marker's JSON-escaped length"
        );
    }

    /// A filename may carry a Unicode line separator (`U+2028`) or paragraph
    /// separator (`U+2029`) — `Zl`/`Zp`, which `char::is_control()` does NOT
    /// catch. The folded header is a one-line `plain_text` title, so both fold
    /// to a space like any control character: no raw separator reaches the
    /// title, and the size estimate is exactly the JSON-escaped length of the
    /// marker that renders (ADR-0076 amendment).
    #[test]
    fn a_line_separator_in_a_file_name_folds_out_of_the_panel_header() {
        for sep in ['\u{2028}', '\u{2029}'] {
            let name = format!("report{sep}part.pdf");
            let panel = read_panel(&name, "application/pdf", b"%PDF", FileDelivery::Undelivered);
            let card = CardBuilder::new()
                .with_state(CardState::Done)
                .with_tool(panel.clone())
                .build();
            let title = card["body"]["elements"][0]["header"]["title"]["content"]
                .as_str()
                .expect("panel title");
            assert!(
                !title.contains(['\u{2028}', '\u{2029}']),
                "no raw line separator in the header title: {title:?}"
            );
            assert_eq!(
                title,
                format!("✅ {} · 📎 report part.pdf · 未发送", vocab::TOOL_READ),
                "{card}"
            );

            let marker = file_header_marker(&panel).expect("a file panel has a header marker");
            assert!(
                !marker.contains(['\u{2028}', '\u{2029}']),
                "the folded marker carries no line separator: {marker:?}"
            );
            assert_eq!(
                file_header_marker_estimate(&panel),
                json_escaped_len(&marker),
                "the estimate is the marker's JSON-escaped length"
            );
        }
    }

    /// A filename that STARTS with line-breaking characters folds to a run of
    /// spaces that the 24-character clip would spend before any printable
    /// character, leaving a `📎` marker with no usable name. Trimming drops the
    /// fold's leading/trailing spaces, so the printable suffix survives — and a
    /// name with nothing printable falls back to a default name — the folded
    /// header always names the file (ADR-0076 amendment).
    #[test]
    fn a_control_heavy_file_name_still_names_the_file_in_the_panel_header() {
        // 24 control characters, then a printable suffix: the clip budget would
        // be spent on the fold's spaces, so only the trimmed suffix names it.
        let leading = format!("{}shot.png", "\u{0001}".repeat(FILE_HEADER_NAME_MAX_CHARS));
        let panel = read_panel(&leading, "image/png", b"ABC", FileDelivery::Undelivered);
        assert_eq!(
            file_header_marker(&panel).as_deref(),
            Some(" · 📎 shot.png · 未发送"),
            "the printable suffix survives the trim"
        );

        // Nothing printable at all: the marker falls back to a default name
        // rather than rendering a bare, nameless `📎`.
        let all_control = "\u{0001}\u{2028}".repeat(20);
        let panel = read_panel(&all_control, "image/png", b"ABC", FileDelivery::Undelivered);
        let marker = file_header_marker(&panel).expect("a file panel has a header marker");
        assert_eq!(
            marker, " · 📎 文件 · 未发送",
            "an all-line-breaking name falls back to a default name"
        );
        assert_eq!(
            file_header_marker_estimate(&panel),
            json_escaped_len(&marker),
            "the estimate is the marker's JSON-escaped length"
        );

        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(
            title,
            format!("✅ {} · 📎 文件 · 未发送", vocab::TOOL_READ),
            "{card}"
        );
    }

    /// The header clip counts CHARACTERS, not bytes: a multibyte (CJK) filename
    /// clips at [`FILE_HEADER_NAME_MAX_CHARS`] from the TAIL without panicking or
    /// splitting a character, while the body keeps the full name (ADR-0076
    /// amendment).
    #[test]
    fn a_multibyte_file_name_clips_at_the_character_boundary() {
        let name = "截图".repeat(20); // 40 characters, 120 bytes
        let panel = read_panel(&name, "image/png", b"ABC", FileDelivery::SentAsFile);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        // `…` plus the last 23 characters, each a whole 3-byte CJK char.
        let expected: String = format!("…{}", name.chars().skip(40 - 23).collect::<String>());
        assert_eq!(
            expected.chars().count(),
            FILE_HEADER_NAME_MAX_CHARS,
            "the fixture must exceed the clip so the boundary is exercised"
        );
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert!(
            title.contains(&format!("📎 {expected}")),
            "the header clips at the character boundary from the tail: {title}"
        );
        let body = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert!(body.contains(&name), "the body keeps the full name: {body}");
    }

    /// The header clip now bites only a genuinely long BASENAME, not a path
    /// (ADR-0076 amendment): a file name longer than
    /// [`FILE_HEADER_NAME_MAX_CHARS`] is clipped to its tail — a leading `…` plus
    /// the last characters, the `…` counted against the budget — so the extension
    /// stays visible while the whole rendered name fits. The body keeps the full
    /// name.
    #[test]
    fn a_long_basename_is_clipped_keeping_its_extension() {
        let name = "a_rather_long_screenshot_name_that_repeats.png";
        assert!(
            name.chars().count() > FILE_HEADER_NAME_MAX_CHARS,
            "the fixture must exceed the clip so the boundary is exercised"
        );
        let panel = read_panel(name, "image/png", b"ABC", FileDelivery::SentAsFile);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        let shown = title.split("📎 ").nth(1).expect("the marker carries the name");
        assert!(
            shown.starts_with('…') && shown.ends_with(".png"),
            "the clip keeps the tail so the extension survives: {shown:?}"
        );
        assert_eq!(
            shown.chars().count(),
            FILE_HEADER_NAME_MAX_CHARS,
            "the `…` counts against the budget: {shown:?}"
        );
        let expected: String = format!(
            "…{}",
            name.chars().skip(name.chars().count() - 23).collect::<String>()
        );
        assert_eq!(shown, expected, "the header clips the basename from the tail");
        let body = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert!(body.contains(name), "the body keeps the full name: {body}");
    }

    /// The basename rule must never leave the marker empty (ADR-0076 amendment):
    /// a name with NO separator is already its own basename and renders whole,
    /// and a name ENDING in a separator has an empty basename, so the folded whole
    /// string is shown instead of a bare `📎`.
    #[test]
    fn a_name_with_no_or_a_trailing_separator_still_names_the_file() {
        // No separator: a bare filename is its own basename and renders unchanged.
        let bare = read_panel("shot.png", "image/png", b"ABC", FileDelivery::Undelivered);
        assert_eq!(
            file_header_marker(&bare).as_deref(),
            Some(" · 📎 shot.png · 未发送"),
            "a separator-less name is its own basename"
        );

        // A trailing separator has an empty basename: fall back to the folded
        // whole name rather than an empty marker. Both separators, and a
        // separators-only name, are covered.
        for name in ["foo/", "foo\\", "/", "dir/sub/"] {
            let panel = read_panel(name, "image/png", b"ABC", FileDelivery::Undelivered);
            let marker = file_header_marker(&panel).expect("a file panel has a header marker");
            assert_eq!(
                marker,
                format!(" · 📎 {name} · 未发送"),
                "a trailing separator falls back to the folded whole name"
            );
            assert!(!marker.contains("📎  ·"), "the marker is never empty: {marker}");
        }
    }

    /// The file marker is appended to the title without disturbing the panel's
    /// identity: the `element_id` a file-carrying panel is built with survives
    /// unchanged (ADR-0076 amendment), so the reader's fold state stays
    /// attached to the panel.
    #[test]
    fn the_file_marker_preserves_the_panel_element_id() {
        let panel = read_panel("doc.pdf", "application/pdf", b"%PDF", FileDelivery::Undelivered);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool_at(panel, None, Some("tool_call_read"), None)
            .build();
        let element = &card["body"]["elements"][0];
        assert_eq!(element["element_id"], "tool_call_read", "{card}");
        assert!(
            element["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("📎 doc.pdf"),
            "the marker is present alongside the id: {element}"
        );
    }

    /// The tracking-line size estimate charges the File Message suffixes too
    /// (#649): the block is derived from the same render, so `· 已发送为文件消息`
    /// and `· 未发送` cost their bytes exactly as `· 已内嵌` does.
    #[test]
    fn file_tracking_estimate_charges_the_file_message_suffixes() {
        let line = |delivery: FileDelivery| {
            let panel = read_panel("doc.pdf", "application/pdf", b"%PDF", delivery);
            file_tracking_estimate(&panel)
        };
        let bare = line(FileDelivery::Unresolved);
        assert!(
            line(FileDelivery::SentAsFile) > bare,
            "the 已发送为文件消息 suffix must be charged"
        );
        assert!(
            line(FileDelivery::Undelivered) > bare,
            "the 未发送 suffix must be charged"
        );
    }

    /// The tracking-line size estimate must charge the block's JSON-ESCAPED
    /// bytes, not its raw UTF-8 length: the panel body is embedded in the card's
    /// JSON `content` string, where every `"` becomes `\"` (and a `\` becomes
    /// `\\`, a control char to its escape — two bytes for the short forms, six
    /// for `\uXXXX`). A raw-length estimate under-counts a
    /// pathological filename by half, letting a card pass the split check and
    /// still be rejected by Feishu on size — which the fenced content fallback
    /// cannot repair. 16,000 quotes are ~16KB raw but ~32KB escaped.
    #[test]
    fn file_tracking_estimate_charges_json_escaping() {
        let quotes = "\"".repeat(16_000);
        let panel = read_panel(&quotes, "image/png", b"ABC", FileDelivery::Undelivered);
        let raw = file_tracking_block(&panel).expect("the panel carries a tracking block");
        let estimate = file_tracking_estimate(&panel);

        assert_eq!(
            estimate,
            raw.len() + quotes.len(),
            "each `\"` must cost its escaped `\\\"` — one extra byte"
        );
        assert!(
            estimate > raw.len(),
            "the estimate must not be the raw UTF-8 length"
        );
    }

    /// The same estimate must also charge the card's MARKDOWN sanitization: the
    /// block runs through `CardMarkdown::element` before it reaches the JSON, so
    /// every `<` a filename carries is escaped to the five-byte `&#60;`. A raw
    /// estimate would charge one byte for each and under-count by 4x.
    #[test]
    fn file_tracking_estimate_charges_markdown_escaping() {
        let angles = "<".repeat(1_000);
        let panel = read_panel(&angles, "image/png", b"ABC", FileDelivery::Undelivered);
        let raw = file_tracking_block(&panel).expect("the panel carries a tracking block");
        let estimate = file_tracking_estimate(&panel);

        assert_eq!(
            estimate,
            raw.len() + angles.len() * 4,
            "each `<` must cost its five-byte `&#60;` — four extra bytes"
        );
    }

    /// A panel with one embedded image per `name`, all delivered `Embedded` —
    /// the shape the image-size estimate tests build from.
    fn embedded_panel(names: &[&str]) -> ToolPanel {
        let files: Vec<(&str, &[u8])> = names.iter().map(|name| (*name, &b"ABC"[..])).collect();
        let mut panel = ToolPanel::for_test_with_files("read", &files);
        for i in 0..names.len() {
            panel.set_file_delivery(
                i,
                FileDelivery::Embedded {
                    image_key: format!("img_{i}"),
                },
            );
        }
        panel
    }

    /// The `img` elements an embedded File Content emits (ADR-0076) each carry
    /// the file NAME in `title` and `alt`, and the whole element is serialized
    /// into the card's JSON. The size estimate must charge those SERIALIZED
    /// bytes — not a count, and not the raw name length — so a near-budget card
    /// splits instead of being rejected by Feishu on size, which the fenced
    /// content fallback cannot repair.
    #[test]
    fn file_image_estimate_charges_the_serialized_image_elements() {
        let names = [
            "a_rather_long_screenshot_name_that_repeats.png",
            "second.png",
            "third.png",
        ];
        let panel = embedded_panel(&names);
        let elements = file_image_elements(&panel);
        assert_eq!(elements.len(), names.len(), "one img per embedded file");
        let expected: usize = elements
            .iter()
            .map(|element| serde_json::to_string(element).unwrap().len())
            .sum();

        assert_eq!(
            file_image_estimate(&panel),
            expected,
            "the estimate must be the serialized contribution of exactly the emitted elements"
        );
        assert!(
            file_image_estimate(&panel) > names.iter().map(|n| n.len()).sum::<usize>(),
            "the estimate must charge more than the raw names: the element scaffolding \
             and the JSON-escaped title copy count too"
        );
    }

    /// The image estimate must charge the element's JSON-ESCAPED bytes: the name
    /// sits in the card's JSON `title` and `alt` strings, where each `"` becomes
    /// `\"` (a `\` `\\`, a control char to its escape — two bytes for the short
    /// forms, six for `\uXXXX`). A raw name-length charge under-counts a
    /// quote-heavy filename, so a card can pass the split check and still be
    /// rejected by Feishu on size. The name sits within the header's clip budget,
    /// so every quote survives into both fields and the escaping is isolated from
    /// clipping.
    #[test]
    fn file_image_estimate_charges_json_escaping() {
        let quotes = "\"".repeat(20);
        let plain = "x".repeat(20);
        let quoted = file_image_estimate(&embedded_panel(&[&quotes]));
        let unquoted = file_image_estimate(&embedded_panel(&[&plain]));

        // Both names render the same element shape and length; only the 20 quotes
        // differ — each costs one extra byte (`"` → `\"`) in EACH of the two
        // fields (`title` and `alt`) the name fills, where a raw charge would
        // bill them the same as the plain name.
        assert_eq!(
            quoted - unquoted,
            quotes.len() * 2,
            "each quote must cost its escaped `\"` byte in both the title and the alt"
        );
        // And the estimate is exactly the emitted element's serialized size.
        let serialized = serde_json::to_string(&file_image_elements(&embedded_panel(&[&quotes]))[0])
            .unwrap()
            .len();
        assert_eq!(quoted, serialized);
    }

    /// A panel with no embedded image charges nothing for images: a delivered
    /// file (`未发送` / `已发送为文件消息`) emits no `img` element, so the image
    /// estimate must stay 0 and never inflate the card-size budget.
    #[test]
    fn file_image_estimate_is_zero_without_an_embedded_image() {
        for delivery in [
            FileDelivery::Unresolved,
            FileDelivery::Undelivered,
            FileDelivery::SentAsFile,
        ] {
            let panel = read_panel("doc.pdf", "application/pdf", b"%PDF", delivery);
            assert_eq!(
                file_image_estimate(&panel),
                0,
                "a non-embedded file emits no img element"
            );
        }
    }

    /// The whole-card content-rejection fallback fences every markdown element
    /// but MUST preserve `img` elements (ADR-0076): an image is not content the
    /// platform can reject, and fencing it would break the card entirely.
    #[test]
    fn the_content_rejection_fallback_never_fences_the_image() {
        let panel = read_panel(
            "shot.png",
            "image/png",
            b"ABC",
            FileDelivery::Embedded {
                image_key: "img_v2_x".into(),
            },
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_fenced_markdown(true)
            .with_tool(panel)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[1]["tag"], "img",
            "the image survives the fallback: {card}"
        );
        assert_eq!(elements[1]["img_key"], "img_v2_x");
        let body = elements[0]["elements"][0]["content"].as_str().unwrap();
        assert!(body.starts_with("```"), "the panel body is fenced: {body}");
    }

    #[test]
    fn tool_panel_completed_is_collapsible() {
        let tool = read_tool(
            ToolStatus::Completed,
            Some(json!("src/main.rs")),
            Some("fn main() {}"),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let panel = &elements[0];
        assert_eq!(panel["tag"].as_str().unwrap(), "collapsible_panel");
        assert!(
            panel["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains(&format!("✅ {}", vocab::TOOL_READ))
        );
        assert!(panel.to_string().contains("fn main() {}"));
    }

    /// The panel's status presentation is the typed status, not a string: the
    /// icon table keeps the card's historical icons (a missing status reads as
    /// the historical completed default, "failed" still reads as a failure)
    /// and liveness/run classification comes from [`ToolStatus`] itself.
    #[test]
    fn typed_status_drives_the_icon_and_liveness() {
        let icon = |status| ToolPanel::for_test("bash", status, None, None).status_icon();
        assert_eq!(icon(ToolStatus::Pending), "⏳");
        assert_eq!(icon(ToolStatus::Running), "⏳");
        assert_eq!(icon(ToolStatus::Completed), "✅");
        assert_eq!(icon(ToolStatus::Error), "❌");
        assert_eq!(icon(ToolStatus::Unknown), "✅");
        assert_eq!(icon(ToolStatus::Other("failed".into())), "❌");
        assert_eq!(icon(ToolStatus::Other("weird".into())), "🔧");

        let live = |status| ToolPanel::for_test("bash", status, None, None).is_live();
        assert!(live(ToolStatus::Pending) && live(ToolStatus::Running));
        assert!(!live(ToolStatus::Completed) && !live(ToolStatus::Error));
        assert!(!live(ToolStatus::Unknown) && !live(ToolStatus::Other("weird".into())));

        // Only a RUNNING call drives the header's "⏳ tool" hint; a pending one
        // is live tail content but not the running tool (ADR-0014/ADR-0045).
        let running = |status| ToolPanel::for_test("bash", status, None, None).is_running();
        assert!(running(ToolStatus::Running));
        assert!(!running(ToolStatus::Pending));
    }

    /// ADR-0060 (#416, restyled by #423): a settled `shell`/`subagent` call
    /// whose metadata says its run is still going renders the 🌙 marker in its
    /// status slot instead of ✅ — it keeps its timeline place as the record of
    /// the request and never claims completion, before AND after the run
    /// retires (the marker is durable). A foreground call, a V1 `task` and a
    /// launch that has not returned its handle yet render exactly as today.
    #[test]
    fn a_backgrounded_call_renders_the_launch_marker() {
        let at = crate::feishu::card::test_local_ms(2026, 9, 16, 14, 5);
        let panel = |name: &str, status: ToolStatus, metadata: Option<serde_json::Value>| {
            CardBuilder::new()
                .with_state(CardState::Done)
                .with_tool_at(
                    ToolPanel::new(ToolCall {
                        identity: ToolIdentity {
                            name: name.into(),
                            call_id: format!("call_{name}"),
                        },
                        status,
                        started_at: None,
                        input: None,
                        metadata,
                        output: ToolOutput::default(),
                        ..ToolCall::default()
                    }),
                    Some(at),
                    None,
                    None,
                )
                .build()
        };
        let title = |card: &serde_json::Value| {
            card["body"]["elements"][0]["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .to_string()
        };

        // The pinned marker: the icon sits in the status slot, the title's
        // other parts are a normal panel's. The metadata keeps saying `running`
        // after a run retires, so the marker is the same before and after.
        let running = Some(json!({"status": "running", "shellID": "sh_1"}));
        assert_eq!(
            title(&panel("shell", ToolStatus::Completed, running.clone())),
            "🌙 shell · 14:05"
        );
        assert_eq!(
            title(&panel(
                "subagent",
                ToolStatus::Completed,
                Some(json!({"status": "running", "sessionID": "ses_1"}))
            )),
            "🌙 subagent · 14:05"
        );
        assert_eq!(BACKGROUNDED_MARKER, "🌙", "the pinned marker");

        // Foreground control: the run finished inside the call — its metadata
        // says `completed`, or names no run status at all.
        assert_eq!(
            title(&panel(
                "shell",
                ToolStatus::Completed,
                Some(json!({"status": "completed"}))
            )),
            format!("✅ {} · 14:05", vocab::TOOL_SHELL)
        );
        assert_eq!(
            title(&panel("shell", ToolStatus::Completed, None)),
            format!("✅ {} · 14:05", vocab::TOOL_SHELL)
        );
        // A V1 `task` call records no run status: unchanged.
        assert_eq!(
            title(&panel(
                "task",
                ToolStatus::Completed,
                Some(json!({"sessionId": "ses_child"}))
            )),
            "✅ task · 14:05"
        );
        // A launch that has not returned its handle yet is still live: ⏳, not
        // the launch marker (it only holds once the call settled).
        assert_eq!(
            title(&panel("shell", ToolStatus::Running, running)),
            format!("⏳ {} · 14:05", vocab::TOOL_SHELL)
        );
    }

    /// #183: the tool panel's header shows the call's start time (`HH:MM`,
    /// local), visible while the panel is collapsed. A panel with no server
    /// time (a pending part, a fallback key) shows no clock.
    #[test]
    fn tool_panel_header_carries_the_start_time() {
        let at = crate::feishu::card::test_local_ms(2026, 9, 16, 14, 5);
        let tool = ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            Some(json!({"command": "cargo test"})),
            Some("ok"),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool_at(tool.clone(), Some(at), None, None)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[0]["header"]["title"]["content"].as_str().unwrap(),
            "✅ bash · 14:05"
        );

        let untimed = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool_at(tool, None, None, None)
            .build();
        let elements = untimed["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[0]["header"]["title"]["content"].as_str().unwrap(),
            "✅ bash"
        );
    }

    /// ADR-0054: a live task panel carries its child session's liveness in the
    /// collapsed title — activity age, current tool, and the child's wait.
    #[test]
    fn live_task_panel_header_carries_child_liveness() {
        let mut tool = ToolPanel::for_test(
            "task",
            ToolStatus::Running,
            Some(json!({"description": "review"})),
            None,
        );
        tool.set_liveness(Some(TaskLiveness {
            activity: ChildActivity::Tool {
                name: "bash".into(),
                started_at: Some(chrono::Utc::now().timestamp_millis() - 12_000),
            },
            last_activity_ms: Some(chrono::Utc::now().timestamp_millis() - 12_000),
            wait: Some(AwaitingAction::Permission),
        }));
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_tool(tool)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let title = elements[0]["header"]["title"]["content"].as_str().unwrap();
        assert!(
            title.starts_with("⏳ task · ")
                && title.contains("bash 1")
                && title.contains(crate::feishu::card::vocab::AWAITING_PERMISSION),
            "the collapsed task title carries the child activity before its wait: {title}"
        );
    }

    /// The liveness line is live-only: a settled panel drops it even if a
    /// stale snapshot was never cleared.
    #[test]
    fn settled_task_panel_drops_child_liveness() {
        let mut tool = ToolPanel::for_test("task", ToolStatus::Completed, None, Some("done"));
        tool.set_liveness(Some(TaskLiveness {
            activity: ChildActivity::Phase(Phase::Waiting),
            last_activity_ms: Some(1),
            wait: None,
        }));
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[0]["header"]["title"]["content"].as_str().unwrap(),
            "✅ task"
        );
    }

    /// The fragment names the activity first and its elapsed time second —
    /// the header's own shape. A phase with no tool falls back to the time
    /// since the newest activity; an untimed tool shows its name alone.
    #[test]
    fn liveness_fragment_names_the_activity_before_its_elapsed_time() {
        let now = 1_000_000;
        let tool = |ago_ms: i64, name: &str| TaskLiveness {
            activity: ChildActivity::Tool {
                name: name.into(),
                started_at: Some(now - ago_ms),
            },
            last_activity_ms: Some(now - ago_ms),
            wait: None,
        };
        assert_eq!(tool(5_000, "bash").title_fragment(now), "bash 5s");
        assert_eq!(tool(7_200_000, "bash").title_fragment(now), "bash 2h0m");

        let thinking = TaskLiveness {
            activity: ChildActivity::Phase(Phase::Waiting),
            last_activity_ms: Some(now - 90_000),
            wait: Some(AwaitingAction::Both),
        };
        assert_eq!(
            thinking.title_fragment(now),
            format!("{} 1m30s · 等待你的授权/回答", phase_token(Phase::Waiting))
        );

        let untimed = TaskLiveness {
            activity: ChildActivity::Tool {
                name: "bash".into(),
                started_at: None,
            },
            last_activity_ms: Some(now - 1_000),
            wait: None,
        };
        assert_eq!(untimed.title_fragment(now), "bash");
        // The age key the fragment's elapsed counts from — the tool's start,
        // or the phase's newest activity; an untimed tool has none, so a
        // reader keyed on the fragment knows there is no number to tick.
        assert_eq!(tool(5_000, "bash").age_clock_ms(), Some(now - 5_000));
        assert_eq!(thinking.age_clock_ms(), Some(now - 90_000));
        assert_eq!(untimed.age_clock_ms(), None);
    }

    /// Review finding 7: an activity with no clock at all renders its phase
    /// word with NO age — never an age measured from the epoch.
    #[test]
    fn a_run_activity_with_no_clock_renders_no_age() {
        let activity = RunActivity {
            call: None,
            phase: Some(Phase::Waiting),
            awaiting: None,
            since: None,
        };
        let untimed = TaskLiveness::from_run(&activity);
        assert_eq!(untimed.age_clock_ms(), None);
        assert_eq!(untimed.title_fragment(1_000_000), phase_token(Phase::Waiting));

        let timed = TaskLiveness::from_run(&RunActivity {
            call: None,
            phase: Some(Phase::Reasoning),
            awaiting: None,
            since: Some(990_000),
        });
        assert_eq!(
            timed.title_fragment(1_000_000),
            format!("{} 10s", phase_token(Phase::Reasoning))
        );
    }

    /// A report updates its run; it is not a receipt and not a panel. Only an
    /// addressing call renders a receipt.
    #[test]
    fn a_report_is_not_a_receipt() {
        let report = ToolCall {
            identity: ToolIdentity {
                name: "codex/sub_agent_activity".into(),
                call_id: "call_activity".into(),
            },
            status: ToolStatus::Completed,
            detail: ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_1".into(),
                    status: None,
                }],
                effect: DelegationEffect::Report,
            },
            ..ToolCall::default()
        };
        assert!(is_report_call(&report));
        assert!(
            !is_receipt_call(&report),
            "a report is not a receipt: it updates its run only"
        );

        let address = ToolCall {
            identity: ToolIdentity {
                name: "wait".into(),
                call_id: "call_wait".into(),
            },
            input: Some(serde_json::json!({"tool": "wait"})),
            detail: ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_1".into(),
                    status: None,
                }],
                effect: DelegationEffect::Address,
            },
            ..ToolCall::default()
        };
        assert!(is_receipt_call(&address));
        assert!(!is_report_call(&address));
        assert_eq!(receipt_line(&address), "↪ wait · thread_1");
    }

    /// The child session id is the LAUNCH relation's, not a tool name's (spec
    /// #1057's P4): a call that launched a run names the child its
    /// `Delegation` ran, and nothing else does — a metadata field alone is no
    /// longer enough, and an addressing call never names one.
    #[test]
    fn a_launch_relation_names_the_child_session() {
        fn launched(name: &str, child: &str) -> ToolPanel {
            ToolPanel::new(ToolCall {
                identity: ToolIdentity {
                    name: name.into(),
                    call_id: format!("call_{child}"),
                },
                detail: ToolDetail::Delegation {
                    runs: vec![crate::backend::AgentRunRef {
                        run_id: child.into(),
                        status: None,
                    }],
                    effect: DelegationEffect::Launch,
                },
                ..ToolCall::default()
            })
        }

        assert_eq!(
            launched("task", "ses_child").child_session_id(),
            Some("ses_child")
        );
        assert_eq!(
            launched("subagent", "ses_v2_child").child_session_id(),
            Some("ses_v2_child"),
            "the relation, not the vendor id, decides"
        );

        // Metadata alone is no launch fact: the old name check is gone.
        let metadata_only = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_bare".into(),
            },
            metadata: Some(json!({"sessionID": "ses_v2_child"})),
            ..ToolCall::default()
        });
        assert_eq!(metadata_only.child_session_id(), None);

        // An addressing call is a receipt on an existing run: it launches
        // nothing.
        let addressing = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "collab".into(),
                call_id: "call_wait".into(),
            },
            detail: ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "thread_1".into(),
                    status: None,
                }],
                effect: DelegationEffect::Address,
            },
            ..ToolCall::default()
        });
        assert_eq!(addressing.child_session_id(), None);

        assert_eq!(
            ToolPanel::for_test("bash", ToolStatus::Running, None, None).child_session_id(),
            None
        );

        // A launch naming SEVERAL children ranks no child above another, so
        // the panel takes no liveness source from it (the ledger reads them
        // all from the runs instead).
        let shared = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "codex/collab_agent".into(),
                call_id: "call_spawn".into(),
            },
            detail: ToolDetail::Delegation {
                runs: ["thread_a", "thread_b"]
                    .iter()
                    .map(|id| crate::backend::AgentRunRef {
                        run_id: (*id).into(),
                        status: None,
                    })
                    .collect(),
                effect: DelegationEffect::Launch,
            },
            ..ToolCall::default()
        });
        assert_eq!(shared.child_session_id(), None);
    }

    #[test]
    fn read_tool_output_strips_xml_wrapper() {
        let raw = "\
<path>/root/workspace/dev/cola/src/main.rs</path>
<type>file</type>
<content>
1: fn main() {
2:     println!(\"hi\");
3: }
</content>";
        let (header, style, body) = read_output(raw);
        assert_eq!(style, BodyStyle::Code(Some("rust")), "language hint from .rs");
        let header = header.unwrap_or_default();
        assert!(
            !header.contains("<path>"),
            "path tag must be stripped: {}",
            header
        );
        assert!(
            !body.contains("<content>"),
            "content tag must be stripped: {}",
            body
        );
        assert!(
            !body.contains("</content>"),
            "closing tag must be stripped: {}",
            body
        );
        assert!(!body.contains("<type>"), "type tag must be stripped: {}", body);
        assert!(
            header.contains("/root/workspace/dev/cola/src/main.rs"),
            "path must still be shown: {}",
            header
        );
        assert!(
            body.contains("fn main() {"),
            "the actual code must be kept: {}",
            body
        );
    }

    #[test]
    fn non_read_tool_output_passes_through() {
        let raw = "some\nplain output";
        assert_eq!(format_named_output("bash", raw).2, raw);
        assert_eq!(format_named_output("bash", raw).0, None);
        // Even a read-named tool without the wrapper is left alone.
        assert_eq!(read_output("no wrapper here").2, "no wrapper here");
    }

    /// #783: the V1-vs-V2 read decision keys off the output's **head**, not a
    /// whole-output `<path>` search. A V2 read whose body merely quotes the
    /// literal `<path>` (any file documenting it, this repo's README included)
    /// must keep its path header, its language token and its V2 first line out
    /// of the body — the old `contains("<path>")` test sent it down the V1
    /// branch, whose line scan found no wrapper and dropped all three.
    #[test]
    fn read_v2_output_with_path_literal_in_body_stays_v2() {
        let raw = "Read file /root/workspace/dev/cola/README.md, lines 1-2\n\
                   1: see `cola --config <path>`\n\
                   2: done";
        let (header, style, body) = read_output(raw);
        assert_eq!(
            header.as_deref(),
            Some("📄 `/root/workspace/dev/cola/README.md`"),
            "path header comes from the V2 first line, not empty"
        );
        assert_eq!(
            style,
            BodyStyle::Code(Some("markdown")),
            "language token from .md must not be lost"
        );
        assert!(
            !body.contains("Read file "),
            "the V2 header line must not leak into the body: {}",
            body
        );
        assert!(
            body.contains("<path>"),
            "the body's own literal must still be rendered: {}",
            body
        );
    }

    /// The head test tolerates a leading blank line (`trim_start`), matching the
    /// V1 line loop that skips one — so a decorated V1 output still takes the V1
    /// branch and keeps its path and language.
    #[test]
    fn read_v1_output_with_leading_blank_line_still_parses() {
        let raw = "\n<path>/x/y.rs</path>\n<type>file</type>\n<content>\n1: use std::fs;\n</content>";
        let (header, style, body) = read_output(raw);
        assert_eq!(header.as_deref(), Some("📄 `/x/y.rs`"));
        assert_eq!(style, BodyStyle::Code(Some("rust")));
        assert!(!body.contains("<path>"), "wrapper stripped: {}", body);
        assert!(body.contains("use std::fs"), "code kept: {}", body);
    }

    /// #202: a `task` output is an XML envelope (`<task id state>`) around the
    /// subagent's report. The panel shows the report alone — the wrapper must
    /// not leak, and a background task's `<summary>` becomes the header.
    #[test]
    fn task_output_strips_the_xml_envelope() {
        let raw = "\
<task id=\"ses_1\" state=\"completed\">
<task_result>
Report line 1

Report line 2
</task_result>
</task>";
        let (header, style, body) = delegation_output(raw);
        assert_eq!(header, None, "no summary, no header");
        assert_eq!(style, BodyStyle::Auto);
        assert!(body.starts_with("Report line 1"), "report kept: {body:?}");
        assert!(body.contains("Report line 2"), "report kept: {body:?}");
        assert!(!body.contains("<task"), "wrapper stripped: {body:?}");
        assert!(!body.contains("task_result"), "wrapper stripped: {body:?}");
    }

    /// A background/error task carries a `<summary>` (the header) and its text
    /// under `<task_error>`; a task output without the envelope passes through.
    #[test]
    fn task_output_keeps_summary_and_error_text() {
        let raw = "\
<task id=\"ses_1\" state=\"error\">
<summary>Background task failed: research</summary>
<task_error>
boom
</task_error>
</task>";
        let (header, _, body) = delegation_output(raw);
        assert_eq!(header.as_deref(), Some("Background task failed: research"));
        assert_eq!(body.trim(), "boom");
        assert_eq!(delegation_output("plain text").2, "plain text");
    }

    /// An empty `<task_result>` must still lose its envelope — dumping the raw
    /// XML back on the card is worse than an empty output.
    #[test]
    fn task_output_with_an_empty_result_still_strips_the_envelope() {
        let raw = "<task id=\"ses_1\" state=\"completed\">\n<task_result>\n</task_result>\n</task>";
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "task".into(),
                call_id: "call_task".into(),
            },
            kind: ToolKind::Other,
            status: ToolStatus::Completed,
            input: Some(json!({"description": "sub"})),
            output: ToolOutput {
                raw: Some(json!(raw)),
                blocks: vec![ContentBlock::Text(raw.to_string())],
                error: None,
            },
            detail: ToolDetail::Delegation {
                runs: vec![crate::backend::AgentRunRef {
                    run_id: "ses_child".into(),
                    status: None,
                }],
                effect: DelegationEffect::Launch,
            },
            ..ToolCall::default()
        });
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(!text.contains("<task"), "envelope leaked: {text}");
    }

    /// #202: a `skill` output is a `<skill_content>` XML envelope; the panel
    /// shows the skill's instructions alone (the sampled `<skill_files>` list is
    /// a file inventory the reader can't use).
    #[test]
    fn skill_output_strips_the_envelope_and_file_list() {
        let raw = "\
<skill_content name=\"implement\">
# Skill: implement

Do the work.

Base directory for this skill: /root/.agents/skills/implement
Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.

<skill_files>
<file>/root/.agents/skills/implement/SKILL.md</file>
<file>/root/.agents/skills/implement/other.md</file>
</skill_files>
</skill_content>";
        let (header, style, body) = format_named_output("skill", raw);
        assert_eq!(header, None, "the input names the skill");
        assert_eq!(style, BodyStyle::Auto);
        assert!(body.contains("Do the work."), "content kept: {body:?}");
        assert!(!body.contains("<skill_content"), "wrapper stripped: {body:?}");
        assert!(!body.contains("skill_files"), "file list stripped: {body:?}");
        assert!(!body.contains("other.md"), "sampled files stripped: {body:?}");
        // An empty envelope is stripped too, never leaked raw.
        let (_, _, empty) = format_named_output("skill", "<skill_content name=\"x\">\n</skill_content>");
        assert_eq!(empty, "");
    }

    /// #202: a `websearch` output is a JSON result envelope. The panel shows a
    /// title+url list (excerpts are the model's reading material) and it stays
    /// markdown even when a URL trips the long-line fence — fencing would show
    /// the literal `1. [title](url)` syntax.
    #[test]
    fn websearch_output_renders_as_an_unfenced_result_list() {
        let long_url = format!("https://example.com/{}", "x".repeat(120));
        let raw = json!({
            "search_id": "search_1",
            "results": [
                {"url": "https://a.example/one", "title": "First result",
                 "publish_date": "2026-01-02", "excerpts": ["ignored"]},
                {"url": long_url, "title": "Second result",
                 "publish_date": null, "excerpts": ["ignored too"]},
            ]
        })
        .to_string();
        let (header, _, body) = web_output(&raw);
        assert_eq!(header.as_deref(), Some("🔎 2 条结果"));
        assert!(
            body.contains("1. [First result](https://a.example/one) · 2026-01-02"),
            "dated first result: {body}"
        );
        assert!(body.contains("2. [Second result]("), "second result: {body}");
        assert!(!body.contains("ignored"), "excerpts dropped: {body}");

        let tool = search_tool(
            "websearch",
            SearchScope::Web,
            ToolStatus::Completed,
            Some(json!({"query": "x"})),
            Some(&raw),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(
            md.contains("1. [First result](https://a.example/one)"),
            "list on card: {md}"
        );
        assert!(!md.contains("```"), "the list must not be fenced: {md}");

        // An unparseable output (the provider's no-results text) passes through.
        let plain = web_output("No search results found.");
        assert_eq!(plain.0, None);
        assert_eq!(plain.2, "No search results found.");
    }

    /// The `exa` provider — the other half of websearch's per-session provider
    /// selection — returns plain-text blocks, not the `parallel` JSON envelope.
    /// The panel must render the same title+url list (highlights dropped).
    #[test]
    fn websearch_exa_text_renders_as_a_result_list() {
        let raw = "\
Title: First doc
URL: https://a.example/doc
Published: 2026-01-02
Author: someone
Highlights:
long excerpt that must not appear

---

Title: N/A
URL: https://b.example/other
Published: N/A
Author: N/A
Highlights:
more text";
        let (header, _, body) = web_output(raw);
        assert_eq!(header.as_deref(), Some("🔎 2 条结果"));
        assert!(
            body.contains("1. [First doc](https://a.example/doc) · 2026-01-02"),
            "dated first result: {body}"
        );
        assert!(
            body.contains("2. https://b.example/other"),
            "an N/A title falls back to the url: {body}"
        );
        assert!(!body.contains("excerpt"), "highlights dropped: {body}");
    }

    #[test]
    fn edit_tool_output_keeps_hunks_and_their_context() {
        // A real OpenCode edit diff (from state.metadata.diff). The card should
        // keep the @@ / context / + / - lines with the change count and drop the
        // header noise, so a change reads in place — the leading space of a
        // context line is diff syntax and stays.
        let diff = "\
Index: /x/src/main.rs
===================================================================
--- /x/src/main.rs
+++ /x/src/main.rs
@@ -10,3 +10,3 @@
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;
\\ No newline at end of file";
        let parsed = parse_edit_diff(diff).expect("a real edit diff parses");
        let header = format!("+{} −{}", parsed.additions, parsed.deletions);
        let body = parsed.body;
        assert_eq!(header, "+1 −1", "count header: {header:?}");
        assert_eq!(
            diff_language_for(parsed.paths.iter().map(String::as_str)),
            "rust",
            "the hunks take the edited file's language"
        );
        assert!(body.contains("@@ -10,3 +10,3 @@"), "hunk header kept: {}", body);
        assert!(body.contains("-let b = 2;"), "removed line kept: {}", body);
        assert!(body.contains("+let b = 3;"), "added line kept: {}", body);
        assert!(body.contains(" let a = 1;"), "context line kept: {}", body);
        assert!(body.contains(" let c = 4;"), "context line kept: {}", body);
        assert!(!body.contains("Index:"), "header noise dropped: {}", body);
        assert!(!body.contains("+++"), "header noise dropped: {}", body);
    }

    #[test]
    fn edit_tool_plain_output_passes_through() {
        // An edit without a parseable diff (running / error text) shows its
        // raw error text; the adapter records no hunk to render.
        let raw = "❌ Could not find oldString in the file.";
        assert_eq!(parse_edit_diff(raw), None);
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "edit".into(),
                call_id: "call_edit".into(),
            },
            status: ToolStatus::Error,
            started_at: None,
            input: Some(json!({"filePath": "src/main.rs"})),
            output: ToolOutput {
                raw: Some(json!(raw)),
                blocks: vec![ContentBlock::Text(raw.into())],
                error: Some(raw.into()),
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(vec![FileEdit {
                path: "src/main.rs".into(),
                action: FileAction::Update,
                rename_to: None,
                diff: None,
            }]),
            ..ToolCall::default()
        });
        let md = panel_md(tool);
        assert!(md.contains(raw), "the error text is the body: {md}");
    }

    #[test]
    fn edit_tool_output_renders_diff_in_panel() {
        let diff = "\
Index: src/main.rs
===================================================================
--- src/main.rs
+++ src/main.rs
@@ -1,4 +1,4 @@
 use std::fs;
-fn main() {}
+fn main() { println!(\"hi\"); }
";
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "edit".into(),
                call_id: "call_edit".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"filePath": "src/main.rs"})),
            // The real diff the tool records in its metadata; the text output
            // is only the success sentence.
            metadata: Some(json!({ "diff": diff })),
            output: ToolOutput {
                raw: Some(json!("Edit applied successfully.")),
                blocks: vec![ContentBlock::Text("Edit applied successfully.".into())],
                error: None,
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(vec![file_edit("src/main.rs", FileAction::Update, diff)]),
            ..ToolCall::default()
        });
        assert_eq!(
            diff_truncation_note(&tool),
            None,
            "a diff that fits carries no note"
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(
            md.contains(&format!("{}\n✏️ 编辑 `src/main.rs`", input_md())),
            "file in input: {}",
            md
        );
        assert!(
            md.contains(&format!("{}\n\n+1 −1", output_md())),
            "count header: {}",
            md
        );
        // Hunks render inside a fenced code block (monospace, no wrapping),
        // with the edited file's own language so the syntax is coloured.
        assert!(md.contains("```rust\n@@ -1,4 +1,4 @@"), "fenced hunk: {}", md);
        assert!(md.contains("-fn main() {}"), "removed line: {}", md);
        assert!(md.contains("+fn main() { println"), "added line: {}", md);
        assert!(md.contains(" use std::fs;"), "context line kept: {}", md);
        assert!(md.contains("\n```"), "closing fence: {}", md);
    }

    /// A Codex `fileChange` panel: the adapter writes one `{path, kind, diff}`
    /// per touched file to `metadata.changes`, the diffs are bare hunks (no
    /// `Index:`/`---` header), and the text output is those diffs joined.
    fn codex_file_change_panel(changes: Vec<serde_json::Value>) -> ToolPanel {
        let diffs = changes
            .iter()
            .filter_map(|change| change.get("diff").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        let text = diffs.join("\n");
        let input_changes = changes
            .iter()
            .map(|change| json!({"path": change["path"], "kind": change["kind"]}))
            .collect::<Vec<_>>();
        // The adapter's own mapping, mirrored for the renderer's fixtures: the
        // wire `kind` union picks the action, `movePath` makes it a rename, and
        // a per-file `diff` is reduced to its hunks.
        let edits = changes
            .iter()
            .map(|change| {
                let (action, rename_to) = match change["kind"]["type"].as_str() {
                    Some("add") => (FileAction::Add, None),
                    Some("delete") => (FileAction::Delete, None),
                    _ => match change["kind"]["movePath"].as_str() {
                        Some(destination) => (FileAction::Rename, Some(destination.to_string())),
                        None => (FileAction::Update, None),
                    },
                };
                FileEdit {
                    path: change["path"].as_str().unwrap_or_default().to_string(),
                    action,
                    rename_to,
                    diff: change["diff"].as_str().and_then(crate::backend::file_edit_hunks),
                }
            })
            .collect();
        ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "codex/file_change".into(),
                call_id: "call_file_change".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({ "changes": input_changes })),
            metadata: Some(json!({ "changes": changes })),
            output: ToolOutput {
                raw: (!text.is_empty()).then(|| json!(text)),
                blocks: if text.is_empty() {
                    Vec::new()
                } else {
                    vec![ContentBlock::Text(text)]
                },
                error: None,
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(edits),
            ..ToolCall::default()
        })
    }

    /// #902: a Codex `file_change` renders the same diff panel an `edit` does —
    /// the `+N −N` count, the hunks in a language-coloured fence and the
    /// 「📂 打开」 button for its one file — instead of the generic path fencing
    /// the joined raw diff as one opaque code block.
    #[test]
    fn codex_file_change_renders_the_same_diff_panel_as_edit() {
        let tool = codex_file_change_panel(vec![json!({
            "path": "src/main.rs",
            "kind": {"type": "update", "movePath": null},
            "diff": "@@ -1,4 +1,4 @@\n use std::fs;\n-fn main() {}\n+fn main() { println!(\"hi\"); }\n",
        })]);
        assert_eq!(
            diff_truncation_note(&tool),
            None,
            "a diff that fits carries no note"
        );
        let md = panel_md_with_session(tool.clone(), "sess");
        assert!(
            md.contains(&format!("{}\n✏️ 编辑 `src/main.rs`", input_md())),
            "file in input: {md}"
        );
        assert!(
            md.contains(&format!("{}\n\n+1 −1", output_md())),
            "count header: {md}"
        );
        assert!(
            md.contains("```rust\n@@ -1,4 +1,4 @@"),
            "fenced hunk with the file's language: {md}"
        );
        assert!(md.contains("-fn main() {}"), "removed line: {md}");
        assert!(md.contains("+fn main() { println"), "added line: {md}");
        assert!(md.contains(" use std::fs;"), "context line kept: {md}");
        assert!(md.contains("\n```"), "closing fence: {md}");

        let title = panel_title(tool.clone());
        assert!(
            title.contains(crate::feishu::card::vocab::TOOL_EDIT),
            "own vocabulary: {title}"
        );
        assert!(
            title.contains("· ✏️ 编辑 main.rs"),
            "named with its action while folded: {title}"
        );

        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool_at(tool, None, Some("tool_call"), Some("sess"))
            .build();
        assert!(
            card.to_string().contains("📂 打开"),
            "a single file has the way in: {card}"
        );
    }

    /// A multi-file `file_change` names no single path: its folded title takes
    /// the count and totals the parsed diff already produces, every file stays
    /// attributable inside the fold, and no 「📂 打开」 button promises a file
    /// the panel cannot choose.
    #[test]
    fn codex_file_change_with_several_files_counts_them_in_the_title() {
        // The first diff ends with a newline, as a real wire diff may: joining
        // it verbatim would leave a blank line between files, which the diff
        // parser reads as the trailing-note boundary.
        let tool = codex_file_change_panel(vec![
            json!({
                "path": "src/a.rs",
                "kind": {"type": "update"},
                "diff": "@@ -1 +1 @@\n-old_a\n+new_a\n",
            }),
            json!({
                "path": "src/b.rs",
                "kind": {"type": "add"},
                "diff": "@@ -0,0 +1 @@\n+new_b",
            }),
        ]);
        let title = panel_title(tool.clone());
        assert!(
            title.contains("· 📄 2 files · +2 −1"),
            "count and totals: {title}"
        );
        let md = panel_md_with_session(tool.clone(), "sess");
        assert!(md.contains("✏️ 编辑 `src/a.rs`"), "first file named: {md}");
        assert!(md.contains("➕ 新增 `src/b.rs`"), "second file named: {md}");
        assert!(
            md.contains("```rust\n@@ -1 +1 @@"),
            "one fence for the patch, with the shared language: {md}"
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool_at(tool, None, Some("tool_call"), Some("sess"))
            .build();
        assert!(
            !card.to_string().contains("📂 打开"),
            "no single file to open: {card}"
        );
    }

    /// The file count and the open target come from the input's `changes`, not
    /// from whichever entries recorded a diff: an entry with no diff still
    /// counts and is named, a rename opens its destination, and a delete names
    /// the file it removed while offering no way in.
    #[test]
    fn codex_file_change_counts_every_change_and_opens_the_right_file() {
        let tool = codex_file_change_panel(vec![
            json!({
                "path": "src/a.rs",
                "kind": {"type": "update", "movePath": null},
                "diff": "@@ -1 +1 @@\n-old\n+new",
            }),
            json!({
                "path": "src/b.rs",
                "kind": {"type": "delete"},
                "diff": "@@ -1 +0,0 @@\n-gone",
            }),
            json!({"path": "assets/logo.png", "kind": {"type": "add"}}),
        ]);
        let title = panel_title(tool.clone());
        assert!(
            title.contains("· 📄 3 files · +1 −2"),
            "the diff-less file still counts: {title}"
        );
        let md = panel_md(tool.clone());
        assert!(
            md.contains("➕ 新增 `assets/logo.png`"),
            "the diff-less file is named in the body: {md}"
        );
        assert_eq!(tool.file_open_path(), None, "three files have no single target");

        // A rename names and opens the SAME file: its destination.
        let rename = codex_file_change_panel(vec![json!({
            "path": "src/old.rs",
            "kind": {"type": "update", "movePath": "src/new.rs"},
            "diff": "@@ -1 +1 @@\n-old\n+new",
        })]);
        assert_eq!(rename.file_open_path().as_deref(), Some("src/new.rs"));
        assert!(
            panel_title(rename).contains("· 📦 重命名 old.rs → new.rs"),
            "the title names both ends of the rename"
        );

        let delete = codex_file_change_panel(vec![json!({
            "path": "src/gone.rs",
            "kind": {"type": "delete"},
            "diff": "@@ -1 +0,0 @@\n-gone",
        })]);
        assert_eq!(delete.file_open_path(), None, "a deleted file cannot open");
        assert!(
            panel_title(delete).contains("· 🗑 删除 gone.rs"),
            "the folded title still names what was deleted"
        );

        // A rename/binary-only patch parses no hunk but still counts its files.
        let no_hunks = codex_file_change_panel(vec![
            json!({"path": "assets/a.bin", "kind": {"type": "add"}}),
            json!({"path": "assets/b.bin", "kind": {"type": "delete"}}),
        ]);
        assert!(
            panel_title(no_hunks).contains("· 📄 2 files · +0 −0"),
            "the input count survives a patch with no hunks"
        );

        // A multi-file rename names its destination in the body too, and the
        // fence follows the destination (here `.py`, not the source `.rs`).
        let multi_rename = codex_file_change_panel(vec![
            json!({
                "path": "src/old.rs",
                "kind": {"type": "update", "movePath": "src/new.py"},
                "diff": "@@ -1 +1 @@\n-old\n+new",
            }),
            json!({
                "path": "lib/other.rs",
                "kind": {"type": "update", "movePath": "lib/other.py"},
                "diff": "@@ -1 +1 @@\n-a\n+b",
            }),
        ]);
        let md = panel_md(multi_rename);
        assert!(
            md.contains("📦 重命名 `src/old.rs` → `src/new.py`"),
            "destination in body: {md}"
        );
        assert!(
            md.contains("📦 重命名 `lib/other.rs` → `lib/other.py`"),
            "second: {md}"
        );
        assert!(
            md.contains("```python"),
            "the fence follows the destination: {md}"
        );
    }

    /// A non-hunk `diff` (a binary change's message) must not poison the files
    /// after it: it rides the diff's tail, and the later hunks still render.
    #[test]
    fn codex_file_change_binary_entry_does_not_swallow_the_later_hunks() {
        let tool = codex_file_change_panel(vec![
            json!({
                "path": "assets/logo.png",
                "kind": {"type": "add"},
                "diff": "Binary files a/logo.png and b/logo.png differ",
            }),
            json!({
                "path": "src/main.rs",
                "kind": {"type": "update"},
                "diff": "@@ -1 +1 @@\n-old\n+new",
            }),
        ]);
        let title = panel_title(tool.clone());
        assert!(
            title.contains("· 📄 2 files · +1 −1"),
            "count and totals: {title}"
        );
        let md = panel_md(tool);
        assert!(
            md.contains("✏️ 编辑 `src/main.rs`"),
            "the later hunk still renders: {md}"
        );
        assert!(md.contains("+new"), "the later hunk body: {md}");
        assert!(
            md.contains("Binary files a/logo.png"),
            "the note rides the tail: {md}"
        );
    }

    /// #1060: one FileEdits renderer, chosen by `(kind, detail)`, names each
    /// file's action for both producers — Codex's `file_change` and OpenCode's
    /// `patch` share the shape, so they share the words, totals and fence.
    #[test]
    fn one_file_edits_renderer_names_each_action_for_each_producer() {
        let diff = "@@ -1 +1 @@\n-old\n+new";
        let cases = [
            (FileAction::Add, "a.rs", None, "➕ 新增"),
            (FileAction::Update, "b.rs", None, "✏️ 编辑"),
            (FileAction::Delete, "c.rs", None, "🗑 删除"),
            (FileAction::Rename, "old.rs", Some("new.rs"), "📦 重命名"),
        ];
        for producer in ["write", "edit", "apply_patch", "patch", "codex/file_change"] {
            for (action, path, rename_to, word) in cases {
                let edit = FileEdit {
                    path: path.to_string(),
                    action,
                    rename_to: rename_to.map(str::to_string),
                    diff: Some(diff.to_string()),
                };
                let tool = panel_with_edits(
                    producer,
                    ToolStatus::Completed,
                    json!({"changes": [{"path": path, "kind": {"type": "update"}}]}),
                    vec![edit],
                    None,
                );
                let md = panel_md(tool);
                assert!(md.contains(word), "{producer}/{action:?}: {md}");
                assert!(md.contains("+1 −1"), "{producer}/{action:?}: {md}");
                assert!(md.contains("```rust"), "{producer}/{action:?}: {md}");
                assert!(md.contains("+new"), "{producer}/{action:?}: {md}");
            }
        }
    }

    /// A pure rename carries no hunk at all; the action and its destination
    /// still show, and the open button follows the destination.
    #[test]
    fn a_no_hunk_rename_names_its_action_and_destination() {
        let rename = panel_with_edits(
            "codex/file_change",
            ToolStatus::Completed,
            json!({"changes": [{"path": "old.rs", "kind": {"type": "update", "movePath": "new.rs"}}]}),
            vec![FileEdit {
                path: "old.rs".into(),
                action: FileAction::Rename,
                rename_to: Some("new.rs".into()),
                diff: None,
            }],
            Some("done"),
        );
        assert_eq!(rename.file_open_path().as_deref(), Some("new.rs"));
        let title = panel_title(rename.clone());
        assert!(title.contains("📦 重命名 old.rs → new.rs"), "title: {title}");
        let md = panel_md_with_session(rename, "ses_1");
        assert!(md.contains("📦 重命名 `old.rs` → `new.rs`"), "input: {md}");
        assert!(md.contains("📦 重命名 `old.rs` → `new.rs`"), "body: {md}");
        assert!(!md.contains("@@"), "no hunk: {md}");
    }

    /// A delete is an action with no way in: the folded title and the Input
    /// line name it, and the panel carries no 「📂 打开」 button.
    #[test]
    fn a_delete_carries_no_open_button_for_either_producer() {
        for producer in ["codex/file_change", "patch"] {
            let delete = panel_with_edits(
                producer,
                ToolStatus::Completed,
                json!({"changes": [{"path": "gone.rs", "kind": {"type": "delete"}}]}),
                vec![FileEdit {
                    path: "gone.rs".into(),
                    action: FileAction::Delete,
                    rename_to: None,
                    diff: None,
                }],
                None,
            );
            assert_eq!(delete.file_open_path(), None, "{producer}: no way in");
            assert!(
                tool_open_button_element(&delete, Some("ses_1")).is_none(),
                "{producer}: no button"
            );
            let md = panel_md_with_session(delete, "ses_1");
            assert!(md.contains("🗑 删除 `gone.rs`"), "{producer}: {md}");
        }
    }

    /// A third-party id that squats `codex/file_change` — even with a
    /// complete-looking payload — has no typed detail, so it stays opaque: the
    /// card renders its own text, never a FileEdits panel.
    #[test]
    fn a_squatted_file_change_shape_without_detail_stays_opaque() {
        let tool = ToolPanel::for_test(
            "codex/file_change",
            ToolStatus::Completed,
            Some(json!({"changes": [{"path": "a.rs", "kind": {"type": "update"}}]})),
            Some("third-party output"),
        );
        let md = panel_md(tool);
        assert!(md.contains("third-party output"), "own text: {md}");
        assert!(!md.contains("✏️ 编辑"), "no FileEdits renderer: {md}");
        assert!(!md.contains("+1 −1"), "no diff totals: {md}");
    }

    /// A failed `fileChange` is not a diff panel: its change stays in metadata,
    /// but the call's error text is the body (as for a failed `edit`).
    #[test]
    fn a_failed_codex_file_change_keeps_its_error_text() {
        let mut tool = codex_file_change_panel(vec![json!({
            "path": "src/main.rs",
            "kind": {"type": "update"},
            "diff": "@@ -1 +1 @@\n-old\n+new",
        })]);
        tool.call.status = ToolStatus::Error;
        tool.call.output.blocks = vec![ContentBlock::Text("patch rejected".into())];
        tool.call.output.error = Some("patch rejected".into());
        assert_eq!(file_edits_output(&tool), None);
        let md = panel_md(tool);
        assert!(!md.contains("+1 −1"), "no diff panel: {md}");
        assert!(md.contains("patch rejected"), "the error text: {md}");

        // A failed MULTI-file change names its file count but no totals: the
        // change is unknown, so `+0 −0` would claim nothing changed.
        let mut multi = codex_file_change_panel(vec![
            json!({
                "path": "src/a.rs",
                "kind": {"type": "update"},
                "diff": "@@ -1 +1 @@\n-old\n+new",
            }),
            json!({
                "path": "src/b.rs",
                "kind": {"type": "update"},
                "diff": "@@ -1 +1 @@\n-old\n+new",
            }),
        ]);
        multi.call.status = ToolStatus::Error;
        multi.call.output.blocks = vec![ContentBlock::Text("patch rejected".into())];
        multi.call.output.error = Some("patch rejected".into());
        let title = panel_title(multi);
        assert!(title.contains("· 📄 2 files"), "count only: {title}");
        assert!(!title.contains("+0 −0"), "no invented totals: {title}");
    }

    /// ADR-0042's amendment applies to the output side too: a tool that
    /// squats `codex/file_change` without the adapter's `{changes: […]}` input
    /// stays on the opaque path even when its text happens to be a diff.
    #[test]
    fn a_foreign_codex_file_change_shape_is_not_rendered_as_a_diff() {
        let diff = "@@ -1 +1 @@\n-old\n+new";
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "codex/file_change".into(),
                call_id: "call_foreign".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"path": "src/main.rs"})),
            metadata: Some(json!({"changes": [{"path": "src/main.rs", "diff": diff}]})),
            output: ToolOutput {
                raw: Some(json!(diff)),
                blocks: vec![ContentBlock::Text(diff.into())],
                error: None,
            },
            ..ToolCall::default()
        });
        let md = panel_md(tool.clone());
        assert!(!md.contains("+1 −1"), "not a diff panel: {md}");
        assert_eq!(tool.file_open_path(), None, "no way in without the shape");
        assert!(
            panel_title(tool).contains("codex/file_change"),
            "the raw id stays the label"
        );
    }

    /// A multi-file `file_change` beyond the diff budget clips bare and says
    /// so, exactly like the OpenCode editing tools: it is a diff body, not a
    /// 3,000-character preview.
    #[test]
    fn a_clipped_codex_file_change_says_it_was_clipped() {
        let mut diff = String::from("@@ -1,1 +1,400 @@\n");
        for n in 1..=400 {
            diff.push_str(&format!("+let value_{n} = compute({n});\n"));
        }
        let tool = codex_file_change_panel(vec![json!({
            "path": "src/main.rs",
            "kind": {"type": "update"},
            "diff": diff,
        })]);
        assert_eq!(
            diff_truncation_note(&tool),
            Some(DIFF_TRUNCATION_NOTE),
            "the diff budget, with its clip note"
        );
        // The render itself takes the byte budget: the fence closes bare and
        // the note follows, and the emitted body is what the estimate charges.
        let md = panel_md(tool.clone());
        assert!(
            md.contains(&format!("```\n\n{DIFF_TRUNCATION_NOTE}")),
            "the fence clips bare, then the note: {md}"
        );
        assert!(panel_output_estimate(&tool) > 0, "the rendered body is charged");
    }

    /// A file-editing panel whose diff exceeds the panel budget clips the body
    /// BARE and says so below the fence (#788, this fix). The `…` `truncate_md`
    /// appends sits immediately before the closing fence, which stops Feishu
    /// rendering the block as code at all — so before this, the LARGEST diffs
    /// were the ones that lost monospace and syntax colour entirely. Unlike a
    /// clipped `read`/`write`, the note promises no way-in: the open button
    /// shows the file's current content, not the diff that changed it.
    #[test]
    fn a_clipped_edit_diff_keeps_its_code_block_and_says_so() {
        for name in ["edit", "apply_patch", "patch"] {
            let tool = long_diff_panel(name);
            assert!(
                diff_truncation_note(&tool).is_some(),
                "{name}: a clipped diff carries the note"
            );
            // The note needs no button, so a routeless panel still renders it —
            // and the estimate still charges it.
            assert_eq!(
                preview_truncation_note_estimate(&tool, None),
                DIFF_TRUNCATION_NOTE.len(),
                "{name}: the note's bytes are charged"
            );
            let md = panel_md(tool);
            assert!(md.contains("```rust"), "{name}: the fence survives: {md}");
            assert!(
                !md.contains('…'),
                "{name}: no ellipsis inside the fenced body: {md}"
            );
            assert!(
                md.contains(&format!("```\n\n{DIFF_TRUNCATION_NOTE}")),
                "{name}: the fence closes bare, right before the note: {md}"
            );
        }

        // A FAILED call's error text is not a diff: no clip note.
        let failed = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "edit".into(),
                call_id: "call_failed".into(),
            },
            status: ToolStatus::Error,
            started_at: None,
            input: Some(json!({"filePath": "src/main.rs"})),
            metadata: None,
            output: ToolOutput {
                raw: Some(json!("Edit failed.")),
                blocks: vec![ContentBlock::Text("Edit failed.".into())],
                error: Some("x".repeat(2 * TOOL_OUTPUT_MAX_CHARS)),
            },
            ..ToolCall::default()
        });
        assert_eq!(
            diff_truncation_note(&failed),
            None,
            "an error body carries no diff note"
        );
    }

    /// #792: the diff body budget is measured in JSON-ESCAPED bytes, not
    /// characters, AND it includes the fence `fenced_code` will add plus that
    /// fence's language tag. The widest single character escapes to six bytes
    /// (a control char as `\uXXXX`), so a character-counted cap of the same
    /// number lets a quote-, newline- or CJK-heavy body blow the card budget
    /// severalfold; a body of backticks costs up to twice its escaped length
    /// again in fence, which a body-only cap misses; and a 4-byte `rust` tag is
    /// enough to push a body that just fits over the cap.
    #[test]
    fn the_diff_clip_counts_json_escaped_bytes_fence_and_language() {
        // Plain ASCII: 6,000 chars plus the fence and `rust` tag fit.
        let ascii = "a".repeat(6_000);
        assert_eq!(
            clip_fenced_body(&ascii, Some("rust"), DIFF_MAX_ESCAPED_BYTES),
            ascii
        );
        // CJK: 5,000 chars is 15,000 escaped bytes; whole characters only.
        let cjk = "中".repeat(5_000);
        assert_eq!(
            clip_fenced_body(&cjk, Some("rust"), DIFF_MAX_ESCAPED_BYTES),
            "中".repeat(3_996)
        );
        // The tag is what clips at the boundary: 3,997 chars plus the 8-byte
        // minimum fence are 11,999 bytes and fit a tagless fence, but the
        // 4-byte `rust` tag makes 12,003 — over the cap.
        let boundary = "中".repeat(3_997);
        assert_eq!(
            clip_fenced_body(&boundary, None, DIFF_MAX_ESCAPED_BYTES),
            boundary
        );
        assert_eq!(
            clip_fenced_body(&boundary, Some("rust"), DIFF_MAX_ESCAPED_BYTES),
            "中".repeat(3_996)
        );
        // A control character is JSON's widest escape: six bytes, so the cap
        // holds 1,998 of them beside the fence and the 4-byte tag.
        let control = "\u{1}".repeat(3_000);
        let clipped = clip_fenced_body(&control, Some("rust"), DIFF_MAX_ESCAPED_BYTES);
        assert_eq!(clipped.chars().count(), 1_998);
        assert_eq!(
            json_escaped_len(&clipped) + fenced_block_overhead(&clipped) + "rust".len(),
            DIFF_MAX_ESCAPED_BYTES
        );
        // A single 20,000-backtick run: the fence is one backtick longer on
        // each side, so only 3,997 survive — 3,997 escaped bytes plus a 7,998
        // byte fence and the 4-byte tag. Clipping on escaped bytes alone would
        // keep 12,000 and then pay ~24,000 more in fence.
        let ticks = "`".repeat(20_000);
        let clipped = clip_fenced_body(&ticks, Some("rust"), DIFF_MAX_ESCAPED_BYTES);
        assert_eq!(clipped.len(), 3_997);
        assert_eq!(
            json_escaped_len(&clipped) + fenced_block_overhead(&clipped) + "rust".len(),
            11_999
        );
        // The per-character charge the clip walks is serde_json's own total.
        let mixed = "a\"\\\n\t\u{1}\u{7f}中";
        let sum: usize = mixed.chars().map(escaped_char_len).sum();
        assert_eq!(sum, json_escaped_len(mixed));
    }

    /// #792: a diff body is capped in JSON-escaped BYTES, not characters. A
    /// 6,000-char ASCII diff fits and renders WHOLE; a 5,000-char CJK diff
    /// escapes to ~15 KB, so it clips BARE — the `…` would stop the fence
    /// rendering as code (#788) — and the note follows the closed fence.
    #[test]
    fn a_diff_panel_caps_its_body_in_escaped_bytes() {
        // 200 added lines ≈ 6,421 ASCII characters: under the byte budget.
        let mut ascii =
            String::from("Index: src/main.rs\n--- src/main.rs\n+++ src/main.rs\n@@ -1,1 +1,200 @@\n");
        for n in 0..200 {
            ascii.push_str(&format!("+let value_{n:03} = compute({n:03});\n"));
        }
        let ascii_body = parse_edit_diff(&ascii).expect("a hunk diff parses").body;
        let ascii_fenced = json_escaped_len(&ascii_body) + fenced_block_overhead(&ascii_body) + "rust".len();
        assert!(
            ascii_fenced <= DIFF_MAX_ESCAPED_BYTES,
            "the fixture fits the byte budget with its fence and tag: {ascii_fenced}"
        );
        let panel = edit_panel(&ascii);
        assert_eq!(
            diff_truncation_note(&panel),
            None,
            "a diff that fits says nothing"
        );
        assert_eq!(preview_truncation_note_estimate(&panel, None), 0);
        let md = panel_md(panel);
        assert!(
            md.contains("+let value_199 = compute(199);"),
            "the whole diff renders: {md}"
        );
        assert!(!md.contains('…'), "nothing was clipped: {md}");

        // 5,000 CJK characters escape to 15,000 bytes: over the budget even
        // though a 12,000-CHAR cap would have kept them.
        let mut cjk =
            String::from("Index: src/main.rs\n--- src/main.rs\n+++ src/main.rs\n@@ -1,1 +1,1 @@\n+");
        cjk.push_str(&"中".repeat(5_000));
        let panel = edit_panel(&cjk);
        assert_eq!(
            diff_truncation_note(&panel),
            Some(DIFF_TRUNCATION_NOTE),
            "an over-budget diff says it was clipped"
        );
        assert_eq!(
            preview_truncation_note_estimate(&panel, None),
            DIFF_TRUNCATION_NOTE.len(),
            "the note's bytes are charged"
        );
        let md = panel_md(panel);
        assert!(!md.contains('…'), "the clipped body carries no ellipsis: {md}");
        let (body, after) = md
            .split_once("```rust\n")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split_once("\n```\n\n"))
            .unwrap_or_else(|| panic!("one closed fence above the note: {md}"));
        assert_eq!(after, DIFF_TRUNCATION_NOTE, "the note follows the fence: {md}");
        let fenced = json_escaped_len(body) + fenced_block_overhead(body) + "rust".len();
        assert!(
            fenced <= DIFF_MAX_ESCAPED_BYTES,
            "the rendered body, fence and tag respect the budget: {fenced}"
        );
        assert!(
            fenced > DIFF_MAX_ESCAPED_BYTES - 8,
            "the clip fills the budget rather than cutting early: {fenced}"
        );
    }

    /// #792: `panel_output_estimate` charges the diff the panel RENDERS — the
    /// parsed body clipped at the byte cap, its count header, its whole fence
    /// and the opening fence's language tag — never the raw recorded diff under
    /// the old 3,000-character preview cap. A fitting 6,000-char diff is charged
    /// in full, so the card estimate cannot trail the body the card carries.
    #[test]
    fn the_diff_estimate_charges_the_body_it_renders() {
        let mut cjk =
            String::from("Index: src/main.rs\n--- src/main.rs\n+++ src/main.rs\n@@ -1,1 +1,1 @@\n+");
        cjk.push_str(&"中".repeat(5_000));
        let panel = edit_panel(&cjk);
        let header = {
            let parsed = parse_edit_diff(&cjk).expect("a hunk diff parses");
            format!("+{} −{}", parsed.additions, parsed.deletions)
        };
        let estimate = panel_output_estimate(&panel);
        let rendered = file_edits_body(&panel).expect("the diff body");
        assert_eq!(
            estimate,
            json_escaped_len(&CardMarkdown::new().element(&header)) + serialized_len(&rendered.render().0),
            "the charge is the rendered body, fence, tag and header"
        );
        assert!(
            estimate < DIFF_MAX_ESCAPED_BYTES + 200,
            "and the charge is capped with the body: {estimate}"
        );

        // A fitting diff charges its WHOLE rendered body: the old 3,000-char
        // slice would have under-charged it by more than half.
        let mut ascii =
            String::from("Index: src/main.rs\n--- src/main.rs\n+++ src/main.rs\n@@ -1,1 +1,200 @@\n");
        for n in 0..200 {
            ascii.push_str(&format!("+let value_{n:03} = compute({n:03});\n"));
        }
        let ascii_body = parse_edit_diff(&ascii).expect("a hunk diff parses").body;
        assert!(
            json_escaped_len(&ascii_body) > TOOL_OUTPUT_MAX_CHARS,
            "the fixture is past the old character cap: {}",
            json_escaped_len(&ascii_body)
        );
        assert_eq!(
            panel_output_estimate(&edit_panel(&ascii)),
            json_escaped_len(&CardMarkdown::new().element("+200 −0"))
                + json_escaped_len(&ascii_body)
                + fenced_block_overhead(&ascii_body)
                + "rust".len(),
            "the whole body is charged, not a 3,000-char preview"
        );
    }

    /// A file-editing panel carrying `diff` in V1's `metadata.diff`, the shape
    /// the render reads (#792).
    /// One `FileEdit` from a raw unified diff, reduced to its hunk body the way
    /// the adapter stores it (the path and action are fields, so the header
    /// block would only repeat them).
    fn file_edit(path: &str, action: FileAction, diff: &str) -> FileEdit {
        FileEdit {
            path: path.to_string(),
            action,
            rename_to: None,
            diff: crate::backend::file_edit_hunks(diff),
        }
    }

    fn edit_panel(diff: &str) -> ToolPanel {
        ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "edit".into(),
                call_id: "call_edit".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"filePath": "src/main.rs"})),
            metadata: Some(json!({ "diff": diff })),
            output: ToolOutput {
                raw: Some(json!("Edit applied successfully.")),
                blocks: vec![ContentBlock::Text("Edit applied successfully.".into())],
                error: None,
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(vec![file_edit("src/main.rs", FileAction::Update, diff)]),
            ..ToolCall::default()
        })
    }

    /// A `write` panel: one `Add` FileEdit with the content as its additions,
    /// the shape the adapter declares for V1 and V2 alike.
    fn write_panel(path: &str, content: &str, status: ToolStatus, output: Option<&str>) -> ToolPanel {
        ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "write".into(),
                call_id: "call_write".into(),
            },
            status,
            started_at: None,
            input: Some(json!({"path": path, "content": content})),
            output: ToolOutput {
                raw: output.map(|text| json!(text)),
                blocks: output
                    .map(|text| vec![ContentBlock::Text(text.to_string())])
                    .unwrap_or_default(),
                error: None,
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(if path.is_empty() {
                Vec::new()
            } else {
                vec![FileEdit {
                    path: path.to_string(),
                    action: FileAction::Add,
                    rename_to: None,
                    diff: {
                        let lines: Vec<&str> = content.lines().collect();
                        (!lines.is_empty()).then(|| {
                            let mut diff = format!("@@ -0,0 +1,{} @@\n", lines.len());
                            for line in lines {
                                diff.push('+');
                                diff.push_str(line);
                                diff.push('\n');
                            }
                            diff.trim_end_matches(['\r', '\n']).to_string()
                        })
                    },
                }]
            }),
            ..ToolCall::default()
        })
    }

    /// A file-editing panel whose rendered diff body is longer than
    /// [`DIFF_MAX_ESCAPED_BYTES`]: the shape the clip note exists for.
    fn long_diff_panel(name: &str) -> ToolPanel {
        let mut diff = String::from(
            "\
Index: src/main.rs
===================================================================
--- src/main.rs
+++ src/main.rs
@@ -1,1 +1,400 @@
",
        );
        for n in 1..=400 {
            diff.push_str(&format!("+let value_{n} = compute({n});\n"));
        }
        ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: name.to_string(),
                call_id: format!("call_{name}"),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"filePath": "src/main.rs"})),
            metadata: Some(json!({ "diff": diff })),
            output: ToolOutput {
                raw: Some(json!("Edit applied successfully.")),
                blocks: vec![ContentBlock::Text("Edit applied successfully.".into())],
                error: None,
            },
            detail: ToolDetail::FileEdits(vec![file_edit("src/main.rs", FileAction::Update, &diff)]),
            kind: ToolKind::Edit,
            ..ToolCall::default()
        })
    }

    /// V2 records the edit's diff as `metadata.files[]` (one
    /// `{file, patch, …}` per touched file), not V1's `metadata.diff` string,
    /// and its text output is a server-generated summary — "Edited <file> (N
    /// replacement)" on live 2.0.18. The panel must find the diff, show the
    /// hunks, and drop the summary as noise.
    #[test]
    fn edit_v2_metadata_files_render_the_hunks_and_drop_the_summary() {
        // The real 2.0.18 shape, captured from `GET /api/session/{id}/message`
        // (field names and patch framing verbatim; the file trimmed).
        let patch = "\
Index: .config/opencode/AGENTS.md
===================================================================
--- .config/opencode/AGENTS.md
+++ .config/opencode/AGENTS.md
@@ -1,6 +1,4 @@
-# English Learning Rule
+# Interaction Rules
 
-I am practicing English.
-## Interaction Rules
 When a skill instructs you to interview the user, use the question tool.
";
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "edit".into(),
                call_id: "call_00_g2baryak60jmfn29i1ftx4ro".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({
                "path": "/root/.config/opencode/AGENTS.md",
                "oldString": "# English Learning Rule",
                "newString": "# Interaction Rules"
            })),
            metadata: Some(json!({
                "files": [{
                    "file": ".config/opencode/AGENTS.md",
                    "patch": patch,
                    "status": "modified",
                    "additions": 1,
                    "deletions": 3
                }],
                "truncated": false
            })),
            output: ToolOutput {
                raw: Some(json!("Edited .config/opencode/AGENTS.md (1 replacement)")),
                blocks: vec![ContentBlock::Text(
                    "Edited .config/opencode/AGENTS.md (1 replacement)".into(),
                )],
                error: None,
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(vec![file_edit(
                ".config/opencode/AGENTS.md",
                FileAction::Update,
                patch,
            )]),
            ..ToolCall::default()
        });
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(
            md.contains(&format!("{}\n✏️ 编辑 `.config/opencode/AGENTS.md`", input_md())),
            "file in input: {md}"
        );
        assert!(
            md.contains(&format!("{}\n\n+1 −3", output_md())),
            "count header: {md}"
        );
        assert!(md.contains("```markdown\n@@ -1,6 +1,4 @@"), "fenced hunk: {md}");
        assert!(md.contains("-# English Learning Rule"), "removed line: {md}");
        assert!(md.contains("+# Interaction Rules"), "added line: {md}");
        assert!(!md.contains("(1 replacement)"), "generated summary dropped: {md}");
    }

    /// #793: V2's `edit` records `metadata.files[]`, and `diff_from_metadata` joins
    /// the per-file patches. Two entries are two files, so the folded header
    /// says `📄 2 files`; the totals stay the joined patch's own counts, and the
    /// body keeps both hunks untouched.
    #[test]
    fn edit_v2_metadata_with_two_files_heads_with_the_file_count() {
        let patch_one = "\
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1 +1 @@
-old
+new";
        let patch_two = "\
===================================================================
--- /a/two.rs
+++ /a/two.rs
@@ -1 +1,2 @@
-x
+y
+z";
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "edit".into(),
                call_id: "call_edit_two".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"path": "/a/one.rs"})),
            metadata: Some(json!({
                "files": [
                    {"file": "/a/one.rs", "patch": patch_one},
                    {"file": "/a/two.rs", "patch": patch_two}
                ],
                "truncated": false
            })),
            output: ToolOutput {
                raw: Some(json!("Edited 2 files")),
                blocks: vec![ContentBlock::Text("Edited 2 files".into())],
                error: None,
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(vec![
                file_edit("/a/one.rs", FileAction::Update, patch_one),
                file_edit("/a/two.rs", FileAction::Update, patch_two),
            ]),
            ..ToolCall::default()
        });
        let title = panel_title(tool.clone());
        assert!(
            title.contains("📄 2 files · +3 −2"),
            "the folded title carries the joined files' count and totals: {title}"
        );
        let md = panel_md(tool);
        assert!(
            !md.contains("📄 2 files"),
            "the expanded body does not repeat the title's count: {md}"
        );
        assert_eq!(
            md.matches("@@ -1 +1 @@").count() + md.matches("@@ -1 +1,2 @@").count(),
            2,
            "both files' hunks stay in the body: {md}"
        );
    }

    /// V1's text output is its success sentence, then — when the LSP reported
    /// diagnostics — a note after a blank line. The summary is dropped, the
    /// note stays after the hunks: the blank line is the boundary, so a
    /// summary in any wording cannot leak into the diff block.
    #[test]
    fn edit_v1_lsp_note_survives_the_summary_strip() {
        let diff = "\
Index: src/main.rs
===================================================================
--- src/main.rs
+++ src/main.rs
@@ -1,2 +1,2 @@
-let a = 1;
+let a = 2;
";
        let text_output = "Edit applied successfully.\n\nLSP errors detected in this file, please fix:\n\
                           unused variable `a`";
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "edit".into(),
                call_id: "call_edit".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"filePath": "src/main.rs"})),
            metadata: Some(json!({ "diff": diff })),
            output: ToolOutput {
                raw: Some(json!(text_output)),
                blocks: vec![ContentBlock::Text(text_output.into())],
                error: None,
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(vec![file_edit("/x/one.rs", FileAction::Update, diff)]),
            ..ToolCall::default()
        });
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(
            md.contains(&format!("{}\n\n+1 −1", output_md())),
            "count header: {md}"
        );
        assert!(md.contains("-let a = 1;"), "removed line: {md}");
        assert!(
            md.contains("LSP errors detected in this file, please fix:"),
            "note kept: {md}"
        );
        assert!(
            !md.contains("Edit applied successfully."),
            "summary dropped: {md}"
        );
    }

    /// #202: an `apply_patch` panel shows the same hunks as `edit`, fenced
    /// (monospace, no wrapping), with each patched file named in the body —
    /// and the tool's LSP note, which follows the success summary in the raw
    /// output, reaches the card after the diff.
    #[test]
    fn apply_patch_output_renders_fenced_hunks_in_panel() {
        let diff = "\
Index: /x/one.rs
===================================================================
--- /x/one.rs
+++ /x/one.rs
@@ -1,3 +1,3 @@
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;";
        // The tool's text output: the success summary, then the LSP note. The
        // panel shows the metadata diff followed by the note; the summary is
        // dropped as noise.
        let text_output = "Success. Updated the following files:\nA /x/one.rs\n\n\
                           LSP errors detected in /x/one.rs, please fix:\nunused variable `c`";
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "apply_patch".into(),
                call_id: "call_patch".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"patchText": "*** Begin Patch"})),
            metadata: Some(json!({ "diff": diff })),
            output: ToolOutput {
                raw: Some(json!(text_output)),
                blocks: vec![ContentBlock::Text(text_output.into())],
                error: None,
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(vec![file_edit("/x/one.rs", FileAction::Update, diff)]),
            ..ToolCall::default()
        });
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(
            md.contains(&format!("{}\n\n+1 −1", output_md())),
            "count header: {md}"
        );
        assert!(md.contains("```rust\n@@ -1,3 +1,3 @@"), "fenced hunks: {md}");
        assert!(md.contains("-let b = 2;"), "removed line: {md}");
        assert!(md.contains("+let b = 3;"), "added line: {md}");
        assert!(md.contains(" let a = 1;"), "context line kept: {md}");
        assert!(
            md.contains("LSP errors detected in /x/one.rs"),
            "LSP note on the card: {md}"
        );
    }

    #[test]
    fn parse_edit_diff_counts_and_tracks_path() {
        let diff = "\
Index: /a/b.rs
===================================================================
--- /a/b.rs
+++ /a/b.rs
@@ -1 +1 @@
-x
+y
@@ -5,0 +6,2 @@
+z1
+z2";
        let d = parse_edit_diff(diff).expect("diff parsed");
        assert_eq!(d.path.as_deref(), Some("/a/b.rs"));
        assert_eq!(d.paths, vec!["/a/b.rs"]);
        assert_eq!(d.additions, 3, "one +y plus two added z lines");
        assert_eq!(d.deletions, 1);
        assert_eq!(
            d.body.lines().filter(|l| l.starts_with("@@")).count(),
            2,
            "two hunk header lines: {}",
            d.body
        );
        assert!(!d.body.contains("b.rs"), "header lines not in body");
    }

    /// A hunk keeps its context lines byte-for-byte (#791): the leading space
    /// is diff syntax, trailing spaces are file content, and an empty context
    /// line is a single space — not the blank line a trailing note starts with.
    #[test]
    fn parse_edit_diff_keeps_hunk_context_verbatim() {
        let diff = concat!(
            "Index: /a/b.rs\n",
            "===================================================================\n",
            "--- /a/b.rs\n",
            "+++ /a/b.rs\n",
            "@@ -1,4 +1,4 @@\n",
            " let a = 1;  \n",
            " \n",
            "-let b = 2;\n",
            "+let b = 3;\n",
            " let c = 4;",
        );
        let d = parse_edit_diff(diff).expect("diff parsed");
        assert_eq!(
            d.body, "@@ -1,4 +1,4 @@\n let a = 1;  \n \n-let b = 2;\n+let b = 3;\n let c = 4;\n",
            "context keeps its leading space, its trailing spaces and an empty \
             context line as one space"
        );
        assert_eq!((d.additions, d.deletions), (1, 1));
        assert!(d.tail.is_empty(), "no note: {:?}", d.tail);
    }

    /// A CRLF diff loses only the `\r`: the context line's own trailing space
    /// is content and stays.
    #[test]
    fn parse_edit_diff_strips_crlf_but_keeps_content_spaces() {
        let diff = concat!(
            "Index: /a/b.rs\r\n",
            "===================================================================\r\n",
            "--- /a/b.rs\r\n",
            "+++ /a/b.rs\r\n",
            "@@ -1,2 +1,2 @@\r\n",
            " keep me \r\n",
            "-old\r\n",
            "+new\r\n",
        );
        let d = parse_edit_diff(diff).expect("diff parsed");
        assert_eq!(d.body, "@@ -1,2 +1,2 @@\n keep me \n-old\n+new\n");
        assert!(!d.body.contains('\r'), "no carriage return: {:?}", d.body);
    }

    /// The blank line is the note's boundary, so the note's own first line may
    /// start with a space — it is a note, not a context line.
    #[test]
    fn parse_edit_diff_keeps_an_indented_note_first_line() {
        let diff = "\
Index: /a/b.rs
===================================================================
--- /a/b.rs
+++ /a/b.rs
@@ -1,2 +1,2 @@
 keep
-old
+new

  LSP errors detected in /a/b.rs
 - a diagnostic that looks like a removal";
        let d = parse_edit_diff(diff).expect("diff parsed");
        assert_eq!(d.body, "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n");
        assert!(
            d.tail.contains("  LSP errors detected"),
            "indented note kept: {:?}",
            d.tail
        );
        assert!(!d.body.contains("LSP"), "note not in hunks: {:?}", d.body);
    }

    /// `diff_from_metadata` joins per-file patch strings — each ending in `\n` —
    /// with `\n`, so a joined multi-file patch carries a real blank line
    /// between files. That line is a boundary, not the note's start: reading it
    /// as one would swallow every file after the first into `tail`.
    #[test]
    fn a_joined_patch_blank_line_is_a_file_boundary_not_the_note_start() {
        let with_index = "\
Index: /a/one.rs
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1 +1 @@
-a
+b
";
        let second_index = "\
Index: /a/two.rs
===================================================================
--- /a/two.rs
+++ /a/two.rs
@@ -1 +1 @@
-c
+d";
        let d = parse_edit_diff(&format!("{with_index}\n{second_index}")).expect("parsed");
        assert_eq!(d.paths, vec!["/a/one.rs", "/a/two.rs"]);
        assert_eq!((d.additions, d.deletions), (2, 2));
        assert!(d.body.contains("+d"), "second hunk kept: {:?}", d.body);
        assert!(d.tail.is_empty(), "no note: {:?}", d.tail);

        // V2's header-only shape: no `Index:`, so the joined patch's next file
        // starts at its `===` underline.
        let header_only = "\
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1 +1 @@
-a
+b
";
        let second_header_only = "\
===================================================================
--- /a/two.rs
+++ /a/two.rs
@@ -1 +1 @@
-c
+d";
        let d = parse_edit_diff(&format!("{header_only}\n{second_header_only}")).expect("parsed");
        assert_eq!(d.paths, vec!["/a/one.rs", "/a/two.rs"]);
        assert!(d.body.contains("+d"), "second hunk kept: {:?}", d.body);
        assert!(d.tail.is_empty(), "no note: {:?}", d.tail);
    }

    /// Review #791: a joined Git-style patch opens its next file with
    /// `diff --git` (then `index <old>..<new> <mode>`) after the blank
    /// separator. That header block must end the previous hunk, or the next
    /// file's `---`/`+++` headers are counted as a deletion and an addition
    /// and land in the body — and the unrecognized `index` line must be
    /// header noise, not the note's start.
    #[test]
    fn a_joined_git_patch_ends_the_previous_hunk_at_diff_git() {
        let first = "\
diff --git a/src/one.rs b/src/one.rs
index 1111111..2222222 100644
--- a/src/one.rs
+++ b/src/one.rs
@@ -1 +1 @@
-a
+b
";
        let second = "\
diff --git a/src/two.rs b/src/two.rs
index 3333333..4444444 100644
--- a/src/two.rs
+++ b/src/two.rs
@@ -1,2 +1,2 @@
 keep
-c
+d";
        let diff = format!("{first}\n{second}");
        let d = parse_edit_diff(&diff).expect("parsed");
        assert_eq!(d.paths, vec!["src/one.rs", "src/two.rs"]);
        assert_eq!(
            (d.additions, d.deletions),
            (2, 2),
            "the second file's ---/+++ headers are not content: {:?}",
            d.body
        );
        assert!(d.body.contains(" keep"), "context kept: {:?}", d.body);
        assert!(d.body.contains("+d"), "second hunk kept: {:?}", d.body);
        assert!(
            !d.body.contains("+++ b/src/two.rs") && !d.body.contains("--- a/src/two.rs"),
            "second file's headers stay out of the body: {:?}",
            d.body
        );
        assert!(
            !d.body.contains("index "),
            "index lines are header noise: {:?}",
            d.body
        );
        assert!(d.tail.is_empty(), "no note: {:?}", d.tail);
    }

    /// Review #791: a note is text, not a patch. The first line after the
    /// blank boundary may merely *start* like a file header — a real file
    /// block spells its `---`/`+++` headers out next, so the note stays in
    /// `tail` instead of being parsed as another file.
    #[test]
    fn a_note_that_starts_like_a_file_header_stays_in_the_tail() {
        let diff = "\
Index: /a/b.rs
===================================================================
--- /a/b.rs
+++ /a/b.rs
@@ -1 +1 @@
 keep
-old
+new

Index: this quoted line is diagnostic prose, not a file header
see the compiler's note above
--- not a real header either";
        let d = parse_edit_diff(diff).expect("parsed");
        assert_eq!(d.paths, vec!["/a/b.rs"], "no phantom file: {:?}", d.paths);
        assert_eq!(d.body, "@@ -1 +1 @@\n keep\n-old\n+new\n");
        assert!(
            d.tail.contains("Index: this quoted line is diagnostic prose"),
            "the note's first line is kept: {:?}",
            d.tail
        );
        assert!(
            d.tail.contains("--- not a real header either"),
            "the whole note is kept: {:?}",
            d.tail
        );
        assert!(!d.body.contains("diagnostic prose"), "note not in hunks");
    }

    /// V2's `edit` metadata is `createTwoFilesPatch` output, which has no
    /// `Index:` line: the `---`/`+++` header is the only place the target is
    /// named. The hunk must still take the file's language, and a concatenated
    /// multi-file patch must not count the next file's header as an addition —
    /// the `====` boundary is what ends the previous hunk.
    #[test]
    fn a_header_only_v2_patch_names_its_files_and_keeps_their_language() {
        let one = "\
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1,3 +1,3 @@
 fn main() {}
-let x = 1;
+let x = 2;";
        let d = parse_edit_diff(one).expect("parsed");
        assert_eq!(d.path.as_deref(), Some("/a/one.rs"));
        assert_eq!(d.paths, vec!["/a/one.rs"]);
        assert_eq!(
            diff_language_for(d.paths.iter().map(String::as_str)),
            "rust",
            "the hunk takes the edited file's language"
        );

        let two = format!(
            "{one}\n===================================================================\n\
             --- /a/two.md\n+++ /a/two.md\n@@ -1 +1 @@\n-old\n+new"
        );
        let d = parse_edit_diff(&two).expect("parsed");
        assert_eq!(d.paths, vec!["/a/one.rs", "/a/two.md"]);
        assert_eq!(
            (d.additions, d.deletions),
            (2, 2),
            "the second file's header is not a +/- line"
        );
        assert_eq!(
            diff_language_for(d.paths.iter().map(String::as_str)),
            PLAIN_TEXT,
            "mixed languages stay plain"
        );
    }

    /// A git-style header's `a/`/`b/` prefix is not part of the path, and an
    /// add/delete's `/dev/null` side names nothing.
    #[test]
    fn diff_header_paths_drop_git_prefixes_and_dev_null() {
        assert_eq!(diff_header_path("--- a/src/x.rs"), Some("src/x.rs".into()));
        assert_eq!(diff_header_path("+++ b/src/x.rs"), Some("src/x.rs".into()));
        assert_eq!(
            diff_header_path("--- /a/b.rs\t2026-10-18"),
            Some("/a/b.rs".into())
        );
        assert_eq!(diff_header_path("+++ /dev/null"), None);
        assert_eq!(diff_header_path("---"), None);
    }

    /// A removed line whose own content starts with `--` renders as `--- …` in
    /// the diff — it must stay a removal (not be dropped as a file header) once
    /// the parser is past the first `@@` hunk.
    #[test]
    fn parse_edit_diff_keeps_dash_prefixed_removed_lines() {
        let diff = "\
Index: /a/lua.lua
===================================================================
--- /a/lua.lua
+++ /a/lua.lua
@@ -1,3 +1,3 @@
--- old comment
+-- new comment
 return 1
";
        let d = parse_edit_diff(diff).expect("diff parsed");
        assert_eq!(d.additions, 1);
        assert_eq!(d.deletions, 1);
        assert!(d.body.contains("--- old comment"), "removal kept: {}", d.body);
        assert!(d.body.contains("+-- new comment"), "addition kept: {}", d.body);
        assert!(d.body.contains(" return 1"), "context kept: {}", d.body);
    }

    /// A multi-file `apply_patch` names each file's action in the Input line
    /// and the body, so every hunk is attributable — no `Index:` header is
    /// parsed by the card any more.
    #[test]
    fn apply_patch_output_keeps_per_file_attribution() {
        let one = "\
@@ -1,3 +1,3 @@
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;";
        let two = "@@ -1 +1 @@\n-x\n+y";
        let tool = panel_with_edits(
            "apply_patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            vec![
                file_edit("/a/one.rs", FileAction::Update, one),
                file_edit("/a/two.rs", FileAction::Update, two),
            ],
            Some("Success. Updated the following files"),
        );
        assert_eq!(
            panel_title(tool.clone()),
            format!("✅ {} · 📄 2 files · +2 −2", vocab::TOOL_EDIT),
            "the folded title owns the count and the totals"
        );
        let body = panel_md(tool);
        assert!(
            body.contains("✏️ 编辑 `/a/one.rs`\n\n```rust\n@@ -1,3 +1,3 @@"),
            "the first file's action and hunk stay together: {body}"
        );
        assert!(
            body.contains("✏️ 编辑 `/a/two.rs`\n\n```rust\n@@ -1 +1 @@"),
            "the second file's action and hunk stay together: {body}"
        );
        assert!(body.contains("+let b = 3;"), "hunk kept: {body}");
        assert!(body.contains("+y"), "second hunk kept: {body}");
        assert!(body.contains(" let a = 1;"), "context kept: {body}");
        assert!(!body.contains("+++"), "file header noise dropped: {body}");
        assert!(
            body.contains("```rust"),
            "both touched files share one language: {body}"
        );
    }

    /// #1060/#793: a multi-file list's COLLAPSED title says how many files it
    /// touched ahead of the totals, and the expanded body names each file's
    /// action instead of repeating the count.
    #[test]
    fn a_multi_file_patch_title_names_the_file_count() {
        let first = "@@ -1 +1,2 @@\n-old\n+new\n+extra";
        let second = "@@ -1 +1 @@\n-x\n+y";
        let third = "@@ -1 +1 @@\n-old\n+new";
        let tool = panel_with_edits(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            vec![
                file_edit("/a/one.rs", FileAction::Update, first),
                file_edit("/a/two.rs", FileAction::Update, second),
                file_edit("/a/three.md", FileAction::Update, third),
            ],
            Some("Success. Updated the following files"),
        );
        assert_eq!(
            panel_title(tool.clone()),
            format!("✅ {} · 📄 3 files · +4 −3", vocab::TOOL_EDIT),
            "the folded title carries the file count and the totals"
        );
        let body = panel_md(tool);
        assert!(
            !body.contains("📄 3 files") && !body.contains("+4 −3"),
            "the body does not repeat the title's count: {body}"
        );
        let output_label = output_md();
        let output = body
            .split(output_label.as_str())
            .nth(1)
            .expect("the output section");
        assert_eq!(
            output.matches("✏️ 编辑").count(),
            3,
            "the body names each file's action: {body}"
        );
    }

    /// #793: the count-and-totals fragment is a `plain_text` title byte the
    /// card carries, so [`panel_file_detail_estimate`] must charge it through
    /// the same derivation the title renders — a fragment the split check
    /// missed would let a near-budget card grow past the serialized cap.
    #[test]
    fn the_multi_file_title_detail_is_charged() {
        let diff = "@@ -1 +1 @@\n-old\n+new";
        let tool = panel_with_edits(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            vec![
                file_edit("/a/one.rs", FileAction::Update, diff),
                file_edit("/a/two.rs", FileAction::Update, diff),
            ],
            Some("Success. Updated the following files"),
        );
        assert_eq!(
            panel_file_detail_estimate(&tool),
            json_escaped_len(" · 📄 2 files · +2 −2"),
            "the folded title's count and totals are card bytes"
        );
    }

    /// #1060: a single-file list names the file's ACTION in the folded title
    /// (and the body heads with the bare totals), replacing the old bare path.
    #[test]
    fn a_single_file_edit_names_its_action_in_the_title() {
        let diff = "@@ -1 +1 @@\n-old\n+new";
        let edit = panel_with_edits(
            "edit",
            ToolStatus::Completed,
            json!({"filePath": "/a/one.rs"}),
            vec![file_edit("/a/one.rs", FileAction::Update, diff)],
            Some("Edit applied successfully."),
        );
        assert_eq!(
            panel_title(edit.clone()),
            format!("✅ {} · ✏️ 编辑 one.rs", vocab::TOOL_EDIT)
        );
        assert!(
            panel_md(edit).contains(&format!("{}\n\n+1 −1", output_md())),
            "the body keeps the bare header"
        );

        let patch = panel_with_edits(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            vec![file_edit("/a/one.rs", FileAction::Update, diff)],
            Some("Success. Updated the following files"),
        );
        assert_eq!(
            panel_title(patch.clone()),
            format!("✅ {} · ✏️ 编辑 one.rs", vocab::TOOL_EDIT)
        );
        assert!(panel_md(patch).contains(&format!("{}\n\n+1 −1", output_md())));
    }

    /// A list that touches two languages has no honest grammar to fence with:
    /// it stays plain rather than colouring one file's hunks with the other's.
    #[test]
    fn a_mixed_language_patch_stays_plain() {
        let diff = "@@ -1 +1 @@\n-old\n+new";
        let tool = panel_with_edits(
            "apply_patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            vec![
                file_edit("/a/one.rs", FileAction::Update, diff),
                file_edit("/a/two.md", FileAction::Update, diff),
            ],
            Some("Success. Updated the following files"),
        );
        let md = panel_md(tool);
        assert!(
            md.contains(&format!("```{PLAIN_TEXT}")),
            "mixed languages stay plain: {md}"
        );
    }

    /// #202 review: the bridge appends the tool's LSP note after the diff; the
    /// parser must keep it as `tail` (not drop it with the context lines), and
    /// a note that quotes `+`/`-` lines must stay in the tail, not the hunks.
    #[test]
    fn parse_edit_diff_keeps_the_trailing_lsp_note() {
        let diff = "\
Index: a.rs
===================================================================
--- a.rs
+++ a.rs
@@ -1 +1 @@
-old
+new

LSP errors detected in a.rs, please fix:
- a diagnostic that starts like a removal
+ a diagnostic that starts like an addition";
        let d = parse_edit_diff(diff).expect("diff parsed");
        assert_eq!(d.body.trim_end(), "@@ -1 +1 @@\n-old\n+new");
        assert!(
            d.tail.contains("LSP errors detected in a.rs"),
            "note kept as tail: {:?}",
            d.tail
        );
        assert!(
            d.tail.contains("a diagnostic that starts like a removal"),
            "diff-looking note lines stay in the tail: {:?}",
            d.tail
        );
        assert!(
            !d.body.contains("diagnostic"),
            "note must not leak into the hunks: {:?}",
            d.body
        );
    }

    #[test]
    fn read_tool_output_renders_in_panel() {
        let raw = "\
<path>/x/y.rs</path>
<type>file</type>
<content>
1: use std::fs;
</content>";
        let tool = read_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "/x/y.rs"})),
            Some(raw),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(!text.contains("<path>"), "wrapper leaks into card: {}", text);
        assert!(!text.contains("<content>"), "wrapper leaks into card: {}", text);
        assert!(text.contains("/x/y.rs"), "path shown: {}", text);
        assert!(text.contains("use std::fs"), "code shown: {}", text);
        // File content renders as a fenced code block (no line wrapping).
        assert!(text.contains("```rust"), "rust fenced code block: {}", text);
        assert!(text.contains("```"), "closing fence: {}", text);
    }

    /// ADR-0107 §5: the structural section labels are copy, and the card speaks
    /// them in Chinese — the English `**Input**`/`**Output**` are gone.
    #[test]
    fn tool_panel_section_labels_are_the_shared_copy() {
        let tool = command_panel(
            "bash",
            ToolStatus::Completed,
            "cargo test",
            None,
            None,
            false,
            Some("ok"),
        );
        let md = panel_md(tool);
        assert!(
            md.contains(&format!("**{}**", crate::feishu::card::vocab::INPUT_LABEL)),
            "the input label is the shared copy: {md}"
        );
        assert!(
            md.contains(&format!("**{}**", crate::feishu::card::vocab::OUTPUT_LABEL)),
            "the output label is the shared copy: {md}"
        );
        assert!(
            !md.contains("**Input**") && !md.contains("**Output**"),
            "no English section label survives: {md}"
        );
    }

    /// An opaque vendor tool literally named `plan` is **not** the plan
    /// surface: the opaque fallback may return the word, but the plan arm is
    /// the closed `Other` + `None` + `todowrite` table entry. It keeps its own
    /// panel and its own title, and it is never dropped for a missing plan
    /// capability (the collision this test pins).
    #[test]
    fn an_opaque_id_named_plan_is_not_the_plan_surface() {
        let tool = ToolPanel::for_test(
            "plan",
            ToolStatus::Completed,
            Some(json!({"note": "vendor"})),
            Some("ok"),
        );
        assert!(!is_plan_call(tool.call()), "a name alone is not the plan arm");
        assert!(!tool.is_plan());
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .unwrap();
        assert_eq!(title, "✅ plan", "the vendor keeps its own name");
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert!(md.contains(&input_md()), "it renders an ordinary panel: {md}");
        assert!(!md.contains("共"), "it is not the checklist: {md}");

        // The adapter's own plan call (Other + None) does take the surface.
        let plan = plan_tool(
            ToolStatus::Completed,
            Some(json!({"todos": [{"content": "x", "status": "pending"}]})),
            Some(&json!([{"content": "x", "status": "pending"}]).to_string()),
        );
        assert!(is_plan_call(plan.call()));
    }

    /// A `todowrite` call renders as a status checklist, not raw JSON: the
    /// folded header carries the plan size plus the status counts, the body is
    /// the iconed list alone, and the payload's priority (noise) never leaks.
    #[test]
    fn todowrite_panel_renders_a_checklist_not_raw_json() {
        let todos = json!([
            {"content": "调研", "status": "completed", "priority": "high"},
            {"content": "实现渲染", "status": "in_progress", "priority": "high"},
            {"content": "补测试", "status": "pending", "priority": "medium"},
            {"content": "旧方案", "status": "cancelled", "priority": "low"},
        ]);
        let tool = plan_tool(
            ToolStatus::Completed,
            Some(json!({ "todos": todos.clone() })),
            // Real outputs are `JSON.stringify(todos, null, 2)`.
            Some(&todos.to_string()),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");

        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(
            title,
            format!("📋 {} · 共 4 项 · 🔄 1 · ⬜ 1 · ✅ 1 · 🚫 1", vocab::TOOL_PLAN),
            "the folded header must carry the size and status counts"
        );
        assert!(
            md.starts_with("- ✅ ~~调研~~"),
            "the checklist is the whole body: {md}"
        );
        assert!(!md.contains(input_md().as_str()), "no Input placeholder: {md}");
        assert!(!md.contains(output_md().as_str()), "no Output marker: {md}");
        assert!(!md.contains("共 4 项"), "the size lives in the header: {md}");
        assert!(md.contains("- ✅ ~~调研~~"), "completed struck: {md}");
        assert!(md.contains("- 🔄 **实现渲染**"), "in-progress bolded: {md}");
        assert!(md.contains("- ⬜ 补测试"), "pending plain: {md}");
        assert!(md.contains("- 🚫 ~~旧方案~~"), "cancelled struck: {md}");
        assert!(!md.contains('"'), "raw JSON leaked: {md}");
        assert!(!md.contains("priority"), "raw priority leaked: {md}");
        assert!(!md.contains("```"), "checklist must stay markdown: {md}");
    }

    /// A still-running `todowrite` has no output to parse: the panel falls back
    /// to the generic Input rendering, where `📋 共 N 项任务` is the only content
    /// the call can show yet (and the header carries no counts).
    #[test]
    fn running_todowrite_panel_shows_the_plan_size_in_its_body() {
        let tool = plan_tool(
            ToolStatus::Running,
            Some(json!({ "todos": [
                {"content": "第一步", "status": "in_progress"},
                {"content": "第二步", "status": "pending"},
            ] })),
            None,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        let title = card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title");
        assert_eq!(
            title,
            format!("📋 {}", vocab::TOOL_PLAN),
            "no counts before the list exists"
        );
        assert!(
            md.contains(&format!("{}\n📋 共 2 项任务", input_md())),
            "plan size: {md}"
        );
    }

    /// A todo item long enough to trip the generic long-line rule must stay a
    /// markdown row: fencing it would show the literal `- ✅ …` syntax.
    #[test]
    fn todowrite_long_item_stays_markdown_not_fenced() {
        let content = format!("修复 {}", "很长".repeat(80));
        let tool = plan_tool(
            ToolStatus::Completed,
            None,
            Some(&json!([{"content": content, "status": "pending"}]).to_string()),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(md.contains("- ⬜ 修复 很长"), "row kept and clipped: {md}");
        assert!(!md.contains("```"), "must not be fenced: {md}");
    }

    /// An unparseable plan output — a running call's placeholder, an error, a
    /// truncated dump, or a list with one malformed row — passes through
    /// unchanged rather than being dropped, and so does an empty list.
    #[test]
    fn plan_output_without_a_todo_array_passes_through() {
        for raw in [
            "0 todos",
            "❌ permission denied",
            "[]",
            "",
            r#"[{"content":"ok","status":"pending"},{"status":"pending"}]"#,
            r#"[{"content":"","status":"pending"}]"#,
        ] {
            let (header, style, body) = plan_output(raw);
            assert_eq!(header, None, "no header for {raw:?}");
            assert_eq!(style, BodyStyle::Auto, "raw fallback for {raw:?}");
            assert_eq!(body, raw, "raw output kept for {raw:?}");
        }
    }

    #[test]
    fn tool_output_long_lines_wrapped_in_code_block() {
        // A non-read tool with a line long enough to wrap must become a code
        // block so Feishu doesn't fold it.
        let long = format!("cargo run {}", "a".repeat(140));
        let tool = ToolPanel::for_test("bash", ToolStatus::Completed, None, Some(&long));
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        // The markdown content (JSON-decoded) must fence the long line.
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(
            md.contains(&format!("```\n{long}\n```")),
            "long line must be fenced: {}",
            md
        );
    }

    #[test]
    fn tool_output_short_plain_lines_not_fenced() {
        // Short, well-formed plain output stays plain text (no fences).
        let tool = ToolPanel::for_test("bash", ToolStatus::Completed, None, Some("all tests passed"));
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(!text.contains("```"), "short output must not be fenced: {}", text);
    }

    /// A failure's decoded message joins the output text on its OWN line: the
    /// pieces are separate, so a missing separator runs them together. The
    /// mutation audit found both separators surviving the suite.
    #[test]
    fn tool_output_joins_content_result_and_error_on_separate_lines() {
        let completed = ToolCall {
            identity: ToolIdentity {
                name: "bash".into(),
                call_id: "call_1".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: None,
            metadata: None,
            output: ToolOutput {
                raw: None,
                blocks: vec![ContentBlock::Text("first block\nsecond block".into())],
                error: None,
            },
            ..ToolCall::default()
        };
        assert_eq!(
            ToolPanel::new(completed.clone()).output().as_deref(),
            Some("first block\nsecond block")
        );

        let failed = ToolCall {
            status: ToolStatus::Error,
            output: ToolOutput {
                raw: None,
                blocks: vec![ContentBlock::Text("before the error".into())],
                error: Some("boom".into()),
            },
            ..completed
        };
        assert_eq!(
            ToolPanel::new(failed).output().as_deref(),
            Some("before the error\n❌ boom")
        );
    }

    /// Regression: a plain output containing `--` — `rg`'s group separator —
    /// was parsed by Feishu as a Setext heading underline, which folded every
    /// line above it, `**输出**` included, into one glued heading
    /// (`## Output99- …`): the marker must own a paragraph, and the body must
    /// be fenced so the underline stays literal.
    #[test]
    fn setext_underline_in_output_is_fenced_and_marker_separated() {
        let out = "99-    /// stays live: unknown must never be read\n\
                   100-    /// as resolved (#130, #144).\n\
                   --\n\
                   244-        }\n\
                   245-    }";
        let tool = ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            Some(json!({"command": "rg -n \"x\" -B3 -A 25 src/a.rs | head -80"})),
            Some(out),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(
            md.contains(&format!("{}\n\n```\n", output_md())),
            "the marker must own a paragraph and the body be fenced: {md}"
        );
        assert!(
            !md.contains(&format!("{}99-", output_md())),
            "the marker must not be swallowed by the output: {md}"
        );
        assert!(
            md.contains(&format!("```\n{out}\n```")),
            "the fenced body must hold the separator verbatim: {md}"
        );
    }

    /// The marker's blank line also protects a body Feishu would otherwise read
    /// as a lazy continuation of the marker paragraph (an indented first line,
    /// which cannot interrupt a paragraph).
    #[test]
    fn output_marker_separated_from_an_indented_first_line() {
        let tool = ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            None,
            Some("    Checking colark v0.8.4\n    Finished dev profile"),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(
            md.contains(&format!("{}\n\n    Checking colark", output_md())),
            "the marker must stay on its own line: {md}"
        );
    }

    #[test]
    fn needs_code_block_flags_long_lines_and_setext_underlines() {
        assert!(needs_code_block(&"a".repeat(101)), "long line wraps");
        assert!(!needs_code_block("all tests passed"));
        // `rg` group separators, `===`/`-` underlines: each folds the lines
        // above it into a heading, so they force a fence.
        assert!(needs_code_block("a\n--\nb"));
        assert!(needs_code_block("a\n=====\nb"));
        assert!(needs_code_block("a\n-\nb"));
        // Markdown that only looks like an underline is left alone.
        assert!(!needs_code_block("- item\n- item"));
        assert!(!needs_code_block("|---|---|"));
    }

    /// The underline rule is CommonMark's: at most three leading spaces, no
    /// internal whitespace, trailing whitespace allowed. Anything wider would
    /// fence outputs that were never at risk.
    #[test]
    fn setext_underline_rule_matches_commonmark() {
        assert!(is_setext_underline("--"));
        assert!(is_setext_underline("="));
        assert!(is_setext_underline("   ---"));
        assert!(is_setext_underline("  ====  "), "trailing whitespace allowed");
        assert!(!is_setext_underline("    ---"), "four spaces is indented code");
        assert!(!is_setext_underline("\t---"), "a tab is not leading spaces");
        assert!(!is_setext_underline("- - -"), "internal whitespace is a break");
        assert!(!is_setext_underline(""));
        assert!(!is_setext_underline("   "));
        assert!(!is_setext_underline("--- x"));
    }

    #[test]
    fn fenced_code_sizes_fence_beyond_inner_backticks() {
        let body = "line with `tick` and\n```\ninner fence\n```\nend";
        let out = fenced_code(body, None);
        // The inner run is 3 backticks, so the outer fence must be longer.
        assert!(
            out.starts_with("````\n"),
            "outer fence must exceed inner run: {}",
            out
        );
        assert!(out.ends_with("\n````"), "closing fence too short: {}", out);
        assert!(out.contains(body), "content must be preserved");
    }

    /// A `read` panel fences with the shared language table, so an extension
    /// the old panel map never knew (`kt`, `cs`) still highlights, and one with
    /// no honest token falls back to `plain_text`.
    #[test]
    fn read_panel_uses_the_shared_language_table() {
        for (path, token) in [
            ("/x/Main.kt", "kotlin"),
            ("/x/A.cs", "cs"),
            ("/x/a.rs", "rust"),
            ("/x/notes.unknownext", PLAIN_TEXT),
            ("/x/noext", PLAIN_TEXT),
        ] {
            let output = format!("Read file {path}, lines 1-1\n1: x");
            let (_, style, _) = read_output(&output);
            assert_eq!(
                style,
                BodyStyle::Code(Some(token)),
                "`{path}` must fence as `{token}`"
            );
        }
    }

    #[test]
    fn tool_input_edit_shows_only_the_file() {
        // The oldString/newString preview (the first line of each, both marked
        // -/+) read as duplicated content and hid the real change, which now
        // comes from the tool's diff output. The input shows the target file
        // and nothing else.
        let tool = edit_tool(
            ToolStatus::Completed,
            Some(json!({
                "filePath": "src/main.rs",
                "oldString": "let a = 1;",
                "newString": "let a = 2;"
            })),
            None,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(text.contains("src/main.rs"), "file missing: {}", text);
        assert!(
            !text.contains("- let a = 1;"),
            "old preview must not show: {}",
            text
        );
        assert!(
            !text.contains("+ let a = 2;"),
            "new preview must not show: {}",
            text
        );
        assert!(!text.contains("oldString"), "raw key leaked: {}", text);
        assert!(!text.contains("newString"), "raw key leaked: {}", text);
        assert!(!text.contains("filePath"), "raw key leaked: {}", text);
    }

    /// The target's field name is the generation's own: V1 spells it
    /// `filePath`, V2 spells it `path` (live 2.0.18). Reading only V1's spelling
    /// renders the placeholder `📄 `?`` and the reader cannot tell what was
    /// edited.
    #[test]
    fn tool_input_edit_shows_the_v2_path_field() {
        let tool = edit_tool(
            ToolStatus::Completed,
            Some(json!({
                "path": "/root/.config/opencode/AGENTS.md",
                "oldString": "# English Learning Rule",
                "newString": "# Interaction Rules"
            })),
            None,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(
            text.contains("✏️ 编辑 `/root/.config/opencode/AGENTS.md`"),
            "file missing: {}",
            text
        );
        assert!(!text.contains("✏️ 编辑 `?`"), "placeholder shown: {}", text);
    }

    #[test]
    fn tool_input_read_shows_path_and_limits() {
        let tool = read_tool(
            ToolStatus::Completed,
            Some(json!({"filePath": "src/foo.rs", "limit": 80})),
            None,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(text.contains("src/foo.rs"), "path missing: {}", text);
        assert!(text.contains("最多 80 行"), "limit missing: {}", text);
        assert!(!text.contains("filePath"), "raw key leaked: {}", text);
    }

    #[test]
    fn tool_input_grep_shows_pattern_once() {
        // Regression: the pattern was rendered twice (as a bare path AND as
        // "匹配 …") for grep/glob inputs.
        let tool = search_tool(
            "grep",
            SearchScope::Content,
            ToolStatus::Completed,
            Some(json!({"pattern": "fn main", "path": "src/main.rs", "include": "*.rs"})),
            None,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(
            text.matches("fn main").count() == 1,
            "pattern must appear exactly once: {}",
            text
        );
        assert!(
            text.contains("匹配 `fn main`"),
            "pattern should be shown as 匹配: {}",
            text
        );
        assert!(text.contains("src/main.rs"), "path missing: {}", text);
    }

    #[test]
    fn tool_input_glob_shows_pattern_once() {
        let tool = search_tool(
            "glob",
            SearchScope::FileName,
            ToolStatus::Completed,
            Some(json!({"pattern": "**/*.ts"})),
            None,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(
            text.matches("**/*.ts").count() == 1,
            "glob pattern must appear once: {}",
            text
        );
    }

    /// #202: a `skill` input is just the skill's name — show it as a one-liner
    /// instead of the generic `- name: …` key-value line.
    #[test]
    fn tool_input_skill_shows_its_name() {
        let tool = ToolPanel::for_test(
            "skill",
            ToolStatus::Completed,
            Some(json!({"name": "implement"})),
            None,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(text.contains("🧩 implement"), "skill name missing: {text}");
        assert!(!text.contains("- name:"), "generic kv leaked: {text}");
    }

    /// #202: a `question` input is a nested `questions` array that the generic
    /// key-value fallback rendered as one clipped JSON blob. The panel names
    /// (the first) question and its count instead.
    #[test]
    fn tool_input_question_summarizes_its_questions() {
        let input = json!({"questions": [
            {"question": "issue 202 你想让我做什么？", "header": "下一步",
             "options": [{"label": "先做盘点审计", "description": "…"},
                         {"label": "直接实现改进", "description": "…"}]},
            {"question": "第二个问题", "header": "其他", "options": []},
        ]});
        let tool = ToolPanel::for_test("question", ToolStatus::Running, Some(input.clone()), None);
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(text.contains("❓ 2 个问题"), "count missing: {text}");
        assert!(
            text.contains("issue 202 你想让我做什么？"),
            "first question missing: {text}"
        );
        assert!(!text.contains("- questions:"), "generic blob leaked: {text}");

        // The count reads the same for one question or many.
        let single = json!({"questions": [{"question": "继续?", "header": "确认", "options": []}]});
        assert_eq!(format_named_input("question", &single), "❓ 1 个问题 · 继续?");
        assert_eq!(
            format_named_input("question", &input),
            "❓ 2 个问题 · issue 202 你想让我做什么？"
        );
    }

    /// Regression: a tool whose input renders as markdown LIST lines (the
    /// generic fallback `- name: ...`, a skill's metadata list) must NOT swallow
    /// the Output marker as a lazy continuation of the last list line — Feishu
    /// then glues `**输出**` to the end of the input. A blank line between
    /// the sections keeps them on separate visual lines.
    #[test]
    fn tool_panel_input_and_output_separated_by_blank_line() {
        let tool = ToolPanel::for_test(
            "skill_apply",
            ToolStatus::Completed,
            Some(json!({
                "skill": "m15",
            })),
            Some("applied"),
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        // The last input list line and the Output marker must not be adjacent —
        // a single newline would make `**输出**` a lazy continuation of the
        // `- skill: m15` list item and Feishu would render them on one line.
        assert!(
            md.contains(&format!("- skill: m15\n\n{}\n", output_md())),
            "Input and Output sections must be separated by a blank line: {:?}",
            md
        );
        assert!(
            !md.contains(&format!("- skill: m15\n{}", output_md())),
            "no glued marker: {:?}",
            md
        );
    }

    #[test]
    fn tool_input_string_shows_as_is() {
        // A bare string input (non-object) renders directly.
        let tool = read_tool(ToolStatus::Completed, Some(json!("src/main.rs")), None);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        assert!(card.to_string().contains("src/main.rs"));
    }

    #[test]
    fn tool_input_unknown_falls_back_to_key_value() {
        let tool = ToolPanel::for_test(
            "custom_tool",
            ToolStatus::Completed,
            Some(json!({"a": "b", "c": 3})),
            None,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(text.contains("- a: b"), "kv line missing: {}", text);
        assert!(text.contains("- c: 3"), "kv line missing: {}", text);
    }

    #[test]
    fn tool_input_first_chunk_clips_long_values() {
        assert_eq!(first_chunk("short", 100), "short");
        assert_eq!(first_chunk("x", 1), "x");
        let long = "a".repeat(50);
        assert_eq!(first_chunk(&long, 10), format!("{}…", "a".repeat(10)));
        // Only the first line of multi-line input is shown.
        assert_eq!(first_chunk("first line\nsecond line", 100), "first line");
    }

    // --- OpenCode 2 built-in tools (spec #467) ------------------------------
    //
    // V2 renamed three built-ins (`bash`→`shell`, `task`→`subagent`,
    // `apply_patch`→`patch`), reshaped `read`/`websearch` output, and added the
    // Code Mode `execute` tool. These tests pin the arms that keep the same
    // information on the card for each.

    /// A ToolPanel over a call with recorded metadata (V2 records run state,
    /// diffs and nested calls there) and one text output block.
    fn panel_with(
        name: &str,
        status: ToolStatus,
        input: serde_json::Value,
        metadata: serde_json::Value,
        output: Option<&str>,
    ) -> ToolPanel {
        ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: name.into(),
                call_id: format!("call_{name}"),
            },
            status,
            started_at: None,
            input: Some(input),
            metadata: Some(metadata),
            output: ToolOutput {
                raw: output.map(|text| serde_json::Value::String(text.to_string())),
                blocks: output
                    .map(|text| vec![ContentBlock::Text(text.to_string())])
                    .unwrap_or_default(),
                error: None,
            },
            ..ToolCall::default()
        })
    }

    /// A panel with an explicit typed `FileEdits` detail, the adapter's own
    /// shape: named separately so a renderer fixture cannot disagree with
    /// itself about what the call touched.
    fn panel_with_edits(
        name: &str,
        status: ToolStatus,
        input: serde_json::Value,
        edits: Vec<FileEdit>,
        output: Option<&str>,
    ) -> ToolPanel {
        ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: name.into(),
                call_id: format!("call_{name}"),
            },
            status,
            started_at: None,
            input: Some(input),
            output: ToolOutput {
                raw: output.map(|text| serde_json::Value::String(text.to_string())),
                blocks: output
                    .map(|text| vec![ContentBlock::Text(text.to_string())])
                    .unwrap_or_default(),
                error: None,
            },
            kind: ToolKind::Edit,
            detail: ToolDetail::FileEdits(edits),
            ..ToolCall::default()
        })
    }

    fn panel_md(tool: ToolPanel) -> String {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content")
            .to_string()
    }

    /// [`panel_md`] with the Session a live Turn card renders with, so the
    /// fold's controls — the 「📂 打开」 button and the truncation note beside it
    /// (ADR-0083 §8 amendment, #770) — can exist. The body is found by tag: a
    /// panel with a button does not lead with its markdown.
    fn panel_md_with_session(tool: ToolPanel, session: &str) -> String {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool_at(tool, None, Some("tool_call"), Some(session))
            .build();
        card["body"]["elements"][0]["elements"]
            .as_array()
            .expect("panel body")
            .iter()
            .find(|element| element["tag"] == "markdown")
            .expect("the panel's markdown body")["content"]
            .as_str()
            .expect("panel markdown content")
            .to_string()
    }

    fn panel_title(tool: ToolPanel) -> String {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        card["body"]["elements"][0]["header"]["title"]["content"]
            .as_str()
            .expect("panel title")
            .to_string()
    }

    /// A shell-like call as the adapters declare it: the `Execute` family's
    /// `Command` shape is what the renderer reads (ADR-0105), so a shell fixture
    /// must carry it — a name alone selects nothing.
    /// A read panel with the typed `Read` detail production declares.
    fn read_tool(status: ToolStatus, input: Option<serde_json::Value>, output: Option<&str>) -> ToolPanel {
        let path = input
            .as_ref()
            .and_then(|input| input.get("filePath").or_else(|| input.get("path")))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let mut tool = panel_with(
            "read",
            status,
            input.unwrap_or(serde_json::Value::Null),
            serde_json::Value::Null,
            output,
        );
        tool.call.kind = ToolKind::Read;
        tool.call.detail = ToolDetail::Read {
            path,
            preview: None,
            directory: false,
        };
        tool
    }

    /// An OpenCode plan call as the adapter declares it: `Other` + `None`,
    /// carrying the neutral `TodoList` payload (ADR-0107's closed `plan`
    /// token). The card reads the payload, never the vendor id.
    fn plan_tool(status: ToolStatus, input: Option<serde_json::Value>, output: Option<&str>) -> ToolPanel {
        let mut tool = ToolPanel::for_test("todowrite", status, input, output);
        tool.call.kind = ToolKind::Other;
        tool.call.detail = ToolDetail::None;
        tool
    }

    /// A search panel with the typed `Search` detail production declares.
    fn search_tool(
        name: &str,
        scope: SearchScope,
        status: ToolStatus,
        input: Option<serde_json::Value>,
        output: Option<&str>,
    ) -> ToolPanel {
        let query = input
            .as_ref()
            .and_then(|input| {
                input.get(match scope {
                    SearchScope::Web => "query",
                    _ => "pattern",
                })
            })
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let mut tool = panel_with(
            name,
            status,
            input.unwrap_or(serde_json::Value::Null),
            serde_json::Value::Null,
            output,
        );
        tool.call.kind = ToolKind::Search;
        tool.call.detail = ToolDetail::Search {
            query,
            matches: None,
            scope,
        };
        tool
    }

    /// A search panel whose typed count comes from `metadata`, the way the
    /// adapter fills it.
    fn search_tool_with_metadata(
        name: &str,
        scope: SearchScope,
        status: ToolStatus,
        input: serde_json::Value,
        metadata: serde_json::Value,
        output: Option<&str>,
    ) -> ToolPanel {
        let mut tool = search_tool(name, scope, status, Some(input), output);
        let matches = metadata
            .get(match scope {
                SearchScope::FileName => "count",
                _ => "matches",
            })
            .and_then(serde_json::Value::as_u64);
        if let ToolDetail::Search { matches: slot, .. } = &mut tool.call.detail {
            *slot = matches;
        }
        tool.call.metadata = Some(metadata);
        tool
    }

    /// A write panel built from the raw input's path/content, with the typed
    /// `Add` detail production declares.
    fn write_tool(status: ToolStatus, input: Option<serde_json::Value>, output: Option<&str>) -> ToolPanel {
        let path = input
            .as_ref()
            .and_then(|input| input.get("filePath").or_else(|| input.get("path")))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let content = input
            .as_ref()
            .and_then(|input| input.get("content"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        write_panel(path, content, status, output)
    }

    /// An edit panel with no diff, the shape a running/successful edit declares
    /// before its metadata arrives.
    fn edit_tool(status: ToolStatus, input: Option<serde_json::Value>, output: Option<&str>) -> ToolPanel {
        let path = input
            .as_ref()
            .and_then(|input| input.get("filePath").or_else(|| input.get("path")))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let mut tool = panel_with(
            "edit",
            status,
            input.unwrap_or(serde_json::Value::Null),
            serde_json::Value::Null,
            output,
        );
        if let Some(path) = path {
            tool.call.kind = ToolKind::Edit;
            tool.call.detail = ToolDetail::FileEdits(vec![FileEdit {
                path,
                action: FileAction::Update,
                rename_to: None,
                diff: None,
            }]);
        }
        tool
    }

    fn command_panel(
        name: &str,
        status: ToolStatus,
        command: &str,
        cwd: Option<&str>,
        exit: Option<i64>,
        timeout: bool,
        output: Option<&str>,
    ) -> ToolPanel {
        let mut tool = panel_with(name, status, json!({"command": command}), json!({}), output);
        tool.call.kind = ToolKind::Execute;
        tool.call.detail = ToolDetail::Command {
            command: command.to_string(),
            cwd: cwd.map(str::to_string),
            description: None,
            exit,
            timeout,
        };
        tool
    }

    /// A Code Mode program as the adapter declares it: the `Program` shape,
    /// never the `execute` name, is what the renderer reads.
    fn program_panel(
        name: &str,
        status: ToolStatus,
        source: &str,
        calls: Vec<NestedCall>,
        error: bool,
        output: Option<&str>,
    ) -> ToolPanel {
        let mut tool = panel_with(name, status, json!({"code": source}), json!({}), output);
        tool.call.kind = ToolKind::Execute;
        tool.call.detail = ToolDetail::Program {
            source: source.to_string(),
            language: None,
            calls,
            error,
        };
        tool
    }

    /// One nested Code Mode call.
    fn nested(name: &str, status: ToolStatus) -> NestedCall {
        NestedCall {
            name: name.to_string(),
            status,
            input: None,
        }
    }

    /// V2 spells the file-editing patch tool `patch` (V1: `apply_patch`) and
    /// records the same `metadata.files[]` diffs `edit` does. The panel must
    /// render the hunks, name the touched files in the input, and drop the
    /// generated success summary.
    #[test]
    fn patch_v2_renders_hunks_and_names_the_files() {
        let patch = "\
Index: /a/one.rs
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1,3 +1,3 @@
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;";
        let tool = panel_with_edits(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch\n*** Update File: /a/one.rs\n@@\n-let b = 2;\n+let b = 3;\n*** End Patch"}),
            vec![file_edit("/a/one.rs", FileAction::Update, patch)],
            Some("Success. Updated the following files:\nM /a/one.rs"),
        );
        let md = panel_md(tool);
        assert!(
            md.contains(&format!("{}\n✏️ 编辑 `/a/one.rs`", input_md())),
            "file in input: {md}"
        );
        assert!(
            md.contains(&format!("{}\n\n+1 −1", output_md())),
            "count header: {md}"
        );
        assert!(md.contains("```rust\n@@"), "fenced hunks: {md}");
        assert!(
            md.contains("-let b = 2;") && md.contains("+let b = 3;"),
            "hunks: {md}"
        );
        assert!(!md.contains("Success. Updated"), "summary dropped: {md}");

        // Several files: the input names each file's action.
        let several = panel_with_edits(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch\n*** Add File: a.rs\n*** Delete File: b.rs\n*** End Patch"}),
            vec![
                FileEdit {
                    path: "a.rs".into(),
                    action: FileAction::Add,
                    rename_to: None,
                    diff: None,
                },
                FileEdit {
                    path: "b.rs".into(),
                    action: FileAction::Delete,
                    rename_to: None,
                    diff: None,
                },
            ],
            None,
        );
        assert_eq!(format_tool_input(&several), "➕ 新增 `a.rs`\n🗑 删除 `b.rs`");
        // An unparseable patchText falls back to the generic key-value list,
        // never a raw JSON dump.
        assert_eq!(
            format_tool_input(&ToolPanel::for_test(
                "patch",
                ToolStatus::Completed,
                Some(json!({"patchText": "not a patch"})),
                None
            )),
            "- patchText: not a patch"
        );
        // V1's `apply_patch` answers the same patch-text shape (P3 completes
        // the mapping on both generations).
        assert_eq!(
            format_tool_input(&panel_with_edits(
                "apply_patch",
                ToolStatus::Completed,
                json!({"patchText": "*** Begin Patch\n*** Update File: a.rs\n@@\n-a\n+b\n*** End Patch"}),
                vec![FileEdit {
                    path: "a.rs".into(),
                    action: FileAction::Update,
                    rename_to: None,
                    diff: Some("@@\n-a\n+b".into()),
                }],
                None
            )),
            "✏️ 编辑 `a.rs`"
        );
    }

    /// V2 spells the subagent tool `subagent` (V1: `task`) and names the agent
    /// type `agent` (V1: `subagent_type`); the panel renders the same one-line
    /// summary for both.
    #[test]
    fn subagent_input_shows_the_agent_and_description() {
        assert_eq!(
            delegation_input(
                &json!({"agent": "general", "description": "review the diff", "prompt": "…", "background": true})
            ),
            "🔀 review the diff\n🤖 `general`"
        );
        assert_eq!(
            delegation_input(&json!({"description": "x", "subagent_type": "build"})),
            "🔀 x\n🤖 `build`"
        );
        assert_eq!(
            delegation_input(&json!({"agent": "explore"})),
            "🔀 子任务\n🤖 `explore`"
        );
    }

    /// ADR-0105 §5: a launch's panel is selected by its neutral `Delegation`
    /// detail, never its identity name. A third adapter's launch under a new id
    /// renders the same Input line and strips the same envelope.
    #[test]
    fn a_launch_panel_is_selected_by_its_detail_not_its_name() {
        let raw = "\
<task id=\"ses_1\" state=\"completed\">
<task_result>
Report
</task_result>
</task>";
        let launch = |name: &str| {
            ToolPanel::new(ToolCall {
                identity: ToolIdentity {
                    name: name.into(),
                    call_id: "call_launch".into(),
                },
                kind: ToolKind::Other,
                status: ToolStatus::Completed,
                input: Some(json!({"description": "review the diff", "agent": "general"})),
                output: ToolOutput {
                    raw: Some(json!(raw)),
                    blocks: vec![ContentBlock::Text(raw.to_string())],
                    error: None,
                },
                detail: ToolDetail::Delegation {
                    runs: vec![crate::backend::AgentRunRef {
                        run_id: "thread_1".into(),
                        status: None,
                    }],
                    effect: DelegationEffect::Launch,
                },
                ..ToolCall::default()
            })
        };

        let third_party = launch("third_party/launch");
        assert_eq!(
            format_tool_input(&third_party),
            "🔀 review the diff\n🤖 `general`",
            "the detail, not the id, names the launch's input"
        );
        let (_, _, body) = format_tool_output(&third_party, raw);
        assert_eq!(body.trim(), "Report", "the envelope is stripped by detail");
        assert!(!body.contains("<task"), "wrapper stripped: {body:?}");
    }

    /// V2's subagent output is a `<subagent sessionID state>` envelope with the
    /// report as its whole body (no `<summary>`/`<task_result>` inner tags like
    /// V1's `<task>`). The wrapper must never reach the card; a background
    /// handle is plain text and stays unchanged.
    #[test]
    fn subagent_v2_output_strips_the_envelope() {
        let raw = "<subagent sessionID=\"ses_child\" state=\"completed\">\n\
                   Report line 1\n\nReport line 2\n</subagent>";
        let (header, style, body) = delegation_output(raw);
        assert_eq!(header, None);
        assert_eq!(style, BodyStyle::Auto);
        assert!(body.starts_with("Report line 1"), "report kept: {body:?}");
        assert!(body.contains("Report line 2"), "report kept: {body:?}");
        assert!(!body.contains("<subagent"), "wrapper stripped: {body:?}");
        assert!(!body.contains("</subagent>"), "wrapper stripped: {body:?}");

        let handle = "The subagent is working in the background (sessionID: ses_x).";
        assert_eq!(delegation_output(handle).2, handle);
    }

    /// V2's `skill` input is `{id}` where V1's was `{name}`; both render the
    /// skill's name, and V2's output envelope (`<skill_content>`) is unchanged
    /// and already handled.
    #[test]
    fn skill_input_reads_v2_id_and_v1_name() {
        assert_eq!(
            format_named_input("skill", &json!({"id": "implement"})),
            "🧩 implement"
        );
        assert_eq!(
            format_named_input("skill", &json!({"name": "caveman"})),
            "🧩 caveman"
        );
    }

    /// ADR-0107: a Codex item kind is not a title. The same tokens every
    /// adapter reaches name its calls (`shell`, `edit`, `websearch`, `read`,
    /// `subagent`), so no translated noun (`命令`, `文件修改`, …) survives. A
    /// third-party tool, and a namespaced id whose adapter never declared a
    /// shape, keep their own full name (ADR-0042).
    #[test]
    fn codex_item_kinds_render_their_tokens() {
        /// One fixture with the typed shape the adapter declares.
        fn typed(
            name: &str,
            kind: ToolKind,
            detail: ToolDetail,
            input: serde_json::Value,
            output: &str,
        ) -> ToolPanel {
            let mut tool = ToolPanel::for_test(name, ToolStatus::Completed, Some(input), Some(output));
            tool.call.kind = kind;
            tool.call.detail = detail;
            tool
        }
        let title = |tool: ToolPanel| panel_title(tool);

        assert_eq!(
            title(typed(
                "codex/command_execution",
                ToolKind::Execute,
                ToolDetail::Command {
                    command: "ls".into(),
                    cwd: Some("/w".into()),
                    description: None,
                    exit: None,
                    timeout: false,
                },
                json!({"command": "ls", "cwd": "/w"}),
                "ok"
            )),
            format!("✅ {}", vocab::TOOL_SHELL)
        );
        // A single-file change also names its file while folded (#902), the way
        // an `edit` panel does.
        assert_eq!(
            title(codex_file_change_panel(vec![json!({
                "path": "a.rs",
                "kind": {"type": "update"},
                "diff": "@@ -1 +1 @@\n-a\n+b"
            })])),
            format!("✅ {} · ✏️ 编辑 a.rs", vocab::TOOL_EDIT)
        );
        assert_eq!(
            title(codex_web_search_panel(json!([]))),
            format!("✅ {}", vocab::TOOL_WEBSEARCH)
        );
        // `imageView` is a `read` in the neutral model (it names a file the
        // card viewed); `imageGeneration` has no read shape and stays opaque.
        assert_eq!(
            title(typed(
                "codex/image",
                ToolKind::Read,
                ToolDetail::Read {
                    path: Some("/w/shot.png".into()),
                    preview: None,
                    directory: false,
                },
                json!({"path": "/w/shot.png"}),
                "ok"
            )),
            format!("✅ {}", vocab::TOOL_READ)
        );
        // A collaboration call names its target on the folded title (issue
        // #962). A LAUNCH is the `subagent` token; an addressing verb has no
        // token — production renders it as a receipt, and the panel path keeps
        // the vendor id rather than a translated noun.
        let collab = |tool: &str| {
            json!({
                "tool": tool,
                "status": "completed",
                "senderThreadId": "thread-root",
                "receiverThreadIds": ["0192f0aa-0000-7000-8000-000000000001"],
                "prompt": null,
                "model": null,
                "reasoningEffort": null,
                "agents": [{
                    "threadId": "0192f0aa-0000-7000-8000-000000000001",
                    "status": "running",
                    "message": null
                }]
            })
        };
        let delegation = |effect| ToolDetail::Delegation {
            runs: Vec::new(),
            effect,
        };
        assert_eq!(
            title(typed(
                "codex/collab_agent",
                ToolKind::Other,
                delegation(DelegationEffect::Launch),
                collab("spawnAgent"),
                "ok"
            )),
            format!("✅ {} · 🧵 0192f0aa", vocab::TOOL_SUBAGENT)
        );
        for verb in ["wait", "sendInput", "futureTool"] {
            assert_eq!(
                title(typed(
                    "codex/collab_agent",
                    ToolKind::Other,
                    delegation(DelegationEffect::Address),
                    collab(verb),
                    "ok"
                )),
                "✅ codex/collab_agent · 🧵 0192f0aa",
                "an addressing call has no token: {verb}"
            );
        }

        // A third-party tool keeps its own name even when it shares a payload
        // key; the ids cola's adapter writes are namespaced and carry a typed
        // shape, so an undeclared one is opaque.
        assert_eq!(
            title(ToolPanel::for_test(
                "image",
                ToolStatus::Completed,
                Some(json!({"path": "/w/shot.png"})),
                Some("ok")
            )),
            "✅ image"
        );
        assert_eq!(
            title(ToolPanel::for_test(
                "command_execution",
                ToolStatus::Completed,
                Some(json!({"command": "ls", "cwd": "/w"})),
                Some("ok")
            )),
            "✅ command_execution"
        );
        assert_eq!(
            title(ToolPanel::for_test(
                "codex/command_execution",
                ToolStatus::Completed,
                Some(json!({"argv": ["ls"]})),
                Some("ok")
            )),
            "✅ codex/command_execution",
            "a partial shape under the namespaced id is not the adapter's call"
        );
        assert_eq!(
            title(ToolPanel::for_test(
                "codex/collab_agent",
                ToolStatus::Completed,
                Some(json!({"tool": "spawnAgent", "status": "completed"})),
                Some("ok")
            )),
            "✅ codex/collab_agent",
            "the adapter always writes the sender, the receiver list and the rows"
        );
        assert_eq!(
            title(ToolPanel::for_test(
                "codex/web_search",
                ToolStatus::Completed,
                Some(json!({"query": "rust"})),
                Some("ok")
            )),
            "✅ codex/web_search",
            "the adapter always writes `action`; a partial shape stays raw"
        );
        assert_eq!(
            title(ToolPanel::for_test(
                "codex/file_change",
                ToolStatus::Completed,
                Some(json!({"changes": [{"path": "a.rs"}]})),
                Some("ok")
            )),
            "✅ codex/file_change",
            "an entry without `kind` is not the adapter's shape"
        );
        // An OpenCode built-in reaches its token through the shape, not the id.
        assert_eq!(
            title(typed(
                "bash",
                ToolKind::Execute,
                ToolDetail::Command {
                    command: "ls".into(),
                    cwd: None,
                    description: None,
                    exit: None,
                    timeout: false,
                },
                json!({"command": "ls"}),
                "ok"
            )),
            format!("✅ {}", vocab::TOOL_SHELL)
        );
        // An MCP tool's own `server/tool` name is never remapped.
        assert_eq!(
            title(ToolPanel::for_test(
                "context7/query-docs",
                ToolStatus::Completed,
                Some(json!({"query": "rust"})),
                Some("ok")
            )),
            "✅ context7/query-docs"
        );
    }

    /// Issue #962: a collaboration call is one folded card whose body carries
    /// the prompt, the model/effort the child was started with, and one row per
    /// target's own state; the folded title names the first target (and how many
    /// others the call covers). A call with no receivers renders no rows.
    #[test]
    fn a_collab_call_renders_prompt_model_and_target_rows() {
        let input = json!({
            "tool": "spawnAgent",
            "status": "completed",
            "senderThreadId": "thread-root",
            "receiverThreadIds": ["0192f0aa-0000-7000-8000-000000000001"],
            "prompt": "Review the diff and report the blocking findings.",
            "model": "gpt-5.1-codex",
            "reasoningEffort": "high",
            "agents": [{
                "threadId": "0192f0aa-0000-7000-8000-000000000001",
                "status": "running",
                "message": null
            }]
        });
        let tool = ToolPanel::for_test("codex/collab_agent", ToolStatus::Completed, Some(input), None);
        let md = panel_md(tool.clone());
        assert!(md.contains("📝 Review the diff"), "the prompt: {md}");
        assert!(md.contains("🤖 `gpt-5.1-codex` · high"), "the model: {md}");
        assert!(
            md.contains(&format!("{}\n\n⏳ `0192f0aa` · 运行中", output_md())),
            "one row for the target's own state, in the Output body: {md}"
        );
        assert_eq!(panel_title(tool), "✅ codex/collab_agent · 🧵 0192f0aa");

        // The start payload has no receiver yet: no row, no target on the
        // title, and the call reads as running.
        let started = ToolPanel::for_test(
            "codex/collab_agent",
            ToolStatus::Running,
            Some(json!({
                "tool": "spawnAgent",
                "status": "inProgress",
                "senderThreadId": "thread-root",
                "receiverThreadIds": [],
                "prompt": "Review the diff and report the blocking findings.",
                "model": "gpt-5.1-codex",
                "reasoningEffort": "high",
                "agents": []
            })),
            None,
        );
        let md = panel_md(started.clone());
        assert!(!md.contains("尚无状态"), "no empty rows: {md}");
        assert_eq!(panel_title(started), "⏳ codex/collab_agent");
    }

    /// Issue #962, review round 1: a long prompt and several targets with long
    /// state messages must not hide later rows. The rows ride the Output body
    /// (its 3,000-character budget), not the Input line's shared 400-character
    /// clip, while the folded title still counts every target.
    #[test]
    fn a_long_prompt_and_many_targets_keep_every_row() {
        let agents: Vec<serde_json::Value> = (1..=6)
            .map(|n| {
                let message = if n == 6 {
                    serde_json::Value::String(format!("worker {n}: {}", "e".repeat(300)))
                } else {
                    serde_json::Value::Null
                };
                json!({
                    "threadId": format!("{n:08}-0000-7000-8000-000000000000"),
                    "status": if n == 6 { "errored" } else { "completed" },
                    "message": message
                })
            })
            .collect();
        let receivers: Vec<serde_json::Value> =
            agents.iter().map(|agent| agent["threadId"].clone()).collect();
        let tool = ToolPanel::for_test(
            "codex/collab_agent",
            ToolStatus::Completed,
            Some(json!({
                "tool": "wait",
                "status": "completed",
                "senderThreadId": "thread-root",
                "receiverThreadIds": receivers,
                "prompt": "p".repeat(500),
                "model": "gpt-5.1-codex",
                "reasoningEffort": "high",
                "agents": agents
            })),
            None,
        );
        let md = panel_md(tool.clone());
        for n in 1..=6 {
            let short = format!("{n:08}");
            assert!(md.contains(&format!("`{short}`")), "target {n} missing: {md}");
        }
        assert!(md.contains("❌ `00000006` · 出错"), "the failure row: {md}");
        assert!(md.contains("📝 ppp"), "the prompt stays in the Input: {md}");
        assert_eq!(panel_title(tool), "✅ codex/collab_agent · 🧵 00000001 +5");
    }

    /// Review round 2: more targets than the output budget can hold end on an
    /// explicit `… 另有 N 个目标` line — every shown row is whole, and the card
    /// never hides an outcome behind a bare clip. The title still counts all.
    #[test]
    fn too_many_targets_end_on_an_explicit_remainder() {
        let agents: Vec<serde_json::Value> = (1..=20)
            .map(|n| {
                json!({
                    "threadId": format!("{n:08}-0000-7000-8000-000000000000"),
                    "status": "completed",
                    "message": "m".repeat(200)
                })
            })
            .collect();
        let receivers: Vec<serde_json::Value> =
            agents.iter().map(|agent| agent["threadId"].clone()).collect();
        let tool = ToolPanel::for_test(
            "codex/collab_agent",
            ToolStatus::Completed,
            Some(json!({
                "tool": "wait",
                "status": "completed",
                "senderThreadId": "thread-root",
                "receiverThreadIds": receivers,
                "prompt": null,
                "model": null,
                "reasoningEffort": null,
                "agents": agents
            })),
            None,
        );
        let md = panel_md(tool.clone());
        let last = md.trim_end().lines().last().expect("a panel body");
        assert!(last.starts_with("… 另有 "), "explicit remainder: {last:?}");
        assert!(last.ends_with(" 个目标"), "{last:?}");
        assert!(md.contains("`00000001`"), "the first rows render: {md}");
        assert!(!md.contains("… ✅"), "no row is clipped mid-body: {md}");
        assert_eq!(panel_title(tool), "✅ codex/collab_agent · 🧵 00000001 +19");
    }

    /// Review round 2: a status this build does not model keeps its own string
    /// on the card rather than being reduced to the wrench icon alone.
    #[test]
    fn an_unknown_collab_status_shows_its_own_string() {
        let tool = ToolPanel::for_test(
            "codex/collab_agent",
            ToolStatus::Other("futureStatus".into()),
            Some(json!({
                "tool": "spawnAgent",
                "status": "futureStatus",
                "senderThreadId": "thread-root",
                "receiverThreadIds": [],
                "prompt": null,
                "model": null,
                "reasoningEffort": null,
                "agents": []
            })),
            None,
        );
        let md = panel_md(tool.clone());
        assert!(md.contains("🔧 futureStatus"), "{md}");
        assert_eq!(panel_title(tool), "🔧 codex/collab_agent");
    }

    /// Every target of a `wait` gets its own row with its own outcome — the
    /// call's completion does not flatten a failed child into success — and the
    /// title counts the others.
    #[test]
    fn a_collab_call_rows_keep_each_targets_own_state() {
        let tool = ToolPanel::for_test(
            "codex/collab_agent",
            ToolStatus::Completed,
            Some(json!({
                "tool": "wait",
                "status": "completed",
                "senderThreadId": "thread-root",
                "receiverThreadIds": [
                    "0192f0aa-0000-7000-8000-000000000001",
                    "0192f0bb-0000-7000-8000-000000000002"
                ],
                "prompt": null,
                "model": null,
                "reasoningEffort": null,
                "agents": [
                    {"threadId": "0192f0aa-0000-7000-8000-000000000001",
                     "status": "completed", "message": null},
                    {"threadId": "0192f0bb-0000-7000-8000-000000000002",
                     "status": "errored", "message": "the worker hit a compile error"}
                ]
            })),
            None,
        );
        let md = panel_md(tool.clone());
        assert!(md.contains("✅ `0192f0aa` · 完成"), "{md}");
        assert!(
            md.contains("❌ `0192f0bb` · 出错 · the worker hit a compile error"),
            "a failed target keeps its failure and its message: {md}"
        );
        assert!(
            !md.contains("📝"),
            "no prompt line when the call carries none: {md}"
        );
        assert_eq!(panel_title(tool), "✅ codex/collab_agent · 🧵 0192f0aa +1");
    }

    /// Issue #962: a `subAgentActivity` is one small row naming what happened,
    /// which child, and the model it was started with; the folded title carries
    /// the child's readable path.
    #[test]
    fn a_sub_agent_activity_row_names_its_path_and_kind() {
        let tool = ToolPanel::for_test(
            "codex/sub_agent_activity",
            ToolStatus::Completed,
            Some(json!({
                "kind": "started",
                "agentThreadId": "0192f0aa-0000-7000-8000-000000000001",
                "agentPath": "/root/worker",
                "model": "gpt-5.1-codex",
                "reasoningEffort": "high"
            })),
            None,
        );
        let md = panel_md(tool.clone());
        assert!(md.contains("🌱 启动 · 🧵 `0192f0aa`"), "{md}");
        assert!(md.contains("🤖 `gpt-5.1-codex` · high"), "{md}");
        assert_eq!(panel_title(tool), "✅ codex/sub_agent_activity · 🧵 /root/worker");
    }

    /// The folded title's target detail (issue #962) is charged to the card
    /// estimate, exactly as the file detail is: an uncharged fragment could let
    /// a split pass the size check and then be rejected by Feishu.
    #[test]
    fn the_collab_target_detail_is_charged() {
        let collab = |agents: serde_json::Value| {
            ToolPanel::for_test(
                "codex/collab_agent",
                ToolStatus::Completed,
                Some(json!({
                    "tool": "spawnAgent",
                    "status": "completed",
                    "senderThreadId": "thread-root",
                    "receiverThreadIds": ["0192f0aa-0000-7000-8000-000000000001"],
                    "prompt": null,
                    "model": null,
                    "reasoningEffort": null,
                    "agents": agents
                })),
                None,
            )
        };
        let row = json!([{
            "threadId": "0192f0aa-0000-7000-8000-000000000001",
            "status": "running",
            "message": null
        }]);
        assert!(panel_target_detail_estimate(&collab(row)) > 0);

        // No rows (the start payload): the title carries no target fragment.
        assert_eq!(panel_target_detail_estimate(&collab(json!([]))), 0);
        // A panel that is not a collaboration panel carries none either.
        let bash = ToolPanel::for_test("bash", ToolStatus::Completed, None, None);
        assert_eq!(panel_target_detail_estimate(&bash), 0);
    }

    /// Review round 4, from a live Codex run: V2's `wait` item is always empty
    /// (`receiverThreadIds: []`, `agentsStates: {}`) and its result is a tool
    /// output the item stream does not carry. The card must not read as a
    /// rendering gap — it names the call's own state and says the target state
    /// was not reported.
    #[test]
    fn a_payload_less_wait_card_says_why_it_has_no_rows() {
        let tool = ToolPanel::for_test(
            "codex/collab_agent",
            ToolStatus::Completed,
            Some(json!({
                "tool": "wait",
                "status": "completed",
                "senderThreadId": "thread-root",
                "receiverThreadIds": [],
                "prompt": null,
                "model": null,
                "reasoningEffort": null,
                "agents": []
            })),
            None,
        );
        let md = panel_md(tool.clone());
        assert!(
            md.contains("✅ 完成 · 未上报目标状态"),
            "the call's own state and why the rows are absent: {md}"
        );
        assert!(!md.contains("_(no details)_"), "never an empty body: {md}");
        assert_eq!(panel_title(tool), "✅ codex/collab_agent");
    }

    /// Issue #962 acceptance 2's readable name: when the read carried the
    /// child's own `agentPath`, the title and the row name it, not its id.
    #[test]
    fn a_target_with_a_known_agent_path_is_named_by_it() {
        let tool = ToolPanel::for_test(
            "codex/collab_agent",
            ToolStatus::Completed,
            Some(json!({
                "tool": "wait",
                "status": "completed",
                "senderThreadId": "thread-root",
                "receiverThreadIds": ["0192f0aa-0000-7000-8000-000000000001"],
                "prompt": null,
                "model": null,
                "reasoningEffort": null,
                "agents": [{
                    "threadId": "0192f0aa-0000-7000-8000-000000000001",
                    "label": "/root/spawn_smoke_test",
                    "status": "completed",
                    "message": null
                }]
            })),
            None,
        );
        assert_eq!(
            panel_title(tool.clone()),
            "✅ codex/collab_agent · 🧵 /root/spawn_smoke_test"
        );
        let md = panel_md(tool);
        assert!(
            md.contains("✅ `/root/spawn_smoke_test` · 完成"),
            "the row names the agent path: {md}"
        );
        assert!(
            !md.contains("未上报目标状态"),
            "a call with rows is not payload-less: {md}"
        );
    }

    /// Review round 7: the payload-less fallback is about the ROW list, not
    /// just the prompt/model lines — a call with target states but no prompt
    /// must not claim no state was reported.
    #[test]
    fn rows_without_a_prompt_do_not_claim_no_state_was_reported() {
        let tool = ToolPanel::for_test(
            "codex/collab_agent",
            ToolStatus::Completed,
            Some(json!({
                "tool": "wait",
                "status": "completed",
                "senderThreadId": "thread-root",
                "receiverThreadIds": ["0192f0aa-0000-7000-8000-000000000001"],
                "prompt": null,
                "model": null,
                "reasoningEffort": null,
                "agents": [{
                    "threadId": "0192f0aa-0000-7000-8000-000000000001",
                    "status": "errored",
                    "message": "boom"
                }]
            })),
            None,
        );
        let md = panel_md(tool);
        assert!(!md.contains("未上报目标状态"), "{md}");
        assert!(md.contains("❌ `0192f0aa` · 出错 · boom"), "{md}");
    }

    /// Review round 5: a very deep `agentPath` must not blow the folded
    /// header's one-line budget — the activity keeps the path's tail (the
    /// agent's own name) and clips its head, like a long file name.
    #[test]
    fn a_deep_activity_path_keeps_its_tail_within_the_header_budget() {
        let deep = format!("/root/{}worker", "segment/".repeat(400));
        let tool = ToolPanel::for_test(
            "codex/sub_agent_activity",
            ToolStatus::Completed,
            Some(json!({
                "kind": "started",
                "agentThreadId": "0192f0aa-0000-7000-8000-000000000001",
                "agentPath": deep,
                "model": null,
                "reasoningEffort": null
            })),
            None,
        );
        let title = panel_title(tool);
        assert!(
            title.starts_with("✅ codex/sub_agent_activity · 🧵 …"),
            "the head is clipped and the tail kept: {title}"
        );
        assert!(
            title.chars().count() <= 60,
            "the header stays a short line: {title}"
        );
        assert!(title.ends_with("segment/worker"), "the leaf survives: {title}");
    }

    /// Review round 6: a path is a valid Unix path even when it carries a
    /// newline or a Unicode line separator, and the folded title is one
    /// `plain_text` line — every break folds to a space (the file-name rule).
    #[test]
    fn a_line_breaking_activity_path_stays_one_title_line() {
        let tool = ToolPanel::for_test(
            "codex/sub_agent_activity",
            ToolStatus::Completed,
            Some(json!({
                "kind": "started",
                "agentThreadId": "0192f0aa-0000-7000-8000-000000000001",
                "agentPath": "/root/worker\nsegment\u{2028}leaf",
                "model": null,
                "reasoningEffort": null
            })),
            None,
        );
        let title = panel_title(tool);
        assert_eq!(title.lines().count(), 1, "one title line: {title:?}");
        assert!(
            !title.contains('\n') && !title.contains('\u{2028}'),
            "no break survives: {title:?}"
        );
    }

    /// The activity arm is gated on the adapter's whole shape (ADR-0042): a
    /// payload missing `agentPath`, or carrying a non-string optional field, is
    /// not the adapter's and stays opaque rather than rendering a partial row.
    #[test]
    fn a_partial_sub_agent_activity_shape_stays_raw() {
        let title = |input: serde_json::Value| {
            let tool = ToolPanel::for_test(
                "codex/sub_agent_activity",
                ToolStatus::Completed,
                Some(input),
                None,
            );
            let card = CardBuilder::new()
                .with_state(CardState::Done)
                .with_tool(tool)
                .build();
            card["body"]["elements"][0]["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let missing_path = json!({
            "kind": "started",
            "agentThreadId": "0192f0aa-0000-7000-8000-000000000001",
            "model": null,
            "reasoningEffort": null
        });
        assert_eq!(
            title(missing_path.clone()),
            "✅ codex/sub_agent_activity",
            "the adapter always writes agentPath"
        );
        assert_eq!(
            title(json!({
                "kind": "started",
                "agentThreadId": "0192f0aa-0000-7000-8000-000000000001",
                "agentPath": "/root/worker",
                "model": 3,
                "reasoningEffort": null
            })),
            "✅ codex/sub_agent_activity",
            "an optional field must be a string or null"
        );
        // The body falls back to the generic key-value list, never a partial
        // activity row.
        let body = format_named_input("codex/sub_agent_activity", &missing_path);
        assert!(
            body.contains("- kind: started") && !body.contains("🌱"),
            "the generic key-value fallback, never a partial row: {body}"
        );
    }

    /// The `Execute` family's `Command` shape renders one Input line on every
    /// producer: `bash` (V1), `shell` (V2) and `codex/command_execution` each
    /// reach it through `(kind, detail)`, never through a name (ADR-0105,
    /// spec #1057's P2). The adapter has already resolved `workdir`/`cwd` into
    /// the shape, so there is no vendor split left in the card.
    #[test]
    fn every_command_producer_renders_the_same_input_line() {
        let expected = "`/usr/bin/zsh -lc 'echo hi'`\n📁 `/work/cola`";
        for name in ["bash", "shell", "codex/command_execution"] {
            let tool = command_panel(
                name,
                ToolStatus::Completed,
                "/usr/bin/zsh -lc 'echo hi'",
                Some("/work/cola"),
                Some(0),
                false,
                None,
            );
            assert_eq!(format_tool_input(&tool), expected, "{name}");
        }
        // A command may itself carry backticks: the inline span grows past the
        // longest run, so the command cannot close it early.
        let quoted = command_panel(
            "shell",
            ToolStatus::Completed,
            "echo `pwd`",
            Some("/work/cola"),
            None,
            false,
            None,
        );
        assert_eq!(format_tool_input(&quoted), "`` echo `pwd` ``\n📁 `/work/cola`");
        // V1's `bash` names the call with a one-line description; the shape
        // keeps it rather than dropping a line the reader had.
        let mut described = command_panel("bash", ToolStatus::Completed, "ls", Some("/w"), None, false, None);
        if let ToolDetail::Command { description, .. } = &mut described.call.detail {
            *description = Some("List the tree".to_string());
        }
        assert_eq!(format_tool_input(&described), "`ls`\n📁 `/w`\n📝 List the tree");
        // A call with NO declared shape is a third party: `codex/*` ids are the
        // adapter's own, so a payload without the shape stays on the opaque path
        // instead of being read as cola's.
        let foreign = ToolPanel::for_test(
            "codex/command_execution",
            ToolStatus::Completed,
            Some(json!({"command": "ls", "cwd": "/w"})),
            None,
        );
        assert!(
            format_tool_input(&foreign).starts_with("- command:"),
            "no shape, no tailored render"
        );
    }

    /// A declared shape owns its line, the empty one included: an empty Input
    /// line renders NO section and never falls through to the raw payload's key
    /// table, so no `command`/`workdir` key leaks onto the card.
    #[test]
    fn an_empty_declared_shape_renders_no_input_section() {
        let command = command_panel("shell", ToolStatus::Completed, "", None, None, false, Some("out"));
        assert_eq!(format_tool_input(&command), "", "an empty command is no line");
        let program = program_panel(
            "execute",
            ToolStatus::Completed,
            "",
            vec![nested("fetch", ToolStatus::Completed)],
            false,
            Some("out"),
        );
        assert_eq!(format_tool_input(&program), "", "an empty source is no line");

        for tool in [command, program] {
            let card = CardBuilder::new()
                .with_state(CardState::Done)
                .with_tool(tool)
                .build();
            let text = card.to_string();
            assert!(!text.contains(input_md().as_str()), "no empty section: {text}");
            assert!(
                !text.contains("- command:") && !text.contains("- workdir:") && !text.contains("- code:"),
                "no raw payload key leaks: {text}"
            );
        }

        // The raw payload may still carry a command the shape does not: the
        // shape is the whole Input line, so it stays silent.
        let mut silent = command_panel("shell", ToolStatus::Completed, "", None, None, false, None);
        silent.call.input = Some(json!({"command": "ls -la", "workdir": "/w"}));
        assert_eq!(format_tool_input(&silent), "", "the shape decides");
    }

    /// A Codex file change's Input line names every touched file with its
    /// action, from the typed `FileEdits` detail; a payload that is not the
    /// adapter's shape stays generic.
    #[test]
    fn codex_file_change_input_lists_its_files() {
        let one = codex_file_change_panel(vec![json!({
            "path": "src/main.rs",
            "kind": {"type": "update"},
            "diff": "@@ -1 +1 @@\n-old\n+new",
        })]);
        assert_eq!(format_tool_input(&one), "✏️ 编辑 `src/main.rs`");

        let two = codex_file_change_panel(vec![
            json!({"path": "a.rs", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n-a\n+b"}),
            json!({"path": "b.rs", "kind": {"type": "add"}, "diff": "@@ -0,0 +1 @@\n+b"}),
        ]);
        assert_eq!(format_tool_input(&two), "✏️ 编辑 `a.rs`\n➕ 新增 `b.rs`");

        // A rename names the old → new pair.
        let rename = codex_file_change_panel(vec![json!({
            "path": "old.rs",
            "kind": {"type": "update", "movePath": "new.rs"},
            "diff": "@@ -1 +1 @@\n-old\n+new",
        })]);
        assert_eq!(format_tool_input(&rename), "📦 重命名 `old.rs` → `new.rs`");

        // A malformed entry is not the adapter's shape and stays generic.
        let partial = format_tool_input(&ToolPanel::for_test(
            "codex/file_change",
            ToolStatus::Completed,
            Some(json!({"changes": [{"path": "a.rs"}, {"path": 17}]})),
            None,
        ));
        assert!(
            partial.starts_with("- changes:"),
            "a malformed entry takes the generic shape, not a partial list: {partial}"
        );
        let no_kind = format_tool_input(&ToolPanel::for_test(
            "codex/file_change",
            ToolStatus::Completed,
            Some(json!({"changes": [{"path": "a.rs"}]})),
            None,
        ));
        assert!(
            no_kind.starts_with("- changes:"),
            "a missing kind takes the generic shape: {no_kind}"
        );

        // A backtick in a path cannot escape the inline span it names.
        let backtick = codex_file_change_panel(vec![json!({
            "path": "a`b.rs",
            "kind": {"type": "update"},
            "diff": "@@ -1 +1 @@\n-a\n+b",
        })]);
        assert_eq!(format_tool_input(&backtick), "✏️ 编辑 ``a`b.rs``");
    }

    /// Codex's `imageView` names the file it looked at, with the same backtick
    /// flattening as a file change's path.
    #[test]
    fn codex_image_input_names_its_file() {
        assert_eq!(
            image_input(&json!({"path": "/w/shot `1`.png"})),
            "🖼 ``/w/shot `1`.png``"
        );
        assert!(
            image_input(&json!({"path": "  "})).starts_with("- path:"),
            "a blank path falls back to the generic rendering"
        );
    }

    /// `imageView` and `imageGeneration` share the identity `codex/image` and a
    /// `path` input, but only the former is a neutral `Read`. The Opaque
    /// generation must not take the image-view Input line or read preview.
    #[test]
    fn an_opaque_image_generation_is_not_the_image_view_presentation() {
        let panel = |kind: ToolKind, detail: ToolDetail, output: &str| {
            ToolPanel::new(ToolCall {
                identity: ToolIdentity {
                    name: "codex/image".into(),
                    call_id: "call_image".into(),
                },
                kind,
                status: ToolStatus::Completed,
                input: Some(json!({"path": "/w/shot.png"})),
                output: ToolOutput {
                    raw: Some(json!(output)),
                    blocks: vec![ContentBlock::Text(output.to_string())],
                    error: None,
                },
                detail,
                ..ToolCall::default()
            })
        };
        let view = panel(
            ToolKind::Read,
            ToolDetail::Read {
                path: Some("/w/shot.png".into()),
                preview: None,
                directory: false,
            },
            "ok",
        );
        assert_eq!(
            format_tool_input(&view),
            "🖼 `/w/shot.png`",
            "the neutral Read takes the image-view Input line"
        );

        let raw = "Read file /w/gen.png, lines 1-1\n1: data";
        let generation = panel(ToolKind::Other, ToolDetail::Opaque, raw);
        assert!(
            format_tool_input(&generation).starts_with("- path:"),
            "an Opaque generation renders the raw input: {}",
            format_tool_input(&generation)
        );
        let (_, _, body) = format_tool_output(&generation, raw);
        assert_eq!(
            body, raw,
            "an Opaque generation renders raw output, never a read preview"
        );
    }

    /// A web search's query leads the Input line, from the typed `Search`
    /// shape; the vendor name is not read.
    #[test]
    fn websearch_input_shows_the_query() {
        let search = search_tool(
            "websearch",
            SearchScope::Web,
            ToolStatus::Completed,
            Some(json!({"query": "rust lifetimes"})),
            None,
        );
        assert_eq!(format_tool_input(&search), "🔎 `rust lifetimes`");

        let codex = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "codex/web_search".into(),
                call_id: "call_search".into(),
            },
            kind: ToolKind::Search,
            detail: ToolDetail::Search {
                query: Some("rust lifetimes".into()),
                matches: None,
                scope: SearchScope::Web,
            },
            status: ToolStatus::Completed,
            input: Some(json!({"query": "rust lifetimes", "action": null})),
            ..ToolCall::default()
        });
        assert_eq!(format_tool_input(&codex), "🔎 `rust lifetimes`");

        // A payload without the adapter's shape stays opaque (ADR-0042's
        // whole-shape gate).
        let no_action = format_tool_input(&ToolPanel::for_test(
            "codex/web_search",
            ToolStatus::Completed,
            Some(json!({"query": "rust lifetimes"})),
            None,
        ));
        assert!(
            no_action.starts_with("- query:"),
            "a missing action takes the generic shape: {no_action}"
        );
    }

    /// V2's `read` output has no XML wrapper: a header line (`Read file
    /// <path>, lines N-M` / `Read directory <path>, entries N-M`) over numbered
    /// lines or a listing. The header line is promoted to the panel header and
    /// the body fences with the path's language (a directory listing without
    /// one).
    #[test]
    fn v2_read_output_promotes_the_header_line() {
        let file = "Read file /x/y.rs, lines 1-2\n1: use std::fs;\n2: fn main() {}";
        let (header, style, body) = read_output(file);
        assert_eq!(style, BodyStyle::Code(Some("rust")), "language from .rs");
        let header = header.unwrap_or_default();
        assert!(header.contains("/x/y.rs"), "path in header: {header}");
        assert!(!body.contains("Read file"), "header line promoted: {body}");
        assert!(body.contains("1: use std::fs;"), "code kept: {body}");

        let dir = "Read directory /x/src, entries 1-2\n/x/src/a.rs\n/x/src/b.rs";
        let (header, style, body) = read_output(dir);
        let header = header.unwrap_or_default();
        assert!(header.contains("/x/src"), "dir path: {header}");
        assert_eq!(style, BodyStyle::Code(None));
        assert!(body.contains("/x/src/a.rs"), "listing kept: {body}");

        let empty = "Read file /x/empty.txt, 0 lines";
        let (header, _, body) = read_output(empty);
        assert!(header.unwrap_or_default().contains("/x/empty.txt"));
        assert!(body.is_empty(), "empty file has no body: {body:?}");

        // A card-level check: the read header and fence reach the panel.
        let tool = read_tool(
            ToolStatus::Completed,
            Some(json!({"path": "/x/y.rs"})),
            Some(file),
        );
        let md = panel_md(tool);
        assert!(
            md.contains(&format!("{}\n\n📄 `/x/y.rs`", output_md())),
            "read header: {md}"
        );
        assert!(md.contains("```rust\n1: use std::fs;"), "fenced body: {md}");
    }

    /// V2's `websearch` output is markdown (`## [title](url)` + an optional
    /// `Published:` line + the excerpt), not V1's JSON/Exa envelopes. The panel
    /// renders the same title+url list and drops the excerpts.
    #[test]
    fn websearch_v2_markdown_renders_as_a_result_list() {
        let raw = "\
## [First result](https://a.example/one)
Published: 2026-01-02T00:00:00.000Z

excerpt one that must not appear

## [Second result](https://b.example/two)

excerpt two";
        let (header, style, body) = web_output(raw);
        assert_eq!(header.as_deref(), Some("🔎 2 条结果"));
        assert_eq!(style, BodyStyle::Markdown);
        assert!(
            body.contains("1. [First result](https://a.example/one) · 2026-01-02T00:00:00.000Z"),
            "dated first result: {body}"
        );
        assert!(
            body.contains("2. [Second result](https://b.example/two)"),
            "second: {body}"
        );
        assert!(!body.contains("excerpt"), "excerpts dropped: {body}");
        assert_eq!(web_output("No search results found.").0, None);
    }

    /// A Codex `webSearch` panel: the adapter writes the opaque `results` JSON
    /// to `metadata.results` (and the raw output), with no text blocks.
    fn codex_web_search_panel(results: serde_json::Value) -> ToolPanel {
        let raw = results.clone();
        ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "codex/web_search".into(),
                call_id: "call_web_search".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"query": "rust lifetimes", "action": null})),
            metadata: Some(json!({ "results": results })),
            output: ToolOutput {
                raw: (!raw.is_null()).then_some(raw),
                blocks: Vec::new(),
                error: None,
            },
            kind: ToolKind::Search,
            detail: ToolDetail::Search {
                query: Some("rust lifetimes".into()),
                matches: None,
                scope: SearchScope::Web,
            },
            ..ToolCall::default()
        })
    }

    /// #902: the adapter's `results` becomes the same result list V2's
    /// `websearch` renders — a known count plus one `[title](url)` per result —
    /// and the excerpt is dropped, so the panel never shows an empty Output.
    #[test]
    fn codex_web_search_renders_a_result_list_and_drops_excerpts() {
        let tool = codex_web_search_panel(json!([
            {
                "title": "First result",
                "url": "https://a.example/one",
                "snippet": "excerpt one that must not appear",
            },
            {
                "url": "https://b.example/two",
                "snippet": "excerpt two",
            },
        ]));
        let title = panel_title(tool.clone());
        assert_eq!(
            title,
            format!("✅ {}", crate::feishu::card::vocab::TOOL_WEBSEARCH),
            "own vocabulary, no invented detail"
        );
        let md = panel_md(tool);
        assert!(md.contains("🔎 `rust lifetimes`"), "query in input: {md}");
        assert!(
            md.contains(&format!("{}\n\n🔎 2 条结果", output_md())),
            "count: {md}"
        );
        assert!(
            md.contains("1. [First result](https://a.example/one)"),
            "titled first result: {md}"
        );
        assert!(
            md.contains("2. https://b.example/two"),
            "a result without a title keeps its url: {md}"
        );
        assert!(!md.contains("excerpt"), "excerpts dropped: {md}");
    }

    /// The `results` payload is opaque in the app-server schema, so anything
    /// outside the recognizable `[{url, title?}]` shape is shown as raw JSON —
    /// never guessed into a result list, and never dropped.
    #[test]
    fn codex_web_search_unrecognized_results_stay_raw() {
        let md = panel_md(codex_web_search_panel(json!([{"foo": 1}])));
        assert!(!md.contains("条结果"), "no invented count: {md}");
        assert!(
            md.contains(&format!("{}\n\n```json", output_md())),
            "fenced raw: {md}"
        );
        assert!(md.contains("\"foo\": 1"), "the payload is shown: {md}");
    }

    /// A search that produced no `results` has no Output section at all — the
    /// panel shows its query rather than an empty Output beside it.
    #[test]
    fn codex_web_search_without_results_has_no_output_section() {
        for empty in [json!(null), json!(""), json!("   ")] {
            let md = panel_md(codex_web_search_panel(empty.clone()));
            assert!(md.contains("🔎 `rust lifetimes`"), "query shown: {empty}");
            assert!(
                !md.contains(output_md().as_str()),
                "no empty Output for {empty}: {md}"
            );
        }
    }

    /// The results renderer is gated on the neutral `Search { scope: Web }`
    /// detail and the payload shape, not on the id: a call without that detail
    /// (however it spells its name) renders its own text, not a result list.
    #[test]
    fn a_foreign_codex_web_search_shape_is_not_rendered_as_a_result_list() {
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "codex/web_search".into(),
                call_id: "call_foreign".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"q": "rust lifetimes"})),
            metadata: Some(json!({"results": [{"title": "x", "url": "https://a.example"}]})),
            output: ToolOutput {
                raw: Some(json!([{"title": "x", "url": "https://a.example"}])),
                blocks: vec![ContentBlock::Text("opaque search output".into())],
                error: None,
            },
            ..ToolCall::default()
        });
        let md = panel_md(tool);
        assert!(md.contains("opaque search output"), "its own text: {md}");
        assert!(!md.contains("条结果"), "not a result list: {md}");
    }

    /// A `results` value that is a provider's own message is shown as that
    /// message inside the `json` fence, not re-quoted as a JSON string.
    #[test]
    fn codex_web_search_text_results_stay_a_fenced_message() {
        let md = panel_md(codex_web_search_panel(json!("No results found.")));
        assert!(
            md.contains(&format!("{}\n\n```json", output_md())),
            "fenced: {md}"
        );
        assert!(md.contains("No results found."), "the message: {md}");
        assert!(!md.contains("条结果"), "no invented count: {md}");
    }

    /// A `results` list with one entry missing a usable url is not a list the
    /// card can confirm: the whole value stays raw rather than dropping the
    /// bad entry and inventing a partial count.
    #[test]
    fn codex_web_search_rejects_one_entry_without_a_usable_url() {
        let md = panel_md(codex_web_search_panel(json!([
            {"title": "ok", "url": "https://a.example"},
            {"title": "bad", "url": "   "},
        ])));
        assert!(!md.contains("条结果"), "not a result list: {md}");
        assert!(md.contains("```json"), "the raw value: {md}");
    }

    /// The adapter writes `results` to both metadata and the raw output; when
    /// metadata carries none (a call from before the field existed), the raw
    /// value is still the same search results.
    #[test]
    fn codex_web_search_reads_the_raw_output_when_metadata_has_no_results() {
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "codex/web_search".into(),
                call_id: "call_raw_only".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"query": "rust lifetimes", "action": null})),
            metadata: Some(json!({"results": null})),
            output: ToolOutput {
                raw: Some(json!([{"title": "Raw", "url": "https://a.example"}])),
                blocks: Vec::new(),
                error: None,
            },
            kind: ToolKind::Search,
            detail: ToolDetail::Search {
                query: Some("rust lifetimes".into()),
                matches: None,
                scope: SearchScope::Web,
            },
            ..ToolCall::default()
        });
        let md = panel_md(tool);
        assert!(
            md.contains("1. [Raw](https://a.example)"),
            "the raw results render: {md}"
        );
    }

    /// Metadata is the adapter's structured field; the raw output is only the
    /// fallback. When the two disagree, the panel shows metadata.
    #[test]
    fn codex_web_search_prefers_metadata_results_over_the_raw_output() {
        let tool = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "codex/web_search".into(),
                call_id: "call_disagree".into(),
            },
            status: ToolStatus::Completed,
            started_at: None,
            input: Some(json!({"query": "rust lifetimes", "action": null})),
            metadata: Some(json!({
                "results": [{"title": "Meta", "url": "https://meta.example"}]
            })),
            output: ToolOutput {
                raw: Some(json!([{"title": "Raw", "url": "https://raw.example"}])),
                blocks: Vec::new(),
                error: None,
            },
            kind: ToolKind::Search,
            detail: ToolDetail::Search {
                query: Some("rust lifetimes".into()),
                matches: None,
                scope: SearchScope::Web,
            },
            ..ToolCall::default()
        });
        let md = panel_md(tool);
        assert!(md.contains("[Meta](https://meta.example)"), "metadata wins: {md}");
        assert!(!md.contains("raw.example"), "the raw value is not used: {md}");
    }

    /// An empty list while the search is still running is not "0 results": the
    /// panel waits. Once settled, zero is a real answer and says so.
    #[test]
    fn codex_web_search_shows_no_empty_list_while_running() {
        let mut live = codex_web_search_panel(json!([]));
        live.call.status = ToolStatus::Running;
        assert!(
            !panel_md(live).contains(output_md().as_str()),
            "a running search has no verdict yet"
        );
        let settled = codex_web_search_panel(json!([]));
        let md = panel_md(settled);
        assert!(
            md.contains("🔎 0 条结果"),
            "a settled empty search says zero: {md}"
        );
    }

    /// The rendered count header and result body reach the card-size estimate,
    /// and the raw fallback also charges its `json` fence tag — a split check
    /// cannot charge zero, or miss the tag, for a panel it shows.
    #[test]
    fn the_codex_web_search_body_is_charged_to_the_estimate() {
        let tool = codex_web_search_panel(json!([
            {"title": "First result", "url": "https://a.example/one"},
        ]));
        let body = "1. [First result](https://a.example/one)";
        let header = "🔎 1 条结果";
        assert_eq!(
            panel_output_estimate(&tool),
            header.len() + body.len(),
            "the count header and the markdown body are both charged"
        );

        // The sanitizer expands `<` outside code, so the estimate must charge
        // what the element carries, not the raw markdown length.
        let escaped = codex_web_search_panel(json!([
            {"title": "a<b", "url": "https://a.example/<x>"},
        ]));
        let escaped_body = "1. [a<b](https://a.example/<x>)";
        assert_eq!(
            panel_output_estimate(&escaped),
            header.len() + serialized_len(escaped_body),
            "the sanitizer's `<` expansion is charged"
        );
        assert!(
            serialized_len(escaped_body) > escaped_body.len(),
            "the expansion is real"
        );

        let raw = codex_web_search_panel(json!([{"foo": 1}]));
        let raw_body = "[\n  {\n    \"foo\": 1\n  }\n]";
        assert_eq!(
            panel_output_estimate(&raw),
            json_escaped_len(raw_body) + fence_lang_bytes(Some("json")),
            "the raw fallback also charges its json fence tag"
        );
    }

    /// Opaque results beyond the panel budget clip BARE inside the `json`
    /// fence and say so: a `…` immediately before a closing fence stops Feishu
    /// rendering the block as code at all (#788), so the body must not get one.
    #[test]
    fn codex_web_search_long_raw_json_keeps_its_fence_and_says_it_clipped() {
        let tool = codex_web_search_panel(json!({"blob": "x".repeat(4_000)}));
        assert_eq!(
            truncation_note(&tool, None),
            Some(SEARCH_TRUNCATION_NOTE),
            "the clip is announced"
        );
        let md = panel_md(tool);
        assert!(md.contains("```json"), "fenced: {md}");
        assert!(!md.contains('…'), "the body clips bare: {md}");
        assert!(
            md.contains(&format!("```\n\n{SEARCH_TRUNCATION_NOTE}")),
            "the fence closes bare, then the note: {md}"
        );
    }

    /// A result list longer than the panel budget is markdown, so the generic
    /// `truncate_md` marker is safe — it must never be fenced (which would
    /// turn the list into literal syntax) and must not lose its count header.
    #[test]
    fn a_long_result_list_truncates_as_markdown() {
        let entries = (0..80)
            .map(|n| json!({"title": format!("result {n}"), "url": format!("https://a.example/{n}")}))
            .collect::<Vec<_>>();
        let md = panel_md(codex_web_search_panel(json!(entries)));
        assert!(md.contains("🔎 80 条结果"), "count kept: {md}");
        assert!(!md.contains("```"), "the list is not fenced: {md}");
        assert!(md.contains('…'), "the list is truncated: {md}");
    }

    /// The `Command` shape's recorded outcome decides the failure verdict on a
    /// settled call: a non-zero exit or a timeout renders the failure icon and
    /// keeps the error body, even though the protocol's status is `completed`
    /// (the same derivation the official 2.0.x client uses). The typed liveness
    /// stays status-based, and the background marker stays ADR-0060's.
    #[test]
    fn a_settled_command_reports_its_recorded_exit_and_timeout() {
        let command = |exit: Option<i64>, timeout: bool, output: &str| {
            command_panel(
                "shell",
                ToolStatus::Completed,
                "make",
                Some("/work"),
                exit,
                timeout,
                Some(output),
            )
        };
        assert_eq!(
            panel_title(command(Some(1), false, "boom")),
            format!("❌ {}", vocab::TOOL_SHELL),
            "a non-zero exit is a failure whatever the protocol status says"
        );
        let failed = panel_md(command(Some(1), false, "boom"));
        assert!(failed.contains("boom"), "the error body survives: {failed}");
        assert_eq!(
            panel_title(command(Some(0), false, "")),
            format!("✅ {}", vocab::TOOL_SHELL)
        );
        assert_eq!(
            panel_title(command(None, true, "timed out")),
            format!("❌ {}", vocab::TOOL_SHELL),
            "a timeout is a failure"
        );
        let timed_out = panel_md(command(None, true, "timed out"));
        assert!(
            timed_out.contains("timed out"),
            "the timeout body survives: {timed_out}"
        );
        // The backgrounded launch keeps its 🌙 marker (ADR-0060): the run is the
        // ledger's, and the call settled by handing back its handle.
        let mut backgrounded = command(Some(0), false, "");
        backgrounded.call.metadata = Some(json!({"status": "running", "shellID": "sh_1"}));
        assert_eq!(panel_title(backgrounded.clone()), "🌙 shell");
        // The icon is presentation only: the call is settled either way.
        assert!(!command(Some(1), false, "boom").is_live());
        assert!(!backgrounded.is_live());
    }

    /// The `Command` panel's Output body IS the shared command output block the
    /// Host Terminal Card renders its log through (ADR-0086's "one render block,
    /// never a lifecycle"): a long line fences, and the body is exactly the
    /// block's — no second clip or fence is applied on the way onto the card.
    #[test]
    fn a_command_panel_renders_the_shared_output_block() {
        let raw = format!("short\n{}", "x".repeat(140));
        let tool = command_panel(
            "shell",
            ToolStatus::Completed,
            "make",
            None,
            Some(0),
            false,
            Some(&raw),
        );
        let (block, cut) = command_output_block(
            &raw,
            OutputClip::Chars {
                max_chars: TOOL_OUTPUT_MAX_CHARS,
            },
            false,
        );
        assert!(!cut, "a body inside the budget is not cut");
        assert!(block.starts_with("```\n"), "a long line fences: {block}");
        let md = panel_md(tool);
        assert!(
            md.contains(&format!("{}\n\n{block}", output_md())),
            "the panel writes the block verbatim: {md}"
        );
    }

    /// One `Command` shape, three producers: `bash`, `shell` and
    /// `codex/command_execution` render the same command, cwd, status icon,
    /// failure verdict and output body for equivalent input. Only the folded
    /// title's display name differs — it is a word, not a renderer key, and the
    /// token rename is the vocabulary ticket's.
    #[test]
    fn every_command_producer_renders_the_same_panel() {
        let panels: Vec<ToolPanel> = ["bash", "shell", "codex/command_execution"]
            .iter()
            .map(|name| {
                command_panel(
                    name,
                    ToolStatus::Completed,
                    "make test",
                    Some("/work"),
                    Some(1),
                    false,
                    Some("error: 3 assertions failed"),
                )
            })
            .collect();
        let (first, rest) = panels.split_first().expect("three producers");
        for panel in rest {
            assert_eq!(panel.status_icon(), first.status_icon(), "one failure verdict");
            assert_eq!(panel_md(panel.clone()), panel_md(first.clone()), "one body");
            assert_eq!(
                format_tool_input(panel),
                format_tool_input(first),
                "one Input line"
            );
        }
        assert_eq!(first.status_icon(), "❌");
        let body = panel_md(first.clone());
        assert!(body.contains("error: 3 assertions failed"), "{body}");
    }

    /// V2 records the result count and truncation in the call's metadata; the
    /// panel's output header carries them. A call without the metadata (a
    /// third-party tool claiming the id) keeps the raw shape.
    #[test]
    fn glob_and_grep_headers_carry_the_result_counts() {
        let glob = search_tool_with_metadata(
            "glob",
            SearchScope::FileName,
            ToolStatus::Completed,
            json!({"pattern": "**/*.rs"}),
            json!({"count": 3, "truncated": false}),
            Some("a.rs\nb.rs\nc.rs"),
        );
        let md = panel_md(glob);
        assert!(
            md.contains(&format!("{}\n\n3 个文件", output_md())),
            "glob count: {md}"
        );

        let truncated = search_tool_with_metadata(
            "glob",
            SearchScope::FileName,
            ToolStatus::Completed,
            json!({"pattern": "**/*.rs"}),
            json!({"count": 100, "truncated": true}),
            Some("a.rs"),
        );
        let md = panel_md(truncated);
        assert!(md.contains("100 个文件 · 已截断"), "truncation noted: {md}");

        let grep = search_tool_with_metadata(
            "grep",
            SearchScope::Content,
            ToolStatus::Completed,
            json!({"pattern": "fn main"}),
            json!({"matches": 2, "truncated": false}),
            Some("Found 2 matches\nsrc/a.rs:\n  Line 1: fn main() {}"),
        );
        let md = panel_md(grep);
        assert!(
            md.contains(&format!("{}\n\n2 处匹配", output_md())),
            "grep count: {md}"
        );

        assert_eq!(format_named_output("glob", "a.rs\nb.rs").0, None);
    }

    /// A Code Mode `Program` (ADR-0105, spec #1057's P2): the source renders as
    /// a fenced JavaScript block and the nested calls the shape records render as
    /// one status row each. A completed program whose shape reports an error —
    /// its own, or a nested call's — shows the failure icon, and the folded title
    /// carries the call count. Selection is by `(kind, detail)`: the `execute`
    /// name alone renders nothing special.
    #[test]
    fn program_panel_renders_the_script_and_its_nested_calls() {
        let tool = program_panel(
            "execute",
            ToolStatus::Completed,
            "const r = await tools.webfetch({ url: \"https://a.example\" });\nreturn r.output;",
            vec![
                nested("webfetch", ToolStatus::Completed),
                nested("opencode_models", ToolStatus::Error),
            ],
            false,
            Some("boom"),
        );
        assert_eq!(
            panel_title(tool.clone()),
            format!("❌ {} · 2 个工具调用 · 1 失败", vocab::TOOL_SCRIPT)
        );
        let md = panel_md(tool);
        assert!(
            md.contains(&format!("{}\n```javascript", input_md())),
            "script fenced: {md}"
        );
        assert!(md.contains("tools.webfetch"), "script kept: {md}");
        assert!(md.contains("- ✅ `webfetch`"), "nested row: {md}");
        assert!(md.contains("- ❌ `opencode_models`"), "failed nested row: {md}");
        assert!(md.contains("boom"), "result kept: {md}");

        // A successful program: ✅ and no failure count.
        let ok = program_panel(
            "execute",
            ToolStatus::Completed,
            "return 1",
            vec![nested("fetch", ToolStatus::Completed)],
            false,
            Some("1"),
        );
        assert_eq!(
            panel_title(ok),
            format!("✅ {} · 1 个工具调用", vocab::TOOL_SCRIPT)
        );

        // The program's OWN error is a fact of its own: a clean nested list does
        // not make a failed program a success, and the header names the failure
        // the ❌ icon stands on instead of showing a clean count beside it.
        let own_error = program_panel(
            "execute",
            ToolStatus::Completed,
            "throw new Error('nope')",
            vec![nested("fetch", ToolStatus::Completed)],
            true,
            Some("nope"),
        );
        assert_eq!(
            panel_title(own_error.clone()),
            format!("❌ {} · 1 个工具调用 · 程序出错", vocab::TOOL_SCRIPT),
            "the program's own error cannot be inferred from the nested rows"
        );
        assert!(
            panel_md(own_error).contains("- ✅ `fetch`"),
            "the clean nested row stays"
        );

        // A program whose own error sits beside a failed nested call names both,
        // in the card's `·`-segment style.
        let both = program_panel(
            "execute",
            ToolStatus::Completed,
            "throw new Error('nope')",
            vec![nested("fetch", ToolStatus::Error)],
            true,
            Some("nope"),
        );
        assert_eq!(
            panel_title(both),
            format!("❌ {} · 1 个工具调用 · 1 失败 · 程序出错", vocab::TOOL_SCRIPT)
        );

        // A live program with no output yet still lists its calls.
        let live = program_panel(
            "execute",
            ToolStatus::Running,
            "await tools.webfetch({ url: \"https://a.example\" })",
            vec![nested("fetch", ToolStatus::Running)],
            false,
            None,
        );
        assert!(panel_title(live.clone()).starts_with(&format!("⏳ {} · 1 个工具调用", vocab::TOOL_SCRIPT)));
        let md = panel_md(live);
        assert!(md.contains("- ⏳ `fetch`"), "live nested row: {md}");

        // A same-named foreign tool WITHOUT the shape stays on the opaque path
        // (ADR-0042): no fenced JavaScript, no call-count title.
        let foreign = panel_with(
            "execute",
            ToolStatus::Completed,
            json!({"script": "console.log(1)"}),
            json!({}),
            Some("done"),
        );
        assert!(
            !panel_md(foreign.clone()).contains("```javascript"),
            "no Code Mode input rendering for a foreign shape"
        );
        assert!(!panel_title(foreign).contains("工具调用"));
    }

    /// A declared language names the source fence; Code Mode's wire declares
    /// none, so the card keeps its historical `javascript` default.
    #[test]
    fn a_program_fences_its_source_with_its_declared_language() {
        let mut declared = program_panel(
            "execute",
            ToolStatus::Completed,
            "return 1",
            Vec::new(),
            false,
            None,
        );
        if let ToolDetail::Program { language, .. } = &mut declared.call.detail {
            *language = Some("lua".to_string());
        }
        assert!(
            format_tool_input(&declared).starts_with("```lua"),
            "the declared language names the fence"
        );
    }

    /// The size estimate charges a `Program`'s assembled body. The nested-call
    /// rows ARE the body — a program that wrote no output text still renders
    /// them — so an estimate that only read the text output charged zero for the
    /// panel it writes.
    #[test]
    fn a_program_body_is_charged_to_the_estimate() {
        let tool = program_panel(
            "execute",
            ToolStatus::Completed,
            "return 1",
            vec![
                nested("fetch", ToolStatus::Completed),
                nested("read", ToolStatus::Completed),
            ],
            false,
            // No output text at all: the rows are the whole body.
            None,
        );
        let md = panel_md(tool.clone());
        assert!(md.contains("- ✅ `fetch`"), "the rows are the body: {md}");
        assert!(md.contains("- ✅ `read`"), "both rows: {md}");
        let estimate = panel_output_estimate(&tool);
        assert!(estimate > 0, "the rows are charged, not zero: {estimate}");
        assert!(
            estimate >= serialized_len("- ✅ `fetch`\n- ✅ `read`"),
            "it covers the rows it writes: {estimate}"
        );

        // A program with neither calls nor output renders no Output section, so
        // it charges nothing — the same walk the render takes.
        let empty = program_panel(
            "execute",
            ToolStatus::Completed,
            "return 1",
            Vec::new(),
            false,
            None,
        );
        assert_eq!(panel_output_estimate(&empty), 0);
    }

    /// The input limit hint carries the search's own unit: a filename search
    /// counts files, a content search and a read count lines.
    #[test]
    fn input_limit_hints_match_the_tool() {
        for (name, input, unit) in [
            ("glob", json!({"pattern": "**/*.rs", "limit": 5}), "个文件"),
            ("read", json!({"path": "a.rs", "limit": 5}), "行"),
            ("grep", json!({"pattern": "x", "limit": 5}), "行"),
        ] {
            let tool = match name {
                "read" => read_tool(ToolStatus::Completed, Some(input), None),
                "glob" => search_tool(
                    "glob",
                    SearchScope::FileName,
                    ToolStatus::Completed,
                    Some(input),
                    None,
                ),
                _ => search_tool(
                    "grep",
                    SearchScope::Content,
                    ToolStatus::Completed,
                    Some(input),
                    None,
                ),
            };
            let line = format_tool_input(&tool);
            assert!(line.contains(&format!("最多 5 {unit}")), "{name}: {line}");
        }
    }
}
