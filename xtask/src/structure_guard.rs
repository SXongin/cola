//! The structure ratchet: `cargo xtask check-structure` (spec #1117, decided in
//! #1109).
//!
//! The code contract's rules are enforced against one committed baseline,
//! `xtask/structure-baseline.json`, with five counters:
//!
//! 1. **Function complexity** — `clippy::cognitive_complexity` (threshold 25)
//!    and `clippy::too_many_lines` (threshold 100) on the production bin
//!    target. Entries are keyed by file + function name + metric, so a rename
//!    or a file move is a move, not a new offender.
//! 2. **File size** — a production file may not cross 2,000 lines for the
//!    first time, and a baselined file may not exceed its recorded size by
//!    more than 10%.
//! 3. **`#[allow(clippy::too_many_arguments)]`** — the 68 existing sites are
//!    grandfathered. A new one must carry a reason and a tracking issue
//!    (`#NNN`) on the attribute itself, or in the comment beside it.
//! 4. **`#[cfg(test)]` production sites** — counted per file; a new one fails.
//!    The test surface (`src/**/tests/**`, `src/bridge/test_support.rs`,
//!    fixtures) is out of scope: it is the test contract's.
//! 5. **`["tag"]` probes** — card JSON may be built and probed only inside
//!    `src/feishu/card`. Outside it, every line is on the baseline's
//!    allow-list; no new line may appear.
//!
//! `--bless` rewrites the baseline from the tree and refuses to raise a value:
//! its first job is to record a removal (or a rename) as a *lower* baseline.
//! Seeding a missing baseline is the one mode that takes the tree as it is.
//! The only way up is a reviewable edit of the JSON in a diff.
//!
//! Thresholds live in `clippy.toml`, so a toolchain bump cannot silently move
//! the bar: clippy reads them, and this guard consumes clippy's findings.
//!
//! Like the other `check-*` guards, a failure to read the tree or run clippy is
//! a loud error — a guard that fails open is no guard.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::source_scan;

/// The committed baseline, relative to the repository root.
pub(crate) const BASELINE_REL: &str = "xtask/structure-baseline.json";

/// A production file may not cross this many lines for the first time.
pub(crate) const FILE_LINES_LIMIT: usize = 2_000;

/// How far a baselined file may grow above its recorded size, in percent.
pub(crate) const FILE_GROWTH_PERCENT: usize = 10;

/// The literal counted by counter 4 and policed by counter 5.
const CFG_TEST: &str = "#[cfg(test)]";
const TAG_PROBE: &str = "[\"tag\"]";

/// The card module owns card JSON; every `["tag"]` probe outside it is a seam
/// leak the baseline allow-lists one line at a time.
const CARD_PREFIX: &str = "src/feishu/card/";

// ---------------------------------------------------------------------------
// The baseline
// ---------------------------------------------------------------------------

/// One clippy metric an entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Metric {
    CognitiveComplexity,
    TooManyLines,
}

impl Metric {
    fn name(self) -> &'static str {
        match self {
            Self::CognitiveComplexity => "cognitive_complexity",
            Self::TooManyLines => "too_many_lines",
        }
    }
}

/// One function offender's score.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct FunctionEntry {
    pub(crate) path: String,
    pub(crate) function: String,
    pub(crate) metric: Metric,
    pub(crate) value: u64,
}

/// One production file's recorded line count (only files over the cap).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct FileEntry {
    pub(crate) path: String,
    pub(crate) lines: usize,
}

/// One grandfathered `#[allow(clippy::too_many_arguments)]`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct AllowEntry {
    pub(crate) path: String,
    pub(crate) function: String,
}

/// One file's `#[cfg(test)]` count.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct CfgTestEntry {
    pub(crate) path: String,
    pub(crate) count: usize,
}

/// One file's allow-listed `["tag"]` probe lines, as trimmed source text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct TagProbeEntry {
    pub(crate) path: String,
    pub(crate) lines: Vec<String>,
}

/// The committed baseline: every value is a ceiling, never a target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Baseline {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) functions: Vec<FunctionEntry>,
    #[serde(default)]
    pub(crate) files: Vec<FileEntry>,
    #[serde(default)]
    pub(crate) too_many_arguments: Vec<AllowEntry>,
    #[serde(default)]
    pub(crate) cfg_test: Vec<CfgTestEntry>,
    #[serde(default)]
    pub(crate) tag_probes: Vec<TagProbeEntry>,
}

impl Baseline {
    /// The baseline this scan records. `previous` is the committed baseline,
    /// or `None` when seeding a missing file.
    ///
    /// Every rule violation refuses the bless — an upward value is never
    /// written. A file's recorded size is additionally clamped to the previous
    /// one, so growth inside the 10% headroom is not baked in as the new floor.
    pub(crate) fn next(scan: &Scan, previous: Option<&Baseline>) -> Result<Self, Vec<String>> {
        if let Some(previous) = previous {
            let violations = check(scan, previous);
            if !violations.is_empty() {
                return Err(violations);
            }
        }

        let mut next = Self {
            version: 1,
            functions: scan.function_entries(),
            files: scan
                .files
                .iter()
                .filter(|file| file.lines > FILE_LINES_LIMIT)
                .map(|file| FileEntry {
                    path: file.path.clone(),
                    lines: file.lines,
                })
                .collect(),
            too_many_arguments: scan.allow_entries(),
            cfg_test: scan.cfg_test_entries(),
            tag_probes: scan.tag_probe_entries(),
        };
        if let Some(previous) = previous {
            for file in &mut next.files {
                if let Some(old) = previous.files.iter().find(|old| old.path == file.path) {
                    file.lines = file.lines.min(old.lines);
                }
            }
            next.files.retain(|file| file.lines > FILE_LINES_LIMIT);
        }
        next.sort();
        Ok(next)
    }

    fn sort(&mut self) {
        self.functions.sort();
        self.files.sort();
        self.too_many_arguments.sort();
        self.cfg_test.sort();
        self.tag_probes.sort();
    }
}

// ---------------------------------------------------------------------------
// The scan
// ---------------------------------------------------------------------------

/// A function offender at its current location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FunctionScore {
    pub(crate) path: String,
    pub(crate) function: String,
    pub(crate) metric: Metric,
    pub(crate) value: u64,
    /// 1-based, for the violation message only.
    pub(crate) line: usize,
}

