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
//! Matching is by module path, not by prose. Whitespace is irrelevant and
//! grouped forms count, so `crate::opencode`, `crate :: opencode`,
//! `crate::{opencode::types}` and a group split across lines all name the
//! adapter; the word OpenCode and the `[opencode]` config table do not.

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

/// One forbidden path found outside the allow-list.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) text: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: `{}` outside the client seam — {}",
            self.path, self.line, FORBIDDEN, self.text
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
    source
        .adapter_mentions()
        .into_iter()
        .map(|offset| {
            let line = source.line_of(offset);
            Finding {
                path: path.to_string(),
                line,
                text: lines.get(line - 1).map_or("", |text| text.trim()).to_string(),
            }
        })
        .collect()
}

/// Whitespace-free source text with a line map, so a module path can be
/// matched however it is spaced or wrapped (`crate::{\n opencode,\n}`).
struct Normalised {
    text: String,
    /// One 1-based line number per byte of `text`.
    lines: Vec<usize>,
}

impl Normalised {
    fn new(content: &str) -> Self {
        let mut text = String::new();
        let mut lines = Vec::new();
        for (index, line) in content.lines().enumerate() {
            for ch in line.chars().filter(|ch| !ch.is_whitespace()) {
                let before = text.len();
                text.push(ch);
                for _ in before..text.len() {
                    lines.push(index + 1);
                }
            }
        }
        Self { text, lines }
    }

    /// The byte offset of every mention of the adapter module.
    ///
    /// Both a plain path (`crate::opencode::types`) and a group that names it
    /// at any depth (`crate::{self, opencode::types}`,
    /// `crate::{opencode::{types, parsing}}`) count; the exempt item paths do
    /// not.
    fn adapter_mentions(&self) -> Vec<usize> {
        let mut mentions = Vec::new();
        let mut search = 0;
        while let Some(offset) = self.text[search..].find("crate::") {
            let start = search + offset + "crate::".len();
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
                mentions.push(start);
            }
        }
        mentions
    }

    fn line_of(&self, offset: usize) -> usize {
        self.lines.get(offset).copied().unwrap_or(1)
    }
}

/// The rest of `text` after the adapter name, when it starts the path.
fn adapter_suffix(text: &str) -> Option<&str> {
    text.strip_prefix("opencode")
        .filter(|suffix| !starts_with_identifier_char(suffix))
}

/// The mentions of the adapter inside a `crate::{…}` group (the content after
/// the opening brace, whitespace already stripped).
///
/// Items are split at top-level commas only, so a nested group stays one item;
/// a `self::` re-root is skipped before the adapter name.
fn group_mentions(grouped: &str, base: usize) -> Vec<usize> {
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

/// Note one group item if it names the adapter, unless it is an exempt item.
fn collect_group_item(item: &str, base: usize, mentions: &mut Vec<usize>) {
    let rooted = item.strip_prefix("self::").unwrap_or(item);
    let offset = base + (item.len() - rooted.len());
    if let Some(suffix) = adapter_suffix(rooted)
        && !is_exempt(suffix)
    {
        mentions.push(offset);
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

fn starts_with_identifier_char(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
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
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .unwrap_or(rest.len());
    PENDING_SEAM_MOVE
        .iter()
        .any(|allowed| allowed.rsplit("::").next() == Some(&rest[..end]))
}

/// `cargo xtask check-client-seam`: fail when a file outside the adapter names
/// `crate::opencode`, or when the sources cannot be scanned at all.
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
         the call through the client seam",
        findings.len()
    );
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(path: &str, planted: &str) -> Vec<Finding> {
        scan_source(path, planted)
    }

    #[test]
    fn a_planted_dto_import_outside_the_seam_is_a_finding() {
        let found = findings(
            "src/bridge/handler.rs",
            "use crate::opencode::types::Session;\nlet x = 1;\n",
        );
        assert_eq!(found.len(), 1, "the planted import must be found: {found:?}");
        assert_eq!(found[0].path, "src/bridge/handler.rs");
        assert_eq!(found[0].line, 1);
        assert_eq!(found[0].text, "use crate::opencode::types::Session;");
    }

    /// The module alias is as much a seam leak as a DTO import: it makes every
    /// `opencode::…` path in the file legal.
    #[test]
    fn a_planted_module_alias_outside_the_seam_is_a_finding() {
        assert_eq!(
            findings("src/bridge/snapshot.rs", "use crate::opencode;\n").len(),
            1
        );
    }

    /// A bare `crate::opencode` (no suffix) is a finding too.
    #[test]
    fn a_planted_bare_path_outside_the_seam_is_a_finding() {
        assert_eq!(findings("src/status.rs", "let m = crate::opencode;\n").len(), 1);
    }

    /// Grouped and spaced forms name the same module and must not slip past
    /// the matcher (adversarial review, #870).
    #[test]
    fn grouped_and_spaced_imports_of_the_adapter_are_findings() {
        for planted in [
            "use crate::{opencode};\n",
            "use crate::{opencode::types::Session};\n",
            "use crate::{self, opencode::types};\n",
            "use crate :: opencode :: types :: Session;\n",
            "use crate::{bridge, opencode::{types::Session, parsing}};\n",
        ] {
            assert_eq!(
                findings("src/bridge/handler.rs", planted).len(),
                1,
                "{planted} must be found"
            );
        }
    }

    /// A grouped import split across lines is still one path.
    #[test]
    fn a_multi_line_grouped_import_is_a_finding() {
        let found = findings("src/bridge/handler.rs", "use crate::{\n    opencode,\n};\n");
        assert_eq!(found.len(), 1, "the planted group must be found: {found:?}");
        assert_eq!(found[0].line, 2);
        assert_eq!(found[0].text, "opencode,");
    }

    #[test]
    fn the_adapter_may_name_itself() {
        let planted = "use crate::opencode::types::Session;\nuse crate::opencode;\n";
        assert!(findings("src/opencode/mod.rs", planted).is_empty());
        assert!(findings("src/opencode/v2/wire.rs", planted).is_empty());
        assert!(findings("src/opencode/tests.rs", planted).is_empty());
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
        assert!(findings("src/bridge/attach.rs", planted).is_empty());
        assert_eq!(
            PENDING_SEAM_MOVE,
            [
                "crate::opencode::generation",
                "crate::opencode::strategy",
                "crate::opencode::transport",
            ]
        );
    }

    /// The exemption is a module path, so it survives the normalised forms too.
    #[test]
    fn the_pending_exemption_survives_the_normalised_form() {
        for planted in [
            "use crate::{opencode::generation};\n",
            "use crate :: opencode :: strategy :: Generation ;\n",
            "use crate::{\n    opencode::transport::Transport,\n};\n",
            "use crate::{self::opencode::generation, opencode::strategy};\n",
        ] {
            assert!(
                findings("src/bridge/attach.rs", planted).is_empty(),
                "{planted} must stay exempt"
            );
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
        ] {
            assert_eq!(
                findings("src/bridge/attach.rs", planted).len(),
                1,
                "{planted} must not be exempt"
            );
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
            assert_eq!(
                findings("src/bridge/attach.rs", planted).len(),
                1,
                "{planted} must report the forbidden item"
            );
        }
    }

    /// Prose and the config table are not path references.
    #[test]
    fn prose_and_the_config_table_do_not_trip_the_guard() {
        let content = "\
//! The OpenCode adapter attaches to a shared server.\n\
let table = cfg.opencode.generation;\n\
let dir = path.join(\"opencode\").join(\"service.json\");\n";
        assert_eq!(findings("src/bridge/attach.rs", content), Vec::new());
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
