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
//! name each other freely — plus a short, explicitly temporary list of the
//! adapter's attach/lifecycle plumbing, which the bridge still drives directly
//! until ticket #875 moves server discovery, attach and lifecycle behind the
//! seam. Those names are listed one by one rather than by file, so an unrelated
//! `crate::opencode::something` in the same file still fails; delete each entry
//! with #875.
//!
//! Matching is by module path, not by prose: only the literal `crate::opencode`
//! counts, so the word OpenCode and the `[opencode]` config table are fine.

use std::path::{Path, PathBuf};

/// Paths (relative to the scanned root's parent, `/`-separated) exempt from the
/// guard.
pub(crate) const ALLOWED_PREFIXES: &[&str] = &[
    // The adapter owns its own internals: client, strategies, wire decoders,
    // transport and the tests that exercise them.
    "src/opencode/",
];

/// The adapter plumbing the bridge still names directly, one path at a time.
///
/// Attach and lifecycle are the last OpenCode-shaped work living in the bridge;
/// ticket #875 moves it behind the client seam. Until then these three item
/// paths are the *only* escape, and each disappears with that ticket.
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
        // allow-list reads as a repository path (`src/opencode/`).
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
        let mut rest = line;
        while let Some(offset) = rest.find(FORBIDDEN) {
            let tail = &rest[offset + FORBIDDEN.len()..];
            if !is_exempt(tail) {
                findings.push(Finding {
                    path: path.to_string(),
                    line: index + 1,
                    text: line.trim().to_string(),
                });
                break;
            }
            rest = tail;
        }
    }
    findings
}

/// Whether the path continues with one of the [`PENDING_SEAM_MOVE`] items.
///
/// The segment must end at a path boundary: `::strategy::Generation` is exempt,
/// `::strategy_builder` is not.
fn is_exempt(tail: &str) -> bool {
    let Some(tail) = tail.strip_prefix("::") else {
        return false;
    };
    let segment_end = tail.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
    let (segment, after) = match segment_end {
        Some(end) => (&tail[..end], &tail[end..]),
        None => (tail, ""),
    };
    let continues = after.is_empty() || after.starts_with("::");
    continues
        && PENDING_SEAM_MOVE
            .iter()
            .any(|allowed| allowed.rsplit("::").next() == Some(segment))
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("error: cannot scan client-seam directory {}: {e}", dir.display()))?;
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

    #[test]
    fn a_planted_dto_import_outside_the_seam_is_a_finding() {
        let findings = scan_source(
            "src/bridge/handler.rs",
            "use crate::opencode::types::Session;\nlet x = 1;\n",
        );
        assert_eq!(
            findings.len(),
            1,
            "the planted import must be found: {findings:?}"
        );
        assert_eq!(findings[0].path, "src/bridge/handler.rs");
        assert_eq!(findings[0].line, 1);
        assert_eq!(findings[0].text, "use crate::opencode::types::Session;");
    }

    /// The module alias is as much a seam leak as a DTO import: it makes every
    /// `opencode::…` path in the file legal.
    #[test]
    fn a_planted_module_alias_outside_the_seam_is_a_finding() {
        let findings = scan_source("src/bridge/snapshot.rs", "use crate::opencode;\n");
        assert_eq!(findings.len(), 1);
    }

    /// A bare `crate::opencode` (no suffix) is a finding too.
    #[test]
    fn a_planted_bare_path_outside_the_seam_is_a_finding() {
        let findings = scan_source("src/status.rs", "let m = crate::opencode;\n");
        assert_eq!(findings.len(), 1);
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
        assert!(scan_source("src/bridge/attach.rs", planted).is_empty());
        assert_eq!(
            PENDING_SEAM_MOVE,
            [
                "crate::opencode::generation",
                "crate::opencode::strategy",
                "crate::opencode::transport",
            ]
        );
    }

    /// The exemption is per item, not per file, and it stops at a path
    /// boundary: a near-miss name is a finding.
    #[test]
    fn a_near_miss_on_an_exempt_item_name_is_a_finding() {
        for planted in [
            "use crate::opencode::strategy_builder::Plan;\n",
            "use crate::opencode::types::Session;\n",
            "use crate::opencode::client::OpenCodeBackend;\n",
            "use crate::opencode::parsing::parse_model;\n",
        ] {
            assert_eq!(
                scan_source("src/bridge/attach.rs", planted).len(),
                1,
                "{planted} must not be exempt"
            );
        }
    }

    /// Several paths on one line are all checked: an exempt item does not
    /// shield a forbidden one beside it.
    #[test]
    fn an_exempt_item_does_not_shield_a_forbidden_one_on_the_same_line() {
        let findings = scan_source(
            "src/bridge/attach.rs",
            "use crate::opencode::strategy::Generation; use crate::opencode::types::Session;\n",
        );
        assert_eq!(findings.len(), 1);
    }

    /// Prose and the config table are not path references.
    #[test]
    fn prose_and_the_config_table_do_not_trip_the_guard() {
        let content = "\
//! The OpenCode adapter attaches to a shared server.\n\
let table = cfg.opencode.generation;\n\
let dir = path.join(\"opencode\").join(\"service.json\");\n";
        assert_eq!(scan_source("src/bridge/attach.rs", content), Vec::new());
    }

    /// The walk is recursive and only looks at `.rs` files.
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

        let findings = scan_tree(&root.join("src")).expect("the planted tree is readable");
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            findings.len(),
            1,
            "only the planted Rust file should report: {findings:?}"
        );
        assert_eq!(findings[0].path, "src/bridge/nested/planted.rs");
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
        let findings =
            scan_tree(&crate::repo_root().join("src")).expect("the repository tree must be readable");
        assert!(
            findings.is_empty(),
            "references to the OpenCode adapter outside it:\n{}",
            findings
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn the_allow_list_covers_only_the_adapter() {
        assert_eq!(ALLOWED_PREFIXES, ["src/opencode/"]);
    }
}
