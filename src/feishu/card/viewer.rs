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

use super::sanitize::{CardMarkdown, MAX_TABLE_ROWS, serialized_len};
use super::session::{BrowseView, browse_view_value};
use super::shell::card_shell;
use super::tool_render::{FileDelivery, delivery_suffix, file_image_element};
use super::{MAX_CARD_JSON_CHARS, MAX_ELEMENT_TEXT_CHARS, chunk_text, fenced_code};

/// What the viewer decided a file's bytes are (§6): decided by CONTENT — magic
/// bytes first, then UTF-8 validity — never by the file's name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ViewerKind {
    /// UTF-8 text: a fenced code block with the extension's language token.
    Text,
    /// UTF-8 text past the preview read cap: no preview (the bytes are too
    /// large), but still named as TEXT — 「文本（过大，未预览）」 — and delivered
    /// as a File Message. Content decides this, so a big UTF-8 text file is
    /// never labelled 二进制 (finding #739 §5).
    TextTooLarge,
    /// Delimited text (`.csv`/`.tsv`): a markdown table.
    Delimited,
    /// An image (by magic bytes): embedded in the card when Feishu accepts it,
    /// else delivered as a File Message.
    Image,
    /// A non-text file (a known binary, or an unknown non-UTF-8 file): no
    /// preview, delivered as a File Message.
    Binary,
}

impl ViewerKind {
    /// The type the card names, alongside the extension.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Text | Self::TextTooLarge => "文本",
            Self::Delimited => "表格",
            Self::Image => "图片",
            Self::Binary => "二进制",
        }
    }
}

/// The whole-bytes lane a **File Viewer** card shows (ADR-0083 §5): what the
/// file is and where its bytes went. Rendered as the card's tracking line
/// (`📎 name · mime · size · 已内嵌` / `· 已发送为文件消息` / `· 未发送`) and, for
/// an embedded image, the `img` element. Named so a file past Feishu's 30MB cap
/// — which is never read — can still carry its real size and the 未发送 state.
pub(crate) struct ViewerFileLane {
    pub(crate) mime: String,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) delivery: FileDelivery,
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

/// The most characters one table CELL renders. A valid CSV with a single ~30 KB
/// field would otherwise spend the whole card's JSON budget on one cell and get
/// the card rejected; a longer cell is clipped (and the clip is reported). The
/// budget covers the sanitizer's `<` expansion (five bytes per `<`), so
/// `MAX_PREVIEW_COLUMNS` worst-case cells plus the rows still fit.
const MAX_CELL_CHARS: usize = 120;

/// The card-JSON share one preview may spend: the naming block, the trailing
/// note and the button row need room, and the whole card must stay under
/// [`MAX_CARD_JSON_CHARS`] or Feishu rejects it outright.
const PREVIEW_JSON_BUDGET: usize = MAX_CARD_JSON_CHARS - 4_000;

/// Chars per fenced preview element: the per-element budget less the fence,
/// its language token and the element's own JSON overhead.
const PREVIEW_CHUNK_CHARS: usize = MAX_ELEMENT_TEXT_CHARS - 64;

/// How a delimited preview was cut to fit the card: a dropped row/column is
/// reported separately from a clipped long cell, so the card's note never
/// claims a cut that did not happen (ADR-0083 §6).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TableCut {
    /// Rows or columns were dropped (the platform's row cap / the column cap,
    /// or the card's own JSON budget).
    rows: bool,
    /// Cells longer than [`MAX_CELL_CHARS`] were clipped.
    cells: bool,
}

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
    /// Whether the card layer already dropped preview content (the card budget
    /// cut it). A file past the preview read cap is never previewed at all, so
    /// it takes the File Message lane rather than being truncated here.
    pub(crate) truncated: bool,
    /// The browse listing this viewer was opened from, when it was (ADR-0083
    /// §3): renders 「← 返回目录」 carrying that state. `None` for `/cat`.
    pub(crate) return_to: Option<BrowseView<'a>>,
    /// The whole-bytes lane this card shows (ADR-0083 §5), when it has one: the
    /// tracking line and, for an embedded image, the `img` element. `None` for a
    /// text preview that took no lane on open (its 「发为文件」 transfer still
    /// works — the click carries the path).
    pub(crate) lane: Option<ViewerFileLane>,
}

