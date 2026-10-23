//! The client seam guard (spec #860, ticket #870).
//!
//! Scans the Rust sources under `src/` for the OpenCode adapter's module path
//! (`crate::opencode`) outside the adapter itself. The `Backend` trait is the
//! seam: its vocabulary — the DTOs in `crate::backend::types` — lives with the
//! contract, and the bridge reaches the active client through
//! `SharedCore::client`, never by naming one adapter. A second adapter
//! (ADR-0090) must not have to edit the bridge to exist, so a caller that
//! spells `crate::opencode` is exactly the coupling this guard forbids.
//!
//! The allow-list is `src/opencode/` itself — the adapter's own modules may
//! name each other freely.
//!
//! Three item paths are exempt as **batch-local scaffolding**: the adapter's
//! attach/lifecycle plumbing ([`PENDING_SEAM_MOVE`]) is still driven from the
//! bridge today, and ticket #875 — in this same batch — moves discovery,
//! attach and lifecycle behind the client seam. The exemption lets #870 and
//! #875 land side by side without moving OpenCode-specific types into the
//! neutral contract before a second adapter earns them (ADR-0090 decision 7).
//! #875 deletes each entry; the batch is not complete until this list is
//! **empty**. It is item-scoped, not file-scoped, and must not be extended: an
//! unrelated `crate::opencode::something` in the same file still fails.
//!
//! Matching runs on a normalised view of each file, not on raw text:
//!
//! - comments are whitespace (`crate /* c */ :: opencode` is the path), nested
//!   block comments included, but never inside a string or char literal;
//! - string and char literal contents are blanked, so prose spelling the path
//!   is not a finding;
//! - whitespace is collapsed to one space, except around `::`, `{`, `}` and `,`,
//!   where it is dropped — so `crate :: opencode`, grouped imports, and an
//!   import split across lines all name the same path, while `crate::opencode
//!   as oc` keeps the ` as oc` boundary that makes the alias a finding;
//! - `$crate` is the same root as `crate`, and a raw identifier is the
//!   identifier it spells: `crate::r#opencode` is the adapter,
//!   `crate::r#opencode_foo` is not.
//!
//! A crate re-root is itself a finding — `use crate as <ident>`,
//! `use crate::{self as <ident>}` and `extern crate self as <ident>` — because
//! renaming the crate root renames the adapter path everywhere in the file. At
//! a crate-root file (`src/main.rs`, `src/lib.rs`, `src/<name>.rs`) the path may
//! also be rooted at `self::`, so `use self::opencode as oc;` is the same leak;
//! inside a submodule `self::` names a sibling module, not the crate, and is
//! not reported.
//!
//! This is a **textual** guard, the same class as `generation_guard`, and it
//! accepts the limits that come with that: it does not follow `include!` of
//! files outside the scanned tree, it cannot see a path assembled at runtime,
//! and it cannot see an alias re-exported as a value.

use std::path::Path;

use crate::source_scan;

/// Paths (relative to the scanned root's parent, `/`-separated) exempt from the
/// guard.
pub(crate) const ALLOWED_PREFIXES: &[&str] = &[
    // The adapter owns its own internals: client, strategies, wire decoders,
    // transport and the tests that exercise them.
    "src/opencode/",
];

/// The adapter plumbing the bridge still names directly, one item path at a
/// time — batch-local scaffolding that ticket #875 deletes.
///
/// Attach and lifecycle are the last OpenCode-shaped work living in the bridge;
/// #875 moves them behind the client seam. Until then these three item paths
/// are the *only* escape, and the batch lands no further entry: when #875 is in,
/// this list is empty and the guard is strict.
pub(crate) const PENDING_SEAM_MOVE: &[&str] = &[
    "crate::opencode::generation",
    "crate::opencode::strategy",
    "crate::opencode::transport",
];

/// The module path the seam forbids outside the adapter.
pub(crate) const FORBIDDEN: &str = "crate::opencode";

/// What leaked: the adapter's module path, or the crate root re-rooted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leak {
    AdapterPath,
    CrateAlias,
}

impl Leak {
    fn describe(self) -> String {
        match self {
            Self::AdapterPath => format!("`{FORBIDDEN}` outside the client seam"),
            Self::CrateAlias => {
                "`use crate as …` re-roots the crate and defeats the client seam (textual guard: \
                 the alias is forbidden where it is created)"
                    .to_string()
            }
        }
    }
}

