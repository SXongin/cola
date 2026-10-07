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

/// The markdown construction a delivered prefix leaves open at its cut point
/// (spec #561, ticket #563): what a tail that starts mid-construct must
/// re-establish to render as the content it continues.
enum OpenConstruct<'a> {
    /// A fenced code block opened with this many backticks and not closed.
    Fence { ticks: usize },
    /// A markdown table whose header (and, when the cut is past it, delimiter)
    /// the tail continues.
    Table {
        header: &'a str,
        delimiter: Option<&'a str>,
    },
}

/// The construction `prefix` leaves open at its end, if any. A fence is
/// tracked line by line exactly as [`CardMarkdown::clean`] tracks it; a table
/// is recognised by its header/delimiter block ending on the prefix's last
/// non-blank line.
fn open_construct(prefix: &str) -> Option<OpenConstruct<'_>> {
    let lines: Vec<&str> = prefix.split('\n').collect();
    let mut fence: Option<usize> = None;
    for line in &lines {
        match fence {
            Some(open) => {
                if closes_fence(line, open) {
                    fence = None;
                }
            }
            None => {
                if let Some(open) = opens_fence(line) {
                    fence = Some(open);
                }
            }
        }
    }
    if let Some(ticks) = fence {
        return Some(OpenConstruct::Fence { ticks });
    }
    // A table is "open" when its last non-blank line is a row of one: walk
    // back to the TABLE's own start — a line belonging to no table (no `|`)
    // ends the block, so a paragraph sitting directly above the header is
    // never crossed — and require the header/delimiter shape (a header alone
    // counts — the cut landed between it and the delimiter).
    let last = lines.iter().rposition(|line| !line.trim().is_empty())?;
    if !lines[last].contains('|') {
        return None;
    }
    let mut start = last;
    while start > 0 && lines[start - 1].trim().contains('|') {
        start -= 1;
    }
    let delimiter = if last > start {
        let candidate = lines[start + 1];
        if !is_delimiter_row(candidate) {
            return None;
        }
        Some(candidate)
    } else {
        None
    };
    Some(OpenConstruct::Table {
        header: lines[start],
        delimiter,
    })
}

/// The markdown lead a projection's cut tail needs before its own text so the
/// cut renders intact (spec #561, ticket #563): a fence opener matching the
/// one the delivered prefix left unclosed, or the header/delimiter rows a
/// table's remaining body rows need to parse as the table they continue.
/// `None` when the prefix ends in a neutral state — or when the tail no
/// longer carries the construct's content (a plain line after a table ends
/// it), where a lead would only invent markdown.
pub(crate) fn neutralize_tail(prefix: &str, suffix: &str) -> Option<String> {
    match open_construct(prefix)? {
        OpenConstruct::Fence { ticks } => Some(format!("{}\n", "`".repeat(ticks))),
        OpenConstruct::Table { header, delimiter } => {
            if !suffix.lines().next().is_some_and(|line| line.contains('|')) {
                return None;
            }
            let mut lead = String::from(header);
            lead.push('\n');
            if let Some(delimiter) = delimiter {
                lead.push_str(delimiter);
                lead.push('\n');
            }
            Some(lead)
        }
    }
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

    /// A tail cut inside a fenced code block (spec #561, ticket #563): the
    /// lead reopens the fence, so the tail's code stays code and the text
    /// after the original closer is not swallowed by an unclosed fence.
    #[test]
    fn a_tail_cut_inside_a_fence_reopens_it() {
        // The opener's own run length is the lead's: a shorter opener could be
        // closed by a run the code legitimately contains.
        assert_eq!(
            neutralize_tail("说明\n```python\nprint(1)\n", "print(2)\n```\n后的文字").as_deref(),
            Some("```\n")
        );
        assert_eq!(
            neutralize_tail("````\ncode\n", "more\n````\ntail").as_deref(),
            Some("````\n")
        );
        // A closed fence (or no fence at all) leaves the tail standing alone.
        assert_eq!(neutralize_tail("```\ncode\n```\n", "尾"), None);
        assert_eq!(neutralize_tail("普通文字\n", "更多文字"), None);
        assert_eq!(neutralize_tail("", "从零开始"), None);

        // The neutralized tail renders as code followed by plain text; the
        // naive cut would leave the fence open and swallow the after-text.
        let lead = neutralize_tail("说明\n```python\nprint(1)\n", "print(2)\n```\n后的文字").unwrap();
        assert_eq!(
            CardMarkdown::new().clean(&format!("{lead}print(2)\n```\n后的文字")),
            "```\nprint(2)\n```\n后的文字"
        );
        assert_eq!(
            CardMarkdown::new().clean("print(2)\n```\n后的文字"),
            "print(2)\n```\n后的文字",
            "without the lead the fence stays open — the mangling the lead prevents"
        );
    }

    /// A tail cut inside a markdown table (spec #561, ticket #563): the lead
    /// repeats the table's header and delimiter, so the remaining body rows
    /// parse as the table they continue instead of stray pipe text.
    #[test]
    fn a_tail_cut_inside_a_table_repeats_the_header() {
        let prefix = "| 名称 | 值 |\n|---|---|\n| 一 | 1 |\n";
        assert_eq!(
            neutralize_tail(prefix, "| 二 | 2 |\n\n尾注").as_deref(),
            Some("| 名称 | 值 |\n|---|---|\n")
        );
        // Cut between the header and the delimiter: the header alone is the
        // lead (the suffix carries the delimiter).
        assert_eq!(
            neutralize_tail("| 名称 | 值 |\n", "|---|---|\n| 一 | 1 |").as_deref(),
            Some("| 名称 | 值 |\n")
        );
        // A table that ended before the cut (blank line) needs no lead, and a
        // suffix that no longer carries a row is plain text.
        assert_eq!(
            neutralize_tail("| a | b |\n|---|---|\n| 1 | 2 |\n\n", "尾注"),
            None
        );
        assert_eq!(neutralize_tail(prefix, "尾注"), None);
        // A closed table's remaining rows render as the same table.
        let lead = neutralize_tail(prefix, "| 二 | 2 |\n").unwrap();
        assert_eq!(
            CardMarkdown::new().clean(&format!("{lead}| 二 | 2 |")),
            "| 名称 | 值 |\n|---|---|\n| 二 | 2 |"
        );
    }

    /// A table directly under a paragraph (no blank line between them) is
    /// still a table at its own start (spec #561, review #569): the lead's
    /// backward scan must not cross the paragraph, or a cut inside the table
    /// gets no lead and the remaining rows render as literal pipe text.
    #[test]
    fn a_table_directly_after_a_paragraph_repeats_its_header() {
        let prefix = "前置段落文字。\n| 名称 | 值 |\n|---|---|\n| 一 | 1 |\n";
        let lead = neutralize_tail(prefix, "| 二 | 2 |\n").unwrap();
        assert_eq!(lead, "| 名称 | 值 |\n|---|---|\n");
        assert_eq!(
            CardMarkdown::new().clean(&format!("{lead}| 二 | 2 |")),
            "| 名称 | 值 |\n|---|---|\n| 二 | 2 |"
        );
        // Without the lead the leftover row renders as literal pipe text.
        assert_eq!(
            CardMarkdown::new().clean("| 二 | 2 |"),
            "| 二 | 2 |",
            "the mangling the lead prevents"
        );
    }
}
