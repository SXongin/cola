use serde_json::json;

/// Card state for Feishu interactive message cards.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CardState {
    #[default]
    Loading,
    Reasoning,
    Streaming,
    /// A card that was finalized mid-turn because the content filled the card;
    /// the rest continues in the next card. Shown as "部分完成，继续中".
    Continued,
    Done,
    Error,
}

/// Feishu rejects JSON 2.0 cards with more than 200 total components/elements
/// (ErrCode 11310 "element exceeds the limit"). A single collapsible panel
/// counts as multiple components (panel + header title + icon + nested
/// markdown), so cap the number of tool panels rendered in one card.
/// A question with more options than this collapses them into a folding/// `overflow` group instead of a tall stack of buttons.
const MAX_VISIBLE_OPTIONS: usize = 3;
/// How much text ONE card carries before it is finalized and the rest continues
/// on the next card. Kept below Feishu's card limits so a card full of text
/// never overflows; long answers flow across continuation cards instead of a
/// separate plain-text message.
pub const MAX_CARD_TEXT_CHARS: usize = 6000;
/// Estimated component ceiling for a card body. Feishu rejects cards over ~200
/// components (ErrCode 11310); when a streaming card would cross this it is
/// finalized with a "to be continued" marker and a fresh continuation card is
/// sent instead.
pub const MAX_CARD_COMPONENTS: usize = 150;
/// Estimated JSON size ceiling for a card body (Feishu caps total card size,
/// not just the element count — roughly 150KB). A streaming card also splits
/// when the estimate crosses this.
pub const MAX_CARD_JSON_CHARS: usize = 100_000;
/// A single text element; bound it so a very long reply never pushes the card
/// over Feishu's total card size / element limits. Reasoning, tool input/output
/// and the question are already truncated per-element.
pub const MAX_ELEMENT_TEXT_CHARS: usize = 3000;
/// Cap for a tool's OUTPUT shown inside its collapsible panel. Higher than the
/// old 800: code-wrapped output renders without line wrapping, so a file or
/// command output can be meaningfully long.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 3000;

/// Split `text` into chunks of at most `max` chars (character-aware), keeping
/// the full content. Used to bound single card elements and timeline items.
pub(crate) fn chunk_text(text: &str, max: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let take: String = rest.chars().take(max).collect();
        chunks.push(take.clone());
        rest = &rest[take.len()..];
    }
    chunks
}

/// Builds Feishu interactive card JSON (v2 schema with collapsible panels).
/// Body elements are appended in the order the builder methods are called, so
/// text and tool panels can be interleaved chronologically.
pub struct CardBuilder {
    state: CardState,
    /// Tool panels in call order (also drives the header's running-tool hint).
    tools: Vec<ToolPanel>,
    /// Body elements, in call order.
    body: Vec<serde_json::Value>,
    footer: Option<String>,
    subtitle: Option<String>,
    /// JSON 2.0 buttons shown only on the Error card (e.g. a retry action).
    error_buttons: Vec<CardActionButton>,
}

/// A JSON 2.0 button (tag: `button`, sits directly in `body.elements`).
#[derive(Debug, Clone)]
pub struct CardActionButton {
    pub text: String,
    /// `primary` | `default` | `danger`.
    pub kind: &'static str,
    /// Callback payload delivered to cola on click.
    pub value: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ToolPanel {
    pub name: String,
    pub status: String,
    /// The raw structured tool input (what OpenCode recorded for the call), kept
    /// as JSON so the panel can render it human-friendly per tool type instead
    /// of dumping a raw JSON blob.
    pub input: Option<serde_json::Value>,
    pub output: Option<String>,
}

impl ToolPanel {
    fn status_icon(&self) -> &'static str {
        match self.status.as_str() {
            "running" | "pending" => "⏳",
            "completed" => "✅",
            // OpenCode marks failed tools as status "error" (not "failed").
            "failed" | "error" => "❌",
            _ => "🔧",
        }
    }
}

/// One tool panel as a folded collapsible element.
fn tool_panel_element(tool: &ToolPanel) -> serde_json::Value {
    let mut content = String::new();
    if let Some(i) = &tool.input {
        let formatted = format_tool_input(&tool.name, i);
        if !formatted.is_empty() {
            content.push_str(&format!("**Input**\n{}\n", truncate_md(&formatted, 400)));
        }
    }
    if let Some(ref o) = tool.output {
        let (header, lang, body) = format_tool_output(&tool.name, o);
        content.push_str("**Output**\n");
        if let Some(h) = &header {
            content.push_str(&format!("{}\n", h));
        }
        let body = truncate_md(&body, TOOL_OUTPUT_MAX_CHARS);
        // File content (read) and anything with long lines render as a fenced
        // code block: Feishu markdown wraps plain text but not code blocks, so
        // long file/command output stays on one visual line instead of folding.
        if tool.name == "read" || needs_code_block(&body) {
            content.push_str(&fenced_code(&body, lang));
        } else {
            content.push_str(&body);
        }
    }
    if content.is_empty() {
        content = "_(no details)_".to_string();
    }
    collapsible_panel(&format!("{} {}", tool.status_icon(), tool.name), &content)
}

