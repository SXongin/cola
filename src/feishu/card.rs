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
/// Feishu's documented ceiling for a card's serialized body (the message API
/// rejects cards above it; ~30KB, returned as `230099` / "create universal
/// card fail" 200800). `MAX_CARD_JSON_CHARS` keeps cards under this by a
/// comfortable margin, since the split estimate trails the real serialized
/// size by the un-accounted tail (footer, inline buttons) plus per-element
/// overhead.
pub const FEISHU_CARD_LIMIT_BYTES: usize = 30_000;
/// Estimated JSON size ceiling for a card body. Feishu's documented card limit
/// is [`FEISHU_CARD_LIMIT_BYTES`], and the message API rejects cards far below
/// the old 100KB assumption — a real 44KB card fails with 230099 / "create
/// universal card fail" (200800). A streaming card splits when the estimate
/// crosses this, keeping every card comfortably under the 30KB cap (the
/// estimate trails the serialized size by a few hundred bytes per element, so
/// the margin absorbs it). The 5KB gap to the hard limit covers the tail
/// sections (footer, inline permission/question buttons) the estimate doesn't
/// count.
pub const MAX_CARD_JSON_CHARS: usize = FEISHU_CARD_LIMIT_BYTES - 5_000;
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

/// Format a duration in seconds for the header's live timer (ADR-0014):
/// `42s`, `1m23s`, `2h5m`, `3d4h`. Whole seconds, so a header signature built
/// from it changes at most once per second — the natural flush throttle.
pub(crate) fn fmt_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// A compact reasoning-length label for the thinking header: `832字`, `2.1k字`.
fn format_chars(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}k字", n as f64 / 1000.0)
    } else {
        format!("{n}字")
    }
}

/// Builds Feishu interactive card JSON (v2 schema with collapsible panels).
/// Body elements are appended in the order the builder methods are called, so
/// text and tool panels can be interleaved chronologically.
/// Progress/liveness signals for the card header (ADR-0014): whether the turn
/// is paused waiting for a permission/question, how long the current phase has
/// run, and the reasoning text length. Bundled so they travel through the
/// builder, the header renderer, and the accumulator as one unit instead of
/// three loose values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeaderProgress {
    /// A permission/question is pending: the header shows "等待你的授权/回答"
    /// instead of the phase label — the turn is paused, not stuck.
    pub waiting: bool,
    /// Seconds the current phase (thinking/reasoning/tool/streaming) has run.
    pub elapsed: Option<u64>,
    /// Reasoning text length, shown on the thinking header as real progress.
    pub reasoning_chars: usize,
}

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
    /// Progress/liveness inputs for the header (ADR-0014): the waiting flag,
    /// the phase timer, and the reasoning length. When absent (all defaults)
    /// the header renders the plain phase label — non-streaming builders
    /// (permission/switch cards) stay unchanged.
    progress: HeaderProgress,
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
            // Trailing blank line so a multi-line input (edit diff, skill
            // metadata list) can't swallow the Output section as a markdown
            // lazy continuation of its last list line — Feishu would render
            // `**Output**` glued to the last input line.
            content.push_str(&format!("**Input**\n{}\n\n", truncate_md(&formatted, 400)));
        }
    }
    if let Some(ref o) = tool.output {
        let (header, lang, body) = format_tool_output(&tool.name, o);
        content.push_str("**Output**\n");
        if let Some(h) = &header {
            content.push_str(&format!("{}\n\n", h));
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
            progress: HeaderProgress::default(),
        }
    }

    /// Progress/liveness inputs for the header (ADR-0014): a waiting flag for
    /// pending permission/question interactions, the phase timer, and the
    /// reasoning length. When absent the header renders the plain phase label —
    /// builders that don't set progress (e.g. permission/switch cards) stay
    /// unchanged.
    pub fn with_progress(mut self, progress: HeaderProgress) -> Self {
        self.progress = progress;
        self
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

        let running = self.tools.iter().find(|t| t.status == "running");
        let (header_title, template) = header_title_and_template(&self.state, running, &self.progress);
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
}

