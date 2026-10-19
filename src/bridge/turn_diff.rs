//! The Turn Diff reconstructed from a Turn's own tool records (spec #797,
//! ticket #800).
//!
//! V1 has no `session.diff` endpoint, so its complete-diff source is the
//! unified patches the file-editing tools recorded on their calls: an `edit`
//! call's `metadata.diff`, an `apply_patch` / `patch` call's
//! `metadata.files[].patch`. [`turn_diff_from_tools`] folds those into the same
//! neutral [`TurnDiff`] V2's `session.diff` produces, so neither the #477 index
//! line nor #801's artifact branches on a Generation, and [`TurnDiff::stats`]
//! gives the index its `N files · +A −D` from exactly the entries the artifact
//! renders.
//!
//! **What this view means — and does not.** It is "what the tools recorded
//! they did", not "what the working tree finally became". A `bash` call that
//! runs a formatter, a generator or `git checkout` changes files without any
//! file-editing tool recording a patch, so those changes are absent here. A
//! file edited twice in one Turn appears once, with the later patch (the Turn's
//! final recorded state for that path). Only Turns that actually recorded a
//! file edit produce a non-empty diff; a read/bash-only Turn produces an empty
//! one, which is what keeps the card from offering a dead entry.

use crate::backend::{Part, SessionTranscript, ToolCall, TurnAnchor, TurnDiff, TurnDiffFile};
use crate::feishu::card::tool_render::{diff_header_path, opens_file_block, recorded_diff};
use indexmap::IndexMap;

/// The tools whose calls can record a file change. `write` records no patch on
/// either generation's successful call, so it is completed from its input (see
/// [`write_diff`]); the other three carry `metadata.diff` /
/// `metadata.files[].patch`.
fn records_a_file_edit(name: &str) -> bool {
    matches!(name, "edit" | "apply_patch" | "patch" | "write")
}

/// Aggregate the file edits the Turn anchored at `anchor` recorded on its tool
/// calls into one neutral [`TurnDiff`].
///
/// Calls are read in transcript order and only the file-editing tools count:
/// `edit`, `apply_patch`, `patch`, and `write` (whose full-content patch the
/// tools do not always record — see [`write_diff`]). Each recorded patch is
/// split at its own file headers; a path edited more than once keeps the LAST
/// patch, because the Turn's final recorded state is what the reader wants to
/// review.
///
/// Only SETTLED calls contribute. A call still `Pending`/`Running` is skipped
/// even when it already carries a patch: V1's `edit` records the diff while it
/// is still asking permission, and nothing has changed on disk until the call
/// completes — exposing it would offer a change the user may yet deny. A
/// failed call is skipped too (it applied nothing). `ToolStatus::Unknown` and a
/// non-failed `Other` count as settled: the transcript's own `is_live` /
/// `is_failure` verdicts are the whole rule, so a call the decoder could not
/// classify is not treated as live forever.
pub(crate) fn turn_diff_from_tools(transcript: &SessionTranscript, anchor: &TurnAnchor) -> TurnDiff {
    let mut files: IndexMap<String, TurnDiffFile> = IndexMap::new();
    for call in transcript
        .turn_for_user(anchor)
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::Tool(call) => Some(call),
            _ => None,
        })
    {
        if call.status.is_live() || call.status.is_failure() || !records_a_file_edit(&call.identity.name) {
            continue;
        }
        let recorded = match recorded_diff(call) {
            Some(patch) => split_patch(&patch),
            // `write` records the file's new content in its input, not a
            // patch: both generations' successful calls carry no diff
            // metadata (V1 records the patch only on the permission ask), so
            // skipping it would hide every file the Turn created.
            None if call.identity.name == "write" => write_diff(call).into_iter().collect(),
            None => Vec::new(),
        };
        for file in recorded {
            // A later occurrence overwrites the earlier one but keeps the
            // path's first position: the Turn's final state for that file,
            // counted once.
            files.insert(file.path.clone(), file);
        }
    }
    TurnDiff(files.into_values().collect())
}

