use crate::backend::{ContentBlock, ToolCall, ToolStatus};

use super::sanitize::CardMarkdown;
use super::shell::{collapsible_panel, panel_time_suffix};
use super::{fenced_code, truncate_md};

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
}

impl ToolPanel {
    pub fn new(call: ToolCall) -> Self {
        Self { call, liveness: None }
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

    /// The raw structured tool input (what OpenCode recorded for the call), kept
    /// as JSON so the panel can render it human-friendly per tool type instead
    /// of dumping a raw JSON blob.
    pub fn input(&self) -> Option<&serde_json::Value> {
        self.call.input.as_ref()
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
        if self.call.identity.name == "edit" || self.call.identity.name == "apply_patch" {
            edit_tool_output(&self.call)
        } else {
            tool_output(&self.call)
        }
    }

    /// The child-session liveness gathered for a live `task` call (ADR-0054).
    pub fn liveness(&self) -> Option<&TaskLiveness> {
        self.liveness.as_ref()
    }

    /// Attach or clear the gathered liveness (the accumulator's refresh path;
    /// `None` clears a stale line once the call settles).
    pub(crate) fn set_liveness(&mut self, liveness: Option<TaskLiveness>) {
        self.liveness = liveness;
    }

    /// The child Session a `task` call runs — `state.metadata.sessionId`, the
    /// camelCase field the event contract carries (AGENTS.md #2). `None` for
    /// any other tool, or when the payload recorded no session id.
    pub(crate) fn child_session_id(&self) -> Option<&str> {
        if self.call.identity.name != "task" {
            return None;
        }
        self.call
            .metadata
            .as_ref()?
            .get("sessionId")
            .and_then(|value| value.as_str())
    }
}

/// A live `task` call's child-session liveness (ADR-0054): what the child is
/// doing right now, as the panel title shows it. Display-only data — the Bridge
/// gathers it, the Platform formats it — so the panel stays a view over the
/// call plus one read-only line.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLiveness {
    /// Age of the child's newest activity, measured when gathered.
    pub last_activity_ago_secs: u64,
    /// The child's newest still-running tool, when it has one.
    pub current_tool: Option<String>,
    /// The child's pending Permission/Question, when it waits on one.
    pub wait: Option<WaitState>,
}

/// Which user-facing wait currently blocks a child session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitState {
    Permission,
    Question,
    Both,
}

impl WaitState {
    /// The card's existing wait vocabulary, as the liveness line reuses it.
    pub fn label(self) -> &'static str {
        match self {
            WaitState::Permission => "等待授权",
            WaitState::Question => "等待回答",
            WaitState::Both => "等待授权/回答",
        }
    }
}

impl TaskLiveness {
    /// The title fragment a live task panel appends: the activity age, the
    /// current tool and the wait, each only when known.
    pub fn title_fragment(&self) -> String {
        let mut parts = vec![format!("{} 前", age_label(self.last_activity_ago_secs))];
        if let Some(tool) = &self.current_tool {
            parts.push(tool.clone());
        }
        if let Some(wait) = self.wait {
            parts.push(wait.label().to_string());
        }
        parts.join(" · ")
    }
}

/// A coarse age label (`12s`, `3m`, `1h`): the liveness line needs a
/// glanceable magnitude, not precision.
fn age_label(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
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
/// recorded in the call's raw metadata (`createTwoFilesPatch`) over the tool's
/// plain text output ("Edit applied successfully." / "Success. Updated the
/// following files: …"), which tells the reader nothing about what changed.
/// Failures keep their extracted error text.
fn edit_tool_output(call: &ToolCall) -> Option<String> {
    let orig = tool_output(call);
    if call.status == ToolStatus::Error {
        return orig;
    }
    let diff = call
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("diff"))
        .and_then(serde_json::Value::as_str)
        .filter(|diff| !diff.is_empty())?;
    // The success sentence (and, for apply_patch, the A/M/D file summary after
    // it) is noise once the diff is shown; anything beyond it (e.g. an "LSP
    // errors detected" note) is kept as a tail after the diff.
    let tail = orig
        .as_deref()
        .and_then(|output| match call.identity.name.as_str() {
            "apply_patch" => strip_patch_summary(output),
            _ => output.strip_prefix("Edit applied successfully."),
        })
        .map(|s| s.trim_start_matches('\n'))
        .filter(|s| !s.is_empty());
    Some(match tail {
        Some(t) => format!("{diff}\n\n{t}"),
        None => diff.to_string(),
    })
}

/// Drop `apply_patch`'s success summary — `Success. Updated the following
/// files:` plus its `A`/`M`/`D` path lines — and return what follows (the LSP
/// note blocks, separated by a blank line), or `None` when nothing follows.
fn strip_patch_summary(output: &str) -> Option<&str> {
    output
        .strip_prefix("Success. Updated the following files:")
        .and_then(|rest| rest.split_once("\n\n").map(|(_, tail)| tail))
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
    let output = tool.output().map(|raw| format_tool_output(name, &raw));
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
            // Trailing blank line so a multi-line input (edit diff, skill
            // metadata list) can't swallow the Output section as a markdown
            // lazy continuation of its last list line — Feishu would render
            // `**Output**` glued to the last input line.
            content.push_str(&format!("**Input**\n{}\n\n", truncate_md(&formatted, 400)));
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
            if let Some(h) = &header {
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
        title.push_str(&format!(" · {}", liveness.title_fragment()));
    }
    collapsible_panel(&title, &content, element_id)
}