/// Header title + template for a card. `running_tool` is the tool currently
/// running (from the builder's tools), driving the "⏳ tool" streaming header.
/// Active states append the progress signals passed in from the accumulator;
/// `progress.waiting` (a pending permission/question) overrides the phase label
/// entirely.
pub(crate) fn header_title_and_template(
    state: &CardState,
    running_tool: Option<&ToolPanel>,
    progress: &HeaderProgress,
) -> (String, &'static str) {
    if progress.waiting {
        return ("⏳ 等待你的授权/回答".to_string(), "orange");
    }
    let (label, template) = match state {
        CardState::Loading => ("⏳ 思考中".to_string(), "blue"),
        CardState::Reasoning => ("💭 推理中".to_string(), "blue"),
        CardState::Streaming => {
            if let Some(tool) = running_tool {
                // One icon only: `status_icon` already marks running/pending
                // with ⏳, so an extra hardcoded 🔧 would show TWO icons
                // (e.g. "🔧 ⏳ bench") on long-running tools.
                (format!("{} {}", tool.status_icon(), tool.name), "orange")
            } else {
                ("✍️ 回复中".to_string(), "blue")
            }
        }
        CardState::Continued => ("⏳ 部分完成，继续中…".to_string(), "blue"),
        CardState::Done => ("✅ 完成".to_string(), "green"),
        CardState::Error => ("❌ 出错".to_string(), "red"),
    };
    let mut title = label;
    match state {
        CardState::Loading | CardState::Reasoning | CardState::Streaming => {
            if let Some(e) = progress.elapsed {
                title.push_str(&format!(" {}", fmt_elapsed(e)));
            }
            // Reasoning is streamed incrementally by OpenCode, so its length is
            // real progress during the thinking phase.
            if *state == CardState::Reasoning && progress.reasoning_chars > 0 {
                title.push_str(&format!(" · {}", format_chars(progress.reasoning_chars)));
            }
        }
        _ => {}
    }
    (title, template)
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

/// The four permission buttons (Allow Once / Allow Always / Deny / Auto-Accept),
/// as body elements. Shared by the standalone permission card and the inline
/// section on the streaming card. The fourth turns on the session's Auto-Accept
/// (cola-side, `/autoaccept`), distinct from "Allow Always" which is a
/// backend-side per-type rule.
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
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "✅ 允许一次" }, "type": "primary", "value": btn_value("once", "✅ 已允许一次", "green") }),
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "🔁 始终允许" }, "type": "default", "value": btn_value("always", "✅ 已始终允许", "green") }),
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "🚫 拒绝" }, "type": "danger", "value": btn_value("reject", "🚫 已拒绝", "red") }),
        json!({ "tag": "button", "text": { "tag": "plain_text", "content": "⚡ 开启自动授权" }, "type": "primary", "value": btn_value("autoaccept", "✅ 已开启自动授权", "blue") }),
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
            "title": { "tag": "plain_text", "content": "🔐 权限请求" },
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
/// form container. A multi-select question (`multiple`) renders its options as
/// toggle buttons instead: each click adds/removes the label and the card shows
/// the running selection ("已选"), finalized by an explicit submit. Already-
/// answered single-select questions (`answered[i] == Some(labels)`) render as a
/// static "已选" line instead of buttons, so answering one question never
/// silently submits the others. A submit button appears when some (but not all)
/// questions are answered, or when any multi-select question holds a selection;
/// a reject button sits at the bottom. The card callback (`action: "question"`)
/// posts the answer back to the session.
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
    // A multi-select question (`multiple`) is NEVER finalized by clicking an
    // option: clicks toggle labels in its answer set until the user submits.
    // Single-select questions are finalized the moment an option is clicked.
    let is_multi = |i: usize| -> bool { questions.get(i).is_some_and(|q| q.multiple == Some(true)) };
    let is_answered = |qi: usize| -> bool { answered.get(qi).and_then(|a| a.as_ref()).is_some() };
    let mut markdown = String::new();
    for (i, q) in questions.iter().enumerate() {
        if is_answered(i) && !is_multi(i) {
            markdown.push_str(&format!("✅ **{}. {}**\n", i + 1, q.question));
        } else {
            markdown.push_str(&format!(
                "**{}. {}{}**\n",
                i + 1,
                q.question,
                if is_multi(i) { "（可多选）" } else { "" }
            ));
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
        if is_answered(qi) && !is_multi(qi) {
            continue;
        }
        // Multi-select stays on buttons (clicks toggle); only single-select
        // collapses into an `overflow` when there are many options.
        if !is_multi(qi) && q.options.len() > MAX_VISIBLE_OPTIONS {
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
                // Multi-select buttons show their selected state so the user
                // can see what's already picked while still toggling.
                let selected = is_multi(qi)
                    && answered
                        .get(qi)
                        .and_then(|a| a.as_ref())
                        .is_some_and(|labels| labels.iter().any(|l| l == &opt.label));
                elements.push(json!({
                    "tag": "button",
                    "text": {
                        "tag": "plain_text",
                        "content": if selected {
                            format!("✅ {}", opt.label)
                        } else {
                            opt.label.clone()
                        },
                    },
                    "type": if selected { "primary" } else { "default" },
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
        // button's `answer` field so the handler stays unchanged. Multi-select
        // questions keep the button set (custom additions are an edge case).
        if !is_multi(qi) && q.custom.unwrap_or(true) {
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
                        // `name` ("submit|req|ses|qi") — ws.rs rebuilds the
                        // value from it when `action.value` is absent. The
                        // directory is deliberately NOT in the name: Feishu
                        // caps `name` at 100 chars and `submit|req|ses|qi|dir`
                        // overflows on deep paths, killing the whole card update
                        // (ErrCode 11310 "name exceed the default maximum 100").
                        // The handler re-resolves the directory from the store /
                        // request flow when the fallback fires.
                        "name": format!("submit|{}|{}|{}", request_id, session_id, qi),
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
    let has_multi = questions.iter().any(|q| q.multiple == Some(true));
    // Submit appears when something is picked while questions remain open
    // (single-select "跳过剩余"), and ALWAYS for a request containing a
    // multi-select question — so the user can explicitly submit an empty
    // selection ("不选") as well as a full one. Without the multi-select case
    // the button vanishes at zero selections and "none" is unexpressible.
    let show_submit = (answered_count > 0 && answered_count < questions.len()) || has_multi;
    if show_submit {
        let label = if has_multi {
            "✅ 提交"
        } else {
            "✅ 提交（跳过剩余）"
        };
        elements.push(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": label },
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
/// One session entry of the `/switch` card: a full-width text row followed by
/// a button row underneath. The text row is its own `column_set` column (not a
/// weighted column squeezed beside the buttons), so the session label and
/// directory wrap naturally instead of being crushed into many narrow lines on
/// mobile. The button row is a `column_set` with `flex_mode: "bisect"` (two
/// equal columns, one per button) so the pair splits evenly on narrow
/// screens instead of overflowing: the primary button switches/adopts into the
/// current thread (`op: "adopt"`); a second "建话题接管" button
/// (`op: "topic_adopt"`) opens a new Feishu topic around the session
/// (ADR-0016). Each button carries the routing payload (action, op, thread_key,
/// target session id).
///
/// Schema 2.0 dropped the v1 `action` container (error 200861, "cards of
/// schema V2 no longer support this capability"), so buttons never live in an
/// `action` element — the button row is a `column_set` with one column per
/// button.
/// A full-width text row (schema-2.0 safe): a single weighted column holding a
/// markdown element. Shared by the `/switch` and `/dir` card rows.
fn card_text_row(text: &str) -> serde_json::Value {
    json!({
        "tag": "column_set",
        "flex_mode": "none",
        "columns": [
            {
                "tag": "column",
                "width": "weighted",
                "weight": 5,
                "vertical_align": "center",
                "elements": [ { "tag": "markdown", "content": text } ]
            }
        ]
    })
}

fn switch_card_row(
    text: &str,
    btn_text: &str,
    thread_key: &crate::config::ThreadKey,
    session_id: &str,
) -> Vec<serde_json::Value> {
    let btn_column = |op: &str, content: &str| {
        json!({
            "tag": "column",
            "width": "auto",
            "vertical_align": "center",
            "elements": [
                {
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": content },
                    "type": "default",
                    "value": {
                        "action": "switch",
                        "op": op,
                        "chat_id": thread_key.chat_id,
                        "thread_id": thread_key.thread_id,
                        "session_id": session_id,
                    },
                }
            ],
        })
    };
    let text_row = card_text_row(text);
    let btn_row = json!({
        "tag": "column_set",
        "flex_mode": "bisect",
        "horizontal_spacing": "default",
        "columns": [ btn_column("adopt", btn_text), btn_column("topic_adopt", "建话题接管") ]
    });
    vec![text_row, btn_row]
}

/// A generic option-picker card: one button per option. Shared by the
/// `/agent`, `/model` and `/autoaccept` dual-form cards. Each button carries
/// the given `action` tag + the routing payload (thread_key) + the option
/// value, so the ack routes the choice back to the right thread.
fn option_picker_card(
    header: &str,
    intro: &str,
    thread_key: &crate::config::ThreadKey,
    action: &str,
    options: &[(String, String)],
) -> serde_json::Value {
    picker_card(header, intro, thread_key, action, None, false, options)
}

/// The `/model` picker-card back button's value: clicking it returns from a
/// provider's model page to the full provider list. A named constant (not a
/// bare string literal) so the handler's comparison cannot typo-silently break
/// navigation.
pub(crate) const PICKER_BACK_TO_PROVIDERS: &str = "__providers__";

/// The two levels of the `/model` picker card (ADR-0012 issue 05): a
/// `Provider` button on the provider-list page (its `value` is a provider id,
/// or [`PICKER_BACK_TO_PROVIDERS`] to go back); a `Model` button records the
/// per-session override. Serialized as the callback's `level` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerLevel {
    Provider,
    Model,
}

impl PickerLevel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PickerLevel::Provider => "provider",
            PickerLevel::Model => "model",
        }
    }
}

/// A picker card whose buttons carry an optional `level` (two-step navigation
/// for the `/model` provider → model flow) and an optional leading "back"
/// button. Each button's callback payload is `{action, chat_id, thread_id,
/// [level], value}`.
fn picker_card(
    header: &str,
    intro: &str,
    thread_key: &crate::config::ThreadKey,
    action: &str,
    level: Option<PickerLevel>,
    back: bool,
    options: &[(String, String)],
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = vec![json!({ "tag": "markdown", "content": intro })];
    if back {
        elements.push(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": "← 返回全部 provider" },
            "type": "default",
            "width": "fill",
            "value": {
                "action": action,
                "chat_id": thread_key.chat_id,
                "thread_id": thread_key.thread_id,
                "level": PickerLevel::Provider.as_str(),
                "value": PICKER_BACK_TO_PROVIDERS,
            },
        }));
    }
    for (label, value) in options {
        let mut payload = json!({
            "action": action,
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
            "value": value,
        });
        if let Some(l) = level {
            payload["level"] = json!(l.as_str());
        }
        elements.push(json!({
            "tag": "button",
            "text": { "tag": "plain_text", "content": label },
            "type": "default",
            "width": "fill",
            "value": payload,
        }));
    }
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": header },
            "template": "blue"
        },
        "body": { "elements": elements }
    })
}

