//! The translation drift guard: `cargo xtask check-doc-drift`.
//!
//! `docs/user-guide.zh-CN.md` is a translation of `docs/user-guide.md`, and
//! the two must agree on everything a reader navigates by: section headings
//! (count and depths), fenced code blocks (count and languages — their bodies
//! are kept verbatim), tables (count and shape), indented code blocks, and
//! links. A source edit that adds or removes any of them leaves the
//! translation silently behind; this guard turns that into a CI failure.
//!
//! The comparison is deliberately structural, never textual: heading *titles*
//! are translated and prose differs by definition, so only the shape of the
//! document is compared. Like the generation guard (spec #364), the check
//! fails loudly when either document cannot be read — a guard that fails open
//! is no guard.

use std::path::Path;

/// The translated pairs: `(source, translation)`, repository-relative.
///
/// The English file is the source of truth; the translation mirrors its
/// structure. Adding a language extends this list and the guard covers it.
pub(crate) const PAIRS: &[(&str, &str)] = &[("docs/user-guide.md", "docs/user-guide.zh-CN.md")];

/// The structural shape of one document.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Signature {
    /// Heading depths in document order (`1` = `#`), outside fenced blocks.
    pub(crate) headings: Vec<usize>,
    /// Fenced code block info strings in document order (`""` for a bare
    /// fence).
    pub(crate) fenced_blocks: Vec<String>,
    /// Standalone indented (4-space) code blocks, as maximal runs.
    pub(crate) indented_blocks: usize,
    /// Tables as `(rows, columns)`; `rows` includes the header and separator.
    pub(crate) tables: Vec<(usize, usize)>,
    /// Markdown link targets (`](...)`), in-page and external together.
    pub(crate) links: usize,
    /// The subset of [`Self::links`] that points inside the document.
    pub(crate) anchor_links: usize,
}

impl Signature {
    /// Extract the shape from Markdown content.
    ///
    /// Only backtick fences are recognised (what this repository uses), and
    /// table cells are split on unescaped `|` so an escaped pipe inside a cell
    /// (e.g. `` `/sub attach <id\|标题>` ``) does not count as a column
    /// boundary. An indented run is a code block only when it follows a blank
    /// line outside a list — a list item's continuation is indented too.
    pub(crate) fn parse(content: &str) -> Self {
        let lines: Vec<&str> = content.lines().collect();
        let mut signature = Self::default();
        let mut in_fence = false;
        let mut in_table = false;
        let mut table_rows = 0usize;
        let mut table_columns = 0usize;
        let mut in_indented_run = false;
        // The last non-blank non-indented line, used to tell a standalone
        // indented code block from a list item's continuation.
        let mut last_opener = String::new();

        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                if in_fence {
                    in_fence = false;
                } else {
                    in_fence = true;
                    signature
                        .fenced_blocks
                        .push(trimmed.trim_start_matches('`').trim().to_string());
                }
                continue;
            }
            if in_fence {
                continue;
            }

            signature.links += line.matches("](").count();
            signature.anchor_links += line.matches("](#").count();

            if line.starts_with('|') {
                if !in_table {
                    in_table = true;
                    table_rows = 0;
                    table_columns = unescaped_pipes(line) - 1;
                }
                table_rows += 1;
                continue;
            }
            if in_table {
                in_table = false;
                signature.tables.push((table_rows, table_columns));
            }

            let hashes = line.chars().take_while(|&c| c == '#').count();
            if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
                signature.headings.push(hashes);
            }

            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with("    ") {
                if !in_indented_run {
                    in_indented_run = true;
                    let follows_blank = index == 0 || lines[index - 1].trim().is_empty();
                    if follows_blank && !is_list_item(&last_opener) {
                        signature.indented_blocks += 1;
                    }
                }
            } else {
                in_indented_run = false;
                last_opener = line.trim_start().to_string();
            }
        }
        if in_table {
            signature.tables.push((table_rows, table_columns));
        }
        signature
    }
}

