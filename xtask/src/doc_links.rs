//! The doc-link guard: `cargo xtask check-doc-links`.
//!
//! The live docs cite three kinds of machine-resolvable target — an `ADR-NNNN`,
//! a `docs/research/…` or `docs/reference/…` path, and an `src/…:line` pin — and
//! a dead one is invisible to review until a reader follows it. This guard
//! resolves every citation and fails with the citing file and line.
//!
//! A pin is checked for existence and range only: the file must exist and the
//! cited lines must be within it. Drift by ±lines after an unrelated edit is
//! expected and is not a finding (ADR-0110); matching the statement at the line
//! would turn every reformat into a doc edit.
//!
//! `docs/research/` is dated evidence about the tree at the commit it records.
//! Its `ADR-NNNN` and doc-path citations are still checked — those name things
//! that outlive the commit — but its `src/…:line` pins are not: the files they
//! name have since moved or been deleted by ordinary refactoring, and rewriting
//! the pins would falsify the record. It remains a valid link target when a live
//! doc cites it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::exit;

/// Root documents scanned in addition to everything under `docs/`.
pub(crate) const ROOT_DOCS: &[&str] = &[
    "AGENTS.md",
    "GLOSSARY.md",
    "CONTRIBUTING.md",
    "CODING_STANDARDS.md",
];

/// Dated research records keep their source pins as written (see the module
/// doc); their ADR and doc-path citations are still checked.
pub(crate) const HISTORICAL_PINS_PREFIX: &str = "docs/research/";

/// An ADR carrying this many amendment banners is a mandatory read at review
/// (ADR-0110 §3); the count is a warning, never a failure.
pub(crate) const BANNER_THRESHOLD: usize = 3;

/// The banner verbs whose `by ADR-NNNN` form ADR-0110 counts.
const BANNER_VERBS: &[&str] = &["Amended", "Superseded", "Retired", "Reopened", "Ratified"];

/// One machine-resolvable citation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Target {
    /// A four-digit ADR number, resolved against `docs/adr/`.
    Adr(String),
    /// A `docs/research/…` or `docs/reference/…` path.
    DocPath(String),
    /// An `src/…:start-end` pin (`end` equals `start` for a single line).
    SourcePin { path: String, start: usize, end: usize },
    /// A pin whose line number does not fit the platform's `usize`; carried as
    /// written so the finding quotes the citation.
    InvalidPin { text: String },
}

impl Target {
    fn text(&self) -> String {
        match self {
            Self::Adr(number) => format!("ADR-{number}"),
            Self::DocPath(path) => path.clone(),
            Self::SourcePin { path, start, end } if end == start => format!("{path}:{start}"),
            Self::SourcePin { path, start, end } => format!("{path}:{start}-{end}"),
            Self::InvalidPin { text } => text.clone(),
        }
    }

    fn is_source_pin(&self) -> bool {
        matches!(self, Self::SourcePin { .. } | Self::InvalidPin { .. })
    }
}

/// One dangling citation: where it was written, what it names, and why it fails.
#[derive(Debug)]
pub(crate) struct Finding {
    pub(crate) path: String,
    pub(crate) line: usize,
    target: String,
    reason: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: dangling {} — {}",
            self.path, self.line, self.target, self.reason
        )
    }
}

/// A scanned document, keyed by its repository-relative path.
#[derive(Debug)]
struct SourceDoc {
    path: String,
    content: String,
}

/// What one full scan found: the failures, the review warnings, and how many
/// citations were checked (so a test can prove the scan was not vacuous).
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub(crate) findings: Vec<Finding>,
    pub(crate) warnings: Vec<String>,
    pub(crate) checked: usize,
}

/// Extract every citation in `content`, one entry per line, deduplicated within
/// a line so a sentence naming the same target twice is reported once.
pub(crate) fn targets(content: &str) -> Vec<(usize, Target)> {
    let mut found = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let mut targets = Vec::new();
        adr_targets(line, &mut targets);
        doc_path_targets(line, &mut targets);
        source_pin_targets(line, &mut targets);
        targets.sort();
        targets.dedup();
        found.extend(targets.into_iter().map(|target| (index + 1, target)));
    }
    found
}