/// One forbidden construct found outside the allow-list.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) leak: Leak,
    pub(crate) text: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {} — {}",
            self.path,
            self.line,
            self.leak.describe(),
            self.text
        )
    }
}

/// Scan `root`'s Rust sources with this guard's matcher.
///
/// The walk, the repository-relative paths and the loud IO errors are shared
/// with the other source guards ([`source_scan`]); only the matching is this
/// guard's own.
pub(crate) fn scan_tree(root: &Path) -> Result<Vec<Finding>, String> {
    let mut findings = Vec::new();
    for file in source_scan::read_sources(root, "client-seam")? {
        findings.extend(scan_source(&file.path, &file.content));
    }
    Ok(findings)
}

/// Scan one file's content. `path` is the repository-relative path
/// (`src/...`), used for the allow-list.
pub(crate) fn scan_source(path: &str, content: &str) -> Vec<Finding> {
    if ALLOWED_PREFIXES.iter().any(|allowed| path.starts_with(allowed)) {
        return Vec::new();
    }
    let source = Normalised::new(content);
    let lines: Vec<&str> = content.lines().collect();
    let mut leaks = source.adapter_mentions(is_crate_root(path));
    leaks.extend(source.crate_alias_mentions());
    leaks.sort_by_key(|&(offset, _)| offset);
    leaks
        .into_iter()
        .map(|(offset, leak)| {
            let line = source.line_of(offset);
            Finding {
                path: path.to_string(),
                line,
                leak,
                text: lines.get(line - 1).map_or("", |text| text.trim()).to_string(),
            }
        })
        .collect()
}

/// Whether this scanned path is a crate-root file, where `self::` is the crate
/// root: `src/main.rs`, `src/lib.rs` or `src/<name>.rs`. In a submodule
/// (`src/bridge/foo.rs`) `self::` names a sibling module instead.
fn is_crate_root(path: &str) -> bool {
    path.strip_prefix("src/")
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
}

/// The token stream of one file, normalised for path matching.
struct Normalised {
    text: String,
    /// One 1-based line number per byte of `text`.
    lines: Vec<usize>,
}

impl Normalised {
    fn new(content: &str) -> Self {
        let tokens = Token::lex(content);
        let mut text = String::new();
        let mut lines = Vec::new();
        for (index, token) in tokens.iter().enumerate() {
            if index > 0 && needs_space(&tokens[index - 1], token) {
                push(&mut text, &mut lines, ' ', token.line);
            }
            for ch in token.text.chars() {
                push(&mut text, &mut lines, ch, token.line);
            }
        }
        Self { text, lines }
    }

    /// The offset and kind of every mention of the adapter module.
    ///
    /// Both a plain path (`crate::opencode::types`) and a group that names it
    /// at any depth (`crate::{self, opencode::types}`,
    /// `crate::{opencode::{types, parsing}}`) count; the exempt item paths do
    /// not. `self::opencode` is the same path only in a crate-root file, where
    /// `self` is the crate; a submodule's `self` names a sibling.
    fn adapter_mentions(&self, crate_root: bool) -> Vec<(usize, Leak)> {
        let roots: &[&str] = if crate_root {
            &["crate::", "self::"]
        } else {
            &["crate::"]
        };
        let mut mentions = Vec::new();
        for root in roots {
            let mut search = 0;
            while let Some(offset) = self.text[search..].find(root) {
                let start = search + offset + root.len();
                search = start;
                let rest = &self.text[start..];
                if let Some(grouped) = rest.strip_prefix('{') {
                    let Some(close) = matching_brace(grouped) else {
                        continue;
                    };
                    mentions.extend(group_mentions(&grouped[..close], start + 1));
                } else if let Some(suffix) = adapter_suffix(rest)
                    && !is_exempt(suffix)
                {
                    mentions.push((start, Leak::AdapterPath));
                }
            }
        }
        mentions
    }

