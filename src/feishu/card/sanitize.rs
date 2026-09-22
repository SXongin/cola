//! Feishu card markdown hygiene.
//!
//! A card's rich-text element is not rendered as plain CommonMark: the
//! platform compiles it with its own parser, which recognizes Feishu tag
//! syntax (`<number_tag>`, `<link>`, `<at>`, …) and counts markdown tables per
//! card. Model-authored text can accidentally contain those constructs, and a
//! rejected card update fails forever — every flush re-sends the same JSON —
//! leaving the turn's card frozen on its last accepted state.
//!
//! [`CardMarkdown`] neutralizes the constructs cola knows the platform
//! rejects, leaving fenced code untouched:
//!
//! - `<` outside a fenced code block becomes Feishu's documented escape
//!   `&#60;`, so a tag opener can never start a tag (visually identical to a
//!   literal `<`). Inline code is NOT exempt: measured 2026-09-22, a tag
//!   inside backticks still reached the parser once an earlier unclosed tag
//!   fragment changed the context, so only fences are trusted.
//! - markdown images become plain links (an image key must be uploaded by the
//!   app; the model cannot hold one, so `![…](…)` is always an invalid key),
//! - markdown tables beyond the per-card budget or over the row cap are
//!   wrapped in a code fence, where Feishu counts no tables and no rows.
//!
//! One instance is one card build: the table budget is per card, shared by
//! every element and panel the build emits.

use super::fenced_code;

/// Markdown tables Feishu accepts on one card, counted across the whole card
/// (elements and panels alike): a 6th table fails with `230099/11310 card
/// table number over limit` (measured 2026-09-22).
pub(crate) const MAX_CARD_TABLES: usize = 5;

/// Body rows one markdown table may carry (header and separator rows are not
/// counted): 50 body rows fail with `230099/11310 element exceeds the limit`;
/// 49 render (measured 2026-09-22).
pub(crate) const MAX_TABLE_ROWS: usize = 49;

/// Feishu's documented escape for a literal `<` in card markdown.
const LESS_THAN_ESCAPE: &str = "&#60;";

/// Sanitize a single markdown element for its own card — for one-shot cards
/// whose text never shares a table budget with other elements (notifications,
/// ack cards). A card that builds several model texts should thread one
/// [`CardMarkdown`] instead.
pub(crate) fn sanitize_markdown(text: &str) -> String {
    CardMarkdown::new().clean(text)
}

/// Per-card markdown hygiene state.
pub(crate) struct CardMarkdown {
    /// Tables this card may still render natively; later ones become code.
    tables_left: usize,
    /// The content-rejection fallback: every element is fenced as code, the
    /// one form the platform's card parser accepts unconditionally.
    fenced: bool,
}

impl CardMarkdown {
    pub(crate) fn new() -> Self {
        Self {
            tables_left: MAX_CARD_TABLES,
            fenced: false,
        }
    }

    /// [`Self::new`] for a card whose content Feishu already rejected once:
    /// the caller fences every element instead of cleaning it.
    pub(crate) fn fenced() -> Self {
        Self {
            tables_left: 0,
            fenced: true,
        }
    }

    /// Whether the whole-card fallback is active.
    pub(crate) fn is_fenced(&self) -> bool {
        self.fenced
    }

    /// The content of one markdown element under this card's policy: cleaned
    /// normally, or fenced verbatim when the fallback is active. Callers that
    /// chunk one blob across several elements (text) chunk first and fence
    /// per chunk, so no element holds an unclosed fence.
    pub(crate) fn element(&mut self, text: &str) -> String {
        if self.fenced {
            fenced_code(text, None)
        } else {
            self.clean(text)
        }
    }