/// The meaningful parts of an `edit` tool's unified diff (recorded by OpenCode
/// in `state.metadata.diff` / an edit permission request's `diff`): the target
/// path and the hunk lines (`@@`, `-`, `+`) plus the counted change. The
/// `Index:`, `===`, `---`, `+++` header noise and unchanged context lines
/// (starting with a space) are dropped — oldString/newString are often
/// near-identical full-block snapshots whose context would otherwise render as
/// walls of repeated text around a few changed lines.
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
/// and the numbered lines; a `task`/`skill` output is an XML envelope around
/// the content the reader wants. An `edit` or `apply_patch` output is
/// substituted by its unified diff (`apply_patch` keeps each file's `Index:`
/// line) — keep only the hunks and report the change count. A `todowrite`
/// output is its todo list JSON-encoded — render it as a status checklist; a
/// `websearch` output is its JSON result envelope — render it as a title+url
/// list. Other tools pass through unchanged.
///
/// Returns `(header, body style, body)`: the header is the short markdown line
/// above the body (file path, change count, result count); the style decides
/// how the body blocks (see [`BodyStyle`]). A `todowrite` header is the list's
/// size and per-status counts — the folded panel's progress line.
fn format_tool_output(name: &str, output: &str) -> (Option<String>, BodyStyle, String) {
    if name == "edit" || name == "apply_patch" {
        let parsed = if name == "apply_patch" {
            parse_multi_file_diff(output)
        } else {
            parse_edit_diff(output)
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
    if name == "websearch"
        && let Some((count, body)) = parse_websearch_results(output)
    {
        return (Some(format!("🔎 {} 条结果", count)), BodyStyle::Markdown, body);
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

/// Strip a `skill` tool's XML envelope: `<skill_content name=…>` around the
/// skill's own markdown, dropping the sampled `<skill_files>` inventory (a
/// file list the reader can't use). `None` when the wrapper isn't there, so the
/// caller shows the output unchanged.
fn parse_skill_envelope(output: &str) -> Option<String> {
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

/// Parse a `websearch` tool's JSON result envelope into an un-fenced markdown
/// list: one `[title](url)` per result, with its publish date when present.
/// Excerpts are deliberately dropped — they are the model's reading material,
/// not the card's. `None` when the payload isn't the expected envelope (the
/// provider's no-results text, a different provider's shape), so the caller
/// shows it unchanged.
fn parse_websearch_results(output: &str) -> Option<(usize, String)> {
    let results = parse_parallel_results(output).or_else(|| parse_exa_results(output))?;
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
            format!("📄 `{}`", get("filePath").unwrap_or("?"))
        }
        "read" | "write" | "glob" | "grep" => {
            let mut parts = Vec::new();
            // `pattern` is the search key for glob/grep — show it as "匹配",
            // not twice. File paths render as plain paths.
            if name == "grep" || name == "glob" {
                if let Some(v) = get("pattern") {
                    parts.push(format!("匹配 `{}`", v));
                }
                for k in ["filePath", "path"] {
                    if let Some(v) = get(k) {
                        parts.push(format!("`{}`", v));
                    }
                }
            } else {
                for k in ["filePath", "path"] {
                    if let Some(v) = get(k) {
                        parts.push(format!("`{}`", v));
                    }
                }
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
                s.push_str(&format!("\n最多 {} 行", limit));
            }
            if s.len() <= 1 { input.to_string() } else { s }
        }
        "webfetch" => {
            if let Some(url) = get("url") {
                format!("🌐 `{}`", url)
            } else {
                input.to_string()
            }
        }
        "task" => {
            let desc = get("description").unwrap_or("子任务");
            let sub = get("subagent_type")
                .map(|s| format!("\n🤖 `{}`", s))
                .unwrap_or_default();
            format!("🔀 {}{}", desc, sub)
        }
        "skill" => match get("name") {
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
        _ => {
            // Generic: one `- key: value` line per scalar field.
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
                input.to_string()
            } else {
                lines.join("\n")
            }
        }
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
    use crate::backend::{ToolIdentity, ToolOutput};
    use crate::feishu::card::CardState;
    use crate::feishu::card::shell::CardBuilder;
    use serde_json::json;

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
            last_activity_ago_secs: 12,
            current_tool: Some("bash".into()),
            wait: Some(WaitState::Permission),
        }));
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_tool(tool)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements[0]["header"]["title"]["content"].as_str().unwrap(),
            "⏳ task · 12s 前 · bash · 等待授权"
        );
    }

    /// The liveness line is live-only: a settled panel drops it even if a
    /// stale snapshot was never cleared.
    #[test]
    fn settled_task_panel_drops_child_liveness() {
        let mut tool = ToolPanel::for_test("task", ToolStatus::Completed, None, Some("done"));
        tool.set_liveness(Some(TaskLiveness {
            last_activity_ago_secs: 12,
            current_tool: None,
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

    /// The fragment composes only the parts that are known; the age alone is
    /// the floor, and the wait reuses the card's wait vocabulary.
    #[test]
    fn liveness_fragment_composes_known_parts() {
        let liveness = |ago, tool: Option<&str>, wait| TaskLiveness {
            last_activity_ago_secs: ago,
            current_tool: tool.map(str::to_string),
            wait,
        };
        assert_eq!(liveness(5, None, None).title_fragment(), "5s 前");
        assert_eq!(liveness(90, Some("read"), None).title_fragment(), "1m 前 · read");
        assert_eq!(
            liveness(7200, Some("bash"), Some(WaitState::Both)).title_fragment(),
            "2h 前 · bash · 等待授权/回答"
        );
    }

    /// The child session id is read from a task call's metadata only
    /// (`state.metadata.sessionId`); every other tool and a metadata-less task
    /// call yield nothing.
    #[test]
    fn task_child_session_id_reads_metadata_session_id() {
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
}
