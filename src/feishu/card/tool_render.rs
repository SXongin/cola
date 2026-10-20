use crate::backend::{ContentBlock, FileContent, ToolCall, ToolStatus};

use super::lang::{PLAIN_TEXT, language_for_path};
use super::sanitize::CardMarkdown;
use super::shell::{collapsible_panel, fmt_elapsed, panel_time_suffix};
use super::{
    AwaitingAction, clip_md, exceeds_md_budget, fenced_block_bytes, fenced_block_overhead, fenced_code,
    fenced_overhead, truncate_md,
};

/// Cap for a tool's OUTPUT shown inside its collapsible panel. Higher than the
/// old 800: code-wrapped output renders without line wrapping, so a file or
/// command output can be meaningfully long.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 3000;

/// V1's `read` output marks a DIRECTORY listing with this tag line in its XML
/// envelope, where a file's carries `<type>file</type>` — the shape that says
/// "this read has no file to open" (ADR-0083 §8 amendment, #770).
const V1_DIRECTORY_TYPE_TAG: &str = "<type>directory</type>";

/// V2's `read` output opens a DIRECTORY listing's header line with this prefix,
/// where a file's opens `Read file `; the same verdict in V2's wire shape.
const V2_DIRECTORY_HEADER_PREFIX: &str = "Read directory ";

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

    /// The tool's built-in id (`bash`, `read`, …) — the rendering key
    /// (ADR-0042).
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
        if self.metadata_failure() {
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
    /// (ADR-0083 §8, #743): a `write`/`edit` call's own `filePath` (V1) —
    /// `path` on V2 — read through the one [`input_path`] rule every file
    /// formatter shares, so the button and the Input line can never disagree
    /// about which file the call touched. A FILE `read` names one too
    /// (ADR-0083 §8 amendment, #770): its panel is a preview of the file
    /// clipped to [`TOOL_OUTPUT_MAX_CHARS`], so the whole file is a tap away
    /// rather than on the card. Two read shapes are excluded, both of which
    /// already reach the reader whole: a read carrying a File Content (an
    /// image or a PDF has its own delivery, ADR-0076) and a DIRECTORY read
    /// ([`read_output_is_directory`]), which names no file at all. `None` for
    /// every other tool, and for a call whose path is absent or unusable (no
    /// input, a non-object input, a missing or non-string key, or a blank
    /// string) — or whose output has not arrived yet, a still-running `read`
    /// having no directory verdict to decide on. Those panels carry no button
    /// rather than a broken one. A path that merely carries surrounding
    /// whitespace is still a name Unix allows, so it stays verbatim — only a
    /// path with nothing printable is unusable. Natural-language paths in the
    /// agent's prose never reach here — only the call's structured input is
    /// read.
    pub(crate) fn file_open_path(&self) -> Option<&str> {
        let obj = self.input()?.as_object()?;
        let path = input_path(obj).filter(|path| !path.trim().is_empty());
        match self.name() {
            "write" | "edit" => path,
            "read" => {
                // A panel of a file the viewer can show WHOLE: the body is a
                // preview, so the way in is what the panel lacks. The two
                // exclusions already reach the reader whole (a File Content's
                // own delivery; a directory, which is no file at all), and a
                // call with no output yet has no verdict — `?` leaves a
                // still-running read buttonless until it renders one.
                if !self.files.is_empty() || read_output_is_directory(&self.output()?) {
                    return None;
                }
                path
            }
            _ => None,
        }
    }

    /// How many card elements the 「📂 打开」 button adds (ADR-0083 §8, #743):
    /// one button inside a qualifying panel, none otherwise — so the component
    /// estimate charges precisely what renders. Decided by the button builder
    /// itself, never re-derived, so the count cannot drift from the render.
    pub(crate) fn open_button_count(&self, session_id: Option<&str>) -> usize {
        usize::from(tool_open_button_element(self, session_id).is_some())
    }

    /// The call's raw metadata (what OpenCode recorded beside the input): V2
    /// keeps a shell's exit code, a search's result count and a Code Mode
    /// program's nested call list here, so the panel reads run facts the text
    /// output does not carry.
    fn metadata(&self) -> Option<&serde_json::Value> {
        self.call.metadata.as_ref()
    }

    /// Whether the call is V2's Code Mode entry by shape: the id `execute`
    /// carrying its `{code}` input. ADR-0042's gate — a same-named foreign tool
    /// with a different input shape stays on the opaque path.
    fn is_code_mode(&self) -> bool {
        self.name() == "execute"
            && self
                .call
                .input
                .as_ref()
                .and_then(|input| input.get("code"))
                .is_some_and(serde_json::Value::is_string)
    }

    /// Whether the call's own recorded metadata reports a failed run even
    /// though the protocol's status settled `completed`: V2 records a shell's
    /// non-zero exit / timeout there, and Code Mode reports its program errors
    /// (its own or a nested call's) in `toolCalls`. The official 2.0.x client
    /// derives its error rendering from exactly these fields; the typed
    /// [`ToolStatus`] stays the protocol's own and is never rewritten.
    fn metadata_failure(&self) -> bool {
        if self.status() != &ToolStatus::Completed {
            return false;
        }
        let Some(metadata) = self.metadata() else {
            return false;
        };
        match self.name() {
            "shell" => {
                metadata.get("timeout").and_then(serde_json::Value::as_bool) == Some(true)
                    || metadata
                        .get("exit")
                        .and_then(serde_json::Value::as_f64)
                        .is_some_and(|exit| exit != 0.0)
            }
            "execute" if self.is_code_mode() => {
                metadata.get("error").and_then(serde_json::Value::as_bool) == Some(true)
                    || metadata
                        .get("toolCalls")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|calls| {
                            calls.iter().any(|call| {
                                call.get("status").and_then(serde_json::Value::as_str) == Some("error")
                            })
                        })
            }
            _ => false,
        }
    }

    /// The output text the panel renders: the decoder's text blocks joined (in
    /// decoder order) with a failure's message appended on its own line, or —
    /// for a file-editing tool — the real diff recorded in the call's raw
    /// metadata.
    ///
    /// This is presentation assembly, not protocol decoding: the decoder
    /// already applied the sources' precedence into the typed output's blocks,
    /// so a payload carrying more than one text source can never render twice.
    pub fn output(&self) -> Option<String> {
        if is_file_editing(&self.call.identity.name) {
            edit_tool_output(&self.call)
        } else {
            tool_output(&self.call)
        }
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

    /// The child Session a `task`/`subagent` call runs — V1 records
    /// `state.metadata.sessionId`, V2 `state.metadata.sessionID` (the camelCase
    /// field each generation's contract carries; AGENTS.md pitfall `wire-shapes`). `None` for any
    /// other tool, or when the payload recorded no session id.
    pub(crate) fn child_session_id(&self) -> Option<&str> {
        if !is_task_tool(&self.call.identity.name) {
            return None;
        }
        let metadata = self.call.metadata.as_ref()?;
        ["sessionId", "sessionID"]
            .iter()
            .find_map(|key| metadata.get(*key).and_then(serde_json::Value::as_str))
    }
}

/// The built-in tools whose calls spawn a child session — V1's `task` and V2's
/// `subagent`: the one call kind whose panel carries liveness, checked by name
/// in the card and the Bridge.
pub(crate) const TASK_TOOLS: [&str; 2] = ["task", "subagent"];

/// Whether `name` is a child-session-spawning built-in ([`TASK_TOOLS`]).
pub(crate) fn is_task_tool(name: &str) -> bool {
    TASK_TOOLS.contains(&name)
}

/// A live `task` call's child-session liveness (ADR-0054): what the child is
/// doing right now, as the panel title shows it. Display-only data — the Bridge
/// gathers it, the Platform formats it — so the panel stays a view over the
/// call plus one read-only line.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLiveness {
    /// The child's current activity: its newest live tool, or the phase its
    /// newest part implies.
    pub activity: ChildActivity,
    /// Epoch ms of the child's newest observed activity: the timer a phase
    /// (no tool running) counts from. Stored as a time, not an age, so a
    /// failed read never freezes a made-up "5s".
    pub last_activity_ms: i64,
    /// The child's pending wait, in the header's own vocabulary (ADR-0054).
    pub wait: Option<AwaitingAction>,
}

/// What a child session is doing right now, as its transcript reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildActivity {
    /// The child's newest live tool. `started_at` is the server clock the
    /// duration counts from; a call with no clock yet shows its name alone.
    Tool { name: String, started_at: Option<i64> },
    /// Between steps: nothing live, waiting on the model.
    Thinking,
    /// Its newest part is reasoning.
    Reasoning,
    /// Its newest part is streamed reply text.
    Replying,
}

impl TaskLiveness {
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
                ChildActivity::Thinking => "思考中",
                ChildActivity::Reasoning => "推理中",
                ChildActivity::Replying => "回复中",
            },
            age_clock_ms: match &self.activity {
                ChildActivity::Tool { started_at, .. } => *started_at,
                ChildActivity::Thinking | ChildActivity::Reasoning | ChildActivity::Replying => {
                    Some(self.last_activity_ms)
                }
            },
            wait: self.wait.and_then(|wait| wait.label()),
        }
    }

    /// The title fragment a live task panel appends, in the header's own
    /// shape — the activity first, then how long it has run: `bash 28s`,
    /// `推理中 12s`, `bash · 等待你的授权`. `now_ms` is passed in so the
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

