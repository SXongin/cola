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
//! rejects, leaving code untouched — fenced blocks and inline spans alike:
//!
//! - `<` outside a fenced code block and outside an inline code span becomes
//!   Feishu's documented escape `&#60;`, so a tag opener can never start a tag
//!   (in normal text the entity decodes back to `<` — visually identical). A
//!   code span's content passes through verbatim: the platform renders code
//!   literally and does not decode entities there, so an escape injected
//!   inside backticks is displayed as the escape itself (#519, observed
//!   2026-10-04 on a live card: `` `<read_file>` `` reached the reader as
//!   `&#60;read_file>`).
//!   The 2026-09-22 reason for escaping inside backticks (a tag still reached
//!   the parser once an earlier unclosed tag fragment changed the context) no
//!   longer applies: every `<` outside a well-formed code context stays
//!   escaped, and a span that does break the parser falls through to the
//!   fenced retry like any other rejection.
//! - markdown images outside code become plain links (an image key must be
//!   uploaded by the app; the model cannot hold one, so `![…](…)` is always an
//!   invalid key),
//! - markdown tables beyond the per-card budget or over the row cap are
//!   wrapped in a code fence, where Feishu counts no tables and no rows,
//! - the numeric entities that neutralize markdown emphasis inside a span that
//!   must stay intact (the **Background Task Ledger**'s bold label): `&` becomes
//!   [`AMPERSAND_ESCAPE`] FIRST, then `*`/`_` become [`ASTERISK_ESCAPE`] /
//!   [`UNDERSCORE_ESCAPE`], so a label's own entity text is not decoded into a
//!   new construct and a command cannot close its `**…**` wrap.
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

/// The numeric entity for a literal `*` in card markdown: Feishu decodes it
/// back to the character, so text can sit inside an emphasis span without
/// opening or closing one.
pub(crate) const ASTERISK_ESCAPE: &str = "&#42;";

/// The numeric entity for a literal `_` in card markdown — the emphasis
/// character's partner of [`ASTERISK_ESCAPE`].
pub(crate) const UNDERSCORE_ESCAPE: &str = "&#95;";

/// The numeric entity for a literal `&` in card markdown. Applied BEFORE
/// [`ASTERISK_ESCAPE`] / [`UNDERSCORE_ESCAPE`], so a piece of text carrying
/// entity syntax of its own (`&#42;`, `&amp;`) renders those characters
/// literally instead of being decoded into a construct.
pub(crate) const AMPERSAND_ESCAPE: &str = "&amp;";

/// A transcript-authored string's own `&` / `*` / `_` swapped for their
/// numeric entities, `&` first (see [`AMPERSAND_ESCAPE`]): the text renders
/// unchanged while it cannot seed a construct, close a `**…**` span or bleed
/// formatting into a neighbouring element. Shared by the ledger's bold label
/// and its activity fragment's tool name, and by the size reserve that must
/// cover both — so the escaping's cost has one owner.
pub(crate) fn escaped_entities(text: &str) -> String {
    text.replace('&', AMPERSAND_ESCAPE)
        .replace('*', ASTERISK_ESCAPE)
        .replace('_', UNDERSCORE_ESCAPE)
}

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

/// Escape `<` and downgrade images in one line, leaving inline code spans
/// verbatim. The caller guarantees the line is outside a fenced code block.
///
/// A code span's content is literal text in CommonMark — the renderer does not
/// decode entities there, so an injected `&#60;` would be shown as the escape
/// itself (#519). The span is therefore passed through exactly as authored,
/// images included.
fn escape_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        let c = line[i..].chars().next().expect("index on a char boundary");
        if c == '`' {
            if escaped_by_backslash(line, i) {
                // The escape consumes `\` + this single backtick (the
                // backslash itself was already emitted as text); any remaining
                // backticks still form a delimiter candidate.
                out.push(c);
                i += 1;
                continue;
            }
            match code_span(line, i) {
                Some((end, span)) => {
                    out.push_str(span);
                    i = end;
                }
                // A backtick string that finds no match is literal text as a
                // whole: advance past the ENTIRE run, never retry from its
                // suffix, or the suffix could pair with a later run and
                // smuggle the run's raw `<` through as span content.
                None => {
                    let run = line[i..].chars().take_while(|c| *c == '`').count();
                    out.push_str(&line[i..i + run]);
                    i += run;
                }
            }
            continue;
        }
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

/// Whether the character at `at` is escaped by an immediately preceding run
/// of an odd number of backslashes. The reference CommonMark parser consumes
/// `\` + the escaped character before backtick handling, so an escaped
/// backtick is never a delimiter (an even run leaves it one). Backslashes
/// INSIDE a span are literal and do not shield its closer — that is
/// [`code_span`]'s business, not this check's.
fn escaped_by_backslash(line: &str, at: usize) -> bool {
    line[..at].chars().rev().take_while(|c| *c == '\\').count() % 2 == 1
}

