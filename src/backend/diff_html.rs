//! The generic **Diff Document** and its self-contained HTML artifact
//! (spec #797, ticket #798).
//!
//! A Turn Diff (#799) and a future repository review (#803) are both "a set of
//! files with unified patches", so the artifact generator takes this neutral
//! [`DiffDocument`], never a turn-specific type. The renderer is a pure
//! function: no I/O, no async, and every repository-derived string goes through
//! [`escape_html`] — the artifact is opened in a user's browser, so the
//! security invariant is absolute.
//!
//! The HTML carries **no script, no inline event handler and no external
//! resource**: CSS is inline and the only font faces are the system monospace
//! stack. Feishu renders a `.html` File Message as a web page on desktop and
//! mobile; this module owns the file's body, while the delivery path owns
//! upload and send.

use crate::backend::{TurnDiff, TurnDiffFile};

/// The most bytes the rendered artifact may occupy: 8 MB. Feishu's File
/// Message preview tolerance was measured at 12 MB (spec #797), and the margin
/// absorbs client-side decoding and framing. Past this bound files are omitted
/// **with a visible note**, never silently truncated.
pub const MAX_DIFF_HTML_BYTES: usize = 8 * 1024 * 1024;

/// One file in a [`DiffDocument`]: the repository-relative path, the unified
/// patch kept verbatim, the source's reported line counts and its status
/// spelling (`modified` / `added` / `deleted`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
    pub status: String,
}

/// The one document both diff surfaces map into: a turn's changes or a
/// repository review's. `title`/`subtitle` name the source (turn time, base
/// branch, "按工具记录聚合", …) and `truncated` carries a caller-supplied
/// omission note; the renderer appends its own note when the size cap drops
/// files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffDocument {
    pub title: String,
    pub subtitle: String,
    pub files: Vec<DiffFile>,
    pub truncated: Option<String>,
}

impl DiffDocument {
    /// Map the neutral Turn Diff (#799) into the generic document. The header
    /// is the caller's: the turn diff type carries the files, not when the
    /// turn happened or which session/project it belongs to.
    pub fn from_turn_diff(
        turn_diff: TurnDiff,
        title: impl Into<String>,
        subtitle: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            subtitle: subtitle.into(),
            files: turn_diff.0.into_iter().map(DiffFile::from).collect(),
            truncated: None,
        }
    }
}

impl From<TurnDiffFile> for DiffFile {
    fn from(file: TurnDiffFile) -> Self {
        Self {
            path: file.path,
            patch: file.patch,
            additions: file.additions,
            deletions: file.deletions,
            status: file.status,
        }
    }
}

/// Escape every character with meaning in HTML text or a quoted attribute.
/// This is the **only** function repository-derived text may pass through
/// before it enters the artifact; a bare `format!` with a path, patch line,
/// status or stat is a security bug.
pub fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Render `doc` as a self-contained HTML page: one `<details>` per file (the
/// first open), one `<pre>` per hunk with old/new line numbers and red/green
/// line classes, and a dark-mode variable set.
pub fn render_diff_html(doc: &DiffDocument) -> String {
    let head = render_head(doc);
    if doc.files.is_empty() {
        return format!("{head}<p class=\"empty\">没有文件改动</p>\n{CLOSING}");
    }

    // Reserve the closing tag, a caller-supplied omission note and room for the
    // cap note BEFORE accepting a file block: a file is included whole or not
    // at all, so the artifact never ends mid-file and never exceeds the cap.
    let caller_note = doc.truncated.as_deref().map(note_html).unwrap_or_default();
    let budget =
        MAX_DIFF_HTML_BYTES.saturating_sub(head.len() + caller_note.len() + CLOSING.len() + CAP_NOTE_RESERVE);

    let mut body = String::new();
    let mut included = 0usize;
    for (index, file) in doc.files.iter().enumerate() {
        let block = render_file(file, index == 0);
        if body.len() + block.len() > budget {
            break;
        }
        body.push_str(&block);
        included += 1;
    }

    let omitted = doc.files.len() - included;
    let mut notes = caller_note;
    if omitted > 0 {
        notes.push_str(&note_html(&format!("已省略 {omitted} 个文件（超出预览上限）")));
    }
    format!("{head}{body}{notes}{CLOSING}")
}

/// The closing of every page. Kept out of the head so the cap can count it.
const CLOSING: &str = "</main>\n</body>\n</html>\n";

/// Space reserved for the renderer's own omission note + an escape-grown
/// caller note. Far above the note's real size, and tiny against 8 MB.
const CAP_NOTE_RESERVE: usize = 1024;

/// The page shell and header: doctype, the inline style sheet, the title and
/// the `N files · +A −D` totals. Every dynamic field is escaped.
fn render_head(doc: &DiffDocument) -> String {
    let title = escape_html(&doc.title);
    let subtitle = escape_html(&doc.subtitle);
    let additions: usize = doc.files.iter().map(|file| file.additions).sum();
    let deletions: usize = doc.files.iter().map(|file| file.deletions).sum();
    let files = escape_html(&doc.files.len().to_string());
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"zh-CN\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <style>\n{CSS}\n</style>\n\
         </head>\n\
         <body>\n\
         <main>\n\
         <header class=\"doc\">\n\
         <h1>{title}</h1>\n\
         <p class=\"subtitle\">{subtitle}</p>\n\
         <p class=\"stats\">{files} files · <span class=\"add\">+{}</span> <span class=\"del\">−{}</span></p>\n\
         </header>\n",
        escape_html(&additions.to_string()),
        escape_html(&deletions.to_string()),
    )
}