/// Compare a translation against its source; every finding is one drifted
/// structural aspect, in a form the reader can act on.
pub(crate) fn diff(source: &Signature, translation: &Signature) -> Vec<String> {
    let mut findings = Vec::new();

    if source.headings != translation.headings {
        findings.push(match first_divergence(&source.headings, &translation.headings) {
            Some((index, source_level, translation_level)) => format!(
                "headings: {} in the source vs {} in the translation; first divergence at \
                 heading #{} (level {} vs {})",
                source.headings.len(),
                translation.headings.len(),
                index + 1,
                source_level,
                translation_level
            ),
            None => format!(
                "headings: {} in the source vs {} in the translation",
                source.headings.len(),
                translation.headings.len()
            ),
        });
    }

    if source.fenced_blocks != translation.fenced_blocks {
        findings.push(
            match first_divergence(&source.fenced_blocks, &translation.fenced_blocks) {
                Some((index, source_info, translation_info)) => format!(
                    "fenced code blocks: {} in the source vs {} in the translation; first \
                     divergence at block #{} (source language \"{source_info}\", translation \
                     \"{translation_info}\")",
                    source.fenced_blocks.len(),
                    translation.fenced_blocks.len(),
                    index + 1
                ),
                None => format!(
                    "fenced code blocks: {} in the source vs {} in the translation",
                    source.fenced_blocks.len(),
                    translation.fenced_blocks.len()
                ),
            },
        );
    }

    if source.indented_blocks != translation.indented_blocks {
        findings.push(format!(
            "indented code blocks: {} in the source vs {} in the translation",
            source.indented_blocks, translation.indented_blocks
        ));
    }

    if source.tables != translation.tables {
        findings.push(match first_divergence(&source.tables, &translation.tables) {
            Some((index, source_shape, translation_shape)) => format!(
                "table #{}: source {} rows × {} columns, translation {} rows × {} columns",
                index + 1,
                source_shape.0,
                source_shape.1,
                translation_shape.0,
                translation_shape.1
            ),
            None => format!(
                "tables: {} in the source vs {} in the translation",
                source.tables.len(),
                translation.tables.len()
            ),
        });
    }

    if source.links != translation.links {
        findings.push(format!(
            "links: {} in the source vs {} in the translation",
            source.links, translation.links
        ));
    }

    if source.anchor_links != translation.anchor_links {
        findings.push(format!(
            "in-page anchor links: {} in the source vs {} in the translation",
            source.anchor_links, translation.anchor_links
        ));
    }

    findings
}

/// The first index at which two sequences differ, with both values.
fn first_divergence<T: PartialEq + Clone>(source: &[T], translation: &[T]) -> Option<(usize, T, T)> {
    source
        .iter()
        .zip(translation)
        .enumerate()
        .find(|(_, (source_item, translation_item))| source_item != translation_item)
        .map(|(index, (source_item, translation_item))| {
            (index, source_item.clone(), translation_item.clone())
        })
}

/// `|` characters that are not escaped with a backslash.
fn unescaped_pipes(line: &str) -> usize {
    let mut pipes = 0;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '|' {
            pipes += 1;
        }
    }
    pipes
}

/// Whether a line opens a Markdown list item (whose 4-space continuation is
/// content, not an indented code block).
fn is_list_item(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
        return true;
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && line[digits..].starts_with(". ")
}

/// Read one document, naming the failing path on error.
pub(crate) fn read_doc(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|e| format!("error: cannot read docs-drift candidate {}: {e}", path.display()))
}

