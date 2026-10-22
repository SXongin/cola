//! The shared source walk behind the `cargo xtask check-*` guards.
//!
//! Both guards read the tree the same way: every `.rs` file under `src/`, keyed
//! by its repository-relative path (`src/bridge/handler.rs`) so an allow-list can
//! spell `src/opencode/…`, and a loud error — never a skipped file — when the
//! tree cannot be read (a guard that fails open is no guard). Only the matchers
//! and their `Finding` types differ, so those stay with each guard.

use std::path::{Path, PathBuf};

/// One scanned source, ready for a guard's matcher.
#[derive(Debug)]
pub(crate) struct SourceFile {
    /// Repository-relative and `/`-separated (`src/bridge/handler.rs`), so the
    /// guards' allow-lists read as repository paths.
    pub(crate) path: String,
    pub(crate) content: String,
}

/// Read every `.rs` file under `root`, sorted by path.
///
/// `guard` names the calling guard in the errors (`generation-guard`,
/// `client-seam`), so a failure says which command could not scan.
pub(crate) fn read_sources(root: &Path, guard: &str) -> Result<Vec<SourceFile>, String> {
    let mut files = Vec::new();
    collect_rust_files(root, &mut files, guard)?;
    files.sort();
    files
        .into_iter()
        .map(|file| {
            let content = std::fs::read_to_string(&file)
                .map_err(|e| format!("error: cannot read scan candidate {}: {e}", file.display()))?;
            Ok(SourceFile {
                path: source_path(root, &file),
                content,
            })
        })
        .collect()
}

/// The repository-relative path of one scanned file: the root's own directory
/// name (`src`) joined with its path below the root, `/`-separated on every
/// platform.
fn source_path(root: &Path, file: &Path) -> String {
    let relative = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    let root_name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{root_name}/{relative}")
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>, guard: &str) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("error: cannot scan {guard} directory {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("error: cannot read an entry of {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, files, guard)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The walk is recursive, only looks at `.rs` files, and reports each one by
    /// its repository-relative path.
    #[test]
    fn the_walk_is_recursive_rust_only_and_repo_relative() {
        let root = std::env::temp_dir().join(format!("cola-source-scan-{}", std::process::id()));
        let nested = root.join("src").join("bridge").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("planted.rs"), "fn planted() {}\n").unwrap();
        std::fs::write(root.join("src").join("top.rs"), "fn top() {}\n").unwrap();
        std::fs::write(root.join("src").join("README.md"), "not rust\n").unwrap();

        let sources = read_sources(&root.join("src"), "test-guard").expect("the tree is readable");
        std::fs::remove_dir_all(&root).unwrap();

        let paths: Vec<&str> = sources.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, ["src/bridge/nested/planted.rs", "src/top.rs"]);
        assert_eq!(sources[0].content, "fn planted() {}\n");
    }

    /// A missing root is a loud error naming the calling guard and the path.
    #[test]
    fn a_missing_root_is_a_loud_error_naming_the_guard() {
        let missing = std::env::temp_dir().join(format!("cola-source-scan-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let error = read_sources(&missing, "test-guard").expect_err("a missing root must fail");
        assert!(error.contains("cannot scan test-guard directory"), "{error}");
        assert!(error.contains(missing.to_string_lossy().as_ref()), "{error}");
    }
}