/// Render a tool's raw output string human-friendly. OpenCode's `read` tool
/// wraps its output in XML tags (`<path>…</path>`, `<type>…</type>`,
/// `<content>…</content>`); strip them so the card shows just the file path and
/// the numbered lines instead of raw markup. Other tools pass through unchanged.
///
/// Returns `(header, language hint, body)`: the header (the file-path line) is
/// markdown; the body is shown as a code block so long lines don't wrap.
fn format_tool_output(name: &str, output: &str) -> (Option<String>, Option<&'static str>, String) {
    if name != "read" || !output.contains("<path>") {
        return (None, None, output.to_string());
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
        return (None, None, output.to_string());
    }
    (Some(format!("📄 `{}`", path)), code_lang_for_path(&path), body)
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

/// Whether `text` contains a line long enough that Feishu's markdown would wrap
/// it — the case where a fenced code block (which does NOT wrap) is the fix.
fn needs_code_block(text: &str) -> bool {
    text.lines().any(|l| l.chars().count() > 100)
}

/// Wrap `text` in a fenced code block so long lines render without wrapping.
/// The fence is sized longer than any backtick run in the content, so an
/// embedded ``` can't break out of the block.
fn fenced_code(text: &str, lang: Option<&str>) -> String {
    let max_run = text
        .chars()
        .fold((0usize, 0usize), |(best, run), c| {
            if c == '`' {
                let run = run + 1;
                (best.max(run), run)
            } else {
                (best, 0)
            }
        })
        .0;
    let fence = "`".repeat((max_run + 1).max(3));
    let head = match lang {
        Some(l) if !l.is_empty() => format!("{fence}{l}"),
        _ => fence.clone(),
    };
    format!("{head}\n{text}\n{fence}")
}

/// Render a tool's input JSON as human-readable markdown, keyed on the tool
/// name. Recognized tools get tailored one-liners (bash → the command, read →
/// the file path, edit → file + a change summary); anything else falls back to
/// a generic key-value list.
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
            let file = get("filePath").unwrap_or("?");
            let old = get("oldString").unwrap_or("");
            let new = get("newString").unwrap_or("");
            let mut s = format!("📄 `{}`", file);
            if !old.is_empty() {
                s.push_str(&format!("\n- {}", first_chunk(old, 120)));
            }
            if !new.is_empty() {
                s.push_str(&format!("\n+ {}", first_chunk(new, 120)));
            }
            s
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

impl CardBuilder {
    pub fn new() -> Self {
        Self {
            state: CardState::Loading,
            tools: Vec::new(),
            body: Vec::new(),
            footer: None,
            subtitle: None,
            error_buttons: Vec::new(),
        }
    }

    /// The session/thread name, shown as the card's subtitle so the header can
    /// stay focused on state. The card is a reply to the user's message, so the
    /// question itself is already visible in the reply context — no need to echo
    /// it again. Empty subtitles are omitted.
    pub fn with_subtitle(mut self, subtitle: &str) -> Self {
        if !subtitle.is_empty() {
            self.subtitle = Some(subtitle.to_string());
        }
        self
    }

    /// Buttons shown on the Error card (rendered only in that state).
    pub fn with_error_buttons(mut self, buttons: Vec<CardActionButton>) -> Self {
        self.error_buttons = buttons;
        self
    }

    pub fn with_state(mut self, state: CardState) -> Self {
        self.state = state;
        self
    }

    pub fn with_text(mut self, text: &str) -> Self {
        // Long text is split across multiple elements (each within the
        // per-element limit) so nothing is truncated; the card splitter bounds
        // how much text one card carries.
        for chunk in chunk_text(text, MAX_ELEMENT_TEXT_CHARS) {
            self.body.push(json!({ "tag": "markdown", "content": chunk }));
        }
        self
    }

    pub fn with_reasoning(mut self, reasoning: &str) -> Self {
        if !reasoning.is_empty() {
            self.body
                .push(collapsible_panel("💭 推理过程", &truncate_md(reasoning, 800)));
        }
        self
    }

    pub fn with_tool(mut self, tool: ToolPanel) -> Self {
        let panel = tool.clone();
        self.tools.push(tool);
        // All tools are shown; the streaming card splits into continuation
        // cards when the component estimate exceeds the Feishu limit.
        self.body.push(tool_panel_element(&panel));
        self
    }

    pub fn with_footer(mut self, footer: &str) -> Self {
        self.footer = Some(footer.to_string());
        self
    }

    /// Push an already-built body element (used for inline permission/question
    /// sections on the streaming card).
    pub fn with_element(mut self, element: serde_json::Value) -> Self {
        self.body.push(element);
        self
    }

    /// Build the Feishu card JSON payload (v2 schema).
    pub fn build(&self) -> serde_json::Value {
        let mut elements = self.body.clone();

        if let Some(ref footer) = self.footer {
            elements.push(json!({"tag": "hr"}));
            elements.push(json!({ "tag": "markdown", "content": footer }));
        }

        // Error card actions: a retry button so the user can re-submit without
        // retyping. Only rendered in the Error state.
        if self.state == CardState::Error {
            for btn in &self.error_buttons {
                elements.push(json!({
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": btn.text },
                    "type": btn.kind,
                    "value": btn.value,
                }));
            }
        }

        let (header_title, template) = self.header_info();
        let mut header = serde_json::json!({
            "title": { "tag": "plain_text", "content": header_title },
            "template": template
        });
        if let Some(ref subtitle) = self.subtitle {
            header["subtitle"] = serde_json::json!({
                "tag": "plain_text",
                "content": subtitle
            });
        }
        json!({
            "schema": "2.0",
            "config": { "wide_screen_mode": true },
            "header": header,
            "body": { "elements": elements }
        })
    }

    /// Dynamic header: the card is a reply, so the header shows only the state
    /// (the question already sits in the reply context above). The session name
    /// goes in the subtitle.
    fn header_info(&self) -> (String, &'static str) {
        match self.state {
            CardState::Loading => ("⏳ 思考中...".to_string(), "blue"),
            CardState::Reasoning => ("💭 推理中...".to_string(), "blue"),
            CardState::Streaming => {
                if let Some(tool) = self.tools.iter().find(|t| t.status == "running") {
                    // One icon only: `status_icon` already marks running/pending
                    // with ⏳, so an extra hardcoded 🔧 would show TWO icons
                    // (e.g. "🔧 ⏳ bench") on long-running tools.
                    (format!("{} {}", tool.status_icon(), tool.name), "orange")
                } else {
                    ("✍️ 回复中...".to_string(), "blue")
                }
            }
            // A split card is finalized; its content continues in the next card.
            CardState::Continued => ("⏳ 部分完成，继续中…".to_string(), "blue"),
            CardState::Done => {
                // The turn itself finished. Individual tool failures are shown
                // on their own panels (❌ + reason); they don't make the whole
                // turn a failure — the model routinely retries or works around
                // a failed tool call and still completes the task.
                ("✅ 完成".to_string(), "green")
            }
            CardState::Error => ("❌ 出错".to_string(), "red"),
        }
    }
}

/// Build a collapsible panel (v2), folded by default.
fn collapsible_panel(title: &str, content: &str) -> serde_json::Value {
    json!({
        "tag": "collapsible_panel",
        "expanded": false,
        "header": {
            "title": { "tag": "plain_text", "content": title },
            "icon": { "tag": "standard_icon", "token": "down-small-ccm_outlined" },
            "icon_position": "right"
        },
        "elements": [
            { "tag": "markdown", "content": content }
        ]
    })
}

/// The three permission buttons (Allow Once / Allow Always / Deny), as body
/// elements. Shared by the standalone permission card and the inline section on
/// the streaming card.
pub fn permission_buttons(
    session_id: &str,
    request_id: &str,
    body: &str,
    directory: &str,
) -> Vec<serde_json::Value> {
    let btn_value = |reply: &str, label: &str, color: &str| {
        json!({
            "action": "perm",
            "reply": reply,
            "session_id": session_id,
            "request_id": request_id,
            "directory": directory,
            "perm_label": label,
            "perm_color": color,
            "perm_body": body,
        })
    };
    vec![
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "✅ Allow Once" }, "type": "primary", "value": btn_value("once", "✅ Allowed once", "green") }),
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "🔁 Allow Always" }, "type": "default", "value": btn_value("always", "✅ Allowed always", "green") }),
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "🚫 Deny" }, "type": "danger", "value": btn_value("reject", "🚫 Denied", "red") }),
    ]
}