    /// The offset of every crate re-root: `crate as <ident>`,
    /// `crate::{self as <ident>}` (in a group) and `extern crate self as
    /// <ident>`.
    ///
    /// The normalized text keeps one space between tokens, so the ` as `
    /// boundary is intact here even though it is dropped around `::`.
    fn crate_alias_mentions(&self) -> Vec<(usize, Leak)> {
        const ALIASES: &[&str] = &["crate as ", "crate self as "];
        let mut mentions = Vec::new();
        for pattern in ALIASES {
            let mut search = 0;
            while let Some(offset) = self.text[search..].find(pattern) {
                let start = search + offset;
                search = start + pattern.len();
                let alias = &self.text[start + pattern.len()..];
                let name_len = alias
                    .find(|ch: char| !is_identifier_continue(ch))
                    .unwrap_or(alias.len());
                if name_len == 0 {
                    continue;
                }
                let rooted = self.text[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|ch| !is_identifier_continue(ch));
                if rooted {
                    mentions.push((start, Leak::CrateAlias));
                }
            }
        }
        mentions
    }

    fn line_of(&self, offset: usize) -> usize {
        self.lines.get(offset).copied().unwrap_or(1)
    }
}

fn push(text: &mut String, lines: &mut Vec<usize>, ch: char, line: usize) {
    let before = text.len();
    text.push(ch);
    for _ in before..text.len() {
        lines.push(line);
    }
}

/// One lexed token: identifiers and punctuation, with its source line.
struct Token {
    text: String,
    line: usize,
}

impl Token {
    /// Lex `content` into tokens, dropping comments (they are whitespace) and
    /// blanking string/char literal contents (they are not code paths).
    fn lex(content: &str) -> Vec<Self> {
        let mut tokens = Vec::new();
        let mut index = 0;
        let mut line = 1;
        while index < content.len() {
            let rest = &content[index..];
            let ch = rest.chars().next().expect("index is on a char boundary");
            if ch == '\n' {
                line += 1;
                index += 1;
                continue;
            }
            if ch.is_whitespace() {
                index += ch.len_utf8();
                continue;
            }
            if rest.starts_with("//") {
                // A line comment runs to (not through) the newline.
                index = rest.find('\n').map_or(content.len(), |end| index + end);
                continue;
            }
            if rest.starts_with("/*") {
                let end = skip_block_comment(content, index);
                line += content[index..end].matches('\n').count();
                index = end;
                continue;
            }
            if let Some(end) = raw_string_end(content, index) {
                line += content[index..end].matches('\n').count();
                index = end;
                continue;
            }
            if ch == '"' {
                let end = string_end(content, index);
                line += content[index..end].matches('\n').count();
                index = end;
                continue;
            }
            if ch == '\''
                && let Some(end) = char_literal_end(content, index)
            {
                index = end;
                continue;
            }
            if ch == '$' && rest[1..].starts_with("crate") && !identifier_continues(&rest[6..]) {
                // `$crate` is the same root as `crate`.
                tokens.push(Self {
                    text: "crate".into(),
                    line,
                });
                index += 1 + "crate".len();
                continue;
            }
            if let Some(raw) = rest.strip_prefix("r#")
                && raw.chars().next().is_some_and(is_identifier_start)
            {
                // A raw identifier is the identifier it spells: `r#opencode` is
                // `opencode`, `r#opencode_foo` is a different name.
                let start = index + 2;
                let mut end = start;
                while end < content.len() {
                    let next = content[end..].chars().next().expect("char boundary");
                    if is_identifier_continue(next) {
                        end += next.len_utf8();
                    } else {
                        break;
                    }
                }
                tokens.push(Self {
                    text: content[start..end].to_string(),
                    line,
                });
                index = end;
                continue;
            }
            if is_identifier_start(ch) {
                let start = index;
                index += ch.len_utf8();
                while index < content.len() {
                    let next = content[index..].chars().next().expect("char boundary");
                    if is_identifier_continue(next) {
                        index += next.len_utf8();
                    } else {
                        break;
                    }
                }
                tokens.push(Self {
                    text: content[start..index].to_string(),
                    line,
                });
                continue;
            }
            if rest.starts_with("::") {
                tokens.push(Self {
                    text: "::".into(),
                    line,
                });
                index += 2;
                continue;
            }
            tokens.push(Self {
                text: ch.to_string(),
                line,
            });
            index += ch.len_utf8();
        }
        tokens
    }
}

/// One space between tokens, except where a path or group is written tight.
fn needs_space(previous: &Token, current: &Token) -> bool {
    const TIGHT: &[&str] = &["::", "{", "}", ","];
    !TIGHT.contains(&previous.text.as_str()) && !TIGHT.contains(&current.text.as_str())
}

fn is_identifier_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_'
}

