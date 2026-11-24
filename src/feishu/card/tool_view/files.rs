//! The **FileEdits** family (ADR-0105, spec #1057's P3): a call that touched
//! files, each with its action and diff. Selection is `(kind, detail)` — never
//! the vendor name — so `edit`, `write`, `patch`, `apply_patch` and
//! `codex/file_change` reach one view in one SHAPE, and a third adapter inherits
//! it by declaring the same detail.

use std::borrow::Cow;

use crate::backend::{FileAction, FileEdit};

use super::super::budget::{self, SurfaceBudget};
use super::super::clip;
use super::super::tool_render::{BodyStyle, ToolPanel, diff_language_for, json_escaped_len, summary_tail};
use super::ToolView;

/// The merged edit list a FileEdits call renders. The adapters already merge
/// duplicate paths; a third adapter may not, so the card re-merges only when it
/// actually sees a repeat and otherwise borrows the detail (the common path
/// costs no clone).
///
/// A Move kind is admitted because ACP declares it; [`FileAction`] has no Move,
/// so the detail's own action (Rename for a move, Update for an in-place edit)
/// is the word shown.
pub(crate) fn file_edits(edits: &[FileEdit]) -> Cow<'_, [FileEdit]> {
    if has_duplicate_paths(edits) {
        Cow::Owned(crate::backend::merge_file_edits(edits.iter().cloned()))
    } else {
        Cow::Borrowed(edits)
    }
}

fn has_duplicate_paths(edits: &[FileEdit]) -> bool {
    edits
        .iter()
        .enumerate()
        .any(|(index, edit)| edits[..index].iter().any(|earlier| earlier.path == edit.path))
}

/// The `#947` action word for one file: add/update/delete/rename, never
/// inferred from a hunk (a pure rename or a binary change has none).
fn file_action_word(action: FileAction) -> &'static str {
    match action {
        FileAction::Add => "➕ 新增",
        FileAction::Update => "✏️ 编辑",
        FileAction::Delete => "🗑 删除",
        FileAction::Rename => "📦 重命名",
    }
}

/// The file a [`FileEdit`] names to the reader: a rename's destination (the
/// file that now exists), otherwise its own path.
pub(crate) fn file_edit_display_path(edit: &FileEdit) -> &str {
    match edit.action {
        FileAction::Rename => edit.rename_to.as_deref().unwrap_or(&edit.path),
        _ => &edit.path,
    }
}

/// One file's action, with each path formatted by the caller: inline code on
/// the Input line, the folded short name on the title. A rename names both
/// ends.
fn file_edit_action_with(edit: &FileEdit, path: impl Fn(&str) -> String) -> String {
    let word = file_action_word(edit.action);
    match edit.action {
        FileAction::Rename => match edit.rename_to.as_deref().filter(|to| !to.is_empty()) {
            Some(to) => format!("{} {} → {}", word, path(&edit.path), path(to)),
            None => format!("{} {}", word, path(&edit.path)),
        },
        _ => format!("{} {}", word, path(&edit.path)),
    }
}

/// One file's action on the Input line (#947): the word, then the path (or a
/// rename's source → destination). The Input line is where the file is already
/// named, so this is where the action belongs for a list.
fn file_edit_action_line(edit: &FileEdit) -> String {
    file_edit_action_with(edit, super::super::tool_render::inline_code)
}

/// The folded title's fragment for one file: the action word and the short
/// name (a rename names both ends). The title is one `plain_text` line, so a
/// deep path keeps its tail; several files keep the count/totals fragment
/// ([`super::super::tool_render::multi_file_detail`]) instead.
pub(crate) fn file_edit_title(edit: &FileEdit) -> String {
    file_edit_action_with(edit, super::super::tool_render::header_file_name)
}

