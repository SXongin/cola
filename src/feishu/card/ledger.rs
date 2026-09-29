//! The Background Task Ledger (ADR-0060).
//!
//! The card-tail section that lists a Session's live Background Tasks — task
//! type, bolded label, start clock, elapsed — riding the newest card of its
//! Card Chain like the Todo Panel and the live Tool Panels. One folded
//! collapsible panel: the pinned count (`⏳ 后台任务（N）`) is its title, so it
//! stays readable folded, and the rows are its body. The Bridge gathers the
//! facts (which tasks are live, what label their originating tool part's input
//! names) and hands them over as [`TaskLedgerRow`]s; this module owns the
//! pinned copy, the elapsed format and nothing else.
//!
//! A completed task leaves that list: its mechanical completion line becomes
//! the collapsed title of one folded entry on the card the task lived on
//! ([`TaskCompletionEntry`]), identity and the run's own server-time span in
//! the fold. The entry's title, body and estimate live here beside the row's,
//! so the two renderings of one task cannot drift apart.
//!
//! Empty means no section: a card with no live task renders exactly what it
//! did before, and V1 (which carries no Background Task facts) never shows
//! one.

use super::{first_n_chars_bytes, fmt_local_time, truncate_md};

/// Characters of a task label the ledger row shows before clipping — shared
/// with the completion entry's collapsed title (`🔔 后台任务完成：<label>`), so
/// one label clips identically wherever the ledger renders it (ADR-0060).
pub(crate) const TASK_LABEL_CHARS: usize = 60;

/// The live list's stable `element_id` on the card (ADR-0060): the ledger is
/// one card-tail section with no timeline position of its own, so one fixed id
/// keeps the reader's fold state across re-renders.
pub(crate) const TASK_LEDGER_ELEMENT_ID: &str = "task_ledger";

/// The kind of Background Task a ledger row names. Only these two background
/// through the V2 tool shape (ADR-0059/0060).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Subagent,
}

impl TaskKind {
    /// The row's type noun: `shell` stays the tool's own name (the pinned
    /// copy), the subagent reads in Chinese like the completion entry's
    /// `子代理完成`.
    fn noun(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Subagent => "子代理",
        }
    }

    /// The completion entry's collapsed-title noun: what finished, in the one
    /// noun the merged-path receipt always used (`🔔 后台任务完成` /
    /// `🔔 子代理完成`). Distinct from [`Self::noun`], which names the task
    /// type on the live row.
    fn completion_noun(self) -> &'static str {
        match self {
            Self::Shell => "后台任务完成",
            Self::Subagent => "子代理完成",
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

/// One completed Background Task as its ledger entry renders it (ADR-0060):
/// the mechanical completion line as the collapsed title, the task's identity
/// and its run's own server-time span in the fold. The Bridge gathers the
/// facts (which Wake completed what, the task it retired) and hands them over;
/// this module owns the pinned copy and the formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCompletionEntry {
    pub kind: TaskKind,
    /// The finished work's label as the Wake named it (`None` or empty renders
    /// the bare completion line — the label is never invented).
    pub label: Option<String>,
    /// The task's identity as the read names it: the shell id / child session
    /// id / launching call's id. `None` renders the body without one.
    pub id: Option<String>,
    /// When the run started, when the read still carries its launch — the
    /// fold's duration. `None` renders identity and clock only.
    pub started_at: Option<i64>,
    /// When it finished: the Wake's own server time, also the fold's clock.
    pub finished_at: i64,
}

/// The completion entry's collapsed title (ADR-0060) — the mechanical
/// completion line the merged-path receipt always carried,
/// `🔔 后台任务完成：<label>` / `🔔 子代理完成：<label>`, bare when the Wake
/// named no label. The label goes through the shared [`folded_label`], so one
/// label has one visible shape wherever the ledger renders it.
pub(crate) fn task_entry_title(entry: &TaskCompletionEntry) -> String {
    let noun = entry.kind.completion_noun();
    match entry.label.as_deref().filter(|label| !label.is_empty()) {
        Some(label) => format!("🔔 {noun}：{}", folded_label(label)),
        None => format!("🔔 {noun}"),
    }
}

