//! The V1 coupling guard (spec #364).
//!
//! Scans the Rust sources under `src/` for V1-distinctive wire literals: route
//! paths and field names that belong to the V1 generation strategy alone. A
//! match anywhere else means a caller has reached around the strategy seam
//! into V1's wire — exactly the coupling that would turn V1 retirement from a
//! deletion into a scavenger hunt (ADR-0055).
//!
//! The allow-list is explicit and small: the whole `src/opencode/v1/` strategy
//! module (its paths, payloads and legacy decoder) and the Platform's
//! tool-render surface (`src/feishu/card/tool_render.rs`), the documented
//! porosity exception where built-in-tool payload field names are tailored
//! (ADR-0042/ADR-0053).
//!
//! The denylist is generation-scoped: it names V1 route and field literals
//! only. Current-generation (`/api/...`) routes are deliberately not policed —
//! V1 retirement must not delete them (both generations serve session creation
//! and compaction, and they live on the generation-blind adapter), and the
//! spec charges this guard with V1 coupling, not with a V2 allow-list.
//!
//! Matching is deliberately conservative so prose does not trip it: route
//! patterns carry their opening quote (a route is always a string literal in
//! Rust, while "permission/question cards" is prose), and the one ambiguous
//! root (`/agent`, which collides with cola's own Feishu command) is matched in
//! URL-call form.

use std::path::{Path, PathBuf};

/// Paths (relative to the scanned root, `/`-separated) exempt from the guard.
pub(crate) const ALLOWED_PREFIXES: &[&str] = &[
    // The V1 generation strategy owns every V1 path, payload and decoder.
    "src/opencode/v1/",
    // The Platform's tool-render surface, the documented porosity exception
    // (ADR-0042/ADR-0053): it tailors built-in tool payloads by field name.
    "src/feishu/card/tool_render.rs",
];

/// The V1-distinctive literals, as `(pattern, what)`.
///
/// `/agent` shares its spelling with cola's own Feishu command, so it is
/// matched in URL-call form; the other route roots are unambiguous enough to
/// scan as quoted literals.
///
/// Deliberately absent: field names both generations carry — `sessionID`,
/// `providerID`, `parentID`, `messageID` — are not V1-distinctive (spec #364
/// §3), so denylisting them would be a false positive, not a guard.
pub(crate) const FORBIDDEN: &[(&str, &str)] = &[
    ("\"/session", "V1 route literal"),
    ("\"/experimental/session", "V1 route literal"),
    ("\"/permission", "V1 route literal"),
    ("\"/question", "V1 route literal"),
    ("\"/provider", "V1 route literal"),
    ("\"/prompt_async", "V1 route literal"),
    ("url(\"/agent\")", "V1 route literal"),
    ("append_pair(\"directory\"", "V1 instance-routing query"),
    ("\"callID\"", "V1 wire field name"),
    ("\"modelID\"", "V1 wire field name"),
    ("\"step-start\"", "V1 wire part type"),
    ("\"step-finish\"", "V1 wire part type"),
    ("\"tool-calls\"", "V1 wire finish reason"),
    ("\"x-next-cursor\"", "V1 pagination header"),
];

/// One forbidden literal found outside the allow-list.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) pattern: &'static str,
    pub(crate) what: &'static str,
    pub(crate) text: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {} `{}` outside the V1 strategy — {}",
            self.path, self.line, self.what, self.pattern, self.text
        )
    }
}

/// The repository root: xtask lives directly under it.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives under the repository root")
        .to_path_buf()
}

/// Scan `root`'s Rust sources recursively.
///
/// IO failures are errors, never skipped files: a missing or unreadable `src/`
/// must fail the command loudly rather than report a clean tree (a guard that
/// fails open is no guard).
pub(crate) fn scan_tree(root: &Path) -> Result<Vec<Finding>, String> {
    let mut files = Vec::new();
    collect_rust_files(root, &mut files)?;
    files.sort();
    let mut findings = Vec::new();
    for file in files {
        let content = std::fs::read_to_string(&file)
            .map_err(|e| format!("error: cannot read scan candidate {}: {e}", file.display()))?;
        let relative = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        // Paths are reported relative to the scanned root's parent so the
        // allow-list reads as a repository path (`src/opencode/v1/`).
        let path = format!("{}/{}", root_name(root), relative);
        findings.extend(scan_source(&path, &content));
    }
    Ok(findings)
}