    /// Sanitize one markdown blob for the card, consuming table budget.
    pub(crate) fn clean(&mut self, text: &str) -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut out: Vec<String> = Vec::with_capacity(lines.len());
        let mut i = 0;
        // `Some(n)` while inside a fenced code block opened by `n` backticks:
        // its lines pass through verbatim (Feishu renders them literally, so
        // escaping inside would show the escape itself).
        let mut fence: Option<usize> = None;
        while i < lines.len() {
            let line = lines[i];
            if let Some(open) = fence {
                out.push(line.to_string());
                if closes_fence(line, open) {
                    fence = None;
                }
                i += 1;
                continue;
            }
            if let Some(open) = opens_fence(line) {
                fence = Some(open);
                out.push(line.to_string());
                i += 1;
                continue;
            }
            if let Some(end) = table_end(&lines, i) {
                let region = lines[i..end].join("\n");
                if self.tables_left == 0 || end - i - 2 > MAX_TABLE_ROWS {
                    out.push(fenced_code(&region, None));
                } else {
                    self.tables_left -= 1;
                    out.extend(lines[i..end].iter().map(|l| escape_line(l)));
                }
                i = end;
                continue;
            }
            out.push(escape_line(line));
            i += 1;
        }
        out.join("\n")
    }
}

/// The end (exclusive) of the markdown table starting at `start`, if any: a
/// row containing a pipe, a delimiter row, then the body rows (non-blank
/// lines that still carry a pipe). Mirrors what the platform counts as one
/// table closely enough to budget defensively.
fn table_end(lines: &[&str], start: usize) -> Option<usize> {
    let header = lines.get(start)?;
    if !header.contains('|') {
        return None;
    }
    if !is_delimiter_row(lines.get(start + 1)?) {
        return None;
    }
    let mut end = start + 2;
    while let Some(line) = lines.get(end) {
        if line.trim().is_empty() || !line.contains('|') {
            break;
        }
        end += 1;
    }
    Some(end)
}

/// A GFM delimiter row: only `-`, `:`, `|` and spaces, with at least one `-`.
fn is_delimiter_row(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed.contains('|')
        && trimmed.contains('-')
        && trimmed.chars().all(|c| matches!(c, '-' | ':' | '|' | ' ' | '\t'))
}

/// A line that opens a fenced code block: up to three leading spaces, a run
/// of three or more backticks, and no backtick in the info string. Returns
/// the run length (a closing fence must be at least as long).
fn opens_fence(line: &str) -> Option<usize> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let run = rest.chars().take_while(|c| *c == '`').count();
    if run < 3 || rest[run..].contains('`') {
        return None;
    }
    Some(run)
}

/// A line that closes a fence opened with `open` backticks: only backticks
/// (at least `open`) and trailing whitespace.
fn closes_fence(line: &str, open: usize) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let run = trimmed.chars().take_while(|c| *c == '`').count();
    run >= open && trimmed[run..].trim().is_empty()
}

/// Escape `<` and downgrade images in one line. The caller guarantees the
/// line is outside a fenced code block.
fn escape_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        let c = line[i..].chars().next().expect("index on a char boundary");
        if c == '<' {
            out.push_str(LESS_THAN_ESCAPE);
        } else if c == '!' && line[i..].starts_with("![") {
            match image_span(line, i) {
                Some((end, link)) => {
                    out.push_str(&link);
                    i = end;
                    continue;
                }
                None => out.push(c),
            }
        } else {
            out.push(c);
        }
        i += c.len_utf8();
    }
    out
}

/// Rewrite the markdown image starting at `start` (the `!`) into a plain
/// link: `![alt](dest)` → `[alt](dest)`. Returns the byte offset just past
/// the image and the rewritten text, or `None` when the span is not a
/// well-formed image (leave it alone — it is text, not an image).
fn image_span(line: &str, start: usize) -> Option<(usize, String)> {
    let alt_end = line[start + 2..].find(']').map(|at| start + 2 + at)?;
    let dest_start = alt_end + 1;
    if !line[dest_start..].starts_with('(') {
        return None;
    }
    let alt = line[start + 2..alt_end].replace('<', LESS_THAN_ESCAPE);
    let mut depth = 0usize;
    let mut escaped = false;
    for (offset, c) in line[dest_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let end = dest_start + offset + 1;
                    return Some((
                        end,
                        format!("[{alt}]({})", &line[dest_start + 1..dest_start + offset]),
                    ));
                }
            }
            _ => {}
        }
    }
    None
}