/// `ADR-NNNN` citations, including the slash-separated shorthand the docs use
/// (`ADR-0105/0106/0107` cites three ADRs).
fn adr_targets(line: &str, out: &mut Vec<Target>) {
    for (start, _) in line.match_indices("ADR-") {
        if start > 0 && is_preceding_path_char(previous_char(line, start)) {
            continue;
        }
        let rest = &line[start + 4..];
        let mut offset = 0;
        loop {
            let candidate = &rest[offset..];
            let digits: String = candidate.chars().take_while(char::is_ascii_digit).collect();
            // `ADR-00012` is not a four-digit citation.
            if digits.len() != 4 || candidate[4..].starts_with(|c: char| c.is_ascii_digit()) {
                break;
            }
            out.push(Target::Adr(digits));
            offset += 4;
            // `ADR-0105/0106` continues after the slash; anything else ends the run.
            let Some(next) = rest[offset..].strip_prefix('/') else {
                break;
            };
            if !next.starts_with(|c: char| c.is_ascii_digit()) {
                break;
            }
            offset += 1;
        }
    }
}

/// `docs/research/…` and `docs/reference/…` paths, with a sentence's full stop
/// and any `#anchor` stripped.
fn doc_path_targets(line: &str, out: &mut Vec<Target>) {
    for prefix in ["docs/research/", "docs/reference/"] {
        for (start, _) in line.match_indices(prefix) {
            if start > 0 && is_preceding_path_char(previous_char(line, start)) {
                continue;
            }
            let rest = &line[start + prefix.len()..];
            let length = rest.find(|c: char| !is_path_char(c)).unwrap_or(rest.len());
            if length == 0 {
                continue;
            }
            // The slice already ends at the first non-path character, so only
            // a sentence's full stop can still be attached to it.
            let path = line[start..start + prefix.len() + length].trim_end_matches('.');
            if path.len() > prefix.len() {
                out.push(Target::DocPath(path.to_string()));
            }
        }
    }
}

/// `src/…:line` and `src/…:start-end` pins. A bare `src/…` path is not a pin.
fn source_pin_targets(line: &str, out: &mut Vec<Target>) {
    for (start, _) in line.match_indices("src/") {
        if start > 0 && is_preceding_path_char(previous_char(line, start)) {
            continue;
        }
        let rest = &line[start..];
        let end = rest.find(|c: char| !is_path_char(c)).unwrap_or(rest.len());
        let path = &rest[..end];
        if path == "src/" || !rest[end..].starts_with(':') {
            continue;
        }
        let after = &rest[end + 1..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            // `src/x.rs:` with no number is not a pin.
            continue;
        }
        let Ok(start_line) = digits.parse::<usize>() else {
            out.push(Target::InvalidPin {
                text: format!("{path}:{digits}"),
            });
            continue;
        };
        let mut end_line = start_line;
        let tail = &after[digits.len()..];
        if let Some(rest) = tail.strip_prefix('-') {
            let end_digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            // `src/x.rs:12-` is not a range; the single-line pin still is.
            if !end_digits.is_empty() {
                match end_digits.parse::<usize>() {
                    Ok(range_end) => end_line = range_end,
                    Err(_) => {
                        out.push(Target::InvalidPin {
                            text: format!("{path}:{digits}-{end_digits}"),
                        });
                        continue;
                    }
                }
            }
        }
        out.push(Target::SourcePin {
            path: path.to_string(),
            start: start_line,
            end: end_line,
        });
    }
}

fn previous_char(line: &str, index: usize) -> char {
    line[..index].chars().next_back().unwrap_or(' ')
}

/// Characters that continue a repository path. `#` is excluded so an anchor is
/// not part of the path, and a non-ASCII alphanumeric is allowed so a file with
/// a Chinese name is still a citation.
fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '/' | '_' | '.' | '-')
}

/// Characters that, immediately before `src/`, `docs/…` or `ADR-`, mean the
/// token is part of a longer path or word. Deliberately ASCII-only: Chinese
/// prose has no spaces, so `见ADR-0042` must still be a citation.
fn is_preceding_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-')
}