/// The `/agent` picker card: one button per available agent. Falls back to an
/// empty intro when no agents are listed (the backend is unreachable).
pub fn build_agent_card(
    thread_key: &crate::config::ThreadKey,
    agents: &[crate::opencode::client::AgentInfo],
) -> serde_json::Value {
    let intro = if agents.is_empty() {
        "_(没有可用 agent)_"
    } else {
        "**选择 agent**（下一条消息开始生效）："
    };
    let options: Vec<(String, String)> = agents
        .iter()
        .filter(|a| a.hidden != Some(true))
        .map(|a| (a.name.clone(), a.name.clone()))
        .collect();
    option_picker_card("🤖 选择 Agent", intro, thread_key, "agent", &options)
}

/// The `/model` picker, step 1: one card page per set of `provider` buttons.
/// Falls back to an empty intro when no providers are listed (the backend is
/// unreachable).
///
/// A shared OpenCode server advertises providers across every model source it
/// knows (gateways included) — hundreds of providers and thousands of models,
/// which in a single button-per-model card blows past Feishu's 30KB / component
/// ceilings and the send fails silently (the `/model` command "does nothing").
/// The flow is therefore two-step: pick a provider here, then one of its
/// models ([`build_model_picker_cards`]); every card is chunked to stay under
/// the card budgets, and any model stays selectable via the
/// `/model <provider/model>` text form.
pub fn build_model_provider_cards(
    thread_key: &crate::config::ThreadKey,
    providers: &[crate::opencode::client::ProviderModels],
) -> Vec<serde_json::Value> {
    let options: Vec<(String, String)> = providers
        .iter()
        .map(|p| (p.provider.clone(), p.provider.clone()))
        .collect();
    if options.is_empty() {
        return vec![picker_card(
            "🎯 选择模型",
            "_(没有可用模型)_",
            thread_key,
            "model",
            Some(PickerLevel::Provider),
            false,
            &[],
        )];
    }
    chunk_picker_cards(
        "🎯 选择模型",
        &format!("**选择 provider**（共 {} 个）：", options.len()),
        thread_key,
        "model",
        Some(PickerLevel::Provider),
        false,
        &options,
    )
}

/// The `/model` picker, step 2: one card page per set of `provider/model`
/// buttons for a single chosen provider. Carries a leading "← 返回全部
/// provider" button so the user can back out of a wrong provider.
pub fn build_model_picker_cards(
    thread_key: &crate::config::ThreadKey,
    provider: &str,
    models: &[crate::opencode::client::ModelOption],
) -> Vec<serde_json::Value> {
    // The provider is already chosen (step 1), so the button LABEL shows just
    // the model name — a `provider/model` label truncates in Feishu's button
    // width. The callback VALUE keeps the full `provider/model` the handler
    // records as the override.
    let options: Vec<(String, String)> = models
        .iter()
        .map(|m| (m.id.clone(), format!("{provider}/{}", m.id)))
        .collect();
    if options.is_empty() {
        return vec![picker_card(
            &format!("🎯 {provider}"),
            "_(该 provider 没有可用模型)_",
            thread_key,
            "model",
            Some(PickerLevel::Model),
            true,
            &[],
        )];
    }
    chunk_picker_cards(
        &format!("🎯 {provider}"),
        "**选择 model**（下一条消息开始生效）：",
        thread_key,
        "model",
        Some(PickerLevel::Model),
        true,
        &options,
    )
}