/// The completion entry's fold body: the task's identity and the run's timing —
/// `shell sh_abc · 14:02 · 12m`, and `子代理 ses_child · 14:02 · 1m` for the
/// subagent kind, whose noun is the ledger's own ([`TaskKind::noun`]). The
/// timing parts carry no Chinese labels (ADR-0060). Each part is omitted when
/// the read named none (an id-less or start-less entry stays honest rather than
/// inventing detail), and the duration is the run's own server-time span
/// (finished − started), so re-rendering the entry never drifts.
pub(crate) fn task_entry_body(entry: &TaskCompletionEntry) -> String {
    let mut body = entry.kind.noun().to_string();
    if let Some(id) = entry.id.as_deref().filter(|id| !id.is_empty()) {
        body.push(' ');
        body.push_str(id);
    }
    if let Some(clock) = fmt_local_time(entry.finished_at) {
        body.push_str(&format!(" · {clock}"));
    }
    if let Some(started) = entry.started_at {
        body.push_str(&format!(
            " · {}",
            fmt_entry_elapsed(secs_since(started, entry.finished_at))
        ));
    }
    body
}

/// The ledger section's title — the pinned copy `⏳ 后台任务（N）`:
///
/// `None` when no task is live (no empty header, so the section renders
/// nothing at all). The count is the folded panel's whole answer to "how many
/// are still running?", readable without unfolding (ADR-0060).
pub(crate) fn task_ledger_title(rows: &[TaskLedgerRow]) -> Option<String> {
    (!rows.is_empty()).then(|| format!("⏳ 后台任务（{}）", rows.len()))
}

/// The ledger section's fold body — one row per live task, the pinned copy:
///
/// ```text
/// · shell：**npm run build** · 14:02 · 3m12s
/// · 子代理：**review the diff** · 14:03 · 1m05s
/// ```
///
/// `None` when no task is live (the title's own emptiness rule, kept here so
/// the two renderings cannot drift). The type word stays plain, the label is
/// bolded through [`bold_label`], then the task's server `started_at` renders
/// as its local `HH:MM` start clock (the completion entry body's own clock)
/// and the bare elapsed follows. A part the read named none of is omitted
/// whole: `· shell · 14:02 · 0m05s`, `· shell：**npm run build**`, `· shell`.
/// Identity stays out of the live row — the completion entry's fold body
/// carries it. Rows render in the order given (the transcript's own), one line
/// each: a multi-line command cannot break the row layout, and one label clips
/// identically wherever the ledger renders it. The caller sanitizes the text
/// like any other model-authored markdown.
pub(crate) fn task_ledger_text(rows: &[TaskLedgerRow], now_ms: i64) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let mut text = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            text.push('\n');
        }
        text.push_str("· ");
        text.push_str(row.kind.noun());
        if let Some(label) = row.label.as_deref().filter(|label| !label.is_empty()) {
            text.push('：');
            text.push_str(&bold_label(label));
        }
        if let Some(at) = row.started_at {
            if let Some(clock) = fmt_local_time(at) {
                text.push_str(&format!(" · {clock}"));
            }
            text.push_str(&format!(" · {}", fmt_task_elapsed(secs_since(at, now_ms))));
        }
    }
    Some(text)
}

/// The ledger's render clock (ADR-0060): each row's elapsed in whole seconds
/// against `now_ms` — the very value [`task_ledger_text`] renders — in row
/// order, so the clock records what the card last rendered. The Bridge compares
/// it at the cadence each path owes (`LedgerCadence`): the live render at whole
/// minutes (its flushes are content-driven; a per-render second clock would be
/// churn), the yielded refresh at whole seconds (its ~8 s reads are a waiting
/// card's only clock). A row with no start time never ticks (`None`), and a
/// start skewed into the future clamps like the render it keys.
pub(crate) fn task_ledger_clock(rows: &[TaskLedgerRow], now_ms: i64) -> Vec<Option<u64>> {
    rows.iter()
        .map(|row| row.started_at.map(|at| secs_since(at, now_ms)))
        .collect()
}