/// The directory name of the scanned root (`src`), used to build the
/// repository-relative path the allow-list matches.
fn root_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Scan one file's content. `path` is the repository-relative path
/// (`src/...`), used for the allow-list.
pub(crate) fn scan_source(path: &str, content: &str) -> Vec<Finding> {
    if ALLOWED_PREFIXES.iter().any(|allowed| path.starts_with(allowed)) {
        return Vec::new();
    }
    let mut findings = Vec::new();
    for (index, line) in content.lines().enumerate() {
        for (pattern, what) in FORBIDDEN {
            if line.contains(pattern) {
                findings.push(Finding {
                    path: path.to_string(),
                    line: index + 1,
                    pattern,
                    what,
                    text: line.trim().to_string(),
                });
            }
        }
    }
    findings
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!(
            "error: cannot scan generation-guard directory {}: {e}",
            dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("error: cannot read an entry of {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, files)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// `cargo xtask check-generation`: fail when a V1 wire literal escaped the V1
/// strategy, or when the sources cannot be scanned at all.
pub(crate) fn run() {
    let findings = match scan_tree(&repo_root().join("src")) {
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
        "error: {} V1 wire coupling(s) outside the V1 strategy — move them into src/opencode/v1/ \
         or add the file to the guard's allow-list with a reason",
        findings.len()
    );
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_planted_v1_route_literal_outside_the_strategy_is_a_finding() {
        let findings = scan_source(
            "src/bridge/handler.rs",
            "let url = \"/session/status\";\nlet other = \"/permission\";\n",
        );
        assert_eq!(
            findings.len(),
            2,
            "both planted routes must be found: {findings:?}"
        );
        assert_eq!(findings[0].path, "src/bridge/handler.rs");
        assert_eq!(findings[0].line, 1);
        assert_eq!(findings[0].pattern, "\"/session");
        assert_eq!(findings[1].pattern, "\"/permission");
    }

    #[test]
    fn a_planted_v1_wire_field_outside_the_strategy_is_a_finding() {
        let findings = scan_source(
            "src/bridge/pollers.rs",
            "let call = part.get(\"callID\");\nlet model = info.get(\"modelID\");\n\
             url.query_pairs_mut().append_pair(\"directory\", d);\n",
        );
        assert_eq!(
            findings.len(),
            3,
            "every planted coupling must be found: {findings:?}"
        );
        assert_eq!(findings[0].pattern, "\"callID\"");
        assert_eq!(findings[1].pattern, "\"modelID\"");
        assert_eq!(findings[2].pattern, "append_pair(\"directory\"");
    }

    #[test]
    fn the_v1_strategy_and_the_tool_render_surface_are_allowed() {
        let planted = "let url = \"/session/status\";\nlet call = \"callID\";\n";
        assert!(scan_source("src/opencode/v1/mod.rs", planted).is_empty());
        assert!(scan_source("src/opencode/v1/wire/legacy.rs", planted).is_empty());
        assert!(scan_source("src/opencode/v1/tests.rs", planted).is_empty());
        assert!(scan_source("src/feishu/card/tool_render.rs", planted).is_empty());
    }

    /// The guard must not fire on prose or cola's own vocabulary: only string
    /// literals that spell a V1 route count, and `/agent` alone is a Feishu
    /// command, not a route.
    #[test]
    fn prose_and_cola_commands_do_not_trip_the_guard() {
        let content = "\
// Cards cover permission/question flows and GET /session documentation.\n\
let command = \"/agent\";\n\
let help = \"/agent build\";\n";
        assert_eq!(scan_source("src/bridge/handler.rs", content), Vec::new());
    }

    /// The walk is recursive and only looks at `.rs` files.
    #[test]
    fn the_tree_walk_reports_nested_rust_files_only() {
        let root = std::env::temp_dir().join(format!("cola-generation-guard-{}", std::process::id()));
        let nested = root.join("src").join("bridge").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("planted.rs"), "let url = \"/question\";\n").unwrap();
        std::fs::write(root.join("src").join("README.md"), "\"/question\"\n").unwrap();

        let findings = scan_tree(&root.join("src")).expect("the planted tree is readable");
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            findings.len(),
            1,
            "only the planted Rust file should report: {findings:?}"
        );
        assert_eq!(findings[0].path, "src/bridge/nested/planted.rs");
    }

    /// A missing or unreadable scan root is a loud error, never a clean tree:
    /// the guard must not fail open (a silently empty scan would make CI green
    /// while nothing was checked).
    #[test]
    fn an_unreadable_scan_root_is_a_loud_error() {
        let missing =
            std::env::temp_dir().join(format!("cola-generation-guard-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let error = scan_tree(&missing).expect_err("a missing root must fail the scan");
        assert!(
            error.contains("cannot scan generation-guard directory"),
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
        let findings = scan_tree(&repo_root().join("src")).expect("the repository tree must be readable");
        assert!(
            findings.is_empty(),
            "V1 wire coupling outside the V1 strategy:\n{}",
            findings
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn the_allow_list_covers_the_v1_strategy_and_tool_render() {
        assert!(ALLOWED_PREFIXES.iter().any(|p| p.starts_with("src/opencode/v1/")));
        assert!(ALLOWED_PREFIXES.contains(&"src/feishu/card/tool_render.rs"));
    }
}