/// One file's slice of a recorded patch, before it becomes a [`TurnDiffFile`].
struct PatchFile {
    path: Option<String>,
    /// The path came from an `Index:` header, which outranks `---`/`+++` (the
    /// ticket's rule): the headers may name a rename's two sides, while the
    /// index line names the entry the patch was recorded for.
    path_from_index: bool,
    patch: String,
    additions: usize,
    deletions: usize,
    old_is_null: bool,
    new_is_null: bool,
    first_hunk: Option<String>,
}

impl PatchFile {
    fn new(path: Option<String>, path_from_index: bool) -> Self {
        Self {
            path,
            path_from_index,
            patch: String::new(),
            additions: 0,
            deletions: 0,
            old_is_null: false,
            new_is_null: false,
            first_hunk: None,
        }
    }

    /// The status in V2's own spelling. `/dev/null` on a side says it outright;
    /// otherwise the first hunk's zero-length range carries the same fact —
    /// OpenCode renders an add or delete with both headers naming the file
    /// (`createTwoFilesPatch(file, file, "", content)`), so `@@ -0,0 +1,N @@`
    /// is an addition and `@@ -1,N +0,0 @@` a deletion.
    fn status(&self) -> &'static str {
        if self.old_is_null {
            return "added";
        }
        if self.new_is_null {
            return "deleted";
        }
        match self.first_hunk.as_deref() {
            Some(hunk) if hunk.starts_with("@@ -0,0 ") => "added",
            Some(hunk) if hunk.contains(" +0,0 ") => "deleted",
            _ => "modified",
        }
    }

    fn into_diff_file(mut self) -> TurnDiffFile {
        // The joined `metadata.files[].patch` strings leave a blank line
        // between files; it is no part of any file's patch.
        let patch = self.patch.trim_end_matches('\n').to_string() + "\n";
        let status = self.status().to_string();
        TurnDiffFile {
            path: self.path.take().unwrap_or_default(),
            patch,
            additions: self.additions,
            deletions: self.deletions,
            status,
        }
    }
}

/// Split one recorded patch into its per-file slices, keeping each patch's
/// text verbatim. A patch can carry several files (`apply_patch`/`patch`, and
/// V2's joined `metadata.files[].patch` strings), so the boundaries are read
/// the way the Tool Panel's parser reads them: an `Index:` line, a
/// `diff --git` marker or a `===` separator starts a new block, and a
/// `---`/`+++` pair names its file only before the first `@@` — past that any
/// `+`/`-` line is hunk content.
///
/// Deliberately not `tool_render::parse_diff`: that parser returns ONE
/// aggregated panel body with the tool's trailing note folded out, plus a
/// deduped path list — it discards the per-file boundary and the add/delete
/// status this view needs, and its tail/header rules are tuned to one panel.
/// The pieces that ARE shareable are reused instead ([`diff_header_path`],
/// [`recorded_diff`]); extending `parse_diff` to emit per-file slices would
/// rewrite the panel's delicate parser for this ticket only.
fn split_patch(patch: &str) -> Vec<TurnDiffFile> {
    let lines = patch
        .lines()
        .map(|raw| raw.strip_suffix('\r').unwrap_or(raw))
        .collect::<Vec<_>>();
    let mut splitter = PatchSplitter::default();
    for index in 0..lines.len() {
        splitter.push(&lines, index);
    }
    splitter.finish()
}

#[derive(Default)]
struct PatchSplitter {
    files: Vec<TurnDiffFile>,
    current: Option<PatchFile>,
    in_hunk: bool,
    /// The tool's trailing note has started. It is not part of any file's
    /// patch: its `-`/`+` lines are diagnostics (or prose), not code changes,
    /// so nothing after this point is counted or emitted.
    in_tail: bool,
    /// A blank line has already been evaluated for the current blank run:
    /// only the run's FIRST blank owns the `opens_file_block` lookahead.
    blank_run_seen: bool,
    saw_old_header: bool,
}

impl PatchSplitter {
    fn start(&mut self, path: Option<String>, path_from_index: bool) {
        self.flush();
        self.current = Some(PatchFile::new(path, path_from_index));
        self.in_hunk = false;
        self.in_tail = false;
        self.blank_run_seen = false;
        self.saw_old_header = false;
    }