/// One file's fold. The first file opens; every later one starts folded.
fn render_file(file: &DiffFile, open: bool) -> String {
    let mut html = String::from("<details class=\"file\"");
    if open {
        html.push_str(" open");
    }
    html.push_str(">\n<summary><span class=\"path\">");
    html.push_str(&escape_html(&file.path));
    html.push_str("</span><span class=\"counts\"><span class=\"add\">+");
    html.push_str(&escape_html(&file.additions.to_string()));
    html.push_str("</span> <span class=\"del\">−");
    html.push_str(&escape_html(&file.deletions.to_string()));
    html.push_str("</span></span>");
    if !file.status.is_empty() {
        html.push_str("<span class=\"status\">");
        html.push_str(&escape_html(&file.status));
        html.push_str("</span>");
    }
    html.push_str("</summary>\n");
    for hunk in parse_hunks(&file.patch) {
        html.push_str(&render_hunk(&hunk));
    }
    html.push_str("</details>\n");
    html
}

/// One hunk (or a loose run of non-header lines) as a `<pre>`: the `@@` header,
/// then one block row per line with old/new line-number columns and the source
/// line verbatim, prefix included.
fn render_hunk(hunk: &Hunk) -> String {
    let mut html = String::from("<pre class=\"hunk\">");
    if !hunk.header.is_empty() {
        html.push_str(
            "<span class=\"line hunk\"><span class=\"ln old\"></span><span class=\"ln new\"></span><span class=\"code\">",
        );
        html.push_str(&escape_html(&hunk.header));
        html.push_str("</span></span>");
    }
    for line in &hunk.lines {
        let class = match line.kind {
            LineKind::Context => "ctx",
            LineKind::Added => "add",
            LineKind::Removed => "del",
            LineKind::Note => "note",
            LineKind::Other => "other",
        };
        html.push_str("<span class=\"line ");
        html.push_str(class);
        html.push_str("\"><span class=\"ln old\">");
        if let Some(number) = line.old {
            html.push_str(&escape_html(&number.to_string()));
        }
        html.push_str("</span><span class=\"ln new\">");
        if let Some(number) = line.new {
            html.push_str(&escape_html(&number.to_string()));
        }
        html.push_str("</span><span class=\"code\">");
        html.push_str(&escape_html(&line.text));
        html.push_str("</span></span>");
    }
    html.push_str("</pre>\n");
    html
}

fn note_html(note: &str) -> String {
    format!("<p class=\"note\">{}</p>\n", escape_html(note))
}

/// The class of one parsed patch line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Context,
    Added,
    Removed,
    /// The `\ No newline at end of file` marker.
    Note,
    /// Text that is neither a hunk line nor one of the four file headers.
    Other,
}

struct ParsedLine {
    kind: LineKind,
    text: String,
    old: Option<usize>,
    new: Option<usize>,
}

struct Hunk {
    /// The raw `@@ … @@` line, empty for a loose run outside a hunk.
    header: String,
    lines: Vec<ParsedLine>,
}