/// Build the interactive permission card (JSON 2.0). Buttons sit directly in
/// `body.elements` — the v1 `action` container is gone in schema 2.0. Each
/// button carries the request id, session id, owning directory and a
/// description so the card callback (`action: "perm"`) can route the reply
/// back to the right instance and render a result card.
pub fn build_permission_card(
    session_id: &str,
    request_id: &str,
    body: &str,
    directory: &str,
) -> serde_json::Value {
    let mut elements = vec![json!({ "tag": "markdown", "content": body })];
    elements.extend(permission_buttons(session_id, request_id, body, directory));
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": "🔐 Permission Required" },
            "template": "orange"
        },
        "body": { "elements": elements }
    })
}

/// A display label for a session title: strips raw Feishu mention tokens
/// (`@_user_N`) and drops meaningless default titles — the `/new`-generated
/// `sess-<uuid>` and the server's `New session - <iso>` / `Child session - <iso>`
/// placeholders (the caller then shows the session ID instead). Used for card
/// subtitles and notification cards.
pub fn clean_session_label(name: &str) -> String {
    let cleaned = crate::feishu::ws::strip_mention_tokens(name);
    if (cleaned.starts_with("sess-") && cleaned.len() == 41)
        || cleaned.starts_with("New session - ")
        || cleaned.starts_with("Child session - ")
    {
        String::new()
    } else {
        cleaned
    }
}

