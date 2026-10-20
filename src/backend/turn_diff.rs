//! The neutral Turn Diff model (spec #797, ticket #799).
//!
//! One Turn's changed files in a neutral shape, decoded from V2's
//! `session.diff`. The card's index line and the HTML artifact read the same
//! view (ADR-0087): there is no V1 aggregation and no transcript-side estimate
//! any more, so a consumer never has to reconcile two counts.

/// One file in a Turn Diff: the path the wire calls `file`, the unified patch
/// (kept verbatim for the HTML artifact) and its reported line counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnDiffFile {
    /// The repository-relative path the diff names.
    pub path: String,
    /// The unified diff body the source carried, with only line endings and a
    /// trailing newline normalized for rendering.
    pub patch: String,
    /// The added-line count the source reported.
    pub additions: usize,
    /// The deleted-line count the source reported.
    pub deletions: usize,
    /// `modified` / `added` / `deleted`, in the source's own spelling.
    pub status: String,
}

/// One Turn's complete file diff, in the source's order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnDiff(pub Vec<TurnDiffFile>);

impl TurnDiff {
    /// The index line's counts: the number of files and the line totals, read
    /// from exactly the entries this view carries. The view is the server's
    /// deduped net answer for the Turn, and the index and the artifact read the
    /// same one (ADR-0087), so folding here is the only count either surface
    /// shows.
    pub fn stats(&self) -> TurnDiffStats {
        TurnDiffStats {
            files: self.0.len(),
            additions: self.0.iter().map(|file| file.additions).sum(),
            deletions: self.0.iter().map(|file| file.deletions).sum(),
        }
    }

    /// Whether the Turn changed no file. A read/bash-only Turn yields an empty
    /// diff, and the card must then render no index line and no button.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The compact counts the Turn's index line shows (#477): `N files · +A −D`,
/// derived from a [`TurnDiff`]'s entries. The index and the artifact read the
/// same `session.diff` view (ADR-0087), so the two cannot disagree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnDiffStats {
    pub files: usize,
    pub additions: usize,
    pub deletions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, additions: usize, deletions: usize) -> TurnDiffFile {
        TurnDiffFile {
            path: path.to_string(),
            patch: String::new(),
            additions,
            deletions,
            status: "modified".to_string(),
        }
    }

    /// The card's index line and the artifact read the SAME deduped entries,
    /// so the totals are a fold over them — never a second count that could
    /// disagree with what the reader opens.
    #[test]
    fn stats_fold_the_reported_files_and_their_final_counts() {
        let diff = TurnDiff(vec![file("a.rs", 2, 1), file("b.rs", 0, 3)]);
        assert_eq!(
            diff.stats(),
            TurnDiffStats {
                files: 2,
                additions: 2,
                deletions: 4,
            }
        );
    }

    #[test]
    fn an_empty_diff_has_zero_stats_and_says_so() {
        assert_eq!(TurnDiff::default().stats(), TurnDiffStats::default());
        assert!(TurnDiff::default().is_empty());
        assert!(!TurnDiff(vec![file("a.rs", 1, 0)]).is_empty());
    }
}
