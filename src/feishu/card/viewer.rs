//! The **File Viewer** card (ADR-0083 §3, §5): one **Host** file shown as a
//! command card — a standalone message that joins no **Card Chain**, is never
//! written to the **Chain Record** and is never reaped, exactly like `/dir`,
//! `/switch` and `/help`.
//!
//! The card names the file it shows — the **resolved** path, its size and its
//! type — so two similarly named files are never ambiguous, and it shapes the
//! preview by what the bytes *are* (§6): a text file is a fenced code block
//! carrying the extension's language token (or `plain_text` when the platform
//! has no token for it), a delimited-text file (`.csv`/`.tsv`) is a markdown
//! table, because that is what those bytes are. Everything is held to this
//! card family's budget: one fenced block per element (never a half-open
//! fence), the table capped at the platform's row limit, and the whole preview
//! within [`MAX_CARD_JSON_CHARS`] — a preview that would outgrow the card says
//! it was cut instead of shipping a rejected card.
//!
//! It carries 「📂 所在目录」 (open the Directory Browser at the file's parent)
//! and, when it was opened *from* a browse listing, 「← 返回目录」 with that
//! listing's whole state — both payloads are the browse handler's own
//! [`BrowseView`] shape, so neither can drift from the browse card (#742 wires
//! the row that passes `return_to`; #739/#740/#741/#744 extend the render).

use serde_json::json;

use crate::backend::file_content::human_size;
use crate::config::ThreadKey;

use super::sanitize::{CardMarkdown, MAX_TABLE_ROWS};
use super::session::{BrowseView, browse_view_value};
use super::shell::card_shell;
use super::{MAX_CARD_JSON_CHARS, MAX_ELEMENT_TEXT_CHARS, chunk_text, fenced_code};

/// What the viewer decided a file's bytes are (§6). #739 adds the image and
/// binary kinds beside these; #738 answers for the two text shapes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ViewerKind {
    /// UTF-8 text: a fenced code block with the extension's language token.
    Text,
    /// Delimited text (`.csv`/`.tsv`): a markdown table.
    Delimited,
}

impl ViewerKind {
    /// The type the card names, alongside the extension.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Text => "文本",
            Self::Delimited => "表格",
        }
    }
}

/// What a file's extension declares: `.csv`/`.tsv` are delimited text,
/// everything else is text (the magic-byte lanes are #739's).
pub(crate) fn kind_for_extension(ext: Option<&str>) -> ViewerKind {
    match ext.map(str::to_ascii_lowercase).as_deref() {
        Some("csv" | "tsv") => ViewerKind::Delimited,
        _ => ViewerKind::Text,
    }
}

/// What the platform renders when a code block carries no language it knows:
/// Feishu's own Plain Text token (research §1.3). The viewer asks for it by
/// name for an extension it has no token for, so the degradation is visible in
/// the card rather than an unlabelled fence.
pub(crate) const PLAIN_TEXT: &str = "plain_text";

/// Every language token Feishu's fenced code blocks accept, verbatim from the
/// docs (research §1.3). The runtime never needs the whole list — the map below
/// is what it reads; this copy exists so the test can prove every mapped token
/// is one the platform documents and the table cannot invent one.
#[cfg(test)]
const FEISHU_CODE_TOKENS: &[&str] = &[
    "plain_text",
    "abap",
    "ada",
    "apache",
    "apex",
    "assembly",
    "bash",
    "c_sharp",
    "cpp",
    "c",
    "cmake",
    "cobol",
    "css",
    "coffee_script",
    "d",
    "dart",
    "delphi",
    "diff",
    "django",
    "docker_file",
    "erlang",
    "fortran",
    "gherkin",
    "go",
    "graphql",
    "groovy",
    "html",
    "htmlbars",
    "http",
    "haskell",
    "json",
    "java",
    "javascript",
    "julia",
    "kotlin",
    "latex",
    "lisp",
    "lua",
    "matlab",
    "makefile",
    "markdown",
    "nginx",
    "objective_c",
    "opengl_shading_language",
    "php",
    "perl",
    "powershell",
    "prolog",
    "properties",
    "protobuf",
    "python",
    "r",
    "ruby",
    "rust",
    "sas",
    "scss",
    "sql",
    "scala",
    "scheme",
    "shell",
    "solidity",
    "swift",
    "toml",
    "thrift",
    "typescript",
    "vbscript",
    "visual_basic",
    "xml",
    "yaml",
];