/// Build the File Viewer card (ADR-0083 §3).
pub(crate) fn build_viewer_card(thread_key: &ThreadKey, view: ViewerCard<'_>) -> serde_json::Value {
    // One markdown hygiene state per card build, so the table budget the
    // preview's own table spends is counted exactly once.
    let mut markdown = CardMarkdown::new();
    let mut elements: Vec<serde_json::Value> = Vec::new();
    let mut truncated = view.truncated;
    let mut clipped_cells = false;

    elements.push(json!({
        "tag": "markdown",
        "content": markdown.clean(&naming_block(&view)),
    }));

    match view.kind {
        // An image the open lane embedded: the `img` element IS the preview.
        // An image Feishu would not embed (past its caps) has no element — its
        // tracking line below says it went out as a File Message instead.
        ViewerKind::Image => {
            if let Some(ViewerFileLane {
                delivery: FileDelivery::Embedded { image_key },
                name,
                ..
            }) = &view.lane
            {
                elements.push(file_image_element(image_key, name));
            }
        }
        // No preview: the bytes went out (or could not) as a File Message, and
        // the tracking line below is the whole card body.
        ViewerKind::Binary | ViewerKind::TextTooLarge => {}
        ViewerKind::Delimited => {
            let delim = if view.ext.map(str::to_ascii_lowercase).as_deref() == Some("tsv") {
                '\t'
            } else {
                ','
            };
            match delimited_table(view.text, delim) {
                Some((table, cut)) => {
                    truncated |= cut.rows;
                    clipped_cells = cut.cells;
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
                _ => "_（内容过长，预览已截断）_".to_string(),
            },
        }));
    }
    if clipped_cells {
        elements.push(json!({
            "tag": "markdown",
            "content": format!("_（过长的单元格已截断，每格最多 {MAX_CELL_CHARS} 字）_"),
        }));
    }

    // The whole-bytes lane's tracking line: names the file and says where its
    // bytes went — 已内嵌 (an image in this card), 已发送为文件消息 (one File
    // Message), or 未发送 (past Feishu's 30MB cap, or a failed upload/send).
    if let Some(lane) = &view.lane {
        elements.push(json!({
            "tag": "markdown",
            "content": markdown.clean(&lane_line(lane)),
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
/// A text file too large to preview names that in the type itself, so a big
/// UTF-8 file is never read as 二进制.
fn naming_block(view: &ViewerCard<'_>) -> String {
    let type_label = match view.kind {
        ViewerKind::TextTooLarge => "文本（过大，未预览）".to_string(),
        kind => match view.ext {
            Some(ext) => format!("{}（.{ext}）", kind.label()),
            None => format!("{}（无扩展名）", kind.label()),
        },
    };
    format!(
        "**文件**：`{}`\n**大小**：{} · **类型**：{}",
        view.path,
        human_size(view.size),
        type_label,
    )
}

/// One tracking line for the card's whole-bytes lane (ADR-0083 §5): `📎 name ·
/// mime · size` plus the shared delivery suffix, so the viewer spells a
/// delivered file exactly as a Tool Panel does (ADR-0076).
fn lane_line(lane: &ViewerFileLane) -> String {
    let base = format!("📎 {} · {} · {}", lane.name, lane.mime, human_size(lane.size));
    match delivery_suffix(&lane.delivery) {
        Some(suffix) => format!("{base} · {suffix}"),
        None => base,
    }
}

/// The viewer's button row (ADR-0083 §3, §5): 「📂 所在目录」 always, 「← 返回目录」
/// when the viewer was opened from a browse listing, and 「发为文件」 always. The
/// two directory buttons carry the browse handler's own payload shape
/// ([`browse_view_value`]) so #742 only has to pass the listing in; the transfer
/// button carries the RESOLVED path the click must upload, plus the browse
/// listing's whole state so the card it PATCHes back to can still carry
/// 「← 返回目录」.
fn viewer_buttons(thread_key: &ThreadKey, view: &ViewerCard<'_>) -> serde_json::Value {
    let mut columns = directory_columns(thread_key, view.path, view.return_to);
    columns.push(button_column("发为文件", transfer_value(thread_key, view)));
    button_row(columns)
}

/// The directory half of the viewer's button row (ADR-0083 §3): 「📂 所在目录」
/// always, and 「← 返回目录」 — the browse handler's `restore` op, which rebuilds
/// the listing's whole state where `go` would clear the filter and page — when
/// the viewer was opened from a browse listing. Shared by the viewer card and
/// the failed-transfer card, which keeps the way back but carries no
/// 「发为文件」 (there is nothing left to send).
fn directory_columns(
    thread_key: &ThreadKey,
    path: &str,
    return_to: Option<BrowseView<'_>>,
) -> Vec<serde_json::Value> {
    let mut columns: Vec<serde_json::Value> = Vec::new();
    if let Some(parent) = std::path::Path::new(path).parent() {
        let parent = parent.to_string_lossy().to_string();
        columns.push(button_column(
            "📂 所在目录",
            browse_view_value(
                thread_key,
                "go",
                BrowseView {
                    dir: &parent,
                    show_hidden: false,
                    keyword: "",
                    page: 1,
                },
            ),
        ));
    }
    if let Some(return_to) = return_to {
        columns.push(button_column(
            "← 返回目录",
            browse_view_value(thread_key, "restore", return_to),
        ));
    }
    columns
}

/// One column holding one button with the given callback payload.
fn button_column(label: &str, value: serde_json::Value) -> serde_json::Value {
    json!({
        "tag": "column",
        "width": "auto",
        "vertical_align": "center",
        "elements": [{
            "tag": "button",
            "type": "default",
            "text": { "tag": "plain_text", "content": label },
            "value": value,
        }],
    })
}

/// The card's button row: a wrapping column set of `columns`.
fn button_row(columns: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "tag": "column_set",
        "flex_mode": "flow",
        "horizontal_spacing": "default",
        "columns": columns,
    })
}

/// The card a FAILED 「发为文件」 transfer PATCHes over the viewer (ADR-0083 §5):
/// the path is named, the outcome is stated in the card's own copy, and the way
/// back to the directory stays. It carries no preview and no 「发为文件」 — there
/// is nothing left to send. Used when the path vanished or became unreadable
/// between the viewer and the click, so the click's result lands on the card
/// instead of being dropped (finding #739 §4).
pub(crate) fn build_viewer_failure_card(
    thread_key: &ThreadKey,
    path: &str,
    line: &str,
    return_to: Option<BrowseView<'_>>,
) -> serde_json::Value {
    let mut markdown = CardMarkdown::new();
    let elements = vec![json!({
        "tag": "markdown",
        "content": markdown.clean(line),
    })];
    let columns = directory_columns(thread_key, path, return_to);
    let mut elements = elements;
    if !columns.is_empty() {
        elements.push(button_row(columns));
    }
    card_shell(&format!("📄 {}", file_name(path)), "blue", elements)
}

/// The 「发为文件」 button's payload (ADR-0083 §5): the **resolved** path the
/// click uploads, its Chat/Topic routing, and — when the viewer was opened from
/// a browse listing — that listing's whole state, so the transfer's card PATCH
/// can rebuild a card that still carries 「← 返回目录」 (ADR-0052's statelessness).
/// The click itself does NO transfer: it records the intent and acknowledges,
/// and a bounded, timed-out step performs the upload and the send off the
/// callback path (ADR-0078's 3 s budget).
fn transfer_value(thread_key: &ThreadKey, view: &ViewerCard<'_>) -> serde_json::Value {
    let mut value = json!({
        "action": "file",
        "op": "send",
        "path": view.path,
        "chat_id": thread_key.chat_id,
        "thread_id": thread_key.thread_id,
    });
    if let Some(return_to) = view.return_to {
        value["return_dir"] = json!(return_to.dir);
        value["return_show_hidden"] = json!(return_to.show_hidden);
        value["return_keyword"] = json!(return_to.keyword);
        value["return_page"] = json!(return_to.page);
    }
    value
}

/// A delimited-text preview as one markdown table: the first record is the
/// header, the next the delimiter, the rest the body. `None` when the text has
/// no records at all; `Some((table, cut))` where `cut` says whether rows,
/// columns or long cells were dropped/clipped to keep the card valid.
///
/// Records are pulled LAZILY and the pull stops at the card's own caps (the row
/// cap and the JSON budget), so a file of millions of tiny rows is parsed only
/// as far as the card needs — never materialized whole (finding #739 §1,
/// round 2).
fn delimited_table(text: &str, delim: char) -> Option<(String, TableCut)> {
    let mut records = delimited_records(text, delim);
    let header = records.next()?;
    let mut cut = TableCut {
        rows: header.len() > MAX_PREVIEW_COLUMNS,
        ..TableCut::default()
    };
    let mut lines: Vec<String> = Vec::new();
    let (header_line, header_clipped) = table_row(&header);
    cut.cells |= header_clipped;
    lines.push(header_line);
    lines.push(table_row(&vec!["---".to_string(); header.len().min(MAX_PREVIEW_COLUMNS)]).0);
    // The header and delimiter rows are the table's floor; the card budget is
    // then spent row by row, so a CSV of very wide rows can never outgrow it.
    let mut used = lines.iter().map(|line| serialized_len(line) + 8).sum::<usize>();
    for (body, row) in records.enumerate() {
        if body == MAX_TABLE_ROWS {
            cut.rows = true;
            break;
        }
        cut.rows |= row.len() > MAX_PREVIEW_COLUMNS;
        let (line, clipped) = table_row(&row);
        cut.cells |= clipped;
        let cost = serialized_len(&line) + 8;
        if used + cost > PREVIEW_JSON_BUDGET {
            cut.rows = true;
            break;
        }
        used += cost;
        lines.push(line);
    }
    Some((lines.join("\n"), cut))
}

/// One markdown table row from its cells: extra columns past the cap drop, a
/// cell longer than [`MAX_CELL_CHARS`] is clipped (flagged in the second
/// return), and a `|` inside a cell is escaped so it cannot split the row. A
/// newline inside a cell (a quoted field may carry one) is flattened to a
/// space, which keeps the row on ONE markdown line.
fn table_row(cells: &[String]) -> (String, bool) {
    let mut clipped = false;
    let shown: Vec<String> = cells
        .iter()
        .take(MAX_PREVIEW_COLUMNS)
        .map(|cell| {
            let flattened = cell.replace(['\n', '\r'], " ");
            let (text, was_clipped) = clip_cell(&flattened);
            clipped |= was_clipped;
            text.replace('|', "\\|")
        })
        .collect();
    (format!("| {} |", shown.join(" | ")), clipped)
}

/// `cell` clipped to [`MAX_CELL_CHARS`] characters, with a trailing `…` when it
/// was cut (counted inside the budget) — never silently. Returns the text and
/// whether it was clipped.
fn clip_cell(cell: &str) -> (String, bool) {
    if cell.chars().count() <= MAX_CELL_CHARS {
        return (cell.to_string(), false);
    }
    let mut text: String = cell.chars().take(MAX_CELL_CHARS.saturating_sub(1)).collect();
    text.push('…');
    (text, true)
}

/// Parse delimited text into records of cells, RFC 4180-style: a `"`-opened
/// field may carry the delimiter, a doubled `""` for a literal quote, and
/// embedded NEWLINES — so `1,"first\nsecond"` is ONE record of two cells, never
/// two rows. A `"` that does not open a field is an ordinary character, so a
/// file that merely contains quotes still reads sensibly. A blank line carries
/// no record; a row of empty cells (a lone `,`) does.
///
/// The parse is LAZY and every record is bounded: `next` reads only as far as
/// the current record's end, and a record holds at most one cell past
/// [`MAX_PREVIEW_COLUMNS`] and one character past [`MAX_CELL_CHARS`] in each —
/// enough for the renderer to see the row was wide or the cell long. A caller
/// that stops pulling at the card's caps therefore never lets a whole file (or
/// one 30 KB field, or a million-column row) turn into allocations (finding
/// #739 §1, round 2).
fn delimited_records(text: &str, delim: char) -> DelimitedRecords<'_> {
    DelimitedRecords {
        chars: text.chars().peekable(),
        delim,
        done: false,
        #[cfg(test)]
        scanned: 0,
    }
}

/// The lazy parser behind [`delimited_records`]: it pulls characters from the
/// text on demand and yields one bounded record per `next`.
struct DelimitedRecords<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    delim: char,
    /// Set once the input is exhausted, so a later `next` never restarts.
    done: bool,
    /// Characters pulled so far — the bounded-work measure the test reads; it
    /// does not exist in the production build.
    #[cfg(test)]
    scanned: usize,
}