/// Split one file's patch into hunks, KEEPING the `@@` header, every context
/// line and every `+`/`-` line verbatim (prefix included), and dropping only
/// the file headers `Index:`, `===`, `---` and `+++`.
///
/// The old/new line counters come from the `@@` header and end the hunk when
/// both counts are spent: after that a `+++` is a header again, never a
/// `+`-prefixed addition. Before the first hunk those four prefixes are always
/// headers, which is what a V1 `recorded_diff` starts with. A blank line is
/// not a file header and is preserved rather than silently dropped.
fn parse_hunks(patch: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut in_hunk = false;
    let mut loose_open = false;
    let mut old_remaining = 0usize;
    let mut new_remaining = 0usize;
    let mut old_line = 0usize;
    let mut new_line = 0usize;

    for raw in patch.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);

        if line.starts_with("@@") {
            let (old_start, old_count, new_start, new_count) =
                parse_hunk_header(line).unwrap_or((1, 1, 1, 1));
            old_line = old_start;
            new_line = new_start;
            old_remaining = old_count;
            new_remaining = new_count;
            in_hunk = true;
            loose_open = false;
            hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }

        // The no-newline marker belongs to the hunk even when its counts are
        // already spent.
        if in_hunk && line.starts_with('\\') {
            if let Some(hunk) = hunks.last_mut() {
                hunk.lines.push(ParsedLine {
                    kind: LineKind::Note,
                    text: line.to_string(),
                    old: None,
                    new: None,
                });
            }
            continue;
        }

        if in_hunk && old_remaining == 0 && new_remaining == 0 {
            in_hunk = false;
        }

        if !in_hunk {
            if is_file_header(line) {
                continue;
            }
            if !loose_open {
                hunks.push(Hunk {
                    header: String::new(),
                    lines: Vec::new(),
                });
                loose_open = true;
            }
            if let Some(hunk) = hunks.last_mut() {
                hunk.lines.push(ParsedLine {
                    kind: LineKind::Other,
                    text: line.to_string(),
                    old: None,
                    new: None,
                });
            }
            continue;
        }

        let (kind, old, new) = if line.starts_with('+') {
            let number = new_line;
            new_line += 1;
            new_remaining = new_remaining.saturating_sub(1);
            (LineKind::Added, None, Some(number))
        } else if line.starts_with('-') {
            let number = old_line;
            old_line += 1;
            old_remaining = old_remaining.saturating_sub(1);
            (LineKind::Removed, Some(number), None)
        } else if line.starts_with(' ') {
            let old_number = old_line;
            let new_number = new_line;
            old_line += 1;
            new_line += 1;
            old_remaining = old_remaining.saturating_sub(1);
            new_remaining = new_remaining.saturating_sub(1);
            (LineKind::Context, Some(old_number), Some(new_number))
        } else {
            (LineKind::Other, None, None)
        };
        if let Some(hunk) = hunks.last_mut() {
            hunk.lines.push(ParsedLine {
                kind,
                text: line.to_string(),
                old,
                new,
            });
        }
    }

    hunks
}

/// Whether a line outside a hunk is one of the four file headers the artifact
/// drops. `---`/`+++` need their trailing space (or be bare): inside a hunk a
/// removed line whose text begins `-- ` renders as `--- …` and is content, and
/// this predicate is never asked about a line inside a hunk.
fn is_file_header(line: &str) -> bool {
    line.starts_with("Index:")
        || line.starts_with("===")
        || line == "---"
        || line.starts_with("--- ")
        || line == "+++"
        || line.starts_with("+++ ")
}

/// Parse `@@ -old_start[,old_count] +new_start[,new_count] @@` into its ranges.
/// An omitted count means 1, as in unified diff's grammar.
fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "@@" {
        return None;
    }
    let old = parse_hunk_range(parts.next()?)?;
    let new = parse_hunk_range(parts.next()?)?;
    Some((old.0, old.1, new.0, new.1))
}

fn parse_hunk_range(range: &str) -> Option<(usize, usize)> {
    let rest = range.strip_prefix('-').or_else(|| range.strip_prefix('+'))?;
    match rest.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((rest.parse().ok()?, 1)),
    }
}