/// A notification card telling the Feishu side that OpenChamber (or another
/// client on the same store) has posted a new user message to a session.
/// Kept deliberately small and link-free — cola doesn't couple to OpenChamber.
pub fn build_external_message_card(session_name: &str, preview: &str) -> serde_json::Value {
    let session_name = clean_session_label(session_name);
    let mut content = String::new();
    if !session_name.is_empty() {
        content.push_str(&format!("**{}**\n", session_name));
    }
    content.push_str(preview);
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": "💬 有新消息" },
            "template": "blue"
        },
        "body": { "elements": [ { "tag": "markdown", "content": content } ] }
    })
}

/// Replacement for a permission/question card whose request was already resolved
/// by another client (e.g. OpenChamber) — so the Feishu card never stays dead.
/// `detail` is the original request text, so the user can see WHAT was handled.
pub fn build_resolved_elsewhere_card(kind: &str, detail: &str) -> serde_json::Value {
    let mut body = format!("该{}请求已在其他端处理，无需操作。", kind);
    if !detail.is_empty() {
        body.push_str(&format!("\n\n{}", detail));
    }
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": "✅ 已处理" },
            "template": "green"
        },
        "body": { "elements": [ { "tag": "markdown", "content": body } ] }
    })
}

/// A one-line-per-question summary of a `question` request, for the stale card.
pub fn question_summary(questions: &[crate::opencode::client::QuestionInfo]) -> String {
    let mut s = String::new();
    for (i, q) in questions.iter().enumerate() {
        s.push_str(&format!("{}. {}\n", i + 1, q.question));
    }
    s
}

