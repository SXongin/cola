//! The Background Task Ledger section (ADR-0060).
//!
//! The card-tail section that lists a Session's live Background Tasks — task
//! type, label, elapsed — riding the newest card of its Card Chain like the
//! Todo Panel and the live Tool Panels. The Bridge gathers the facts (which
//! tasks are live, what label their originating tool part's input names) and
//! hands them over as [`TaskLedgerRow`]s; this module owns the pinned copy,
//! the elapsed format and nothing else.
//!
//! Empty means no section: a card with no live task renders exactly what it
//! did before, and V1 (which carries no Background Task facts) never shows
//! one.

use super::truncate_md;

/// Characters of a task label the ledger row shows before clipping — shared
/// with the completion receipt (`🔔 后台任务完成：<label>`), so the two
/// renderings of one label clip identically (ADR-0060).
pub(crate) const TASK_LABEL_CHARS: usize = 60;

/// The kind of Background Task a ledger row names. Only these two background
/// through the V2 tool shape (ADR-0059/0060).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Subagent,
}

impl TaskKind {
    /// The row's type noun: `shell` stays the tool's own name (the pinned
    /// copy), the subagent reads in Chinese like the completion receipt's
    /// `子代理完成`.
    fn noun(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Subagent => "子代理",
        }
    }
}

/// One live Background Task as its ledger row renders it: the task's type, the
/// label joined from the originating tool part's input by `call_id` (`None`
/// when that input names no label — the row then renders bare), and when the
/// run started (`None` renders no elapsed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLedgerRow {
    pub kind: TaskKind,
    pub label: Option<String>,
    pub started_at: Option<i64>,
}

/// The ledger section's markdown text — the pinned copy:
///
/// ```text
/// ⏳ 后台任务（N）
/// · shell：<label> · 3m12s
/// · 子代理：<description> · 1m05s
/// ```
///
/// `None` when no task is live (no empty header). Rows render in the order
/// given (the transcript's own), one line each: a label's newlines fold to
/// spaces so a multi-line command cannot break the row layout, and the label
/// is clipped like the completion receipt. The caller sanitizes the text like
/// any other model-authored markdown.
pub(crate) fn task_ledger_text(rows: &[TaskLedgerRow], now_ms: i64) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let mut text = format!("⏳ 后台任务（{}）", rows.len());
    for row in rows {
        text.push('\n');
        text.push_str("· ");
        text.push_str(row.kind.noun());
        if let Some(label) = row.label.as_deref().filter(|label| !label.is_empty()) {
            let label = label.replace(['\n', '\r'], " ");
            text.push('：');
            text.push_str(&truncate_md(&label, TASK_LABEL_CHARS));
        }
        if let Some(at) = row.started_at {
            text.push_str(&format!(" · {}", fmt_task_elapsed(secs_since(at, now_ms))));
        }
    }
    Some(text)
}

/// Estimated serialized size (bytes) of the ledger section, for the card
/// splitter's tail reserve: the header, one row per task with its clipped
/// label and a short elapsed tail, plus the element overhead. Rough like the
/// Bridge's `panel_estimate` for the tail's other sections, but owned here so
/// the estimate and [`task_ledger_text`] cannot drift apart — both clip a
/// label at [`TASK_LABEL_CHARS`].
pub(crate) fn task_ledger_estimate(rows: &[TaskLedgerRow]) -> usize {
    let first_n_bytes = |s: &str, n: usize| s.chars().take(n).map(|c| c.len_utf8()).sum::<usize>();
    let labels: usize = rows
        .iter()
        .filter_map(|row| row.label.as_deref())
        .map(|label| first_n_bytes(label, TASK_LABEL_CHARS))
        .sum();
    300 + labels + rows.len() * 120
}