/// The one shape a task label takes inside the ledger: newlines fold to spaces
/// (a multi-line command cannot break a row or a completion entry's panel
/// header) and the text clips like a short line. Shared by the live row and
/// the completion entry's title, so the two renderings of one label cannot
/// drift apart (ADR-0060).
fn folded_label(label: &str) -> String {
    let label = label.replace(['\n', '\r'], " ");
    truncate_md(&label, TASK_LABEL_CHARS)
}

/// The live row's label: the shared folded/clipped shape ([`folded_label`])
/// wrapped in the row's bold markers, with the label's own `*`/`_` swapped for
/// their numeric entities so a command can never close the span or bleed
/// formatting into the next row. Feishu decodes the entities back to the
/// literal characters (the same mechanism as the `<` escape), so the label
/// reads unchanged while exactly one bold span exists per row. The completion
/// entry's title is plain text and keeps [`folded_label`] raw — nothing there
/// interprets markdown.
fn bold_label(label: &str) -> String {
    let escaped = folded_label(label).replace('*', "&#42;").replace('_', "&#95;");
    format!("**{escaped}**")
}

/// Estimated serialized size (bytes) of the ledger section, for the card
/// splitter's tail reserve: the folded panel's title and element overhead, plus
/// one row per task — its ` · HH:MM` start clock, its ` · XmYYs` elapsed, and
/// its bolded label, counted exactly as [`bold_label`] renders it (clipped at
/// [`TASK_LABEL_CHARS`], entity-escaped and wrapped) so the estimate and the
/// row cannot drift apart. Rough like the Bridge's `panel_estimate` for the
/// tail's other sections.
pub(crate) fn task_ledger_estimate(rows: &[TaskLedgerRow]) -> usize {
    let labels: usize = rows
        .iter()
        .filter_map(|row| row.label.as_deref().filter(|label| !label.is_empty()))
        .map(|label| bold_label(label).len())
        .sum();
    // The +80 is the folded panel's own element overhead, exactly like the
    // completion entry's estimate charges it (`task_entry_estimate`).
    300 + labels + rows.len() * 120 + 80
}

