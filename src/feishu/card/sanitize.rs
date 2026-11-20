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

use super::{MAX_ELEMENT_TEXT_CHARS, fenced_code};

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

/// The JSON-string bytes the card will carry for `text`, EXACTLY: the
/// sanitizer's content expansion (`<` becomes the five-byte `&#60;`, what
/// [`CardMarkdown::clean`] does outside a code context) applied first, then the
/// text encoded as a JSON string value by [`serde_json`] — so control
/// characters (`\u0000` costs six), quotes, backslashes and non-ASCII are
/// measured by the encoder that will write them, with no per-character table to
/// keep in sync.
///
/// A pure function of the text: the state machine deciding *where* an escape
/// applies stays in [`CardMarkdown::clean`], so a `<` inside a fenced block or an
/// inline code span is expanded here and not on the card — an over-charge, the
/// safe side for a per-row cap or a splitter, which must never under-charge what
/// Feishu receives. Two sanitizer expansions are NOT string-content
/// substitutions and stay outside this measure, both bounded by the card text
/// they wrap: a table that outgrows its budget gains a code fence around it, an
/// image span collapses to a shorter link, and the rejection fallback's fence
/// grows with the chunk it wraps ([`fenced_code`] uses one backtick more than the
/// chunk's longest run) — each is absorbed by the per-element wrapper constant
/// the callers add on top, and the element's own character cap bounds it.
pub(crate) fn serialized_len(text: &str) -> usize {
    let expanded = if text.contains('<') {
        text.replace('<', LESS_THAN_ESCAPE)
    } else {
        text.to_string()
    };
    serde_json::to_string(&expanded)
        .map(|quoted| quoted.len().saturating_sub(2))
        .unwrap_or_else(|_| expanded.len())
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
    // A table is "open" when its last non-blank line is a row of one. The
    // run of pipe lines is located, then the DELIMITER row inside it names the
    // header (the line above it) — a prose line that merely contains a pipe
    // directly above the table must not be taken for its header. A run with no
    // delimiter is a cut between the header and it, where the last pipe line is
    // the only line that can be the header. A blank line after the last row
    // CLOSES the table: the tail then starts a construct of its own (a separate
    // table must not inherit this one's header, #1096).
    let body = prefix.strip_suffix('\n').unwrap_or(prefix);
    if body.contains('\n')
        && body
            .rsplit('\n')
            .next()
            .is_some_and(|line| line.trim().is_empty())
    {
        return None;
    }
    let last = lines.iter().rposition(|line| !line.trim().is_empty())?;
    if !lines[last].contains('|') {
        return None;
    }
    let header = table_header_index(&lines, last)?;
    let delimiter = lines
        .get(header + 1)
        .filter(|line| is_delimiter_row(line))
        .copied();
    Some(OpenConstruct::Table {
        header: lines[header],
        delimiter,
    })
}