/// Max picker buttons per card. Feishu counts each button's inner `plain_text`
/// as a separate element against the JSON 2.0 ceiling of 200 elements+components
/// (error 300305 / 11310 "element exceeds the limit" — measured via the cardkit
/// create API at 99 buttons max with an intro markdown). 80 keeps a comfortable
/// margin including the back button's two elements.
pub const MAX_PICKER_BUTTONS_PER_CARD: usize = 80;

/// Build picker cards from `options`, chunking so each card stays under
/// Feishu's component and JSON-size ceilings. Each card beyond the first
/// carries a page hint so the user knows there is more.
fn chunk_picker_cards(
    header: &str,
    intro: &str,
    thread_key: &crate::config::ThreadKey,
    action: &str,
    level: Option<PickerLevel>,
    back: bool,
    options: &[(String, String)],
) -> Vec<serde_json::Value> {
    let mut pages: Vec<&[(String, String)]> = Vec::new();
    let mut start = 0usize;
    while start < options.len() {
        // Estimate JSON bytes like the streaming splitter (a button is ~200
        // bytes + the label's UTF-8); bound the button COUNT by the picker
        // budget, not the streaming 150-component constant — buttons cost ~2
        // elements each against Feishu's 200-element ceiling.
        let mut bytes = intro.len() + 64;
        let mut end = start;
        while end < options.len()
            && end - start < MAX_PICKER_BUTTONS_PER_CARD
            && bytes + 200 + options[end].0.len() * 2 <= MAX_CARD_JSON_CHARS
        {
            bytes += 200 + options[end].0.len() * 2;
            end += 1;
        }
        if end == start {
            end = start + 1; // a single oversized option still gets its own card
        }
        pages.push(&options[start..end]);
        start = end;
    }
    let n_pages = pages.len();
    let mut cards: Vec<serde_json::Value> = Vec::with_capacity(n_pages);
    for (i, page) in pages.into_iter().enumerate() {
        let mut intro_text = intro.to_string();
        if n_pages > 1 {
            intro_text.push_str(&format!("（第 {} / {} 页）", i + 1, n_pages));
        }
        cards.push(picker_card(
            header,
            &intro_text,
            thread_key,
            action,
            level,
            back,
            page,
        ));
    }
    cards
}

/// The `/autoaccept` toggle card: two buttons showing the current state.
pub fn build_autoaccept_card(thread_key: &crate::config::ThreadKey, current_on: bool) -> serde_json::Value {
    let intro = format!(
        "**当前自动审批：{}**\n自动审批开启后，本会话的权限请求不再弹卡，直接 Allow。",
        if current_on { "🔁 开" } else { "❌ 关" }
    );
    let options = vec![
        ("🔁 开启自动审批".to_string(), "on".to_string()),
        ("❌ 关闭自动审批".to_string(), "off".to_string()),
    ];
    option_picker_card("🔁 自动审批", &intro, thread_key, "autoaccept", &options)
}

/// The `/think` picker card (ADR-0020): the current model, its declared
/// variants (each model's own set — no universal scale), and a "默认（清除）"
/// button. `current` is the session's active variant (None = server default).
/// Only built when `variants` is non-empty — the caller replies a text message
/// for models that declare none.
pub fn build_think_card(
    thread_key: &crate::config::ThreadKey,
    provider: &str,
    model: &str,
    current: Option<&str>,
    variants: &[String],
) -> serde_json::Value {
    let label = format!("{provider}/{model}");
    let current_label = current
        .map(|v| format!("`{v}`"))
        .unwrap_or_else(|| "默认".to_string());
    let intro =
        format!("**当前模型**：`{label}`\n**当前思考等级**：{current_label}\n选择后下一条消息开始生效：");
    let mut options = vec![("默认（清除）".to_string(), "default".to_string())];
    for v in variants {
        options.push((v.clone(), v.clone()));
    }
    option_picker_card("🧠 思考等级", &intro, thread_key, "think", &options)
}

/// The `/help` reference card: a pure command manual grouped by 会话 / 操作 /
/// 运维, one line per command with a short description. No buttons — reading is
/// its only job (previously the "试试"/"看卡" buttons made it an execution
/// launcher, which mixed concerns and was hard to keep consistent). Detail for a
/// single command stays text via `/help <command>`.
pub fn build_help_card() -> serde_json::Value {
    let groups: &[(&str, &[(&str, &str)])] = &[
        (
            "📂 会话",
            &[
                ("/new [名字]", "在当前项目新建会话"),
                (
                    "/dir [路径] [名字]",
                    "切换项目，在新目录开会话（无参弹最近目录卡片）",
                ),
                (
                    "/switch [关键字]",
                    "会话卡片，或按名称/目录/ID 切换（含 list / forget）",
                ),
                ("/topic [目录] [名字]", "新话题 + 新会话（无参用当前项目目录）"),
                ("/topic --adopt <关键字> [--force]", "围绕已有会话开话题"),
                ("/name <名字>", "重命名当前会话"),
            ],
        ),
        (
            "⚙️ 操作",
            &[
                ("/agent <名字>", "切换 agent（下条消息生效）"),
                ("/model <提供方/模型>", "切换模型（下条消息生效）"),
                ("/think [等级]", "设置/清除思考等级（下条消息生效）"),
                ("/autoaccept [on|off]", "查看或切换自动授权"),
                ("/stop", "中断当前执行"),
                ("/compact", "压缩上下文"),
            ],
        ),
        (
            "🛠 运维",
            &[
                ("/help [命令]", "全部命令，或单命令详情"),
                ("/restart", "重启 cola"),
                ("/restart-opencode", "重启 OpenCode 服务器（仅 cola 启动的）"),
                ("/update", "检查并应用自更新"),
            ],
        ),
    ];
    let mut elements: Vec<serde_json::Value> = Vec::new();
    for (title, rows) in groups {
        elements.push(json!({ "tag": "markdown", "content": format!("**{title}**") }));
        for (cmd, desc) in *rows {
            elements.push(json!({
                "tag": "markdown",
                "content": format!("`{cmd}` · {desc}"),
            }));
        }
    }
    elements.push(json!({
        "tag": "markdown",
        "content": "详细用法发 `/help <命令>`，如 `/help switch`。群话题规则见首次引导。",
    }));
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": "📖 cola 命令" },
            "template": "blue"
        },
        "body": { "elements": elements }
    })
}