    fn flush(&mut self) {
        if let Some(file) = self.current.take()
            && file.path.is_some()
        {
            self.files.push(file.into_diff_file());
        }
    }

    fn push(&mut self, lines: &[&str], index: usize) {
        let line = lines[index];
        if self.in_tail {
            // Everything after the note's blank boundary is the note, even a
            // line that looks like a diff header (the panel parser's rule).
            return;
        }
        if line.is_empty() {
            // A unified hunk cannot hold a truly empty line (a blank source
            // line is the context line `" "`), so a blank line inside a hunk
            // ends it. It is either the separator before a joined patch's next
            // file — `opens_file_block` carries the panel parser's evidence —
            // or the start of the tool's trailing note. Neither is patch
            // content, so the line itself is dropped too.
            //
            // Only the first blank of a run asks for that evidence: the
            // helper skips blanks and reads the same next non-empty lines for
            // every blank in the run, so asking per blank would rescan the run
            // once per blank — quadratic on a long blank run.
            if self.in_hunk && !self.blank_run_seen && !opens_file_block(lines, index + 1) {
                self.in_tail = true;
            }
            self.blank_run_seen = true;
            return;
        }
        self.blank_run_seen = false;
        if let Some(path) = line.strip_prefix("Index: ") {
            self.start(Some(path.trim().to_string()), true);
            self.line(line);
            return;
        }
        if line.starts_with("diff --git ") {
            self.start(None, false);
            self.line(line);
            return;
        }
        if line.starts_with("===") {
            // A separator after a file's headers/hunks opens the next file; the
            // one between `Index:` and `---` belongs to the block it follows.
            let opens_a_file = self
                .current
                .as_ref()
                .is_some_and(|file| file.first_hunk.is_some() || self.saw_old_header);
            if self.current.is_none() || opens_a_file {
                self.start(None, false);
            }
            self.in_hunk = false;
            self.line(line);
            return;
        }
        if !self.in_hunk && line.starts_with("--- ") {
            if self.saw_old_header || self.current.is_none() {
                self.start(None, false);
            }
            if let Some(file) = self.current.as_mut() {
                self.saw_old_header = true;
                if header_is_null(line, "--- ") {
                    file.old_is_null = true;
                } else if file.path.is_none()
                    && let Some(path) = diff_header_path(line)
                {
                    file.path = Some(path);
                }
            }
            self.line(line);
            return;
        }
        if !self.in_hunk && line.starts_with("+++ ") {
            if self.current.is_none() {
                self.start(None, false);
            }
            if let Some(file) = self.current.as_mut() {
                if header_is_null(line, "+++ ") {
                    file.new_is_null = true;
                } else if !file.path_from_index
                    && let Some(path) = diff_header_path(line)
                {
                    // Without an `Index:`, the new side names the file a
                    // rename/move landed on.
                    file.path = Some(path);
                }
            }
            self.line(line);
            return;
        }
        if line.starts_with("@@")
            && let Some(file) = self.current.as_mut()
        {
            if file.first_hunk.is_none() {
                file.first_hunk = Some(line.to_string());
            }
            self.in_hunk = true;
        } else if self.in_hunk
            && let Some(file) = self.current.as_mut()
        {
            if line.starts_with('+') {
                file.additions += 1;
            } else if line.starts_with('-') {
                file.deletions += 1;
            }
        }
        self.line(line);
    }

    fn line(&mut self, line: &str) {
        if let Some(file) = self.current.as_mut() {
            file.patch.push_str(line);
            file.patch.push('\n');
        }
    }

    fn finish(mut self) -> Vec<TurnDiffFile> {
        self.flush();
        self.files
    }
}

/// Whether a `---`/`+++` header names the empty side of an add/delete.
fn header_is_null(line: &str, marker: &str) -> bool {
    line.strip_prefix(marker)
        .map(|rest| rest.split('\t').next().unwrap_or(rest).trim() == "/dev/null")
        .unwrap_or(false)
}