/// The inline code span starting at `start` (a backtick), if it closes on this
/// line: the byte offset just past the closing run and the span text
/// (delimiters included), verbatim.
///
/// CommonMark pairing: a maximal run of `n` backticks closes at the next run
/// of exactly `n` backticks; runs of other lengths inside the span are
/// content, and a backslash before the closer does not shield it. `None` means
/// the run is literal text — an unclosed opener, or a run that can only close
/// a span opened elsewhere. The caller consumes a failed run whole, and a span
/// crossing a line break is never trusted (both runs stay unmatched per line),
/// so unsupported shapes are escaped conservatively instead of trusted.
fn code_span(line: &str, start: usize) -> Option<(usize, &str)> {
    let open = line[start..].chars().take_while(|c| *c == '`').count();
    let mut i = start + open;
    while i < line.len() {
        if line.as_bytes()[i] == b'`' {
            let run = line[i..].chars().take_while(|c| *c == '`').count();
            if run == open {
                let end = i + run;
                return Some((end, &line[start..end]));
            }
            i += run;
            continue;
        }
        let c = line[i..].chars().next().expect("index on a char boundary");
        i += c.len_utf8();
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed inline code span is literal text: `<` keeps its authored shape
    /// and images are not downgraded. Observed 2026-10-04 (#519) on a live
    /// card: the platform does not decode entities inside code spans, so the
    /// escape there was displayed as the escape itself.
    #[test]
    fn inline_code_spans_pass_through_verbatim() {
        for line in [
            "a `<read_file>` b",
            "`<at id=x></at>`",
            "看 `<number_tag>` 和 `<link>`",
            "`![x](./x.png)`",
            "`&#60;`",
            "x `<a>` y `<b>` z",
        ] {
            assert_eq!(escape_line(line), line, "{line:?}");
        }
    }

    /// Outside spans the existing rewrites still apply: `<` is escaped and an
    /// image becomes a link.
    #[test]
    fn text_outside_spans_is_still_rewritten() {
        assert_eq!(
            escape_line("a <number_tag> b ![x](./x.png)"),
            "a &#60;number_tag> b [x](./x.png)"
        );
        assert_eq!(escape_line("`<a>` <b>"), "`<a>` &#60;b>");
    }

    /// CommonMark pairing: a run of `n` backticks closes at the next run of
    /// exactly `n`; runs of other lengths are span content.
    #[test]
    fn code_span_pairing_matches_run_lengths() {
        assert_eq!(escape_line("`` a ` <x> ``"), "`` a ` <x> ``");
        assert_eq!(escape_line("`a `` <x> `"), "`a `` <x> `");
        assert_eq!(escape_line("``a `` <x>``"), "``a `` &#60;x>``");
    }

    /// Unpaired runs are literal text, not a span: what follows is escaped as
    /// usual (conservative — an opener needs its closer to be trusted).
    #[test]
    fn unpaired_backticks_leave_the_text_escaped() {
        assert_eq!(escape_line("`open <number_tag> b"), "`open &#60;number_tag> b");
        assert_eq!(escape_line("``"), "``");
    }

    /// A failed backtick string is literal as a WHOLE: its suffix must never
    /// be retried as a delimiter, or it could pair with a later run and pass
    /// raw `<` through as span content (the maximal run is the delimiter,
    /// CommonMark 0.31 §6.2).
    #[test]
    fn a_failed_run_is_never_split_into_a_delimiter() {
        assert_eq!(escape_line("``a <number_tag> `"), "``a &#60;number_tag> `");
    }

    /// Backslash escapes are consumed before backtick pairing: an odd run
    /// escapes the backtick, an even run leaves it a delimiter. A backslash
    /// does NOT shield a closer inside a span.
    #[test]
    fn backslash_escapes_follow_commonmark() {
        assert_eq!(escape_line(r"\`<x>\`"), r"\`&#60;x>\`");
        assert_eq!(escape_line(r"\\`<x>`"), r"\\`<x>`");
        assert_eq!(escape_line(r"`a\` <x> `"), r"`a\` &#60;x> `");
    }

    /// A span crossing a line break is never trusted: both runs stay unmatched
    /// per line and the content is escaped conservatively.
    #[test]
    fn spans_crossing_lines_are_escaped_conservatively() {
        assert_eq!(
            CardMarkdown::new().clean("`open <number_tag>\nmore` <link>"),
            "`open &#60;number_tag>\nmore` &#60;link>"
        );
    }

    /// The whole-blob entry point (`clean`): a span survives while a later
    /// plain line is still sanitized.
    #[test]
    fn clean_keeps_spans_verbatim_and_escapes_plain_text() {
        assert_eq!(
            CardMarkdown::new().clean("看 `<read_file>` 变体\n\n<number_tag>"),
            "看 `<read_file>` 变体\n\n&#60;number_tag>"
        );
    }
}