/// Estimated serialized size (bytes) of one completion entry's folded panel,
/// for the card splitter's timeline accounting: the title (its label clipped
/// like [`task_entry_title`] clips it), the body's identity and clock, and the
/// panel's element overhead. Owned here so it cannot drift from the render.
pub(crate) fn task_entry_estimate(entry: &TaskCompletionEntry) -> usize {
    let label = entry
        .label
        .as_deref()
        .map(|label| first_n_chars_bytes(label, TASK_LABEL_CHARS))
        .unwrap_or(0);
    let id = entry.id.as_deref().map(str::len).unwrap_or(0);
    300 + label + id + 80
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

/// The completion entry's duration: minute-granular, so a fixed entry reads
/// the same on every re-render and matches the pinned body (`12m`). Bare like
/// the row's elapsed, with the same `XhYYm` shape above an hour.
fn fmt_entry_elapsed(secs: u64) -> String {
    if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Seconds between two epoch-ms clocks, never negative (a clock skewed into
/// the future clamps to zero instead of rendering a negative age).
fn secs_since(at_ms: i64, now_ms: i64) -> u64 {
    ((now_ms - at_ms).max(0) / 1000) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned copy (ADR-0060, #412/#423): the title is the count, the body
    /// one row per task — type noun, bolded label, start clock and bare elapsed
    /// — `3m12s` / `1m05s` for this pair.
    #[test]
    fn the_section_renders_the_pinned_copy() {
        let start = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        // 127 s apart at 14:04:07, so the second row's clock is 14:04 and its
        // elapsed 1m05s at `now` (14:05:12).
        let second_start = start + 127_000;
        let now = start + 192_000;
        let rows = vec![
            TaskLedgerRow {
                kind: TaskKind::Shell,
                label: Some("gh run watch".into()),
                started_at: Some(start),
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some("review the diff".into()),
                started_at: Some(second_start),
            },
        ];
        assert_eq!(task_ledger_title(&rows).unwrap(), "⏳ 后台任务（2）");
        assert_eq!(
            task_ledger_text(&rows, now).unwrap(),
            "· shell：**gh run watch** · 14:02 · 3m12s\n· 子代理：**review the diff** · 14:04 · 1m05s"
        );
    }

    /// The four shape edges of a live row (ADR-0060 amendment, #423): the
    /// start clock ` · HH:MM` and the ` · XmYYs` elapsed arrive together only
    /// when the read carried a start, and a missing label drops its `：…` span
    /// whole — never a dangling colon or separator. The type word stays plain
    /// and only the label is bolded.
    #[test]
    fn the_row_renders_its_four_shapes() {
        let at = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let now = at + 192_000; // 14:05:12, so the elapsed is 3m12s
        let row = |label: Option<&str>, started_at: Option<i64>| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: label.map(str::to_string),
            started_at,
        };
        let render = |row: &TaskLedgerRow| task_ledger_text(std::slice::from_ref(row), now).unwrap();

        assert_eq!(
            render(&row(Some("npm run build"), Some(at))),
            "· shell：**npm run build** · 14:02 · 3m12s"
        );
        assert_eq!(render(&row(None, Some(at))), "· shell · 14:02 · 3m12s");
        assert_eq!(
            render(&row(Some("npm run build"), None)),
            "· shell：**npm run build**"
        );
        assert_eq!(render(&row(None, None)), "· shell");
        // The subagent keeps its own noun.
        assert_eq!(
            task_ledger_text(
                &[TaskLedgerRow {
                    kind: TaskKind::Subagent,
                    label: Some("review the diff".into()),
                    started_at: Some(at),
                }],
                now,
            )
            .unwrap(),
            "· 子代理：**review the diff** · 14:02 · 3m12s"
        );
    }

    /// A label's own emphasis characters cannot bleed out of its bold span: the
    /// row swaps `*`/`_` for their numeric entities before the `**…**` wrap, so
    /// exactly one bold span exists per row whatever the command contains (the
    /// completion entry's plain-text title keeps the raw label).
    #[test]
    fn a_labels_emphasis_characters_stay_inside_its_bold_span() {
        let rows = vec![TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some("git log --format=*_*_* -- foo_bar".into()),
            started_at: None,
        }];
        let row = task_ledger_text(&rows, 0).unwrap();
        assert_eq!(
            row, "· shell：**git log --format=&#42;&#95;&#42;&#95;&#42; -- foo&#95;bar**",
            "only the entities replace the characters"
        );
        assert_eq!(
            row.matches("**").count(),
            2,
            "one bold span is its two markers, nothing for a command to close: {row}"
        );
        // Both the wrap and the entities run through the builder's markdown
        // sanitizer untouched (it only rewrites `<` and images).
        use crate::feishu::card::CardState;
        use crate::feishu::card::shell::CardBuilder;

        let card = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_task_ledger(&rows)
            .build();
        let body = card["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(body, row, "the sanitizer leaves the row alone: {card}");
    }

    /// No live task means no section — neither the title nor the body renders,
    /// so a card (and V1, which has no Background Task facts) is unchanged.
    #[test]
    fn an_empty_ledger_renders_nothing() {
        assert_eq!(task_ledger_title(&[]), None);
        assert_eq!(task_ledger_text(&[], 1_800_000_000_000), None);
    }

    /// A task whose input names no label renders bare (the type first, then the
    /// clock and elapsed), and a task with no start time shows neither clock
    /// nor elapsed.
    #[test]
    fn a_label_less_task_renders_bare() {
        let now = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let rows = vec![
            TaskLedgerRow {
                kind: TaskKind::Shell,
                label: None,
                started_at: Some(now - 5_000), // 14:01:55
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some(String::new()),
                started_at: None,
            },
        ];
        assert_eq!(
            task_ledger_text(&rows, now).unwrap(),
            "· shell · 14:01 · 0m05s\n· 子代理"
        );
    }

    /// The label clips exactly like the completion entry's title: at
    /// [`TASK_LABEL_CHARS`] characters plus the `…` marker (inside the bold),
    /// and a multi-line command folds to one row.
    #[test]
    fn labels_clip_like_the_receipt() {
        let now = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let long = "x".repeat(TASK_LABEL_CHARS + 20);
        let rows = vec![TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some(format!("{long}\nsecond line")),
            started_at: Some(now),
        }];
        let text = task_ledger_text(&rows, now).unwrap();
        let row = text.lines().next().unwrap();
        assert_eq!(
            row,
            format!("· shell：**{}…** · 14:02 · 0m00s", "x".repeat(TASK_LABEL_CHARS))
        );
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

    /// The render clock records the elapsed in whole SECONDS — the value the
    /// row renders (ADR-0060): the Bridge compares it at the cadence each path
    /// owes, so a row with no start time has no clock at all, and a future
    /// start clamps like the render it keys.
    #[test]
    fn the_render_clock_reads_whole_seconds() {
        let start = 1_800_000_000_000;
        let rows = vec![
            TaskLedgerRow {
                kind: TaskKind::Shell,
                label: None,
                started_at: Some(start),
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: None,
                started_at: None,
            },
        ];

        let clock = task_ledger_clock(&rows, start + 59_000);
        assert_eq!(clock, vec![Some(59), None], "the rendered seconds, in row order");
        assert_eq!(
            task_ledger_clock(&rows, start + 60_000),
            vec![Some(60), None],
            "the seconds keep moving through a minute"
        );
        assert_eq!(
            task_ledger_clock(&rows, start - 5_000),
            vec![Some(0), None],
            "a future start clamps like the render"
        );
        assert_eq!(task_ledger_clock(&[], start), Vec::<Option<u64>>::new());
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
            task_ledger_estimate(std::slice::from_ref(&capped)) + "…".len(),
            "a longer label costs no more than the rendered clip plus its marker"
        );
        let pair = [capped.clone(), capped.clone()];
        assert!(
            task_ledger_estimate(&pair) > task_ledger_estimate(std::slice::from_ref(&capped)),
            "each task adds a row to the estimate"
        );
        // The estimate counts the label exactly as the row renders it: the
        // bold markers and any neutralized emphasis characters included.
        let starred = row("*".repeat(TASK_LABEL_CHARS));
        assert!(
            task_ledger_estimate(&[starred]) > task_ledger_estimate(std::slice::from_ref(&capped)),
            "an escaped label costs what its entities render"
        );
    }

    /// The pinned completion entry (ADR-0060, #412): the mechanical completion
    /// line as the collapsed title, identity and timing as the fold body —
    /// `shell sh_abc · 14:02 · 12m`, with the local clock of the Wake.
    #[test]
    fn the_completion_entry_renders_the_pinned_copy() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let entry = TaskCompletionEntry {
            kind: TaskKind::Shell,
            label: Some("gh run watch".into()),
            id: Some("sh_abc".into()),
            started_at: Some(finished - 12 * 60_000),
            finished_at: finished,
        };
        assert_eq!(task_entry_title(&entry), "🔔 后台任务完成：gh run watch");
        assert_eq!(task_entry_body(&entry), "shell sh_abc · 14:02 · 12m");

        let subagent = TaskCompletionEntry {
            kind: TaskKind::Subagent,
            label: Some("review the diff".into()),
            id: Some("ses_child".into()),
            started_at: Some(finished - 65_000),
            finished_at: finished,
        };
        assert_eq!(task_entry_title(&subagent), "🔔 子代理完成：review the diff");
        assert_eq!(task_entry_body(&subagent), "子代理 ses_child · 14:02 · 1m");
    }

    /// A Wake that named no label renders the bare completion line — it says
    /// only what finished, never inventing detail.
    #[test]
    fn an_unnamed_completion_renders_the_bare_title() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        for (kind, title) in [
            (TaskKind::Shell, "🔔 后台任务完成"),
            (TaskKind::Subagent, "🔔 子代理完成"),
        ] {
            let entry = TaskCompletionEntry {
                kind,
                label: None,
                id: Some("sh_abc".into()),
                started_at: None,
                finished_at: finished,
            };
            assert_eq!(task_entry_title(&entry), title);
            assert_eq!(task_entry_body(&entry), format!("{} sh_abc · 14:02", kind.noun()));
        }
        // An empty label is the same as none: no dangling `：`.
        let empty = TaskCompletionEntry {
            kind: TaskKind::Shell,
            label: Some(String::new()),
            id: None,
            started_at: None,
            finished_at: finished,
        };
        assert_eq!(task_entry_title(&empty), "🔔 后台任务完成");
    }

    /// Each body part is omitted when the read named none: no identity, no
    /// start time, or neither — never a placeholder or a `·` with nothing
    /// after it.
    #[test]
    fn the_entry_body_omits_unknown_parts() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let body_of = |id: Option<&str>, started_at: Option<i64>| {
            task_entry_body(&TaskCompletionEntry {
                kind: TaskKind::Shell,
                label: None,
                id: id.map(str::to_string),
                started_at,
                finished_at: finished,
            })
        };
        assert_eq!(
            body_of(Some("sh_abc"), Some(finished - 180_000)),
            "shell sh_abc · 14:02 · 3m"
        );
        assert_eq!(body_of(None, Some(finished - 180_000)), "shell · 14:02 · 3m");
        assert_eq!(body_of(Some(""), Some(finished)), "shell · 14:02 · 0m");
        assert_eq!(body_of(Some("sh_abc"), None), "shell sh_abc · 14:02");
        assert_eq!(body_of(None, None), "shell · 14:02");
    }

    /// The entry's label clips and folds exactly like the live row's: at
    /// [`TASK_LABEL_CHARS`] characters plus `…`, one line even for a
    /// multi-line command.
    #[test]
    fn entry_labels_fold_and_clip_like_the_row() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let long = "x".repeat(TASK_LABEL_CHARS + 20);
        let entry = TaskCompletionEntry {
            kind: TaskKind::Shell,
            label: Some(format!("{long}\nsecond line")),
            id: Some("sh_abc".into()),
            started_at: None,
            finished_at: finished,
        };
        assert_eq!(
            task_entry_title(&entry),
            format!("🔔 后台任务完成：{}…", "x".repeat(TASK_LABEL_CHARS))
        );
    }

    /// The entry's duration is minute-granular (never drifting seconds), and
    /// switches to the row's `XhYYm` shape above an hour; a negative span (a
    /// clock skewed into the future) clamps to zero.
    #[test]
    fn entry_elapsed_is_minute_granular() {
        assert_eq!(fmt_entry_elapsed(0), "0m");
        assert_eq!(fmt_entry_elapsed(5), "0m");
        assert_eq!(fmt_entry_elapsed(59), "0m");
        assert_eq!(fmt_entry_elapsed(60), "1m");
        assert_eq!(fmt_entry_elapsed(12 * 60), "12m");
        assert_eq!(fmt_entry_elapsed(3_599), "59m");
        assert_eq!(fmt_entry_elapsed(3_600), "1h00m");
        assert_eq!(fmt_entry_elapsed(3_900), "1h05m");
        assert_eq!(fmt_entry_elapsed(90_000), "25h00m");
    }
}