/// A `write` call's full-file patch, built from the call's input because
/// neither generation records one on a successful `write`: V1 puts the diff on
/// the permission ask only, V2's `write` returns no diff metadata at all. The
/// synthesized patch shows the file's whole content against an empty old side
/// (an addition).
///
/// **Best-effort status.** V1's `metadata.exists` says whether the file was
/// overwritten (`modified`) or created (`added`). V2 sends no `exists` and no
/// diff, so a V2 overwrite is indistinguishable from a create here and is
/// labelled `added`; there is no reliable in-call signal without parsing the
/// model-facing output text, which this neutral view deliberately does not do.
/// The patch body still shows the file's final content, which is what the
/// reader needs; only the one-word label can overstate a V2 overwrite.
fn write_diff(call: &ToolCall) -> Option<TurnDiffFile> {
    let input = call.input.as_ref()?;
    let path = input
        .get("filePath")
        .or_else(|| input.get("path"))
        .and_then(|path| path.as_str())
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    let content = input.get("content")?.as_str()?;
    let lines = content
        .split_terminator('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>();
    let range = if lines.is_empty() {
        "@@ -0,0 +0,0 @@".to_string()
    } else {
        format!("@@ -0,0 +1,{} @@", lines.len())
    };
    let mut patch = format!(
        "Index: {path}\n===================================================================\n--- {path}\n+++ {path}\n{range}\n"
    );
    for line in &lines {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    let existed = call
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("exists"))
        .and_then(|exists| exists.as_bool());
    Some(TurnDiffFile {
        path: path.to_string(),
        patch,
        additions: lines.len(),
        deletions: 0,
        status: if existed == Some(true) {
            "modified"
        } else {
            "added"
        }
        .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        MessageId, MessageRole, MessageTime, Part, ToolIdentity, ToolOutput, ToolStatus, TranscriptMessage,
    };
    use serde_json::{Value, json};

    const ANCHOR_MS: i64 = 1_000;

    fn anchor() -> TurnAnchor {
        TurnAnchor {
            message_id: MessageId::new("msg_user"),
            created_ms: ANCHOR_MS,
        }
    }

    /// One completed tool call as the decoder hands it to the Bridge.
    fn call(name: &str, metadata: Option<Value>) -> ToolCall {
        ToolCall {
            identity: ToolIdentity {
                name: name.to_string(),
                call_id: format!("call_{name}"),
            },
            status: ToolStatus::Completed,
            started_at: Some(ANCHOR_MS + 10),
            input: None,
            metadata,
            output: ToolOutput::default(),
        }
    }

    /// A transcript whose anchored Turn carries `calls` on one assistant
    /// message.
    fn transcript(calls: Vec<ToolCall>) -> SessionTranscript {
        SessionTranscript::new(vec![
            TranscriptMessage {
                id: MessageId::new("msg_user"),
                role: MessageRole::User,
                time: Some(MessageTime {
                    created: ANCHOR_MS,
                    completed: Some(ANCHOR_MS),
                }),
                model: None,
                tokens: None,
                error: None,
                parts: Vec::new(),
            },
            TranscriptMessage {
                id: MessageId::new("msg_assistant"),
                role: MessageRole::Assistant,
                time: Some(MessageTime {
                    created: ANCHOR_MS + 10,
                    completed: Some(ANCHOR_MS + 20),
                }),
                model: None,
                tokens: None,
                error: None,
                parts: calls.into_iter().map(Part::Tool).collect(),
            },
        ])
    }

    fn paths(diff: &TurnDiff) -> Vec<&str> {
        diff.0.iter().map(|file| file.path.as_str()).collect()
    }

    #[test]
    fn one_edit_yields_its_file_and_patch_counts() {
        let patch = "\
Index: src/main.rs
===================================================================
--- src/main.rs
+++ src/main.rs
@@ -1,2 +1,3 @@
 a
-b
+c
+d
";
        let diff = turn_diff_from_tools(
            &transcript(vec![call("edit", Some(json!({ "diff": patch })))]),
            &anchor(),
        );

        assert_eq!(paths(&diff), ["src/main.rs"]);
        assert_eq!(diff.0[0].additions, 2);
        assert_eq!(diff.0[0].deletions, 1);
        assert_eq!(diff.0[0].status, "modified");
        assert_eq!(diff.0[0].patch, patch, "the patch stays verbatim for the HTML");
        assert_eq!(
            diff.stats(),
            crate::backend::TurnDiffStats {
                files: 1,
                additions: 2,
                deletions: 1,
            }
        );
    }

    /// The Turn's final recorded state for a path: two edits of the same file
    /// yield ONE entry carrying the later patch, and the index folds that entry
    /// once — neither the file count nor the line totals double.
    #[test]
    fn a_file_edited_twice_keeps_only_its_later_patch() {
        let first = "\
Index: src/main.rs
===================================================================
--- src/main.rs
+++ src/main.rs
@@ -1 +1 @@
-old
+first
";
        let second = "\
Index: src/main.rs
===================================================================
--- src/main.rs
+++ src/main.rs
@@ -1 +1,2 @@
-first
+second
+third
";
        let diff = turn_diff_from_tools(
            &transcript(vec![
                call("edit", Some(json!({ "diff": first }))),
                call("edit", Some(json!({ "diff": second }))),
            ]),
            &anchor(),
        );

        assert_eq!(paths(&diff), ["src/main.rs"]);
        assert_eq!(diff.0[0].patch, second, "the later patch is the final state");
        assert_eq!((diff.0[0].additions, diff.0[0].deletions), (2, 1));
        assert_eq!(
            diff.stats(),
            crate::backend::TurnDiffStats {
                files: 1,
                additions: 2,
                deletions: 1,
            },
            "the deduped entry is counted once"
        );
    }

    /// Only a Turn that recorded a file edit yields an entry: a Turn that just
    /// read and ran commands leaves the card no index line and no dead button.
    #[test]
    fn a_bash_and_read_only_turn_yields_an_empty_diff() {
        let diff = turn_diff_from_tools(
            &transcript(vec![
                call("bash", Some(json!({ "output": "ok" }))),
                call("read", Some(json!({ "output": "fn main() {}" }))),
            ]),
            &anchor(),
        );

        assert!(diff.is_empty());
        assert_eq!(diff.stats(), crate::backend::TurnDiffStats::default());
    }

    /// `write` is a file edit even though neither generation records a diff on
    /// its successful call: the entry is built from the call's input, and a
    /// new file takes V2's `added` spelling.
    #[test]
    fn a_write_of_a_new_file_yields_an_added_entry() {
        let mut write = call(
            "write",
            Some(json!({ "filepath": "docs/new.md", "exists": false })),
        );
        write.input = Some(json!({
            "filePath": "docs/new.md",
            "content": "hello\nworld\n",
        }));

        let diff = turn_diff_from_tools(&transcript(vec![write]), &anchor());

        assert_eq!(paths(&diff), ["docs/new.md"]);
        assert_eq!(diff.0[0].status, "added");
        assert_eq!((diff.0[0].additions, diff.0[0].deletions), (2, 0));
        assert!(
            diff.0[0].patch.contains("+hello\n+world\n"),
            "the whole new content is the patch: {:?}",
            diff.0[0].patch
        );
    }

    /// The Turn's calls can carry either generation's recorded shape in the
    /// same aggregation: V1's top-level `metadata.diff` and V2's
    /// `metadata.files[].patch` both parse to the same neutral type, so the
    /// caller never branches on a Generation and the index folds one view.
    #[test]
    fn v1_and_v2_metadata_shapes_share_one_neutral_view() {
        let v1 = call(
            "edit",
            Some(json!({
                "diff": "Index: one.rs\n===================================================================\n--- one.rs\n+++ one.rs\n@@ -1 +1 @@\n-a\n+b\n",
            })),
        );
        let v2 = call(
            "patch",
            Some(json!({
                "files": [
                    {
                        "file": "two.rs",
                        "patch": "===================================================================\n--- two.rs\n+++ two.rs\n@@ -1 +1,2 @@\n x\n+y\n",
                    },
                    {
                        "file": "three.rs",
                        "patch": "===================================================================\n--- three.rs\n+++ three.rs\n@@ -1 +1,2 @@\n z\n+w\n",
                    },
                ],
            })),
        );

        let diff = turn_diff_from_tools(&transcript(vec![v1, v2]), &anchor());

        assert_eq!(paths(&diff), ["one.rs", "two.rs", "three.rs"]);
        assert_eq!(
            diff.stats(),
            crate::backend::TurnDiffStats {
                files: 3,
                additions: 3,
                deletions: 1,
            }
        );
    }

    /// One `apply_patch` touching two files yields two entries, each named by
    /// its own `Index:` header — not one joined blob.
    #[test]
    fn an_apply_patch_of_two_files_yields_two_entries() {
        let diff_text = "\
Index: src/a.rs
===================================================================
--- src/a.rs
+++ src/a.rs
@@ -1 +1 @@
-a
+A

Index: src/b.rs
===================================================================
--- src/b.rs
+++ src/b.rs
@@ -1 +1,2 @@
 b
+B
";
        let diff = turn_diff_from_tools(
            &transcript(vec![call("apply_patch", Some(json!({ "diff": diff_text })))]),
            &anchor(),
        );

        assert_eq!(paths(&diff), ["src/a.rs", "src/b.rs"]);
        assert_eq!((diff.0[0].additions, diff.0[0].deletions), (1, 1));
        assert_eq!((diff.0[1].additions, diff.0[1].deletions), (1, 0));
        assert_eq!(
            diff.stats(),
            crate::backend::TurnDiffStats {
                files: 2,
                additions: 2,
                deletions: 1,
            }
        );
    }

    /// `Index:` outranks the `---`/`+++` pair: a rename's two headers name two
    /// sides, while the index line names the entry the patch belongs to.
    #[test]
    fn the_index_path_outranks_the_file_headers() {
        let patch = "\
Index: src/old.rs
===================================================================
--- a/src/old.rs
+++ b/src/new.rs
@@ -1 +1 @@
-a
+b
";
        let diff = turn_diff_from_tools(
            &transcript(vec![call("edit", Some(json!({ "diff": patch })))]),
            &anchor(),
        );

        assert_eq!(paths(&diff), ["src/old.rs"]);
    }

    /// An add and a delete that OpenCode renders with both headers naming the
    /// file (`createTwoFilesPatch(file, file, …)`) still take V2's `added` /
    /// `deleted` statuses, from the hunk's zero-length range.
    #[test]
    fn an_add_and_a_delete_take_v2s_status_spellings() {
        let patch = "\
Index: src/new.rs
===================================================================
--- src/new.rs
+++ src/new.rs
@@ -0,0 +1,2 @@
+one
+two

Index: src/gone.rs
===================================================================
--- src/gone.rs
+++ src/gone.rs
@@ -1,2 +0,0 @@
-one
-two
";
        let diff = turn_diff_from_tools(
            &transcript(vec![call("apply_patch", Some(json!({ "diff": patch })))]),
            &anchor(),
        );

        assert_eq!(paths(&diff), ["src/new.rs", "src/gone.rs"]);
        assert_eq!(diff.0[0].status, "added");
        assert_eq!(diff.0[1].status, "deleted");
    }

    /// A hunk line that happens to start with `---`/`+++` is file CONTENT once
    /// the first `@@` opened the hunk (a Lua/YAML `---` comment is the real
    /// shape): it counts as a removal/addition and never splits the file.
    #[test]
    fn hunk_lines_that_look_like_headers_stay_content() {
        let patch = "\
Index: src/notes.lua
===================================================================
--- src/notes.lua
+++ src/notes.lua
@@ -1,2 +1,2 @@
--- a lua comment
--- an older comment
+++ a lua comment
";
        let diff = turn_diff_from_tools(
            &transcript(vec![call("edit", Some(json!({ "diff": patch })))]),
            &anchor(),
        );

        assert_eq!(paths(&diff), ["src/notes.lua"]);
        assert_eq!((diff.0[0].additions, diff.0[0].deletions), (1, 2));
    }

    /// The V1 edit shape the Tool Panel already knows: after the hunks, a blank
    /// line then the LSP diagnostic note, whose `-`/`+` lines look like hunk
    /// content. The note is neither patch content nor a code change, so the
    /// artifact does not show it and #477's stats do not count it.
    #[test]
    fn a_trailing_lsp_note_is_not_part_of_the_patch_or_its_counts() {
        let patch = "\
Index: a.rs
===================================================================
--- a.rs
+++ a.rs
@@ -1 +1 @@
-old
+new

LSP errors detected in a.rs, please fix:
- a diagnostic that starts like a removal
+ a diagnostic that starts like an addition";
        let diff = turn_diff_from_tools(
            &transcript(vec![call("edit", Some(json!({ "diff": patch })))]),
            &anchor(),
        );

        assert_eq!(paths(&diff), ["a.rs"]);
        assert_eq!((diff.0[0].additions, diff.0[0].deletions), (1, 1));
        assert!(
            !diff.0[0].patch.contains("LSP") && !diff.0[0].patch.contains("diagnostic"),
            "the note must not reach the artifact's patch: {:?}",
            diff.0[0].patch
        );
        assert_eq!(
            diff.0[0].patch.trim_end(),
            "Index: a.rs\n===================================================================\n--- a.rs\n+++ a.rs\n@@ -1 +1 @@\n-old\n+new",
            "the file's patch ends with its last hunk line"
        );
        assert_eq!(
            diff.stats(),
            crate::backend::TurnDiffStats {
                files: 1,
                additions: 1,
                deletions: 1,
            },
            "the index folds the note-free counts"
        );
    }

    /// A blank line between joined per-file patches is a file boundary, not the
    /// note's start: the panel parser's `opens_file_block` evidence keeps the
    /// second file (and its counts) intact.
    #[test]
    fn a_blank_between_joined_files_does_not_swallow_the_second_file() {
        let first = "===================================================================\n--- a.rs\n+++ a.rs\n@@ -1 +1 @@\n-a\n+A\n";
        let second = "===================================================================\n--- b.rs\n+++ b.rs\n@@ -1 +1,2 @@\n b\n+B\n";
        let diff = turn_diff_from_tools(
            &transcript(vec![call(
                "patch",
                Some(json!({ "files": [
                    { "file": "a.rs", "patch": first },
                    { "file": "b.rs", "patch": second },
                ] })),
            )]),
            &anchor(),
        );

        assert_eq!(paths(&diff), ["a.rs", "b.rs"]);
        assert_eq!((diff.0[0].additions, diff.0[0].deletions), (1, 1));
        assert_eq!((diff.0[1].additions, diff.0[1].deletions), (1, 0));
        assert_eq!(
            diff.stats(),
            crate::backend::TurnDiffStats {
                files: 2,
                additions: 2,
                deletions: 1,
            }
        );
    }

    /// A run of blank lines is one boundary, not one per line: the first
    /// blank's `opens_file_block` verdict covers the run, so a joined file
    /// after several blanks still survives, and a note after several blanks is
    /// still excluded.
    #[test]
    fn a_blank_run_keeps_the_joined_file_and_still_ends_at_the_note() {
        let patch = "\
===================================================================
--- a.rs
+++ a.rs
@@ -1 +1 @@
-a
+A



===================================================================
--- b.rs
+++ b.rs
@@ -1 +1,2 @@
 b
+B


LSP errors detected in b.rs, please fix:
- a diagnostic that starts like a removal
+ a diagnostic that starts like an addition";
        let diff = turn_diff_from_tools(
            &transcript(vec![call("patch", Some(json!({ "diff": patch })))]),
            &anchor(),
        );

        assert_eq!(paths(&diff), ["a.rs", "b.rs"]);
        assert_eq!((diff.0[0].additions, diff.0[0].deletions), (1, 1));
        assert_eq!((diff.0[1].additions, diff.0[1].deletions), (1, 0));
        assert!(
            !diff.0[1].patch.contains("LSP") && !diff.0[1].patch.contains("diagnostic"),
            "the note after a blank run stays out: {:?}",
            diff.0[1].patch
        );
        assert_eq!(
            diff.stats(),
            crate::backend::TurnDiffStats {
                files: 2,
                additions: 2,
                deletions: 1,
            }
        );
    }

    /// V2's `write` input spells the path `path` and records no `exists` (and
    /// no diff), so the entry takes `added`. This is also the best-effort
    /// limit pinned down: a V2 OVERWRITE is indistinguishable here and gets the
    /// same `added` label; only V1's `metadata.exists` can say `modified`.
    #[test]
    fn a_v2_write_of_a_new_file_yields_an_added_entry() {
        let mut write = call("write", None);
        write.input = Some(json!({ "path": "src/fresh.rs", "content": "fn fresh() {}\n" }));

        let diff = turn_diff_from_tools(&transcript(vec![write]), &anchor());

        assert_eq!(paths(&diff), ["src/fresh.rs"]);
        assert_eq!(diff.0[0].status, "added");
        assert_eq!((diff.0[0].additions, diff.0[0].deletions), (1, 0));
    }

    /// A call that failed did not apply its change, so a recorded permission
    /// patch on it must not surface as an edit the Turn made.
    #[test]
    fn a_failed_edit_contributes_no_entry() {
        let mut failed = call(
            "edit",
            Some(json!({
                "diff": "Index: src/bad.rs\n===================================================================\n--- src/bad.rs\n+++ src/bad.rs\n@@ -1 +1 @@\n-a\n+b\n",
            })),
        );
        failed.status = ToolStatus::Error;

        let diff = turn_diff_from_tools(&transcript(vec![failed]), &anchor());

        assert!(diff.is_empty());
    }

    /// A live call can already carry the patch it is asking permission for:
    /// V1's `edit` records `metadata.diff` on the permission ask, before the
    /// request is answered and before any file changed. Neither the aggregation
    /// nor the index may expose it until the call settles; the same call once
    /// completed contributes, and the stats fold follows.
    #[test]
    fn a_live_edit_contributes_only_once_it_settles() {
        let patch = "\
Index: src/main.rs
===================================================================
--- src/main.rs
+++ src/main.rs
@@ -1 +1 @@
-a
+b
";
        let mut edit = call("edit", Some(json!({ "diff": patch })));

        for status in [ToolStatus::Pending, ToolStatus::Running] {
            edit.status = status;
            let live = turn_diff_from_tools(&transcript(vec![edit.clone()]), &anchor());
            assert!(live.is_empty(), "a live call has no applied change: {live:?}");
            assert_eq!(
                live.stats(),
                crate::backend::TurnDiffStats::default(),
                "the index folds the same filtered view"
            );
        }

        edit.status = ToolStatus::Completed;
        let settled = turn_diff_from_tools(&transcript(vec![edit]), &anchor());
        assert_eq!(paths(&settled), ["src/main.rs"]);
        assert_eq!(
            settled.stats(),
            crate::backend::TurnDiffStats {
                files: 1,
                additions: 1,
                deletions: 1,
            }
        );
    }

    /// A status the decoder could not classify is not live: `Unknown` and a
    /// non-failed `Other` are settled by the transcript's own `is_live` /
    /// `is_failure` verdicts, so a recorded patch on one still counts.
    #[test]
    fn an_unclassified_but_settled_status_still_counts() {
        let patch = "\
Index: src/main.rs
===================================================================
--- src/main.rs
+++ src/main.rs
@@ -1 +1 @@
-a
+b
";
        for status in [ToolStatus::Unknown, ToolStatus::Other("done".into())] {
            let mut edit = call("edit", Some(json!({ "diff": patch })));
            edit.status = status;
            let diff = turn_diff_from_tools(&transcript(vec![edit]), &anchor());
            assert_eq!(paths(&diff), ["src/main.rs"]);
        }
    }

    /// Overwriting an existing file is still an edit; V1's `metadata.exists`
    /// keeps the label V2 would report (`modified`) even though the generated
    /// patch has no before-side to diff.
    #[test]
    fn a_write_over_an_existing_file_is_modified() {
        let mut write = call(
            "write",
            Some(json!({ "filepath": "src/main.rs", "exists": true })),
        );
        write.input = Some(json!({ "filePath": "src/main.rs", "content": "fn main() {}\n" }));

        let diff = turn_diff_from_tools(&transcript(vec![write]), &anchor());

        assert_eq!(paths(&diff), ["src/main.rs"]);
        assert_eq!(diff.0[0].status, "modified");
    }
}
