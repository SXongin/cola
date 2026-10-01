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

use super::sanitize::{AMPERSAND_ESCAPE, ASTERISK_ESCAPE, UNDERSCORE_ESCAPE};
use super::{first_n_chars_bytes, fmt_local_time, truncate_md};

/// Characters of a task label the ledger row shows before clipping — shared
/// with the completion entry's collapsed title (`🔔 shell 完成：<label>`), so
/// one label clips identically wherever the ledger renders it (ADR-0060).
pub(crate) const TASK_LABEL_CHARS: usize = 60;

/// The live list's stable `element_id` on the card (ADR-0060): the ledger is
/// one card-tail section with no timeline position of its own, so one fixed id
/// keeps the reader's fold state across re-renders.
pub(crate) const TASK_LEDGER_ELEMENT_ID: &str = "task_ledger";

/// The suffix a row the runtime could not confirm as running carries (issue
/// #454): appended after the row's own parts, so the row still shows its task
/// facts while the marker says the liveness is unverified.
const UNCONFIRMED_MARKER: &str = " · ⚠️ 状态待确认";

/// The kind of Background Task a ledger row names. Only these two background
/// through the V2 tool shape (ADR-0059/0060).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Subagent,
}

impl TaskKind {
    /// The row's type noun: the tool's own name, `shell` / `subagent` (#502).
    /// Shared by the live row and the completion entry's fold body, so one
    /// task type reads the same wherever the ledger renders it.
    fn noun(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Subagent => "subagent",
        }
    }

    /// The completion entry's collapsed-title phrase: the tool's own name plus
    /// what happened to the task — `🔔 shell 完成` / `🔔 subagent 已取消`. A
    /// Wake that reported a cancelled/error state, a runtime-confirmed end
    /// without a Wake, and a lost completion record each get their own ending —
    /// the entry never claims 完成 for work that was stopped or lost (issue
    /// #454). Built on [`Self::noun`], so the completion entry and the live row
    /// name the task type with one word.
    fn completion_noun(self, ending: &TaskEnding) -> String {
        let ending = match ending {
            TaskEnding::Wake { state } if state.as_deref() == Some("cancelled") => "已取消",
            TaskEnding::Wake { state } if state.as_deref() == Some("error") => "失败",
            TaskEnding::Wake { .. } => "完成",
            TaskEnding::RuntimeEnded => "结束",
            TaskEnding::Lost => "已失联",
        };
        format!("{} {ending}", self.noun())
    }
}

/// Why a Background Task left the live list, as its completion entry names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEnding {
    /// The backend's own completion Wake; `state` is what the Wake reported
    /// (`completed` / `cancelled` / `error`), when it reported one.
    Wake { state: Option<String> },
    /// A runtime reconciliation confirmed the shell ended while no Wake
    /// arrived (issue #454) — the card must not keep waiting on a lost
    /// completion record.
    RuntimeEnded,
    /// The runtime has no record of the task at all: the completion record was
    /// lost, and the task cannot be running under the attached server.
    Lost,
}

/// One live Background Task as its ledger row renders it: the task's type, the
/// label joined from the originating tool part's input by `call_id` (`None`
/// when that input names no label — the row then renders bare), when the run
/// started (`None` renders no elapsed), and whether a runtime reconciliation
/// could not confirm it as running (issue #454 — the row then carries the
/// 状态待确认 marker, and only its own Wake or a positive terminal verdict can
/// retire it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLedgerRow {
    pub kind: TaskKind,
    pub label: Option<String>,
    pub started_at: Option<i64>,
    /// True when a reconciliation read reported the task not running while no
    /// Wake retired it. The row stays live (the runtime read is evidence, not
    /// an ending) but reads as unconfirmed.
    pub unconfirmed: bool,
}

/// One completed Background Task as its ledger entry renders it (ADR-0060):
/// the mechanical completion line as the collapsed title, the task's identity
/// and its run's own server-time span in the fold. The Bridge gathers the
/// facts (which Wake completed what, the task it retired, or the runtime
/// ending that retired it without a Wake) and hands them over; this module owns
/// the pinned copy and the formats.
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
    /// When the run ended: the Wake's own server time, or the runtime's
    /// reported completion time. `None` for a [`TaskEnding::Lost`] task (the
    /// runtime had nothing to report) — the body then renders identity only,
    /// never an invented clock or duration.
    pub finished_at: Option<i64>,
    /// What ended the task, and how the collapsed title names it.
    pub ending: TaskEnding,
}