/// Build the interactive `/switch` session card (ADR-0012, issue 04): a
/// search box, up to `MAX_SWITCH_ROWS` session rows (each with a
/// switch/adopt button), and a "＋new" footer button that creates a fresh
/// session in the current project (equivalent to `/new`). `keyword` is the
/// active filter (empty = all); `active_id`/`mapped_ids` drive the row
/// labels and buttons.
pub const MAX_SWITCH_ROWS: usize = 6;

pub fn build_switch_card(
    thread_key: &crate::config::ThreadKey,
    sessions: &[crate::opencode::client::SessionListInfo],
    keyword: &str,
    active_id: Option<&str>,
    mapped_ids: &[String],
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = Vec::new();

    // Search form: an input + a submit button. The routing payload rides in the
    // button's `name` (form submits don't always deliver the button `value`),
    // and the typed keyword arrives as `form_value.search`.
    elements.push(json!({
        "tag": "form",
        "name": "switch_search",
        "elements": [
            {
                "tag": "input",
                "name": "search",
                "placeholder": { "tag": "plain_text", "content": "🔍 搜索标题 / 目录 / ID" },
                "value": { "tag": "plain_text", "content": keyword },
                "max_length": 100,
                "width": "fill",
            },
            {
                "tag": "button",
                "text": { "tag": "plain_text", "content": "搜索" },
                "type": "primary",
                "form_action_type": "submit",
                "name": format!("switchsearch|{}|{}", thread_key.chat_id, thread_key.thread_id),
                "value": {
                    "action": "switch",
                    "op": "search",
                    "chat_id": thread_key.chat_id,
                    "thread_id": thread_key.thread_id,
                },
            },
        ],
    }));

    if sessions.is_empty() {
        elements.push(json!({ "tag": "markdown", "content": "_(无匹配会话)_" }));
    } else {
        let header = if keyword.is_empty() {
            "**最近会话**"
        } else {
            &format!("**匹配 `{keyword}` 的会话**")
        };
        elements.push(json!({ "tag": "markdown", "content": header }));
        for s in sessions.iter().take(MAX_SWITCH_ROWS) {
            let label = crate::bridge::command::title_or_id_tail(s);
            let text = if active_id == Some(s.id.as_str()) {
                format!(
                    "{label} · {} · {}\n_(active)_",
                    s.directory,
                    crate::bridge::command::id_tail(&s.id)
                )
            } else if mapped_ids.contains(&s.id) {
                format!(
                    "{label} · {} · {}\n_(本会话)_",
                    s.directory,
                    crate::bridge::command::id_tail(&s.id)
                )
            } else {
                format!(
                    "{label} · {} · {}",
                    s.directory,
                    crate::bridge::command::id_tail(&s.id)
                )
            };
            let btn = if active_id == Some(s.id.as_str()) {
                "✅ 当前"
            } else if mapped_ids.contains(&s.id) {
                "切换"
            } else {
                "接管"
            };
            elements.extend(switch_card_row(&text, btn, thread_key, &s.id));
        }
    }

    // Footer: "＋new" creates a fresh session in the current project.
    elements.push(json!({
        "tag": "button",
        "text": { "tag": "plain_text", "content": "＋ 新建会话" },
        "type": "primary",
        "value": {
            "action": "switch",
            "op": "new",
            "chat_id": thread_key.chat_id,
            "thread_id": thread_key.thread_id,
        },
    }));

    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": "📂 会话管理" },
            "template": "blue"
        },
        "body": { "elements": elements }
    })
}

/// The `/dir` Recent Directories card (no-arg form): one directory per entry,
/// capped at [`MAX_SWITCH_ROWS`]. Each entry is two rows — a full-width text
/// row (directory path, marked `当前` when it is the thread's active session
/// directory) and a button row beneath it, mirroring the `/switch` card
/// layout. The pick button carries the routing payload (action, op,
/// thread_key, directory), so the ack re-roots the thread into that directory.
/// Schema-2.0 safe: no v1 `action` container (see `switch_card_row`).
pub fn build_dir_card(
    thread_key: &crate::config::ThreadKey,
    dirs: &[String],
    current_dir: Option<&str>,
) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = Vec::new();

    if dirs.is_empty() {
        elements.push(json!({
            "tag": "markdown",
            "content": "_(还没有最近目录。用 `/dir <路径>` 或 `/new` 创建会话。)_"
        }));
    } else {
        elements.push(json!({ "tag": "markdown", "content": "**最近目录**" }));
        for dir in dirs.iter().take(MAX_SWITCH_ROWS) {
            let is_current = current_dir == Some(dir.as_str());
            let text = if is_current {
                format!("`{dir}`\n_(当前)_")
            } else {
                format!("`{dir}`")
            };
            let btn = if is_current {
                "✅ 当前"
            } else {
                "切换到这里"
            };
            elements.extend(dir_card_row(&text, btn, thread_key, dir));
        }
        // Truncated entries aren't lost: `/switch` adopts an EXISTING session
        // by directory, and `/new` then opens a fresh one in that project —
        // the two-step path to a new session in an overflow directory.
        if dirs.len() > MAX_SWITCH_ROWS {
            elements.push(json!({
                "tag": "markdown",
                "content": format!(
                    "_(还有 {} 个最近目录未显示。用 `/switch <路径>` 接管已有会话，再 `/new` 新建。)_",
                    dirs.len() - MAX_SWITCH_ROWS
                )
            }));
        }
    }

    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": "📂 最近目录" },
            "template": "blue"
        },
        "body": { "elements": elements }
    })
}