impl DelimitedRecords<'_> {
    /// How many characters the parse has pulled so far (tests only).
    #[cfg(test)]
    fn scanned(&self) -> usize {
        self.scanned
    }

    /// Count one character pulled from the input (tests only).
    #[inline]
    fn count(&mut self) {
        #[cfg(test)]
        {
            self.scanned += 1;
        }
    }
}

impl Iterator for DelimitedRecords<'_> {
    type Item = Vec<String>;

    fn next(&mut self) -> Option<Vec<String>> {
        if self.done {
            return None;
        }
        let mut record: Vec<String> = Vec::new();
        let mut cell = BoundedCell::default();
        let mut quoted = false;
        // Whether the current record has seen anything (a delimiter or a
        // non-blank character): a blank line carries none and is dropped.
        let mut touched = false;
        while let Some(c) = self.chars.next() {
            self.count();
            if quoted {
                match c {
                    '"' if self.chars.peek() == Some(&'"') => {
                        cell.push('"');
                        self.chars.next();
                        self.count();
                    }
                    '"' => quoted = false,
                    _ => cell.push(c),
                }
                continue;
            }
            if c == '"' && cell.is_blank() {
                quoted = true;
                touched = true;
            } else if c == self.delim {
                push_record_cell(&mut record, &mut cell);
                touched = true;
            } else if c == '\n' || c == '\r' {
                if c == '\r' && self.chars.peek() == Some(&'\n') {
                    self.chars.next();
                    self.count();
                }
                push_record_cell(&mut record, &mut cell);
                if touched {
                    return Some(std::mem::take(&mut record));
                }
                record.clear();
                touched = false;
            } else {
                if !c.is_whitespace() {
                    touched = true;
                }
                cell.push(c);
            }
        }
        // The final record: a file may not end in a newline.
        self.done = true;
        if touched {
            push_record_cell(&mut record, &mut cell);
            Some(record)
        } else {
            None
        }
    }
}