/// Count the `Amended/Superseded/Retired/Reopened/Ratified by ADR-NNNN` banners
/// in one ADR. A dated amendment ("Amended 2026-10-01 (#470)") is a banner too
/// in the review sense, but it is not the `by ADR` form ADR-0110 counts.
pub(crate) fn banner_count(content: &str) -> usize {
    content
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            let Some(rest) = line.strip_prefix("> **") else {
                return false;
            };
            let Some(end) = rest.find("**") else {
                return false;
            };
            let label = &rest[..end];
            BANNER_VERBS.iter().any(|verb| label.starts_with(verb)) && label.contains("by ADR-")
        })
        .count()
}

/// Every ADR file's four-digit number and its repository-relative path.
fn read_adrs(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let dir = root.join("docs/adr");
    let entries = std::fs::read_dir(&dir).map_err(|e| {
        format!(
            "error: cannot scan doc-links ADR directory {}: {e}",
            dir.display()
        )
    })?;
    let mut adrs = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("error: cannot read an entry of {}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let bytes = name.as_bytes();
        if bytes.len() < 6
            || !name.ends_with(".md")
            || !bytes[..4].iter().all(u8::is_ascii_digit)
            || bytes[4] != b'-'
        {
            continue;
        }
        // The first four bytes are ASCII digits, so the slice is a char boundary.
        let number = &name[..4];
        if let Some(previous) = adrs.insert(number.to_string(), format!("docs/adr/{name}")) {
            return Err(format!(
                "error: ADR number {number} is claimed by both {previous} and {name}"
            ));
        }
    }
    if adrs.is_empty() {
        return Err(format!("error: no numbered ADRs found under {}", dir.display()));
    }
    Ok(adrs)
}

/// Every Markdown document the guard reads, sorted by path. `docs/research/` is
/// skipped as a source; the root documents are always read.
fn scan_sources(root: &Path) -> Result<Vec<SourceDoc>, String> {
    let mut paths = Vec::new();
    collect_markdown(&root.join("docs"), &mut paths, &mut BTreeSet::new())?;
    for relative in ROOT_DOCS {
        let path = root.join(relative);
        if !path.is_file() {
            return Err(format!("error: doc-links scan target {relative} does not exist"));
        }
        paths.push(path);
    }
    paths.sort();

    paths
        .into_iter()
        .map(|path| {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("error: cannot read doc-links candidate {}: {e}", path.display()))?;
            Ok(SourceDoc {
                path: relative_path(root, &path),
                content,
            })
        })
        .collect()
}

