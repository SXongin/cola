use super::shell::collapsible_panel;
use super::{fenced_code, truncate_md};

/// Cap for a tool's OUTPUT shown inside its collapsible panel. Higher than the
/// old 800: code-wrapped output renders without line wrapping, so a file or
/// command output can be meaningfully long.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 3000;

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
    pub(super) fn status_icon(&self) -> &'static str {
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
pub(super) fn tool_panel_element(tool: &ToolPanel) -> serde_json::Value {
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
        // File content (read) and edit hunks render as a fenced code block, as
        // does anything with long lines: Feishu markdown wraps plain text but
        // not code blocks, so long file/command output stays on one visual line
        // instead of folding.
        if tool.name == "read" || tool.name == "edit" || needs_code_block(&body) {
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
}

/// Parse a unified diff produced by OpenCode's `edit` tool into its hunks.
/// Returns `None` when `diff` isn't a recognizable edit diff (plain text
/// output, an error message), so callers fall back to showing it unchanged.
pub(crate) fn parse_edit_diff(diff: &str) -> Option<EditDiff> {
    let mut path = None;
    let mut additions = 0usize;
    let mut deletions = 0usize;
    let mut body = String::new();
    let mut changed = false;
    // The `--- path` / `+++ path` file headers only appear BEFORE the first
    // `@@` hunk. Past that point any `-`/`+` line is real content — a removed
    // line whose text starts with `-- ` (Lua/YAML `--` comments) renders as
    // `--- …` and must stay a removal, not be mistaken for a header.
    let mut in_hunk = false;
    for line in diff.lines() {
        let l = line.trim_end();
        if let Some(p) = l.strip_prefix("Index: ") {
            path = Some(p.to_string());
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
    })
}

/// Render a tool's raw output string human-friendly. OpenCode's `read` tool
/// wraps its output in XML tags (`<path>…</path>`, `<type>…</type>`,
/// `<content>…</content>`); strip them so the card shows just the file path and
/// the numbered lines instead of raw markup. An `edit` tool's output is its
/// unified diff (substituted by the renderer) — keep only the hunks and report
/// the change count. Other tools pass through unchanged.
///
/// Returns `(header, language hint, body)`: the header (the file-path line) is
/// markdown; the body is shown as a code block so long lines don't wrap.
fn format_tool_output(name: &str, output: &str) -> (Option<String>, Option<&'static str>, String) {
    if name == "edit" {
        // The panel input already shows the target file, so the header is just
        // the change count; the hunks render as a code block.
        return match parse_edit_diff(output) {
            Some(d) => (Some(format!("+{} −{}", d.additions, d.deletions)), None, d.body),
            None => (None, None, output.to_string()),
        };
    }
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
    use crate::feishu::card::{CardBuilder, CardState};
    use serde_json::json;

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
        let (header, lang, body) = format_tool_output("edit", diff);
        assert_eq!(header.as_deref(), Some("+1 −1"), "count header: {:?}", header);
        assert_eq!(lang, None, "edit hunks have no language hint");
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
        let (header, lang, body) = format_tool_output("edit", raw);
        assert_eq!(header, None);
        assert_eq!(lang, None);
        assert_eq!(body, raw);
        assert_eq!(parse_edit_diff(raw), None);
    }

    #[test]
    fn edit_tool_output_renders_diff_in_panel() {
        let tool = ToolPanel {
            name: "edit".into(),
            status: "completed".into(),
            input: Some(json!({"filePath": "src/main.rs"})),
            output: Some(
                "\
Index: src/main.rs
===================================================================
--- src/main.rs
+++ src/main.rs
@@ -1,4 +1,4 @@
 use std::fs;
-fn main() {}
+fn main() { println!(\"hi\"); }
"
                .into(),
            ),
        };
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
        assert!(md.contains("**Output**\n+1 −1"), "count header: {}", md);
        // Hunks render inside a fenced code block (monospace, no wrapping).
        assert!(md.contains("```\n@@ -1,4 +1,4 @@"), "fenced hunk: {}", md);
        assert!(md.contains("-fn main() {}"), "removed line: {}", md);
        assert!(md.contains("+fn main() { println"), "added line: {}", md);
        assert!(!md.contains("use std::fs;"), "context line dropped: {}", md);
        assert!(md.contains("\n```"), "closing fence: {}", md);
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
    fn tool_input_edit_shows_only_the_file() {
        // The oldString/newString preview (the first line of each, both marked
        // -/+) read as duplicated content and hid the real change, which now
        // comes from the tool's diff output. The input shows the target file
        // and nothing else.
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

    /// Regression: a tool whose input renders as markdown LIST lines (the
    /// generic fallback `- name: ...`, a skill's metadata list) must NOT swallow
    /// the Output marker as a lazy continuation of the last list line — Feishu
    /// then glues `**Output**` to the end of the input. A blank line between
    /// the sections keeps them on separate visual lines.
    #[test]
    fn tool_panel_input_and_output_separated_by_blank_line() {
        let tool = ToolPanel {
            name: "skill_apply".into(),
            status: "completed".into(),
            input: Some(json!({
                "skill": "m15",
            })),
            output: Some("applied".into()),
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