/// The extension → language-token table (lowercased extension, no dot).
/// Deliberately absent — they render as [`PLAIN_TEXT`] — are the extensions
/// whose token would be a guess: `.ts`/`.tsx`/`.jsx` (a `.ts` file may be a
/// TypeScript source, an MPEG transport stream or a translation table), `.log`,
/// `.txt`, `.ini`, `.cfg`, `.conf`, `.env` (no token exists) and everything
/// else unknown. An absent extension is never forced into a near-miss
/// spelling: honest plain text beats a highlight that lies.
const EXT_LANGUAGE: &[(&str, &str)] = &[
    ("md", "markdown"),
    ("markdown", "markdown"),
    ("rs", "rust"),
    ("py", "python"),
    ("pyi", "python"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("ts", PLAIN_TEXT),
    ("tsx", PLAIN_TEXT),
    ("jsx", PLAIN_TEXT),
    ("go", "go"),
    ("java", "java"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("swift", "swift"),
    ("c", "c"),
    ("h", "c"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("hpp", "cpp"),
    ("hh", "cpp"),
    ("cs", "c_sharp"),
    ("rb", "ruby"),
    ("php", "php"),
    ("pl", "perl"),
    ("pm", "perl"),
    ("lua", "lua"),
    ("r", "r"),
    ("sql", "sql"),
    ("sh", "bash"),
    ("bash", "bash"),
    ("zsh", "shell"),
    ("ps1", "powershell"),
    ("psm1", "powershell"),
    ("json", "json"),
    ("toml", "toml"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("xml", "xml"),
    ("xsd", "xml"),
    ("svg", "xml"),
    ("html", "html"),
    ("htm", "html"),
    ("css", "css"),
    ("scss", "scss"),
    ("diff", "diff"),
    ("patch", "diff"),
    ("proto", "protobuf"),
    ("tex", "latex"),
    ("hs", "haskell"),
    ("scala", "scala"),
    ("dart", "dart"),
    ("groovy", "groovy"),
    ("gradle", "groovy"),
    ("erl", "erlang"),
    ("graphql", "graphql"),
    ("gql", "graphql"),
    ("dockerfile", "docker_file"),
    ("mk", "makefile"),
    ("cmake", "cmake"),
    ("ex", PLAIN_TEXT),
    ("txt", PLAIN_TEXT),
    ("log", PLAIN_TEXT),
    ("ini", PLAIN_TEXT),
    ("cfg", PLAIN_TEXT),
    ("conf", PLAIN_TEXT),
    ("env", PLAIN_TEXT),
];

/// The language token for a file's extension, or [`PLAIN_TEXT`] when the
/// platform has none for it.
pub(crate) fn language_token(ext: Option<&str>) -> &'static str {
    let Some(ext) = ext.map(str::to_ascii_lowercase) else {
        return PLAIN_TEXT;
    };
    EXT_LANGUAGE
        .iter()
        .find(|(candidate, _)| *candidate == ext)
        .map(|(_, token)| *token)
        .unwrap_or(PLAIN_TEXT)
}

/// The columns one delimited-text preview renders. A table wider than this
/// spends the card's budget on a row nobody reads; the extra columns are
/// dropped with the truncation note (the structural-overflow lane is #741's).
const MAX_PREVIEW_COLUMNS: usize = 20;

/// The card-JSON share one preview may spend: the naming block, the trailing
/// note and the button row need room, and the whole card must stay under
/// [`MAX_CARD_JSON_CHARS`] or Feishu rejects it outright.
const PREVIEW_JSON_BUDGET: usize = MAX_CARD_JSON_CHARS - 4_000;

/// Chars per fenced preview element: the per-element budget less the fence,
/// its language token and the element's own JSON overhead.
const PREVIEW_CHUNK_CHARS: usize = MAX_ELEMENT_TEXT_CHARS - 64;

/// Everything the viewer card renders.
pub(crate) struct ViewerCard<'a> {
    /// The **resolved** absolute path (never the typed spelling).
    pub(crate) path: &'a str,
    /// The file's byte size.
    pub(crate) size: u64,
    /// What the viewer decided the bytes are.
    pub(crate) kind: ViewerKind,
    /// The extension (without the dot), as the path spells it.
    pub(crate) ext: Option<&'a str>,
    /// The preview's text: the bytes read, already bounded by the read cap.
    pub(crate) text: &'a str,
    /// Whether the read already dropped content (the file is larger than the
    /// preview read cap).
    pub(crate) truncated: bool,
    /// The browse listing this viewer was opened from, when it was (ADR-0083
    /// §3): renders 「← 返回目录」 carrying that state. `None` for `/cat`.
    pub(crate) return_to: Option<BrowseView<'a>>,
}

/// Build the File Viewer card (ADR-0083 §3).
pub(crate) fn build_viewer_card(thread_key: &ThreadKey, view: ViewerCard<'_>) -> serde_json::Value {
    // One markdown hygiene state per card build, so the table budget the
    // preview's own table spends is counted exactly once.
    let mut markdown = CardMarkdown::new();
    let mut elements: Vec<serde_json::Value> = Vec::new();
    let mut truncated = view.truncated;

    elements.push(json!({
        "tag": "markdown",
        "content": markdown.clean(&naming_block(&view)),
    }));

    match view.kind {
        ViewerKind::Delimited => {
            let delim = if view.ext.map(str::to_ascii_lowercase).as_deref() == Some("tsv") {
                '\t'
            } else {
                ','
            };
            match delimited_table(view.text, delim) {
                Some((table, rows_dropped)) => {
                    truncated |= rows_dropped;
                    elements.push(json!({
                        "tag": "markdown",
                        "content": markdown.clean(&table),
                    }));
                }
                // A delimited file with no rows (whitespace only) has no table
                // to render: show the bytes in a block rather than invent one.
                None => {
                    let block = fenced_code(view.text, Some(PLAIN_TEXT));
                    elements.push(json!({
                        "tag": "markdown",
                        "content": markdown.clean(&block),
                    }));
                }
            }
        }
        ViewerKind::Text => {
            let lang = language_token(view.ext);
            let mut used = 0usize;
            for chunk in chunk_text(view.text, PREVIEW_CHUNK_CHARS) {
                let block = fenced_code(&chunk, Some(lang));
                let cost = super::sanitize::serialized_len(&block) + 128;
                if used + cost > PREVIEW_JSON_BUDGET {
                    truncated = true;
                    break;
                }
                used += cost;
                elements.push(json!({
                    "tag": "markdown",
                    "content": markdown.clean(&block),
                }));
            }
        }
    }

    if truncated {
        elements.push(json!({
            "tag": "markdown",
            "content": match view.kind {
                ViewerKind::Delimited => format!("_（表格过长，仅显示前 {MAX_TABLE_ROWS} 行）_"),
                ViewerKind::Text => "_（内容过长，预览已截断）_".to_string(),
            },
        }));
    }

    elements.push(viewer_buttons(thread_key, &view));
    card_shell(&format!("📄 {}", file_name(view.path)), "blue", elements)
}

/// The file name the header shows: the path's last component.
fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// The viewer's naming block (ADR-0083 §3): which file this is — the RESOLVED
/// path, its size and its type — so two similarly named files never look alike.
fn naming_block(view: &ViewerCard<'_>) -> String {
    let type_label = match view.ext {
        Some(ext) => format!("{}（.{ext}）", view.kind.label()),
        None => format!("{}（无扩展名）", view.kind.label()),
    };
    format!(
        "**文件**：`{}`\n**大小**：{} · **类型**：{}",
        view.path,
        human_size(view.size),
        type_label,
    )
}

/// The viewer's button row (ADR-0083 §3): 「📂 所在目录」 always, 「← 返回目录」
/// when the viewer was opened from a browse listing. Both carry the browse
/// handler's own payload shape ([`browse_view_value`]) so #742 only has to pass
/// the listing in.
fn viewer_buttons(thread_key: &ThreadKey, view: &ViewerCard<'_>) -> serde_json::Value {
    let mut columns: Vec<serde_json::Value> = Vec::new();
    let mut push = |label: &str, op: &str, target: BrowseView<'_>| {
        columns.push(json!({
            "tag": "column",
            "width": "auto",
            "vertical_align": "center",
            "elements": [{
                "tag": "button",
                "type": "default",
                "text": { "tag": "plain_text", "content": label },
                "value": browse_view_value(thread_key, op, target),
            }],
        }));
    };
    if let Some(parent) = std::path::Path::new(view.path).parent() {
        let parent = parent.to_string_lossy().to_string();
        push(
            "📂 所在目录",
            "go",
            BrowseView {
                dir: &parent,
                show_hidden: false,
                keyword: "",
                page: 1,
            },
        );
    }
    if let Some(return_to) = view.return_to {
        // `restore` is the browse handler's pure rebuild: the listing's whole
        // state comes back exactly, where `go` would clear the filter and page.
        push("← 返回目录", "restore", return_to);
    }
    json!({
        "tag": "column_set",
        "flex_mode": "flow",
        "horizontal_spacing": "default",
        "columns": columns,
    })
}

/// A delimited-text preview as one markdown table: the first row is the header,
/// the next the delimiter, the rest the body. `None` when the text has no rows
/// at all; `Some((table, truncated))` with `truncated` true when rows or
/// columns were dropped for the card's row cap / column cap.
fn delimited_table(text: &str, delim: char) -> Option<(String, bool)> {
    let mut rows = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| split_delimited(line, delim));
    let header = rows.next()?;
    let mut lines: Vec<String> = Vec::new();
    let mut truncated = header.len() > MAX_PREVIEW_COLUMNS;
    lines.push(table_row(&header));
    lines.push(table_row(&vec![
        "---".to_string();
        header.len().min(MAX_PREVIEW_COLUMNS)
    ]));
    for (body, row) in rows.enumerate() {
        if body == MAX_TABLE_ROWS {
            truncated = true;
            break;
        }
        truncated |= row.len() > MAX_PREVIEW_COLUMNS;
        lines.push(table_row(&row));
    }
    Some((lines.join("\n"), truncated))
}