fn is_identifier_continue(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn identifier_continues(text: &str) -> bool {
    text.chars().next().is_some_and(is_identifier_continue)
}

/// The rest of `text` after the adapter name, when it starts the path.
fn adapter_suffix(text: &str) -> Option<&str> {
    text.strip_prefix("opencode")
        .filter(|suffix| !suffix.chars().next().is_some_and(is_identifier_continue))
}

/// The mentions inside a `crate::{…}` group (the content after the opening
/// brace): the adapter's paths, and a `self as <ident>` re-root.
///
/// Items are split at top-level commas only, so a nested group stays one item;
/// a `self::` re-root is skipped before the adapter name.
fn group_mentions(grouped: &str, base: usize) -> Vec<(usize, Leak)> {
    let mut mentions = Vec::new();
    let mut depth = 0usize;
    let mut item_start = 0usize;
    for (index, ch) in grouped.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                collect_group_item(&grouped[item_start..index], base + item_start, &mut mentions);
                item_start = index + 1;
            }
            _ => {}
        }
    }
    collect_group_item(&grouped[item_start..], base + item_start, &mut mentions);
    mentions
}

/// Note one group item if it names the adapter or re-roots the crate, unless it
/// is an exempt item.
fn collect_group_item(item: &str, base: usize, mentions: &mut Vec<(usize, Leak)>) {
    let rooted = item.strip_prefix("self::").unwrap_or(item);
    let offset = base + (item.len() - rooted.len());
    if let Some(alias) = rooted.strip_prefix("self as ")
        && alias.chars().next().is_some_and(is_identifier_start)
    {
        mentions.push((offset, Leak::CrateAlias));
        return;
    }
    if let Some(suffix) = adapter_suffix(rooted)
        && !is_exempt(suffix)
    {
        mentions.push((offset, Leak::AdapterPath));
    }
}

fn matching_brace(after_open: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (index, ch) in after_open.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether the path continues with one of the [`PENDING_SEAM_MOVE`] items.
///
/// The item name is the whole identifier after `opencode::`: `strategy` is
/// exempt, `strategy_builder` is not.
fn is_exempt(suffix: &str) -> bool {
    let Some(rest) = suffix.strip_prefix("::") else {
        return false;
    };
    let end = rest
        .find(|ch: char| !is_identifier_continue(ch))
        .unwrap_or(rest.len());
    PENDING_SEAM_MOVE
        .iter()
        .any(|allowed| allowed.rsplit("::").next() == Some(&rest[..end]))
}

/// The byte just past `/* … */` at `at`, nested blocks included.
fn skip_block_comment(content: &str, at: usize) -> usize {
    let mut depth = 1usize;
    let mut index = at + 2;
    while index < content.len() {
        let rest = &content[index..];
        if rest.starts_with("/*") {
            depth += 1;
            index += 2;
        } else if rest.starts_with("*/") {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return index;
            }
        } else {
            index += rest.chars().next().expect("char boundary").len_utf8();
        }
    }
    index
}

/// The byte just past a normal string literal opening at `at`.
fn string_end(content: &str, at: usize) -> usize {
    let mut index = at + 1;
    while index < content.len() {
        let ch = content[index..].chars().next().expect("char boundary");
        if ch == '\\' {
            index += ch.len_utf8();
            if index < content.len() {
                index += content[index..].chars().next().expect("char boundary").len_utf8();
            }
        } else {
            index += ch.len_utf8();
            if ch == '"' {
                return index;
            }
        }
    }
    index
}

/// The byte just past a raw string literal (`r"…"`, `r#"…"#`, `br#"…"#`)
/// opening at `at`, when there is one.
fn raw_string_end(content: &str, at: usize) -> Option<usize> {
    let rest = &content[at..];
    let prefix = usize::from(rest.starts_with('b') || rest.starts_with('c'));
    let rest = rest[prefix..].strip_prefix('r')?;
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    rest[hashes..].strip_prefix('"')?;
    let body_start = at + prefix + 1 + hashes + 1;
    let closing = format!("\"{}", "#".repeat(hashes));
    let end = content[body_start..].find(&closing)?;
    Some(body_start + end + closing.len())
}