/// Build the interactive question card (JSON 2.0). Options render as buttons
/// when few, or collapse into a folding `overflow` group when many; a question
/// that allows a custom answer (`custom`, default true) gets an input box in a
/// form container. Already-answered questions (`answered[i] == Some(labels)`)
/// render as a static "已选" line instead of buttons, so answering one question
/// never silently submits the others. A submit button appears when some (but
/// not all) questions are answered; a reject button sits at the bottom. The
/// card callback (`action: "question"`) posts the answer back to the session.
/// The body elements of a question prompt: a markdown summary (with ✅ on
/// answered questions), option buttons (or a folding `overflow` when many),
/// a custom-answer input form, an optional "submit/skip" button, and a reject
/// button. Shared by the standalone question card and the inline section on the
/// streaming card.
pub fn question_elements(
    request_id: &str,
    session_id: &str,
    questions: &[crate::opencode::client::QuestionInfo],
    directory: &str,
    answered: &[Option<Vec<String>>],
) -> Vec<serde_json::Value> {
    let is_answered = |qi: usize| -> bool { answered.get(qi).and_then(|a| a.as_ref()).is_some() };
    let mut markdown = String::new();
    for (i, q) in questions.iter().enumerate() {
        if is_answered(i) {
            markdown.push_str(&format!("✅ **{}. {}**\n", i + 1, q.question));
        } else {
            markdown.push_str(&format!("**{}. {}**\n", i + 1, q.question));
        }
        for opt in &q.options {
            if opt.description.is_empty() {
                markdown.push_str(&format!("- {}\n", opt.label));
            } else {
                markdown.push_str(&format!("- {} ({})\n", opt.label, opt.description));
            }
        }
        if let Some(Some(labels)) = answered.get(i) {
            markdown.push_str(&format!("  👉 已选：{}\n", labels.join("、")));
        }
        markdown.push('\n');
    }

    // Overflow (folding button group) delivers the chosen option as a STRING
    // (`action.option`), so each option's value encodes the question index and
    // the answer ("qi|label") and ws.rs decodes it back into the standard
    // answer shape.
    let mut elements: Vec<serde_json::Value> = vec![json!({ "tag": "markdown", "content": markdown })];
    for (qi, q) in questions.iter().enumerate() {
        if is_answered(qi) {
            continue;
        }
        if q.options.len() > MAX_VISIBLE_OPTIONS {
            let options: Vec<serde_json::Value> = q
                .options
                .iter()
                .map(|opt| {
                    json!({
                        "text": { "tag": "plain_text", "content": opt.label },
                        "value": format!("{}|{}", qi, opt.label),
                    })
                })
                .collect();
            elements.push(json!({
                "tag": "overflow",
                "width": "fill",
                "options": options,
                "value": {
                    "action": "question",
                    "reply": "answer",
                    "request_id": request_id,
                    "session_id": session_id,
                    "directory": directory,
                },
            }));
        } else {
            for opt in &q.options {
                elements.push(json!({
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": opt.label },
                    "type": "default",
                    "value": {
                        "action": "question",
                        "reply": "answer",
                        "request_id": request_id,
                        "session_id": session_id,
                        "directory": directory,
                        "question_index": qi,
                        "answer": opt.label,
                    },
                }));
            }
        }

        // Free-text answer (OpenCode questions allow custom by default). The
        // typed text arrives in `action.form_value`; ws.rs injects it into the
        // button's `answer` field so the handler stays unchanged.
        if q.custom.unwrap_or(true) {
            let input_name = format!("custom_{}", qi);
            elements.push(json!({
                "tag": "form",
                "name": format!("form_{}", qi),
                "elements": [
                    {
                        "tag": "input",
                        "name": input_name,
                        "placeholder": { "tag": "plain_text", "content": "✍️ 输入自定义答案" },
                        "max_length": 500,
                        "width": "fill",
                    },
                    {
                        "tag": "button",
                        "text": { "tag": "plain_text", "content": "✍️ 自定义" },
                        "type": "default",
                        "form_action_type": "submit",
                        // Form submit callbacks don't always carry the button
                        // `value`, so the routing payload is ALSO encoded in the
                        // `name` ("submit|req|ses|qi|dir") — ws.rs rebuilds the
                        // value from it when `action.value` is absent.
                        "name": format!("submit|{}|{}|{}|{}", request_id, session_id, qi, directory),
                        "value": {
                            "action": "question",
                            "reply": "answer",
                            "request_id": request_id,
                            "session_id": session_id,
                            "directory": directory,
                            "question_index": qi,
                        },
                    },
                ],
            }));
        }
    }
    let answered_count = answered.iter().filter(|a| a.is_some()).count();
    if answered_count > 0 && answered_count < questions.len() {
        elements.push(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "✅ 提交（跳过剩余）" },
            "type": "primary",
            "value": {
                "action": "question",
                "reply": "submit",
                "request_id": request_id,
                "session_id": session_id,
                "directory": directory,
            },
        }));
    }
    elements.push(json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "🚫 无法回答" },
        "type": "danger",
        "value": {
            "action": "question",
            "reply": "reject",
            "request_id": request_id,
            "session_id": session_id,
            "directory": directory,
        },
    }));

    elements
}