/// `cargo xtask check-doc-drift`: fail when a translation's structure no longer
/// matches its source, or when either document cannot be read.
pub(crate) fn run() {
    let root = crate::repo_root();
    let mut drifted = 0usize;
    for (source, translation) in PAIRS {
        let source_content = match read_doc(&root.join(source)) {
            Ok(content) => content,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        };
        let translation_content = match read_doc(&root.join(translation)) {
            Ok(content) => content,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        };
        let findings = diff(
            &Signature::parse(&source_content),
            &Signature::parse(&translation_content),
        );
        if findings.is_empty() {
            continue;
        }
        drifted += 1;
        eprintln!("{translation} has drifted from {source}:");
        for finding in &findings {
            eprintln!("  - {finding}");
        }
        eprintln!(
            "  update {translation} so its structure matches {source}, then re-run \
             `cargo xtask check-doc-drift`"
        );
    }
    if drifted > 0 {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal translated pair: the structure matches, the prose does not.
    const SOURCE: &str = "\
# Guide

Intro.

## One

```bash
echo hi
```

| A | B |
| --- | --- |
| 1 | 2 |

[one](#one) and [readme](../README.md)
";
    const TRANSLATION: &str = "\
# 指南

简介。

## 一

```bash
echo hi
```

| 甲 | 乙 |
| --- | --- |
| 一 | 二 |

[一](#一) and [readme](../README.md)
";

    /// The parser must see a non-trivial shape, or the in-sync assertions
    /// below would pass vacuously on empty signatures.
    #[test]
    fn parsing_a_pair_yields_the_same_non_trivial_shape() {
        let source = Signature::parse(SOURCE);
        assert_eq!(source.headings, vec![1, 2]);
        assert_eq!(source.fenced_blocks, vec!["bash"]);
        assert_eq!(source.tables, vec![(3, 2)]);
        assert_eq!(source.links, 2);
        assert_eq!(source.anchor_links, 1);
        assert_eq!(source, Signature::parse(TRANSLATION));
    }

    #[test]
    fn a_translated_pair_compares_clean() {
        assert!(diff(&Signature::parse(SOURCE), &Signature::parse(TRANSLATION)).is_empty());
    }

    #[test]
    fn a_missing_section_is_reported() {
        let drifted = TRANSLATION.replace("## 一\n", "");
        let findings = diff(&Signature::parse(SOURCE), &Signature::parse(&drifted));
        assert_eq!(findings.len(), 1, "only the heading drifted: {findings:?}");
        assert!(findings[0].starts_with("headings:"), "{}", findings[0]);
    }

    #[test]
    fn a_changed_heading_level_is_reported() {
        let drifted = TRANSLATION.replace("## 一", "### 一");
        let findings = diff(&Signature::parse(SOURCE), &Signature::parse(&drifted));
        assert_eq!(findings.len(), 1, "only the heading drifted: {findings:?}");
        assert!(
            findings[0].contains("first divergence at heading #2 (level 2 vs 3)"),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn a_missing_code_block_is_reported() {
        let drifted = TRANSLATION.replace("```bash\necho hi\n```\n", "");
        let findings = diff(&Signature::parse(SOURCE), &Signature::parse(&drifted));
        assert_eq!(findings.len(), 1, "only the fence drifted: {findings:?}");
        assert!(findings[0].starts_with("fenced code blocks:"), "{}", findings[0]);
    }

    #[test]
    fn a_changed_fence_language_is_reported() {
        let drifted = TRANSLATION.replace("```bash", "```sh");
        let findings = diff(&Signature::parse(SOURCE), &Signature::parse(&drifted));
        assert_eq!(findings.len(), 1, "only the fence drifted: {findings:?}");
        assert!(
            findings[0].contains("source language \"bash\", translation \"sh\""),
            "{}",
            findings[0]
        );
    }

    #[test]
    fn a_table_row_change_is_reported() {
        let drifted = TRANSLATION.replace("| 一 | 二 |\n", "");
        let findings = diff(&Signature::parse(SOURCE), &Signature::parse(&drifted));
        assert_eq!(findings.len(), 1, "only the table drifted: {findings:?}");
        assert!(findings[0].starts_with("table #1:"), "{}", findings[0]);
    }

    #[test]
    fn a_missing_link_is_reported() {
        let drifted = TRANSLATION.replace("[readme](../README.md)", "readme");
        let findings = diff(&Signature::parse(SOURCE), &Signature::parse(&drifted));
        assert_eq!(findings.len(), 1, "only the links drifted: {findings:?}");
        assert!(findings[0].starts_with("links:"), "{}", findings[0]);
    }

    #[test]
    fn a_missing_anchor_link_is_reported() {
        let drifted = TRANSLATION.replace("[一](#一)", "一");
        let findings = diff(&Signature::parse(SOURCE), &Signature::parse(&drifted));
        assert_eq!(
            findings.len(),
            2,
            "the anchor and the total link count both drifted: {findings:?}"
        );
        assert!(
            findings.iter().any(|f| f.starts_with("in-page anchor links:")),
            "{findings:?}"
        );
    }

    /// A toml fence's `#` comments are not headings — the exact trap the
    /// repository's `[bridge]` block carries.
    #[test]
    fn headings_inside_fenced_blocks_are_not_counted() {
        let content = "```toml\n# session_file = \"x\"\n# access_file = \"y\"\n```\n\n# Real\n";
        let signature = Signature::parse(content);
        assert_eq!(signature.headings, vec![1]);
        assert_eq!(signature.fenced_blocks, vec!["toml"]);
    }

    #[test]
    fn a_standalone_indented_block_is_counted_but_a_list_continuation_is_not() {
        let standalone = "Run this:\n\n    gh attestation verify x\n";
        assert_eq!(Signature::parse(standalone).indented_blocks, 1);
        let continuation = "- `auto` — attaches\n    and continues here.\n";
        assert_eq!(Signature::parse(continuation).indented_blocks, 0);
    }

    /// Escaped pipes inside a table cell are cell content, not column borders.
    #[test]
    fn escaped_pipes_do_not_add_table_columns() {
        let content = "| Cmd | What |\n| --- | --- |\n| `/sub attach <id\\|标题>` | x |\n";
        assert_eq!(Signature::parse(content).tables, vec![(3, 2)]);
    }

    #[test]
    fn an_unreadable_document_is_a_loud_error() {
        let missing = std::env::temp_dir().join(format!("cola-doc-drift-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let error = read_doc(&missing).expect_err("a missing document must fail the read");
        assert!(
            error.contains("cannot read docs-drift candidate"),
            "the error must name the failure: {error}"
        );
        assert!(error.contains(&missing.to_string_lossy().to_string()), "{error}");
    }

    /// The acceptance criterion's "passes on the tree": the repository's own
    /// pair is structurally in sync, so any future drift fails this test too.
    #[test]
    fn the_repository_pair_is_in_sync() {
        let root = crate::repo_root();
        for (source, translation) in PAIRS {
            let source_content = read_doc(&root.join(source)).expect("the source must be readable");
            let translation_content =
                read_doc(&root.join(translation)).expect("the translation must be readable");
            let source_signature = Signature::parse(&source_content);
            // Guard against a vacuous pass: an empty or unparsable document
            // must not compare "in sync" with another empty one.
            assert!(
                source_signature.headings.len() > 10
                    && !source_signature.fenced_blocks.is_empty()
                    && !source_signature.tables.is_empty()
                    && source_signature.links > 0,
                "{source} parsed to an implausible shape: {source_signature:?}"
            );
            let findings = diff(&source_signature, &Signature::parse(&translation_content));
            assert!(
                findings.is_empty(),
                "{translation} has drifted from {source}:\n{}",
                findings.join("\n")
            );
        }
    }
}