/// One parsed cell, bounded: it stores at most [`MAX_CELL_CHARS`] + 1
/// characters (the one extra lets [`clip_cell`] see the cell was cut), skips
/// its leading whitespace (trimmed either way), and counts every character it
/// consumes — so a pathological field can never grow without limit.
#[derive(Default)]
struct BoundedCell {
    text: String,
    /// Characters consumed since the first non-whitespace one; leading
    /// whitespace is trimmed anyway and so is not counted.
    counted: usize,
    /// Whether a non-whitespace character has been consumed: a cell with none
    /// is blank, which is what lets a `"` open a quoted field.
    started: bool,
}

impl BoundedCell {
    /// Append `c`, holding the stored text to [`MAX_CELL_CHARS`] + 1 characters.
    fn push(&mut self, c: char) {
        if !self.started {
            if c.is_whitespace() {
                return;
            }
            self.started = true;
        }
        if self.counted <= MAX_CELL_CHARS {
            self.text.push(c);
        }
        self.counted += 1;
    }

    /// Whether the cell has seen no non-whitespace character yet.
    fn is_blank(&self) -> bool {
        !self.started
    }

    /// The cell's text, trimmed like the whole-file parser did, and reset for
    /// the next cell.
    fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.counted = 0;
        self.started = false;
        text.trim().to_string()
    }

    /// Discard the cell's text (a column past the cap) and reset.
    fn reset(&mut self) {
        self.text.clear();
        self.counted = 0;
        self.started = false;
    }
}