/// The ledger row's elapsed: bare, with no Chinese label (ADR-0060) — `3m12s`,
/// `1m05s` under an hour, `1h05m` above it. Seconds are zero-padded so the
/// rows stay visually aligned; the minutes above an hour are too.
fn fmt_task_elapsed(secs: u64) -> String {
    if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Seconds between two epoch-ms clocks, never negative (a clock skewed into
/// the future clamps to `0m00s` instead of going negative).
fn secs_since(at_ms: i64, now_ms: i64) -> u64 {
    ((now_ms - at_ms).max(0) / 1000) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned copy (ADR-0060, #412): one row per task, type noun, label
    /// and bare elapsed — `3m12s` / `1m05s` for this pair.
    #[test]
    fn the_section_renders_the_pinned_copy() {
        let now = 1_800_000_000_000;
        let rows = vec![
            TaskLedgerRow {
                kind: TaskKind::Shell,
                label: Some("gh run watch".into()),
                started_at: Some(now - 192_000),
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some("review the diff".into()),
                started_at: Some(now - 65_000),
            },
        ];
        assert_eq!(
            task_ledger_text(&rows, now).unwrap(),
            "⏳ 后台任务（2）\n· shell：gh run watch · 3m12s\n· 子代理：review the diff · 1m05s"
        );
    }

    /// No live task means no section — the empty header never renders, so a
    /// card (and V1, which has no Background Task facts) is unchanged.
    #[test]
    fn an_empty_ledger_renders_nothing() {
        assert_eq!(task_ledger_text(&[], 1_800_000_000_000), None);
    }

    /// A task whose input names no label renders bare (type + elapsed only),
    /// and a task with no start time shows no elapsed at all.
    #[test]
    fn a_label_less_task_renders_bare() {
        let now = 1_800_000_000_000;
        let rows = vec![
            TaskLedgerRow {
                kind: TaskKind::Shell,
                label: None,
                started_at: Some(now - 5_000),
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some(String::new()),
                started_at: None,
            },
        ];
        assert_eq!(
            task_ledger_text(&rows, now).unwrap(),
            "⏳ 后台任务（2）\n· shell · 0m05s\n· 子代理"
        );
    }

    /// The label clips exactly like the completion receipt: at
    /// [`TASK_LABEL_CHARS`] characters plus the `…` marker, and a multi-line
    /// command folds to one row.
    #[test]
    fn labels_clip_like_the_receipt() {
        let now = 1_800_000_000_000;
        let long = "x".repeat(TASK_LABEL_CHARS + 20);
        let rows = vec![TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some(format!("{long}\nsecond line")),
            started_at: Some(now),
        }];
        let text = task_ledger_text(&rows, now).unwrap();
        let row = text.lines().nth(1).unwrap();
        assert_eq!(row, format!("· shell：{}… · 0m00s", "x".repeat(TASK_LABEL_CHARS)));
        assert!(!text.contains("second line"), "one task, one row: {text}");
    }

    /// Elapsed is bare and minute-granular past an hour: `Xh Ym`, zero-padded.
    #[test]
    fn elapsed_switches_to_hours_at_an_hour() {
        assert_eq!(fmt_task_elapsed(0), "0m00s");
        assert_eq!(fmt_task_elapsed(5), "0m05s");
        assert_eq!(fmt_task_elapsed(192), "3m12s");
        assert_eq!(fmt_task_elapsed(3_599), "59m59s");
        assert_eq!(fmt_task_elapsed(3_600), "1h00m");
        assert_eq!(fmt_task_elapsed(3_900), "1h05m");
        assert_eq!(fmt_task_elapsed(90_000), "25h00m");
    }

    /// A start time in the future (clock skew) clamps to zero instead of
    /// rendering a negative duration.
    #[test]
    fn a_future_start_clamps_to_zero() {
        let rows = vec![TaskLedgerRow {
            kind: TaskKind::Shell,
            label: None,
            started_at: Some(2_000),
        }];
        assert!(
            task_ledger_text(&rows, 1_000).unwrap().ends_with(" · 0m00s"),
            "a skewed clock must not render a negative age"
        );
    }

    /// The estimate is clipped like the render it estimates: a label longer
    /// than the cap costs no more than a capped one, and each task adds a row.
    #[test]
    fn the_estimate_clips_labels_like_the_render() {
        let row = |label: String| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some(label),
            started_at: None,
        };
        let long = row("x".repeat(TASK_LABEL_CHARS + 500));
        let capped = row("x".repeat(TASK_LABEL_CHARS));
        assert_eq!(
            task_ledger_estimate(&[long]),
            task_ledger_estimate(std::slice::from_ref(&capped)),
            "the estimate must not grow past the rendered clip"
        );
        let pair = [capped.clone(), capped.clone()];
        assert!(
            task_ledger_estimate(&pair) > task_ledger_estimate(std::slice::from_ref(&capped)),
            "each task adds a row to the estimate"
        );
    }
}