/// The completion entry's collapsed title (ADR-0060) — the mechanical
/// completion line the merged-path receipt always carried,
/// `🔔 shell 完成：<label>` / `🔔 subagent 完成：<label>`, bare when the Wake
/// named no label. The label goes through the shared [`folded_label`], so one
/// label has one visible shape wherever the ledger renders it.
pub(crate) fn task_entry_title(entry: &TaskCompletionEntry) -> String {
    let noun = entry.kind.completion_noun(&entry.ending);
    match entry.label.as_deref().filter(|label| !label.is_empty()) {
        Some(label) => format!("🔔 {noun}：{}", folded_label(label)),
        None => format!("🔔 {noun}"),
    }
}

/// The completion entry's fold body: the task's identity and the run's timing —
/// `shell sh_abc · 14:02 · 12m`, and `subagent ses_child · 14:02 · 1m` for the
/// subagent kind, whose noun is the ledger's own ([`TaskKind::noun`]). The
/// timing parts carry no Chinese labels (ADR-0060). Each part is omitted when
/// the read named none (an id-less or start-less entry stays honest rather than
/// inventing detail), the duration is the run's own server-time span
/// (finished − started), so re-rendering the entry never drifts, and a task
/// with no completion time (the lost ending) renders identity only.
pub(crate) fn task_entry_body(entry: &TaskCompletionEntry) -> String {
    let mut body = entry.kind.noun().to_string();
    if let Some(id) = entry.id.as_deref().filter(|id| !id.is_empty()) {
        body.push(' ');
        body.push_str(id);
    }
    let Some(finished) = entry.finished_at else {
        return body;
    };
    if let Some(clock) = fmt_local_time(finished) {
        body.push_str(&format!(" · {clock}"));
    }
    if let Some(started) = entry.started_at {
        body.push_str(&format!(
            " · {}",
            fmt_entry_elapsed(secs_since(started, finished))
        ));
    }
    body
}

/// The ledger section's title — the pinned copy `⏳ 后台任务（N）`, with the
/// unconfirmed count appended when a runtime reconciliation could not confirm
/// some rows as running (issue #454): `⏳ 后台任务（2 · 1 待确认）`.
///
/// `None` when no task is live (no empty header, so the section renders
/// nothing at all). The count is the folded panel's whole answer to "how many
/// are still running?", readable without unfolding (ADR-0060).
pub(crate) fn task_ledger_title(rows: &[TaskLedgerRow]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let unconfirmed = rows.iter().filter(|row| row.unconfirmed).count();
    Some(if unconfirmed > 0 {
        format!("⏳ 后台任务（{} · {} 待确认）", rows.len(), unconfirmed)
    } else {
        format!("⏳ 后台任务（{}）", rows.len())
    })
}

