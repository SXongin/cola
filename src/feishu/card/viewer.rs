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
use super::session::{BrowseView, browse_view_value, pager_element};
use super::shell::card_shell;
use super::tool_render::{FileDelivery, delivery_suffix, file_image_element};
use super::{FEISHU_CARD_LIMIT_BYTES, MAX_CARD_JSON_CHARS, fenced_code};

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
/// (`📎 name · mime · size · 已内嵌` / `· 已发送为文件消息` / `· 未发送` or an
/// ambiguous-send status) and, for
/// an embedded image, the `img` element. Named so a file past Feishu's 30MB cap
/// — which is never read — can still carry its real size and the 未发送 state.
pub(crate) struct ViewerFileLane {
    pub(crate) mime: String,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) delivery: FileDelivery,
    /// Identity of the bytes this lane actually names, for stateless refreshes.
    pub(crate) content_identity: Option<u64>,
    /// An image already embedded before a later file transfer. It remains on
    /// the viewer while `delivery` records the additional File Message.
    pub(crate) embedded_image_key: Option<String>,
    /// The Rich Text Message carrying a structural overflow, when one landed.
    /// This id is carried by every viewer control instead of process-local state.
    pub(crate) rich_message_id: Option<String>,
    /// A refresh of the carried Rich Text Message failed without replacing it.
    pub(crate) refresh_failed: bool,
}

/// Why a preview cannot ride the card (ADR-0083 §5, #741): the RENDERED
/// structure — element/row counts, never byte size — past what a card
/// holds. A large single code block that fits the budget is none of these and
/// stays on the card.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StructuralOverflow {
    /// A delimited preview past the card's row budget: more body rows than
    /// [`MAX_TABLE_ROWS`].
    TableOverBudget {
        /// Markdown tables the preview renders (always one: a single-file
        /// preview renders a single markdown table, so the card's five-table
        /// cap is unreachable here — the card-level sanitizer still enforces
        /// it across a card's elements).
        tables: usize,
        /// Body rows the preview's table carries.
        rows: usize,
    },
}

/// Detect whether a preview the card would render structurally overflows it
/// (#741, ADR-0083 §5): branch on the RENDERED shape — tables and rows for
/// delimited text — never on byte size. Text is always pageable, including a
/// minified one-line file (#740). `None` when the preview fits the card's
/// structural limits. The byte lanes ([`ViewerKind::Image`],
/// [`ViewerKind::Binary`], [`ViewerKind::TextTooLarge`]) never take this lane.
pub(crate) fn detect_structural_overflow(
    kind: ViewerKind,
    ext: Option<&str>,
    text: &str,
) -> Option<StructuralOverflow> {
    match kind {
        ViewerKind::Text => None,
        ViewerKind::Delimited => {
            let delim = delim_for(ext);
            let mut records = delimited_records(text, delim);
            // No records at all: no table renders (the card fences the bytes),
            // so the preview is one fenced block — the text rule.
            records.next()?;
            let mut rows = 0usize;
            for _ in records {
                rows += 1;
                // Past the row cap the verdict is settled: stop pulling, so a
                // file of millions of rows costs this decision a bounded
                // prefix, never the whole file.
                if rows > MAX_TABLE_ROWS {
                    break;
                }
            }
            // Only the row cap can fire here: a single-file preview renders a
            // single markdown table, so the card's five-table cap is
            // unreachable from this lane (no second table can ever render).
            (rows > MAX_TABLE_ROWS).then_some(StructuralOverflow::TableOverBudget { tables: 1, rows })
        }
        ViewerKind::Image | ViewerKind::Binary | ViewerKind::TextTooLarge => None,
    }
}

/// The delimiter one delimited-text preview parses with: tab for `.tsv`,
/// comma otherwise.
fn delim_for(ext: Option<&str>) -> char {
    if ext.map(str::to_ascii_lowercase).as_deref() == Some("tsv") {
        '\t'
    } else {
        ','
    }
}

/// The Rich Text Message body for a structurally-overflowing preview (#741):
/// the content intact — the full fenced block for text (with its language
/// hint), the full markdown table for delimited text (every row, unclipped).
/// `None` when that body would exceed the documented 30 KB message-body cap
/// ([`FEISHU_CARD_LIMIT_BYTES`], the card limit this lane sizes itself by):
/// such content does NOT take this lane — it goes to the File Message lane,
/// with the card saying so. The shared cap is the documented card limit, not
/// a measured `post` limit — no `post` body was ever sized against the live
/// platform here — so this lane treats it as the budget, not a proven fact.
/// `None` for the byte lanes, which never take this lane.
pub(crate) fn rich_post_md(kind: ViewerKind, ext: Option<&str>, text: &str) -> Option<String> {
    match kind {
        ViewerKind::Text => {
            // Byte length lower-bounds the serialized body (JSON escaping only
            // expands), so a preview past the cap is decided with no build.
            if text.len() > FEISHU_CARD_LIMIT_BYTES {
                return None;
            }
            let md = fenced_code(text, Some(language_token(ext)));
            (serialized_len(&md) + 128 <= FEISHU_CARD_LIMIT_BYTES).then_some(md)
        }
        ViewerKind::Delimited => full_delimited_table(text, delim_for(ext)),
        ViewerKind::Image | ViewerKind::Binary | ViewerKind::TextTooLarge => None,
    }
}

/// The `post` message content carrying `md` as its one rich-text element —
/// the shape the generic reply path sends as `msg_type: "post"` (the same
/// single-`md` envelope cc-connect's card-overflow fallback uses).
pub(crate) fn rich_post_content(md: &str) -> serde_json::Value {
    json!({
        "zh_cn": {
            "content": [[{ "tag": "md", "text": md }]],
        },
    })
}

/// A delimited-text preview as one WHOLE markdown table for the Rich Text
/// Message lane (#741): the first record is the header, the rest the body —
/// every row, every column, no clipping — because the lane carries the content
/// intact or not at all. `None` when the text holds no records, or when the
/// table would exceed the documented 30 KB body cap
/// ([`FEISHU_CARD_LIMIT_BYTES`], the card limit this lane sizes itself by —
/// asserted from the documented limit, never measured against a live `post`
/// send): the build aborts past the cap, so a file of millions of rows costs
/// a bounded prefix, never the whole file (the File Message lane owns what
/// does not fit here).
///
/// Cells keep their structural validity: a `|` cannot split the row and an
/// embedded newline stays on one markdown line — the same validity the card's
/// own [`table_row`] keeps, without its budgets.
fn full_delimited_table(text: &str, delim: char) -> Option<String> {
    let mut records = full_delimited_records(text, delim);
    let header = records.next()?;
    if records.overflowed() {
        return None;
    }
    let mut lines = vec![full_table_row(&header)];
    lines.push(full_table_row(&vec!["---".to_string(); header.len().max(1)]));
    let mut used = lines.iter().map(|line| serialized_len(line) + 8).sum::<usize>();
    if used > FEISHU_CARD_LIMIT_BYTES {
        return None;
    }
    while let Some(row) = records.next() {
        if records.overflowed() {
            return None;
        }
        let line = full_table_row(&row);
        used += serialized_len(&line) + 8;
        if used > FEISHU_CARD_LIMIT_BYTES {
            return None;
        }
        lines.push(line);
    }
    let table = lines.join("\n");
    (serialized_len(&table) + 128 <= FEISHU_CARD_LIMIT_BYTES).then_some(table)
}

/// One markdown table row for the Rich Text Message lane: every column kept, a
/// `|` inside a cell escaped so it cannot split the row, a newline inside a
/// cell flattened so the row stays on one markdown line — the same validity
/// the card's own [`table_row`] keeps, without its budgets.
fn full_table_row(cells: &[String]) -> String {
    let shown: Vec<String> = cells
        .iter()
        .map(|cell| cell.replace(['\n', '\r'], " ").replace('|', "\\|"))
        .collect();
    format!("| {} |", shown.join(" | "))
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

/// How a delimited preview was cut to fit the card, so its note names the rows,
/// columns and budget limits that actually affected the rendered table
/// (ADR-0083 §6).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TableCut {
    /// Body rows present in the rendered table (not including its header).
    rows_shown: usize,
    /// At least one body row was omitted.
    rows_dropped: bool,
    /// At least one rendered row/header had columns beyond the preview cap.
    columns_dropped: bool,
    /// A row did not fit the remaining card JSON budget.
    budget_limited: bool,
    /// Cells longer than [`MAX_CELL_CHARS`] were clipped.
    cells: bool,
}