/// The line index of the table header whose last row is `last`, when the run
/// of pipe lines ending there has a header shape: a delimiter row in the run
/// names the header as the line above it (so a prose line containing a pipe
/// just above the table is not mistaken for it); with no delimiter the cut sits
/// between the header and it, and the last pipe line is the header.
fn table_header_index(lines: &[&str], last: usize) -> Option<usize> {
    let mut start = last;
    while start > 0 && lines[start - 1].trim().contains('|') {
        start -= 1;
    }
    match (start..=last).find(|&index| is_delimiter_row(lines[index])) {
        // The delimiter is the table's second line, so the header is above it;
        // a run that begins with a delimiter has no header in this prefix.
        Some(delimiter) if delimiter > start => Some(delimiter - 1),
        Some(_) => None,
        None => Some(last),
    }
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

/// Split `text` into chunks of at most `max` characters for separate card
/// elements (issue #1096), each **self-contained** as markdown: a chunk that
/// starts inside a construct the previous chunk left open re-establishes it
/// with the lead [`neutralize_tail`] gives a cut tail, and a chunk that ends
/// inside a fenced block closes it. The platform parses every element on its
/// own, so a bare continuation would render the code it carries as prose and
/// turn the original closer into a new opener, swallowing what follows.
///
/// A table that fits is moved whole to the next element instead of being cut —
/// a repeated header is a second table, and the platform caps tables per card —
/// so the table lead only appears for a table already started in an earlier
/// chunk (a caller that cleans per chunk, or one too long for the budget).
///
/// Cuts prefer line boundaries (so an opener or closer is never split
/// mid-token); a line longer than the budget falls back to a character cut.
/// The added lines are markdown, not content: callers that account for the
/// text's own characters must use the timeline's `lead` mechanism instead.
pub(crate) fn chunk_markdown(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut chunks = Vec::new();
    // The ORIGINAL text delivered so far (never the added markers): the
    // prefix whose open construct the next chunk must re-establish.
    let mut consumed = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let lead = neutralize_tail(&consumed, rest).unwrap_or_default();
        let mut budget = max.saturating_sub(lead.chars().count()).max(1);
        let mut raw = take_line_bounded(rest, budget);
        // Keep a native table whole on one element: a cut inside one repeats
        // its header as a SECOND table, and the platform caps tables per card.
        // When the table started inside this take, end the chunk before it and
        // let the next take carry it whole (a table too long for one element
        // was already fenced by `clean`).
        if !matches!(open_construct(&consumed), Some(OpenConstruct::Table { .. }))
            && matches!(
                open_construct(&format!("{consumed}{raw}")),
                Some(OpenConstruct::Table { .. })
            )
            && let Some(offset) = open_table_start(raw)
            && offset > 0
        {
            raw = &raw[..offset];
        }
        // A fence still open at the cut costs a newline plus the opener's own
        // run; shrink the take until the chunk plus its closer fits.
        let mut ticks = open_fence_ticks(&format!("{consumed}{raw}"));
        while let Some(open) = ticks {
            let overhead = lead.chars().count() + open + 1;
            if raw.chars().count() + overhead <= max {
                break;
            }
            let over = raw.chars().count() + overhead - max;
            let next = budget.saturating_sub(over).max(1);
            if next >= budget {
                break;
            }
            budget = next;
            raw = take_line_bounded(rest, budget);
            ticks = open_fence_ticks(&format!("{consumed}{raw}"));
        }
        let mut chunk = String::with_capacity(lead.len() + raw.len() + 8);
        chunk.push_str(&lead);
        chunk.push_str(raw);
        if let Some(ticks) = ticks {
            if !chunk.ends_with('\n') {
                chunk.push('\n');
            }
            chunk.push_str(&"`".repeat(ticks));
        }
        chunks.push(chunk);
        consumed.push_str(raw);
        rest = &rest[raw.len()..];
    }
    chunks
}

/// The tick count of the fence `prefix` leaves open, if it leaves one.
fn open_fence_ticks(prefix: &str) -> Option<usize> {
    match open_construct(prefix)? {
        OpenConstruct::Fence { ticks } => Some(ticks),
        OpenConstruct::Table { .. } => None,
    }
}

/// The byte offset at which the table `prefix` ends inside starts (its header
/// line), when the prefix's last non-blank line is a row of one. Mirrors
/// [`open_construct`]'s shape test.
fn open_table_start(prefix: &str) -> Option<usize> {
    let lines: Vec<&str> = prefix.split('\n').collect();
    let last = lines.iter().rposition(|line| !line.trim().is_empty())?;
    if !lines[last].contains('|') {
        return None;
    }
    let header = table_header_index(&lines, last)?;
    Some(lines[..header].iter().map(|line| line.len() + 1).sum())
}

/// The characters a native table costs its element once every row is escaped
/// (a row's `<` expands), newlines included: the element chunker measures the
/// escaped text, so a table that fits before escaping may not fit after.
fn native_table_len(region: &str) -> usize {
    region
        .split('\n')
        .map(|line| escape_line(line).chars().count() + 1)
        .sum()
}