/// A production file's production-line count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileSize {
    pub(crate) path: String,
    pub(crate) lines: usize,
}

/// One `#[allow(clippy::too_many_arguments)]` site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AllowSite {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) function: String,
    /// A reason plus a tracking issue, see [`rationale_is_tracked`].
    pub(crate) tracked: bool,
}

/// One production file's `#[cfg(test)]` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CfgTestSites {
    pub(crate) path: String,
    pub(crate) lines: Vec<usize>,
}

/// One `["tag"]` probe outside the card module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TagProbe {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) text: String,
}

/// Everything the five counters measure on one tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) functions: Vec<FunctionScore>,
    pub(crate) files: Vec<FileSize>,
    pub(crate) allows: Vec<AllowSite>,
    pub(crate) cfg_test: Vec<CfgTestSites>,
    pub(crate) tag_probes: Vec<TagProbe>,
}

impl Scan {
    fn function_entries(&self) -> Vec<FunctionEntry> {
        self.functions
            .iter()
            .map(|score| FunctionEntry {
                path: score.path.clone(),
                function: score.function.clone(),
                metric: score.metric,
                value: score.value,
            })
            .collect()
    }

    fn allow_entries(&self) -> Vec<AllowEntry> {
        let mut entries: Vec<AllowEntry> = self
            .allows
            .iter()
            .map(|site| AllowEntry {
                path: site.path.clone(),
                function: site.function.clone(),
            })
            .collect();
        entries.sort();
        entries.dedup();
        entries
    }

    fn cfg_test_entries(&self) -> Vec<CfgTestEntry> {
        self.cfg_test
            .iter()
            .filter(|file| !file.lines.is_empty())
            .map(|file| CfgTestEntry {
                path: file.path.clone(),
                count: file.lines.len(),
            })
            .collect()
    }