/// Everything the viewer card renders.
pub(crate) struct ViewerCard<'a> {
    /// The **resolved** absolute path (never the typed spelling).
    pub(crate) path: &'a str,
    /// The directory the current Session uses as its path scope, when known.
    pub(crate) session_dir: Option<&'a str>,
    /// The file's byte size.
    pub(crate) size: u64,
    /// What the viewer decided the bytes are.
    pub(crate) kind: ViewerKind,
    /// The extension (without the dot), as the path spells it.
    pub(crate) ext: Option<&'a str>,
    /// The preview's text: the bytes read, already bounded by the read cap.
    pub(crate) text: &'a str,
    /// Stable process-local identity for this exact File Content snapshot,
    /// carried by re-read and transfer controls to rotate deliver-once state.
    pub(crate) content_identity: Option<u64>,
    /// Whether the card layer already dropped preview content (the card budget
    /// cut it). A file past the preview read cap is never previewed at all, so
    /// it takes the File Message lane rather than being truncated here.
    pub(crate) truncated: bool,
    /// The 1-based page of the line-windowed text preview this card shows
    /// (#740, ADR-0083 §6): a text preview longer than one card holds pages by
    /// lines through the shared pager, and a flip rebuilds the card at the
    /// payload's page — the viewer keeps no per-card state, so a rebuild from
    /// a flipped page renders the same window (ADR-0052). `1` on open; the
    /// builder clamps it into `[1, total_pages]`. Ignored by every other kind.
    pub(crate) page: usize,
    /// The browse listing this viewer was opened from, when it was (ADR-0083
    /// §3): renders 「← 返回目录」 carrying that state. `None` for `/cat`.
    pub(crate) return_to: Option<BrowseView<'a>>,
    /// The whole-bytes lane this card shows (ADR-0083 §5), when it has one: the
    /// tracking line and, for an embedded image, the `img` element. `None` for a
    /// text preview that took no lane on open (its 「发为文件」 transfer still
    /// works — the click carries the path).
    pub(crate) lane: Option<ViewerFileLane>,
    /// The preview went out as its own message and LANDED (#741): a
    /// structurally overflowing preview rides the Rich Text Message lane (or
    /// the File Message lane past the post cap), so the card carries NO
    /// preview elements or truncation note. Only a File Message carries the
    /// original bytes; Rich Text is a rendered preview and keeps 「发为文件」.
    /// A whole-bytes lane for an image or binary likewise records where its
    /// content went; an embedded image remains visible and keeps the explicit
    /// File Message action. `false` whenever a required
    /// lane did not land: a structural overflow then keeps its truncated
    /// preview, honest cut note and rescue button, with the tracking line
    /// stating the content was not delivered.
    pub(crate) overflowed: bool,
}

/// Build the File Viewer card (ADR-0083 §3).
pub(crate) fn build_viewer_card(thread_key: &ThreadKey, view: ViewerCard<'_>) -> serde_json::Value {
    // One markdown hygiene state per card build, so the table budget the
    // preview's own table spends is counted exactly once.
    let mut markdown = CardMarkdown::new();
    let mut elements: Vec<serde_json::Value> = Vec::new();
    let truncated = view.truncated;
    let mut clipped_cells = false;
    let mut table_cut = None;

    elements.push(json!({
        "tag": "markdown",
        "content": markdown.clean(&naming_block(&view)),
    }));

    // A LANDED overflow preview went out as its own message (#741): the card
    // carries no preview, no truncation note and no rescue delivery — the
    // tracking line below says where the content went. A preview the lane did
    // NOT deliver keeps all three, so the content stays reachable on the card.
    if !view.overflowed {
        match view.kind {
            // An image the open lane embedded: the `img` element IS the preview.
            // An image Feishu would not embed (past its caps) has no element — its
            // tracking line below says it went out as a File Message instead.
            ViewerKind::Image => {
                if let Some(lane) = &view.lane {
                    let image_key = lane.embedded_image_key.as_deref().or(match &lane.delivery {
                        FileDelivery::Embedded { image_key } => Some(image_key.as_str()),
                        _ => None,
                    });
                    if let Some(image_key) = image_key {
                        elements.push(file_image_element(image_key, &lane.name));
                    }
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
                        clipped_cells = cut.cells;
                        table_cut = Some(cut);
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
                // ONE fenced block when it fits the card's preview budget — a
                // text preview is never cut per element, so a line is never
                // split and the fence never appears twice. The budget is
                // cola's own card-JSON share, not a Feishu per-element cap, so
                // a single large block is exactly what the paged path already
                // renders per page.
                let block = fenced_code(view.text, Some(lang));
                if serialized_len(&block) + 128 <= PREVIEW_JSON_BUDGET {
                    elements.push(json!({
                        "tag": "markdown",
                        "content": markdown.clean(&block),
                    }));
                } else {
                    // Past it, the shared pager windows the preview by LINES
                    // (#740): the budget decides only THAT it pages, never
                    // what the reader sees.
                    push_text_page(&mut elements, &mut markdown, thread_key, &view, lang);
                }
            }
        }
    }

    if !view.overflowed {
        if truncated {
            elements.push(json!({
                "tag": "markdown",
                "content": "_（内容过长，预览已截断）_",
            }));
        }
        if let Some(cut) = table_cut.filter(|cut| cut.rows_dropped || cut.columns_dropped) {
            elements.push(json!({
                "tag": "markdown",
                "content": markdown.clean(&table_cut_note(cut)),
            }));
        }
        if clipped_cells {
            elements.push(json!({
                "tag": "markdown",
                "content": format!("_（过长的单元格已截断，每格最多 {MAX_CELL_CHARS} 字）_"),
            }));
        }
    }

    // The whole-bytes lane's tracking line: names the file and says where its
    // bytes went — 已内嵌 (an image in this card), 已发送为文件消息 (one File
    // Message), 已发送为富文本消息 (one Rich Text Message, #741), or 未发送
    // (past Feishu's 30MB cap, or a failed upload/send).
    if let Some(lane) = &view.lane {
        elements.push(json!({
            "tag": "markdown",
            "content": markdown.clean(&lane_line(lane)),
        }));
    }

    elements.push(viewer_buttons(thread_key, &view));
    card_shell(&format!("📄 {}", file_name(view.path)), "blue", elements)
}

/// The paged text preview (#740, ADR-0083 §6): the requested 1-based page's
/// window of whole lines as one fenced block, plus the shared pager — whose
/// buttons carry the file's path, the TARGET page and (when opened from one)
/// the browse listing's whole state, so a flip rebuilds the same window with
/// the way back intact (ADR-0052's statelessness). The pager reports the
/// preview's TRUE line total, so a reader knows what they are not seeing. A
/// preview that fits one card never reaches here, so a single-page preview
/// renders no pager.
fn push_text_page(
    elements: &mut Vec<serde_json::Value>,
    markdown: &mut CardMarkdown,
    thread_key: &ThreadKey,
    view: &ViewerCard<'_>,
    lang: &str,
) {
    let pages = paginate_text(view.text, lang);
    let total_pages = pages.windows.len().max(1);
    let page = view.page.clamp(1, total_pages);
    let (start, end) = pages.windows.get(page - 1).copied().unwrap_or((0, 0));
    let mut window_text = String::new();
    for segment in pages.segments.get(start..end).unwrap_or(&[]) {
        if !window_text.is_empty() {
            window_text.push('\n');
        }
        match segment {
            PageSegment::Line(index) => window_text.push_str(pages.lines[*index]),
            PageSegment::Piece(piece) => window_text.push_str(piece),
        }
    }
    let block = fenced_code(&window_text, Some(lang));
    elements.push(json!({
        "tag": "markdown",
        "content": markdown.clean(&block),
    }));
    // The pager's extra fields are strings only: the path plus the browse
    // listing to return to, mirroring the 「发为文件」 payload's `return_*`
    // shape — never the row's `dir`/`page`, whose `page` is the LISTING's.
    let return_page = view.return_to.map(|listing| listing.page.to_string());
    let mut extra: Vec<(&str, &str)> = vec![("path", view.path)];
    let lane_identity = view.content_identity.map(|identity| identity.to_string());
    if let Some(identity) = lane_identity.as_deref() {
        extra.push(("viewer_file_identity", identity));
    }
    if let Some(session_dir) = view.session_dir {
        extra.push(("session_dir", session_dir));
    }
    if let Some(listing) = view.return_to {
        extra.push(("return_dir", listing.dir));
        extra.push((
            "return_show_hidden",
            if listing.show_hidden { "true" } else { "false" },
        ));
        extra.push(("return_keyword", listing.keyword));
        extra.push(("return_page", return_page.as_deref().unwrap_or("1")));
    }
    let lane_size = view.lane.as_ref().map(|lane| lane.size.to_string());
    let lane_delivery = view.lane.as_ref().map(|lane| match &lane.delivery {
        super::tool_render::FileDelivery::Unresolved => "unresolved",
        super::tool_render::FileDelivery::Embedded { .. } => "embedded",
        super::tool_render::FileDelivery::SentAsFile => "sent_as_file",
        super::tool_render::FileDelivery::FileSendOutcomeUnknown => "file_send_outcome_unknown",
        super::tool_render::FileDelivery::SentAsRichText => "sent_as_rich_text",
        super::tool_render::FileDelivery::Undelivered => "undelivered",
    });
    let lane_image_key = view.lane.as_ref().and_then(|lane| {
        lane.embedded_image_key.as_deref().or(match &lane.delivery {
            super::tool_render::FileDelivery::Embedded { image_key } => Some(image_key.as_str()),
            _ => None,
        })
    });
    let lane_rich_message_id = view
        .lane
        .as_ref()
        .and_then(|lane| lane.rich_message_id.as_deref());
    let lane_content_identity = view
        .lane
        .as_ref()
        .and_then(|lane| lane.content_identity)
        .map(|identity| identity.to_string());
    let lane_refresh_failed = view
        .lane
        .as_ref()
        .map(|lane| if lane.refresh_failed { "true" } else { "false" });
    if let (Some(lane), Some(size), Some(delivery), Some(refresh_failed)) = (
        view.lane.as_ref(),
        lane_size.as_deref(),
        lane_delivery,
        lane_refresh_failed,
    ) {
        extra.extend([
            ("viewer_lane_mime", lane.mime.as_str()),
            ("viewer_lane_name", lane.name.as_str()),
            ("viewer_lane_size", size),
            ("viewer_lane_delivery", delivery),
            ("viewer_lane_refresh_failed", refresh_failed),
        ]);
        if let Some(image_key) = lane_image_key {
            extra.push(("viewer_lane_image_key", image_key));
        }
        if let Some(message_id) = lane_rich_message_id {
            extra.push(("viewer_lane_rich_message_id", message_id));
        }
        if let Some(identity) = lane_content_identity.as_deref() {
            extra.push(("viewer_lane_content_identity", identity));
        }
    }
    if let Some(pager) = pager_element(
        "file",
        thread_key,
        "",
        None,
        page,
        total_pages,
        pages.lines.len(),
        &extra,
    ) {
        elements.push(pager);
    }
}

/// A line-windowed text preview: `lines` are the preview's TRUE lines (what
/// the pager reports the total of), `segments` are what packs into windows —
/// whole lines, except a line that alone outgrows a page contributes its
/// char-split pieces — and `windows` are the `(start, end)` SEGMENT ranges the
/// pager flips through, each rendering as one fenced block within
/// [`PREVIEW_JSON_BUDGET`].
struct TextPages<'a> {
    lines: Vec<&'a str>,
    segments: Vec<PageSegment>,
    windows: Vec<(usize, usize)>,
}