/// The Input line a FileEdits panel renders: one action line per touched file
/// (#947), so a multi-file patch names every file's action rather than only
/// its first path. Ticket #1060: several files keep the count and totals
/// fragment — the per-file actions belong in the body.
pub(crate) fn file_edits_input(edits: &[FileEdit], failed: bool) -> String {
    if edits.len() > 1 {
        if failed {
            return format!("📄 {} files", edits.len());
        }
        let (additions, deletions) = file_edits_totals(edits);
        return format!("📄 {} files · +{additions} −{deletions}", edits.len());
    }
    edits
        .iter()
        .map(file_edit_action_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The additions and deletions every file's hunks record: `+`/`-` lines, as a
/// unified diff writes them. One count for the totals the panel heads and the
/// folded title carries, so they cannot disagree.
pub(crate) fn file_edits_totals(edits: &[FileEdit]) -> (usize, usize) {
    let mut additions = 0;
    let mut deletions = 0;
    for diff in edits.iter().filter_map(|edit| edit.diff.as_deref()) {
        for line in diff.lines() {
            if line.starts_with('+') {
                additions += 1;
            } else if line.starts_with('-') {
                deletions += 1;
            }
        }
    }
    (additions, deletions)
}

/// One file's section in a FileEdits panel: its action line (markdown, outside
/// any fence) and its own hunks, so a multi-file body keeps every action next
/// to the change it names.
pub(crate) struct FileEditSection {
    pub(crate) action: Option<String>,
    pub(crate) hunks: String,
}

/// The pieces a FileEdits panel composes: per-file sections (action + hunks)
/// and the two trailing notes the ticket keeps apart — a `write`'s LSP note
/// rides after the fence (pre-P3 #788), an edit/patch note stays inside it.
pub(crate) struct FileEditsBody {
    pub(crate) header: Option<String>,
    pub(crate) sections: Vec<FileEditSection>,
    pub(crate) lang: &'static str,
    outside_tail: Option<String>,
    inside_tail: Option<String>,
}

/// The FileEdits panel's pieces, unclipped. `None` for a FAILED call (its error
/// text is the body) and for a list with no hunk and no action line to show.
pub(crate) fn file_edits_body(tool: &ToolPanel) -> Option<FileEditsBody> {
    let ToolView::FileEdits(edits) = ToolView::of(tool) else {
        return None;
    };
    if ToolView::of(tool).failed(tool) {
        return None;
    }
    let multi = edits.len() > 1;
    let mut sections: Vec<FileEditSection> = Vec::new();
    for edit in edits.iter() {
        let hunks = edit
            .diff
            .as_deref()
            .filter(|diff| !diff.trim().is_empty())
            .unwrap_or_default()
            .to_string();
        let action = (multi || hunks.is_empty()).then(|| file_edit_action_line(edit));
        if action.is_some() || !hunks.is_empty() {
            sections.push(FileEditSection { action, hunks });
        }
    }
    if sections.is_empty() {
        return None;
    }
    let lang = diff_language_for(edits.iter().map(file_edit_display_path));
    let header = match edits.as_ref() {
        [only] => only.diff.as_deref().map(|_| {
            let (additions, deletions) = file_edits_totals(std::slice::from_ref(only));
            format!("+{additions} −{deletions}")
        }),
        _ => None,
    };
    let tail = file_edits_tail(tool);
    // A `write`'s note (its own success summary precedes it) belonged outside
    // the fence before P3 (#788), under its own 500-char bound. Any other
    // producer's note stays inside, under the diff budget; an Add alone is not
    // enough — an `apply_patch` Add keeps its note inside.
    let (outside_tail, inside_tail) =
        if edits.len() == 1 && edits[0].action == FileAction::Add && is_write_output(tool) {
            (
                tail.map(|tail| clip::clip(&tail).cut(SurfaceBudget::chars(WRITE_TAIL_MAX_CHARS))),
                None,
            )
        } else {
            (None, tail)
        };
    Some(FileEditsBody {
        header,
        sections,
        lang,
        outside_tail,
        inside_tail,
    })
}

impl FileEditsBody {
    /// The markdown body the panel writes, with every file's action line
    /// immediately before its own fence. Each hunk block and the action list
    /// share [`budget::DIFF_FENCED`]: the action list stops growing when
    /// the budget is spent, a hunk block clips to the remainder, and the
    /// returned flag tells the caller its truncation note is owed.
    pub(crate) fn render(&self) -> (String, bool) {
        let mut body = String::new();
        let mut clipped = false;
        let mut inside_tail = self.inside_tail.as_deref();
        let last_section = self.sections.len().checked_sub(1);
        for (index, section) in self.sections.iter().enumerate() {
            let action = section.action.as_deref();
            if let Some(action) = action {
                let separator = if body.is_empty() { 0 } else { 2 };
                if json_escaped_len(&body) + separator + json_escaped_len(action) > budget::DIFF_FENCED.max {
                    clipped = true;
                    break;
                }
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str(action);
            }
            let mut hunks = section.hunks.clone();
            if Some(index) == last_section
                && let Some(tail) = inside_tail.take()
            {
                if !hunks.is_empty() {
                    hunks.push('\n');
                }
                hunks.push_str(tail);
            }
            if hunks.is_empty() {
                continue;
            }
            let separator = if body.is_empty() { 0 } else { 2 };
            let remaining = budget::DIFF_FENCED
                .max
                .saturating_sub(json_escaped_len(&body) + separator);
            let clipped_hunks =
                clip::clip(&hunks).cut_fenced(Some(self.lang), SurfaceBudget::fenced(remaining));
            if clipped_hunks.is_empty() {
                clipped = true;
                break;
            }
            if clipped_hunks != hunks {
                clipped = true;
            }
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&super::super::fenced_code(&clipped_hunks, Some(self.lang)));
        }
        if let Some(tail) = &self.outside_tail {
            let separator = if body.is_empty() { 0 } else { 2 };
            let remaining = budget::DIFF_FENCED
                .max
                .saturating_sub(json_escaped_len(&body) + separator);
            if remaining > 0 {
                let original = tail.chars().count();
                let tail = clip::clip(tail).cut(SurfaceBudget::chars(remaining));
                if tail.chars().count() < original {
                    clipped = true;
                }
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str(&tail);
            } else {
                clipped = true;
            }
        }
        (body, clipped)
    }

    pub(crate) fn clipped(&self) -> bool {
        self.render().1
    }
}

/// Whether the call's output carries a `write` tool's success summary (V1's
/// `Wrote file successfully.`, V2's `Wrote/Created file successfully: …`).
/// This is the producer's OUTPUT shape, not its id: an `apply_patch` Add must
/// keep its LSP note inside the diff fence.
fn is_write_output(tool: &ToolPanel) -> bool {
    let Some(text) = tool.output() else {
        return false;
    };
    let first = text.lines().next().unwrap_or("").trim_start();
    [
        "Wrote file successfully",
        "Created file successfully",
        "Successfully wrote",
    ]
    .iter()
    .any(|prefix| first.starts_with(prefix))
}

/// The FileEdits panel's Output: markdown action lines outside the fence, the
/// hunks inside it, already clipped under the diff byte budget. `BodyStyle`
/// is `Markdown` because the body is composed: the generic renderer must not
/// wrap the action lines in a second fence.
pub(crate) fn file_edits_output(tool: &ToolPanel) -> Option<super::ToolBody> {
    let body = file_edits_body(tool)?;
    let (text, _) = body.render();
    if text.trim().is_empty() {
        return None;
    }
    Some((body.header, BodyStyle::Markdown, text))
}

/// The tool's own trailing note after its success summary (V1's LSP
/// diagnostics). `None` for a successful call with no note.
pub(crate) fn file_edits_tail(tool: &ToolPanel) -> Option<String> {
    if ToolView::of(tool).failed(tool) {
        return None;
    }
    tool.output()
        .as_deref()
        .and_then(summary_tail)
        .map(|tail| tail.trim_end().to_string())
        .filter(|tail| !tail.is_empty())
}

/// The cap on a `write` panel's LSP note, restored to pre-P3's placement
/// outside the fence (#788): the note is a preview, so it keeps its own bound
/// rather than riding the diff budget.
const WRITE_TAIL_MAX_CHARS: usize = 500;