/// The longest prefix of `text` within `max` chars ending at a line boundary,
/// falling back to the character cut when the line itself is longer than the
/// budget (or the text carries no newline).
pub(crate) fn take_line_bounded(text: &str, max: usize) -> &str {
    let max = max.max(1);
    // The whole text fits: take it all, newlines and all.
    let Some((end, _)) = text.char_indices().nth(max) else {
        return text;
    };
    let head = &text[..end];
    match head.rfind('\n') {
        Some(pos) => &text[..=pos],
        None => head,
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
                if self.tables_left == 0
                    || end - i - 2 > MAX_TABLE_ROWS
                    // A table that outgrows one element would be cut into a
                    // second table by the element chunker (its header is
                    // repeated), and the platform caps tables per card — so
                    // such a table is code here, exactly like an over-budget
                    // one (issue #1096).
                    || native_table_len(&region) > MAX_ELEMENT_TEXT_CHARS
                {
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

    /// A blob split across elements (issue #1096): a fence crossing the
    /// budget must leave no chunk starting inside code and none ending inside
    /// a fence. Every chunk is self-contained, so the code lines render
    /// verbatim (the sanitizer leaves them alone inside the fence) and the
    /// prose after the original closer is escaped as prose (outside it).
    #[test]
    fn a_fence_crossing_the_element_budget_stays_self_contained() {
        let code: String = (0..200).map(|i| format!("print(<{i}>)  # 一行代码\n")).collect();
        let text = format!("说明\n```python\n{code}```\n后的 <文字>");
        let chunks = chunk_markdown(&text, 3000);
        assert!(chunks.len() > 1, "the fixture must cross the budget");
        for chunk in &chunks {
            assert!(
                chunk.chars().count() <= 3000,
                "chunk over the element budget: {} chars",
                chunk.chars().count()
            );
            assert_eq!(neutralize_tail(chunk, "尾"), None, "a chunk ends inside a fence");
        }
        let rendered: String = chunks
            .iter()
            .map(|chunk| CardMarkdown::new().clean(chunk))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("print(<199>)"),
            "a code line rendered as prose (its `<` escaped): {rendered}"
        );
        assert!(
            rendered.contains("&#60;文字>"),
            "prose swallowed into a code block: {rendered}"
        );
    }

    /// A chunk boundary is a line boundary whenever the budget allows one, so
    /// an opener or closer is never cut mid-token (issue #1096). Only a line
    /// longer than the budget itself falls back to a character cut.
    #[test]
    fn chunking_prefers_line_boundaries() {
        let text = format!("第一行\n第二行\n{}", "长".repeat(30));
        let chunks = chunk_markdown(&text, 6);
        assert_eq!(
            chunks[0], "第一行\n",
            "a cut lands on the line end, not mid-token"
        );
        assert_eq!(chunks[1], "第二行\n");
        assert_eq!(chunks[2], "长".repeat(6), "a long line falls back to a char cut");
    }

    /// A table crossing the element budget keeps its header on every chunk
    /// (the same lead [`neutralize_tail`] gives a cut tail).
    #[test]
    fn a_table_crossing_the_element_budget_repeats_its_header() {
        let rows: String = (0..40).map(|i| format!("| 行 {i} | 值 {i} |\n")).collect();
        let text = format!("| 名称 | 值 |\n|---|---|\n{rows}尾注");
        let chunks = chunk_markdown(&text, 60);
        assert!(chunks.len() > 1);
        for chunk in chunks.iter().skip(1) {
            if chunk.contains('|') {
                assert!(
                    chunk.starts_with("| 名称 | 值 |\n|---|---|\n"),
                    "a continued chunk lost the table header: {chunk:?}"
                );
            }
        }
    }

    /// A table that outgrows one element is fenced as code (issue #1096): the
    /// element chunker would otherwise repeat its header on the next element,
    /// and the platform caps native tables per card.
    #[test]
    fn a_table_too_long_for_one_element_is_fenced() {
        let row = format!("| {} | 值 |\n", "长".repeat(100));
        let table = format!("| 名称 | 值 |\n|---|---|\n{}", row.repeat(30));
        assert!(table.chars().count() > MAX_ELEMENT_TEXT_CHARS);
        let cleaned = CardMarkdown::new().clean(&table);
        assert!(
            cleaned.starts_with("```\n"),
            "the long table is one code block: {cleaned:?}"
        );
        assert!(cleaned.contains("| 名称 | 值 |"), "the rows survive: {cleaned:?}");
    }

    /// A table that fits one element is moved whole to the next element rather
    /// than cut (issue #1096): a cut would repeat its header as a second table.
    #[test]
    fn a_fitting_table_moves_whole_to_the_next_element() {
        let prose = "说明。\n".repeat(400);
        let row = format!("| {} | 值 |\n", "x".repeat(50));
        let table = format!("| 名称 | 值 |\n|---|---|\n{}", row.repeat(10));
        let text = format!("{prose}{table}尾注");
        let chunks = chunk_markdown(&text, 1500);
        assert!(
            chunks
                .iter()
                .filter(|chunk| chunk.contains("| 名称 | 值 |"))
                .count()
                == 1,
            "the table must stay on one element: {chunks:?}"
        );
        for chunk in &chunks {
            assert_eq!(
                neutralize_tail(chunk, "| 尾 | 值 |"),
                None,
                "a chunk ends inside a table"
            );
        }
    }

    /// The gate's shape (issue #1096): four native tables and a fifth longer
    /// than the element budget. The fifth is code, so the card never carries a
    /// sixth native table.
    #[test]
    fn a_long_fifth_table_does_not_become_a_sixth_native_table() {
        let small = "| a | b |\n|---|---|\n| 1 | 2 |";
        let row = format!("| {} | 值 |\n", "x".repeat(120));
        let long = format!("| 长表 | 值 |\n|---|---|\n{}", row.repeat(30));
        let text = format!("{small}\n\n{small}\n\n{small}\n\n{small}\n\n{long}\n\n尾注");
        let cleaned = CardMarkdown::new().clean(&text);
        let chunks = chunk_markdown(&cleaned, MAX_ELEMENT_TEXT_CHARS);
        let native: usize = chunks
            .iter()
            .map(|chunk| chunk.lines().filter(|line| *line == "| a | b |").count())
            .sum();
        assert_eq!(native, 4, "four native tables: {chunks:?}");
        let long_headers: usize = chunks
            .iter()
            .map(|chunk| chunk.matches("| 长表 | 值 |").count())
            .sum();
        assert_eq!(long_headers, 1, "the long table appears once: {chunks:?}");
        let long_chunk = chunks
            .iter()
            .find(|chunk| chunk.contains("| 长表 | 值 |"))
            .expect("the long table renders");
        let before = long_chunk
            .rsplit_once("| 长表 | 值 |")
            .map(|(before, _)| before)
            .unwrap();
        assert!(
            before.trim_end().ends_with("```"),
            "the long table renders as code: {long_chunk:?}"
        );
    }

    /// A prose line that merely contains a pipe directly above a table is not
    /// its header: the delimiter row names the real header, so a cut inside
    /// the table still repeats the right one (issue #1096).
    #[test]
    fn a_pipe_prose_line_does_not_hide_the_table_header() {
        let prefix = "参见 a | b 的说明\n| 名称 | 值 |\n|---|---|\n| 一 | 1 |\n";
        assert_eq!(
            neutralize_tail(prefix, "| 二 | 2 |\n").as_deref(),
            Some("| 名称 | 值 |\n|---|---|\n")
        );
        // The same prose line and table through the element chunker: the table
        // is moved whole, so its header is not split off.
        let prose = format!("{}参见 a | b 的说明\n", "说明。\n".repeat(300));
        let row = format!("| {} | 值 |\n", "x".repeat(50));
        let table = format!("| 名称 | 值 |\n|---|---|\n{}", row.repeat(10));
        let chunks = chunk_markdown(&format!("{prose}{table}尾注"), 1500);
        assert!(
            chunks.iter().any(|chunk| chunk.contains("| 名称 | 值 |")),
            "the table must render: {chunks:?}"
        );
        let header = "| 名称 | 值 |";
        let row = format!("| {} | 值 |", "x".repeat(50));
        assert_eq!(
            chunks.iter().filter(|chunk| chunk.contains(header)).count(),
            1,
            "the table header appears once: {chunks:?}"
        );
        for chunk in &chunks {
            if chunk.contains(&row) {
                assert!(
                    chunk.contains(header),
                    "table rows split off their header: {chunk:?}"
                );
            }
        }
    }

    /// A table closed by a blank line is NOT open (issue #1096): a separate
    /// table that follows on the tail must not inherit the previous header.
    #[test]
    fn a_table_closed_by_a_blank_line_is_not_reopened() {
        let prefix = "| 名称 | 值 |\n|---|---|\n| 一 | 1 |\n\n";
        assert_eq!(
            neutralize_tail(prefix, "| 其他 | 值 |\n|---|---|\n| 二 | 2 |"),
            None
        );
        // The same prefix without the blank line is a cut inside the table.
        assert_eq!(
            neutralize_tail(prefix.trim_end_matches('\n'), "| 一 | 2 |"),
            Some("| 名称 | 值 |\n|---|---|\n".to_string())
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