fn collect_markdown(
    dir: &Path,
    paths: &mut Vec<PathBuf>,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    // Canonicalising before the visited check keeps a directory symlink (the
    // repo has none under `docs/`, but a future one must not loop) from
    // walking the same tree through ever-longer paths.
    let canonical = dir
        .canonicalize()
        .map_err(|e| format!("error: cannot resolve doc-links directory {}: {e}", dir.display()))?;
    if !visited.insert(canonical) {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("error: cannot scan doc-links directory {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("error: cannot read an entry of {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_markdown(&path, paths, visited)?;
        } else if path.extension().is_some_and(|ext| ext == "md") {
            paths.push(path);
        }
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The dangling citations in one document, from its already-extracted targets.
fn dangling(
    root: &Path,
    adrs: &BTreeMap<String, String>,
    path: &str,
    targets: &[(usize, Target)],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut line_counts: BTreeMap<String, Option<usize>> = BTreeMap::new();
    for (line, target) in targets {
        let reason = match target {
            Target::InvalidPin { .. } => Some("invalid line number".to_string()),
            Target::Adr(number) if !adrs.contains_key(number) => Some("no such ADR".to_string()),
            Target::Adr(_) => None,
            Target::DocPath(doc) if !root.join(doc).is_file() => Some("no such file".to_string()),
            Target::DocPath(_) => None,
            Target::SourcePin { path, start, end } => {
                let lines = *line_counts.entry(path.clone()).or_insert_with(|| {
                    std::fs::read_to_string(root.join(path))
                        .ok()
                        .map(|content| content.lines().count())
                });
                match lines {
                    None => Some("no such source file".to_string()),
                    Some(_) if *start == 0 || *end < *start => Some("invalid line range".to_string()),
                    Some(count) if *start > count || *end > count => {
                        Some(format!("the source file has {count} lines"))
                    }
                    Some(_) => None,
                }
            }
        };
        if let Some(reason) = reason {
            findings.push(Finding {
                path: path.to_string(),
                line: *line,
                target: target.text(),
                reason,
            });
        }
    }
    findings
}

/// Warnings for every ADR carrying an amendment count at or above the threshold.
fn banner_warnings(root: &Path, adrs: &BTreeMap<String, String>) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    for adr in adrs.values() {
        let content = std::fs::read_to_string(root.join(adr))
            .map_err(|e| format!("error: cannot read doc-links ADR {adr}: {e}"))?;
        let count = banner_count(&content);
        if count >= BANNER_THRESHOLD {
            warnings.push(format!(
                "warning: {adr}: {count} amendment banners — a mandatory read at review (ADR-0110 §3)"
            ));
        }
    }
    Ok(warnings)
}

/// Run the whole guard: fail on any dangling citation, warn on the banner count.
pub(crate) fn check_tree(root: &Path) -> Result<Report, String> {
    let adrs = read_adrs(root)?;
    let sources = scan_sources(root)?;
    let mut report = Report {
        warnings: banner_warnings(root, &adrs)?,
        ..Report::default()
    };
    for source in &sources {
        let historical = source.path.starts_with(HISTORICAL_PINS_PREFIX);
        let targets: Vec<(usize, Target)> = targets(&source.content)
            .into_iter()
            .filter(|(_, target)| !(historical && target.is_source_pin()))
            .collect();
        report.checked += targets.len();
        report
            .findings
            .extend(dangling(root, &adrs, &source.path, &targets));
    }
    Ok(report)
}

/// `cargo xtask check-doc-links`: fail on a dangling doc citation, or when the
/// tree cannot be scanned. Banner counts are printed as warnings only.
pub(crate) fn run() {
    exit(run_on(&crate::repo_root()));
}

/// The command body and its exit status, without the process exit: a dangling
/// citation is a failure, a banner warning is not.
fn run_on(root: &Path) -> i32 {
    let report = match check_tree(root) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    for warning in &report.warnings {
        eprintln!("{warning}");
    }
    if report.findings.is_empty() {
        return 0;
    }
    for finding in &report.findings {
        eprintln!("{finding}");
    }
    eprintln!(
        "{} dangling doc citation(s) — fix the reference or delete it; re-run \
         `cargo xtask check-doc-links`",
        report.findings.len()
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway repository tree; each test gets its own directory.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("cola-doc-links-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("docs/adr")).expect("create the test tree");
            for doc in ROOT_DOCS {
                std::fs::write(root.join(doc), "# doc\n").expect("write a root document");
            }
            std::fs::write(root.join("docs/adr/0001-baseline.md"), "# baseline\n")
                .expect("write the baseline ADR");
            Self { root }
        }

        fn file(&self, relative: &str, content: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().expect("a file has a parent"))
                .expect("create the parent directory");
            std::fs::write(path, content).expect("write the file");
        }

        fn check(&self) -> Report {
            check_tree(&self.root).expect("the test tree must scan")
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn every_citation_kind_is_extracted() {
        let line = "See ADR-0042, `docs/research/x/y.md`, and `src/bridge/core.rs:47-52`.";
        let targets: Vec<Target> = targets(line).into_iter().map(|(_, t)| t).collect();
        assert_eq!(
            targets,
            vec![
                Target::Adr("0042".to_string()),
                Target::DocPath("docs/research/x/y.md".to_string()),
                Target::SourcePin {
                    path: "src/bridge/core.rs".to_string(),
                    start: 47,
                    end: 52
                },
            ]
        );
    }

    #[test]
    fn markdown_punctuation_and_anchors_are_not_part_of_a_path() {
        let line = "see docs/reference/api.md#section, and docs/research/a/b.md).";
        let paths: Vec<String> = targets(line)
            .into_iter()
            .filter_map(|(_, target)| match target {
                Target::DocPath(path) => Some(path),
                _ => None,
            })
            .collect();
        assert_eq!(
            paths,
            vec![
                "docs/reference/api.md".to_string(),
                "docs/research/a/b.md".to_string()
            ]
        );
    }

    #[test]
    fn a_bare_source_path_is_not_a_pin() {
        assert!(targets("`src/bridge/core.rs`").is_empty());
    }

    #[test]
    fn a_shorthand_adr_list_resolves_every_number() {
        let targets: Vec<Target> = targets("ADR-0105/0106/0107")
            .into_iter()
            .map(|(_, target)| target)
            .collect();
        assert_eq!(
            targets,
            vec![
                Target::Adr("0105".to_string()),
                Target::Adr("0106".to_string()),
                Target::Adr("0107".to_string()),
            ]
        );
    }

    #[test]
    fn a_five_digit_number_is_not_an_adr_citation() {
        assert!(targets("ADR-00012").is_empty());
    }

    #[test]
    fn a_host_repository_path_is_not_a_cola_pin() {
        assert!(targets("packages/core/src/session.ts:75-82").is_empty());
    }

    #[test]
    fn a_degenerate_src_prefix_is_not_a_pin() {
        assert!(targets("`src/:12`").is_empty());
    }

    #[test]
    fn a_non_adr_continuation_ends_the_shorthand_run() {
        for line in ["ADR-0105/x", "ADR-0105/01062"] {
            let targets: Vec<Target> = targets(line).into_iter().map(|(_, target)| target).collect();
            assert_eq!(targets, vec![Target::Adr("0105".to_string())], "{line}");
        }
    }

    #[test]
    fn a_trailing_hyphen_without_a_range_end_is_a_single_line_pin() {
        let targets: Vec<Target> = targets("`src/a.rs:12-`")
            .into_iter()
            .map(|(_, target)| target)
            .collect();
        assert_eq!(
            targets,
            vec![Target::SourcePin {
                path: "src/a.rs".to_string(),
                start: 12,
                end: 12,
            }]
        );
    }

    #[test]
    fn an_existing_reference_resolves() {
        let tree = Tree::new("resolves");
        tree.file("docs/research/a/b.md", "# b\n");
        tree.file("src/bridge/core.rs", "one\ntwo\nthree\n");
        tree.file(
            "docs/adr/0002-ok.md",
            "ADR-0001, `docs/research/a/b.md`, `src/bridge/core.rs:2-3`.\n",
        );
        assert_eq!(tree.check().findings.len(), 0);
        assert!(tree.check().checked > 0, "the scan must not be vacuous");
    }

    #[test]
    fn a_missing_adr_fails_naming_the_citing_line() {
        let tree = Tree::new("missing-adr");
        tree.file("docs/adr/0002-note.md", "intro\nSee ADR-9999.\n");
        let findings = tree.check().findings;
        assert_eq!(findings.len(), 1, "{findings:?}");
        let finding = findings[0].to_string();
        assert!(finding.contains("docs/adr/0002-note.md:2"), "{finding}");
        assert!(finding.contains("ADR-9999"), "{finding}");
    }

    #[test]
    fn a_missing_reference_path_fails() {
        let tree = Tree::new("missing-path");
        tree.file("docs/note.md", "see `docs/research/gone.md`\n");
        let findings = tree.check().findings;
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].to_string().contains("docs/research/gone.md"),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn a_pin_to_a_missing_source_file_fails() {
        let tree = Tree::new("missing-source");
        tree.file("docs/note.md", "`src/bridge/gone.rs:1`\n");
        let findings = tree.check().findings;
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].to_string().contains("no such source file"),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn a_pin_that_fits_the_file_resolves() {
        let tree = Tree::new("pin-fits");
        tree.file("src/small.rs", "one\ntwo\nthree\n");
        tree.file("docs/note.md", "`src/small.rs:3-3`\n");
        assert_eq!(tree.check().findings.len(), 0);
    }

    #[test]
    fn a_pin_may_name_any_file_under_src() {
        let tree = Tree::new("pin-non-rs");
        tree.file("src/feishu/fixtures/card.json", "one\ntwo\n");
        tree.file("docs/note.md", "`src/feishu/fixtures/card.json:2`\n");
        assert_eq!(tree.check().findings.len(), 0);
    }

    #[test]
    fn a_line_past_the_end_fails() {
        let tree = Tree::new("pin-past-end");
        tree.file("src/small.rs", "one\ntwo\nthree\n");
        tree.file("docs/note.md", "`src/small.rs:4`\n");
        assert_eq!(tree.check().findings.len(), 1);
    }

    #[test]
    fn a_range_past_the_end_fails() {
        let tree = Tree::new("pin-range-past-end");
        tree.file("src/small.rs", "one\ntwo\nthree\n");
        tree.file("docs/note.md", "`src/small.rs:2-4`\n");
        assert_eq!(tree.check().findings.len(), 1);
    }

    #[test]
    fn an_inverted_range_fails() {
        let tree = Tree::new("pin-inverted");
        tree.file("src/small.rs", "one\ntwo\nthree\n");
        tree.file("docs/note.md", "`src/small.rs:3-2`\n");
        assert_eq!(tree.check().findings.len(), 1);
    }

    #[test]
    fn a_line_number_too_large_to_parse_fails() {
        let tree = Tree::new("pin-start-overflow");
        tree.file("src/small.rs", "one\n");
        tree.file("docs/note.md", "`src/small.rs:99999999999999999999999999`\n");
        let findings = tree.check().findings;
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].to_string().contains("invalid line number"),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn a_range_end_too_large_to_parse_fails() {
        let tree = Tree::new("pin-end-overflow");
        tree.file("src/small.rs", "one\n");
        tree.file("docs/note.md", "`src/small.rs:1-99999999999999999999999999`\n");
        let findings = tree.check().findings;
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].to_string().contains("invalid line number"),
            "{}",
            findings[0]
        );
    }

    /// A directory symlink back into the scanned tree must be visited once, not
    /// followed through ever-longer paths. The property is termination: a
    /// regression hangs this test rather than failing it.
    #[cfg(unix)]
    #[test]
    fn a_directory_symlink_cycle_is_scanned_once() {
        let tree = Tree::new("symlink-cycle");
        std::os::unix::fs::symlink(tree.root.join("docs"), tree.root.join("docs/loop"))
            .expect("create the directory symlink");
        let report = tree.check();
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn a_citation_with_a_non_ascii_path_component_resolves() {
        let tree = Tree::new("unicode-path");
        tree.file("docs/reference/接口.md", "# api\n");
        tree.file("src/路径.rs", "one\n");
        tree.file("docs/note.md", "`docs/reference/接口.md`, `src/路径.rs:1`\n");
        assert_eq!(tree.check().findings.len(), 0);
    }

    #[test]
    fn a_missing_non_ascii_reference_fails() {
        let tree = Tree::new("unicode-missing");
        tree.file("docs/note.md", "`docs/reference/接口.md`\n");
        assert_eq!(tree.check().findings.len(), 1);
    }

    /// Chinese prose has no spaces, so a citation directly after it must still
    /// be extracted.
    #[test]
    fn a_citation_after_chinese_prose_is_extracted() {
        let targets: Vec<Target> = targets("见ADR-0042")
            .into_iter()
            .map(|(_, target)| target)
            .collect();
        assert_eq!(targets, vec![Target::Adr("0042".to_string())]);
    }

    #[test]
    fn a_non_ascii_adr_filename_is_ignored_without_panicking() {
        let tree = Tree::new("non-ascii-adr");
        tree.file("docs/adr/é-notes.md", "# notes\n");
        assert_eq!(tree.check().findings.len(), 0);
    }

    #[test]
    fn a_dated_amendment_is_not_a_banner_by_adr() {
        let content = "> **Amended by ADR-0002**: x\n\
                       > **Amended 2026-10-01 (#470)**: y\n\
                       > **Amended by the 2026-11-04 update below**: z\n\
                       > **Partly superseded by the 2026-09-26 update**: w\n";
        assert_eq!(banner_count(content), 1);
    }

    #[test]
    fn three_banners_are_reported_and_two_are_not() {
        let tree = Tree::new("banner-count");
        tree.file(
            "docs/adr/0002-three.md",
            "> **Amended by ADR-0001**: a\n> **Reopened by ADR-0001**: b\n\
             > **Retired by ADR-0001**: c\n",
        );
        tree.file(
            "docs/adr/0003-two.md",
            "> **Amended by ADR-0001**: a\n> **Reopened by ADR-0001**: b\n",
        );
        let warnings = tree.check().warnings;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("0002-three"), "{warnings:?}");
        assert!(warnings[0].contains("3 amendment banners"), "{warnings:?}");
    }

    /// A research snapshot's ADR and doc-path citations name things that
    /// outlive the commit, so they are checked like a live doc's.
    #[test]
    fn research_adr_and_path_citations_are_checked() {
        let tree = Tree::new("research-citations");
        tree.file(
            "docs/research/snapshot.md",
            "See ADR-9999 and `docs/research/gone.md`.\n",
        );
        let findings = tree.check().findings;
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(
            findings
                .iter()
                .all(|finding| finding.path == "docs/research/snapshot.md")
        );
    }

    /// Its source pins are dated evidence about a past tree: skipped there,
    /// checked in a live doc.
    #[test]
    fn research_source_pins_are_not_checked_but_live_pins_are() {
        let tree = Tree::new("research-pins");
        tree.file("docs/research/snapshot.md", "`src/bridge/gone.rs:12`\n");
        assert!(tree.check().findings.is_empty());
        tree.file("docs/note.md", "`src/bridge/gone.rs:12`\n");
        assert_eq!(tree.check().findings.len(), 1);
    }

    fn main_source() -> String {
        std::fs::read_to_string(crate::repo_root().join("xtask/src/main.rs"))
            .expect("the xtask entry point must be readable")
    }

    #[test]
    fn the_command_is_registered() {
        assert!(
            main_source().contains("\"check-doc-links\" => doc_links::run()"),
            "the dispatch arm must exist"
        );
    }

    /// The usage line and the unknown-task line are the two help strings; the
    /// third occurrence is the dispatch arm itself.
    #[test]
    fn the_help_texts_list_the_command() {
        assert!(
            main_source().matches("check-doc-links").count() >= 3,
            "both help texts must name the task"
        );
    }

    /// The command must live in the ungated `docs-drift` job, not `Check`:
    /// `Check` is `classify`-gated, so a docs-only PR would never run it.
    #[test]
    fn ci_runs_the_command_in_the_docs_job() {
        let ci = std::fs::read_to_string(crate::repo_root().join(".github/workflows/ci.yml"))
            .expect("CI must be readable");
        let job = ci
            .split("\n  docs-drift:")
            .nth(1)
            .and_then(|rest| rest.split("\n  audit:").next())
            .expect("the docs-drift job must exist");
        assert!(job.contains("cargo xtask check-doc-links"));
    }

    #[test]
    fn the_verification_loop_runs_the_command() {
        let contributing = std::fs::read_to_string(crate::repo_root().join("CONTRIBUTING.md"))
            .expect("CONTRIBUTING.md must be readable");
        assert!(contributing.contains("cargo xtask check-doc-links"));
    }

    #[test]
    fn a_clean_tree_runs_to_success() {
        let tree = Tree::new("exit-clean");
        tree.file("docs/note.md", "ADR-0001\n");
        assert_eq!(run_on(&tree.root), 0);
    }

    #[test]
    fn a_dangling_citation_runs_to_failure() {
        let tree = Tree::new("exit-failure");
        tree.file("docs/note.md", "ADR-9999\n");
        assert_eq!(run_on(&tree.root), 1);
    }

    /// The acceptance criterion's "passes on the tree", with a plausibility
    /// floor so a scan that reads nothing cannot pass vacuously.
    #[test]
    fn the_repository_tree_has_no_dangling_citations() {
        let report = check_tree(&crate::repo_root()).expect("the repository must scan");
        assert!(
            report.checked > 1_000,
            "only {} citations were checked — the scan is not reading the docs",
            report.checked
        );
        assert!(
            report.findings.is_empty(),
            "dangling citations:\n{}",
            report
                .findings
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