/// One packable segment of a text preview: a whole line by index, or one
/// char-split piece of an overlong line.
enum PageSegment {
    Line(usize),
    Piece(String),
}

impl PageSegment {
    /// The segment's serialized cost, longest backtick run and line count —
    /// what the packer accumulates.
    fn measure(&self, lines: &[&str]) -> (usize, usize, usize) {
        let text = match self {
            PageSegment::Line(index) => lines[*index],
            PageSegment::Piece(piece) => piece,
        };
        (serialized_len(text), line_tick_run(text), 1)
    }
}

/// Split a text preview into line windows for the pager (#740): whole lines
/// packed greedily so each window's fenced block fits [`PREVIEW_JSON_BUDGET`] —
/// a line that alone outgrows a page (a minified one-liner) is char-split
/// first, so every window still fits and no card is ever rejected.
fn paginate_text<'a>(text: &'a str, lang: &str) -> TextPages<'a> {
    let lines: Vec<&str> = text.lines().collect();
    let mut segments: Vec<PageSegment> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let (cost, run, _) = PageSegment::Line(index).measure(&lines);
        if page_block_cost(cost, run, 1, lang) + 128 <= PREVIEW_JSON_BUDGET {
            segments.push(PageSegment::Line(index));
        } else {
            segments.extend(
                split_overlong_line(line, lang)
                    .into_iter()
                    .map(PageSegment::Piece),
            );
        }
    }
    // Greedily pack segments; each window's cost is exact (see
    // `page_block_cost`), so a window never outgrows the page budget.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut sum = 0usize;
    let mut run = 0usize;
    let mut count = 0usize;
    for (index, segment) in segments.iter().enumerate() {
        let (cost, segment_run, segment_count) = segment.measure(&lines);
        if count > 0
            && page_block_cost(sum + cost, run.max(segment_run), count + segment_count, lang) + 128
                > PREVIEW_JSON_BUDGET
        {
            windows.push((start, index));
            start = index;
            sum = 0;
            run = 0;
            count = 0;
        }
        sum += cost;
        run = run.max(segment_run);
        count += segment_count;
    }
    if start < segments.len() {
        windows.push((start, segments.len()));
    }
    TextPages {
        lines,
        segments,
        windows,
    }
}

/// The exact [`serialized_len`] of a fenced page block whose lines cost `sum`
/// serialized chars, carry longest backtick run `run` and number `n` — JSON
/// escaping is per-character, so joining lines is additive and a window is
/// measured without ever rebuilding its block. `lang` is the fence's language
/// token, whose bytes the head carries.
fn page_block_cost(sum: usize, run: usize, n: usize, lang: &str) -> usize {
    let fence_len = (run + 1).max(3);
    let head_len = fence_len + lang.len();
    head_len + fence_len + sum + 2 * (n + 1)
}