/// The ledger section's fold body — one row per live task, the pinned copy:
///
/// ```text
/// · shell：**npm run build** · 14:02 · 3m12s
/// · subagent：**review the diff** · 14:04 · 1m05s
/// ```
///
/// `None` when no task is live (the title's own emptiness rule, kept here so
/// the two renderings cannot drift). The type word stays plain, the label is
/// bolded through [`bold_label`], then the task's server `started_at` renders
/// as its local `HH:MM` start clock (the completion entry body's own clock)
/// and the bare elapsed follows. A part the read named none of is omitted
/// whole: `· shell · 14:02 · 0m05s`, `· shell：**npm run build**`, `· shell`.
/// A row a runtime reconciliation could not confirm as running carries the
/// trailing 状态待确认 marker (issue #454), after every part it did carry.
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
        if row.unconfirmed {
            text.push_str(UNCONFIRMED_MARKER);
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
/// wrapped in the row's bold markers, with the label's own `&` / `*` / `_`
/// swapped for the sanitizer's numeric entities so a command can never close
/// the span or bleed formatting into the next row. `&` goes first: a label
/// carrying entity text of its own renders it literally instead of seeding a
/// new construct. Feishu decodes the entities back to the characters, so the
/// label reads unchanged while exactly one bold span exists per row. The
/// completion entry's title is plain text and keeps [`folded_label`] raw —
/// nothing there interprets markdown.
fn bold_label(label: &str) -> String {
    let escaped = folded_label(label)
        .replace('&', AMPERSAND_ESCAPE)
        .replace('*', ASTERISK_ESCAPE)
        .replace('_', UNDERSCORE_ESCAPE);
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
                unconfirmed: false,
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some("review the diff".into()),
                started_at: Some(second_start),
                unconfirmed: false,
            },
        ];
        assert_eq!(task_ledger_title(&rows).unwrap(), "⏳ 后台任务（2）");
        assert_eq!(
            task_ledger_text(&rows, now).unwrap(),
            "· shell：**gh run watch** · 14:02 · 3m12s\n· subagent：**review the diff** · 14:04 · 1m05s"
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
            unconfirmed: false,
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
                    unconfirmed: false,
                }],
                now,
            )
            .unwrap(),
            "· subagent：**review the diff** · 14:02 · 3m12s"
        );
    }

    /// A row a runtime reconciliation could not confirm as running carries the
    /// trailing marker after every part it did carry, and the title names the
    /// unconfirmed count without unfolding (issue #454).
    #[test]
    fn an_unconfirmed_row_carries_the_marker_and_the_title_counts_it() {
        let start = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let now = start + 192_000; // 14:05:12, elapsed 3m12s
        let rows = vec![
            TaskLedgerRow {
                kind: TaskKind::Shell,
                label: Some("npm run build".into()),
                started_at: Some(start),
                unconfirmed: true,
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: None,
                started_at: None,
                unconfirmed: false,
            },
        ];
        assert_eq!(task_ledger_title(&rows).unwrap(), "⏳ 后台任务（2 · 1 待确认）");
        assert_eq!(
            task_ledger_text(&rows, now).unwrap(),
            "· shell：**npm run build** · 14:02 · 3m12s · ⚠️ 状态待确认\n· subagent"
        );
        // A fully unconfirmed section keeps the same shape; the marker only
        // appends, it never replaces a row's own facts.
        let all_unconfirmed = vec![TaskLedgerRow {
            kind: TaskKind::Shell,
            label: None,
            started_at: None,
            unconfirmed: true,
        }];
        assert_eq!(
            task_ledger_title(&all_unconfirmed).unwrap(),
            "⏳ 后台任务（1 · 1 待确认）"
        );
        assert_eq!(
            task_ledger_text(&all_unconfirmed, now).unwrap(),
            "· shell · ⚠️ 状态待确认"
        );
    }

    /// The entry names what actually happened (issue #454): a Wake's own
    /// reported state, a runtime-confirmed end, or a lost record — never 完成
    /// for work that was cancelled, failed, or lost — and a lost entry carries
    /// identity only, with no invented completion clock.
    #[test]
    fn the_entry_names_the_ending_and_a_lost_entry_has_no_clock() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        let shell_entry = |ending: TaskEnding, finished_at: Option<i64>| TaskCompletionEntry {
            kind: TaskKind::Shell,
            label: Some("gh run watch".into()),
            id: Some("sh_abc".into()),
            started_at: Some(finished - 12 * 60_000),
            finished_at,
            ending,
        };
        let wake = |state: Option<&str>| TaskEnding::Wake {
            state: state.map(str::to_string),
        };

        assert_eq!(
            task_entry_title(&shell_entry(wake(None), Some(finished))),
            "🔔 shell 完成：gh run watch"
        );
        assert_eq!(
            task_entry_title(&shell_entry(wake(Some("cancelled")), Some(finished))),
            "🔔 shell 已取消：gh run watch"
        );
        assert_eq!(
            task_entry_title(&shell_entry(wake(Some("error")), Some(finished))),
            "🔔 shell 失败：gh run watch"
        );
        assert_eq!(
            task_entry_title(&shell_entry(TaskEnding::RuntimeEnded, Some(finished))),
            "🔔 shell 结束：gh run watch"
        );
        assert_eq!(
            task_entry_title(&shell_entry(TaskEnding::Lost, None)),
            "🔔 shell 已失联：gh run watch"
        );
        assert_eq!(
            task_entry_body(&shell_entry(TaskEnding::RuntimeEnded, Some(finished))),
            "shell sh_abc · 14:02 · 12m"
        );
        assert_eq!(
            task_entry_body(&shell_entry(TaskEnding::Lost, None)),
            "shell sh_abc",
            "the lost ending has nothing to clock"
        );

        // The subagent keeps its own noun per ending.
        let subagent = |ending: TaskEnding| TaskCompletionEntry {
            kind: TaskKind::Subagent,
            label: None,
            id: Some("ses_child".into()),
            started_at: None,
            finished_at: None,
            ending,
        };
        assert_eq!(
            task_entry_title(&subagent(TaskEnding::Lost)),
            "🔔 subagent 已失联"
        );
        assert_eq!(
            task_entry_title(&subagent(wake(Some("cancelled")))),
            "🔔 subagent 已取消"
        );
    }

    /// A label's own markdown characters cannot bleed out of its bold span:
    /// the row swaps `&` first, then `*`/`_`, for the sanitizer's numeric
    /// entities before the `**…**` wrap, so exactly one bold span exists per
    /// row whatever the command contains, and a label carrying entity text of
    /// its own (`&#42;`, `&amp;`) renders that text literally once decoded
    /// instead of seeding a construct. The completion entry's plain-text title
    /// keeps the raw label.
    #[test]
    fn a_labels_markdown_characters_stay_inside_its_bold_span() {
        let row = |label: &str| TaskLedgerRow {
            kind: TaskKind::Shell,
            label: Some(label.into()),
            started_at: None,
            unconfirmed: false,
        };
        let render = |label: &str| task_ledger_text(&[row(label)], 0).unwrap();

        // `*`/`_` are neutralized inside the wrap.
        let starry_rendered = render("git log --format=*_*_* -- foo_bar");
        assert_eq!(
            starry_rendered, "· shell：**git log --format=&#42;&#95;&#42;&#95;&#42; -- foo&#95;bar**",
            "only the entities replace the characters"
        );
        // `&` goes first: a label's own entity text stays literal text once
        // decoded (`&amp;#42;` → `&#42;`), never a new emphasis character.
        let entity_rendered = render("echo &#42; & done");
        assert_eq!(entity_rendered, "· shell：**echo &amp;#42; &amp; done**");
        // The order matters when a label holds both characters: with the
        // escape applied to `*` first, `a*b` would become `a&amp;#42;b` and
        // decode to the entity text instead of the star.
        assert_eq!(render("a*b & c"), "· shell：**a&#42;b &amp; c**");

        for label in [
            "git log --format=*_*_* -- foo_bar",
            "echo &#42; & done",
            "a*b_c&d",
        ] {
            let each = render(label);
            assert_eq!(
                each.matches("**").count(),
                2,
                "one bold span is its two markers, nothing for a command to close: {each}"
            );
        }

        // Both the wrap and the entities run through the builder's markdown
        // sanitizer untouched (it only rewrites `<` and images) — the label
        // carrying `&` / literal entity text included.
        use crate::feishu::card::CardState;
        use crate::feishu::card::shell::CardBuilder;

        let built = CardBuilder::new()
            .with_state(CardState::Streaming)
            .with_task_ledger(&[row("git log --format=*_*_* -- foo_bar"), row("echo &#42; & done")])
            .build();
        let body = built["body"]["elements"][0]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(
            body,
            format!("{starry_rendered}\n{entity_rendered}"),
            "the sanitizer leaves the rows alone: {built}"
        );
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
                unconfirmed: false,
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: Some(String::new()),
                started_at: None,
                unconfirmed: false,
            },
        ];
        assert_eq!(
            task_ledger_text(&rows, now).unwrap(),
            "· shell · 14:01 · 0m05s\n· subagent"
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
            unconfirmed: false,
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
            unconfirmed: false,
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
                unconfirmed: false,
            },
            TaskLedgerRow {
                kind: TaskKind::Subagent,
                label: None,
                started_at: None,
                unconfirmed: false,
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
            unconfirmed: false,
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
            finished_at: Some(finished),
            ending: TaskEnding::Wake { state: None },
        };
        assert_eq!(task_entry_title(&entry), "🔔 shell 完成：gh run watch");
        assert_eq!(task_entry_body(&entry), "shell sh_abc · 14:02 · 12m");

        let subagent = TaskCompletionEntry {
            kind: TaskKind::Subagent,
            label: Some("review the diff".into()),
            id: Some("ses_child".into()),
            started_at: Some(finished - 65_000),
            finished_at: Some(finished),
            ending: TaskEnding::Wake { state: None },
        };
        assert_eq!(task_entry_title(&subagent), "🔔 subagent 完成：review the diff");
        assert_eq!(task_entry_body(&subagent), "subagent ses_child · 14:02 · 1m");
    }

    /// A Wake that named no label renders the bare completion line — it says
    /// only what finished, never inventing detail.
    #[test]
    fn an_unnamed_completion_renders_the_bare_title() {
        let finished = crate::feishu::card::test_local_ms(2026, 9, 29, 14, 2);
        for (kind, title) in [
            (TaskKind::Shell, "🔔 shell 完成"),
            (TaskKind::Subagent, "🔔 subagent 完成"),
        ] {
            let entry = TaskCompletionEntry {
                kind,
                label: None,
                id: Some("sh_abc".into()),
                started_at: None,
                finished_at: Some(finished),
                ending: TaskEnding::Wake { state: None },
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
            finished_at: Some(finished),
            ending: TaskEnding::Wake { state: None },
        };
        assert_eq!(task_entry_title(&empty), "🔔 shell 完成");
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
                finished_at: Some(finished),
                ending: TaskEnding::Wake { state: None },
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
            finished_at: Some(finished),
            ending: TaskEnding::Wake { state: None },
        };
        assert_eq!(
            task_entry_title(&entry),
            format!("🔔 shell 完成：{}…", "x".repeat(TASK_LABEL_CHARS))
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