    fn tag_probe_entries(&self) -> Vec<TagProbeEntry> {
        let mut by_path: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for probe in &self.tag_probes {
            by_path
                .entry(probe.path.clone())
                .or_default()
                .push(probe.text.clone());
        }
        by_path
            .into_iter()
            .map(|(path, mut lines)| {
                lines.sort();
                TagProbeEntry { path, lines }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------

/// Every violation of the contract's five counters, one line each, in a stable
/// order (functions, files, allows, `cfg(test)`, tag probes).
pub(crate) fn check(scan: &Scan, baseline: &Baseline) -> Vec<String> {
    let mut violations = function_violations(scan, baseline);
    violations.extend(file_violations(scan, baseline));
    violations.extend(allow_violations(scan, baseline));
    violations.extend(cfg_test_violations(scan, baseline));
    violations.extend(tag_probe_violations(scan, baseline));
    violations
}

fn function_violations(scan: &Scan, baseline: &Baseline) -> Vec<String> {
    let mut existing: Vec<&FunctionEntry> = baseline.functions.iter().collect();
    let mut appeared: Vec<&FunctionScore> = Vec::new();
    let mut violations = Vec::new();

    for score in &scan.functions {
        let found = existing.iter().position(|entry| {
            entry.path == score.path && entry.function == score.function && entry.metric == score.metric
        });
        match found {
            Some(index) => {
                let entry = existing.remove(index);
                if score.value > entry.value {
                    violations.push(format!(
                        "{}:{}: {} — {} {} is worse than the baseline's {}",
                        score.path,
                        score.line,
                        score.function,
                        score.metric.name(),
                        score.value,
                        entry.value
                    ));
                }
            }
            None => appeared.push(score),
        }
    }

    // A rename or a file move keeps the function's body, so its score is
    // unchanged: pair an appeared entry with exactly one vanished entry of the
    // same metric and value and record it as a move, not a new offender.
    for score in appeared {
        let candidates: Vec<usize> = existing
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.metric == score.metric && entry.value == score.value)
            .map(|(index, _)| index)
            .collect();
        if candidates.len() == 1 {
            existing.remove(candidates[0]);
        } else {
            violations.push(format!(
                "{}:{}: {} — {} {} is a new offender",
                score.path,
                score.line,
                score.function,
                score.metric.name(),
                score.value
            ));
        }
    }
    violations
}

fn file_violations(scan: &Scan, baseline: &Baseline) -> Vec<String> {
    let mut violations = Vec::new();
    for file in &scan.files {
        match baseline.files.iter().find(|entry| entry.path == file.path) {
            Some(entry) => {
                let ceiling = entry.lines + entry.lines * FILE_GROWTH_PERCENT / 100;
                if file.lines > ceiling {
                    violations.push(format!(
                        "{} (file_lines): {} production lines exceeds the baselined {} by more \
                         than {}%",
                        file.path, file.lines, entry.lines, FILE_GROWTH_PERCENT
                    ));
                }
            }
            None if file.lines > FILE_LINES_LIMIT => {
                violations.push(format!(
                    "{} (file_lines): {} production lines crosses the {}-line cap for a new file",
                    file.path, file.lines, FILE_LINES_LIMIT
                ));
            }
            None => {}
        }
    }
    violations
}

fn allow_violations(scan: &Scan, baseline: &Baseline) -> Vec<String> {
    let mut violations = Vec::new();
    for site in &scan.allows {
        let known = baseline
            .too_many_arguments
            .iter()
            .any(|entry| entry.path == site.path && entry.function == site.function);
        if !known && !site.tracked {
            violations.push(format!(
                "{}:{}: new #[allow(clippy::too_many_arguments)] without a reason and a tracking \
                 issue — write `reason = \"… #NNN\"` in the attribute or a `// … #NNN` comment \
                 beside it",
                site.path, site.line
            ));
        }
    }
    violations
}

fn cfg_test_violations(scan: &Scan, baseline: &Baseline) -> Vec<String> {
    let mut violations = Vec::new();
    for file in &scan.cfg_test {
        let allowed = baseline
            .cfg_test
            .iter()
            .find(|entry| entry.path == file.path)
            .map_or(0, |entry| entry.count);
        for line in file.lines.iter().skip(allowed) {
            violations.push(format!(
                "{}:{}: new #[cfg(test)] production site ({} in the file, baseline {})",
                file.path,
                line,
                file.lines.len(),
                allowed
            ));
        }
    }
    violations
}

fn tag_probe_violations(scan: &Scan, baseline: &Baseline) -> Vec<String> {
    let mut violations = Vec::new();
    for probe in &scan.tag_probes {
        let allowed = baseline
            .tag_probes
            .iter()
            .find(|entry| entry.path == probe.path)
            .map(|entry| &entry.lines);
        let listed = allowed.is_some_and(|lines| {
            lines.iter().filter(|line| **line == probe.text).count()
                > scan
                    .tag_probes
                    .iter()
                    .filter(|other| {
                        other.path == probe.path && other.text == probe.text && other.line < probe.line
                    })
                    .count()
        });
        if !listed {
            violations.push(format!(
                "{}:{}: new `[\"tag\"]` probe outside src/feishu/card — build and probe card JSON \
                 through the card module, or allow-list this line in {BASELINE_REL}",
                probe.path, probe.line
            ));
        }
    }
    violations
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

/// `cargo xtask check-structure [--bless]`.
pub(crate) fn cli() {
    let mut bless = false;
    for arg in std::env::args().skip(2) {
        match arg.as_str() {
            "--bless" => bless = true,
            other => {
                eprintln!("unknown argument: {other}\nusage: cargo xtask check-structure [--bless]");
                std::process::exit(1);
            }
        }
    }

    let root = crate::repo_root();
    let baseline_path = root.join(BASELINE_REL);
    let previous = match std::fs::read_to_string(&baseline_path) {
        Ok(text) => match serde_json::from_str::<Baseline>(&text) {
            Ok(baseline) => Some(baseline),
            Err(error) => {
                eprintln!("error: {BASELINE_REL} is not a valid baseline: {error}");
                std::process::exit(1);
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            eprintln!("error: cannot read {BASELINE_REL}: {error}");
            std::process::exit(1);
        }
    };

    let scan = match scan_tree(&root) {
        Ok(scan) => scan,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    if bless {
        match Baseline::next(&scan, previous.as_ref()) {
            Ok(next) => {
                let json = serde_json::to_string_pretty(&next)
                    .map(|json| format!("{json}\n"))
                    .unwrap_or_else(|error| {
                        eprintln!("error: cannot serialise the baseline: {error}");
                        std::process::exit(1);
                    });
                if let Err(error) = std::fs::write(&baseline_path, json) {
                    eprintln!("error: cannot write {BASELINE_REL}: {error}");
                    std::process::exit(1);
                }
                eprintln!(
                    "check-structure: blessed {} function offender(s), {} file(s) over the cap, \
                     {} allow(s), {} cfg(test) site(s), {} tag probe(s)",
                    next.functions.len(),
                    next.files.len(),
                    next.too_many_arguments.len(),
                    next.cfg_test.iter().map(|entry| entry.count).sum::<usize>(),
                    next.tag_probes
                        .iter()
                        .map(|entry| entry.lines.len())
                        .sum::<usize>(),
                );
            }
            Err(violations) => {
                for violation in &violations {
                    eprintln!("{violation}");
                }
                eprintln!(
                    "error: refusing --bless: {} violation(s). A bless never raises a value; \
                     lower the tree, or edit {BASELINE_REL} in the diff",
                    violations.len()
                );
                std::process::exit(1);
            }
        }
        return;
    }

    let Some(previous) = previous else {
        eprintln!("error: {BASELINE_REL} is missing — run `cargo xtask check-structure --bless` to seed it");
        std::process::exit(1);
    };
    let violations = check(&scan, &previous);
    if violations.is_empty() {
        return;
    }
    for violation in &violations {
        eprintln!("{violation}");
    }
    eprintln!(
        "error: {} structure violation(s) — the ratchet only moves down. Fix the offender, or \
         re-bless a removal with `cargo xtask check-structure --bless`",
        violations.len()
    );
    std::process::exit(1);
}

/// Every counter on one tree: clippy for the functions, the sources for the
/// rest.
pub(crate) fn scan_tree(root: &Path) -> Result<Scan, String> {
    let mut scan = scan_sources(root)?;
    scan.functions = scan_functions(root)?;
    Ok(scan)
}

// ---------------------------------------------------------------------------
// The scan: clippy
// ---------------------------------------------------------------------------

/// Run clippy on the production bin target and read its two complexity lints.
fn scan_functions(root: &Path) -> Result<Vec<FunctionScore>, String> {
    let output = Command::new("cargo")
        .current_dir(root)
        .args([
            "clippy",
            "-p",
            "colark",
            "--bin",
            "cola",
            "--message-format=json",
            "--",
            "-W",
            "clippy::cognitive_complexity",
            "-W",
            "clippy::too_many_lines",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("error: cannot run clippy: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "error: clippy exited {} while measuring the structure ratchet",
            output.status
        ));
    }
    let json = String::from_utf8_lossy(&output.stdout);
    let diagnostics = parse_clippy(&json)?;
    let mut scores = Vec::with_capacity(diagnostics.len());
    for diagnostic in diagnostics {
        let content = std::fs::read_to_string(root.join(&diagnostic.path)).map_err(|error| {
            format!(
                "error: cannot read {} to name a clippy finding: {error}",
                diagnostic.path
            )
        })?;
        scores.push(FunctionScore {
            function: function_name_at(&content, diagnostic.line),
            path: diagnostic.path,
            metric: diagnostic.metric,
            value: diagnostic.value,
            line: diagnostic.line,
        });
    }
    Ok(scores)
}

/// One complexity finding, before its function name is resolved against the
/// file (clippy's span starts at the function's declaration line).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RawDiagnostic {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) metric: Metric,
    pub(crate) value: u64,
}

/// Parse `cargo clippy --message-format=json` output.
///
/// Only the two complexity lints are taken; every other diagnostic is
/// clippy's regular job. A line that is not a compiler message is build
/// output, not an error.
pub(crate) fn parse_clippy(json: &str) -> Result<Vec<RawDiagnostic>, String> {
    let mut diagnostics = Vec::new();
    for line in json.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if record["reason"] != "compiler-message" {
            continue;
        }
        let message = &record["message"];
        let code = message["code"]["code"].as_str().unwrap_or_default();
        let metric = match code {
            "clippy::cognitive_complexity" => Metric::CognitiveComplexity,
            "clippy::too_many_lines" => Metric::TooManyLines,
            _ => continue,
        };
        let Some(span) = message["spans"]
            .as_array()
            .and_then(|spans| spans.iter().find(|span| span["is_primary"] == true))
        else {
            continue;
        };
        let path = span["file_name"].as_str().unwrap_or_default().replace('\\', "/");
        let Some(line) = span["line_start"].as_u64() else {
            continue;
        };
        let text = message["message"].as_str().unwrap_or_default();
        let Some(value) = first_integer(text) else {
            return Err(format!(
                "error: cannot read the score from a {code} diagnostic: {text}"
            ));
        };
        diagnostics.push(RawDiagnostic {
            path,
            line: line as usize,
            metric,
            value,
        });
    }
    Ok(diagnostics)
}

/// The first run of ASCII digits in `text` — the score in both lints'
/// messages (`… complexity of (35/25)`, `too many lines (168/100)`).
fn first_integer(text: &str) -> Option<u64> {
    let digits: String = text
        .chars()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// The name of the function a clippy span at `line` belongs to.
///
/// A span normally starts at the `fn` declaration. `async_trait` re-parents
/// the generated methods' spans onto the `#[async_trait]` attribute, where the
/// function name does not exist; those entries are named after the `impl` the
/// attribute decorates, which is unique per file and stable across edits.
pub(crate) fn function_name_at(content: &str, line: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = line.saturating_sub(1).min(lines.len());
    let first = lines.get(start).map_or("", |text| text.trim());
    if first.starts_with('#') {
        for text in lines.iter().skip(start).take(60) {
            let text = text.trim();
            if text.starts_with("impl ") || text.starts_with("impl<") {
                return text.split('{').next().unwrap_or(text).trim().to_string();
            }
        }
    }
    for text in lines.iter().skip(start).take(60) {
        if let Some(name) = fn_name(text) {
            return name.to_string();
        }
    }
    "<anonymous>".to_string()
}

fn fn_name(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if line.starts_with("//") {
        return None;
    }
    let mut search = line;
    while let Some(offset) = search.find("fn ") {
        let before = &search[..offset];
        let whole_word = before
            .chars()
            .next_back()
            .is_none_or(|ch| !is_identifier_continue(ch));
        let after = &search[offset + 3..];
        if whole_word {
            let name_len = after
                .find(|ch: char| !is_identifier_continue(ch))
                .unwrap_or(after.len());
            if name_len > 0
                && after
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_alphabetic() || ch == '_')
            {
                return Some(&after[..name_len]);
            }
        }
        search = &search[offset + 3..];
    }
    None
}

// ---------------------------------------------------------------------------
// The scan: the text counters
// ---------------------------------------------------------------------------

/// Every production source, read once and measured by counters 2–5.
fn scan_sources(root: &Path) -> Result<Scan, String> {
    let files = source_scan::read_sources(&root.join("src"), "structure")?;
    let mut scan = Scan::default();
    for file in &files {
        let masked = masked(&file.content);
        // A signature-cap silencer is a silencer in a test too: counter 3 sees
        // the whole tree, test surface included.
        scan.allows
            .extend(allow_sites(&file.path, &file.content, &masked));
        scan.tag_probes.extend(tag_probes(&file.path, &file.content));
        if !is_production_path(&file.path) {
            continue;
        }
        scan.files.push(FileSize {
            path: file.path.clone(),
            lines: production_lines_masked(&file.content, &masked),
        });
        if let Some(sites) = cfg_test_sites(&file.path, &masked) {
            scan.cfg_test.push(sites);
        }
    }
    disambiguate_allows(&mut scan.allows);
    scan.files.sort_by(|left, right| left.path.cmp(&right.path));
    scan.functions.sort_by(|left, right| {
        (&left.path, &left.function, left.metric).cmp(&(&right.path, &right.function, right.metric))
    });
    Ok(scan)
}

/// Two allows can annotate same-named functions in one file (two `impl` blocks'
/// `new`). The baseline keys an allow by file + function, so the second one
/// keeps a stable `#2` suffix instead of vanishing into the first.
fn disambiguate_allows(sites: &mut [AllowSite]) {
    sites.sort_by(|left, right| (&left.path, left.line).cmp(&(&right.path, right.line)));
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    for site in sites.iter_mut() {
        let count = seen
            .entry((site.path.clone(), site.function.clone()))
            .or_insert(0);
        *count += 1;
        if *count > 1 {
            site.function = format!("{}#{}", site.function, count);
        }
    }
}

/// The test surface is the test contract's: integration-test modules, the
/// shared harness, and the fixtures a `rustc` invocation compiles.
fn is_production_path(path: &str) -> bool {
    !(path.contains("/tests/") || path.ends_with("/test_support.rs") || path.contains("/fixtures/"))
}

/// A file's production lines: everything outside a `#[cfg(test)]` item.
///
/// The item's extent comes from the masked source (comments and literals
/// blanked), so a brace inside a fixture string cannot stretch the range and a
/// doc comment *mentioning* `#[cfg(test)]` is not a site at all.
#[cfg(test)]
pub(crate) fn production_lines(content: &str) -> usize {
    production_lines_masked(content, &masked(content))
}

fn production_lines_masked(content: &str, masked: &[u8]) -> usize {
    let total = content.lines().count();
    let starts = line_starts(masked);
    let mut excluded = 0usize;
    let mut index = 0usize;
    while index < starts.len() {
        let start = starts[index];
        let end_of_line = starts.get(index + 1).copied().unwrap_or(masked.len());
        let text = trim(&masked[start..end_of_line]);
        if text != CFG_TEST.as_bytes() {
            index += 1;
            continue;
        }
        let end = item_end(masked, end_of_line);
        let after = line_index(&starts, end);
        excluded += after.saturating_sub(index);
        index = after.max(index + 1);
    }
    total.saturating_sub(excluded)
}

/// The `#[cfg(test)]` lines of one production file, 1-based.
fn cfg_test_sites(path: &str, masked: &[u8]) -> Option<CfgTestSites> {
    let starts = line_starts(masked);
    let lines: Vec<usize> = starts
        .iter()
        .enumerate()
        .filter_map(|(index, start)| {
            let end_of_line = starts.get(index + 1).copied().unwrap_or(masked.len());
            (trim(&masked[*start..end_of_line]) == CFG_TEST.as_bytes()).then_some(index + 1)
        })
        .collect();
    (!lines.is_empty()).then(|| CfgTestSites {
        path: path.to_string(),
        lines,
    })
}

/// Every `#[allow(clippy::too_many_arguments)]` in one file.
fn allow_sites(path: &str, content: &str, masked: &[u8]) -> Vec<AllowSite> {
    const LINT: &str = "clippy::too_many_arguments";
    let mut sites = Vec::new();
    let mut search = 0usize;
    while let Some(offset) = find(&masked[search..], LINT.as_bytes()) {
        let at = search + offset;
        search = at + LINT.len();
        let Some(open) = rfind(&masked[..at], b"#[") else {
            continue;
        };
        if masked[open..at].contains(&b']') {
            continue;
        }
        let Some(close) = find(&masked[at..], b"]").map(|end| at + end) else {
            continue;
        };
        let attribute = &content[open..=close];
        let line = line_index(&line_starts(masked), open);
        let trailing = content[close + 1..].split('\n').next().unwrap_or_default().trim();
        let trailing = trailing.strip_prefix("//").unwrap_or_default();
        let above = preceding_comments(content, open);
        let tracked = rationale_is_tracked(&format!("{} {trailing} {above}", attribute_reason(attribute)));
        sites.push(AllowSite {
            path: path.to_string(),
            line,
            function: function_name_at(content, line),
            tracked,
        });
    }
    sites
}

/// The contiguous `//` comment lines immediately above `at`, if any.
fn preceding_comments(content: &str, at: usize) -> String {
    let lines: Vec<&str> = content[..at].lines().collect();
    let mut comments = Vec::new();
    for line in lines.iter().rev() {
        let text = line.trim();
        if text.starts_with("//") {
            comments.push(text.trim_start_matches('/').trim());
        } else {
            break;
        }
    }
    comments.reverse();
    comments.join(" ")
}

/// The `reason = "…"` text of an `#[allow(…)]`, when it carries one.
fn attribute_reason(attribute: &str) -> &str {
    let Some(start) = attribute.find("reason") else {
        return "";
    };
    let rest = &attribute[start + "reason".len()..];
    let rest = rest.trim_start().strip_prefix('=').unwrap_or(rest);
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix('"') else {
        return "";
    };
    let end = rest.find('"').unwrap_or(rest.len());
    &rest[..end]
}

/// A rationale names a tracking issue (`#123`, `#1013`) and says why in
/// something more than the issue number.
pub(crate) fn rationale_is_tracked(text: &str) -> bool {
    let mut chars = text.chars().peekable();
    let mut issue = false;
    while let Some(ch) = chars.next() {
        if ch == '#' && chars.peek().is_some_and(|next| next.is_ascii_digit()) {
            issue = true;
            while chars.peek().is_some_and(|next| next.is_ascii_digit()) {
                chars.next();
            }
        }
    }
    let prose: String = text
        .split(|ch: char| ch == '#' || !ch.is_alphanumeric())
        .filter(|word| !word.chars().all(|ch| ch.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join("");
    issue && prose.chars().count() >= 10
}

/// Every `["tag"]` probe outside the card module.
///
/// A probe is code: a doc line spelling `["tag"]` in prose is not one. (A
/// probe assembled inside a string literal still counts — the guard is a text
/// guard and errs toward the allow-list, never toward silence.)
fn tag_probes(path: &str, content: &str) -> Vec<TagProbe> {
    if path.starts_with(CARD_PREFIX) {
        return Vec::new();
    }
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| line.contains(TAG_PROBE))
        .map(|(index, line)| TagProbe {
            path: path.to_string(),
            line: index + 1,
            text: line.trim().to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Structural scanning helpers
// ---------------------------------------------------------------------------

/// The byte offsets at which each line starts.
fn line_starts(text: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in text.iter().enumerate() {
        if *byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// The 1-based line containing `offset`.
fn line_index(starts: &[usize], offset: usize) -> usize {
    starts.partition_point(|start| *start <= offset).max(1)
}

/// `source` with comment and literal text blanked to spaces, newlines kept.
///
/// Byte positions and line numbers survive the blanking, so the masked copy is
/// only ever used for structure (braces, item ends) — never as text.
fn masked(source: &str) -> Vec<u8> {
    let src = source.as_bytes();
    let mut out = src.to_vec();
    let mut index = 0usize;
    while index < src.len() {
        match src[index] {
            b'/' if src.get(index + 1) == Some(&b'/') => {
                let start = index;
                while index < src.len() && src[index] != b'\n' {
                    index += 1;
                }
                blank(&mut out, start, index);
            }
            b'/' if src.get(index + 1) == Some(&b'*') => {
                let start = index;
                let mut depth = 1usize;
                index += 2;
                while index < src.len() && depth > 0 {
                    if src[index] == b'/' && src.get(index + 1) == Some(&b'*') {
                        depth += 1;
                        index += 2;
                    } else if src[index] == b'*' && src.get(index + 1) == Some(&b'/') {
                        depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                blank(&mut out, start, index);
            }
            b'"' => {
                let end = string_end(src, index);
                if end > index + 1 {
                    blank(&mut out, index + 1, end - 1);
                }
                index = end;
            }
            b'\'' => {
                if let Some(end) = char_literal_end(src, index) {
                    if end > index + 1 {
                        blank(&mut out, index + 1, end - 1);
                    }
                    index = end;
                } else {
                    index += 1;
                }
            }
            _ => {
                if let Some((content_start, content_end, end)) = raw_string(src, index) {
                    blank(&mut out, content_start, content_end);
                    index = end;
                } else {
                    index += 1;
                }
            }
        }
    }
    out
}

fn blank(out: &mut [u8], start: usize, end: usize) {
    let len = out.len();
    let (start, end) = (start.min(len), end.min(len));
    for byte in &mut out[start..end] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

fn trim(mut text: &[u8]) -> &[u8] {
    while text.first().is_some_and(u8::is_ascii_whitespace) {
        text = &text[1..];
    }
    while text.last().is_some_and(u8::is_ascii_whitespace) {
        text = &text[..text.len() - 1];
    }
    text
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// The byte just past the item introduced at `at` (after its attributes).
fn item_end(masked: &[u8], at: usize) -> usize {
    let mut index = at;
    loop {
        while index < masked.len() && masked[index].is_ascii_whitespace() {
            index += 1;
        }
        if masked.get(index) == Some(&b'#') {
            let mut open = index + 1;
            if masked.get(open) == Some(&b'!') {
                open += 1;
            }
            if masked.get(open) == Some(&b'[') {
                let mut depth = 0usize;
                while open < masked.len() {
                    match masked[open] {
                        b'[' => depth += 1,
                        b']' => {
                            depth -= 1;
                            if depth == 0 {
                                open += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    open += 1;
                }
                index = open;
                continue;
            }
        }
        break;
    }

    let mut depth = 0usize;
    while index < masked.len() {
        match masked[index] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b'{' if depth == 0 => return brace_end(masked, index),
            b';' | b',' if depth == 0 => return index + 1,
            _ => {}
        }
        index += 1;
    }
    masked.len()
}

fn brace_end(masked: &[u8], at: usize) -> usize {
    let mut depth = 0usize;
    let mut index = at;
    while index < masked.len() {
        match masked[index] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    masked.len()
}

fn string_end(src: &[u8], at: usize) -> usize {
    let mut index = at + 1;
    while index < src.len() {
        match src[index] {
            b'\\' => index += 2,
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    index.min(src.len())
}

fn char_literal_end(src: &[u8], at: usize) -> Option<usize> {
    let first = *src.get(at + 1)?;
    if first == b'\'' {
        return None;
    }
    let body_end = if first != b'\\' {
        at + 1 + char_width(src, at + 1)
    } else {
        match *src.get(at + 2)? {
            b'x' => at + 5,
            b'u' => {
                let close = src[at + 3..].iter().position(|byte| *byte == b'}')?;
                at + 3 + close + 1
            }
            _ => at + 2 + char_width(src, at + 2),
        }
    };
    (src.get(body_end) == Some(&b'\'')).then_some(body_end + 1)
}

/// The UTF-8 width of the char starting at `at`.
fn char_width(src: &[u8], at: usize) -> usize {
    let byte = src.get(at).copied().unwrap_or_default();
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// A raw string (`r"…"`, `r#"…"#`) at `at`: `(content_start, content_end, end)`.
fn raw_string(src: &[u8], at: usize) -> Option<(usize, usize, usize)> {
    let mut index = at;
    match src.get(index)? {
        b'r' => {}
        b'b' | b'c' if src.get(index + 1) == Some(&b'r') => index += 1,
        _ => return None,
    }
    if at > 0 && is_identifier_byte(src[at - 1]) {
        return None;
    }
    if src.get(index) != Some(&b'r') {
        return None;
    }
    index += 1;
    let mut hashes = 0usize;
    while src.get(index) == Some(&b'#') {
        hashes += 1;
        index += 1;
    }
    if src.get(index) != Some(&b'"') {
        return None;
    }
    let content_start = index + 1;
    let mut cursor = content_start;
    while cursor < src.len() {
        if src[cursor] == b'"' {
            let mut closing = cursor + 1;
            let mut counted = 0usize;
            while counted < hashes && src.get(closing) == Some(&b'#') {
                counted += 1;
                closing += 1;
            }
            if counted == hashes {
                return Some((content_start, cursor, closing));
            }
        }
        cursor += 1;
    }
    None
}

fn is_identifier_continue(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> Baseline {
        Baseline {
            version: 1,
            ..Baseline::default()
        }
    }

    fn score(path: &str, function: &str, metric: Metric, value: u64) -> FunctionScore {
        FunctionScore {
            path: path.to_string(),
            function: function.to_string(),
            metric,
            value,
            line: 7,
        }
    }

    fn fn_entry(path: &str, function: &str, metric: Metric, value: u64) -> FunctionEntry {
        FunctionEntry {
            path: path.to_string(),
            function: function.to_string(),
            metric,
            value,
        }
    }

    /// The functions counter: a key that is not in the baseline is a new
    /// offender, whatever its score.
    #[test]
    fn a_new_offender_fails_the_check() {
        let mut scan = Scan::default();
        scan.functions
            .push(score("src/bridge/x.rs", "big", Metric::TooManyLines, 120));
        let violations = check(&scan, &baseline());
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("src/bridge/x.rs:7: big"));
        assert!(violations[0].contains("new offender"), "{violations:?}");
    }

    /// A known function that grows past its recorded score fails; at or under
    /// the recorded score passes (the ratchet only measures worse).
    #[test]
    fn a_worse_score_fails_the_check() {
        let mut base = baseline();
        base.functions
            .push(fn_entry("src/bridge/x.rs", "big", Metric::TooManyLines, 120));

        let mut scan = Scan::default();
        scan.functions
            .push(score("src/bridge/x.rs", "big", Metric::TooManyLines, 121));
        let violations = check(&scan, &base);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("is worse than the baseline's 120"),
            "{violations:?}"
        );

        for value in [120, 90] {
            scan.functions[0].value = value;
            assert!(check(&scan, &base).is_empty(), "{value} is not worse");
        }
    }

    /// A rename keeps the body, so its score is unchanged: it is a move, not a
    /// new offender. A rename that also grew is still a new offender.
    #[test]
    fn a_rename_is_a_move_not_a_new_offender() {
        let mut base = baseline();
        base.functions
            .push(fn_entry("src/bridge/x.rs", "old_name", Metric::TooManyLines, 120));

        let mut scan = Scan::default();
        scan.functions
            .push(score("src/bridge/x.rs", "new_name", Metric::TooManyLines, 120));
        assert!(check(&scan, &base).is_empty(), "{:?}", check(&scan, &base));

        scan.functions[0].value = 140;
        assert_eq!(check(&scan, &base).len(), 1, "a rename that grew is new growth");
    }

    /// Moving a function to another file is the same move as a rename.
    #[test]
    fn a_file_move_is_a_move_not_a_new_offender() {
        let mut base = baseline();
        base.functions
            .push(fn_entry("src/bridge/x.rs", "big", Metric::TooManyLines, 120));
        let mut scan = Scan::default();
        scan.functions
            .push(score("src/bridge/y.rs", "big", Metric::TooManyLines, 120));
        assert!(check(&scan, &base).is_empty());
    }

    /// A new `too_many_arguments` allow needs a reason and a tracking issue;
    /// a grandfathered one does not, and removing one is never a violation.
    #[test]
    fn a_new_allow_must_carry_a_reason_and_a_tracking_issue() {
        let site = |tracked| AllowSite {
            path: "src/bridge/x.rs".to_string(),
            line: 42,
            function: "wide".to_string(),
            tracked,
        };
        let mut scan = Scan::default();
        scan.allows.push(site(false));
        let violations = check(&scan, &baseline());
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("src/bridge/x.rs:42"), "{violations:?}");

        scan.allows[0].tracked = true;
        assert!(check(&scan, &baseline()).is_empty());

        let mut base = baseline();
        base.too_many_arguments.push(AllowEntry {
            path: "src/bridge/x.rs".to_string(),
            function: "wide".to_string(),
        });
        scan.allows[0].tracked = false;
        assert!(
            check(&scan, &base).is_empty(),
            "a baselined allow is grandfathered"
        );
    }

    /// The rationale check is the rule's phrasing, not a vibe: an issue ref on
    /// its own is not a reason, and prose without one does not track.
    #[test]
    fn a_rationale_needs_both_prose_and_an_issue() {
        assert!(rationale_is_tracked("the trait's whole context #1013"));
        assert!(rationale_is_tracked("the click context #1"));
        assert!(!rationale_is_tracked("the trait's whole context"));
        assert!(!rationale_is_tracked("#1013"));
        assert!(!rationale_is_tracked("fix #1013"));
    }

    /// Both spellings of the rationale are read: the trailing comment and
    /// `reason = "…"`, and the annotated function is the one below.
    #[test]
    fn allow_sites_read_the_reason_and_the_tracking_issue() {
        let content = r#"#[allow(clippy::too_many_arguments, reason = "the click context #1013")]
fn a(x: u8) {}

#[allow(clippy::too_many_arguments)] // the dispatcher carries the request facts #1013
fn b(x: u8) {}

#[allow(clippy::too_many_arguments)]
fn c(x: u8) {}
"#;
        let masked = masked(content);
        let sites = allow_sites("src/bridge/x.rs", content, &masked);
        let tracked: Vec<bool> = sites.iter().map(|site| site.tracked).collect();
        assert_eq!(tracked, vec![true, true, false], "{sites:?}");
        let names: Vec<&str> = sites.iter().map(|site| site.function.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    /// A `["tag"]` probe outside the card module must be on the baseline's
    /// allow-list; a new line fails, even beside an allowed one.
    #[test]
    fn a_new_tag_probe_fails_the_check() {
        let probe = |line, text: &str| TagProbe {
            path: "src/bridge/x.rs".to_string(),
            line,
            text: text.to_string(),
        };
        let mut base = baseline();
        base.tag_probes.push(TagProbeEntry {
            path: "src/bridge/x.rs".to_string(),
            lines: vec!["element[\"tag\"] == \"img\"".to_string()],
        });

        let mut scan = Scan::default();
        scan.tag_probes.push(probe(10, "element[\"tag\"] == \"img\""));
        assert!(check(&scan, &base).is_empty());

        scan.tag_probes.push(probe(20, "panel[\"tag\"] == \"hr\""));
        let violations = check(&scan, &base);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("src/bridge/x.rs:20"), "{violations:?}");
    }

    /// The card module owns the probes; the guard does not look inside it.
    #[test]
    fn tag_probes_inside_the_card_module_are_free() {
        let content = "assert_eq!(element[\"tag\"], \"markdown\");\n";
        assert!(tag_probes("src/feishu/card/session.rs", content).is_empty());
        assert_eq!(tag_probes("src/bridge/x.rs", content).len(), 1);
    }

    /// Prose is not a probe.
    #[test]
    fn a_doc_comment_spelling_a_tag_probe_is_not_a_probe() {
        let content = "/// Probes `element[\"tag\"]` indirectly.\nlet tag = element[\"tag\"];\n";
        assert_eq!(tag_probes("src/bridge/x.rs", content).len(), 1);
    }

    /// File size: a new file may not cross the cap, and a baselined file gets
    /// exactly the 10% headroom the contract grants.
    #[test]
    fn file_size_is_a_cap_for_new_files_and_headroom_for_baselined_ones() {
        let file = |lines| FileSize {
            path: "src/bridge/x.rs".to_string(),
            lines,
        };
        let mut scan = Scan::default();

        scan.files.push(file(FILE_LINES_LIMIT));
        assert!(check(&scan, &baseline()).is_empty(), "at the cap is allowed");
        scan.files[0].lines = FILE_LINES_LIMIT + 1;
        assert_eq!(check(&scan, &baseline()).len(), 1, "over the cap fails");

        let mut base = baseline();
        base.files.push(FileEntry {
            path: "src/bridge/x.rs".to_string(),
            lines: 10_000,
        });
        scan.files[0].lines = 11_000;
        assert!(check(&scan, &base).is_empty(), "10% is the headroom");
        scan.files[0].lines = 11_001;
        assert_eq!(check(&scan, &base).len(), 1, "past the headroom fails");
    }

    /// `cfg(test)` is counted per file: one more than the baseline fails, and
    /// the violation names the new site.
    #[test]
    fn a_new_cfg_test_site_fails_the_check() {
        let mut base = baseline();
        base.cfg_test.push(CfgTestEntry {
            path: "src/bridge/x.rs".to_string(),
            count: 2,
        });

        let mut scan = Scan::default();
        scan.cfg_test.push(CfgTestSites {
            path: "src/bridge/x.rs".to_string(),
            lines: vec![10, 20, 30],
        });
        let violations = check(&scan, &base);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("src/bridge/x.rs:30"), "{violations:?}");

        scan.cfg_test[0].lines.pop();
        assert!(check(&scan, &base).is_empty());
    }

    /// `--bless` records a lower score and a removal; it refuses a worse one.
    #[test]
    fn blessing_lowers_the_baseline_and_refuses_a_worse_one() {
        let mut base = baseline();
        base.functions
            .push(fn_entry("src/bridge/x.rs", "big", Metric::TooManyLines, 120));

        let mut scan = Scan::default();
        scan.functions
            .push(score("src/bridge/x.rs", "big", Metric::TooManyLines, 118));
        let next = Baseline::next(&scan, Some(&base)).expect("a lower score blesses");
        assert_eq!(next.functions[0].value, 118);

        scan.functions.clear();
        let next = Baseline::next(&scan, Some(&base)).expect("a removal blesses");
        assert!(next.functions.is_empty(), "the fixed entry is dropped");

        scan.functions
            .push(score("src/bridge/x.rs", "big", Metric::TooManyLines, 130));
        let refused = Baseline::next(&scan, Some(&base)).expect_err("a worse score is refused");
        assert_eq!(refused.len(), 1);
        assert!(refused[0].contains("is worse"));
    }

    /// A bless inside the 10% headroom records the new size only when it is
    /// lower: growth is never baked in as the next floor.
    #[test]
    fn blessing_never_raises_a_recorded_file_size() {
        let mut base = baseline();
        base.files.push(FileEntry {
            path: "src/bridge/x.rs".to_string(),
            lines: 10_000,
        });

        let mut scan = Scan::default();
        scan.files.push(FileSize {
            path: "src/bridge/x.rs".to_string(),
            lines: 10_500,
        });
        let next = Baseline::next(&scan, Some(&base)).expect("growth inside headroom blesses");
        assert_eq!(next.files[0].lines, 10_000, "the floor does not move up");

        scan.files[0].lines = 9_000;
        let next = Baseline::next(&scan, Some(&base)).expect("a shrink blesses");
        assert_eq!(next.files[0].lines, 9_000);
    }

    /// Seeding is the one mode that takes the tree as it is: today's 88
    /// offenders, 68 allows and 41 probes cannot be violations of themselves.
    #[test]
    fn seeding_writes_the_tree_as_it_is() {
        let mut scan = Scan::default();
        scan.functions
            .push(score("src/bridge/x.rs", "big", Metric::TooManyLines, 140));
        scan.allows.push(AllowSite {
            path: "src/bridge/x.rs".to_string(),
            line: 1,
            function: "wide".to_string(),
            tracked: false,
        });
        let next = Baseline::next(&scan, None).expect("a missing baseline seeds");
        assert_eq!(next.functions.len(), 1);
        assert_eq!(next.too_many_arguments.len(), 1);
    }

    /// One clippy diagnostic per line, filtered to the two complexity lints;
    /// cargo's build records on the same stream are not diagnostics.
    #[test]
    fn clippy_json_yields_the_two_complexity_lints() {
        let json = r#"
{"reason":"compiler-artifact","target":{"name":"cola"}}
{"reason":"compiler-message","message":{"code":{"code":"clippy::cognitive_complexity"},"message":"the function has a cognitive complexity of (162/25)","spans":[{"is_primary":true,"file_name":"src/bridge/command.rs","line_start":1594,"text":[{"text":"fn handle_command("}]}]}}
{"reason":"compiler-message","message":{"code":{"code":"clippy::too_many_lines"},"message":"this function has too many lines (934/100)","spans":[{"is_primary":true,"file_name":"src/bridge/command.rs","line_start":1594,"text":[{"text":"fn handle_command("}]}]}}
{"reason":"compiler-message","message":{"code":{"code":"clippy::needless_return"},"message":"unneeded `return` statement","spans":[]}}
not json at all
"#;
        let diagnostics = parse_clippy(json).expect("the stream parses");
        assert_eq!(
            diagnostics,
            vec![
                RawDiagnostic {
                    path: "src/bridge/command.rs".to_string(),
                    line: 1594,
                    metric: Metric::CognitiveComplexity,
                    value: 162,
                },
                RawDiagnostic {
                    path: "src/bridge/command.rs".to_string(),
                    line: 1594,
                    metric: Metric::TooManyLines,
                    value: 934,
                },
            ]
        );
    }

    /// The function name comes from the declaration line, and `async_trait`
    /// (whose generated methods re-parent their spans onto the attribute) is
    /// named after the `impl` it decorates.
    #[test]
    fn the_function_name_follows_the_span() {
        let content = "\
fn head() {}

    pub(crate) async fn send_projected_successor(
        a: u8,
    ) -> u8 {
        0
    }
";
        assert_eq!(function_name_at(content, 3), "send_projected_successor");
        assert_eq!(function_name_at(content, 1), "head");

        let async_trait = "\
pub struct PermissionKind;

#[async_trait::async_trait]
impl RequestKind for PermissionKind {
    fn label(&self) -> &'static str {
        \"\"
    }
}
";
        assert_eq!(
            function_name_at(async_trait, 3),
            "impl RequestKind for PermissionKind"
        );
    }

    /// Production lines are the lines outside `#[cfg(test)]` items — and only
    /// the code, never a doc comment that merely spells the attribute.
    #[test]
    fn production_lines_exclude_cfg_test_items() {
        let content = "\
//! A doc comment that mentions `#[cfg(test)]` is prose.

fn production() {
    let fixture = \"mod tests { #[cfg(test)] }\";
    let _ = fixture;
}

#[cfg(test)]
use super::fixture::Thing;

#[cfg(test)]
pub(super) fn helper() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn works() {
        assert_eq!(1, 1);
    }
}
";
        // The function above the first attribute: 5 lines, the blank line, and
        // the doc line — 7 in total. Everything from the first `#[cfg(test)]`
        // to the end is test-only.
        assert_eq!(production_lines(content), 9);
    }

    /// A `cfg(test)` on a struct field ends at its comma, not at the next
    /// brace: the surrounding production code stays counted.
    #[test]
    fn a_cfg_test_field_does_not_swallow_its_neighbours() {
        let content = "\
struct Turn {
    #[cfg(test)]
    scripted_message: Option<i64>,
    live: u64,
}
";
        assert_eq!(production_lines(content), 3);
    }

    /// The scan reports a cfg(test) site only for the attribute itself.
    #[test]
    fn cfg_test_sites_ignore_doc_prose() {
        let content = "\
/// Read through `#[cfg(test)]` `set_cursor`.\n#[cfg(test)]\nfn helper() {}\n";
        let masked = masked(content);
        let sites = cfg_test_sites("src/bridge/x.rs", &masked).expect("one site");
        assert_eq!(sites.lines, vec![2]);
    }

    /// The committed baseline matches the tree's text counters (functions need
    /// clippy, so they are the command's job; the rest is checked here).
    #[test]
    fn the_committed_baseline_matches_the_tree() {
        let root = crate::repo_root();
        let scan = scan_sources(&root).expect("the tree is readable");
        let text = std::fs::read_to_string(root.join(BASELINE_REL)).expect("the baseline exists");
        let baseline: Baseline = serde_json::from_str(&text).expect("the baseline parses");
        let violations = check(&scan, &baseline);
        assert!(
            violations.is_empty(),
            "the committed baseline drifted from the tree; run `cargo xtask check-structure \
             --bless` only for a lower tree:\n{}",
            violations.join("\n")
        );
    }
}