/// The byte just past a char literal opening at `at`, when there is one.
///
/// A lifetime (`'a`, `'static`) has no closing quote and returns `None`.
fn char_literal_end(content: &str, at: usize) -> Option<usize> {
    let rest = &content[at + 1..];
    let first = rest.chars().next()?;
    if first == '\'' {
        return None;
    }
    let body_end = if first != '\\' {
        at + 1 + first.len_utf8()
    } else {
        // An escape: `\n`, `\'`, `\\`, `\x41`, `\u{1F600}`.
        match content[at + 2..].chars().next()? {
            'x' => at + 2 + 1 + 2,
            'u' => {
                let close = content[at + 3..].find('}')?;
                at + 3 + close + 1
            }
            escaped => at + 2 + escaped.len_utf8(),
        }
    };
    content
        .get(body_end..)
        .filter(|after| after.starts_with('\''))
        .map(|_| body_end + 1)
}

/// `cargo xtask check-client-seam`: fail when a file outside the adapter names
/// `crate::opencode` or re-roots the crate, or when the sources cannot be
/// scanned at all.
pub(crate) fn run() {
    let findings = match scan_tree(&crate::repo_root().join("src")) {
        Ok(findings) => findings,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    if findings.is_empty() {
        return;
    }
    for finding in &findings {
        eprintln!("{finding}");
    }
    eprintln!(
        "error: {} reference(s) to the OpenCode adapter outside src/opencode/ — import the \
         contract from crate::backend, reach the backend through SharedCore::client, or route \
         the call through the client seam. `use crate as …` is never allowed there: it renames \
         the adapter path everywhere in the file",
        findings.len()
    );
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(planted: &str) -> Vec<Finding> {
        scan_source("src/bridge/handler.rs", planted)
    }

    #[test]
    fn a_planted_dto_import_outside_the_seam_is_a_finding() {
        let found = findings("use crate::opencode::types::Session;\nlet x = 1;\n");
        assert_eq!(found.len(), 1, "the planted import must be found: {found:?}");
        assert_eq!(found[0].path, "src/bridge/handler.rs");
        assert_eq!(found[0].line, 1);
        assert_eq!(found[0].leak, Leak::AdapterPath);
        assert_eq!(found[0].text, "use crate::opencode::types::Session;");
    }

    /// The module alias is as much a seam leak as a DTO import: it makes every
    /// `opencode::…` path in the file legal.
    #[test]
    fn a_planted_module_alias_outside_the_seam_is_a_finding() {
        assert_eq!(findings("use crate::opencode;\n").len(), 1);
    }

    /// A bare `crate::opencode` (no suffix) is a finding too.
    #[test]
    fn a_planted_bare_path_outside_the_seam_is_a_finding() {
        assert_eq!(findings("let m = crate::opencode;\n").len(), 1);
    }

    /// Grouped and spaced forms name the same module and must not slip past
    /// the matcher.
    #[test]
    fn grouped_and_spaced_imports_of_the_adapter_are_findings() {
        for planted in [
            "use crate::{opencode};\n",
            "use crate::{opencode::types::Session};\n",
            "use crate::{self, opencode::types};\n",
            "use crate :: opencode :: types :: Session;\n",
            "use crate::{bridge, opencode::{types::Session, parsing}};\n",
            "use crate::opencode as oc;\n",
        ] {
            assert_eq!(findings(planted).len(), 1, "{planted} must be found");
        }
    }

    /// A grouped import split across lines is still one path.
    #[test]
    fn a_multi_line_grouped_import_is_a_finding() {
        let found = findings("use crate::{\n    opencode,\n};\n");
        assert_eq!(found.len(), 1, "the planted group must be found: {found:?}");
        assert_eq!(found[0].line, 2);
        assert_eq!(found[0].text, "opencode,");
    }

    /// A comment is whitespace inside a path, in every position.
    #[test]
    fn comments_inside_a_path_do_not_hide_it() {
        for planted in [
            "use crate::/*c*/opencode::types::Session;\n",
            "use crate /* c */ :: opencode::types::Session;\n",
            "use crate::opencode /* c */ :: types::Session;\n",
            "use crate::{/* a */ opencode::types::Session};\n",
            "use crate::/* outer /* nested */ still */opencode::types::Session;\n",
            "use crate::{\n    opencode, // the adapter\n};\n",
        ] {
            assert_eq!(findings(planted).len(), 1, "{planted} must be found");
        }
    }

    /// `self::` is the crate root only in a crate-root file; in a submodule it
    /// names a sibling module.
    #[test]
    fn a_crate_root_self_path_is_a_finding_only_at_the_root() {
        for (path, planted) in [
            ("src/main.rs", "use self::opencode as oc;\n"),
            ("src/lib.rs", "use self::opencode::types::Session;\n"),
            ("src/status.rs", "use self::{opencode};\n"),
        ] {
            assert_eq!(scan_source(path, planted).len(), 1, "{path}: {planted}");
        }
        for path in [
            "src/bridge/foo.rs",
            "src/bridge/handler.rs",
            "src/opencode/v2/wire.rs",
        ] {
            if path.starts_with("src/opencode/") {
                continue;
            }
            assert!(
                scan_source(path, "use self::opencode as oc;\n").is_empty(),
                "{path} must not treat self:: as the crate root"
            );
        }
    }

    /// A raw identifier is the identifier it spells.
    #[test]
    fn a_raw_identifier_spelling_the_adapter_is_a_finding() {
        for planted in [
            "use crate::r#opencode::types::Session;\n",
            "use crate::{r#opencode::types::Session};\n",
        ] {
            assert_eq!(findings(planted).len(), 1, "{planted} must be found");
        }
        assert_eq!(
            scan_source("src/main.rs", "use self::r#opencode::types::Session;\n").len(),
            1
        );
        for planted in ["use crate::r#opencode_foo::Thing;\n", "let r#opencode = 1;\n"] {
            assert!(
                findings(planted).is_empty(),
                "{planted} is a different identifier"
            );
        }
    }

    /// `$crate` is the crate root, so a macro body can leak the adapter too.
    #[test]
    fn a_dollar_crate_path_is_a_finding() {
        let planted = "macro_rules! m { () => { $crate::opencode::types::Session } }\n";
        let found = findings(planted);
        assert_eq!(found.len(), 1, "the macro path must be found: {found:?}");
    }

    /// Re-rooting the crate renames the adapter path, so the alias is a
    /// finding where it is born.
    #[test]
    fn re_rooting_the_crate_is_a_finding() {
        let found = findings("use crate as c;\n\nfn f() -> c::opencode::types::Session { todo!() }\n");
        assert_eq!(found.len(), 1, "the alias must be found: {found:?}");
        assert_eq!(found[0].line, 1);
        assert_eq!(found[0].leak, Leak::CrateAlias);
        assert_eq!(found[0].text, "use crate as c;");
        // A group-rooted alias and the `extern crate self` spelling are the
        // same re-root.
        for planted in [
            "use crate::{self as c};\n",
            "extern crate self as c;\n",
            "use crate::{self as c, backend::types::Session};\n",
        ] {
            let found = findings(planted);
            assert_eq!(found.len(), 1, "{planted} must be found: {found:?}");
            assert_eq!(found[0].leak, Leak::CrateAlias);
        }
        // The ordinary path alias of an item is not a crate re-root.
        assert!(findings("use crate::backend::types::Session as S;\n").is_empty());
        assert!(findings("extern crate foo as bar;\n").is_empty());
    }

    /// String and char literal contents are not code: prose spelling the path
    /// must not be a finding, and a `//` inside a string is not a comment.
    #[test]
    fn literals_are_blanked_and_do_not_hide_code() {
        for planted in [
            "let s = \"crate :: opencode\";\n",
            "let s = \"crate::opencode::types::Session\";\n",
            "let s = r#\"crate::opencode\"#;\n",
            "let s = br\"crate::opencode\";\n",
            "let c = '\"';\nlet s = \"crate::opencode\";\n",
            "let s = \"a \\\" crate::opencode\";\n",
            "let s = \"line // not a comment\";\n",
            "/// crate::opencode in prose\n",
        ] {
            assert!(findings(planted).is_empty(), "{planted} must not report");
        }
        // A string carrying `//` must not swallow the rest of the line.
        let found = findings("let s = \"// not a comment\"; use crate::opencode::types::Session;\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, 1);
    }

    #[test]
    fn the_adapter_may_name_itself() {
        let planted = "use crate::opencode::types::Session;\nuse crate::opencode;\n";
        assert!(scan_source("src/opencode/mod.rs", planted).is_empty());
        assert!(scan_source("src/opencode/v2/wire.rs", planted).is_empty());
        assert!(scan_source("src/opencode/tests.rs", planted).is_empty());
    }

    /// Attach and lifecycle are still the bridge's (#875): the three documented
    /// items are exempt, and only those.
    #[test]
    fn the_pending_attach_plumbing_is_exempt_for_now() {
        let planted = "\
use crate::opencode::generation::{self, ProbeOutcome};
use crate::opencode::strategy::Generation;
use crate::opencode::transport::Transport;
";
        assert!(findings(planted).is_empty());
        assert_eq!(
            PENDING_SEAM_MOVE,
            [
                "crate::opencode::generation",
                "crate::opencode::strategy",
                "crate::opencode::transport",
            ]
        );
    }

    /// The exemption is a module path, so it survives the normalized forms too.
    #[test]
    fn the_pending_exemption_survives_the_normalized_form() {
        for planted in [
            "use crate::{opencode::generation};\n",
            "use crate :: opencode :: strategy :: Generation ;\n",
            "use crate::{\n    opencode::transport::Transport,\n};\n",
            "use crate::{self::opencode::generation, opencode::strategy};\n",
            "use crate::/*c*/opencode::transport::Transport;\n",
        ] {
            assert!(findings(planted).is_empty(), "{planted} must stay exempt");
        }
    }

    /// The exemption is per item, not per file, and it stops at an identifier
    /// boundary: a near-miss name is a finding.
    #[test]
    fn a_near_miss_on_an_exempt_item_name_is_a_finding() {
        for planted in [
            "use crate::opencode::strategy_builder::Plan;\n",
            "use crate::opencode::types::Session;\n",
            "use crate::opencode::client::OpenCodeBackend;\n",
            "use crate::opencode::parsing::parse_model;\n",
            "use crate::{opencode::strategy_builder};\n",
            "use crate::opencode as oc;\n",
        ] {
            assert_eq!(findings(planted).len(), 1, "{planted} must not be exempt");
        }
    }

    /// An exempt item does not shield a forbidden one beside it, on one line or
    /// inside one group.
    #[test]
    fn an_exempt_item_does_not_shield_a_forbidden_one() {
        for planted in [
            "use crate::opencode::strategy::Generation; use crate::opencode::types::Session;\n",
            "use crate::{opencode::generation, opencode::types::Session};\n",
            "use crate::{\n    opencode::transport,\n    opencode::client::OpenCodeBackend,\n};\n",
        ] {
            assert_eq!(findings(planted).len(), 1, "{planted} must report one");
        }
    }

    /// Prose and the config table are not path references.
    #[test]
    fn prose_and_the_config_table_do_not_trip_the_guard() {
        let content = "\
//! The OpenCode adapter attaches to a shared server.\n\
let table = cfg.opencode.generation;\n\
let dir = path.join(\"opencode\").join(\"service.json\");\n";
        assert_eq!(findings(content), Vec::new());
    }

    /// The tree walk is shared with the generation guard; through this guard it
    /// is still recursive, `.rs`-only, and reports repository-relative paths.
    #[test]
    fn the_tree_walk_reports_nested_rust_files_only() {
        let root = std::env::temp_dir().join(format!("cola-client-seam-{}", std::process::id()));
        let nested = root.join("src").join("bridge").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("planted.rs"),
            "use crate::opencode::types::Session;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("README.md"),
            "use crate::opencode::types::Session;\n",
        )
        .unwrap();

        let found = scan_tree(&root.join("src")).expect("the planted tree is readable");
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            found.len(),
            1,
            "only the planted Rust file should report: {found:?}"
        );
        assert_eq!(found[0].path, "src/bridge/nested/planted.rs");
    }

    /// A missing or unreadable scan root is a loud error, never a clean tree.
    #[test]
    fn an_unreadable_scan_root_is_a_loud_error() {
        let missing = std::env::temp_dir().join(format!("cola-client-seam-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let error = scan_tree(&missing).expect_err("a missing root must fail the scan");
        assert!(
            error.contains("cannot scan client-seam directory"),
            "the error must name the failing path: {error}"
        );
        assert!(
            error.contains(missing.to_string_lossy().as_ref()),
            "unexpected: {error}"
        );
    }

    /// The acceptance criterion's "passes on the tree": the repository's own
    /// sources are clean, so any future escape fails this test and CI.
    #[test]
    fn the_repository_tree_is_clean() {
        let found = scan_tree(&crate::repo_root().join("src")).expect("the repository tree must be readable");
        assert!(
            found.is_empty(),
            "references to the OpenCode adapter outside it:\n{}",
            found.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn the_allow_list_covers_only_the_adapter() {
        assert_eq!(ALLOWED_PREFIXES, ["src/opencode/"]);
    }
}