/// A line's longest backtick run — the page fence must outrun it, exactly as
/// [`fenced_code`] outruns its whole block's.
fn line_tick_run(line: &str) -> usize {
    let mut best = 0usize;
    let mut run = 0usize;
    for c in line.chars() {
        if c == '`' {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

/// Split one overlong line — alone past a page's budget — into pieces that
/// each fit a page, halving on char boundaries and measuring exactly, so a
/// minified one-liner still pages instead of breaking the card.
fn split_overlong_line(line: &str, lang: &str) -> Vec<String> {
    let mut pieces = vec![line.to_string()];
    let mut index = 0;
    while index < pieces.len() {
        let fits = {
            let piece = &pieces[index];
            page_block_cost(serialized_len(piece), line_tick_run(piece), 1, lang) + 128 <= PREVIEW_JSON_BUDGET
        };
        if fits {
            index += 1;
            continue;
        }
        let piece = std::mem::take(&mut pieces[index]);
        let mid = char_mid(&piece);
        if mid == 0 || mid >= piece.len() {
            // A single character always fits a page — unreachable, but never
            // loop forever on it.
            pieces[index] = piece;
            index += 1;
            continue;
        }
        pieces[index] = piece[..mid].to_string();
        pieces.insert(index + 1, piece[mid..].to_string());
    }
    pieces
}

/// A mid-string char boundary for halving: half the bytes, floored to a
/// boundary so a char is never split.
fn char_mid(text: &str) -> usize {
    let mut mid = text.len() / 2;
    while mid > 0 && !text.is_char_boundary(mid) {
        mid -= 1;
    }
    mid
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
    if lane.refresh_failed {
        return match lane.rich_message_id.as_deref() {
            Some(message_id) => format!("{base} · 富文本刷新失败；旧消息仍在 `{message_id}`"),
            None => format!("{base} · 刷新失败"),
        };
    }
    if matches!(lane.delivery, FileDelivery::SentAsRichText)
        && let Some(message_id) = lane.rich_message_id.as_deref()
    {
        return format!("{base} · 已发送为富文本消息（当前消息 `{message_id}`）");
    }
    match delivery_suffix(&lane.delivery) {
        Some(suffix) => format!("{base} · {suffix}"),
        None => base,
    }
}

/// The viewer's button row (ADR-0083 §3, §5): 「← 返回目录」 when opened from a
/// browse listing, otherwise 「📂 所在目录」, and 「发为文件」 unless the
/// preview already landed as its own message (`overflowed`): a landed preview
/// needs no rescue delivery, while a preview the lane did NOT deliver keeps
/// the button as its way out. The directory control carries the browse
/// handler's own payload shape ([`browse_view_value`]) so #742 only has to
/// pass the listing in; the transfer button carries the RESOLVED path the
/// click must upload, plus the browse listing's whole state so the card it
/// PATCHes back to can still carry 「← 返回目录」.
fn viewer_buttons(thread_key: &ThreadKey, view: &ViewerCard<'_>) -> serde_json::Value {
    let mut columns = vec![button_column("🔄 重新读取", reread_value(thread_key, view))];
    columns.extend(directory_columns(thread_key, view.path, view.return_to));
    // A File Message already carries the original bytes; Rich Text carries only
    // a rendered preview, so the transfer action remains essential there.
    // An ambiguous timed-out File Message is also not retried: it may have
    // landed and Feishu cannot edit or retract it.
    let file_message_may_already_be_delivered = view.lane.as_ref().is_some_and(|lane| {
        matches!(
            &lane.delivery,
            super::tool_render::FileDelivery::SentAsFile
                | super::tool_render::FileDelivery::FileSendOutcomeUnknown
        )
    });
    if !file_message_may_already_be_delivered {
        columns.push(button_column("发为文件", transfer_value(thread_key, view)));
    }
    button_row(columns)
}

/// The stateless re-read action (ADR-0052, ADR-0083 §9): the resolved file,
/// current viewer page, return-listing state and existing overflow message id.
/// The viewer's own Feishu message id is intentionally absent: the card is
/// built before Feishu returns that id, and `ws` supplies it as callback context.
fn reread_value(thread_key: &ThreadKey, view: &ViewerCard<'_>) -> serde_json::Value {
    let mut value = json!({
        "action": "file",
        "op": "reread",
        "path": view.path,
        "chat_id": thread_key.chat_id,
        "thread_id": thread_key.thread_id,
        "page": view.page,
    });
    if let Some(return_to) = view.return_to {
        value["return_dir"] = json!(return_to.dir);
        value["return_show_hidden"] = json!(return_to.show_hidden);
        value["return_keyword"] = json!(return_to.keyword);
        value["return_page"] = json!(return_to.page);
    }
    if let Some(session_dir) = view.session_dir {
        value["session_dir"] = json!(session_dir);
    }
    if let Some(identity) = view.content_identity {
        value["viewer_file_identity"] = json!(identity.to_string());
        if view
            .lane
            .as_ref()
            .is_some_and(|lane| matches!(lane.delivery, FileDelivery::SentAsFile))
        {
            value["viewer_sent_file_identity"] = json!(identity.to_string());
        } else if view
            .lane
            .as_ref()
            .is_some_and(|lane| matches!(lane.delivery, FileDelivery::FileSendOutcomeUnknown))
        {
            value["viewer_ambiguous_file_identity"] = json!(identity.to_string());
        }
    }
    if let Some(image_key) = view.lane.as_ref().and_then(|lane| {
        lane.embedded_image_key.as_deref().or(match &lane.delivery {
            FileDelivery::Embedded { image_key } => Some(image_key.as_str()),
            _ => None,
        })
    }) {
        value["viewer_embedded_image_key"] = json!(image_key);
    }
    if let Some(message_id) = view
        .lane
        .as_ref()
        .and_then(|lane| lane.rich_message_id.as_deref())
    {
        value["rich_message_id"] = json!(message_id);
    }
    value
}

/// The directory half of the viewer's button row (ADR-0083 §3): 「← 返回目录」
/// when the viewer replaced a browse listing, otherwise 「📂 所在目录」. Shared by the viewer card and
/// the failed-transfer card, which keeps the way back but carries no
/// 「发为文件」 (there is nothing left to send).
fn directory_columns(
    thread_key: &ThreadKey,
    path: &str,
    return_to: Option<BrowseView<'_>>,
) -> Vec<serde_json::Value> {
    let mut columns: Vec<serde_json::Value> = Vec::new();
    if let Some(return_to) = return_to {
        columns.push(button_column(
            "← 返回目录",
            browse_view_value(thread_key, "restore", return_to),
        ));
    } else if let Some(parent) = std::path::Path::new(path).parent() {
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
    columns
}

/// Recover the whole-bytes lane carried by a pager callback, so a stateless
/// page flip cannot erase the tracking line or re-enable a completed transfer.
pub(crate) fn lane_from_page_payload(value: &serde_json::Value) -> Option<ViewerFileLane> {
    let mime = value.get("viewer_lane_mime")?.as_str()?.to_string();
    let name = value.get("viewer_lane_name")?.as_str()?.to_string();
    let size = value.get("viewer_lane_size")?.as_str()?.parse().ok()?;
    let delivery = match value.get("viewer_lane_delivery")?.as_str()? {
        "unresolved" => super::tool_render::FileDelivery::Unresolved,
        "embedded" => super::tool_render::FileDelivery::Embedded {
            image_key: value.get("viewer_lane_image_key")?.as_str()?.to_string(),
        },
        "sent_as_file" => super::tool_render::FileDelivery::SentAsFile,
        "file_send_outcome_unknown" => super::tool_render::FileDelivery::FileSendOutcomeUnknown,
        "sent_as_rich_text" => super::tool_render::FileDelivery::SentAsRichText,
        "undelivered" => super::tool_render::FileDelivery::Undelivered,
        _ => return None,
    };
    Some(ViewerFileLane {
        mime,
        name,
        size,
        delivery,
        embedded_image_key: value
            .get("viewer_lane_image_key")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        rich_message_id: value
            .get("viewer_lane_rich_message_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        refresh_failed: value
            .get("viewer_lane_refresh_failed")
            .and_then(serde_json::Value::as_str)
            == Some("true"),
        content_identity: value
            .get("viewer_lane_content_identity")
            .and_then(serde_json::Value::as_str)
            .and_then(|identity| identity.parse().ok()),
    })
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
        // The viewer page the click came from, so the transfer's card PATCH
        // rebuilds the page the reader was on (#740).
        "page": view.page,
    });
    if let Some(return_to) = view.return_to {
        value["return_dir"] = json!(return_to.dir);
        value["return_show_hidden"] = json!(return_to.show_hidden);
        value["return_keyword"] = json!(return_to.keyword);
        value["return_page"] = json!(return_to.page);
    }
    if let Some(session_dir) = view.session_dir {
        value["session_dir"] = json!(session_dir);
    }
    if let Some(identity) = view.content_identity {
        value["viewer_file_identity"] = json!(identity.to_string());
    }
    if let Some(image_key) = view.lane.as_ref().and_then(|lane| {
        lane.embedded_image_key.as_deref().or(match &lane.delivery {
            super::tool_render::FileDelivery::Embedded { image_key } => Some(image_key.as_str()),
            _ => None,
        })
    }) {
        value["viewer_embedded_image_key"] = json!(image_key);
    }
    // The transfer does not erase an existing Rich Text Message: its id (and
    // a prior refresh failure) must ride the payload so the rebuilt card keeps
    // tracking it, and a later 重新读取 edits that message instead of posting
    // a duplicate (#744, Codex gate round 7). Strings, like the pager's own
    // lane carriage (`lane_from_page_payload`).
    if let Some(lane) = view.lane.as_ref() {
        if let Some(message_id) = lane.rich_message_id.as_deref() {
            value["viewer_lane_rich_message_id"] = json!(message_id);
        }
        if lane.refresh_failed {
            value["viewer_lane_refresh_failed"] = json!("true");
        }
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
        columns_dropped: header.len() > MAX_PREVIEW_COLUMNS,
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
            cut.rows_dropped = true;
            break;
        }
        let (line, clipped) = table_row(&row);
        let cost = serialized_len(&line) + 8;
        if used + cost > PREVIEW_JSON_BUDGET {
            cut.rows_dropped = true;
            cut.budget_limited = true;
            break;
        }
        used += cost;
        lines.push(line);
        cut.rows_shown += 1;
        cut.columns_dropped |= row.len() > MAX_PREVIEW_COLUMNS;
        cut.cells |= clipped;
    }
    Some((lines.join("\n"), cut))
}

/// A truthful note for the delimited preview's actual cuts. The parser stops
/// as soon as the row cap or JSON budget is reached, so it never invents a
/// total for rows it deliberately did not scan.
fn table_cut_note(cut: TableCut) -> String {
    let mut details = vec![format!("显示 {} 行", cut.rows_shown)];
    if cut.rows_dropped {
        details.push(if cut.budget_limited {
            "预览预算不足，后续行未显示".to_string()
        } else {
            "达到行数上限，后续行未显示".to_string()
        });
    }
    if cut.columns_dropped {
        details.push(format!("超出前 {MAX_PREVIEW_COLUMNS} 列的列已省略"));
    }
    format!("_（{}）_", details.join("；"))
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
    DelimitedRecords::new(text, delim, MAX_CELL_CHARS, MAX_PREVIEW_COLUMNS)
}

/// The rich-text parser has no preview cell/column caps. It keeps only a
/// body-sized prefix of a pathological cell or row so it can reject the post
/// lane without silently clipping or allocating an unbounded record.
fn full_delimited_records(text: &str, delim: char) -> DelimitedRecords<'_> {
    DelimitedRecords::new(text, delim, FEISHU_CARD_LIMIT_BYTES, FEISHU_CARD_LIMIT_BYTES)
}

#[cfg(test)]
thread_local! {
    /// Characters the delimited parser has pulled on the calling thread (tests
    /// only): [`delimited_table`] must stop pulling at the card's caps, and the
    /// renderer's own test needs to observe that from the CARD build, not just
    /// from the parser directly (finding #739 §1, round 2 — a regression that
    /// collected every record before rendering would otherwise pass). A
    /// thread-local keeps each test's build to itself.
    static DELIMITED_SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The lazy parser behind [`delimited_records`]: it pulls characters from the
/// text on demand and yields one bounded record per `next`.
struct DelimitedRecords<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    delim: char,
    max_cell_chars: usize,
    max_columns: usize,
    overflowed: bool,
    /// Set once the input is exhausted, so a later `next` never restarts.
    done: bool,
    /// Characters pulled so far — the bounded-work measure the test reads; it
    /// does not exist in the production build.
    #[cfg(test)]
    scanned: usize,
}

impl DelimitedRecords<'_> {
    fn new(text: &str, delim: char, max_cell_chars: usize, max_columns: usize) -> DelimitedRecords<'_> {
        DelimitedRecords {
            chars: text.chars().peekable(),
            delim,
            max_cell_chars,
            max_columns,
            overflowed: false,
            done: false,
            #[cfg(test)]
            scanned: 0,
        }
    }

    fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// How many characters the parse has pulled so far (tests only).
    #[cfg(test)]
    fn scanned(&self) -> usize {
        self.scanned
    }

    /// Count one character pulled from the input (tests only). The per-instance
    /// count is what a parser-level test reads; the thread-local total is what
    /// the CARD build's test reads, so it can see that `delimited_table` stopped
    /// pulling even though it holds no handle on the parser.
    #[inline]
    fn count(&mut self) {
        #[cfg(test)]
        {
            self.scanned += 1;
            DELIMITED_SCANNED.with(|total| total.set(total.get() + 1));
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
                        self.overflowed |= cell.push('"', self.max_cell_chars);
                        self.chars.next();
                        self.count();
                    }
                    '"' => quoted = false,
                    _ => self.overflowed |= cell.push(c, self.max_cell_chars),
                }
                continue;
            }
            if c == '"' && cell.is_blank() {
                quoted = true;
                touched = true;
            } else if c == self.delim {
                self.overflowed |= push_record_cell(&mut record, &mut cell, self.max_columns);
                touched = true;
            } else if c == '\n' || c == '\r' {
                if c == '\r' && self.chars.peek() == Some(&'\n') {
                    self.chars.next();
                    self.count();
                }
                self.overflowed |= push_record_cell(&mut record, &mut cell, self.max_columns);
                if touched {
                    return Some(std::mem::take(&mut record));
                }
                record.clear();
                touched = false;
            } else {
                if !c.is_whitespace() {
                    touched = true;
                }
                self.overflowed |= cell.push(c, self.max_cell_chars);
            }
        }
        // The final record: a file may not end in a newline.
        self.done = true;
        if touched {
            self.overflowed |= push_record_cell(&mut record, &mut cell, self.max_columns);
            Some(record)
        } else {
            None
        }
    }
}