/// Build the interactive question card (JSON 2.0): a header plus the question
/// body elements from `question_elements`.
pub fn build_question_card(
    request_id: &str,
    session_id: &str,
    questions: &[crate::opencode::client::QuestionInfo],
    directory: &str,
    answered: &[Option<Vec<String>>],
) -> serde_json::Value {
    let elements = question_elements(request_id, session_id, questions, directory, answered);
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": "❓ AI 想问你" },
            "template": "blue"
        },
        "body": { "elements": elements }
    })
}
fn truncate_md(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max_len).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_card_header() {
        let card = CardBuilder::new().with_state(CardState::Loading).build();
        let header = &card["header"]["title"]["content"];
        assert!(header.as_str().unwrap().contains("思考中"));
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
    }

    #[test]
    fn reasoning_is_collapsible_panel() {
        let card = CardBuilder::new()
            .with_state(CardState::Reasoning)
            .with_reasoning("Let me analyze this code...")
            .build();
        let header = &card["header"]["title"]["content"];
        assert!(header.as_str().unwrap().contains("推理中"));
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[0]["tag"].as_str().unwrap(), "collapsible_panel");
        assert!(!elements[0]["expanded"].as_bool().unwrap());
        assert!(elements[0].to_string().contains("analyze"));
    }

    #[test]
    fn streaming_card_shows_text() {
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_text("pub fn main() {")
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(
            elements.last().unwrap()["content"].as_str().unwrap(),
            "pub fn main() {"
        );
    }

    #[test]
    fn tool_panel_completed_is_collapsible() {
        let tool = ToolPanel {
            name: "read".into(),
            status: "completed".into(),
            input: Some(json!("src/main.rs")),
            output: Some("fn main() {}".into()),
        };
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

    #[test]
    fn running_tool_shows_in_header() {
        let tool = ToolPanel {
            name: "bash".into(),
            status: "running".into(),
            input: Some(json!({"command": "cargo test"})),
            output: None,
        };
        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_tool(tool)
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert!(header.contains("bash"));
        // The running tool's header must show exactly ONE icon (the status icon
        // ⏳), not a duplicated "🔧 ⏳" pair.
        assert_eq!(
            header, "⏳ bash",
            "running-tool header must be a single icon: {}",
            header
        );
        assert_eq!(card["header"]["template"].as_str().unwrap(), "orange");
    }

    #[test]
    fn done_card_green() {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_text("Done!")
            .build();
        assert_eq!(card["header"]["template"].as_str().unwrap(), "green");
        assert!(
            card["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("✅")
        );
    }

    #[test]
    fn error_card_shows_red_header() {
        let card = CardBuilder::new()
            .with_state(CardState::Error)
            .with_text("**错误**: 503 request queue full")
            .build();
        assert_eq!(card["header"]["template"].as_str().unwrap(), "red");
        assert!(
            card["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("出错")
        );
        assert!(
            card["body"]["elements"][0]["content"]
                .as_str()
                .unwrap()
                .contains("503")
        );
    }

    #[test]
    fn clean_session_label_handles_uuid_and_mentions() {
        // `/new`-generated `sess-<uuid>` names are meaningless → empty label, so
        // the caller shows the session ID instead (never "新会话").
        assert_eq!(
            clean_session_label("sess-7a025fa5-74a1-44e0-b5c5-80b9a21f71bc"),
            ""
        );
        assert_eq!(clean_session_label("@_user_1 你好"), "你好");
        assert_eq!(clean_session_label("frontend-refactor"), "frontend-refactor");
        // A notification card shows the cleaned label, not the raw name.
        let card = build_external_message_card("sess-7a025fa5-74a1-44e0-b5c5-80b9a21f71bc", "hi");
        let text = card.to_string();
        assert!(!text.contains("sess-"), "raw sess-uuid must not leak: {}", text);
    }

    #[test]
    fn subtitle_shows_session_name_in_header() {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_subtitle("proj-lib")
            .with_text("有 3 个文件。")
            .build();
        assert_eq!(
            card["header"]["subtitle"]["content"].as_str().unwrap(),
            "proj-lib"
        );
        // The header itself is state-only — the question is not echoed.
        assert_eq!(card["header"]["title"]["content"].as_str().unwrap(), "✅ 完成");
        // No "问:" body element (the reply context already shows the question).
        let text = card.to_string();
        assert!(!text.contains("**问**"), "question echoed: {}", text);
    }

    #[test]
    fn failed_tool_does_not_fail_whole_card() {
        // A failed tool call is a normal part of an agent run — the model
        // retries or works around it. The card stays "✅ 完成"; the failure is
        // shown only on the tool's own panel (❌ + reason).
        let tool = ToolPanel {
            name: "edit".into(),
            status: "error".into(),
            input: Some(json!({"filePath": "src/main.rs"})),
            output: Some("❌ Could not find oldString...".into()),
        };
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        assert_eq!(card["header"]["template"].as_str().unwrap(), "green");
        assert!(
            card["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .contains("完成")
        );
    }

    #[test]
    fn question_card_has_option_buttons_with_answer_payload() {
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "选择要在哪个目录继续".into(),
            header: "目录".into(),
            options: vec![
                crate::opencode::client::QuestionOption {
                    label: "/a".into(),
                    description: "dir a".into(),
                },
                crate::opencode::client::QuestionOption {
                    label: "/b".into(),
                    description: String::new(),
                },
            ],
            multiple: None,
            custom: None,
        }];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &[None]);
        let text = card.to_string();
        assert!(
            text.contains("选择要在哪个目录继续"),
            "question text missing: {}",
            text
        );

        // JSON 2.0: buttons live directly in body.elements (no v1 action row).
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
        let elements = card["body"]["elements"].as_array().unwrap();
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        // one button per option + a reject button
        assert_eq!(buttons.len(), 3);

        let a_btn = buttons.iter().find(|b| b["text"]["content"] == "/a").unwrap();
        let value = &a_btn["value"];
        assert_eq!(value["action"], "question");
        assert_eq!(value["reply"], "answer");
        assert_eq!(value["request_id"], "que_1");
        assert_eq!(value["session_id"], "ses_1");
        assert_eq!(value["question_index"], 0);
        assert_eq!(value["answer"], "/a");

        let reject = buttons.iter().find(|b| b["value"]["reply"] == "reject").unwrap();
        assert_eq!(reject["value"]["request_id"], "que_1");
    }

    #[test]
    fn question_card_many_options_collapse_into_overflow() {
        let mut options = Vec::new();
        for i in 0..5 {
            options.push(crate::opencode::client::QuestionOption {
                label: format!("/a{}", i),
                description: String::new(),
            });
        }
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "选一个目录".into(),
            header: "目录".into(),
            options,
            multiple: None,
            custom: None,
        }];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &[None]);
        let elements = card["body"]["elements"].as_array().unwrap();
        let overflow = elements
            .iter()
            .find(|e| e["tag"] == "overflow")
            .expect("many options collapse into an overflow");
        let options = overflow["options"].as_array().unwrap();
        assert_eq!(options.len(), 5);
        // Each option encodes "qi|label"; ws.rs decodes it back to an answer.
        assert_eq!(options[0]["value"].as_str().unwrap(), "0|/a0");
        assert_eq!(options[4]["value"].as_str().unwrap(), "0|/a4");
        // The overflow carries the routing payload for the reply.
        assert_eq!(overflow["value"]["request_id"], "que_1");
        assert_eq!(overflow["value"]["reply"], "answer");
        // No raw option buttons for the collapsed question.
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        assert_eq!(buttons.len(), 1, "only the reject button remains");
        assert_eq!(buttons[0]["value"]["reply"], "reject");
    }

    #[test]
    fn question_card_custom_answer_form_added_when_custom_allowed() {
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "q".into(),
            header: "h".into(),
            options: vec![crate::opencode::client::QuestionOption {
                label: "/a".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: None, // default: custom answers allowed
        }];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &[None]);
        let elements = card["body"]["elements"].as_array().unwrap();
        let form = elements
            .iter()
            .find(|e| e["tag"] == "form")
            .expect("custom-allowed question gets an input form");
        assert_eq!(form["name"], "form_0");
        assert!(form.to_string().contains("input"), "form needs an input");
        assert!(form.to_string().contains("form_action_type"));
    }

    #[test]
    fn question_card_custom_disabled_has_no_input_form() {
        let questions = vec![crate::opencode::client::QuestionInfo {
            question: "q".into(),
            header: "h".into(),
            options: vec![crate::opencode::client::QuestionOption {
                label: "/a".into(),
                description: String::new(),
            }],
            multiple: None,
            custom: Some(false),
        }];
        let card = build_question_card("que_1", "ses_1", &questions, "/tmp/proj/lib", &[None]);
        let elements = card["body"]["elements"].as_array().unwrap();
        assert!(
            elements.iter().all(|e| e["tag"] != "form"),
            "custom-disabled question must not get an input form: {}",
            card
        );
    }

    #[test]
    fn tool_panels_are_all_rendered() {
        // All tool panels are shown; the streaming card splits into
        // continuation cards when the component estimate crosses the limit.
        let mut builder = CardBuilder::new().with_state(CardState::Done);
        for i in 0..25 {
            builder = builder.with_tool(ToolPanel {
                name: format!("tool {}", i),
                status: "completed".into(),
                input: Some(json!("in")),
                output: Some("out".into()),
            });
        }
        let card = builder.build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let panels = elements
            .iter()
            .filter(|e| e["tag"].as_str() == Some("collapsible_panel"))
            .count();
        assert_eq!(panels, 25, "every tool panel must be rendered");
        assert!(
            !elements.iter().any(|e| e.to_string().contains("工具未显示")),
            "no hidden-tools note should appear"
        );
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
        let (header, lang, body) = format_tool_output("read", raw);
        assert_eq!(lang, Some("rust"), "language hint from .rs: {:?}", lang);
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

    #[test]
    fn read_tool_output_renders_in_panel() {
        let raw = "\
<path>/x/y.rs</path>
<type>file</type>
<content>
1: use std::fs;
</content>";
        let tool = ToolPanel {
            name: "read".into(),
            status: "completed".into(),
            input: Some(json!({"filePath": "/x/y.rs"})),
            output: Some(raw.into()),
        };
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

    #[test]
    fn tool_output_long_lines_wrapped_in_code_block() {
        // A non-read tool with a line long enough to wrap must become a code
        // block so Feishu doesn't fold it.
        let long = format!("cargo run {}", "a".repeat(140));
        let tool = ToolPanel {
            name: "bash".into(),
            status: "completed".into(),
            input: None,
            output: Some(long.clone()),
        };
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
        let tool = ToolPanel {
            name: "bash".into(),
            status: "completed".into(),
            input: None,
            output: Some("all tests passed".into()),
        };
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(!text.contains("```"), "short output must not be fenced: {}", text);
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
    fn long_text_splits_across_multiple_elements() {
        let long = "x".repeat(MAX_ELEMENT_TEXT_CHARS * 2 + 100);
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_text(&long)
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let text_els: Vec<_> = elements.iter().filter(|e| e["tag"] == "markdown").collect();
        assert!(
            text_els.len() >= 3,
            "long text must split into multiple elements, got {}: {}",
            text_els.len(),
            card
        );
        // No element exceeds the per-element cap, and NOTHING is truncated —
        // the full text is preserved across the elements.
        for el in &text_els {
            let content = el["content"].as_str().unwrap();
            assert!(
                content.chars().count() <= MAX_ELEMENT_TEXT_CHARS,
                "element over cap: {} chars",
                content.chars().count()
            );
        }
        let joined: String = text_els.iter().map(|e| e["content"].as_str().unwrap()).collect();
        assert_eq!(joined, long, "all text must be preserved, none truncated");
    }

    #[test]
    fn short_text_is_not_truncated() {
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_text("short reply")
            .build();
        let elements = card["body"]["elements"].as_array().unwrap();
        let text_el = elements.iter().find(|e| e["tag"] == "markdown").unwrap();
        assert_eq!(text_el["content"].as_str().unwrap(), "short reply");
    }

    #[test]
    fn permission_card_carries_reply_payload() {
        let card = build_permission_card("ses_abc", "per_123", "AI 想要执行 bash", "/tmp/proj/lib");
        assert_eq!(
            card["header"]["title"]["content"].as_str().unwrap(),
            "🔐 Permission Required"
        );
        // JSON 2.0: body.elements hold the markdown + buttons directly (no v1
        // action row).
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
        let elements = card["body"]["elements"].as_array().unwrap();
        // element 0 = description markdown, rest = buttons
        assert!(elements[0]["content"].as_str().unwrap().contains("AI 想要执行"));
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        assert_eq!(buttons.len(), 3);
        let values: Vec<_> = buttons.iter().map(|a| &a["value"]).collect();
        // Each button must carry the reply + request_id + session_id so the
        // card callback can route the answer back.
        let once = values.iter().find(|v| v["reply"] == "once").unwrap();
        assert_eq!(once["request_id"], "per_123");
        assert_eq!(once["session_id"], "ses_abc");
        assert_eq!(once["perm_label"], "✅ Allowed once");
        assert_eq!(once["perm_color"], "green");
        assert!(once["perm_body"].as_str().unwrap().contains("bash"));
        let reject = values.iter().find(|v| v["reply"] == "reject").unwrap();
        assert_eq!(reject["perm_color"], "red");
        let always = values.iter().find(|v| v["reply"] == "always").unwrap();
        assert!(always["perm_label"].as_str().unwrap().contains("always"));
    }

    #[test]
    fn tool_input_bash_shows_command_and_workdir() {
        let tool = ToolPanel {
            name: "bash".into(),
            status: "completed".into(),
            input: Some(json!({"command": "cargo test --all", "workdir": "/proj"})),
            output: None,
        };
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
    fn tool_input_edit_shows_file_and_diff() {
        let tool = ToolPanel {
            name: "edit".into(),
            status: "completed".into(),
            input: Some(json!({
                "filePath": "src/main.rs",
                "oldString": "let a = 1;",
                "newString": "let a = 2;"
            })),
            output: None,
        };
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let text = card.to_string();
        assert!(text.contains("src/main.rs"), "file missing: {}", text);
        assert!(text.contains("- let a = 1;"), "old line missing: {}", text);
        assert!(text.contains("+ let a = 2;"), "new line missing: {}", text);
        assert!(!text.contains("filePath"), "raw key leaked: {}", text);
    }

    #[test]
    fn tool_input_read_shows_path_and_limits() {
        let tool = ToolPanel {
            name: "read".into(),
            status: "completed".into(),
            input: Some(json!({"filePath": "src/foo.rs", "limit": 80})),
            output: None,
        };
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
        let tool = ToolPanel {
            name: "grep".into(),
            status: "completed".into(),
            input: Some(json!({"pattern": "fn main", "path": "src/main.rs", "include": "*.rs"})),
            output: None,
        };
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
        let tool = ToolPanel {
            name: "glob".into(),
            status: "completed".into(),
            input: Some(json!({"pattern": "**/*.ts"})),
            output: None,
        };
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

    #[test]
    fn tool_input_string_shows_as_is() {
        // A bare string input (non-object) renders directly.
        let tool = ToolPanel {
            name: "read".into(),
            status: "completed".into(),
            input: Some(json!("src/main.rs")),
            output: None,
        };
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        assert!(card.to_string().contains("src/main.rs"));
    }

    #[test]
    fn tool_input_unknown_falls_back_to_key_value() {
        let tool = ToolPanel {
            name: "custom_tool".into(),
            status: "completed".into(),
            input: Some(json!({"a": "b", "c": 3})),
            output: None,
        };
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