/// For a file-editing tool (`edit`, `apply_patch`), prefer the REAL diff
/// recorded in the call's raw metadata over the tool's plain text output
/// ("Edit applied successfully." / "Success. Updated the following files: …" /
/// V2's "Edited <file> (N replacement)"), which tells the reader nothing about
/// what changed. Failures keep their extracted error text.
fn edit_tool_output(call: &ToolCall) -> Option<String> {
    let text = tool_output(call);
    if call.status == ToolStatus::Error {
        return text;
    }
    let diff = recorded_diff(call)?;
    // Everything up to the text output's first blank line is the tool's
    // success summary — noise once the hunks are shown, whatever generation
    // generated it. A note the tool reported after it (V1's "LSP errors
    // detected …") keeps its place as a tail after the diff.
    let tail = text.as_deref().and_then(summary_tail);
    Some(match tail {
        Some(t) => format!("{diff}\n\n{t}"),
        None => diff,
    })
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

/// The unified diff a file-editing call recorded, whichever generation wrote
/// it: see [`diff_from_metadata`]. `None` when the call carries no metadata.
pub(crate) fn recorded_diff(call: &ToolCall) -> Option<String> {
    diff_from_metadata(call.metadata.as_ref())
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
/// most fixtures need). Production code always wraps a decoded [`ToolCall`];
/// fixtures that exercise metadata build the call directly.
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
/// line, but the body starts collapsed, so a `write`/`edit` panel used to say
/// nothing about the file it touched until the reader unfolded it — and a text
/// `read` likewise.
///
/// A MULTI-file patch is the exception that names no path (#793): its folded
/// header carries the deduped file count and the change totals instead
/// (`📄 3 files · +42 −7`). One path could not describe several files, the
/// `Index:` lines that separate them are inside the fold, and the count is what
/// tells the reader whether expanding is worth it. The [`EditDiff`] comes from
/// the same parse rule the body's header used ([`file_edit_diff`]), so the
/// count and the totals cannot disagree with the hunks below.
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
    if is_file_editing(tool.name())
        && let Some(diff) = tool
            .output()
            .as_deref()
            .and_then(|output| file_edit_diff(tool.name(), output))
        && diff.paths.len() > 1
    {
        return Some(format!(
            " · 📄 {} files · +{} −{}",
            diff.paths.len(),
            diff.additions,
            diff.deletions
        ));
    }
    let (icon, path) = match tool.name() {
        "write" | "edit" => ("📄", tool.file_open_path()?),
        // The book icon the read panel's Input line already uses — and the one
        // that still reads correctly for a directory read. Only a read WITHOUT
        // a File Content gets the fragment: one with one is already named by
        // the `📎` marker or the image caption.
        "read" => {
            if !tool.files().is_empty() {
                return None;
            }
            let obj = tool.input()?.as_object()?;
            ("📖", input_path(obj).filter(|path| !path.trim().is_empty())?)
        }
        _ => return None,
    };
    // A directory read may name its path with a trailing separator. Dropping it
    // lets the short-name rule reach the directory's own name instead of
    // rendering the whole path; a path that is nothing BUT separators keeps its
    // form (`header_file_name` then falls back to it).
    let trimmed = path.trim_end_matches(['/', '\\']);
    let name = if trimmed.is_empty() { path } else { trimmed };
    Some(format!(" · {icon} {}", header_file_name(name)))
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
fn json_escaped_len(text: &str) -> usize {
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
    let (label, value) = if super::viewer::is_html_page_path(path) {
        ("🌐 预览网页", super::session::tool_page_value(session_id, path))
    } else {
        ("📂 打开", super::session::tool_file_value(session_id, path))
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

/// Whether a panel's body is a file-editing DIFF ([`BodyStyle::Code`] from an
/// `edit` / `apply_patch` / `patch` panel): the one body shape under the diff
/// byte budget ([`DIFF_MAX_ESCAPED_BYTES`]). Every other body — a `read` or
/// `write` preview, an `Auto` block — stays on [`TOOL_OUTPUT_MAX_CHARS`].
fn is_diff_body(name: &str, style: &BodyStyle) -> bool {
    is_file_editing(name) && matches!(style, BodyStyle::Code(_))
}

/// The language token a [`BodyStyle::Code`] body renders its opening fence
/// with, `None` for every other style. The token is part of the fenced block,
/// so the diff budget, its note and its estimate all read it here.
fn code_lang(style: &BodyStyle) -> Option<&'static str> {
    match style {
        BodyStyle::Code(lang) => *lang,
        BodyStyle::Auto | BodyStyle::Markdown => None,
    }
}

/// The diff body a file-editing panel renders, UNclipped: the parsed hunks of
/// an `edit` / `apply_patch` / `patch` call. `None` for every other panel and
/// for an editing call whose output is not a recognized diff (a FAILED call's
/// error text falls back to [`BodyStyle::Auto`]), which keeps the preview path
/// untouched. The ONE derivation the render, [`diff_truncation_note`] and
/// [`panel_output_estimate`] share, so the clip, its note and its charge
/// cannot drift.
fn diff_output(tool: &ToolPanel) -> Option<(Option<String>, BodyStyle, String)> {
    if !is_file_editing(tool.name()) {
        return None;
    }
    let (header, style, body) = panel_output(tool)?;
    is_diff_body(tool.name(), &style).then_some((header, style, body))
}

/// The note a file-editing panel appends under a diff its budget CLIPPED
/// ([`DIFF_TRUNCATION_NOTE`]): the clip's only signal, because the fenced body
/// clips bare (#788). Renders IFF the body the panel shows is a recognized diff
/// ([`diff_output`]) whose escaped body, fence and language tag exceed
/// [`DIFF_MAX_ESCAPED_BYTES`] ([`fenced_body_fits`]) — a FAILED call's error
/// text is not a diff and takes no note, since there is no change to name.
/// Unlike [`preview_truncation_note`] it needs no button: "已截断" is true even
/// when nothing can be opened, so the reader still knows the diff is partial.
fn diff_truncation_note(tool: &ToolPanel) -> Option<&'static str> {
    diff_output(tool)
        .filter(|(_, style, body)| !fenced_body_fits(body, code_lang(style), DIFF_MAX_ESCAPED_BYTES))
        .map(|_| DIFF_TRUNCATION_NOTE)
}

/// The note a panel appends under a body its budget clipped: a `read`/`write`
/// preview's way-in note, or a file-editing diff's bare clip signal. One
/// function, because the render and the size estimate must agree on exactly one
/// note (a panel never carries both: a tool is either an editing tool or it
/// isn't).
fn truncation_note(tool: &ToolPanel, session_id: Option<&str>) -> Option<&'static str> {
    preview_truncation_note(tool, session_id).or_else(|| diff_truncation_note(tool))
}

/// Whether a `read`/`write` panel's rendered body is clipped by its own budget
/// — the clamp [`tool_panel_element`] applies ([`truncate_md`] at
/// [`TOOL_OUTPUT_MAX_CHARS`]) — measured from the same body the render shows,
/// the way every size estimate here re-derives its render, so the note
/// [`preview_truncation_note`] renders is the one charged. Only the panel's own
/// file PREVIEW counts: a FAILED `write` renders its error text instead
/// ([`write_body`] is `None`), which the note must never promise the way into.
/// A file-editing diff is [`diff_truncation_note`]'s, not this one's.
fn preview_body_clipped(tool: &ToolPanel) -> bool {
    let body = if tool.name() == "write" {
        write_body(tool).map(|(_, _, body)| body)
    } else if tool.name() == "read" {
        tool.output().map(|raw| format_tool_output("read", &raw).2)
    } else {
        None
    };
    body.is_some_and(|body| exceeds_md_budget(&body, TOOL_OUTPUT_MAX_CHARS))
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

/// The Output triple a panel renders: `execute`'s nested-call body, a `write`
/// call's own `content` ([`write_body`]), or the tool's output text through the
/// per-tool formatter. One function, because the render and the size estimate
/// must agree on which body a panel shows.
fn panel_output(tool: &ToolPanel) -> Option<(Option<String>, BodyStyle, String)> {
    if tool.is_code_mode() {
        execute_output(tool)
    } else if let Some(write) = write_body(tool) {
        Some(write)
    } else {
        tool.output().map(|raw| format_tool_output(tool.name(), &raw))
    }
}

/// The body a `write` panel renders in place of its success sentence (#788):
/// the call's own `content` input — the file it produced, which is what a reader
/// wants and what the panel's 「📂 打开」 already opens the way to.
///
/// `None` — the panel then renders its output text as before — for a call that
/// FAILED ([`call_failed`]: its error text is the body) and for one whose input
/// carries no usable `content` string. The header is the same `📄 <path>` shape
/// the `read` panel's body carries, and the fence takes the touched file's
/// language ([`language_for_path`]), so a written `.md` previews its SOURCE like
/// the viewer's default.
fn write_body(tool: &ToolPanel) -> Option<(Option<String>, BodyStyle, String)> {
    if tool.name() != "write" || call_failed(tool) {
        return None;
    }
    let obj = tool.input()?.as_object()?;
    let content = obj.get("content")?.as_str()?;
    let path = input_path(obj).filter(|path| !path.trim().is_empty());
    let header = path.map(|path| format!("📄 `{path}`"));
    let lang = path.map(language_for_path).unwrap_or(PLAIN_TEXT);
    Some((header, BodyStyle::Code(Some(lang)), content.to_string()))
}

/// Whether the call settled as a FAILURE, by the same rule its status icon
/// draws ([`ToolPanel::status_icon`]): [`ToolStatus::is_failure`], or runtime
/// metadata that reports one. A failed `write` keeps its error text rather than
/// the content it never wrote (#788).
fn call_failed(tool: &ToolPanel) -> bool {
    tool.metadata_failure() || tool.status().is_failure()
}

/// The character cap on a `write` panel's trailing note (#788). The note is a
/// preview like the content, and one panel carries BOTH: a content of
/// [`TOOL_OUTPUT_MAX_CHARS`] characters can serialize to six bytes a character
/// (JSON's widest escape), so the note's own cap must leave that worst case plus
/// the panel's fixed overhead under the card budget — a single panel cannot be
/// split, and an oversized one would be sent whole and rejected by Feishu.
const WRITE_TAIL_MAX_CHARS: usize = 500;

/// The trailing note a `write` panel keeps after its fenced content (#788): the
/// text the tool reported after its success summary (V1's diagnostics), shown as
/// its own paragraph OUTSIDE the content's [`TOOL_OUTPUT_MAX_CHARS`] preview
/// budget, so a long note is never clipped by the content's clamp nor mistaken
/// for a clipped content — while its own
/// [`WRITE_TAIL_MAX_CHARS`] cap keeps a single panel under the card budget.
/// `None` whenever the panel renders no content body ([`write_body`]), or the
/// tool reported no note.
fn write_tail(tool: &ToolPanel) -> Option<String> {
    write_body(tool)?;
    tool.output()
        .as_deref()
        .and_then(summary_tail)
        .map(|tail| truncate_md(tail, WRITE_TAIL_MAX_CHARS))
}

/// The serialized byte length the trailing note [`write_tail`] appends to a
/// `write` panel (#788), for the size estimate — the note is card JSON like the
/// truncation note whose charge [`preview_truncation_note_estimate`] measures.
/// `0` when the note does not render. Kept beside the render so the two cannot
/// drift.
pub(crate) fn write_tail_estimate(tool: &ToolPanel) -> usize {
    write_tail(tool)
        .map(|tail| json_escaped_len(&CardMarkdown::new().element(&tail)))
        .unwrap_or(0)
}

/// The serialized byte length the Output body a panel renders contributes to
/// its card size (#788): a file-editing panel's PARSED diff ([`diff_output`],
/// clipped at [`DIFF_MAX_ESCAPED_BYTES`] as the render writes it, #792); a
/// `write` call's own `content` ([`write_body`], which the output text does not
/// carry) together with the `📄 <path>` header that body renders; else the
/// tool's output text. Every body is measured JSON-ESCAPED (the form it takes
/// inside the card's `content` string, each newline and quote costing its extra
/// byte) and with the dynamic fence [`fenced_overhead`] charges for a body full
/// of backticks. Kept beside the render so the estimate cannot charge a body
/// the panel never writes, nor miss the file a `write` just produced — and a
/// diff is charged under the SAME cap it renders under, never the old
/// 3,000-character preview (#792).
///
/// The boundary #792's acceptance line names: a diff that already fit the old
/// 3,000-character preview renders exactly as it did before and is charged at
/// its rendered size — the clip is a no-op, so nothing grows. Only a diff the
/// byte budget newly reveals (over 3,000 characters but inside
/// [`DIFF_MAX_ESCAPED_BYTES`]) is charged larger than the old preview charge,
/// because the card now carries those bytes. Ordinary `read`/`write` panels
/// keep their 3,000-character budget and their old charge untouched.
pub(crate) fn panel_output_estimate(tool: &ToolPanel) -> usize {
    // A diff renders its parsed hunks clipped at the byte budget, not the raw
    // recorded diff at the preview cap: charge exactly that body, the count
    // header above it and the WHOLE fence around it, language tag included.
    // The clip includes the same three in its budget, so the charge is exactly
    // what the render writes and can never trail a backtick run's fence or the
    // opening tag.
    if let Some((header, style, body)) = diff_output(tool) {
        let header = header
            .map(|header| json_escaped_len(&CardMarkdown::new().element(&header)))
            .unwrap_or(0);
        let lang = code_lang(&style);
        let body = clip_fenced_body(&body, lang, DIFF_MAX_ESCAPED_BYTES);
        return header + json_escaped_len(&body) + fenced_block_overhead(&body) + fence_lang_bytes(lang);
    }
    let write = write_body(tool);
    // The header rides the body's markdown OUTSIDE any fence, so it is charged
    // through the card's own sanitizer — a path that closes its inline code span
    // (`a\`<<<<.rs`) has its `<`-es expanded there, and the raw length would
    // under-count them.
    let header = write
        .as_ref()
        .and_then(|(header, _, _)| header.as_deref())
        .map(|header| json_escaped_len(&CardMarkdown::new().element(header)))
        .unwrap_or(0);
    let body = write
        .map(|(_, _, body)| body)
        .or_else(|| tool.output())
        .map(|body| {
            let body = truncate_md(&body, TOOL_OUTPUT_MAX_CHARS);
            json_escaped_len(&body) + fenced_overhead(&body)
        })
        .unwrap_or(0);
    header + body
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
    // One name read for the panel's name-keyed decisions: the output
    // formatter's dispatch, the todo section's detection, and the input
    // formatter's dispatch.
    let name = tool.name();
    // `execute` assembles its body from the call's nested-call metadata, and a
    // `write` renders its own `content` input, so those have their own builders;
    // every other tool's body is its output text through the per-tool formatter
    // (see [`panel_output`]). The result-count header (glob/grep) is
    // metadata-side, so it is applied over either path.
    let output = panel_output(tool);
    let output = output
        .map(|(header, style, body)| (search_count_header(name, tool.metadata()).or(header), style, body));
    // The todo panel is a status section, not a transcript: a parsed list is
    // the panel, so the generic Input line and Output marker would only frame
    // the checklist (a still-running call has no parsed output yet — the Input
    // line `📋 共 N 项任务` is all it can show), and its prefix names the
    // section rather than the call (a finished call would sit at a permanent ✅
    // while the counts right beside it still report open items).
    let todo_panel = name == "todowrite";
    let todowrite_list = todo_panel
        && output
            .as_ref()
            .is_some_and(|(_, style, _)| *style == BodyStyle::Markdown);
    let mut content = String::new();
    if !todowrite_list && let Some(i) = tool.input() {
        let formatted = format_tool_input(name, i);
        if !formatted.is_empty() {
            // A program is the exception to the 400-char input budget: its
            // code is the panel, so it gets the output budget.
            let budget = if tool.is_code_mode() {
                TOOL_OUTPUT_MAX_CHARS
            } else {
                400
            };
            // Trailing blank line so a multi-line input (edit diff, skill
            // metadata list) can't swallow the Output section as a markdown
            // lazy continuation of its last list line — Feishu would render
            // `**Output**` glued to the last input line.
            content.push_str(&format!("**Input**\n{}\n\n", truncate_md(&formatted, budget)));
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
            content.push_str("**Output**\n\n");
            if tool.is_code_mode() {
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
            // clipped diff has. A `read`/`write` preview keeps its 3,000-CHAR
            // budget and `truncate_md`'s marker (or `clip_md` when it carries
            // the way-in note), exactly as before.
            let note = truncation_note(tool, session_id);
            let body = if is_diff_body(name, &style) {
                clip_fenced_body(&body, code_lang(&style), DIFF_MAX_ESCAPED_BYTES)
            } else if note.is_some() && matches!(style, BodyStyle::Code(_)) {
                clip_md(&body, TOOL_OUTPUT_MAX_CHARS)
            } else {
                truncate_md(&body, TOOL_OUTPUT_MAX_CHARS)
            };
            // File content (read) and edit/apply_patch hunks always render as a
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
            // The tool's own trailing note rides after the fenced body, outside
            // the content's preview budget (#788), the way the truncation note
            // closes the section below it (ADR-0083 §8 amendment, #770/#788).
            if let Some(tail) = write_tail(tool) {
                content.push_str(&format!("\n\n{tail}"));
            }
            // A clipped `read`/`write` body says so, naming the button above it
            // (ADR-0083 §8 amendment, #770/#788). Only those two get it: they
            // are the panels whose clip hides a file the viewer can show whole,
            // and the note appears exactly when that button does.
            if let Some(note) = note {
                content.push_str(&format!("\n\n{note}"));
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
    let mut title = format!("{icon} {}{}", name, panel_time_suffix(at_ms));
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

/// [`parse_edit_diff`] for a multi-file patch (`apply_patch`): each `Index:`
/// line stays in the body, because a patch can touch several files and —
/// unlike `edit`, whose single target is named by the input panel — the hunks
/// alone would not say which file they belong to.
fn parse_multi_file_diff(diff: &str) -> Option<EditDiff> {
    parse_diff(diff, true)
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
            // patch (`recorded_diff` joins per-file strings that end in `\n`
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
/// header block in a concatenated patch. [`recorded_diff`] joins per-file
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

/// The header a diff panel's Output section leads with: the change count.
/// `None` for a MULTI-file patch — its count and totals ride the FOLDED title
/// ([`panel_file_detail`], #793), which stays visible when the panel expands,
/// so repeating the line under `**Output**` would say it twice. A single file
/// keeps the bare totals: an `edit` names its target on the Input line, and a
/// path repeated in one patch counts once, because `paths` is deduped. The
/// count and the totals come from this one [`EditDiff`], never from a second
/// scan of the body.
fn diff_header(diff: &EditDiff) -> Option<String> {
    (diff.paths.len() <= 1).then(|| format!("+{} −{}", diff.additions, diff.deletions))
}

/// The [`EditDiff`] a file-editing tool's output parses to, with that tool's
/// framing: `edit` drops the file-header block (its Input line names the one
/// target), `apply_patch`/`patch` keep each file's `Index:` line. `None` for
/// any other tool and for an output that is not a recognized diff. The one
/// parse rule the Output header and the folded title share, so the two cannot
/// disagree.
fn file_edit_diff(name: &str, output: &str) -> Option<EditDiff> {
    if !is_file_editing(name) {
        return None;
    }
    if name == "edit" {
        parse_edit_diff(output)
    } else {
        parse_multi_file_diff(output)
    }
}

/// The language token a diff's hunks are fenced with (measured 2026-10-18: a
/// `rust` fence colours the keywords and strings inside a `+`/`-` hunk, where
/// Feishu's documented `diff` token renders plain).
///
/// A multi-file patch uses the one language every touched file shares, and
/// [`PLAIN_TEXT`] when they disagree, so a `.rs` hunk is never coloured with a
/// `.md` file's grammar. The paths come from [`EditDiff::paths`] — an `Index:`
/// line when the patch has one, else the `---`/`+++` header (V2's `edit`).
fn diff_language(diff: &EditDiff) -> &'static str {
    diff_language_for(diff.paths.iter().map(String::as_str).chain(diff.path.as_deref()))
}

/// [`diff_language`] over an explicit path list: the one language every path
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

/// One item of a `todowrite` tool payload (OpenCode's `Todo.Info`). The
/// payload's `priority` is dropped on purpose: it rarely varies within one
/// list (agents mark nearly everything high), so showing it would add a marker
/// to almost every row and buy no signal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TodoItem {
    content: String,
    status: String,
}

/// Parse a `todowrite` payload into its items. The tool's input is
/// `{"todos":[…]}` and its output is that same array JSON-encoded, so both
/// shapes are accepted. Anything else (an empty list, a running call's
/// placeholder, an error text) returns `None`, and the caller falls back to
/// showing the payload unchanged.
fn parse_todos(value: &serde_json::Value) -> Option<Vec<TodoItem>> {
    let arr = match value {
        serde_json::Value::Array(arr) => arr,
        serde_json::Value::Object(obj) => obj.get("todos")?.as_array()?,
        _ => return None,
    };
    let mut items = Vec::new();
    for v in arr {
        // A list with one malformed row falls back to the raw payload as a
        // whole: rendering the readable rows and silently dropping the rest
        // would lose content the model wrote.
        let content = v.get("content")?.as_str()?;
        if content.is_empty() {
            return None;
        }
        // An unknown status renders as pending (`todo_style`'s fallback);
        // normalize it here so the row and the counts line can't disagree.
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("pending");
        let status = if TODO_STATUSES.iter().any(|(s, _, _)| *s == status) {
            status
        } else {
            "pending"
        };
        items.push(TodoItem {
            content: content.to_string(),
            status: status.to_string(),
        });
    }
    (!items.is_empty()).then_some(items)
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

fn todo_style(status: &str) -> (&'static str, TodoMarkup) {
    TODO_STATUSES
        .iter()
        .find(|(s, _, _)| *s == status)
        .map(|(_, icon, markup)| (*icon, *markup))
        .unwrap_or(("⬜", TodoMarkup::Plain))
}

/// The header of a todo panel: one `icon count` group per non-empty status, so
/// the folded panel still answers "how far along is this?" without unfolding.
fn todo_counts(todos: &[TodoItem]) -> String {
    TODO_STATUSES
        .iter()
        .filter_map(|(status, icon, _)| {
            let n = todos.iter().filter(|t| t.status == *status).count();
            (n > 0).then(|| format!("{icon} {n}"))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The checklist body of a todo panel: one `- <icon> content` line per item,
/// in the order the agent wrote them (that order carries the plan's sequence).
/// Finished and dropped items are struck through so the eye can skip them; the
/// in-progress item is bolded — it is the only actionable row.
fn format_todo_list(todos: &[TodoItem]) -> String {
    todos
        .iter()
        .map(|t| {
            let text = first_chunk(&t.content, 120);
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

/// Whether a `read` call's raw output is a DIRECTORY listing, by output shape
/// (ADR-0083 §8 amendment, #770): V1's XML envelope marks it
/// `<type>directory</type>` where a file's marks `<type>file</type>`, and V2's
/// header line opens `Read directory ` where a file's opens `Read file `. The
/// one place the two shapes are spelled — [`format_tool_output`] skips the V1
/// tag from here, [`parse_v2_read_output`] reads V2's prefix from here — so the
/// verdict a `read` panel's open path hangs on cannot drift from the parsers it
/// comes from. Each generation is read in its own envelope: the V1 tag only
/// counts ABOVE `<content>`, so a file whose own text spells the tag is still a
/// file, and the V2 prefix only on the first line, where its header lives.
fn read_output_is_directory(output: &str) -> bool {
    if output.contains("<path>") {
        return output
            .lines()
            .take_while(|line| line.trim() != "<content>")
            .any(|line| line.trim() == V1_DIRECTORY_TYPE_TAG);
    }
    output
        .lines()
        .next()
        .is_some_and(|first| first.starts_with(V2_DIRECTORY_HEADER_PREFIX))
}

/// Whether a tool is one whose panel shows its recorded diff in place of the
/// tool's success sentence ([`edit_tool_output`]) — the names the diff parser
/// and the clip note both key on. `edit` records a single file's diff; V1's
/// `apply_patch` and V2's `patch` record a multi-file patch.
fn is_file_editing(name: &str) -> bool {
    matches!(name, "edit" | "apply_patch" | "patch")
}

/// Render a tool's raw output string human-friendly. OpenCode's `read` tool
/// wraps its output in XML tags (`<path>…</path>`, `<type>…</type>`,
/// `<content>…</content>`) — strip them so the card shows just the file path
/// and the numbered lines; a `task`/`subagent` output is an XML envelope around
/// the content the reader wants. An `edit`, `apply_patch` or `patch` output is
/// substituted by its unified diff (`apply_patch`/`patch` keep each file's
/// `Index:` line) — keep only the hunks and report the change count, led by the
/// touched-file count when the patch names more than one file. A
/// `todowrite` output is its todo list JSON-encoded — render it as a status
/// checklist; a `websearch` output is a result envelope — render it as a
/// title+url list. Other tools pass through unchanged.
///
/// **OpenCode 2 shapes.** The same arms also recognize the current
/// generation's shapes: `subagent`'s `<subagent sessionID state>` envelope,
/// `read`'s `Read file <path>, lines N-M` header line (no XML wrapper), and
/// `websearch`'s markdown result blocks (`## [title](url)` + `Published:`).
/// `patch` is V2's `apply_patch`.
///
/// Returns `(header, body style, body)`: the header is the short markdown line
/// above the body (file path, change count, result count); the style decides
/// how the body blocks (see [`BodyStyle`]). A `todowrite` header is the list's
/// size and per-status counts — the folded panel's progress line. A multi-file
/// diff returns no header: its count and totals ride the folded title
/// ([`panel_file_detail`], #793), which stays visible when the panel expands.
fn format_tool_output(name: &str, output: &str) -> (Option<String>, BodyStyle, String) {
    if is_file_editing(name) {
        return match file_edit_diff(name, output) {
            Some(d) => {
                // The hunks, then the tool's trailing note (LSP diagnostics) on
                // its own paragraph — inside the same code block so the panel
                // never loses it.
                let lang = diff_language(&d);
                let header = diff_header(&d);
                let mut body = d.body;
                if !d.tail.is_empty() {
                    body.push('\n');
                    body.push_str(d.tail.trim_end());
                    body.push('\n');
                }
                (header, BodyStyle::Code(Some(lang)), body)
            }
            None => (None, BodyStyle::Auto, output.to_string()),
        };
    }
    if name == "task"
        && output.starts_with("<task ")
        && let Some((header, body)) = parse_task_envelope(output)
    {
        return (header, BodyStyle::Auto, body);
    }
    if name == "subagent"
        && output.starts_with("<subagent ")
        && let Some(body) = parse_subagent_envelope(output)
    {
        return (None, BodyStyle::Auto, body);
    }
    if name == "skill"
        && output.starts_with("<skill_content ")
        && let Some(body) = parse_skill_envelope(output)
    {
        return (None, BodyStyle::Auto, body);
    }
    if name == "todowrite" {
        // The output is the list itself, `JSON.stringify(todos, null, 2)`.
        // Parse it back into a checklist; a payload that doesn't parse (a
        // running call, an error, a truncated dump) falls through unchanged.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(output)
            && let Some(todos) = parse_todos(&value)
        {
            return (
                Some(format!("共 {} 项 · {}", todos.len(), todo_counts(&todos))),
                BodyStyle::Markdown,
                format_todo_list(&todos),
            );
        }
    }
    if name == "websearch" {
        // V1 emits a JSON envelope (`parallel`) or plain-text blocks (`exa`);
        // V2 emits markdown result blocks. Each parser shape-gates itself, so
        // an unrecognized provider output still passes through unchanged.
        if let Some((count, body)) = parse_websearch_results(output) {
            return (Some(format!("🔎 {} 条结果", count)), BodyStyle::Markdown, body);
        }
    }
    // V1's read output *starts* with the `<path>…</path>` wrapper; V2's carries
    // no wrapper at all. The test is on the **head**, not the whole output: a V2
    // read whose body merely *mentions* the literal `<path>` (any file quoting
    // it, e.g. a README documenting `--config <path>`) must still take the V2
    // branch below, or its path header, language token and first body line are
    // all lost. `trim_start` matches the V1 loop treating a leading blank line
    // as decoration.
    let v1_read_wrapper = name == "read" && output.trim_start().starts_with("<path>");
    if name == "read" && !v1_read_wrapper {
        // V2's read output carries no XML wrapper: a header line over numbered
        // lines or a directory listing. Anything else falls through to raw.
        if let Some(parsed) = parse_v2_read_output(output) {
            return parsed;
        }
    }
    if !v1_read_wrapper {
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
/// `glob`, `matches` for `grep`, plus its `truncated` flag). `None` when the
/// call carries neither — a generation that doesn't record it, or a third-party
/// tool that merely claims the id — so the panel keeps its raw shape.
fn search_count_header(name: &str, metadata: Option<&serde_json::Value>) -> Option<String> {
    let metadata = metadata?;
    let (count, unit) = match name {
        "glob" => (metadata.get("count")?.as_u64()?, "个文件"),
        "grep" => (metadata.get("matches")?.as_u64()?, "处匹配"),
        _ => return None,
    };
    let truncated = metadata.get("truncated").and_then(serde_json::Value::as_bool) == Some(true);
    Some(if truncated {
        format!("{count} {unit} · 已截断")
    } else {
        format!("{count} {unit}")
    })
}

/// The `execute` panel's output (V2's Code Mode entry): one status row per
/// nested tool call the program made — the call's metadata records them live,
/// in start order, while the program runs — followed by the formatted result.
/// The header is the call count (plus failures), which the panel promotes to
/// its folded title (`title_details`). `None` when there is neither a nested
/// call nor a result, so the panel falls back to its input alone.
fn execute_output(tool: &ToolPanel) -> Option<(Option<String>, BodyStyle, String)> {
    let calls = tool
        .metadata()
        .and_then(|metadata| metadata.get("toolCalls"))
        .and_then(serde_json::Value::as_array);
    let result = tool.output();
    if calls.is_none() && result.is_none() {
        return None;
    }
    let calls = calls.map(Vec::as_slice).unwrap_or_default();
    let failures = calls
        .iter()
        .filter(|call| call.get("status").and_then(serde_json::Value::as_str) == Some("error"))
        .count();
    let header = (!calls.is_empty()).then(|| match failures {
        0 => format!("{} 个工具调用", calls.len()),
        n => format!("{} 个工具调用 · {} 失败", calls.len(), n),
    });
    let mut body = calls
        .iter()
        .map(|call| {
            let name = call
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let status = call
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("- {} `{}`", nested_status_icon(status), name)
        })
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
fn nested_status_icon(status: &str) -> &'static str {
    match status {
        "completed" => "✅",
        "error" => "❌",
        "running" | "pending" => "⏳",
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
    let lines = results
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
        .collect::<Vec<_>>();
    Some((results.len(), lines.join("\n")))
}

/// `(title, url, publish date)` of one websearch result, shared by both
/// provider parsers.
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

/// Render a tool's input JSON as human-readable markdown, keyed on the tool
/// name. Recognized tools get tailored one-liners (bash → the command, read →
/// the file path, edit → the file; the edit's actual change comes from its diff
/// output); anything else falls back to a generic key-value list.
fn format_tool_input(name: &str, input: &serde_json::Value) -> String {
    // Non-object inputs (a bare path string, a number) just display as-is.
    let obj = match input.as_object() {
        Some(o) => o,
        None => return input.as_str().map(|s| s.to_string()).unwrap_or_default(),
    };
    let get = |k: &str| obj.get(k).and_then(|v| v.as_str());
    match name {
        "bash" | "shell" => {
            let mut s = String::new();
            if let Some(cmd) = get("command") {
                s.push_str(&format!("`{}`", cmd));
            }
            if let Some(dir) = get("workdir") {
                s.push_str(&format!("\n📁 `{}`", dir));
            }
            if let Some(d) = get("description") {
                s.push_str(&format!("\n📝 {}", d));
            }
            if s.is_empty() { input.to_string() } else { s }
        }
        "edit" => {
            // Only the target file. oldString/newString are near-identical
            // full-block snapshots (the change is usually a few lines deep in
            // them), so showing their -/+ first lines reads as duplicated
            // content and hides the real change — which now comes from the
            // tool's unified diff (see `parse_edit_diff`).
            format!("📄 `{}`", input_path(obj).unwrap_or("?"))
        }
        "read" | "write" | "glob" | "grep" => {
            let mut parts = Vec::new();
            // `pattern` is the search key for glob/grep — show it as "匹配",
            // not twice. File paths render as plain paths.
            if (name == "grep" || name == "glob")
                && let Some(v) = get("pattern")
            {
                parts.push(format!("匹配 `{}`", v));
            }
            if let Some(path) = input_path(obj) {
                parts.push(format!("`{}`", path));
            }
            if let Some(v) = get("include") {
                parts.push(format!("include `{}`", v));
            }
            let mut s = match name {
                "read" => "📖",
                "write" => "✏️",
                "glob" | "grep" => "🔍",
                _ => "🔧",
            }
            .to_string();
            s.push(' ');
            s.push_str(&parts.join(" · "));
            if let Some(offset) = obj.get("offset").and_then(|v| v.as_i64()) {
                s.push_str(&format!("\n从第 {} 行起", offset));
            }
            if let Some(limit) = obj.get("limit").and_then(|v| v.as_i64()) {
                // `limit` counts the tool's own unit: read pages lines, glob
                // counts files (grep's limit counts matching lines).
                let unit = if name == "glob" { "个文件" } else { "行" };
                s.push_str(&format!("\n最多 {} {}", limit, unit));
            }
            if s.len() <= 1 { input.to_string() } else { s }
        }
        // V2's `patch` gets the touched-file summary. V1's `apply_patch` keeps
        // its pre-V2 generic fallback: spec #467 US18 puts every V1 rendering
        // change out of scope, so only the V2 id takes this arm.
        "patch" => {
            let files = patch_files(get("patchText").unwrap_or(""));
            match files.as_slice() {
                [] => generic_tool_input(obj),
                [only] => format!("📄 `{only}`"),
                [first, ..] => format!("📄 `{first}` 等 {} 个文件", files.len()),
            }
        }
        "webfetch" => {
            if let Some(url) = get("url") {
                format!("🌐 `{}`", url)
            } else {
                input.to_string()
            }
        }
        "websearch" => match get("query") {
            Some(query) => format!("🔎 `{}`", query),
            None => generic_tool_input(obj),
        },
        "execute" => match get("code") {
            // V2's Code Mode entry: the program is the panel's input, fenced
            // with the output budget (`tool_panel_element` widens the input
            // clamp for this tool); the pre-clamp keeps the closing fence
            // inside the panel's budget.
            Some(code) => fenced_code(&truncate_md(code, TOOL_OUTPUT_MAX_CHARS - 64), Some("javascript")),
            None => generic_tool_input(obj),
        },
        "task" | "subagent" => {
            let desc = get("description").unwrap_or("子任务");
            // V1 names the subagent type `subagent_type`, V2 `agent`.
            let sub = get("agent")
                .or_else(|| get("subagent_type"))
                .map(|s| format!("\n🤖 `{}`", s))
                .unwrap_or_default();
            format!("🔀 {}{}", desc, sub)
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
        "todowrite" => match parse_todos(input) {
            // The list itself is the Output section; the input only reports
            // how big the plan is — and is all a still-running panel can show.
            Some(todos) => format!("📋 共 {} 项任务", todos.len()),
            None => input.to_string(),
        },
        _ => generic_tool_input(obj),
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

/// The files a patch text touches, in the order it names them: the
/// `*** Update File: <path>` / `*** Add File:` / `*** Delete File:` headers of
/// OpenCode's patch format (V1 `apply_patch` and V2 `patch`).
fn patch_files(patch_text: &str) -> Vec<&str> {
    patch_text
        .lines()
        .filter_map(|line| {
            ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        })
        .collect()
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
            status: ToolStatus::Completed,
            started_at: None,
            input: None,
            metadata: None,
            output: ToolOutput {
                raw: None,
                blocks: vec![ContentBlock::File(content)],
                error: None,
            },
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
            header, "✅ read",
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
        assert_eq!(title, "✅ read", "{card}");
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
        assert_eq!(title, "✅ read · 📎 doc.pdf · 未发送", "{card}");
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
        assert_eq!(title, "✅ read · 📎 doc.pdf", "{card}");
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
        assert_eq!(title, "✅ read · 📎 doc.pdf · 未发送", "{card}");
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
        assert_eq!(title, "✅ read · 📎 shot.png", "{card}");
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
        assert_eq!(title, "✅ read · 📎 shot.png", "{card}");
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
            let panel = ToolPanel::for_test(name, ToolStatus::Completed, Some(input), Some("ok"));
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
            let panel = ToolPanel::for_test("read", ToolStatus::Completed, Some(input), Some(output));
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
            let panel = ToolPanel::for_test(name, ToolStatus::Completed, Some(input), Some(&output));
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
            let panel = ToolPanel::for_test("read", ToolStatus::Completed, Some(input), Some(output));
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
        let running = ToolPanel::for_test(
            "read",
            ToolStatus::Running,
            Some(json!({"filePath": "/x/y.rs"})),
            None,
        );
        assert_eq!(running.file_open_path(), None, "no output, no verdict");
        assert!(tool_open_button_element(&running, Some("ses_1")).is_none());

        let settled = ToolPanel::for_test(
            "read",
            ToolStatus::Completed,
            Some(json!({"filePath": "/x/y.rs"})),
            Some("Read file /x/y.rs, lines 1-1\n1: fn main() {}"),
        );
        assert_eq!(settled.file_open_path(), Some("/x/y.rs"));
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
        let clipped = ToolPanel::for_test(
            "read",
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
        let panel = ToolPanel::for_test(
            "read",
            ToolStatus::Completed,
            Some(json!({"filePath": "/x/big.rs"})),
            Some(&v1),
        );
        let md = panel_md_with_session(panel, "ses_1");
        assert!(
            md.ends_with(&format!("\n\n{PREVIEW_TRUNCATION_NOTE}")),
            "V1 clips too: {md}"
        );

        let small = ToolPanel::for_test(
            "read",
            ToolStatus::Completed,
            Some(json!({"path": "/x/small.rs"})),
            Some("Read file /x/small.rs, lines 1-1\n1: fn main() {}"),
        );
        assert_eq!(preview_truncation_note_estimate(&small, Some("ses_1")), 0);
        let md = panel_md_with_session(small, "ses_1");
        assert!(!md.contains(PREVIEW_TRUNCATION_NOTE), "nothing was cut: {md}");

        let directory = ToolPanel::for_test(
            "read",
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

    /// #788: a `write` panel renders the call's own `content` as its body —
    /// fenced with the touched file's language from the shared table — instead
    /// of the tool's bare success sentence, so the file the agent just produced
    /// is on the card. The body names the file (`📄 <path>`, the `read` panel's
    /// shape), the call's own Input line stays, and the folded header and
    /// 「📂 打开」 are unchanged.
    #[test]
    fn a_write_panels_content_renders_as_a_fenced_body() {
        // V1's `filePath` and V2's `path`; the fence follows the extension — a
        // known one, a `.md` (its SOURCE, like the viewer's default) and an
        // unknown one that stays plain.
        for (input, path, lang) in [
            (
                json!({"filePath": "src/main.rs", "content": "line one\nline two"}),
                "src/main.rs",
                "rust",
            ),
            (
                json!({"path": "notes.md", "content": "line one\nline two"}),
                "notes.md",
                "markdown",
            ),
            (
                json!({"filePath": "data.unknownext", "content": "line one\nline two"}),
                "data.unknownext",
                "plain_text",
            ),
        ] {
            let panel = ToolPanel::for_test(
                "write",
                ToolStatus::Completed,
                Some(input),
                Some("Wrote file successfully."),
            );
            let md = panel_md_with_session(panel.clone(), "ses_1");
            assert!(md.contains("line one\nline two"), "the content is the body: {md}");
            assert!(
                !md.contains("Wrote file successfully."),
                "the success sentence is dropped as noise: {md}"
            );
            assert!(
                md.contains(&format!("📄 `{path}`")),
                "the body names the file: {md}"
            );
            assert!(
                md.contains(&format!("```{lang}")),
                "the fence follows the extension: {md}"
            );
            assert!(md.contains("**Input**"), "the call's own Input line stays: {md}");
            assert_eq!(panel.file_open_path(), Some(path), "the way in is unchanged");
            assert!(
                tool_open_button_element(&panel, Some("ses_1")).is_some(),
                "the open button still renders"
            );
        }
    }

    /// A `write` body is clipped to [`TOOL_OUTPUT_MAX_CHARS`] exactly like a
    /// `read` preview, and says so through the same note — and only when the
    /// panel carries the way-in button, so a Session-less one promises nothing
    /// (ADR-0083 §8 amendment, #770/#788).
    #[test]
    fn a_clipped_write_body_says_the_preview_was_clipped() {
        let content = "x".repeat(TOOL_OUTPUT_MAX_CHARS + 1_000);
        let clipped = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"path": "/x/big.rs", "content": content})),
            Some("Wrote file successfully."),
        );
        assert!(preview_truncation_note(&clipped, Some("ses_1")).is_some());
        assert_eq!(
            preview_truncation_note_estimate(&clipped, Some("ses_1")),
            PREVIEW_TRUNCATION_NOTE.len(),
            "the note's bytes are charged"
        );
        let md = panel_md_with_session(clipped.clone(), "ses_1");
        assert!(
            md.ends_with(&format!("\n\n{PREVIEW_TRUNCATION_NOTE}")),
            "the note closes a clipped write body: {md}"
        );
        assert!(
            !md.contains('…'),
            "a clipped fenced body carries NO `…`: the marker stops Feishu \
             rendering the block as code (#788), so the note is the signal: {md}"
        );
        assert!(
            !md.contains("Wrote file successfully."),
            "a clipped body still drops the success sentence: {md}"
        );

        // A body that fits carries nothing.
        let small = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"path": "/x/small.rs", "content": "fn main() {}"})),
            Some("Wrote file successfully."),
        );
        assert_eq!(preview_truncation_note_estimate(&small, Some("ses_1")), 0);
        let md = panel_md_with_session(small, "ses_1");
        assert!(!md.contains(PREVIEW_TRUNCATION_NOTE), "nothing was cut: {md}");

        // No Session to route the button by: no button, no note.
        assert_eq!(preview_truncation_note_estimate(&clipped, None), 0);
        let md = panel_md(clipped);
        assert!(!md.contains(PREVIEW_TRUNCATION_NOTE), "no button, no note: {md}");
    }

    /// The body is only replaced by the content when a `write` really carries
    /// one (#788): a call with no usable `content` — and a FAILED write, whose
    /// error text is the body — keeps today's output panel, while a trailing
    /// note the tool reported after its success summary is kept below the fenced
    /// content, the way [`edit_tool_output`] treats an edit's tail.
    #[test]
    fn a_write_falls_back_to_its_output_when_it_has_no_content() {
        // No `content`: today's panel, the output text and all.
        let bare = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"filePath": "report.md"})),
            Some("Wrote file successfully."),
        );
        let md = panel_md_with_session(bare, "ses_1");
        assert!(
            md.contains("Wrote file successfully."),
            "the output text stays: {md}"
        );
        assert!(!md.contains("```"), "no content, no fence: {md}");

        // A FAILED write keeps its error rather than content it never wrote.
        let failed = ToolPanel::for_test(
            "write",
            ToolStatus::Error,
            Some(json!({"filePath": "report.md", "content": "secret"})),
            Some("Unable to write report.md"),
        );
        let md = panel_md_with_session(failed, "ses_1");
        assert!(
            md.contains("Unable to write report.md"),
            "the error is the body: {md}"
        );
        assert!(
            !md.contains("secret"),
            "the content cannot replace an error: {md}"
        );

        // A status this build does not model but the server named "failed" is a
        // failure too — its error text must not be replaced by the content.
        let failed = ToolPanel::for_test(
            "write",
            ToolStatus::Other("failed".into()),
            Some(json!({"filePath": "report.md", "content": "secret"})),
            Some("write failed"),
        );
        let md = panel_md_with_session(failed, "ses_1");
        assert!(
            md.contains("write failed"),
            "a named failure keeps its error: {md}"
        );
        assert!(
            !md.contains("secret"),
            "the content cannot replace a failure: {md}"
        );

        // The wire shape an error-only `"failed"` call carries (V2's
        // `state.error`, no text block): the error still reaches the panel —
        // suppressing the content must not lose it (#788).
        let error_only = ToolPanel::new(crate::backend::ToolCall {
            identity: crate::backend::ToolIdentity {
                name: "write".into(),
                call_id: "call_write".into(),
            },
            status: ToolStatus::Other("failed".into()),
            started_at: None,
            input: Some(json!({"path": "src/main.rs", "content": "fn main() {}"})),
            metadata: None,
            output: crate::backend::ToolOutput {
                raw: None,
                blocks: Vec::new(),
                error: Some("permission denied".into()),
            },
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

        // A long FAILED-write error is not a file preview: its panel carries no
        // "open the full file" note, whatever its length (#788).
        let long_error = format!("write failed: {}", "e".repeat(TOOL_OUTPUT_MAX_CHARS * 2));
        let failed = ToolPanel::for_test(
            "write",
            ToolStatus::Error,
            Some(json!({"filePath": "report.md", "content": "secret"})),
            Some(&long_error),
        );
        assert_eq!(
            preview_truncation_note_estimate(&failed, Some("ses_1")),
            0,
            "an error body is not a clipped preview"
        );
        let md = panel_md_with_session(failed, "ses_1");
        assert!(md.contains("write failed:"), "the error is the body: {md}");
        assert!(
            !md.contains(PREVIEW_TRUNCATION_NOTE),
            "no note over an error: {md}"
        );

        // The tool's trailing note survives AFTER the fence, outside the
        // content's preview budget.
        let noted = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"filePath": "src/main.rs", "content": "fn main() {}"})),
            Some("Wrote file successfully.\n\nLSP errors detected in this file, please fix:"),
        );
        let md = panel_md_with_session(noted, "ses_1");
        assert!(md.contains("fn main() {}"), "the content is the body: {md}");
        assert!(
            md.contains("```\n\nLSP errors detected"),
            "the trailing note rides outside the fence: {md}"
        );
        assert!(
            !md.contains("Wrote file successfully."),
            "the summary is still dropped: {md}"
        );

        // A note is never counted against the content's budget, so a note larger
        // than the content cannot fake a truncated preview (#788).
        let long_tail = "N".repeat(400);
        let noted = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"filePath": "src/main.rs", "content": "fn main() {}"})),
            Some(&format!("Wrote file successfully.\n\n{long_tail}")),
        );
        assert_eq!(
            preview_truncation_note_estimate(&noted, Some("ses_1")),
            0,
            "the content fits; a long note cannot fake a clip"
        );
        let md = panel_md_with_session(noted, "ses_1");
        assert!(md.contains(&long_tail), "the note is not clipped: {md}");
        assert!(!md.contains(PREVIEW_TRUNCATION_NOTE), "no clip, no note: {md}");

        // A note past its own cap is clipped there, not rendered whole: a lone
        // panel cannot split, so one panel must stay bounded.
        let huge_tail = "N".repeat(4 * TOOL_OUTPUT_MAX_CHARS);
        let noted = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"filePath": "src/main.rs", "content": "fn main() {}"})),
            Some(&format!("Wrote file successfully.\n\n{huge_tail}")),
        );
        assert_eq!(
            write_tail_estimate(&noted),
            json_escaped_len(&truncate_md(&huge_tail, WRITE_TAIL_MAX_CHARS)),
            "the clipped note's bytes are charged"
        );
        let md = panel_md_with_session(noted, "ses_1");
        assert!(!md.contains(&huge_tail), "the whole note is not rendered: {md}");
        assert!(md.contains('…'), "the oversized note is marked as clipped: {md}");
    }

    /// A body full of backticks sizes a fence longer than its content, and the
    /// estimate charges that fence: a 3,000-backtick content renders ~6 KB of
    /// fence nothing counted, letting a near-budget card evade the split (#788).
    #[test]
    fn the_write_body_estimate_charges_the_dynamic_fence() {
        let content = "`".repeat(TOOL_OUTPUT_MAX_CHARS);
        let panel = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"filePath": "notes.md", "content": content.clone()})),
            Some("Wrote file successfully."),
        );
        assert!(
            fenced_overhead(&content) > TOOL_OUTPUT_MAX_CHARS,
            "the fence dwarfs the body it wraps"
        );
        assert_eq!(
            panel_output_estimate(&panel),
            TOOL_OUTPUT_MAX_CHARS + fenced_overhead(&content) + json_escaped_len("📄 `notes.md`"),
            "the fence beyond the minimum is charged"
        );
    }

    /// A `write` panel's Output section renders its own `📄 <path>` header, and
    /// that path is not part of the content: a long path is charged beside the
    /// body, or a turn heavy on deep paths would undercount its cards (#788).
    #[test]
    fn the_write_body_estimate_charges_the_output_header_path() {
        let long_path = format!("{}.rs", "dir/".repeat(1_000));
        let panel = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"filePath": long_path, "content": "x"})),
            Some("Wrote file successfully."),
        );
        assert_eq!(
            panel_output_estimate(&panel),
            json_escaped_len(&format!("📄 `{long_path}`")) + 1,
            "the header's path and the one-char body are both charged"
        );

        // A path that closes its inline code span early (`a`<<<<.rs`) is charged
        // as the SANITIZER renders it: its bare `<`s expand to `&lt;`, which a
        // raw-length charge would miss (#788).
        let tricky = "a`<<<<<<<<<<<<.rs";
        let panel = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"filePath": tricky, "content": "x"})),
            Some("Wrote file successfully."),
        );
        let header = format!("📄 `{tricky}`");
        let rendered = CardMarkdown::new().element(&header);
        assert!(
            rendered.len() > header.len(),
            "the sanitizer expands the bare `<`s: {rendered}"
        );
        assert_eq!(
            panel_output_estimate(&panel),
            json_escaped_len(&rendered) + 1,
            "the expanded header is charged, not the raw path"
        );
    }

    /// The body charge is the JSON-ESCAPED length the card serializes, not the
    /// raw UTF-8 length: a written file full of newlines and quotes costs two
    /// bytes each inside the card's `content` string, and a raw-length estimate
    /// would let a near-budget card pass the split check while the serialized
    /// card exceeds Feishu's cap (#788).
    #[test]
    fn the_write_body_estimate_charges_the_escaped_bytes() {
        let content = "\n".repeat(1_000);
        let panel = ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"filePath": "src/main.rs", "content": content})),
            Some("Wrote file successfully."),
        );
        assert_eq!(
            panel_output_estimate(&panel),
            2 * 1_000 + json_escaped_len("📄 `src/main.rs`"),
            "each newline costs its escaped byte pair inside the card"
        );
        assert!(
            panel_output_estimate(&panel) > content.len(),
            "the raw byte length would under-charge"
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
        let panel = ToolPanel::for_test(
            "write",
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
        let panel = ToolPanel::for_test(
            "write",
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
            markdown["content"].as_str().unwrap().contains("**Input**"),
            "the body is still the rendered call: {markdown}"
        );
    }

    /// A `write`/`edit` panel's folded header names the file it touched
    /// (ADR-0083 §8 amendment, #766): the path's BASENAME, through the same
    /// short-name rule the `📎` marker uses, so an absolute path never reaches
    /// the one-line header.
    #[test]
    fn write_and_edit_headers_name_the_touched_file() {
        for (name, input, want) in [
            (
                "write",
                json!({"filePath": "report.md", "content": "hi"}),
                "report.md",
            ),
            ("write", json!({"path": "src/v2w.rs"}), "v2w.rs"),
            (
                "edit",
                json!({"filePath": "/root/workspace/dev/cola/src/feishu/card/tool_render.rs"}),
                "tool_render.rs",
            ),
            ("edit", json!({"path": "src/v2.rs"}), "v2.rs"),
        ] {
            let title = panel_title(ToolPanel::for_test(
                name,
                ToolStatus::Completed,
                Some(input),
                Some("ok"),
            ));
            assert!(
                title.ends_with(&format!(" · 📄 {want}")),
                "{name} header must name {want}: {title}"
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
        let text = panel_title(ToolPanel::for_test(
            "read",
            ToolStatus::Completed,
            Some(json!({"filePath": "/x/y.rs"})),
            Some("<path>/x/y.rs</path>"),
        ));
        assert!(text.ends_with(" · 📖 y.rs"), "text read header: {text}");

        // A directory read's path may keep its trailing separator: the
        // detail still reaches the directory's own name, never the whole path.
        let dir = panel_title(ToolPanel::for_test(
            "read",
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
        let clipped = panel_title(ToolPanel::for_test(
            "write",
            ToolStatus::Completed,
            Some(json!({"filePath": long})),
            Some("ok"),
        ));
        let shown = clipped.rsplit("📄 ").next().expect("the detail");
        assert!(
            shown.starts_with('…') && shown.ends_with(".md"),
            "a long name keeps its tail: {clipped}"
        );
        assert!(
            shown.chars().count() <= FILE_HEADER_NAME_MAX_CHARS,
            "clipped to the budget: {clipped}"
        );

        let folded = panel_title(ToolPanel::for_test(
            "edit",
            ToolStatus::Completed,
            Some(json!({"filePath": "re\nport.md"})),
            Some("ok"),
        ));
        assert!(folded.contains("📄 re port.md"), "folded to one line: {folded}");
    }

    /// The header detail is charged at its JSON-escaped length, exactly like
    /// the File Content marker, so card splitting sees the bytes the header
    /// renders (ADR-0083 §8 amendment, #766).
    #[test]
    fn the_header_detail_estimate_charges_the_rendered_bytes() {
        let named = ToolPanel::for_test(
            "write",
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
        let anonymous = ToolPanel::for_test("write", ToolStatus::Completed, None, Some("ok"));
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
        assert_eq!(title, "✅ read · 📎 report part.pdf · 未发送", "{card}");

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
            assert_eq!(title, "✅ read · 📎 report part.pdf · 未发送", "{card}");

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
        assert_eq!(title, "✅ read · 📎 文件 · 未发送", "{card}");
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
        let tool = ToolPanel::for_test(
            "read",
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
                .contains("✅ read")
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
            "✅ shell · 14:05"
        );
        assert_eq!(
            title(&panel("shell", ToolStatus::Completed, None)),
            "✅ shell · 14:05"
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
            "⏳ shell · 14:05"
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
            last_activity_ms: chrono::Utc::now().timestamp_millis() - 12_000,
            wait: Some(AwaitingAction::Permission),
        }));
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_tool(tool)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let title = elements[0]["header"]["title"]["content"].as_str().unwrap();
        assert!(
            title.starts_with("⏳ task · ") && title.contains("bash 1") && title.contains("等待你的授权"),
            "the collapsed task title carries the child activity before its wait: {title}"
        );
    }

    /// The liveness line is live-only: a settled panel drops it even if a
    /// stale snapshot was never cleared.
    #[test]
    fn settled_task_panel_drops_child_liveness() {
        let mut tool = ToolPanel::for_test("task", ToolStatus::Completed, None, Some("done"));
        tool.set_liveness(Some(TaskLiveness {
            activity: ChildActivity::Thinking,
            last_activity_ms: 1,
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
            last_activity_ms: now - ago_ms,
            wait: None,
        };
        assert_eq!(tool(5_000, "bash").title_fragment(now), "bash 5s");
        assert_eq!(tool(7_200_000, "bash").title_fragment(now), "bash 2h0m");

        let thinking = TaskLiveness {
            activity: ChildActivity::Thinking,
            last_activity_ms: now - 90_000,
            wait: Some(AwaitingAction::Both),
        };
        assert_eq!(thinking.title_fragment(now), "思考中 1m30s · 等待你的授权/回答");

        let untimed = TaskLiveness {
            activity: ChildActivity::Tool {
                name: "bash".into(),
                started_at: None,
            },
            last_activity_ms: now - 1_000,
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

    /// The child session id is read from a child-spawning call's metadata only:
    /// V1's `task` records it as `sessionId`, V2's `subagent` as `sessionID`.
    /// Every other tool and a metadata-less call yield nothing.
    #[test]
    fn task_child_session_id_reads_both_generations_metadata() {
        let plain = ToolPanel::for_test("task", ToolStatus::Running, None, None);
        assert_eq!(plain.child_session_id(), None);

        let with_metadata = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "task".into(),
                call_id: "call_1".into(),
            },
            status: ToolStatus::Running,
            started_at: None,
            input: None,
            metadata: Some(json!({"sessionId": "ses_child", "parentSessionId": "ses_parent"})),
            output: ToolOutput::default(),
        });
        assert_eq!(with_metadata.child_session_id(), Some("ses_child"));

        let subagent = ToolPanel::new(ToolCall {
            identity: ToolIdentity {
                name: "subagent".into(),
                call_id: "call_2".into(),
            },
            status: ToolStatus::Running,
            started_at: None,
            input: None,
            metadata: Some(json!({"sessionID": "ses_v2_child", "status": "running"})),
            output: ToolOutput::default(),
        });
        assert_eq!(subagent.child_session_id(), Some("ses_v2_child"));

        // A `subagent` part without a recorded child id yields nothing.
        let bare = ToolPanel::for_test("subagent", ToolStatus::Running, None, None);
        assert_eq!(bare.child_session_id(), None);

        let mut bash = ToolPanel::for_test("bash", ToolStatus::Running, None, None);
        bash.call = ToolCall {
            metadata: Some(json!({"sessionId": "ses_child"})),
            ..bash.call.clone()
        };
        assert_eq!(bash.child_session_id(), None);
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
        let (header, style, body) = format_tool_output("read", raw);
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
        assert_eq!(format_tool_output("bash", raw).2, raw);
        assert_eq!(format_tool_output("bash", raw).0, None);
        // Even a read-named tool without the wrapper is left alone.
        assert_eq!(format_tool_output("read", "no wrapper here").2, "no wrapper here");
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
        let (header, style, body) = format_tool_output("read", raw);
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
        let (header, style, body) = format_tool_output("read", raw);
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
        let (header, style, body) = format_tool_output("task", raw);
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
        let (header, _, body) = format_tool_output("task", raw);
        assert_eq!(header.as_deref(), Some("Background task failed: research"));
        assert_eq!(body.trim(), "boom");
        assert_eq!(format_tool_output("task", "plain text").2, "plain text");
    }

    /// An empty `<task_result>` must still lose its envelope — dumping the raw
    /// XML back on the card is worse than an empty output.
    #[test]
    fn task_output_with_an_empty_result_still_strips_the_envelope() {
        let raw = "<task id=\"ses_1\" state=\"completed\">\n<task_result>\n</task_result>\n</task>";
        let tool = ToolPanel::for_test(
            "task",
            ToolStatus::Completed,
            Some(json!({"description": "sub"})),
            Some(raw),
        );
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
        let (header, style, body) = format_tool_output("skill", raw);
        assert_eq!(header, None, "the input names the skill");
        assert_eq!(style, BodyStyle::Auto);
        assert!(body.contains("Do the work."), "content kept: {body:?}");
        assert!(!body.contains("<skill_content"), "wrapper stripped: {body:?}");
        assert!(!body.contains("skill_files"), "file list stripped: {body:?}");
        assert!(!body.contains("other.md"), "sampled files stripped: {body:?}");
        // An empty envelope is stripped too, never leaked raw.
        let (_, _, empty) = format_tool_output("skill", "<skill_content name=\"x\">\n</skill_content>");
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
        let (header, _, body) = format_tool_output("websearch", &raw);
        assert_eq!(header.as_deref(), Some("🔎 2 条结果"));
        assert!(
            body.contains("1. [First result](https://a.example/one) · 2026-01-02"),
            "dated first result: {body}"
        );
        assert!(body.contains("2. [Second result]("), "second result: {body}");
        assert!(!body.contains("ignored"), "excerpts dropped: {body}");

        let tool = ToolPanel::for_test(
            "websearch",
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
        let plain = format_tool_output("websearch", "No search results found.");
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
        let (header, _, body) = format_tool_output("websearch", raw);
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
        let (header, style, body) = format_tool_output("edit", diff);
        assert_eq!(header.as_deref(), Some("+1 −1"), "count header: {:?}", header);
        assert_eq!(
            style,
            BodyStyle::Code(Some("rust")),
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
        // An edit without a parseable diff (running / error text) stays as-is.
        let raw = "❌ Could not find oldString in the file.";
        let (header, style, body) = format_tool_output("edit", raw);
        assert_eq!(header, None);
        assert_eq!(style, BodyStyle::Auto);
        assert_eq!(body, raw);
        assert_eq!(parse_edit_diff(raw), None);
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
            md.contains("**Input**\n📄 `src/main.rs`"),
            "file in input: {}",
            md
        );
        assert!(md.contains("**Output**\n\n+1 −1"), "count header: {}", md);
        // Hunks render inside a fenced code block (monospace, no wrapping),
        // with the edited file's own language so the syntax is coloured.
        assert!(md.contains("```rust\n@@ -1,4 +1,4 @@"), "fenced hunk: {}", md);
        assert!(md.contains("-fn main() {}"), "removed line: {}", md);
        assert!(md.contains("+fn main() { println"), "added line: {}", md);
        assert!(md.contains(" use std::fs;"), "context line kept: {}", md);
        assert!(md.contains("\n```"), "closing fence: {}", md);
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
        let (_, _, ascii_body) = format_tool_output("edit", &ascii);
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
        let (header, _, _) = format_tool_output("edit", &cjk);
        let header = header.expect("the count header");
        let estimate = panel_output_estimate(&panel);
        let md = panel_md(panel);
        let (body, _) = md
            .split_once("```rust\n")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split_once("\n```\n\n"))
            .expect("the clipped fenced body");
        assert_eq!(
            estimate,
            json_escaped_len(&CardMarkdown::new().element(&header))
                + json_escaped_len(body)
                + fenced_block_overhead(body)
                + "rust".len(),
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
        let (_, _, ascii_body) = format_tool_output("edit", &ascii);
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
        });
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(
            md.contains("**Input**\n📄 `/root/.config/opencode/AGENTS.md`"),
            "file in input: {md}"
        );
        assert!(md.contains("**Output**\n\n+1 −3"), "count header: {md}");
        assert!(md.contains("```markdown\n@@ -1,6 +1,4 @@"), "fenced hunk: {md}");
        assert!(md.contains("-# English Learning Rule"), "removed line: {md}");
        assert!(md.contains("+# Interaction Rules"), "added line: {md}");
        assert!(!md.contains("(1 replacement)"), "generated summary dropped: {md}");
    }

    /// #793: V2's `edit` records `metadata.files[]`, and `recorded_diff` joins
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
        });
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(md.contains("**Output**\n\n+1 −1"), "count header: {md}");
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
        });
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        assert!(md.contains("**Output**\n\n+1 −1"), "count header: {md}");
        assert!(
            md.contains("```rust\nIndex: /x/one.rs"),
            "fenced body with the patched file: {md}"
        );
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

    /// `recorded_diff` joins per-file patch strings — each ending in `\n` —
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
        let d = parse_multi_file_diff(&diff).expect("parsed");
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
        let (_, style, _) = format_tool_output("edit", one);
        assert_eq!(style, BodyStyle::Code(Some("rust")));

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
        let (header, style, _) = format_tool_output("apply_patch", &two);
        assert_eq!(header, None, "two files: the folded title owns the count (#793)");
        assert_eq!(
            style,
            BodyStyle::Code(Some(PLAIN_TEXT)),
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

    /// #202: an `apply_patch` can touch several files, so its hunks keep each
    /// file's `Index:` line in the body — unlike `edit`, whose single target is
    /// named by the input panel.
    #[test]
    fn apply_patch_output_keeps_per_file_attribution() {
        let diff = "\
Index: /a/one.rs
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1,3 +1,3 @@
 let a = 1;
-let b = 2;
+let b = 3;
 let c = 4;
Index: /a/two.rs
===================================================================
--- /a/two.rs
+++ /a/two.rs
@@ -1 +1 @@
-x
+y";
        let (header, style, body) = format_tool_output("apply_patch", diff);
        assert_eq!(
            header, None,
            "a multi-file patch has no body header: the folded title owns it (#793)"
        );
        assert_eq!(
            style,
            BodyStyle::Code(Some("rust")),
            "both touched files share one language"
        );
        assert!(body.contains("Index: /a/one.rs"), "first file named: {body}");
        assert!(body.contains("Index: /a/two.rs"), "second file named: {body}");
        assert!(body.contains("+let b = 3;"), "hunk kept: {body}");
        assert!(body.contains("+y"), "second hunk kept: {body}");
        assert!(body.contains(" let a = 1;"), "context kept: {body}");
        assert!(!body.contains("+++"), "file header noise dropped: {body}");
    }

    /// #793: a multi-file patch's COLLAPSED title says how many files it
    /// touched ahead of the totals — the body's `Index:` separators are
    /// invisible until the reader expands, so the count is what says whether
    /// expanding is worth it. The title owns that line, so the expanded body
    /// does not repeat it, and the count and totals are one `EditDiff`: the
    /// paths it deduped and the lines it summed.
    #[test]
    fn a_multi_file_patch_title_names_the_file_count() {
        let diff = "\
Index: /a/one.rs
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1 +1,2 @@
-old
+new
+extra
Index: /a/two.rs
===================================================================
--- /a/two.rs
+++ /a/two.rs
@@ -1 +1 @@
-x
+y
Index: /a/three.md
===================================================================
--- /a/three.md
+++ /a/three.md
@@ -1 +1 @@
-old
+new";
        let tool = panel_with(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            json!({"diff": diff}),
            Some("Success. Updated the following files"),
        );
        assert_eq!(
            panel_title(tool.clone()),
            "✅ patch · 📄 3 files · +4 −3",
            "the folded title carries the file count and the totals"
        );
        let body = panel_md(tool);
        assert!(
            !body.contains("📄 3 files") && !body.contains("+4 −3"),
            "the body does not repeat the title's count: {body}"
        );
        assert_eq!(
            body.matches("Index: ").count(),
            3,
            "the body keeps one `Index:` separator per file: {body}"
        );
    }

    /// #793: the count-and-totals fragment is a `plain_text` title byte the
    /// card carries, so [`panel_file_detail_estimate`] must charge it through
    /// the same derivation the title renders — a fragment the split check
    /// missed would let a near-budget card grow past the serialized cap.
    #[test]
    fn the_multi_file_title_detail_is_charged() {
        let diff = "\
Index: /a/one.rs
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1 +1 @@
-old
+new
Index: /a/two.rs
===================================================================
--- /a/two.rs
+++ /a/two.rs
@@ -1 +1 @@
-old
+new";
        let tool = panel_with(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            json!({"diff": diff}),
            Some("Success. Updated the following files"),
        );
        assert_eq!(
            panel_file_detail_estimate(&tool),
            json_escaped_len(" · 📄 2 files · +2 −2"),
            "the folded title's count and totals are card bytes"
        );
    }

    /// #793: a patch that names the same file twice is still one file — the
    /// deduped `paths` decide, so the title never claims `2 files` for one
    /// target. A lone file also gets no `1 file` prefix, which would only
    /// repeat the path the Input line already shows.
    #[test]
    fn a_repeated_file_keeps_the_bare_change_count() {
        let diff = "\
Index: /a/one.rs
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1 +1 @@
-old
+new
Index: /a/one.rs
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -5 +5 @@
-old
+new";
        let (header, _, body) = format_tool_output("patch", diff);
        assert_eq!(header.as_deref(), Some("+2 −2"), "one file, bare totals");
        assert_eq!(
            body.lines().filter(|l| l.starts_with("Index: ")).count(),
            2,
            "the body is untouched: {body}"
        );
        let tool = panel_with(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            json!({"diff": diff}),
            Some("Success. Updated the following files"),
        );
        assert_eq!(
            panel_title(tool.clone()),
            "✅ patch",
            "one file gets no count detail in the title"
        );
        assert!(panel_md(tool).contains("+2 −2"), "the body keeps the totals");
    }

    /// #793 regression: a single-file `edit`/`patch` panel's folded title and
    /// body stay byte-identical to before the file count existed — the title
    /// names the edited file, and the body heads with the bare totals.
    #[test]
    fn a_single_file_edit_keeps_todays_title_and_body() {
        let diff = "\
Index: /a/one.rs
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1 +1 @@
-old
+new";
        let (header, _, _) = format_tool_output("edit", diff);
        assert_eq!(header.as_deref(), Some("+1 −1"));

        let edit = panel_with(
            "edit",
            ToolStatus::Completed,
            json!({"filePath": "src/main.rs"}),
            json!({"diff": diff}),
            Some("Edit applied successfully."),
        );
        assert_eq!(panel_title(edit.clone()), "✅ edit · 📄 main.rs");
        assert!(
            panel_md(edit).contains("**Output**\n\n+1 −1"),
            "the body keeps today's bare header"
        );

        let patch = panel_with(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch"}),
            json!({"diff": diff}),
            Some("Success. Updated the following files"),
        );
        assert_eq!(
            panel_title(patch.clone()),
            "✅ patch",
            "a one-file patch never gained a detail"
        );
        assert!(panel_md(patch).contains("**Output**\n\n+1 −1"));
    }

    /// A patch that touches two languages has no honest grammar to fence with:
    /// it stays plain rather than colouring one file's hunks with the other's.
    #[test]
    fn a_mixed_language_patch_stays_plain() {
        let diff = "\
Index: /a/one.rs
===================================================================
--- /a/one.rs
+++ /a/one.rs
@@ -1 +1 @@
-old
+new
Index: /a/two.md
===================================================================
--- /a/two.md
+++ /a/two.md
@@ -1 +1 @@
-old
+new";
        let (_, style, _) = format_tool_output("apply_patch", diff);
        assert_eq!(style, BodyStyle::Code(Some(PLAIN_TEXT)));
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
        let tool = ToolPanel::for_test(
            "read",
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
        let tool = ToolPanel::for_test(
            "todowrite",
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
            title, "📋 todowrite · 共 4 项 · 🔄 1 · ⬜ 1 · ✅ 1 · 🚫 1",
            "the folded header must carry the size and status counts"
        );
        assert!(
            md.starts_with("- ✅ ~~调研~~"),
            "the checklist is the whole body: {md}"
        );
        assert!(!md.contains("**Input**"), "no Input placeholder: {md}");
        assert!(!md.contains("**Output**"), "no Output marker: {md}");
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
        let tool = ToolPanel::for_test(
            "todowrite",
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
        assert_eq!(title, "📋 todowrite", "no counts before the list exists");
        assert!(md.contains("**Input**\n📋 共 2 项任务"), "plan size: {md}");
    }

    /// A todo item long enough to trip the generic long-line rule must stay a
    /// markdown row: fencing it would show the literal `- ✅ …` syntax.
    #[test]
    fn todowrite_long_item_stays_markdown_not_fenced() {
        let content = format!("修复 {}", "很长".repeat(80));
        let tool = ToolPanel::for_test(
            "todowrite",
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

    /// An unparseable `todowrite` output — a running call's placeholder, an
    /// error, a truncated dump, or a list with one malformed row — passes
    /// through unchanged rather than being dropped, and so does an empty list.
    #[test]
    fn todowrite_output_without_a_todo_array_passes_through() {
        for raw in [
            "0 todos",
            "❌ permission denied",
            "[]",
            "",
            r#"[{"content":"ok","status":"pending"},{"status":"pending"}]"#,
        ] {
            let (header, style, body) = format_tool_output("todowrite", raw);
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
    /// line above it, `**Output**` included, into one glued heading
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
            md.contains("**Output**\n\n```\n"),
            "the marker must own a paragraph and the body be fenced: {md}"
        );
        assert!(
            !md.contains("**Output**99-"),
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
            md.contains("**Output**\n\n    Checking colark"),
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
            let (_, style, _) = format_tool_output("read", &output);
            assert_eq!(
                style,
                BodyStyle::Code(Some(token)),
                "`{path}` must fence as `{token}`"
            );
        }
    }

    #[test]
    fn tool_input_bash_shows_command_and_workdir() {
        let tool = ToolPanel::for_test(
            "bash",
            ToolStatus::Completed,
            Some(json!({"command": "cargo test --all", "workdir": "/proj"})),
            None,
        );
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(text.contains("cargo test --all"), "command missing: {}", text);
        assert!(text.contains("/proj"), "workdir missing: {}", text);
        assert!(!text.contains("workdir"), "raw key leaked: {}", text);
        assert!(!text.contains("command"), "raw key leaked: {}", text);
    }

    #[test]
    fn tool_input_edit_shows_only_the_file() {
        // The oldString/newString preview (the first line of each, both marked
        // -/+) read as duplicated content and hid the real change, which now
        // comes from the tool's diff output. The input shows the target file
        // and nothing else.
        let tool = ToolPanel::for_test(
            "edit",
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
        let tool = ToolPanel::for_test(
            "edit",
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
            text.contains("📄 `/root/.config/opencode/AGENTS.md`"),
            "file missing: {}",
            text
        );
        assert!(!text.contains("📄 `?`"), "placeholder shown: {}", text);
    }

    #[test]
    fn tool_input_read_shows_path_and_limits() {
        let tool = ToolPanel::for_test(
            "read",
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
        let tool = ToolPanel::for_test(
            "grep",
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
        let tool = ToolPanel::for_test(
            "glob",
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
        assert_eq!(format_tool_input("question", &single), "❓ 1 个问题 · 继续?");
        assert_eq!(
            format_tool_input("question", &input),
            "❓ 2 个问题 · issue 202 你想让我做什么？"
        );
    }

    /// Regression: a tool whose input renders as markdown LIST lines (the
    /// generic fallback `- name: ...`, a skill's metadata list) must NOT swallow
    /// the Output marker as a lazy continuation of the last list line — Feishu
    /// then glues `**Output**` to the end of the input. A blank line between
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
        // a single newline would make `**Output**` a lazy continuation of the
        // `- skill: m15` list item and Feishu would render them on one line.
        assert!(
            md.contains("- skill: m15\n\n**Output**\n"),
            "Input and Output sections must be separated by a blank line: {:?}",
            md
        );
        assert!(
            !md.contains("- skill: m15\n**Output**"),
            "no glued marker: {:?}",
            md
        );
    }

    #[test]
    fn tool_input_string_shows_as_is() {
        // A bare string input (non-object) renders directly.
        let tool = ToolPanel::for_test("read", ToolStatus::Completed, Some(json!("src/main.rs")), None);
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
        let tool = panel_with(
            "patch",
            ToolStatus::Completed,
            json!({"patchText": "*** Begin Patch\n*** Update File: /a/one.rs\n@@\n-let b = 2;\n+let b = 3;\n*** End Patch"}),
            json!({"files": [{"file": "/a/one.rs", "patch": patch, "status": "modified",
                              "additions": 1, "deletions": 1}]}),
            Some("Success. Updated the following files:\nM /a/one.rs"),
        );
        let md = panel_md(tool);
        assert!(md.contains("**Input**\n📄 `/a/one.rs`"), "file in input: {md}");
        assert!(md.contains("**Output**\n\n+1 −1"), "count header: {md}");
        assert!(md.contains("```rust\nIndex: /a/one.rs"), "fenced hunks: {md}");
        assert!(
            md.contains("-let b = 2;") && md.contains("+let b = 3;"),
            "hunks: {md}"
        );
        assert!(md.contains(" let a = 1;"), "context kept: {md}");
        assert!(!md.contains("Success. Updated"), "summary dropped: {md}");

        // Several files: the input names the first and counts the rest.
        assert_eq!(
            format_tool_input(
                "patch",
                &json!({"patchText": "*** Begin Patch\n*** Add File: a.rs\n*** Delete File: b.rs\n*** End Patch"})
            ),
            "📄 `a.rs` 等 2 个文件"
        );
        // An unparseable patchText falls back to the generic key-value list,
        // never a raw JSON dump.
        assert_eq!(
            format_tool_input("patch", &json!({"patchText": "not a patch"})),
            "- patchText: not a patch"
        );
        // V1's `apply_patch` keeps its historical generic rendering even when
        // its patchText carries the same `*** … File:` headers (#467 US18:
        // every V1 rendering change is out of scope). Only the V2 id takes the
        // file-summary arm.
        assert_eq!(
            format_tool_input(
                "apply_patch",
                &json!({"patchText": "*** Begin Patch\n*** Update File: a.rs\n*** End Patch"})
            ),
            "- patchText: *** Begin Patch"
        );
    }

    /// V2 spells the subagent tool `subagent` (V1: `task`) and names the agent
    /// type `agent` (V1: `subagent_type`); the panel renders the same one-line
    /// summary for both.
    #[test]
    fn subagent_input_shows_the_agent_and_description() {
        assert_eq!(
            format_tool_input(
                "subagent",
                &json!({"agent": "general", "description": "review the diff", "prompt": "…", "background": true})
            ),
            "🔀 review the diff\n🤖 `general`"
        );
        assert_eq!(
            format_tool_input("task", &json!({"description": "x", "subagent_type": "build"})),
            "🔀 x\n🤖 `build`"
        );
        assert_eq!(
            format_tool_input("subagent", &json!({"agent": "explore"})),
            "🔀 子任务\n🤖 `explore`"
        );
    }

    /// V2's subagent output is a `<subagent sessionID state>` envelope with the
    /// report as its whole body (no `<summary>`/`<task_result>` inner tags like
    /// V1's `<task>`). The wrapper must never reach the card; a background
    /// handle is plain text and stays unchanged.
    #[test]
    fn subagent_v2_output_strips_the_envelope() {
        let raw = "<subagent sessionID=\"ses_child\" state=\"completed\">\n\
                   Report line 1\n\nReport line 2\n</subagent>";
        let (header, style, body) = format_tool_output("subagent", raw);
        assert_eq!(header, None);
        assert_eq!(style, BodyStyle::Auto);
        assert!(body.starts_with("Report line 1"), "report kept: {body:?}");
        assert!(body.contains("Report line 2"), "report kept: {body:?}");
        assert!(!body.contains("<subagent"), "wrapper stripped: {body:?}");
        assert!(!body.contains("</subagent>"), "wrapper stripped: {body:?}");

        let handle = "The subagent is working in the background (sessionID: ses_x).";
        assert_eq!(format_tool_output("subagent", handle).2, handle);
    }

    /// V2's `skill` input is `{id}` where V1's was `{name}`; both render the
    /// skill's name, and V2's output envelope (`<skill_content>`) is unchanged
    /// and already handled.
    #[test]
    fn skill_input_reads_v2_id_and_v1_name() {
        assert_eq!(
            format_tool_input("skill", &json!({"id": "implement"})),
            "🧩 implement"
        );
        assert_eq!(
            format_tool_input("skill", &json!({"name": "caveman"})),
            "🧩 caveman"
        );
    }

    /// A `websearch` input had no V1 arm either: the query leads the panel.
    #[test]
    fn websearch_input_shows_the_query() {
        assert_eq!(
            format_tool_input("websearch", &json!({"query": "rust lifetimes"})),
            "🔎 `rust lifetimes`"
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
        let (header, style, body) = format_tool_output("read", file);
        assert_eq!(style, BodyStyle::Code(Some("rust")), "language from .rs");
        let header = header.unwrap_or_default();
        assert!(header.contains("/x/y.rs"), "path in header: {header}");
        assert!(!body.contains("Read file"), "header line promoted: {body}");
        assert!(body.contains("1: use std::fs;"), "code kept: {body}");

        let dir = "Read directory /x/src, entries 1-2\n/x/src/a.rs\n/x/src/b.rs";
        let (header, style, body) = format_tool_output("read", dir);
        let header = header.unwrap_or_default();
        assert!(header.contains("/x/src"), "dir path: {header}");
        assert_eq!(style, BodyStyle::Code(None));
        assert!(body.contains("/x/src/a.rs"), "listing kept: {body}");

        let empty = "Read file /x/empty.txt, 0 lines";
        let (header, _, body) = format_tool_output("read", empty);
        assert!(header.unwrap_or_default().contains("/x/empty.txt"));
        assert!(body.is_empty(), "empty file has no body: {body:?}");

        // A card-level check: the read header and fence reach the panel.
        let tool = ToolPanel::for_test(
            "read",
            ToolStatus::Completed,
            Some(json!({"path": "/x/y.rs"})),
            Some(file),
        );
        let md = panel_md(tool);
        assert!(md.contains("**Output**\n\n📄 `/x/y.rs`"), "read header: {md}");
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
        let (header, style, body) = format_tool_output("websearch", raw);
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
        assert_eq!(
            format_tool_output("websearch", "No search results found.").0,
            None
        );
    }

    /// V2 records a shell command's outcome in the call's metadata; a settled
    /// call that exited non-zero or timed out renders the failure icon even
    /// though the protocol's status is `completed` (the same derivation the
    /// official 2.0.x client uses). The typed liveness stays status-based.
    #[test]
    fn a_settled_shell_reports_metadata_failures() {
        let with = |metadata: serde_json::Value| {
            panel_with(
                "shell",
                ToolStatus::Completed,
                json!({"command": "false"}),
                metadata,
                Some(""),
            )
        };
        assert_eq!(
            panel_title(with(json!({"status": "completed", "exit": 1}))),
            "❌ shell"
        );
        assert_eq!(
            panel_title(with(json!({"status": "completed", "exit": 0}))),
            "✅ shell"
        );
        assert_eq!(
            panel_title(with(json!({"status": "completed", "timeout": true}))),
            "❌ shell"
        );
        // A backgrounded launch keeps its 🌙 marker: the run is the ledger's.
        assert_eq!(
            panel_title(with(json!({"status": "running", "shellID": "sh_1"}))),
            "🌙 shell"
        );
        // The icon is presentation only: the call is settled either way.
        assert!(!with(json!({"status": "completed", "exit": 1})).is_live());
    }

    /// V2 records the result count and truncation in the call's metadata; the
    /// panel's output header carries them. A call without the metadata (a
    /// third-party tool claiming the id) keeps the raw shape.
    #[test]
    fn glob_and_grep_headers_carry_the_result_counts() {
        let glob = panel_with(
            "glob",
            ToolStatus::Completed,
            json!({"pattern": "**/*.rs"}),
            json!({"count": 3, "truncated": false}),
            Some("a.rs\nb.rs\nc.rs"),
        );
        let md = panel_md(glob);
        assert!(md.contains("**Output**\n\n3 个文件"), "glob count: {md}");

        let truncated = panel_with(
            "glob",
            ToolStatus::Completed,
            json!({"pattern": "**/*.rs"}),
            json!({"count": 100, "truncated": true}),
            Some("a.rs"),
        );
        let md = panel_md(truncated);
        assert!(md.contains("100 个文件 · 已截断"), "truncation noted: {md}");

        let grep = panel_with(
            "grep",
            ToolStatus::Completed,
            json!({"pattern": "fn main"}),
            json!({"matches": 2, "truncated": false}),
            Some("Found 2 matches\nsrc/a.rs:\n  Line 1: fn main() {}"),
        );
        let md = panel_md(grep);
        assert!(md.contains("**Output**\n\n2 处匹配"), "grep count: {md}");

        assert_eq!(format_tool_output("glob", "a.rs\nb.rs").0, None);
    }

    /// V2's Code Mode entry (`execute`): the script renders as a fenced
    /// JavaScript block and the nested calls the program made render as one
    /// status row each from the call's metadata (present while it runs). A
    /// completed program whose metadata reports an error — its own or a nested
    /// call's — shows the failure icon, and the folded title carries the call
    /// count.
    #[test]
    fn execute_panel_renders_the_script_and_its_nested_calls() {
        let tool = panel_with(
            "execute",
            ToolStatus::Completed,
            json!({"code": "const r = await tools.webfetch({ url: \"https://a.example\" });\nreturn r.output;"}),
            json!({"toolCalls": [
                {"tool": "webfetch", "status": "completed", "input": {"url": "https://a.example"}},
                {"tool": "opencode_models", "status": "error"}
            ]}),
            Some("boom"),
        );
        assert_eq!(panel_title(tool.clone()), "❌ execute · 2 个工具调用 · 1 失败");
        let md = panel_md(tool);
        assert!(md.contains("**Input**\n```javascript"), "script fenced: {md}");
        assert!(md.contains("tools.webfetch"), "script kept: {md}");
        assert!(md.contains("- ✅ `webfetch`"), "nested row: {md}");
        assert!(md.contains("- ❌ `opencode_models`"), "failed nested row: {md}");
        assert!(md.contains("boom"), "result kept: {md}");

        // A successful program: ✅ and no failure count.
        let ok = panel_with(
            "execute",
            ToolStatus::Completed,
            json!({"code": "return 1"}),
            json!({"toolCalls": [{"tool": "fetch", "status": "completed"}]}),
            Some("1"),
        );
        assert_eq!(panel_title(ok), "✅ execute · 1 个工具调用");

        // A live program with no output yet still lists its calls.
        let live = panel_with(
            "execute",
            ToolStatus::Running,
            json!({"code": "await tools.webfetch({ url: \"https://a.example\" })"}),
            json!({"toolCalls": [{"tool": "fetch", "status": "running", "input": {"url": "https://a.example"}}]}),
            None,
        );
        assert!(panel_title(live.clone()).starts_with("⏳ execute · 1 个工具调用"));
        let md = panel_md(live);
        assert!(md.contains("- ⏳ `fetch`"), "live nested row: {md}");

        // A same-named foreign tool without the Code Mode input shape stays on
        // the opaque path: no fenced JavaScript, no call-count title.
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

    /// The input limit hint carries the tool's own unit: `read` pages lines,
    /// `glob` counts files.
    #[test]
    fn input_limit_hints_match_the_tool() {
        assert!(
            format_tool_input("glob", &json!({"pattern": "**/*.rs", "limit": 5})).contains("最多 5 个文件")
        );
        assert!(format_tool_input("read", &json!({"path": "a.rs", "limit": 5})).contains("最多 5 行"));
        assert!(format_tool_input("grep", &json!({"pattern": "x", "limit": 5})).contains("最多 5 行"));
    }
}