/// One parsed cell, bounded: it stores at most its parser's limit + 1
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
    /// Append `c`, holding the stored text to `max_chars` + 1 characters.
    /// Returns true once the input exceeds the storage bound.
    fn push(&mut self, c: char, max_chars: usize) -> bool {
        if !self.started {
            if c.is_whitespace() {
                return false;
            }
            self.started = true;
        }
        if self.counted <= max_chars {
            self.text.push(c);
        }
        self.counted += 1;
        self.counted > max_chars
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
fn push_record_cell(record: &mut Vec<String>, cell: &mut BoundedCell, max_columns: usize) -> bool {
    if record.len() <= max_columns {
        record.push(cell.take());
        false
    } else {
        cell.reset();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feishu::card::{MAX_ELEMENT_TEXT_CHARS, chunk_text};

    /// The per-chunk size `ViewerKind::Text` fenced before #769 (the removed
    /// `PREVIEW_CHUNK_CHARS`): the old render split anything longer into several
    /// elements, so a card rendering such a fixture as ONE element is a shape
    /// the old code could not produce.
    const OLD_PREVIEW_CHUNK_CHARS: usize = MAX_ELEMENT_TEXT_CHARS - 64;

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

    /// The card's fenced preview blocks, in body order: the markdown elements
    /// that open a fence (never the naming block, note or tracking line).
    fn preview_blocks(card: &serde_json::Value) -> Vec<&str> {
        card["body"]["elements"]
            .as_array()
            .expect("the card body carries elements")
            .iter()
            .filter_map(|element| {
                element["content"]
                    .as_str()
                    .filter(|content| content.starts_with("```"))
            })
            .collect()
    }

    /// Whether the card carries the shared pager — any page-targeting button.
    fn has_pager(card: &serde_json::Value) -> bool {
        card_buttons(card)
            .iter()
            .any(|button| button["value"]["op"] == "page")
    }

    /// The two text shapes' boundary, read off what the CARD renders: the last
    /// text length that still renders as one whole block `(whole, paged)`, and
    /// the first that pages. The predicate is monotone in length, so a binary
    /// search reaches the adjacent pair in ~30 card builds rather than one per
    /// length — and it never re-derives the budget arithmetic the render uses.
    fn whole_and_paged_boundary() -> (usize, usize) {
        let text_card = |len: usize| {
            build_viewer_card(
                &key(),
                view("/tmp/notes.txt", ViewerKind::Text, Some("txt"), &"x".repeat(len)),
            )
        };
        let pages = |len: usize| has_pager(&text_card(len));
        let mut whole = 0usize;
        let mut paged = 1usize;
        while !pages(paged) {
            whole = paged;
            paged *= 2;
        }
        while whole + 1 < paged {
            let mid = whole + (paged - whole) / 2;
            if pages(mid) {
                paged = mid;
            } else {
                whole = mid;
            }
        }
        (whole, paged)
    }

    fn view<'a>(path: &'a str, kind: ViewerKind, ext: Option<&'a str>, text: &'a str) -> ViewerCard<'a> {
        ViewerCard {
            path,
            session_dir: None,
            size: text.len() as u64,
            kind,
            ext,
            text,
            content_identity: None,
            truncated: false,
            page: 1,
            return_to: None,
            lane: None,
            overflowed: false,
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
            content_identity: None,
            embedded_image_key: None,
            rich_message_id: None,
            refresh_failed: false,
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
        assert_eq!(
            cut,
            TableCut {
                rows_shown: 2,
                ..TableCut::default()
            },
            "nothing was cut"
        );

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
        assert!(
            cut.columns_dropped,
            "columns past the cap are dropped and reported"
        );

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
        assert!(cut.rows_dropped, "a table past the row cap says so");

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
        assert_eq!(
            cut,
            TableCut {
                rows_shown: 2,
                ..TableCut::default()
            }
        );

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
        assert!(text.contains("后续行未显示"), "the row cut is stated: {text}");
        assert!(text.contains("单元格已截断"), "the cell clip is stated: {text}");
    }

    #[test]
    fn delimited_budget_note_reports_the_rows_shown_and_budget_cut() {
        let header = (0..MAX_PREVIEW_COLUMNS)
            .map(|i| format!("column{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let row = (0..MAX_PREVIEW_COLUMNS)
            .map(|_| "x".repeat(MAX_CELL_CHARS - 10))
            .collect::<Vec<_>>()
            .join(",");
        let csv = format!(
            "{header}\n{}",
            std::iter::repeat_n(row, 100).collect::<Vec<_>>().join("\n")
        );
        let card = build_viewer_card(
            &key(),
            view("/work/large.csv", ViewerKind::Delimited, Some("csv"), &csv),
        );
        let text = card_text(&card);
        let note = text
            .lines()
            .find(|line| line.contains("预算不足"))
            .unwrap_or_else(|| panic!("the budget cut is stated: {text}"));

        assert!(note.contains("显示 "), "the note gives rows shown: {note}");
        assert!(
            note.contains("后续行未显示"),
            "the note names omitted rows: {note}"
        );
        assert!(!note.contains("仅显示前 49 行"), "no false fixed count: {note}");
    }

    #[test]
    fn delimited_columns_only_note_does_not_claim_rows_were_cut() {
        let header = (0..=MAX_PREVIEW_COLUMNS)
            .map(|i| format!("column{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let row = (0..=MAX_PREVIEW_COLUMNS)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let csv = format!("{header}\n{row}");
        let card = build_viewer_card(
            &key(),
            view("/work/wide.csv", ViewerKind::Delimited, Some("csv"), &csv),
        );
        let text = card_text(&card);
        let note = text
            .lines()
            .find(|line| line.contains("列已省略"))
            .unwrap_or_else(|| panic!("the dropped column is stated: {text}"));

        assert!(note.contains("显示 1 行"), "the row count is honest: {note}");
        assert!(
            note.contains(&format!("超出前 {MAX_PREVIEW_COLUMNS} 列")),
            "the column cap is stated: {note}"
        );
        assert!(!note.contains("后续行未显示"), "rows were not dropped: {note}");
        assert!(!note.contains("预算不足"), "the budget was not exhausted: {note}");
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
        assert!(text.contains("后续行未显示"), "the row cut is stated: {text}");

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

    /// The parser-level test above proves the laziness of `delimited_records`;
    /// this one proves the CARD BUILD actually relies on it. It asserts the
    /// bounded scan from `build_viewer_card` (via the thread-local the parser
    /// feeds), so a regression that collected every record before rendering —
    /// `delimited_table` calling `records.collect()` — fails here, even though
    /// the card's rendered output (a capped table) would look identical
    /// (finding #739 §1, round 2).
    #[test]
    fn the_card_build_parses_a_delimited_preview_lazily() {
        let rows = 400_000;
        let mut csv = String::with_capacity(rows * 4 + 8);
        csv.push_str("n,v\n");
        for _ in 0..rows {
            csv.push_str("a,1\n");
        }

        DELIMITED_SCANNED.with(|total| total.set(0));
        let card = build_viewer_card(
            &key(),
            view("/tmp/many.csv", ViewerKind::Delimited, Some("csv"), &csv),
        );
        let scanned = DELIMITED_SCANNED.with(|total| total.get());
        assert!(
            scanned > 0,
            "the card build did parse the preview (0 chars scanned)"
        );
        assert!(
            scanned < csv.len() / 4,
            "the card build must not scan the whole file: {scanned} of {} chars",
            csv.len()
        );
        // The rendered table is unchanged by the laziness: it still says it was
        // cut.
        let text = card_text(&card);
        assert!(text.contains("后续行未显示"), "the row cut is stated: {text}");
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
        let expected_parent = std::path::Path::new("/tmp/proj/src/main.rs")
            .parent()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(out["value"]["action"], "dir");
        assert_eq!(out["value"]["view"], "browse");
        assert_eq!(out["value"]["op"], "go");
        assert_eq!(out["value"]["dir"], expected_parent);
        assert_eq!(out["value"]["chat_id"], "chat_1");
        assert_eq!(out["value"]["thread_id"], "chat_1");
    }

    /// ADR-0083 §4 (amended #771): the scope is a privacy hint carried by the
    /// ADR and the user guide, not a line repeated on every viewer card. An
    /// outside-session path renders its resolved path like any other — the
    /// naming block is 文件 / 大小 / 类型 and nothing more.
    #[test]
    fn viewer_names_an_outside_session_path_without_a_scope_notice() {
        let mut viewer = view(
            "/tmp/outside/notes.md",
            ViewerKind::Text,
            Some("md"),
            "private notes",
        );
        viewer.session_dir = Some("/tmp/session");
        let card = build_viewer_card(&key(), viewer);

        let text = card_text(&card);
        assert!(
            text.contains("/tmp/outside/notes.md"),
            "the resolved path is still named: {text}"
        );
        assert!(text.contains("**大小**"), "the size is named: {text}");
        assert!(text.contains("文本（.md）"), "the type is named: {text}");
        assert!(
            !text.contains("⚠️"),
            "no warning row on an outside-session path: {text}"
        );
        assert!(
            !text.contains("会话目录之外"),
            "the card no longer repeats the scope: {text}"
        );
        assert!(
            !text.contains("不是访问边界"),
            "the card no longer implies a boundary: {text}"
        );
    }

    /// ADR-0052/0083: re-read is stateless and carries the resolved path, the
    /// current viewer page and the complete browse listing to return to.
    #[test]
    fn reread_payload_round_trips_viewer_and_return_listing_state() {
        let mut view = view(
            "/tmp/proj/src/main.rs",
            ViewerKind::Text,
            Some("rs"),
            "fn main() {}\n",
        );
        view.page = 2;
        view.return_to = Some(super::super::session::BrowseView {
            dir: "/tmp/proj",
            show_hidden: true,
            keyword: "main",
            page: 3,
        });
        let card = build_viewer_card(&key(), view);
        let reread = card_buttons(&card)
            .into_iter()
            .find(|button| button["text"]["content"] == "🔄 重新读取")
            .expect("every File Viewer offers a re-read")
            .clone();
        assert_eq!(reread["value"]["action"], "file");
        assert_eq!(reread["value"]["op"], "reread");
        assert_eq!(reread["value"]["path"], "/tmp/proj/src/main.rs");
        assert_eq!(reread["value"]["page"], 2);
        assert_eq!(reread["value"]["return_dir"], "/tmp/proj");
        assert!(reread["value"]["return_show_hidden"].as_bool().unwrap());
        assert_eq!(reread["value"]["return_keyword"], "main");
        assert_eq!(reread["value"]["return_page"], 3);
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
        assert!(
            !buttons.iter().any(|b| b["text"]["content"] == "📂 所在目录"),
            "a browse-opened viewer offers the return target instead of its parent"
        );

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
        assert!(
            card_buttons(&card)
                .iter()
                .any(|b| b["text"]["content"] == "📂 所在目录"),
            "a /cat viewer offers its parent directory"
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

    #[test]
    fn page_payload_recovers_a_completed_non_undelivered_lane() {
        let lane = lane_from_page_payload(&json!({
            "viewer_lane_mime": "image/png",
            "viewer_lane_name": "shot.png",
            "viewer_lane_size": "2048",
            "viewer_lane_delivery": "sent_as_file",
            "viewer_lane_image_key": "img_v2_shot",
            "viewer_lane_refresh_failed": "false",
        }))
        .expect("a completed whole-bytes lane survives a page flip");

        assert_eq!(lane.mime, "image/png");
        assert_eq!(lane.size, 2048);
        assert_eq!(lane.delivery, FileDelivery::SentAsFile);
        assert_eq!(lane.embedded_image_key.as_deref(), Some("img_v2_shot"));
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

    /// A viewer whose preview fits carries 「发为文件」 (ADR-0083 §5) — including
    /// a text preview, whose full bytes are the reason the button exists. Its
    /// payload names the RESOLVED path, routes to the Chat/Topic, and carries
    /// the browse listing's whole state so the PATCHed card can still return.
    #[test]
    fn fitting_viewers_carry_the_send_as_file_button() {
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

    /// #741, deliver-once: a preview that LANDED as its own message carries no
    /// 「发为文件」 — the content is already delivered, so the button would
    /// deliver the same file a second time. Rich Text carries only the rendered
    /// preview, so the original-byte transfer remains available.
    #[test]
    fn a_landed_rich_text_overflow_keeps_the_original_file_transfer() {
        let mut landed = view(
            "/tmp/proj/people.csv",
            ViewerKind::Delimited,
            Some("csv"),
            "name,age\nann,3\n",
        );
        landed.overflowed = true;
        landed.lane = Some(ViewerFileLane {
            mime: "text/csv".to_string(),
            name: "people.csv".to_string(),
            size: 15,
            delivery: FileDelivery::SentAsRichText,
            content_identity: None,
            embedded_image_key: None,
            rich_message_id: Some("om_rich".into()),
            refresh_failed: false,
        });
        let card = build_viewer_card(&key(), landed);
        let text = card_text(&card);
        assert!(
            text.contains("已发送为富文本消息"),
            "the tracking line says where the content went: {text}"
        );
        assert!(
            card_buttons(&card)
                .iter()
                .any(|b| b["text"]["content"] == "发为文件"),
            "Rich Text carries a preview, not the original bytes: {text}"
        );

        // The same lane, undelivered: the truncated preview, its cut note and
        // the rescue button stay, with the tracking line stating the content
        // did not land.
        let mut failed = view(
            "/tmp/proj/people.csv",
            ViewerKind::Delimited,
            Some("csv"),
            "name,age\nann,3\n",
        );
        failed.lane = Some(ViewerFileLane {
            mime: "text/csv".to_string(),
            name: "people.csv".to_string(),
            size: 15,
            delivery: FileDelivery::Undelivered,
            content_identity: None,
            embedded_image_key: None,
            rich_message_id: None,
            refresh_failed: false,
        });
        let card = build_viewer_card(&key(), failed);
        let text = card_text(&card);
        assert!(text.contains("| ann | 3 |"), "the preview stays: {text}");
        assert!(text.contains("未发送"), "the miss is stated: {text}");
        assert!(
            !text.contains("富文本"),
            "no claim to a lane that did not land: {text}"
        );
        assert!(
            card_buttons(&card)
                .iter()
                .any(|b| b["text"]["content"] == "发为文件"),
            "the rescue button stays when nothing landed: {text}"
        );
    }

    #[test]
    fn every_viewer_lane_keeps_file_transfer_except_an_already_sent_file_message() {
        let carries_transfer = |view| {
            card_buttons(&build_viewer_card(&key(), view))
                .iter()
                .any(|button| button["text"]["content"] == "发为文件")
        };

        let text = view("/tmp/notes.txt", ViewerKind::Text, Some("txt"), "notes");
        assert!(carries_transfer(text), "text previews need original bytes");

        let mut csv = view(
            "/tmp/people.csv",
            ViewerKind::Delimited,
            Some("csv"),
            "name,age\nann,3\n",
        );
        csv.overflowed = true;
        csv.lane = Some(ViewerFileLane {
            mime: "text/csv".into(),
            name: "people.csv".into(),
            size: 15,
            delivery: FileDelivery::SentAsRichText,
            content_identity: None,
            embedded_image_key: None,
            rich_message_id: Some("om_rich".into()),
            refresh_failed: false,
        });
        assert!(carries_transfer(csv), "Rich Text is not the original file");

        let image = with_lane(
            view("/tmp/shot.png", ViewerKind::Image, Some("png"), ""),
            "image/png",
            "shot.png",
            8,
            FileDelivery::Embedded {
                image_key: "img_shot".into(),
            },
        );
        assert!(carries_transfer(image), "the embedded image is still a preview");

        let binary = with_lane(
            view("/tmp/archive.bin", ViewerKind::Binary, Some("bin"), ""),
            "application/octet-stream",
            "archive.bin",
            8,
            FileDelivery::SentAsFile,
        );
        assert!(
            !carries_transfer(binary),
            "the File Message already carries the original bytes"
        );
    }

    /// A preview longer than the card can hold pages by lines through the
    /// shared pager instead of being cut (#740) — still within the card
    /// budget, still one closed fence per element, and the pager reports the
    /// preview's true line total (#741 owns the structural-overflow lane).
    #[test]
    fn a_long_preview_pages_within_the_card_budget() {
        let body = "let x = 1;\n".repeat(40_000);
        let card = build_viewer_card(&key(), view("/tmp/huge.rs", ViewerKind::Text, Some("rs"), &body));
        let json_len = serde_json::to_string(&card).unwrap().len();
        assert!(
            json_len < MAX_CARD_JSON_CHARS,
            "the card must fit its own budget: {json_len}"
        );
        let text = card_text(&card);
        assert!(text.contains("```rust"), "the window still renders a block");
        assert!(!text.contains("已截断"), "paging replaced the budget cut: {text}");
        assert!(
            text.contains("共 40000 个"),
            "the pager reports the preview's true line total: {text}"
        );
        assert!(
            card_buttons(&card)
                .iter()
                .any(|b| b["text"]["content"] == "下一页" && b["value"]["op"] == "page"),
            "the pager flips: {text}"
        );

        // Every fenced element is closed, so no element leaves a fence open.
        for element in card["body"]["elements"].as_array().unwrap() {
            if let Some(content) = element["content"].as_str()
                && content.starts_with("```")
            {
                assert_eq!(
                    content.matches("```").count(),
                    2,
                    "one window per element, each fence closed"
                );
            }
        }

        // An out-of-range page lands on the last page, never an empty card.
        let mut last = view("/tmp/huge.rs", ViewerKind::Text, Some("rs"), &body);
        last.page = usize::MAX;
        let card = build_viewer_card(&key(), last);
        let text = card_text(&card);
        assert!(
            text.contains("```rust") && text.contains("let x = 1;"),
            "the clamped last page still shows lines: {text}"
        );
    }

    /// A minified one-liner — a single line past a page's budget — is
    /// char-split across pages, and every page still fits the card: no window
    /// ever breaks the budget, however the bytes run together.
    #[test]
    fn an_overlong_line_pages_without_breaking_the_card_budget() {
        let line = "x".repeat(30_000);
        let mut first = view("/tmp/bundle.min.js", ViewerKind::Text, Some("js"), &line);
        first.page = 1;
        let card = build_viewer_card(&key(), first);
        let json_len = serde_json::to_string(&card).unwrap().len();
        assert!(
            json_len < MAX_CARD_JSON_CHARS,
            "the first window must fit its own budget: {json_len}"
        );
        let text = card_text(&card);
        assert!(
            !text.contains("已截断"),
            "even one line pages instead of being cut: {text}"
        );
        assert!(
            text.contains("共 1 个"),
            "the pager still reports the true line total: {text}"
        );

        let mut second = view("/tmp/bundle.min.js", ViewerKind::Text, Some("js"), &line);
        second.page = 2;
        let card = build_viewer_card(&key(), second);
        let json_len = serde_json::to_string(&card).unwrap().len();
        assert!(
            json_len < MAX_CARD_JSON_CHARS,
            "the second window must fit too: {json_len}"
        );
        assert!(
            card_text(&card).contains('x'),
            "the split line continues on the next page"
        );
    }

    /// #769: a preview that fits the card's JSON budget renders as ONE fenced
    /// block — never one element per raw chunk — so no line is cut at a
    /// character boundary and the fence appears exactly once. Both fixtures are
    /// ones the OLD per-chunk render split (two elements just past 4,000 chars,
    /// seven near 17.7 KB, measured on 6f45e61a), so a one-element card is a
    /// shape that render could not produce — reverting the arm fails this test.
    #[test]
    fn a_fitting_text_preview_renders_one_whole_block() {
        for lines in [160usize, 660] {
            let body: String = (0..lines)
                .map(|i| format!("line {i}: {}\n", "x".repeat(16)))
                .collect();
            assert!(
                body.len() >= 4_000 && chunk_text(&body, OLD_PREVIEW_CHUNK_CHARS).len() >= 2,
                "{lines} lines: a 4000-char file the old per-chunk render split, got {} chars",
                body.len()
            );
            let card = build_viewer_card(
                &key(),
                view("/tmp/notes.txt", ViewerKind::Text, Some("txt"), &body),
            );
            let json_len = serde_json::to_string(&card).unwrap().len();
            assert!(
                json_len < MAX_CARD_JSON_CHARS,
                "{lines} lines: the card fits its own budget: {json_len}"
            );
            let blocks = preview_blocks(&card);
            assert_eq!(
                blocks.len(),
                1,
                "{lines} lines: exactly one preview element: {blocks:#?}"
            );
            let block = blocks[0];
            assert_eq!(
                block.matches("```").count(),
                2,
                "{lines} lines: one fence, closed once: {block}"
            );
            assert!(
                block.contains(&body),
                "{lines} lines: the preview carries every line whole, no character cut"
            );
            assert!(!has_pager(&card), "{lines} lines: one block needs no pager");
            assert_eq!(
                detect_structural_overflow(ViewerKind::Text, Some("txt"), &body),
                None,
                "{lines} lines: a text preview never takes the Rich Text / File lane"
            );
        }
    }

    /// #769: past the card's preview budget the preview pages by LINES — one
    /// closed fenced block per page, the flip moving to the next window — and a
    /// whole line is never split to make a page fit. The paged path itself is
    /// #740's and unchanged by #769 (measured byte-identical on 6f45e61a), so
    /// this pins the pager's SHAPE; the assertions that discriminate the change
    /// are the one-block ones above.
    #[test]
    fn a_text_preview_past_the_budget_pages_one_block_per_page() {
        let body: String = (0..8_000)
            .map(|i| format!("line {i}: {}\n", "y".repeat(64)))
            .collect();
        let page_card = |page: usize| {
            let mut card = view("/tmp/big.txt", ViewerKind::Text, Some("txt"), &body);
            card.page = page;
            build_viewer_card(&key(), card)
        };
        let first = page_card(1);
        assert!(has_pager(&first), "the pager appears past the budget");
        assert_eq!(preview_blocks(&first).len(), 1, "one block on page 1");
        assert!(
            card_text(&first).contains("共 8000 个"),
            "the pager reports the preview's true line total"
        );
        assert!(
            card_buttons(&first).iter().any(|button| {
                button["text"]["content"] == "下一页"
                    && button["value"]["op"] == "page"
                    && button["value"]["page"] == 2
            }),
            "the flip targets the next page"
        );

        let second = page_card(2);
        assert_eq!(preview_blocks(&second).len(), 1, "one block on page 2");
        let (first_block, second_block) = (preview_blocks(&first)[0], preview_blocks(&second)[0]);
        assert_ne!(first_block, second_block, "the flip moves the window");
        // Page 2's pager flips both ways: back to 1, on to 3.
        let page2_buttons = card_buttons(&second);
        assert!(
            page2_buttons.iter().any(|button| {
                button["text"]["content"] == "上一页"
                    && button["value"]["page"] == 1
                    && button["disabled"] == false
            }),
            "page 2 can go back to page 1"
        );
        assert!(
            page2_buttons
                .iter()
                .any(|button| { button["text"]["content"] == "下一页" && button["value"]["page"] == 3 }),
            "page 2 can go on to page 3"
        );
        assert!(
            card_text(&second).contains("第 2/"),
            "the indicator names the page that is shown"
        );
        for block in [first_block, second_block] {
            assert_eq!(block.matches("```").count(), 2, "a closed fence: {block}");
            // The window cuts on line boundaries: everything it shows is a
            // whole line of the file.
            for line in block.lines().skip(1).take_while(|line| *line != "```") {
                assert!(
                    body.lines().any(|source| source == line),
                    "a whole source line: {line}"
                );
            }
        }
        for card in [&first, &second] {
            let json_len = serde_json::to_string(card).unwrap().len();
            assert!(
                json_len < MAX_CARD_JSON_CHARS,
                "every page fits the card budget: {json_len}"
            );
        }
    }

    /// #769: the two shapes meet at the card's preview budget. The last text
    /// whose single block fits the budget renders whole with no pager; one
    /// character more takes the pager — the boundary is asserted against the
    /// budget itself, never loosened to a vague "large enough".
    #[test]
    fn text_shapes_flip_exactly_at_the_preview_budget() {
        let (whole, paged) = whole_and_paged_boundary();
        // The same accounting the render charges: the fenced block's serialized
        // length plus the element wrapper — so the flip is pinned to the
        // documented budget, not to whatever the render happens to do.
        let lang = language_token(Some("txt"));
        let cost = |len: usize| serialized_len(&fenced_code(&"x".repeat(len), Some(lang))) + 128;
        assert!(
            cost(whole) <= PREVIEW_JSON_BUDGET,
            "the last whole preview fits the budget: {} > {PREVIEW_JSON_BUDGET}",
            cost(whole)
        );
        assert!(
            cost(paged) > PREVIEW_JSON_BUDGET,
            "one character more is past it: {} <= {PREVIEW_JSON_BUDGET}",
            cost(paged)
        );

        let card_of = |len: usize| {
            build_viewer_card(
                &key(),
                view("/tmp/notes.txt", ViewerKind::Text, Some("txt"), &"x".repeat(len)),
            )
        };
        let last = card_of(whole);
        assert_eq!(
            preview_blocks(&last).len(),
            1,
            "the last whole preview is one block"
        );
        assert!(!has_pager(&last), "…with no pager");
        let first = card_of(paged);
        assert_eq!(preview_blocks(&first).len(), 1, "each page is one block");
        assert!(has_pager(&first), "the pager starts exactly here");
        for len in [whole, paged] {
            let json_len = serde_json::to_string(&card_of(len)).unwrap().len();
            assert!(
                json_len < MAX_CARD_JSON_CHARS,
                "{len} chars fit the card budget: {json_len}"
            );
        }
    }

    /// #741: the structural-overflow lane is chosen from the RENDERED
    /// structure, never from byte size — past the 49-row table cap today, and
    /// never for text: a preview past the card's own budget pages by lines
    /// instead (#740/#769). (A single-file preview renders a single table, so
    /// the card's five-table cap is unreachable here by construction.)
    #[test]
    fn structural_overflow_is_detected_from_the_rendered_structure() {
        // A table past the row cap overflows, whatever its byte size — by
        // exactly one row past it.
        let long_csv = format!(
            "h1,h2\n{}",
            (0..MAX_TABLE_ROWS + 1)
                .map(|i| format!("r{i},v{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(
            detect_structural_overflow(ViewerKind::Delimited, Some("csv"), &long_csv),
            Some(StructuralOverflow::TableOverBudget {
                tables: 1,
                rows: MAX_TABLE_ROWS + 1,
            }),
            "one row past the 49-row cap takes the lane"
        );
        // Exactly at the cap fits.
        let at_cap = format!(
            "h1,h2\n{}",
            (0..MAX_TABLE_ROWS)
                .map(|i| format!("r{i},v{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            detect_structural_overflow(ViewerKind::Delimited, Some("csv"), &at_cap).is_none(),
            "a table within the row cap stays on the card"
        );
        // Text is paged regardless of its length; even a minified file stays in
        // the card lane.
        let many = "x".repeat(PREVIEW_JSON_BUDGET * 2);
        assert!(
            detect_structural_overflow(ViewerKind::Text, Some("txt"), &many).is_none(),
            "text past the card budget remains pageable"
        );
        let fitting = "y".repeat(PREVIEW_JSON_BUDGET - 1_000);
        assert!(
            detect_structural_overflow(ViewerKind::Text, Some("txt"), &fitting).is_none(),
            "a large block that fits the budget stays on the card"
        );
        // The byte lanes never take this lane.
        assert!(detect_structural_overflow(ViewerKind::Binary, Some("bin"), "x").is_none());
        assert!(detect_structural_overflow(ViewerKind::Image, Some("png"), "x").is_none());
        assert!(detect_structural_overflow(ViewerKind::TextTooLarge, Some("log"), "x").is_none());
    }

    /// #769: a text preview's length never triggers the structural-overflow
    /// lane — the old raw-chunk count used to suggest it might. The card's own
    /// budget decides between the two shapes (one block vs. the line pager),
    /// and the lane verdict is the same at, either side of, and far past it.
    #[test]
    fn text_size_never_triggers_structural_overflow() {
        let (whole, paged) = whole_and_paged_boundary();
        for len in [whole, paged, PREVIEW_JSON_BUDGET * 4] {
            assert_eq!(
                detect_structural_overflow(ViewerKind::Text, Some("txt"), &"x".repeat(len)),
                None,
                "{len} chars of text stays on the card"
            );
        }
    }

    /// #741: the Rich Text Message lane flips within one row of the documented
    /// 30 KB body cap — a delimited preview just under it carries the content
    /// intact, one row more takes no post body at all (the File lane owns it).
    #[test]
    fn the_rich_post_body_flips_within_one_row_of_the_cap() {
        // One wide column per row, so a single row costs ~1.4 KB and the flip
        // lands within one row of the cap rather than thousands of rows in.
        fn csv_rows(n: usize) -> String {
            let mut csv = String::from("h\n");
            for _ in 0..n {
                csv.push_str(&"x".repeat(1400));
                csv.push('\n');
            }
            csv
        }
        // Grow to the flip: the first row count whose body does not fit.
        let mut over = 0;
        while rich_post_md(ViewerKind::Delimited, Some("csv"), &csv_rows(over)).is_some() {
            over += 1;
        }
        assert!(over > 5, "the flip is a genuinely large table, got {over} rows");
        let under = over - 1;
        let md = rich_post_md(ViewerKind::Delimited, Some("csv"), &csv_rows(under))
            .expect("one row under the flip still fits");
        assert_eq!(
            md.lines().count(),
            under + 2,
            "the fitting body carries every row, none cut"
        );
        // The fitting body is JUST under the cap: its slack is less than one
        // row's own lane cost, so the flip is within one row of the budget.
        let posted_row = md.lines().nth(2).expect("first posted body row");
        assert!(
            posted_row.contains(&"x".repeat(1400)),
            "the posted row retains the full field"
        );
        let row_cost = serialized_len(posted_row) + 8;
        let slack = FEISHU_CARD_LIMIT_BYTES.saturating_sub(serialized_len(&md) + 128);
        assert!(
            slack < row_cost,
            "the fitting body is just under the cap ({slack} bytes of slack, one row costs {row_cost})"
        );
        assert!(
            rich_post_md(ViewerKind::Delimited, Some("csv"), &csv_rows(over)).is_none(),
            "one row more takes no post body"
        );
    }

    /// #741: the Rich Text Message carries the content intact — the full
    /// table, every row — while a body that would exceed the documented
    /// 30 KB cap takes no post body at all (it goes to the File lane).
    #[test]
    fn the_rich_post_body_carries_the_content_intact_within_the_cap() {
        let csv = format!(
            "name,age\n{}",
            (0..60)
                .map(|i| format!("r{i},{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let md = rich_post_md(ViewerKind::Delimited, Some("csv"), &csv)
            .expect("a 60-row small-cell table fits the post cap");
        assert!(md.contains("| name | age |"), "the header: {md}");
        assert!(md.contains("| r59 | 59 |"), "every row, none cut: {md}");

        // The rich lane has none of the preview's 120-character/20-column
        // limits. A long field and wide row in a structurally overflowing
        // table both survive intact while the whole post fits its real cap.
        let long = "x".repeat(1_400);
        let header = (0..25).map(|i| format!("c{i}")).collect::<Vec<_>>().join(",");
        let rows = (0..60)
            .map(|row| {
                let mut cells = (0..25).map(|col| format!("r{row}c{col}")).collect::<Vec<_>>();
                if row == 0 {
                    cells[0] = long.clone();
                }
                cells.join(",")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let wide = format!("{header}\n{rows}");
        let md = rich_post_md(ViewerKind::Delimited, Some("csv"), &wide)
            .expect("the full 25-column table fits the post cap");
        assert!(md.contains(&long), "the full long field is posted");
        assert!(md.contains("r59c24"), "the last column and row are posted");
        assert_eq!(md.lines().count(), 62, "header, separator, and all rows");
        // A body past the cap is no post body: the File lane owns it. Cells
        // are parser-bounded, so the overflow comes from ROWS — thousands of
        // them — and the build aborts past the cap instead of materializing
        // the whole table.
        let many_rows = format!(
            "name,age\n{}",
            (0..4_000)
                .map(|i| format!("r{i},{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            rich_post_md(ViewerKind::Delimited, Some("csv"), &many_rows).is_none(),
            "a post body past 30 KB does not take this lane"
        );
        // A text preview past the card budget never fits a post body either:
        // its bytes dwarf the same cap, so it goes to the File lane.
        let many = "x".repeat(PREVIEW_JSON_BUDGET * 2);
        assert!(rich_post_md(ViewerKind::Text, Some("txt"), &many).is_none());
        // ...while a fitting text preview has an intact post body by
        // construction (unused on open — it stays on the card).
        let md = rich_post_md(ViewerKind::Text, Some("rs"), "fn main() {}\n").expect("fits");
        assert!(md.contains("```rust") && md.contains("fn main() {}"), "{md}");
    }

    /// #741: the post content is one `md` message — the shape the generic
    /// reply path sends as `msg_type: "post"`.
    #[test]
    fn the_post_content_is_one_md_message() {
        let content = rich_post_content("hello");
        assert_eq!(content["zh_cn"]["content"][0][0]["tag"], "md");
        assert_eq!(content["zh_cn"]["content"][0][0]["text"], "hello");
    }
}