/// One `/dir` card entry: a full-width text row plus a single-button row that
/// re-roots the thread into the directory (`op: "pick"`). Mirrors the
/// `/switch` card's two-row-per-entry layout (schema-2.0 safe, no `action`
/// container).
fn dir_card_row(
    text: &str,
    btn_text: &str,
    thread_key: &crate::config::ThreadKey,
    directory: &str,
) -> Vec<serde_json::Value> {
    let text_row = card_text_row(text);
    let btn_row = json!({
        "tag": "column_set",
        "flex_mode": "none",
        "horizontal_spacing": "default",
        "columns": [
            {
                "tag": "column",
                "width": "auto",
                "vertical_align": "center",
                "elements": [
                    {
                        "tag": "button",
                        "text": { "tag": "plain_text", "content": btn_text },
                        "type": "primary",
                        "value": {
                            "action": "dir",
                            "op": "pick",
                            "chat_id": thread_key.chat_id,
                            "thread_id": thread_key.thread_id,
                            "directory": directory,
                        },
                    }
                ]
            }
        ]
    });
    vec![text_row, btn_row]
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

    /// A provider list of any size must never build a card over Feishu's
    /// ceilings: the `/model` picker chunks into pages under the byte budget.
    #[test]
    fn provider_cards_chunk_without_exceeding_feishu_limits() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        // A shared server advertising hundreds of providers.
        let providers: Vec<crate::opencode::client::ProviderModels> = (0..212)
            .map(|p| crate::opencode::client::ProviderModels {
                provider: format!("provider-{p}"),
                models: vec![crate::opencode::client::ModelOption {
                    id: "m".into(),
                    variants: Vec::new(),
                }],
            })
            .collect();
        let cards = build_model_provider_cards(&key, &providers);
        assert!(cards.len() > 1, "huge provider list must split: {}", cards.len());
        let mut total_buttons = 0;
        for card in &cards {
            let elements = card["body"]["elements"].as_array().unwrap();
            let buttons = elements.iter().filter(|e| e["tag"] == "button").count();
            total_buttons += buttons;
            assert!(
                buttons <= MAX_PICKER_BUTTONS_PER_CARD,
                "page over ceiling: {buttons}"
            );
            assert!(
                card.to_string().len() <= FEISHU_CARD_LIMIT_BYTES,
                "page over bytes"
            );
        }
        assert_eq!(total_buttons, providers.len(), "every provider gets a button");
    }

    /// The model picker for one provider carries a back button and honors the
    /// card budgets even for a provider with hundreds of models.
    #[test]
    fn model_picker_chunks_and_has_back_button() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let models: Vec<crate::opencode::client::ModelOption> = (0..400)
            .map(|i| crate::opencode::client::ModelOption {
                id: format!("model-{i}"),
                variants: Vec::new(),
            })
            .collect();
        let cards = build_model_picker_cards(&key, "opencode", &models);
        assert!(cards.len() > 1, "huge model list must split: {}", cards.len());
        let first = &cards[0];
        let buttons = first["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["tag"] == "button")
            .count();
        // The first card also carries the "back" button, so model buttons are
        // one fewer than the total button count.
        assert!(buttons - 1 <= MAX_PICKER_BUTTONS_PER_CARD);
        assert!(
            first.to_string().contains("返回全部 provider"),
            "model picker must offer a way back"
        );
        let payload = &first["body"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["tag"] == "button")
            .unwrap()["value"];
        assert_eq!(payload["level"].as_str(), Some("provider"), "back button level");
        assert_eq!(payload["value"].as_str(), Some("__providers__"));
        assert!(first.to_string().contains("opencode/model-0"));
        for card in &cards {
            assert!(card.to_string().len() <= FEISHU_CARD_LIMIT_BYTES);
        }
    }

    /// An empty provider list degrades to a single "no models" card, not zero.
    #[test]
    fn provider_cards_degrade_to_empty_intro() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let cards = build_model_provider_cards(&key, &[]);
        assert_eq!(cards.len(), 1);
        assert!(cards[0].to_string().contains("没有可用模型"));
    }

    /// The `/think` card lists the current model, its declared variants, and a
    /// "默认（清除）" button; each button carries the `think` action and the
    /// routing payload.
    #[test]
    fn think_card_lists_model_and_variants() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_think_card(
            &key,
            "opencode-go",
            "deepseek-v4-flash",
            Some("high"),
            &["low".into(), "high".into()],
        );
        let text = card.to_string();
        assert!(text.contains("思考等级"), "header: {text}");
        assert!(text.contains("opencode-go/deepseek-v4-flash"), "model: {text}");
        assert!(text.contains("当前思考等级"), "current label: {text}");
        assert!(text.contains("默认（清除）"), "default option: {text}");
        assert!(text.contains("\"value\":\"high\""), "variant button: {text}");
        assert!(text.contains("\"action\":\"think\""), "action tag: {text}");
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
    fn fmt_elapsed_formats_durations() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(42), "42s");
        assert_eq!(fmt_elapsed(83), "1m23s");
        assert_eq!(fmt_elapsed(3700), "1h1m");
        assert_eq!(fmt_elapsed(90_000), "1d1h");
    }

    #[test]
    fn loading_header_with_progress_shows_timer() {
        // Without progress inputs the header is the plain phase label (existing
        // builders/tests unchanged).
        let plain = CardBuilder::new().with_state(CardState::Loading).build();
        assert_eq!(plain["header"]["title"]["content"].as_str().unwrap(), "⏳ 思考中");
        // With a phase timer the active header counts up.
        let card = CardBuilder::new()
            .with_state(CardState::Loading)
            .with_progress(HeaderProgress {
                elapsed: Some(83),
                ..Default::default()
            })
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert!(header.contains("思考中"), "{}", header);
        assert!(header.contains("1m23s"), "timer missing: {}", header);
    }

    #[test]
    fn reasoning_header_shows_timer_and_length() {
        let card = CardBuilder::new()
            .with_state(CardState::Reasoning)
            .with_progress(HeaderProgress {
                elapsed: Some(83),
                reasoning_chars: 2100,
                ..Default::default()
            })
            .build();
        let header = card["header"]["title"]["content"].as_str().unwrap();
        assert!(header.contains("推理中"), "{}", header);
        assert!(header.contains("1m23s"), "timer missing: {}", header);
        assert!(header.contains("2.1k字"), "reasoning length missing: {}", header);
        assert_eq!(card["header"]["template"].as_str().unwrap(), "blue");
    }

    #[test]
    fn waiting_header_overrides_phase() {
        // A pending permission/question pauses the turn: the header says the
        // truth — it's waiting for the user, not stuck.
        let card = CardBuilder::new()
            .with_state(CardState::Reasoning)
            .with_progress(HeaderProgress {
                waiting: true,
                elapsed: Some(83),
                ..Default::default()
            })
            .build();
        assert_eq!(
            card["header"]["title"]["content"].as_str().unwrap(),
            "⏳ 等待你的授权/回答"
        );
        assert_eq!(card["header"]["template"].as_str().unwrap(), "orange");
    }

    #[test]
    fn header_changes_with_progress_and_state() {
        // The render poll flushes when the header changes: the timer tick and a
        // state change must each yield distinct headers.
        let header_of = |state: CardState, progress: HeaderProgress| {
            CardBuilder::new()
                .with_state(state)
                .with_progress(progress)
                .build()["header"]["title"]["content"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let tick = |elapsed: Option<u64>| HeaderProgress {
            elapsed,
            ..Default::default()
        };
        assert_ne!(
            header_of(CardState::Streaming, tick(Some(0))),
            header_of(CardState::Streaming, tick(Some(1))),
            "timer tick must change the header"
        );
        assert_ne!(
            header_of(CardState::Streaming, tick(Some(1))),
            header_of(
                CardState::Streaming,
                HeaderProgress {
                    waiting: true,
                    ..tick(Some(1))
                }
            ),
            "waiting must change the header"
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
            "🔐 权限请求"
        );
        // JSON 2.0: body.elements hold the markdown + buttons directly (no v1
        // action row).
        assert_eq!(card["schema"].as_str().unwrap(), "2.0");
        let elements = card["body"]["elements"].as_array().unwrap();
        // element 0 = description markdown, rest = buttons
        assert!(elements[0]["content"].as_str().unwrap().contains("AI 想要执行"));
        let buttons: Vec<_> = elements.iter().filter(|e| e["tag"] == "button").collect();
        assert_eq!(buttons.len(), 4);
        let values: Vec<_> = buttons.iter().map(|a| &a["value"]).collect();
        // Each button must carry the reply + request_id + session_id so the
        // card callback can route the answer back.
        let once = values.iter().find(|v| v["reply"] == "once").unwrap();
        assert_eq!(once["request_id"], "per_123");
        assert_eq!(once["session_id"], "ses_abc");
        assert_eq!(once["perm_label"], "✅ 已允许一次");
        assert_eq!(once["perm_color"], "green");
        assert!(once["perm_body"].as_str().unwrap().contains("bash"));
        let reject = values.iter().find(|v| v["reply"] == "reject").unwrap();
        assert_eq!(reject["perm_color"], "red");
        let always = values.iter().find(|v| v["reply"] == "always").unwrap();
        assert!(always["perm_label"].as_str().unwrap().contains("始终允许"));
        // The Auto-Accept toggle carries the session id so it can flip the flag.
        let autoaccept = values.iter().find(|v| v["reply"] == "autoaccept").unwrap();
        assert_eq!(autoaccept["session_id"], "ses_abc");
        assert_eq!(autoaccept["request_id"], "per_123");
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

    /// Regression: a tool whose input renders as markdown LIST lines (edit's
    /// `- old`/`+ new`, skill's `- name: ...`) must NOT swallow the Output
    /// marker as a lazy continuation of the last list line — Feishu then glues
    /// `**Output**` to the end of the input. A blank line between the sections
    /// keeps them on separate visual lines.
    #[test]
    fn tool_panel_input_and_output_separated_by_blank_line() {
        let tool = ToolPanel {
            name: "edit".into(),
            status: "completed".into(),
            input: Some(json!({
                "filePath": "src/main.rs",
                "oldString": "let a = 1;",
                "newString": "let a = 2;"
            })),
            output: Some("Edited file successfully: src/main.rs".into()),
        };
        let card = CardBuilder::new()
            .with_state(CardState::Done)
            .with_tool(tool)
            .build();
        let md = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .expect("panel markdown content");
        // The last input list line and the Output marker must not be adjacent —
        // a single newline would make `**Output**` a lazy continuation of the
        // `+ let a = 2;` list item and Feishu would render them on one line.
        assert!(
            md.contains("+ let a = 2;\n\n**Output**\n"),
            "Input and Output sections must be separated by a blank line: {:?}",
            md
        );
        assert!(
            !md.contains("+ let a = 2;\n**Output**"),
            "no glued marker: {:?}",
            md
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

    /// The `/help` card is a pure command manual: every command appears with a
    /// description, and there are NO buttons (no "试试"/"看卡", no `button`
    /// elements) — reading is its only job.
    #[test]
    fn help_card_is_a_buttonless_command_manual() {
        let card = build_help_card();
        let text = card.to_string();
        // Every top-level command is listed once.
        for cmd in [
            "/new",
            "/dir",
            "/switch",
            "/topic",
            "/name",
            "/agent",
            "/model",
            "/autoaccept",
            "/stop",
            "/compact",
            "/help",
            "/restart",
            "/restart-opencode",
            "/update",
        ] {
            assert!(text.contains(cmd), "missing command {cmd} in help card: {text}");
        }
        // Pure reference: no execution/preview buttons anywhere.
        assert!(
            !text.contains("试试") && !text.contains("看卡"),
            "help card must be buttonless: {text}"
        );
        assert!(
            !text.contains("\"tag\":\"button\"") && !text.contains("\"tag\": \"button\""),
            "help card must not embed buttons: {text}"
        );
    }

    /// The `/help` card must stay schema-V2-compatible: the `note` element is
    /// no longer supported (Feishu rejects the card with ErrCode 200861), which
    /// silently killed `/help` (the 400 only landed in the log). The footer hint
    /// renders as `markdown` instead.
    #[test]
    fn help_card_has_no_schema_v2_unsupported_note() {
        let card = build_help_card();
        let text = card.to_string();
        assert!(
            !text.contains("\"tag\":\"note\"") && !text.contains("\"tag\": \"note\""),
            "help card must not use the schema-V2-unsupported note element: {text}"
        );
    }

    /// The `/switch` session card must stay schema-V2-compatible: schema 2.0
    /// dropped the v1 `action` container (Feishu rejects the card with ErrCode
    /// 200861, "cards of schema V2 no longer support this capability"), which
    /// killed the `/switch` card (the 400 only landed in the log). Row buttons
    /// render as a nested `column_set` instead, one column per button.
    ///
    /// Each session entry is two rows: a full-width text row, then a button row
    /// beneath it — the text is NOT squeezed into a weighted column beside the
    /// buttons, which crushed the label/directory into many narrow wrapped
    /// lines on mobile.
    #[test]
    fn switch_card_has_no_schema_v2_unsupported_action_container() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let sessions = vec![crate::opencode::client::SessionListInfo {
            id: "ses_alpha01".into(),
            title: "重写登录".into(),
            directory: "/work/auth".into(),
            parent_id: None,
            agent: None,
            model: None,
            time: None,
        }];
        let card = build_switch_card(&key, &sessions, "", None, &[]);
        let text = card.to_string();
        assert!(
            !text.contains("\"tag\":\"action\"") && !text.contains("\"tag\": \"action\""),
            "switch card must not use the schema-V2-unsupported action container: {text}"
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let rows: Vec<&serde_json::Value> = elements.iter().filter(|e| e["tag"] == "column_set").collect();
        assert_eq!(
            rows.len(),
            2,
            "one text row + one button row per session entry: {text}"
        );
        let text_row_columns = rows[0]["columns"].as_array().unwrap();
        assert_eq!(
            text_row_columns.len(),
            1,
            "text row is a single full-width column: {text}"
        );
        assert_eq!(
            text_row_columns[0]["elements"][0]["tag"], "markdown",
            "text row renders the session label/directory: {text}"
        );
        let btn_columns = rows[1]["columns"].as_array().unwrap();
        assert_eq!(btn_columns.len(), 2, "both buttons side by side: {text}");
        assert_eq!(btn_columns[0]["elements"][0]["tag"], "button");
        assert_eq!(btn_columns[1]["elements"][0]["tag"], "button");
    }

    /// The `/dir` Recent Directories card must be schema-V2-compatible (no v1
    /// `action` container — ErrCode 200861), with one text row + one button row
    /// per directory, and each button carrying the pick routing payload.
    #[test]
    fn dir_card_has_no_schema_v2_unsupported_action_container() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs = vec!["/work/auth".to_string(), "/work/billing".to_string()];
        let card = build_dir_card(&key, &dirs, Some("/work/auth"));
        let text = card.to_string();
        assert!(
            !text.contains("\"tag\":\"action\"") && !text.contains("\"tag\": \"action\""),
            "dir card must not use the schema-V2-unsupported action container: {text}"
        );
        let elements = card["body"]["elements"].as_array().unwrap();
        let rows: Vec<&serde_json::Value> = elements.iter().filter(|e| e["tag"] == "column_set").collect();
        assert_eq!(
            rows.len(),
            4,
            "one text row + one button row per directory: {text}"
        );
        // The second entry's button carries the pick payload for its directory.
        let btn = &rows[3]["columns"][0]["elements"][0];
        assert_eq!(btn["tag"], "button");
        assert_eq!(btn["value"]["action"], "dir");
        assert_eq!(btn["value"]["op"], "pick");
        assert_eq!(btn["value"]["directory"], "/work/billing");
        assert_eq!(btn["value"]["chat_id"], "chat_1");
        assert_eq!(btn["value"]["thread_id"], "chat_1");
        // The current directory is marked, and its row reads "当前" not "切换".
        let current_text = rows[0]["columns"][0]["elements"][0]["content"].as_str().unwrap();
        assert!(
            current_text.contains("当前"),
            "current dir marked: {current_text}"
        );
        assert_eq!(
            rows[1]["columns"][0]["elements"][0]["text"]["content"], "✅ 当前",
            "current dir button reads 当前: {text}"
        );
        assert_eq!(
            rows[3]["columns"][0]["elements"][0]["text"]["content"], "切换到这里",
            "non-current dir button reads 切换到这里: {text}"
        );
    }

    /// An empty Recent Directories list renders a hint and no buttons.
    #[test]
    fn dir_card_empty_state_is_buttonless_hint() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let card = build_dir_card(&key, &[], None);
        let text = card.to_string();
        assert!(text.contains("还没有最近目录"), "empty hint: {text}");
        assert!(
            !text.contains("\"tag\":\"button\"") && !text.contains("\"tag\": \"button\""),
            "empty dir card has no buttons: {text}"
        );
    }

    /// More than `MAX_SWITCH_ROWS` recent directories render only the most
    /// recent six — the rest are silently dropped (same cap as the `/switch`
    /// card).
    #[test]
    fn dir_card_caps_rows_at_max_switch_rows() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs: Vec<String> = (1..=9).map(|i| format!("/work/proj{i}")).collect();
        let card = build_dir_card(&key, &dirs, None);
        let text = card.to_string();
        let elements = card["body"]["elements"].as_array().unwrap();
        let rows: Vec<&serde_json::Value> = elements.iter().filter(|e| e["tag"] == "column_set").collect();
        assert_eq!(
            rows.len(),
            MAX_SWITCH_ROWS * 2,
            "one text row + one button row for each of the capped six: {text}"
        );
        assert!(text.contains("/work/proj1"), "most recent shown: {text}");
        assert!(text.contains("/work/proj6"), "sixth most recent shown: {text}");
        assert!(
            !text.contains("/work/proj7"),
            "seventh+ directory dropped: {text}"
        );
        assert!(
            text.contains("还有 3 个最近目录未显示"),
            "truncated count hints at the fallback: {text}"
        );
        assert!(
            text.contains("/switch") && text.contains("接管") && text.contains("/new"),
            "hint encodes the switch-then-new flow: {text}"
        );
    }

    /// A Recent Directories card that fits under the cap shows no truncation
    /// hint.
    #[test]
    fn dir_card_under_cap_has_no_truncation_hint() {
        let key = crate::config::ThreadKey::new("chat_1".into(), "chat_1".into());
        let dirs = vec!["/work/auth".to_string(), "/work/billing".to_string()];
        let card = build_dir_card(&key, &dirs, None);
        let text = card.to_string();
        assert!(
            !text.contains("还有") && !text.contains("未显示"),
            "no truncation hint under the cap: {text}"
        );
    }
}