/// The artifact's inline style sheet. No external font or resource, and no
/// event-handler declaration; the light/dark palettes are one variable set
/// switched by `prefers-color-scheme` (spec #797's measured requirement).
const CSS: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #ffffff;
  --fg: #1f2328;
  --muted: #59636e;
  --border: #d1d9e0;
  --panel: #f6f8fa;
  --add-bg: #e6ffec;
  --add-fg: #116329;
  --del-bg: #ffebe9;
  --del-fg: #a40e26;
  --hunk-bg: #eef4fb;
  --ln-fg: #6e7781;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d1117;
    --fg: #e6edf3;
    --muted: #9198a1;
    --border: #30363d;
    --panel: #161b22;
    --add-bg: #12261e;
    --add-fg: #7ee787;
    --del-bg: #2d1214;
    --del-fg: #ffa198;
    --hunk-bg: #161b22;
    --ln-fg: #8b949e;
  }
}
* { box-sizing: border-box; }
html { background: var(--bg); }
body {
  margin: 0;
  padding: 24px;
  background: var(--bg);
  color: var(--fg);
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", sans-serif;
  line-height: 1.5;
}
main { max-width: 1200px; margin: 0 auto; }
header.doc { margin-bottom: 20px; }
h1 { margin: 0 0 4px; font-size: 20px; font-weight: 600; }
.subtitle { margin: 0; color: var(--muted); font-size: 13px; }
.stats { margin: 8px 0 0; font-size: 14px; }
.add { color: var(--add-fg); }
.del { color: var(--del-fg); }
.empty, .note {
  padding: 16px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel);
}
.note { margin-top: 12px; color: var(--muted); }
details.file {
  margin: 12px 0;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--bg);
  overflow: hidden;
}
details.file > summary {
  padding: 10px 12px;
  background: var(--panel);
  border-bottom: 1px solid var(--border);
  cursor: pointer;
}
.path {
  margin-right: 8px;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
  font-size: 13px;
  font-weight: 600;
  overflow-wrap: anywhere;
}
.counts {
  margin-right: 8px;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
  font-size: 13px;
}
.status { color: var(--muted); font-size: 12px; }
pre.hunk {
  margin: 0;
  padding: 0;
  overflow-x: auto;
  background: var(--hunk-bg);
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
  font-size: 12px;
  line-height: 1.55;
  white-space: pre;
}
.line {
  display: block;
  width: max-content;
  min-width: 100%;
  white-space: pre;
}
.line.add { background: var(--add-bg); }
.line.del { background: var(--del-bg); }
.line.hunk { background: var(--hunk-bg); color: var(--muted); }
.line.note { color: var(--muted); }
.ln {
  display: inline-block;
  width: 4em;
  padding: 0 8px;
  text-align: right;
  color: var(--ln-fg);
  user-select: none;
}
.code { display: inline-block; padding-right: 16px; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, patch: &str, additions: usize, deletions: usize) -> DiffFile {
        DiffFile {
            path: path.to_string(),
            patch: patch.to_string(),
            additions,
            deletions,
            status: "modified".to_string(),
        }
    }

    fn doc(files: Vec<DiffFile>) -> DiffDocument {
        DiffDocument {
            title: "本次改动".to_string(),
            subtitle: "session ses_1 · 2026-10-19 12:00".to_string(),
            files,
            truncated: None,
        }
    }

    /// Acceptance: every repository-derived string is escaped, and the
    /// artifact never carries a script or an event-handler attribute.
    #[test]
    fn injection_is_escaped_and_no_script_survives() {
        let html = render_diff_html(&DiffDocument {
            title: "<script>title</script>".to_string(),
            subtitle: "</style><script>alert(0)</script>".to_string(),
            files: vec![DiffFile {
                path: "<script>alert(1)</script>.rs".to_string(),
                patch: "@@ -1,1 +1,1 @@\n-</style><script>alert(2)</script>\n+&quot; and it's\n".to_string(),
                additions: 1,
                deletions: 1,
                status: "modified".to_string(),
            }],
            truncated: Some("<script>note</script>".to_string()),
        });

        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;.rs"));
        assert!(html.contains("&lt;/style&gt;&lt;script&gt;alert(2)&lt;/script&gt;"));
        assert!(html.contains("+&amp;quot;"));
        assert!(html.contains("it&#39;s"));
        assert!(html.contains("&lt;script&gt;title&lt;/script&gt;"));
        assert!(html.contains("&lt;script&gt;note&lt;/script&gt;"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("onerror="));
        assert!(!html.contains("onclick="));
        assert!(!html.contains("<link"));
        assert!(!html.contains("src="));
    }

    /// Acceptance: two files and multiple hunks — one `<details>` per file, the
    /// first open, the reported stats unchanged, and context lines present.
    #[test]
    fn two_files_with_multiple_hunks_keep_stats_and_context() {
        let first = file(
            "src/a.rs",
            "Index: src/a.rs\n===\n--- a/src/a.rs\n+++ b/src/a.rs\n\
             @@ -1,3 +1,4 @@\n context one\n-old\n+new\n+added\n context two\n\
             @@ -10,1 +11,1 @@\n-remove2\n+add2\n",
            3,
            2,
        );
        let second = file("src/b.rs", "@@ -5,1 +5,2 @@\n same\n+plus\n", 1, 0);
        let html = render_diff_html(&doc(vec![first, second]));

        assert_eq!(html.matches("<details").count(), 2);
        assert_eq!(html.matches("<details class=\"file\" open>").count(), 1);
        // The per-file counts (`+A −D`) match the input.
        assert!(html.contains(">+3<"));
        assert!(html.contains(">−2<"));
        assert!(html.contains(">+1<"));
        assert!(html.contains(">−0<"));
        // The document totals (`N files · +A −D`).
        assert!(html.contains("2 files"));
        assert!(html.contains(">+4<"));
        // Context lines and every hunk header survive.
        assert!(html.contains(" context one"));
        assert!(html.contains(" context two"));
        assert!(html.contains(" same"));
        assert!(html.contains("@@ -1,3 +1,4 @@"));
        assert!(html.contains("@@ -10,1 +11,1 @@"));
        assert!(html.contains("@@ -5,1 +5,2 @@"));
        // File headers are the only dropped lines.
        assert!(!html.contains("Index:"));
        assert!(!html.contains("==="));
        assert!(!html.contains("--- a/src/a.rs"));
        assert!(!html.contains("+++ b/src/a.rs"));
    }

    /// The four header prefixes are only headers OUTSIDE a hunk: a removed
    /// line whose text begins `-- ` renders as `--- …`, and an added line
    /// whose text begins `++ ` renders as `+++ …`. Both are content.
    #[test]
    fn header_lookalikes_inside_a_hunk_are_content() {
        let html = render_diff_html(&doc(vec![file(
            "src/a.rs",
            "Index: src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
             @@ -1,1 +1,1 @@\n--- old comment\n+++ new comment\n",
            1,
            1,
        )]));

        assert!(html.contains("--- old comment"));
        assert!(html.contains("+++ new comment"));
        assert!(html.contains("class=\"line del\""));
        assert!(html.contains("class=\"line add\""));
        assert!(!html.contains("--- a/src/a.rs"));
        assert!(!html.contains("+++ b/src/a.rs"));
    }

    /// Acceptance: a patch past the cap drops whole files in order, says so
    /// visibly, and the artifact itself stays under the cap.
    #[test]
    fn over_cap_omits_files_visibly_and_stays_under_the_cap() {
        let half = "x".repeat(5 * 1024 * 1024);
        let patch = format!("@@ -1,1 +1,1 @@\n+{half}\n");
        let first = file("src/big-one.rs", &patch, 1, 0);
        let second = file("src/big-two.rs", &patch, 1, 0);

        let html = render_diff_html(&doc(vec![first, second]));

        assert_eq!(html.matches("<details").count(), 1);
        assert!(html.contains("src/big-one.rs"));
        assert!(!html.contains("src/big-two.rs"));
        assert!(html.contains("已省略 1 个文件（超出预览上限）"));
        assert!(html.len() < MAX_DIFF_HTML_BYTES);
    }

    /// Acceptance: an empty diff is a stated page, not an empty file and not a
    /// panic.
    #[test]
    fn empty_diff_renders_a_stated_page() {
        let html = render_diff_html(&doc(Vec::new()));
        assert!(html.contains("没有文件改动"));
        assert!(!html.contains("<details"));
    }

    /// Acceptance: a structural dark-mode variable set exists.
    #[test]
    fn dark_mode_variables_are_present() {
        let html = render_diff_html(&doc(vec![file("src/a.rs", "@@ -1 +1 @@\n-a\n+b\n", 1, 1)]));
        assert!(html.contains("prefers-color-scheme: dark"));
    }

    /// Acceptance: `TurnDiff` maps into the shared document without a
    /// turn-specific generator.
    #[test]
    fn turn_diff_maps_into_a_diff_document() {
        let turn = TurnDiff(vec![
            TurnDiffFile {
                path: "src/a.rs".to_string(),
                patch: "@@ -1 +1 @@\n-a\n+b\n".to_string(),
                additions: 1,
                deletions: 1,
                status: "modified".to_string(),
            },
            TurnDiffFile {
                path: "src/new.rs".to_string(),
                patch: "@@ -0,0 +1,1 @@\n+hello\n".to_string(),
                additions: 1,
                deletions: 0,
                status: "added".to_string(),
            },
        ]);

        let document = DiffDocument::from_turn_diff(turn, "本次改动", "session ses_1 · 12:00");

        assert_eq!(document.title, "本次改动");
        assert_eq!(document.subtitle, "session ses_1 · 12:00");
        assert_eq!(document.files.len(), 2);
        assert_eq!(document.files[0].path, "src/a.rs");
        assert_eq!(document.files[0].status, "modified");
        assert_eq!(document.files[1].path, "src/new.rs");
        assert_eq!(document.files[1].status, "added");
        assert_eq!(document.truncated, None);

        let html = render_diff_html(&document);
        assert!(html.contains("src/a.rs"));
        assert!(html.contains("src/new.rs"));
        assert!(html.contains(">added<"));
    }
}