/// One markdown table row from its cells: extra columns past the cap drop, and
/// a `|` inside a cell is escaped so it cannot split the row.
fn table_row(cells: &[String]) -> String {
    let shown: Vec<String> = cells
        .iter()
        .take(MAX_PREVIEW_COLUMNS)
        .map(|cell| cell.replace('|', "\\|"))
        .collect();
    format!("| {} |", shown.join(" | "))
}

/// Split one delimited line into its cells, honoring RFC-4180-style quoting: a
/// `"`-opened field may carry the delimiter and a doubled `""` for a literal
/// quote. A `"` that does not open a field is an ordinary character, so a file
/// that merely contains quotes still reads sensibly.
fn split_delimited(line: &str, delim: char) -> Vec<String> {
    let mut cells: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                current.push(c);
            }
            continue;
        }
        if c == '"' && current.trim().is_empty() {
            quoted = true;
        } else if c == delim {
            cells.push(current.trim().to_string());
            current = String::new();
        } else {
            current.push(c);
        }
    }
    cells.push(current.trim().to_string());
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ThreadKey {
        ThreadKey::new("chat_1".into(), "chat_1".into())
    }

    /// The card's visible text (every `content`/`text` field, walked) —
    /// `card_text`'s rule, local so the card layer's own tests need no bridge
    /// harness.
    fn card_text(card: &serde_json::Value) -> String {
        fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, v) in map {
                        match (key.as_str(), v.as_str()) {
                            ("content" | "text", Some(s)) => out.push(s.to_string()),
                            _ => walk(v, out),
                        }
                    }
                }
                serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(card, &mut out);
        out.join("\n")
    }

    /// Every button element on the card, in walk order.
    fn card_buttons(card: &serde_json::Value) -> Vec<&serde_json::Value> {
        fn walk<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("tag").and_then(|t| t.as_str()) == Some("button") {
                        out.push(value);
                    }
                    map.values().for_each(|v| walk(v, out));
                }
                serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(card, &mut out);
        out
    }

    fn view<'a>(path: &'a str, kind: ViewerKind, ext: Option<&'a str>, text: &'a str) -> ViewerCard<'a> {
        ViewerCard {
            path,
            size: text.len() as u64,
            kind,
            ext,
            text,
            truncated: false,
            return_to: None,
        }
    }

    /// Every token the map can produce is one Feishu documents, no extension is
    /// listed twice, and the extensions whose token would be a guess stay
    /// `plain_text` (ADR-0083 §6, research §1.3).
    #[test]
    fn extension_map_only_names_platform_tokens() {
        for (ext, token) in EXT_LANGUAGE {
            assert!(
                FEISHU_CODE_TOKENS.contains(token),
                "`{ext}` maps to `{token}`, which Feishu does not document"
            );
            assert_eq!(
                language_token(Some(ext)),
                *token,
                "`{ext}` must map to its own table entry"
            );
            let upper = ext.to_uppercase();
            assert_eq!(
                language_token(Some(&upper)),
                *token,
                "the match is case-insensitive (`{upper}`)"
            );
        }
        let mut exts: Vec<&str> = EXT_LANGUAGE.iter().map(|(e, _)| *e).collect();
        exts.sort();
        let before = exts.len();
        exts.dedup();
        assert_eq!(exts.len(), before, "no duplicate extension entries");

        // An extension with no honest token renders as plain text rather than a
        // near-miss highlight.
        assert_eq!(language_token(Some("ts")), PLAIN_TEXT);
        assert_eq!(language_token(Some("log")), PLAIN_TEXT);
        assert_eq!(language_token(Some("env")), PLAIN_TEXT);
        assert_eq!(language_token(Some("zzz")), PLAIN_TEXT);
        assert_eq!(language_token(None), PLAIN_TEXT);
    }

    /// The two text shapes: a `.csv`/`.tsv` file is a table, everything else is
    /// code — and the language token follows the extension.
    #[test]
    fn kind_and_language_follow_the_extension() {
        assert_eq!(kind_for_extension(Some("csv")), ViewerKind::Delimited);
        assert_eq!(kind_for_extension(Some("TSV")), ViewerKind::Delimited);
        assert_eq!(kind_for_extension(Some("rs")), ViewerKind::Text);
        assert_eq!(kind_for_extension(None), ViewerKind::Text);
    }

    /// The delimited preview is a markdown table: quoted delimiters stay one
    /// cell, a `|` in a cell cannot split the row, and rows past the platform's
    /// cap are dropped and reported.
    #[test]
    fn delimited_preview_is_a_markdown_table() {
        let (table, truncated) = delimited_table("name,age\n\"Doe, Jane\",41\nbob\",42\n", ',').unwrap();
        assert_eq!(
            table,
            "| name | age |\n| --- | --- |\n| Doe, Jane | 41 |\n| bob\" | 42 |"
        );
        assert!(!truncated);

        let (piped, _) = delimited_table("a|b,c\nd,e", ',').unwrap();
        assert!(
            piped.contains("| a\\|b | c |"),
            "a pipe in a cell is escaped: {piped}"
        );

        let wide = format!(
            "{}\n{}",
            (1..=30).map(|i| i.to_string()).collect::<Vec<_>>().join(","),
            "x"
        );
        let (table, _) = delimited_table(&wide, ',').unwrap();
        assert_eq!(
            table.lines().next().unwrap().matches('|').count(),
            MAX_PREVIEW_COLUMNS + 1
        );

        let long = (0..MAX_TABLE_ROWS + 5)
            .map(|i| format!("r{i},1"))
            .collect::<Vec<_>>()
            .join("\n");
        let (table, truncated) = delimited_table(&long, ',').unwrap();
        assert_eq!(
            table.lines().count(),
            MAX_TABLE_ROWS + 2,
            "header + delimiter + capped body"
        );
        assert!(truncated, "a table past the row cap says so");

        assert!(delimited_table("   \n\n", ',').is_none(), "no rows, no table");
    }

    /// The card names the file it shows: the resolved path, its size and its
    /// type — and its body is a fenced block carrying the extension's token.
    #[test]
    fn viewer_card_names_the_file_and_fences_the_preview() {
        let card = build_viewer_card(
            &key(),
            view(
                "/tmp/proj/src/main.rs",
                ViewerKind::Text,
                Some("rs"),
                "fn main() {}\n",
            ),
        );
        let text = card_text(&card);
        assert!(
            text.contains("/tmp/proj/src/main.rs"),
            "the resolved path: {text}"
        );
        assert!(text.contains("13 B"), "the size: {text}");
        assert!(text.contains("文本（.rs）"), "the type: {text}");
        assert!(text.contains("```rust"), "the language token: {text}");
        assert!(text.contains("fn main() {}"), "the body: {text}");
        assert!(
            !text.contains("已截断"),
            "a short preview is not truncated: {text}"
        );
        // The viewer's own entry into the directory browser.
        let buttons = card_buttons(&card);
        let out = buttons
            .iter()
            .find(|b| b["text"]["content"] == "📂 所在目录")
            .unwrap_or_else(|| panic!("所在目录 button missing on {text}"));
        assert_eq!(out["value"]["action"], "dir");
        assert_eq!(out["value"]["view"], "browse");
        assert_eq!(out["value"]["op"], "go");
        assert_eq!(out["value"]["dir"], "/tmp/proj/src");
        assert_eq!(out["value"]["chat_id"], "chat_1");
        assert_eq!(out["value"]["thread_id"], "chat_1");
    }

    /// An extension the platform has no token for degrades honestly, and a
    /// `.csv` previews as a table rather than a code block.
    #[test]
    fn unmapped_extensions_say_plain_text_and_csv_is_a_table() {
        let card = build_viewer_card(
            &key(),
            view("/tmp/run.log", ViewerKind::Text, Some("log"), "boom\n"),
        );
        let text = card_text(&card);
        assert!(text.contains("```plain_text"), "{text}");
        assert!(text.contains("文本（.log）"), "{text}");

        let card = build_viewer_card(
            &key(),
            view(
                "/tmp/people.csv",
                ViewerKind::Delimited,
                Some("csv"),
                "name,age\nann,3\n",
            ),
        );
        let text = card_text(&card);
        assert!(text.contains("表格（.csv）"), "{text}");
        assert!(text.contains("| name | age |"), "{text}");
        assert!(text.contains("| ann | 3 |"), "{text}");
        assert!(!text.contains("```"), "a table is not fenced: {text}");
    }

    /// 「← 返回目录」 rides only a viewer opened from a browse listing, and its
    /// payload carries that listing's whole state (ADR-0083 §3) — the shape the
    /// browse handler's `restore` op reads (#742 passes the listing in).
    #[test]
    fn return_button_carries_the_browse_listing_state() {
        let mut with_return = view("/tmp/proj/a.txt", ViewerKind::Text, Some("txt"), "hi");
        with_return.return_to = Some(BrowseView {
            dir: "/tmp/proj",
            show_hidden: true,
            keyword: "a",
            page: 3,
        });
        let card = build_viewer_card(&key(), with_return);
        let buttons = card_buttons(&card);
        let back = buttons
            .iter()
            .find(|b| b["text"]["content"] == "← 返回目录")
            .expect("返回目录 button");
        assert_eq!(back["value"]["action"], "dir");
        assert_eq!(back["value"]["view"], "browse");
        assert_eq!(back["value"]["op"], "restore");
        assert_eq!(back["value"]["dir"], "/tmp/proj");
        assert_eq!(back["value"]["show_hidden"], true);
        assert_eq!(back["value"]["keyword"], "a");
        assert_eq!(back["value"]["page"], 3);

        // A `/cat` viewer has no listing to return to.
        let card = build_viewer_card(
            &key(),
            view("/tmp/proj/a.txt", ViewerKind::Text, Some("txt"), "hi"),
        );
        assert!(
            !card_buttons(&card)
                .iter()
                .any(|b| b["text"]["content"] == "← 返回目录"),
            "no return button without a listing"
        );
    }

    /// A preview longer than the card can hold is cut inside the budget and
    /// says so — never a rejected card, never a silently empty one (#740 owns
    /// the pager, #741 the rich-text lane).
    #[test]
    fn a_long_preview_stays_within_the_card_budget() {
        let body = "let x = 1;\n".repeat(40_000);
        let card = build_viewer_card(&key(), view("/tmp/huge.rs", ViewerKind::Text, Some("rs"), &body));
        let json_len = serde_json::to_string(&card).unwrap().len();
        assert!(
            json_len < MAX_CARD_JSON_CHARS,
            "the card must fit its own budget: {json_len}"
        );
        let text = card_text(&card);
        assert!(text.contains("```rust"), "the preview still renders a block");
        assert!(text.contains("已截断"), "and admits the cut: {text}");

        // Every fenced element is closed, so no element leaves a fence open.
        for element in card["body"]["elements"].as_array().unwrap() {
            if let Some(content) = element["content"].as_str()
                && content.starts_with("```")
            {
                assert_eq!(
                    content.matches("```").count(),
                    2,
                    "one chunk per element, each fence closed"
                );
            }
        }
    }
}
