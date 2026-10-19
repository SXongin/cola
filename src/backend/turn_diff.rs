//! The neutral Turn Diff model (spec #797, ticket #799).
//!
//! One Turn's changed files as the Bridge renders them: the index line's
//! `N files · +A −D` and the complete-diff HTML both read this view, so the two
//! surfaces cannot disagree. V2 reads it from `session.diff`; V1 has no such
//! endpoint and aggregates the Turn's tool metadata instead (#800). Both paths
//! produce this one type, so no caller branches on a Generation.

/// One file in a Turn Diff: the path the wire calls `file`, the unified patch
/// (kept verbatim for the HTML artifact) and its reported line counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnDiffFile {
    /// The repository-relative path the diff names.
    pub path: String,
    /// The unified diff body exactly as the source carried it.
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