/// Push one parsed cell onto a record, holding the record to
/// [`MAX_PREVIEW_COLUMNS`] + 1 cells: one past the column cap, which is enough
/// for the renderer's `cut` to see the row was wide. Any further cell is
/// dropped with its text, so a million-column row cannot materialize.
fn push_record_cell(record: &mut Vec<String>, cell: &mut BoundedCell) {
    if record.len() <= MAX_PREVIEW_COLUMNS {
        record.push(cell.take());
    } else {
        cell.reset();
    }
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
            lane: None,
        }
    }

    /// A whole-bytes lane on `view`, for the image/binary card shapes.
    fn with_lane<'a>(
        mut view: ViewerCard<'a>,
        mime: &str,
        name: &str,
        size: u64,
        delivery: FileDelivery,
    ) -> ViewerCard<'a> {
        view.lane = Some(ViewerFileLane {
            mime: mime.to_string(),
            name: name.to_string(),
            size,
            delivery,
        });
        view
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
        let (table, cut) = delimited_table("name,age\n\"Doe, Jane\",41\nbob\",42\n", ',').unwrap();
        assert_eq!(
            table,
            "| name | age |\n| --- | --- |\n| Doe, Jane | 41 |\n| bob\" | 42 |"
        );
        assert_eq!(cut, TableCut::default(), "nothing was cut");

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
        let (table, cut) = delimited_table(&wide, ',').unwrap();
        assert_eq!(
            table.lines().next().unwrap().matches('|').count(),
            MAX_PREVIEW_COLUMNS + 1
        );
        assert!(cut.rows, "columns past the cap are dropped and reported");

        let long = (0..MAX_TABLE_ROWS + 5)
            .map(|i| format!("r{i},1"))
            .collect::<Vec<_>>()
            .join("\n");
        let (table, cut) = delimited_table(&long, ',').unwrap();
        assert_eq!(
            table.lines().count(),
            MAX_TABLE_ROWS + 2,
            "header + delimiter + capped body"
        );
        assert!(cut.rows, "a table past the row cap says so");

        assert!(delimited_table("   \n\n", ',').is_none(), "no rows, no table");
    }

    /// A quoted field may carry the delimiter, an escaped `""` for a literal
    /// quote, and an EMBEDDED NEWLINE: such a field is ONE cell of ONE record,
    /// never two rows (ADR-0083 §6, finding #739 §2). The newline is flattened
    /// in the rendered row, so the markdown table stays structurally valid.
    #[test]
    fn delimited_preview_keeps_a_quoted_newline_in_one_cell() {
        let text = "name,note\nann,\"first\nsecond\"\nbob,\"say \"\"hi\"\"\"\n";
        let (table, cut) = delimited_table(text, ',').unwrap();
        assert_eq!(
            table,
            "| name | note |\n| --- | --- |\n| ann | first second |\n| bob | say \"hi\" |"
        );
        assert_eq!(cut, TableCut::default());

        // The parser is the pin: two records, the first body row carrying the
        // newline inside its second cell until `table_row` flattens it.
        assert_eq!(
            delimited_records(text, ',').collect::<Vec<_>>(),
            vec![
                vec!["name".to_string(), "note".to_string()],
                vec!["ann".to_string(), "first\nsecond".to_string()],
                vec!["bob".to_string(), "say \"hi\"".to_string()],
            ]
        );

        // A CRLF inside quotes is data too, and a CRLF line ending still ends a
        // record.
        assert_eq!(
            delimited_records("a,b\r\nc,\"d\r\ne\"\r\n", ',').collect::<Vec<_>>(),
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string(), "d\r\ne".to_string()],
            ]
        );
    }

    /// An oversized delimited file — one ~30 KB field, then a table of very wide
    /// rows — is clipped and budgeted: the card stays under its own JSON limit
    /// and the cut is reported, never an invalid card (finding #739 §1).
    #[test]
    fn an_oversized_delimited_preview_stays_within_the_card_budget() {
        // One row with a single ~30 KB field: the cell is clipped, honestly.
        let huge_cell = format!("name,notes\nrow,{}", "x".repeat(30_000));
        let (table, cut) = delimited_table(&huge_cell, ',').unwrap();
        assert!(cut.cells, "a long cell is clipped: {cut:?}");
        assert!(
            table.len() < 1_000,
            "the ~30 KB field must not reach the table: {}",
            table.len()
        );
        assert!(table.contains('…'), "the clip is visible: {table}");

        let card = build_viewer_card(
            &key(),
            view("/tmp/huge.csv", ViewerKind::Delimited, Some("csv"), &huge_cell),
        );
        let json_len = serde_json::to_string(&card).unwrap().len();
        assert!(
            json_len < MAX_CARD_JSON_CHARS,
            "the card must fit its own budget: {json_len}"
        );
        let text = card_text(&card);
        assert!(text.contains("单元格已截断"), "the cut is stated: {text}");

        // Many rows, every cell near the clip: the row budget stops the table
        // before the card's JSON budget can be outgrown.
        let wide = format!(
            "{}\n{}",
            "name,notes",
            (0..MAX_TABLE_ROWS + 10)
                .map(|i| format!("r{i},{}", "y".repeat(5_000)))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let card = build_viewer_card(
            &key(),
            view("/tmp/wide.csv", ViewerKind::Delimited, Some("csv"), &wide),
        );
        let json_len = serde_json::to_string(&card).unwrap().len();
        assert!(
            json_len < MAX_CARD_JSON_CHARS,
            "a CSV of wide rows must stay within the card budget: {json_len}"
        );
        let text = card_text(&card);
        assert!(text.contains("表格过长"), "the row cut is stated: {text}");
        assert!(text.contains("单元格已截断"), "the cell clip is stated: {text}");
    }

    /// A delimited file far larger than any card — hundreds of thousands of
    /// tiny rows — is parsed only as far as the card's caps require: the parser
    /// pulls a bounded prefix of the file (never the whole of it), the card
    /// stays valid, and one pathological field is bounded too. This is the fix
    /// for finding #739 §1 (round 2): an eager parse materializes the whole
    /// file before the row cap can ever apply.
    #[test]
    fn a_huge_delimited_file_is_parsed_only_as_far_as_the_card_needs() {
        // ~1.6 MB over 400k tiny rows: cheap to build, and far past what any
        // card renders. The test itself must not expand into record form.
        let rows = 400_000;
        let mut csv = String::with_capacity(rows * 4 + 8);
        csv.push_str("n,v\n");
        for _ in 0..rows {
            csv.push_str("a,1\n");
        }

        // Bounded WORK: pulling only the records the card needs must scan a
        // small prefix of the file, not all of it.
        let mut records = delimited_records(&csv, ',');
        assert_eq!(records.next().unwrap(), vec!["n", "v"]);
        let body = records.by_ref().take(MAX_TABLE_ROWS + 1).count();
        assert_eq!(body, MAX_TABLE_ROWS + 1);
        assert!(
            records.scanned() < csv.len() / 4,
            "the parse must not scan the whole file: {} of {} chars",
            records.scanned(),
            csv.len()
        );

        // Bounded OUTPUT: the card renders the capped table, stays valid, and
        // says the table was cut.
        let card = build_viewer_card(
            &key(),
            view("/tmp/many.csv", ViewerKind::Delimited, Some("csv"), &csv),
        );
        let json_len = serde_json::to_string(&card).unwrap().len();
        assert!(
            json_len < MAX_CARD_JSON_CHARS,
            "the card must fit its own budget: {json_len}"
        );
        let text = card_text(&card);
        assert!(text.contains("表格过长"), "the row cut is stated: {text}");

        // One pathological field (a whole 8 MB cell) is bounded before it can
        // materialize: the parser stores a clipped prefix, no more.
        let mut giant = String::from("h\n");
        giant.push_str(&"x".repeat(crate::bridge::viewer::PREVIEW_READ_CAP));
        let cell = delimited_records(&giant, ',').nth(1).unwrap().pop().unwrap();
        assert!(
            cell.chars().count() <= MAX_CELL_CHARS + 1,
            "a giant field is bounded to the cell cap: {}",
            cell.chars().count()
        );
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

    /// An image the open lane embedded: the card names it 图片, renders the
    /// `img` element carrying the reusable `image_key`, and its tracking line
    /// says 已内嵌 (ADR-0083 §5).
    #[test]
    fn image_kind_embeds_the_image_and_shows_the_embedded_line() {
        let card = build_viewer_card(
            &key(),
            with_lane(
                view("/tmp/proj/shot.png", ViewerKind::Image, Some("png"), ""),
                "image/png",
                "shot.png",
                2048,
                FileDelivery::Embedded {
                    image_key: "img_v2_shot".into(),
                },
            ),
        );
        let text = card_text(&card);
        assert!(text.contains("图片（.png）"), "the type is named: {text}");
        assert!(
            text.contains("📎 shot.png · image/png · 2.0 KB · 已内嵌"),
            "the tracking line: {text}"
        );

        fn find_img<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.get("tag").and_then(|t| t.as_str()) == Some("img") {
                        out.push(value);
                    }
                    map.values().for_each(|v| find_img(v, out));
                }
                serde_json::Value::Array(items) => items.iter().for_each(|v| find_img(v, out)),
                _ => {}
            }
        }
        let mut images = Vec::new();
        find_img(&card, &mut images);
        assert_eq!(images.len(), 1, "exactly one embedded image: {card}");
        assert_eq!(images[0]["img_key"], "img_v2_shot");
        assert_eq!(images[0]["preview"], true, "a click enlarges it");
    }

    /// A binary is delivered as a File Message: no preview element, a tracking
    /// line that says where the bytes went — and NO copy promising a client-side
    /// preview (the platform's 在线预览 list is unverified, ADR-0083 §10).
    #[test]
    fn binary_kind_shows_only_the_tracking_line() {
        let card = build_viewer_card(
            &key(),
            with_lane(
                view("/tmp/proj/archive.zip", ViewerKind::Binary, Some("zip"), ""),
                "application/zip",
                "archive.zip",
                3_145_728,
                FileDelivery::SentAsFile,
            ),
        );
        let text = card_text(&card);
        assert!(text.contains("二进制（.zip）"), "the type is named: {text}");
        assert!(
            text.contains("📎 archive.zip · application/zip · 3.0 MB · 已发送为文件消息"),
            "the tracking line: {text}"
        );
        assert!(!text.contains("```"), "a binary is not previewed: {text}");
        assert!(
            !text.contains("内联预览") && !text.contains("在线预览"),
            "no copy promises a client-side preview: {text}"
        );

        // A file past the 30MB cap (or a failed upload/send) says 未发送.
        let over = build_viewer_card(
            &key(),
            with_lane(
                view("/tmp/proj/huge.bin", ViewerKind::Binary, Some("bin"), ""),
                "application/octet-stream",
                "huge.bin",
                crate::feishu::file::MAX_FILE_BYTES + 1,
                FileDelivery::Undelivered,
            ),
        );
        assert!(card_text(&over).contains("未发送"), "{}", card_text(&over));
    }

    /// Every viewer carries 「发为文件」 (ADR-0083 §5) — including a text preview,
    /// whose full bytes are the reason the button exists. Its payload names the
    /// RESOLVED path, routes to the Chat/Topic, and carries the browse listing's
    /// whole state so the PATCHed card can still return.
    #[test]
    fn every_viewer_carries_the_send_as_file_button() {
        let card = build_viewer_card(
            &key(),
            view("/tmp/proj/notes.md", ViewerKind::Text, Some("md"), "# hi\n"),
        );
        let buttons = card_buttons(&card);
        let send = buttons
            .iter()
            .find(|b| b["text"]["content"] == "发为文件")
            .unwrap_or_else(|| panic!("发为文件 button missing on {}", card_text(&card)));
        assert_eq!(send["value"]["action"], "file");
        assert_eq!(send["value"]["op"], "send");
        assert_eq!(send["value"]["path"], "/tmp/proj/notes.md");
        assert_eq!(send["value"]["chat_id"], "chat_1");
        assert_eq!(send["value"]["thread_id"], "chat_1");
        assert!(
            send["value"].get("return_dir").is_none(),
            "a /cat viewer has no listing to carry"
        );

        let mut from_browse = view("/tmp/proj/notes.md", ViewerKind::Text, Some("md"), "# hi\n");
        from_browse.return_to = Some(BrowseView {
            dir: "/tmp/proj",
            show_hidden: true,
            keyword: "no",
            page: 2,
        });
        let card = build_viewer_card(&key(), from_browse);
        let send = card_buttons(&card)
            .into_iter()
            .find(|b| b["text"]["content"] == "发为文件")
            .expect("发为文件 button");
        assert_eq!(send["value"]["return_dir"], "/tmp/proj");
        assert_eq!(send["value"]["return_show_hidden"], true);
        assert_eq!(send["value"]["return_keyword"], "no");
        assert_eq!(send["value"]["return_page"], 2);
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
