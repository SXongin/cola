use crate::backend::{ContentBlock, FileContent, ToolCall, ToolStatus};

use super::sanitize::CardMarkdown;
use super::shell::{collapsible_panel, fmt_elapsed, panel_time_suffix};
use super::{AwaitingAction, fenced_code, truncate_md};

/// Cap for a tool's OUTPUT shown inside its collapsible panel. Higher than the
/// old 800: code-wrapped output renders without line wrapping, so a file or
/// command output can be meaningfully long.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 3000;

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
/// anything. Ticket #649 extends the enum with the File Message states
/// (`已发送为文件消息` / `未发送`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDelivery {
    /// The render path has not resolved this content yet. A card built before
    /// the pre-resolve runs (a unit fixture) shows just the tracking line.
    Unresolved,
    /// An image within Feishu's caps, uploaded once and embedded in the card
    /// immediately after its Tool Panel: the reusable `image_key` the `img`
    /// element references. The panel line reads `· 已内嵌`.
    Embedded { image_key: String },
    /// Considered with no card surface in this build: a non-image, or an image
    /// past Feishu's caps. Renders the bare tracking line. Ticket #649 replaces
    /// this with its File Message deliver-or-not states.
    NoSurface,
}

impl FileDelivery {
    /// The reusable `image_key` when this content is embedded in the card.
    fn as_embedded_key(&self) -> Option<&str> {
        match self {
            FileDelivery::Embedded { image_key } => Some(image_key),
            FileDelivery::Unresolved | FileDelivery::NoSurface => None,
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
        if self.call.identity.name == "edit"
            || self.call.identity.name == "apply_patch"
            || self.call.identity.name == "patch"
        {
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
    /// field each generation's contract carries; AGENTS.md #2). `None` for any
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
    if call.status == ToolStatus::Error
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
/// it: V1 stores one plain string at `metadata.diff`; V2 stores
/// `metadata.files[]`, one `{file, patch, …}` per touched file, whose patches
/// join here in file order. Each patch keeps its own `Index:` header —
/// `apply_patch`'s multi-file parser reads those for per-file attribution.
fn recorded_diff(call: &ToolCall) -> Option<String> {
    let metadata = call.metadata.as_ref()?;
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
/// carries no File Content. Extensible: ticket #649 adds the File Message
/// states to the delivery arm.
fn file_tracking_block(tool: &ToolPanel) -> Option<String> {
    if tool.files().is_empty() {
        return None;
    }
    Some(
        tool.files()
            .iter()
            .map(|file| match &file.delivery {
                FileDelivery::Embedded { .. } => format!("{} · 已内嵌", file.content.record_line()),
                FileDelivery::Unresolved | FileDelivery::NoSurface => file.content.record_line(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// The `img` elements this panel renders (ADR-0076): one per embedded File
/// Content, all pushed immediately AFTER the panel so the image is visible and
/// its adjacency to the panel gives the correspondence. `title` is
/// `📎 <name>`; `preview` is on so a click enlarges it. A content with no
/// embed renders nothing here.
pub(super) fn file_image_elements(tool: &ToolPanel) -> Vec<serde_json::Value> {
    tool.files()
        .iter()
        .filter_map(|file| {
            file.delivery.as_embedded_key().map(|image_key| {
                serde_json::json!({
                    "tag": "img",
                    "img_key": image_key,
                    "alt": { "tag": "plain_text", "content": file.content.name },
                    "title": { "tag": "plain_text", "content": format!("📎 {}", file.content.name) },
                    "preview": true,
                })
            })
        })
        .collect()
}

/// One tool panel as a folded collapsible element. `at_ms` is the part's
/// server `state.time.start`; when present, the panel header carries it as
/// `· HH:MM` so the start time is visible while collapsed (#183), and when
/// absent (a payload with no server time) no clock is rendered. `element_id`
/// is the panel's stable identity on the card (see
/// [`super::shell::collapsible_panel_chunks`]).
pub(super) fn tool_panel_element(
    tool: &ToolPanel,
    at_ms: Option<i64>,
    element_id: Option<&str>,
    md: &mut CardMarkdown,
) -> serde_json::Value {
    // One name read for the panel's name-keyed decisions: the output
    // formatter's dispatch, the todo section's detection, and the input
    // formatter's dispatch.
    let name = tool.name();
    // `execute` assembles its body from the call's nested-call metadata as well
    // as its text output, so it has its own output builder; every other tool's
    // body is its output text through the per-tool formatter. The result-count
    // header (glob/grep) is metadata-side, so it is applied over either path.
    let output = if tool.is_code_mode() {
        execute_output(tool)
    } else {
        tool.output().map(|raw| format_tool_output(name, &raw))
    };
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
            let body = truncate_md(&body, TOOL_OUTPUT_MAX_CHARS);
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
    collapsible_panel(&title, &content, element_id)
}

/// The meaningful parts of a unified file diff (a file-editing tool's recorded
/// diff, or an edit permission request's `diff`): the target path and the hunk
/// lines (`@@`, `-`, `+`) plus the counted change. The `Index:`, `===`, `---`,
/// `+++` header noise and unchanged context lines (starting with a space) are
/// dropped — oldString/newString are often near-identical full-block snapshots
/// whose context would otherwise render as walls of repeated text around a few
/// changed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditDiff {
    pub path: Option<String>,
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

fn parse_diff(diff: &str, keep_file_headers: bool) -> Option<EditDiff> {
    let mut path = None;
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
    // Once a non-diff line shows up (the bridge appended the tool's LSP note
    // after the diff), everything after it is that note — even a line that
    // happens to start like diff syntax (a diagnostic quoting `+`/`-` code).
    let mut in_tail = false;
    for line in diff.lines() {
        let l = line.trim_end();
        if in_tail {
            tail.push_str(l);
            tail.push('\n');
            continue;
        }
        if let Some(p) = l.strip_prefix("Index: ") {
            path = Some(p.to_string());
            // A new `Index:` starts a new file's header block even after an
            // earlier file's hunks (a concatenated multi-file patch), so its
            // `---`/`+++` lines are headers again, not content.
            in_hunk = false;
            if keep_file_headers {
                body.push_str(l);
                body.push('\n');
            }
        } else if !in_hunk && l.starts_with("diff --git ") {
            // `a/old b/new` — the explicit Index/filepath carries the target.
        } else if !in_hunk
            && (l.starts_with("--- ")
                || l.starts_with("+++ ")
                || l == "---"
                || l == "+++"
                || l.starts_with("==="))
        {
            // The old/new file headers of a unified diff (`--- path` / `+++ path`)
            // plus the `====` separator.
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
        } else if !l.is_empty() && !l.starts_with(' ') && !l.starts_with('\\') {
            // Not diff syntax and not an (indented) context line: the start of
            // the trailing note. Blank lines (the separator) and the
            // `\ No newline…` marker are not it.
            in_tail = true;
            tail.push_str(l);
            tail.push('\n');
        }
        // Anything else (context lines, "\ No newline…") is dropped.
    }
    if !changed {
        return None;
    }
    Some(EditDiff {
        path,
        additions,
        deletions,
        body,
        tail,
    })
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

/// Render a tool's raw output string human-friendly. OpenCode's `read` tool
/// wraps its output in XML tags (`<path>…</path>`, `<type>…</type>`,
/// `<content>…</content>`) — strip them so the card shows just the file path
/// and the numbered lines; a `task`/`subagent` output is an XML envelope around
/// the content the reader wants. An `edit`, `apply_patch` or `patch` output is
/// substituted by its unified diff (`apply_patch`/`patch` keep each file's
/// `Index:` line) — keep only the hunks and report the change count. A
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
/// size and per-status counts — the folded panel's progress line.
fn format_tool_output(name: &str, output: &str) -> (Option<String>, BodyStyle, String) {
    if name == "edit" || name == "apply_patch" || name == "patch" {
        let parsed = if name == "edit" {
            parse_edit_diff(output)
        } else {
            parse_multi_file_diff(output)
        };
        return match parsed {
            Some(d) => {
                // The hunks, then the tool's trailing note (LSP diagnostics) on
                // its own paragraph — inside the same code block so the panel
                // never loses it.
                let mut body = d.body;
                if !d.tail.is_empty() {
                    body.push('\n');
                    body.push_str(d.tail.trim_end());
                    body.push('\n');
                }
                (
                    Some(format!("+{} −{}", d.additions, d.deletions)),
                    BodyStyle::Code(None),
                    body,
                )
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
    if name == "read" && !output.contains("<path>") {
        // V2's read output carries no XML wrapper: a header line over numbered
        // lines or a directory listing. Anything else falls through to raw.
        if let Some(parsed) = parse_v2_read_output(output) {
            return parsed;
        }
    }
    if name != "read" || !output.contains("<path>") {
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
            || l.trim() == "<type>directory</type>"
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
        BodyStyle::Code(code_lang_for_path(&path)),
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
        let path = head.strip_prefix("Read directory ")?;
        ("📁", path)
    };
    let style = if icon == "📄" {
        BodyStyle::Code(code_lang_for_path(path))
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

/// A language hint for a file path's extension, used as the fenced-code-block
/// language on a `read` panel. Unknown/extension-less paths get `text`.
fn code_lang_for_path(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    Some(match ext {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "jsx" => "jsx",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "ps1" => "powershell",
        "md" | "markdown" => "markdown",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" | "svg" => "xml",
        "html" | "htm" => "html",
        "css" | "scss" | "less" => "css",
        "sql" => "sql",
        "txt" | "log" => "text",
        _ => "text",
    })
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
    /// (not inside it — the panel is collapsed by default), `title`
    /// `📎 <name>`, `preview` on.
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
        assert_eq!(elements[1]["alt"]["content"], "shot.png");
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

    /// A File Content with no embed this build (a non-image, or an upload that
    /// failed) keeps the bare tracking line and emits NO `img` element.
    #[test]
    fn a_file_content_without_an_embed_renders_only_its_line() {
        let panel = read_panel("doc.pdf", "application/pdf", b"%PDF", FileDelivery::NoSurface);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(panel)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 1, "no image element: {card}");
        let body = elements[0]["elements"][0]["content"].as_str().unwrap();
        assert!(body.contains("📎 doc.pdf · application/pdf · 4 B"), "{body}");
        assert!(!body.contains("已内嵌"), "{body}");
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
            .with_tool_at(tool.clone(), Some(at), None)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[0]["header"]["title"]["content"].as_str().unwrap(),
            "✅ bash · 14:05"
        );

        let untimed = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool_at(tool, None, None)
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
    fn edit_tool_output_reduces_diff_to_hunks() {
        // A real OpenCode edit diff (from state.metadata.diff). The card should
        // keep the @@ / + / - lines with the change count and drop the header
        // noise plus the unchanged context, so a small edit in a large block
        // doesn't read as two walls of repeated file content.
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
        assert_eq!(style, BodyStyle::Code(None), "edit hunks are fenced");
        assert!(body.contains("@@ -10,3 +10,3 @@"), "hunk header kept: {}", body);
        assert!(body.contains("-let b = 2;"), "removed line kept: {}", body);
        assert!(body.contains("+let b = 3;"), "added line kept: {}", body);
        assert!(!body.contains("let a = 1;"), "context line dropped: {}", body);
        assert!(!body.contains("let c = 4;"), "context line dropped: {}", body);
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
        // Hunks render inside a fenced code block (monospace, no wrapping).
        assert!(md.contains("```\n@@ -1,4 +1,4 @@"), "fenced hunk: {}", md);
        assert!(md.contains("-fn main() {}"), "removed line: {}", md);
        assert!(md.contains("+fn main() { println"), "added line: {}", md);
        assert!(!md.contains("use std::fs;"), "context line dropped: {}", md);
        assert!(md.contains("\n```"), "closing fence: {}", md);
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
        assert!(md.contains("```\n@@ -1,6 +1,4 @@"), "fenced hunk: {md}");
        assert!(md.contains("-# English Learning Rule"), "removed line: {md}");
        assert!(md.contains("+# Interaction Rules"), "added line: {md}");
        assert!(!md.contains("(1 replacement)"), "generated summary dropped: {md}");
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
            md.contains("```\nIndex: /x/one.rs"),
            "fenced body with the patched file: {md}"
        );
        assert!(md.contains("-let b = 2;"), "removed line: {md}");
        assert!(md.contains("+let b = 3;"), "added line: {md}");
        assert!(!md.contains("let a = 1;"), "context line dropped: {md}");
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
        assert!(!d.body.contains("return 1"), "context dropped: {}", d.body);
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
        assert_eq!(header.as_deref(), Some("+2 −2"), "count header: {header:?}");
        assert_eq!(style, BodyStyle::Code(None), "hunks are fenced");
        assert!(body.contains("Index: /a/one.rs"), "first file named: {body}");
        assert!(body.contains("Index: /a/two.rs"), "second file named: {body}");
        assert!(body.contains("+let b = 3;"), "hunk kept: {body}");
        assert!(body.contains("+y"), "second hunk kept: {body}");
        assert!(!body.contains("let a = 1;"), "context dropped: {body}");
        assert!(!body.contains("+++"), "file header noise dropped: {body}");
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

    #[test]
    fn code_lang_for_path_maps_common_extensions() {
        assert_eq!(code_lang_for_path("src/main.rs"), Some("rust"));
        assert_eq!(code_lang_for_path("x.py"), Some("python"));
        assert_eq!(code_lang_for_path("f.sh"), Some("bash"));
        assert_eq!(code_lang_for_path("noext"), None);
        assert_eq!(code_lang_for_path("f.unknownext"), Some("text"));
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
        assert!(md.contains("```\nIndex: /a/one.rs"), "fenced hunks: {md}");
        assert!(
            md.contains("-let b = 2;") && md.contains("+let b = 3;"),
            "hunks: {md}"
        );
        assert!(!md.contains("let a = 1;"), "context dropped: {md}");
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
